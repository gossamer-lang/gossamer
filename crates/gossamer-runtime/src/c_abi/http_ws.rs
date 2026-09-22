//! C-ABI dispatch shims for the bidirectional `std::http::websocket`
//! surface: `serve` / `connect` plus the handle ops `send_text` /
//! `send_binary` / `recv` / `close`. Mirrors the bytecode-VM builtins in
//! `gossamer-interp/src/stdlib_builtins/http_ws.rs` so the compiled
//! (Cranelift / LLVM) tier drives the same RFC 6455 framing engine
//! (`gossamer_ws::WebSocket`) natively instead of failing to link.
//!
//! Handle model - a process-global registry keyed by an `i64` handle,
//! the same shape `net_tcp.rs` uses for sockets. A handle is a plain
//! integer at the Gossamer level, so it crosses goroutine boundaries
//! freely. Each `WebSocket` is held behind `Arc<Mutex<_>>` so a blocking
//! `recv` releases the registry lock before parking on the socket: each
//! op clones the `Arc` under the registry lock, releases it, then locks
//! the connection for the (possibly blocking) frame read / write. The
//! send/recv loop on one connection is request/response-serialised, so
//! the per-connection lock never contends with itself.
//!
//! `WsStream` is plaintext `TcpStream`. TLS-terminated WebSockets
//! (`wss://`) are not yet wired through this registry; `connect`
//! rejects a `wss://` URL with an explicit error rather than silently
//! downgrading.

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::wildcard_imports)]

use std::collections::HashMap;
use std::net::TcpStream;
use std::os::raw::c_char;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::Mutex;

use gossamer_ws::{Message, WebSocket};

/// Underlying transport for a connected WebSocket. Plaintext only today;
/// the registry type widens to an enum when `wss://` lands.
type WsStream = TcpStream;

/// One registered connection. `Arc<Mutex<_>>` so a blocking `recv`
/// releases the registry lock before parking on the socket.
type WsConn = Arc<Mutex<WebSocket<WsStream>>>;

/// Process-global handle registry shared with every linked copy of the
/// runtime. `Option` so the `Mutex::new(None)` initialiser is const.
static WS_CONNS: Mutex<Option<HashMap<i64, WsConn>>> = Mutex::new(None);
static NEXT_WS_HANDLE: AtomicI64 = AtomicI64::new(1);

fn next_handle() -> i64 {
    NEXT_WS_HANDLE.fetch_add(1, Ordering::Relaxed)
}

fn cstr_to_str(p: *const c_char) -> String {
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

/// Packs `Err(errors::Error)` as the runtime's `i128` Result.
fn ws_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    super::vec::gos_rt_result_new(1, err as i64)
}

fn conn_clone(h: i64) -> Option<WsConn> {
    WS_CONNS.lock().as_ref().and_then(|m| m.get(&h).cloned())
}

fn register_conn(ws: WebSocket<WsStream>) -> i64 {
    let h = next_handle();
    WS_CONNS
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(h, Arc::new(Mutex::new(ws)));
    h
}

/// Wraps an accepted socket: completes the server handshake, registers
/// the connection, and dispatches the user handler `handler(env, handle)`
/// (a `fn handle(&self, ws: i64)` Gossamer method). The handler drives the
/// blocking recv/send loop and returns to close; the handle is then
/// unregistered. A handshake failure drops the socket without invoking
/// the handler.
fn serve_ws_conn(mut stream: TcpStream, env_addr: usize, fn_addr: usize) {
    if gossamer_ws::server_accept(&mut stream).is_err() {
        return;
    }
    let handle = register_conn(WebSocket::server(stream));
    // SAFETY: `fn_addr` came from `gos_fn_addr("T::handle")` at the user's
    // `websocket::serve(addr, app)` call site; `env_addr` is the `&app`
    // pointer passed alongside. A `fn handle(&self, ws: i64)` Gossamer
    // method lowers to a `void(ptr, i64)` C-ABI function.
    type WsHandlerFn = unsafe extern "C" fn(env: *mut u8, ws: i64);
    let handler: WsHandlerFn = unsafe { std::mem::transmute::<usize, WsHandlerFn>(fn_addr) };
    unsafe { handler(env_addr as *mut u8, handle) };
    if let Some(m) = WS_CONNS.lock().as_mut() {
        m.remove(&handle);
    }
}

/// `websocket::serve(addr, handler) -> Result<(), Error>`. Binds a TCP
/// listener, upgrades each connection to a WebSocket, and hands the connected
/// handle to the user handler on a bounded per-connection thread. It reuses
/// the HTTP server's admission and deadline configuration.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_ws_serve(
    addr: *const c_char,
    handler_env: *mut u8,
    handler_fn: i64,
) -> i128 {
    {
        let addr_s = if addr.is_null() {
            "0.0.0.0:8080".to_string()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(addr) }
        };
        let listener = match crate::listen::bind_tcp(&addr_s) {
            Ok(l) => l,
            Err(e) => return ws_err(&format!("websocket::serve: {e}")),
        };
        let env_addr = handler_env as usize;
        let fn_addr = handler_fn as usize;
        super::http_server::accept_serve(
            listener,
            super::http_server::ConnHome::Thread,
            move |stream| {
                serve_ws_conn(stream, env_addr, fn_addr);
            },
        );
    }
    super::vec::gos_rt_result_new(0, 0)
}

/// `websocket::connect(url) -> Result<i64, Error>`. Connects to a
/// `ws://host:port/path` endpoint, performs the client-side upgrade, and
/// returns the connected handle. `wss://` (TLS) is rejected explicitly -
/// the plaintext registry does not yet carry a TLS transport.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ws_serve_connect(url: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let url = cstr_to_str(url);
        match ws_client_connect(&url) {
            Ok(handle) => super::vec::gos_rt_result_new(0, handle),
            Err(e) => ws_err(&format!("websocket::connect: {e}")),
        }
    })
}

/// Performs the client TCP connect + RFC 6455 upgrade against `url`.
fn ws_client_connect(url: &str) -> Result<i64, String> {
    let (authority, path) = gossamer_ws::parse_ws_url(url).map_err(|e| format!("{e}"))?;
    let mut stream =
        TcpStream::connect(&authority).map_err(|e| format!("connect {authority}: {e}"))?;
    gossamer_ws::client_handshake(&mut stream, &authority, &path).map_err(|e| format!("{e}"))?;
    Ok(register_conn(WebSocket::client(stream)))
}

/// `websocket::send_text(ws, s) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ws_send_text(h: i64, s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let Some(conn) = conn_clone(h) else {
            return ws_err("send_text: stale handle");
        };
        let text = cstr_to_str(s);
        let mut ws = conn.lock();
        match ws.send_text(&text) {
            Ok(()) => super::vec::gos_rt_result_new(0, 0),
            Err(e) => ws_err(&format!("{e}")),
        }
    })
}

/// `websocket::send_binary(ws, data: [u8]) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ws_send_binary(h: i64, data: *const super::vec::GosVec) -> i128 {
    ffi_entry!(0i128, {
        let Some(conn) = conn_clone(h) else {
            return ws_err("send_binary: stale handle");
        };
        let bytes = unsafe { super::encoding::gosvec_u8(data) };
        let mut ws = conn.lock();
        match ws.send_binary(&bytes) {
            Ok(()) => super::vec::gos_rt_result_new(0, 0),
            Err(e) => ws_err(&format!("{e}")),
        }
    })
}

/// `websocket::recv(ws) -> Result<String, Error>`. Returns the next text
/// message (binary frames are surfaced UTF-8-lossy); transparently
/// answers ping/pong control frames. A peer close or an I/O error is an
/// `Err` - the loop's exit signal.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ws_recv(h: i64) -> i128 {
    ffi_entry!(0i128, {
        let Some(conn) = conn_clone(h) else {
            return ws_err("recv: stale handle");
        };
        let mut ws = conn.lock();
        loop {
            match ws.receive() {
                Ok(Message::Text(s)) => {
                    return super::vec::gos_rt_result_new(
                        0,
                        super::string::alloc_cstring(s.as_bytes()) as i64,
                    );
                }
                Ok(Message::Binary(b)) => {
                    let s = String::from_utf8_lossy(&b);
                    return super::vec::gos_rt_result_new(
                        0,
                        super::string::alloc_cstring(s.as_bytes()) as i64,
                    );
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => {}
                Ok(Message::Close { code, reason }) => {
                    return ws_err(&format!("ws closed: {code} {reason}"));
                }
                Err(e) => return ws_err(&format!("{e}")),
            }
        }
    })
}

/// `websocket::close(ws) -> Result<(), Error>`. Sends a normal close
/// frame (best effort) and unregisters the handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ws_close(h: i64) -> i128 {
    ffi_entry!(0i128, {
        if let Some(conn) = conn_clone(h) {
            let _ = conn.lock().send_close(1000, "");
        }
        if let Some(m) = WS_CONNS.lock().as_mut() {
            m.remove(&h);
        }
        super::vec::gos_rt_result_new(0, 0)
    })
}
