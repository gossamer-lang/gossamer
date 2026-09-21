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
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Static manifest of every registered stdlib module.
//! Each stdlib milestone extends this table with
//! the modules it adds. Entries are listed in phase-introduction order
//! so a `gos doc` walk renders modules in the same sequence as the
//! implementation plan.

#![forbid(unsafe_code)]
use crate::registry::{StdItem, StdItemKind, StdModule};

use super::*;

pub const TLS: StdModule = StdModule {
    path: "std::tls",
    summary: "Rustls-backed TLS support exposed through `http::serve_tls` and `net::TcpStream` TLS upgrades. The configuration constructors are host-runtime internals, not Gossamer callables.",
    items: &[],
};

pub const HTML_TEMPLATE: StdModule = StdModule {
    path: "std::html::template",
    summary: "Context-aware HTML templates with auto-escape (text/attr/URL/JS). The context classifier is heuristic - sound for typical server-rendered responses but NOT a content-security-policy substitute; sanitize untrusted HTML fragments with a dedicated sanitizer.",
    items: &[StdItem {
        name: "render_json",
        kind: StdItemKind::Function,
        doc: "render_json(source, json_data) -> Result<String, Error>: renders a context-aware HTML template against a JSON data context. Stateless and wired bit-identically across every tier.",
    }],
};

pub const HTTP: StdModule = StdModule {
    path: "std::http",
    summary: "HTTP/1.1 and HTTP/2 client and server. HTTP/2 negotiates via ALPN over TLS automatically (Go-style); h2c entry points are explicit. Write a handler as a cohort and an arena: `cohort { }` joins or cancels every goroutine the request spawned before the response is written, and its first child failure becomes the block's `Err` for the handler to turn into a status; `arena { }` bump-allocates what the request builds and frees it wholesale on every exit path, with escape checked at compile time. Dependency injection is closure capture - build the router from closures capturing the pool and the configuration.",
    items: &[
        StdItem {
            name: "Request",
            kind: StdItemKind::Type,
            doc: "HTTP request value passed to a handler. Fields: `method`, `path`, `query`, `query_pairs`, `headers`, `body`, `raw_body`, `peer_addr`.",
        },
        StdItem {
            name: "Response",
            kind: StdItemKind::Type,
            doc: "HTTP response value returned from a handler.",
        },
        StdItem {
            name: "Method",
            kind: StdItemKind::Type,
            doc: "HTTP method enumeration.",
        },
        StdItem {
            name: "StatusCode",
            kind: StdItemKind::Type,
            doc: "HTTP status code.",
        },
        StdItem {
            name: "Headers",
            kind: StdItemKind::Type,
            doc: "Case-insensitive header map.",
        },
        StdItem {
            name: "Server",
            kind: StdItemKind::Type,
            doc: "A configured server. `http::serve(addr, handler)` keeps every default; build a `Server` when a deployment needs its own budgets, its bound address read back, or a shutdown it drives. Setters chain and answer the server: `read_header_timeout_ms`, `read_body_timeout_ms`, `write_timeout_ms`, `idle_timeout_ms`, `request_timeout_ms`, `max_header_bytes`, `max_body_bytes`, `max_connections`, `server_name`. Then `listen(addr) -> Result<(), Error>` binds - so `addr() -> String` reads back a port-0 assignment before `serve(handler)` blocks - and `shutdown(deadline_ms) -> bool` stops accepting and waits for in-flight requests, answering whether the drain finished.",
        },
        StdItem {
            name: "serve",
            kind: StdItemKind::Function,
            doc: "Convenience: bind and serve an HTTP handler. `Result<(), Error>` - a bind failure is an Err value.",
        },
        StdItem {
            name: "serve_tls",
            kind: StdItemKind::Function,
            doc: "TLS-terminating server: `serve_tls(addr, cert_pem, key_pem, handler) -> Result<(), Error>`. Builds a rustls config from the PEM cert chain + key and serves HTTPS with the same handler contract as `serve`.",
        },
        StdItem {
            name: "Client",
            kind: StdItemKind::Type,
            doc: "HTTP client; configure redirects and timeout via `Client::builder()`.",
        },
        StdItem {
            name: "ResponseStream",
            kind: StdItemKind::Type,
            doc: "A streaming response body. `http::stream` produces one over an upstream response, read with `next_line` / `next_chunk`; `ResponseStream::new()` opens one the handler writes itself, with `write(text)` and `write_bytes(bytes)` answering the queued byte count (or -1 once closed), `is_open()` saying whether the client is still there, and `close()` ending the body. Either kind is consumed by `Response::stream(status, content_type, body)` - write from a goroutine and the handler answers immediately while the bytes are framed as they arrive.",
        },
        StdItem {
            name: "request",
            kind: StdItemKind::Function,
            doc: "One-shot request with a string body: `(method, url, body, headers) -> Result<Response, Error>`.",
        },
        StdItem {
            name: "request_bytes",
            kind: StdItemKind::Function,
            doc: "One-shot request with a byte body: `(method, url, body: [u8], headers) -> Result<Response, Error>`.",
        },
        StdItem {
            name: "stream",
            kind: StdItemKind::Function,
            doc: "One-shot request read incrementally: `(method, url, body, headers) -> Result<ResponseStream, Error>`.",
        },
        StdItem {
            name: "get",
            kind: StdItemKind::Function,
            doc: "One-shot GET: `(url, headers) -> Result<Response, Error>`.",
        },
        StdItem {
            name: "post",
            kind: StdItemKind::Function,
            doc: "One-shot POST: `(url, body, content_type) -> Result<Response, Error>`.",
        },
        StdItem {
            name: "put",
            kind: StdItemKind::Function,
            doc: "One-shot PUT: `(url, body, content_type) -> Result<Response, Error>`.",
        },
        StdItem {
            name: "delete",
            kind: StdItemKind::Function,
            doc: "One-shot DELETE: `(url, body, headers) -> Result<Response, Error>`.",
        },
        StdItem {
            name: "head",
            kind: StdItemKind::Function,
            doc: "One-shot HEAD: `(url, headers) -> Result<Response, Error>`.",
        },
        StdItem {
            name: "options",
            kind: StdItemKind::Function,
            doc: "One-shot OPTIONS: `(url, headers) -> Result<Response, Error>`.",
        },
        // HTTP/2 surface - folded in per the Go model.
        StdItem {
            name: "Http2Handler",
            kind: StdItemKind::Trait,
            doc: "Bounded-body HTTP/2 handler: serve(Request) -> Response.",
        },
        StdItem {
            name: "Http2StreamingHandler",
            kind: StdItemKind::Trait,
            doc: "Chunked-body HTTP/2 handler: serve(Request, StreamingResponseWriter) -> Result.",
        },
        StdItem {
            name: "StreamingResponseWriter",
            kind: StdItemKind::Type,
            doc: "Streaming HTTP/2 response writer; set_status / header / write_chunk / finish.",
        },
        StdItem {
            name: "Http2Config",
            kind: StdItemKind::Type,
            doc: "Per-connection HTTP/2 tuning (window sizes, max concurrent streams, frame caps).",
        },
        StdItem {
            name: "Http2ServerHandle",
            kind: StdItemKind::Type,
            doc: "Handle to a running HTTP/2 connection for shutdown / in-flight counts.",
        },
        StdItem {
            name: "Http2Error",
            kind: StdItemKind::Type,
            doc: "HTTP/2 server error: Io, Protocol, Handler.",
        },
        StdItem {
            name: "serve_h2c",
            kind: StdItemKind::Function,
            doc: "Bind a plain-TCP listener and serve h2c (HTTP/2 cleartext).",
        },
        StdItem {
            name: "Trailers",
            kind: StdItemKind::Type,
            doc: "HTTP/2 trailing HEADERS (alias for Headers) - used by `ResponseWriter::write_trailers` and `Request::trailers`.",
        },
        StdItem {
            name: "PushOptions",
            kind: StdItemKind::Type,
            doc: "Prioritization knobs for `ResponseWriter::push_promise` (weight, depends_on, exclusive).",
        },
        StdItem {
            name: "PushStream",
            kind: StdItemKind::Type,
            doc: "Server-initiated push stream returned by `ResponseWriter::push_promise`. Supports send_head / write / write_trailers / end.",
        },
    ],
};

pub const HTTP_ROUTER: StdModule = StdModule {
    path: "std::http::router",
    summary: "Go 1.22-class ServeMux: method-aware path patterns with parameter captures + prefix routes.",
    items: &[
        StdItem {
            name: "Router",
            kind: StdItemKind::Type,
            doc: "Routing table. Build with `Router::new()`, register routes via the verb methods, then pass to `http::serve`. Verb methods return the router so they chain with `|>`.",
        },
        StdItem {
            name: "Params",
            kind: StdItemKind::Type,
            doc: "Captured path parameters. Read inside a handler with `r.path_value(name) -> String`; returns `\"\"` for an undeclared name. All tiers.",
        },
        StdItem {
            name: "Handler",
            kind: StdItemKind::Trait,
            doc: "Anything callable as `Fn(Request, Params) -> Response`.",
        },
        StdItem {
            name: "new",
            kind: StdItemKind::Function,
            doc: "Allocate a fresh Router handle.",
        },
        StdItem {
            name: "add",
            kind: StdItemKind::Function,
            doc: "Register a pattern-only route: `(router, method, pattern)`. Used with `lookup` for low-level dispatch.",
        },
        StdItem {
            name: "lookup",
            kind: StdItemKind::Function,
            doc: "Find the index of the first route matching `(method, path)`. Returns `Option<i64>`.",
        },
    ],
};

pub const HTTP_MIDDLEWARE: StdModule = StdModule {
    path: "std::http::middleware",
    summary: "Composable middleware: request_id, cors, security_headers, hsts, cache_control, etag, rate_limit, body_limit, timeout, compress_gzip, logger, recoverer, basic_auth, bearer_auth, safe_defaults.",
    items: &[
        StdItem {
            name: "Handler",
            kind: StdItemKind::Trait,
            doc: "Anything serving (Request, Params) -> Response.",
        },
        StdItem {
            name: "Chain",
            kind: StdItemKind::Type,
            doc: "Helper for composing middleware in a single value.",
        },
        StdItem {
            name: "new_request_id",
            kind: StdItemKind::Function,
            doc: "Generate a process-monotonic request id string. Available in interp + compiled.",
        },
        StdItem {
            name: "tag",
            kind: StdItemKind::Function,
            doc: "Wrap a handler (`tag(inner) -> Handler`), prepending `mw:` to each response body. Deterministic composition primitive; available in interp + compiled.",
        },
        StdItem {
            name: "accepts_gzip",
            kind: StdItemKind::Function,
            doc: "Check an Accept-Encoding header for a gzip token. Available in interp + compiled.",
        },
        StdItem {
            name: "decode_basic_auth",
            kind: StdItemKind::Function,
            doc: "Decode a Basic-auth Authorization header into (user, password). Interp tier.",
        },
        StdItem {
            name: "bearer_ok",
            kind: StdItemKind::Function,
            doc: "Run a verify closure on the request's Bearer token; false (without calling verify) when no Bearer header is present. Available in interp + compiled.",
        },
        StdItem {
            name: "CorsConfig",
            kind: StdItemKind::Type,
            doc: "CORS configuration. `CorsConfig::permissive()` allows any origin and the common verbs; `CorsConfig::new(origin, methods, headers, max_age)` spells one out.",
        },
        StdItem {
            name: "HstsConfig",
            kind: StdItemKind::Type,
            doc: "HSTS configuration. `HstsConfig::safe_default()` is one year for this host; `HstsConfig::strict()` is two years with subdomains and preload.",
        },
        StdItem {
            name: "SecurityHeaders",
            kind: StdItemKind::Type,
            doc: "Security-header preset. `SecurityHeaders::strict()` adds CSP / COOP / Permissions-Policy on top of the baseline; `SecurityHeaders::off()` emits nothing.",
        },
        StdItem {
            name: "CacheControl",
            kind: StdItemKind::Type,
            doc: "Cache-Control policy. `CacheControl::no_store()` never caches; `CacheControl::immutable_for(seconds)` marks a content-hashed asset immutable.",
        },
        StdItem {
            name: "RateLimit",
            kind: StdItemKind::Type,
            doc: "Token-bucket budget. `RateLimit::per_ip(capacity, refill_per_sec)`.",
        },
        StdItem {
            name: "request_id",
            kind: StdItemKind::Function,
            doc: "`request_id(inner) -> Handler` - stamps `X-Request-Id` on every response, using a process-monotonic `req-<n>` counter so a chain's output is identical on every tier.",
        },
        StdItem {
            name: "cors",
            kind: StdItemKind::Function,
            doc: "`cors(inner, config: CorsConfig) -> Handler` - CORS response headers. Example: `middleware::cors(app, middleware::CorsConfig::permissive())`.",
        },
        StdItem {
            name: "security_headers",
            kind: StdItemKind::Function,
            doc: "`security_headers(inner, preset: SecurityHeaders) -> Handler` - X-Content-Type-Options, X-Frame-Options, and Referrer-Policy; the `strict` preset adds CSP, COOP, and Permissions-Policy. Example: `middleware::security_headers(app, middleware::SecurityHeaders::strict())`.",
        },
        StdItem {
            name: "etag",
            kind: StdItemKind::Function,
            doc: "`etag(inner) -> Handler` - sets a strong `ETag` derived from the response body, so the same body always yields the same validator.",
        },
        StdItem {
            name: "rate_limit",
            kind: StdItemKind::Function,
            doc: "`rate_limit(inner, config: RateLimit) -> Handler` - token-bucket limiter; past the budget the response becomes 429 with `Retry-After`. Example: `middleware::rate_limit(app, middleware::RateLimit::per_ip(100, 10))`.",
        },
        StdItem {
            name: "hsts",
            kind: StdItemKind::Function,
            doc: "`hsts(inner, config: HstsConfig) -> Handler` - sets `Strict-Transport-Security`. Example: `middleware::hsts(app, middleware::HstsConfig::safe_default())`.",
        },
        StdItem {
            name: "cache_control",
            kind: StdItemKind::Function,
            doc: "`cache_control(inner, config: CacheControl) -> Handler` - sets `Cache-Control`. Example: `middleware::cache_control(app, middleware::CacheControl::no_store())`.",
        },
        StdItem {
            name: "body_limit",
            kind: StdItemKind::Function,
            doc: "`body_limit(inner, max_bytes: i64) -> Handler` - responses larger than the budget become 413.",
        },
        StdItem {
            name: "compress_gzip",
            kind: StdItemKind::Function,
            doc: "`compress_gzip(inner) -> Handler` - advertises negotiated compression with `Vary: Accept-Encoding`; pair with `middleware::accepts_gzip` to decide per request.",
        },
        StdItem {
            name: "logger",
            kind: StdItemKind::Function,
            doc: "`logger(inner) -> Handler` - writes one `[http] <status> <bytes>b` line per response to stderr.",
        },
        StdItem {
            name: "recoverer",
            kind: StdItemKind::Function,
            doc: "`recoverer(inner) -> Handler` - replaces a 5xx response body with a fixed `internal server error`, so handler internals never leak.",
        },
        StdItem {
            name: "timeout",
            kind: StdItemKind::Function,
            doc: "`timeout(inner, budget_ms: i64) -> Handler` - stamps the budget as `X-Timeout-Ms` for downstream proxies.",
        },
        StdItem {
            name: "basic_auth",
            kind: StdItemKind::Function,
            doc: "`basic_auth(inner, realm: String) -> Handler` - adds `WWW-Authenticate: Basic realm=\"...\"` to a 401 response. Decode credentials with `middleware::decode_basic_auth`.",
        },
        StdItem {
            name: "bearer_auth",
            kind: StdItemKind::Function,
            doc: "`bearer_auth(inner, realm: String) -> Handler` - adds `WWW-Authenticate: Bearer` to a 401 response. Verify tokens with `middleware::bearer_ok`.",
        },
        StdItem {
            name: "safe_defaults",
            kind: StdItemKind::Function,
            doc: "`safe_defaults(inner) -> Handler` - strict security headers, HSTS, and a request id in one wrapper.",
        },
    ],
};

pub const HTTP_STATIC_FILES: StdModule = StdModule {
    path: "std::http::static_files",
    summary: "Caching static-file handler: ETag, Last-Modified, byte ranges, MIME sniff.",
    items: &[
        StdItem {
            name: "FileServer",
            kind: StdItemKind::Type,
            doc: "Static-file handler rooted at a directory (Rust-side; streaming).",
        },
        StdItem {
            name: "serve_file",
            kind: StdItemKind::Function,
            doc: "Read a single file and return it as a Response struct. Interp tier.",
        },
        StdItem {
            name: "mime_for_path",
            kind: StdItemKind::Function,
            doc: "Guess a MIME type from a file path's extension. Available in interp + compiled.",
        },
    ],
};

pub const HTTP_PROXY: StdModule = StdModule {
    path: "std::http::proxy",
    summary: "Reverse proxy on top of http::Client. Director-style request mutator + hop-by-hop strip + error handler.",
    items: &[
        StdItem {
            name: "Proxy",
            kind: StdItemKind::Type,
            doc: "Reverse-proxy handler (Rust-side).",
        },
        StdItem {
            name: "Director",
            kind: StdItemKind::Type,
            doc: "Fn(&mut Request) request mutator (Rust-side).",
        },
        StdItem {
            name: "forward",
            kind: StdItemKind::Function,
            doc: "One-shot upstream forward: `(url, method, body) -> Result<Response, Error>`. Interp tier.",
        },
    ],
};

pub const HTTP_WEBSOCKET: StdModule = StdModule {
    path: "std::http::websocket",
    summary: "RFC 6455 WebSocket support. Server-side accept + send_text / send_binary / ping / pong / close.",
    items: &[
        StdItem {
            name: "WebSocket",
            kind: StdItemKind::Type,
            doc: "Accepted WebSocket connection (Rust-side framing).",
        },
        StdItem {
            name: "Message",
            kind: StdItemKind::Type,
            doc: "Text / Binary / Ping / Pong / Close.",
        },
        StdItem {
            name: "accept",
            kind: StdItemKind::Function,
            doc: "Validate a WebSocket upgrade request and answer the 101 Switching Protocols response that completes the handshake.",
        },
        StdItem {
            name: "Error",
            kind: StdItemKind::Type,
            doc: "Io / Protocol / BadHandshake.",
        },
        StdItem {
            name: "accept_key",
            kind: StdItemKind::Function,
            doc: "Compute RFC 6455 Sec-WebSocket-Accept from a client nonce. Available in interp + compiled.",
        },
        StdItem {
            name: "is_websocket_upgrade",
            kind: StdItemKind::Function,
            doc: "Test whether an incoming Request carries a WebSocket upgrade handshake. Interp tier.",
        },
        StdItem {
            name: "serve",
            kind: StdItemKind::Function,
            doc: "serve(addr, handler) -> Result<(), Error>: bind, upgrade each connection, dispatch the handler's handle(self, ws) per connection.",
        },
        StdItem {
            name: "connect",
            kind: StdItemKind::Function,
            doc: "connect(url) -> Result<i64, Error>: client TCP connect + RFC 6455 upgrade; returns a WebSocket handle.",
        },
        StdItem {
            name: "send_text",
            kind: StdItemKind::Function,
            doc: "send_text(ws, s) -> Result<(), Error>: send one text frame.",
        },
        StdItem {
            name: "send_binary",
            kind: StdItemKind::Function,
            doc: "send_binary(ws, data) -> Result<(), Error>: send one binary frame.",
        },
        StdItem {
            name: "recv",
            kind: StdItemKind::Function,
            doc: "recv(ws) -> Result<String, Error>: next text message; Err on close/error.",
        },
        StdItem {
            name: "close",
            kind: StdItemKind::Function,
            doc: "close(ws) -> Result<(), Error>: send a close frame and release the handle.",
        },
    ],
};

pub const HTTP_SSE: StdModule = StdModule {
    path: "std::http::sse",
    summary: "Server-Sent Events (text/event-stream) emitter with heartbeat ticks and retry hint.",
    items: &[
        StdItem {
            name: "Stream",
            kind: StdItemKind::Type,
            doc: "Active SSE stream - handler writes events through it (Rust-side).",
        },
        StdItem {
            name: "Event",
            kind: StdItemKind::Type,
            doc: "One SSE event (id, event, data, retry).",
        },
        StdItem {
            name: "encode_event",
            kind: StdItemKind::Function,
            doc: "Render one event block as a string: `(event, data, id) -> String`. Available in interp + compiled.",
        },
        StdItem {
            name: "encode_comment",
            kind: StdItemKind::Function,
            doc: "Render a `:`-prefixed keepalive line. Available in interp + compiled.",
        },
        StdItem {
            name: "encode_retry",
            kind: StdItemKind::Function,
            doc: "Render a `retry:` reconnect-hint directive in milliseconds. Available in interp + compiled.",
        },
    ],
};

pub const HTTP_CHUNKED: StdModule = StdModule {
    path: "std::http::chunked",
    summary: "RFC 7230 §4.1 chunked transfer-encoding reader and writer.",
    items: &[
        StdItem {
            name: "Reader",
            kind: StdItemKind::Type,
            doc: "Decodes a chunked body from any Read source (Rust-side; streaming).",
        },
        StdItem {
            name: "Writer",
            kind: StdItemKind::Type,
            doc: "Encodes raw bytes into chunked frames over any Write sink (Rust-side; streaming).",
        },
        StdItem {
            name: "encode",
            kind: StdItemKind::Function,
            doc: "One-shot: wraps a buffer in chunked transfer-encoding with terminator. Available in interp + compiled.",
        },
        StdItem {
            name: "decode",
            kind: StdItemKind::Function,
            doc: "One-shot: concatenates data chunks from a complete chunked body. Available in interp + compiled.",
        },
    ],
};

pub const HTTP_NATIVE_CLIENT: StdModule = StdModule {
    path: "std::http::native_client",
    summary: "Goroutine-driven HTTP/1.1 client over std::net (no ureq, no blocking pool).",
    items: &[
        StdItem {
            name: "Client",
            kind: StdItemKind::Type,
            doc: "Native h1 client (Rust-side; full builder surface).",
        },
        StdItem {
            name: "Error",
            kind: StdItemKind::Type,
            doc: "Connect / Tls / Http / Redirect / Timeout / Io.",
        },
        StdItem {
            name: "get",
            kind: StdItemKind::Function,
            doc: "One-shot GET → Result<Response, Error>. Interp tier (compiled tier shares http::get).",
        },
        StdItem {
            name: "post",
            kind: StdItemKind::Function,
            doc: "One-shot POST: `(url, body, content_type)`. Interp tier.",
        },
        StdItem {
            name: "put",
            kind: StdItemKind::Function,
            doc: "One-shot PUT: `(url, body, content_type)`. Interp tier.",
        },
        StdItem {
            name: "delete",
            kind: StdItemKind::Function,
            doc: "One-shot DELETE → Result<Response, Error>. Interp tier.",
        },
    ],
};

pub const HTTP_COOKIE: StdModule = StdModule {
    path: "std::http::cookie",
    summary: "RFC 6265 cookie parser and Set-Cookie builder.",
    items: &[
        StdItem {
            name: "Cookie",
            kind: StdItemKind::Type,
            doc: "Parsed cookie with name, value, and Set-Cookie attributes.",
        },
        StdItem {
            name: "CookieBuilder",
            kind: StdItemKind::Type,
            doc: "Fluent builder for Set-Cookie response headers.",
        },
        StdItem {
            name: "SameSite",
            kind: StdItemKind::Type,
            doc: "SameSite attribute: Strict / Lax / None.",
        },
        StdItem {
            name: "parse_cookie_header",
            kind: StdItemKind::Function,
            doc: "Parse a Cookie request header into (name, value) pairs.",
        },
        StdItem {
            name: "serialize",
            kind: StdItemKind::Function,
            doc: "Render a Cookie as a Set-Cookie header value.",
        },
    ],
};

pub const HTTP_CSRF: StdModule = StdModule {
    path: "std::http::csrf",
    summary: "Double-submit-cookie CSRF protection with Origin / Referer allowlist.",
    items: &[
        StdItem {
            name: "Config",
            kind: StdItemKind::Type,
            doc: "Signing key, cookie / header names, and origin allowlist.",
        },
        StdItem {
            name: "RouteAuth",
            kind: StdItemKind::Type,
            doc: "Per-route policy: Required, Optional, or Skipped.",
        },
        StdItem {
            name: "issue_token",
            kind: StdItemKind::Function,
            doc: "Mint a fresh CSRF token bound to the configured signing key.",
        },
        StdItem {
            name: "verify_token",
            kind: StdItemKind::Function,
            doc: "Constant-time verify of a presented token against the cookie value.",
        },
        StdItem {
            name: "extract_token",
            kind: StdItemKind::Function,
            doc: "Pull a token from the configured header or form field.",
        },
        StdItem {
            name: "origin_allowed",
            kind: StdItemKind::Function,
            doc: "Origin / Referer allowlist check for unsafe methods.",
        },
        StdItem {
            name: "check",
            kind: StdItemKind::Function,
            doc: "Combined origin + token gate; returns Err on failure.",
        },
        StdItem {
            name: "attach_cookie",
            kind: StdItemKind::Function,
            doc: "Set the CSRF cookie on a Response.",
        },
    ],
};

pub const HTTP_FORM: StdModule = StdModule {
    path: "std::http::form",
    summary: "application/x-www-form-urlencoded parser and builder.",
    items: &[
        StdItem {
            name: "Form",
            kind: StdItemKind::Type,
            doc: "Parsed url-encoded body, queryable by field name.",
        },
        StdItem {
            name: "FormBuilder",
            kind: StdItemKind::Type,
            doc: "Builder for url-encoded request bodies.",
        },
    ],
};

pub const HTTP_HEALTH: StdModule = StdModule {
    path: "std::http::health",
    summary: "Liveness and readiness endpoints are ordinary handlers over `std::lifecycle`: answer 200 from a liveness route, and 200/503 from `lifecycle::is_ready()` on a readiness route, which drops to false on its own when shutdown begins. A probe registry with per-check timeouts belongs in an application package.",
    items: &[],
};

pub const HTTP_MULTIPART: StdModule = StdModule {
    path: "std::http::multipart",
    summary: "RFC 7578 multipart/form-data streaming parser.",
    items: &[
        StdItem {
            name: "Config",
            kind: StdItemKind::Type,
            doc: "Per-form size, part-count, and disk-spill limits.",
        },
        StdItem {
            name: "Part",
            kind: StdItemKind::Type,
            doc: "One field or file entry from a multipart body.",
        },
        StdItem {
            name: "PartData",
            kind: StdItemKind::Type,
            doc: "In-memory bytes or spilled-to-disk path for a part.",
        },
        StdItem {
            name: "Form",
            kind: StdItemKind::Type,
            doc: "Parsed multipart envelope: fields + file parts.",
        },
        StdItem {
            name: "parse",
            kind: StdItemKind::Function,
            doc: "Stream-parse from any Read source into a Form.",
        },
    ],
};

pub const HTTP_QUERY: StdModule = StdModule {
    path: "std::http::query",
    summary: "A request's query string is already parsed: read `request.query` for the raw text and `request.query_pairs` for the decoded name/value pairs.",
    items: &[],
};

pub const HTTP_SESSION: StdModule = StdModule {
    path: "std::http::session",
    summary: "Signs and verifies a session payload. The cookie itself - name, attributes, expiry, a server-side store, id rotation on privilege change, revocation - is application policy and belongs in a session package built on these two.",
    items: &[
        StdItem {
            name: "with_session",
            kind: StdItemKind::Function,
            doc: "Run a closure with the session bound; persist any mutations.",
        },
        StdItem {
            name: "sign",
            kind: StdItemKind::Function,
            doc: "Sign session data into a tamper-evident cookie value.",
        },
        StdItem {
            name: "verify",
            kind: StdItemKind::Function,
            doc: "Verify and decode a signed session cookie value.",
        },
    ],
};

pub const HTTP_STATE: StdModule = StdModule {
    path: "std::http::state",
    summary: "Dependency injection is closure capture: build the router from closures that capture the pool, the cache, and the configuration, and each handler reads what it captured. A captured heap value is shared, so one map serves every request.",
    items: &[],
};

pub const JWT: StdModule = StdModule {
    path: "std::jwt",
    summary: "RFC 7519 tokens. Signs with HS256 / HS384 / HS512, ES256, and EdDSA; verifies those plus the RS256 / RS384 / RS512 family every mainstream identity provider mints with. Claims cross the boundary as JSON text.",
    items: &[
        StdItem {
            name: "verify",
            kind: StdItemKind::Function,
            doc: "`verify(token, alg, key, leeway_secs, issuer, audience) -> Result<String, errors::Error>` - the verifier a service protecting an endpoint uses. One entry point for every algorithm, RS* included; `key` is the shared secret for HS* and the PEM public key for the rest. `issuer` and `audience` are enforced when non-empty - leaving them empty accepts a token minted for another service by anyone sharing the key.",
        },
        StdItem {
            name: "header",
            kind: StdItemKind::Function,
            doc: "`header(token) -> Result<String, errors::Error>` - the JOSE header as JSON, read WITHOUT verifying the signature. Read `kid` to choose a key from a key set; nothing in it is trustworthy until `verify` succeeds.",
        },
        StdItem {
            name: "sign_hs",
            kind: StdItemKind::Function,
            doc: "Sign claims with HMAC-SHA family using a shared key.",
        },
        StdItem {
            name: "verify_hs",
            kind: StdItemKind::Function,
            doc: "Verify an HS* token against a shared key with a clock-skew allowance. Prefer `verify`, which also enforces the issuer and the audience.",
        },
        StdItem {
            name: "sign_es256",
            kind: StdItemKind::Function,
            doc: "Sign with ECDSA P-256 from a PEM-encoded private key.",
        },
        StdItem {
            name: "verify_es256",
            kind: StdItemKind::Function,
            doc: "Verify an ES256 token against a PEM-encoded public key. Prefer `verify`, which also enforces the issuer and the audience.",
        },
        StdItem {
            name: "sign_eddsa",
            kind: StdItemKind::Function,
            doc: "Sign with Ed25519 from a PEM-encoded private key.",
        },
        StdItem {
            name: "verify_eddsa",
            kind: StdItemKind::Function,
            doc: "Verify an EdDSA token against a PEM-encoded public key. Prefer `verify`, which also enforces the issuer and the audience.",
        },
    ],
};

pub const HTTP_H3: StdModule = StdModule {
    path: "std::http_h3",
    summary: "HTTP/3 over QUIC. std::http_h3 is the retained 0.27 spelling; no std::http::h3 alias.",
    items: &[
        StdItem {
            name: "Handler",
            kind: StdItemKind::Trait,
            doc: "Per-request handler. `fn serve(&self, request: Request) -> Response`.",
        },
        StdItem {
            name: "H3Error",
            kind: StdItemKind::Type,
            doc: "Transport / protocol error variants surfaced from quinn + h3.",
        },
        StdItem {
            name: "serve",
            kind: StdItemKind::Function,
            doc: "Run an HTTP/3 server bound to `addr` with TLS certificate + key paths and the supplied handler.",
        },
        StdItem {
            name: "Client",
            kind: StdItemKind::Type,
            doc: "HTTP/3 client. `new` validates against the Mozilla root store; `insecure` skips verification (dev only). Methods: `get`, `post`, `put`, `delete`, `head`, `request`.",
        },
    ],
};
