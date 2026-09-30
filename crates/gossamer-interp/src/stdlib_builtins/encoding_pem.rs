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

pub(crate) fn install_encoding_pem(globals: &mut Vec<(&'static str, Value)>) {
    install_module_pub(
        "encoding::pem",
        &[
            ("encode", builtin_pem_encode),
            ("decode", builtin_pem_decode),
            ("decode_all", builtin_pem_decode_all),
        ],
        globals,
    );
    // Leaf intrinsics for the injected-source `Block` wrappers
    // (gossamer-parse autoderive). They return tuples / Vec-of-tuples
    // / String - the wrappers fold those into real `Block` structs,
    // so the same Gossamer code runs on every tier.
    for (name, call) in [
        (
            "__gos_pem_decode_raw",
            builtin_pem_decode_raw as BuiltinFnPub,
        ),
        ("__gos_pem_decode_all_raw", builtin_pem_decode_all_raw),
        ("__gos_pem_encode_raw", builtin_pem_encode_raw),
    ] {
        globals.push((name, crate::builtins::builtin_pub(name, call)));
    }
}

pub(crate) fn builtin_pem_decode_raw(args: &[Value]) -> RuntimeResult<Value> {
    let input = args.first().and_then(as_str).unwrap_or("");
    match gossamer_std::encoding::pem::decode(input) {
        Ok((block, _rest)) => {
            let bytes_val = Value::Array(Arc::new(
                block
                    .bytes
                    .into_iter()
                    .map(|b| Value::Int(i64::from(b)))
                    .collect(),
            ));
            Ok(ok_variant(Value::Tuple(Arc::from(vec![
                Value::String(block.block_type.into()),
                bytes_val,
            ]))))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_pem_decode_all_raw(args: &[Value]) -> RuntimeResult<Value> {
    let input = args.first().and_then(as_str).unwrap_or("");
    match gossamer_std::encoding::pem::decode_all(input) {
        Ok(blocks) => {
            let values: Vec<Value> = blocks
                .into_iter()
                .map(|block| {
                    let bytes_val = Value::Array(Arc::new(
                        block
                            .bytes
                            .into_iter()
                            .map(|b| Value::Int(i64::from(b)))
                            .collect(),
                    ));
                    Value::Tuple(Arc::from(vec![
                        Value::String(block.block_type.into()),
                        bytes_val,
                    ]))
                })
                .collect();
            Ok(ok_variant(Value::Array(Arc::new(values))))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_pem_encode_raw(args: &[Value]) -> RuntimeResult<Value> {
    let block_type = args.first().and_then(as_str).unwrap_or("").to_string();
    let bytes: Vec<u8> = args.get(1).map(Value::bytes_or_empty).unwrap_or_default();
    let block = gossamer_std::encoding::pem::Block { block_type, bytes };
    Ok(Value::String(
        gossamer_std::encoding::pem::encode(&block).into(),
    ))
}

pub(crate) fn pem_block_to_value(block: gossamer_std::encoding::pem::Block) -> Value {
    let bytes_val = Value::Array(Arc::new(
        block
            .bytes
            .into_iter()
            .map(|b| Value::Int(i64::from(b)))
            .collect(),
    ));
    Value::struct_(
        "encoding::pem::Block",
        Arc::unwrap_or_clone(Arc::new(vec![
            ("block_type", Value::String(block.block_type.into())),
            ("bytes", bytes_val),
        ])),
    )
}

pub(crate) fn pem_block_from_value(v: &Value) -> gossamer_std::encoding::pem::Block {
    let (mut block_type, mut bytes) = (String::new(), Vec::new());
    if let Value::Struct(s) = v {
        for (k, val) in &s.fields {
            match *k {
                "block_type" => {
                    if let Value::String(s) = val {
                        block_type = s.as_str().to_string();
                    }
                }
                "bytes" => bytes = val.bytes_or_empty(),
                _ => {}
            }
        }
    }
    gossamer_std::encoding::pem::Block { block_type, bytes }
}

pub(crate) fn builtin_pem_encode(args: &[Value]) -> RuntimeResult<Value> {
    let block = pem_block_from_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::String(
        gossamer_std::encoding::pem::encode(&block).into(),
    ))
}

pub(crate) fn builtin_pem_decode(args: &[Value]) -> RuntimeResult<Value> {
    let input = args.first().and_then(as_str).unwrap_or("");
    match gossamer_std::encoding::pem::decode(input) {
        Ok((block, _rest)) => Ok(ok_variant(pem_block_to_value(block))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_pem_decode_all(args: &[Value]) -> RuntimeResult<Value> {
    let input = args.first().and_then(as_str).unwrap_or("");
    match gossamer_std::encoding::pem::decode_all(input) {
        Ok(blocks) => {
            let values: Vec<Value> = blocks.into_iter().map(pem_block_to_value).collect();
            Ok(ok_variant(Value::Array(Arc::new(values))))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

// ----------------------------------------------------------------------
// utf16

pub(crate) fn install_utf16(globals: &mut Vec<(&'static str, Value)>) {
    // Register only qualified `utf16::*` names to avoid shadowing built-in
    // method dispatch (e.g. `rune_len` conflicts with utf8::rune_len).
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("is_surrogate", builtin_utf16_is_surrogate),
        ("rune_len", builtin_utf16_rune_len),
        ("decode_surrogate_pair", builtin_utf16_decode_surrogate_pair),
        ("encode_string", builtin_utf16_encode_string),
        ("decode_to_string", builtin_utf16_decode_to_string),
    ];
    for (short, call) in entries {
        // Register both the bare `utf16::*` form and the fully
        // qualified `encoding::utf16::*` form (matching how the
        // compiled tier resolves `use std::encoding; encoding::utf16::…`
        // and the sibling base32 / ascii85 modules).
        for prefix in ["utf16", "encoding::utf16"] {
            let qualified: &'static str = Box::leak(format!("{prefix}::{short}").into_boxed_str());
            globals.push((qualified, crate::builtins::builtin_pub(qualified, *call)));
        }
    }
}

pub(crate) fn builtin_utf16_is_surrogate(args: &[Value]) -> RuntimeResult<Value> {
    let r = args.first().and_then(value_to_int).unwrap_or(0);
    Ok(Value::Bool(utf16_std::is_surrogate(r as u16)))
}

pub(crate) fn builtin_utf16_rune_len(args: &[Value]) -> RuntimeResult<Value> {
    let ch = arg_char(args, 0);
    Ok(Value::Int(utf16_std::rune_len(ch) as i64))
}

pub(crate) fn builtin_utf16_decode_surrogate_pair(args: &[Value]) -> RuntimeResult<Value> {
    let high = args.first().and_then(value_to_int).unwrap_or(0) as u16;
    let low = args.get(1).and_then(value_to_int).unwrap_or(0) as u16;
    match utf16_std::decode_surrogate_pair(high, low) {
        Some(ch) => Ok(some_variant(Value::Char(ch))),
        None => Ok(none_variant()),
    }
}

pub(crate) fn builtin_utf16_encode_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    let units = utf16_std::encode_string(s);
    Ok(Value::Array(Arc::new(
        units
            .into_iter()
            .map(|u| Value::Int(i64::from(u)))
            .collect(),
    )))
}

pub(crate) fn builtin_utf16_decode_to_string(args: &[Value]) -> RuntimeResult<Value> {
    let units: Vec<u16> = match args.first() {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| value_to_int(v).map(|n| n as u16))
            .collect(),
        _ => Vec::new(),
    };
    Ok(Value::String(utf16_std::decode_to_string(&units).into()))
}

// ----------------------------------------------------------------------
// iter

/// Helper to extract elements from a `Value::Array` / `Value::IntArray`.
pub(crate) fn collect_array(v: &Value) -> Vec<Value> {
    if let Some(items) = crate::stdlib_builtins::iter::drain_lazy_iter(v) {
        return items;
    }
    match v {
        Value::Array(arr) => arr.as_ref().clone(),
        Value::IntArray(arr) => arr.iter().map(|&n| Value::Int(n)).collect(),
        Value::FloatVec(arr) => arr.iter().map(|&f| Value::Float(f)).collect(),
        Value::FloatArray(_) => v.float_array_elems(),
        // Byte-packed storage holds a `u8` sequence, whose elements are
        // integers like any other numeric sequence's.
        Value::ByteArray(bytes) => bytes.iter().map(|b| Value::Int(i64::from(*b))).collect(),
        Value::InlineByteArray(bytes) => bytes.iter().map(|&b| Value::Int(i64::from(b))).collect(),
        Value::ByteVec(bytes) => bytes.iter().map(|&b| Value::Int(i64::from(b))).collect(),
        // A map or set holds its values, so a traversal over one sees the
        // same elements its `iter()` yields, in the same order.
        Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_) => {
            crate::stdlib_builtins::iter::materialized_keyed_elements(v)
        }
        _ if crate::stdlib_builtins::set::set_snapshot(v).is_some() => {
            crate::stdlib_builtins::set::set_snapshot(v).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}
