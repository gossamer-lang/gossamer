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
use gossamer_runtime::codec::percent::{self, Component};

/// Entry point invoked from `builtins::install`.
use super::*;

pub(crate) fn install_url_escape(globals: &mut Vec<(&'static str, Value)>) {
    for (short, call) in [
        ("query_escape", builtin_url_query_escape as BuiltinFnPub),
        ("path_escape", builtin_url_path_escape),
        ("query_unescape", builtin_url_query_unescape),
        ("path_unescape", builtin_url_path_unescape),
    ] {
        let q: &'static str = Box::leak(format!("url::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
}

pub(crate) fn builtin_url_query_escape(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::String(percent::encode(s, Component::Query).into()))
}
pub(crate) fn builtin_url_path_escape(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::String(percent::encode(s, Component::Path).into()))
}
pub(crate) fn builtin_url_query_unescape(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::String(percent::decode(s, Component::Query).into()))
}
pub(crate) fn builtin_url_path_unescape(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::String(percent::decode(s, Component::Path).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(builtin: BuiltinFnPub, args: Vec<Value>) -> Value {
        builtin(&args).unwrap()
    }

    #[test]
    fn strings_join_inserts_separator() {
        let parts = Value::Array(Arc::new(vec![
            Value::String("a".into()),
            Value::String("b".into()),
            Value::String("c".into()),
        ]));
        let out = call(builtin_strings_join, vec![parts, Value::String(",".into())]);
        if let Value::String(s) = out {
            assert_eq!(s.as_str(), "a,b,c");
        } else {
            panic!("expected string, got {out:?}");
        }
    }

    #[test]
    fn strconv_parse_int_round_trip() {
        let parsed = call(builtin_strconv_parse_i64, vec![Value::String("42".into())]);
        if let Value::Variant(inner) = parsed {
            assert_eq!(inner.name, "Ok");
            assert!(matches!(inner.fields.first(), Some(Value::Int(42))));
        } else {
            panic!("expected Ok variant");
        }
        let formatted = call(builtin_strconv_format_i64, vec![Value::Int(-7)]);
        if let Value::String(s) = formatted {
            assert_eq!(s.as_str(), "-7");
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn set_supports_full_lifecycle() {
        let s = call(builtin_set_new, vec![]);
        assert!(matches!(s, Value::Struct(_)));
        let after_insert = call(builtin_set_insert, vec![s.clone(), Value::Int(1)]);
        assert!(matches!(after_insert, Value::Bool(true)));
        let duplicate = call(builtin_set_insert, vec![s.clone(), Value::Int(1)]);
        assert!(matches!(duplicate, Value::Bool(false)));
        let n = call(builtin_set_len, vec![s.clone()]);
        assert!(matches!(n, Value::Int(1)));
        let has = call(builtin_set_contains, vec![s.clone(), Value::Int(1)]);
        assert!(matches!(has, Value::Bool(true)));
        let after_remove = call(builtin_set_remove, vec![s.clone(), Value::Int(1)]);
        assert!(matches!(after_remove, Value::Bool(true)));
        let empty = call(builtin_set_is_empty, vec![s]);
        assert!(matches!(empty, Value::Bool(true)));
    }

    // The socket builtins this drives are gated out of the wasm build, and
    // that target has no loopback to bind either.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn tcp_listener_round_trip_via_loopback() {
        let listener = call(
            builtin_tcp_listener_bind,
            vec![Value::String("127.0.0.1:0".into())],
        );
        let listener_handle = match listener {
            Value::Variant(inner) if inner.name == "Ok" => inner.fields[0].clone(),
            other => panic!("bind failed: {other:?}"),
        };
        let addr = match call(
            builtin_tcp_listener_local_addr,
            vec![listener_handle.clone()],
        ) {
            Value::Variant(inner) if inner.name == "Ok" => match &inner.fields[0] {
                Value::String(s) => s.as_str().to_string(),
                other => panic!("expected addr string, got {other:?}"),
            },
            other => panic!("local_addr failed: {other:?}"),
        };
        let addr_clone = addr.clone();
        let join = std::thread::spawn(move || {
            let conn = call(
                builtin_tcp_stream_connect,
                vec![Value::String(addr_clone.into())],
            );
            let conn_handle = match conn {
                Value::Variant(inner) if inner.name == "Ok" => inner.fields[0].clone(),
                other => panic!("connect failed: {other:?}"),
            };
            call(
                builtin_tcp_stream_write,
                vec![conn_handle.clone(), Value::String("hello".into())],
            );
            call(builtin_tcp_stream_close, vec![conn_handle]);
        });
        let accepted = call(builtin_tcp_listener_accept, vec![listener_handle.clone()]);
        let stream_handle = match accepted {
            Value::Variant(inner) if inner.name == "Ok" => match &inner.fields[0] {
                Value::Tuple(parts) => parts[0].clone(),
                other => panic!("expected tuple, got {other:?}"),
            },
            other => panic!("accept failed: {other:?}"),
        };
        let read = call(
            builtin_tcp_stream_read_to_string,
            vec![stream_handle.clone()],
        );
        match read {
            Value::Variant(inner) if inner.name == "Ok" => match &inner.fields[0] {
                Value::String(s) => assert_eq!(s.as_str(), "hello"),
                other => panic!("expected string, got {other:?}"),
            },
            other => panic!("read failed: {other:?}"),
        }
        call(builtin_tcp_stream_close, vec![stream_handle]);
        call(builtin_tcp_listener_close, vec![listener_handle]);
        join.join().unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn udp_round_trip_via_loopback() {
        let server = call(builtin_udp_bind, vec![Value::String("127.0.0.1:0".into())]);
        let server_handle = match server {
            Value::Variant(inner) if inner.name == "Ok" => inner.fields[0].clone(),
            other => panic!("bind failed: {other:?}"),
        };
        let addr = match call(builtin_udp_local_addr, vec![server_handle.clone()]) {
            Value::Variant(inner) if inner.name == "Ok" => match &inner.fields[0] {
                Value::String(s) => s.as_str().to_string(),
                _ => panic!("addr was not string"),
            },
            _ => panic!("local_addr failed"),
        };
        let client = call(builtin_udp_bind, vec![Value::String("127.0.0.1:0".into())]);
        let client_handle = match client {
            Value::Variant(inner) if inner.name == "Ok" => inner.fields[0].clone(),
            _ => panic!("client bind failed"),
        };
        call(
            builtin_udp_send_to,
            vec![
                client_handle.clone(),
                Value::String("ping".into()),
                Value::String(addr.into()),
            ],
        );
        let recv = call(
            builtin_udp_recv_from,
            vec![server_handle.clone(), Value::Int(64)],
        );
        match recv {
            Value::Variant(inner) if inner.name == "Ok" => match &inner.fields[0] {
                Value::Tuple(parts) => match &parts[0] {
                    Value::Array(bytes) => {
                        let payload: Vec<u8> = bytes
                            .iter()
                            .filter_map(|v| match v {
                                Value::Int(n) => u8::try_from(*n).ok(),
                                _ => None,
                            })
                            .collect();
                        assert_eq!(payload, b"ping");
                    }
                    _ => panic!("expected bytes array"),
                },
                _ => panic!("expected tuple"),
            },
            other => panic!("recv failed: {other:?}"),
        }
        call(builtin_udp_close, vec![server_handle]);
        call(builtin_udp_close, vec![client_handle]);
    }

    #[test]
    fn time_instant_returns_monotonic_handle() {
        let inst = call(builtin_time_instant_now, vec![]);
        gossamer_runtime::platform::sleep(std::time::Duration::from_millis(2));
        let elapsed = call(builtin_time_instant_elapsed_ms, vec![inst]);
        match elapsed {
            Value::Int(n) => assert!(n >= 0),
            other => panic!("expected int, got {other:?}"),
        }
    }
}
