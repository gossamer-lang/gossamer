#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    clippy::module_name_repetitions
)]
//! Bytecode-VM builtins for request-scoped values - the interp mirror
//! of `gos_rt_http_request_set_value` / `gos_rt_http_request_value`
//! (Go `context.WithValue`). The bag lives in a hidden `__values`
//! field on the Request struct (the `__params` / `path_value`
//! pattern in `http_router`).
//!
//! VM structs are immutable values, so `set_value` cannot mutate in
//! place the way the compiled `*mut GosHttpRequest` shim does. Instead
//! it rebuilds the Request struct with the updated bag (replace-then-
//! push) and returns it. A fixture that threads the return value
//! (`let r = r.set_value("user", "alice")`) therefore behaves
//! identically on the VM and the compiled tiers - the compiled tiers
//! return the same pointer they mutated.

use std::sync::Arc;

use gossamer_ast::Ident;

use crate::builtins::BuiltinFnPub;
use crate::value::{RuntimeResult, SmolStr, Value};

use super::*;

/// Extracts a string argument, defaulting to empty. A private copy
/// local to this module so the pure request-value builtins do not
/// depend on the SSE module (gated off the wasm sandbox).
fn arg_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.as_str().to_string(),
        _ => String::new(),
    }
}

pub(crate) fn install_http_request_values(globals: &mut Vec<(&'static str, Value)>) {
    for key in ["Request::value", "http::Request::value"] {
        globals.push((
            key,
            crate::builtins::builtin_pub(key, builtin_request_value as BuiltinFnPub),
        ));
    }
    for key in ["Request::set_value", "http::Request::set_value"] {
        globals.push((
            key,
            crate::builtins::builtin_pub(key, builtin_request_set_value as BuiltinFnPub),
        ));
    }
    for key in ["Request::form_value", "http::Request::form_value"] {
        globals.push((
            key,
            crate::builtins::builtin_pub(key, builtin_request_form_value as BuiltinFnPub),
        ));
    }
    for key in ["Request::basic_auth", "http::Request::basic_auth"] {
        globals.push((
            key,
            crate::builtins::builtin_pub(key, builtin_request_basic_auth as BuiltinFnPub),
        ));
    }
}

/// Reads the raw request body string from a Request value's `body`
/// field, empty when absent.
fn request_body_string(req: &Value) -> String {
    let Value::Struct(inner) = req else {
        return String::new();
    };
    for (field, val) in &inner.fields {
        if (*field) == "body" {
            if let Value::String(s) = val {
                return s.as_str().to_string();
            }
        }
    }
    String::new()
}

/// Case-insensitively reads a header value from a Request value's
/// `headers` field (a `[(String, String)]` array of tuples).
fn request_header_value(req: &Value, name: &str) -> Option<String> {
    let Value::Struct(inner) = req else {
        return None;
    };
    for (field, val) in &inner.fields {
        if (*field) == "headers" {
            if let Value::Array(items) = val {
                for item in items.iter() {
                    if let Value::Tuple(t) = item {
                        if let (Some(Value::String(k)), Some(Value::String(v))) =
                            (t.first(), t.get(1))
                        {
                            if k.as_str().eq_ignore_ascii_case(name) {
                                return Some(v.as_str().to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// First value for `key` in an x-www-form-urlencoded body, or `""`.
fn form_lookup(body: &str, key: &str) -> String {
    for pair in body.split('&') {
        let (raw_key, raw_val) = pair.split_once('=').unwrap_or((pair, ""));
        if gossamer_runtime::codec::percent::decode(
            raw_key,
            gossamer_runtime::codec::percent::Component::Query,
        ) == key
        {
            return gossamer_runtime::codec::percent::decode(
                raw_val,
                gossamer_runtime::codec::percent::Component::Query,
            );
        }
    }
    String::new()
}

/// `Request::form_value(req, key) -> String` - the first
/// x-www-form-urlencoded body value for `key`, or `""` when absent.
/// Matches the compiled `gos_rt_http_request_form_value` shim.
pub(crate) fn builtin_request_form_value(args: &[Value]) -> RuntimeResult<Value> {
    let key = arg_str(args.get(1));
    let body = args.first().map(request_body_string).unwrap_or_default();
    Ok(Value::String(SmolStr::from(form_lookup(
        &body,
        key.as_str(),
    ))))
}

/// `Request::basic_auth(req) -> Option<(String, String)>` - the decoded
/// `(user, password)` from an `Authorization: Basic <base64>` header,
/// or `None` when absent or malformed. Matches the compiled
/// `gos_rt_http_request_basic_auth` shim.
pub(crate) fn builtin_request_basic_auth(args: &[Value]) -> RuntimeResult<Value> {
    let Some(req) = args.first() else {
        return Ok(none_variant());
    };
    let Some(header) = request_header_value(req, "authorization") else {
        return Ok(none_variant());
    };
    let token = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "));
    let Some(token) = token else {
        return Ok(none_variant());
    };
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
        Value::String(SmolStr::from(user)),
        Value::String(SmolStr::from(pass)),
    ]));
    Ok(some_variant(pair))
}

/// Reads the request-scoped value attached under `name` from a Request
/// value's hidden `__values` field.
fn values_lookup(req: &Value, name: &str) -> Option<String> {
    let Value::Struct(inner) = req else {
        return None;
    };
    for (field, val) in &inner.fields {
        if (*field) == "__values" {
            if let Value::Array(items) = val {
                for item in items.iter() {
                    if let Value::Tuple(t) = item {
                        if let (Some(Value::String(k)), Some(Value::String(v))) =
                            (t.first(), t.get(1))
                        {
                            if k.as_str() == name {
                                return Some(v.as_str().to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// `Request::value(req, name) -> String` - empty string when absent.
/// Matches the compiled `gos_rt_http_request_value` shim.
pub(crate) fn builtin_request_value(args: &[Value]) -> RuntimeResult<Value> {
    let name = arg_str(args.get(1));
    let found = args
        .first()
        .and_then(|req| values_lookup(req, name.as_str()))
        .unwrap_or_default();
    Ok(Value::String(SmolStr::from(found)))
}

/// `Request::set_value(req, name, value) -> Request` - returns a
/// Request with the value attached (replace-then-push). Matches the
/// compiled `gos_rt_http_request_set_value` shim's bag semantics; the
/// rebuild-and-return is the VM stand-in for the compiled in-place
/// mutation.
pub(crate) fn builtin_request_set_value(args: &[Value]) -> RuntimeResult<Value> {
    let name = arg_str(args.get(1));
    let value = arg_str(args.get(2));
    let Some(Value::Struct(inner)) = args.first() else {
        return Ok(args.first().cloned().unwrap_or(Value::Unit));
    };
    // Existing values minus any prior entry for `name`, plus the new
    // pair (replace-then-push).
    let mut pairs: Vec<Value> = Vec::new();
    for (field, val) in &inner.fields {
        if (*field) == "__values" {
            if let Value::Array(items) = val {
                for item in items.iter() {
                    if let Value::Tuple(t) = item {
                        if let Some(Value::String(k)) = t.first() {
                            if k.as_str() == name.as_str() {
                                continue;
                            }
                        }
                    }
                    pairs.push(item.clone());
                }
            }
        }
    }
    pairs.push(Value::Tuple(Arc::from(vec![
        Value::String(SmolStr::from(name.as_str())),
        Value::String(SmolStr::from(value.as_str())),
    ])));
    // Rebuild the struct, dropping the old `__values` field and
    // appending the refreshed one; every other field is carried over.
    let mut fields: Vec<(&'static str, Value)> = inner
        .fields
        .iter()
        .filter(|(f, _)| (**f) != "__values")
        .map(|(name, value)| (*name, value.clone()))
        .collect();
    fields.push(("__values", Value::Array(Arc::new(pairs))));
    Ok(Value::struct_(inner.name.clone(), fields))
}
