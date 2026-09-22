#![allow(
    clippy::wildcard_imports,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::single_call_fn,
    clippy::needless_pass_by_value
)]
//! Interp-tier bidirectional `std::http::websocket` surface: `serve` /
//! `connect` plus the handle ops `send_text` / `send_binary` / `recv` /
//! `close`. Drives the same RFC 6455 framing engine (`gossamer_ws`) as
//! the compiled-tier `gos_rt_ws_*` shims, so the wire behaviour is
//! identical across the bytecode VM, Cranelift JIT, and LLVM AOT tiers.
//!
//! Handle model - a process-global registry keyed by an `i64` handle.
//! The VM runs goroutines on an OS-thread pool, so the registry is a
//! `Mutex` (not thread-local): a handle created by the server goroutine's
//! handler and a handle created by the client goroutine both resolve from
//! any worker thread. Each connection is held behind `Arc<Mutex<_>>` so a
//! blocking `recv` releases the registry lock before parking on the
//! socket.

use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use gossamer_ws::{Message, WebSocket};

use crate::builtins::{BuiltinFnPub, as_str, err_variant, ok_variant, value_to_int};
use crate::value::{NativeDispatch, RuntimeResult, Value};

/// One registered connection. `Arc<Mutex<_>>` so a blocking `recv`
/// releases the registry lock before parking on the socket.
type WsConn = Arc<Mutex<WebSocket<TcpStream>>>;

/// Process-global WebSocket handle registry. Shared across the VM's
/// goroutine worker threads, so a handle crosses goroutine boundaries.
fn ws_registry() -> &'static Mutex<HashMap<i64, WsConn>> {
    static REGISTRY: OnceLock<Mutex<HashMap<i64, WsConn>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_WS_HANDLE: AtomicI64 = AtomicI64::new(1);

fn next_handle() -> i64 {
    NEXT_WS_HANDLE.fetch_add(1, Ordering::Relaxed)
}

fn conn_clone(h: i64) -> Option<WsConn> {
    ws_registry().lock().get(&h).cloned()
}

fn register_conn(ws: WebSocket<TcpStream>) -> i64 {
    let h = next_handle();
    ws_registry().lock().insert(h, Arc::new(Mutex::new(ws)));
    h
}

fn unregister_conn(h: i64) {
    ws_registry().lock().remove(&h);
}

pub(crate) fn install_http_ws(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("connect", builtin_ws_connect),
        ("send_text", builtin_ws_send_text),
        ("send_binary", builtin_ws_send_binary),
        ("recv", builtin_ws_recv),
        ("close", builtin_ws_close),
    ];
    for (short, call) in entries {
        let qualified: &'static str =
            Box::leak(format!("http::websocket::{short}").into_boxed_str());
        globals.push((qualified, crate::builtins::builtin_pub(qualified, *call)));
        let ws_qualified: &'static str = Box::leak(format!("websocket::{short}").into_boxed_str());
        globals.push((
            ws_qualified,
            crate::builtins::builtin_pub(ws_qualified, *call),
        ));
    }
}

/// `websocket::serve(addr, handler) -> Result<(), Error>`. Binds a TCP
/// listener, upgrades each connection, and dispatches the handler's
/// `handle(&self, ws: i64)` method with the connected handle. The handler
/// runs the blocking recv/send loop and returns to close the connection.
/// Registered as a native dispatcher in `builtins::install` so it can
/// re-enter the VM to call the user handler.
pub(crate) fn native_websocket_serve(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let Some(Value::String(addr)) = args.first() else {
        return Ok(err_variant("websocket::serve: expected address string"));
    };
    let addr = addr.as_str().to_string();
    let Some(handler) = args.get(1).cloned() else {
        return Ok(err_variant("websocket::serve: missing handler"));
    };
    // Resolve `handle` to the handler's specific impl by struct name, the
    // same dispatch shape `http::serve` uses for `serve`.
    let handle_method = match &handler {
        Value::Struct(inner) => format!("{}::handle", inner.name),
        _ => "handle".to_string(),
    };
    let listener = match gossamer_runtime::listen::bind_tcp(&addr) {
        Ok(l) => l,
        Err(e) => return Ok(err_variant(format!("websocket::serve: {e}"))),
    };
    let dispatch_cell = std::cell::RefCell::new(dispatch);
    loop {
        let Ok((mut stream, _peer)) = listener.accept() else {
            break;
        };
        if gossamer_ws::server_accept(&mut stream).is_err() {
            continue;
        }
        let handle = register_conn(WebSocket::server(stream));
        let mut guard = dispatch_cell.borrow_mut();
        let _ = guard.call_fn(&handle_method, vec![handler.clone(), Value::Int(handle)]);
        drop(guard);
        unregister_conn(handle);
    }
    Ok(ok_variant(Value::Unit))
}

/// `websocket::connect(url) -> Result<i64, Error>`. Client TCP connect +
/// RFC 6455 upgrade against a `ws://host:port/path` URL; returns the
/// connected handle.
pub(crate) fn builtin_ws_connect(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.first().and_then(as_str).unwrap_or("").to_string();
    let (authority, path) = match gossamer_ws::parse_ws_url(&url) {
        Ok(parts) => parts,
        Err(e) => return Ok(err_variant(format!("websocket::connect: {e}"))),
    };
    match gossamer_runtime::sched_global::run_blocking("websocket-connect", move || {
        let mut stream = TcpStream::connect(&authority).map_err(|e| e.to_string())?;
        gossamer_ws::client_handshake(&mut stream, &authority, &path).map_err(|e| e.to_string())?;
        Ok::<_, String>(WebSocket::client(stream))
    }) {
        Ok(Ok(ws)) => Ok(ok_variant(Value::Int(register_conn(ws)))),
        Ok(Err(e)) | Err(e) => Ok(err_variant(format!("websocket::connect: {e}"))),
    }
}

/// `websocket::send_text(ws, s) -> Result<(), Error>`.
pub(crate) fn builtin_ws_send_text(args: &[Value]) -> RuntimeResult<Value> {
    let Some(h) = args.first().and_then(value_to_int) else {
        return Ok(err_variant("websocket::send_text: missing handle"));
    };
    let Some(conn) = conn_clone(h) else {
        return Ok(err_variant("websocket::send_text: stale handle"));
    };
    let text = args.get(1).and_then(as_str).unwrap_or("").to_string();
    match conn.lock().send_text(&text) {
        Ok(()) => Ok(ok_variant(Value::Unit)),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

/// `websocket::send_binary(ws, data: [u8]) -> Result<(), Error>`.
pub(crate) fn builtin_ws_send_binary(args: &[Value]) -> RuntimeResult<Value> {
    let Some(h) = args.first().and_then(value_to_int) else {
        return Ok(err_variant("websocket::send_binary: missing handle"));
    };
    let Some(conn) = conn_clone(h) else {
        return Ok(err_variant("websocket::send_binary: stale handle"));
    };
    let bytes: Vec<u8> = match args.get(1).and_then(Value::byte_slice) {
        Some(bytes) => bytes.into_owned(),
        None => {
            return Ok(err_variant(
                "websocket::send_binary: expected string or byte array",
            ));
        }
    };
    match conn.lock().send_binary(&bytes) {
        Ok(()) => Ok(ok_variant(Value::Unit)),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

/// `websocket::recv(ws) -> Result<String, Error>`. Returns the next text
/// message (binary frames surfaced UTF-8-lossy); answers ping/pong
/// control frames transparently. A peer close or I/O error is an `Err` -
/// the loop's exit signal.
pub(crate) fn builtin_ws_recv(args: &[Value]) -> RuntimeResult<Value> {
    let Some(h) = args.first().and_then(value_to_int) else {
        return Ok(err_variant("websocket::recv: missing handle"));
    };
    let Some(conn) = conn_clone(h) else {
        return Ok(err_variant("websocket::recv: stale handle"));
    };
    let mut ws = conn.lock();
    loop {
        match ws.receive() {
            Ok(Message::Text(s)) => return Ok(ok_variant(Value::String(s.into()))),
            Ok(Message::Binary(b)) => {
                let s = String::from_utf8_lossy(&b).into_owned();
                return Ok(ok_variant(Value::String(s.into())));
            }
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            Ok(Message::Close { code, reason }) => {
                return Ok(err_variant(format!("ws closed: {code} {reason}")));
            }
            Err(e) => return Ok(err_variant(format!("{e}"))),
        }
    }
}

/// `websocket::close(ws) -> Result<(), Error>`. Sends a normal close
/// frame (best effort) and unregisters the handle.
pub(crate) fn builtin_ws_close(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(h) = args.first().and_then(value_to_int) {
        if let Some(conn) = conn_clone(h) {
            let _ = conn.lock().send_close(1000, "");
        }
        unregister_conn(h);
    }
    Ok(ok_variant(Value::Unit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_connect_completes_a_real_upgrade() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("listener address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            gossamer_ws::server_accept(&mut stream).expect("server handshake");
        });

        let result = builtin_ws_connect(&[Value::String(format!("ws://{addr}/chat").into())])
            .expect("connect call");
        let Value::Variant(inner) = result else {
            panic!("expected Result variant");
        };
        assert_eq!(inner.name, "Ok");
        let handle = inner.fields[0].clone();
        builtin_ws_close(&[handle]).expect("close websocket");
        server.join().expect("server thread");
    }
}
