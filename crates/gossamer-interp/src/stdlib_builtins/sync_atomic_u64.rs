//! `sync::AtomicU64` builtins.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64 as StdAtomicU64, Ordering};

use super::{atomic_cell, atomic_handle};
use crate::builtins::{BuiltinFnPub, value_to_int};
use crate::value::{RuntimeResult, Value};

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

pub(crate) fn with_atomic_u64<R>(
    value: &Value,
    f: impl FnOnce(&Arc<StdAtomicU64>) -> R,
) -> Option<R> {
    atomic_cell(value, "sync::AtomicU64").map(|cell| f(&cell))
}

pub(crate) fn builtin_atomic_u64_new(args: &[Value]) -> RuntimeResult<Value> {
    let init = args.first().and_then(value_to_int).unwrap_or(0) as u64;
    Ok(atomic_handle("sync::AtomicU64", StdAtomicU64::new(init)))
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
