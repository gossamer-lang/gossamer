#![allow(
    unused_imports,
    dead_code,
    clippy::needless_pass_by_value,
    clippy::missing_errors_doc
)]
//! `std::sync::Shared` builtins for the bytecode VM - a value one
//! goroutine publishes and any number reach under a lock.
//!
//! The handle is a struct carrying an `id`; the real
//! `parking_lot::Mutex<Value>` lives in a process-global registry keyed
//! by that id (the shape `sync::Map`, `sync::Once`, and `sync::RwLock`
//! use), so a handle minted on one goroutine worker thread resolves on
//! another. This is the bit-identical VM mirror of the compiled
//! `gos_rt_shared_*` shims; the difference is only in what a slot holds -
//! a `Value` here, the word carrying it there.
//!
//! `with` / `update` run a closure, so they are `native` builtins: a
//! plain `BuiltinFnPub` cannot call back into the interpreter. Both hold
//! the lock across the callback, which is what makes a read-modify-write
//! whole rather than a read and a later write that another goroutine can
//! land between.

use std::collections::HashMap as StdHashMap;
use std::sync::Arc;

use crate::builtins::BuiltinFnPub;
use crate::value::{NativeDispatch, RuntimeResult, Value};

/// A builtin the interpreter dispatches through, so it can call a closure.
type NativeBuiltin = fn(&mut dyn NativeDispatch, &[Value]) -> RuntimeResult<Value>;

type Cell = parking_lot::Mutex<Value>;

pub(crate) fn install_shared(globals: &mut Vec<(&'static str, Value)>) {
    let plain: &[(&str, BuiltinFnPub)] = &[
        ("Shared::new", builtin_shared_new),
        ("sync::Shared::new", builtin_shared_new),
        ("Shared::get", builtin_shared_get),
        ("sync::Shared::get", builtin_shared_get),
        ("Shared::set", builtin_shared_set),
        ("sync::Shared::set", builtin_shared_set),
    ];
    for (name, call) in plain {
        globals.push((*name, crate::builtins::builtin_pub(name, *call)));
    }
    let native: &[(&str, NativeBuiltin)] = &[
        ("Shared::with", native_shared_with),
        ("sync::Shared::with", native_shared_with),
        ("Shared::update", native_shared_update),
        ("sync::Shared::update", native_shared_update),
    ];
    for (name, call) in native {
        globals.push((*name, Value::native(name, *call)));
    }
}

fn cell_of(value: &Value) -> Option<Arc<Cell>> {
    crate::value::handle_state(value, "sync::Shared", "__shared")
}

pub(crate) fn builtin_shared_new(args: &[Value]) -> RuntimeResult<Value> {
    let initial = args.first().cloned().unwrap_or(Value::Unit);
    Ok(crate::value::state_handle(
        "sync::Shared",
        "__shared",
        Arc::new(Cell::new(initial)),
    ))
}

pub(crate) fn builtin_shared_get(args: &[Value]) -> RuntimeResult<Value> {
    Ok(args
        .first()
        .and_then(cell_of)
        .map_or(Value::Unit, |cell| cell.lock().clone()))
}

pub(crate) fn builtin_shared_set(args: &[Value]) -> RuntimeResult<Value> {
    let next = args.get(1).cloned().unwrap_or(Value::Unit);
    if let Some(cell) = args.first().and_then(cell_of) {
        *cell.lock() = next;
    }
    Ok(Value::Unit)
}

pub(crate) fn native_shared_with(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let Some(cell) = args.first().and_then(cell_of) else {
        return Ok(Value::Unit);
    };
    let Some(f) = args.get(1).cloned() else {
        return Ok(Value::Unit);
    };
    // The guard is held across the callback so a reader sees one whole
    // value rather than a state a writer is midway through.
    let guard = cell.lock();
    let value = guard.clone();
    let result = dispatch.call_value(&f, vec![value]);
    drop(guard);
    result
}

pub(crate) fn native_shared_update(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let Some(cell) = args.first().and_then(cell_of) else {
        return Ok(Value::Unit);
    };
    let Some(f) = args.get(1).cloned() else {
        return Ok(Value::Unit);
    };
    let mut guard = cell.lock();
    let current = guard.clone();
    // The lock spans the read and the write, so two goroutines updating
    // at once cannot lose one another's work.
    let next = dispatch.call_value(&f, vec![current])?;
    *guard = next.clone();
    drop(guard);
    Ok(next)
}
