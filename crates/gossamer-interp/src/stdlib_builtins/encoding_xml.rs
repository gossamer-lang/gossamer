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
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeResult, Value, dense_map};

/// Entry point invoked from `builtins::install`.
use super::*;

pub(crate) fn install_encoding_xml(globals: &mut Vec<(&'static str, Value)>) {
    for (short, call) in [
        ("parse", builtin_xml_parse as BuiltinFnPub),
        ("encode", builtin_xml_encode),
        ("escape", builtin_xml_escape),
    ] {
        let q: &'static str = Box::leak(format!("encoding::xml::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
}

pub(crate) fn xml_node_to_value(node: &gossamer_std::encoding::xml::Node) -> Value {
    use gossamer_std::encoding::xml::Node;
    match node {
        Node::Text(s) => {
            let mut map = dense_map();
            map.insert(
                MapKey::Str("__xml_type".into()),
                Value::String("text".into()),
            );
            map.insert(MapKey::Str("value".into()), Value::String(s.clone().into()));
            Value::Map(Arc::new(parking_lot::Mutex::new(map.into())))
        }
        Node::Element {
            name,
            attrs,
            children,
        } => {
            let mut map = dense_map();
            map.insert(
                MapKey::Str("__xml_type".into()),
                Value::String("element".into()),
            );
            map.insert(
                MapKey::Str("name".into()),
                Value::String(name.clone().into()),
            );
            let mut attr_map = dense_map();
            for (k, v) in attrs {
                attr_map.insert(
                    MapKey::Str(k.clone().into()),
                    Value::String(v.clone().into()),
                );
            }
            map.insert(
                MapKey::Str("attrs".into()),
                Value::Map(Arc::new(parking_lot::Mutex::new(attr_map.into()))),
            );
            let child_vals: Vec<Value> = children.iter().map(xml_node_to_value).collect();
            map.insert(
                MapKey::Str("children".into()),
                Value::Array(Arc::new(child_vals)),
            );
            Value::Map(Arc::new(parking_lot::Mutex::new(map.into())))
        }
    }
}

pub(crate) fn value_to_xml_node(v: &Value) -> Option<gossamer_std::encoding::xml::Node> {
    use gossamer_std::encoding::xml::Node;
    let map = match v {
        Value::Map(m) => m.lock(),
        _ => return None,
    };
    let xml_type = match map.get(&MapKey::Str("__xml_type".into())) {
        Some(Value::String(s)) => s.as_str().to_string(),
        _ => return None,
    };
    if xml_type == "text" {
        let s = match map.get(&MapKey::Str("value".into())) {
            Some(Value::String(s)) => s.as_str().to_string(),
            _ => String::new(),
        };
        return Some(Node::Text(s));
    }
    if xml_type == "element" {
        let name = match map.get(&MapKey::Str("name".into())) {
            Some(Value::String(s)) => s.as_str().to_string(),
            _ => return None,
        };
        let attrs = match map.get(&MapKey::Str("attrs".into())) {
            Some(Value::Map(m)) => {
                let inner = m.lock();
                inner
                    .iter()
                    .filter_map(|(k, v)| {
                        let key = match k {
                            MapKey::Str(s) => s.to_string(),
                            _ => return None,
                        };
                        let val = match v {
                            Value::String(s) => s.as_str().to_string(),
                            _ => return None,
                        };
                        Some((key, val))
                    })
                    .collect()
            }
            _ => std::collections::BTreeMap::new(),
        };
        let children = match map.get(&MapKey::Str("children".into())) {
            Some(Value::Array(arr)) => arr.iter().filter_map(value_to_xml_node).collect(),
            _ => Vec::new(),
        };
        return Some(Node::Element {
            name,
            attrs,
            children,
        });
    }
    None
}

pub(crate) fn builtin_xml_parse(args: &[Value]) -> RuntimeResult<Value> {
    let src = args.first().and_then(as_str).unwrap_or("").to_string();
    match gossamer_std::encoding::xml::parse(&src) {
        Ok(node) => Ok(ok_variant(xml_node_to_value(&node))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_xml_encode(args: &[Value]) -> RuntimeResult<Value> {
    let v = args.first().unwrap_or(&Value::Unit);
    match value_to_xml_node(v) {
        Some(node) => Ok(Value::String(
            gossamer_std::encoding::xml::encode(&node).into(),
        )),
        None => Ok(Value::String("".into())),
    }
}

pub(crate) fn builtin_xml_escape(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    Ok(Value::String(
        gossamer_std::encoding::xml::escape(&s).into(),
    ))
}

// ----------------------------------------------------------------------
// crypto::insecure (MD5, SHA-1)
