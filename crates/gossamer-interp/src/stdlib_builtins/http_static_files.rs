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

/// Extracts a string argument, defaulting to empty. A private copy
/// local to this module so the pure static-file builtins do not
/// depend on the SSE module (gated off the wasm sandbox).
fn arg_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.as_str().to_string(),
        _ => String::new(),
    }
}

pub(crate) fn install_http_static_files(globals: &mut Vec<(&'static str, Value)>) {
    for (name, call) in [
        ("serve_file", builtin_static_serve_file as BuiltinFnPub),
        ("mime_for_path", builtin_static_mime_for_path),
    ] {
        let qualified: &'static str =
            Box::leak(format!("http::static_files::{name}").into_boxed_str());
        globals.push((qualified, crate::builtins::builtin_pub(qualified, call)));
        let short: &'static str = Box::leak(format!("static_files::{name}").into_boxed_str());
        globals.push((short, crate::builtins::builtin_pub(short, call)));
    }
}

pub(crate) fn builtin_static_serve_file(args: &[Value]) -> RuntimeResult<Value> {
    let path = arg_str(args.first());
    let mime_path = path.clone();
    match gossamer_runtime::sched_global::run_blocking("http-static-read", move || {
        std::fs::read(path)
    }) {
        Ok(Ok(bytes)) => {
            let mime = guess_mime_from_path(&mime_path);
            let mut fields = vec![
                ("status", Value::Int(200)),
                (
                    "body",
                    Value::String(String::from_utf8_lossy(&bytes).into_owned().into()),
                ),
                (
                    "content_type",
                    Value::String(SmolStr::from(mime.to_string())),
                ),
            ];
            fields.push(("headers", Value::Array(Arc::new(Vec::new()))));
            Ok(ok_variant(Value::struct_(
                "http::Response",
                Arc::unwrap_or_clone(Arc::new(fields)),
            )))
        }
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_static_mime_for_path(args: &[Value]) -> RuntimeResult<Value> {
    let path = arg_str(args.first());
    let mime = guess_mime_from_path(&path);
    Ok(Value::String(SmolStr::from(mime.to_string())))
}

pub(crate) fn guess_mime_from_path(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_file_reads_through_blocking_pool() {
        let path = gossamer_runtime::platform::temp_dir().join(format!(
            "gossamer-static-file-{}",
            gossamer_runtime::platform::process_id()
        ));
        std::fs::write(&path, "static body").expect("write fixture");
        let result =
            builtin_static_serve_file(&[Value::String(path.to_string_lossy().into_owned().into())])
                .expect("serve file");
        assert!(matches!(result, Value::Variant(inner) if inner.name == "Ok"));
        std::fs::remove_file(path).expect("remove fixture");
    }
}
