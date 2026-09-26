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
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeResult, Value};

/// Entry point invoked from `builtins::install`.
use super::*;

pub(crate) fn install_strconv(globals: &mut Vec<(&'static str, Value)>) {
    install_module_pub(
        "strconv",
        &[
            ("parse_i64", builtin_strconv_parse_i64),
            ("parse_u64", builtin_strconv_parse_u64),
            ("parse_f64", builtin_strconv_parse_f64),
            ("parse_bool", builtin_strconv_parse_bool),
            ("format_i64", builtin_strconv_format_i64),
            ("format_f64", builtin_strconv_format_f64),
            ("parse_i64_radix", builtin_strconv_parse_i64_radix),
            ("format_i64_radix", builtin_strconv_format_i64_radix),
            ("quote", builtin_strconv_quote),
            ("unquote", builtin_strconv_unquote),
        ],
        globals,
    );
    globals.push((
        "__gos_debug_quote",
        Value::builtin("__gos_debug_quote", builtin_debug_quote),
    ));
    globals.push((
        "__gos_f32_display",
        Value::builtin("__gos_f32_display", builtin_f32_display),
    ));
    globals.push((
        "__gos_f32_debug",
        Value::builtin("__gos_f32_debug", builtin_f32_debug),
    ));
    globals.push((
        "__gos_dyn_display",
        Value::builtin("__gos_dyn_display", builtin_dyn_display),
    ));
    globals.push((
        "__gos_dyn_debug",
        Value::builtin("__gos_dyn_debug", builtin_dyn_debug),
    ));
}

/// `{}` of a `DynValue`: the value it holds, rendered as that value is.
pub(crate) fn builtin_dyn_display(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        args.first()
            .map(ToString::to_string)
            .unwrap_or_default()
            .into(),
    ))
}

/// `{:?}` of a `DynValue`: a string or char in the spelling that builds it,
/// a float with its fractional part, anything else as `{}` shows it.
pub(crate) fn builtin_dyn_debug(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        match args.first() {
            Some(Value::String(s)) => format!("{:?}", s.as_str()),
            Some(Value::Char(c)) => format!("{c:?}"),
            Some(Value::Float(f)) => gossamer_runtime::builtins::format_float_debug(*f),
            Some(other) => other.to_string(),
            None => String::new(),
        }
        .into(),
    ))
}

fn f32_slot(args: &[Value]) -> f64 {
    match args.first() {
        Some(Value::Float(f)) => *f,
        Some(other) => other.as_i64().map_or(0.0, |n| n as f64),
        None => 0.0,
    }
}

/// `{}` of an `f32`: the shortest digits of the single-precision value.
pub(crate) fn builtin_f32_display(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        gossamer_runtime::builtins::format_f32(f32_slot(args)).into(),
    ))
}

/// `{:?}` of an `f32`.
pub(crate) fn builtin_f32_debug(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        gossamer_runtime::builtins::format_f32_debug(f32_slot(args)).into(),
    ))
}

/// `{:?}` of a `String` or `char`: Rust's `Debug` quoting, as every tier
/// renders one at the top level and nested in a container.
pub(crate) fn builtin_debug_quote(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        match args.first() {
            Some(Value::String(s)) => format!("{:?}", s.as_str()),
            Some(Value::Char(c)) => format!("{c:?}"),
            Some(other) => format!("{other:?}"),
            None => String::new(),
        }
        .into(),
    ))
}

pub(crate) fn builtin_strconv_parse_i64_radix(args: &[Value]) -> RuntimeResult<Value> {
    let text = match arg_str_at(args, 0, "strconv::parse_i64_radix", "argument") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let base = args.get(1).and_then(value_to_int).unwrap_or(10);
    let radix = u32::try_from(base).unwrap_or(0);
    match strconv_std::parse_i64_radix(&text, radix) {
        Ok(n) => Ok(ok_variant(Value::Int(n))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_strconv_format_i64_radix(args: &[Value]) -> RuntimeResult<Value> {
    let n = args.first().and_then(value_to_int).unwrap_or(0);
    let base = args.get(1).and_then(value_to_int).unwrap_or(10);
    let radix = u32::try_from(base).unwrap_or(10);
    Ok(Value::String(
        strconv_std::format_i64_radix(n, radix).into(),
    ))
}

pub(crate) fn builtin_strconv_quote(args: &[Value]) -> RuntimeResult<Value> {
    let text = match arg_str_at(args, 0, "strconv::quote", "argument") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    Ok(Value::String(strconv_std::quote(&text).into()))
}

pub(crate) fn builtin_strconv_unquote(args: &[Value]) -> RuntimeResult<Value> {
    let text = match arg_str_at(args, 0, "strconv::unquote", "argument") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    match strconv_std::unquote(&text) {
        Ok(s) => Ok(ok_variant(Value::String(s.into()))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_strconv_parse_i64(args: &[Value]) -> RuntimeResult<Value> {
    let text = match arg_str_at(args, 0, "strconv::parse_i64", "argument") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    match strconv_std::parse_i64(&text) {
        Ok(n) => Ok(ok_variant(Value::Int(n))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_strconv_parse_u64(args: &[Value]) -> RuntimeResult<Value> {
    let text = match arg_str_at(args, 0, "strconv::parse_u64", "argument") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    match strconv_std::parse_u64(&text) {
        Ok(n) => Ok(ok_variant(Value::Uint(n))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_strconv_parse_f64(args: &[Value]) -> RuntimeResult<Value> {
    let text = match arg_str_at(args, 0, "strconv::parse_f64", "argument") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    match strconv_std::parse_f64(&text) {
        Ok(n) => Ok(ok_variant(Value::Float(n))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_strconv_parse_bool(args: &[Value]) -> RuntimeResult<Value> {
    let text = match arg_str_at(args, 0, "strconv::parse_bool", "argument") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    match strconv_std::parse_bool(&text) {
        Ok(b) => Ok(ok_variant(Value::Bool(b))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_strconv_format_i64(args: &[Value]) -> RuntimeResult<Value> {
    let n = args.first().and_then(value_to_int).unwrap_or(0);
    Ok(Value::String(strconv_std::format_i64(n).into()))
}

pub(crate) fn builtin_strconv_format_f64(args: &[Value]) -> RuntimeResult<Value> {
    let f = match args.first() {
        Some(Value::Float(f)) => *f,
        Some(Value::Int(n)) => *n as f64,
        _ => 0.0,
    };
    Ok(Value::String(strconv_std::format_f64(f).into()))
}

// ----------------------------------------------------------------------
// path
