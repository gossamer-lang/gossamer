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

pub(crate) fn install_path(globals: &mut Vec<(&'static str, Value)>) {
    install_module_pub(
        "path",
        &[
            ("parent", builtin_path_parent),
            ("file_name", builtin_path_file_name),
            ("file_stem", builtin_path_stem),
            ("extension", builtin_path_ext),
            ("is_absolute", builtin_path_is_absolute),
            ("normalize", builtin_path_normalize),
            ("join", builtin_path_join_two),
            ("components", builtin_path_components),
            ("prefixes", builtin_path_prefixes),
            ("unique_prefixes", builtin_path_unique_prefixes),
            ("starts_with", builtin_path_starts_with),
            ("matches", builtin_path_matches),
            ("glob", builtin_path_glob),
        ],
        globals,
    );
    // Qualified-only `path::split` - install_module_pub above would
    // also register a bare `split` which silently overwrote the
    // string `split`, making `"a\nb".split("\n")` return a tuple.
    let joined: &'static str = "path::split";
    globals.push((
        joined,
        crate::builtins::builtin_pub(joined, builtin_path_split),
    ));
}

pub(crate) fn builtin_path_base(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::String(path_std::base(path).into()))
}

pub(crate) fn builtin_path_dir(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::String(path_std::dir(path).into()))
}

pub(crate) fn builtin_path_join_two(args: &[Value]) -> RuntimeResult<Value> {
    let a = args.first().and_then(as_str).unwrap_or("");
    let b = args.get(1).and_then(as_str).unwrap_or("");
    Ok(Value::String(path_std::join(a, b).into()))
}

pub(crate) fn builtin_path_clean(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::String(path_std::clean(path).into()))
}

pub(crate) fn builtin_path_split(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    let (dir, file) = path_std::split(path);
    Ok(Value::Tuple(Arc::from(vec![
        Value::String(dir.into()),
        Value::String(file.into()),
    ])))
}

pub(crate) fn builtin_path_components(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    Ok(string_array(path_std::components(path)))
}

pub(crate) fn builtin_path_prefixes(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    Ok(string_array(path_std::prefixes(path)))
}

pub(crate) fn builtin_path_unique_prefixes(args: &[Value]) -> RuntimeResult<Value> {
    let text = args.first().and_then(as_str).unwrap_or("");
    Ok(string_array(path_std::unique_prefixes(text)))
}

pub(crate) fn builtin_path_starts_with(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    let prefix = args.get(1).and_then(as_str).unwrap_or("");
    Ok(Value::Bool(path_std::has_prefix(path, prefix)))
}

/// `path::matches(pattern, name) -> bool` - Go `filepath.Match`
/// single-segment shell-glob matching; `*` and `?` never cross a `/`.
pub(crate) fn builtin_path_matches(args: &[Value]) -> RuntimeResult<Value> {
    let pattern = args.first().and_then(as_str).unwrap_or("");
    let name = args.get(1).and_then(as_str).unwrap_or("");
    Ok(Value::Bool(path_std::matches(pattern, name)))
}

/// `path::glob(pattern) -> Result<Vec<String>, errors::Error>` - sorted
/// filesystem matches for a `*` / `?` / `[abc]` / `**` pattern.
pub(crate) fn builtin_path_glob(args: &[Value]) -> RuntimeResult<Value> {
    let pattern = args.first().and_then(as_str).unwrap_or("");
    // A compile-time glob means the same set of files whatever
    // directory the build was started from, exactly as a compile-time
    // read means the same file.
    let pattern = gossamer_runtime::comptime_paths::resolve(pattern);
    let pattern = pattern.as_str();
    crate::comptime_gate::guard_read_pattern("path::glob", pattern)?;
    match path_std::glob(pattern) {
        Ok(paths) => {
            // Both halves of what a compile-time glob depends on: the
            // files it matched, and the directory whose listing decides
            // whether tomorrow's glob matches the same set.
            gossamer_runtime::comptime_inputs::record_each(&paths);
            gossamer_runtime::comptime_inputs::record(literal_prefix(pattern));
            Ok(ok_variant(string_array(paths)))
        }
        Err(e) => Ok(err_variant(e.to_string())),
    }
}

/// The directory part of `pattern` up to its first wildcard, which is
/// the deepest directory every match must live under.
fn literal_prefix(pattern: &str) -> &str {
    let wildcard = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
    match pattern[..wildcard].rfind(['/', '\\']) {
        Some(slash) => &pattern[..slash.max(1)],
        None => ".",
    }
}

pub(crate) fn builtin_path_parent(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    match path_std::parent(path) {
        Some(p) => Ok(some_variant(Value::String(p.into()))),
        None => Ok(none_variant()),
    }
}

pub(crate) fn builtin_path_file_name(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    match path_std::file_name(path) {
        Some(f) => Ok(some_variant(Value::String(f.into()))),
        None => Ok(none_variant()),
    }
}

pub(crate) fn builtin_path_stem(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    match path_std::stem(path) {
        Some(s) => Ok(some_variant(Value::String(s.into()))),
        None => Ok(none_variant()),
    }
}

pub(crate) fn builtin_path_ext(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    let ext = path_std::ext(path);
    if ext.is_empty() {
        Ok(none_variant())
    } else {
        Ok(some_variant(Value::String(ext.into())))
    }
}

pub(crate) fn builtin_path_is_absolute(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::Bool(path_std::is_absolute(path)))
}

pub(crate) fn builtin_path_normalize(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::String(path_std::clean(path).into()))
}

// ----------------------------------------------------------------------
// utf8

pub(crate) fn install_utf8(globals: &mut Vec<(&'static str, Value)>) {
    install_module_pub(
        "utf8",
        &[
            ("rune_count", builtin_utf8_count_runes),
            ("rune_count_in_string", builtin_utf8_rune_count_in_string),
            ("rune_len", builtin_utf8_rune_len),
            ("is_valid", builtin_utf8_is_valid),
            ("valid_string", builtin_utf8_valid_string),
            ("valid_rune", builtin_utf8_valid_rune),
            ("full_rune", builtin_utf8_full_rune),
            ("full_rune_in_string", builtin_utf8_full_rune_in_string),
            ("rune_start", builtin_utf8_rune_start),
            ("decode_rune", builtin_utf8_decode_rune),
            ("decode_rune_in_string", builtin_utf8_decode_rune_in_string),
            ("decode_last_rune", builtin_utf8_decode_last_rune),
            (
                "decode_last_rune_in_string",
                builtin_utf8_decode_last_rune_in_string,
            ),
            ("append_rune", builtin_utf8_append_rune),
        ],
        globals,
    );
}

pub(crate) fn builtin_utf8_count_runes(args: &[Value]) -> RuntimeResult<Value> {
    let bytes = bytes_from_utf8_arg(args.first());
    Ok(Value::Int(utf8_std::rune_count(&bytes) as i64))
}

pub(crate) fn builtin_utf8_rune_len(args: &[Value]) -> RuntimeResult<Value> {
    // Mirror the compiled `gos_rt_utf8_rune_len(c: u32)` shim: a rune is
    // a Unicode scalar (passed as a char or its codepoint int); invalid
    // scalars yield -1, matching Go's `utf8.RuneLen`.
    let scalar: u32 = match args.first() {
        Some(Value::Char(c)) => *c as u32,
        Some(Value::Int(n)) => *n as u32,
        Some(Value::String(s)) => match s.as_str().chars().next() {
            Some(c) => c as u32,
            None => return Ok(Value::Int(-1)),
        },
        _ => return Ok(Value::Int(-1)),
    };
    Ok(Value::Int(
        char::from_u32(scalar).map_or(-1, |c| c.len_utf8() as i64),
    ))
}

pub(crate) fn builtin_utf8_is_valid(args: &[Value]) -> RuntimeResult<Value> {
    match args.first().and_then(Value::byte_slice) {
        Some(bytes) => Ok(Value::Bool(std::str::from_utf8(&bytes).is_ok())),
        None => Ok(Value::Bool(false)),
    }
}

pub(crate) fn builtin_utf8_valid_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::Bool(utf8_std::valid_string(s)))
}

pub(crate) fn builtin_utf8_valid_rune(args: &[Value]) -> RuntimeResult<Value> {
    let r = match args.first() {
        Some(Value::Int(n)) => *n as u32,
        Some(Value::Char(c)) => *c as u32,
        _ => return Ok(Value::Bool(false)),
    };
    Ok(Value::Bool(utf8_std::valid_rune(r)))
}

pub(crate) fn builtin_utf8_rune_count_in_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::Int(utf8_std::rune_count_in_string(s) as i64))
}

pub(crate) fn builtin_utf8_full_rune(args: &[Value]) -> RuntimeResult<Value> {
    let bytes = bytes_from_utf8_arg(args.first());
    Ok(Value::Bool(utf8_std::full_rune(&bytes)))
}

pub(crate) fn builtin_utf8_full_rune_in_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::Bool(utf8_std::full_rune_in_string(s)))
}

pub(crate) fn builtin_utf8_rune_start(args: &[Value]) -> RuntimeResult<Value> {
    let b = match args.first() {
        Some(Value::Int(n)) => *n as u8,
        _ => return Ok(Value::Bool(false)),
    };
    Ok(Value::Bool(utf8_std::rune_start(b)))
}

pub(crate) fn bytes_from_utf8_arg(v: Option<&Value>) -> Vec<u8> {
    v.map(Value::bytes_or_empty).unwrap_or_default()
}

pub(crate) fn decode_rune_result(ch: char, n: usize) -> Value {
    Value::Tuple(Arc::from(vec![Value::Char(ch), Value::Int(n as i64)]))
}

pub(crate) fn builtin_utf8_decode_rune(args: &[Value]) -> RuntimeResult<Value> {
    let bytes = bytes_from_utf8_arg(args.first());
    let (ch, n) = utf8_std::decode_rune(&bytes);
    Ok(decode_rune_result(ch, n))
}

pub(crate) fn builtin_utf8_decode_rune_in_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    let (ch, n) = utf8_std::decode_rune_in_string(s);
    Ok(decode_rune_result(ch, n))
}

pub(crate) fn builtin_utf8_decode_last_rune(args: &[Value]) -> RuntimeResult<Value> {
    let bytes = bytes_from_utf8_arg(args.first());
    let (ch, n) = utf8_std::decode_last_rune(&bytes);
    Ok(decode_rune_result(ch, n))
}

pub(crate) fn builtin_utf8_decode_last_rune_in_string(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    let (ch, n) = utf8_std::decode_last_rune_in_string(s);
    Ok(decode_rune_result(ch, n))
}

pub(crate) fn builtin_utf8_append_rune(args: &[Value]) -> RuntimeResult<Value> {
    let buf = bytes_from_utf8_arg(args.first());
    let r = match args.get(1) {
        Some(Value::Char(c)) => *c,
        Some(Value::Int(n)) => char::from_u32(*n as u32).unwrap_or('\0'),
        Some(Value::String(s)) => s.as_str().chars().next().unwrap_or('\0'),
        _ => '\0',
    };
    let result = utf8_std::append_rune(buf, r);
    Ok(Value::Array(Arc::new(
        result
            .into_iter()
            .map(|b| Value::Int(i64::from(b)))
            .collect(),
    )))
}

// ----------------------------------------------------------------------
// os extras
