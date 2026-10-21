#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::doc_markdown,
    clippy::needless_pass_by_value,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::single_call_fn,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
//! Gossamer-callable `std::io` stream adapters. A reader or writer is an
//! `io::MemoryStream` naming a counted row of `gossamer_std::io_handles`; the row
//! is given back with the last copy of the value.

use std::sync::Arc;

use gossamer_std::io_handles as handles;

use crate::builtins::{BuiltinFnPub, as_str, err_variant, ok_variant, value_to_int};
use crate::value::{RuntimeResult, Value};

use super::*;
use crate::stdlib_builtins::encoding_pem::collect_array;

/// Entry point invoked from `stdlib_builtins::install`.
pub(crate) fn install_io_streams(globals: &mut Vec<(&'static str, Value)>) {
    for (name, call) in [
        (
            "io::string_reader",
            builtin_io_string_reader as BuiltinFnPub,
        ),
        ("io::buffer_writer", builtin_io_buffer_writer),
        ("io::limit_reader", builtin_io_limit_reader),
        ("io::tee_reader", builtin_io_tee_reader),
        ("io::multi_reader", builtin_io_multi_reader),
        ("io::pipe", builtin_io_pipe),
        ("io::copy_n", builtin_io_copy_n),
        ("io::drain", builtin_io_drain),
        ("io::contents", builtin_io_contents),
        ("io::write", builtin_io_write),
        ("io::close_writer", builtin_io_close_writer),
    ] {
        globals.push((name, crate::builtins::builtin_pub(name, call)));
    }
}

/// The registry row an `io::MemoryStream` argument names.
fn handle(v: Option<&Value>) -> i64 {
    v.and_then(super::time::handle_id).unwrap_or(0)
}

/// An `io::MemoryStream` value owning the reference to row `id` its creation took.
fn stream(id: i64) -> Value {
    super::time::handle_struct("io::MemoryStream", id, handles::retire)
}

/// `io::string_reader(text) -> io::MemoryStream`.
pub(crate) fn builtin_io_string_reader(args: &[Value]) -> RuntimeResult<Value> {
    Ok(stream(handles::string_reader(
        args.first().and_then(as_str).unwrap_or(""),
    )))
}

/// `io::buffer_writer() -> io::MemoryStream`.
pub(crate) fn builtin_io_buffer_writer(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(stream(handles::buffer_writer()))
}

/// `io::limit_reader(src, limit) -> io::MemoryStream`.
pub(crate) fn builtin_io_limit_reader(args: &[Value]) -> RuntimeResult<Value> {
    let limit = args.get(1).and_then(value_to_int).unwrap_or(0);
    Ok(stream(handles::limit_reader(handle(args.first()), limit)))
}

/// `io::tee_reader(src, sink) -> io::MemoryStream`.
pub(crate) fn builtin_io_tee_reader(args: &[Value]) -> RuntimeResult<Value> {
    Ok(stream(handles::tee_reader(
        handle(args.first()),
        handle(args.get(1)),
    )))
}

/// `io::multi_reader(sources) -> io::MemoryStream`.
pub(crate) fn builtin_io_multi_reader(args: &[Value]) -> RuntimeResult<Value> {
    let ids = args
        .first()
        .map(collect_array)
        .unwrap_or_default()
        .iter()
        .filter_map(super::time::handle_id)
        .collect();
    Ok(stream(handles::multi_reader(ids)))
}

/// `io::pipe() -> (io::MemoryStream, io::MemoryStream)`.
pub(crate) fn builtin_io_pipe(_args: &[Value]) -> RuntimeResult<Value> {
    let (reader, writer) = handles::pipe();
    Ok(Value::Tuple(Arc::from(vec![
        stream(reader),
        stream(writer),
    ])))
}

/// `io::copy_n(dst, src, n) -> Result<i64, errors::Error>`.
pub(crate) fn builtin_io_copy_n(args: &[Value]) -> RuntimeResult<Value> {
    let n = args.get(2).and_then(value_to_int).unwrap_or(0);
    if n < 0 {
        return Ok(err_variant("io::copy_n: negative byte count"));
    }
    Ok(ok_variant(Value::Int(handles::copy_n(
        handle(args.first()),
        handle(args.get(1)),
        n,
    ))))
}

/// `io::drain(src) -> String`.
pub(crate) fn builtin_io_drain(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(handles::drain(handle(args.first())).into()))
}

/// `io::contents(writer) -> String`.
pub(crate) fn builtin_io_contents(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        handles::contents(handle(args.first())).into(),
    ))
}

/// `io::write(writer, text) -> i64`.
pub(crate) fn builtin_io_write(args: &[Value]) -> RuntimeResult<Value> {
    let text = args.get(1).and_then(as_str).unwrap_or("");
    Ok(Value::Int(
        handles::write(handle(args.first()), text.as_bytes()) as i64,
    ))
}

/// `io::close_writer(writer)`.
pub(crate) fn builtin_io_close_writer(args: &[Value]) -> RuntimeResult<Value> {
    handles::close_writer(handle(args.first()));
    Ok(Value::Unit)
}
