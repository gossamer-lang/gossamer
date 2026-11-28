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

pub(crate) fn install_html(globals: &mut Vec<(&'static str, Value)>) {
    install_module_pub(
        "html",
        &[
            ("escape", builtin_html_escape),
            ("unescape", builtin_html_unescape),
        ],
        globals,
    );
    for (name, call) in [
        (
            "__gos_mail_address_raw",
            builtin_mail_address_raw as BuiltinFnPub,
        ),
        ("__gos_mail_address_list_raw", builtin_mail_address_list_raw),
        ("__gos_template_new_raw", builtin_template_new_raw),
        ("__gos_template_render_raw", builtin_template_render_raw),
        (
            "__gos_template_render_source_raw",
            builtin_template_render_source_raw,
        ),
        ("__gos_template_release_raw", builtin_template_release_raw),
    ] {
        globals.push((name, crate::builtins::builtin_pub(name, call)));
    }
}

fn int_arg(args: &[Value], n: usize) -> i64 {
    args.get(n)
        .and_then(crate::builtins::value_to_int)
        .unwrap_or(0)
}

/// `__gos_template_new_raw(source, html, strict)`: the id of the parsed template.
pub(crate) fn builtin_template_new_raw(args: &[Value]) -> RuntimeResult<Value> {
    let source = args.first().and_then(as_str).unwrap_or("");
    Ok(
        match gossamer_runtime::templates::new_template(
            source,
            int_arg(args, 1) != 0,
            int_arg(args, 2) != 0,
        ) {
            Ok(id) => ok_variant(Value::Int(id)),
            Err(e) => err_variant(e),
        },
    )
}

/// `__gos_template_render_raw(id, json)`: a parsed template rendered.
pub(crate) fn builtin_template_render_raw(args: &[Value]) -> RuntimeResult<Value> {
    let json = args.get(1).and_then(as_str).unwrap_or("");
    Ok(html_answer(gossamer_runtime::templates::render_template(
        int_arg(args, 0),
        json,
    )))
}

/// `__gos_template_render_source_raw(source, html, json)`: parsed and rendered once.
pub(crate) fn builtin_template_render_source_raw(args: &[Value]) -> RuntimeResult<Value> {
    let source = args.first().and_then(as_str).unwrap_or("");
    let json = args.get(2).and_then(as_str).unwrap_or("");
    Ok(html_answer(gossamer_runtime::templates::render_source(
        source,
        int_arg(args, 1) != 0,
        json,
    )))
}

/// `__gos_template_release_raw(id)`: gives back the parsed template.
pub(crate) fn builtin_template_release_raw(args: &[Value]) -> RuntimeResult<Value> {
    gossamer_runtime::templates::release_template(int_arg(args, 0));
    Ok(ok_variant(Value::Unit))
}

fn html_answer(answer: Result<String, String>) -> Value {
    match answer {
        Ok(json) => ok_variant(Value::String(json.into())),
        Err(e) => err_variant(e),
    }
}

/// `__gos_mail_address_raw(text)`, the leaf under the injected
/// `mail::parse_address`.
pub(crate) fn builtin_mail_address_raw(args: &[Value]) -> RuntimeResult<Value> {
    let text = args.first().and_then(as_str).unwrap_or("");
    Ok(html_answer(gossamer_runtime::mail_address::address_json(
        text,
    )))
}

/// `__gos_mail_address_list_raw(text)`, the leaf under the injected
/// `mail::parse_address_list`.
pub(crate) fn builtin_mail_address_list_raw(args: &[Value]) -> RuntimeResult<Value> {
    let text = args.first().and_then(as_str).unwrap_or("");
    Ok(html_answer(
        gossamer_runtime::mail_address::address_list_json(text),
    ))
}

pub(crate) fn builtin_html_escape(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    Ok(Value::String(gossamer_std::html::escape(&s).into()))
}

pub(crate) fn builtin_html_unescape(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    Ok(Value::String(gossamer_std::html::unescape(&s).into()))
}

// ----------------------------------------------------------------------
// encoding::base64 and encoding::hex (qualified paths under `use std::encoding`)

pub(crate) fn install_encoding_base64_hex(globals: &mut Vec<(&'static str, Value)>) {
    for (short, call) in [
        ("base64::encode", builtin_enc_base64_encode as BuiltinFnPub),
        ("base64::decode", builtin_enc_base64_decode),
        ("hex::encode", builtin_enc_hex_encode),
        ("hex::decode", builtin_enc_hex_decode),
    ] {
        let q: &'static str = Box::leak(format!("encoding::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
        // The `use std::encoding::base64` alias makes the call resolve to the
        // short `base64::encode` / `hex::encode` form; bind it too so the
        // interpreter matches the compiled tiers.
        globals.push((short, crate::builtins::builtin_pub(short, call)));
    }
}

pub(crate) fn builtin_enc_base64_encode(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::String(
        gossamer_std::encoding::base64::encode(&data).into(),
    ))
}

pub(crate) fn builtin_enc_base64_decode(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    match gossamer_std::encoding::base64::decode(&s) {
        Ok(bytes) => Ok(ok_variant(bytes_to_array(bytes))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_enc_hex_encode(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::String(
        gossamer_std::encoding::hex::encode(&data).into(),
    ))
}

pub(crate) fn builtin_enc_hex_decode(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    match gossamer_std::encoding::hex::decode(&s) {
        Ok(bytes) => Ok(ok_variant(bytes_to_array(bytes))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

// ----------------------------------------------------------------------
// encoding::base32

pub(crate) fn install_encoding_base32(globals: &mut Vec<(&'static str, Value)>) {
    for (short, call) in [
        ("encode", builtin_base32_encode as BuiltinFnPub),
        ("decode", builtin_base32_decode),
        ("encode_string", builtin_base32_encode_string),
        ("decode_string", builtin_base32_decode_string),
        ("encode_hex", builtin_base32_encode_hex),
        ("decode_hex", builtin_base32_decode_hex),
    ] {
        let q: &'static str = Box::leak(format!("encoding::base32::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
}

pub(crate) fn builtin_base32_encode(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::String(
        gossamer_std::encoding::base32::encode(&data).into(),
    ))
}

pub(crate) fn builtin_base32_decode(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    match gossamer_std::encoding::base32::decode(&s) {
        Ok(bytes) => Ok(ok_variant(bytes_to_array(bytes))),
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_base32_encode_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    Ok(Value::String(
        gossamer_std::encoding::base32::encode_string(&s).into(),
    ))
}

pub(crate) fn builtin_base32_decode_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    match gossamer_std::encoding::base32::decode_string(&s) {
        Ok(out) => Ok(ok_variant(Value::String(out.into()))),
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_base32_encode_hex(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::String(
        gossamer_std::encoding::base32::encode_hex(&data).into(),
    ))
}

pub(crate) fn builtin_base32_decode_hex(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    match gossamer_std::encoding::base32::decode_hex(&s) {
        Ok(bytes) => Ok(ok_variant(bytes_to_array(bytes))),
        Err(e) => Ok(err_variant(e)),
    }
}

// ----------------------------------------------------------------------
// encoding::ascii85

pub(crate) fn install_encoding_ascii85(globals: &mut Vec<(&'static str, Value)>) {
    for (short, call) in [
        ("encode", builtin_ascii85_encode as BuiltinFnPub),
        ("decode", builtin_ascii85_decode),
    ] {
        let q: &'static str = Box::leak(format!("encoding::ascii85::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
}

pub(crate) fn builtin_ascii85_encode(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    Ok(Value::String(
        gossamer_std::encoding::ascii85::encode(&data).into(),
    ))
}

pub(crate) fn builtin_ascii85_decode(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    match gossamer_std::encoding::ascii85::decode(&s) {
        Ok(bytes) => Ok(ok_variant(bytes_to_array(bytes))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

// ----------------------------------------------------------------------
// encoding::xml
