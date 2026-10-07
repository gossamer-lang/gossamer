#![allow(
    unused_imports,
    dead_code,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::missing_errors_doc,
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value
)]
//! `std::sync::RwLock` builtins for the bytecode VM - a reader-writer
//! lock guarding a single `i64`. The handle is a struct whose one field
//! holds the `parking_lot::RwLock` itself, shared by every copy of the
//! handle and freed with the last one, so a handle minted on one goroutine
//! worker thread reaches the same lock on another.
//!
//! `with_read` / `with_write` are `native` builtins so the closure can
//! be invoked through the interpreter dispatcher - a plain
//! `BuiltinFnPub` cannot call the callback. They are registered as
//! free data-last calls (`sync::RwLock::with_read(lock, f)`), the same
//! shape as `sync::Once::call`, and are the bit-identical VM mirror of
//! the compiled `gos_rt_rwlock_with_read` / `_with_write` shims.

use std::sync::Arc;

use crate::builtins::{BuiltinFnPub, value_to_int};
use crate::value::{NativeDispatch, RuntimeResult, Value};

type Lock = parking_lot::RwLock<i64>;

pub(crate) fn install_rwlock(globals: &mut Vec<(&'static str, Value)>) {
    let plain: &[(&str, BuiltinFnPub)] = &[
        ("RwLock::new", builtin_rwlock_new),
        ("sync::RwLock::new", builtin_rwlock_new),
        ("sync::RwLock::read", builtin_rwlock_get),
        ("RwLock::read", builtin_rwlock_get),
        ("sync::RwLock::write", builtin_rwlock_set),
        ("RwLock::write", builtin_rwlock_set),
    ];
    for (name, call) in plain {
        globals.push((*name, crate::builtins::builtin_pub(name, *call)));
    }
    // `with_read` / `with_write` run a closure, so they must be native.
    let native: &[&str] = &[
        "sync::RwLock::with_read",
        "RwLock::with_read",
        "sync::RwLock::with_write",
        "RwLock::with_write",
    ];
    for name in native {
        let call = if name.ends_with("with_read") {
            native_rwlock_with_read
        } else {
            native_rwlock_with_write
        };
        globals.push((*name, Value::native(name, call)));
    }
}

fn lock_handle(lock: Lock) -> Value {
    crate::value::state_handle("sync::RwLock", "__rwlock", Arc::new(lock))
}

fn lock_of(value: &Value) -> Option<Arc<Lock>> {
    crate::value::handle_state(value, "sync::RwLock", "__rwlock")
}

pub(crate) fn builtin_rwlock_new(args: &[Value]) -> RuntimeResult<Value> {
    let init = args.first().and_then(value_to_int).unwrap_or(0);
    Ok(lock_handle(Lock::new(init)))
}

pub(crate) fn builtin_rwlock_get(args: &[Value]) -> RuntimeResult<Value> {
    let v = args
        .first()
        .and_then(lock_of)
        .map(|a| *a.read())
        .unwrap_or(0);
    Ok(Value::Int(v))
}

pub(crate) fn builtin_rwlock_set(args: &[Value]) -> RuntimeResult<Value> {
    let val = args.get(1).and_then(value_to_int).unwrap_or(0);
    if let Some(arc) = args.first().and_then(lock_of) {
        *arc.write() = val;
    }
    Ok(Value::Unit)
}

pub(crate) fn native_rwlock_with_read(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let Some(arc) = args.first().and_then(lock_of) else {
        return Ok(Value::Int(0));
    };
    let Some(f) = args.get(1).cloned() else {
        return Ok(Value::Int(0));
    };
    let value = *arc.read();
    dispatch.call_value(&f, vec![Value::Int(value)])
}

pub(crate) fn native_rwlock_with_write(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let Some(arc) = args.first().and_then(lock_of) else {
        return Ok(Value::Int(0));
    };
    let Some(f) = args.get(1).cloned() else {
        return Ok(Value::Int(0));
    };
    let mut guard = arc.write();
    let current = *guard;
    let result = dispatch.call_value(&f, vec![Value::Int(current)])?;
    let next = value_to_int(&result).unwrap_or(current);
    *guard = next;
    Ok(Value::Int(next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_is_freed_with_its_last_handle() {
        let handle = builtin_rwlock_new(&[Value::Int(7)]).expect("RwLock::new");
        let copy = handle.clone();
        let lock = lock_of(&handle).expect("a lock handle");
        assert_eq!(*lock.read(), 7);
        drop(handle);
        drop(copy);
        assert_eq!(
            Arc::strong_count(&lock),
            1,
            "only this test still holds the lock"
        );
    }
}
