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
use std::sync::atomic::Ordering;

use crate::value::SmolStr;

use gossamer_std::bufio as bufio_std;
use gossamer_std::math as math_std;
#[cfg(not(target_arch = "wasm32"))]
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
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeResult, Value};

/// Entry point invoked from `builtins::install`.
use gossamer_std::http_middleware as mw_std;

use super::*;

/// `(source name, transform kind)` for every wrapper beyond `tag`. The
/// kind numbering is ABI with `gossamer_std::http_middleware::kind` and
/// the compiled tiers' `middleware_kind`.
const MIDDLEWARE_WRAPPERS: &[(&str, i64)] = &[
    ("request_id", mw_std::kind::REQUEST_ID),
    ("cors", mw_std::kind::CORS),
    ("security_headers", mw_std::kind::SECURITY_HEADERS),
    ("etag", mw_std::kind::ETAG),
    ("rate_limit", mw_std::kind::RATE_LIMIT),
    ("hsts", mw_std::kind::HSTS),
    ("cache_control", mw_std::kind::CACHE_CONTROL),
    ("body_limit", mw_std::kind::BODY_LIMIT),
    ("compress_gzip", mw_std::kind::COMPRESS_GZIP),
    ("logger", mw_std::kind::LOGGER),
    ("recoverer", mw_std::kind::RECOVERER),
    ("timeout", mw_std::kind::TIMEOUT),
    ("basic_auth", mw_std::kind::BASIC_AUTH),
    ("bearer_auth", mw_std::kind::BEARER_AUTH),
    ("safe_defaults", mw_std::kind::SAFE_DEFAULTS),
];

/// Configuration constructors, registered under their `Type::method`
/// spelling so `middleware::CorsConfig::permissive()` resolves.
const MIDDLEWARE_CONFIGS: &[(&str, BuiltinFnPub)] = &[
    ("CorsConfig::permissive", builtin_mw_cors_permissive),
    ("CorsConfig::new", builtin_mw_cors_new),
    ("HstsConfig::safe_default", builtin_mw_hsts_safe_default),
    ("HstsConfig::strict", builtin_mw_hsts_strict),
    ("SecurityHeaders::strict", builtin_mw_security_strict),
    ("SecurityHeaders::off", builtin_mw_security_off),
    ("CacheControl::no_store", builtin_mw_cache_no_store),
    (
        "CacheControl::immutable_for",
        builtin_mw_cache_immutable_for,
    ),
    ("RateLimit::per_ip", builtin_mw_rate_limit_per_ip),
];

pub(crate) fn install_http_middleware(globals: &mut Vec<(&'static str, Value)>) {
    for (name, call) in [
        ("new_request_id", builtin_mw_new_request_id as BuiltinFnPub),
        ("decode_basic_auth", builtin_mw_decode_basic_auth),
        ("accepts_gzip", builtin_mw_accepts_gzip),
        ("tag", builtin_mw_tag),
    ] {
        let qualified: &'static str =
            Box::leak(format!("http::middleware::{name}").into_boxed_str());
        globals.push((qualified, crate::builtins::builtin_pub(qualified, call)));
        let short: &'static str = Box::leak(format!("middleware::{name}").into_boxed_str());
        globals.push((short, crate::builtins::builtin_pub(short, call)));
    }
    for &(name, kind) in MIDDLEWARE_WRAPPERS {
        let wrapper: BuiltinFnPub = match kind {
            mw_std::kind::REQUEST_ID => |a| wrap_with(a, mw_std::kind::REQUEST_ID),
            mw_std::kind::CORS => |a| wrap_with(a, mw_std::kind::CORS),
            mw_std::kind::SECURITY_HEADERS => |a| wrap_with(a, mw_std::kind::SECURITY_HEADERS),
            mw_std::kind::ETAG => |a| wrap_with(a, mw_std::kind::ETAG),
            mw_std::kind::RATE_LIMIT => |a| wrap_with(a, mw_std::kind::RATE_LIMIT),
            mw_std::kind::HSTS => |a| wrap_with(a, mw_std::kind::HSTS),
            mw_std::kind::CACHE_CONTROL => |a| wrap_with(a, mw_std::kind::CACHE_CONTROL),
            mw_std::kind::BODY_LIMIT => |a| wrap_with(a, mw_std::kind::BODY_LIMIT),
            mw_std::kind::COMPRESS_GZIP => |a| wrap_with(a, mw_std::kind::COMPRESS_GZIP),
            mw_std::kind::LOGGER => |a| wrap_with(a, mw_std::kind::LOGGER),
            mw_std::kind::RECOVERER => |a| wrap_with(a, mw_std::kind::RECOVERER),
            mw_std::kind::TIMEOUT => |a| wrap_with(a, mw_std::kind::TIMEOUT),
            mw_std::kind::BASIC_AUTH => |a| wrap_with(a, mw_std::kind::BASIC_AUTH),
            mw_std::kind::BEARER_AUTH => |a| wrap_with(a, mw_std::kind::BEARER_AUTH),
            _ => |a| wrap_with(a, mw_std::kind::SAFE_DEFAULTS),
        };
        for prefix in ["http::middleware::", "middleware::"] {
            let key: &'static str = Box::leak(format!("{prefix}{name}").into_boxed_str());
            globals.push((key, crate::builtins::builtin_pub(key, wrapper)));
        }
    }
    for &(name, call) in MIDDLEWARE_CONFIGS {
        for prefix in ["http::middleware::", "middleware::"] {
            let key: &'static str = Box::leak(format!("{prefix}{name}").into_boxed_str());
            globals.push((key, crate::builtins::builtin_pub(key, call)));
        }
    }
}

/// Builds the `Middleware` handle a wrapper returns: the inner handler
/// plus the transform selector and its configuration string.
fn wrap_with(args: &[Value], kind: i64) -> RuntimeResult<Value> {
    let inner = args.first().cloned().unwrap_or(Value::Unit);
    let config = args.get(1).map(Value::to_string).unwrap_or_default();
    Ok(Value::struct_(
        "http::middleware::Handler",
        vec![
            ("__mw_inner", inner),
            ("__mw_kind", Value::Int(kind)),
            ("__mw_config", Value::String(SmolStr::from(config))),
        ],
    ))
}

fn builtin_mw_cors_permissive(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(mw_std::cors_permissive().into()))
}

fn builtin_mw_cors_new(args: &[Value]) -> RuntimeResult<Value> {
    let origin = arg_str(args.first());
    let methods = arg_str(args.get(1));
    let headers = arg_str(args.get(2));
    let max_age = args.get(3).and_then(value_to_int).unwrap_or(0);
    Ok(Value::String(
        mw_std::cors_config(&origin, &methods, &headers, max_age).into(),
    ))
}

fn builtin_mw_hsts_safe_default(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(mw_std::hsts_safe_default().into()))
}

fn builtin_mw_hsts_strict(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(mw_std::hsts_strict().into()))
}

fn builtin_mw_security_strict(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(mw_std::security_headers_strict().into()))
}

fn builtin_mw_security_off(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(mw_std::security_headers_off().into()))
}

fn builtin_mw_cache_no_store(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(mw_std::cache_control_no_store().into()))
}

fn builtin_mw_cache_immutable_for(args: &[Value]) -> RuntimeResult<Value> {
    let seconds = args.first().and_then(value_to_int).unwrap_or(0);
    Ok(Value::String(
        mw_std::cache_control_immutable_for(seconds).into(),
    ))
}

fn builtin_mw_rate_limit_per_ip(args: &[Value]) -> RuntimeResult<Value> {
    let capacity = args.first().and_then(value_to_int).unwrap_or(0);
    let refill = args.get(1).and_then(value_to_int).unwrap_or(0);
    Ok(Value::String(
        mw_std::rate_limit_config(capacity, refill).into(),
    ))
}

pub(crate) fn builtin_mw_new_request_id(_args: &[Value]) -> RuntimeResult<Value> {
    use std::sync::atomic::AtomicU64;
    pub(crate) static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = gossamer_runtime::platform::system_time_now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    Ok(Value::String(format!("{nanos:x}-{n:x}").into()))
}

/// `http::middleware::decode_basic_auth(header) -> Option<(String, String)>`.
/// Decodes a `Basic <base64(user:pass)>` Authorization header value (the
/// `Basic ` scheme prefix is optional) into `(user, password)`, or `None`
/// for malformed / non-decodable input. Mirrors the compiled-tier
/// `gos_rt_mw_decode_basic_auth` shim so both tiers classify identically.
pub(crate) fn builtin_mw_decode_basic_auth(args: &[Value]) -> RuntimeResult<Value> {
    let header = arg_str(args.first());
    let token = header.strip_prefix("Basic ").unwrap_or(&header);
    let Ok(decoded) = gossamer_std::encoding::base64::decode(token.trim()) else {
        return Ok(none_variant());
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return Ok(none_variant());
    };
    let Some((user, pass)) = decoded.split_once(':') else {
        return Ok(none_variant());
    };
    let pair = Value::Tuple(Arc::from(vec![
        Value::String(user.to_string().into()),
        Value::String(pass.to_string().into()),
    ]));
    Ok(some_variant(pair))
}

/// `http::middleware::tag(inner) -> Handler` - wraps a handler into a
/// `Middleware` value carrying the inner handler; `Middleware::serve`
/// runs the inner serve and prepends `mw:` to the response body. The
/// deterministic interp twin of the compiled `gos_rt_middleware_new` /
/// `gos_rt_middleware_serve` composition.
pub(crate) fn builtin_mw_tag(args: &[Value]) -> RuntimeResult<Value> {
    let inner = args.first().cloned().unwrap_or(Value::Unit);
    Ok(Value::struct_(
        "http::middleware::Handler",
        vec![("__mw_inner", inner)],
    ))
}

/// Body bytes of a Response field value, in either its String or its
/// byte-array shape.
fn body_bytes(body: &Value) -> Vec<u8> {
    body.bytes_or_empty()
}

/// Rebuilds a body value in the shape the response already used, so a
/// chained middleware composes byte-identically with the compiled tier.
fn body_value(original: &Value, bytes: Vec<u8>) -> Value {
    match original {
        Value::Array(_) | Value::IntArray(_) => Value::Array(Arc::new(
            bytes.iter().map(|b| Value::Int(i64::from(*b))).collect(),
        )),
        _ => Value::String(SmolStr::from(String::from_utf8_lossy(&bytes).into_owned())),
    }
}

/// Header pairs of a Response `headers` field value.
fn header_pairs(headers: Option<&Value>) -> Vec<(String, String)> {
    match headers {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::Tuple(pair) if pair.len() == 2 => {
                    Some((pair[0].to_string(), pair[1].to_string()))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn header_value(pairs: &[(String, String)]) -> Value {
    Value::Array(Arc::new(
        pairs
            .iter()
            .map(|(k, v)| {
                Value::Tuple(Arc::from(vec![
                    Value::String(SmolStr::from(k.as_str())),
                    Value::String(SmolStr::from(v.as_str())),
                ]))
            })
            .collect(),
    ))
}

/// A `Request` value's `name` field as text, or `""` when it is absent or
/// not a string.
fn request_field_str<'a>(request: &'a Value, name: &str) -> &'a str {
    match request {
        Value::Struct(inner) => inner
            .fields
            .iter()
            .find(|(f, _)| (**f) == name)
            .and_then(|(_, v)| match v {
                Value::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or(""),
        _ => "",
    }
}

/// A `Request` value's headers as the name/value pairs a middleware reads.
fn request_header_pairs(request: &Value) -> Vec<(String, String)> {
    match request {
        Value::Struct(inner) => header_pairs(
            inner
                .fields
                .iter()
                .find(|(f, _)| (**f) == "headers")
                .map(|(_, v)| v),
        ),
        _ => Vec::new(),
    }
}

/// Byte length of a `Request` value's body.
fn request_body_len(request: &Value) -> usize {
    match request {
        Value::Struct(inner) => inner
            .fields
            .iter()
            .find(|(f, _)| (**f) == "body")
            .map_or(0, |(_, v)| body_bytes(v).len()),
        _ => 0,
    }
}

/// Builds the `Response` value a control answers with when it decides
/// before the inner handler runs.
fn response_value_from_parts(parts: &mw_std::ResponseParts) -> Value {
    Value::struct_(
        "http::Response",
        vec![
            ("status", Value::Int(parts.status)),
            (
                "body",
                Value::String(SmolStr::from(String::from_utf8_lossy(&parts.body).as_ref())),
            ),
            ("headers", header_value(&parts.headers)),
        ],
    )
}

/// `Middleware::serve(mw, request)` - invoked by `http::serve`'s dispatch
/// when the handler is a composed `Middleware`. Runs the wrapped handler
/// (a struct handler's `{T}::serve` or a nested `Middleware::serve`) then
/// applies the `mw:` body transform.
pub(crate) fn native_middleware_serve(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let slot = |name: &str| match args.first() {
        Some(Value::Struct(s)) => s
            .fields
            .iter()
            .find(|(f, _)| (**f) == name)
            .map(|(_, v)| v.clone()),
        _ => None,
    };
    let inner = slot("__mw_inner");
    let kind = slot("__mw_kind")
        .as_ref()
        .and_then(value_to_int)
        .unwrap_or(mw_std::kind::TAG);
    let config = slot("__mw_config")
        .map(|v| v.to_string())
        .unwrap_or_default();
    let Some(inner) = inner else {
        return Ok(err_variant("middleware: missing inner handler"));
    };
    let request = args.get(1).cloned().unwrap_or(Value::Unit);
    // Request phase: a control that sheds load, rejects an oversized body,
    // or demands a credential answers here, before the inner handler runs -
    // which is the only point at which shedding load saves any work.
    let request_headers = request_header_pairs(&request);
    let request_parts = mw_std::RequestParts {
        method: request_field_str(&request, "method"),
        path: request_field_str(&request, "path"),
        headers: &request_headers,
        body_len: request_body_len(&request),
        peer_addr: request_field_str(&request, "peer_addr"),
    };
    let accepts_gzip = request_parts.accepts_gzip();
    if let mw_std::Before::Answer(parts) = mw_std::apply_request(kind, &config, &request_parts) {
        return Ok(ok_variant(response_value_from_parts(&parts)));
    }
    let result = crate::value::dispatch_request(dispatch, &inner, request)?;
    // The wrapped serve returns `Ok(Response)` or a bare `Response`.
    let response = match &result {
        Value::Variant(v) if v.name == "Ok" && !v.fields.is_empty() => &v.fields[0],
        other => other,
    };
    let Value::Struct(resp_inner) = response else {
        return Ok(result);
    };
    let field = |name: &str| {
        resp_inner
            .fields
            .iter()
            .find(|(f, _)| (**f) == name)
            .map(|(_, v)| v.clone())
    };
    let body_field = field("body").unwrap_or(Value::String(SmolStr::from("")));
    let mut parts = mw_std::ResponseParts {
        status: field("status")
            .as_ref()
            .and_then(value_to_int)
            .unwrap_or(200),
        body: body_bytes(&body_field),
        headers: header_pairs(field("headers").as_ref()),
    };
    mw_std::apply_with_request(kind, &config, &mut parts, accepts_gzip);
    let mut new_fields: Vec<(&'static str, Value)> = resp_inner
        .fields
        .iter()
        .map(|(f, v)| match *f {
            "body" => (*f, body_value(&body_field, std::mem::take(&mut parts.body))),
            "status" => (*f, Value::Int(parts.status)),
            "headers" => (*f, header_value(&parts.headers)),
            _ => (*f, v.clone()),
        })
        .collect();
    if !new_fields.iter().any(|(f, _)| *f == "headers") {
        new_fields.push(("headers", header_value(&parts.headers)));
    }
    Ok(ok_variant(Value::struct_("http::Response", new_fields)))
}

pub(crate) fn builtin_mw_accepts_gzip(args: &[Value]) -> RuntimeResult<Value> {
    let header = arg_str(args.first());
    let accepts = header
        .split(',')
        .any(|tok| tok.trim().eq_ignore_ascii_case("gzip"));
    Ok(Value::Bool(accepts))
}

// ----------------------------------------------------------------------
// uuid
