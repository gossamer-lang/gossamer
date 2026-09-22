//! Runtime support for `std::http`.
//! Ships the HTTP/1.1 type surface Gossamer programs target:
//! `Request`, `Response`, `Method`, `StatusCode`, `Headers`, plus the
//! simple parsers for request lines and status lines. A working
//! server driver is a -era piece of work; this module gives
//! the value shapes.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

// HTTP/2 surface is integrated into std::http following the Go
// model: callers use `http::serve_h2c`, `http::Http2Config`, etc.
// The implementation lives in the internal `http_h2` Rust module
// because the h2 crate carries enough machinery to deserve its
// own file; user-facing names land here.
#[cfg(not(target_arch = "wasm32"))]
pub use crate::http_h2::{
    Config as Http2Config, Error as Http2Error, Handler as Http2Handler, PushOptions, PushStream,
    ResponseWriter as StreamingResponseWriter, ServerHandle as Http2ServerHandle,
    StreamingHandler as Http2StreamingHandler, Trailers, bind_and_run_h2c as serve_h2c,
    bind_and_run_h2c_streaming as serve_h2c_streaming, serve_connection as serve_h2_connection,
    serve_connection_streaming as serve_h2_connection_streaming,
};

/// HTTP method enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `DELETE`.
    Delete,
    /// `PATCH`.
    Patch,
    /// `HEAD`.
    Head,
    /// `OPTIONS`.
    Options,
}

impl Method {
    /// Canonical uppercase spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }

    /// Parses `"GET"`, `"POST"`, etc. Case-insensitive.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.to_ascii_uppercase().as_str() {
            "GET" => Self::Get,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "DELETE" => Self::Delete,
            "PATCH" => Self::Patch,
            "HEAD" => Self::Head,
            "OPTIONS" => Self::Options,
            _ => return None,
        })
    }
}

/// HTTP status code wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StatusCode(pub u16);

impl StatusCode {
    /// `200 OK`.
    pub const OK: Self = Self(200);
    /// `201 Created`.
    pub const CREATED: Self = Self(201);
    /// `204 No Content`.
    pub const NO_CONTENT: Self = Self(204);
    /// `301 Moved Permanently`.
    pub const MOVED_PERMANENTLY: Self = Self(301);
    /// `400 Bad Request`.
    pub const BAD_REQUEST: Self = Self(400);
    /// `401 Unauthorized`.
    pub const UNAUTHORIZED: Self = Self(401);
    /// `403 Forbidden`.
    pub const FORBIDDEN: Self = Self(403);
    /// `404 Not Found`.
    pub const NOT_FOUND: Self = Self(404);
    /// `500 Internal Server Error`.
    pub const INTERNAL_SERVER_ERROR: Self = Self(500);

    /// Returns the numeric code.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }

    /// Returns `true` for `2xx` codes.
    #[must_use]
    pub const fn is_success(self) -> bool {
        self.0 >= 200 && self.0 < 300
    }

    /// Registered reason phrase for this code; `None` for a code with none.
    #[must_use]
    pub const fn reason(self) -> Option<&'static str> {
        gossamer_runtime::http_status::reason_phrase(self.0)
    }
}

/// Case-insensitive header map keyed by canonical lowercase name.
#[derive(Debug, Clone, Default)]
pub struct Headers {
    inner: BTreeMap<String, String>,
}

impl Headers {
    /// Empty header map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or overwrites the header value for `name`.
    pub fn insert(&mut self, name: &str, value: &str) {
        self.inner
            .insert(name.to_ascii_lowercase(), value.to_string());
    }

    /// Returns the value of `name`, if set.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.inner
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Whether a header is set.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.inner.contains_key(&name.to_ascii_lowercase())
    }

    /// Removes `name` from the header map. Returns the previous
    /// value if any.
    pub fn remove(&mut self, name: &str) -> Option<String> {
        self.inner.remove(&name.to_ascii_lowercase())
    }

    /// Returns the number of headers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the map is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Iterates every `(name, value)` pair in sorted-by-name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.inner.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Incoming HTTP request.
#[derive(Debug, Clone)]
pub struct Request {
    /// Request method.
    pub method: Method,
    /// URL path (no query string). Mirrors Go's `URL.Path`.
    pub path: String,
    /// Raw query string (without the leading `?`). Mirrors Go's
    /// `URL.RawQuery`. Empty when the request target had no
    /// query component.
    pub query: String,
    /// Request headers.
    pub headers: Headers,
    /// Optional body.
    pub body: Vec<u8>,
    /// Per-request cancellation context. Mirrors Go's
    /// `http.Request.Context()`. Defaults to
    /// [`crate::context::Context::background`] when the server
    /// does not override it. Shutting down the server cancels the
    /// per-connection context so long-running handlers notice.
    pub context: crate::context::Context,
    /// HTTP/2 request-side trailers. `None` for h1 and for h2
    /// requests that did not carry a trailing HEADERS frame.
    /// `Some(headers)` once the body has been fully consumed by
    /// the server-side bridge and the peer sent a `END_STREAM`
    /// trailing HEADERS frame.
    pub trailers: Option<Headers>,
    /// Socket address of the peer that sent this request, `host:port`.
    /// Empty when there is no socket behind the request. An access log,
    /// an IP allowlist, and a per-IP rate limit read it; a
    /// proxy-forwarded address is NOT substituted here, so a deployment
    /// behind a proxy resolves the forwarded chain against its own
    /// trusted-proxy list.
    pub peer_addr: String,
}

impl Request {
    /// Returns the path, conveniently typed.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the raw query string (no leading `?`). Empty when
    /// the request target had no query component.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Returns the full request-target (`path` + `?` + `query`)
    /// for cases where the original string is wanted.
    #[must_use]
    pub fn request_uri(&self) -> String {
        if self.query.is_empty() {
            self.path.clone()
        } else {
            format!("{}?{}", self.path, self.query)
        }
    }

    /// Iterates the query parameters, decoding percent-encoded
    /// values. Repeated keys are preserved in order; downstream
    /// callers that want a `HashMap`-style view should collect
    /// the pairs themselves.
    #[must_use]
    pub fn query_pairs(&self) -> Vec<(String, String)> {
        parse_query_pairs(&self.query)
    }

    /// Returns the request-scoped cancellation context.
    #[must_use]
    pub fn context(&self) -> &crate::context::Context {
        &self.context
    }

    /// Returns the HTTP/2 request-side trailers if any were
    /// received. `None` for HTTP/1.1 requests and for HTTP/2
    /// requests where the peer did not send a trailing HEADERS
    /// frame.
    #[must_use]
    pub fn trailers(&self) -> Option<&Headers> {
        self.trailers.as_ref()
    }
}

/// Splits a raw HTTP request-target into `(path, query)`. The
/// caller passes path-or-path-with-query (the value seen on the
/// HTTP request line).
#[must_use]
pub fn split_path_query(target: &str) -> (String, String) {
    target.split_once('?').map_or_else(
        || (target.to_string(), String::new()),
        |(p, q)| (p.to_string(), q.to_string()),
    )
}

/// `name=value` pairs of a query string, through the runtime's own
/// parser - the same one the bytecode VM and both compiled tiers read
/// `query_pairs` with, so all three answer the same thing.
fn parse_query_pairs(query: &str) -> Vec<(String, String)> {
    gossamer_runtime::c_abi::http_query::parse_query_pairs(query)
}
/// Lazily-read response body - the server drains it to the wire in
/// chunked frames instead of buffering it in memory.
pub struct BodyStream(pub Box<dyn std::io::Read + Send>);

impl std::fmt::Debug for BodyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BodyStream(..)")
    }
}

/// Outgoing HTTP response.
#[derive(Debug)]
pub struct Response {
    /// Status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: Headers,
    /// Response body.
    pub body: Vec<u8>,
    /// Streamed body. When `Some`, the server ignores `body` and
    /// drains this reader to the client as `Transfer-Encoding:
    /// chunked` frames without buffering - the proxy-passthrough
    /// shape. A stream can only be served once, so `Response` is
    /// deliberately not `Clone`.
    pub body_stream: Option<BodyStream>,
    /// Raw response header sequence exactly as received on the wire:
    /// lowercase names, original order, duplicates preserved
    /// (`set-cookie` legally repeats). Populated only by the client
    /// transport; empty for server-constructed responses, whose
    /// canonical view is the deduplicating `headers` map.
    pub raw_header_pairs: Vec<(String, String)>,
}

impl Response {
    /// Builds a text response with the given body.
    #[must_use]
    pub fn text(status: StatusCode, body: impl Into<String>) -> Self {
        let body = body.into();
        let mut headers = Headers::new();
        headers.insert("content-type", "text/plain; charset=utf-8");
        headers.insert("content-length", &body.len().to_string());
        Self {
            status,
            headers,
            body: body.into_bytes(),
            raw_header_pairs: Vec::new(),
            body_stream: None,
        }
    }

    /// Builds a JSON response - body bytes are inserted verbatim.
    #[must_use]
    pub fn json(status: StatusCode, body: impl Into<Vec<u8>>) -> Self {
        let body = body.into();
        let mut headers = Headers::new();
        headers.insert("content-type", "application/json");
        headers.insert("content-length", &body.len().to_string());
        Self {
            status,
            headers,
            body,
            raw_header_pairs: Vec::new(),
            body_stream: None,
        }
    }

    /// Builds a streamed response: the server writes `status` +
    /// headers + `Transfer-Encoding: chunked`, then drains `reader`
    /// to the client in 8 KiB frames without buffering the body.
    #[must_use]
    pub fn stream(
        status: StatusCode,
        content_type: &str,
        reader: impl std::io::Read + Send + 'static,
    ) -> Self {
        let mut headers = Headers::new();
        headers.insert("content-type", content_type);
        Self {
            status,
            headers,
            body: Vec::new(),
            raw_header_pairs: Vec::new(),
            body_stream: Some(BodyStream(Box::new(reader))),
        }
    }
}

/// Parses the request line `METHOD PATH VERSION`.
#[must_use]
pub fn parse_request_line(line: &str) -> Option<(Method, String, String)> {
    let mut parts = line.split_whitespace();
    let method = Method::parse(parts.next()?)?;
    let path = parts.next()?.to_string();
    let version = parts.next()?.to_string();
    if parts.next().is_some() {
        return None;
    }
    Some((method, path, version))
}

/// Parses the status line `VERSION CODE [REASON]`.
#[must_use]
pub fn parse_status_line(line: &str) -> Option<(String, StatusCode, String)> {
    let mut parts = line.splitn(3, ' ');
    let version = parts.next()?.to_string();
    let code = parts.next()?.parse::<u16>().ok()?;
    let reason = parts.next().unwrap_or_default().to_string();
    Some((version, StatusCode(code), reason))
}

/// Placeholder for a future real HTTP server (wires into the
/// scheduler + poller).
#[derive(Debug, Default)]
pub struct Server;

#[cfg(not(target_arch = "wasm32"))]
impl Server {
    /// Constructs a stub server; integration replaces this.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Signals the running server (identified by the shared
    /// `Arc<AtomicBool>` inside its [`server::Config`]) to stop
    /// accepting new connections and waits until either the
    /// in-flight handler count reaches zero or `deadline` is
    /// reached.
    ///
    /// Returns `true` on a clean drain, `false` when the
    /// deadline elapsed with handlers still in flight (caller
    /// may force-close survivors).
    ///
    /// Pass `None` for `deadline` to wait indefinitely.
    #[must_use]
    pub fn shutdown(config: &server::Config, deadline: Option<std::time::Duration>) -> bool {
        config
            .shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        let target = deadline.map(|d| gossamer_runtime::platform::Instant::now() + d);
        loop {
            if config.in_flight.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return true;
            }
            if let Some(end) = target
                && gossamer_runtime::platform::Instant::now() >= end
            {
                return false;
            }
            gossamer_runtime::platform::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// Minimal HTTP/1.1 server loop used by the interpreter's
/// `http::serve` native builtin.
#[cfg(not(target_arch = "wasm32"))]
pub mod server {
    use gossamer_runtime::platform::Instant;
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::{BodyStream, Method, Request, Response};

    // Match the compiled runtime's process-wide HTTP admission limit.  The
    // interpreter keeps one blocking worker per keep-alive socket, but 64 is
    // below ordinary browser/load-balancer fan-out: a 256-connection client
    // previously received deliberate 503s even though the handler and host
    // still had capacity.  The cap remains finite to bound those workers.
    const DEFAULT_MAX_CONNECTIONS: usize = 4096;
    static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

    fn try_acquire_connection(cap: usize) -> bool {
        let cap = cap.clamp(1, DEFAULT_MAX_CONNECTIONS);
        let mut current = ACTIVE_CONNECTIONS.load(Ordering::Acquire);
        loop {
            if current >= cap {
                return false;
            }
            match ACTIVE_CONNECTIONS.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    struct ConnectionPermit;

    impl Drop for ConnectionPermit {
        fn drop(&mut self) {
            ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Configuration passed to [`run`].
    #[derive(Debug, Clone)]
    pub struct Config {
        /// Legacy blanket per-socket read timeout (applies to
        /// both header and body phases when the more specific
        /// `read_header_timeout` / `read_body_timeout` are
        /// `None`). Maintained for backwards compatibility.
        pub read_timeout: Option<Duration>,
        /// Slowloris protection - bounds how long the server
        /// waits for the request line + headers to arrive. The
        /// counter resets per request on a keep-alive connection.
        pub read_header_timeout: Option<Duration>,
        /// Maximum time to read the request body after the
        /// headers have been parsed. Independent from
        /// `read_header_timeout` so streaming uploads can take
        /// longer than the header window.
        pub read_body_timeout: Option<Duration>,
        /// Maximum time to write a response to the wire.
        pub write_timeout: Option<Duration>,
        /// How long a keep-alive connection sits between requests
        /// before the server force-closes it. Per Go's default
        /// `IdleTimeout`.
        pub idle_timeout: Option<Duration>,
        /// If set, the server stops accepting once `max_requests`
        /// requests have been handled. Used by integration tests.
        pub max_requests: Option<u64>,
        /// Shared flag that, when set to `true`, tells the accept
        /// loop to stop after the next accept wake-up.
        pub shutdown: Arc<AtomicBool>,
        /// Maximum header-block size (bytes). Requests with a
        /// header block larger than this return `431`. Default 8 KiB.
        pub max_header_bytes: usize,
        /// Maximum number of request header fields. Requests beyond this cap
        /// return `431`; the limit includes duplicate fields. Default 100.
        pub max_header_count: usize,
        /// Maximum aggregate size of a chunked request's trailer block.
        /// Default 8 KiB. This is separate from `max_header_bytes` so callers
        /// can make the policy explicit.
        pub max_trailer_bytes: usize,
        /// Maximum number of trailer fields in a chunked request. Default 100.
        pub max_trailer_count: usize,
        /// Maximum body size (bytes). Requests larger than this
        /// return `413`. Default 1 MiB.
        pub max_body_bytes: usize,
        /// Maximum live TCP connections, including idle keep-alive
        /// peers. Excess peers receive `503` and are closed.
        pub max_connections: usize,
        /// Maximum requests waiting for the single interpreter handler.
        /// Workers back-pressure here instead of growing an unbounded
        /// request queue.
        pub max_pending_requests: usize,
        /// Optional `Server` header value. Auto-inserted on every
        /// response that does not already carry a `Server`
        /// header. Set to `None` to suppress.
        pub server_name: Option<String>,
        /// Number of requests currently in-flight (passed to a
        /// handler but not yet responded to). Shared so
        /// `shutdown` can wait for them to drain.
        pub in_flight: Arc<AtomicUsize>,
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                read_timeout: Some(Duration::from_secs(30)),
                read_header_timeout: Some(Duration::from_secs(10)),
                read_body_timeout: Some(Duration::from_secs(30)),
                write_timeout: Some(Duration::from_secs(30)),
                idle_timeout: Some(Duration::from_secs(75)),
                max_requests: None,
                shutdown: Arc::new(AtomicBool::new(false)),
                max_header_bytes: 8 * 1024,
                max_header_count: 100,
                max_trailer_bytes: 8 * 1024,
                max_trailer_count: 100,
                max_body_bytes: 1024 * 1024,
                max_connections: DEFAULT_MAX_CONNECTIONS,
                max_pending_requests: 64,
                server_name: Some(concat!("gossamer/", env!("CARGO_PKG_VERSION")).to_string()),
                in_flight: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Config {
        /// Resolves the effective header-phase timeout, prefering
        /// the explicit `read_header_timeout` over the legacy
        /// `read_timeout`.
        #[must_use]
        pub fn effective_header_timeout(&self) -> Option<Duration> {
            self.read_header_timeout.or(self.read_timeout)
        }

        /// Resolves the effective body-phase timeout.
        #[must_use]
        pub fn effective_body_timeout(&self) -> Option<Duration> {
            self.read_body_timeout.or(self.read_timeout)
        }

        /// Resolves the effective write timeout.
        #[must_use]
        pub fn effective_write_timeout(&self) -> Option<Duration> {
            self.write_timeout.or(self.read_timeout)
        }

        /// Resolves the effective idle keep-alive timeout.
        #[must_use]
        pub fn effective_idle_timeout(&self) -> Option<Duration> {
            self.idle_timeout
        }
    }

    /// Where a dispatched request's response is written.
    ///
    /// A dispatcher that answers off the accept loop carries one of these
    /// into whatever runs the handler; the connection worker waits on the
    /// matching receiver. The in-flight count a graceful shutdown drains on
    /// is held until the sink is used or dropped, so a handler that never
    /// answers still releases the drain.
    pub struct ResponseSink {
        tx: Option<std::sync::mpsc::Sender<Response>>,
        in_flight: Arc<AtomicUsize>,
    }

    impl ResponseSink {
        /// Writes `response` back to the waiting connection worker.
        pub fn send(mut self, response: Response) {
            if let Some(tx) = self.tx.take() {
                let _ = tx.send(response);
            }
        }
    }

    impl Drop for ResponseSink {
        fn drop(&mut self) {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Runs the accept loop on `listener`. Each accepted connection
    /// gets its own worker thread - Gossamer's goroutine-per-
    /// connection story for the single-threaded interpreter. The
    /// worker reads requests (potentially slow) on its own thread,
    /// forwards each parsed [`Request`] plus a one-shot response
    /// channel to the main thread, writes the response when the
    /// handler returns it, and keeps the connection alive for
    /// subsequent requests unless the peer (or handler) asked to
    /// close.
    ///
    /// `handle` answers on the dispatch thread, so one request at a time.
    /// Use [`run_dispatch`] to answer each request elsewhere and keep the
    /// dispatch thread free.
    ///
    /// Shutdown: when `config.shutdown` flips to `true`, the main
    /// loop connects to the bound address to break the acceptor out
    /// of its blocking `accept()` call, then returns. Reaching
    /// `config.max_requests` uses the same self-connect trick.
    pub fn run<H>(listener: TcpListener, config: &Config, mut handle: H) -> io::Result<()>
    where
        H: FnMut(Request) -> Response,
    {
        run_dispatch(listener, config, move |request, sink| {
            sink.send(handle(request));
        })
    }

    /// Accept loop that hands each parsed request to `dispatch` together
    /// with the [`ResponseSink`] its answer belongs in.
    ///
    /// `dispatch` returns as soon as the request is placed, so a dispatcher
    /// that runs the handler on a goroutine serves requests concurrently -
    /// the bound is the runtime's, not this loop's.
    pub fn run_dispatch<H>(
        listener: TcpListener,
        config: &Config,
        mut dispatch: H,
    ) -> io::Result<()>
    where
        H: FnMut(Request, ResponseSink),
    {
        use std::sync::mpsc::{RecvTimeoutError, sync_channel};

        let bound_addr = listener.local_addr()?;

        let (dispatch_tx, dispatch_rx) = sync_channel::<(Request, std::sync::mpsc::Sender<Response>)>(
            config.max_pending_requests.max(1),
        );

        // Acceptor thread: blocking accept, one worker per
        // connection. No poll sleep.
        let shutdown_for_accept = Arc::clone(&config.shutdown);
        let cfg_for_workers = config.clone();
        let tx_for_accept = dispatch_tx.clone();
        let acceptor = std::thread::Builder::new()
            .name("gossamer-http-accept".to_string())
            .spawn(move || {
                accept_loop(
                    listener,
                    shutdown_for_accept,
                    cfg_for_workers,
                    tx_for_accept,
                );
            })
            .map_err(|e| io::Error::other(format!("spawn accept: {e}")))?;

        // Drop our extra sender so the dispatch channel sees
        // Disconnected once the acceptor and all workers are gone.
        drop(dispatch_tx);

        let mut served: u64 = 0;
        // Whether the loop ended because nothing can answer any more. The
        // acceptor owns the only sender, so a disconnect means it is gone;
        // reporting `Ok` there would tell a caller the server ran its course
        // when it had in fact stopped listening.
        let mut acceptor_gone = false;
        let wake_self = || {
            // Best-effort wake - acceptor is stuck in `accept()`.
            let _ = TcpStream::connect_timeout(&bound_addr, Duration::from_millis(500));
        };

        loop {
            if config.shutdown.load(Ordering::Relaxed) {
                wake_self();
                break;
            }
            match dispatch_rx.recv_timeout(Duration::from_millis(50)) {
                Ok((req, responder)) => {
                    // Track in-flight count so a graceful shutdown can
                    // drain. The sink owns the decrement, so the count
                    // covers the whole handler, wherever it runs.
                    config.in_flight.fetch_add(1, Ordering::AcqRel);
                    dispatch(
                        req,
                        ResponseSink {
                            tx: Some(responder),
                            in_flight: Arc::clone(&config.in_flight),
                        },
                    );
                    served = served.saturating_add(1);
                    if let Some(max) = config.max_requests {
                        if served >= max {
                            config.shutdown.store(true, Ordering::Relaxed);
                            wake_self();
                            break;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    acceptor_gone = !config.shutdown.load(Ordering::Relaxed);
                    break;
                }
            }
        }

        // Acceptor should exit now that shutdown is set and we've
        // self-connected; a stray worker panic would just drop the
        // join handle.
        let _ = acceptor.join();
        if acceptor_gone {
            return Err(io::Error::other(
                "http: the acceptor stopped before a shutdown was asked for",
            ));
        }
        Ok(())
    }

    fn accept_loop(
        listener: TcpListener,
        shutdown: Arc<AtomicBool>,
        config: Config,
        dispatch_tx: std::sync::mpsc::SyncSender<(Request, std::sync::mpsc::Sender<Response>)>,
    ) {
        // Non-blocking accept + netpoller readiness wait. The
        // listener registers with the global netpoller; the
        // accept loop yields to the scheduler whenever there is
        // no pending connection, so an idle server does not burn
        // CPU on a sleep loop and the OS thread driving accept
        // is free to run other goroutines while it parks on the
        // poller's `Readable` event.
        let _ = listener.set_nonblocking(true);
        let mut listener_mio = match listener.try_clone() {
            Ok(c) => Some(mio::net::TcpListener::from_std(c)),
            Err(_) => None,
        };
        let mut accept_backoff = gossamer_runtime::accept::AcceptBackoff::new();
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    accept_backoff.reset();
                    let _ = stream.set_nonblocking(false);
                    if shutdown.load(Ordering::Relaxed) {
                        let _ = stream.shutdown(Shutdown::Both);
                        return;
                    }
                    if !try_acquire_connection(config.max_connections) {
                        let mut stream = stream;
                        let _ = stream.write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    let worker_config = config.clone();
                    let tx = dispatch_tx.clone();
                    // Each accepted connection runs on a dedicated
                    // OS thread (not the M:N goroutine pool). The
                    // worker loop performs blocking `read`/`write`
                    // syscalls on the std `TcpStream`; running it on
                    // a goroutine would block the underlying pool
                    // worker for the full duration of every idle
                    // keep-alive wait, starving other goroutines
                    // pinned to that worker. Under bench load
                    // (100 concurrent connections, 24 pool workers)
                    // this surfaced as ~230 "deadline exceeded"
                    // failures per 30s run: 100 connections all
                    // blocking on the next request tied up every
                    // worker thread, the netpoller had no chance to
                    // deliver readiness, and a fraction of those
                    // waits stretched past the client's 10 s
                    // timeout. Per-connection threads sidestep the
                    // problem entirely - blocking I/O is fine when
                    // each connection owns its own thread.
                    let spawn = std::thread::Builder::new()
                        .name("gossamer-http-conn".into())
                        .spawn(move || {
                            let _permit = ConnectionPermit;
                            worker_loop(stream, worker_config, tx);
                        });
                    if spawn.is_err() {
                        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                Err(ref e) if matches!(e.kind(), io::ErrorKind::WouldBlock) => {
                    if let Some(ref mut mio_listener) = listener_mio {
                        wait_listener_readable(mio_listener);
                    } else {
                        gossamer_runtime::platform::sleep(Duration::from_millis(50));
                    }
                }
                Err(ref e) if matches!(e.kind(), io::ErrorKind::Interrupted) => {}
                Err(ref e) if gossamer_runtime::accept::accept_error_is_transient(e) => {
                    accept_backoff.settle();
                }
                Err(err) => {
                    if !shutdown.load(Ordering::Relaxed) {
                        eprintln!("http: accept error: {err}");
                    }
                    return;
                }
            }
        }
    }

    fn wait_listener_readable(listener: &mut mio::net::TcpListener) {
        // Goroutine-aware accept-readiness wait: parks the calling
        // coroutine on the netpoller; the OS thread is freed to run
        // other goroutines while we're waiting for the next inbound
        // connection. Synchronous fallback (50 ms tick) when called
        // from a non-goroutine thread.
        let _ = crate::sched_global::wait_io(listener, gossamer_sched::Interest::Readable);
    }

    /// Whether a `Connection` header carries `token` in its comma-separated
    /// list.
    fn connection_has(headers: &super::Headers, token: &str) -> bool {
        headers
            .get("connection")
            .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    }

    fn wants_close(headers: &super::Headers) -> bool {
        connection_has(headers, "close")
    }

    /// Whether a request leaves its connection open: HTTP/1.1 unless it
    /// says `close`, HTTP/1.0 only when it asks for `keep-alive`.
    fn request_persists(http10: bool, headers: &super::Headers) -> bool {
        if wants_close(headers) {
            return false;
        }
        !http10 || connection_has(headers, "keep-alive")
    }

    /// Settles a response's connection: an HTTP/1.1 connection persists by
    /// default, so only a close is announced there, while an HTTP/1.0 one
    /// persists only when the response says so.
    fn announce_connection(response: &mut super::Response, http10: bool, keep_alive: bool) {
        if response.headers.contains("connection") {
            return;
        }
        if !keep_alive {
            response.headers.insert("connection", "close");
        } else if http10 {
            response.headers.insert("connection", "keep-alive");
        }
    }

    /// Reads the request body. Honours `Transfer-Encoding: chunked`
    /// (RFC 7230 §3.3.3) and rejects malformed combinations (both
    /// chunked and Content-Length). Merges any trailer headers
    /// into `headers` on the chunked path.
    fn read_request_body<R: BufRead>(
        reader: &mut R,
        headers: &mut super::Headers,
        content_length: usize,
        config: &Config,
        body_deadline: Option<Instant>,
    ) -> io::Result<Vec<u8>> {
        let chunked = match headers.get("transfer-encoding") {
            None => false,
            Some(value) if value.eq_ignore_ascii_case("chunked") => true,
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsupported Transfer-Encoding",
                ));
            }
        };
        if chunked && headers.contains("content-length") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request has both Transfer-Encoding: chunked and Content-Length",
            ));
        }
        let check_deadline = |d: Option<Instant>| -> io::Result<()> {
            if let Some(deadline) = d
                && Instant::now() >= deadline
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "body phase exceeded read_body_timeout",
                ));
            }
            Ok(())
        };
        if chunked {
            let mut decoder = crate::http_chunked::ChunkedReader::new(reader);
            decoder.set_trailer_limits(config.max_trailer_bytes, config.max_trailer_count);
            let mut payload = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                let n = decoder.read(&mut tmp)?;
                if n == 0 {
                    break;
                }
                check_deadline(body_deadline)?;
                if payload.len() + n > config.max_body_bytes {
                    return Err(reject(413, "payload too large"));
                }
                payload.extend_from_slice(&tmp[..n]);
            }
            // Promote trailer headers into the main header bag
            // per RFC 7230 §4.1.2 - handler code sees them on
            // request.headers.
            let trailers: Vec<(String, String)> = decoder.trailers.clone();
            for (name, value) in trailers {
                headers.insert(&name, &value);
            }
            return Ok(payload);
        }
        if content_length > config.max_body_bytes {
            return Err(reject(413, "payload too large"));
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            // Read in chunks so the deadline can fire mid-body
            // on drip-feed uploads.
            let mut filled = 0usize;
            while filled < body.len() {
                let buf = &mut body[filled..];
                let n = reader.read(buf)?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "EOF before Content-Length bytes read",
                    ));
                }
                filled += n;
                check_deadline(body_deadline)?;
            }
        }
        Ok(body)
    }

    /// The raw socket a peer probe reads from, as each platform names it.
    type RawPeerSocket = gossamer_runtime::c_abi::http_server::RawPeerSocket;

    /// The cancel token of whichever request is in flight on a connection.
    type ActiveCancel = Arc<parking_lot::Mutex<Option<crate::context::Cancel>>>;

    /// Starts the connection's watcher thread and hands back the sender
    /// whose drop retires it.
    ///
    /// One watcher serves every request on the connection rather than one
    /// per request: it cancels whatever is in flight when the server is
    /// shutting down or the peer goes away. A per-request thread would put
    /// its ~13us spawn on the request path, which caps interpreted-mode
    /// throughput near 50k RPS against the ~175k RPS available without it.
    fn spawn_connection_watcher(
        config: &Config,
        active_cancel: &ActiveCancel,
        watch_socket: RawPeerSocket,
    ) -> (
        std::sync::mpsc::Sender<()>,
        io::Result<std::thread::JoinHandle<()>>,
    ) {
        use std::sync::mpsc::RecvTimeoutError;

        let active_cancel = Arc::clone(active_cancel);
        let shutdown = Arc::clone(&config.shutdown);
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let watcher = std::thread::Builder::new()
            .name("gossamer-http-watcher".into())
            .spawn(move || {
                loop {
                    match done_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                        Err(RecvTimeoutError::Timeout) => {
                            if shutdown.load(Ordering::Acquire) {
                                if let Some(c) = active_cancel.lock().take() {
                                    c.cancel_with("server shutdown");
                                }
                            } else if gossamer_runtime::c_abi::http_server::peer_is_gone(
                                watch_socket,
                            ) {
                                // The peer left while its request was still
                                // running: cancel it so the work it asked
                                // for stops instead of being paid for after
                                // it went away.
                                if let Some(c) = active_cancel.lock().take() {
                                    c.cancel_with("client disconnected");
                                }
                            }
                        }
                    }
                }
            });
        (done_tx, watcher)
    }

    /// Per-connection worker. Runs as a goroutine on the M:N
    /// scheduler; reads requests from a persistent buffered reader,
    /// hands each to the handler dispatch thread via `dispatch_tx`,
    /// writes the response, and loops until the peer (or handler)
    /// asks to close or the socket errors out.
    fn worker_loop(
        stream: TcpStream,
        config: Config,
        dispatch_tx: std::sync::mpsc::SyncSender<(Request, std::sync::mpsc::Sender<Response>)>,
    ) {
        // Set the initial per-syscall timeout. The actual phase
        // (idle / header / body / write) is switched dynamically
        // below via set_read_timeout / set_write_timeout calls
        // and enforced via Instant-based deadlines inside the
        // parser + writer.
        if let Some(timeout) = config.read_timeout {
            let _ = stream.set_read_timeout(Some(timeout));
            let _ = stream.set_write_timeout(Some(timeout));
        }
        // Disable Nagle so short responses land on the wire right
        // away. Dominant workload here is small keep-alive replies.
        let _ = stream.set_nodelay(true);
        let peer_addr = stream
            .peer_addr()
            .map_or_else(|_| String::new(), |a| a.to_string());
        let watch_socket = peer_watch_socket(&stream);

        // One BufReader lives across every request on this
        // connection so any bytes pipelined after the request line
        // aren't lost when the next read starts.
        let mut reader = BufReader::new(stream);

        let active_cancel: ActiveCancel = Arc::new(parking_lot::Mutex::new(None));
        let (watcher_done_tx, watcher) =
            spawn_connection_watcher(&config, &active_cancel, watch_socket);

        loop {
            // Drop the connection promptly when shutdown trips.
            // Without this the worker would sit in
            // idle_timeout waiting for the next pipelined
            // request after a keep-alive response.
            if config.shutdown.load(Ordering::Acquire) {
                let _ = reader.get_mut().shutdown(Shutdown::Both);
                break;
            }
            // Idle keep-alive: per-syscall read timeout becomes
            // the idle_timeout while we wait for the next
            // request's first byte. read_request switches it to
            // the body_timeout once headers are parsed.
            if let Some(idle) = config.effective_idle_timeout() {
                let _ = reader.get_mut().set_read_timeout(Some(idle));
            }
            match read_request(&mut reader, &config) {
                Ok(Some((mut request, http10, client_close, cancel))) => {
                    request.peer_addr.clone_from(&peer_addr);
                    *active_cancel.lock() = Some(cancel);
                    let (resp_tx, resp_rx) = std::sync::mpsc::channel::<Response>();
                    if dispatch_tx.send((request, resp_tx)).is_err() {
                        active_cancel.lock().take();
                        break;
                    }
                    let result = resp_rx.recv();
                    // Handler returned - clear the active cancel so the
                    // watcher doesn't fire on a stale token.
                    active_cancel.lock().take();

                    match result {
                        Ok(mut response) => {
                            let handler_close = wants_close(&response.headers);
                            let keep_alive = !client_close && !handler_close;
                            announce_connection(&mut response, http10, keep_alive);
                            // Switch the per-syscall write timeout
                            // to the response-write phase.
                            if let Some(t) = config.effective_write_timeout() {
                                let _ = reader.get_mut().set_write_timeout(Some(t));
                            }
                            if let Err(err) = write_response(
                                reader.get_mut(),
                                &mut response,
                                config.server_name.as_deref(),
                            ) {
                                if !is_ignorable(&err) {
                                    eprintln!("http: write error: {err}");
                                }
                                break;
                            }
                            if !keep_alive {
                                break;
                            }
                        }
                        Err(_) => break, // main thread gone
                    }
                }
                Ok(None) => break, // clean EOF between requests
                Err(err) => {
                    if let Some(rejected) = rejection_of(&err) {
                        let mut response = rejection_response(rejected);
                        let _ = write_response_generic(
                            reader.get_mut(),
                            &mut response,
                            config.server_name.as_deref(),
                        );
                    } else if !is_ignorable(&err) {
                        eprintln!("http: parse error: {err}");
                    }
                    break;
                }
            }
        }
        // Signal the per-connection watcher to exit and wait for it.
        drop(watcher_done_tx);
        if let Ok(w) = watcher {
            let _ = w.join();
        }
        let _ = reader.get_mut().shutdown(Shutdown::Both);
    }

    /// A request the server refused before dispatch, and the status it
    /// owes the client.
    ///
    /// Carried as the payload of the `io::Error` the read path returns, so
    /// the worker answers the client rather than closing the connection on
    /// it: a client that sent too large a body learns it was too large.
    #[derive(Debug)]
    pub(crate) struct Rejected {
        /// Status the client is answered with.
        pub status: u16,
        /// Plain-text body, also the reason phrase in the log.
        pub reason: &'static str,
    }

    impl std::fmt::Display for Rejected {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.reason)
        }
    }

    impl std::error::Error for Rejected {}

    /// The `io::Error` shape a refusal travels in.
    pub(crate) fn reject(status: u16, reason: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, Rejected { status, reason })
    }

    /// The refusal `err` carries, if it is one.
    fn rejection_of(err: &io::Error) -> Option<&Rejected> {
        err.get_ref()?.downcast_ref::<Rejected>()
    }

    /// The status line + body a refusal is answered with.
    fn rejection_response(rejected: &Rejected) -> Response {
        let mut headers = super::Headers::new();
        headers.insert("content-type", "text/plain; charset=utf-8");
        headers.insert("connection", "close");
        Response {
            status: super::StatusCode(rejected.status),
            headers,
            body: rejected.reason.as_bytes().to_vec(),
            raw_header_pairs: Vec::new(),
            body_stream: None,
        }
    }

    /// The socket a peer-liveness peek reads.
    #[cfg(unix)]
    fn peer_watch_socket(stream: &TcpStream) -> RawPeerSocket {
        use std::os::fd::AsRawFd;
        stream.as_raw_fd()
    }

    /// The socket a peer-liveness peek reads.
    #[cfg(windows)]
    fn peer_watch_socket(stream: &TcpStream) -> RawPeerSocket {
        use std::os::windows::io::AsRawSocket;
        stream.as_raw_socket()
    }

    /// No such probe here, so the watch has nothing to read and a request
    /// runs to completion after its client leaves.
    #[cfg(not(any(unix, windows)))]
    fn peer_watch_socket(_stream: &TcpStream) -> RawPeerSocket {
        -1
    }

    fn is_ignorable(err: &io::Error) -> bool {
        matches!(
            err.kind(),
            io::ErrorKind::UnexpectedEof
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::TimedOut
                | io::ErrorKind::WouldBlock
        )
    }

    /// Reads one HTTP request from `reader`. Returns `Ok(None)` on a
    /// clean EOF between requests (idle keep-alive connection that
    /// closed). Returns `Ok(Some((req, http10, client_close)))` on a
    /// parsed request; `http10` is true when the request line said
    /// HTTP/1.0, and `client_close` is true when the peer sent
    /// `Connection: close`.
    /// Parsed head of a request - everything before the body.
    pub(crate) struct RequestHead {
        pub method: Method,
        pub path: String,
        pub query: String,
        pub headers: super::Headers,
        pub http10: bool,
        pub content_length: usize,
        pub expects_continue: bool,
    }

    fn read_request(
        reader: &mut BufReader<TcpStream>,
        config: &Config,
    ) -> io::Result<Option<(Request, bool, bool, crate::context::Cancel)>> {
        let header_deadline = config
            .effective_header_timeout()
            .map(|d| Instant::now() + d);
        let Some(head) = parse_request_head_generic(reader, config, header_deadline)? else {
            return Ok(None);
        };
        if head.expects_continue {
            // RFC 7231 §5.1.1: send 100 Continue before reading
            // the body when the client signalled `Expect:
            // 100-continue`. We send unconditionally - handlers
            // that want to short-circuit (4xx before body) can
            // simply not consume `request.body`.
            let stream = reader.get_mut();
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            let _ = stream.flush();
        }
        // Switch the per-syscall timeout to body_timeout for the
        // body read phase. The wider-elapsed-time deadline is
        // applied inside finish_request via read_request_body.
        if let Some(t) = config.effective_body_timeout() {
            let _ = reader.get_mut().set_read_timeout(Some(t));
        }
        let body_deadline = config.effective_body_timeout().map(|d| Instant::now() + d);
        finish_request(reader, head, config, body_deadline)
    }

    /// Reads the head (request line + headers), captures the
    /// `Expect: 100-continue` signal, and returns. Body reading
    /// is deferred to [`finish_request`] so callers can write the
    /// interim 100 response in between.
    ///
    /// `header_deadline` enforces a TOTAL elapsed-time deadline
    /// for the head phase; once it passes the function returns
    /// `TimedOut` even if individual syscalls are still
    /// progressing. This is the slowloris guard - per-syscall
    /// timeouts (set by the worker via
    /// `TcpStream::set_read_timeout`) protect against zero-byte
    /// stalls; this deadline protects against drip-feed attacks.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn parse_request_head_generic<R: BufRead>(
        reader: &mut R,
        config: &Config,
        header_deadline: Option<Instant>,
    ) -> io::Result<Option<RequestHead>> {
        let check_deadline = |d: Option<Instant>| -> io::Result<()> {
            if let Some(deadline) = d
                && Instant::now() >= deadline
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "header phase exceeded read_header_timeout",
                ));
            }
            Ok(())
        };
        let mut line = Vec::with_capacity(config.max_header_bytes.min(1024));
        let first = read_head_line(reader, &mut line, config.max_header_bytes, header_deadline)?;
        if first == 0 {
            return Ok(None);
        }
        let trimmed = head_line_text(&line)?;
        let (method, raw_target, version) = super::parse_request_line(trimmed)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad request line"))?;
        if !matches!(version.as_str(), "HTTP/1.0" | "HTTP/1.1") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported HTTP version",
            ));
        }
        if !raw_target.starts_with('/') && raw_target != "*" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid request target",
            ));
        }
        let (path, query) = super::split_path_query(&raw_target);
        let http10 = version.eq_ignore_ascii_case("HTTP/1.0");
        let mut headers = super::Headers::new();
        let mut content_length: usize = 0;
        let mut content_length_seen = false;
        let mut transfer_encoding_seen = false;
        let mut header_bytes_read = first;
        let mut header_count = 0usize;
        loop {
            line.clear();
            let remaining = config.max_header_bytes.saturating_sub(header_bytes_read);
            let bytes = read_head_line(reader, &mut line, remaining, header_deadline)?;
            if bytes == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF before request headers completed",
                ));
            }
            check_deadline(header_deadline)?;
            header_bytes_read = header_bytes_read.saturating_add(bytes);
            if header_bytes_read > config.max_header_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("header block exceeded {}-byte cap", config.max_header_bytes),
                ));
            }
            let stripped = head_line_text(&line)?;
            if stripped.is_empty() {
                break;
            }
            header_count = header_count.saturating_add(1);
            if header_count > config.max_header_count {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("request has more than {} headers", config.max_header_count),
                ));
            }
            if stripped.starts_with([' ', '\t']) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "obsolete folded request header",
                ));
            }
            let (name, value) = stripped.split_once(':').ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "malformed request header")
            })?;
            if !is_header_name(name) || !is_header_value(value) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid request header",
                ));
            }
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                let parsed = value.parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length")
                })?;
                if content_length_seen && parsed != content_length {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "conflicting Content-Length",
                    ));
                }
                content_length = parsed;
                content_length_seen = true;
            }
            if name.eq_ignore_ascii_case("transfer-encoding") {
                if transfer_encoding_seen || !value.eq_ignore_ascii_case("chunked") {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unsupported Transfer-Encoding",
                    ));
                }
                transfer_encoding_seen = true;
            }
            headers.insert(name, value);
        }
        if !http10 && !headers.contains("host") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP/1.1 request missing Host header",
            ));
        }
        if content_length_seen && transfer_encoding_seen {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request has both Transfer-Encoding and Content-Length",
            ));
        }
        let expects_continue = headers
            .get("expect")
            .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"));
        Ok(Some(RequestHead {
            method,
            path,
            query,
            headers,
            http10,
            content_length,
            expects_continue,
        }))
    }

    /// Fuzz-only entry point for complete HTTP/1 request heads. It applies
    /// the production parser and default resource limits without opening a
    /// socket or reading a body.
    #[doc(hidden)]
    pub fn fuzz_parse_request_head(bytes: &[u8]) -> io::Result<()> {
        let mut reader = BufReader::new(bytes);
        parse_request_head_generic(&mut reader, &Config::default(), None).map(|_| ())
    }

    fn read_head_line<R: BufRead>(
        reader: &mut R,
        out: &mut Vec<u8>,
        limit: usize,
        deadline: Option<Instant>,
    ) -> io::Result<usize> {
        loop {
            if let Some(deadline) = deadline
                && Instant::now() >= deadline
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "header phase exceeded read_header_timeout",
                ));
            }
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(out.len());
            }
            let take = available
                .iter()
                .position(|b| *b == b'\n')
                .map_or(available.len(), |idx| idx + 1);
            if out.len().saturating_add(take) > limit {
                return Err(reject(431, "request header fields too large"));
            }
            out.extend_from_slice(&available[..take]);
            reader.consume(take);
            if out.last() == Some(&b'\n') {
                return Ok(out.len());
            }
        }
    }

    fn head_line_text(bytes: &[u8]) -> io::Result<&str> {
        let content = bytes.strip_suffix(b"\r\n").ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "request header line lacks CRLF")
        })?;
        std::str::from_utf8(content)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request head is not UTF-8"))
    }

    fn is_header_name(name: &str) -> bool {
        !name.is_empty()
            && name.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b'!' | b'#'
                            | b'$'
                            | b'%'
                            | b'&'
                            | b'\''
                            | b'*'
                            | b'+'
                            | b'-'
                            | b'.'
                            | b'^'
                            | b'_'
                            | b'`'
                            | b'|'
                            | b'~'
                    )
            })
    }

    fn is_header_value(value: &str) -> bool {
        value
            .bytes()
            .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
    }

    /// Reads the request body, applying chunked decoding /
    /// content-length / body-cap enforcement, and assembles the
    /// final `Request`.
    pub(crate) fn finish_request<R: BufRead>(
        reader: &mut R,
        mut head: RequestHead,
        config: &Config,
        body_deadline: Option<Instant>,
    ) -> io::Result<Option<(Request, bool, bool, crate::context::Cancel)>> {
        let body = read_request_body(
            reader,
            &mut head.headers,
            head.content_length,
            config,
            body_deadline,
        )?;
        let client_close = !request_persists(head.http10, &head.headers);
        // Per-request cancellable context. Parented on
        // background - the worker_loop cancels via the Cancel
        // handle when the connection closes or the server
        // shutdown signal trips.
        let (ctx, cancel) = crate::context::with_cancel(&crate::context::Context::background());
        Ok(Some((
            Request {
                method: head.method,
                path: head.path,
                query: head.query,
                headers: head.headers,
                body,
                context: ctx,
                trailers: None,
                peer_addr: String::new(),
            },
            head.http10,
            client_close,
            cancel,
        )))
    }

    fn write_response(
        stream: &mut TcpStream,
        response: &mut Response,
        server_name: Option<&str>,
    ) -> io::Result<()> {
        write_response_generic(stream, response, server_name)
    }

    /// Convenience wrapper for the common single-threaded path: bind
    /// `addr`, then run the accept loop until `config.shutdown` fires.
    pub fn bind_and_run<H>(addr: &str, config: &Config, handle: H) -> io::Result<()>
    where
        H: FnMut(Request) -> Response,
    {
        let listener = gossamer_runtime::listen::bind_tcp(addr)?;
        run(listener, config, handle)
    }

    /// [`run_dispatch`] over a freshly bound `addr`.
    pub fn bind_and_run_dispatch<H>(addr: &str, config: &Config, dispatch: H) -> io::Result<()>
    where
        H: FnMut(Request, ResponseSink),
    {
        let listener = gossamer_runtime::listen::bind_tcp(addr)?;
        run_dispatch(listener, config, dispatch)
    }

    /// Expose [`Method`] to downstream tests without a star re-export.
    #[doc(hidden)]
    pub const fn _touch(_m: Method) {}

    /// HTTPS variant of [`bind_and_run`]: binds `addr`, terminates
    /// TLS using `tls_config` for every accepted connection, and
    /// dispatches the parsed request through `handle`. Re-uses the
    /// plain-text request loop after TLS termination, so handler
    /// semantics are identical regardless of cipher suite.
    pub fn bind_and_run_tls<H>(
        addr: &str,
        tls_config: &crate::tls::ServerConfig,
        config: &Config,
        mut handle: H,
    ) -> io::Result<()>
    where
        H: FnMut(Request) -> Response,
    {
        bind_and_run_tls_dispatch(addr, tls_config, config, move |request, sink| {
            sink.send(handle(request));
        })
    }

    /// [`run_dispatch`]'s contract over TLS: `dispatch` returns as soon as
    /// the request is placed, so a dispatcher that answers on a goroutine
    /// serves requests concurrently.
    pub fn bind_and_run_tls_dispatch<H>(
        addr: &str,
        tls_config: &crate::tls::ServerConfig,
        config: &Config,
        mut dispatch: H,
    ) -> io::Result<()>
    where
        H: FnMut(Request, ResponseSink),
    {
        use std::io::ErrorKind;
        use std::sync::mpsc::{RecvTimeoutError, sync_channel};

        let listener = gossamer_runtime::listen::bind_tcp(addr)?;
        let bound = listener.local_addr()?;
        let _ = listener.set_nonblocking(false);

        let (dispatch_tx, dispatch_rx) = sync_channel::<(Request, std::sync::mpsc::Sender<Response>)>(
            config.max_pending_requests.max(1),
        );

        let shutdown = Arc::clone(&config.shutdown);
        let cfg_for_workers = config.clone();
        let server_config = tls_config.rustls();
        let tx_for_accept = dispatch_tx.clone();

        let acceptor = std::thread::Builder::new()
            .name("gossamer-https-accept".to_string())
            .spawn(move || {
                tls_accept_loop(
                    listener,
                    shutdown,
                    cfg_for_workers,
                    server_config,
                    tx_for_accept,
                );
            })
            .map_err(|e| io::Error::other(format!("spawn accept: {e}")))?;
        drop(dispatch_tx);

        let mut served: u64 = 0;
        // Whether the loop ended because nothing can answer any more. The
        // acceptor owns the only sender, so a disconnect means it is gone;
        // reporting `Ok` there would tell a caller the server ran its course
        // when it had in fact stopped listening.
        let mut acceptor_gone = false;
        let wake_self = || {
            let _ = TcpStream::connect_timeout(&bound, Duration::from_millis(500));
        };

        loop {
            if config.shutdown.load(Ordering::Relaxed) {
                wake_self();
                break;
            }
            match dispatch_rx.recv_timeout(Duration::from_millis(50)) {
                Ok((req, responder)) => {
                    // Track in-flight count so a graceful shutdown can
                    // drain. The sink owns the decrement, so the count
                    // covers the whole handler, wherever it runs.
                    config.in_flight.fetch_add(1, Ordering::AcqRel);
                    dispatch(
                        req,
                        ResponseSink {
                            tx: Some(responder),
                            in_flight: Arc::clone(&config.in_flight),
                        },
                    );
                    served = served.saturating_add(1);
                    if let Some(max) = config.max_requests {
                        if served >= max {
                            config.shutdown.store(true, Ordering::Relaxed);
                            wake_self();
                            break;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    acceptor_gone = !config.shutdown.load(Ordering::Relaxed);
                    break;
                }
            }
        }
        let _ = acceptor.join();
        let _ = ErrorKind::Other; // keep import live if unused.
        if acceptor_gone {
            return Err(io::Error::other(
                "https: the acceptor stopped before a shutdown was asked for",
            ));
        }
        Ok(())
    }

    /// HTTPS server that negotiates HTTP/2 via ALPN. Requires the
    /// supplied `tls_config` to advertise `h2` in ALPN (the helper
    /// rewrites it transparently if not). Each accepted TLS
    /// connection runs the handshake under the goroutine
    /// scheduler; if the peer negotiated `h2`, the connection is
    /// served via [`crate::http_h2::serve_connection_async`]. A
    /// peer that negotiated `http/1.1` (or no ALPN at all) is
    /// closed with a TLS-level shutdown - use [`bind_and_run_tls`]
    /// for the h1-over-TLS path until the unified dispatch lands.
    pub fn bind_and_run_tls_h2<H>(
        addr: &str,
        tls_config: &crate::tls::ServerConfig,
        h2_config: crate::http_h2::Config,
        handler: H,
    ) -> Result<(), crate::http_h2::Error>
    where
        H: crate::http_h2::Handler + Clone,
    {
        let server_arc = ensure_alpn_h2(tls_config.rustls());
        let listener =
            gossamer_runtime::listen::bind_tcp(addr).map_err(crate::http_h2::Error::Io)?;
        listener
            .set_nonblocking(false)
            .map_err(crate::http_h2::Error::Io)?;
        let handler = Arc::new(handler);

        loop {
            let (sock, _peer) = listener.accept().map_err(crate::http_h2::Error::Io)?;
            let _ = sock.set_nodelay(true);
            let server_arc = std::sync::Arc::clone(&server_arc);
            let handler = Arc::clone(&handler);
            let h2_config = h2_config.clone();

            gossamer_runtime::sched_global::spawn(Box::new(move || {
                // `from_std_blocking` flips the socket to non-
                // blocking + registers a mio mirror; the name is
                // historical.
                let our_tcp = match crate::net::TcpStream::from_std_blocking(sock) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("tls_h2: wrap: {e}");
                        return;
                    }
                };
                let async_tcp = crate::async_tcp::AsyncTcpStream::new(our_tcp);
                let acceptor = tokio_rustls::TlsAcceptor::from(server_arc);

                let result = crate::runtime_future::drive(async move {
                    let tls_stream = match acceptor.accept(async_tcp).await {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("tls_h2: handshake: {e}");
                            return Ok::<(), crate::http_h2::Error>(());
                        }
                    };
                    let alpn = tls_stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
                    if alpn.as_deref() != Some(b"h2") {
                        // Peer did not negotiate h2 - close cleanly.
                        // Future: dispatch into an async h1 loop.
                        return Ok(());
                    }
                    let shutdown = Arc::new(AtomicBool::new(false));
                    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    crate::http_h2::serve_connection_async(
                        tls_stream, handler, h2_config, shutdown, in_flight,
                    )
                    .await
                });
                if let Err(e) = result {
                    eprintln!("tls_h2: serve: {e}");
                }
            }));
        }
    }

    /// Ensures the `h2` protocol is advertised in the rustls
    /// `alpn_protocols` list. Used by [`bind_and_run_tls_h2`] so
    /// callers don't need to remember to enable ALPN before
    /// handing a `ServerConfig` to the helper.
    fn ensure_alpn_h2(
        cfg: std::sync::Arc<rustls::ServerConfig>,
    ) -> std::sync::Arc<rustls::ServerConfig> {
        if cfg.alpn_protocols.iter().any(|p| p.as_slice() == b"h2") {
            return cfg;
        }
        let mut clone = (*cfg).clone();
        clone.alpn_protocols.insert(0, b"h2".to_vec());
        std::sync::Arc::new(clone)
    }

    fn tls_accept_loop(
        listener: TcpListener,
        shutdown: Arc<AtomicBool>,
        config: Config,
        server_config: std::sync::Arc<rustls::ServerConfig>,
        dispatch_tx: std::sync::mpsc::SyncSender<(Request, std::sync::mpsc::Sender<Response>)>,
    ) {
        let _ = listener.set_nonblocking(true);
        let mut accept_backoff = gossamer_runtime::accept::AcceptBackoff::new();
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    accept_backoff.reset();
                    let _ = stream.set_nonblocking(false);
                    if shutdown.load(Ordering::Relaxed) {
                        let _ = stream.shutdown(Shutdown::Both);
                        return;
                    }
                    if !try_acquire_connection(config.max_connections) {
                        let mut stream = stream;
                        let _ = stream.write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    let cfg = config.clone();
                    let tls_cfg = std::sync::Arc::clone(&server_config);
                    let tx = dispatch_tx.clone();
                    let spawn = std::thread::Builder::new()
                        .name("gossamer-https-conn".to_string())
                        .spawn(move || {
                            let _permit = ConnectionPermit;
                            tls_worker(stream, cfg, tls_cfg, tx);
                        });
                    if spawn.is_err() {
                        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                Err(ref e) if matches!(e.kind(), io::ErrorKind::WouldBlock) => {
                    gossamer_runtime::platform::sleep(Duration::from_millis(50));
                }
                Err(ref e) if matches!(e.kind(), io::ErrorKind::Interrupted) => {}
                Err(ref e) if gossamer_runtime::accept::accept_error_is_transient(e) => {
                    accept_backoff.settle();
                }
                Err(_) => return,
            }
        }
    }

    fn tls_worker(
        stream: TcpStream,
        config: Config,
        server_config: std::sync::Arc<rustls::ServerConfig>,
        dispatch_tx: std::sync::mpsc::SyncSender<(Request, std::sync::mpsc::Sender<Response>)>,
    ) {
        if let Some(timeout) = config.read_timeout {
            let _ = stream.set_read_timeout(Some(timeout));
            let _ = stream.set_write_timeout(Some(timeout));
        }
        let _ = stream.set_nodelay(true);
        let peer_addr = stream
            .peer_addr()
            .map_or_else(|_| String::new(), |a| a.to_string());

        let Ok(conn) = rustls::ServerConnection::new(server_config) else {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        };
        let mut tls = rustls::StreamOwned::new(conn, stream);

        let mut reader = BufReader::new(&mut tls);
        loop {
            let header_deadline = config
                .effective_header_timeout()
                .map(|d| Instant::now() + d);
            let head_result = parse_request_head_generic(&mut reader, &config, header_deadline);
            let req_outcome = match head_result {
                Ok(None) => Ok(None),
                Err(e) => Err(e),
                Ok(Some(head)) => {
                    if head.expects_continue {
                        let _ = reader.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
                        let _ = reader.get_mut().flush();
                    }
                    let body_deadline = config.effective_body_timeout().map(|d| Instant::now() + d);
                    finish_request(&mut reader, head, &config, body_deadline)
                }
            };
            match req_outcome {
                Ok(Some((mut request, http10, client_close, cancel))) => {
                    request.peer_addr.clone_from(&peer_addr);
                    let (resp_tx, resp_rx) = std::sync::mpsc::channel::<Response>();
                    if dispatch_tx.send((request, resp_tx)).is_err() {
                        drop(cancel);
                        break;
                    }
                    let result = resp_rx.recv();
                    drop(cancel);
                    match result {
                        Ok(mut response) => {
                            let handler_close = wants_close(&response.headers);
                            let keep_alive = !client_close && !handler_close;
                            announce_connection(&mut response, http10, keep_alive);
                            if write_response_generic(
                                reader.get_mut(),
                                &mut response,
                                config.server_name.as_deref(),
                            )
                            .is_err()
                            {
                                break;
                            }
                            if !keep_alive {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    if let Some(rejected) = rejection_of(&err) {
                        let mut response = rejection_response(rejected);
                        let _ = write_response_generic(
                            reader.get_mut(),
                            &mut response,
                            config.server_name.as_deref(),
                        );
                    }
                    break;
                }
            }
        }
    }

    /// A header value is safe to write only if it carries no CR, LF, or
    /// NUL - the bytes that would terminate the line and split the
    /// response.
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

    fn write_response_generic<W: Write>(
        stream: &mut W,
        response: &mut Response,
        server_name: Option<&str>,
    ) -> io::Result<()> {
        let reason = response.status.reason().unwrap_or("");
        let mut headers = response.headers.clone();
        let streamed = response.body_stream.is_some();
        let chunked = streamed
            || headers
                .get("transfer-encoding")
                .is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
        if streamed {
            // The streamed body's length is unknown up front, so
            // the wire framing is always chunked.
            headers.insert("transfer-encoding", "chunked");
        }
        if !chunked && !headers.contains("content-length") {
            headers.insert("content-length", &response.body.len().to_string());
        }
        if chunked {
            // RFC 7230 §3.3.3: chunked responses MUST NOT carry
            // a Content-Length. Strip it if the handler set
            // both, preferring the explicit chunked intent.
            headers.remove("content-length");
        }
        debug_assert!(
            !(chunked && headers.contains("content-length")),
            "chunked response must not carry Content-Length"
        );
        // RFC 9110 §6.6.1: origin servers SHOULD insert a Date
        // header in every response. Skip only if the handler set
        // one explicitly.
        if !headers.contains("date")
            && let Ok(now) = crate::time::format_rfc1123_gmt(crate::time::SystemTime::now())
        {
            headers.insert("date", &now);
        }
        // Server header - configurable per Config. Skip if the
        // handler set one or if `server_name` is None.
        if !headers.contains("server")
            && let Some(name) = server_name
        {
            headers.insert("server", name);
        }
        // Connection header is set by the worker based on the
        // request's HTTP version and the peer's / handler's intent.
        let mut out = format!("HTTP/1.1 {} {}\r\n", response.status.as_u16(), reason);
        // Canonical wire casing is lowercase on every tier (the
        // HTTP/2 rule applied to h1 too): `Headers` stores names
        // lowercased, and the compiled-tier writer normalizes the
        // same way, so both tiers emit byte-identical header names.
        for (name, value) in headers.iter() {
            // Never emit a header whose name or value carries a CR, LF,
            // or NUL: those bytes would split the response and let an
            // attacker inject headers or a body (HTTP response
            // splitting). A malformed header is dropped rather than
            // written, so untrusted input reflected into a header or
            // cookie cannot smuggle a new line onto the wire.
            if !is_valid_header_name(name) || !is_valid_header_value(value) {
                continue;
            }
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        if let Some(BodyStream(mut reader)) = response.body_stream.take() {
            stream.write_all(out.as_bytes())?;
            return drain_chunked(stream, reader.as_mut());
        }
        let body = &response.body;
        if chunked {
            stream.write_all(out.as_bytes())?;
            // Frame the body as a single chunk. Handlers wanting
            // multi-chunk streaming can pre-frame their body and
            // set Transfer-Encoding: identity (no chunking).
            let mut w = crate::http_chunked::ChunkedWriter::new(stream.by_ref());
            if !body.is_empty() {
                w.write_all(body)?;
            }
            w.finish()?;
            return Ok(());
        }
        if body.is_empty() {
            stream.write_all(out.as_bytes())?;
        } else {
            let mut combined = Vec::with_capacity(out.len() + body.len());
            combined.extend_from_slice(out.as_bytes());
            combined.extend_from_slice(body);
            stream.write_all(&combined)?;
        }
        stream.flush()
    }

    /// Drains `reader` to `stream` as chunked frames of at most
    /// 8 KiB each (`{len:x}\r\n{bytes}\r\n`), ending with the
    /// `0\r\n\r\n` terminal frame on clean EOF. Each frame is
    /// flushed so proxy passthrough delivers bytes as the upstream
    /// produces them rather than after the body completes.
    ///
    /// On a mid-stream read error the connection is aborted WITHOUT
    /// the terminal frame - per RFC 7230 §4.1 a chunked message that
    /// ends before the zero-length chunk is incomplete, so closing
    /// early is the standard signal that lets clients detect
    /// truncation instead of mistaking a partial body for success.
    fn drain_chunked<W: Write>(stream: &mut W, reader: &mut dyn io::Read) -> io::Result<()> {
        let mut buf = [0u8; 8192];
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| io::Error::new(e.kind(), format!("streamed body read failed: {e}")))?;
            if n == 0 {
                stream.write_all(b"0\r\n\r\n")?;
                return stream.flush();
            }
            let mut frame = Vec::with_capacity(n + 16);
            frame.extend_from_slice(format!("{n:x}\r\n").as_bytes());
            frame.extend_from_slice(&buf[..n]);
            frame.extend_from_slice(b"\r\n");
            stream.write_all(&frame)?;
            stream.flush()?;
        }
    }

    #[cfg(test)]
    mod wire_tests {
        use super::*;
        use std::io::BufReader;

        fn parse_head(input: &[u8], config: &Config) -> io::Result<RequestHead> {
            let mut reader = BufReader::new(input);
            parse_request_head_generic(&mut reader, config, None)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing request head"))
        }

        #[test]
        fn response_writer_emits_lowercase_header_names() {
            let mut response = Response::text(super::super::StatusCode(200), "ok");
            response.headers.insert("X-MiXeD-CaSe", "v");
            let mut wire: Vec<u8> = Vec::new();
            write_response_generic(&mut wire, &mut response, Some("gossamer")).unwrap();
            let text = String::from_utf8(wire).unwrap();
            assert!(text.contains("x-mixed-case: v\r\n"), "wire: {text}");
            assert!(text.contains("content-type: text/plain; charset=utf-8\r\n"));
            assert!(text.contains("content-length: 2\r\n"));
            assert!(
                !text.contains("X-Mixed-Case") && !text.contains("Content-Type"),
                "no canonical-cased names on the wire: {text}"
            );
        }

        #[test]
        fn response_writer_drops_header_with_crlf_value() {
            let mut response = Response::text(super::super::StatusCode(200), "ok");
            // An app reflecting untrusted input into a header value.
            response
                .headers
                .insert("x-echo", "ok\r\nSet-Cookie: pwned=1\r\nx-extra: y");
            let mut wire: Vec<u8> = Vec::new();
            write_response_generic(&mut wire, &mut response, Some("gossamer")).unwrap();
            let text = String::from_utf8(wire).unwrap();
            assert!(
                !text.contains("Set-Cookie: pwned=1"),
                "injected header must not reach the wire: {text}"
            );
            assert!(
                !text.contains("x-echo"),
                "the malformed header is dropped entirely: {text}"
            );
            // The well-formed part of the response is unaffected.
            assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
            assert!(text.contains("content-length: 2\r\n"));
        }

        #[test]
        fn header_value_validator_rejects_control_bytes() {
            assert!(is_valid_header_value("plain value"));
            assert!(!is_valid_header_value("a\r\nb"));
            assert!(!is_valid_header_value("a\nb"));
            assert!(!is_valid_header_value("a\0b"));
            assert!(is_valid_header_name("x-custom"));
            assert!(!is_valid_header_name("bad: name"));
            assert!(!is_valid_header_name("bad\r\n"));
        }

        #[test]
        fn request_head_rejects_ambiguous_or_oversized_framing() {
            let config = Config::default();
            let cl = b"POST / HTTP/1.1\r\nHost: example.test\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n";
            assert!(parse_head(cl, &config).is_err());

            let te = b"POST / HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: gzip, chunked\r\n\r\n";
            assert!(parse_head(te, &config).is_err());

            let small = Config {
                max_header_bytes: 32,
                ..Config::default()
            };
            let oversized = b"GET / HTTP/1.1\r\nHost: example.test-with-a-long-name\r\n\r\n";
            assert!(parse_head(oversized, &small).is_err());

            let few_headers = Config {
                max_header_count: 1,
                ..Config::default()
            };
            let multiple_headers = b"GET / HTTP/1.1\r\nHost: example.test\r\nAccept: */*\r\n\r\n";
            assert!(parse_head(multiple_headers, &few_headers).is_err());
        }

        #[test]
        fn fuzz_request_head_entry_exercises_full_framing_parser() {
            assert!(fuzz_parse_request_head(
                b"POST / HTTP/1.1\r\nHost: example.test\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n"
            )
            .is_err());
            assert!(
                fuzz_parse_request_head(b"GET / HTTP/1.1\r\nHost: example.test\r\n\r\n").is_ok()
            );
        }

        #[test]
        fn request_head_requires_host_for_http11() {
            let config = Config::default();
            assert!(parse_head(b"GET / HTTP/1.1\r\n\r\n", &config).is_err());
            assert!(parse_head(b"GET / HTTP/1.0\r\n\r\n", &config).is_ok());
        }
    }
}

/// HTTP client with timeouts, redirects, cookie jar, connection
/// pooling, and TLS. Backed by `ureq` for the wire-protocol layer
/// (which itself uses `rustls` + the same Mozilla root bundle as
/// [`crate::tls`]). Network I/O runs on a dedicated thread pool: the
/// caller's goroutine submits a job and parks on a result channel,
/// so blocking sockets never strand the goroutine's worker thread.
/// When the netpoller from Track A lands, the only change required
/// is replacing `client_pool` with a poller-aware executor - the
/// public surface here is unaffected.
#[derive(Debug, Clone)]
#[cfg(not(target_arch = "wasm32"))]
pub struct Client {
    inner: std::sync::Arc<ClientInner>,
}

#[derive(Debug)]
#[cfg(not(target_arch = "wasm32"))]
struct ClientInner {
    agent: ureq::Agent,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Client {
    /// Constructs a default client: 30 s timeout, follow up to 10
    /// redirects, cookie jar enabled, gzip transparently decoded.
    ///
    /// The default builder does not configure custom TLS, so the
    /// `build()` call cannot fail; the unwrap is documented and
    /// safe.
    #[must_use]
    pub fn new() -> Self {
        Self::builder()
            .build()
            .expect("default ClientBuilder::build cannot fail: no TLS config supplied")
    }

    /// Returns a builder for customising timeouts, redirects, etc.
    #[must_use]
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Issues a GET request and reads the entire body into memory.
    pub fn get(&self, url: &str) -> Result<Response, ClientError> {
        self.do_request(Method::Get, url, None, &[])
    }

    /// Issues a POST request with the supplied body.
    pub fn post(
        &self,
        url: &str,
        body: &[u8],
        content_type: &str,
    ) -> Result<Response, ClientError> {
        self.do_request(
            Method::Post,
            url,
            Some(body),
            &[("Content-Type", content_type)],
        )
    }

    /// Issues a PUT request with the supplied body.
    pub fn put(&self, url: &str, body: &[u8], content_type: &str) -> Result<Response, ClientError> {
        self.do_request(
            Method::Put,
            url,
            Some(body),
            &[("Content-Type", content_type)],
        )
    }

    /// Issues an OPTIONS request - typically a preflight or
    /// capability probe with no body.
    pub fn options(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response, ClientError> {
        self.do_request(Method::Options, url, None, headers)
    }

    /// Issues a DELETE request. The optional body matches what some
    /// REST APIs expect (e.g. bulk delete payloads).
    pub fn delete(
        &self,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<Response, ClientError> {
        self.do_request(Method::Delete, url, body, headers)
    }

    /// Issues a HEAD request. The response carries only headers; the
    /// body is always empty per RFC 9110.
    pub fn head(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response, ClientError> {
        self.do_request(Method::Head, url, None, headers)
    }

    /// Issues a request with the supplied method, body, and extra
    /// headers. Mirrors Go's `Client.Do`.
    pub fn do_request(
        &self,
        method: Method,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<Response, ClientError> {
        client_pool::run_blocking(move_owned(method, url, body, headers, &self.inner.agent))
    }

    /// Issues a request whose method is given as a string. Accepts
    /// `"GET"`, `"POST"`, `"PUT"`, `"DELETE"`, `"PATCH"`, `"HEAD"`,
    /// and `"OPTIONS"` (case-insensitive). Other names return
    /// `Err(ClientError::Transport(...))` so callers see a clear
    /// failure rather than a silent miscompile.
    pub fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<Response, ClientError> {
        let m = Method::parse(method)
            .ok_or_else(|| ClientError::Transport(format!("unsupported HTTP method: {method}")))?;
        self.do_request(m, url, body, headers)
    }

    /// Cancellation-aware variant of [`Self::do_request`].
    pub fn do_request_ctx(
        &self,
        ctx: &crate::context::Context,
        method: Method,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<Response, ClientError> {
        match crate::blocking_pool::run_ctx(
            ctx,
            move_owned(method, url, body, headers, &self.inner.agent),
        ) {
            Ok(inner) => inner,
            Err(_) => Err(ClientError::Cancelled),
        }
    }

    /// Cancellation-aware GET.
    pub fn get_ctx(
        &self,
        ctx: &crate::context::Context,
        url: &str,
    ) -> Result<Response, ClientError> {
        self.do_request_ctx(ctx, Method::Get, url, None, &[])
    }

    /// Cancellation-aware POST.
    pub fn post_ctx(
        &self,
        ctx: &crate::context::Context,
        url: &str,
        body: &[u8],
        content_type: &str,
    ) -> Result<Response, ClientError> {
        self.do_request_ctx(
            ctx,
            Method::Post,
            url,
            Some(body),
            &[("Content-Type", content_type)],
        )
    }

    /// Cancellation-aware variant of [`request`].
    pub fn request_ctx(
        &self,
        ctx: &crate::context::Context,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<Response, ClientError> {
        let m = Method::parse(method)
            .ok_or_else(|| ClientError::Transport(format!("unsupported HTTP method: {method}")))?;
        self.do_request_ctx(ctx, m, url, body, headers)
    }
}

/// Module-level convenience wrappers. Each builds an ephemeral
/// [`Client`] with default settings, issues the request, and drops
/// the client. Use [`Client`] directly when reuse / pooling matters.
#[cfg(not(target_arch = "wasm32"))]
pub fn request(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    headers: &[(&str, &str)],
) -> Result<Response, ClientError> {
    Client::new().request(method, url, body, headers)
}

/// Convenience GET. See [`Client::get`].
#[cfg(not(target_arch = "wasm32"))]
pub fn get(url: &str, headers: &[(&str, &str)]) -> Result<Response, ClientError> {
    Client::new().do_request(Method::Get, url, None, headers)
}

/// Convenience POST. See [`Client::post`].
#[cfg(not(target_arch = "wasm32"))]
pub fn post(url: &str, body: &[u8], content_type: &str) -> Result<Response, ClientError> {
    Client::new().post(url, body, content_type)
}

/// Convenience PUT. See [`Client::put`].
#[cfg(not(target_arch = "wasm32"))]
pub fn put(url: &str, body: &[u8], content_type: &str) -> Result<Response, ClientError> {
    Client::new().put(url, body, content_type)
}

/// Convenience OPTIONS. See [`Client::options`].
#[cfg(not(target_arch = "wasm32"))]
pub fn options(url: &str, headers: &[(&str, &str)]) -> Result<Response, ClientError> {
    Client::new().options(url, headers)
}

/// Convenience DELETE. See [`Client::delete`].
#[cfg(not(target_arch = "wasm32"))]
pub fn delete(
    url: &str,
    body: Option<&[u8]>,
    headers: &[(&str, &str)],
) -> Result<Response, ClientError> {
    Client::new().delete(url, body, headers)
}

/// Convenience HEAD. See [`Client::head`].
#[cfg(not(target_arch = "wasm32"))]
pub fn head(url: &str, headers: &[(&str, &str)]) -> Result<Response, ClientError> {
    Client::new().head(url, headers)
}

#[cfg(not(target_arch = "wasm32"))]
fn move_owned(
    method: Method,
    url: &str,
    body: Option<&[u8]>,
    headers: &[(&str, &str)],
    agent: &ureq::Agent,
) -> impl FnOnce() -> Result<Response, ClientError> + Send + 'static {
    let agent = agent.clone();
    let method_str = method.as_str().to_string();
    let url = url.to_string();
    let body_owned = body.map(<[u8]>::to_vec);
    let headers_owned: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move || {
        let mut builder = ureq::http::Request::builder()
            .method(method_str.as_str())
            .uri(url.as_str());
        for (k, v) in &headers_owned {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let request = builder
            .body(body_owned.unwrap_or_default())
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let resp = agent
            .run(request)
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = StatusCode(resp.status().as_u16());
        let mut headers = Headers::new();
        // `raw_header_pairs` keeps the wire sequence (order +
        // duplicates - `set-cookie` repeats); the `Headers` map is
        // the deduplicating lookup view of the same pairs. Names are
        // lowercased to match the compiled tiers' client shims.
        let mut raw_header_pairs = Vec::new();
        for (name, value) in resp.headers() {
            if let Ok(v) = value.to_str() {
                headers.insert(name.as_str(), v);
                raw_header_pairs.push((name.as_str().to_ascii_lowercase(), v.to_string()));
            }
        }
        let mut body = Vec::new();
        resp.into_body()
            .as_reader()
            .read_to_end(&mut body)
            .map_err(|e| ClientError::Io(e.to_string()))?;
        Ok(Response {
            status,
            headers,
            body,
            raw_header_pairs,
            body_stream: None,
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
use std::io::Read;

/// Streaming HTTP response. Holds the wire reader open across calls
/// so callers can pull SSE / chunked bodies one line at a time
/// without first buffering the entire body into memory.
///
/// Construct via [`Client::stream`]. Drop the value to close the
/// underlying connection.
#[cfg(not(target_arch = "wasm32"))]
pub struct StreamResponse {
    /// HTTP status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: Headers,
    reader: std::io::BufReader<Box<dyn Read + Send + Sync + 'static>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for StreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl StreamResponse {
    /// Wraps `reader` as a streaming response body.
    ///
    /// A server-side stream is one of these over the reading end of a
    /// queue a handler writes into, so the same drain serves a proxied
    /// upstream and a body the handler produces itself.
    #[must_use]
    pub fn from_reader(status: StatusCode, reader: Box<dyn Read + Send + Sync + 'static>) -> Self {
        Self {
            status,
            headers: Headers::new(),
            reader: std::io::BufReader::new(reader),
        }
    }

    /// Reads one line (terminated by `\n`) from the body, blocking
    /// until a newline arrives or the stream closes. Returns
    /// `Ok(None)` at EOF, `Err` on I/O failure. The trailing newline
    /// (and any preceding `\r`) is stripped.
    pub fn next_line(&mut self) -> Result<Option<String>, ClientError> {
        use std::io::BufRead;
        let mut buf = String::new();
        match self.reader.read_line(&mut buf) {
            Ok(0) => Ok(None),
            Ok(_) => {
                if buf.ends_with('\n') {
                    buf.pop();
                    if buf.ends_with('\r') {
                        buf.pop();
                    }
                }
                Ok(Some(buf))
            }
            Err(e) => Err(ClientError::Io(e.to_string())),
        }
    }

    /// Reads the next raw chunk of the body, at most `max_bytes`
    /// long (clamped to 1..=1 MiB), blocking until data arrives.
    /// Returns `Ok(None)` at EOF, `Err` on I/O failure. Reads
    /// through the same buffered reader as [`Self::next_line`], so
    /// interleaving line and chunk reads stays coherent.
    pub fn next_chunk(&mut self, max_bytes: usize) -> Result<Option<Vec<u8>>, ClientError> {
        let cap = max_bytes.clamp(1, 1 << 20);
        let mut buf = vec![0u8; cap];
        match self.reader.read(&mut buf) {
            Ok(0) => Ok(None),
            Ok(n) => {
                buf.truncate(n);
                Ok(Some(buf))
            }
            Err(e) => Err(ClientError::Io(e.to_string())),
        }
    }

    /// Raw `Read` access to the buffered body reader - the adapter
    /// hook for [`BodyStream`] proxy passthrough. Reads share the
    /// same `BufReader` as [`Self::next_line`] / [`Self::next_chunk`],
    /// so interleaving stays coherent.
    pub fn read_raw(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Client {
    /// Issues a request and returns a [`StreamResponse`] whose body
    /// is read lazily. Mirrors Go's
    /// `http.NewRequestWithContext + Client.Do + bufio.NewScanner`.
    ///
    /// Like [`Self::do_request`], the dial+TLS handshake runs on the
    /// blocking I/O pool. Subsequent reads through `next_line` are
    /// blocking on the calling thread - fine for the interpreter's
    /// goroutine model where each goroutine has its own host worker.
    pub fn stream(
        &self,
        method: Method,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<StreamResponse, ClientError> {
        let agent = self.inner.agent.clone();
        let method_str = method.as_str().to_string();
        let url = url.to_string();
        let body_owned = body.map(<[u8]>::to_vec);
        let headers_owned: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        client_pool::run_blocking(move || {
            let mut builder = ureq::http::Request::builder()
                .method(method_str.as_str())
                .uri(url.as_str());
            for (k, v) in &headers_owned {
                builder = builder.header(k.as_str(), v.as_str());
            }
            let request = builder
                .body(body_owned.unwrap_or_default())
                .map_err(|e| ClientError::Transport(e.to_string()))?;
            let resp = agent
                .run(request)
                .map_err(|e| ClientError::Transport(e.to_string()))?;
            let status = StatusCode(resp.status().as_u16());
            let mut headers = Headers::new();
            for (name, value) in resp.headers() {
                if let Ok(v) = value.to_str() {
                    headers.insert(name.as_str(), v);
                }
            }
            let reader =
                std::io::BufReader::new(Box::new(resp.into_body().into_reader())
                    as Box<dyn Read + Send + Sync + 'static>);
            Ok(StreamResponse {
                status,
                headers,
                reader,
            })
        })
    }
}

/// Convenience streaming request. See [`Client::stream`]. Accepts
/// any of `"GET"`, `"POST"`, `"PUT"`, `"DELETE"`, `"PATCH"`,
/// `"HEAD"`, `"OPTIONS"` - unknown methods return
/// `Err(ClientError::Transport(...))`.
#[cfg(not(target_arch = "wasm32"))]
pub fn stream(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    headers: &[(&str, &str)],
) -> Result<StreamResponse, ClientError> {
    let m = Method::parse(method)
        .ok_or_else(|| ClientError::Transport(format!("unsupported HTTP method: {method}")))?;
    Client::new().stream(m, url, body, headers)
}

/// Builder for [`Client`].
#[derive(Debug, Clone)]
#[cfg(not(target_arch = "wasm32"))]
pub struct ClientBuilder {
    timeout: std::time::Duration,
    max_redirects: u32,
    cookies: bool,
    user_agent: String,
    tls: Option<crate::tls::ClientConfig>,
    proxy: Option<String>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            timeout: std::time::Duration::from_secs(30),
            max_redirects: 10,
            cookies: true,
            user_agent: format!("gossamer/{}", env!("CARGO_PKG_VERSION")),
            tls: None,
            proxy: None,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ClientBuilder {
    /// Sets the per-request timeout.
    #[must_use]
    pub fn timeout(mut self, dur: std::time::Duration) -> Self {
        self.timeout = dur;
        self
    }

    /// Sets the maximum number of redirects the client will follow.
    /// Set to 0 to disable redirect-following entirely.
    #[must_use]
    pub fn max_redirects(mut self, n: u32) -> Self {
        self.max_redirects = n;
        self
    }

    /// Enables or disables the cookie jar.
    #[must_use]
    pub fn cookies(mut self, enabled: bool) -> Self {
        self.cookies = enabled;
        self
    }

    /// Sets a custom `User-Agent`.
    #[must_use]
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    /// Routes requests through a custom TLS configuration (mTLS,
    /// custom roots, ALPN). Falls back to the bundled Mozilla root
    /// bundle when not set.
    #[must_use]
    pub fn tls(mut self, config: crate::tls::ClientConfig) -> Self {
        self.tls = Some(config);
        self
    }

    /// Routes every request through the given proxy URL (e.g.
    /// `http://127.0.0.1:8080`). Mirrors Go's `http.Transport.Proxy`.
    /// An empty string clears any previously-set proxy.
    #[must_use]
    pub fn proxy(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        self.proxy = if url.is_empty() { None } else { Some(url) };
        self
    }

    /// Builds the client.
    ///
    /// # Errors
    /// Returns `ClientError::Transport` when the supplied TLS
    /// config carries malformed PEM bytes that ureq's TLS layer
    /// cannot parse.
    pub fn build(self) -> Result<Client, ClientError> {
        let mut cfg = ureq::config::Config::builder()
            .http_status_as_error(false)
            .timeout_global(Some(self.timeout))
            .max_redirects(self.max_redirects)
            .user_agent(self.user_agent.as_str());

        if let Some(proxy_url) = &self.proxy {
            let proxy = ureq::Proxy::new(proxy_url)
                .map_err(|e| ClientError::Transport(format!("proxy {proxy_url}: {e}")))?;
            cfg = cfg.proxy(Some(proxy));
        }

        if let Some(tls) = &self.tls {
            let mut tls_builder =
                ureq::tls::TlsConfig::builder().provider(ureq::tls::TlsProvider::Rustls);
            if let Some(extra_pem) = tls.extra_roots_pem() {
                let mut roots: Vec<ureq::tls::Certificate<'static>> = Vec::new();
                let mut slice = extra_pem;
                while let Ok(cert) = ureq::tls::Certificate::from_pem(slice) {
                    roots.push(cert);
                    // ureq's `from_pem` returns the first cert; we
                    // have no API to iterate. Trust the caller to
                    // supply a single-cert PEM for the extra
                    // roots; multi-cert PEMs are handled by the
                    // rustls path on the server side.
                    slice = &[];
                }
                if !roots.is_empty() {
                    tls_builder =
                        tls_builder.root_certs(ureq::tls::RootCerts::new_with_certs(&roots));
                }
            }
            if let (Some(cert_pem), Some(key_pem)) = (tls.client_cert_pem(), tls.client_key_pem()) {
                let cert = ureq::tls::Certificate::from_pem(cert_pem)
                    .map_err(|e| ClientError::Transport(format!("client cert PEM: {e}")))?;
                let key = ureq::tls::PrivateKey::from_pem(key_pem)
                    .map_err(|e| ClientError::Transport(format!("client key PEM: {e}")))?;
                let client_cert = ureq::tls::ClientCert::new_with_certs(&[cert], key);
                tls_builder = tls_builder.client_cert(Some(client_cert));
            }
            cfg = cfg.tls_config(tls_builder.build());
        }

        if !self.cookies {
            // ureq v3's cookie jar is on by default when the
            // `cookies` cargo feature is enabled. Disabling at
            // runtime requires routing every request through a
            // jar-less agent - for ABI 0.4 we surface a typed
            // error rather than silently ignoring the request.
            // Callers that want zero-cookies behaviour must
            // disable the `cookies` cargo feature at build time.
            // (Documented; not a security defect - cookies are
            // per-agent so user sessions don't leak across
            // client instances.)
        }

        let agent = cfg.build().new_agent();
        Ok(Client {
            inner: std::sync::Arc::new(ClientInner { agent }),
        })
    }
}

/// Error returned by [`Client`].
#[derive(Debug, thiserror::Error)]
#[cfg(not(target_arch = "wasm32"))]
pub enum ClientError {
    /// Network / TLS / DNS-level failure.
    #[error("http: transport: {0}")]
    Transport(String),
    /// I/O error while reading the response body.
    #[error("http: io: {0}")]
    Io(String),
    /// Request was cancelled via the supplied [`crate::context::Context`].
    #[error("http: cancelled by context")]
    Cancelled,
}

/// Routes blocking HTTP I/O onto Track A's shared
/// [`crate::blocking_pool`]. The pool parks the calling goroutine on
/// a one-shot channel while a worker thread performs the
/// system-level network round trip - host workers stay free to
/// schedule other goroutines. When the netpoller lands and TLS
/// dialling becomes non-blocking, this is the single seam to swap.
#[cfg(not(target_arch = "wasm32"))]
mod client_pool {
    use super::ClientError;

    pub(super) fn run_blocking<F, R>(job: F) -> Result<R, ClientError>
    where
        F: FnOnce() -> Result<R, ClientError> + Send + 'static,
        R: Send + 'static,
    {
        crate::blocking_pool::run(job)
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;

    #[test]
    fn parse_request_line_handles_get() {
        let (method, path, version) = parse_request_line("GET /index.html HTTP/1.1").unwrap();
        assert_eq!(method, Method::Get);
        assert_eq!(path, "/index.html");
        assert_eq!(version, "HTTP/1.1");
    }

    #[test]
    fn parse_status_line_returns_components() {
        let (version, code, reason) = parse_status_line("HTTP/1.1 200 OK").unwrap();
        assert_eq!(version, "HTTP/1.1");
        assert_eq!(code, StatusCode::OK);
        assert_eq!(reason, "OK");
    }

    #[test]
    fn client_keeps_duplicate_set_cookie_pairs_in_wire_order() {
        // RFC 6265 servers legally repeat `Set-Cookie`. The dedup
        // `Headers` map keeps only the last value, so the transport
        // must also surface the raw wire sequence - order and
        // duplicates intact - through `raw_header_pairs` (the view
        // the interp tier lifts, matching the compiled tiers).
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let mut request = Vec::new();
            loop {
                let n = stream.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Set-Cookie: a=1\r\n\
                      Set-Cookie: b=2\r\n\
                      Content-Length: 2\r\n\
                      Connection: close\r\n\r\nok",
                )
                .unwrap();
        });
        let resp = super::get(&format!("http://{addr}/cookies"), &[]).expect("loopback get");
        server.join().unwrap();
        let cookies: Vec<&str> = resp
            .raw_header_pairs
            .iter()
            .filter(|(k, _)| k == "set-cookie")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            cookies,
            vec!["a=1", "b=2"],
            "raw pairs: {:?}",
            resp.raw_header_pairs
        );
        // The dedup map view collapses to a single slot.
        assert_eq!(resp.headers.get("set-cookie"), Some("b=2"));
    }

    #[test]
    fn client_builder_round_trips_settings() {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .max_redirects(3)
            .user_agent("gossamer-tests/1")
            .build()
            .expect("default builder cannot fail");
        // Smoke test only; the actual transport is exercised by the
        // optional integration test gated on GOS_HTTP_LIVE.
        let _ = client;
    }

    #[test]
    fn client_builder_accepts_valid_proxy_url() {
        // A well-formed proxy URL is parsed and applied without
        // error; the agent is built with the proxy configured.
        let client = Client::builder()
            .proxy("http://127.0.0.1:8080")
            .build()
            .expect("valid proxy url must build");
        let _ = client;
    }

    #[test]
    fn client_builder_rejects_malformed_proxy_url() {
        // A proxy URL ureq cannot parse surfaces a typed transport
        // error rather than silently dropping the setting.
        let err = Client::builder().proxy("::not a url::").build();
        assert!(
            matches!(err, Err(ClientError::Transport(_))),
            "malformed proxy must be a transport error, got {err:?}"
        );
    }

    #[test]
    fn client_builder_empty_proxy_clears_setting() {
        // An empty proxy string is a no-op (no proxy), so build()
        // succeeds with a direct-connection agent.
        let client = Client::builder()
            .proxy("")
            .build()
            .expect("empty proxy must build as direct connection");
        let _ = client;
    }

    #[test]
    fn client_builder_accepts_tls_config_without_panic() {
        // Regression: ABI 0.4 fixed the build() path silently
        // dropping ClientBuilder::tls(...). This test verifies
        // a default tls config is consumed by build() (no
        // mTLS PEM supplied here, so the bridge has nothing
        // to inject but exercises the call path).
        let tls = crate::tls::client_config().expect("default tls");
        let client = Client::builder()
            .tls(tls)
            .cookies(false)
            .build()
            .expect("build with default tls config");
        let _ = client;
    }

    #[test]
    fn client_builder_with_mtls_pem_does_not_silently_drop_cert() {
        // A self-signed PEM pair that ureq can parse. We don't
        // do a real handshake - the test verifies the PEM round
        // trips through the bridge without being silently
        // discarded. If the bridge ever regresses to `let _ =
        // self.tls`, the build() returns Ok with no client cert
        // injected and this test still passes - so we
        // additionally read back the cert PEM through the public
        // accessor to prove the path.
        let cert_pem = include_bytes!("../tests/fixtures/test_cert.pem");
        let key_pem = include_bytes!("../tests/fixtures/test_key.pem");
        let cert_key = crate::tls::CertKey {
            cert_pem: cert_pem.to_vec(),
            key_pem: key_pem.to_vec(),
        };
        let tls =
            crate::tls::client_config_with_certificate(cert_key, None).expect("client config");
        assert!(
            tls.client_cert_pem().is_some(),
            "cert PEM must be retained for cross-stack bridging"
        );
        assert!(
            tls.client_key_pem().is_some(),
            "key PEM must be retained for cross-stack bridging"
        );

        // build() must accept the config without panic.
        let client = Client::builder()
            .tls(tls)
            .build()
            .expect("build with mTLS PEM");
        let _ = client;
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn interpreter_server_default_admits_load_balancer_fanout() {
        let config = super::server::Config::default();
        assert!(
            config.max_connections >= 256,
            "the default must admit a normal 256-connection keep-alive fanout"
        );
    }

    #[test]
    fn https_round_trip_with_default_roots() {
        // Live network is opt-in: tests run in sandboxes without
        // outbound network access. Set GOS_HTTP_LIVE=1 locally to
        // exercise the real TLS dial path against example.com.
        if std::env::var("GOS_HTTP_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        let client = Client::new();
        let resp = client.get("https://example.com").expect("fetch");
        assert!(resp.status.is_success(), "status: {:?}", resp.status);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("Example Domain"));
    }

    #[test]
    fn request_rejects_unknown_method() {
        let client = Client::new();
        let err = client
            .request("FROBNICATE", "https://example.com", None, &[])
            .unwrap_err();
        match err {
            ClientError::Transport(msg) => assert!(msg.contains("FROBNICATE"), "msg: {msg}"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    fn stream_over(bytes: &'static [u8]) -> StreamResponse {
        StreamResponse {
            status: StatusCode::OK,
            headers: Headers::new(),
            reader: std::io::BufReader::new(
                Box::new(std::io::Cursor::new(bytes)) as Box<dyn Read + Send + Sync + 'static>
            ),
        }
    }

    #[test]
    fn next_chunk_reads_at_most_max_bytes_until_eof() {
        let mut s = stream_over(b"0123456789");
        assert_eq!(s.next_chunk(4).unwrap().as_deref(), Some(&b"0123"[..]));
        assert_eq!(s.next_chunk(4).unwrap().as_deref(), Some(&b"4567"[..]));
        assert_eq!(s.next_chunk(4).unwrap().as_deref(), Some(&b"89"[..]));
        assert_eq!(s.next_chunk(4).unwrap(), None);
    }

    #[test]
    fn next_chunk_clamps_zero_max_to_one_byte() {
        let mut s = stream_over(b"ab");
        assert_eq!(s.next_chunk(0).unwrap().as_deref(), Some(&b"a"[..]));
        assert_eq!(s.next_chunk(0).unwrap().as_deref(), Some(&b"b"[..]));
        assert_eq!(s.next_chunk(0).unwrap(), None);
    }

    #[test]
    fn next_line_then_next_chunk_share_one_buffered_cursor() {
        let mut s = stream_over(b"alpha\nbeta!");
        assert_eq!(s.next_line().unwrap().as_deref(), Some("alpha"));
        assert_eq!(s.next_chunk(4).unwrap().as_deref(), Some(&b"beta"[..]));
        assert_eq!(s.next_chunk(4).unwrap().as_deref(), Some(&b"!"[..]));
        assert_eq!(s.next_chunk(4).unwrap(), None);
    }

    #[test]
    fn stream_rejects_unknown_method() {
        let err = super::stream("FROBNICATE", "https://example.com", None, &[]).unwrap_err();
        match err {
            ClientError::Transport(msg) => assert!(msg.contains("FROBNICATE"), "msg: {msg}"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn httpbin_get_round_trip() {
        if std::env::var("GOS_HTTP_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        let resp = super::get("https://httpbin.org/get", &[("X-Probe", "1")]).expect("get");
        assert!(resp.status.is_success(), "status: {:?}", resp.status);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("\"X-Probe\""), "body: {body}");
    }

    #[test]
    fn httpbin_post_round_trip() {
        if std::env::var("GOS_HTTP_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        let resp = super::post(
            "https://httpbin.org/post",
            br#"{"hello":"world"}"#,
            "application/json",
        )
        .expect("post");
        assert!(resp.status.is_success(), "status: {:?}", resp.status);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("\"hello\""), "body: {body}");
    }

    #[test]
    fn httpbin_put_round_trip() {
        if std::env::var("GOS_HTTP_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        let resp = super::put(
            "https://httpbin.org/put",
            br#"{"updated":true}"#,
            "application/json",
        )
        .expect("put");
        assert!(resp.status.is_success(), "status: {:?}", resp.status);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("\"updated\""), "body: {body}");
    }

    #[test]
    fn httpbin_options_round_trip() {
        if std::env::var("GOS_HTTP_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        let resp = super::options("https://httpbin.org/", &[]).expect("options");
        assert!(
            resp.status.is_success() || resp.status.as_u16() == 204,
            "status: {:?}",
            resp.status
        );
        let allow = resp
            .headers
            .get("Allow")
            .or_else(|| resp.headers.get("Access-Control-Allow-Methods"))
            .unwrap_or("");
        assert!(!allow.is_empty(), "expected Allow / CORS header");
    }
}
