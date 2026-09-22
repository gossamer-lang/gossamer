#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    unsafe_op_in_unsafe_fn,
    unsafe_code,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::cognitive_complexity,
    clippy::unused_io_amount,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Wires up Gossamer-callable builtins for stdlib modules whose
//! Rust-side implementation already exists but had no user-facing
//! exposure. Each `install_*` helper is invoked from
//! `builtins::install` so user code that writes
//! `strings::join`, `strconv::parse_i64`, `net::TcpStream::connect`,
//! `time::Instant::now`, etc. resolves to a real callable.
//!
//! All builtins return a `Result`-shaped variant (`Ok` / `Err`) on
//! fallible operations so callers can chain `?` without wrapping.

use std::cell::RefCell;
use std::collections::HashMap as StdHashMap;
use std::io::Read as IoRead;
use std::sync::Arc;

use gossamer_ast::Ident;
use std::sync::atomic::{AtomicBool as StdAtomicBool, AtomicI64 as StdAtomicI64, Ordering};

use crate::value::SmolStr;

use gossamer_std::bufio as bufio_std;
use gossamer_std::math as math_std;
use gossamer_std::net as net_std;
use gossamer_std::os as os_std;
use gossamer_std::path as path_std;
use gossamer_std::strconv as strconv_std;
use gossamer_std::strings as strings_std;
use gossamer_std::unicode as unicode_std;
use gossamer_std::utf8 as utf8_std;

use gossamer_std::iter as iter_std;
use gossamer_std::utf16 as utf16_std;

use crate::builtins::{
    BuiltinFnPub, as_str, err_variant, install_module_pub, none_variant, ok_variant, some_variant,
    value_to_int,
};
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeError, RuntimeResult, Value};

/// Entry point invoked from `builtins::install`.
use super::*;

/// Payload bytes for a socket write. Every byte-sequence representation is
/// accepted: a `Vec<u8>` and a `[u8; N]` reach the runtime as one of the
/// packed forms rather than a boxed `Array`, and a `String` writes its
/// UTF-8. Anything else is not a byte sequence and answers `None` so the
/// caller reports it.
fn socket_write_bytes(arg: Option<&Value>) -> Option<Vec<u8>> {
    match arg? {
        Value::String(_)
        | Value::Array(_)
        | Value::IntArray(_)
        | Value::ByteArray(_)
        | Value::InlineByteArray(_)
        | Value::ByteVec(_) => Some(crate::stdlib_builtins::crypto::value_to_bytes(arg?)),
        _ => None,
    }
}

pub(crate) fn install_net(globals: &mut Vec<(&'static str, Value)>) {
    install_module_pub("net", &[("lookup", builtin_net_resolve)], globals);
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("TcpListener::bind", builtin_tcp_listener_bind),
        ("TcpListener::accept", builtin_tcp_listener_accept),
        ("TcpListener::local_addr", builtin_tcp_listener_local_addr),
        ("TcpListener::close", builtin_tcp_listener_close),
        ("TcpStream::connect", builtin_tcp_stream_connect),
        ("TcpStream::read", builtin_tcp_stream_read),
        (
            "TcpStream::read_to_string",
            builtin_tcp_stream_read_to_string,
        ),
        ("TcpStream::write", builtin_tcp_stream_write),
        ("TcpStream::write_all", builtin_tcp_stream_write),
        ("TcpStream::start_tls", builtin_tcp_stream_start_tls),
        (
            "TcpStream::start_tls_insecure",
            builtin_tcp_stream_start_tls_insecure,
        ),
        ("TcpStream::start_tls_ca", builtin_tcp_stream_start_tls_ca),
        (
            "TcpStream::peer_certificate",
            builtin_tcp_stream_peer_certificate,
        ),
        (
            "TcpStream::set_read_timeout_ms",
            builtin_tcp_stream_set_read_timeout_ms,
        ),
        (
            "TcpStream::set_write_timeout_ms",
            builtin_tcp_stream_set_write_timeout_ms,
        ),
        ("TcpStream::set_nodelay", builtin_tcp_stream_set_nodelay),
        ("TcpStream::read_into", builtin_tcp_stream_read_into),
        (
            "TcpStream::clear_read_timeout",
            builtin_tcp_stream_clear_read_timeout,
        ),
        (
            "TcpStream::clear_write_timeout",
            builtin_tcp_stream_clear_write_timeout,
        ),
        ("TcpStream::close", builtin_tcp_stream_close),
        ("UdpSocket::bind", builtin_udp_bind),
        ("UdpSocket::send_to", builtin_udp_send_to),
        ("UdpSocket::recv_from", builtin_udp_recv_from),
        ("UdpSocket::local_addr", builtin_udp_local_addr),
        ("UdpSocket::close", builtin_udp_close),
        ("UnixListener::bind", builtin_unix_listener_bind),
        ("UnixListener::accept", builtin_unix_listener_accept),
        ("UnixListener::close", builtin_unix_listener_close),
        ("UnixStream::connect", builtin_unix_stream_connect),
        ("UnixStream::read", builtin_unix_stream_read),
        (
            "UnixStream::read_to_string",
            builtin_unix_stream_read_to_string,
        ),
        ("UnixStream::write", builtin_unix_stream_write),
        ("UnixStream::write_all", builtin_unix_stream_write),
        ("UnixStream::close", builtin_unix_stream_close),
    ];
    for (short, call) in entries {
        let qualified: &'static str = Box::leak(format!("net::{short}").into_boxed_str());
        globals.push((qualified, crate::builtins::builtin_pub(qualified, *call)));
        globals.push((*short, crate::builtins::builtin_pub(short, *call)));
    }
    // Bare-name dispatch for method-call shape (`stream.read(n)`).
    for (short, call) in &[
        ("recv_from", builtin_udp_recv_from as BuiltinFnPub),
        ("send_to", builtin_udp_send_to as BuiltinFnPub),
        ("accept", builtin_tcp_listener_accept as BuiltinFnPub),
        (
            "local_addr",
            builtin_tcp_listener_local_addr as BuiltinFnPub,
        ),
    ] {
        globals.push((*short, crate::builtins::builtin_pub(short, *call)));
    }
}

pub(crate) fn builtin_net_resolve(args: &[Value]) -> RuntimeResult<Value> {
    let host = args.first().and_then(as_str).unwrap_or("");
    let needle = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:0")
    };
    match net_std::resolve(&needle) {
        Ok(addrs) => {
            let values: Vec<Value> = addrs
                .into_iter()
                .map(|a| Value::String(a.ip().to_string().into()))
                .collect();
            Ok(ok_variant(Value::Array(Arc::new(values))))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_tcp_listener_bind(args: &[Value]) -> RuntimeResult<Value> {
    let addr = match arg_str_at(args, 0, "TcpListener::bind", "address") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    match net_std::TcpListener::bind(&addr) {
        Ok(listener) => {
            let id = next_net_id();
            TCP_LISTENER_REGISTRY.with(|r| {
                r.borrow_mut()
                    .insert(id, Arc::new(parking_lot::Mutex::new(listener)));
            });
            Ok(ok_variant(handle_struct("net::TcpListener", id)))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_tcp_listener_accept(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("TcpListener::accept: missing handle"));
    };
    let Some(listener) = fetch_socket(&TCP_LISTENER_REGISTRY, id) else {
        return Ok(err_variant("TcpListener::accept: stale handle"));
    };
    // Accept on a clone so the handle is not locked while the goroutine is
    // parked waiting for a connection; another goroutine may use it meanwhile.
    let cloned = listener.lock().try_clone();
    let res = cloned
        .and_then(|mut l| l.accept())
        .map_err(|e| e.to_string());
    match res {
        Ok((stream, addr)) => {
            // Nagle off, as Go leaves a `TCPConn`: a request/response protocol
            // writes a reply as a few small writes, and holding the later ones
            // until the peer acknowledges the first pairs with the peer's
            // delayed acknowledgement to cost a ~40 ms stall per exchange.
            // `set_nodelay` turns it back on.
            let _ = stream.set_nodelay(true);
            let sid = next_net_id();
            TCP_STREAM_REGISTRY.with(|r| {
                r.borrow_mut()
                    .insert(sid, Arc::new(parking_lot::Mutex::new(Some(stream))));
            });
            let pair = Value::Tuple(Arc::from(vec![
                handle_struct("net::TcpStream", sid),
                Value::String(addr.to_string().into()),
            ]));
            Ok(ok_variant(pair))
        }
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_tcp_listener_local_addr(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("TcpListener::local_addr: missing handle"));
    };
    let Some(listener) = fetch_socket(&TCP_LISTENER_REGISTRY, id) else {
        return Ok(err_variant("TcpListener::local_addr: stale handle"));
    };
    match listener.lock().local_addr() {
        Ok(addr) => Ok(ok_variant(Value::String(addr.to_string().into()))),
        Err(e) => Ok(err_variant(e.to_string())),
    }
}

pub(crate) fn builtin_tcp_listener_close(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(id) = args.first().and_then(handle_id) {
        TCP_LISTENER_REGISTRY.with(|r| {
            r.borrow_mut().remove(&id);
        });
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_tcp_stream_connect(args: &[Value]) -> RuntimeResult<Value> {
    let addr = match arg_str_at(args, 0, "TcpStream::connect", "address") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    match gossamer_runtime::sched_global::run_blocking("tcp-connect", move || {
        net_std::TcpStream::connect(&addr)
    }) {
        Ok(Ok(stream)) => {
            // Nagle off by default, matching an accepted stream and Go's own
            // `TCPConn`; see `builtin_tcp_listener_accept`.
            let _ = stream.set_nodelay(true);
            let id = next_net_id();
            TCP_STREAM_REGISTRY.with(|r| {
                r.borrow_mut()
                    .insert(id, Arc::new(parking_lot::Mutex::new(Some(stream))));
            });
            Ok(ok_variant(handle_struct("net::TcpStream", id)))
        }
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_tcp_stream_read(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("TcpStream::read: missing handle"));
    };
    let max = match args.get(1).and_then(value_to_int) {
        Some(n) if n <= 0 => {
            return Err(RuntimeError::Type(
                "TcpStream::read: size must be positive".to_string(),
            ));
        }
        Some(n) => n.min(1 << 24),
        None => 4096,
    };
    let res = if tls_has(id) {
        match fetch_socket(&TLS_STREAM_REGISTRY, id) {
            Some(arc) => {
                match gossamer_runtime::sched_global::run_blocking("tls-stream-read", move || {
                    let mut buf = vec![0u8; max as usize];
                    match arc.lock().read(&mut buf) {
                        Ok(n) => {
                            buf.truncate(n);
                            Ok(buf)
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }) {
                    Ok(result) => result,
                    Err(e) => Err(e),
                }
            }
            None => Err("TcpStream::read: stale handle".to_string()),
        }
    } else {
        match clone_tcp_stream(id) {
            Ok(mut stream) => {
                match gossamer_runtime::sched_global::run_blocking("tcp-stream-read", move || {
                    let mut buf = vec![0u8; max as usize];
                    match stream.read(&mut buf) {
                        Ok(n) => {
                            buf.truncate(n);
                            Ok(buf)
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }) {
                    Ok(result) => result,
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    };
    match res {
        Ok(bytes) => Ok(ok_variant(Value::Array(Arc::new(
            bytes
                .into_iter()
                .map(|b| Value::Int(i64::from(b)))
                .collect(),
        )))),
        Err(e) => Ok(err_variant(e)),
    }
}

/// `net::TcpStream::read_into(buf, max)`. Reads one chunk of at most `max`
/// bytes and replaces the buffer's contents with it, answering the byte count.
/// `Ok(0)` is the peer's clean close.
pub(crate) fn builtin_tcp_stream_read_into(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("TcpStream::read_into: missing handle"));
    };
    let Some(Value::MutCell(cell)) = args.get(1) else {
        return Ok(err_variant(
            "TcpStream::read_into: expected &mut Vec<u8> buffer",
        ));
    };
    let max = match args.get(2).and_then(value_to_int) {
        Some(n) if n <= 0 => {
            return Err(RuntimeError::Type(
                "TcpStream::read_into: size must be positive".to_string(),
            ));
        }
        Some(n) => n.min(1 << 24),
        None => 4096,
    };
    let res = if tls_has(id) {
        match fetch_socket(&TLS_STREAM_REGISTRY, id) {
            Some(arc) => {
                match gossamer_runtime::sched_global::run_blocking(
                    "tls-stream-read-into",
                    move || {
                        let mut buf = vec![0u8; max as usize];
                        match arc.lock().read(&mut buf) {
                            Ok(n) => {
                                buf.truncate(n);
                                Ok(buf)
                            }
                            Err(e) => Err(e.to_string()),
                        }
                    },
                ) {
                    Ok(result) => result,
                    Err(e) => Err(e),
                }
            }
            None => Err("TcpStream::read_into: stale handle".to_string()),
        }
    } else {
        match clone_tcp_stream(id) {
            Ok(mut stream) => {
                match gossamer_runtime::sched_global::run_blocking(
                    "tcp-stream-read-into",
                    move || {
                        let mut buf = vec![0u8; max as usize];
                        match stream.read(&mut buf) {
                            Ok(n) => {
                                buf.truncate(n);
                                Ok(buf)
                            }
                            Err(e) => Err(e.to_string()),
                        }
                    },
                ) {
                    Ok(result) => result,
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    };
    match res {
        Ok(bytes) => {
            let n = bytes.len() as i64;
            *cell.lock() = Value::Array(Arc::new(
                bytes
                    .into_iter()
                    .map(|b| Value::Int(i64::from(b)))
                    .collect(),
            ));
            Ok(ok_variant(Value::Int(n)))
        }
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_tcp_stream_read_to_string(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("TcpStream::read_to_string: missing handle"));
    };
    let res = if tls_has(id) {
        match fetch_socket(&TLS_STREAM_REGISTRY, id) {
            Some(arc) => match gossamer_runtime::sched_global::run_blocking(
                "tls-stream-read-string",
                move || {
                    let mut guard = arc.lock();
                    read_all_to_string(|buf| guard.read(buf).map_err(|e| e.to_string()))
                },
            ) {
                Ok(result) => result,
                Err(e) => Err(e),
            },
            None => Err("TcpStream::read_to_string: stale handle".to_string()),
        }
    } else {
        match clone_tcp_stream(id) {
            Ok(mut stream) => match gossamer_runtime::sched_global::run_blocking(
                "tcp-stream-read-string",
                move || read_all_to_string(|buf| stream.read(buf).map_err(|e| e.to_string())),
            ) {
                Ok(result) => result,
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        }
    };
    match res {
        Ok(s) => Ok(ok_variant(Value::String(s.into()))),
        Err(e) => Ok(err_variant(e)),
    }
}

fn read_all_to_string(
    mut read: impl FnMut(&mut [u8]) -> Result<usize, String>,
) -> Result<String, String> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match read(&mut chunk) {
            Ok(0) => break Ok(String::from_utf8_lossy(&out).into_owned()),
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(e) => break Err(e.clone()),
        }
    }
}

pub(crate) fn builtin_tcp_stream_write(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("TcpStream::write: missing handle"));
    };
    let Some(bytes) = socket_write_bytes(args.get(1)) else {
        return Ok(err_variant(
            "TcpStream::write: expected string or byte array",
        ));
    };
    let bytes_len = bytes.len() as i64;
    let res = if tls_has(id) {
        match fetch_socket(&TLS_STREAM_REGISTRY, id) {
            Some(arc) => {
                match gossamer_runtime::sched_global::run_blocking("tls-stream-write", move || {
                    arc.lock().write_all(&bytes).map_err(|e| e.to_string())
                }) {
                    Ok(result) => result,
                    Err(e) => Err(e),
                }
            }
            None => Err("TcpStream::write: stale handle".to_string()),
        }
    } else {
        match clone_tcp_stream(id) {
            Ok(mut stream) => {
                match gossamer_runtime::sched_global::run_blocking("tcp-stream-write", move || {
                    stream.write_all(&bytes).map_err(|e| e.to_string())
                }) {
                    Ok(result) => result,
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    };
    match res {
        Ok(()) => Ok(ok_variant(Value::Int(bytes_len))),
        Err(e) => Ok(err_variant(e)),
    }
}

/// Duplicate a plaintext TCP descriptor while holding its registry/object lock
/// only briefly. The duplicate shares the connection, but can block on a
/// blocking-pool thread without pinning a scheduler worker or its registry.
fn clone_tcp_stream(id: i64) -> Result<net_std::TcpStream, String> {
    match fetch_socket(&TCP_STREAM_REGISTRY, id) {
        Some(arc) => match arc.lock().as_ref() {
            Some(stream) => stream.try_clone().map_err(|e| e.to_string()),
            None => Err("TcpStream: closed handle".to_string()),
        },
        None => Err("TcpStream: stale handle".to_string()),
    }
}

fn tcp_timeout_duration(ms: i64) -> Option<std::time::Duration> {
    (ms > 0).then(|| std::time::Duration::from_millis(ms as u64))
}

fn set_tcp_stream_timeout(id: i64, ms: i64, read: bool) -> Result<(), String> {
    let timeout = tcp_timeout_duration(ms);
    if tls_has(id) {
        return match fetch_socket(&TLS_STREAM_REGISTRY, id) {
            Some(arc) => {
                let guard = arc.lock();
                let res = if read {
                    guard.set_read_timeout(timeout)
                } else {
                    guard.set_write_timeout(timeout)
                };
                res.map_err(|e| e.to_string())
            }
            None => Err("TcpStream::timeout: stale handle".to_string()),
        };
    }
    match fetch_socket(&TCP_STREAM_REGISTRY, id) {
        Some(arc) => {
            let guard = arc.lock();
            match guard.as_ref() {
                Some(stream) => {
                    let res = if read {
                        stream.set_read_timeout(timeout)
                    } else {
                        stream.set_write_timeout(timeout)
                    };
                    res.map_err(|e| e.to_string())
                }
                None => Err("TcpStream::timeout: closed handle".to_string()),
            }
        }
        None => Err("TcpStream::timeout: stale handle".to_string()),
    }
}

fn tcp_stream_timeout_builtin(args: &[Value], read: bool, clear: bool) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("TcpStream::timeout: missing handle"));
    };
    let ms = if clear {
        0
    } else {
        let ms = args.get(1).and_then(value_to_int).unwrap_or(0);
        if ms < 0 {
            return Err(RuntimeError::Type(
                "TcpStream::timeout: timeout_ms must be non-negative".to_string(),
            ));
        }
        ms
    };
    match set_tcp_stream_timeout(id, ms, read) {
        Ok(()) => Ok(ok_variant(Value::Unit)),
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_tcp_stream_set_read_timeout_ms(args: &[Value]) -> RuntimeResult<Value> {
    tcp_stream_timeout_builtin(args, true, false)
}

pub(crate) fn builtin_tcp_stream_set_write_timeout_ms(args: &[Value]) -> RuntimeResult<Value> {
    tcp_stream_timeout_builtin(args, false, false)
}

/// `net::TcpStream::set_nodelay(on)`. A stream arrives with Nagle already off,
/// so this is for the caller that wants it back on.
pub(crate) fn builtin_tcp_stream_set_nodelay(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("TcpStream::set_nodelay: missing handle"));
    };
    let on = matches!(args.get(1), Some(Value::Bool(true)));
    match set_tcp_stream_nodelay(id, on) {
        Ok(()) => Ok(ok_variant(Value::Unit)),
        Err(e) => Ok(err_variant(e)),
    }
}

fn set_tcp_stream_nodelay(id: i64, on: bool) -> Result<(), String> {
    if tls_has(id) {
        return match fetch_socket(&TLS_STREAM_REGISTRY, id) {
            Some(arc) => arc.lock().set_nodelay(on).map_err(|e| e.to_string()),
            None => Err("TcpStream::set_nodelay: stale handle".to_string()),
        };
    }
    match fetch_socket(&TCP_STREAM_REGISTRY, id) {
        Some(arc) => match arc.lock().as_ref() {
            Some(stream) => stream.set_nodelay(on).map_err(|e| e.to_string()),
            None => Err("TcpStream::set_nodelay: closed handle".to_string()),
        },
        None => Err("TcpStream::set_nodelay: stale handle".to_string()),
    }
}

pub(crate) fn builtin_tcp_stream_clear_read_timeout(args: &[Value]) -> RuntimeResult<Value> {
    tcp_stream_timeout_builtin(args, true, true)
}

pub(crate) fn builtin_tcp_stream_clear_write_timeout(args: &[Value]) -> RuntimeResult<Value> {
    tcp_stream_timeout_builtin(args, false, true)
}

#[cfg(test)]
mod tcp_timeout_tests {
    use super::*;

    fn ok_payload(value: Value) -> Value {
        match value {
            Value::Variant(inner) if inner.name.as_str() == "Ok" => inner
                .fields
                .first()
                .cloned()
                .expect("Ok payload should exist"),
            other => panic!("expected Ok variant, got {other:?}"),
        }
    }

    #[test]
    fn tcp_stream_timeout_builtins_accept_real_stream_handle() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let accept_thread = std::thread::spawn(move || {
            let _ = listener.accept();
        });

        let stream = ok_payload(
            builtin_tcp_stream_connect(&[Value::String(addr.to_string().into())]).expect("connect"),
        );
        ok_payload(
            builtin_tcp_stream_set_read_timeout_ms(&[stream.clone(), Value::Int(10)])
                .expect("set read timeout"),
        );
        ok_payload(
            builtin_tcp_stream_set_write_timeout_ms(&[stream.clone(), Value::Int(10)])
                .expect("set write timeout"),
        );
        ok_payload(
            builtin_tcp_stream_clear_read_timeout(std::slice::from_ref(&stream))
                .expect("clear read timeout"),
        );
        ok_payload(
            builtin_tcp_stream_clear_write_timeout(std::slice::from_ref(&stream))
                .expect("clear write timeout"),
        );
        assert!(matches!(
            builtin_tcp_stream_close(&[stream]).expect("close"),
            Value::Unit
        ));
        accept_thread.join().expect("accept thread");
    }
}

// --- Unix-domain sockets ------------------------------------------
//
// AF_UNIX stream sockets. The real implementation is `#[cfg(unix)]`;
// on non-unix targets every entry point returns an `Err`, matching the
// compiled tier's Windows stub.

#[cfg(unix)]
pub(crate) fn builtin_unix_listener_bind(args: &[Value]) -> RuntimeResult<Value> {
    let path = match arg_str_at(args, 0, "UnixListener::bind", "path") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    match std::os::unix::net::UnixListener::bind(&path) {
        Ok(l) => {
            let id = next_net_id();
            UNIX_LISTENER_REGISTRY.with(|r| {
                r.borrow_mut()
                    .insert(id, Arc::new(parking_lot::Mutex::new(l)));
            });
            Ok(ok_variant(handle_struct("net::UnixListener", id)))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

#[cfg(unix)]
pub(crate) fn builtin_unix_listener_accept(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("UnixListener::accept: missing handle"));
    };
    let Some(listener) = fetch_socket(&UNIX_LISTENER_REGISTRY, id) else {
        return Ok(err_variant("UnixListener::accept: stale handle"));
    };
    let listener = match listener.lock().try_clone() {
        Ok(listener) => listener,
        Err(e) => return Ok(err_variant(e.to_string())),
    };
    match gossamer_runtime::sched_global::run_blocking("unix-accept", move || listener.accept()) {
        Ok(Ok((stream, addr))) => {
            let sid = next_net_id();
            UNIX_STREAM_REGISTRY.with(|r| {
                r.borrow_mut()
                    .insert(sid, Arc::new(parking_lot::Mutex::new(stream)));
            });
            let addr_str = addr
                .as_pathname()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let pair = Value::Tuple(Arc::from(vec![
                handle_struct("net::UnixStream", sid),
                Value::String(addr_str.into()),
            ]));
            Ok(ok_variant(pair))
        }
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

#[cfg(unix)]
pub(crate) fn builtin_unix_listener_close(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(id) = args.first().and_then(handle_id) {
        UNIX_LISTENER_REGISTRY.with(|r| {
            r.borrow_mut().remove(&id);
        });
    }
    Ok(Value::Unit)
}

#[cfg(unix)]
pub(crate) fn builtin_unix_stream_connect(args: &[Value]) -> RuntimeResult<Value> {
    let path = match arg_str_at(args, 0, "UnixStream::connect", "path") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    match gossamer_runtime::sched_global::run_blocking("unix-connect", move || {
        std::os::unix::net::UnixStream::connect(&path)
    }) {
        Ok(Ok(s)) => {
            let id = next_net_id();
            UNIX_STREAM_REGISTRY.with(|r| {
                r.borrow_mut()
                    .insert(id, Arc::new(parking_lot::Mutex::new(s)));
            });
            Ok(ok_variant(handle_struct("net::UnixStream", id)))
        }
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

#[cfg(unix)]
pub(crate) fn builtin_unix_stream_read(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("UnixStream::read: missing handle"));
    };
    let max = match args.get(1).and_then(value_to_int) {
        Some(n) if n <= 0 => {
            return Err(RuntimeError::Type(
                "UnixStream::read: size must be positive".to_string(),
            ));
        }
        Some(n) => n.min(1 << 24),
        None => 4096,
    };
    let res = match clone_unix_stream(id) {
        Ok(mut stream) => {
            match gossamer_runtime::sched_global::run_blocking("unix-stream-read", move || {
                let mut buf = vec![0u8; max as usize];
                match stream.read(&mut buf) {
                    Ok(n) => {
                        buf.truncate(n);
                        Ok(buf)
                    }
                    Err(e) => Err(e.to_string()),
                }
            }) {
                Ok(result) => result,
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    };
    match res {
        Ok(bytes) => Ok(ok_variant(Value::Array(Arc::new(
            bytes
                .into_iter()
                .map(|b| Value::Int(i64::from(b)))
                .collect(),
        )))),
        Err(e) => Ok(err_variant(e)),
    }
}

#[cfg(unix)]
pub(crate) fn builtin_unix_stream_read_to_string(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("UnixStream::read_to_string: missing handle"));
    };
    let res = match clone_unix_stream(id) {
        Ok(mut stream) => match gossamer_runtime::sched_global::run_blocking(
            "unix-stream-read-string",
            move || read_all_to_string(|buf| stream.read(buf).map_err(|e| e.to_string())),
        ) {
            Ok(result) => result,
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };
    match res {
        Ok(s) => Ok(ok_variant(Value::String(s.into()))),
        Err(e) => Ok(err_variant(e)),
    }
}

#[cfg(unix)]
pub(crate) fn builtin_unix_stream_write(args: &[Value]) -> RuntimeResult<Value> {
    use std::io::Write as _;
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("UnixStream::write: missing handle"));
    };
    let Some(bytes) = socket_write_bytes(args.get(1)) else {
        return Ok(err_variant(
            "UnixStream::write: expected string or byte array",
        ));
    };
    let bytes_len = bytes.len() as i64;
    let res = match clone_unix_stream(id) {
        Ok(mut stream) => {
            match gossamer_runtime::sched_global::run_blocking("unix-stream-write", move || {
                stream.write_all(&bytes).map_err(|e| e.to_string())
            }) {
                Ok(result) => result,
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    };
    match res {
        Ok(()) => Ok(ok_variant(Value::Int(bytes_len))),
        Err(e) => Ok(err_variant(e)),
    }
}

#[cfg(unix)]
fn clone_unix_stream(id: i64) -> Result<std::os::unix::net::UnixStream, String> {
    fetch_socket(&UNIX_STREAM_REGISTRY, id)
        .ok_or_else(|| "UnixStream: stale handle".to_string())?
        .lock()
        .try_clone()
        .map_err(|e| e.to_string())
}

#[cfg(unix)]
pub(crate) fn builtin_unix_stream_close(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(id) = args.first().and_then(handle_id) {
        UNIX_STREAM_REGISTRY.with(|r| {
            r.borrow_mut().remove(&id);
        });
    }
    Ok(Value::Unit)
}

#[cfg(not(unix))]
fn unix_unsupported(op: &str) -> RuntimeResult<Value> {
    Ok(err_variant(format!(
        "net::{op}: Unix-domain sockets are not supported on this platform"
    )))
}

#[cfg(not(unix))]
pub(crate) fn builtin_unix_listener_bind(_args: &[Value]) -> RuntimeResult<Value> {
    unix_unsupported("UnixListener::bind")
}
#[cfg(not(unix))]
pub(crate) fn builtin_unix_listener_accept(_args: &[Value]) -> RuntimeResult<Value> {
    unix_unsupported("UnixListener::accept")
}
#[cfg(not(unix))]
pub(crate) fn builtin_unix_listener_close(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Unit)
}
#[cfg(not(unix))]
pub(crate) fn builtin_unix_stream_connect(_args: &[Value]) -> RuntimeResult<Value> {
    unix_unsupported("UnixStream::connect")
}
#[cfg(not(unix))]
pub(crate) fn builtin_unix_stream_read(_args: &[Value]) -> RuntimeResult<Value> {
    unix_unsupported("UnixStream::read")
}
#[cfg(not(unix))]
pub(crate) fn builtin_unix_stream_read_to_string(_args: &[Value]) -> RuntimeResult<Value> {
    unix_unsupported("UnixStream::read_to_string")
}
#[cfg(not(unix))]
pub(crate) fn builtin_unix_stream_write(_args: &[Value]) -> RuntimeResult<Value> {
    unix_unsupported("UnixStream::write")
}
#[cfg(not(unix))]
pub(crate) fn builtin_unix_stream_close(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Unit)
}

/// True when `id` names an upgraded TLS stream rather than a plaintext
/// socket. `read` / `write` / `close` consult this so the existing
/// `net::TcpStream` method surface drives either transport.
fn tls_has(id: i64) -> bool {
    TLS_STREAM_REGISTRY.with(|r| r.borrow().contains_key(&id))
}

/// `stream.peer_certificate()` - the DER bytes of the peer's end-entity
/// certificate, empty when the stream is not TLS or the peer sent none.
pub(crate) fn builtin_tcp_stream_peer_certificate(args: &[Value]) -> RuntimeResult<Value> {
    let der = args
        .first()
        .and_then(handle_id)
        .and_then(|id| {
            TLS_STREAM_REGISTRY
                .with(|r| r.borrow().get(&id).map(|tls| tls.lock().peer_certificate()))
        })
        .unwrap_or_default();
    Ok(Value::ByteVec(Arc::new(der)))
}

pub(crate) fn builtin_tcp_stream_start_tls(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("TcpStream::start_tls: missing handle"));
    };
    let host = match arg_str_at(args, 1, "TcpStream::start_tls", "host") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let Some(stream) = take_tcp_stream(id) else {
        return Ok(err_variant("TcpStream::start_tls: stale handle"));
    };
    match stream.start_tls(&host) {
        Ok(tls) => {
            let nid = next_net_id();
            TLS_STREAM_REGISTRY.with(|r| {
                r.borrow_mut()
                    .insert(nid, Arc::new(parking_lot::Mutex::new(tls)));
            });
            Ok(ok_variant(handle_struct("net::TcpStream", nid)))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

/// Removes the plaintext stream behind `id` and moves the inner
/// `TcpStream` out by value, so a TLS upgrade's blocking handshake runs
/// with no registry lock and no per-socket lock held.
fn take_tcp_stream(id: i64) -> Option<net_std::TcpStream> {
    let arc = TCP_STREAM_REGISTRY.with(|r| r.borrow_mut().remove(&id))?;
    arc.lock().take()
}

pub(crate) fn builtin_tcp_stream_start_tls_insecure(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("TcpStream::start_tls_insecure: missing handle"));
    };
    let host = match arg_str_at(args, 1, "TcpStream::start_tls_insecure", "host") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let Some(stream) = take_tcp_stream(id) else {
        return Ok(err_variant("TcpStream::start_tls_insecure: stale handle"));
    };
    match stream.start_tls_insecure(&host) {
        Ok(tls) => {
            let nid = next_net_id();
            TLS_STREAM_REGISTRY.with(|r| {
                r.borrow_mut()
                    .insert(nid, Arc::new(parking_lot::Mutex::new(tls)));
            });
            Ok(ok_variant(handle_struct("net::TcpStream", nid)))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_tcp_stream_start_tls_ca(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("TcpStream::start_tls_ca: missing handle"));
    };
    let host = match arg_str_at(args, 1, "TcpStream::start_tls_ca", "host") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let ca_pem = match arg_str_at(args, 2, "TcpStream::start_tls_ca", "ca_pem") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let Some(stream) = take_tcp_stream(id) else {
        return Ok(err_variant("TcpStream::start_tls_ca: stale handle"));
    };
    match stream.start_tls_ca(&host, &ca_pem) {
        Ok(tls) => {
            let nid = next_net_id();
            TLS_STREAM_REGISTRY.with(|r| {
                r.borrow_mut()
                    .insert(nid, Arc::new(parking_lot::Mutex::new(tls)));
            });
            Ok(ok_variant(handle_struct("net::TcpStream", nid)))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_tcp_stream_close(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(id) = args.first().and_then(handle_id) {
        TCP_STREAM_REGISTRY.with(|r| {
            r.borrow_mut().remove(&id);
        });
        TLS_STREAM_REGISTRY.with(|r| {
            r.borrow_mut().remove(&id);
        });
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_udp_bind(args: &[Value]) -> RuntimeResult<Value> {
    let addr = match arg_str_at(args, 0, "UdpSocket::bind", "address") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    match net_std::UdpSocket::bind(&addr) {
        Ok(sock) => {
            let id = next_net_id();
            UDP_REGISTRY.with(|r| {
                r.borrow_mut()
                    .insert(id, Arc::new(parking_lot::Mutex::new(sock)));
            });
            Ok(ok_variant(handle_struct("net::UdpSocket", id)))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_udp_send_to(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("UdpSocket::send_to: missing handle"));
    };
    let Some(bytes) = socket_write_bytes(args.get(1)) else {
        return Ok(err_variant(
            "UdpSocket::send_to: expected string or byte array",
        ));
    };
    let addr = args.get(2).and_then(as_str).unwrap_or("").to_string();
    let res = match fetch_socket(&UDP_REGISTRY, id) {
        Some(arc) => arc.lock().send_to(&bytes, &addr).map_err(|e| e.to_string()),
        None => Err("UdpSocket::send_to: stale handle".to_string()),
    };
    match res {
        Ok(n) => Ok(ok_variant(Value::Int(n as i64))),
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_udp_recv_from(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("UdpSocket::recv_from: missing handle"));
    };
    let max = match args.get(1).and_then(value_to_int) {
        Some(n) if n <= 0 => {
            return Err(RuntimeError::Type(
                "UdpSocket::recv_from: size must be positive".to_string(),
            ));
        }
        Some(n) => n.min(1 << 16),
        None => 1500,
    };
    let res = match fetch_socket(&UDP_REGISTRY, id) {
        Some(arc) => {
            let mut buf = vec![0u8; max as usize];
            match arc.lock().recv_from(&mut buf) {
                Ok((n, addr)) => {
                    buf.truncate(n);
                    Ok((buf, addr))
                }
                Err(e) => Err(e.to_string()),
            }
        }
        None => Err("UdpSocket::recv_from: stale handle".to_string()),
    };
    match res {
        Ok((bytes, addr)) => {
            let bytes_v = Value::Array(Arc::new(
                bytes
                    .into_iter()
                    .map(|b| Value::Int(i64::from(b)))
                    .collect(),
            ));
            Ok(ok_variant(Value::Tuple(Arc::from(vec![
                bytes_v,
                Value::String(addr.to_string().into()),
            ]))))
        }
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_udp_local_addr(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("UdpSocket::local_addr: missing handle"));
    };
    let Some(arc) = fetch_socket(&UDP_REGISTRY, id) else {
        return Ok(err_variant("UdpSocket::local_addr: stale handle"));
    };
    match arc.lock().local_addr() {
        Ok(addr) => Ok(ok_variant(Value::String(addr.to_string().into()))),
        Err(e) => Ok(err_variant(e.to_string())),
    }
}

pub(crate) fn builtin_udp_close(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(id) = args.first().and_then(handle_id) {
        UDP_REGISTRY.with(|r| {
            r.borrow_mut().remove(&id);
        });
    }
    Ok(Value::Unit)
}

#[cfg(test)]
mod net_registry_tests {
    use super::*;
    use std::thread;

    fn unwrap_ok(v: Value) -> Value {
        match v {
            Value::Variant(inner) if inner.name == "Ok" => inner.fields[0].clone(),
            other => panic!("expected Ok variant, got {other:?}"),
        }
    }

    fn str_of(v: &Value) -> String {
        match v {
            Value::String(s) => s.to_string(),
            other => panic!("expected String, got {other:?}"),
        }
    }

    fn bytes_of(v: &Value) -> Vec<u8> {
        match v {
            Value::Array(arr) => arr
                .iter()
                .map(|x| match x {
                    Value::Int(n) => *n as u8,
                    other => panic!("expected Int byte, got {other:?}"),
                })
                .collect(),
            other => panic!("expected Array, got {other:?}"),
        }
    }

    fn byte_array(bytes: &[u8]) -> Value {
        Value::Array(Arc::new(
            bytes.iter().map(|b| Value::Int(i64::from(*b))).collect(),
        ))
    }

    // Reads from `stream` until `want` bytes arrive or the peer closes
    // (an empty read = EOF). `read` busy-polls inside `net_std` until
    // data is ready, so this blocks on readiness, not a fixed sleep.
    fn read_exact(stream: &Value, want: usize) -> Vec<u8> {
        let mut got = Vec::new();
        while got.len() < want {
            let chunk = bytes_of(&unwrap_ok(
                builtin_tcp_stream_read(&[stream.clone(), Value::Int(want as i64)]).unwrap(),
            ));
            if chunk.is_empty() {
                break;
            }
            got.extend_from_slice(&chunk);
        }
        got
    }

    // A socket handle minted on one OS worker thread must stay usable
    // from another: goroutines migrate across the worker pool, so the
    // backing registry has to be process-global, not thread-local. A
    // thread-local registry would make `local_addr` on the second thread
    // report a stale handle (empty map), and the ephemeral port assigned
    // at bind would be invisible.
    #[test]
    fn listener_handle_survives_thread_boundary() {
        let handle = thread::spawn(|| {
            unwrap_ok(builtin_tcp_listener_bind(&[Value::String("127.0.0.1:0".into())]).unwrap())
        })
        .join()
        .unwrap();

        let addr_v = {
            let handle = handle.clone();
            thread::spawn(move || {
                unwrap_ok(builtin_tcp_listener_local_addr(std::slice::from_ref(&handle)).unwrap())
            })
            .join()
            .unwrap()
        };

        let addr = str_of(&addr_v);
        assert!(addr.starts_with("127.0.0.1:"), "addr = {addr}");
        assert!(
            !addr.ends_with(":0"),
            "ephemeral port should be bound: {addr}"
        );

        builtin_tcp_listener_close(std::slice::from_ref(&handle)).unwrap();
    }

    // End-to-end loopback round trip exercising the accept-then-handle
    // pattern across a thread boundary: the accepted stream's handle is
    // created on the accepting thread and used on a different thread.
    // While the handler is parked in `read`, it holds only its own
    // per-socket mutex - the client thread keeps touching the global
    // registry (connect / write / read), which would deadlock if the
    // registry lock were held across blocking I/O. Readiness comes from
    // connect-after-bind; no fixed sleep.
    #[test]
    fn accepted_stream_round_trips_across_thread_boundary() {
        let listener =
            unwrap_ok(builtin_tcp_listener_bind(&[Value::String("127.0.0.1:0".into())]).unwrap());
        let addr = str_of(&unwrap_ok(
            builtin_tcp_listener_local_addr(std::slice::from_ref(&listener)).unwrap(),
        ));

        let client = thread::spawn(move || {
            let stream =
                unwrap_ok(builtin_tcp_stream_connect(&[Value::String(addr.into())]).unwrap());
            unwrap_ok(
                builtin_tcp_stream_write(&[stream.clone(), Value::String("ping".into())]).unwrap(),
            );
            let echoed = read_exact(&stream, 4);
            builtin_tcp_stream_close(std::slice::from_ref(&stream)).unwrap();
            echoed
        });

        let accepted =
            unwrap_ok(builtin_tcp_listener_accept(std::slice::from_ref(&listener)).unwrap());
        let stream = match accepted {
            Value::Tuple(parts) => parts[0].clone(),
            other => panic!("expected (stream, addr) tuple, got {other:?}"),
        };

        let handler = thread::spawn(move || {
            let request = read_exact(&stream, 4);
            unwrap_ok(builtin_tcp_stream_write(&[stream.clone(), byte_array(&request)]).unwrap());
            builtin_tcp_stream_close(std::slice::from_ref(&stream)).unwrap();
        });

        handler.join().unwrap();
        let echoed = client.join().unwrap();
        assert_eq!(echoed, b"ping", "round-trip payload mismatch");

        builtin_tcp_listener_close(std::slice::from_ref(&listener)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_connect_and_accept_use_real_socket_handles() {
        let path = gossamer_runtime::platform::temp_dir().join(format!(
            "gossamer-net-{}-{}",
            gossamer_runtime::platform::process_id(),
            next_net_id()
        ));
        let path = path.to_string_lossy().into_owned();
        let listener =
            unwrap_ok(builtin_unix_listener_bind(&[Value::String(path.clone().into())]).unwrap());

        let client_path = path.clone();
        let client = thread::spawn(move || {
            unwrap_ok(builtin_unix_stream_connect(&[Value::String(client_path.into())]).unwrap())
        });

        let accepted =
            unwrap_ok(builtin_unix_listener_accept(std::slice::from_ref(&listener)).unwrap());
        let stream = match accepted {
            Value::Tuple(parts) => parts[0].clone(),
            other => panic!("expected (stream, addr) tuple, got {other:?}"),
        };
        let client = client.join().unwrap();

        builtin_unix_stream_close(std::slice::from_ref(&stream)).unwrap();
        builtin_unix_stream_close(std::slice::from_ref(&client)).unwrap();
        builtin_unix_listener_close(std::slice::from_ref(&listener)).unwrap();
        std::fs::remove_file(path).unwrap();
    }
}

/// `smtp::send(addr, from, to, subject, body)` and its authenticated
/// counterpart. Both go through `gossamer_runtime::smtp`, the same
/// transaction the compiled tiers run.
pub(crate) fn install_smtp(globals: &mut Vec<(&'static str, Value)>) {
    for (name, call) in [
        (
            "smtp::send",
            builtin_smtp_send as crate::builtins::BuiltinFnPub,
        ),
        ("smtp::send_auth", builtin_smtp_send_auth),
    ] {
        globals.push((name, crate::builtins::builtin_pub(name, call)));
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn smtp_arg(args: &[Value], index: usize) -> String {
    match args.get(index) {
        Some(Value::String(s)) => s.as_str().to_string(),
        Some(other) => format!("{other}"),
        None => String::new(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn smtp_dispatch(args: &[Value], with_credentials: bool) -> RuntimeResult<Value> {
    let addr = smtp_arg(args, 0);
    let from = smtp_arg(args, 1);
    let to = smtp_arg(args, 2);
    let subject = smtp_arg(args, 3);
    let body = smtp_arg(args, 4);
    let message = gossamer_runtime::smtp::Message {
        from: &from,
        to: &to,
        subject: &subject,
        body: &body,
    };
    let username = smtp_arg(args, 5);
    let password = smtp_arg(args, 6);
    let credentials = with_credentials.then(|| gossamer_runtime::smtp::Credentials {
        username: &username,
        password: &password,
    });
    match gossamer_runtime::smtp::send(&addr, &message, credentials.as_ref()) {
        Ok(()) => Ok(ok_variant(Value::Unit)),
        Err(text) => Ok(err_variant(text)),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn builtin_smtp_send(args: &[Value]) -> RuntimeResult<Value> {
    smtp_dispatch(args, false)
}

#[cfg(not(target_arch = "wasm32"))]
fn builtin_smtp_send_auth(args: &[Value]) -> RuntimeResult<Value> {
    smtp_dispatch(args, true)
}

#[cfg(target_arch = "wasm32")]
fn builtin_smtp_send(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(err_variant("smtp::send: no sockets on wasm32"))
}

#[cfg(target_arch = "wasm32")]
fn builtin_smtp_send_auth(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(err_variant("smtp::send_auth: no sockets on wasm32"))
}
