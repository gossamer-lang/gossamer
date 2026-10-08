//! VM builtins for `std::collections::Deque<T>` and restricted queue/stack wrappers.
//!
//! On the bytecode tier, a deque value is represented as
//! `Value::Struct { name: "...", fields: [("__deque" | "__queue" | "__stack", id)] }`
//! where `id` is a handle into `DEQUE_REGISTRY`. Method dispatch
//! routes through `qualified_key(owner, method)` so each exposed owner
//! must register its source-facing method names.

use std::cell::RefCell;
use std::collections::{HashMap as StdHashMap, VecDeque as StdVecDeque};
use std::sync::Arc;

use gossamer_ast::Ident;

use crate::builtins::{BuiltinFnPub, builtin_pub, none_variant, some_variant};
use crate::value::{RuntimeResult, Value};

use super::set::GlobalReg;

// Process-global (not `thread_local!`): goroutines run on an OS
// worker-thread pool, so a deque handle minted on one thread must
// resolve on another after the goroutine migrates between workers.
// Mirrors the `sync::*` registries.
static NEXT_DEQUE_HANDLE: GlobalReg<i64> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(1)));
static DEQUE_REGISTRY: GlobalReg<StdHashMap<i64, StdVecDeque<Value>>> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));

/// Install all Deque VM builtins into the global table.
pub(crate) fn install_deque(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("Deque::new", builtin_deque_new),
        ("collections::Deque::new", builtin_deque_new),
        ("Deque::from", builtin_deque_from),
        ("collections::Deque::from", builtin_deque_from),
        ("Queue::new", builtin_queue_new),
        ("collections::Queue::new", builtin_queue_new),
        ("Queue::from", builtin_queue_from),
        ("collections::Queue::from", builtin_queue_from),
        ("Queue::push", builtin_deque_push_back),
        ("Queue::pop", builtin_deque_pop_front),
        ("Queue::peek", builtin_deque_peek_front),
        ("Queue::len", builtin_deque_len),
        ("Queue::is_empty", builtin_deque_is_empty),
        ("Queue::clear", builtin_deque_clear),
        ("Stack::new", builtin_stack_new),
        ("collections::Stack::new", builtin_stack_new),
        ("Stack::from", builtin_stack_from),
        ("collections::Stack::from", builtin_stack_from),
        ("Stack::push", builtin_deque_push_back),
        ("Stack::pop", builtin_deque_pop_back),
        ("Stack::peek", builtin_deque_peek_back),
        ("Stack::len", builtin_deque_len),
        ("Stack::is_empty", builtin_deque_is_empty),
        ("Stack::clear", builtin_deque_clear),
        ("Deque::push_back", builtin_deque_push_back),
        ("Deque::push_front", builtin_deque_push_front),
        ("Deque::pop_front", builtin_deque_pop_front),
        ("Deque::pop_back", builtin_deque_pop_back),
        ("Deque::peek_front", builtin_deque_peek_front),
        ("Deque::peek_back", builtin_deque_peek_back),
        ("Deque::len", builtin_deque_len),
        ("Deque::is_empty", builtin_deque_is_empty),
        ("Deque::clear", builtin_deque_clear),
    ];
    for (name, call) in entries {
        globals.push((*name, builtin_pub(name, *call)));
    }
}

fn next_deque_handle() -> i64 {
    NEXT_DEQUE_HANDLE.with(|c| {
        let mut v = c.borrow_mut();
        let id = *v;
        *v += 1;
        id
    })
}

fn deque_handle(id: i64) -> Value {
    deque_handle_named(id, "Deque")
}

fn deque_handle_named(id: i64, name: &'static str) -> Value {
    let field = match name {
        "Queue" => "__queue",
        "Stack" => "__stack",
        _ => "__deque",
    };
    let key = super::set::registry_key(id, |id| DEQUE_REGISTRY.retire(id));
    Value::struct_(name, vec![(field, key)])
}

/// A deque, queue, or stack with its own registry entry, so a binding taken
/// from another leaves that one untouched. Any other value clones the cheap
/// way `Value::clone` does.
pub(crate) fn deque_deep_clone(value: &Value) -> Value {
    let Value::Struct(inner) = value else {
        return value.clone();
    };
    let name: &'static str = match inner.name.as_str() {
        "Deque" => "Deque",
        "Queue" => "Queue",
        "Stack" => "Stack",
        _ => return value.clone(),
    };
    let Some(id) = deque_id_of(value) else {
        return value.clone();
    };
    let entries = DEQUE_REGISTRY.with(|r| r.borrow().get(&id).cloned().unwrap_or_default());
    let new_id = next_deque_handle();
    DEQUE_REGISTRY.with(|r| {
        r.borrow_mut().insert(new_id, entries);
    });
    let handle = deque_handle_named(new_id, name);
    carry_render_fields(&handle, inner)
}

/// `handle` with every non-identity field of `source` copied onto it. A
/// rendered handle carries the element descriptor that tells a `Vec`
/// element from a fixed-array one; a clone that dropped it would render
/// the copy in the other spelling.
fn carry_render_fields(handle: &Value, source: &crate::value::StructInner) -> Value {
    let extra: Vec<(&'static str, Value)> = source
        .fields
        .iter()
        .filter(|(name, _)| !matches!(**name, "__deque" | "__queue" | "__stack" | "__heap"))
        .map(|(name, value)| (*name, value.clone()))
        .collect();
    if extra.is_empty() {
        return handle.clone();
    }
    let Value::Struct(new_inner) = handle else {
        return handle.clone();
    };
    let mut fields = new_inner.fields.to_vec();
    fields.extend(extra);
    Value::struct_(new_inner.name.as_str(), fields)
}

pub(crate) fn deque_snapshot(value: &Value) -> Option<Vec<Value>> {
    let id = deque_id_of(value)?;
    DEQUE_REGISTRY.with(|r| r.borrow().get(&id).map(|d| d.iter().cloned().collect()))
}

fn deque_id_of(value: &Value) -> Option<i64> {
    if let Value::Struct(inner) = value {
        if matches!(inner.name.as_str(), "Deque" | "Queue" | "Stack") {
            for (i, v) in &inner.fields {
                if matches!(*i, "__deque" | "__queue" | "__stack") {
                    return super::set::registry_id(v);
                }
            }
        }
    }
    None
}

fn builtin_deque_new(_args: &[Value]) -> RuntimeResult<Value> {
    builtin_deque_new_named("Deque")
}

fn builtin_queue_new(_args: &[Value]) -> RuntimeResult<Value> {
    builtin_deque_new_named("Queue")
}

fn builtin_stack_new(_args: &[Value]) -> RuntimeResult<Value> {
    builtin_deque_new_named("Stack")
}

fn builtin_deque_new_named(name: &'static str) -> RuntimeResult<Value> {
    let id = next_deque_handle();
    DEQUE_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, StdVecDeque::new());
    });
    Ok(deque_handle_named(id, name))
}

fn builtin_deque_from(args: &[Value]) -> RuntimeResult<Value> {
    builtin_deque_from_named(args, "Deque")
}

fn builtin_queue_from(args: &[Value]) -> RuntimeResult<Value> {
    builtin_deque_from_named(args, "Queue")
}

fn builtin_stack_from(args: &[Value]) -> RuntimeResult<Value> {
    builtin_deque_from_named(args, "Stack")
}

fn builtin_deque_from_named(args: &[Value], name: &'static str) -> RuntimeResult<Value> {
    let id = next_deque_handle();
    let deque = StdVecDeque::from(crate::stdlib_builtins::encoding_pem::collect_array(
        args.first().unwrap_or(&Value::Unit),
    ));
    DEQUE_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, deque);
    });
    Ok(deque_handle_named(id, name))
}

fn builtin_deque_push_back(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args.first().cloned().unwrap_or(Value::Unit);
    let Some(id) = deque_id_of(&handle) else {
        return Ok(handle);
    };
    let value = args.get(1).cloned().unwrap_or(Value::Unit);
    DEQUE_REGISTRY.with(|r| {
        if let Some(d) = r.borrow_mut().get_mut(&id) {
            d.push_back(value);
        }
    });
    Ok(handle)
}

fn builtin_deque_push_front(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args.first().cloned().unwrap_or(Value::Unit);
    let Some(id) = deque_id_of(&handle) else {
        return Ok(handle);
    };
    let value = args.get(1).cloned().unwrap_or(Value::Unit);
    DEQUE_REGISTRY.with(|r| {
        if let Some(d) = r.borrow_mut().get_mut(&id) {
            d.push_front(value);
        }
    });
    Ok(handle)
}

fn builtin_deque_pop_front(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(deque_id_of) else {
        return Ok(none_variant());
    };
    let result = DEQUE_REGISTRY.with(|r| r.borrow_mut().get_mut(&id).and_then(|d| d.pop_front()));
    match result {
        Some(v) => Ok(some_variant(v)),
        None => Ok(none_variant()),
    }
}

fn builtin_deque_pop_back(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(deque_id_of) else {
        return Ok(none_variant());
    };
    let result = DEQUE_REGISTRY.with(|r| r.borrow_mut().get_mut(&id).and_then(|d| d.pop_back()));
    match result {
        Some(v) => Ok(some_variant(v)),
        None => Ok(none_variant()),
    }
}

fn builtin_deque_peek_front(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(deque_id_of) else {
        return Ok(none_variant());
    };
    let result = DEQUE_REGISTRY.with(|r| r.borrow().get(&id).and_then(|d| d.front().cloned()));
    match result {
        Some(v) => Ok(some_variant(v)),
        None => Ok(none_variant()),
    }
}

fn builtin_deque_peek_back(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(deque_id_of) else {
        return Ok(none_variant());
    };
    let result = DEQUE_REGISTRY.with(|r| r.borrow().get(&id).and_then(|d| d.back().cloned()));
    match result {
        Some(v) => Ok(some_variant(v)),
        None => Ok(none_variant()),
    }
}

fn builtin_deque_len(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(deque_id_of) else {
        return Ok(Value::Int(0));
    };
    let n = DEQUE_REGISTRY.with(|r| r.borrow().get(&id).map_or(0, StdVecDeque::len));
    Ok(Value::Int(n as i64))
}

fn builtin_deque_is_empty(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(deque_id_of) else {
        return Ok(Value::Bool(true));
    };
    let empty = DEQUE_REGISTRY.with(|r| r.borrow().get(&id).is_none_or(StdVecDeque::is_empty));
    Ok(Value::Bool(empty))
}

fn builtin_deque_clear(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(deque_id_of) else {
        return Ok(Value::Unit);
    };
    // The elements are dropped after the registry is released: one may be a
    // deque whose last handle retires its own entry.
    let elements = DEQUE_REGISTRY.with(|r| r.borrow_mut().get_mut(&id).map(std::mem::take));
    drop(elements);
    Ok(Value::Unit)
}

#[cfg(test)]
mod deque_registry_tests {
    use super::*;
    use std::thread;

    fn int_of(v: &Value) -> i64 {
        match v {
            Value::Int(n) => *n,
            other => panic!("expected Int, got {other:?}"),
        }
    }

    // A deque handle minted on one worker thread must stay usable from
    // another: goroutines migrate across the OS worker pool, so the
    // backing registry has to be process-global, not thread-local. A
    // thread-local registry would leave the handle's entry invisible
    // (an empty map) on the second thread, so the push would no-op and
    // `len` would read 0.
    #[test]
    fn deque_handle_survives_thread_boundary() {
        let handle = thread::spawn(|| builtin_deque_new(&[]).unwrap())
            .join()
            .unwrap();
        builtin_deque_push_back(&[handle.clone(), Value::Int(10)]).unwrap();
        builtin_deque_push_back(&[handle.clone(), Value::Int(20)]).unwrap();
        assert_eq!(
            int_of(&builtin_deque_len(std::slice::from_ref(&handle)).unwrap()),
            2
        );
        match builtin_deque_pop_front(std::slice::from_ref(&handle)).unwrap() {
            Value::Variant(inner) => {
                assert_eq!(inner.name, "Some");
                assert_eq!(int_of(&inner.fields[0]), 10);
            }
            other => panic!("expected Some, got {other:?}"),
        }
    }

    fn live(id: i64) -> bool {
        DEQUE_REGISTRY.with(|r| r.borrow().contains_key(&id))
    }

    #[test]
    fn a_deque_entry_retires_with_its_last_handle_nested_ones_too() {
        let outer = builtin_deque_new(&[]).unwrap();
        let inner = builtin_deque_new(&[]).unwrap();
        let (outer_id, inner_id) = (deque_id_of(&outer).unwrap(), deque_id_of(&inner).unwrap());
        builtin_deque_push_back(&[outer.clone(), inner]).unwrap();
        let copy = outer.clone();
        drop(outer);
        assert!(
            live(outer_id) && live(inner_id),
            "a copy still holds the outer deque"
        );
        builtin_deque_clear(std::slice::from_ref(&copy)).unwrap();
        assert!(
            !live(inner_id),
            "clearing dropped the inner deque's last handle"
        );
        builtin_deque_push_back(&[copy.clone(), builtin_deque_new(&[]).unwrap()]).unwrap();
        drop(copy);
        assert!(!live(outer_id));
    }
}
