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
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeResult, Value, dense_map};

/// Entry point invoked from `builtins::install`.
use super::*;

pub(crate) fn install_encoding_yaml(globals: &mut Vec<(&'static str, Value)>) {
    // Register only qualified names - bare `parse` and `encode` shadow
    // built-in string/value method dispatch.
    for (short, call) in [
        ("parse", builtin_yaml_parse as BuiltinFnPub),
        ("parse_all", builtin_yaml_parse_all),
        ("encode", builtin_yaml_encode),
    ] {
        let q: &'static str = Box::leak(format!("encoding::yaml::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
    // Text-shaped converters mirroring `toml::to_json` / `from_json`.
    // The auto-derive synthesizer composes these with `from_json` to
    // emit `<Type>::from_yaml` / `to_yaml` without per-tier glue.
    for (short, call) in [
        ("to_json", builtin_yaml_to_json as BuiltinFnPub),
        ("from_json", builtin_yaml_from_json),
        ("is_valid", builtin_yaml_is_valid),
    ] {
        let q: &'static str = Box::leak(format!("encoding::yaml::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
        let q: &'static str = Box::leak(format!("yaml::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
}

pub(crate) fn builtin_yaml_to_json(args: &[Value]) -> RuntimeResult<Value> {
    let text = args.first().and_then(as_str).unwrap_or("");
    Ok(match gossamer_std::encoding::yaml::to_json(text) {
        Ok(s) => ok_variant(Value::String(s.into())),
        Err(e) => err_variant(format!("yaml::to_json: {e}")),
    })
}

pub(crate) fn builtin_yaml_from_json(args: &[Value]) -> RuntimeResult<Value> {
    let text = args.first().and_then(as_str).unwrap_or("");
    Ok(match gossamer_std::encoding::yaml::from_json(text) {
        Ok(s) => ok_variant(Value::String(s.into())),
        Err(e) => err_variant(format!("yaml::from_json: {e}")),
    })
}

pub(crate) fn builtin_yaml_is_valid(args: &[Value]) -> RuntimeResult<Value> {
    let text = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::Bool(
        gossamer_std::encoding::yaml::parse(text).is_ok(),
    ))
}

pub(crate) fn yaml_value_to_gossamer(v: gossamer_std::encoding::yaml::Value) -> Value {
    use gossamer_std::encoding::yaml::Value as YV;
    match v {
        YV::Null => Value::Unit,
        YV::Bool(b) => Value::Bool(b),
        YV::Int(n) => Value::Int(n),
        YV::Float(f) => Value::Float(f),
        YV::String(s) => Value::String(s.into()),
        YV::Seq(seq) => Value::Array(Arc::new(
            seq.into_iter().map(yaml_value_to_gossamer).collect(),
        )),
        YV::Map(pairs) => {
            let mut hmap = dense_map();
            for (k, v) in pairs {
                let key = match k {
                    YV::String(s) => MapKey::Str(s.into()),
                    YV::Int(n) => MapKey::Int(n),
                    YV::Bool(b) => MapKey::Bool(b),
                    _ => MapKey::NonHashable,
                };
                hmap.insert(key, yaml_value_to_gossamer(v));
            }
            Value::Map(Arc::new(parking_lot::Mutex::new(hmap.into())))
        }
    }
}

pub(crate) fn gossamer_value_to_yaml(v: &Value) -> gossamer_std::encoding::yaml::Value {
    use gossamer_std::encoding::yaml::Value as YV;
    match v {
        Value::Unit => YV::Null,
        Value::Bool(b) => YV::Bool(*b),
        Value::Int(n) => YV::Int(*n),
        Value::Float(f) => YV::Float(*f),
        Value::String(s) => YV::String(s.as_str().to_string()),
        Value::Array(arr) => YV::Seq(arr.iter().map(gossamer_value_to_yaml).collect()),
        Value::Json(json) => json_value_to_yaml(json.as_value()),
        Value::Map(map) => {
            let guard = map.lock();
            let pairs: Vec<_> = guard
                .iter()
                .map(|(k, v)| {
                    let yk = match k {
                        MapKey::Str(s) => YV::String(s.to_string()),
                        MapKey::Int(n) => YV::Int(*n),
                        MapKey::Bool(b) => YV::Bool(*b),
                        _ => YV::Null,
                    };
                    (yk, gossamer_value_to_yaml(v))
                })
                .collect();
            YV::Map(pairs)
        }
        _ => YV::Null,
    }
}

fn json_value_to_yaml(v: &gossamer_std::json::Value) -> gossamer_std::encoding::yaml::Value {
    use gossamer_std::encoding::yaml::Value as YV;
    use gossamer_std::json::Value as JV;
    match v {
        JV::Null => YV::Null,
        JV::Bool(b) => YV::Bool(*b),
        JV::Int(n) => YV::Int(*n),
        // YAML's integer is signed, so an unsigned value past its range keeps
        // its magnitude as a float.
        JV::Uint(n) => YV::Float(*n as f64),
        JV::Number(f) => YV::Float(*f),
        JV::String(s) => YV::String(s.clone()),
        JV::Array(items) => YV::Seq(items.iter().map(json_value_to_yaml).collect()),
        JV::Object(map) => YV::Map(
            map.iter()
                .map(|(k, v)| (YV::String(k.clone()), json_value_to_yaml(v)))
                .collect(),
        ),
    }
}

pub(crate) fn builtin_yaml_parse(args: &[Value]) -> RuntimeResult<Value> {
    let src = args.first().and_then(as_str).unwrap_or("");
    // Project YAML onto the JSON value tree so `yaml::parse` returns a
    // `json::Value` - the same dynamic-document type the compiled tier
    // produces (`gos_rt_yaml_parse`), keeping the surface bit-identical.
    match gossamer_std::encoding::yaml::to_json(src) {
        Ok(json_text) => match gossamer_std::json::parse(&json_text) {
            Ok(v) => Ok(ok_variant(
                crate::stdlib_builtins::json_builtins::json_std_to_value(v),
            )),
            Err(e) => Ok(err_variant(format!("yaml::parse: {e}"))),
        },
        Err(e) => Ok(err_variant(format!("yaml::parse: {e}"))),
    }
}

pub(crate) fn builtin_yaml_parse_all(args: &[Value]) -> RuntimeResult<Value> {
    let src = args.first().and_then(as_str).unwrap_or("");
    match gossamer_std::encoding::yaml::parse_all(src) {
        Ok(vs) => {
            let arr = Value::Array(Arc::new(
                vs.into_iter().map(yaml_value_to_gossamer).collect(),
            ));
            Ok(ok_variant(arr))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_yaml_encode(args: &[Value]) -> RuntimeResult<Value> {
    let yv = gossamer_value_to_yaml(args.first().unwrap_or(&Value::Unit));
    match gossamer_std::encoding::yaml::encode(&yv) {
        Ok(s) => Ok(ok_variant(Value::String(s.into()))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

// ----------------------------------------------------------------------
// compress (gzip / flate / zlib)

pub(crate) fn bytes_to_array(v: Vec<u8>) -> Value {
    Value::Array(Arc::new(
        v.into_iter().map(|b| Value::Int(i64::from(b))).collect(),
    ))
}
