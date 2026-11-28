#![allow(
    unused_imports,
    dead_code,
    clippy::wildcard_imports,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::needless_range_loop,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::module_name_repetitions
)]
//! Bytecode-VM builtins for `std::http::cookie`, matching the compiled-tier
//! shims in `gossamer-runtime/src/c_abi/http_security.rs` so every tier
//! answers the same bytes. They delegate to `gossamer_std::http_cookie`.

use std::sync::Arc;

use crate::builtins::{BuiltinFnPub, as_str, err_variant, ok_variant};
use crate::value::{RuntimeResult, Value};

use super::*;

/// Entry point invoked from `stdlib_builtins::install`.
pub(crate) fn install_http_security(globals: &mut Vec<(&'static str, Value)>) {
    for (path, call) in [
        (
            "http::cookie::parse_cookie_header",
            builtin_cookie_parse_header as BuiltinFnPub,
        ),
        ("http::cookie::serialize", builtin_cookie_serialize),
    ] {
        globals.push((path, crate::builtins::builtin_pub(path, call)));
    }
}

// -- cookie --------------------------------------------------------------

pub(crate) fn builtin_cookie_parse_header(args: &[Value]) -> RuntimeResult<Value> {
    let header = as_str(args.first().unwrap_or(&Value::Unit)).unwrap_or("");
    let pairs = gossamer_std::http_cookie::parse_cookie_header(header);
    let items: Vec<Value> = pairs
        .into_iter()
        .map(|(name, value)| {
            Value::Tuple(Arc::from(vec![
                Value::String(name.into()),
                Value::String(value.into()),
            ]))
        })
        .collect();
    Ok(Value::Array(Arc::new(items)))
}

pub(crate) fn builtin_cookie_serialize(args: &[Value]) -> RuntimeResult<Value> {
    let name = as_str(args.first().unwrap_or(&Value::Unit)).unwrap_or("");
    let value = as_str(args.get(1).unwrap_or(&Value::Unit)).unwrap_or("");
    let header = gossamer_std::http_cookie::Cookie::new(name, value).to_header_value();
    Ok(Value::String(header.into()))
}
