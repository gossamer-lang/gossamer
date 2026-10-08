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
//! `std::math::rand` builtins for the bytecode VM - the deterministic
//! SplitMix64 `Rng`. The handle is a struct `{ __rng: id }`; the real
//! `Rng` state lives in a process-global registry keyed by `id`, so
//! `&mut self` methods mutate through the registry instead of relying
//! on the VM's receiver write-back (mirrors `sync::Map` / `HashSet`).

use std::cell::RefCell;
use std::collections::HashMap as StdHashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, Ordering};

use gossamer_ast::Ident;
use gossamer_std::mathrand::Rng;

use crate::builtins::{BuiltinFnPub, value_to_int};
use crate::value::{RuntimeResult, Value};

/// Process-global RNG registry. Global (not `thread_local!`) so a
/// handle minted on one goroutine worker thread resolves on another,
/// matching the `sync::Map` / `HashSet` registries.
static RNG_REGISTRY: LazyLock<parking_lot::ReentrantMutex<RefCell<StdHashMap<i64, Rng>>>> =
    LazyLock::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
static NEXT_RNG_ID: AtomicI64 = AtomicI64::new(1);

fn with_registry<R>(f: impl FnOnce(&RefCell<StdHashMap<i64, Rng>>) -> R) -> R {
    let guard = RNG_REGISTRY.lock();
    f(&guard)
}

pub(crate) fn install_math_rand(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("Rng::new", builtin_rng_new),
        ("Rng::next_u64", builtin_rng_next_u64),
        ("Rng::next_u32", builtin_rng_next_u32),
        ("Rng::range_u64", builtin_rng_range_u64),
        ("Rng::next_f64", builtin_rng_next_f64),
    ];
    for (name, call) in entries {
        // `Rng::method` resolves a bare `use std::math::rand::Rng`
        // call and the struct-handle method dispatch (the handle is
        // named `rand::Rng`, so `qualified_method_key` emits
        // `rand::Rng::method`). The module-qualified spellings cover
        // `use std::math::rand` (`rand::Rng::method`) and
        // `use std::math` (`math::rand::Rng::method`).
        let rand_q: &'static str = Box::leak(format!("rand::{name}").into_boxed_str());
        let full_q: &'static str = Box::leak(format!("math::rand::{name}").into_boxed_str());
        globals.push((*name, crate::builtins::builtin_pub(name, *call)));
        globals.push((rand_q, crate::builtins::builtin_pub(rand_q, *call)));
        globals.push((full_q, crate::builtins::builtin_pub(full_q, *call)));
    }
}

fn rng_handle(id: i64) -> Value {
    let key = super::set::registry_key(id, |id| {
        with_registry(|r| r.borrow_mut().remove(&id));
    });
    Value::struct_("rand::Rng", vec![("__rng", key)])
}

fn rng_id_of(value: &Value) -> Option<i64> {
    if let Value::Struct(inner) = value {
        if inner.name == "rand::Rng" {
            for (i, v) in &inner.fields {
                if (*i) == "__rng" {
                    return super::set::registry_id(v);
                }
            }
        }
    }
    None
}

pub(crate) fn builtin_rng_new(args: &[Value]) -> RuntimeResult<Value> {
    let seed = args.first().and_then(value_to_int).unwrap_or(0);
    let id = NEXT_RNG_ID.fetch_add(1, Ordering::Relaxed);
    with_registry(|r| {
        r.borrow_mut().insert(id, Rng::new(seed as u64));
    });
    Ok(rng_handle(id))
}

pub(crate) fn builtin_rng_next_u64(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(rng_id_of) else {
        return Ok(Value::Int(0));
    };
    let v = with_registry(|r| r.borrow_mut().get_mut(&id).map(Rng::next_u64));
    Ok(Value::Int(v.unwrap_or(0) as i64))
}

pub(crate) fn builtin_rng_next_u32(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(rng_id_of) else {
        return Ok(Value::Int(0));
    };
    let v = with_registry(|r| r.borrow_mut().get_mut(&id).map(Rng::next_u32));
    Ok(Value::Int(i64::from(v.unwrap_or(0))))
}

pub(crate) fn builtin_rng_range_u64(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(rng_id_of) else {
        return Ok(Value::Int(0));
    };
    let lo = args.get(1).and_then(value_to_int).unwrap_or(0) as u64;
    let hi = args.get(2).and_then(value_to_int).unwrap_or(0) as u64;
    let v = with_registry(|r| {
        r.borrow_mut().get_mut(&id).map(|rng| {
            if hi <= lo {
                lo
            } else {
                lo + rng.next_u64() % (hi - lo)
            }
        })
    });
    Ok(Value::Int(v.unwrap_or(lo) as i64))
}

pub(crate) fn builtin_rng_next_f64(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(rng_id_of) else {
        return Ok(Value::Float(0.0));
    };
    let v = with_registry(|r| r.borrow_mut().get_mut(&id).map(Rng::next_f64));
    Ok(Value::Float(v.unwrap_or(0.0)))
}
