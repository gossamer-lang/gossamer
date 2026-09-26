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

pub(crate) fn install_hash_fnv(globals: &mut Vec<(&'static str, Value)>) {
    for (short, call) in [
        ("fnv::hash64", builtin_hash_fnv_hash64 as BuiltinFnPub),
        ("fnv::hash32", builtin_hash_fnv_hash32),
        ("fnv::hash_string", builtin_hash_fnv_hash_string),
    ] {
        let q: &'static str = Box::leak(format!("hash::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
}

// ----------------------------------------------------------------------
// hash::crc32 and hash::adler32

pub(crate) fn install_hash_crc32_adler32(globals: &mut Vec<(&'static str, Value)>) {
    for (short, call) in [
        (
            "crc32::checksum",
            builtin_hash_crc32_checksum as BuiltinFnPub,
        ),
        ("crc32::checksum_string", builtin_hash_crc32_checksum_string),
        ("crc32::update", builtin_hash_crc32_update),
        ("crc32::update_window", builtin_hash_crc32_update_window),
        ("crc32c::checksum", builtin_hash_crc32c_checksum),
        (
            "crc32c::checksum_string",
            builtin_hash_crc32c_checksum_string,
        ),
        ("crc32c::update", builtin_hash_crc32c_update),
        ("crc32c::update_window", builtin_hash_crc32c_update_window),
        ("adler32::checksum", builtin_hash_adler32_checksum),
        (
            "adler32::checksum_string",
            builtin_hash_adler32_checksum_string,
        ),
        ("adler32::update", builtin_hash_adler32_update),
    ] {
        let q: &'static str = Box::leak(format!("hash::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
}

pub(crate) fn builtin_hash_crc32_checksum(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::Int(i64::from(gossamer_std::hash::crc32::checksum(
        &data,
    ))))
}

pub(crate) fn builtin_hash_crc32_checksum_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    Ok(Value::Int(i64::from(
        gossamer_std::hash::crc32::checksum_string(&s),
    )))
}

pub(crate) fn builtin_hash_crc32_update(args: &[Value]) -> RuntimeResult<Value> {
    let crc = args.first().and_then(value_to_int).unwrap_or(0) as u32;
    let data = bytes_from_value(args.get(1).unwrap_or(&Value::Unit));
    Ok(Value::Int(i64::from(gossamer_std::hash::crc32::update(
        crc, &data,
    ))))
}

pub(crate) fn builtin_hash_crc32_update_window(args: &[Value]) -> RuntimeResult<Value> {
    let crc = args.first().and_then(value_to_int).unwrap_or(0) as u32;
    let data = args.get(1).unwrap_or(&Value::Unit);
    let start = args.get(2).and_then(value_to_int).unwrap_or(-1);
    let end = args.get(3).and_then(value_to_int).unwrap_or(-1);
    if start < 0 || end < start {
        return Ok(Value::Int(i64::from(crc)));
    }
    // The window is what the checksum reads, so the window is what is
    // gathered: continuing a checksum over one record of a resident file
    // costs that record rather than the file.
    let Some(window) = data.byte_window(start as usize, end as usize) else {
        return Ok(Value::Int(i64::from(crc)));
    };
    Ok(Value::Int(i64::from(gossamer_std::hash::crc32::update(
        crc, &window,
    ))))
}

pub(crate) fn builtin_hash_crc32c_checksum(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::Int(i64::from(gossamer_std::hash::crc32c::checksum(
        &data,
    ))))
}

pub(crate) fn builtin_hash_crc32c_checksum_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    Ok(Value::Int(i64::from(
        gossamer_std::hash::crc32c::checksum_string(&s),
    )))
}

pub(crate) fn builtin_hash_crc32c_update(args: &[Value]) -> RuntimeResult<Value> {
    let crc = args.first().and_then(value_to_int).unwrap_or(0) as u32;
    let data = bytes_from_value(args.get(1).unwrap_or(&Value::Unit));
    Ok(Value::Int(i64::from(gossamer_std::hash::crc32c::update(
        crc, &data,
    ))))
}

pub(crate) fn builtin_hash_crc32c_update_window(args: &[Value]) -> RuntimeResult<Value> {
    let crc = args.first().and_then(value_to_int).unwrap_or(0) as u32;
    let data = args.get(1).unwrap_or(&Value::Unit);
    let start = args.get(2).and_then(value_to_int).unwrap_or(-1);
    let end = args.get(3).and_then(value_to_int).unwrap_or(-1);
    if start < 0 || end < start {
        return Ok(Value::Int(i64::from(crc)));
    }
    let Some(window) = data.byte_window(start as usize, end as usize) else {
        return Ok(Value::Int(i64::from(crc)));
    };
    Ok(Value::Int(i64::from(gossamer_std::hash::crc32c::update(
        crc, &window,
    ))))
}

pub(crate) fn builtin_hash_adler32_checksum(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::Int(i64::from(
        gossamer_std::hash::adler32::checksum(&data),
    )))
}

pub(crate) fn builtin_hash_adler32_checksum_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    Ok(Value::Int(i64::from(
        gossamer_std::hash::adler32::checksum_string(&s),
    )))
}

pub(crate) fn builtin_hash_adler32_update(args: &[Value]) -> RuntimeResult<Value> {
    let adler = args.first().and_then(value_to_int).unwrap_or(1) as u32;
    let data = bytes_from_value(args.get(1).unwrap_or(&Value::Unit));
    Ok(Value::Int(i64::from(gossamer_std::hash::adler32::update(
        adler, &data,
    ))))
}

pub(crate) fn builtin_hash_fnv_hash64(args: &[Value]) -> RuntimeResult<Value> {
    let input = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::Uint(gossamer_std::hash::fnv::hash64(&input)))
}

pub(crate) fn builtin_hash_fnv_hash32(args: &[Value]) -> RuntimeResult<Value> {
    let input = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::Int(i64::from(gossamer_std::hash::fnv::hash32(
        &input,
    ))))
}

pub(crate) fn builtin_hash_fnv_hash_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::Uint(gossamer_std::hash::fnv::hash_string(s)))
}

// ----------------------------------------------------------------------
// archive::zip
