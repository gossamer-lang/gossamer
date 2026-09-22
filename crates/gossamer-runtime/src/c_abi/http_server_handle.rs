#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]

//! `std::http::Server` - the configurable server object.
//!
//! `http::serve(addr, handler)` is the one-line shape and keeps every
//! default. A deployment that needs a header deadline, a body budget
//! larger than the default, its bound address read back, or a shutdown it
//! can drive builds a `Server` instead:
//!
//! ```text
//! let s = http::Server::new()
//!     .read_header_timeout_ms(5000)
//!     .max_body_bytes(20 * 1024 * 1024)
//! s.listen("127.0.0.1:0")?
//! println!("listening on {}", s.addr())
//! s.serve(app)?
//! ```
//!
//! `listen` binds before `serve` blocks, so the address is readable
//! immediately - which is what lets a test bind port 0 and still know
//! where to send its request.

use std::os::raw::c_char;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use parking_lot::Mutex;

use super::http_server::ServerLimits;

/// One configured server: its limits, its bound listener, and the state a
/// shutdown drains against.
pub struct GosHttpServer {
    limits: Mutex<ServerLimits>,
    listener: Mutex<Option<std::net::TcpListener>>,
    bound_addr: Mutex<String>,
    shutdown: Arc<AtomicBool>,
    in_flight: Arc<AtomicUsize>,
}

impl GosHttpServer {
    fn new() -> Self {
        Self {
            limits: Mutex::new(ServerLimits::default()),
            listener: Mutex::new(None),
            bound_addr: Mutex::new(String::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// Live servers by handle. A handle is an index, never a pointer, so a
/// stale one is refused rather than dereferenced.
fn registry() -> &'static Mutex<Vec<Arc<GosHttpServer>>> {
    static REGISTRY: std::sync::OnceLock<Mutex<Vec<Arc<GosHttpServer>>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

fn server_at(handle: i64) -> Option<Arc<GosHttpServer>> {
    let index = usize::try_from(handle).ok()?;
    registry().lock().get(index).map(Arc::clone)
}

fn err_result(message: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
    crate::c_abi::vec::pack_result(1, err as i64)
}

/// `http::Server::new() -> Server` - a server carrying every default.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_new() -> i64 {
    ffi_entry!(-1, {
        let mut servers = registry().lock();
        servers.push(Arc::new(GosHttpServer::new()));
        i64::try_from(servers.len() - 1).unwrap_or(-1)
    })
}

/// Sets one integer limit and answers the server, so the setters chain.
fn set_limit(handle: i64, apply: impl FnOnce(&mut ServerLimits)) -> i64 {
    if let Some(server) = server_at(handle) {
        apply(&mut server.limits.lock());
    }
    handle
}

/// Milliseconds, clamped at zero. A negative budget is not a shorter one.
fn millis(value: i64) -> u64 {
    u64::try_from(value.max(0)).unwrap_or(0)
}

/// A byte or connection budget, clamped at zero.
fn budget(value: i64) -> usize {
    usize::try_from(value.max(0)).unwrap_or(usize::MAX)
}

/// `server.read_header_timeout_ms(ms)` - how long the request line plus
/// header block has to arrive. Zero disables it. This is the slowloris
/// bound: a socket idle timeout does not stop a client that trickles one
/// header every 25 seconds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_read_header_timeout_ms(handle: i64, ms: i64) -> i64 {
    ffi_entry!(handle, {
        set_limit(handle, |l| l.read_header_timeout_ms = millis(ms))
    })
}

/// `server.read_body_timeout_ms(ms)` - how long the body has to arrive
/// after the headers. Kept separate so a long upload does not have to buy
/// a long header window.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_read_body_timeout_ms(handle: i64, ms: i64) -> i64 {
    ffi_entry!(handle, {
        set_limit(handle, |l| l.read_body_timeout_ms = millis(ms))
    })
}

/// `server.write_timeout_ms(ms)` - how long a response has to reach a peer
/// that stopped reading.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_write_timeout_ms(handle: i64, ms: i64) -> i64 {
    ffi_entry!(handle, {
        set_limit(handle, |l| l.write_timeout_ms = millis(ms))
    })
}

/// `server.idle_timeout_ms(ms)` - how long a keep-alive connection may sit
/// between requests.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_idle_timeout_ms(handle: i64, ms: i64) -> i64 {
    ffi_entry!(handle, {
        set_limit(handle, |l| l.idle_timeout_ms = millis(ms))
    })
}

/// `server.max_header_bytes(n)` - largest accepted header block; past it
/// the answer is 431.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_max_header_bytes(handle: i64, n: i64) -> i64 {
    ffi_entry!(handle, {
        set_limit(handle, |l| l.max_header_bytes = budget(n))
    })
}

/// `server.max_body_bytes(n)` - largest accepted body; past it the answer
/// is 413. The default is 1 MiB, which an application accepting a photo, a
/// PDF, or a CSV import raises here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_max_body_bytes(handle: i64, n: i64) -> i64 {
    ffi_entry!(handle, {
        set_limit(handle, |l| l.max_body_bytes = budget(n))
    })
}

/// `server.max_connections(n)` - largest number of live connections; past
/// it the answer is 503.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_max_connections(handle: i64, n: i64) -> i64 {
    ffi_entry!(handle, {
        set_limit(handle, |l| l.max_connections = budget(n).max(1))
    })
}

/// `server.request_timeout_ms(ms)` - how long a request's context lives
/// before it is cancelled. Zero leaves the request uncancelled by time; it
/// still ends when the handler returns, the peer disconnects, or shutdown
/// begins.
///
/// This is a deadline the handler observes, not a kill: a handler that
/// never looks at its context runs to completion. Pass it to whatever
/// should stop with the request - a query, an outbound call, a spawned
/// worker - and those stop on time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_request_timeout_ms(handle: i64, ms: i64) -> i64 {
    ffi_entry!(handle, {
        set_limit(handle, |l| l.request_timeout_ms = millis(ms))
    })
}

/// `server.server_name(name)` - the `Server` response header, or none when
/// empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_server_name(handle: i64, name: *const c_char) -> i64 {
    ffi_entry!(handle, {
        let text = if name.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(name) }
        };
        set_limit(handle, move |l| l.server_name = text)
    })
}

/// `server.listen(addr) -> Result<(), errors::Error>` - binds now, so the
/// bound address is readable before `serve` blocks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_listen(handle: i64, addr: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let Some(server) = server_at(handle) else {
            return err_result("http::Server::listen: stale server handle");
        };
        let addr_s = if addr.is_null() {
            "0.0.0.0:8080".to_string()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(addr) }
        };
        let listener = match crate::listen::bind_tcp(&addr_s) {
            Ok(l) => l,
            Err(e) => return err_result(&format!("http::Server::listen: {e}")),
        };
        let bound = listener
            .local_addr()
            .map_or_else(|_| addr_s.clone(), |a| a.to_string());
        *server.bound_addr.lock() = bound;
        *server.listener.lock() = Some(listener);
        crate::c_abi::vec::pack_result(0, 0)
    })
}

/// `server.addr() -> String` - the address the listener bound, or `""`
/// before `listen`. Binding port 0 and reading it back is how a test
/// finds a free port without racing another test for one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_addr(handle: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let text = server_at(handle).map_or_else(String::new, |s| s.bound_addr.lock().clone());
        crate::c_abi::string::alloc_cstring(text.as_bytes())
    })
}

/// `server.serve(handler) -> Result<(), errors::Error>` - accepts until
/// shutdown. Binds `addr` first when `listen` was not called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_serve(
    handle: i64,
    handler_env: *mut u8,
    handler_fn: i64,
) -> i128 {
    ffi_entry!(0i128, {
        let Some(server) = server_at(handle) else {
            return err_result("http::Server::serve: stale server handle");
        };
        let Some(listener) = server.listener.lock().take() else {
            return err_result("http::Server::serve: call listen(addr) first");
        };
        let limits = server.limits.lock().clone();
        let env_addr = handler_env as usize;
        let fn_addr = handler_fn as usize;
        let gate = std::sync::Arc::new(super::http_server::RequestGate {
            shutdown: Arc::clone(&server.shutdown),
            in_flight: Arc::clone(&server.in_flight),
        });
        let served = super::http_server::accept_serve_with(
            listener,
            &limits,
            &server.shutdown,
            move |stream, peer, limits| {
                super::http_server::serve_one_connection(
                    stream, peer, limits, &gate, env_addr, fn_addr,
                );
            },
        );
        if !served {
            return err_result("http::Server::serve: the listener stopped accepting");
        }
        crate::c_abi::vec::pack_result(0, 0)
    })
}

/// `server.shutdown(deadline_ms) -> bool` - stops accepting, then waits
/// for in-flight requests to finish. A keep-alive connection waiting
/// between requests is not one.
///
/// Answers whether the drain completed: `false` means the deadline
/// elapsed with requests still running, which is the caller's cue to
/// report it rather than exit believing the drain was clean.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_server_shutdown(handle: i64, deadline_ms: i64) -> i64 {
    ffi_entry!(0, {
        let Some(server) = server_at(handle) else {
            return 0;
        };
        server.shutdown.store(true, Ordering::Release);
        // The acceptor is blocked in `accept()`; a self-connect wakes it
        // so it observes the flag instead of waiting for the next client.
        let addr = server.bound_addr.lock().clone();
        if let Ok(sock) = addr.parse::<std::net::SocketAddr>() {
            let _ =
                std::net::TcpStream::connect_timeout(&sock, std::time::Duration::from_millis(200));
        }
        let deadline = crate::platform::Instant::now()
            + std::time::Duration::from_millis(deadline_ms.max(0) as u64);
        while server.in_flight.load(Ordering::Acquire) > 0 {
            if crate::platform::Instant::now() >= deadline {
                return 0;
            }
            crate::platform::sleep(std::time::Duration::from_millis(5));
        }
        1
    })
}
