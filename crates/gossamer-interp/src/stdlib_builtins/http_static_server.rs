#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::single_call_fn
)]
//! Interp-tier `http::static_files::FileServer` - the stateful
//! file-serving handler. `FileServer::new(root, prefix)` builds a
//! handle; dispatched through `http::serve`, each request runs
//! `FileServer::serve(req)` which strips the prefix, rejects path
//! traversal, reads the file, and returns `Result<Response, Error>`.
//! Mirrors the compiled-tier `gos_rt_file_server_new` /
//! `gos_rt_file_server_serve` shims byte-for-byte (prefix-strip,
//! `..` rejection, MIME table, 404/403 shapes) for tier parity.

use std::sync::Arc;

use gossamer_ast::Ident;

use crate::builtins::{BuiltinFnPub, ok_variant};
use crate::value::{NativeDispatch, RuntimeResult, SmolStr, Value};

use super::*;

pub(crate) fn install_http_static_server(globals: &mut Vec<(&'static str, Value)>) {
    for alias in [
        "FileServer::new",
        "static_files::FileServer::new",
        "http::static_files::FileServer::new",
    ] {
        globals.push((
            alias,
            crate::builtins::builtin_pub(alias, builtin_file_server_new as BuiltinFnPub),
        ));
    }
}

/// `FileServer::new(root, prefix)` - store the document root and the
/// URL prefix to strip. An absent prefix arg defaults to "".
pub(crate) fn builtin_file_server_new(args: &[Value]) -> RuntimeResult<Value> {
    let root = arg_str(args.first());
    let prefix = arg_str(args.get(1));
    let fields = vec![
        ("__fs_root", Value::String(SmolStr::from(root))),
        ("__fs_prefix", Value::String(SmolStr::from(prefix))),
    ];
    Ok(Value::struct_("http::static_files::FileServer", fields))
}

fn file_server_fields(v: Option<&Value>) -> (String, String) {
    let mut root = String::new();
    let mut prefix = String::new();
    if let Some(Value::Struct(inner)) = v {
        for (field, val) in &inner.fields {
            match ((*field), val) {
                ("__fs_root", Value::String(s)) => root = s.as_str().to_string(),
                ("__fs_prefix", Value::String(s)) => prefix = s.as_str().to_string(),
                _ => {}
            }
        }
    }
    (root, prefix)
}

/// Reads a request header value by name (case-insensitive) from the
/// interp `Request` struct's `headers` array.
fn request_header(req: Option<&Value>, name: &str) -> Option<String> {
    let Some(Value::Struct(inner)) = req else {
        return None;
    };
    for (field, val) in &inner.fields {
        if (*field) == "headers"
            && let Value::Array(items) = val
        {
            for item in items.iter() {
                if let Value::Tuple(t) = item
                    && let (Some(Value::String(k)), Some(Value::String(v))) = (t.first(), t.get(1))
                    && k.as_str().eq_ignore_ascii_case(name)
                {
                    return Some(v.as_str().to_string());
                }
            }
        }
    }
    None
}

/// Builds a `[(name, value)]` header array Value from `pairs`.
fn header_array(pairs: &[(&str, String)]) -> Value {
    let items: Vec<Value> = pairs
        .iter()
        .map(|(k, v)| {
            Value::Tuple(Arc::from(vec![
                Value::String(SmolStr::from((*k).to_string())),
                Value::String(SmolStr::from(v.clone())),
            ]))
        })
        .collect();
    Value::Array(Arc::new(items))
}

/// Builds a `Response` struct with a byte-array body so binary slices /
/// `multipart/byteranges` bodies survive without UTF-8 lossy mangling,
/// matching the compiled tier's `body_bytes`.
fn file_response(status: i64, bytes: &[u8], mime: &str, headers: &[(&str, String)]) -> Value {
    let body: Vec<Value> = bytes.iter().map(|b| Value::Int(i64::from(*b))).collect();
    let fields = vec![
        ("status", Value::Int(status)),
        ("body", Value::Array(Arc::new(body))),
        (
            "content_type",
            Value::String(SmolStr::from(mime.to_string())),
        ),
        ("headers", header_array(headers)),
    ];
    Value::struct_("http::Response", fields)
}

/// The `301` that sends a directory request to the same path with a
/// trailing slash, so the page's own relative links resolve under the
/// directory rather than beside it.
fn redirect_response(location: &str) -> Value {
    let fields = vec![
        ("status", Value::Int(301)),
        ("body", Value::String(SmolStr::from(""))),
        (
            "content_type",
            Value::String(SmolStr::from("text/plain; charset=utf-8")),
        ),
        (
            "headers",
            header_array(&[("location", location.to_string())]),
        ),
    ];
    Value::struct_("http::Response", fields)
}

fn forbidden_response() -> Value {
    let fields = vec![
        ("status", Value::Int(403)),
        ("body", Value::String(SmolStr::from("forbidden"))),
        (
            "content_type",
            Value::String(SmolStr::from("text/plain; charset=utf-8")),
        ),
        ("headers", Value::Array(Arc::new(Vec::new()))),
    ];
    Value::struct_("http::Response", fields)
}

/// `FileServer::serve(server, request)` - invoked by `http::serve`'s
/// dispatch loop when the handler is a `FileServer`. The dispatch
/// argument is unused (no callback into user code).
pub(crate) fn native_file_server_serve(
    _dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let (root, prefix) = file_server_fields(args.first());
    let (_method, path) =
        super::http_router::request_method_and_path(args.get(1).unwrap_or(&Value::Unit));

    let rel = path.strip_prefix(&prefix).unwrap_or(&path);
    let rel = rel.trim_start_matches('/');
    use gossamer_runtime::c_abi::http_bridges::{
        STATIC_FILE_MAX_BYTES, StaticResolution, resolve_static_path,
    };
    let full = match resolve_static_path(std::path::Path::new(&root), rel, STATIC_FILE_MAX_BYTES) {
        StaticResolution::File(p) => p,
        StaticResolution::Forbidden => return Ok(ok_variant(forbidden_response())),
        StaticResolution::Redirect => {
            return Ok(ok_variant(redirect_response(&format!("{path}/"))));
        }
        StaticResolution::NotFound => {
            return Ok(ok_variant(super::http_router::http_404_response()));
        }
    };
    let mime_path = full.to_string_lossy().into_owned();
    match gossamer_runtime::sched_global::run_blocking("http-static-read", move || {
        std::fs::read(full)
    }) {
        Ok(Ok(bytes)) => {
            let mime = super::http_static_files::guess_mime_from_path(&mime_path);
            let total = bytes.len() as u64;
            let range_header = request_header(args.get(1), "range");
            use gossamer_runtime::c_abi::http_bridges::{
                RangeOutcome, build_multipart_body, content_range_value, evaluate_range,
                multipart_content_type, range_slice,
            };
            let response = match evaluate_range(range_header.as_deref(), total) {
                RangeOutcome::Whole => {
                    file_response(200, &bytes, mime, &[("accept-ranges", "bytes".to_string())])
                }
                RangeOutcome::Single { start, end } => {
                    let slice = range_slice(&bytes, start, end);
                    file_response(
                        206,
                        &slice,
                        mime,
                        &[
                            ("accept-ranges", "bytes".to_string()),
                            ("content-range", content_range_value(start, end, total)),
                        ],
                    )
                }
                RangeOutcome::Multi(ranges) => {
                    let body = build_multipart_body(&bytes, &ranges, mime, total);
                    file_response(
                        206,
                        &body,
                        &multipart_content_type(),
                        &[("accept-ranges", "bytes".to_string())],
                    )
                }
                RangeOutcome::Unsatisfiable => file_response(
                    416,
                    &[],
                    "text/plain; charset=utf-8",
                    &[("content-range", format!("bytes */{total}"))],
                ),
            };
            Ok(ok_variant(response))
        }
        Ok(Err(_)) | Err(_) => Ok(ok_variant(super::http_router::http_404_response())),
    }
}
