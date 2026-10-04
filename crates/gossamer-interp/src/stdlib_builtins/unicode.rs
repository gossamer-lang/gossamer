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

use super::string_array;

/// Entry point invoked from `builtins::install`.
use super::*;

pub(crate) fn install_unicode(globals: &mut Vec<(&'static str, Value)>) {
    install_module_pub(
        "unicode",
        &[
            ("is_letter", builtin_unicode_is_letter),
            ("char_width", builtin_unicode_char_width),
            ("str_width", builtin_unicode_str_width),
            ("is_digit", builtin_unicode_is_digit),
            ("is_number", builtin_unicode_is_number),
            ("is_space", builtin_unicode_is_space),
            ("is_upper", builtin_unicode_is_upper),
            ("is_lower", builtin_unicode_is_lower),
            ("is_title", builtin_unicode_is_title),
            ("is_punct", builtin_unicode_is_punct),
            ("is_symbol", builtin_unicode_is_symbol),
            ("is_mark", builtin_unicode_is_mark),
            ("is_print", builtin_unicode_is_print),
            ("is_graphic", builtin_unicode_is_graphic),
            ("is_control", builtin_unicode_is_control),
            ("is_assigned", builtin_unicode_is_assigned),
            ("to_upper", builtin_unicode_to_upper),
            ("to_lower", builtin_unicode_to_lower),
            ("to_title", builtin_unicode_to_title),
            ("simple_fold", builtin_unicode_simple_fold),
            ("combining_class", builtin_unicode_combining_class),
            ("to_upper_str", builtin_unicode_to_upper_str),
            ("to_lower_str", builtin_unicode_to_lower_str),
            ("fold_case", builtin_unicode_fold_case),
            ("nfc", builtin_unicode_nfc),
            ("nfd", builtin_unicode_nfd),
            ("nfkc", builtin_unicode_nfkc),
            ("nfkd", builtin_unicode_nfkd),
            ("is_nfc", builtin_unicode_is_nfc),
            ("is_nfd", builtin_unicode_is_nfd),
            ("is_nfkc", builtin_unicode_is_nfkc),
            ("is_nfkd", builtin_unicode_is_nfkd),
            ("graphemes", builtin_unicode_graphemes),
            ("grapheme_count", builtin_unicode_grapheme_count),
            ("words", builtin_unicode_words),
            ("word_bounds", builtin_unicode_word_bounds),
            ("word_count", builtin_unicode_word_count),
            ("sentences", builtin_unicode_sentences),
            ("sentence_count", builtin_unicode_sentence_count),
        ],
        globals,
    );
}

pub(crate) fn builtin_unicode_char_width(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(unicode_std::char_width(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_str_width(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(unicode_std::str_width(&arg_str(args, 0))))
}
pub(crate) fn builtin_unicode_is_letter(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_letter(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_digit(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_digit(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_number(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_number(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_space(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_space(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_upper(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_upper(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_lower(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_lower(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_title(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_title(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_punct(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_punct(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_symbol(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_symbol(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_mark(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_mark(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_print(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_print(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_graphic(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_graphic(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_control(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_control(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_to_upper(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Char(unicode_std::to_upper(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_to_lower(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Char(unicode_std::to_lower(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_to_title(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Char(unicode_std::to_title(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_simple_fold(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Char(unicode_std::simple_fold(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_is_assigned(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_assigned(arg_char(args, 0))))
}
pub(crate) fn builtin_unicode_combining_class(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(unicode_std::combining_class(arg_char(args, 0))))
}

fn arg_str(args: &[Value], idx: usize) -> String {
    match args.get(idx) {
        Some(Value::String(s)) => s.as_str().to_string(),
        Some(Value::Char(c)) => c.to_string(),
        _ => String::new(),
    }
}

pub(crate) fn builtin_unicode_to_upper_str(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        unicode_std::to_upper_str(&arg_str(args, 0)).into(),
    ))
}
pub(crate) fn builtin_unicode_to_lower_str(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        unicode_std::to_lower_str(&arg_str(args, 0)).into(),
    ))
}
pub(crate) fn builtin_unicode_fold_case(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        unicode_std::fold_case(&arg_str(args, 0)).into(),
    ))
}

pub(crate) fn builtin_unicode_nfc(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(unicode_std::nfc(&arg_str(args, 0)).into()))
}
pub(crate) fn builtin_unicode_nfd(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(unicode_std::nfd(&arg_str(args, 0)).into()))
}
pub(crate) fn builtin_unicode_nfkc(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(unicode_std::nfkc(&arg_str(args, 0)).into()))
}
pub(crate) fn builtin_unicode_nfkd(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(unicode_std::nfkd(&arg_str(args, 0)).into()))
}
pub(crate) fn builtin_unicode_is_nfc(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_nfc(&arg_str(args, 0))))
}
pub(crate) fn builtin_unicode_is_nfd(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_nfd(&arg_str(args, 0))))
}
pub(crate) fn builtin_unicode_is_nfkc(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_nfkc(&arg_str(args, 0))))
}
pub(crate) fn builtin_unicode_is_nfkd(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(unicode_std::is_nfkd(&arg_str(args, 0))))
}

pub(crate) fn builtin_unicode_graphemes(args: &[Value]) -> RuntimeResult<Value> {
    Ok(string_array(unicode_std::graphemes(&arg_str(args, 0))))
}
pub(crate) fn builtin_unicode_grapheme_count(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(unicode_std::grapheme_count(&arg_str(args, 0))))
}
pub(crate) fn builtin_unicode_words(args: &[Value]) -> RuntimeResult<Value> {
    Ok(string_array(unicode_std::words(&arg_str(args, 0))))
}
pub(crate) fn builtin_unicode_word_bounds(args: &[Value]) -> RuntimeResult<Value> {
    Ok(string_array(unicode_std::word_bounds(&arg_str(args, 0))))
}
pub(crate) fn builtin_unicode_word_count(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(unicode_std::word_count(&arg_str(args, 0))))
}
pub(crate) fn builtin_unicode_sentences(args: &[Value]) -> RuntimeResult<Value> {
    Ok(string_array(unicode_std::sentences(&arg_str(args, 0))))
}
pub(crate) fn builtin_unicode_sentence_count(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(unicode_std::sentence_count(&arg_str(args, 0))))
}

// ----------------------------------------------------------------------
// encoding::binary (extended)
