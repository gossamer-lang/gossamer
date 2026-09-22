//! HTTP/2 server compiled into the runtime staticlib.
//!
//! Mirror of `gossamer_std::http_h2` that lives inside
//! `gossamer-runtime` so the compiled tier can call into it from
//! `gos_rt_http2_bind_and_run_h2c`. Implements:
//!
//! - A goroutine-driven future driver (`drive`).
//! - An `AsyncRead+AsyncWrite` bridge over a non-blocking
//!   `std::net::TcpStream` + mio mirror.
//! - An `h2::server`-backed serve loop that converts each
//!   inbound stream into a [`crate::c_abi::GosHttpRequest`] and
//!   dispatches through the user handler's function pointer.
//!
//! The goroutine scheduler is the only executor; no Tokio
//! runtime is involved. See
//! `crates/gossamer-std/HTTP_H2_ARCH.md` for the architectural
//! rationale (the std-side module documents the same model).

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::similar_names,
    clippy::cast_possible_truncation,
    clippy::manual_let_else,
    clippy::let_unit_value,
    clippy::ignored_unit_patterns,
    clippy::explicit_iter_loop,
    clippy::map_unwrap_or,
    clippy::items_after_statements,
    clippy::doc_markdown
)]

use crate::platform::Instant;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::c_abi::{GosHttpRequest, drop_handler_result, extract_response_into, gos_rt_gc_reset};
use crate::sched::{Gid, Interest, ParkReason};
use crate::sched_global;

/// The compiled HTTP/2 ABI currently hands a complete request to the
/// generated handler. Keep that compatibility path bounded until it grows a
/// reader-based request interface.
const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONCURRENT_STREAMS: u32 = 64;
const MAX_HEADER_LIST_BYTES: u32 = 16 * 1024;
const STREAM_DEADLINE: Duration = Duration::from_secs(30);
const MAX_SEND_CHUNK_BYTES: usize = 16 * 1024;

/// Balances one accepted stream even when its handler unwinds while suspended
/// or the connection is torn down underneath a pending response.
struct InFlightGuard(Arc<AtomicUsize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Acquires one locally enforced stream slot.  The h2 SETTINGS value is only
/// advisory while peers are converging on it, so accepting solely on that
/// setting would still permit unbounded goroutine creation during a race.
fn try_begin_stream(in_flight: &AtomicUsize) -> bool {
    let mut current = in_flight.load(Ordering::Acquire);
    loop {
        if current >= MAX_CONCURRENT_STREAMS as usize {
            return false;
        }
        match in_flight.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

/// Scheduler-backed deadline for h2 I/O. Tokio is deliberately not a runtime
/// dependency here; parked h2 futures must be resumed by Gossamer's own
/// scheduler timer so the timer and I/O wakers share the same goroutine.
struct Deadline<F> {
    future: Pin<Box<F>>,
    expires_at: Instant,
    timer_gid: Option<Gid>,
}

impl<F> Deadline<F> {
    fn new(future: F, expires_at: Instant) -> Self {
        Self {
            future: Box::pin(future),
            expires_at,
            timer_gid: None,
        }
    }

    fn clear_timer(&mut self) {
        if let Some(gid) = self.timer_gid.take() {
            sched_global::forget_waker(gid);
        }
    }
}

impl<F: Future> Future for Deadline<F> {
    type Output = Result<F::Output, &'static str>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        if Instant::now() >= this.expires_at {
            this.clear_timer();
            return Poll::Ready(Err("HTTP/2 stream deadline exceeded"));
        }
        if this.timer_gid.is_none() {
            let timer_gid = sched_global::add_timer(this.expires_at);
            let wake = cx.waker().clone();
            sched_global::register_waker(timer_gid, Box::new(move || wake.wake_by_ref()));
            this.timer_gid = Some(timer_gid);
        }
        match this.future.as_mut().poll(cx) {
            Poll::Ready(output) => {
                this.clear_timer();
                Poll::Ready(Ok(output))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<F> Drop for Deadline<F> {
    fn drop(&mut self) {
        self.clear_timer();
    }
}

/// Boots the h2c server. Binds `addr` - a bind failure propagates
/// as `Err` so the C-ABI shim can hand the caller's
/// `Result<(), http::Error>` match an `Err` value (interp parity).
/// Each accepted TCP connection is wrapped in an `AsyncTcpStream`
/// and driven by a goroutine that calls `h2::server::handshake` +
/// an accept loop. Per-stream handler dispatch happens inside
/// child goroutines so a slow handler doesn't block other streams
/// on the same connection.
pub fn serve_h2c_with_handler(addr: &str, env_addr: usize, fn_addr: usize) -> std::io::Result<()> {
    let listener = crate::listen::bind_tcp(addr)?;
    let _ = listener.set_nonblocking(false);
    let mut backoff = crate::accept::AcceptBackoff::new();
    loop {
        let (sock, _peer) = match listener.accept() {
            Ok(pair) => {
                backoff.reset();
                pair
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if crate::accept::accept_error_is_transient(&e) => {
                backoff.settle();
                continue;
            }
            Err(_) => break Ok(()),
        };
        let _ = sock.set_nodelay(true);
        sched_global::spawn(Box::new(move || {
            if sock.set_nonblocking(true).is_err() {
                return;
            }
            let mio_clone = match sock.try_clone() {
                Ok(c) => mio::net::TcpStream::from_std(c),
                Err(_) => return,
            };
            let async_io = AsyncTcpStream {
                inner: sock,
                mio: mio_clone,
                read_waker: Arc::new(Mutex::new(None)),
                write_waker: Arc::new(Mutex::new(None)),
            };
            let _ = drive(serve_one_connection(async_io, env_addr, fn_addr));
        }));
    }
}

async fn serve_one_connection(io: AsyncTcpStream, env_addr: usize, fn_addr: usize) {
    let mut builder = h2::server::Builder::new();
    builder
        .max_concurrent_streams(MAX_CONCURRENT_STREAMS)
        .max_header_list_size(MAX_HEADER_LIST_BYTES);
    let mut conn = match builder.handshake::<_, Bytes>(io).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("h2 handshake: {e}");
            return;
        }
    };
    let in_flight = Arc::new(AtomicUsize::new(0));
    loop {
        match conn.accept().await {
            Some(Ok((req, mut respond))) => {
                if !try_begin_stream(&in_flight) {
                    respond.send_reset(h2::Reason::REFUSED_STREAM);
                    continue;
                }
                let in_flight_for_stream = Arc::clone(&in_flight);
                sched_global::spawn(Box::new(move || {
                    let _in_flight = InFlightGuard(in_flight_for_stream);
                    if let Err(error) = drive(serve_one_stream(req, respond, env_addr, fn_addr)) {
                        eprintln!("h2 stream: {error}");
                    }
                }));
            }
            Some(Err(e)) => {
                eprintln!("h2 accept: {e}");
                break;
            }
            None => break,
        }
    }
}

async fn serve_one_stream(
    h2_req: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    env_addr: usize,
    fn_addr: usize,
) -> Result<(), String> {
    // Keep all handler-owned region slabs alive across async suspension and
    // release unbalanced regions on normal completion, cancellation, or a
    // response-write failure. The scheduler transfers the arena with this
    // coroutine if its worker changes while it is parked.
    let _request_arena = crate::c_abi::rc::RequestArenaGuard::new();
    let deadline = Instant::now() + STREAM_DEADLINE;
    let (parts, mut body_stream) = h2_req.into_parts();
    let method = parts.method.as_str().to_string();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or_else(|| "/".to_string(), |pq| pq.as_str().to_string());
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in parts.headers.iter() {
        if let Ok(v) = value.to_str() {
            headers.push((name.as_str().to_string(), v.to_string()));
        }
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = Deadline::new(body_stream.data(), deadline)
        .await
        .map_err(str::to_owned)?
    {
        let chunk = chunk.map_err(|e| format!("request body: {e}"))?;
        let _ = body_stream.flow_control().release_capacity(chunk.len());
        if body.len().saturating_add(chunk.len()) > MAX_REQUEST_BODY_BYTES {
            return Err(format!(
                "request body exceeds {MAX_REQUEST_BODY_BYTES}-byte cap"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let mut gos_req = GosHttpRequest::for_h2(method, path_and_query, headers, body);

    let (status, response_body) = if fn_addr == 0 {
        (200u16, b"ok".to_vec())
    } else {
        // SAFETY: fn_addr / env_addr come from a `gos_fn_addr`
        // intrinsic at the user's call site. The handler ABI is
        // the same `(env, req) -> i128` shape that
        // `gos_rt_http_serve` uses. Handlers must return
        // `Result<http::Response, http::Error>` - the runtime
        // reads disc==0 + payload as the GosHttpResponse.
        // SAFETY: fn_addr is the address of a Gossamer handler
        // emitted by the LLVM/Cranelift backend. The signature is
        // fixed by the HTTP server contract - `unsafe extern "C"
        // fn(*mut env, *mut GosHttpRequest) -> i128`.
        // The transmute reconstructs that typed pointer. The
        // handler is called once and its return value is owned by
        // this frame.
        let handler: HandlerFn = unsafe { std::mem::transmute(fn_addr) };
        let env_ptr = env_addr as *mut u8;
        let req_ptr: *mut GosHttpRequest = &raw mut gos_req;
        // SAFETY: env_ptr and req_ptr are valid for the duration of
        // this call frame; the handler is required to either
        // consume them inline or copy.
        let result_ptr = unsafe { handler(env_ptr, req_ptr) };
        let mut wire_buf: Vec<u8> = Vec::with_capacity(256);
        // HTTP/2 frames each stream itself and carries no connection
        // header, so the h1 writer is asked for its keep-alive form and
        // the header is dropped with the rest of the h1 head below.
        let ok = extract_response_into(result_ptr, &mut wire_buf, &mut true, false);
        // SAFETY: drop_handler_result frees the GosResult* the
        // handler returned. result_ptr is non-null and owned by
        // this frame (extracted above without taking ownership).
        unsafe { drop_handler_result(result_ptr) };
        gos_rt_gc_reset();
        if ok {
            parse_http1_wire_buf(&wire_buf)
        } else {
            (500u16, b"handler error".to_vec())
        }
    };

    if response_body.len() > MAX_RESPONSE_BODY_BYTES {
        return Err(format!(
            "response body exceeds {MAX_RESPONSE_BODY_BYTES}-byte cap"
        ));
    }

    let status_code = http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::OK);
    let head = match http::Response::builder().status(status_code).body(()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("h2 response build: {e}");
            return Err(format!("response build: {e}"));
        }
    };
    let body_empty = response_body.is_empty();
    let mut sender = match respond.send_response(head, body_empty) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("h2 send_response: {e}");
            return Err(format!("send response: {e}"));
        }
    };
    if !body_empty {
        send_response_body(&mut sender, response_body, deadline).await?;
    }
    Ok(())
}

/// Sends a bounded complete response in flow-control-sized frames.  Reserving
/// capacity before each frame avoids transferring the entire response into h2
/// while a slow peer withholds window credit.
async fn send_response_body(
    sender: &mut h2::SendStream<Bytes>,
    body: Vec<u8>,
    deadline: Instant,
) -> Result<(), String> {
    if body.len() > MAX_RESPONSE_BODY_BYTES {
        return Err(format!(
            "response body exceeds {MAX_RESPONSE_BODY_BYTES}-byte cap"
        ));
    }
    let mut offset = 0;
    while offset < body.len() {
        let desired = (body.len() - offset).min(MAX_SEND_CHUNK_BYTES);
        sender.reserve_capacity(desired);
        let capacity = Deadline::new(
            std::future::poll_fn(|cx| sender.poll_capacity(cx)),
            deadline,
        )
        .await
        .map_err(str::to_owned)?
        .ok_or_else(|| "response stream closed".to_string())?
        .map_err(|e| format!("response capacity: {e}"))?;
        let len = capacity.min(desired);
        if len == 0 {
            return Err("response stream has zero capacity".to_string());
        }
        offset += len;
        sender
            .send_data(
                Bytes::copy_from_slice(&body[offset - len..offset]),
                offset == body.len(),
            )
            .map_err(|e| format!("send response body: {e}"))?;
    }
    Ok(())
}

type HandlerFn = unsafe extern "C-unwind" fn(env: *mut u8, req: *mut GosHttpRequest) -> i128;

/// Parses the HTTP/1.1 wire buffer `extract_response_into`
/// emits - `HTTP/1.1 <status> <reason>\r\n[headers]\r\n\r\n<body>` -
/// into `(status, body)`.
fn parse_http1_wire_buf(buf: &[u8]) -> (u16, Vec<u8>) {
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(buf.len());
    let body = if header_end < buf.len() {
        buf[header_end..].to_vec()
    } else {
        Vec::new()
    };
    let status = buf
        .split(|&b| b == b'\n')
        .next()
        .and_then(|line| {
            let s = std::str::from_utf8(line).ok()?;
            let mut parts = s.split_whitespace();
            let _version = parts.next()?;
            parts.next().and_then(|c| c.parse::<u16>().ok())
        })
        .unwrap_or(200);
    (status, body)
}

// ---------------------------------------------------------------------------
// Goroutine future-driver. Mirrors gossamer-std::runtime_future.
// ---------------------------------------------------------------------------

struct GoroutineWaker {
    gid: Gid,
    woke: AtomicBool,
}

impl Wake for GoroutineWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.woke.store(true, Ordering::Release);
        sched_global::scheduler().unpark(self.gid);
    }
}

/// Polls `fut` to completion on the calling goroutine; parks on
/// `Pending` and resumes when the registered waker fires.
fn drive<F>(fut: F) -> F::Output
where
    F: Future,
{
    let gid = sched_global::current_gid().expect("drive must run inside a goroutine");
    let waker_arc = Arc::new(GoroutineWaker {
        gid,
        woke: AtomicBool::new(false),
    });
    let waker: Waker = waker_arc.clone().into();
    let mut ctx = Context::from_waker(&waker);
    let mut pinned: Pin<Box<F>> = Box::pin(fut);
    loop {
        waker_arc.woke.store(false, Ordering::Release);
        match pinned.as_mut().poll(&mut ctx) {
            Poll::Ready(value) => return value,
            Poll::Pending => {
                if waker_arc.woke.load(Ordering::Acquire) {
                    continue;
                }
                sched_global::park(ParkReason::Io, |_p| {});
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AsyncRead+AsyncWrite over a mio-registered non-blocking TcpStream.
// Mirrors gossamer-std::async_tcp.
// ---------------------------------------------------------------------------

struct AsyncTcpStream {
    inner: std::net::TcpStream,
    mio: mio::net::TcpStream,
    read_waker: Arc<Mutex<Option<Waker>>>,
    write_waker: Arc<Mutex<Option<Waker>>>,
}

impl AsyncRead for AsyncTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let slice = buf.initialize_unfilled();
        use std::io::Read;
        match self.inner.read(slice) {
            Ok(n) => {
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let slot = Arc::clone(&self.read_waker);
                arm_io_wake(&mut self.mio, Interest::Readable, cx, slot);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl AsyncWrite for AsyncTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        use std::io::Write;
        match self.inner.write(buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let slot = Arc::clone(&self.write_waker);
                arm_io_wake(&mut self.mio, Interest::Writable, cx, slot);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn arm_io_wake(
    stream: &mut mio::net::TcpStream,
    interest: Interest,
    cx: &mut Context<'_>,
    waker_slot: Arc<Mutex<Option<Waker>>>,
) {
    let Some(gid) = sched_global::current_gid() else {
        return;
    };
    *waker_slot.lock() = Some(cx.waker().clone());
    let waker_slot_for_fire = Arc::clone(&waker_slot);
    sched_global::register_waker(
        gid,
        Box::new(move || {
            if let Some(w) = waker_slot_for_fire.lock().take() {
                w.wake();
            }
        }),
    );
    let _ = sched_global::with_poller(|p| p.register_io(stream, interest, gid));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{MAX_CONCURRENT_STREAMS, try_begin_stream};

    #[test]
    fn local_stream_admission_never_exceeds_the_advertised_cap() {
        let in_flight = AtomicUsize::new(0);
        for _ in 0..MAX_CONCURRENT_STREAMS {
            assert!(try_begin_stream(&in_flight));
        }
        assert!(!try_begin_stream(&in_flight));
        assert_eq!(
            in_flight.load(Ordering::Acquire),
            MAX_CONCURRENT_STREAMS as usize
        );
    }
}
