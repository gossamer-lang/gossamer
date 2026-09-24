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
use std::sync::atomic::Ordering;

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

pub(crate) fn install_archive_tar(globals: &mut Vec<(&'static str, Value)>) {
    for (short, call) in [
        ("tar::read", builtin_archive_tar_read as BuiltinFnPub),
        ("tar::write", builtin_archive_tar_write),
    ] {
        let q: &'static str = Box::leak(format!("archive::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
    // Leaf for the injected real-struct `TarEntry` wrapper: each
    // entry as a `(name, data, is_dir)` tuple.
    globals.push((
        "__gos_tar_read_raw",
        crate::builtins::builtin_pub("__gos_tar_read_raw", builtin_tar_read_raw),
    ));
}

pub(crate) fn builtin_tar_read_raw(args: &[Value]) -> RuntimeResult<Value> {
    let input = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    match gossamer_std::archive::tar::read(&input) {
        Ok(entries) => {
            let arr = Value::Array(Arc::new(
                entries
                    .into_iter()
                    .map(|e| {
                        let data = Value::Array(Arc::new(
                            e.data
                                .into_iter()
                                .map(|b| Value::Int(i64::from(b)))
                                .collect(),
                        ));
                        Value::Tuple(Arc::from(vec![
                            Value::String(e.name.into()),
                            data,
                            Value::Bool(e.is_dir),
                        ]))
                    })
                    .collect(),
            ));
            Ok(ok_variant(arr))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn tar_entry_to_value(entry: gossamer_std::archive::tar::TarEntry) -> Value {
    Value::struct_(
        "archive::TarEntry",
        Arc::unwrap_or_clone(Arc::new(vec![
            ("name", Value::String(entry.name.into())),
            ("data", bytes_to_array(entry.data)),
            ("is_dir", Value::Bool(entry.is_dir)),
        ])),
    )
}

pub(crate) fn builtin_archive_tar_read(args: &[Value]) -> RuntimeResult<Value> {
    let input = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    match gossamer_std::archive::tar::read(&input) {
        Ok(entries) => {
            let arr = Value::Array(Arc::new(
                entries.into_iter().map(tar_entry_to_value).collect(),
            ));
            Ok(ok_variant(arr))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_archive_tar_write(args: &[Value]) -> RuntimeResult<Value> {
    let pairs = match args.first() {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| match v {
                Value::Tuple(t) => {
                    let name = match t.first()? {
                        Value::String(s) => s.as_str().to_string(),
                        _ => return None,
                    };
                    let data = bytes_from_value(t.get(1)?);
                    Some((name, data))
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let refs: Vec<(&str, &[u8])> = pairs
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    match gossamer_std::archive::tar::write(&refs) {
        Ok(out) => Ok(ok_variant(bytes_to_array(out))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

// ----------------------------------------------------------------------
// sync::AtomicU64

use std::sync::atomic::AtomicU64 as StdAtomicU64;

use super::set::GlobalReg;

// Process-global (not `thread_local!`): goroutines run on an OS
// worker-thread pool, so a handle minted on one thread must resolve on
// another. A `thread_local!` registry silently lost every cross-goroutine
// barrier rendezvous (`Barrier::wait` no-op'd on the worker thread,
// deadlocking the main goroutine). Mirrors the `sync::*` registries.
pub(crate) static ATOMIC_U64_REGISTRY: GlobalReg<
    std::collections::HashMap<i64, Arc<StdAtomicU64>>,
> = GlobalReg::new(|| {
    parking_lot::ReentrantMutex::new(std::cell::RefCell::new(std::collections::HashMap::new()))
});
pub(crate) static BARRIER_REGISTRY: GlobalReg<
    std::collections::HashMap<i64, Arc<gossamer_std::sync::Barrier>>,
> = GlobalReg::new(|| {
    parking_lot::ReentrantMutex::new(std::cell::RefCell::new(std::collections::HashMap::new()))
});

pub(crate) fn install_sync_atomic_u64(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("AtomicU64::new", builtin_atomic_u64_new),
        ("AtomicU64::load", builtin_atomic_u64_load),
        ("AtomicU64::store", builtin_atomic_u64_store),
        ("AtomicU64::fetch_add", builtin_atomic_u64_fetch_add),
        ("AtomicU64::fetch_sub", builtin_atomic_u64_fetch_sub),
        ("AtomicU64::compare_exchange", builtin_atomic_u64_cas),
    ];
    for (name, call) in entries {
        let qualified: &'static str = Box::leak(format!("sync::{name}").into_boxed_str());
        globals.push((qualified, crate::builtins::builtin_pub(qualified, *call)));
        globals.push((*name, crate::builtins::builtin_pub(name, *call)));
    }
}

pub(crate) fn atomic_u64_handle(id: i64) -> Value {
    atomic_handle("sync::AtomicU64", id)
}

pub(crate) fn atomic_u64_id_of(value: &Value) -> Option<i64> {
    atomic_id_of(value, "sync::AtomicU64")
}

pub(crate) fn with_atomic_u64<R>(
    value: &Value,
    f: impl FnOnce(&Arc<StdAtomicU64>) -> R,
) -> Option<R> {
    let id = atomic_u64_id_of(value)?;
    ATOMIC_U64_REGISTRY.with(|r| r.borrow().get(&id).map(f))
}

pub(crate) fn builtin_atomic_u64_new(args: &[Value]) -> RuntimeResult<Value> {
    let init = args.first().and_then(value_to_int).unwrap_or(0) as u64;
    let id = next_atomic_id();
    ATOMIC_U64_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, Arc::new(StdAtomicU64::new(init)));
    });
    Ok(atomic_u64_handle(id))
}

pub(crate) fn builtin_atomic_u64_load(args: &[Value]) -> RuntimeResult<Value> {
    let n = args
        .first()
        .and_then(|v| with_atomic_u64(v, |a| a.load(Ordering::SeqCst)))
        .unwrap_or(0);
    Ok(Value::Int(n as i64))
}

pub(crate) fn builtin_atomic_u64_store(args: &[Value]) -> RuntimeResult<Value> {
    let val = args.get(1).and_then(value_to_int).unwrap_or(0) as u64;
    if let Some(handle) = args.first() {
        let _ = with_atomic_u64(handle, |a| a.store(val, Ordering::SeqCst));
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_atomic_u64_fetch_add(args: &[Value]) -> RuntimeResult<Value> {
    let delta = args.get(1).and_then(value_to_int).unwrap_or(0) as u64;
    let prev = args
        .first()
        .and_then(|v| with_atomic_u64(v, |a| a.fetch_add(delta, Ordering::SeqCst)))
        .unwrap_or(0);
    Ok(Value::Int(prev as i64))
}

pub(crate) fn builtin_atomic_u64_fetch_sub(args: &[Value]) -> RuntimeResult<Value> {
    let delta = args.get(1).and_then(value_to_int).unwrap_or(0) as u64;
    let prev = args
        .first()
        .and_then(|v| with_atomic_u64(v, |a| a.fetch_sub(delta, Ordering::SeqCst)))
        .unwrap_or(0);
    Ok(Value::Int(prev as i64))
}

pub(crate) fn builtin_atomic_u64_cas(args: &[Value]) -> RuntimeResult<Value> {
    let current = args.get(1).and_then(value_to_int).unwrap_or(0) as u64;
    let new = args.get(2).and_then(value_to_int).unwrap_or(0) as u64;
    let ok = args
        .first()
        .and_then(|v| {
            with_atomic_u64(v, |a| {
                a.compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            })
        })
        .unwrap_or(false);
    Ok(Value::Bool(ok))
}

// ----------------------------------------------------------------------
// sync::Barrier
