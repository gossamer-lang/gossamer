#![allow(
    clippy::similar_names,
    clippy::type_complexity,
    clippy::map_unwrap_or,
    clippy::redundant_closure,
    clippy::items_after_statements,
    clippy::let_and_return,
    clippy::needless_pass_by_value,
    clippy::doc_markdown,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args
)]

//! HTTP middleware suite.
//!
//! Each middleware is a function that wraps a `Handler` (the
//! router's handler type) and returns a new handler. Composition
//! is straightforward function chaining:
//!
//! ```no_run
//! use gossamer_std::http::{Response, StatusCode};
//! use gossamer_std::http_router::Router;
//! use gossamer_std::http_middleware as mw;
//!
//! let mut app = Router::new();
//! app.get("/", |_r, _p| Response::text(StatusCode::OK, "ok"));
//!
//! let _handler = mw::logger(mw::request_id(app));
//! ```
//!
//! The shipped middlewares:
//!
//! - `logger` - request/response logging.
//! - `recoverer` - catches handler panics, returns 500.
//! - `request_id` - stamps every response with `X-Request-Id`.
//! - `cors` - CORS preflight + per-response headers.
//! - `basic_auth` - HTTP Basic auth gate.
//! - `compress_gzip` - gzips response bodies when the client
//!   advertises `Accept-Encoding: gzip`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gossamer_runtime::platform::Instant;

use crate::http::{Headers, Request, Response, StatusCode};
use crate::http_router::{Params, Router};

/// Trait covering anything callable as a request handler. Both
/// `Router` and `Arc<dyn Fn(...)>` impl this.
pub trait Handler: Send + Sync + 'static {
    /// Invokes the handler.
    fn serve(&self, request: &Request, params: &Params) -> Response;
}

impl Handler for Router {
    fn serve(&self, request: &Request, _params: &Params) -> Response {
        Router::serve(self, request)
    }
}

impl<F> Handler for F
where
    F: Fn(&Request, &Params) -> Response + Send + Sync + 'static,
{
    fn serve(&self, request: &Request, params: &Params) -> Response {
        self(request, params)
    }
}

/// Wraps a `Router` in a chain of middleware. The wrapper itself
/// is a `Handler` so middleware compose naturally.
pub struct Chain<H: Handler> {
    inner: Arc<H>,
}

impl<H: Handler> Chain<H> {
    /// Creates a new chain wrapping `inner`.
    pub fn new(inner: H) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}

impl<H: Handler> Handler for Chain<H> {
    fn serve(&self, request: &Request, params: &Params) -> Response {
        self.inner.serve(request, params)
    }
}

// --- Logger ----------------------------------------------------------

/// Wraps `inner` with structured per-request logging. Each
/// request prints one line:
///
/// `<method> <path> <status> <bytes> <duration_ms>`
///
/// to stderr. Override the sink by replacing `slog`'s default
/// handler (the wrapper writes through `slog::info!`).
pub fn logger<H: Handler + 'static>(inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    let wrapped = move |req: &Request, params: &Params| -> Response {
        let start = Instant::now();
        let resp = inner.serve(req, params);
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "[http] {} {} {} {}b {:.3}ms",
            req.method.as_str(),
            req.path,
            resp.status.as_u16(),
            resp.body.len(),
            elapsed_ms
        );
        resp
    };
    wrapped
}

// --- Recoverer -------------------------------------------------------

/// Wraps `inner` with panic recovery. If the inner handler
/// panics, a `500 Internal Server Error` response is returned and
/// the panic message is logged to stderr. The panic is suppressed
/// - the server continues serving other requests.
pub fn recoverer<H: Handler + std::panic::RefUnwindSafe + 'static>(inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    move |req: &Request, params: &Params| -> Response {
        // parking_lot locks in the request's `Context` are not
        // RefUnwindSafe (no poison machinery), and they are never held
        // across the handler call, so observing `req` after a caught
        // panic is safe - assert it, matching the HTTP/2 path.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.serve(req, params)));
        match result {
            Ok(r) => r,
            Err(payload) => {
                let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "<non-string panic>".to_string()
                };
                eprintln!("[http] PANIC in handler: {msg}");
                let mut headers = Headers::new();
                headers.insert("content-type", "text/plain; charset=utf-8");
                Response {
                    status: StatusCode(500),
                    headers,
                    body: b"internal server error".to_vec(),
                    raw_header_pairs: Vec::new(),
                    body_stream: None,
                }
            }
        }
    }
}

// --- Request-ID ------------------------------------------------------

static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Stamps each response with an `X-Request-Id` header. If the
/// incoming request already carries one, it is preserved (echoed
/// back on the response).
pub fn request_id<H: Handler + 'static>(inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    move |req: &Request, params: &Params| -> Response {
        let id = req
            .headers
            .get("x-request-id")
            .map_or_else(|| next_request_id(), str::to_string);
        let mut resp = inner.serve(req, params);
        resp.headers.insert("x-request-id", &id);
        resp
    }
}

fn next_request_id() -> String {
    let n = REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = gossamer_runtime::platform::system_time_now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{n:x}")
}

// --- CORS ------------------------------------------------------------

/// CORS configuration.
#[derive(Clone, Debug)]
pub struct CorsConfig {
    /// Allowed origin (`Access-Control-Allow-Origin`). Use `*`
    /// for any.
    pub allow_origin: String,
    /// Methods accepted on preflight.
    pub allow_methods: Vec<String>,
    /// Headers accepted on preflight.
    pub allow_headers: Vec<String>,
    /// Whether to allow credentials.
    pub allow_credentials: bool,
    /// Max-age for preflight response in seconds.
    pub max_age: u32,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allow_origin: "*".to_string(),
            allow_methods: vec![
                "GET".into(),
                "POST".into(),
                "PUT".into(),
                "DELETE".into(),
                "OPTIONS".into(),
            ],
            allow_headers: vec!["content-type".into(), "authorization".into()],
            allow_credentials: false,
            max_age: 600,
        }
    }
}

/// CORS middleware. Handles preflight `OPTIONS` requests
/// inline; for normal requests it forwards to the inner handler
/// and decorates the response with the CORS headers.
pub fn cors<H: Handler + 'static>(config: CorsConfig, inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    let cfg = config;
    move |req: &Request, params: &Params| -> Response {
        if req.method.as_str().eq_ignore_ascii_case("OPTIONS") {
            let mut headers = Headers::new();
            apply_cors_headers(&cfg, &mut headers);
            return Response {
                status: StatusCode(204),
                headers,
                body: Vec::new(),
                raw_header_pairs: Vec::new(),
                body_stream: None,
            };
        }
        let mut resp = inner.serve(req, params);
        apply_cors_headers(&cfg, &mut resp.headers);
        resp
    }
}

fn apply_cors_headers(cfg: &CorsConfig, headers: &mut Headers) {
    headers.insert("access-control-allow-origin", &cfg.allow_origin);
    headers.insert(
        "access-control-allow-methods",
        &cfg.allow_methods.join(", "),
    );
    headers.insert(
        "access-control-allow-headers",
        &cfg.allow_headers.join(", "),
    );
    if cfg.allow_credentials {
        headers.insert("access-control-allow-credentials", "true");
    }
    headers.insert("access-control-max-age", &cfg.max_age.to_string());
}

// --- Basic auth ------------------------------------------------------

/// HTTP Basic authentication middleware. Returns `401
/// Unauthorized` with a `WWW-Authenticate` challenge when the
/// request does not carry valid credentials.
///
/// The `verify` closure receives `(username, password)` and
/// returns `true` to accept. Comparison should be
/// constant-time; the caller is responsible for that.
pub fn basic_auth<H, V>(realm: impl Into<String>, verify: V, inner: H) -> impl Handler
where
    H: Handler + 'static,
    V: Fn(&str, &str) -> bool + Send + Sync + 'static,
{
    let inner = Arc::new(inner);
    let realm = realm.into();
    move |req: &Request, params: &Params| -> Response {
        let header = req.headers.get("authorization").unwrap_or("");
        if let Some(rest) = header
            .strip_prefix("Basic ")
            .or_else(|| header.strip_prefix("basic "))
        {
            if let Some((user, pass)) = decode_basic(rest.trim())
                && verify(&user, &pass)
            {
                return inner.serve(req, params);
            }
        }
        let mut headers = Headers::new();
        headers.insert(
            "www-authenticate",
            &format!("Basic realm=\"{}\", charset=\"UTF-8\"", realm),
        );
        Response {
            status: StatusCode(401),
            headers,
            body: b"unauthorized".to_vec(),
            raw_header_pairs: Vec::new(),
            body_stream: None,
        }
    }
}

fn decode_basic(b64: &str) -> Option<(String, String)> {
    let decoded = base64_decode(b64.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, pass) = text.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

fn base64_decode(input: &str) -> Result<Vec<u8>, ()> {
    // Standard RFC 4648 base64 (no URL-safe variant). Built
    // inline to avoid a dependency on the std::encoding feature
    // which is gated.
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for ch in input.chars() {
        if ch == '=' {
            break;
        }
        let v: u32 = match ch {
            'A'..='Z' => u32::from(ch as u8 - b'A'),
            'a'..='z' => u32::from(ch as u8 - b'a') + 26,
            '0'..='9' => u32::from(ch as u8 - b'0') + 52,
            '+' => 62,
            '/' => 63,
            ' ' | '\r' | '\n' | '\t' => continue,
            _ => return Err(()),
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

// --- Compress (gzip) -------------------------------------------------

/// Gzip-compresses the response body when the client advertises
/// `Accept-Encoding: gzip`. Sets `Content-Encoding: gzip` and
/// recalculates `Content-Length`. Bodies below `min_bytes` are
/// emitted uncompressed (the framing overhead isn't worth it).
///
/// This middleware depends on the `compress` Cargo feature
/// (flate2). Without the feature, it's a no-op pass-through.
pub fn compress_gzip<H: Handler + 'static>(min_bytes: usize, inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    move |req: &Request, params: &Params| -> Response {
        #[allow(
            unused_mut,
            reason = "the binding is mutated only on the compression path this build may not include"
        )]
        let mut resp = inner.serve(req, params);
        if resp.body.len() < min_bytes {
            return resp;
        }
        let accept = req.headers.get("accept-encoding").unwrap_or("");
        if !accept
            .split(',')
            .any(|tok| tok.trim().eq_ignore_ascii_case("gzip"))
        {
            return resp;
        }
        {
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            if encoder.write_all(&resp.body).is_ok()
                && let Ok(compressed) = encoder.finish()
            {
                resp.body = compressed;
                resp.headers.insert("content-encoding", "gzip");
                resp.headers.remove("content-length");
                let len = resp.body.len().to_string();
                resp.headers.insert("content-length", &len);
            }
        }
        resp
    }
}

// --- Body limit -------------------------------------------------------

/// Rejects requests whose body exceeds `max_bytes` with 413.
///
/// Checked against `Content-Length` when present, then against
/// `request.body.len()` (post-read). Streaming bodies that arrive
/// without `Content-Length` are accepted up to the read cap; the
/// caller should also configure the HTTP server's max body size.
pub fn body_limit<H: Handler + 'static>(max_bytes: usize, inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    move |req: &Request, params: &Params| -> Response {
        if let Some(cl) = req.headers.get("content-length")
            && let Ok(n) = cl.parse::<usize>()
            && n > max_bytes
        {
            return payload_too_large();
        }
        if req.body.len() > max_bytes {
            return payload_too_large();
        }
        inner.serve(req, params)
    }
}

fn payload_too_large() -> Response {
    let mut h = Headers::new();
    h.insert("content-type", "text/plain; charset=utf-8");
    Response {
        status: StatusCode(413),
        headers: h,
        body: b"payload too large".to_vec(),
        raw_header_pairs: Vec::new(),
        body_stream: None,
    }
}

// --- Timeout ----------------------------------------------------------

/// Wraps the handler in a deadline. If it does not return within
/// `limit`, the wrapper returns 504 Gateway Timeout. The
/// underlying handler keeps running (we have no preemption);
/// cooperative cancellation is the caller's job via
/// `request.context`.
///
/// Uses a fresh OS thread per request to enforce the deadline.
/// For request rates > a few hundred QPS, prefer co-operating
/// with `request.context` deadlines in the handler.
pub fn timeout<H: Handler + 'static>(limit: std::time::Duration, inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    move |req: &Request, params: &Params| -> Response {
        let inner_clone = Arc::clone(&inner);
        let req = req.clone();
        let params = params.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Response>(1);
        std::thread::spawn(move || {
            let resp = inner_clone.serve(&req, &params);
            let _ = tx.send(resp);
        });
        match rx.recv_timeout(limit) {
            Ok(resp) => resp,
            Err(_) => gateway_timeout(),
        }
    }
}

fn gateway_timeout() -> Response {
    let mut h = Headers::new();
    h.insert("content-type", "text/plain; charset=utf-8");
    Response {
        status: StatusCode(504),
        headers: h,
        body: b"gateway timeout".to_vec(),
        raw_header_pairs: Vec::new(),
        body_stream: None,
    }
}

// --- HSTS -------------------------------------------------------------

/// Strict-Transport-Security configuration.
#[derive(Clone, Debug)]
pub struct HstsConfig {
    /// `max-age` in seconds. RFC 6797 recommends >= 1 year for
    /// production after rollout (default = 31536000).
    pub max_age_secs: u64,
    /// `includeSubDomains` directive.
    pub include_subdomains: bool,
    /// `preload` directive - only set if the domain is enrolled
    /// in the HSTS preload list.
    pub preload: bool,
}

impl Default for HstsConfig {
    fn default() -> Self {
        Self {
            max_age_secs: 31_536_000,
            include_subdomains: true,
            preload: false,
        }
    }
}

impl HstsConfig {
    /// Conservative starter config for a domain not yet enrolled
    /// in the HSTS preload list.
    #[must_use]
    pub fn safe_default() -> Self {
        Self::default()
    }

    /// Strictest practical config - set after the domain has run
    /// HSTS-only for a quarter and is preload-eligible.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            max_age_secs: 63_072_000,
            include_subdomains: true,
            preload: true,
        }
    }

    fn header_value(&self) -> String {
        let mut v = format!("max-age={}", self.max_age_secs);
        if self.include_subdomains {
            v.push_str("; includeSubDomains");
        }
        if self.preload {
            v.push_str("; preload");
        }
        v
    }
}

/// Adds `Strict-Transport-Security` to every response.
pub fn hsts<H: Handler + 'static>(config: HstsConfig, inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    let value = config.header_value();
    move |req: &Request, params: &Params| -> Response {
        let mut resp = inner.serve(req, params);
        resp.headers.insert("strict-transport-security", &value);
        resp
    }
}

// --- Security headers -------------------------------------------------

/// Bundle of restrictive default security headers.
#[derive(Clone, Debug)]
pub struct SecurityHeaders {
    /// When `true`, emit `X-Content-Type-Options: nosniff`.
    pub content_type_options_nosniff: bool,
    /// Optional `X-Frame-Options` value (e.g. `DENY`, `SAMEORIGIN`).
    pub frame_options: Option<String>,
    /// Optional `Referrer-Policy` value
    /// (e.g. `strict-origin-when-cross-origin`).
    pub referrer_policy: Option<String>,
    /// Optional `Permissions-Policy` directive list.
    pub permissions_policy: Option<String>,
    /// Optional `Content-Security-Policy` directive string.
    pub content_security_policy: Option<String>,
    /// Optional `Cross-Origin-Opener-Policy` value
    /// (e.g. `same-origin`, `same-origin-allow-popups`).
    pub cross_origin_opener_policy: Option<String>,
    /// Optional `Cross-Origin-Resource-Policy` value
    /// (e.g. `same-origin`, `same-site`, `cross-origin`).
    pub cross_origin_resource_policy: Option<String>,
}

impl Default for SecurityHeaders {
    fn default() -> Self {
        Self::strict()
    }
}

impl SecurityHeaders {
    /// Restrictive defaults suitable for an API or admin app.
    /// Apps that embed in iframes / consume third-party scripts
    /// must override `frame_options` and `content_security_policy`.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            content_type_options_nosniff: true,
            frame_options: Some("DENY".to_string()),
            referrer_policy: Some("strict-origin-when-cross-origin".to_string()),
            permissions_policy: Some("geolocation=(), microphone=(), camera=()".to_string()),
            content_security_policy: Some(
                "default-src 'self'; frame-ancestors 'none'; base-uri 'self'".to_string(),
            ),
            cross_origin_opener_policy: Some("same-origin".to_string()),
            cross_origin_resource_policy: Some("same-origin".to_string()),
        }
    }

    /// Headers off - explicit opt-out for cases where the user
    /// configures them upstream (e.g. behind a reverse proxy).
    #[must_use]
    pub fn off() -> Self {
        Self {
            content_type_options_nosniff: false,
            frame_options: None,
            referrer_policy: None,
            permissions_policy: None,
            content_security_policy: None,
            cross_origin_opener_policy: None,
            cross_origin_resource_policy: None,
        }
    }
}

/// Injects [`SecurityHeaders`] into every response (only when the
/// header is not already set - the handler can override).
pub fn security_headers<H: Handler + 'static>(config: SecurityHeaders, inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    move |req: &Request, params: &Params| -> Response {
        let mut resp = inner.serve(req, params);
        if config.content_type_options_nosniff && !resp.headers.contains("x-content-type-options") {
            resp.headers.insert("x-content-type-options", "nosniff");
        }
        if let Some(v) = &config.frame_options
            && !resp.headers.contains("x-frame-options")
        {
            resp.headers.insert("x-frame-options", v);
        }
        if let Some(v) = &config.referrer_policy
            && !resp.headers.contains("referrer-policy")
        {
            resp.headers.insert("referrer-policy", v);
        }
        if let Some(v) = &config.permissions_policy
            && !resp.headers.contains("permissions-policy")
        {
            resp.headers.insert("permissions-policy", v);
        }
        if let Some(v) = &config.content_security_policy
            && !resp.headers.contains("content-security-policy")
        {
            resp.headers.insert("content-security-policy", v);
        }
        if let Some(v) = &config.cross_origin_opener_policy
            && !resp.headers.contains("cross-origin-opener-policy")
        {
            resp.headers.insert("cross-origin-opener-policy", v);
        }
        if let Some(v) = &config.cross_origin_resource_policy
            && !resp.headers.contains("cross-origin-resource-policy")
        {
            resp.headers.insert("cross-origin-resource-policy", v);
        }
        resp
    }
}

// --- Cache-Control ----------------------------------------------------

/// `Cache-Control` directive builder. The bool fields each map
/// 1:1 to a distinct RFC 7234 directive; bundling them as a bitset
/// would lose the named-field clarity callers rely on.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool maps 1:1 to a named RFC 7234 directive; collapsing to a bitset \
    loses the per-field clarity callers rely on when building Cache-Control values"
)]
#[derive(Clone, Debug, Default)]
pub struct CacheControl {
    /// Emit the `no-store` directive.
    pub no_store: bool,
    /// Emit the `no-cache` directive.
    pub no_cache: bool,
    /// Emit the `public` directive.
    pub public: bool,
    /// Emit the `private` directive.
    pub private: bool,
    /// When set, emit `max-age=<secs>`.
    pub max_age_secs: Option<u64>,
    /// When set, emit `s-maxage=<secs>` (shared cache lifetime).
    pub s_maxage_secs: Option<u64>,
    /// Emit the `immutable` directive.
    pub immutable: bool,
    /// Emit the `must-revalidate` directive.
    pub must_revalidate: bool,
}

impl CacheControl {
    /// `no-store, no-cache, must-revalidate` - appropriate for
    /// any response carrying personal or session-tied data.
    #[must_use]
    pub fn no_store() -> Self {
        Self {
            no_store: true,
            no_cache: true,
            must_revalidate: true,
            ..Self::default()
        }
    }

    /// `public, max-age=N, immutable` - for fingerprinted assets
    /// like `app-3f9c2a.css`.
    #[must_use]
    pub fn immutable_for(secs: u64) -> Self {
        Self {
            public: true,
            max_age_secs: Some(secs),
            immutable: true,
            ..Self::default()
        }
    }

    fn header_value(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.no_store {
            parts.push("no-store".into());
        }
        if self.no_cache {
            parts.push("no-cache".into());
        }
        if self.public {
            parts.push("public".into());
        }
        if self.private {
            parts.push("private".into());
        }
        if let Some(n) = self.max_age_secs {
            parts.push(format!("max-age={n}"));
        }
        if let Some(n) = self.s_maxage_secs {
            parts.push(format!("s-maxage={n}"));
        }
        if self.immutable {
            parts.push("immutable".into());
        }
        if self.must_revalidate {
            parts.push("must-revalidate".into());
        }
        parts.join(", ")
    }
}

/// Stamps `Cache-Control` onto every response (unless the
/// handler already set it).
pub fn cache_control<H: Handler + 'static>(config: CacheControl, inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    let value = config.header_value();
    move |req: &Request, params: &Params| -> Response {
        let mut resp = inner.serve(req, params);
        if !resp.headers.contains("cache-control") {
            resp.headers.insert("cache-control", &value);
        }
        resp
    }
}

// --- ETag -------------------------------------------------------------

/// Adds an `ETag` header (SHA-256 hex-truncated) and short-circuits
/// to 304 on a matching `If-None-Match`.
pub fn etag<H: Handler + 'static>(inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    move |req: &Request, params: &Params| -> Response {
        let mut resp = inner.serve(req, params);
        if resp.body.is_empty() || !resp.status.is_success() {
            return resp;
        }
        let tag = compute_etag(&resp.body);
        if let Some(client_tag) = req.headers.get("if-none-match")
            && client_tag.trim_matches('"') == tag.trim_matches('"')
        {
            let mut h = Headers::new();
            h.insert("etag", &tag);
            return Response {
                status: StatusCode(304),
                headers: h,
                body: Vec::new(),
                raw_header_pairs: Vec::new(),
                body_stream: None,
            };
        }
        resp.headers.insert("etag", &tag);
        resp
    }
}

fn compute_etag(body: &[u8]) -> String {
    use crate::crypto::sha256;
    let digest = sha256::digest(body);
    let mut hex = String::with_capacity(34);
    hex.push('"');
    for b in &digest[..16] {
        hex.push(nibble(b >> 4));
        hex.push(nibble(b & 0xf));
    }
    hex.push('"');
    hex
}

const fn nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + n - 10) as char,
        _ => '?',
    }
}

// --- Bearer auth ------------------------------------------------------

/// HTTP Bearer token authentication. The `verify` closure receives
/// the raw token (everything after `Bearer `) and returns
/// `Ok(())` to admit the request or `Err(reason)` to reject with
/// 401.
///
/// `verify` should run in constant time relative to the token -
/// any HMAC / JWT verifier built on `crate::crypto::subtle` is
/// safe; raw string equality is not.
pub fn bearer_auth<H, V>(realm: impl Into<String>, verify: V, inner: H) -> impl Handler
where
    H: Handler + 'static,
    V: Fn(&str) -> Result<(), String> + Send + Sync + 'static,
{
    let inner = Arc::new(inner);
    let realm = realm.into();
    move |req: &Request, params: &Params| -> Response {
        let header = req.headers.get("authorization").unwrap_or("");
        let token = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "));
        match token {
            Some(t) => match verify(t.trim()) {
                Ok(()) => inner.serve(req, params),
                Err(reason) => unauthorized_bearer(&realm, Some(&reason)),
            },
            None => unauthorized_bearer(&realm, None),
        }
    }
}

fn unauthorized_bearer(realm: &str, error: Option<&str>) -> Response {
    let mut h = Headers::new();
    let challenge = match error {
        Some(e) => {
            format!(r#"Bearer realm="{realm}", error="invalid_token", error_description="{e}""#)
        }
        None => format!(r#"Bearer realm="{realm}""#),
    };
    h.insert("www-authenticate", &challenge);
    h.insert("content-type", "text/plain; charset=utf-8");
    Response {
        status: StatusCode(401),
        headers: h,
        body: b"unauthorized".to_vec(),
        raw_header_pairs: Vec::new(),
        body_stream: None,
    }
}

// --- Rate limiter (token bucket per IP) -------------------------------

/// Per-key token-bucket rate limiter. Keys are typically the
/// peer IP, but any string extractor works.
#[derive(Clone)]
pub struct RateLimit {
    inner: Arc<RateLimitState>,
}

struct RateLimitState {
    capacity: f64,
    refill_per_sec: f64,
    buckets: parking_lot::Mutex<
        std::collections::HashMap<String, (f64, gossamer_runtime::platform::Instant)>,
    >,
    max_keys: usize,
}

impl RateLimit {
    /// `capacity` tokens; refill rate `capacity / window` per second.
    #[must_use]
    pub fn new(capacity: u32, window: std::time::Duration) -> Self {
        let refill = f64::from(capacity) / window.as_secs_f64().max(0.001);
        Self {
            inner: Arc::new(RateLimitState {
                capacity: f64::from(capacity),
                refill_per_sec: refill,
                buckets: parking_lot::Mutex::new(std::collections::HashMap::new()),
                max_keys: 100_000,
            }),
        }
    }

    /// Per-IP convenience (uses `X-Forwarded-For` first hop, then
    /// `X-Real-IP`, then a literal `"unknown"`).
    #[must_use]
    pub fn per_ip(capacity: u32, window: std::time::Duration) -> Self {
        Self::new(capacity, window)
    }

    /// Returns `true` if a token is available (and consumes it),
    /// `false` if the bucket is empty.
    #[must_use]
    pub fn try_consume(&self, key: &str) -> bool {
        let mut guard = self.inner.buckets.lock();
        if guard.len() > self.inner.max_keys {
            // Hard cap: drop the oldest half to prevent unbounded growth.
            let half = guard.len() / 2;
            let drop_keys: Vec<String> = guard.keys().take(half).cloned().collect();
            for k in drop_keys {
                guard.remove(&k);
            }
        }
        let now = gossamer_runtime::platform::Instant::now();
        let entry = guard
            .entry(key.to_string())
            .or_insert_with(|| (self.inner.capacity, now));
        let (tokens, last) = *entry;
        let elapsed = now.saturating_duration_since(last).as_secs_f64();
        let mut new_tokens =
            (tokens + elapsed * self.inner.refill_per_sec).min(self.inner.capacity);
        let allowed = new_tokens >= 1.0;
        if allowed {
            new_tokens -= 1.0;
        }
        *entry = (new_tokens, now);
        allowed
    }
}

fn extract_client_key(req: &Request) -> String {
    if let Some(xff) = req.headers.get("x-forwarded-for")
        && let Some(first) = xff.split(',').next()
        && !first.trim().is_empty()
    {
        return first.trim().to_string();
    }
    if let Some(real) = req.headers.get("x-real-ip")
        && !real.trim().is_empty()
    {
        return real.trim().to_string();
    }
    "unknown".to_string()
}

/// Rate-limit middleware. Rejects with 429 when the bucket for the
/// extracted key is empty.
pub fn rate_limit<H: Handler + 'static>(limiter: RateLimit, inner: H) -> impl Handler {
    let inner = Arc::new(inner);
    move |req: &Request, params: &Params| -> Response {
        let key = extract_client_key(req);
        if !limiter.try_consume(&key) {
            return too_many_requests();
        }
        inner.serve(req, params)
    }
}

fn too_many_requests() -> Response {
    let mut h = Headers::new();
    h.insert("content-type", "text/plain; charset=utf-8");
    h.insert("retry-after", "1");
    Response {
        status: StatusCode(429),
        headers: h,
        body: b"too many requests".to_vec(),
        raw_header_pairs: Vec::new(),
        body_stream: None,
    }
}

// --- Safe defaults composer -------------------------------------------

/// Composes the recommended "safe defaults" middleware chain in
/// the canonical order:
///
/// `request_id -> logger -> recoverer -> security_headers -> body_limit -> timeout`
///
/// CSRF, CORS, session, and rate-limit are intentionally NOT
/// included - they require app-specific configuration. Wire them
/// explicitly above the safe-defaults chain.
pub fn safe_defaults<H: Handler + std::panic::RefUnwindSafe + 'static>(inner: H) -> impl Handler {
    let chain = body_limit(1024 * 1024, inner);
    let chain = timeout(std::time::Duration::from_secs(30), chain);
    let chain = security_headers(SecurityHeaders::strict(), chain);
    let chain = recoverer(chain);
    let chain = logger(chain);
    request_id(chain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
    use crate::http::Method;

    fn req(method: Method, path: &str) -> Request {
        Request {
            method,
            path: path.to_string(),
            query: String::new(),
            headers: Headers::new(),
            body: Vec::new(),
            context: Context::background(),
            trailers: None,
            peer_addr: String::new(),
        }
    }

    fn text_response(status: u16, body: &str) -> Response {
        Response {
            status: StatusCode(status),
            headers: Headers::new(),
            body: body.as_bytes().to_vec(),
            raw_header_pairs: Vec::new(),
            body_stream: None,
        }
    }

    #[test]
    fn logger_passes_response_through() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "ok");
        let wrapped = logger(inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert_eq!(resp.status, StatusCode(200));
        assert_eq!(resp.body, b"ok");
    }

    #[test]
    fn recoverer_catches_panic_returns_500() {
        let inner = |_req: &Request, _p: &Params| -> Response {
            panic!("handler exploded");
        };
        let wrapped = recoverer(inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert_eq!(resp.status, StatusCode(500));
        assert_eq!(resp.body, b"internal server error");
    }

    #[test]
    fn recoverer_passes_clean_response_through() {
        let inner = |_req: &Request, _p: &Params| text_response(201, "created");
        let wrapped = recoverer(inner);
        let resp = wrapped.serve(&req(Method::Post, "/x"), &Params::default());
        assert_eq!(resp.status, StatusCode(201));
        assert_eq!(resp.body, b"created");
    }

    #[test]
    fn request_id_stamps_response_header() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "");
        let wrapped = request_id(inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert!(resp.headers.get("x-request-id").is_some());
    }

    #[test]
    fn request_id_preserves_incoming() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "");
        let wrapped = request_id(inner);
        let mut r = req(Method::Get, "/x");
        r.headers.insert("x-request-id", "custom-id-123");
        let resp = wrapped.serve(&r, &Params::default());
        assert_eq!(resp.headers.get("x-request-id"), Some("custom-id-123"));
    }

    #[test]
    fn cors_preflight_returns_204_with_headers() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "real");
        let wrapped = cors(CorsConfig::default(), inner);
        let resp = wrapped.serve(&req(Method::Options, "/x"), &Params::default());
        assert_eq!(resp.status, StatusCode(204));
        assert_eq!(resp.headers.get("access-control-allow-origin"), Some("*"));
        assert!(resp.headers.get("access-control-allow-methods").is_some());
    }

    #[test]
    fn cors_decorates_non_preflight_response() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "ok");
        let wrapped = cors(CorsConfig::default(), inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert_eq!(resp.body, b"ok");
        assert_eq!(resp.headers.get("access-control-allow-origin"), Some("*"));
    }

    #[test]
    fn basic_auth_rejects_missing_credentials() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "secret");
        let wrapped = basic_auth(
            "test-realm",
            |u, p| u == "alice" && p == "wonderland",
            inner,
        );
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert_eq!(resp.status, StatusCode(401));
        let challenge = resp.headers.get("www-authenticate").unwrap_or("");
        assert!(challenge.contains("test-realm"));
    }

    #[test]
    fn basic_auth_accepts_correct_credentials() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "secret");
        let wrapped = basic_auth("test", |u, p| u == "alice" && p == "wonderland", inner);
        let mut r = req(Method::Get, "/x");
        // base64("alice:wonderland") = "YWxpY2U6d29uZGVybGFuZA=="
        r.headers
            .insert("authorization", "Basic YWxpY2U6d29uZGVybGFuZA==");
        let resp = wrapped.serve(&r, &Params::default());
        assert_eq!(resp.status, StatusCode(200));
        assert_eq!(resp.body, b"secret");
    }

    #[test]
    fn basic_auth_rejects_wrong_password() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "secret");
        let wrapped = basic_auth("test", |u, p| u == "alice" && p == "wonderland", inner);
        let mut r = req(Method::Get, "/x");
        // base64("alice:wrong") = "YWxpY2U6d3Jvbmc="
        r.headers.insert("authorization", "Basic YWxpY2U6d3Jvbmc=");
        let resp = wrapped.serve(&r, &Params::default());
        assert_eq!(resp.status, StatusCode(401));
    }

    #[test]
    fn base64_round_trip_minimal_vectors() {
        // Sanity-check the inline base64 decoder.
        assert_eq!(base64_decode("YWJj").unwrap(), b"abc");
        assert_eq!(base64_decode("YWI=").unwrap(), b"ab");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert_eq!(base64_decode("").unwrap(), b"");
    }

    #[test]
    fn compress_gzip_encodes_when_accepted_and_above_threshold() {
        let payload: Vec<u8> = (0..2048).map(|i| (i & 0xff) as u8).collect();
        let payload_cloned = payload.clone();
        let inner = move |_req: &Request, _p: &Params| Response {
            status: StatusCode(200),
            headers: Headers::new(),
            body: payload_cloned.clone(),
            raw_header_pairs: Vec::new(),
            body_stream: None,
        };
        let wrapped = compress_gzip(64, inner);
        let mut r = req(Method::Get, "/x");
        r.headers.insert("accept-encoding", "gzip, deflate");
        let resp = wrapped.serve(&r, &Params::default());
        assert_eq!(resp.headers.get("content-encoding"), Some("gzip"));
        assert!(resp.body.len() < payload.len(), "expected compression");
        // Decode and verify round-trip.
        use std::io::Read;
        let mut decoder = flate2::read::GzDecoder::new(&resp.body[..]);
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn compress_gzip_skips_below_threshold() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "small");
        let wrapped = compress_gzip(64, inner);
        let mut r = req(Method::Get, "/x");
        r.headers.insert("accept-encoding", "gzip");
        let resp = wrapped.serve(&r, &Params::default());
        assert!(resp.headers.get("content-encoding").is_none());
        assert_eq!(resp.body, b"small");
    }

    #[test]
    fn compress_gzip_skips_when_not_accepted() {
        let payload: Vec<u8> = vec![0u8; 4096];
        let payload_cloned = payload.clone();
        let inner = move |_req: &Request, _p: &Params| Response {
            status: StatusCode(200),
            headers: Headers::new(),
            body: payload_cloned.clone(),
            raw_header_pairs: Vec::new(),
            body_stream: None,
        };
        let wrapped = compress_gzip(64, inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert!(resp.headers.get("content-encoding").is_none());
        assert_eq!(resp.body.len(), payload.len());
    }

    // ----- body_limit -----

    #[test]
    fn body_limit_rejects_too_large_content_length() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "ok");
        let wrapped = body_limit(1024, inner);
        let mut r = req(Method::Post, "/x");
        r.headers.insert("content-length", "2048");
        let resp = wrapped.serve(&r, &Params::default());
        assert_eq!(resp.status, StatusCode(413));
    }

    #[test]
    fn body_limit_rejects_too_large_body() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "ok");
        let wrapped = body_limit(8, inner);
        let mut r = req(Method::Post, "/x");
        r.body = b"this is more than eight bytes".to_vec();
        let resp = wrapped.serve(&r, &Params::default());
        assert_eq!(resp.status, StatusCode(413));
    }

    #[test]
    fn body_limit_passes_under_threshold() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "ok");
        let wrapped = body_limit(1024, inner);
        let mut r = req(Method::Post, "/x");
        r.body = b"tiny".to_vec();
        let resp = wrapped.serve(&r, &Params::default());
        assert_eq!(resp.status, StatusCode(200));
    }

    // ----- timeout -----

    #[test]
    fn timeout_returns_504_when_handler_too_slow() {
        let inner = |_req: &Request, _p: &Params| -> Response {
            gossamer_runtime::platform::sleep(std::time::Duration::from_millis(200));
            text_response(200, "late")
        };
        let wrapped = timeout(std::time::Duration::from_millis(20), inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert_eq!(resp.status, StatusCode(504));
    }

    #[test]
    fn timeout_passes_fast_handler() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "fast");
        let wrapped = timeout(std::time::Duration::from_secs(5), inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert_eq!(resp.status, StatusCode(200));
        assert_eq!(resp.body, b"fast");
    }

    // ----- HSTS -----

    #[test]
    fn hsts_adds_header() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "");
        let wrapped = hsts(HstsConfig::default(), inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        let v = resp.headers.get("strict-transport-security").unwrap_or("");
        assert!(v.contains("max-age=31536000"));
        assert!(v.contains("includeSubDomains"));
    }

    #[test]
    fn hsts_strict_includes_preload() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "");
        let wrapped = hsts(HstsConfig::strict(), inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        let v = resp.headers.get("strict-transport-security").unwrap_or("");
        assert!(v.contains("preload"));
        assert!(v.contains("max-age=63072000"));
    }

    // ----- Security headers -----

    #[test]
    fn security_headers_inserts_full_bundle() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "");
        let wrapped = security_headers(SecurityHeaders::strict(), inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert_eq!(resp.headers.get("x-content-type-options"), Some("nosniff"));
        assert_eq!(resp.headers.get("x-frame-options"), Some("DENY"));
        assert!(resp.headers.get("referrer-policy").is_some());
        assert!(resp.headers.get("content-security-policy").is_some());
    }

    #[test]
    fn security_headers_does_not_override_existing() {
        let inner = |_req: &Request, _p: &Params| -> Response {
            let mut h = Headers::new();
            h.insert("x-frame-options", "SAMEORIGIN");
            Response {
                status: StatusCode(200),
                headers: h,
                body: Vec::new(),
                raw_header_pairs: Vec::new(),
                body_stream: None,
            }
        };
        let wrapped = security_headers(SecurityHeaders::strict(), inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert_eq!(resp.headers.get("x-frame-options"), Some("SAMEORIGIN"));
    }

    // ----- Cache-control -----

    #[test]
    fn cache_control_no_store() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "");
        let wrapped = cache_control(CacheControl::no_store(), inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        let v = resp.headers.get("cache-control").unwrap_or("");
        assert!(v.contains("no-store"));
        assert!(v.contains("no-cache"));
        assert!(v.contains("must-revalidate"));
    }

    #[test]
    fn cache_control_immutable_for_assets() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "");
        let wrapped = cache_control(CacheControl::immutable_for(31_536_000), inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        let v = resp.headers.get("cache-control").unwrap_or("");
        assert!(v.contains("public"));
        assert!(v.contains("max-age=31536000"));
        assert!(v.contains("immutable"));
    }

    // ----- ETag -----

    #[test]
    fn etag_adds_header_for_body() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "hello");
        let wrapped = etag(inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert!(resp.headers.get("etag").is_some());
    }

    #[test]
    fn etag_returns_304_on_matching_if_none_match() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "hello");
        let wrapped = etag(inner);
        // First request gets the tag.
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        let tag = resp.headers.get("etag").unwrap().to_string();
        // Second request with If-None-Match.
        let mut r = req(Method::Get, "/x");
        r.headers.insert("if-none-match", &tag);
        let resp2 = wrapped.serve(&r, &Params::default());
        assert_eq!(resp2.status, StatusCode(304));
        assert!(resp2.body.is_empty());
    }

    // ----- Bearer auth -----

    #[test]
    fn bearer_auth_accepts_valid_token() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "secret");
        let wrapped = bearer_auth(
            "api",
            |t| {
                if t == "tok-abc" {
                    Ok(())
                } else {
                    Err("nope".into())
                }
            },
            inner,
        );
        let mut r = req(Method::Get, "/x");
        r.headers.insert("authorization", "Bearer tok-abc");
        let resp = wrapped.serve(&r, &Params::default());
        assert_eq!(resp.status, StatusCode(200));
    }

    #[test]
    fn bearer_auth_rejects_missing_token() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "secret");
        let wrapped = bearer_auth("api", |_| Ok(()), inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        assert_eq!(resp.status, StatusCode(401));
        assert!(
            resp.headers
                .get("www-authenticate")
                .unwrap_or("")
                .starts_with("Bearer")
        );
    }

    #[test]
    fn bearer_auth_rejects_wrong_token() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "secret");
        let wrapped = bearer_auth(
            "api",
            |t| {
                if t == "good" {
                    Ok(())
                } else {
                    Err("bad token".into())
                }
            },
            inner,
        );
        let mut r = req(Method::Get, "/x");
        r.headers.insert("authorization", "Bearer wrong");
        let resp = wrapped.serve(&r, &Params::default());
        assert_eq!(resp.status, StatusCode(401));
    }

    // ----- Rate limit -----

    #[test]
    fn rate_limit_allows_within_capacity_blocks_overflow() {
        let limiter = RateLimit::new(2, std::time::Duration::from_mins(1));
        let inner = |_req: &Request, _p: &Params| text_response(200, "ok");
        let wrapped = rate_limit(limiter, inner);
        let mut r = req(Method::Get, "/x");
        r.headers.insert("x-forwarded-for", "1.2.3.4");
        // first two pass
        assert_eq!(
            wrapped.serve(&r, &Params::default()).status,
            StatusCode(200)
        );
        assert_eq!(
            wrapped.serve(&r, &Params::default()).status,
            StatusCode(200)
        );
        // third gets 429
        assert_eq!(
            wrapped.serve(&r, &Params::default()).status,
            StatusCode(429)
        );
    }

    #[test]
    fn rate_limit_separates_keys() {
        let limiter = RateLimit::new(1, std::time::Duration::from_mins(1));
        let inner = |_req: &Request, _p: &Params| text_response(200, "ok");
        let wrapped = rate_limit(limiter, inner);
        let mut r1 = req(Method::Get, "/x");
        r1.headers.insert("x-forwarded-for", "1.1.1.1");
        let mut r2 = req(Method::Get, "/x");
        r2.headers.insert("x-forwarded-for", "2.2.2.2");
        assert_eq!(
            wrapped.serve(&r1, &Params::default()).status,
            StatusCode(200)
        );
        assert_eq!(
            wrapped.serve(&r2, &Params::default()).status,
            StatusCode(200)
        );
        // each key exhausted independently
        assert_eq!(
            wrapped.serve(&r1, &Params::default()).status,
            StatusCode(429)
        );
        assert_eq!(
            wrapped.serve(&r2, &Params::default()).status,
            StatusCode(429)
        );
    }

    // ----- Safe defaults composer -----

    #[test]
    fn safe_defaults_composes_chain() {
        let inner = |_req: &Request, _p: &Params| text_response(200, "ok");
        let wrapped = safe_defaults(inner);
        let resp = wrapped.serve(&req(Method::Get, "/x"), &Params::default());
        // request-id, security headers should be present.
        assert!(resp.headers.get("x-request-id").is_some());
        assert_eq!(resp.headers.get("x-content-type-options"), Some("nosniff"));
        assert_eq!(resp.headers.get("x-frame-options"), Some("DENY"));
    }
}

// --- Gossamer-facing middleware transforms ---------------------------
//
// The language surface composes middleware as handler handles carrying a
// transform selector plus one configuration string, and the transforms
// themselves live in `gossamer_runtime::c_abi::http_middleware` - the one
// place both the bytecode VM and the compiled tiers reach, so a change to
// a control cannot land on one tier and miss the other. Re-exported here
// under the names the interpreter and this module's own middleware use.

pub use gossamer_runtime::c_abi::http_middleware::{
    Before, RequestParts, ResponseParts, apply, apply_request, apply_with_request,
    cache_control_immutable_for, cache_control_no_store, cors_config, cors_permissive,
    hsts_safe_default, hsts_strict, middleware_kind as kind, rate_limit_allow, rate_limit_allow_at,
    rate_limit_config, rate_limit_reset, security_headers_off, security_headers_strict,
    sequential_request_id,
};

#[cfg(test)]
mod transform_tests {
    use super::*;

    fn parts(body: &str) -> ResponseParts {
        ResponseParts {
            status: 200,
            body: body.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    }

    #[test]
    fn cors_sets_every_configured_field() {
        let mut p = parts("ok");
        apply(
            kind::CORS,
            "https://app.test|GET, PUT|X-Api-Key|600",
            &mut p,
        );
        assert!(p.headers.contains(&(
            "Access-Control-Allow-Origin".to_string(),
            "https://app.test".to_string()
        )));
        assert!(
            p.headers
                .contains(&("Vary".to_string(), "Origin".to_string()))
        );
    }

    #[test]
    fn security_headers_off_preset_adds_nothing() {
        let mut p = parts("ok");
        apply(kind::SECURITY_HEADERS, "off", &mut p);
        assert!(p.headers.is_empty());
    }

    #[test]
    fn etag_is_stable_for_the_same_body() {
        let mut a = parts("payload");
        let mut b = parts("payload");
        apply(kind::ETAG, "", &mut a);
        apply(kind::ETAG, "", &mut b);
        assert_eq!(a.headers, b.headers);
    }

    #[test]
    fn rate_limit_rejects_past_capacity() {
        // Refill is one token a second, so the three back-to-back draws
        // see a budget that has not measurably grown.
        let config = "2|1";
        let client = "rate_limit_rejects_past_capacity";
        assert!(rate_limit_allow(config, client));
        assert!(rate_limit_allow(config, client));
        assert!(!rate_limit_allow(config, client));
    }

    #[test]
    fn rate_limit_buckets_are_per_client() {
        let config = "1|1";
        assert!(rate_limit_allow(config, "per_client_a"));
        assert!(!rate_limit_allow(config, "per_client_a"));
        // A second client starts with its own full budget.
        assert!(rate_limit_allow(config, "per_client_b"));
    }

    #[test]
    fn rate_limit_refills_over_time() {
        let config = "1|1000";
        let client = "rate_limit_refills_over_time";
        let start = gossamer_runtime::platform::Instant::now();
        assert!(rate_limit_allow_at(config, client, start));
        // A second draw at the same instant finds the bucket empty.
        assert!(!rate_limit_allow_at(config, client, start));
        // A thousand tokens a second puts one back after a millisecond.
        let later = start + std::time::Duration::from_millis(1);
        assert!(rate_limit_allow_at(config, client, later));
        assert!(!rate_limit_allow_at(config, client, later));
    }

    #[test]
    fn set_header_replaces_rather_than_duplicates() {
        let mut p = parts("ok");
        apply(kind::CACHE_CONTROL, "no-store", &mut p);
        apply(kind::CACHE_CONTROL, "max-age=60", &mut p);
        assert_eq!(p.headers.len(), 1);
        assert_eq!(p.headers[0].1, "max-age=60");
    }
}
