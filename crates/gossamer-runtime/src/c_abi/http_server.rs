#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::raw::c_char;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

// ---------------------------------------------------------------
// HTTP server
// ---------------------------------------------------------------
//
// HTTP/1 TCP listener with scheduler-owned, non-blocking connections.
// Per connection we keep a `ConnScratch` reused
// across keep-alive requests so the steady state allocates
// nothing on the parse / response paths beyond what the user's
// handler does inside the gossamer arena (which is reset
// between requests). Phase 2 of the http_optimizations plan
// swaps `parse_request_into` for httparse and adds
// BufReader/BufWriter; today the parser is a naive CRLF split
// that's enough for HTTP/1.1 keep-alive bench traffic.

const STATIC_OK_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok";
const RESPONSE_500_BYTES: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 21\r\nConnection: keep-alive\r\n\r\ninternal server error";
/// The same answer for a connection that ends with it, so the header a
/// client reads is the one the socket then does.
const RESPONSE_500_CLOSE_BYTES: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 21\r\nConnection: close\r\n\r\ninternal server error";
const RESPONSE_400_BYTES: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESPONSE_413_BYTES: &[u8] =
    b"HTTP/1.1 413 Content Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESPONSE_431_BYTES: &[u8] =
    b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

// Upper bound on the request head (request line plus header block)
// before its terminating CRLFCRLF. A client that streams header bytes
// without ever completing the head would otherwise grow the read
// accumulator without limit. Parity source of truth: the interp
// server's 8 KiB `Config::default().max_header_bytes`
// (gossamer-std/src/http.rs).
const MAX_REQUEST_HEAD_BYTES: usize = 8 * 1024;

// Upper bound on a request body, Content-Length-declared or
// de-chunked. Parity source of truth: the interp server's
// `Config::default().max_body_bytes` (gossamer-std/src/http.rs,
// 1 MiB) - interp `http::serve` builds that default config
// (gossamer-interp builtins.rs, `http_std::server::Config::default()`)
// with no Gossamer-level or env override, so the compiled tier
// enforces the same fixed cap. Also keeps `header_end +
// content_length` from overflowing on a hostile Content-Length.
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

// Longest accepted chunk-size line (hex size + optional chunk
// extension). The hex part itself is capped at 16 digits to mirror
// the interp decoder's `max_size_digits`; this bounds the
// extension tail so a hostile peer cannot stream an endless line.
const MAX_CHUNK_SIZE_LINE_BYTES: usize = 1024;

// Cap on the trailer block after the terminal 0-chunk. Mirrors the
// interp server's 8 KiB `max_header_bytes` header-block cap -
// trailers are headers.
const MAX_TRAILER_BYTES: usize = 8 * 1024;

/// The bounds one server applies to every connection it accepts.
///
/// `http::serve` uses the defaults; a `http::Server` sets what it needs.
/// Every default is the interp tier's `Config::default()`, so the two
/// tiers refuse and accept the same requests.
#[derive(Debug, Clone)]
pub struct ServerLimits {
    /// How long the request line plus header block has to arrive. Zero
    /// disables it. Slowloris protection: a socket idle timeout does not
    /// bound a client that trickles one header every 25 seconds.
    pub read_header_timeout_ms: u64,
    /// How long the body has to arrive after the headers. Zero disables.
    pub read_body_timeout_ms: u64,
    /// How long a response has to reach a peer that stopped reading.
    pub write_timeout_ms: u64,
    /// How long a keep-alive connection may sit between requests.
    pub idle_timeout_ms: u64,
    /// Largest accepted header block; past it the answer is 431.
    pub max_header_bytes: usize,
    /// Largest accepted body; past it the answer is 413.
    pub max_body_bytes: usize,
    /// Largest number of live connections; past it the answer is 503.
    pub max_connections: usize,
    /// `Server` response header, or empty to send none.
    pub server_name: String,
    /// How long one request's context lives before it is cancelled.
    /// Zero leaves the request uncancelled by time - it still ends when
    /// the handler returns, the peer disconnects, or shutdown begins.
    pub request_timeout_ms: u64,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            read_header_timeout_ms: http_read_timeout_ms().unwrap_or(0),
            read_body_timeout_ms: http_read_timeout_ms().unwrap_or(0),
            write_timeout_ms: http_write_timeout_ms().unwrap_or(0),
            idle_timeout_ms: http_read_timeout_ms().unwrap_or(0),
            max_header_bytes: MAX_REQUEST_HEAD_BYTES,
            max_body_bytes: MAX_REQUEST_BODY_BYTES,
            max_connections: http_max_conn(),
            server_name: concat!("gossamer/", env!("CARGO_PKG_VERSION")).to_string(),
            request_timeout_ms: 0,
        }
    }
}

impl ServerLimits {
    /// Cap on the raw, still-encoded bytes of one chunked frame. The
    /// decoded cap bounds payload; this bounds framing overhead, so a
    /// peer drip-feeding 1-byte chunks with maximal extensions cannot
    /// grow the accumulator without limit.
    fn max_chunked_raw_bytes(&self) -> usize {
        self.max_body_bytes
            .saturating_mul(2)
            .saturating_add(16 * 1024)
    }
}

/// Per-connection mutable scratch. Reused across keep-alive
/// requests so steady state allocates only inside the gossamer
/// arena, which is reset between requests.
struct ConnScratch {
    /// Filled in place by `parse_request_into` and handed to
    /// the user handler as `*mut GosHttpRequest`. Lives for
    /// the entire connection.
    request: GosHttpRequest,
    /// Bytes written to the wire. Truncated, never freed,
    /// across requests.
    response_buf: Vec<u8>,
}

impl ConnScratch {
    fn new() -> Self {
        Self {
            request: GosHttpRequest {
                method: String::with_capacity(8),
                url: String::with_capacity(64),
                headers: Vec::with_capacity(16),
                body: Vec::with_capacity(0),
                body_offset: 0,
                params: Vec::new(),
                values: Vec::new(),
                agent: None,
                peer: String::new(),
                context_site: None,
                context: 0,
            },
            response_buf: Vec::with_capacity(512),
        }
    }
}

/// Live count of compiled HTTP server connections. Each accepted connection
/// bumps this on dispatch and decrements when its connection thread finishes;
/// final body line; the cap from `GOSSAMER_HTTP_MAX_CONN` rejects
/// further connections with a 503 once the count reaches its
/// ceiling. Process-global so multiple `http::serve` calls inside
/// the same program share back-pressure.
static HTTP_ACTIVE_CONNS: AtomicUsize = AtomicUsize::new(0);

/// RAII guard that decrements [`HTTP_ACTIVE_CONNS`] when the
/// connection thread's body unwinds or returns, and counts the thread among
/// the actors that can still reach a channel for as long as it serves.
/// Created inside the thread closure so both run even if
/// `handle_http_conn` panics.
/// Counts a live connection for as long as its task runs. A connection on a
/// thread of its own also counts as an actor that can reach a channel; a
/// goroutine is one already.
struct HttpConnGuard {
    _actor: Option<crate::sched_global::ExternalActor>,
}

impl HttpConnGuard {
    fn enter(home: ConnHome) -> Self {
        Self {
            _actor: (home == ConnHome::Thread).then(crate::sched_global::ExternalActor::enter),
        }
    }
}

impl Drop for HttpConnGuard {
    fn drop(&mut self) {
        HTTP_ACTIVE_CONNS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Default per-process cap on concurrent HTTP server connections,
/// overridable via the `GOSSAMER_HTTP_MAX_CONN` env var. 4096 is
/// well below the typical 65 535 fd ceiling and leaves headroom
/// for the listener, the netpoller, log files, and the rest of
/// the runtime's open files.
const DEFAULT_HTTP_MAX_CONN: usize = 4096;

fn http_max_conn() -> usize {
    static CACHE: AtomicUsize = AtomicUsize::new(0);
    // Sentinel 0 means "not yet read" - the cap can never legally
    // be zero (that would refuse every connection). Resolve once
    // per process and cache.
    let cached = CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let cap = std::env::var("GOSSAMER_HTTP_MAX_CONN")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_HTTP_MAX_CONN);
    CACHE.store(cap, Ordering::Relaxed);
    cap
}

/// Per-connection idle / slow-read timeout in milliseconds. Mirrors the
/// interp tier's 30s default; `GOSSAMER_HTTP_READ_TIMEOUT_MS=0` disables
/// it.
const DEFAULT_HTTP_READ_TIMEOUT_MS: u64 = 30_000;

/// Per-connection write timeout. A peer which stops reading must not retain a
/// scheduler goroutine forever while its send buffer remains full. The interp
/// server uses the same 30s default for its write timeout.
const DEFAULT_HTTP_WRITE_TIMEOUT_MS: u64 = 30_000;

/// Returns the configured idle / slow-read timeout. A zero value disables
/// it. Every compiled HTTP transport consumes this through the netpoller.
fn http_read_timeout_ms() -> Option<u64> {
    let ms = std::env::var("GOSSAMER_HTTP_READ_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_HTTP_READ_TIMEOUT_MS);
    (ms != 0).then_some(ms)
}

/// Returns the configured response-write timeout. `0` disables it. Kept
/// separate from the read setting so a deployment can allow long uploads
/// without granting an unbounded slow-reader resource hold.
fn http_write_timeout_ms() -> Option<u64> {
    let ms = std::env::var("GOSSAMER_HTTP_WRITE_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_HTTP_WRITE_TIMEOUT_MS);
    (ms != 0).then_some(ms)
}

/// Applies the compiled server's slow-peer limits before transferring an
/// accepted socket to its connection thread. The timeout is socket-local, so
/// one stalled peer cannot retain a thread indefinitely.
fn configure_socket_timeouts(stream: &TcpStream) {
    let read = http_read_timeout_ms().map(std::time::Duration::from_millis);
    let write = http_write_timeout_ms().map(std::time::Duration::from_millis);
    let _ = stream.set_read_timeout(read);
    let _ = stream.set_write_timeout(write);
}

const RESPONSE_503_BYTES: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Packs an `Err(errors::Error)` runtime `Result` carrying `msg` -
/// the bind-failure value `gos_rt_http_serve` and
/// `gos_rt_http2_bind_and_run_h2c` hand back to the caller's
/// `Result<(), http::Error>` match.
fn http_serve_err_result(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    super::vec::pack_result(1, err as i64)
}

/// Starts an HTTP listener and dispatches each request to
/// `handler_fn(handler_env, request)`. Returns 200/payload from
/// the handler's `Ok(Response)`, 500 from `Err`, and a static
/// `200 OK\r\n\r\nok` when `handler_fn` is null (legacy stub).
///
/// Returns the Gossamer-visible `Result<(), http::Error>`: a
/// packed `Err` when the address cannot be bound (interp parity -
/// the VM hands the same `Err` to the caller's match), or a packed
/// `Ok(())` if the accept loop ever exits (graceful shutdown).
///
/// Concurrent connections are capped at `GOSSAMER_HTTP_MAX_CONN`
/// (default 4096). When the cap is hit the listener accepts the
/// connection, writes a 503 Service Unavailable response, closes
/// the socket without spawning a thread, and continues. This
/// applies bounded back-pressure so a flood of clients cannot exhaust the
/// file-descriptor or thread budget.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_http_serve(
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
            Err(e) => {
                // Startup-time failure: hand back `Err` with the
                // interp's message shape so all tiers print the same
                // text from `match http::serve(..) { Err(e) => .. }`.
                return http_serve_err_result(&format!("http::serve: {e}"));
            }
        };
        let env_addr = handler_env as usize;
        let fn_addr = handler_fn as usize;
        // HTTP/1 keep-alive requests block on the next read between responses.
        // Keep that wait local to the connection: routing every ready socket
        // through the scheduler's one global poller serializes high-throughput
        // traffic on the poller lock. Admission remains bounded below.
        accept_serve(listener, ConnHome::Goroutine, move |stream| {
            let peer = stream
                .peer_addr()
                .map_or_else(|_| String::new(), |a| a.to_string());
            let Ok(mut conn) = GoroutineTcpConn::new(
                stream,
                http_read_timeout_ms().unwrap_or(0),
                http_write_timeout_ms().unwrap_or(0),
            ) else {
                return;
            };
            handle_http_conn_from(&mut conn, env_addr, fn_addr, &peer);
        });
    }
    // The accept loop exited: graceful shutdown request or a fatal
    // listener error. Either way the server ran - report `Ok(())`,
    // matching the interp's `bind_and_run` return shape.
    super::vec::pack_result(0, 0)
}

/// Where an accepted connection runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnHome {
    /// A goroutine of its own. The connection's transfers must park the
    /// goroutine rather than block ([`GoroutineTcpConn`]), and a connection
    /// waiting on its peer holds no thread and no allocator heap of its own.
    Goroutine,
    /// An OS thread of its own, with the socket's deadlines applied, for a
    /// transport whose I/O blocks.
    Thread,
}

/// Starts `task` for one accepted connection where `home` says. Answers
/// `false` when nothing could be started, having run nothing.
fn start_connection(home: ConnHome, task: Box<dyn FnOnce() + Send + 'static>) -> bool {
    match home {
        ConnHome::Goroutine => crate::sched_global::try_spawn(task).is_some(),
        ConnHome::Thread => std::thread::Builder::new()
            .name("gos-http-conn".to_string())
            .spawn(task)
            .is_ok(),
    }
}

/// Accept loop shared by compiled HTTP, TLS, and WebSocket servers. Each
/// accepted socket is served where `home` says. `HTTP_ACTIVE_CONNS` caps the
/// live connection count at `GOSSAMER_HTTP_MAX_CONN` (default 4096), replying
/// 503 past the cap so a client flood cannot exhaust file descriptors.
pub(crate) fn accept_serve<F>(listener: TcpListener, home: ConnHome, serve_conn: F)
where
    F: Fn(TcpStream) + Send + Sync + Clone + 'static,
{
    install_http_shutdown_signal_handler();
    let _wake_guard = listener
        .local_addr()
        .ok()
        .map(register_http_shutdown_wake_addr);
    // The loop itself can still admit a connection whose handler reaches a
    // channel, so it counts as an actor while it runs.
    let _actor = crate::sched_global::ExternalActor::enter();
    let mut backoff = crate::accept::AcceptBackoff::new();
    loop {
        if crate::sched_global::is_shutdown_requested() {
            break;
        }
        let stream = match listener.accept() {
            Ok((s, _)) => {
                backoff.reset();
                s
            }
            // A signal delivered to this thread - the scheduler's own
            // preemption among them - interrupts the call without touching
            // the listener, so it is asked again at once rather than paced
            // like a shortage.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if crate::accept::accept_error_is_transient(&e) => {
                backoff.settle();
                continue;
            }
            Err(_) => break,
        };
        if crate::sched_global::is_shutdown_requested() {
            let _ = stream.shutdown(Shutdown::Both);
            break;
        }
        let _ = stream.set_nodelay(true);
        if home == ConnHome::Thread {
            configure_socket_timeouts(&stream);
        }
        let cap = http_max_conn();
        let current = HTTP_ACTIVE_CONNS.fetch_add(1, Ordering::AcqRel);
        if current >= cap {
            HTTP_ACTIVE_CONNS.fetch_sub(1, Ordering::AcqRel);
            // Best-effort 503 + close; ignore write errors -
            // the client might already be gone.
            let mut stream = stream;
            use std::io::Write;
            let _ = stream.write_all(RESPONSE_503_BYTES);
            let _ = stream.flush();
            continue;
        }
        // Keep the socket outside the task until it starts, so a thread or
        // goroutine limit reports a truthful 503 instead of resetting the
        // accepted connection.
        let serve = serve_conn.clone();
        let slot = std::sync::Arc::new(parking_lot::Mutex::new(Some(stream)));
        let task_slot = std::sync::Arc::clone(&slot);
        let spawned = start_connection(
            home,
            Box::new(move || {
                let Some(stream) = task_slot.lock().take() else {
                    return;
                };
                let _guard = HttpConnGuard::enter(home);
                // One connection is one fault domain: a handler panic ends
                // that request, not the server.
                let _faults = crate::c_abi::panic::IsolatedFaults::enter();
                serve(stream);
            }),
        );
        if !spawned {
            HTTP_ACTIVE_CONNS.fetch_sub(1, Ordering::AcqRel);
            if let Some(mut stream) = slot.lock().take() {
                use std::io::Write;
                let _ = stream.write_all(RESPONSE_503_BYTES);
                let _ = stream.flush();
            }
        }
    }
}

/// Accept loop for one configured [`ServerLimits`], with its own shutdown
/// flag and in-flight count.
///
/// The process-wide loop [`accept_serve`] drives is the one-line
/// `http::serve`; this is what a `http::Server` runs, so two servers in
/// one process apply their own budgets and drain independently.
pub(crate) fn accept_serve_with<F>(
    listener: TcpListener,
    limits: &ServerLimits,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    in_flight: &std::sync::Arc<AtomicUsize>,
    serve_conn: F,
) -> bool
where
    F: Fn(TcpStream, String, &ServerLimits) + Send + Sync + Clone + 'static,
{
    install_http_shutdown_signal_handler();
    let _wake_guard = listener
        .local_addr()
        .ok()
        .map(register_http_shutdown_wake_addr);
    let live = std::sync::Arc::new(AtomicUsize::new(0));
    // The loop itself can still admit a connection whose handler reaches a
    // channel, so it counts as an actor while it runs.
    let _actor = crate::sched_global::ExternalActor::enter();
    let mut backoff = crate::accept::AcceptBackoff::new();
    loop {
        if shutdown.load(Ordering::Acquire) || crate::sched_global::is_shutdown_requested() {
            break;
        }
        let stream = match listener.accept() {
            Ok((s, _)) => {
                backoff.reset();
                s
            }
            // A signal delivered to this thread - the scheduler's own
            // preemption among them - interrupts the call without touching
            // the listener, so it is asked again at once rather than paced
            // like a shortage.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if crate::accept::accept_error_is_transient(&e) => {
                backoff.settle();
                continue;
            }
            // The listener itself is gone, which nobody asked for.
            Err(_) => return false,
        };
        if shutdown.load(Ordering::Acquire) || crate::sched_global::is_shutdown_requested() {
            let _ = stream.shutdown(Shutdown::Both);
            break;
        }
        let _ = stream.set_nodelay(true);
        if live.load(Ordering::Acquire) >= limits.max_connections {
            let mut stream = stream;
            use std::io::Write;
            let _ = stream.write_all(RESPONSE_503_BYTES);
            let _ = stream.flush();
            continue;
        }
        let peer = stream
            .peer_addr()
            .map_or_else(|_| String::new(), |a| a.to_string());
        live.fetch_add(1, Ordering::AcqRel);
        in_flight.fetch_add(1, Ordering::AcqRel);
        let serve = serve_conn.clone();
        let conn_limits = limits.clone();
        let live_for_thread = std::sync::Arc::clone(&live);
        let in_flight_for_thread = std::sync::Arc::clone(in_flight);
        let slot = std::sync::Arc::new(parking_lot::Mutex::new(Some(stream)));
        let task_slot = std::sync::Arc::clone(&slot);
        let spawned = start_connection(
            ConnHome::Goroutine,
            Box::new(move || {
                let _counts = ConnCounts {
                    live: live_for_thread,
                    in_flight: in_flight_for_thread,
                };
                let Some(stream) = task_slot.lock().take() else {
                    return;
                };
                // One connection is one fault domain: a handler panic ends
                // that request, not the server.
                let _faults = crate::c_abi::panic::IsolatedFaults::enter();
                serve(stream, peer, &conn_limits);
            }),
        );
        if !spawned {
            live.fetch_sub(1, Ordering::AcqRel);
            in_flight.fetch_sub(1, Ordering::AcqRel);
            if let Some(mut stream) = slot.lock().take() {
                use std::io::Write;
                let _ = stream.write_all(RESPONSE_503_BYTES);
                let _ = stream.flush();
            }
        }
    }
    true
}

/// Decrements a connection's live and in-flight counts on every exit path,
/// including an unwind, so a shutdown drain cannot wait on a thread that
/// already ended.
struct ConnCounts {
    live: std::sync::Arc<AtomicUsize>,
    in_flight: std::sync::Arc<AtomicUsize>,
}

impl Drop for ConnCounts {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Serves one accepted connection under `limits`, on the goroutine
/// [`accept_serve_with`] started for it.
pub(crate) fn serve_one_connection(
    stream: TcpStream,
    peer: String,
    limits: &ServerLimits,
    env_addr: usize,
    fn_addr: usize,
) {
    // Each read is bounded by the longer of the two read phases; the header
    // phase's own, shorter bound is enforced against the accumulator.
    let read_ms = limits
        .read_header_timeout_ms
        .max(limits.read_body_timeout_ms);
    let Ok(mut conn) = GoroutineTcpConn::new(stream, read_ms, limits.write_timeout_ms) else {
        return;
    };
    handle_http_conn_limited(&mut conn, env_addr, fn_addr, &peer, limits);
}

struct HttpWakeAddrGuard(SocketAddr);

impl Drop for HttpWakeAddrGuard {
    fn drop(&mut self) {
        http_wake_addrs().lock().retain(|addr| *addr != self.0);
    }
}

fn register_http_shutdown_wake_addr(addr: SocketAddr) -> HttpWakeAddrGuard {
    http_wake_addrs().lock().push(addr);
    HttpWakeAddrGuard(addr)
}

fn http_wake_addrs() -> &'static parking_lot::Mutex<Vec<SocketAddr>> {
    static ADDRS: OnceLock<parking_lot::Mutex<Vec<SocketAddr>>> = OnceLock::new();
    ADDRS.get_or_init(|| parking_lot::Mutex::new(Vec::new()))
}

/// Breaks every live accept loop out of its blocking `accept()` by
/// connecting to the address it is bound to.
///
/// An acceptor parked in `accept()` reaches its shutdown check only when a
/// connection arrives, so the flag alone leaves it parked until one does.
/// The self-connect is the arrival, and the loop's own check closes the
/// connection and leaves. This is what the interp tier's acceptor does, so
/// a shutdown ends the same way on every tier.
pub(crate) fn wake_http_acceptors() {
    let addrs = http_wake_addrs().lock().clone();
    for addr in addrs {
        let _ = TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200));
    }
}

#[cfg(unix)]
fn install_http_shutdown_signal_handler() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let Ok(mut signals) = signal_hook::iterator::Signals::new([libc::SIGINT, libc::SIGTERM])
        else {
            return;
        };
        std::thread::Builder::new()
            .name("gos-http-shutdown".to_string())
            .spawn(move || {
                for _ in signals.forever() {
                    crate::sched_global::request_shutdown();
                    wake_http_acceptors();
                }
            })
            .ok();
    });
}

#[cfg(not(unix))]
fn install_http_shutdown_signal_handler() {}

/// TLS-terminating HTTP/1.1 server:
/// `serve_tls(addr, cert_pem, key_pem, handler)`. Mirror of
/// [`gos_rt_http_serve`] that builds a rustls server config from the
/// PEM-encoded certificate chain and private key, then drives each
/// accepted connection through the same request/response core after TLS
/// termination - so HTTPS handlers behave identically to plaintext ones.
/// Returns the Gossamer `Result<(), http::Error>`: `Err` on bind or
/// TLS-config failure, `Ok(())` on graceful shutdown.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_http_serve_tls(
    addr: *const c_char,
    cert_pem: *const c_char,
    key_pem: *const c_char,
    handler_env: *mut u8,
    handler_fn: i64,
) -> i128 {
    {
        let addr_s = if addr.is_null() {
            "0.0.0.0:8443".to_string()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(addr) }
        };
        let cert = if cert_pem.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(cert_pem) }
        };
        let key = if key_pem.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(key_pem) }
        };
        let server_config = match build_server_config_from_pem(cert.as_bytes(), key.as_bytes()) {
            Ok(c) => c,
            Err(e) => return http_serve_err_result(&format!("http::serve_tls: {e}")),
        };
        let listener = match crate::listen::bind_tcp(&addr_s) {
            Ok(l) => l,
            Err(e) => return http_serve_err_result(&format!("http::serve_tls: {e}")),
        };
        let env_addr = handler_env as usize;
        let fn_addr = handler_fn as usize;
        accept_serve(listener, ConnHome::Goroutine, move |stream| {
            serve_tls_conn(stream, env_addr, fn_addr, server_config.clone());
        });
    }
    super::vec::pack_result(0, 0)
}

/// rustls-terminated server connection. The accepted socket has the same
/// read/write deadlines and bounded admission as plaintext HTTP.
struct TlsServerConn {
    inner: rustls::StreamOwned<rustls::ServerConnection, GoroutineTcpConn>,
}

impl HttpIo for TlsServerConn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.inner, buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        std::io::Write::write_all(&mut self.inner, buf)?;
        std::io::Write::flush(&mut self.inner)
    }
}

/// Wraps an accepted socket in a rustls server session and serves it
/// through the shared request/response core. A handshake that never
/// completes is dropped when the connection thread's read loop returns.
fn serve_tls_conn(
    stream: TcpStream,
    env_addr: usize,
    fn_addr: usize,
    server_config: std::sync::Arc<rustls::ServerConfig>,
) {
    let Ok(conn) = rustls::ServerConnection::new(server_config) else {
        return;
    };
    let peer = stream
        .peer_addr()
        .map_or_else(|_| String::new(), |a| a.to_string());
    let Ok(transport) = GoroutineTcpConn::new(
        stream,
        http_read_timeout_ms().unwrap_or(0),
        http_write_timeout_ms().unwrap_or(0),
    ) else {
        return;
    };
    let mut tls = TlsServerConn {
        inner: rustls::StreamOwned::new(conn, transport),
    };
    handle_http_conn_from(&mut tls, env_addr, fn_addr, &peer);
}

/// Builds a rustls `ServerConfig` from a PEM certificate chain and
/// private key. No client-certificate verification.
fn build_server_config_from_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<std::sync::Arc<rustls::ServerConfig>, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certs = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("parse certificate PEM: {e}"))?;
    if certs.is_empty() {
        return Err("no certificates in PEM".to_string());
    }
    let key = PrivateKeyDer::pem_slice_iter(key_pem)
        .next()
        .ok_or_else(|| "no private key in PEM".to_string())?
        .map_err(|e| format!("parse key PEM: {e}"))?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("build server config: {e}"))?;
    Ok(std::sync::Arc::new(config))
}

type HandlerFn = unsafe extern "C-unwind" fn(env: *mut u8, req: *mut GosHttpRequest) -> i128;

/// HTTP/2 cleartext server. Mirror of [`gos_rt_http_serve`] for
/// HTTP/2 - the MIR lowerer emits this call when the compiled
/// program invokes `http2::bind_and_run_h2c(addr, app, config)`.
/// The h2 server implementation lives in
/// [`crate::http2_server`]; this thunk just adapts the C-ABI
/// signature into the Rust API.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http2_bind_and_run_h2c(
    addr: *const c_char,
    handler_env: *mut u8,
    handler_fn: i64,
) -> i128 {
    let addr_s = if addr.is_null() {
        "0.0.0.0:8080".to_string()
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(addr) }
    };
    let env_addr = handler_env as usize;
    let fn_addr = handler_fn as usize;
    match crate::http2_server::serve_h2c_with_handler(&addr_s, env_addr, fn_addr) {
        Ok(()) => super::vec::pack_result(0, 0),
        Err(e) => http_serve_err_result(&format!("http::serve_h2c: {e}")),
    }
}

/// Byte transport for one HTTP connection. Plaintext sockets and TLS streams
/// share this request/response core.
trait HttpIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()>;
    /// The socket behind this transport, for a peer-liveness peek. `None`
    /// where there is no descriptor to peek - a TLS session's plaintext is
    /// not the wire, and an in-memory transport has no peer at all.
    #[cfg(any(unix, windows))]
    fn peer_socket(&self) -> Option<RawPeerSocket> {
        None
    }
}

/// The raw socket behind `socket`, as a peer probe reads it.
#[cfg(any(unix, windows))]
fn raw_peer_socket(
    #[cfg(unix)] socket: &impl std::os::fd::AsRawFd,
    #[cfg(windows)] socket: &impl std::os::windows::io::AsRawSocket,
) -> RawPeerSocket {
    #[cfg(unix)]
    {
        socket.as_raw_fd()
    }
    #[cfg(windows)]
    {
        socket.as_raw_socket()
    }
}

/// Accepted-connection transport for a connection served on a goroutine. The
/// socket is non-blocking and registered with the netpoller once; a transfer
/// that would block parks the goroutine until the socket is ready or the
/// transfer's deadline passes, so a waiting connection holds no thread.
pub(crate) struct GoroutineTcpConn {
    stream: mio::net::TcpStream,
    registration: Option<crate::netpoll::Registration>,
    read_timeout: Option<std::time::Duration>,
    write_timeout: Option<std::time::Duration>,
}

impl GoroutineTcpConn {
    /// Takes `stream` over, with `read_ms` and `write_ms` bounding each read
    /// and each write (zero for no bound).
    pub(crate) fn new(stream: TcpStream, read_ms: u64, write_ms: u64) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        let mut stream = mio::net::TcpStream::from_std(stream);
        let registration = crate::netpoll::Registration::new(&mut stream)?;
        let bound = |ms: u64| (ms != 0).then(|| std::time::Duration::from_millis(ms));
        Ok(Self {
            stream,
            registration: Some(registration),
            read_timeout: bound(read_ms),
            write_timeout: bound(write_ms),
        })
    }

    /// Waits for `direction`, answering `TimedOut` once `deadline` passed.
    fn wait(
        &self,
        direction: crate::netpoll::Direction,
        deadline: Option<crate::platform::Instant>,
    ) -> std::io::Result<()> {
        let Some(registration) = self.registration.as_ref() else {
            return Err(std::io::Error::from(std::io::ErrorKind::NotConnected));
        };
        if registration.wait(direction, deadline) {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "HTTP connection timed out",
            ))
        }
    }
}

impl Drop for GoroutineTcpConn {
    fn drop(&mut self) {
        if let Some(registration) = self.registration.take() {
            registration.deregister(&mut self.stream);
        }
    }
}

impl std::io::Read for GoroutineTcpConn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut deadline = None;
        loop {
            match std::io::Read::read(&mut self.stream, buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // The bound runs from the first time the read had to wait,
                    // as a socket receive timeout does.
                    let at = *deadline.get_or_insert_with(|| {
                        self.read_timeout
                            .map(|t| crate::platform::Instant::now() + t)
                    });
                    self.wait(crate::netpoll::Direction::Read, at)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl std::io::Write for GoroutineTcpConn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut deadline = None;
        loop {
            match std::io::Write::write(&mut self.stream, buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let at = *deadline.get_or_insert_with(|| {
                        self.write_timeout
                            .map(|t| crate::platform::Instant::now() + t)
                    });
                    self.wait(crate::netpoll::Direction::Write, at)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl HttpIo for GoroutineTcpConn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(self, buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        std::io::Write::write_all(self, buf)
    }
    #[cfg(any(unix, windows))]
    fn peer_socket(&self) -> Option<RawPeerSocket> {
        Some(raw_peer_socket(&self.stream))
    }
}

#[cfg(test)]
impl HttpIo for HttpConn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        HttpConn::read(self, buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        HttpConn::write_all(self, buf)
    }
}

/// Pulls one read's worth of bytes into `accum`. Returns `false`
/// on clean EOF or a socket error - the caller closes the
/// connection.
fn read_more<C: HttpIo>(conn: &mut C, accum: &mut Vec<u8>, buf: &mut [u8]) -> bool {
    match conn.read(buf) {
        Ok(0) | Err(_) => false,
        Ok(n) => {
            accum.extend_from_slice(&buf[..n]);
            true
        }
    }
}

#[cfg(test)]
fn handle_http_conn<C: HttpIo>(conn: &mut C, env_addr: usize, fn_addr: usize) {
    handle_http_conn_from(conn, env_addr, fn_addr, "");
}

/// [`handle_http_conn_limited`] under the default server limits, with
/// the peer address every request on this connection is stamped with.
fn handle_http_conn_from<C: HttpIo>(conn: &mut C, env_addr: usize, fn_addr: usize, peer: &str) {
    handle_http_conn_limited(conn, env_addr, fn_addr, peer, &ServerLimits::default());
}

/// [`handle_http_conn_from`] under one server's own limits.
fn handle_http_conn_limited<C: HttpIo>(
    conn: &mut C,
    env_addr: usize,
    fn_addr: usize,
    peer: &str,
    limits: &ServerLimits,
) {
    let mut scratch = ConnScratch::new();
    let mut accum: Vec<u8> = Vec::with_capacity(8192);
    let mut buf: Vec<u8> = vec![0u8; 8192];
    // The peer watch is registered once for the connection; each request
    // publishes its own context into this slot and clears it when it ends.
    let watch_ctx = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    #[cfg(any(unix, windows))]
    let _peer_watch = conn
        .peer_socket()
        .and_then(|socket| watch_for_disconnect(socket, std::sync::Arc::clone(&watch_ctx)));
    // Tracks whether a `100 Continue` interim response has already been
    // sent for the request currently being assembled, so the body-wait
    // re-entry below does not emit it more than once. Reset when a
    // request completes (a pipelined successor may also expect it).
    let mut continue_sent = false;
    loop {
        let Some(header_end) = find_header_end(&accum) else {
            // Bound the request head: a slow client that never sends the
            // terminating CRLFCRLF cannot grow `accum` without limit.
            // Mirrors the interp server's 8 KiB header cap.
            if accum.len() > limits.max_header_bytes {
                let _ = conn.write_all(RESPONSE_431_BYTES);
                return;
            }
            if read_more(conn, &mut accum, &mut buf) {
                continue;
            }
            return;
        };
        // RFC 9110 §10.1.1: grant a client waiting on
        // `Expect: 100-continue` before its body is read, matching the
        // interp tier.
        if !continue_sent && request_expects_continue(&accum[..header_end]) {
            if conn.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
                return;
            }
            continue_sent = true;
        }
        // Consume the body too: `req_end` covers the header section
        // plus the body bytes (Content-Length-declared, or the full
        // chunked frame). Anything past it is the next pipelined
        // request - keep it in `accum` for the next iteration.
        let mut chunked_req: Option<ChunkedRequest> = None;
        let req_end;
        if transfer_encoding_is_chunked(&accum[..header_end]) {
            // RFC 9112 §6.3.3 + interp-server parity (gossamer-std
            // http.rs `read_request_body`): a request carrying both
            // `Transfer-Encoding: chunked` and `Content-Length` is
            // request-smuggling-shaped - reject it.
            if header_value(&accum[..header_end], b"content-length").is_some() {
                let _ = conn.write_all(RESPONSE_400_BYTES);
                return;
            }
            match assemble_chunked_request(&accum, header_end, limits) {
                ChunkedAssembly::Incomplete => {
                    if read_more(conn, &mut accum, &mut buf) {
                        continue;
                    }
                    return;
                }
                ChunkedAssembly::Reject(response) => {
                    let _ = conn.write_all(response);
                    return;
                }
                ChunkedAssembly::Ready(decoded) => {
                    req_end = decoded.raw_end;
                    chunked_req = Some(decoded);
                }
            }
        } else {
            let body_len = content_length(&accum[..header_end]);
            if body_len > limits.max_body_bytes {
                let _ = conn.write_all(RESPONSE_413_BYTES);
                return;
            }
            req_end = header_end + body_len;
            if accum.len() < req_end {
                if read_more(conn, &mut accum, &mut buf) {
                    continue;
                }
                return;
            }
        }

        scratch.response_buf.clear();
        // A handler can park on I/O, a channel, or a timer. Its arena state
        // remains attached to that coroutine while parked, and this guard
        // closes any raw regions it left open on every exit path below,
        // including write timeout, peer shutdown, and unwinding.
        let _request_arena = crate::c_abi::rc::RequestArenaGuard::new();

        // A request the connection outlives keeps it; one that names its
        // own end closes it once the answer is written.
        let mut keep_alive = true;
        if fn_addr == 0 {
            // Legacy stub path: ignore the request, send static
            // 200/ok. No arena allocation happens here.
            scratch.response_buf.extend_from_slice(STATIC_OK_RESPONSE);
        } else {
            // Reset the request scratch in place. Field
            // capacities persist; we only push back into them.
            scratch.request.method.clear();
            scratch.request.url.clear();
            scratch.request.headers.clear();
            scratch.request.body.clear();
            scratch.request.body_offset = 0;
            scratch.request.peer.clear();
            scratch.request.peer.push_str(peer);
            // The request's own context. It is cancelled below when the
            // request ends, so anything the handler starts under it stops
            // with the request rather than outliving it.
            // The request's context is opened the first time its handler
            // asks for one; the deadline it will expire at starts here, with
            // the request. A peer that goes away while the handler runs
            // cancels whatever the request opened, so work it asked for stops
            // instead of being paid for after it left.
            scratch.request.context = 0;
            let request_deadline = (limits.request_timeout_ms != 0).then(|| {
                crate::platform::Instant::now()
                    + std::time::Duration::from_millis(limits.request_timeout_ms)
            });
            scratch.request.context_site = Some(crate::c_abi::http_client::RequestContextSite {
                deadline: request_deadline,
                watch: std::sync::Arc::clone(&watch_ctx),
            });

            // Chunked requests parse from the canonical de-chunked
            // rewrite; everything else parses straight from the
            // accumulator.
            let parsed = match &chunked_req {
                Some(c) => parse_request_into(&c.canonical, c.header_end, &mut scratch.request),
                None => parse_request_into(&accum[..req_end], header_end, &mut scratch.request),
            };
            if !parsed {
                // Malformed request: send 400 and close. Keeping
                // the connection open after an unparseable request
                // is unsafe - we don't know how many bytes the
                // bogus request claimed, so the next request would
                // be misaligned. The connection will be reopened
                // by the client.
                let _ = conn.write_all(RESPONSE_400_BYTES);
                return;
            }

            keep_alive = {
                let head: &[u8] = match &chunked_req {
                    Some(c) => &c.canonical[..c.header_end],
                    None => &accum[..header_end],
                };
                let request_line = head.split(|b| *b == b'\n').next().unwrap_or(head);
                !request_ends_connection(request_line, &scratch.request.headers)
            };

            // Chunked trailer headers (RFC 7230 §4.1.2) need no
            // extra promotion here: `splice_canonical` splices them
            // into the canonical header section, so the eager parse
            // above already merged them into `request.headers` with
            // the interp's lowercase/dedupe semantics.

            // SAFETY: `fn_addr` came from `gos_fn_addr("T::serve")`
            // at the user's `http::serve(addr, app)` call site;
            // env_addr is the `&app` pointer passed alongside.
            // The address is recovered through the exposed-provenance API the
            // rest of the C-ABI uses, so the pointer it produces carries the
            // provenance the ptr-to-int cast at the call site exposed.
            let handler: HandlerFn = unsafe {
                std::mem::transmute::<*const (), HandlerFn>(std::ptr::with_exposed_provenance(
                    fn_addr,
                ))
            };
            let env_ptr = env_addr as *mut u8;
            let req_ptr: *mut GosHttpRequest = &raw mut scratch.request;
            // A panicking handler is a server fault, not the program's: the
            // client gets a 500 and the operator gets the record naming the
            // request, exactly as the bytecode VM reports it.
            let result_ptr =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    handler(env_ptr, req_ptr)
                })) {
                    Ok(result) => result,
                    Err(payload) => {
                        report_request_panic(&payload, &scratch.request);
                        end_request_context(&mut scratch.request, &watch_ctx);
                        scratch.response_buf.extend_from_slice(RESPONSE_500_BYTES);
                        unsafe { gos_rt_gc_reset() };
                        if conn.write_all(&scratch.response_buf).is_err() {
                            return;
                        }
                        accum.drain(..req_end);
                        continue_sent = false;
                        continue;
                    }
                };
            if let Some(handle) = streamed_ok_handle(result_ptr) {
                end_request_context(&mut scratch.request, &watch_ctx);
                // Streamed response (`Response::stream`): write the
                // head, then drain the upstream reader straight to
                // the connection in chunked frames - no buffering.
                extract_stream_head_into(result_ptr, &mut scratch.response_buf);
                unsafe { drop_handler_result(result_ptr) };
                unsafe { gos_rt_gc_reset() };
                if conn.write_all(&scratch.response_buf).is_err() {
                    return;
                }
                if !drain_stream_chunked(conn, handle) {
                    // Mid-stream failure: close WITHOUT the terminal
                    // frame so the client detects truncation
                    // (RFC 7230 §4.1 - an unterminated chunked body
                    // is incomplete). Mirrors the std server.
                    return;
                }
                if !keep_alive {
                    return;
                }
                accum.drain(..req_end);
                continue_sent = false;
                continue;
            }
            if !extract_response_into(result_ptr, &mut scratch.response_buf, &mut keep_alive) {
                report_request_error(result_ptr, &scratch.request);
                scratch.response_buf.extend_from_slice(if keep_alive {
                    RESPONSE_500_BYTES
                } else {
                    RESPONSE_500_CLOSE_BYTES
                });
            }
            unsafe { drop_handler_result(result_ptr) };
            end_request_context(&mut scratch.request, &watch_ctx);

            // Legacy collector hook. RequestArenaGuard owns real arena
            // cleanup and deliberately remains live until the response has
            // been written, so a suspended handler can never have its live
            // region reset by a later request on this connection.
            unsafe { gos_rt_gc_reset() };
        }
        if conn.write_all(&scratch.response_buf).is_err() {
            return;
        }
        if !keep_alive {
            return;
        }
        // Drop the consumed request from the accumulator while
        // preserving any pipelined remainder. `drain` shifts the
        // tail into place; capacity is retained.
        accum.drain(..req_end);
        continue_sent = false;
    }
}

/// Reports a handler that answered `Err`, or answered something that is
/// not a response, in the `slog` record shape every tier's server path
/// uses. The error's own message reaches the operator; the client still
/// gets the bare 500 that leaks nothing about the fault.
fn report_request_error(result: i128, req: &GosHttpRequest) {
    let message = if crate::c_abi::vec::gos_rt_result_disc(result) == 0 {
        "handler did not return http::Response".to_string()
    } else {
        let err = crate::c_abi::vec::gos_rt_result_payload(result)
            as *const crate::c_abi::errors::GosError;
        crate::c_abi::errors::error_chain_text(err)
    };
    crate::c_abi::slog::emit_json_line(
        "ERROR",
        "http: handler failed",
        &[
            ("method", req.method.as_str()),
            ("path", req.url_path_only()),
            ("status", "500"),
            ("error", &message),
        ],
    );
}

/// The raw socket a peer probe reads from, as each platform names it.
#[cfg(unix)]
pub type RawPeerSocket = std::os::fd::RawFd;

/// The raw socket a peer probe reads from, as each platform names it.
#[cfg(windows)]
pub type RawPeerSocket = std::os::windows::io::RawSocket;

/// The raw socket a peer probe reads from, as each platform names it.
#[cfg(not(any(unix, windows)))]
pub type RawPeerSocket = i32;

/// Whether the peer closed its side of `socket`, by a zero-length peek.
///
/// `MSG_PEEK` leaves whatever is queued in place, so a pipelined request
/// still arrives intact, and `MSG_DONTWAIT` keeps the probe from blocking
/// on a peer that is merely quiet. Only a zero-byte answer means the peer
/// is gone. Answers `false` on a platform with no such probe, where a
/// request simply runs to completion after its client leaves.
#[cfg(unix)]
#[allow(
    unsafe_code,
    reason = "probing a socket needs a raw recv with no safe wrapper"
)]
#[must_use]
pub fn peer_is_gone(socket: RawPeerSocket) -> bool {
    let mut probe = [0u8; 1];
    // SAFETY: `socket` is a live socket owned by the caller for the
    // duration of the call, and the buffer outlives it.
    let seen = unsafe {
        libc::recv(
            socket,
            probe.as_mut_ptr().cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    seen == 0
}

/// Whether the peer closed its side of `socket`, by a zero-length peek.
///
/// Winsock has no `MSG_DONTWAIT`, and putting the socket in non-blocking
/// mode would change it for the connection thread too, so readiness comes
/// from a `select` with a zero timeout instead: a socket that is not
/// readable has neither data nor an end-of-stream waiting, and a readable
/// one answers the `MSG_PEEK` immediately without consuming the byte.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "probing a socket needs a raw recv with no safe wrapper"
)]
#[must_use]
pub fn peer_is_gone(socket: RawPeerSocket) -> bool {
    use windows_sys::Win32::Networking::WinSock::{FD_SET, MSG_PEEK, TIMEVAL, recv, select};

    let handle = socket as windows_sys::Win32::Networking::WinSock::SOCKET;
    let mut readable = FD_SET {
        fd_count: 1,
        fd_array: [0; 64],
    };
    readable.fd_array[0] = handle;
    let immediate = TIMEVAL {
        tv_sec: 0,
        tv_usec: 0,
    };
    // SAFETY: `socket` is a live socket owned by the caller for the
    // duration of the call, and both structures outlive it. Winsock
    // ignores `nfds`.
    let ready = unsafe {
        select(
            0,
            &raw mut readable,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw const immediate,
        )
    };
    if ready <= 0 {
        return false;
    }
    let mut probe = [0u8; 1];
    // SAFETY: as above; the probe buffer outlives the call and
    // `MSG_PEEK` leaves whatever is queued in place.
    let seen = unsafe { recv(handle, probe.as_mut_ptr(), 1, MSG_PEEK) };
    seen == 0
}

#[cfg(not(any(unix, windows)))]
#[must_use]
pub fn peer_is_gone(_socket: RawPeerSocket) -> bool {
    false
}

/// One connection's socket and the slot naming the context to cancel when
/// its peer goes away.
///
/// The slot holds the in-flight request's context, or zero between requests.
/// Registration is per connection, so a request's own path writes one relaxed
/// store rather than taking the registry lock.
#[cfg(any(unix, windows))]
struct PeerWatchEntry {
    id: u64,
    socket: RawPeerSocket,
    ctx: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// Every request currently being watched, and the id the next one takes.
#[cfg(any(unix, windows))]
#[derive(Default)]
struct PeerWatchState {
    entries: Vec<PeerWatchEntry>,
    next_id: u64,
    watcher_running: bool,
}

#[cfg(any(unix, windows))]
static PEER_WATCH: std::sync::LazyLock<parking_lot::Mutex<PeerWatchState>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(PeerWatchState::default()));

/// Signalled when the first entry of an idle round is registered, so the
/// watcher parks instead of waking on an empty set.
#[cfg(any(unix, windows))]
static PEER_WATCH_WAKE: parking_lot::Condvar = parking_lot::Condvar::new();

/// Watches an accepted socket for the peer going away while a request is
/// in flight, and cancels that request's context when it does.
///
/// The connection thread is inside the handler and cannot also read, so the
/// probe is a zero-length peek from elsewhere: it never consumes a byte, so
/// a pipelined successor still arrives intact. A client that aborts
/// therefore stops the work it asked for instead of paying for it to finish.
///
/// One process-wide thread does the probing for every watched connection.
/// Registration happens once, when the connection is accepted, and the slot
/// it returns is what a request publishes its context into - so serving a
/// request costs a relaxed store rather than a turn of a process-wide lock.
/// A thread per connection would cost a stack mapping, a clone, and a join,
/// and those mappings serialise on the address space across every connection
/// the server is serving.
#[cfg(any(unix, windows))]
fn watch_for_disconnect(
    socket: RawPeerSocket,
    ctx: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> Option<DisconnectWatch> {
    let mut state = PEER_WATCH.lock();
    if !state.watcher_running {
        // Started on the first watched request, so a program that serves
        // nothing never carries the thread.
        if std::thread::Builder::new()
            .name("gos-http-peer-watch".to_string())
            .spawn(peer_watch_loop)
            .is_err()
        {
            return None;
        }
        state.watcher_running = true;
    }
    let id = state.next_id;
    state.next_id = state.next_id.wrapping_add(1);
    let was_idle = state.entries.is_empty();
    state.entries.push(PeerWatchEntry { id, socket, ctx });
    drop(state);
    if was_idle {
        PEER_WATCH_WAKE.notify_one();
    }
    Some(DisconnectWatch { id })
}

/// Probes every watched connection that has a request in flight once per
/// interval, cancelling the context of any whose peer has left.
///
/// Probing happens under the registry lock, and a connection deregisters under
/// that same lock while it still owns its socket. A descriptor is therefore
/// never probed after the connection that registered it has ended, so a
/// recycled one is never mistaken for the socket it replaced. A connection
/// between requests publishes a zero context and is left alone: its own next
/// read is what observes the close.
#[cfg(any(unix, windows))]
fn peer_watch_loop() {
    use std::sync::atomic::Ordering;
    loop {
        {
            let mut state = PEER_WATCH.lock();
            while state.entries.is_empty() {
                PEER_WATCH_WAKE.wait(&mut state);
            }
            for entry in &state.entries {
                let ctx = entry.ctx.load(Ordering::Relaxed);
                if ctx != 0 && peer_is_gone(entry.socket) {
                    // The connection clears its own slot when the request
                    // ends, so a context is cancelled at most once here.
                    if entry
                        .ctx
                        .compare_exchange(ctx, 0, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                    {
                        crate::c_abi::context::close_request_context(ctx);
                    }
                }
            }
        }
        crate::platform::sleep(std::time::Duration::from_millis(PEER_WATCH_INTERVAL_MS));
    }
}

/// How long the watcher waits between rounds of probes. It only bounds how
/// late a disconnect is noticed; a request that ends first deregisters and
/// is never probed again.
const PEER_WATCH_INTERVAL_MS: u64 = 20;

/// Deregisters the connection from the peer watch when it closes.
#[cfg(any(unix, windows))]
struct DisconnectWatch {
    id: u64,
}

#[cfg(any(unix, windows))]
impl Drop for DisconnectWatch {
    fn drop(&mut self) {
        let mut state = PEER_WATCH.lock();
        if let Some(at) = state.entries.iter().position(|entry| entry.id == self.id) {
            state.entries.swap_remove(at);
        }
    }
}

/// Opens `request`'s context on first use, publishing it to the peer watch
/// so a client that leaves still cancels it. `None` for a request the server
/// is not serving, which has nothing to bound it.
pub(crate) fn open_served_request_context(request: &GosHttpRequest) -> Option<usize> {
    let site = request.context_site.as_ref()?;
    let ctx = crate::c_abi::context::open_request_context_at(site.deadline);
    site.watch.store(ctx, std::sync::atomic::Ordering::Release);
    Some(ctx)
}

/// Cancels and retires the request's context.
///
/// Runs on every path out of a served request, so a handler that spawned
/// work under the context does not leave it running once the request it
/// belongs to is over.
fn cancel_request_context(request: &mut GosHttpRequest) {
    let ctx = std::mem::replace(&mut request.context, 0);
    if ctx != 0 {
        crate::c_abi::context::close_request_context(ctx);
    }
}

/// Ends the request's context and stops the peer watch from naming it.
///
/// The slot is cleared first, so the watcher cannot pick up a context this
/// call is about to close.
fn end_request_context(request: &mut GosHttpRequest, watch_ctx: &std::sync::atomic::AtomicUsize) {
    watch_ctx.store(0, std::sync::atomic::Ordering::Release);
    request.context_site = None;
    cancel_request_context(request);
}

/// Reports a handler panic in the `slog` record shape every tier's server
/// path uses, naming the request that provoked it.
fn report_request_panic(payload: &Box<dyn std::any::Any + Send>, req: &GosHttpRequest) {
    let message = payload
        .downcast_ref::<&'static str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .or_else(|| {
            payload
                .downcast_ref::<gossamer_coro::GosPanic>()
                .map(|p| p.0.clone())
        })
        .unwrap_or_else(|| "(non-string panic payload)".to_string());
    crate::c_abi::slog::emit_json_line(
        "ERROR",
        "http: handler failed",
        &[
            ("method", req.method.as_str()),
            ("path", req.url_path_only()),
            ("status", "500"),
            ("error", &message),
        ],
    );
}

/// True when the request signals `Expect: 100-continue` (RFC 9110
/// §10.1.1), so the server should grant the interim response before the
/// body is read.
fn request_expects_continue(header_section: &[u8]) -> bool {
    header_value(header_section, b"expect").is_some_and(|v| v.eq_ignore_ascii_case(b"100-continue"))
}

/// Connection wrapper that bridges a non-blocking [`TcpStream`] to
/// the global netpoller. Reads and writes that would block register
/// interest with [`crate::sched_global`] and park the calling
/// goroutine on a Condvar; the netpoller wakes the waker when the
/// kernel reports readiness.
#[cfg(test)]
pub(crate) struct HttpConn {
    stream: TcpStream,
    mio_stream: mio::net::TcpStream,
}

#[cfg(test)]
impl HttpConn {
    pub(crate) fn wrap(stream: TcpStream) -> Option<Self> {
        // The std handle drives I/O while a clone is registered with mio.
        // Non-blocking mode is shared by both handles on supported socket
        // platforms, so a WouldBlock parks only this goroutine.
        if stream.set_nonblocking(true).is_err() {
            return None;
        }
        let cloned = stream.try_clone().ok()?;
        Some(Self {
            mio_stream: mio::net::TcpStream::from_std(cloned),
            stream,
        })
    }

    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let deadline = http_read_timeout_ms()
            .map(|ms| crate::platform::Instant::now() + std::time::Duration::from_millis(ms));
        loop {
            match std::io::Read::read(&mut self.stream, buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Some(deadline) = deadline {
                        if !self.wait_until(crate::sched::Interest::Readable, deadline)? {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "HTTP connection read timed out",
                            ));
                        }
                    } else {
                        self.wait(crate::sched::Interest::Readable)?;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn write_all(&mut self, mut buf: &[u8]) -> std::io::Result<()> {
        let deadline = http_write_timeout_ms()
            .map(|ms| crate::platform::Instant::now() + std::time::Duration::from_millis(ms));
        while !buf.is_empty() {
            match std::io::Write::write(&mut self.stream, buf) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "wrote zero bytes",
                    ));
                }
                Ok(n) => buf = &buf[n..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Some(deadline) = deadline {
                        if !self.wait_until(crate::sched::Interest::Writable, deadline)? {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "HTTP connection write timed out",
                            ));
                        }
                    } else {
                        self.wait(crate::sched::Interest::Writable)?;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn wait(&mut self, interest: crate::sched::Interest) -> std::io::Result<()> {
        // Goroutine-aware wait: park the calling coroutine on the
        // netpoller's readiness signal. The worker thread is freed
        // to run other goroutines while we wait. When called from
        // a non-goroutine OS thread (e.g. tooling code), the helper
        // falls back to a brief OS-thread sleep.
        crate::sched_global::wait_io(&mut self.mio_stream, interest)
    }

    fn wait_until(
        &mut self,
        interest: crate::sched::Interest,
        deadline: crate::platform::Instant,
    ) -> std::io::Result<bool> {
        crate::sched_global::wait_io_until(&mut self.mio_stream, interest, deadline)
    }
}

// `rustls::StreamOwned` and the WebSocket framing crate intentionally depend
// only on the standard `Read` / `Write` traits. Implementing those traits on
// the scheduler-aware adapter lets both protocols use the same non-blocking
// socket and park only their goroutine when a peer is slow.
#[cfg(test)]
impl std::io::Read for HttpConn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        Self::read(self, buf)
    }
}

#[cfg(test)]
impl std::io::Write for HttpConn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Self::write_all(self, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Returns the index *one past* the trailing `\r\n\r\n` of the
/// first complete header section in `buf`, or `None` when the
/// buffer doesn't yet contain a full request header.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    let needle = b"\r\n\r\n";
    buf.windows(4).position(|w| w == needle).map(|p| p + 4)
}

/// Drops the `GosHttpResponse` referenced by the handler's
/// `Result` so each request doesn't leak. Every response reaching
/// here was Box-allocated (`gos_rt_http_response_text_new` /
/// `_json_new` use `Box::into_raw`); this is the unique reclaim
/// site. The gos-allocated body c-string is freed explicitly via
/// `gos_rt_str_free`, then `Box::from_raw` drops the struct,
/// structurally freeing `headers`, `body_bytes`, and
/// `content_type`. A null pointer or an `Err` result is a no-op.
pub(crate) unsafe fn drop_handler_result(result: i128) {
    if super::vec::gos_rt_result_disc(result) == 0 {
        let response_ptr = super::vec::gos_rt_result_payload(result) as *mut GosHttpResponse;
        unsafe { crate::c_abi::http_client::gos_rt_http_response_free(response_ptr) };
    }
    // Result is now a 2-word by-value `i128` (no heap box), so there is
    // nothing to free here - this is exactly the per-request box leak that
    // the by-value representation eliminated everywhere.
}

/// Returns the trimmed value of the first `name` header in a raw
/// header section (request line + headers, up to the trailing
/// `\r\n\r\n`), or `None` when absent. Header-name match is
/// case-insensitive per RFC 7230 §3.2.
pub(crate) fn header_value<'a>(header_section: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    for line in header_section.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        if line[..colon].eq_ignore_ascii_case(name) {
            return Some(line[colon + 1..].trim_ascii());
        }
    }
    None
}

/// Parses the `Content-Length` value out of a raw header section.
/// Returns 0 when the header is absent or unparseable - the
/// no-body fast path.
fn content_length(header_section: &[u8]) -> usize {
    header_value(header_section, b"content-length")
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// True when the request declares `Transfer-Encoding: chunked`.
/// Mirrors the interp server's detection (gossamer-std http.rs
/// `read_request_body`): the whole value must equal `chunked`
/// case-insensitively, so multi-coding values like `gzip, chunked`
/// are not treated as chunked on either tier.
fn transfer_encoding_is_chunked(header_section: &[u8]) -> bool {
    header_value(header_section, b"transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case(b"chunked"))
}

/// A fully decoded inbound chunked request, ready for dispatch.
struct ChunkedRequest {
    /// One past the final CRLF of the chunked frame in the
    /// accumulator - everything beyond it is the next pipelined
    /// request and must stay in the accumulator.
    raw_end: usize,
    /// Canonical rewrite of the request: the original header
    /// section with trailer headers spliced in before the blank
    /// line, followed by the de-chunked body. This is exactly the
    /// buffer shape `parse_request_into` stores, so the body
    /// accessors (`gos_rt_http_request_body_str`,
    /// `gos_rt_http_request_raw_body`) see clean payload bytes.
    canonical: Vec<u8>,
    /// Offset of the body within `canonical` (one past `\r\n\r\n`).
    header_end: usize,
}

/// Outcome of scanning the accumulator for a complete chunked frame.
enum ChunkedAssembly {
    /// The frame has not terminated yet - read more bytes.
    Incomplete,
    /// Protocol violation or cap breach: write these response
    /// bytes and close the connection.
    Reject(&'static [u8]),
    /// Frame complete and decoded.
    Ready(ChunkedRequest),
}

/// Finds the `\r\n` terminating a line that starts at `from`,
/// scanning at most `cap` bytes of line content. Returns the
/// absolute index of the `\r`, or `None` when no CRLF lies within
/// the window (more data needed, or the line is over `cap`).
fn find_crlf(buf: &[u8], from: usize, cap: usize) -> Option<usize> {
    let end = buf.len().min(from.saturating_add(cap).saturating_add(2));
    buf.get(from..end)?
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|p| from + p)
}

/// Decodes the chunked frame that starts at `header_end` in
/// `accum` (RFC 7230 §4.1): hex size lines with optional ignored
/// chunk extensions, CRLF-framed data, terminal 0-chunk, then a
/// trailer block up to a blank line. The decoded body is capped at
/// `max_body` - declared chunk sizes count against the cap the
/// moment the size line is parsed, so a hostile declaration is
/// rejected before its data arrives. The still-encoded frame is
/// capped at [`ServerLimits::max_chunked_raw_bytes`] while incomplete.
///
/// The scan restarts from `header_end` on every call; cost is
/// bounded by the caps, and chunked uploads are not the keep-alive
/// fast path.
fn assemble_chunked_request(
    accum: &[u8],
    header_end: usize,
    limits: &ServerLimits,
) -> ChunkedAssembly {
    let max_body = limits.max_body_bytes;
    let max_raw = limits.max_chunked_raw_bytes();
    let incomplete = || {
        if accum.len() - header_end > max_raw {
            ChunkedAssembly::Reject(RESPONSE_413_BYTES)
        } else {
            ChunkedAssembly::Incomplete
        }
    };
    let mut pos = header_end;
    let mut body: Vec<u8> = Vec::new();
    loop {
        let Some(line_end) = find_crlf(accum, pos, MAX_CHUNK_SIZE_LINE_BYTES) else {
            return if accum.len() - pos > MAX_CHUNK_SIZE_LINE_BYTES {
                ChunkedAssembly::Reject(RESPONSE_400_BYTES)
            } else {
                incomplete()
            };
        };
        let line = &accum[pos..line_end];
        // Chunk extensions (after `;`) are ignored, as in the
        // interp decoder.
        let size_part = line
            .split(|&b| b == b';')
            .next()
            .unwrap_or(line)
            .trim_ascii();
        // 16 hex digits mirrors the interp decoder's
        // `max_size_digits`; anything longer is malformed.
        if size_part.is_empty() || size_part.len() > 16 {
            return ChunkedAssembly::Reject(RESPONSE_400_BYTES);
        }
        let Some(size) = std::str::from_utf8(size_part)
            .ok()
            .and_then(|s| u64::from_str_radix(s, 16).ok())
        else {
            return ChunkedAssembly::Reject(RESPONSE_400_BYTES);
        };
        // u128 arithmetic: `size` can be up to 2^64-1 from a
        // hostile declaration; the sum must not wrap before the
        // cap comparison.
        if body.len() as u128 + u128::from(size) > max_body as u128 {
            return ChunkedAssembly::Reject(RESPONSE_413_BYTES);
        }
        let size = size as usize;
        pos = line_end + 2;
        if size == 0 {
            let trailer_start = pos;
            let mut trailers: Vec<(String, String)> = Vec::new();
            loop {
                let Some(line_end) = find_crlf(accum, pos, MAX_TRAILER_BYTES) else {
                    return if accum.len() - trailer_start > MAX_TRAILER_BYTES {
                        ChunkedAssembly::Reject(RESPONSE_400_BYTES)
                    } else {
                        incomplete()
                    };
                };
                if line_end + 2 - trailer_start > MAX_TRAILER_BYTES {
                    return ChunkedAssembly::Reject(RESPONSE_400_BYTES);
                }
                if line_end == pos {
                    let raw_end = line_end + 2;
                    let (canonical, canonical_header_end) =
                        splice_canonical(&accum[..header_end], &body, &trailers);
                    return ChunkedAssembly::Ready(ChunkedRequest {
                        raw_end,
                        canonical,
                        header_end: canonical_header_end,
                    });
                }
                // Trailer lines without a colon are ignored -
                // interp `read_trailers_block` parity.
                if let Ok(text) = std::str::from_utf8(&accum[pos..line_end])
                    && let Some((name, value)) = text.split_once(':')
                {
                    trailers.push((name.trim().to_string(), value.trim().to_string()));
                }
                pos = line_end + 2;
            }
        }
        let data_end = pos + size;
        if accum.len() < data_end + 2 {
            return incomplete();
        }
        if &accum[data_end..data_end + 2] != b"\r\n" {
            return ChunkedAssembly::Reject(RESPONSE_400_BYTES);
        }
        body.extend_from_slice(&accum[pos..data_end]);
        pos = data_end + 2;
    }
}

/// Rewrites a chunked request into the canonical buffer shape
/// `parse_request_into` expects: the header section (sans its
/// blank line) + trailer headers + blank line + de-chunked body.
/// Returns the buffer and the body offset within it. The
/// `Transfer-Encoding` header line is kept verbatim, matching the
/// interp server (which leaves it in `request.headers`).
fn splice_canonical(head: &[u8], body: &[u8], trailers: &[(String, String)]) -> (Vec<u8>, usize) {
    let trailer_len: usize = trailers.iter().map(|(k, v)| k.len() + v.len() + 4).sum();
    let mut out = Vec::with_capacity(head.len() + trailer_len + body.len());
    // `head` ends with the blank-line `\r\n\r\n`; keep the last
    // header's CRLF, drop the blank line, splice trailers, close.
    out.extend_from_slice(&head[..head.len() - 2]);
    for (k, v) in trailers {
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    let header_end = out.len();
    out.extend_from_slice(body);
    (out, header_end)
}

/// Parses `raw` (header section + body; `header_end` is one past
/// the `\r\n\r\n`) into `request` in place. Returns false on
/// malformed input. The request line and the header lines are both
/// extracted eagerly: `request.headers` must mirror the interp
/// server's `Headers` map (lowercase names, trimmed values,
/// last-wins dedupe, sorted by name) so `r.headers` /
/// `r.headers.get(..)` agree across tiers. The whole raw buffer is
/// stashed in `request.body` with `body_offset` marking the body
/// start, so the body accessors (`gos_rt_http_request_body_str`,
/// `gos_rt_http_request_raw_body`) slice it out without another
/// copy. The body may be arbitrary binary; only the header section
/// must be UTF-8.
fn parse_request_into(raw: &[u8], header_end: usize, request: &mut GosHttpRequest) -> bool {
    let header_end = header_end.min(raw.len());
    let Ok(text) = std::str::from_utf8(&raw[..header_end]) else {
        return false;
    };
    let Some(request_line_end) = text.find("\r\n") else {
        return false;
    };
    let request_line = &text[..request_line_end];
    let mut parts = request_line.split_whitespace();
    let Some(method) = parts.next() else {
        return false;
    };
    let Some(url) = parts.next() else {
        return false;
    };
    request.method.push_str(method);
    request.url.push_str(url);
    for line in text[request_line_end + 2..].split("\r\n") {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            request
                .headers
                .push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    normalize_header_bag(&mut request.headers);
    request.body.extend_from_slice(raw);
    request.body_offset = header_end;
    true
}

/// Collapses a parsed header list to the interp `Headers` map view:
/// name sort, then last-wins dedupe of equal names (the BTreeMap
/// insert-overwrite semantics the interp server applies).
pub(crate) fn normalize_header_bag(headers: &mut Vec<(String, String)>) {
    // Stable sort keeps insertion order within equal names, so the
    // last element of each equal-name run is the latest occurrence.
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let mut write = 0;
    for read in 0..headers.len() {
        if read + 1 < headers.len() && headers[read + 1].0 == headers[read].0 {
            continue;
        }
        headers.swap(write, read);
        write += 1;
    }
    headers.truncate(write);
}

/// A header value is safe to write only if it carries no CR, LF, or
/// NUL - the bytes that would terminate the line and split the
/// response. Mirrors the interpreter server's gate.
fn is_valid_header_value(value: &str) -> bool {
    !value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

/// A header name is safe only if it is a non-empty token with no
/// CR/LF/NUL and no framing characters (`:`, space).
fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .bytes()
            .any(|b| b == b'\r' || b == b'\n' || b == 0 || b == b':' || b == b' ')
}

/// Writes `result`'s response payload (status + headers +
/// body) into `out` as raw HTTP/1.1 bytes. Returns false if
/// `result` doesn't carry a valid OK response.
/// Default `Server` identity, byte-identical to the interp tier
/// (`gossamer-std/src/http.rs` Config default).
const SERVER_HEADER: &str = concat!("gossamer/", env!("CARGO_PKG_VERSION"));

// The rendered `Date` header and the second it names, per connection thread.
// An HTTP-date has one-second resolution, so every response inside a second
// carries the same one and the civil-time conversion runs once for all of
// them rather than once each.
thread_local! {
    static HTTP_DATE_CACHE: std::cell::RefCell<(i64, String)> =
        const { std::cell::RefCell::new((i64::MIN, String::new())) };
}

/// Appends the current HTTP-date to `out` without rendering or copying it
/// into a String of its own.
fn append_http_date_now(out: &mut Vec<u8>) {
    let secs = crate::platform::system_time_now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    HTTP_DATE_CACHE.with_borrow_mut(|(cached_secs, cached)| {
        if *cached_secs != secs {
            *cached = render_http_date(secs);
            *cached_secs = secs;
        }
        out.extend_from_slice(cached.as_bytes());
    });
}

/// A Unix second as an RFC 1123 / RFC 9110 HTTP-date
/// (`Sun, 06 Nov 1994 08:49:37 GMT`). Uses the same civil-time conversion as
/// `gos_rt_time_format_rfc3339`, so the rendering matches the interp tier's
/// `time::format_rfc1123_gmt` byte for byte.
fn render_http_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let is_leap = |yr: i64| (yr % 4 == 0 && yr % 100 != 0) || yr % 400 == 0;
    let year_days = |yr: i64| if is_leap(yr) { 366 } else { 365 };
    let mut year = 1970_i64;
    let mut remain = days;
    while remain >= year_days(year) {
        remain -= year_days(year);
        year += 1;
    }
    let dim = |m: i64, yr: i64| -> i64 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if is_leap(yr) => 29,
            2 => 28,
            _ => 30,
        }
    };
    let mut month = 1_i64;
    while remain >= dim(month, year) {
        remain -= dim(month, year);
        month += 1;
    }
    let day = remain + 1;
    let tod = secs.rem_euclid(86_400);
    let dow =
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][((days + 4).rem_euclid(7)) as usize];
    let mo = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(month - 1) as usize];
    format!(
        "{dow}, {day:02} {mo} {year:04} {h:02}:{mi:02}:{s:02} GMT",
        h = tod / 3600,
        mi = (tod % 3600) / 60,
        s = tod % 60,
    )
}

/// Whether this request's connection ends with its response.
///
/// HTTP/1.1 keeps a connection open unless the request says otherwise and
/// HTTP/1.0 ends it unless the request asks to keep it, so a client that
/// wrote `Connection: close` is answered on a connection that closes. The
/// header carries a comma-separated token list, so a token is matched
/// rather than the whole value.
pub(crate) fn request_ends_connection(request_line: &[u8], headers: &[(String, String)]) -> bool {
    let mut close = false;
    let mut keep = false;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("connection") {
            continue;
        }
        for token in value.split(',') {
            let token = token.trim();
            close |= token.eq_ignore_ascii_case("close");
            keep |= token.eq_ignore_ascii_case("keep-alive");
        }
    }
    if close {
        return true;
    }
    let http_1_0 = request_line
        .windows(8)
        .any(|w| w.eq_ignore_ascii_case(b"HTTP/1.0"));
    http_1_0 && !keep
}

pub(crate) fn extract_response_into(
    result: i128,
    out: &mut Vec<u8>,
    keep_alive: &mut bool,
) -> bool {
    if super::vec::gos_rt_result_disc(result) != 0 {
        return false;
    }
    let response_ptr = super::vec::gos_rt_result_payload(result) as *const GosHttpResponse;
    if response_ptr.is_null() {
        return false;
    }
    let response = unsafe { &*response_ptr };
    // Streamed responses are handled by the h1 server's chunked
    // drain before this function is reached. Callers that buffer
    // (the h2c bridge) serve the empty `body` instead - release the
    // pending reader here so the upstream connection closes rather
    // than leaking in the registry, matching the interp tier.
    if response.stream_handle >= 0 {
        drop(super::http_client::stream_take_for_serve(
            response.stream_handle,
        ));
    }
    // Prefer the exact byte body when present: `body_bytes` is set
    // for byte-array bodies (`gos_rt_http_response_set_body_bytes`)
    // and client-lifted responses, and may legally contain NUL bytes
    // that the c-string `body` mirror truncates at.
    let body_bytes: &[u8] = match &response.body_bytes {
        Some(bytes) => bytes.as_slice(),
        None if response.body.is_null() => b"",
        None => unsafe { crate::c_abi::gos_str_arg_bytes(response.body.as_ptr()) },
    };
    out.extend_from_slice(b"HTTP/1.1 ");
    let mut buf = itoa::Buffer::new();
    out.extend_from_slice(buf.format(response.status).as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(status_reason(response.status).as_bytes());
    out.extend_from_slice(b"\r\n");
    let mut has_content_length = false;
    let mut has_content_type = false;
    let mut has_date = false;
    let mut has_server = false;
    let mut has_connection = false;
    for (k, v) in &response.headers {
        // Never emit a header whose name or value carries a CR, LF, or
        // NUL - those bytes would split the response and let an attacker
        // inject headers or a body (HTTP response splitting). Drop the
        // malformed header rather than write it, matching the interp
        // server so untrusted input reflected into a header or cookie
        // cannot smuggle a new line onto the wire.
        if !is_valid_header_name(k) || !is_valid_header_value(v) {
            continue;
        }
        if k.eq_ignore_ascii_case("content-length") {
            has_content_length = true;
        }
        if k.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        if k.eq_ignore_ascii_case("date") {
            has_date = true;
        }
        if k.eq_ignore_ascii_case("server") {
            has_server = true;
        }
        // A handler naming the connection decides it: the response carries
        // its header alone, and the connection ends with it when it says so.
        if k.eq_ignore_ascii_case("connection") {
            has_connection = true;
            *keep_alive &= !v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close"));
        }
        // Canonical wire casing is lowercase on every tier: the
        // interp server writes through the lowercase-keyed
        // `Headers` map, so handler-given names normalize here.
        out.extend_from_slice(k.to_ascii_lowercase().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    // RFC 9110 origin-server headers, matching the interp tier: a Date
    // (unless the handler set one) and a Server identity.
    if !has_date {
        out.extend_from_slice(b"date: ");
        append_http_date_now(out);
        out.extend_from_slice(b"\r\n");
    }
    if !has_server {
        out.extend_from_slice(b"server: ");
        out.extend_from_slice(SERVER_HEADER.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !has_content_type {
        // Precedence: explicit header (above) > constructor-recorded
        // content type > the interp tier's default of text/plain.
        out.extend_from_slice(b"content-type: ");
        if response.content_type.is_empty() {
            out.extend_from_slice(b"text/plain; charset=utf-8");
        } else {
            out.extend_from_slice(response.content_type.as_bytes());
        }
        out.extend_from_slice(b"\r\n");
    }
    if !has_content_length {
        out.extend_from_slice(b"content-length: ");
        out.extend_from_slice(buf.format(body_bytes.len()).as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !has_connection {
        out.extend_from_slice(if *keep_alive {
            b"connection: keep-alive\r\n".as_slice()
        } else {
            b"connection: close\r\n".as_slice()
        });
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body_bytes);
    true
}

/// A handler response decomposed into `(status, headers, body)` for
/// transports that frame the response themselves (HTTP/3, HTTP/2).
pub(crate) type StructuredResponse = (u16, Vec<(String, String)>, Vec<u8>);

/// Extracts a handler `result`'s response as a [`StructuredResponse`]
/// for transports that frame the response themselves (HTTP/3, HTTP/2)
/// rather than emitting HTTP/1.1 wire bytes. Header names are
/// lowercased and CR/LF/NUL-bearing headers are dropped with the same
/// gate as [`extract_response_into`]; an absent content-type is
/// defaulted identically so the body the handler returned is served
/// byte-for-byte across tiers. Returns `None` when `result` is an
/// `Err` or carries a null response.
pub(crate) fn extract_response_struct(result: i128) -> Option<StructuredResponse> {
    if super::vec::gos_rt_result_disc(result) != 0 {
        return None;
    }
    let response_ptr = super::vec::gos_rt_result_payload(result) as *const GosHttpResponse;
    if response_ptr.is_null() {
        return None;
    }
    let response = unsafe { &*response_ptr };
    // A streamed response cannot be framed by the buffered h3 path;
    // release the pending reader so the upstream connection closes
    // rather than leaking, matching the h2c bridge and the interp
    // tier. The buffered `body` (empty for a pure stream) is served.
    if response.stream_handle >= 0 {
        drop(super::http_client::stream_take_for_serve(
            response.stream_handle,
        ));
    }
    let body: Vec<u8> = match &response.body_bytes {
        Some(bytes) => bytes.clone(),
        None if response.body.is_null() => Vec::new(),
        None => unsafe { crate::c_abi::gos_str_arg_bytes(response.body.as_ptr()) }.to_vec(),
    };
    let mut headers: Vec<(String, String)> = Vec::with_capacity(response.headers.len() + 1);
    let mut has_content_type = false;
    for (k, v) in &response.headers {
        if !is_valid_header_name(k) || !is_valid_header_value(v) {
            continue;
        }
        if k.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        headers.push((k.to_ascii_lowercase(), v.clone()));
    }
    if !has_content_type {
        let ct = if response.content_type.is_empty() {
            "text/plain; charset=utf-8".to_string()
        } else {
            response.content_type.clone().into_owned()
        };
        headers.push(("content-type".to_string(), ct));
    }
    Some((response.status as u16, headers, body))
}

/// Returns the stream-registry handle when `result` is an Ok
/// response built by `gos_rt_http_response_stream_new`
/// (`stream_handle >= 0`); `None` for buffered responses and errors.
fn streamed_ok_handle(result: i128) -> Option<i64> {
    if super::vec::gos_rt_result_disc(result) != 0 {
        return None;
    }
    let response_ptr = super::vec::gos_rt_result_payload(result) as *const GosHttpResponse;
    if response_ptr.is_null() {
        return None;
    }
    let handle = unsafe { (*response_ptr).stream_handle };
    (handle >= 0).then_some(handle)
}

/// Writes the head of a streamed response (status line + headers +
/// `Transfer-Encoding: chunked` + keep-alive) into `out`. Handler
/// headers are honored with the same content-type precedence as
/// `extract_response_into`; any handler-set `Content-Length` or
/// `Transfer-Encoding` is dropped - chunked framing is unconditional
/// and RFC 7230 §3.3.3 forbids carrying both.
fn extract_stream_head_into(result: i128, out: &mut Vec<u8>) {
    let response_ptr = super::vec::gos_rt_result_payload(result) as *const GosHttpResponse;
    let response = unsafe { &*response_ptr };
    out.extend_from_slice(b"HTTP/1.1 ");
    let mut buf = itoa::Buffer::new();
    out.extend_from_slice(buf.format(response.status).as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(status_reason(response.status).as_bytes());
    out.extend_from_slice(b"\r\n");
    let mut has_content_type = false;
    let mut has_date = false;
    let mut has_server = false;
    for (k, v) in &response.headers {
        // Drop CR/LF/NUL-bearing headers - see `extract_response_into`.
        if !is_valid_header_name(k) || !is_valid_header_value(v) {
            continue;
        }
        if k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        if k.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        if k.eq_ignore_ascii_case("date") {
            has_date = true;
        }
        if k.eq_ignore_ascii_case("server") {
            has_server = true;
        }
        // Lowercase wire casing - same rule as `extract_response_into`.
        out.extend_from_slice(k.to_ascii_lowercase().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !has_date {
        out.extend_from_slice(b"date: ");
        append_http_date_now(out);
        out.extend_from_slice(b"\r\n");
    }
    if !has_server {
        out.extend_from_slice(b"server: ");
        out.extend_from_slice(SERVER_HEADER.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !has_content_type {
        out.extend_from_slice(b"content-type: ");
        if response.content_type.is_empty() {
            out.extend_from_slice(b"text/plain; charset=utf-8");
        } else {
            out.extend_from_slice(response.content_type.as_bytes());
        }
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"transfer-encoding: chunked\r\nconnection: keep-alive\r\n\r\n");
}

/// Drains the pending stream for `handle` to `conn` as chunked
/// frames of at most 8 KiB (`{len:x}\r\n{bytes}\r\n`), ending with
/// the `0\r\n\r\n` terminal frame on clean EOF. Returns `false` on
/// any failure - the caller closes the connection without the
/// terminal frame so the client sees the truncation. A handle that
/// was already served (or never registered) drains as an empty
/// chunked body, matching the interp tier.
fn drain_stream_chunked<C: HttpIo>(conn: &mut C, handle: i64) -> bool {
    let Some(arc) = super::http_client::stream_take_for_serve(handle) else {
        return conn.write_all(b"0\r\n\r\n").is_ok();
    };
    let mut reader = arc.lock();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut *reader, &mut buf) {
            Ok(0) => return conn.write_all(b"0\r\n\r\n").is_ok(),
            Ok(n) => {
                let mut frame = Vec::with_capacity(n + 16);
                frame.extend_from_slice(format!("{n:x}\r\n").as_bytes());
                frame.extend_from_slice(&buf[..n]);
                frame.extend_from_slice(b"\r\n");
                if conn.write_all(&frame).is_err() {
                    return false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
}

/// Reason phrase for the status line: the registered one, or empty for a
/// code with none, which RFC 9112 permits.
fn status_reason(status: i64) -> &'static str {
    u16::try_from(status)
        .ok()
        .and_then(crate::http_status::reason_phrase)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[cfg(not(tsan))]
    fn scheduler_wait_timeout() -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    #[cfg(tsan)]
    fn scheduler_wait_timeout() -> std::time::Duration {
        // TSan instrumentation substantially slows the scheduler and netpoll
        // wake paths these tests exercise.
        std::time::Duration::from_secs(20)
    }

    /// In-memory `HttpIo` for driving `handle_http_conn` in tests:
    /// serves `input` to `read`, records every `write_all` in `written`.
    struct MockConn {
        input: std::io::Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl HttpIo for MockConn {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::io::Read::read(&mut self.input, buf)
        }
        fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
            self.written.extend_from_slice(buf);
            Ok(())
        }
    }

    struct DisconnectingConn {
        input: std::io::Cursor<Vec<u8>>,
    }

    impl HttpIo for DisconnectingConn {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::io::Read::read(&mut self.input, buf)
        }
        fn write_all(&mut self, _buf: &[u8]) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "peer closed connection",
            ))
        }
    }

    unsafe extern "C-unwind" fn unbalanced_arena_handler(
        _env: *mut u8,
        _req: *mut GosHttpRequest,
    ) -> i128 {
        crate::c_abi::rc::gos_rt_arena_push();
        let ptr = crate::c_abi::gc::gos_rt_gc_alloc(64);
        assert!(ptr.is_null() || crate::c_abi::rc::in_region_arena(ptr));
        let response =
            unsafe { gos_rt_http_response_text_new(200, crate::c_abi::string::test_gos_str("ok")) };
        super::super::vec::pack_result(0, response as i64)
    }

    unsafe extern "C-unwind" fn suspended_arena_handler(
        env: *mut u8,
        _req: *mut GosHttpRequest,
    ) -> i128 {
        let resumed = unsafe { &*(env.cast::<std::sync::atomic::AtomicUsize>()) };
        crate::c_abi::rc::gos_rt_arena_push();
        let ptr = crate::c_abi::gc::gos_rt_gc_alloc(64);
        if !ptr.is_null() {
            assert!(crate::c_abi::rc::in_region_arena(ptr));
        }
        resumed.store(1, Ordering::Release);
        crate::sched_global::sleep_until(
            crate::platform::Instant::now() + std::time::Duration::from_millis(5),
        );
        resumed.store(
            usize::from(crate::c_abi::rc::region_is_active()) + 1,
            Ordering::Release,
        );
        let response =
            unsafe { gos_rt_http_response_text_new(200, crate::c_abi::string::test_gos_str("ok")) };
        super::super::vec::pack_result(0, response as i64)
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn disconnect_after_handler_cleans_unbalanced_request_arena() {
        let mut conn = DisconnectingConn {
            input: std::io::Cursor::new(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec()),
        };
        handle_http_conn(
            &mut conn,
            0,
            (unbalanced_arena_handler as HandlerFn) as usize,
        );
        assert!(
            !crate::c_abi::rc::region_is_active(),
            "connection shutdown must release a handler-owned arena"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)] // scheduler goroutines use mmap-backed stacks
    fn suspended_handler_retains_its_arena_until_response_completion() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::mpsc;

        let resumed = Arc::new(AtomicUsize::new(0));
        let env = Arc::as_ptr(&resumed) as usize;
        let (done_tx, done_rx) = mpsc::channel();
        assert!(
            crate::sched_global::try_spawn(Box::new(move || {
                let mut conn = MockConn {
                    input: std::io::Cursor::new(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec()),
                    written: Vec::new(),
                };
                handle_http_conn(
                    &mut conn,
                    env,
                    (suspended_arena_handler as HandlerFn) as usize,
                );
                done_tx.send(()).expect("test receiver remains live");
            }))
            .is_some()
        );
        done_rx
            .recv_timeout(scheduler_wait_timeout())
            .expect("suspended handler must resume and finish");
        assert_eq!(
            resumed.load(Ordering::Acquire),
            2,
            "the handler arena must survive its scheduler suspension"
        );
    }

    #[test]
    fn oversized_request_head_is_rejected_with_431() {
        // A request line followed by 16 KiB of header bytes with no
        // terminating CRLFCRLF: the head cap must fire and reply 431
        // before the accumulator can grow without limit. The request
        // never completes, so the (null) handler address is never used.
        let mut raw = b"GET / HTTP/1.1\r\n".to_vec();
        raw.extend(std::iter::repeat_n(b'X', 16 * 1024));
        let mut conn = MockConn {
            input: std::io::Cursor::new(raw),
            written: Vec::new(),
        };
        handle_http_conn(&mut conn, 0, 0);
        let resp = String::from_utf8_lossy(&conn.written);
        assert!(
            resp.starts_with("HTTP/1.1 431"),
            "expected 431 for oversized head, got: {resp}"
        );
    }

    fn rendered(result: i128) -> String {
        let mut out = Vec::new();
        assert!(extract_response_into(result, &mut out, &mut true));
        unsafe { drop_handler_result(result) };
        String::from_utf8_lossy(&out).to_ascii_lowercase()
    }

    #[test]
    fn text_response_renders_text_plain_content_type() {
        let resp =
            unsafe { gos_rt_http_response_text_new(200, crate::c_abi::string::test_gos_str("ok")) };
        let result = super::super::vec::pack_result(0, resp as i64);
        let bytes = rendered(result);
        assert!(
            bytes.contains("content-type: text/plain; charset=utf-8"),
            "rendered response: {bytes}"
        );
        assert!(bytes.ends_with("ok"), "rendered response: {bytes}");
    }

    #[test]
    fn nul_embedded_byte_body_serves_full_bytes_with_correct_length() {
        // A byte-array body may contain NUL bytes; the writer must
        // serve `body_bytes` in full instead of the c-string mirror
        // (which stops at the first NUL).
        let resp =
            unsafe { gos_rt_http_response_text_new(200, crate::c_abi::string::test_gos_str("A")) };
        unsafe { (*resp).body_bytes = Some(vec![0x41, 0x00, 0x42, 0x00, 0x43]) };
        let result = super::super::vec::pack_result(0, resp as i64);
        let mut out = Vec::new();
        assert!(extract_response_into(result, &mut out, &mut true));
        unsafe { drop_handler_result(result) };
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(
            text.contains("content-length: 5"),
            "rendered response: {text:?}"
        );
        assert!(
            out.ends_with(&[0x41, 0x00, 0x42, 0x00, 0x43]),
            "rendered response: {out:?}"
        );
    }

    #[test]
    fn handler_header_names_render_lowercase_on_the_wire() {
        // Wire casing is canonical-lowercase on every tier; a
        // handler-supplied mixed-case name must normalize.
        let resp =
            unsafe { gos_rt_http_response_text_new(200, crate::c_abi::string::test_gos_str("ok")) };
        unsafe {
            (*resp)
                .headers
                .push(("X-Mixed-Case".to_string(), "Value-Kept".to_string()));
        }
        let result = super::super::vec::pack_result(0, resp as i64);
        let mut out = Vec::new();
        assert!(extract_response_into(result, &mut out, &mut true));
        unsafe { drop_handler_result(result) };
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(
            text.contains("x-mixed-case: Value-Kept"),
            "rendered response: {text:?}"
        );
        assert!(
            !text.contains("X-Mixed-Case"),
            "mixed-case name must not reach the wire: {text:?}"
        );
    }

    #[test]
    fn json_response_renders_application_json_content_type() {
        let resp =
            unsafe { gos_rt_http_response_json_new(200, crate::c_abi::string::test_gos_str("{}")) };
        let result = super::super::vec::pack_result(0, resp as i64);
        let bytes = rendered(result);
        assert!(
            bytes.contains("content-type: application/json"),
            "rendered response: {bytes}"
        );
    }

    #[test]
    fn explicit_content_type_header_wins_over_constructor_default() {
        let resp = unsafe {
            gos_rt_http_response_text_new(200, crate::c_abi::string::test_gos_str("<p>hi</p>"))
        };
        unsafe {
            (*resp)
                .headers
                .push(("Content-Type".to_string(), "text/html".to_string()));
        }
        let result = super::super::vec::pack_result(0, resp as i64);
        let bytes = rendered(result);
        assert!(
            bytes.contains("content-type: text/html"),
            "rendered response: {bytes}"
        );
        assert!(
            !bytes.contains("text/plain"),
            "constructor default must not also render: {bytes}"
        );
    }

    #[test]
    fn empty_content_type_falls_back_to_text_plain() {
        let resp =
            unsafe { gos_rt_http_response_text_new(200, crate::c_abi::string::test_gos_str("ok")) };
        unsafe { (*resp).content_type = std::borrow::Cow::Borrowed("") };
        let result = super::super::vec::pack_result(0, resp as i64);
        let bytes = rendered(result);
        assert!(
            bytes.contains("content-type: text/plain; charset=utf-8"),
            "rendered response: {bytes}"
        );
    }

    #[test]
    fn content_length_scans_header_section_case_insensitively() {
        assert_eq!(
            content_length(b"POST / HTTP/1.1\r\ncOnTeNt-LeNgTh: 42\r\n\r\n"),
            42
        );
        assert_eq!(content_length(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"), 0);
        assert_eq!(
            content_length(b"POST / HTTP/1.1\r\nContent-Length: nope\r\n\r\n"),
            0
        );
    }

    #[test]
    fn hostile_content_length_is_rejected_before_req_end_arithmetic() {
        let huge =
            content_length(b"POST / HTTP/1.1\r\nContent-Length: 18446744073709551615\r\n\r\n");
        assert_eq!(huge, usize::MAX);
        assert!(huge > MAX_REQUEST_BODY_BYTES);
        let modest = content_length(b"POST / HTTP/1.1\r\nContent-Length: 4096\r\n\r\n");
        assert!(modest <= MAX_REQUEST_BODY_BYTES);
    }

    #[test]
    fn raw_body_resolves_h1_lazy_buffer_past_header_section() {
        let mut raw: Vec<u8> = b"POST /upload HTTP/1.1\r\nContent-Length: 4\r\n\r\n".to_vec();
        let header_end = raw.len();
        let payload = [0x68u8, 0xFF, 0x00, 0x69];
        raw.extend_from_slice(&payload);

        let mut request = GosHttpRequest {
            method: String::new(),
            url: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
            body_offset: 0,
            params: Vec::new(),
            values: Vec::new(),
            agent: None,
            peer: String::new(),
            context_site: None,
            context: 0,
        };
        assert!(parse_request_into(&raw, header_end, &mut request));
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "/upload");

        let v = unsafe { gos_rt_http_request_raw_body(std::ptr::from_ref(&request)) };
        assert!(!v.is_null());
        let vec_ref = unsafe { &*v };
        assert_eq!(vec_ref.len, 4);
        assert_eq!(vec_ref.elem_bytes, 1);
        let got = unsafe { std::slice::from_raw_parts(vec_ref.ptr.as_ptr(), 4) };
        assert_eq!(got, &[0x68, 0xFF, 0x00, 0x69]);
        unsafe { super::super::map::gos_rt_vec_free(v) };

        // The lossy `body` accessor resolves the same region: its
        // c-string keeps the bytes up to the embedded NUL.
        let s = unsafe { gos_rt_http_request_body_str(std::ptr::from_ref(&request)) };
        assert_eq!(unsafe { CStr::from_ptr(s) }.to_bytes(), &[0x68, 0xFF]);
        unsafe { crate::c_abi::string::gos_rt_str_free(s) };
    }

    #[test]
    fn parse_request_into_populates_normalized_headers() {
        let raw: &[u8] =
            b"GET /x HTTP/1.1\r\nHost: h\r\nX-Dup: first\r\nAccept: */*\r\nx-dup:  second \r\n\r\n";
        let header_end = raw.len();
        let mut request = GosHttpRequest {
            method: String::new(),
            url: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
            body_offset: 0,
            params: Vec::new(),
            values: Vec::new(),
            agent: None,
            peer: String::new(),
            context_site: None,
            context: 0,
        };
        assert!(parse_request_into(raw, header_end, &mut request));
        // Interp `Headers` map view: lowercase names, trimmed
        // values, last-wins dedupe, sorted by name.
        assert_eq!(
            request.headers,
            vec![
                ("accept".to_string(), "*/*".to_string()),
                ("host".to_string(), "h".to_string()),
                ("x-dup".to_string(), "second".to_string()),
            ]
        );
    }

    #[test]
    fn parse_request_into_merges_chunked_trailers_into_headers() {
        let mut raw = chunked_head();
        let header_end = raw.len();
        raw.extend_from_slice(b"3\r\nabc\r\n0\r\nX-Trailer: tval\r\n\r\n");
        let ChunkedAssembly::Ready(c) =
            assemble_chunked_request(&raw, header_end, &ServerLimits::default())
        else {
            panic!("complete frame must assemble");
        };
        let mut request = fresh_request();
        assert!(parse_request_into(&c.canonical, c.header_end, &mut request));
        assert!(
            request
                .headers
                .contains(&("x-trailer".to_string(), "tval".to_string())),
            "trailer promoted with interp lowercase semantics: {:?}",
            request.headers
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn streamed_response_drains_chunked_frames_to_loopback() {
        use std::io::Read;

        // > 8 KiB so the drain emits two data frames + terminal.
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let reader: super::http_client::StreamReader =
            std::io::BufReader::new(Box::new(std::io::Cursor::new(payload.clone()))
                as Box<dyn std::io::Read + Send + Sync>);
        let handle = super::http_client::stream_registry_register(reader);
        let blob = [handle, 200i64, 0i64];
        let ct = std::ffi::CString::new("application/octet-stream").unwrap();
        let resp = unsafe {
            gos_rt_http_response_stream_new(
                201,
                crate::c_abi::string::test_gos_ptr(&ct),
                blob.as_ptr(),
            )
        };
        let result = super::super::vec::pack_result(0, resp as i64);
        assert_eq!(streamed_ok_handle(result), Some(handle));

        let mut head = Vec::new();
        extract_stream_head_into(result, &mut head);
        unsafe { drop_handler_result(result) };
        let head_text = String::from_utf8_lossy(&head).to_ascii_lowercase();
        assert!(head_text.starts_with("http/1.1 201"), "head: {head_text}");
        assert!(head_text.contains("transfer-encoding: chunked"));
        assert!(head_text.contains("content-type: application/octet-stream"));
        assert!(
            !head_text.contains("content-length"),
            "chunked head must not carry Content-Length: {head_text}"
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut sock = std::net::TcpStream::connect(addr).unwrap();
            let mut raw = Vec::new();
            sock.read_to_end(&mut raw).unwrap();
            raw
        });
        let (server_side, _) = listener.accept().unwrap();
        let mut conn = HttpConn::wrap(server_side).expect("wrap");
        assert!(drain_stream_chunked(&mut conn, handle), "clean drain");
        drop(conn);

        let raw = client.join().unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(b"2000\r\n");
        expected.extend_from_slice(&payload[..8192]);
        expected.extend_from_slice(b"\r\n");
        expected.extend_from_slice(format!("{:x}\r\n", payload.len() - 8192).as_bytes());
        expected.extend_from_slice(&payload[8192..]);
        expected.extend_from_slice(b"\r\n0\r\n\r\n");
        assert_eq!(raw, expected, "exact chunked framing with terminal frame");

        // The handle was taken at serve time - a second drain answers
        // only the terminal frame (empty chunked body).
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut sock = std::net::TcpStream::connect(addr).unwrap();
            let mut raw = Vec::new();
            sock.read_to_end(&mut raw).unwrap();
            raw
        });
        let (server_side, _) = listener.accept().unwrap();
        let mut conn = HttpConn::wrap(server_side).expect("wrap");
        assert!(drain_stream_chunked(&mut conn, handle));
        drop(conn);
        assert_eq!(client.join().unwrap(), b"0\r\n\r\n");
    }

    /// Reader that yields one chunk and then fails, modelling an
    /// upstream connection dying mid-proxy.
    struct FailAfterFirstRead {
        first: Option<Vec<u8>>,
    }

    impl std::io::Read for FailAfterFirstRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.first.take() {
                Some(bytes) => {
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                None => Err(std::io::Error::other("upstream died")),
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn streamed_response_mid_stream_error_closes_without_terminal_frame() {
        use std::io::Read;

        let reader: super::http_client::StreamReader =
            std::io::BufReader::new(Box::new(FailAfterFirstRead {
                first: Some(b"partial".to_vec()),
            }) as Box<dyn std::io::Read + Send + Sync>);
        let handle = super::http_client::stream_registry_register(reader);
        super::http_client::stream_consume_for_response(handle);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut sock = std::net::TcpStream::connect(addr).unwrap();
            let mut raw = Vec::new();
            sock.read_to_end(&mut raw).unwrap();
            raw
        });
        let (server_side, _) = listener.accept().unwrap();
        let mut conn = HttpConn::wrap(server_side).expect("wrap");
        assert!(
            !drain_stream_chunked(&mut conn, handle),
            "mid-stream read error must report failure so the caller closes"
        );
        drop(conn);

        let raw = client.join().unwrap();
        assert_eq!(
            raw, b"7\r\npartial\r\n",
            "the delivered frame reaches the wire; no terminal frame follows"
        );
    }

    fn fresh_request() -> GosHttpRequest {
        GosHttpRequest {
            method: String::new(),
            url: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
            body_offset: 0,
            params: Vec::new(),
            values: Vec::new(),
            agent: None,
            peer: String::new(),
            context_site: None,
            context: 0,
        }
    }

    fn chunked_head() -> Vec<u8> {
        b"POST /up HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec()
    }

    #[test]
    fn transfer_encoding_detection_matches_interp_semantics() {
        assert!(transfer_encoding_is_chunked(
            b"POST / HTTP/1.1\r\nTransfer-Encoding: Chunked\r\n\r\n"
        ));
        assert!(transfer_encoding_is_chunked(
            b"POST / HTTP/1.1\r\ntransfer-encoding:chunked\r\n\r\n"
        ));
        // Multi-coding values are not "chunked" on the interp tier
        // (whole-value match) - same here.
        assert!(!transfer_encoding_is_chunked(
            b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"
        ));
        assert!(!transfer_encoding_is_chunked(
            b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"
        ));
    }

    #[test]
    fn chunked_assembly_round_trips_binary_chunks() {
        let mut raw = chunked_head();
        let header_end = raw.len();
        raw.extend_from_slice(b"4\r\n");
        raw.extend_from_slice(&[0x00, 0xFF, 0x68, 0x69]);
        raw.extend_from_slice(b"\r\n2\r\n");
        raw.extend_from_slice(&[0xFE, 0x00]);
        raw.extend_from_slice(b"\r\n0\r\n\r\n");

        let ChunkedAssembly::Ready(c) =
            assemble_chunked_request(&raw, header_end, &ServerLimits::default())
        else {
            panic!("complete frame must assemble");
        };
        assert_eq!(c.raw_end, raw.len());

        let mut request = fresh_request();
        assert!(parse_request_into(&c.canonical, c.header_end, &mut request));
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "/up");
        assert_eq!(
            &request.body[request.body_offset..],
            &[0x00, 0xFF, 0x68, 0x69, 0xFE, 0x00],
            "body accessors must see the de-chunked binary payload"
        );
    }

    #[test]
    fn chunked_assembly_incomplete_until_terminal_then_exact_boundary() {
        let mut raw = chunked_head();
        let header_end = raw.len();
        raw.extend_from_slice(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\nX-T: v\r\n\r\n");
        let frame_end = raw.len();

        for cut in header_end..frame_end {
            assert!(
                matches!(
                    assemble_chunked_request(&raw[..cut], header_end, &ServerLimits::default()),
                    ChunkedAssembly::Incomplete
                ),
                "prefix of {cut} bytes must be Incomplete"
            );
        }

        // Pipelined bytes after the frame must not move the boundary.
        raw.extend_from_slice(b"GET /next HTTP/1.1\r\nHost: x\r\n\r\n");
        let ChunkedAssembly::Ready(c) =
            assemble_chunked_request(&raw, header_end, &ServerLimits::default())
        else {
            panic!("complete frame must assemble");
        };
        assert_eq!(
            c.raw_end, frame_end,
            "raw_end must stop at the frame boundary"
        );
    }

    #[test]
    fn chunked_assembly_merges_trailer_headers_into_canonical() {
        let mut raw = chunked_head();
        let header_end = raw.len();
        raw.extend_from_slice(b"3\r\nabc\r\n0\r\nX-Trailer: tval\r\nbogus-no-colon\r\n\r\n");

        let ChunkedAssembly::Ready(c) =
            assemble_chunked_request(&raw, header_end, &ServerLimits::default())
        else {
            panic!("complete frame must assemble");
        };
        let canonical = String::from_utf8_lossy(&c.canonical).into_owned();
        let header_section = &canonical[..c.header_end];
        assert!(
            header_section.contains("X-Trailer: tval\r\n"),
            "trailer spliced into the header section: {header_section}"
        );
        assert!(
            header_section.contains("Transfer-Encoding: chunked\r\n"),
            "original headers preserved verbatim: {header_section}"
        );
        assert_eq!(&canonical[c.header_end..], "abc");
    }

    #[test]
    fn chunked_assembly_rejects_malformed_size_line() {
        let mut raw = chunked_head();
        let header_end = raw.len();
        raw.extend_from_slice(b"zz\r\nWiki\r\n0\r\n\r\n");
        let ChunkedAssembly::Reject(resp) =
            assemble_chunked_request(&raw, header_end, &ServerLimits::default())
        else {
            panic!("bad hex size must be rejected");
        };
        assert_eq!(resp, RESPONSE_400_BYTES);

        // Missing CRLF after the chunk data is framing corruption.
        let mut raw = chunked_head();
        let header_end = raw.len();
        raw.extend_from_slice(b"4\r\nWikiXX0\r\n\r\n");
        let ChunkedAssembly::Reject(resp) =
            assemble_chunked_request(&raw, header_end, &ServerLimits::default())
        else {
            panic!("missing data CRLF must be rejected");
        };
        assert_eq!(resp, RESPONSE_400_BYTES);
    }

    #[test]
    fn chunked_assembly_rejects_declared_oversize_before_data_arrives() {
        let mut raw = chunked_head();
        let header_end = raw.len();
        raw.extend_from_slice(format!("{:x}\r\n", MAX_REQUEST_BODY_BYTES + 1).as_bytes());
        let ChunkedAssembly::Reject(resp) =
            assemble_chunked_request(&raw, header_end, &ServerLimits::default())
        else {
            panic!("hostile declared size must be rejected from the size line alone");
        };
        assert_eq!(resp, RESPONSE_413_BYTES);
    }

    #[test]
    fn chunked_assembly_rejects_cumulative_body_over_cap() {
        let mut raw = chunked_head();
        let header_end = raw.len();
        let chunk = vec![b'a'; 64 * 1024];
        for _ in 0..17 {
            raw.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            raw.extend_from_slice(&chunk);
            raw.extend_from_slice(b"\r\n");
        }
        raw.extend_from_slice(b"0\r\n\r\n");
        assert!(17 * chunk.len() > MAX_REQUEST_BODY_BYTES);
        let ChunkedAssembly::Reject(resp) =
            assemble_chunked_request(&raw, header_end, &ServerLimits::default())
        else {
            panic!("cumulative de-chunked size past the cap must be rejected");
        };
        assert_eq!(resp, RESPONSE_413_BYTES);
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "2 MiB raw-framing stress is prohibitively slow under the MIR interpreter; native and ASan run it"
    )]
    fn chunked_assembly_rejects_runaway_raw_framing() {
        // Endless 1-byte chunks with no terminal frame: the decoded
        // size stays under the cap, but the raw frame keeps growing
        // - the raw cap must stop it while still Incomplete.
        let mut raw = chunked_head();
        let header_end = raw.len();
        while raw.len() - header_end <= ServerLimits::default().max_chunked_raw_bytes() {
            raw.extend_from_slice(b"1\r\nA\r\n");
        }
        let ChunkedAssembly::Reject(resp) =
            assemble_chunked_request(&raw, header_end, &ServerLimits::default())
        else {
            panic!("unterminated chunk stream past the raw cap must be rejected");
        };
        assert_eq!(resp, RESPONSE_413_BYTES);
    }

    /// Echo handler used by the loopback tests: renders the body
    /// length, the body text, and the `X-Trailer` header value so
    /// assertions can see exactly what the handler observed.
    unsafe extern "C-unwind" fn echo_handler(_env: *mut u8, req: *mut GosHttpRequest) -> i128 {
        let request = unsafe { &*req };
        let body = &request.body[request.body_offset.min(request.body.len())..];
        let trailer = request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-trailer"))
            .map_or(String::new(), |(_, v)| v.clone());
        let text = format!(
            "len={};body={};trailer={}",
            body.len(),
            String::from_utf8_lossy(body),
            trailer
        );
        let c = std::ffi::CString::new(text).unwrap();
        let resp =
            unsafe { gos_rt_http_response_text_new(200, crate::c_abi::string::test_gos_ptr(&c)) };
        super::super::vec::pack_result(0, resp as i64)
    }

    /// Handler that raises the fault a Gossamer `panic!` raises, so the
    /// connection core is exercised on the path a panicking handler takes.
    unsafe extern "C-unwind" fn panicking_handler(
        _env: *mut u8,
        _req: *mut GosHttpRequest,
    ) -> i128 {
        crate::c_abi::panic::panic_text("handler exploded");
        0
    }

    #[test]
    fn panicking_handler_answers_500_and_keeps_serving() {
        let addr = (panicking_handler as HandlerFn) as usize;
        let raw = roundtrip_raw_bytes(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n", addr);
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.starts_with("HTTP/1.1 500"),
            "a panicking handler must answer 500, got: {text}"
        );
    }

    /// One connection's bytes held in memory: the client's request bytes
    /// are read out in `read`-sized pieces and then EOF, and everything the
    /// server writes back accumulates in `written`. The framing core is
    /// written against [`HttpIo`], so a request/response exchange needs a
    /// transport, not a socket - a `peer_socket` of `None` is exactly the
    /// "no peer to peek at" case the trait documents.
    struct MemoryConn {
        inbound: Vec<u8>,
        read_cursor: usize,
        written: Vec<u8>,
    }

    impl MemoryConn {
        fn new(client_bytes: &[u8]) -> Self {
            Self {
                inbound: client_bytes.to_vec(),
                read_cursor: 0,
                written: Vec::new(),
            }
        }
    }

    impl HttpIo for MemoryConn {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let remaining = &self.inbound[self.read_cursor..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.read_cursor += n;
            Ok(n)
        }
        fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
            self.written.extend_from_slice(buf);
            Ok(())
        }
    }

    /// Runs the HTTP framing core over one connection's worth of client
    /// bytes and returns everything the server wrote back. Non-blocking
    /// transport behaviour is covered by its dedicated netpoll tests.
    fn roundtrip_raw_bytes(client_bytes: &[u8], fn_addr: usize) -> Vec<u8> {
        let _faults = crate::c_abi::panic::IsolatedFaults::enter();
        let mut conn = MemoryConn::new(client_bytes);
        handle_http_conn(&mut conn, 0, fn_addr);
        conn.written
    }

    fn echo_fn_addr() -> usize {
        (echo_handler as HandlerFn) as usize
    }

    #[test]
    #[cfg_attr(miri, ignore)] // scheduler goroutines use mmap-backed stacks
    fn nonblocking_connection_parks_then_resumes_on_netpoll_readiness() {
        use std::io::{Read, Write};
        use std::sync::mpsc;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut sock = TcpStream::connect(addr).unwrap();
            // The server must reach its non-blocking wait before this arrives.
            crate::platform::sleep(Duration::from_millis(20));
            sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            let _ = sock.shutdown(std::net::Shutdown::Write);
            let mut response = String::new();
            sock.read_to_string(&mut response).unwrap();
            response
        });
        let (stream, _) = listener.accept().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let fn_addr = echo_fn_addr();
        assert!(
            crate::sched_global::try_spawn(Box::new(move || {
                let mut conn = HttpConn::wrap(stream).expect("wrap non-blocking socket");
                handle_http_conn(&mut conn, 0, fn_addr);
                done_tx.send(()).unwrap();
            }))
            .is_some()
        );
        done_rx
            .recv_timeout(scheduler_wait_timeout())
            .expect("connection goroutine must resume from netpoll readiness");
        let response = client.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // scheduler goroutines use mmap-backed stacks
    fn nonblocking_connection_deadline_wakes_without_socket_timeout() {
        use crate::platform::Instant;
        use std::sync::mpsc;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = TcpStream::connect(addr).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let started = Instant::now();
        assert!(
            crate::sched_global::try_spawn(Box::new(move || {
                let mut conn = HttpConn::wrap(stream).expect("wrap non-blocking socket");
                let ready = conn
                    .wait_until(
                        crate::sched::Interest::Readable,
                        Instant::now() + Duration::from_millis(25),
                    )
                    .expect("register deadline wait");
                done_tx.send(ready).unwrap();
            }))
            .is_some()
        );
        assert!(
            !done_rx
                .recv_timeout(scheduler_wait_timeout())
                .expect("deadline must wake connection goroutine"),
            "an idle socket must report a deadline rather than false readiness"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(15),
            "deadline fired implausibly early"
        );
    }

    #[test]
    fn chunked_request_dechunks_for_handler_and_preserves_pipelined_boundary() {
        let mut wire: Vec<u8> =
            b"POST /up HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        wire.extend_from_slice(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Trailer: tval\r\n\r\n");
        wire.extend_from_slice(b"GET /next HTTP/1.1\r\nHost: x\r\n\r\n");

        let raw = roundtrip_raw_bytes(&wire, echo_fn_addr());
        let text = String::from_utf8_lossy(&raw).into_owned();
        let responses = text.matches("HTTP/1.1 ").count();
        assert_eq!(
            responses, 2,
            "one response per request - chunked body must not be misparsed as a pipelined request: {text}"
        );
        assert!(
            text.contains("len=9;body=Wikipedia;trailer=tval"),
            "handler must see the de-chunked body and the promoted trailer: {text}"
        );
        assert!(
            text.contains("len=0;body=;trailer="),
            "pipelined GET after the chunked frame must parse cleanly: {text}"
        );
        assert!(
            !text.contains("400"),
            "no request in this exchange is malformed: {text}"
        );
    }

    #[test]
    fn chunked_with_content_length_rejected_400_on_wire() {
        let wire = b"POST /up HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nContent-Length: 4\r\n\r\n4\r\nWiki\r\n0\r\n\r\n";
        let raw = roundtrip_raw_bytes(wire, echo_fn_addr());
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(
            text.starts_with("HTTP/1.1 400 "),
            "smuggling-shaped request must get 400 and close: {text}"
        );
        assert_eq!(text.matches("HTTP/1.1 ").count(), 1);
    }

    #[test]
    fn content_length_over_cap_rejected_413_on_wire() {
        let wire = format!(
            "POST /up HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            MAX_REQUEST_BODY_BYTES + 1
        );
        let raw = roundtrip_raw_bytes(wire.as_bytes(), echo_fn_addr());
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(
            text.starts_with("HTTP/1.1 413 "),
            "declared Content-Length past the cap must get 413: {text}"
        );
    }

    #[test]
    fn declared_chunk_over_cap_rejected_413_mid_stream() {
        // The hostile size line alone triggers the reject - the
        // server must not wait for (or buffer) the declared body.
        let mut wire = chunked_head();
        wire.extend_from_slice(format!("{:x}\r\nAA", MAX_REQUEST_BODY_BYTES + 1).as_bytes());
        let raw = roundtrip_raw_bytes(&wire, echo_fn_addr());
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(
            text.starts_with("HTTP/1.1 413 "),
            "oversize chunk declaration must get 413 mid-stream: {text}"
        );
    }

    #[test]
    fn parse_request_into_accepts_binary_body_after_utf8_headers() {
        let mut raw: Vec<u8> = b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\n".to_vec();
        let header_end = raw.len();
        raw.extend_from_slice(&[0xFE, 0xFD]);
        let mut request = GosHttpRequest {
            method: String::new(),
            url: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
            body_offset: 0,
            params: Vec::new(),
            values: Vec::new(),
            agent: None,
            peer: String::new(),
            context_site: None,
            context: 0,
        };
        assert!(parse_request_into(&raw, header_end, &mut request));
        assert_eq!(request.body_offset, header_end);
        assert_eq!(&request.body[request.body_offset..], &[0xFE, 0xFD]);
    }

    #[test]
    fn handler_header_names_write_lowercase_on_the_wire() {
        let resp =
            unsafe { gos_rt_http_response_text_new(200, crate::c_abi::string::test_gos_str("ok")) };
        unsafe {
            super::http_client::gos_rt_http_response_set_header(
                resp,
                crate::c_abi::string::test_gos_str("X-Custom-Thing"),
                crate::c_abi::string::test_gos_str("v"),
            );
        }
        let result = super::super::vec::pack_result(0, resp as i64);
        let mut out = Vec::new();
        assert!(extract_response_into(result, &mut out, &mut true));
        unsafe { drop_handler_result(result) };
        // Raw bytes - no case folding before the assertions.
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.contains("x-custom-thing: v\r\n"), "wire: {text}");
        assert!(!text.contains("X-Custom-Thing"), "wire: {text}");
        assert!(text.contains("content-type: text/plain; charset=utf-8\r\n"));
        assert!(text.contains("connection: keep-alive\r\n"));
        assert!(text.contains("content-length: 2\r\n"));
    }

    #[test]
    fn stream_head_header_names_write_lowercase_on_the_wire() {
        let reader: super::http_client::StreamReader = std::io::BufReader::new(Box::new(
            std::io::Cursor::new(Vec::new()),
        )
            as Box<dyn std::io::Read + Send + Sync>);
        let handle = super::http_client::stream_registry_register(reader);
        let blob = [handle, 200i64, 0i64];
        let resp = unsafe {
            gos_rt_http_response_stream_new(
                200,
                crate::c_abi::string::test_gos_str("text/csv"),
                blob.as_ptr(),
            )
        };
        unsafe {
            super::http_client::gos_rt_http_response_set_header(
                resp,
                crate::c_abi::string::test_gos_str("X-MiXeD"),
                crate::c_abi::string::test_gos_str("v"),
            );
        }
        let result = super::super::vec::pack_result(0, resp as i64);
        let mut head = Vec::new();
        extract_stream_head_into(result, &mut head);
        unsafe { drop_handler_result(result) };
        drop(super::http_client::stream_take_for_serve(handle));
        let text = String::from_utf8_lossy(&head).into_owned();
        assert!(text.contains("x-mixed: v\r\n"), "head: {text}");
        assert!(!text.contains("X-MiXeD"), "head: {text}");
        assert!(text.contains("transfer-encoding: chunked\r\nconnection: keep-alive\r\n"));
        assert!(text.contains("content-type: text/csv\r\n"));
    }

    /// A shutdown reaches an acceptor parked inside `accept()` only as a
    /// connection: the wake is what carries it, so it must arrive on every
    /// platform the server runs on.
    #[test]
    #[cfg_attr(miri, ignore)] // a real accept()/connect() pair: Miri has no socket syscalls
    fn the_wake_releases_a_thread_parked_in_accept() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let _registered = register_http_shutdown_wake_addr(addr);
        let (tx, rx) = std::sync::mpsc::channel();
        let parked = std::thread::spawn(move || {
            let _ = tx.send(listener.accept().is_ok());
        });

        wake_http_acceptors();

        let accepted = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the parked accept returns once the wake connects");
        assert!(accepted, "the wake connection is the one `accept` answers");
        parked.join().expect("parked thread ends");
    }

    /// The server's own accept loop ends on the same pair - its shutdown
    /// flag plus the wake that delivers it - and stops holding the port.
    #[test]
    #[cfg_attr(miri, ignore)] // a real accept()/connect() pair: Miri has no socket syscalls
    fn the_accept_loop_ends_on_a_flagged_wake() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let limits = ServerLimits::default();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
        let (tx, rx) = std::sync::mpsc::channel();
        let acceptor = {
            let shutdown = std::sync::Arc::clone(&shutdown);
            let in_flight = std::sync::Arc::clone(&in_flight);
            std::thread::spawn(move || {
                accept_serve_with(listener, &limits, &shutdown, &in_flight, |_, _, _| {});
                let _ = tx.send(());
            })
        };

        // Serving one connection puts the loop back at `accept()`, which is
        // where a shutdown has to be able to find it.
        let served = std::net::TcpStream::connect(addr).expect("connect");
        drop(served);

        shutdown.store(true, Ordering::Release);
        wake_http_acceptors();

        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the woken loop leaves instead of holding the port");
        acceptor.join().expect("acceptor thread ends");
    }
}
