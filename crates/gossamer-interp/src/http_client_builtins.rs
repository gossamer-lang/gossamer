//! VM hooks for `std::http::Client`.
//!
//! Every client entry point - the method-style surface
//! (`Client::get` / `post` / `put` / `options` / `delete` / `head`
//! plus `Request::send`) and the free functions (`http::get`,
//! `http::post`, `http::put`, `http::options`, `http::delete`,
//! `http::head`, `http::request`, `http::request_bytes`,
//! `http::stream`) - wraps `gossamer_std::http::Client`:
//! ureq-backed, HTTPS via rustls (Mozilla roots), 10 redirects,
//! cookies, gzip decode, connection pool - all on the shared
//! blocking I/O pool so per-goroutine workers stay free.
//!
//! `http::Client::builder().max_redirects(n).timeout_ms(t).build()`
//! produces a policy-carrying client whose `request` /
//! `request_bytes` honor the configuration: `max_redirects(0)`
//! never follows (3xx returned raw - the proxy-correct mode),
//! exceeding a non-zero budget is a "too many redirects" transport
//! error.
//!
//! Codegen note: the free-function surface (`http::get`,
//! `http::request`, `http::request_bytes`, `http::stream`,
//! `ResponseStream::next_line`), the chained builder surface
//! (`client.<verb>(url).header(..).body(..).send()`), and the
//! configured-client surface (`Client::builder` chain +
//! `client.request` / `request_bytes`) are also native on the
//! Cranelift / LLVM tiers via the matching `gos_rt_http_*` runtime
//! shims; `Request::send` returns the same `Result<Response, _>`
//! shape on every tier.
//!
//! Kept in its own module so the main `builtins.rs` file stays
//! under the 2000-line hard limit defined in `GUIDELINES.md`.

// Builtins return `RuntimeResult<Value>` to match the dispatcher's
// expected signature even when they never fail.
#![allow(clippy::unnecessary_wraps)]
use std::sync::Arc;

use crate::value::{RuntimeError, RuntimeResult, SmolStr, Value};

fn as_str(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s.as_str()),
        _ => None,
    }
}

// ------------------------------------------------------------------
// HTTP client builtins

/// Default redirect-following budget, matching `Client::new()`.
const DEFAULT_MAX_REDIRECTS: i64 = 10;
/// Default per-request timeout in milliseconds, matching `Client::new()`.
const DEFAULT_TIMEOUT_MS: i64 = 30_000;

/// Process-wide registry of persistent `gossamer_std::http::Client`
/// instances built with `.cookie_jar(true)`. The built `Client`
/// Gossamer struct carries the id in a `__client` field; the request
/// builtins look the engine up so the cookie jar survives across
/// requests on the same client (the runtime tiers do this by holding
/// a persistent `ureq::Agent` on the boxed `GosHttpClient`). The
/// `gossamer_std::http::Client` is `Send + Sync` (an `Arc` inside).
static NEXT_CLIENT_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
static CLIENT_REGISTRY: parking_lot::Mutex<Option<rustc_hash::FxHashMap<i64, StdClient>>> =
    parking_lot::Mutex::new(None);

fn client_registry_register(client: StdClient) -> i64 {
    let id = NEXT_CLIENT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut guard = CLIENT_REGISTRY.lock();
    guard
        .get_or_insert_with(rustc_hash::FxHashMap::default)
        .insert(id, client);
    id
}

fn client_registry_lookup(id: i64) -> Option<StdClient> {
    CLIENT_REGISTRY.lock().as_ref()?.get(&id).cloned()
}

pub(crate) fn builtin_http_client_new(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::struct_(
        "Client",
        crate::value::empty_struct_fields(),
    ))
}

fn config_fields(
    max_redirects: i64,
    timeout_ms: i64,
    cookie_jar: bool,
    proxy: &str,
) -> Vec<(&'static str, Value)> {
    vec![
        ("max_redirects", Value::Int(max_redirects)),
        ("timeout_ms", Value::Int(timeout_ms)),
        ("cookie_jar", Value::Bool(cookie_jar)),
        ("proxy", Value::String(SmolStr::from(proxy.to_string()))),
    ]
}

/// Reads the four builder settings off a `ClientBuilder` receiver,
/// filling defaults for any absent field.
fn builder_settings(inner: &crate::value::StructInner) -> (i64, i64, bool, String) {
    (
        int_field(inner, "max_redirects").unwrap_or(DEFAULT_MAX_REDIRECTS),
        int_field(inner, "timeout_ms").unwrap_or(DEFAULT_TIMEOUT_MS),
        bool_field(inner, "cookie_jar").unwrap_or(false),
        str_field(inner, "proxy").unwrap_or_default(),
    )
}

fn int_field(inner: &crate::value::StructInner, name: &str) -> Option<i64> {
    inner
        .fields
        .iter()
        .find(|(ident, _)| (**ident) == name)
        .and_then(|(_, v)| match v {
            Value::Int(n) => Some(*n),
            Value::Uint(n) => Some(*n as i64),
            _ => None,
        })
}

fn bool_field(inner: &crate::value::StructInner, name: &str) -> Option<bool> {
    inner
        .fields
        .iter()
        .find(|(ident, _)| (**ident) == name)
        .and_then(|(_, v)| match v {
            Value::Bool(b) => Some(*b),
            _ => None,
        })
}

fn str_field(inner: &crate::value::StructInner, name: &str) -> Option<String> {
    inner
        .fields
        .iter()
        .find(|(ident, _)| (**ident) == name)
        .and_then(|(_, v)| as_str(v).map(str::to_string))
}

/// Receiver guard for the `ClientBuilder` chain builtins.
fn expect_builder<'a>(
    args: &'a [Value],
    label: &str,
) -> RuntimeResult<&'a crate::value::StructInner> {
    match args.first() {
        Some(Value::Struct(inner)) if inner.name == "ClientBuilder" => Ok(inner),
        _ => Err(RuntimeError::Type(format!(
            "{label}: expected ClientBuilder"
        ))),
    }
}

/// Normalises a `max_redirects` setting: values past `u32::MAX`
/// clamp to `u32::MAX`.
fn clamp_max_redirects(n: i64) -> i64 {
    n.clamp(0, i64::from(u32::MAX))
}

/// Normalises a `timeout_ms` setting: zero falls back to the 30 s default.
fn clamp_timeout_ms(t: i64) -> i64 {
    if t <= 0 { DEFAULT_TIMEOUT_MS } else { t }
}

/// `http::Client::builder() -> ClientBuilder` - starts a client
/// configuration chain with `Client::new()`'s defaults (10
/// redirects, 30 s timeout).
pub(crate) fn builtin_http_client_builder(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::struct_(
        "ClientBuilder",
        config_fields(DEFAULT_MAX_REDIRECTS, DEFAULT_TIMEOUT_MS, false, ""),
    ))
}

/// `ClientBuilder::max_redirects(n) -> ClientBuilder` - rebuilt
/// immutably, like `Request::header`. `0` disables
/// redirect-following entirely (3xx responses are returned raw).
pub(crate) fn builtin_http_client_builder_max_redirects(args: &[Value]) -> RuntimeResult<Value> {
    let inner = expect_builder(args, "ClientBuilder::max_redirects")?;
    let (_, timeout_ms, cookie_jar, proxy) = builder_settings(inner);
    let n = match args.get(1) {
        Some(Value::Int(n)) if *n < 0 => {
            return Err(RuntimeError::Type(
                "ClientBuilder::max_redirects: count must be non-negative".to_string(),
            ));
        }
        Some(Value::Int(n)) => clamp_max_redirects(*n),
        _ => DEFAULT_MAX_REDIRECTS,
    };
    Ok(Value::struct_(
        "ClientBuilder",
        config_fields(n, timeout_ms, cookie_jar, &proxy),
    ))
}

/// `ClientBuilder::timeout_ms(t) -> ClientBuilder` - rebuilt
/// immutably; zero falls back to the 30 s default.
pub(crate) fn builtin_http_client_builder_timeout_ms(args: &[Value]) -> RuntimeResult<Value> {
    let inner = expect_builder(args, "ClientBuilder::timeout_ms")?;
    let (max_redirects, _, cookie_jar, proxy) = builder_settings(inner);
    let t = match args.get(1) {
        Some(Value::Int(t)) if *t < 0 => {
            return Err(RuntimeError::Type(
                "ClientBuilder::timeout_ms: timeout_ms must be non-negative".to_string(),
            ));
        }
        Some(Value::Int(t)) => clamp_timeout_ms(*t),
        _ => DEFAULT_TIMEOUT_MS,
    };
    Ok(Value::struct_(
        "ClientBuilder",
        config_fields(max_redirects, t, cookie_jar, &proxy),
    ))
}

/// `ClientBuilder::cookie_jar(enabled) -> ClientBuilder` - rebuilt
/// immutably. When enabled, the built client reuses one persistent
/// engine so `Set-Cookie` survives across requests; when disabled,
/// each request runs on a fresh engine (no cookie carryover).
pub(crate) fn builtin_http_client_builder_cookie_jar(args: &[Value]) -> RuntimeResult<Value> {
    let inner = expect_builder(args, "ClientBuilder::cookie_jar")?;
    let (max_redirects, timeout_ms, _, proxy) = builder_settings(inner);
    let enabled = matches!(args.get(1), Some(Value::Bool(true)));
    Ok(Value::struct_(
        "ClientBuilder",
        config_fields(max_redirects, timeout_ms, enabled, &proxy),
    ))
}

/// `ClientBuilder::proxy(url) -> ClientBuilder` - rebuilt immutably;
/// routes every request through `url`. An empty string clears it.
pub(crate) fn builtin_http_client_builder_proxy(args: &[Value]) -> RuntimeResult<Value> {
    let inner = expect_builder(args, "ClientBuilder::proxy")?;
    let (max_redirects, timeout_ms, cookie_jar, _) = builder_settings(inner);
    let proxy = args.get(1).and_then(as_str).unwrap_or("");
    Ok(Value::struct_(
        "ClientBuilder",
        config_fields(max_redirects, timeout_ms, cookie_jar, proxy),
    ))
}

/// `ClientBuilder::build() -> Client` - carries the configured
/// policy on the Client struct. When the cookie jar is enabled, a
/// persistent engine is built once and registered; the Client struct
/// carries its id in `__client` so the jar survives across requests.
/// The configured Client still works with the legacy `get`/`post`
/// pending-request chain (those builtins dispatch by struct name).
pub(crate) fn builtin_http_client_builder_build(args: &[Value]) -> RuntimeResult<Value> {
    let inner = expect_builder(args, "ClientBuilder::build")?;
    let (max_redirects, timeout_ms, cookie_jar, proxy) = builder_settings(inner);
    let mut fields = config_fields(max_redirects, timeout_ms, cookie_jar, &proxy);
    if cookie_jar {
        match configured_std_client(
            clamp_max_redirects(max_redirects) as u32,
            clamp_timeout_ms(timeout_ms) as u64,
            &proxy,
        ) {
            Ok(client) => {
                let id = client_registry_register(client);
                fields.push(("__client", Value::Int(id)));
            }
            Err(e) => return Ok(crate::builtins::err_variant(e)),
        }
    }
    Ok(Value::struct_("Client", fields))
}

/// Reads the redirect/timeout policy off a `Client` receiver. A
/// legacy `Client::new()` receiver (no config fields) gets the
/// defaults, so both constructors share the request builtins.
fn client_config(receiver: Option<&Value>) -> (u32, u64) {
    let (mut max_redirects, mut timeout_ms) = (DEFAULT_MAX_REDIRECTS, DEFAULT_TIMEOUT_MS);
    if let Some(Value::Struct(inner)) = receiver {
        if inner.name == "Client" {
            if let Some(n) = int_field(inner, "max_redirects") {
                max_redirects = clamp_max_redirects(n);
            }
            if let Some(t) = int_field(inner, "timeout_ms") {
                timeout_ms = clamp_timeout_ms(t);
            }
        }
    }
    // Both values are clamped to their unsigned ranges above.
    (max_redirects as u32, timeout_ms as u64)
}

/// The `gossamer_std::http::Client` a request on `receiver` runs on:
/// the registered persistent engine (cookie jar survives) when the
/// receiver carries a `__client` id, otherwise a fresh engine built
/// from the receiver's policy (redirects / timeout / proxy).
fn client_for_request(receiver: Option<&Value>) -> Result<StdClient, String> {
    if let Some(Value::Struct(inner)) = receiver
        && inner.name == "Client"
        && let Some(id) = int_field(inner, "__client")
        && let Some(client) = client_registry_lookup(id)
    {
        return Ok(client);
    }
    let (max_redirects, timeout_ms) = client_config(receiver);
    let proxy = match receiver {
        Some(Value::Struct(inner)) => str_field(inner, "proxy").unwrap_or_default(),
        _ => String::new(),
    };
    configured_std_client(max_redirects, timeout_ms, &proxy)
}

/// Builds a `gossamer_std` client carrying the receiver's policy.
/// The default builder configures no custom TLS, so `build()` fails
/// only on a malformed proxy URL; that error is surfaced as Err so
/// the builtin stays total.
fn configured_std_client(
    max_redirects: u32,
    timeout_ms: u64,
    proxy: &str,
) -> Result<StdClient, String> {
    let mut builder = StdClient::builder()
        .max_redirects(max_redirects)
        .timeout(std::time::Duration::from_millis(timeout_ms));
    if !proxy.is_empty() {
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|e| format!("{e}"))
}

/// Extracts the persistent-client registry id (`__client`) off a
/// `Client` receiver, if it was built with the cookie jar enabled.
fn client_id_of(receiver: Option<&Value>) -> Option<i64> {
    match receiver {
        Some(Value::Struct(inner)) if inner.name == "Client" => int_field(inner, "__client"),
        _ => None,
    }
}

pub(crate) fn builtin_http_client_get(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.get(1).and_then(as_str).unwrap_or("");
    Ok(pending_request_for("GET", url, client_id_of(args.first())))
}

/// Receiver guard shared by the `Request` builder builtins: the
/// leading argument must be a `Request` struct value.
fn expect_request<'a>(
    args: &'a [Value],
    label: &str,
) -> RuntimeResult<&'a crate::value::StructInner> {
    match args.first() {
        Some(Value::Struct(inner)) if inner.name == "Request" => Ok(inner),
        _ => Err(RuntimeError::Type(format!("{label}: expected Request"))),
    }
}

/// `Request::header(name, value) -> Request` - returns a new Request
/// with the pair appended to its `headers` array-of-tuples field.
pub(crate) fn builtin_http_request_header(args: &[Value]) -> RuntimeResult<Value> {
    let inner = expect_request(args, "Request::header")?;
    let name = args.get(1).and_then(as_str).unwrap_or("");
    let value = args.get(2).and_then(as_str).unwrap_or("");
    let pair = Value::Tuple(Arc::from(vec![
        Value::String(SmolStr::from(name.to_string())),
        Value::String(SmolStr::from(value.to_string())),
    ]));
    let mut fields = inner.fields.to_vec();
    if let Some((_, slot)) = fields.iter_mut().find(|(ident, _)| (*ident) == "headers") {
        let mut items = match slot {
            Value::Array(existing) => existing.as_ref().clone(),
            _ => Vec::new(),
        };
        items.push(pair);
        *slot = Value::Array(Arc::new(items));
    } else {
        fields.push(("headers", Value::Array(Arc::new(vec![pair]))));
    }
    Ok(Value::struct_("Request", fields))
}

/// `Request::body(text) -> Request` - returns a new Request whose
/// `body` field holds the given string.
pub(crate) fn builtin_http_request_body(args: &[Value]) -> RuntimeResult<Value> {
    let inner = expect_request(args, "Request::body")?;
    let body = args.get(1).and_then(as_str).unwrap_or("");
    let body_value = Value::String(SmolStr::from(body.to_string()));
    let mut fields = inner.fields.to_vec();
    if let Some((_, slot)) = fields.iter_mut().find(|(ident, _)| (*ident) == "body") {
        *slot = body_value;
    } else {
        fields.push(("body", body_value));
    }
    Ok(Value::struct_("Request", fields))
}

pub(crate) fn builtin_http_request_send(args: &[Value]) -> RuntimeResult<Value> {
    let inner = expect_request(args, "Request::send")?;
    let field = |name: &str| {
        inner
            .fields
            .iter()
            .find(|(ident, _)| (**ident) == name)
            .map(|(_, v)| v)
    };
    let url = field("url").and_then(as_str).unwrap_or("").to_string();
    let method = field("method")
        .and_then(as_str)
        .unwrap_or("GET")
        .to_string();
    let Some(parsed) = gossamer_std::http::Method::parse(&method) else {
        return Ok(crate::builtins::err_variant(format!(
            "Request::send: unknown method `{method}`"
        )));
    };
    let header_pairs = extract_header_pairs(field("headers"));
    let headers = header_refs(&header_pairs);
    let body = field("body").and_then(as_str).unwrap_or("").to_string();
    let body_opt: Option<&[u8]> = if body.is_empty() {
        None
    } else {
        Some(body.as_bytes())
    };
    // Reuse the originating client's persistent engine (cookie jar /
    // proxy) when this request came from `client.<verb>(url)`; a
    // standalone request uses a fresh default-policy engine.
    let client = field("__client")
        .and_then(|v| match v {
            Value::Int(id) => client_registry_lookup(*id),
            _ => None,
        })
        .unwrap_or_else(gossamer_std::http::Client::new);
    match client.do_request(parsed, &url, body_opt, &headers) {
        Ok(resp) => Ok(crate::builtins::ok_variant(lift_response(resp))),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_http_response_bytes(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Struct(inner)) = args.first() else {
        return Err(RuntimeError::Type(
            "Response::bytes: expected Response".to_string(),
        ));
    };
    if inner.name != "Response" {
        return Err(RuntimeError::Type(
            "Response::bytes: expected Response".to_string(),
        ));
    }
    let body = inner
        .fields
        .iter()
        .find(|(ident, _)| (**ident) == "body")
        .and_then(|(_, v)| as_str(v))
        .unwrap_or_default();
    let bytes: Vec<Value> = body.bytes().map(|b| Value::Int(i64::from(b))).collect();
    Ok(crate::builtins::ok_variant(Value::Array(Arc::new(bytes))))
}

// ------------------------------------------------------------------
// HTTPS-capable client surface.
//
// Wraps `gossamer_std::http::Client` (ureq-backed: TLS via rustls,
// redirects, cookies, gzip, connection pool). Both the method-style
// API (`client.post(url, body)`) and the free-function API
// (`http::post(url, body, ct)`, `http::request(method, ...)`) route
// through the same `Client::do_request` core.
//
// `http::stream(method, url, body, headers)` returns a
// `ResponseStream { __handle, status, content_type }` whose
// `next_line()` reads one line at a time so SSE / chunked bodies
// don't need to be buffered in full.

use gossamer_std::http::{Client as StdClient, Method, StreamResponse};

/// Process-wide registry of in-flight streaming HTTP responses.
/// Handles are allocated monotonically and never reused, so a stale
/// `ResponseStream` value whose stream was already consumed by
/// `Response::stream(...)` looks up an absent handle (yielding `None`)
/// rather than colliding with a later stream that recycled its slot.
/// Mirrors the compiled tier's `NEXT_STREAM_HANDLE` registry so both
/// tiers behave identically.
static NEXT_STREAM_HANDLE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
static STREAM_REGISTRY: parking_lot::Mutex<
    Option<rustc_hash::FxHashMap<i64, Arc<parking_lot::Mutex<StreamResponse>>>>,
> = parking_lot::Mutex::new(None);

/// Registers a stream the interpreter itself produced - a response body a
/// handler writes as it goes - in the same registry a client stream uses,
/// so one drain serves both.
pub(crate) fn stream_register_public(stream: StreamResponse) -> i64 {
    stream_register(stream)
}

fn stream_register(stream: StreamResponse) -> i64 {
    let handle = NEXT_STREAM_HANDLE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let arc = Arc::new(parking_lot::Mutex::new(stream));
    STREAM_REGISTRY
        .lock()
        .get_or_insert_with(rustc_hash::FxHashMap::default)
        .insert(handle, arc);
    handle
}

fn stream_lookup(handle: i64) -> Option<Arc<parking_lot::Mutex<StreamResponse>>> {
    STREAM_REGISTRY.lock().as_ref()?.get(&handle).cloned()
}

/// Streams already claimed by a `Response::stream(...)` value and
/// waiting to be drained to a client by the server writer. Keyed by
/// the original registry handle; `stream_take_for_serve` is one-shot.
static PENDING_SERVE: parking_lot::Mutex<
    Option<rustc_hash::FxHashMap<i64, Arc<parking_lot::Mutex<StreamResponse>>>>,
> = parking_lot::Mutex::new(None);

/// Moves `handle` from the client registry into the pending-serve
/// registry. After this, `next_line` / `next_chunk` on the same
/// `ResponseStream` yield `None` - the stream now belongs to the
/// response. No-op when the handle was already consumed.
pub(crate) fn stream_consume_for_response(handle: i64) {
    let taken = {
        let mut reg = STREAM_REGISTRY.lock();
        reg.as_mut().and_then(|map| map.remove(&handle))
    };
    if let Some(arc) = taken {
        PENDING_SERVE
            .lock()
            .get_or_insert_with(rustc_hash::FxHashMap::default)
            .insert(handle, arc);
    }
}

/// Takes the pending stream for `handle` - one-shot, so serving the
/// same streamed response twice drains an empty body the second time.
pub(crate) fn stream_take_for_serve(
    handle: i64,
) -> Option<Arc<parking_lot::Mutex<StreamResponse>>> {
    PENDING_SERVE.lock().as_mut()?.remove(&handle)
}

/// Extracts the `__handle` of a `ResponseStream` value.
pub(crate) fn response_stream_handle(value: &Value) -> Option<i64> {
    handle_field(value, "ResponseStream")
}

/// `Read` adapter over a registry stream so the server writer can
/// drain it as a [`gossamer_std::http::BodyStream`].
pub(crate) struct StreamBody(pub(crate) Arc<parking_lot::Mutex<StreamResponse>>);

impl std::io::Read for StreamBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.lock().read_raw(buf)
    }
}

fn handle_field(value: &Value, expected_name: &str) -> Option<i64> {
    let Value::Struct(inner) = value else {
        return None;
    };
    if inner.name != expected_name {
        return None;
    }
    for (ident, val) in &inner.fields {
        if (*ident) == "__handle" {
            if let Value::Int(n) = val {
                return Some(*n);
            }
        }
    }
    None
}

/// Lifts a `gossamer_std::http::Response` into the Gossamer
/// `Response` struct shape (`status`, `body`, `raw_bytes`,
/// `content_type`, `location`, `headers`).
fn lift_response(resp: gossamer_std::http::Response) -> Value {
    let body_str = String::from_utf8_lossy(&resp.body).into_owned();
    let raw: Vec<Value> = resp
        .body
        .iter()
        .map(|b| Value::Int(i64::from(*b)))
        .collect();
    let content_type = resp
        .headers
        .get("content-type")
        .unwrap_or("text/plain")
        .to_string();
    let location = resp.headers.get("location").unwrap_or("").to_string();
    let status = i64::from(resp.status.0);
    let mut fields = vec![
        ("status", Value::Int(status)),
        ("body", Value::String(body_str.into())),
        ("raw_bytes", Value::Array(Arc::new(raw))),
        ("content_type", Value::String(SmolStr::from(content_type))),
        ("location", Value::String(SmolStr::from(location))),
    ];
    // The wire sequence (order + duplicates - `set-cookie` repeats)
    // is what the compiled tiers lift, so prefer it; the sorted
    // dedup `Headers` view is the fallback for responses built
    // without a transport (e.g. tests constructing `Response`).
    let headers: Vec<Value> = if resp.raw_header_pairs.is_empty() {
        resp.headers
            .iter()
            .map(|(name, value)| header_pair_value(name, value))
            .collect()
    } else {
        resp.raw_header_pairs
            .iter()
            .map(|(name, value)| header_pair_value(name, value))
            .collect()
    };
    fields.push(("headers", Value::Array(Arc::new(headers))));
    Value::struct_("Response", fields)
}

/// `(name, value)` header tuple in the lifted `Response.headers` shape.
fn header_pair_value(name: &str, value: &str) -> Value {
    Value::Tuple(Arc::from(vec![
        Value::String(SmolStr::from(name)),
        Value::String(SmolStr::from(value)),
    ]))
}

fn extract_header_pairs(value: Option<&Value>) -> Vec<(String, String)> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items.iter() {
        if let Value::Tuple(t) = item {
            if t.len() >= 2 {
                if let (Some(k), Some(v)) = (as_str(&t[0]), as_str(&t[1])) {
                    out.push((k.to_string(), v.to_string()));
                }
            }
        }
    }
    out
}

fn header_refs(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
    pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}

/// `http::request(method, url, body, headers) -> Result<Response, String>`.
pub(crate) fn builtin_http_request(args: &[Value]) -> RuntimeResult<Value> {
    let method_str = args.first().and_then(as_str).unwrap_or("");
    let Some(method) = Method::parse(method_str) else {
        return Ok(crate::builtins::err_variant(format!(
            "http::request: unknown method `{method_str}`"
        )));
    };
    let url = args.get(1).and_then(as_str).unwrap_or("");
    let body = args.get(2).and_then(as_str).unwrap_or("");
    let header_pairs = extract_header_pairs(args.get(3));
    let headers = header_refs(&header_pairs);
    let body_opt: Option<&[u8]> = if body.is_empty() {
        None
    } else {
        Some(body.as_bytes())
    };
    let client = StdClient::new();
    match client.do_request(method, url, body_opt, &headers) {
        Ok(resp) => Ok(crate::builtins::ok_variant(lift_response(resp))),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

/// `http::request_bytes(method, url, body: [u8], headers) -> Result<Response, String>`.
pub(crate) fn builtin_http_request_bytes(args: &[Value]) -> RuntimeResult<Value> {
    let method_str = args.first().and_then(as_str).unwrap_or("");
    let Some(method) = Method::parse(method_str) else {
        return Ok(crate::builtins::err_variant(format!(
            "http::request_bytes: unknown method `{method_str}`"
        )));
    };
    let url = args.get(1).and_then(as_str).unwrap_or("");
    // A byte body can arrive as `Value::Array` of Ints, or - when
    // The body is whatever byte vector the caller built, in whichever
    // representation the VM chose for it.
    let body: Vec<u8> = args.get(2).map(Value::bytes_or_empty).unwrap_or_default();
    let header_pairs = extract_header_pairs(args.get(3));
    let headers = header_refs(&header_pairs);
    let body_opt: Option<&[u8]> = if body.is_empty() {
        None
    } else {
        Some(body.as_slice())
    };
    let client = StdClient::new();
    match client.do_request(method, url, body_opt, &headers) {
        Ok(resp) => Ok(crate::builtins::ok_variant(lift_response(resp))),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

/// `http::get(url, headers) -> Result<Response, String>`.
pub(crate) fn builtin_http_get(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.first().and_then(as_str).unwrap_or("");
    let header_pairs = extract_header_pairs(args.get(1));
    let headers = header_refs(&header_pairs);
    let client = StdClient::new();
    match client.do_request(Method::Get, url, None, &headers) {
        Ok(resp) => Ok(crate::builtins::ok_variant(lift_response(resp))),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

/// `http::post(url, body, content_type) -> Result<Response, String>`.
pub(crate) fn builtin_http_post(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.first().and_then(as_str).unwrap_or("");
    let body = args.get(1).and_then(as_str).unwrap_or("");
    let content_type = args.get(2).and_then(as_str).unwrap_or("application/json");
    let client = StdClient::new();
    match client.post(url, body.as_bytes(), content_type) {
        Ok(resp) => Ok(crate::builtins::ok_variant(lift_response(resp))),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

/// `http::put(url, body, content_type) -> Result<Response, String>`.
pub(crate) fn builtin_http_put(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.first().and_then(as_str).unwrap_or("");
    let body = args.get(1).and_then(as_str).unwrap_or("");
    let content_type = args.get(2).and_then(as_str).unwrap_or("application/json");
    let client = StdClient::new();
    match client.put(url, body.as_bytes(), content_type) {
        Ok(resp) => Ok(crate::builtins::ok_variant(lift_response(resp))),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

/// `http::options(url, headers) -> Result<Response, String>`.
pub(crate) fn builtin_http_options(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.first().and_then(as_str).unwrap_or("");
    let header_pairs = extract_header_pairs(args.get(1));
    let headers = header_refs(&header_pairs);
    let client = StdClient::new();
    match client.options(url, &headers) {
        Ok(resp) => Ok(crate::builtins::ok_variant(lift_response(resp))),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

/// `http::delete(url, body, headers) -> Result<Response, String>`.
pub(crate) fn builtin_http_delete(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.first().and_then(as_str).unwrap_or("");
    let body = args.get(1).and_then(as_str).unwrap_or("");
    let header_pairs = extract_header_pairs(args.get(2));
    let headers = header_refs(&header_pairs);
    let body_opt: Option<&[u8]> = if body.is_empty() {
        None
    } else {
        Some(body.as_bytes())
    };
    let client = StdClient::new();
    match client.delete(url, body_opt, &headers) {
        Ok(resp) => Ok(crate::builtins::ok_variant(lift_response(resp))),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

/// `http::head(url, headers) -> Result<Response, String>`.
pub(crate) fn builtin_http_head(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.first().and_then(as_str).unwrap_or("");
    let header_pairs = extract_header_pairs(args.get(1));
    let headers = header_refs(&header_pairs);
    let client = StdClient::new();
    match client.head(url, &headers) {
        Ok(resp) => Ok(crate::builtins::ok_variant(lift_response(resp))),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

/// `http::stream(method, url, body, headers) -> Result<ResponseStream, String>`.
pub(crate) fn builtin_http_stream(args: &[Value]) -> RuntimeResult<Value> {
    let method_str = args.first().and_then(as_str).unwrap_or("");
    let Some(method) = Method::parse(method_str) else {
        return Ok(crate::builtins::err_variant(format!(
            "http::stream: unknown method `{method_str}`"
        )));
    };
    let url = args.get(1).and_then(as_str).unwrap_or("");
    let body = args.get(2).and_then(as_str).unwrap_or("");
    let header_pairs = extract_header_pairs(args.get(3));
    let headers = header_refs(&header_pairs);
    let body_opt: Option<&[u8]> = if body.is_empty() {
        None
    } else {
        Some(body.as_bytes())
    };
    let client = StdClient::new();
    match client.stream(method, url, body_opt, &headers) {
        Ok(stream) => {
            let status = i64::from(stream.status.0);
            let content_type = stream
                .headers
                .get("content-type")
                .unwrap_or("text/plain")
                .to_string();
            let handle = stream_register(stream);
            let fields = vec![
                ("__handle", Value::Int(handle)),
                ("status", Value::Int(status)),
                ("content_type", Value::String(SmolStr::from(content_type))),
            ];
            Ok(crate::builtins::ok_variant(Value::struct_(
                "ResponseStream",
                Arc::unwrap_or_clone(Arc::new(fields)),
            )))
        }
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

/// `ResponseStream::next_line() -> Option<String>`.
pub(crate) fn builtin_response_stream_next_line(args: &[Value]) -> RuntimeResult<Value> {
    let Some(handle) = args.first().and_then(|v| handle_field(v, "ResponseStream")) else {
        return Err(RuntimeError::Type(
            "ResponseStream::next_line: receiver must be ResponseStream".to_string(),
        ));
    };
    let Some(arc) = stream_lookup(handle) else {
        return Ok(crate::builtins::none_variant());
    };
    let mut guard = arc.lock();
    match guard.next_line() {
        Ok(Some(line)) => Ok(crate::builtins::some_variant(Value::String(line.into()))),
        Ok(None) | Err(_) => Ok(crate::builtins::none_variant()),
    }
}

/// `ResponseStream::next_chunk(max_bytes) -> Option<[u8]>`. Bytes
/// lift as an Array of Ints, matching the `resp.raw_bytes`
/// convention; EOF and I/O failure both surface as `None`.
pub(crate) fn builtin_response_stream_next_chunk(args: &[Value]) -> RuntimeResult<Value> {
    let Some(handle) = args.first().and_then(|v| handle_field(v, "ResponseStream")) else {
        return Err(RuntimeError::Type(
            "ResponseStream::next_chunk: receiver must be ResponseStream".to_string(),
        ));
    };
    let max_bytes = match args.get(1) {
        Some(Value::Int(n)) if *n <= 0 => {
            return Err(RuntimeError::Type(
                "ResponseStream::next_chunk: size must be positive".to_string(),
            ));
        }
        Some(Value::Int(n)) => usize::try_from(*n).unwrap_or(0),
        _ => 0,
    };
    let Some(arc) = stream_lookup(handle) else {
        return Ok(crate::builtins::none_variant());
    };
    let mut guard = arc.lock();
    match guard.next_chunk(max_bytes) {
        Ok(Some(bytes)) => {
            let elems: Vec<Value> = bytes.iter().map(|b| Value::Int(i64::from(*b))).collect();
            Ok(crate::builtins::some_variant(Value::Array(Arc::new(elems))))
        }
        Ok(None) | Err(_) => Ok(crate::builtins::none_variant()),
    }
}

// ------------------------------------------------------------------
// Method-style API: Client::post / put / options / delete / head /
// request - matching the existing `Client::get` call shape. The
// builder produces a `Request { method, url }` struct; calling
// `.send()` on it dispatches through `gossamer_std::http::Client`.

fn pending_request_for(method: &str, url: &str, client_id: Option<i64>) -> Value {
    let mut fields = vec![
        ("method", Value::String(SmolStr::from(method.to_string()))),
        ("url", Value::String(SmolStr::from(url.to_string()))),
    ];
    if let Some(id) = client_id {
        fields.push(("__client", Value::Int(id)));
    }
    Value::struct_("Request", fields)
}

pub(crate) fn builtin_http_client_post(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.get(1).and_then(as_str).unwrap_or("");
    Ok(pending_request_for("POST", url, client_id_of(args.first())))
}

pub(crate) fn builtin_http_client_put(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.get(1).and_then(as_str).unwrap_or("");
    Ok(pending_request_for("PUT", url, client_id_of(args.first())))
}

pub(crate) fn builtin_http_client_options(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.get(1).and_then(as_str).unwrap_or("");
    Ok(pending_request_for(
        "OPTIONS",
        url,
        client_id_of(args.first()),
    ))
}

pub(crate) fn builtin_http_client_delete(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.get(1).and_then(as_str).unwrap_or("");
    Ok(pending_request_for(
        "DELETE",
        url,
        client_id_of(args.first()),
    ))
}

pub(crate) fn builtin_http_client_head(args: &[Value]) -> RuntimeResult<Value> {
    let url = args.get(1).and_then(as_str).unwrap_or("");
    Ok(pending_request_for("HEAD", url, client_id_of(args.first())))
}

/// `Client::request(method, url, body, headers) -> Result<Response, String>` -
/// same semantics and error strings as the free `http::request`,
/// except the receiver's configured redirect/timeout policy is
/// honored. `max_redirects(0)` returns 3xx responses raw; exceeding
/// a non-zero budget is a transport error ("too many redirects").
pub(crate) fn builtin_http_client_request(args: &[Value]) -> RuntimeResult<Value> {
    let method_str = args.get(1).and_then(as_str).unwrap_or("");
    let Some(method) = Method::parse(method_str) else {
        return Ok(crate::builtins::err_variant(format!(
            "Client::request: unknown method `{method_str}`"
        )));
    };
    let url = args.get(2).and_then(as_str).unwrap_or("");
    let body = args.get(3).and_then(as_str).unwrap_or("");
    let header_pairs = extract_header_pairs(args.get(4));
    let headers = header_refs(&header_pairs);
    let body_opt: Option<&[u8]> = if body.is_empty() {
        None
    } else {
        Some(body.as_bytes())
    };
    let client = match client_for_request(args.first()) {
        Ok(c) => c,
        Err(e) => return Ok(crate::builtins::err_variant(e)),
    };
    match client.do_request(method, url, body_opt, &headers) {
        Ok(resp) => Ok(crate::builtins::ok_variant(lift_response(resp))),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

/// `Client::request_bytes(method, url, body: [u8], headers) ->
/// Result<Response, String>` - binary-body sibling of
/// `Client::request`, honoring the configured policy.
pub(crate) fn builtin_http_client_request_bytes(args: &[Value]) -> RuntimeResult<Value> {
    let method_str = args.get(1).and_then(as_str).unwrap_or("");
    let Some(method) = Method::parse(method_str) else {
        return Ok(crate::builtins::err_variant(format!(
            "Client::request_bytes: unknown method `{method_str}`"
        )));
    };
    let url = args.get(2).and_then(as_str).unwrap_or("");
    // The body is whatever byte vector the caller built, in whichever
    // representation the VM chose for it.
    let body: Vec<u8> = args.get(3).map(Value::bytes_or_empty).unwrap_or_default();
    let header_pairs = extract_header_pairs(args.get(4));
    let headers = header_refs(&header_pairs);
    let body_opt: Option<&[u8]> = if body.is_empty() {
        None
    } else {
        Some(body.as_slice())
    };
    let client = match client_for_request(args.first()) {
        Ok(c) => c,
        Err(e) => return Ok(crate::builtins::err_variant(e)),
    };
    match client.do_request(method, url, body_opt, &headers) {
        Ok(resp) => Ok(crate::builtins::ok_variant(lift_response(resp))),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Standalone pending request (no originating client) for the
    /// `Request::header`/`body`/`send` chain tests.
    fn pending_request(method: &str, url: &str) -> Value {
        pending_request_for(method, url, None)
    }

    fn request_field<'a>(req: &'a Value, name: &str) -> Option<&'a Value> {
        let Value::Struct(inner) = req else {
            return None;
        };
        inner
            .fields
            .iter()
            .find(|(ident, _)| (**ident) == name)
            .map(|(_, v)| v)
    }

    #[test]
    fn request_header_and_body_chain_rebuilds_request_fields() {
        let req = pending_request("POST", "http://127.0.0.1:1/x");
        let req = builtin_http_request_header(&[
            req,
            Value::String("x-a".into()),
            Value::String("1".into()),
        ])
        .unwrap();
        let req = builtin_http_request_header(&[
            req,
            Value::String("x-b".into()),
            Value::String("2".into()),
        ])
        .unwrap();
        let req = builtin_http_request_body(&[req, Value::String("payload".into())]).unwrap();
        let Some(Value::Array(headers)) = request_field(&req, "headers") else {
            panic!("chained request must carry a headers array");
        };
        assert_eq!(headers.len(), 2);
        let Value::Tuple(first) = &headers[0] else {
            panic!("header entries must be tuples");
        };
        assert_eq!(as_str(&first[0]), Some("x-a"));
        assert_eq!(as_str(&first[1]), Some("1"));
        assert_eq!(
            request_field(&req, "body").and_then(as_str),
            Some("payload")
        );
        assert_eq!(request_field(&req, "method").and_then(as_str), Some("POST"));
    }

    #[test]
    fn request_send_honors_chained_header_and_body() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let mut req = Vec::new();
            loop {
                let n = stream.read(&mut buf).expect("read");
                req.extend_from_slice(&buf[..n]);
                if n == 0 || req.windows(7).any(|w| w == b"payload") {
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .expect("write");
            String::from_utf8_lossy(&req).into_owned()
        });
        let req = pending_request("POST", &format!("http://{addr}/echo"));
        let req = builtin_http_request_header(&[
            req,
            Value::String("x-test".into()),
            Value::String("vm".into()),
        ])
        .unwrap();
        let req = builtin_http_request_body(&[req, Value::String("payload".into())]).unwrap();
        let sent = builtin_http_request_send(&[req]).unwrap();
        let Value::Variant(variant) = &sent else {
            panic!("send must return a Result variant, got {sent:?}");
        };
        assert_eq!(variant.name, "Ok");
        let request_text = server.join().expect("server thread");
        assert!(request_text.starts_with("POST /echo HTTP/1.1"));
        assert!(request_text.to_ascii_lowercase().contains("x-test: vm"));
        assert!(request_text.ends_with("payload"));
    }

    #[test]
    fn request_send_transport_failure_is_err_with_client_error_display() {
        let req = pending_request("GET", "http://127.0.0.1:1/refused");
        let sent = builtin_http_request_send(&[req]).unwrap();
        let Value::Variant(variant) = &sent else {
            panic!("send must return a Result variant, got {sent:?}");
        };
        assert_eq!(variant.name, "Err");
        // `err_variant` wraps the message in an `errors::Error`
        // struct whose `message` field carries the rendered text.
        let Some(Value::Struct(err)) = variant.fields.first() else {
            panic!("Err payload must be an errors::Error struct");
        };
        let msg = err
            .fields
            .iter()
            .find(|(ident, _)| (**ident) == "message")
            .and_then(|(_, v)| as_str(v))
            .unwrap_or("");
        assert!(
            msg.starts_with("http: transport:") && msg.contains("Connection refused"),
            "unexpected transport error shape: {msg}"
        );
    }

    fn struct_field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
        let Value::Struct(inner) = value else {
            return None;
        };
        inner
            .fields
            .iter()
            .find(|(ident, _)| (**ident) == name)
            .map(|(_, v)| v)
    }

    fn int_field_of(value: &Value, name: &str) -> Option<i64> {
        match struct_field(value, name) {
            Some(Value::Int(n)) => Some(*n),
            _ => None,
        }
    }

    fn ok_payload(result: &Value) -> &Value {
        let Value::Variant(variant) = result else {
            panic!("expected a Result variant, got {result:?}");
        };
        assert_eq!(variant.name, "Ok", "expected Ok, got {variant:?}");
        variant.fields.first().expect("Ok payload")
    }

    fn err_message(result: &Value) -> String {
        let Value::Variant(variant) = result else {
            panic!("expected a Result variant, got {result:?}");
        };
        assert_eq!(variant.name, "Err", "expected Err, got {variant:?}");
        let Some(Value::Struct(err)) = variant.fields.first() else {
            panic!("Err payload must be an errors::Error struct");
        };
        err.fields
            .iter()
            .find(|(ident, _)| (**ident) == "message")
            .and_then(|(_, v)| as_str(v))
            .unwrap_or("")
            .to_string()
    }

    /// Serves a 2-hop redirect chain on a loopback listener:
    /// `/one` → 302 `/two` → 302 `/three` → 200 "landed". Handles
    /// up to `connections` sequential requests, then exits.
    fn spawn_redirect_chain_server(connections: usize) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
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
                // Drain the header section so the client can reuse
                // its connection logic cleanly.
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
                let response = match path {
                    "/one" => "HTTP/1.1 302 Found\r\nLocation: /two\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                        .to_string(),
                    "/two" => "HTTP/1.1 302 Found\r\nLocation: /three\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                        .to_string(),
                    _ => "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nlanded"
                        .to_string(),
                };
                let mut stream = reader.into_inner();
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn configured_client(max_redirects: i64, timeout_ms: i64) -> Value {
        let b = builtin_http_client_builder(&[]).unwrap();
        let b = builtin_http_client_builder_max_redirects(&[b, Value::Int(max_redirects)]).unwrap();
        let b = builtin_http_client_builder_timeout_ms(&[b, Value::Int(timeout_ms)]).unwrap();
        builtin_http_client_builder_build(&[b]).unwrap()
    }

    #[test]
    fn builder_chain_rebuilds_immutably_and_build_produces_client() {
        let b = builtin_http_client_builder(&[]).unwrap();
        assert_eq!(int_field_of(&b, "max_redirects"), Some(10));
        assert_eq!(int_field_of(&b, "timeout_ms"), Some(30_000));
        let b2 = builtin_http_client_builder_max_redirects(&[b.clone(), Value::Int(0)]).unwrap();
        // The original builder is untouched (immutably rebuilt).
        assert_eq!(int_field_of(&b, "max_redirects"), Some(10));
        assert_eq!(int_field_of(&b2, "max_redirects"), Some(0));
        let b3 = builtin_http_client_builder_timeout_ms(&[b2, Value::Int(5000)]).unwrap();
        let client = builtin_http_client_builder_build(&[b3]).unwrap();
        let Value::Struct(inner) = &client else {
            panic!("build must produce a Client struct");
        };
        assert_eq!(inner.name, "Client");
        assert_eq!(int_field_of(&client, "max_redirects"), Some(0));
        assert_eq!(int_field_of(&client, "timeout_ms"), Some(5000));
    }

    #[test]
    fn client_request_default_policy_follows_redirect_chain() {
        let (base, server) = spawn_redirect_chain_server(3);
        let client = builtin_http_client_builder(&[])
            .and_then(|b| builtin_http_client_builder_build(&[b]))
            .unwrap();
        let sent = builtin_http_client_request(&[
            client,
            Value::String("GET".into()),
            Value::String(format!("{base}/one").into()),
            Value::String("".into()),
            Value::Array(Arc::new(Vec::new())),
        ])
        .unwrap();
        let resp = ok_payload(&sent);
        assert_eq!(int_field_of(resp, "status"), Some(200));
        assert_eq!(struct_field(resp, "body").and_then(as_str), Some("landed"));
        server.join().unwrap();
    }

    #[test]
    fn client_request_max_redirects_zero_returns_raw_302_with_location() {
        let (base, server) = spawn_redirect_chain_server(1);
        let client = configured_client(0, 30_000);
        let sent = builtin_http_client_request(&[
            client,
            Value::String("GET".into()),
            Value::String(format!("{base}/one").into()),
            Value::String("".into()),
            Value::Array(Arc::new(Vec::new())),
        ])
        .unwrap();
        let resp = ok_payload(&sent);
        assert_eq!(int_field_of(resp, "status"), Some(302));
        assert_eq!(
            struct_field(resp, "location").and_then(as_str),
            Some("/two")
        );
        server.join().unwrap();
    }

    /// ureq hop counting (empirically pinned): `max_redirects(1)` on
    /// a 2-hop chain follows the first redirect, then hits the second
    /// 3xx with the budget exhausted and fails the request with a
    /// "too many redirects" transport error - it does NOT return the
    /// intermediate 3xx. Only `max_redirects(0)` returns 3xx raw.
    #[test]
    fn client_request_max_redirects_one_on_two_hop_chain_is_too_many_redirects() {
        let (base, server) = spawn_redirect_chain_server(2);
        let client = configured_client(1, 30_000);
        let sent = builtin_http_client_request(&[
            client,
            Value::String("GET".into()),
            Value::String(format!("{base}/one").into()),
            Value::String("".into()),
            Value::Array(Arc::new(Vec::new())),
        ])
        .unwrap();
        let msg = err_message(&sent);
        assert!(
            msg.starts_with("http: transport:") && msg.contains("too many redirects"),
            "unexpected redirect-overflow error shape: {msg}"
        );
        server.join().unwrap();
    }

    #[test]
    fn client_request_unknown_method_names_the_method() {
        let client = configured_client(0, 30_000);
        let sent = builtin_http_client_request(&[
            client.clone(),
            Value::String("BREW".into()),
            Value::String("http://127.0.0.1:1/never".into()),
            Value::String("".into()),
            Value::Array(Arc::new(Vec::new())),
        ])
        .unwrap();
        assert_eq!(err_message(&sent), "Client::request: unknown method `BREW`");
        let sent = builtin_http_client_request_bytes(&[
            client,
            Value::String("BREW".into()),
            Value::String("http://127.0.0.1:1/never".into()),
            Value::Array(Arc::new(Vec::new())),
            Value::Array(Arc::new(Vec::new())),
        ])
        .unwrap();
        assert_eq!(
            err_message(&sent),
            "Client::request_bytes: unknown method `BREW`"
        );
    }

    #[test]
    fn legacy_client_new_receiver_gets_default_policy() {
        let (base, server) = spawn_redirect_chain_server(3);
        let legacy = builtin_http_client_new(&[]).unwrap();
        let sent = builtin_http_client_request(&[
            legacy,
            Value::String("GET".into()),
            Value::String(format!("{base}/one").into()),
            Value::String("".into()),
            Value::Array(Arc::new(Vec::new())),
        ])
        .unwrap();
        let resp = ok_payload(&sent);
        assert_eq!(int_field_of(resp, "status"), Some(200));
        server.join().unwrap();
    }

    #[test]
    fn client_request_timeout_ms_aborts_a_stalled_response() {
        // A listener that is never accepted from is a server that never
        // responds: the kernel completes the handshake into its backlog
        // and nothing answers, so the configured 100 ms global timeout
        // must abort the request with a timeout transport error.
        //
        // No thread waits on the listener. The request may time out
        // before it connects (the client resolves the host on a thread
        // of its own under the same budget), and an accept would then
        // wait for a connection that never arrives.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        let client = configured_client(10, 100);
        let sent = builtin_http_client_request(&[
            client,
            Value::String("GET".into()),
            Value::String(format!("http://{addr}/stall").into()),
            Value::String("".into()),
            Value::Array(Arc::new(Vec::new())),
        ])
        .unwrap();
        let msg = err_message(&sent);
        assert!(
            msg.starts_with("http: transport:") && msg.contains("timeout"),
            "unexpected timeout error shape: {msg}"
        );
        // The listener outlives the request, so the connection stays
        // stalled until the verdict is in.
        drop(listener);
    }

    #[test]
    fn lift_response_exposes_headers_as_tuple_array() {
        let mut headers = gossamer_std::http::Headers::new();
        headers.insert("content-type", "application/json");
        headers.insert("x-request-id", "abc123");
        let resp = gossamer_std::http::Response {
            status: gossamer_std::http::StatusCode(200),
            headers,
            body: b"{}".to_vec(),
            raw_header_pairs: Vec::new(),
            body_stream: None,
        };
        let Value::Struct(lifted) = lift_response(resp) else {
            panic!("lift_response must produce a struct value");
        };
        let (_, headers_val) = lifted
            .fields
            .iter()
            .find(|(ident, _)| (**ident) == "headers")
            .expect("lifted Response must carry a `headers` field");
        let Value::Array(items) = headers_val else {
            panic!("`headers` must be an array, got {headers_val:?}");
        };
        assert_eq!(items.len(), 2);
        // `Headers` is keyed by lowercase name in a BTreeMap, so the
        // lifted pairs come back in sorted-by-name order.
        let expected = [
            ("content-type", "application/json"),
            ("x-request-id", "abc123"),
        ];
        for (item, (name, value)) in items.iter().zip(expected) {
            let Value::Tuple(pair) = item else {
                panic!("each header entry must be a tuple, got {item:?}");
            };
            assert_eq!(pair.len(), 2);
            assert_eq!(as_str(&pair[0]), Some(name));
            assert_eq!(as_str(&pair[1]), Some(value));
        }
    }

    #[test]
    fn lift_response_lowercases_mixed_case_header_names() {
        let mut headers = gossamer_std::http::Headers::new();
        headers.insert("X-MiXeD-CaSe", "v");
        let resp = gossamer_std::http::Response {
            status: gossamer_std::http::StatusCode(200),
            headers,
            body: b"ok".to_vec(),
            raw_header_pairs: Vec::new(),
            body_stream: None,
        };
        let Value::Struct(lifted) = lift_response(resp) else {
            panic!("lift_response must produce a struct value");
        };
        let (_, headers_val) = lifted
            .fields
            .iter()
            .find(|(ident, _)| (**ident) == "headers")
            .expect("lifted Response must carry a `headers` field");
        let Value::Array(items) = headers_val else {
            panic!("`headers` must be an array, got {headers_val:?}");
        };
        let Value::Tuple(pair) = &items[0] else {
            panic!("each header entry must be a tuple");
        };
        // Same canonical lowercase name the compiled tier lifts.
        assert_eq!(as_str(&pair[0]), Some("x-mixed-case"));
        assert_eq!(as_str(&pair[1]), Some("v"));
    }

    #[test]
    fn lift_response_prefers_raw_pairs_keeping_duplicate_set_cookie_order() {
        // The `Headers` map collapses repeated names, but the wire
        // sequence keeps both `set-cookie` pairs - and the compiled
        // tiers lift the wire sequence, so the interp must too.
        let mut headers = gossamer_std::http::Headers::new();
        headers.insert("set-cookie", "b=2");
        headers.insert("content-type", "text/plain");
        let resp = gossamer_std::http::Response {
            status: gossamer_std::http::StatusCode(200),
            headers,
            body: b"ok".to_vec(),
            raw_header_pairs: vec![
                ("set-cookie".to_string(), "a=1".to_string()),
                ("set-cookie".to_string(), "b=2".to_string()),
                ("content-type".to_string(), "text/plain".to_string()),
            ],
            body_stream: None,
        };
        let Value::Struct(lifted) = lift_response(resp) else {
            panic!("lift_response must produce a struct value");
        };
        let (_, headers_val) = lifted
            .fields
            .iter()
            .find(|(ident, _)| (**ident) == "headers")
            .expect("lifted Response must carry a `headers` field");
        let Value::Array(items) = headers_val else {
            panic!("`headers` must be an array, got {headers_val:?}");
        };
        let pairs: Vec<(&str, &str)> = items
            .iter()
            .map(|item| {
                let Value::Tuple(pair) = item else {
                    panic!("each header entry must be a tuple, got {item:?}");
                };
                (as_str(&pair[0]).unwrap(), as_str(&pair[1]).unwrap())
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("set-cookie", "a=1"),
                ("set-cookie", "b=2"),
                ("content-type", "text/plain"),
            ],
        );
    }
}
