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
use std::sync::atomic::{AtomicBool as StdAtomicBool, AtomicI64 as StdAtomicI64, Ordering};

use crate::value::{JsonInner, SmolStr};

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
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeResult, Value, dense_map};

/// Entry point invoked from `builtins::install`.
use super::*;

pub(crate) fn install_json_builtins(globals: &mut Vec<(&'static str, Value)>) {
    // Only register under encoding::json:: prefix to avoid shadowing the
    // existing json:: builtins in builtins.rs (which carry json::get / as_str
    // / the JsonValue ecosystem). Code that writes
    // `use std::encoding` and calls `encoding::json::parse` uses these.
    for (short, call) in [
        ("parse", builtin_json_std_parse as BuiltinFnPub),
        ("encode", builtin_json_std_encode),
        ("encode_pretty", builtin_json_std_encode_pretty),
        ("valid", builtin_json_std_valid),
    ] {
        let q: &'static str = Box::leak(format!("encoding::json::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
}

pub(crate) fn json_std_to_value(v: gossamer_std::json::Value) -> Value {
    use gossamer_std::json::Value as JV;
    match v {
        JV::Null => Value::Unit,
        JV::Bool(b) => Value::Bool(b),
        // `Int` and `Number` are distinct so an integer round-trips
        // exactly and a float (including integer-valued like `2.0`)
        // round-trips as a float, matching the serde-backed compiled tier.
        JV::Int(n) => Value::Int(n),
        JV::Number(f) => Value::Float(f),
        JV::String(s) => Value::String(s.into()),
        JV::Array(arr) => Value::Array(Arc::new(arr.into_iter().map(json_std_to_value).collect())),
        JV::Object(map) => {
            let mut hmap = dense_map();
            for (k, v) in map {
                hmap.insert(MapKey::Str(k.into()), json_std_to_value(v));
            }
            Value::Map(Arc::new(parking_lot::Mutex::new(hmap)))
        }
    }
}

pub(crate) fn builtin_json_std_parse(args: &[Value]) -> RuntimeResult<Value> {
    let src = args.first().and_then(as_str).unwrap_or("");
    match gossamer_std::json::parse(src) {
        // Preserve the stdlib parser's tree. The former conversion to
        // `Value::Map` / `Value::Array` was immediately undone by encode in
        // the common parse-render path, doubling the live document and its
        // allocation traffic.
        Ok(v) => Ok(ok_variant(Value::Json(Arc::new(JsonInner::new(v))))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_json_std_encode(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        return Ok(Value::String(
            gossamer_std::json::encode(value.as_value()).into(),
        ));
    }
    let jv = crate::builtins::gossamer_to_json_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::String(gossamer_std::json::encode(&jv).into()))
}

pub(crate) fn builtin_json_std_encode_pretty(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        return Ok(Value::String(
            gossamer_std::json::encode_pretty(value.as_value()).into(),
        ));
    }
    let jv = crate::builtins::gossamer_to_json_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::String(gossamer_std::json::encode_pretty(&jv).into()))
}

pub(crate) fn builtin_json_std_valid(args: &[Value]) -> RuntimeResult<Value> {
    let src = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::Bool(gossamer_std::json::parse(src).is_ok()))
}

// ----------------------------------------------------------------------
// time completeness (sleep, now, unix_ms, format_rfc3339, parse_rfc3339)
