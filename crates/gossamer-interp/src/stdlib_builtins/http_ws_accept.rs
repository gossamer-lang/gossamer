#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::single_call_fn
)]
//! Interp-tier `http::websocket::accept` - the RFC 6455 server-side
//! handshake. Validates the upgrade headers, computes the
//! Sec-WebSocket-Accept token, and returns a `Result<Response, Error>`
//! whose Ok payload is a 101 Switching Protocols Response. Mirrors
//! `gossamer_std::http_websocket::accept` and the compiled-tier
//! `gos_rt_ws_accept` shim byte-for-byte (status, headers, and error
//! strings) so the surface is tier-parity stable.

use std::sync::Arc;

use gossamer_ast::Ident;

use crate::builtins::{BuiltinFnPub, err_variant, ok_variant};
use crate::value::{RuntimeResult, SmolStr, Value};

use super::*;

pub(crate) fn install_http_ws_accept(globals: &mut Vec<(&'static str, Value)>) {
    for name in ["http::websocket::accept", "websocket::accept"] {
        globals.push((
            name,
            crate::builtins::builtin_pub(name, builtin_ws_accept as BuiltinFnPub),
        ));
    }
}

/// Case-insensitive header lookup over a server `Request` value's
/// `headers` field (`Array<Tuple<String, String>>`).
fn header_lookup(request: &Value, name: &str) -> Option<String> {
    let Value::Struct(inner) = request else {
        return None;
    };
    for (field, val) in &inner.fields {
        if (*field) != "headers" {
            continue;
        }
        if let Value::Array(arr) = val {
            for entry in arr.iter() {
                if let Value::Tuple(t) = entry {
                    if let (Some(Value::String(k)), Some(Value::String(v))) = (t.first(), t.get(1))
                    {
                        if k.as_str().eq_ignore_ascii_case(name) {
                            return Some(v.as_str().to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Builds the 101 Switching Protocols `Response` value carrying the
/// negotiated upgrade headers.
fn upgrade_response(token: &str) -> Value {
    let headers: Vec<Value> = [
        ("upgrade", "websocket"),
        ("connection", "Upgrade"),
        ("sec-websocket-accept", token),
    ]
    .iter()
    .map(|(k, v)| {
        Value::Tuple(Arc::from(vec![
            Value::String(SmolStr::from(*k)),
            Value::String(SmolStr::from(*v)),
        ]))
    })
    .collect();
    let fields = vec![
        ("status", Value::Int(101)),
        ("body", Value::String(SmolStr::from(""))),
        ("content_type", Value::String(SmolStr::from(""))),
        ("headers", Value::Array(Arc::new(headers))),
    ];
    Value::struct_("http::Response", fields)
}

pub(crate) fn builtin_ws_accept(args: &[Value]) -> RuntimeResult<Value> {
    let Some(request) = args.first() else {
        return Ok(err_variant("missing Upgrade header"));
    };

    let Some(upgrade) = header_lookup(request, "upgrade") else {
        return Ok(err_variant("missing Upgrade header"));
    };
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Ok(err_variant(format!("bad Upgrade: {upgrade}")));
    }
    let Some(connection) = header_lookup(request, "connection") else {
        return Ok(err_variant("missing Connection header"));
    };
    let has_upgrade_token = connection
        .split(',')
        .any(|tok| tok.trim().eq_ignore_ascii_case("upgrade"));
    if !has_upgrade_token {
        return Ok(err_variant(format!("bad Connection: {connection}")));
    }
    let version = header_lookup(request, "sec-websocket-version").unwrap_or_default();
    if version.trim() != "13" {
        return Ok(err_variant(format!("bad version: {version}")));
    }
    let Some(key) = header_lookup(request, "sec-websocket-key") else {
        return Ok(err_variant("missing Sec-WebSocket-Key"));
    };

    // base64(sha1(key + GUID)) - identical derivation to
    // `builtin_ws_accept_key` so accept_key and accept never drift.
    let mut input = key.into_bytes();
    input.extend_from_slice(super::http_websocket::WS_GUID);
    let digest = gossamer_std::crypto::insecure::sha1(&input);
    let token = gossamer_std::encoding::base64::encode(&digest);

    Ok(ok_variant(upgrade_response(&token)))
}
