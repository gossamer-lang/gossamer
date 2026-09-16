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

use std::os::raw::c_char;
use std::sync::atomic::{AtomicI64, Ordering};

use super::*;

pub(crate) use crate::c_abi::vec::decode_header_tuple_vec;

// ---------------------------------------------------------------
// HTTP client shims for the compiled tiers - ureq-backed (TLS via
// rustls, cookies, redirects), mirroring the interp's
// `gossamer_std::http::Client` defaults so tier output matches.
// ---------------------------------------------------------------

/// Redirect/timeout/cookie/proxy policy carried by a configured
/// client. Defaults mirror `gossamer_std::http::Client::new()`
/// (10 redirects, 30 s, no persistent cookie jar, no proxy).
#[derive(Clone)]
pub struct ClientConfig {
    pub max_redirects: u32,
    pub timeout_ms: u64,
    /// When `true`, requests on this client reuse one persistent
    /// `ureq::Agent` so `Set-Cookie` survives across requests (the
    /// agent's jar is shared across its clones). When `false`, each
    /// request gets a fresh agent - no cookie carryover.
    pub cookie_jar: bool,
    /// Proxy URL (`http://host:port`, `socks5://...`) every request is
    /// routed through, or `None` for a direct connection.
    pub proxy: Option<String>,
}

impl ClientConfig {
    pub const DEFAULT: Self = Self {
        max_redirects: 10,
        timeout_ms: 30_000,
        cookie_jar: false,
        proxy: None,
    };
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Normalises a `max_redirects` setting: negatives clamp to 0
/// (never follow), values past `u32::MAX` clamp to `u32::MAX`.
fn clamp_max_redirects(n: i64) -> u32 {
    u32::try_from(n.max(0)).unwrap_or(u32::MAX)
}

/// Normalises a `timeout_ms` setting: zero or negative values fall
/// back to the 30 s default.
fn clamp_timeout_ms(t: i64) -> u64 {
    if t <= 0 {
        ClientConfig::DEFAULT.timeout_ms
    } else {
        t as u64
    }
}

pub struct GosHttpClient {
    pub config: ClientConfig,
    /// Persistent ureq engine built once at `build()`. Holds the
    /// cookie jar that survives across requests when
    /// `config.cookie_jar` is set, and carries the configured proxy /
    /// redirect / timeout policy.
    pub agent: ureq::Agent,
}

/// Configuration accumulator for `http::Client::builder()` chains.
/// Ownership: the pointer is consumed exactly once by
/// `gos_rt_http_client_builder_build`; the chainable setters mutate
/// in place and return the same pointer (no RC management, mirroring
/// `gos_rt_http_response_with_header`).
pub struct GosClientBuilder {
    pub config: ClientConfig,
}

pub struct GosHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Offset of the body region within `body`. The h1 server path
    /// (`parse_request_into`) stores the full raw request - header
    /// section + body - in `body` so headers can be scanned lazily,
    /// and points this at the first body byte. Every other
    /// constructor stores the body alone and leaves this 0.
    pub body_offset: usize,
    /// Router-captured path parameters (`/users/{id}` matched against
    /// `/users/42` yields `[("id", "42")]`). Empty unless the request
    /// was dispatched through a `Router` that matched a capture
    /// pattern; `gos_rt_router_serve` populates it before invoking the
    /// handler. Read back via `gos_rt_http_request_path_value`.
    pub params: Vec<(String, String)>,
    /// Request-scoped values (Go's `context.WithValue` analog).
    /// Attached by `gos_rt_http_request_set_value` (replace-then-push,
    /// the `set_header` pattern) and read back via
    /// `gos_rt_http_request_value`; empty until a handler sets one.
    pub values: Vec<(String, String)>,
    /// Agent a chained `client.<verb>(url)....send()` runs on, captured
    /// from the originating client so its cookie jar / proxy / policy
    /// apply. `None` for server-side requests and standalone pending
    /// requests, which fall back to the default-policy agent.
    pub agent: Option<ureq::Agent>,
    /// Request-scoped `context::Context`, as its pointer address, or 0
    /// when the request has none.
    ///
    /// The server creates one per request and cancels it when the request
    /// ends - whether the handler returned, the deadline elapsed, the peer
    /// disconnected, or the process began shutting down. Work a handler
    /// starts under it therefore stops when the request it belongs to is
    /// over, which is the guarantee a caller can act on.
    pub context: usize,
    /// Socket address of the peer that sent this request, `host:port`.
    /// Empty for a client-side request and for any server path that has
    /// no socket behind it. An access log, an IP allowlist, and a
    /// per-IP rate limit read it; a proxy-forwarded address is NOT
    /// substituted here, so a deployment behind a proxy resolves the
    /// forwarded chain itself against its own trusted-proxy list.
    pub peer: String,
}

/// Body slice of a request: past `body_offset` when the h1 server
/// stored the raw request buffer, the whole vec otherwise.
pub(crate) fn request_body_slice(req: &GosHttpRequest) -> &[u8] {
    &req.body[req.body_offset.min(req.body.len())..]
}

impl GosHttpRequest {
    /// Builds a request from h2's parsed `(method, path?query,
    /// headers, body)` tuple. Mirrors the manually-parsed form
    /// `parse_request_into` produces for the h1 path.
    #[must_use]
    pub fn for_h2(
        method: String,
        path_and_query: String,
        mut headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Self {
        // h2 names are already lowercase; sort + dedupe so the
        // header bag matches the interp `Headers` map view.
        super::http_server::normalize_header_bag(&mut headers);
        Self {
            method,
            url: path_and_query,
            headers,
            body,
            body_offset: 0,
            params: Vec::new(),
            values: Vec::new(),
            agent: None,
            peer: String::new(),
            context: 0,
        }
    }
}

pub struct GosHttpResponse {
    pub status: i64,
    pub body: SyncRawPtr<c_char>,
    pub headers: Vec<(String, String)>,
    /// Raw response bytes. Held so callers can access the binary
    /// payload (image downloads, gzipped content, etc.) via
    /// `resp.raw_bytes` without going through the UTF-8-lossy
    /// `body` field. `None` when the response came from a legacy
    /// constructor that didn't populate this - accessors should
    /// then fall back to the `body` c-string bytes.
    pub body_bytes: Option<Vec<u8>>,
    /// Content type recorded by the constructor; used by the server
    /// writer only when `headers` carries no explicit content-type.
    ///
    /// Borrowed for the constant kinds a handler answers with, so the two
    /// constructors a request path runs through allocate nothing for it.
    pub content_type: std::borrow::Cow<'static, str>,
    /// Stream-registry handle for streamed bodies; -1 = buffered.
    pub stream_handle: i64,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_new() -> *mut GosHttpClient {
    ffi_entry!(std::ptr::null_mut(), {
        let config = ClientConfig::DEFAULT;
        let agent = build_agent(&config);
        Box::into_raw(Box::new(GosHttpClient { config, agent }))
    })
}

/// `http::Client::builder() -> ClientBuilder` - starts a client
/// configuration chain with `Client::new()`'s defaults.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_builder_new() -> *mut GosClientBuilder {
    ffi_entry!(std::ptr::null_mut(), {
        Box::into_raw(Box::new(GosClientBuilder {
            config: ClientConfig::DEFAULT,
        }))
    })
}

/// `ClientBuilder::max_redirects(n) -> ClientBuilder` - mutates in
/// place and returns the same pointer (chainable). Negatives clamp
/// to 0 (never follow).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_builder_max_redirects(
    builder: *mut GosClientBuilder,
    n: i64,
) -> *mut GosClientBuilder {
    ffi_entry!(builder, {
        if !builder.is_null() {
            unsafe { (*builder).config.max_redirects = clamp_max_redirects(n) };
        }
        builder
    })
}

/// `ClientBuilder::timeout_ms(t) -> ClientBuilder` - mutates in
/// place and returns the same pointer (chainable). Non-positive
/// values fall back to the 30 s default.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_builder_timeout_ms(
    builder: *mut GosClientBuilder,
    t: i64,
) -> *mut GosClientBuilder {
    ffi_entry!(builder, {
        if !builder.is_null() {
            unsafe { (*builder).config.timeout_ms = clamp_timeout_ms(t) };
        }
        builder
    })
}

/// `ClientBuilder::cookie_jar(enabled) -> ClientBuilder` - toggles the
/// persistent cookie jar (chainable). When enabled, requests on the
/// built client reuse one agent so `Set-Cookie` survives across
/// requests; when disabled, every request gets a fresh agent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_builder_cookie_jar(
    builder: *mut GosClientBuilder,
    enabled: i32,
) -> *mut GosClientBuilder {
    ffi_entry!(builder, {
        if !builder.is_null() {
            unsafe { (*builder).config.cookie_jar = enabled != 0 };
        }
        builder
    })
}

/// `ClientBuilder::proxy(url) -> ClientBuilder` - routes every request
/// through `url` (chainable). An empty string clears the proxy.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_builder_proxy(
    builder: *mut GosClientBuilder,
    url: *const c_char,
) -> *mut GosClientBuilder {
    ffi_entry!(builder, {
        if !builder.is_null() {
            let proxy = if url.is_null() {
                None
            } else {
                let s = unsafe { crate::c_abi::gos_str_arg_string(url) };
                if s.is_empty() { None } else { Some(s) }
            };
            unsafe { (*builder).config.proxy = proxy };
        }
        builder
    })
}

/// `ClientBuilder::build() -> Client` - consumes the builder Box
/// (exactly once; the builder pointer is dead after this call) and
/// produces a configured client carrying a persistent ureq agent.
/// Like the legacy `gos_rt_http_client_new` allocation, the client
/// itself is never reclaimed by generated code: client locals are
/// opaque i64 handles with no drop registration, so both constructors
/// lean on process teardown (one allocation per built client).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_builder_build(
    builder: *mut GosClientBuilder,
) -> *mut GosHttpClient {
    ffi_entry!(std::ptr::null_mut(), {
        let config = if builder.is_null() {
            ClientConfig::DEFAULT
        } else {
            unsafe { Box::from_raw(builder) }.config
        };
        let agent = build_agent(&config);
        Box::into_raw(Box::new(GosHttpClient { config, agent }))
    })
}

/// Allocates the pending builder request the `client.<verb>(url)`
/// shims hand back for `.header(..)` / `.body(..)` / `.send()`
/// chaining. Captures the originating client's agent so the eventual
/// `.send()` honours its cookie jar / proxy / policy.
unsafe fn client_pending_request(
    method: &str,
    url: *const c_char,
    client: *const GosHttpClient,
) -> *mut GosHttpRequest {
    let url = if url.is_null() {
        String::new()
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(url) }
    };
    let agent = if client.is_null() {
        None
    } else {
        Some(client_agent(client))
    };
    Box::into_raw(Box::new(GosHttpRequest {
        method: method.to_string(),
        url,
        headers: Vec::new(),
        body: Vec::new(),
        body_offset: 0,
        params: Vec::new(),
        values: Vec::new(),
        agent,
        peer: String::new(),
        context: 0,
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_get(
    client: *mut GosHttpClient,
    url: *const c_char,
) -> *mut GosHttpRequest {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { client_pending_request("GET", url, client) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_post(
    client: *mut GosHttpClient,
    url: *const c_char,
) -> *mut GosHttpRequest {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { client_pending_request("POST", url, client) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_put(
    client: *mut GosHttpClient,
    url: *const c_char,
) -> *mut GosHttpRequest {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { client_pending_request("PUT", url, client) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_options(
    client: *mut GosHttpClient,
    url: *const c_char,
) -> *mut GosHttpRequest {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { client_pending_request("OPTIONS", url, client) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_delete(
    client: *mut GosHttpClient,
    url: *const c_char,
) -> *mut GosHttpRequest {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { client_pending_request("DELETE", url, client) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_head(
    client: *mut GosHttpClient,
    url: *const c_char,
) -> *mut GosHttpRequest {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { client_pending_request("HEAD", url, client) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_header(
    req: *mut GosHttpRequest,
    name: *const c_char,
    value: *const c_char,
) -> *mut GosHttpRequest {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() {
            return req;
        }
        let n = if name.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(name) }
        };
        let v = if value.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(value) }
        };
        unsafe { (*req).headers.push((n, v)) };
        req
    })
}

/// Mutating header insert used by the chained `req.headers.insert`
/// lowering (return-by-receiver kept off so the call has no value).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_set_header(
    req: *mut GosHttpRequest,
    name: *const c_char,
    value: *const c_char,
) {
    ffi_entry!((), {
        if req.is_null() {
            return;
        }
        let n = if name.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(name) }
        };
        let v = if value.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(value) }
        };
        let req = unsafe { &mut *req };
        req.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&n));
        req.headers.push((n, v));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_get_header(
    req: *const GosHttpRequest,
    name: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() || name.is_null() {
            return alloc_cstring(b"");
        }
        let n = unsafe { crate::c_abi::gos_str_arg_string(name) };
        let req = unsafe { &*req };
        let found = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&n))
            .map_or(String::new(), |(_, v)| v.clone());
        alloc_cstring(found.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_body(
    req: *mut GosHttpRequest,
    body: *const c_char,
) -> *mut GosHttpRequest {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() {
            return req;
        }
        let b = if body.is_null() {
            Vec::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(body) }.to_vec()
        };
        unsafe {
            (*req).body = b;
            (*req).body_offset = 0;
        }
        req
    })
}

/// `Request::send() -> Result<Response, errors::Error>` for the
/// chained builder (`client.post(url).header(..).body(..).send()`).
/// Consumes the request. Ok payload is a `*mut GosHttpResponse`;
/// transport failures pack the interp-matching error message so the
/// `.map_err(..)` / `?` surface behaves identically on every tier.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_send(req: *mut GosHttpRequest) -> i128 {
    ffi_entry!(0i128, {
        if req.is_null() {
            return err_result_with_msg("Request::send: request is null");
        }
        let GosHttpRequest {
            method,
            url,
            headers,
            body,
            body_offset: _,
            params: _,
            values: _,
            agent,
            peer: _,
            context: _,
        } = *unsafe { Box::from_raw(req) };
        // Reuse the originating client's agent (cookie jar / proxy /
        // policy) when this request came from `client.<verb>(url)`;
        // a standalone request uses a default-policy agent.
        let agent = agent.unwrap_or_else(default_agent);
        http_request_buffered("Request::send", &method, &url, body, &headers, &agent)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_query(req: *const GosHttpRequest) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() {
            return alloc_cstring(b"");
        }
        // Naive query extraction: everything after the first `?`
        // in the URL (without the leading `?`).
        let url = &unsafe { &*req }.url;
        if let Some(pos) = url.find('?') {
            alloc_cstring(&url.as_bytes()[pos + 1..])
        } else {
            alloc_cstring(b"")
        }
    })
}

/// `request.context` - the request-scoped context, cancelled when the
/// request ends.
///
/// A handler passes it to anything that should stop with the request: a
/// database query, an outbound call, a spawned worker. It answers a
/// background context when the request has none, so a caller never has to
/// branch on its absence.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_context(
    req: *mut GosHttpRequest,
) -> *mut crate::c_abi::context::GosCtx {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() {
            return unsafe { crate::c_abi::context::gos_rt_ctx_background() };
        }
        let request = unsafe { &mut *req };
        if request.context != 0 {
            return request.context as *mut crate::c_abi::context::GosCtx;
        }
        // A served request opens its context here, the first time its handler
        // asks for one: a handler that never does costs neither the allocation
        // nor the two turns of the live-request registry it would take.
        match crate::c_abi::http_server::open_current_request_context() {
            Some(ctx) => {
                request.context = ctx;
                ctx as *mut crate::c_abi::context::GosCtx
            }
            None => unsafe { crate::c_abi::context::gos_rt_ctx_background() },
        }
    })
}

/// `request.peer_addr` - the `host:port` of the peer that sent this
/// request, or `""` when there is no socket behind it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_peer_addr(req: *const GosHttpRequest) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() {
            return alloc_cstring(b"");
        }
        alloc_cstring(unsafe { &*req }.peer.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_body_str(req: *const GosHttpRequest) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() {
            return alloc_cstring(b"");
        }
        alloc_cstring(request_body_slice(unsafe { &*req }))
    })
}

/// Raw request body bytes (binary-safe counterpart of `body`).
/// Resolves the body region through `request_body_slice` - the
/// same split `gos_rt_http_request_body_str` uses - so the h1
/// lazy-buffer and h2/builder direct-body shapes both work. The
/// returned vec is the canonical packed `Vec<u8>` shape produced by
/// `bytes_to_gosvec`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_raw_body(
    req: *const GosHttpRequest,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let bytes: &[u8] = if req.is_null() {
            &[]
        } else {
            request_body_slice(unsafe { &*req })
        };
        super::encoding::bytes_to_gosvec(bytes)
    })
}

/// Returns the request's URL path (the part after the host).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_path(req: *const GosHttpRequest) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() {
            return alloc_cstring(b"");
        }
        let r = unsafe { &*req };
        let path = if let Some(rest) = r
            .url
            .strip_prefix("http://")
            .or_else(|| r.url.strip_prefix("https://"))
        {
            match rest.find('/') {
                Some(i) => &rest[i..],
                None => "/",
            }
        } else {
            r.url.as_str()
        };
        // The interp tier serves `path` query-stripped (Go's
        // `URL.Path`); the raw request-target keeps `?query`, so
        // cut it here for tier parity. `request.query` carries it.
        let path = match path.split_once('?') {
            Some((bare, _)) => bare,
            None => path,
        };
        alloc_cstring(path.as_bytes())
    })
}

/// Shared lookup for the router-captured path parameter `name`.
unsafe fn path_param_lookup(req: *const GosHttpRequest, name: *const c_char) -> Option<String> {
    if req.is_null() || name.is_null() {
        return None;
    }
    let wanted = unsafe { crate::c_abi::gos_str_arg_string(name) };
    let r = unsafe { &*req };
    r.params
        .iter()
        .find(|(k, _)| *k == wanted)
        .map(|(_, v)| v.clone())
}

/// `Request.path_value(name) -> String` - the router-captured path
/// parameter for `name`, or `""` when the request didn't match a
/// pattern carrying that capture. Mirrors Go's `http.Request.PathValue`.
/// Populated by `gos_rt_router_serve`; empty for a plain
/// `http::serve(addr, app)` that routes by hand.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_path_value(
    req: *const GosHttpRequest,
    name: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        match unsafe { path_param_lookup(req, name) } {
            Some(v) => alloc_cstring(v.as_bytes()),
            None => alloc_cstring(b""),
        }
    })
}

/// `Request.path_int(name) -> Option<i64>` - the captured path
/// parameter parsed as a base-10 integer (the typed extractor for
/// `/users/{id}` where `id` is numeric). `None` when the capture is
/// absent or not an integer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_path_int(
    req: *const GosHttpRequest,
    name: *const c_char,
) -> i128 {
    ffi_entry!(crate::c_abi::vec::gos_rt_result_new(1, 0), {
        match unsafe { path_param_lookup(req, name) }.and_then(|s| s.trim().parse::<i64>().ok()) {
            Some(n) => crate::c_abi::vec::gos_rt_result_new(0, n),
            None => crate::c_abi::vec::gos_rt_result_new(1, 0),
        }
    })
}

/// `Request.path_float(name) -> Option<f64>` - the captured path
/// parameter parsed as an `f64`. `None` when absent or unparseable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_path_float(
    req: *const GosHttpRequest,
    name: *const c_char,
) -> i128 {
    ffi_entry!(crate::c_abi::vec::gos_rt_result_new_f64(1, 0.0), {
        match unsafe { path_param_lookup(req, name) }.and_then(|s| s.trim().parse::<f64>().ok()) {
            Some(n) => crate::c_abi::vec::gos_rt_result_new_f64(0, n),
            None => crate::c_abi::vec::gos_rt_result_new_f64(1, 0.0),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_method(req: *const GosHttpRequest) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() {
            return alloc_cstring(b"");
        }
        alloc_cstring(unsafe { &*req }.method.as_bytes())
    })
}

/// Copies a borrowed body c-string into an owned gos-allocated copy (freed in
/// `drop_handler_result`); null passes through as null.
fn gos_response_own_body(body: *const c_char) -> *mut c_char {
    if body.is_null() {
        std::ptr::null_mut()
    } else {
        alloc_cstring(unsafe { crate::c_abi::gos_str_arg_bytes(body) })
    }
}

/// Reclaims a response box and the body copy it owns.
///
/// The unique reclaim site for a `http::Response` value: the server calls it
/// through `drop_handler_result` once a handler's answer has been written, and
/// compiled code calls it directly for a response that never leaves the frame
/// that built it. Null is a no-op, so a response moved out is reclaimed once,
/// by whoever ends up holding it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_free(response: *mut GosHttpResponse) {
    ffi_entry!((), {
        if response.is_null() {
            return;
        }
        unsafe { crate::c_abi::string::gos_rt_str_free((*response).body.as_ptr()) };
        drop(unsafe { Box::from_raw(response) });
    });
}

/// Box-allocates a `text/plain` response with the given status and an owned
/// copy of `body`; freed by `drop_handler_result`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_text_new(
    status: i64,
    body: *const c_char,
) -> *mut GosHttpResponse {
    ffi_entry!(std::ptr::null_mut(), {
        // Box-allocate per request rather than reusing a per-thread
        // buffer. The thread-local optimization saved a malloc/free
        // pair, but exposed a subtle aliasing hazard under concurrent
        // load: when many connection threads exit in rapid succession,
        // the TLS-owned `headers: Vec<(String, String)>` had its drop
        // path running concurrently with whatever code happened to be
        // using the response pointer. Switching to Box::into_raw +
        // Box::from_raw makes ownership explicit - `drop_handler_result`
        // is the unique reclaim site.
        Box::into_raw(Box::new(GosHttpResponse {
            status,
            body: SyncRawPtr::new(gos_response_own_body(body)),
            headers: Vec::new(),
            body_bytes: None,
            content_type: std::borrow::Cow::Borrowed("text/plain; charset=utf-8"),
            stream_handle: -1,
        }))
    })
}

/// Box-allocates an `application/json` response with the given status and an
/// owned copy of `body`; freed by `drop_handler_result`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_json_new(
    status: i64,
    body: *const c_char,
) -> *mut GosHttpResponse {
    ffi_entry!(std::ptr::null_mut(), {
        Box::into_raw(Box::new(GosHttpResponse {
            status,
            body: SyncRawPtr::new(gos_response_own_body(body)),
            headers: Vec::new(),
            body_bytes: None,
            content_type: std::borrow::Cow::Borrowed("application/json"),
            stream_handle: -1,
        }))
    })
}

/// `Response::stream(status, content_type, rs) -> Response` - wraps a
/// live `ResponseStream` so the server drains it to the client as
/// chunked frames (proxy passthrough). `rs` points at the 3-slot blob
/// `[handle, status, content_type]` from `gos_rt_http_stream`.
/// Construction CONSUMES the stream: the handle moves out of the
/// client registry, so a later `next_chunk` / `next_line` on the same
/// `ResponseStream` yields `None`, and a second serve of the same
/// handle answers an empty chunked body. Mirrors the interp tier's
/// `builtin_http_response_stream` exactly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_stream_new(
    status: i64,
    content_type: *const c_char,
    rs: *const i64,
) -> *mut GosHttpResponse {
    ffi_entry!(std::ptr::null_mut(), {
        let handle = if rs.is_null() { -1 } else { unsafe { *rs } };
        stream_consume_for_response(handle);
        let ct = if content_type.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(content_type) }
        };
        Box::into_raw(Box::new(GosHttpResponse {
            status,
            body: SyncRawPtr::new(std::ptr::null_mut()),
            headers: Vec::new(),
            body_bytes: None,
            content_type: std::borrow::Cow::Owned(ct),
            stream_handle: handle,
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_status(resp: *const GosHttpResponse) -> i64 {
    ffi_entry!(-1, {
        if resp.is_null() {
            return 0;
        }
        unsafe { (*resp).status }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_body(resp: *const GosHttpResponse) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if resp.is_null() {
            return alloc_cstring(b"");
        }
        unsafe { (*resp).body.as_ptr() }
    })
}

/// Returns the raw response bytes as a freshly allocated `Vec<u8>`
/// (`*mut GosVec`), preserving the exact transport bytes including
/// non-UTF-8 sequences. Mirrors the interp tier's `resp.raw_bytes`
/// field used by binary-download callers (image fetch, gzip body,
/// etc.). When the response was built without bytes (`text_new` /
/// `json_new`), falls back to the c-string bytes.
///
/// Representation contract: the returned vec is packed with
/// `elem_bytes = 1`, matching `bytes_to_gosvec`. Every
/// consumer must honor the stride: the codegen inline get/set
/// fast paths and the `gos_rt_vec_*` element helpers (first /
/// last / contains / count_of / index_of / loads / stores) are
/// stride-aware. Cross-tier coverage:
/// `feature-testing-examples/http_raw_bytes.gos`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_raw_bytes(
    resp: *const GosHttpResponse,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let bytes: &[u8] = if resp.is_null() {
            &[]
        } else if let Some(stored) = unsafe { &(*resp).body_bytes } {
            stored.as_slice()
        } else {
            let cstr_ptr = unsafe { (*resp).body.as_ptr() };
            if cstr_ptr.is_null() {
                &[]
            } else {
                unsafe { crate::c_abi::gos_str_arg_bytes(cstr_ptr) }
            }
        };
        // Allocate the GosVec with capacity for all bytes and write
        // them directly into the backing buffer. The previous
        // per-byte `gos_rt_vec_push` path went through the slow
        // growth loop and triggered the JIT-cached helper's
        // single-shot-byte memcpy - which on some lowering paths
        // received `&b as *const u8` from a transient stack frame
        // that was clobbered between iterations, producing a
        // truncated 2-byte vec. Bulk memcpy is also simpler.
        let len_i64 = bytes.len() as i64;
        let v = unsafe { crate::c_abi::vec::gos_rt_vec_with_capacity(1, len_i64) };
        if !bytes.is_empty() {
            let vec_ref = unsafe { &mut *v };
            if !vec_ref.ptr.is_null() {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        vec_ref.ptr.as_ptr(),
                        bytes.len(),
                    );
                }
                vec_ref.len = len_i64;
            }
        }
        v
    })
}

/// Returns the response's content type (`r.content_type`); empty
/// string when the upstream response carried no content-type header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_content_type(
    resp: *const GosHttpResponse,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if resp.is_null() {
            return alloc_cstring(b"");
        }
        alloc_cstring(unsafe { &(*resp).content_type }.as_bytes())
    })
}

/// Returns the response's `Location` header (`r.location`); empty
/// string when absent. Mirrors the interp tier's lifted `location`
/// field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_location(
    resp: *const GosHttpResponse,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if resp.is_null() {
            return alloc_cstring(b"");
        }
        let found = unsafe { &(*resp).headers }
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("location"))
            .map_or(String::new(), |(_, v)| v.clone());
        alloc_cstring(found.as_bytes())
    })
}

/// Slot layout of the header vecs returned by
/// [`gos_rt_http_response_headers`]: 16-byte `(name, value)` tuples whose
/// two words both own fresh c-strings, unconditionally.
static HEADER_SLOT_CHILDREN: [crate::c_abi::vec::VecSlotChild; 2] = [
    crate::c_abi::vec::VecSlotChild {
        gate: -1,
        disc_word: 0,
        word: 0,
        kind: crate::c_abi::vec::vec_elem_kind::STRING,
    },
    crate::c_abi::vec::VecSlotChild {
        gate: -1,
        disc_word: 0,
        word: 1,
        kind: crate::c_abi::vec::vec_elem_kind::STRING,
    },
];

/// Builds the `GosVec` of 16-byte `(*c_char, *c_char)` slots shared by
/// the request/response header accessors. The vec owns the slot
/// c-strings (consumer loops borrow them); `gos_rt_vec_free` deep-frees
/// every slot via the registered slot-children layout, including slots
/// an early `break` never reached.
fn header_pairs_to_gosvec(pairs: &[(String, String)]) -> *mut crate::c_abi::vec::GosVec {
    #[repr(C)]
    struct Pair {
        name: i64,
        value: i64,
    }
    let v = unsafe { crate::c_abi::vec::gos_rt_vec_with_capacity(16, pairs.len() as i64) };
    for (name, value) in pairs {
        let entry = Pair {
            name: alloc_cstring(name.as_bytes()) as i64,
            value: alloc_cstring(value.as_bytes()) as i64,
        };
        unsafe {
            crate::c_abi::vec::gos_rt_vec_push(v, std::ptr::addr_of!(entry).cast::<u8>());
        }
    }
    // Tagged after the pushes - the vec owns the fresh strings.
    crate::c_abi::vec::vec_set_slot_children(v, &HEADER_SLOT_CHILDREN);
    v
}

/// Returns upstream response headers as a `GosVec` of 16-byte `(*c_char, *c_char)` slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_headers(
    resp: *const GosHttpResponse,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let pairs: &[(String, String)] = if resp.is_null() {
            &[]
        } else {
            unsafe { &(*resp).headers }
        };
        header_pairs_to_gosvec(pairs)
    })
}

/// Returns inbound request headers as a `GosVec` of 16-byte `(*c_char, *c_char)` slots.
/// Names arrive lowercased, deduplicated, and name-sorted from
/// `parse_request_into`, matching the interp server's `Headers` map view.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_headers(
    req: *const GosHttpRequest,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let pairs: &[(String, String)] = if req.is_null() {
            &[]
        } else {
            unsafe { &(*req).headers }
        };
        header_pairs_to_gosvec(pairs)
    })
}

/// The request's query string parsed into decoded `(name, value)`
/// pairs, in the order they appear.
///
/// A field on the Gossamer side and a parse on this one: the query is
/// carried in the URL, so the pairs are derived on each read rather than
/// stored. `a=1&b`, and a repeated name, both answer what they say -
/// a bare name pairs with the empty string, and a repeat is a second
/// entry rather than a replacement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_query_pairs(
    req: *const GosHttpRequest,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() {
            return header_pairs_to_gosvec(&[]);
        }
        let url = &unsafe { &*req }.url;
        let query = url.find('?').map_or("", |pos| &url[pos + 1..]);
        let pairs = parse_query_pairs(query);
        header_pairs_to_gosvec(&pairs)
    })
}

/// Query-string parsing, shared with every target: the module holding it is
/// not gated on a native platform, so the bytecode VM, both compiled tiers,
/// and a wasm build all read a query with the same parser.
pub use super::http_query::{decode_query_component, parse_query_pairs};

/// Sets `Header: Value` on a response, replacing any prior value
/// with the same case-insensitive name. Used by the chained
/// `r.headers.insert(name, value)` lowering.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_set_header(
    resp: *mut GosHttpResponse,
    name: *const c_char,
    value: *const c_char,
) {
    ffi_entry!((), {
        if resp.is_null() {
            return;
        }
        let n = if name.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(name) }
        };
        let v = if value.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(value) }
        };
        let resp = unsafe { &mut *resp };
        resp.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&n));
        resp.headers.push((n, v));
    });
}

/// Chainable header attach for `resp.with_header(name, value)`.
/// Replace-then-push (same semantics as
/// [`gos_rt_http_response_set_header`]): a prior header with the
/// same case-insensitive name is removed, then the new pair is
/// appended. Returns `resp` itself - no new allocation - so chains
/// keep mutating the single boxed response the constructor minted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_with_header(
    resp: *mut GosHttpResponse,
    name: *const c_char,
    value: *const c_char,
) -> *mut GosHttpResponse {
    ffi_entry!(resp, {
        unsafe { gos_rt_http_response_set_header(resp, name, value) };
        resp
    })
}

/// Overrides the constructor-recorded content type. Emitted by the
/// `http::Response { content_type: … }` struct-literal lowering on the
/// compiled tiers; the server writers fall back to this value when the
/// header list carries no explicit content-type (same precedence as
/// the interp tier's `value_to_response`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_set_content_type(
    resp: *mut GosHttpResponse,
    content_type: *const c_char,
) {
    ffi_entry!((), {
        if resp.is_null() || content_type.is_null() {
            return;
        }
        let ct = unsafe { crate::c_abi::gos_str_arg_string(content_type) };
        unsafe { (*resp).content_type = ct.into() };
    });
}

/// Replaces the response body with the bytes of a `GosVec`. Emitted by
/// the `http::Response { body: [104u8, …] }` struct-literal lowering on
/// the compiled tiers (the interp's `value_to_response` accepts byte
/// arrays as bodies). Stride-aware: a packed `elem_bytes == 1` vec is
/// copied directly; the canonical i64-per-slot layout narrows each slot
/// to its low byte, matching the interp's u8 narrowing. Updates both
/// the c-string `body` (read by the server writers) and `body_bytes`
/// (read by `.raw_bytes`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_set_body_bytes(
    resp: *mut GosHttpResponse,
    bytes: *const crate::c_abi::vec::GosVec,
) {
    ffi_entry!((), {
        if resp.is_null() {
            return;
        }
        let collected: Vec<u8> = if bytes.is_null() {
            Vec::new()
        } else {
            let v = unsafe { &*bytes };
            let len = usize::try_from(v.len).unwrap_or(0);
            if v.ptr.is_null() || len == 0 {
                Vec::new()
            } else if v.elem_bytes == 1 {
                unsafe { std::slice::from_raw_parts(v.ptr.as_ptr(), len) }.to_vec()
            } else {
                let stride = v.elem_bytes as usize;
                (0..len)
                    .map(|i| unsafe { *v.ptr.as_ptr().add(i * stride) })
                    .collect()
            }
        };
        let r = unsafe { &mut *resp };
        let old = r.body.as_ptr();
        if !old.is_null() {
            unsafe { crate::c_abi::string::gos_rt_str_free(old) };
        }
        r.body = SyncRawPtr::new(alloc_cstring(&collected));
        r.body_bytes = Some(collected);
    });
}

/// Reads `Header` value from a response, empty string when absent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_get_header(
    resp: *const GosHttpResponse,
    name: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if resp.is_null() || name.is_null() {
            return alloc_cstring(b"");
        }
        let n = unsafe { crate::c_abi::gos_str_arg_string(name) };
        let resp = unsafe { &*resp };
        let found = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&n))
            .map_or(String::new(), |(_, v)| v.clone());
        alloc_cstring(found.as_bytes())
    })
}

// ---------------------------------------------------------------
// http::stream - POST/GET that returns a line-by-line body reader
// keyed by an integer handle. Mirrors the interp's
// `builtin_http_stream` shape: the call returns a
// `Result<ResponseStream, errors::Error>` whose Ok payload is a
// 3-slot heap aggregate `[__handle: i64, status: i64, content_type:
// *c_char]`. Subsequent `.next_line()` calls dispatch to
// `gos_rt_http_stream_next_line`.
//
// Implementation: the wire reader stays open across FFI calls by
// living inside a process-global `Mutex<HashMap<i64, BufReader>>`
// keyed by the handle stashed in the ResponseStream blob.
// `next_line` calls `read_line` on the held reader so SSE bodies
// stream live (askq's `[thinking…]` dots arrive token-by-token
// rather than after the full LLM completion buffers up).
// ---------------------------------------------------------------

pub(crate) type StreamReader = std::io::BufReader<Box<dyn std::io::Read + Send + Sync>>;

static STREAM_REGISTRY: parking_lot::Mutex<
    Option<rustc_hash::FxHashMap<i64, std::sync::Arc<parking_lot::Mutex<StreamReader>>>>,
> = parking_lot::Mutex::new(None);
static NEXT_STREAM_HANDLE: AtomicI64 = AtomicI64::new(1);

pub(crate) fn stream_registry_register(reader: StreamReader) -> i64 {
    let handle = NEXT_STREAM_HANDLE.fetch_add(1, Ordering::SeqCst);
    let mut guard = STREAM_REGISTRY.lock();
    let map = guard.get_or_insert_with(rustc_hash::FxHashMap::default);
    map.insert(handle, std::sync::Arc::new(parking_lot::Mutex::new(reader)));
    handle
}

fn stream_registry_lookup(handle: i64) -> Option<std::sync::Arc<parking_lot::Mutex<StreamReader>>> {
    let guard = STREAM_REGISTRY.lock();
    guard.as_ref()?.get(&handle).cloned()
}

fn stream_registry_drop(handle: i64) {
    let mut guard = STREAM_REGISTRY.lock();
    if let Some(map) = guard.as_mut() {
        map.remove(&handle);
    }
}

/// Streams already claimed by a `Response::stream(...)` value and
/// waiting to be drained to a client by the server writer. Keyed by
/// the original registry handle; `stream_take_for_serve` is one-shot.
static PENDING_SERVE: parking_lot::Mutex<
    Option<rustc_hash::FxHashMap<i64, std::sync::Arc<parking_lot::Mutex<StreamReader>>>>,
> = parking_lot::Mutex::new(None);

/// Moves `handle` from the client registry into the pending-serve
/// registry. After this, `next_line` / `next_chunk` on the same
/// `ResponseStream` return `None` - the stream now belongs to the
/// response. No-op when the handle was already consumed. Mirrors the
/// interp tier's `stream_consume_for_response` exactly.
pub(crate) fn stream_consume_for_response(handle: i64) {
    let taken = {
        let mut guard = STREAM_REGISTRY.lock();
        guard.as_mut().and_then(|map| map.remove(&handle))
    };
    if let Some(arc) = taken {
        let mut pending = PENDING_SERVE.lock();
        pending
            .get_or_insert_with(rustc_hash::FxHashMap::default)
            .insert(handle, arc);
    }
}

/// Takes the pending stream for `handle` - one-shot, so serving the
/// same streamed response twice drains an empty body the second time.
pub(crate) fn stream_take_for_serve(
    handle: i64,
) -> Option<std::sync::Arc<parking_lot::Mutex<StreamReader>>> {
    let mut pending = PENDING_SERVE.lock();
    pending.as_mut()?.remove(&handle)
}

/// Builds a 3-slot ResponseStream blob `[__handle, status,
/// content_type]`. Field order matches `stdlib_struct_shapes`.
/// Box-allocated so the pointer outlives any LLVM
/// `arena_save`/`arena_restore` window the caller's compiled code
/// emits - see fix_architecture_ownership.md Stage 4.
pub(crate) fn alloc_response_stream_blob_public(
    handle: i64,
    status: i64,
    content_type: &str,
) -> *mut i64 {
    alloc_response_stream_blob(handle, status, content_type)
}

fn alloc_response_stream_blob(handle: i64, status: i64, content_type: &str) -> *mut i64 {
    let ct_cs = alloc_cstring(content_type.as_bytes()) as i64;
    Box::into_raw(Box::new([handle, status, ct_cs])).cast::<i64>()
}

fn err_result_with_msg(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    gos_rt_result_new(1, err as i64)
}

/// Method whitelist mirroring `gossamer_std::http::Method::parse`
/// (the runtime crate cannot depend on gossamer-std, so the
/// case-insensitive set is replicated here).
fn validate_http_method(method: &str) -> Option<String> {
    let upper = method.to_ascii_uppercase();
    match upper.as_str() {
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS" => Some(upper),
        _ => None,
    }
}

/// Builds a fresh ureq engine from a client config. Settings mirror
/// `gossamer_std::http::Client::new()` defaults (global timeout, 10
/// redirects, `gossamer/{version}` UA, non-2xx are Ok responses -
/// matching the interp tier's `http_status_as_error(false)`), plus the
/// configured proxy. An agent owns a connection pool and a cookie jar,
/// both shared across its clones, so one is built per client (and once
/// for the free functions) and reused for every request that client
/// makes; a client without a cookie jar empties the jar before each
/// request instead of building another engine.
fn build_agent(config: &ClientConfig) -> ureq::Agent {
    // Redirect semantics (ureq 3, will_error left at its default):
    // `max_redirects(0)` never follows and returns 3xx raw; exceeding
    // a non-zero budget is a "too many redirects" transport error -
    // matching `gossamer_std::http::ClientBuilder`.
    let mut cfg = ureq::config::Config::builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_millis(config.timeout_ms)))
        .max_redirects(config.max_redirects)
        .user_agent(concat!("gossamer/", env!("CARGO_PKG_VERSION")));
    if let Some(proxy_url) = &config.proxy {
        // A malformed proxy URL drops to a direct connection rather
        // than panicking; the interp tier surfaces the parse error at
        // build time, but the request path stays infallible here.
        if let Ok(proxy) = ureq::Proxy::new(proxy_url) {
            cfg = cfg.proxy(Some(proxy));
        }
    }
    cfg.build().new_agent()
}

/// The engine every free-function request runs on.
///
/// One agent for the process is one connection pool: a second request
/// to a host it already reached rides the connection the first opened
/// instead of paying another TCP (and TLS) handshake, which is what the
/// interp tier's native client does and what an idle keep-alive
/// connection is for. Opening a fresh connection per request also
/// consumes an ephemeral port per request, and a host that cannot
/// assign one refuses the connect outright.
///
/// The free functions carry no cookie jar, so the jar is emptied before
/// the request is built: nothing a previous response stored is ever
/// sent.
fn default_agent() -> ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    let agent = AGENT
        .get_or_init(|| build_agent(&ClientConfig::DEFAULT))
        .clone();
    agent.cookie_jar_lock().clear();
    agent
}

/// The agent a client request runs on: the client's own persistent
/// engine, so its connection pool, proxy, redirect and timeout policy
/// apply. A client without a cookie jar starts each request from an
/// empty one, which keeps `Set-Cookie` from carrying across requests
/// while still reusing the connection. A null client gets the
/// default-policy agent.
fn client_agent(client: *const GosHttpClient) -> ureq::Agent {
    if client.is_null() {
        return default_agent();
    }
    let c = unsafe { &*client };
    let agent = c.agent.clone();
    if !c.config.cookie_jar {
        agent.cookie_jar_lock().clear();
    }
    agent
}

/// Upper bound on a buffered response body (post-inflate) for the
/// compiled HTTP client. Matches the interp native client's
/// `max_body_bytes` default (gossamer-std/src/http_native_client.rs) so
/// both tiers reject the same decompression-bomb / oversized-response
/// shape.
const MAX_RESPONSE_BODY_BYTES: u64 = 32 * 1024 * 1024;
/// A watchdog signal is sampled roughly every five milliseconds.  Keep this
/// bounded budget long enough to cross several signal ticks without turning a
/// permanently interrupted transport into an unbounded retry loop.
const EINTR_IDEMPOTENT_RETRIES: usize = 64;

/// A watchdog SIGURG can interrupt a blocking libc/transport call even when
/// the signal handler uses `SA_RESTART`; not every operation performed by a
/// TLS or HTTP stack is restartable by the kernel.  Retrying an idempotent
/// request is safe and prevents scheduler preemption from surfacing as a
/// spurious application-level transport failure.  Never apply this to POST
/// or PATCH: an interrupted send may already have reached the peer.
fn retries_interrupted_request(method: &str, error: &ureq::Error) -> bool {
    matches!(method, "GET" | "HEAD" | "PUT" | "DELETE" | "OPTIONS")
        && matches!(error, ureq::Error::Io(e) if e.kind() == std::io::ErrorKind::Interrupted)
}

/// Reads at most `cap` bytes from `reader`, erroring rather than
/// truncating when the stream would exceed it. Reading one byte past
/// the cap distinguishes "exactly at cap" from "over cap".
fn read_body_capped(reader: impl std::io::Read, cap: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buf: Vec<u8> = Vec::new();
    let mut limited = reader.take(cap + 1);
    limited
        .read_to_end(&mut buf)
        .map_err(|e| format!("http: io: {e}"))?;
    if buf.len() as u64 > cap {
        return Err(format!("http: response body exceeds {cap}-byte cap"));
    }
    Ok(buf)
}

/// Runs a buffered request on `agent` and lifts the response into a
/// fully populated `GosHttpResponse` (status, body, headers,
/// body_bytes, content_type, stream_handle = -1). Shared by every
/// buffered client entry point.
fn http_response_with_agent(
    label: &str,
    method: &str,
    url: &str,
    body: Vec<u8>,
    header_pairs: &[(String, String)],
    agent: &ureq::Agent,
) -> Result<*mut GosHttpResponse, String> {
    let label = label.to_string();
    let method = method.to_string();
    let url = url.to_string();
    let headers = header_pairs.to_vec();
    let agent = agent.clone();
    let response = crate::sched_global::run_blocking("http-client", move || {
        run_buffered_request(&label, &method, &url, body, &headers, &agent)
    })??;
    Ok(Box::into_raw(Box::new(GosHttpResponse {
        status: response.status,
        body: SyncRawPtr::new(alloc_cstring(response.body.as_slice())),
        headers: response.headers,
        body_bytes: Some(response.body),
        content_type: response.content_type.into(),
        stream_handle: -1,
    })))
}

/// Owned wire response transferred back from a scheduler blocking worker.
/// Keeping it free of raw runtime pointers makes the handoff `Send` and keeps
/// allocation ownership on the parked caller's coroutine.
struct BufferedResponse {
    status: i64,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    content_type: String,
}

fn run_buffered_request(
    label: &str,
    method: &str,
    url: &str,
    body: Vec<u8>,
    header_pairs: &[(String, String)],
    agent: &ureq::Agent,
) -> Result<BufferedResponse, String> {
    let Some(method_upper) = validate_http_method(method) else {
        return Err(format!("{label}: unknown method `{method}`"));
    };
    // Error strings mirror `gossamer_std::http::ClientError`'s
    // Display ("http: transport: ..." / "http: io: ...") so the
    // interp tier and the compiled tiers report byte-identical
    // failure messages for the same failure class.
    let mut retries_left = EINTR_IDEMPOTENT_RETRIES;
    let resp = loop {
        let mut builder = ureq::http::Request::builder()
            .method(method_upper.as_str())
            .uri(url);
        for (k, v) in header_pairs {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let request = builder
            .body(body.clone())
            .map_err(|e| format!("http: transport: {e}"))?;
        match agent.run(request) {
            Ok(response) => break response,
            Err(error)
                if retries_left > 0
                    && retries_interrupted_request(method_upper.as_str(), &error) =>
            {
                retries_left -= 1;
                // Let the just-interrupted syscall and signal-dispatch work
                // settle before rebuilding the request. A yield alone often
                // lands directly in the watchdog's next five-millisecond
                // SIGURG tick and repeats the same failure.
                crate::platform::sleep(std::time::Duration::from_millis(1));
            }
            Err(error) => return Err(format!("http: transport: {error}")),
        }
    };
    let status = i64::from(resp.status().as_u16());
    let mut hdrs: Vec<(String, String)> = Vec::new();
    for (name, value) in resp.headers() {
        // Lifted header names are lowercase on every tier: the interp
        // client lifts from the lowercase-keyed `Headers` map, and the
        // `http` crate already normalizes - the explicit lowercase
        // keeps the invariant local rather than inherited.
        hdrs.push((
            name.as_str().to_ascii_lowercase(),
            value.to_str().unwrap_or("").to_string(),
        ));
    }
    // gzip auto-inflate is on, so the reader yields decompressed bytes;
    // capping it here bounds the post-inflate size and defeats a
    // "decompression bomb" that expands to gigabytes from a small
    // compressed payload.
    let body = read_body_capped(resp.into_body().into_reader(), MAX_RESPONSE_BODY_BYTES)?;
    // "text/plain" fallback matches the interp tier's `lift_response`
    // so `.content_type` agrees across tiers when the header is absent.
    let content_type = hdrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map_or_else(|| "text/plain".to_string(), |(_, v)| v.clone());
    Ok(BufferedResponse {
        status,
        body,
        headers: hdrs,
        content_type,
    })
}

/// Packs the shared engine's result into the i128 `Result<Response,
/// errors::Error>` convention used by the free-function shims.
fn http_request_buffered(
    label: &str,
    method: &str,
    url: &str,
    body: Vec<u8>,
    header_pairs: &[(String, String)],
    agent: &ureq::Agent,
) -> i128 {
    match http_response_with_agent(label, method, url, body, header_pairs, agent) {
        Ok(resp) => gos_rt_result_new(0, resp as i64),
        Err(msg) => err_result_with_msg(&msg),
    }
}

/// `http::request(method, url, body, headers) -> Result<Response, errors::Error>`.
/// Buffered full-method client entry for the compiled tiers. `body`
/// is the request body c-string (null or empty = no body); `headers`
/// is a `Vec<(String, String)>` of 16-byte tuple slots. Ok payload is
/// a `*mut GosHttpResponse` so field access routes through the
/// existing `gos_rt_http_response_*` dispatch.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request(
    method: *const c_char,
    url: *const c_char,
    body: *const c_char,
    headers: *mut GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        let method_str = if method.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(method) }
        };
        let url_str = if url.is_null() {
            return err_result_with_msg("http::request: url is null");
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(url) }
        };
        let body_bytes = if body.is_null() {
            Vec::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(body) }.to_vec()
        };
        let header_pairs = decode_header_tuple_vec(headers);
        http_request_buffered(
            "http::request",
            &method_str,
            &url_str,
            body_bytes,
            &header_pairs,
            &default_agent(),
        )
    })
}

/// `http::request_bytes(method, url, body: [u8], headers) -> Result<Response, errors::Error>`.
/// Binary-body sibling of `gos_rt_http_request`: `body` is a byte
/// `GosVec` (null or empty = no body) so upload payloads with NULs
/// or non-UTF-8 bytes survive intact.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_bytes(
    method: *const c_char,
    url: *const c_char,
    body: *const crate::c_abi::vec::GosVec,
    headers: *mut GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        let method_str = if method.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(method) }
        };
        let url_str = if url.is_null() {
            return err_result_with_msg("http::request_bytes: url is null");
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(url) }
        };
        let body_bytes = unsafe { super::encoding::gosvec_u8(body) };
        let header_pairs = decode_header_tuple_vec(headers);
        http_request_buffered(
            "http::request_bytes",
            &method_str,
            &url_str,
            body_bytes,
            &header_pairs,
            &default_agent(),
        )
    })
}

/// `Client::request(method, url, body, headers) -> Result<Response, errors::Error>`.
/// Same semantics and error strings as the free `gos_rt_http_request`
/// except the receiver's configured redirect/timeout policy is
/// honored: `max_redirects(0)` returns 3xx raw, exceeding a non-zero
/// budget is a "too many redirects" transport error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_request(
    client: *const GosHttpClient,
    method: *const c_char,
    url: *const c_char,
    body: *const c_char,
    headers: *mut GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        let method_str = if method.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(method) }
        };
        let url_str = if url.is_null() {
            return err_result_with_msg("Client::request: url is null");
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(url) }
        };
        let body_bytes = if body.is_null() {
            Vec::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(body) }.to_vec()
        };
        let header_pairs = decode_header_tuple_vec(headers);
        http_request_buffered(
            "Client::request",
            &method_str,
            &url_str,
            body_bytes,
            &header_pairs,
            &client_agent(client),
        )
    })
}

/// `Client::request_bytes(method, url, body: [u8], headers) -> Result<Response, errors::Error>`.
/// Binary-body sibling of `gos_rt_http_client_request`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_client_request_bytes(
    client: *const GosHttpClient,
    method: *const c_char,
    url: *const c_char,
    body: *const crate::c_abi::vec::GosVec,
    headers: *mut GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        let method_str = if method.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(method) }
        };
        let url_str = if url.is_null() {
            return err_result_with_msg("Client::request_bytes: url is null");
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(url) }
        };
        let body_bytes = unsafe { super::encoding::gosvec_u8(body) };
        let header_pairs = decode_header_tuple_vec(headers);
        http_request_buffered(
            "Client::request_bytes",
            &method_str,
            &url_str,
            body_bytes,
            &header_pairs,
            &client_agent(client),
        )
    })
}

/// `http::get(url, headers) -> Result<http::Response, errors::Error>`.
/// One-shot GET. Ok payload is a `*mut GosHttpResponse` so field
/// access (`r.status`, `r.body`) routes through the existing
/// `gos_rt_http_response_*` dispatch.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_http_get(url: *const c_char, headers: *mut GosVec) -> i128 {
    ffi_entry!(0i128, {
        let url_str = if url.is_null() {
            return unsafe { err_result_with_msg("http::get: url is null") };
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(url) }
        };
        let header_pairs = decode_header_tuple_vec(headers);
        http_request_buffered(
            "http::get",
            "GET",
            &url_str,
            Vec::new(),
            &header_pairs,
            &default_agent(),
        )
    })
}

/// One-shot bodyless verb (`HEAD` / `OPTIONS`) shim. `url` + `headers`
/// match `gos_rt_http_get`; `method`/`label` select the verb.
fn http_verb_no_body(method: &str, label: &str, url: *const c_char, headers: *mut GosVec) -> i128 {
    let url_str = if url.is_null() {
        return unsafe { err_result_with_msg(&format!("{label}: url is null")) };
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(url) }
    };
    let header_pairs = decode_header_tuple_vec(headers);
    http_request_buffered(
        label,
        method,
        &url_str,
        Vec::new(),
        &header_pairs,
        &default_agent(),
    )
}

/// One-shot body verb (`POST` / `PUT`) shim. `body` is the request
/// body c-string, `content_type` becomes the `Content-Type` header.
fn http_verb_body(
    method: &str,
    label: &str,
    url: *const c_char,
    body: *const c_char,
    content_type: *const c_char,
) -> i128 {
    let url_str = if url.is_null() {
        return unsafe { err_result_with_msg(&format!("{label}: url is null")) };
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(url) }
    };
    let body_bytes = if body.is_null() {
        Vec::new()
    } else {
        unsafe { crate::c_abi::gos_str_arg_bytes(body) }.to_vec()
    };
    let ct = if content_type.is_null() {
        String::new()
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(content_type) }
    };
    let header_pairs = vec![("Content-Type".to_string(), ct)];
    http_request_buffered(
        label,
        method,
        &url_str,
        body_bytes,
        &header_pairs,
        &default_agent(),
    )
}

/// `http::head(url, headers) -> Result<http::Response, errors::Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_http_head(url: *const c_char, headers: *mut GosVec) -> i128 {
    ffi_entry!(0i128, {
        http_verb_no_body("HEAD", "http::head", url, headers)
    })
}

/// `http::options(url, headers) -> Result<http::Response, errors::Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_http_options(url: *const c_char, headers: *mut GosVec) -> i128 {
    ffi_entry!(0i128, {
        http_verb_no_body("OPTIONS", "http::options", url, headers)
    })
}

/// `http::post(url, body, content_type) -> Result<http::Response, errors::Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_http_post(
    url: *const c_char,
    body: *const c_char,
    content_type: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        http_verb_body("POST", "http::post", url, body, content_type)
    })
}

/// `http::put(url, body, content_type) -> Result<http::Response, errors::Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_http_put(
    url: *const c_char,
    body: *const c_char,
    content_type: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        http_verb_body("PUT", "http::put", url, body, content_type)
    })
}

/// `http::delete(url, body, headers) -> Result<http::Response, errors::Error>`.
/// `body` is the request body c-string (null or empty = no body);
/// `headers` is a `Vec<(String, String)>` of 16-byte tuple slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_delete(
    url: *const c_char,
    body: *const c_char,
    headers: *mut GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        let url_str = if url.is_null() {
            return err_result_with_msg("http::delete: url is null");
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(url) }
        };
        let body_bytes = if body.is_null() {
            Vec::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(body) }.to_vec()
        };
        let header_pairs = decode_header_tuple_vec(headers);
        http_request_buffered(
            "http::delete",
            "DELETE",
            &url_str,
            body_bytes,
            &header_pairs,
            &default_agent(),
        )
    })
}

/// `native_client::get(url) -> Result<Response, errors::Error>` -
/// one-shot GET with no extra headers (the bare `NativeClient` helper).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_nc_get(url: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        http_verb_no_body("GET", "native_client::get", url, std::ptr::null_mut())
    })
}

/// `native_client::delete(url) -> Result<Response, errors::Error>` -
/// one-shot DELETE with no body and no extra headers.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_nc_delete(url: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        http_verb_no_body("DELETE", "native_client::delete", url, std::ptr::null_mut())
    })
}

/// `application/octet-stream` when `content_type` is null/empty,
/// matching the interp `NativeClient` body-verb default.
fn nc_content_type(content_type: *const c_char) -> String {
    let ct = if content_type.is_null() {
        String::new()
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(content_type) }
    };
    if ct.is_empty() {
        "application/octet-stream".to_string()
    } else {
        ct
    }
}

/// `native_client::post(url, body, content_type) -> Result<Response, errors::Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_nc_post(
    url: *const c_char,
    body: *const c_char,
    content_type: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        let url_str = if url.is_null() {
            return unsafe { err_result_with_msg("native_client::post: url is null") };
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(url) }
        };
        let body_bytes = if body.is_null() {
            Vec::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(body) }.to_vec()
        };
        let header_pairs = vec![("Content-Type".to_string(), nc_content_type(content_type))];
        http_request_buffered(
            "native_client::post",
            "POST",
            &url_str,
            body_bytes,
            &header_pairs,
            &default_agent(),
        )
    })
}

/// `native_client::put(url, body, content_type) -> Result<Response, errors::Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_nc_put(
    url: *const c_char,
    body: *const c_char,
    content_type: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        let url_str = if url.is_null() {
            return unsafe { err_result_with_msg("native_client::put: url is null") };
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(url) }
        };
        let body_bytes = if body.is_null() {
            Vec::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(body) }.to_vec()
        };
        let header_pairs = vec![("Content-Type".to_string(), nc_content_type(content_type))];
        http_request_buffered(
            "native_client::put",
            "PUT",
            &url_str,
            body_bytes,
            &header_pairs,
            &default_agent(),
        )
    })
}

/// `proxy::forward(upstream_url, method, body) -> Result<Response, errors::Error>`.
/// One-shot upstream request: GET / DELETE ignore the body; POST / PUT
/// send it with `application/octet-stream`; unknown methods fall back to
/// GET. Mirrors the interp `proxy::forward` over a fresh `NativeClient`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_proxy_forward_url(
    upstream_url: *const c_char,
    method: *const c_char,
    body: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        let url_str = if upstream_url.is_null() {
            return unsafe { err_result_with_msg("proxy::forward: url is null") };
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(upstream_url) }
        };
        let method_str = if method.is_null() {
            "GET".to_string()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(method) }
        };
        let body_bytes = if body.is_null() {
            Vec::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(body) }.to_vec()
        };
        let agent = default_agent();
        match method_str.to_ascii_uppercase().as_str() {
            "POST" => http_request_buffered(
                "proxy::forward",
                "POST",
                &url_str,
                body_bytes,
                &[(
                    "Content-Type".to_string(),
                    "application/octet-stream".to_string(),
                )],
                &agent,
            ),
            "PUT" => http_request_buffered(
                "proxy::forward",
                "PUT",
                &url_str,
                body_bytes,
                &[(
                    "Content-Type".to_string(),
                    "application/octet-stream".to_string(),
                )],
                &agent,
            ),
            "DELETE" => http_request_buffered(
                "proxy::forward",
                "DELETE",
                &url_str,
                Vec::new(),
                &[],
                &agent,
            ),
            _ => http_request_buffered("proxy::forward", "GET", &url_str, Vec::new(), &[], &agent),
        }
    })
}

/// `http::stream(method, url, body, headers) -> Result<ResponseStream, errors::Error>`.
///
/// `headers` is a `Vec<(String, String)>` whose backing storage is
/// a tight array of 16-byte tuples `(*c_char, *c_char)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_stream(
    method: *const c_char,
    url: *const c_char,
    body: *const c_char,
    headers: *mut GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        let method_str = if method.is_null() {
            "GET".to_string()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(method) }
        };
        let url_str = if url.is_null() {
            return unsafe { err_result_with_msg("http::stream: url is null") };
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(url) }
        };
        let body_str = if body.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(body) }
        };
        let header_pairs = decode_header_tuple_vec(headers);

        // Build an agent with no read timeout - SSE / chunked
        // chat-completion bodies can have multi-second gaps between
        // tokens (askq's reasoning phase) and the default 30s read
        // timeout would tear the connection mid-stream.
        // http_status_as_error(false) so 4xx/5xx bodies are surfaced
        // to the caller as a live ResponseStream rather than dropped.
        let agent = ureq::config::Config::builder()
            .http_status_as_error(false)
            .timeout_connect(Some(std::time::Duration::from_secs(30)))
            .build()
            .new_agent();
        let mut builder = ureq::http::Request::builder()
            .method(method_str.as_str())
            .uri(url_str.as_str());
        for (k, v) in &header_pairs {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let body_bytes = if body_str.is_empty() {
            Vec::new()
        } else {
            body_str.into_bytes()
        };
        let request = match builder.body(body_bytes) {
            Ok(r) => r,
            Err(e) => {
                return unsafe {
                    err_result_with_msg(&format!("http::stream: build request: {e}"))
                };
            }
        };
        let resp =
            match crate::sched_global::run_blocking("http-stream", move || agent.run(request)) {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return unsafe { err_result_with_msg(&format!("http::stream: {e}")) },
                Err(e) => return unsafe { err_result_with_msg(&format!("http::stream: {e}")) },
            };
        let status = i64::from(resp.status().as_u16());
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/plain")
            .to_string();
        let reader = std::io::BufReader::new(
            Box::new(resp.into_body().into_reader()) as Box<dyn std::io::Read + Send + Sync>
        );
        let handle = stream_registry_register(reader);
        let blob = unsafe { alloc_response_stream_blob(handle, status, &content_type) };
        if blob.is_null() {
            stream_registry_drop(handle);
            return unsafe { err_result_with_msg("http::stream: arena alloc failed") };
        }
        unsafe { gos_rt_result_new(0, blob as i64) }
    })
}

/// `ResponseStream::next_line() -> Option<String>`.
///
/// `rs` points at the 3-slot blob `[handle, status, content_type]`
/// returned by `gos_rt_http_stream`. Returns a `*mut GosResult`
/// shaped as `Option<String>` (disc 0 = Some, 1 = None). EOF or
/// I/O failure drops the stream from the registry and returns
/// None.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_stream_next_line(rs: *const i64) -> i128 {
    ffi_entry!(0i128, {
        if rs.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let handle = unsafe { *rs };
        let Some(arc) = stream_registry_lookup(handle) else {
            return unsafe { gos_rt_result_new(1, 0) };
        };
        use std::io::BufRead;
        let mut buf = String::new();
        let read_result = arc.lock().read_line(&mut buf);
        match read_result {
            Ok(0) => {
                stream_registry_drop(handle);
                unsafe { gos_rt_result_new(1, 0) }
            }
            Ok(_) => {
                if buf.ends_with('\n') {
                    buf.pop();
                    if buf.ends_with('\r') {
                        buf.pop();
                    }
                }
                let cs = alloc_cstring(buf.as_bytes()) as i64;
                unsafe { gos_rt_result_new(0, cs) }
            }
            Err(_) => {
                stream_registry_drop(handle);
                unsafe { gos_rt_result_new(1, 0) }
            }
        }
    })
}

/// `ResponseStream::next_chunk(max_bytes) -> Option<[u8]>`.
///
/// `rs` points at the 3-slot blob `[handle, status, content_type]`
/// returned by `gos_rt_http_stream`. Returns the packed-i128 Option
/// (disc 0 = Some, 1 = None); EOF or I/O failure drops the stream
/// from the registry and returns None.
///
/// Representation contract: the Some payload is a PACKED
/// `elem_bytes = 1` `GosVec` - one byte per element, matching
/// `gos_rt_http_response_raw_bytes`; every consumer op (indexing,
/// for-loop, len, hex::encode, …) is stride-aware. `max_bytes` is
/// clamped to 1..=1 MiB, mirroring the interp tier's
/// `StreamResponse::next_chunk`. Reads come from the same
/// registry `BufReader` as `next_line`, so interleaving line and
/// chunk reads on one stream stays coherent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_stream_next_chunk(rs: *const i64, max_bytes: i64) -> i128 {
    ffi_entry!(0i128, {
        if rs.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let handle = unsafe { *rs };
        let Some(arc) = stream_registry_lookup(handle) else {
            return unsafe { gos_rt_result_new(1, 0) };
        };
        let cap = usize::try_from(max_bytes.clamp(1, 1 << 20)).unwrap_or(1);
        let mut buf = vec![0u8; cap];
        let read_result = {
            use std::io::Read;
            arc.lock().read(&mut buf)
        };
        match read_result {
            Ok(0) => {
                stream_registry_drop(handle);
                unsafe { gos_rt_result_new(1, 0) }
            }
            Ok(n) => {
                let v = unsafe { crate::c_abi::vec::gos_rt_vec_with_capacity(1, n as i64) };
                if !v.is_null() {
                    let vec_ref = unsafe { &mut *v };
                    if !vec_ref.ptr.is_null() {
                        unsafe {
                            std::ptr::copy_nonoverlapping(buf.as_ptr(), vec_ref.ptr.as_ptr(), n);
                        }
                        vec_ref.len = n as i64;
                    }
                }
                unsafe { gos_rt_result_new(0, v as i64) }
            }
            Err(_) => {
                stream_registry_drop(handle);
                unsafe { gos_rt_result_new(1, 0) }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn only_idempotent_requests_retry_interrupted_transport() {
        let interrupted = ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::Interrupted));
        assert!(retries_interrupted_request("GET", &interrupted));
        assert!(retries_interrupted_request("PUT", &interrupted));
        assert!(!retries_interrupted_request("POST", &interrupted));

        let refused = ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert!(!retries_interrupted_request("GET", &refused));
    }

    #[test]
    fn response_body_over_cap_is_rejected() {
        // An endless (decompressed) stream must be refused, not slurped.
        let bomb = std::io::repeat(0xABu8);
        let err = read_body_capped(bomb, 1024).unwrap_err();
        assert!(err.contains("exceeds"), "got: {err}");
    }

    #[test]
    fn response_body_within_cap_is_returned() {
        let ok = std::io::Cursor::new(vec![0u8; 512]);
        let body = read_body_capped(ok, 1024).unwrap();
        assert_eq!(body.len(), 512);
    }

    #[test]
    fn response_body_exactly_at_cap_is_returned() {
        let ok = std::io::Cursor::new(vec![0u8; 1024]);
        let body = read_body_capped(ok, 1024).unwrap();
        assert_eq!(body.len(), 1024);
    }

    #[test]
    fn response_headers_materialises_tuple_vec_of_cstring_pairs() {
        let resp = Box::into_raw(Box::new(GosHttpResponse {
            status: 200,
            body: SyncRawPtr::new(alloc_cstring(b"ok")),
            headers: vec![
                ("content-type".to_string(), "text/plain".to_string()),
                ("x-request-id".to_string(), "abc123".to_string()),
            ],
            body_bytes: None,
            content_type: "text/plain".into(),
            stream_handle: -1,
        }));
        let v = unsafe { gos_rt_http_response_headers(resp) };
        assert!(!v.is_null());
        let vec_ref = unsafe { &*v };
        assert_eq!(vec_ref.len, 2);
        assert_eq!(vec_ref.elem_bytes, 16);
        // The vec owns the slot strings (AGGR_OWNED slot-children
        // layout); reads below are borrows and `gos_rt_vec_free`
        // reclaims every slot string.
        assert_eq!(
            vec_ref.elem_kind,
            crate::c_abi::vec::vec_elem_kind::AGGR_OWNED
        );
        let expected = [("content-type", "text/plain"), ("x-request-id", "abc123")];
        for (i, (name, value)) in expected.iter().enumerate() {
            let slot = unsafe { vec_ref.ptr.add(i * 16) };
            // Slots hold cstring pointers exposed as integers by the
            // flat-slot ABI; recover provenance before use.
            let name_ptr = unsafe { crate::c_abi::vec::slot_read_word(slot) }.cast::<c_char>();
            let value_ptr =
                unsafe { crate::c_abi::vec::slot_read_word(slot.add(8)) }.cast::<c_char>();
            let got_name = unsafe { CStr::from_ptr(name_ptr) }.to_str().unwrap();
            let got_value = unsafe { CStr::from_ptr(value_ptr) }.to_str().unwrap();
            assert_eq!(got_name, *name);
            assert_eq!(got_value, *value);
        }
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
        let resp_box = unsafe { Box::from_raw(resp) };
        unsafe { crate::c_abi::string::gos_rt_str_free(resp_box.body.as_ptr()) };
    }

    #[test]
    fn response_headers_on_null_response_returns_empty_vec() {
        let v = unsafe { gos_rt_http_response_headers(std::ptr::null()) };
        assert!(!v.is_null());
        assert_eq!(unsafe { (*v).len }, 0);
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
    }

    #[test]
    fn request_raw_body_returns_h2_direct_body_verbatim() {
        let payload = [0x68u8, 0xFF, 0x00, 0x69];
        let req = GosHttpRequest::for_h2(
            "POST".to_string(),
            "/upload".to_string(),
            Vec::new(),
            payload.to_vec(),
        );
        let v = unsafe { gos_rt_http_request_raw_body(std::ptr::from_ref(&req)) };
        assert!(!v.is_null());
        let vec_ref = unsafe { &*v };
        assert_eq!(vec_ref.len, 4);
        assert_eq!(vec_ref.elem_bytes, 1);
        let got = unsafe { std::slice::from_raw_parts(vec_ref.ptr.as_ptr(), 4) };
        assert_eq!(got, &[0x68, 0xFF, 0x00, 0x69]);
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
    }

    #[test]
    fn request_raw_body_on_null_request_returns_empty_vec() {
        let v = unsafe { gos_rt_http_request_raw_body(std::ptr::null()) };
        assert!(!v.is_null());
        assert_eq!(unsafe { (*v).len }, 0);
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
    }

    #[test]
    fn request_headers_returns_owned_tuple_vec() {
        let req = GosHttpRequest {
            method: "GET".to_string(),
            url: "/".to_string(),
            headers: vec![
                ("accept".to_string(), "*/*".to_string()),
                ("x-token".to_string(), "t1".to_string()),
            ],
            body: Vec::new(),
            body_offset: 0,
            params: Vec::new(),
            values: Vec::new(),
            agent: None,
            peer: String::new(),
            context: 0,
        };
        let v = unsafe { gos_rt_http_request_headers(std::ptr::from_ref(&req)) };
        assert!(!v.is_null());
        let vec_ref = unsafe { &*v };
        assert_eq!(vec_ref.len, 2);
        assert_eq!(vec_ref.elem_bytes, 16);
        assert_eq!(
            vec_ref.elem_kind,
            crate::c_abi::vec::vec_elem_kind::AGGR_OWNED
        );
        let expected = [("accept", "*/*"), ("x-token", "t1")];
        for (i, (name, value)) in expected.iter().enumerate() {
            let slot = unsafe { vec_ref.ptr.add(i * 16) };
            // Slots hold cstring pointers exposed as integers by the
            // flat-slot ABI; recover provenance before use.
            let name_ptr = unsafe { crate::c_abi::vec::slot_read_word(slot) }.cast::<c_char>();
            let value_ptr =
                unsafe { crate::c_abi::vec::slot_read_word(slot.add(8)) }.cast::<c_char>();
            assert_eq!(unsafe { CStr::from_ptr(name_ptr) }.to_str().unwrap(), *name);
            assert_eq!(
                unsafe { CStr::from_ptr(value_ptr) }.to_str().unwrap(),
                *value
            );
        }
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
    }

    #[test]
    fn request_headers_on_null_request_returns_empty_vec() {
        let v = unsafe { gos_rt_http_request_headers(std::ptr::null()) };
        assert!(!v.is_null());
        assert_eq!(unsafe { (*v).len }, 0);
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
    }

    #[test]
    fn request_path_strips_query_component() {
        let mk = |url: &str| GosHttpRequest {
            method: "GET".to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            body_offset: 0,
            params: Vec::new(),
            values: Vec::new(),
            agent: None,
            peer: String::new(),
            context: 0,
        };
        let cases = [
            ("/a?x=1", "/a"),
            ("/a", "/a"),
            ("http://h.example/a/b?x=1&y=2", "/a/b"),
            ("/", "/"),
        ];
        for (url, want) in cases {
            let req = mk(url);
            let p = unsafe { gos_rt_http_request_path(std::ptr::from_ref(&req)) };
            assert_eq!(
                unsafe { CStr::from_ptr(p) }.to_str().unwrap(),
                want,
                "path of {url}"
            );
            unsafe { crate::c_abi::string::gos_rt_str_free(p) };
        }
    }

    /// Serves one canned HTTP/1.1 response on a loopback listener and
    /// returns `(url, join_handle)`; the handle yields the raw request
    /// bytes the server received (headers + body).
    fn spawn_one_shot_server(
        status_line: &str,
        response_headers: &str,
        response_body: &[u8],
    ) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status_line = status_line.to_string();
        let response_headers = response_headers.to_string();
        let response_body = response_body.to_vec();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request: Vec<u8> = Vec::new();
            let mut buf = [0u8; 1024];
            let body_len = loop {
                let n = stream.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..n]);
                if let Some(split) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..split]).into_owned();
                    let content_length = head
                        .lines()
                        .find_map(|l| {
                            let (name, value) = l.split_once(':')?;
                            name.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    break split + 4 + content_length;
                }
            };
            while request.len() < body_len {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            let response = format!(
                "{status_line}\r\n{response_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&response_body).unwrap();
            request
        });
        (format!("http://{addr}/echo"), handle)
    }

    fn header_tuple_vec(pairs: &[(&str, &str)]) -> *mut GosVec {
        #[repr(C)]
        struct Pair {
            name: i64,
            value: i64,
        }
        let v = unsafe { crate::c_abi::vec::gos_rt_vec_with_capacity(16, pairs.len() as i64) };
        for (name, value) in pairs {
            let entry = Pair {
                name: alloc_cstring(name.as_bytes()) as i64,
                value: alloc_cstring(value.as_bytes()) as i64,
            };
            unsafe {
                crate::c_abi::vec::gos_rt_vec_push(v, std::ptr::addr_of!(entry).cast::<u8>());
            }
        }
        v
    }

    fn free_header_tuple_vec(v: *mut GosVec) {
        let vec_ref = unsafe { &*v };
        for i in 0..vec_ref.len as usize {
            let slot = unsafe { vec_ref.ptr.add(i * 16) };
            // Slots hold cstring pointers exposed as integers by the
            // flat-slot ABI; recover provenance before use.
            let name_ptr = unsafe { crate::c_abi::vec::slot_read_word(slot) }.cast::<c_char>();
            let value_ptr =
                unsafe { crate::c_abi::vec::slot_read_word(slot.add(8)) }.cast::<c_char>();
            unsafe {
                crate::c_abi::string::gos_rt_str_free(name_ptr);
                crate::c_abi::string::gos_rt_str_free(value_ptr);
            }
        }
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
    }

    fn free_response(resp: *mut GosHttpResponse) {
        let resp_box = unsafe { Box::from_raw(resp) };
        unsafe { crate::c_abi::string::gos_rt_str_free(resp_box.body.as_ptr()) };
    }

    /// Opens a stream against a one-shot server and returns the
    /// ResponseStream blob pointer (slot 0 = registry handle).
    fn open_stream(url: &str) -> *mut i64 {
        let method = std::ffi::CString::new("GET").unwrap();
        let url_cs = std::ffi::CString::new(url).unwrap();
        let body = std::ffi::CString::new("").unwrap();
        let packed = unsafe {
            gos_rt_http_stream(
                crate::c_abi::string::test_gos_ptr(&method),
                crate::c_abi::string::test_gos_ptr(&url_cs),
                crate::c_abi::string::test_gos_ptr(&body),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(gos_rt_result_disc(packed), 0, "stream open must succeed");
        let blob = gos_rt_result_payload(packed) as *mut i64;
        assert!(!blob.is_null());
        blob
    }

    fn next_chunk_bytes(blob: *const i64, max: i64) -> Option<Vec<u8>> {
        let packed = unsafe { gos_rt_http_stream_next_chunk(blob, max) };
        if gos_rt_result_disc(packed) != 0 {
            return None;
        }
        let v = gos_rt_result_payload(packed) as *mut GosVec;
        assert!(!v.is_null());
        let vec_ref = unsafe { &*v };
        assert_eq!(
            vec_ref.elem_bytes, 1,
            "next_chunk payload must be the packed elem_bytes=1 byte-vec shape"
        );
        let out = unsafe {
            std::slice::from_raw_parts(vec_ref.ptr.as_ptr(), vec_ref.len as usize).to_vec()
        };
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
        Some(out)
    }

    /// A second request to a host the client already reached rides the
    /// connection the first one opened. Opening a fresh one per request
    /// pays another handshake and another ephemeral port, and a host with
    /// none to assign refuses the connect outright.
    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn a_second_request_to_the_same_host_rides_the_first_connection() {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                counter.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().expect("clone the accepted stream");
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    // Every request head ends at a blank line and carries
                    // no body, so one answer per blank line serves the
                    // whole connection.
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n"
                            && writer
                                .write_all(
                                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                                )
                                .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });

        let url = format!("http://{addr}/x");
        for _ in 0..2 {
            let response =
                run_buffered_request("test", "GET", &url, Vec::new(), &[], &default_agent())
                    .expect("buffered response");
            assert_eq!(response.status, 200);
            assert_eq!(response.body, b"ok");
        }
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "the second request opened another connection"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn buffered_response_lifts_mixed_case_header_names_lowercase() {
        let (url, server) = spawn_one_shot_server("HTTP/1.1 200 OK", "X-MiXeD-CaSe: v\r\n", b"ok");
        let resp = http_response_with_agent(
            "test",
            "GET",
            &url,
            Vec::new(),
            &[],
            &build_agent(&ClientConfig::DEFAULT),
        )
        .expect("buffered response");
        let resp_ref = unsafe { &*resp };
        assert!(
            resp_ref
                .headers
                .iter()
                .any(|(k, v)| k == "x-mixed-case" && v == "v"),
            "headers: {:?}",
            resp_ref.headers
        );
        assert!(
            resp_ref
                .headers
                .iter()
                .all(|(k, _)| *k == k.to_ascii_lowercase()),
            "every lifted name is lowercase: {:?}",
            resp_ref.headers
        );
        free_response(resp);
        let _ = server.join();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn stream_next_chunk_drains_body_in_max_byte_chunks_then_eof() {
        // 10 bytes with values above 0x7F to catch sign-extension /
        // stride bugs: "Aÿ™zABC" = 41 C3 BF E2 84 A2 7A 41 42 43.
        let body: &[u8] = &[0x41, 0xC3, 0xBF, 0xE2, 0x84, 0xA2, 0x7A, 0x41, 0x42, 0x43];
        let (url, server) = spawn_one_shot_server("HTTP/1.1 200 OK", "", body);
        let blob = open_stream(&url);
        assert_eq!(
            next_chunk_bytes(blob, 4).as_deref(),
            Some(&body[0..4]),
            "first chunk"
        );
        assert_eq!(
            next_chunk_bytes(blob, 4).as_deref(),
            Some(&body[4..8]),
            "second chunk"
        );
        assert_eq!(
            next_chunk_bytes(blob, 4).as_deref(),
            Some(&body[8..10]),
            "tail chunk"
        );
        assert_eq!(next_chunk_bytes(blob, 4), None, "EOF must be None");
        // EOF dropped the stream from the registry; further calls
        // keep returning None instead of erroring.
        assert_eq!(next_chunk_bytes(blob, 4), None);
        unsafe { crate::c_abi::string::gos_rt_str_free((*blob.add(2)) as *mut c_char) };
        drop(unsafe { Box::from_raw(blob.cast::<[i64; 3]>()) });
        server.join().unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn stream_next_chunk_interleaves_coherently_with_next_line() {
        let (url, server) = spawn_one_shot_server("HTTP/1.1 200 OK", "", b"alpha\nbeta!");
        let blob = open_stream(&url);
        let packed = unsafe { gos_rt_http_stream_next_line(blob) };
        assert_eq!(gos_rt_result_disc(packed), 0);
        let line = gos_rt_result_payload(packed) as *mut c_char;
        assert_eq!(unsafe { CStr::from_ptr(line) }.to_str().unwrap(), "alpha");
        unsafe { crate::c_abi::string::gos_rt_str_free(line) };
        assert_eq!(next_chunk_bytes(blob, 4).as_deref(), Some(&b"beta"[..]));
        assert_eq!(next_chunk_bytes(blob, 4).as_deref(), Some(&b"!"[..]));
        assert_eq!(next_chunk_bytes(blob, 4), None);
        unsafe { crate::c_abi::string::gos_rt_str_free((*blob.add(2)) as *mut c_char) };
        drop(unsafe { Box::from_raw(blob.cast::<[i64; 3]>()) });
        server.join().unwrap();
    }

    #[test]
    fn stream_next_chunk_on_null_or_stale_handle_returns_none() {
        let packed = unsafe { gos_rt_http_stream_next_chunk(std::ptr::null(), 4) };
        assert_eq!(gos_rt_result_disc(packed), 1);
        let stale = [-7i64, 200, 0];
        let packed = unsafe { gos_rt_http_stream_next_chunk(stale.as_ptr(), 4) };
        assert_eq!(gos_rt_result_disc(packed), 1);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn response_stream_new_consumes_handle_and_pending_serve_is_one_shot() {
        let (url, server) = spawn_one_shot_server("HTTP/1.1 200 OK", "", b"proxied body");
        let blob = open_stream(&url);
        let handle = unsafe { *blob };
        let ct = std::ffi::CString::new("text/plain").unwrap();
        let resp = unsafe {
            gos_rt_http_response_stream_new(200, crate::c_abi::string::test_gos_ptr(&ct), blob)
        };
        assert!(!resp.is_null());
        assert_eq!(unsafe { (*resp).stream_handle }, handle);
        assert!(
            unsafe { (*resp).body.as_ptr() }.is_null(),
            "streamed body has no c-string"
        );
        // Construction consumed the client registry entry: the
        // ResponseStream's own readers now yield None - identical to
        // the interp tier's consume semantics.
        assert_eq!(
            next_chunk_bytes(blob, 64),
            None,
            "next_chunk after Response::stream must be None"
        );
        // The pending-serve registry hands the live reader out once.
        let arc = stream_take_for_serve(handle).expect("pending stream present");
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut *arc.lock(), &mut body).unwrap();
        assert_eq!(body, b"proxied body");
        assert!(
            stream_take_for_serve(handle).is_none(),
            "a second serve of the same handle must drain nothing"
        );
        drop(unsafe { Box::from_raw(resp) });
        unsafe { crate::c_abi::string::gos_rt_str_free((*blob.add(2)) as *mut c_char) };
        drop(unsafe { Box::from_raw(blob.cast::<[i64; 3]>()) });
        server.join().unwrap();
    }

    #[test]
    fn response_stream_new_on_null_or_stale_blob_yields_dead_handle() {
        let ct = std::ffi::CString::new("text/plain").unwrap();
        let resp = unsafe {
            gos_rt_http_response_stream_new(
                200,
                crate::c_abi::string::test_gos_ptr(&ct),
                std::ptr::null(),
            )
        };
        assert!(!resp.is_null());
        assert_eq!(
            unsafe { (*resp).stream_handle },
            -1,
            "null blob marks buffered"
        );
        drop(unsafe { Box::from_raw(resp) });

        let stale = [-7i64, 200, 0];
        let resp = unsafe {
            gos_rt_http_response_stream_new(
                200,
                crate::c_abi::string::test_gos_ptr(&ct),
                stale.as_ptr(),
            )
        };
        assert_eq!(unsafe { (*resp).stream_handle }, -7);
        assert!(
            stream_take_for_serve(-7).is_none(),
            "stale handle never lands in the pending registry"
        );
        drop(unsafe { Box::from_raw(resp) });
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn http_request_round_trips_method_body_headers_and_response_fields() {
        let (url, server) = spawn_one_shot_server(
            "HTTP/1.1 201 Created",
            "Content-Type: application/json\r\nX-Req-Id: t1\r\n",
            b"{\"ok\":true}",
        );
        let method = std::ffi::CString::new("post").unwrap();
        let url_cs = std::ffi::CString::new(url).unwrap();
        let body = std::ffi::CString::new("hi there").unwrap();
        let headers = header_tuple_vec(&[("x-test", "yes")]);
        let packed = unsafe {
            gos_rt_http_request(
                crate::c_abi::string::test_gos_ptr(&method),
                crate::c_abi::string::test_gos_ptr(&url_cs),
                crate::c_abi::string::test_gos_ptr(&body),
                headers,
            )
        };
        free_header_tuple_vec(headers);
        assert_eq!(gos_rt_result_disc(packed), 0);
        let resp = gos_rt_result_payload(packed) as *mut GosHttpResponse;
        assert!(!resp.is_null());
        let resp_ref = unsafe { &*resp };
        assert_eq!(resp_ref.status, 201);
        let got_body = unsafe { CStr::from_ptr(resp_ref.body.as_ptr()) }
            .to_str()
            .unwrap();
        assert_eq!(got_body, "{\"ok\":true}");
        assert_eq!(
            resp_ref.body_bytes.as_deref(),
            Some(b"{\"ok\":true}".as_slice())
        );
        assert_eq!(resp_ref.content_type, "application/json");
        assert_eq!(resp_ref.stream_handle, -1);
        assert!(
            resp_ref
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-req-id") && v == "t1")
        );
        let ct = unsafe { gos_rt_http_response_content_type(resp) };
        assert_eq!(
            unsafe { CStr::from_ptr(ct) }.to_str().unwrap(),
            "application/json"
        );
        unsafe { crate::c_abi::string::gos_rt_str_free(ct) };
        let loc = unsafe { gos_rt_http_response_location(resp) };
        assert_eq!(unsafe { CStr::from_ptr(loc) }.to_str().unwrap(), "");
        unsafe { crate::c_abi::string::gos_rt_str_free(loc) };
        free_response(resp);

        let request = server.join().unwrap();
        let request_text = String::from_utf8_lossy(&request).into_owned();
        assert!(
            request_text.starts_with("POST /echo HTTP/1.1"),
            "lowercase method must be sent uppercased: {request_text}"
        );
        assert!(request_text.to_ascii_lowercase().contains("x-test: yes"));
        assert!(request_text.ends_with("hi there"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn http_request_bytes_preserves_binary_upload_body() {
        let (url, server) = spawn_one_shot_server("HTTP/1.1 200 OK", "", b"ok");
        let method = std::ffi::CString::new("PUT").unwrap();
        let url_cs = std::ffi::CString::new(url).unwrap();
        let payload: &[u8] = &[104, 105, 0, 255];
        let body = unsafe { crate::c_abi::vec::gos_rt_vec_with_capacity(1, payload.len() as i64) };
        for b in payload {
            unsafe { crate::c_abi::vec::gos_rt_vec_push(body, std::ptr::from_ref(b)) };
        }
        let packed = unsafe {
            gos_rt_http_request_bytes(
                crate::c_abi::string::test_gos_ptr(&method),
                crate::c_abi::string::test_gos_ptr(&url_cs),
                body,
                std::ptr::null_mut(),
            )
        };
        unsafe { crate::c_abi::map::gos_rt_vec_free(body) };
        assert_eq!(gos_rt_result_disc(packed), 0);
        let resp = gos_rt_result_payload(packed) as *mut GosHttpResponse;
        assert_eq!(unsafe { (*resp).status }, 200);
        // No Content-Type in the canned response: the fallback must
        // match the interp tier's "text/plain" default.
        assert_eq!(unsafe { &(*resp).content_type }, "text/plain");
        free_response(resp);

        let request = server.join().unwrap();
        let body_start = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert_eq!(&request[body_start..], payload);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn http_request_send_posts_builder_body_and_returns_real_status() {
        let (url, server) = spawn_one_shot_server(
            "HTTP/1.1 202 Accepted",
            "Content-Type: text/plain\r\nX-Srv: builder\r\n",
            b"accepted",
        );
        let client = unsafe { gos_rt_http_client_new() };
        let url_cs = std::ffi::CString::new(url).unwrap();
        let req =
            unsafe { gos_rt_http_client_post(client, crate::c_abi::string::test_gos_ptr(&url_cs)) };
        assert!(!req.is_null());
        let hdr_name = std::ffi::CString::new("x-builder").unwrap();
        let hdr_value = std::ffi::CString::new("yes").unwrap();
        let req = unsafe {
            gos_rt_http_request_header(
                req,
                crate::c_abi::string::test_gos_ptr(&hdr_name),
                crate::c_abi::string::test_gos_ptr(&hdr_value),
            )
        };
        let body_cs = std::ffi::CString::new("payload!").unwrap();
        let req =
            unsafe { gos_rt_http_request_body(req, crate::c_abi::string::test_gos_ptr(&body_cs)) };
        let packed = unsafe { gos_rt_http_request_send(req) };
        assert_eq!(gos_rt_result_disc(packed), 0);
        let resp = gos_rt_result_payload(packed) as *mut GosHttpResponse;
        assert!(!resp.is_null());
        let resp_ref = unsafe { &*resp };
        assert_eq!(resp_ref.status, 202);
        let got_body = unsafe { CStr::from_ptr(resp_ref.body.as_ptr()) }
            .to_str()
            .unwrap();
        assert_eq!(got_body, "accepted");
        assert_eq!(resp_ref.content_type, "text/plain");
        assert!(
            resp_ref
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-srv") && v == "builder")
        );
        free_response(resp);
        drop(unsafe { Box::from_raw(client) });

        let request = server.join().unwrap();
        let request_text = String::from_utf8_lossy(&request).into_owned();
        assert!(
            request_text.starts_with("POST /echo HTTP/1.1"),
            "builder .send() must issue a POST: {request_text}"
        );
        assert!(request_text.to_ascii_lowercase().contains("x-builder: yes"));
        assert!(request_text.ends_with("payload!"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn http_request_send_get_follows_engine_and_reports_real_status() {
        let (url, server) = spawn_one_shot_server(
            "HTTP/1.1 404 Not Found",
            "Content-Type: text/plain\r\n",
            b"missing",
        );
        let client = unsafe { gos_rt_http_client_new() };
        let url_cs = std::ffi::CString::new(url).unwrap();
        let req =
            unsafe { gos_rt_http_client_get(client, crate::c_abi::string::test_gos_ptr(&url_cs)) };
        let packed = unsafe { gos_rt_http_request_send(req) };
        assert_eq!(gos_rt_result_disc(packed), 0);
        let resp = gos_rt_result_payload(packed) as *mut GosHttpResponse;
        assert!(!resp.is_null());
        let resp_ref = unsafe { &*resp };
        // The legacy hand-rolled GET path hardcoded 200 for TLS and
        // dropped headers; the shared engine must surface the real
        // status and the response headers.
        assert_eq!(resp_ref.status, 404);
        assert!(
            resp_ref
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v == "text/plain")
        );
        free_response(resp);
        drop(unsafe { Box::from_raw(client) });
        server.join().unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn duplicate_set_cookie_headers_survive_in_wire_order() {
        // RFC 6265 servers legally repeat `Set-Cookie`; the lifted
        // header sequence must keep both pairs in wire order so a
        // proxy can forward every cookie. Locks the contract the
        // interp tier mirrors through `Response.raw_header_pairs`.
        let (url, server) = spawn_one_shot_server(
            "HTTP/1.1 200 OK",
            "Set-Cookie: a=1\r\nSet-Cookie: b=2\r\nContent-Type: text/plain\r\n",
            b"ok",
        );
        let client = unsafe { gos_rt_http_client_new() };
        let url_cs = std::ffi::CString::new(url).unwrap();
        let req =
            unsafe { gos_rt_http_client_get(client, crate::c_abi::string::test_gos_ptr(&url_cs)) };
        let packed = unsafe { gos_rt_http_request_send(req) };
        assert_eq!(gos_rt_result_disc(packed), 0);
        let resp = gos_rt_result_payload(packed) as *mut GosHttpResponse;
        assert!(!resp.is_null());
        let resp_ref = unsafe { &*resp };
        let cookies: Vec<&str> = resp_ref
            .headers
            .iter()
            .filter(|(k, _)| k == "set-cookie")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            cookies,
            vec!["a=1", "b=2"],
            "all headers: {:?}",
            resp_ref.headers
        );
        free_response(resp);
        drop(unsafe { Box::from_raw(client) });
        server.join().unwrap();
    }

    #[test]
    fn http_request_unknown_method_returns_err_naming_the_method() {
        let method = std::ffi::CString::new("BREW").unwrap();
        let url_cs = std::ffi::CString::new("http://127.0.0.1:1/never").unwrap();
        let packed = unsafe {
            gos_rt_http_request(
                crate::c_abi::string::test_gos_ptr(&method),
                crate::c_abi::string::test_gos_ptr(&url_cs),
                std::ptr::null(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(gos_rt_result_disc(packed), 1);
        let err = gos_rt_result_payload(packed) as *mut crate::c_abi::errors::GosError;
        assert!(!err.is_null());
        let msg_cs = unsafe { crate::c_abi::errors::gos_rt_error_message(err) };
        let msg = unsafe { CStr::from_ptr(msg_cs) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { crate::c_abi::string::gos_rt_str_free(msg_cs) };
        unsafe { crate::c_abi::rc::gos_rt_rc_release(err.cast()) };
        assert_eq!(msg, "http::request: unknown method `BREW`");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn http_request_send_transport_failure_packs_interp_shaped_err() {
        let client = unsafe { gos_rt_http_client_new() };
        // Port 1 is never listening; the dial must fail.
        let url_cs = std::ffi::CString::new("http://127.0.0.1:1/refused").unwrap();
        let req =
            unsafe { gos_rt_http_client_get(client, crate::c_abi::string::test_gos_ptr(&url_cs)) };
        let packed = unsafe { gos_rt_http_request_send(req) };
        assert_eq!(gos_rt_result_disc(packed), 1);
        let err = gos_rt_result_payload(packed) as *mut crate::c_abi::errors::GosError;
        assert!(!err.is_null());
        let msg_cs = unsafe { crate::c_abi::errors::gos_rt_error_message(err) };
        let msg = unsafe { CStr::from_ptr(msg_cs) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { crate::c_abi::string::gos_rt_str_free(msg_cs) };
        unsafe { crate::c_abi::rc::gos_rt_rc_release(err.cast()) };
        drop(unsafe { Box::from_raw(client) });
        // Same message class + prefix the interp tier renders via
        // `ClientError::Transport`'s Display.
        assert!(
            msg.starts_with("http: transport:") && msg.contains("Connection refused"),
            "unexpected transport error shape: {msg}"
        );
    }

    /// Serves a redirect hop plus its target on a loopback listener:
    /// `/one` → 302 `Location: /data`, anything else → 200 "landed".
    /// Handles up to `connections` sequential requests.
    fn spawn_redirect_server(connections: usize) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for _ in 0..connections {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) if line == "\r\n" || line == "\n" => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                let response = if path == "/one" {
                    "HTTP/1.1 302 Found\r\nLocation: /data\r\nContent-Length: 0\r\n\
                     Connection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nlanded"
                };
                let mut stream = reader.into_inner();
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn builder_chain_mutates_in_place_and_build_consumes_into_configured_client() {
        let b = unsafe { gos_rt_http_client_builder_new() };
        assert!(!b.is_null());
        let b1 = unsafe { gos_rt_http_client_builder_max_redirects(b, 0) };
        let b2 = unsafe { gos_rt_http_client_builder_timeout_ms(b1, 5000) };
        assert_eq!(b, b1, "chain must reuse the same allocation");
        assert_eq!(b, b2);
        let client = unsafe { gos_rt_http_client_builder_build(b2) };
        assert!(!client.is_null());
        let config = unsafe { &(*client).config };
        assert_eq!(config.max_redirects, 0);
        assert_eq!(config.timeout_ms, 5000);
        drop(unsafe { Box::from_raw(client) });
    }

    #[test]
    fn builder_clamps_negative_settings_to_zero_and_default() {
        let b = unsafe { gos_rt_http_client_builder_new() };
        let b = unsafe { gos_rt_http_client_builder_max_redirects(b, -7) };
        let b = unsafe { gos_rt_http_client_builder_timeout_ms(b, -1) };
        let client = unsafe { gos_rt_http_client_builder_build(b) };
        let config = unsafe { &(*client).config };
        assert_eq!(config.max_redirects, 0);
        assert_eq!(config.timeout_ms, 30_000);
        drop(unsafe { Box::from_raw(client) });
    }

    #[test]
    fn client_new_keeps_default_policy() {
        let client = unsafe { gos_rt_http_client_new() };
        let config = unsafe { &(*client).config };
        assert_eq!(config.max_redirects, 10);
        assert_eq!(config.timeout_ms, 30_000);
        drop(unsafe { Box::from_raw(client) });
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn client_request_default_policy_follows_redirect() {
        let (base, server) = spawn_redirect_server(2);
        let b = unsafe { gos_rt_http_client_builder_new() };
        let client = unsafe { gos_rt_http_client_builder_build(b) };
        let method = std::ffi::CString::new("GET").unwrap();
        let url_cs = std::ffi::CString::new(format!("{base}/one")).unwrap();
        let packed = unsafe {
            gos_rt_http_client_request(
                client,
                crate::c_abi::string::test_gos_ptr(&method),
                crate::c_abi::string::test_gos_ptr(&url_cs),
                std::ptr::null(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(gos_rt_result_disc(packed), 0);
        let resp = gos_rt_result_payload(packed) as *mut GosHttpResponse;
        let resp_ref = unsafe { &*resp };
        assert_eq!(resp_ref.status, 200);
        assert_eq!(resp_ref.body_bytes.as_deref(), Some(b"landed".as_slice()));
        free_response(resp);
        drop(unsafe { Box::from_raw(client) });
        server.join().unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // network round-trip: Miri has no socket syscalls
    fn client_request_max_redirects_zero_returns_raw_302_with_location() {
        let (base, server) = spawn_redirect_server(1);
        let b = unsafe { gos_rt_http_client_builder_new() };
        let b = unsafe { gos_rt_http_client_builder_max_redirects(b, 0) };
        let client = unsafe { gos_rt_http_client_builder_build(b) };
        let method = std::ffi::CString::new("GET").unwrap();
        let url_cs = std::ffi::CString::new(format!("{base}/one")).unwrap();
        let packed = unsafe {
            gos_rt_http_client_request(
                client,
                crate::c_abi::string::test_gos_ptr(&method),
                crate::c_abi::string::test_gos_ptr(&url_cs),
                std::ptr::null(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(gos_rt_result_disc(packed), 0);
        let resp = gos_rt_result_payload(packed) as *mut GosHttpResponse;
        assert_eq!(unsafe { (*resp).status }, 302);
        let loc = unsafe { gos_rt_http_response_location(resp) };
        assert_eq!(unsafe { CStr::from_ptr(loc) }.to_str().unwrap(), "/data");
        unsafe { crate::c_abi::string::gos_rt_str_free(loc) };
        free_response(resp);
        drop(unsafe { Box::from_raw(client) });
        server.join().unwrap();
    }

    #[test]
    fn client_request_unknown_method_returns_err_naming_the_method() {
        let client = unsafe { gos_rt_http_client_new() };
        let method = std::ffi::CString::new("BREW").unwrap();
        let url_cs = std::ffi::CString::new("http://127.0.0.1:1/never").unwrap();
        let packed = unsafe {
            gos_rt_http_client_request(
                client,
                crate::c_abi::string::test_gos_ptr(&method),
                crate::c_abi::string::test_gos_ptr(&url_cs),
                std::ptr::null(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(gos_rt_result_disc(packed), 1);
        let err = gos_rt_result_payload(packed) as *mut crate::c_abi::errors::GosError;
        let msg_cs = unsafe { crate::c_abi::errors::gos_rt_error_message(err) };
        let msg = unsafe { CStr::from_ptr(msg_cs) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { crate::c_abi::string::gos_rt_str_free(msg_cs) };
        unsafe { crate::c_abi::rc::gos_rt_rc_release(err.cast()) };
        assert_eq!(msg, "Client::request: unknown method `BREW`");

        let packed = unsafe {
            gos_rt_http_client_request_bytes(
                client,
                crate::c_abi::string::test_gos_ptr(&method),
                crate::c_abi::string::test_gos_ptr(&url_cs),
                std::ptr::null(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(gos_rt_result_disc(packed), 1);
        let err = gos_rt_result_payload(packed) as *mut crate::c_abi::errors::GosError;
        let msg_cs = unsafe { crate::c_abi::errors::gos_rt_error_message(err) };
        let msg = unsafe { CStr::from_ptr(msg_cs) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { crate::c_abi::string::gos_rt_str_free(msg_cs) };
        unsafe { crate::c_abi::rc::gos_rt_rc_release(err.cast()) };
        drop(unsafe { Box::from_raw(client) });
        assert_eq!(msg, "Client::request_bytes: unknown method `BREW`");
    }

    #[test]
    fn with_header_replaces_then_pushes_and_returns_same_pointer() {
        let resp =
            unsafe { gos_rt_http_response_text_new(201, crate::c_abi::string::test_gos_str("ok")) };
        let r1 = unsafe {
            gos_rt_http_response_with_header(
                resp,
                crate::c_abi::string::test_gos_str("x-a"),
                crate::c_abi::string::test_gos_str("1"),
            )
        };
        let r2 = unsafe {
            gos_rt_http_response_with_header(
                r1,
                crate::c_abi::string::test_gos_str("X-A"),
                crate::c_abi::string::test_gos_str("2"),
            )
        };
        let r3 = unsafe {
            gos_rt_http_response_with_header(
                r2,
                crate::c_abi::string::test_gos_str("x-b"),
                crate::c_abi::string::test_gos_str("3"),
            )
        };
        assert_eq!(resp, r1, "chain must reuse the same allocation");
        assert_eq!(resp, r2);
        assert_eq!(resp, r3);
        let headers = unsafe { &(*resp).headers };
        assert_eq!(
            headers.as_slice(),
            &[
                ("X-A".to_string(), "2".to_string()),
                ("x-b".to_string(), "3".to_string()),
            ],
            "same-name attach replaces (case-insensitive), new name appends"
        );
        let resp_box = unsafe { Box::from_raw(resp) };
        unsafe { crate::c_abi::string::gos_rt_str_free(resp_box.body.as_ptr()) };
    }
}
