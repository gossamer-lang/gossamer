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

/// Entry point invoked from `builtins::install`.
use super::*;

static NEXT_BINARY_HEAP_HANDLE: super::set::GlobalReg<i64> =
    super::set::GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(1)));
static BINARY_HEAP_REGISTRY: super::set::GlobalReg<StdHashMap<i64, HeapState>> =
    super::set::GlobalReg::new(|| {
        parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new()))
    });

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeapOrder {
    Max,
    Min,
}

#[derive(Clone, Debug)]
struct HeapState {
    owner: &'static str,
    order: HeapOrder,
    values: Vec<Value>,
}

pub(crate) fn install_container_heap(globals: &mut Vec<(&'static str, Value)>) {
    for (name, call) in [
        ("MaxHeap::new", builtin_max_heap_new as BuiltinFnPub),
        ("collections::MaxHeap::new", builtin_max_heap_new),
        ("MaxHeap::from", builtin_max_heap_from),
        ("collections::MaxHeap::from", builtin_max_heap_from),
        ("MaxHeap::push", builtin_binary_heap_push),
        ("MaxHeap::pop", builtin_binary_heap_pop),
        ("MaxHeap::peek", builtin_binary_heap_peek),
        ("MaxHeap::len", builtin_binary_heap_len),
        ("MaxHeap::is_empty", builtin_binary_heap_is_empty),
        ("MaxHeap::clear", builtin_binary_heap_clear),
        ("MinHeap::new", builtin_min_heap_new),
        ("collections::MinHeap::new", builtin_min_heap_new),
        ("MinHeap::from", builtin_min_heap_from),
        ("collections::MinHeap::from", builtin_min_heap_from),
        ("MinHeap::push", builtin_binary_heap_push),
        ("MinHeap::pop", builtin_binary_heap_pop),
        ("MinHeap::peek", builtin_binary_heap_peek),
        ("MinHeap::len", builtin_binary_heap_len),
        ("MinHeap::is_empty", builtin_binary_heap_is_empty),
        ("MinHeap::clear", builtin_binary_heap_clear),
    ] {
        globals.push((name, crate::builtins::builtin_pub(name, call)));
    }
}

fn next_binary_heap_handle() -> i64 {
    NEXT_BINARY_HEAP_HANDLE.with(|c| {
        let mut v = c.borrow_mut();
        let id = *v;
        *v += 1;
        id
    })
}

fn binary_heap_handle(owner: &'static str, id: i64) -> Value {
    let key = super::set::registry_key(id, |id| BINARY_HEAP_REGISTRY.retire(id));
    Value::struct_(owner, vec![("__heap", key)])
}

fn binary_heap_id_of(value: &Value) -> Option<i64> {
    if let Value::Struct(inner) = value
        && matches!(inner.name.as_str(), "MaxHeap" | "MinHeap")
    {
        for (name, field) in &inner.fields {
            if *name == "__heap" {
                return super::set::registry_id(field);
            }
        }
    }
    None
}

/// A heap with its own registry entry, so a binding taken from another
/// leaves that one untouched. Any other value clones the cheap way
/// `Value::clone` does.
pub(crate) fn binary_heap_deep_clone(value: &Value) -> Value {
    let Value::Struct(inner) = value else {
        return value.clone();
    };
    let owner: &'static str = match inner.name.as_str() {
        "MaxHeap" => "MaxHeap",
        "MinHeap" => "MinHeap",
        _ => return value.clone(),
    };
    let Some(id) = binary_heap_id_of(value) else {
        return value.clone();
    };
    let Some(state) = BINARY_HEAP_REGISTRY.with(|r| r.borrow().get(&id).cloned()) else {
        return value.clone();
    };
    let new_id = next_binary_heap_handle();
    BINARY_HEAP_REGISTRY.with(|r| {
        r.borrow_mut().insert(new_id, state);
    });
    let handle = binary_heap_handle(owner, new_id);
    // A rendered handle carries the element descriptor that tells a `Vec`
    // element from a fixed-array one; the copy keeps it.
    let extra: Vec<(&'static str, Value)> = inner
        .fields
        .iter()
        .filter(|(name, _)| **name != "__heap")
        .map(|(name, value)| (*name, value.clone()))
        .collect();
    if extra.is_empty() {
        return handle;
    }
    let Value::Struct(new_inner) = &handle else {
        return handle;
    };
    let mut fields = new_inner.fields.to_vec();
    fields.extend(extra);
    Value::struct_(new_inner.name.as_str(), fields)
}

pub(crate) fn binary_heap_snapshot(value: &Value) -> Option<Vec<Value>> {
    let id = binary_heap_id_of(value)?;
    BINARY_HEAP_REGISTRY.with(|r| r.borrow().get(&id).map(|state| state.values.clone()))
}

fn max_heap_sift_up(xs: &mut [i64], mut i: usize) {
    while i > 0 {
        let parent = (i - 1) / 2;
        if xs[parent] < xs[i] {
            xs.swap(parent, i);
            i = parent;
        } else {
            break;
        }
    }
}

fn max_heap_sift_down(xs: &mut [i64], mut i: usize) {
    loop {
        let l = 2 * i + 1;
        let r = 2 * i + 2;
        let mut largest = i;
        if l < xs.len() && xs[l] > xs[largest] {
            largest = l;
        }
        if r < xs.len() && xs[r] > xs[largest] {
            largest = r;
        }
        if largest == i {
            break;
        }
        xs.swap(largest, i);
        i = largest;
    }
}

fn max_heap_push(xs: &mut Vec<i64>, value: i64) {
    xs.push(value);
    let start = xs.len() - 1;
    max_heap_sift_up(xs, start);
}

fn max_heap_pop(xs: &mut Vec<i64>) -> Option<i64> {
    if xs.is_empty() {
        return None;
    }
    let root = xs[0];
    let last = xs.pop().unwrap_or(root);
    if !xs.is_empty() {
        xs[0] = last;
        max_heap_sift_down(xs, 0);
    }
    Some(root)
}

fn binary_heap_ordering(
    a: &Value,
    b: &Value,
    order: HeapOrder,
) -> RuntimeResult<std::cmp::Ordering> {
    match order {
        HeapOrder::Max => crate::vm::value_ordering(a, b),
        HeapOrder::Min => crate::vm::value_ordering(b, a),
    }
}

fn binary_heap_sift_up(xs: &mut [Value], mut i: usize, order: HeapOrder) -> RuntimeResult<()> {
    while i > 0 {
        let parent = (i - 1) / 2;
        if binary_heap_ordering(&xs[parent], &xs[i], order)?.is_lt() {
            xs.swap(parent, i);
            i = parent;
        } else {
            break;
        }
    }
    Ok(())
}

fn binary_heap_sift_down(xs: &mut [Value], mut i: usize, order: HeapOrder) -> RuntimeResult<()> {
    loop {
        let l = 2 * i + 1;
        let r = 2 * i + 2;
        let mut largest = i;
        if l < xs.len() && binary_heap_ordering(&xs[largest], &xs[l], order)?.is_lt() {
            largest = l;
        }
        if r < xs.len() && binary_heap_ordering(&xs[largest], &xs[r], order)?.is_lt() {
            largest = r;
        }
        if largest == i {
            break;
        }
        xs.swap(largest, i);
        i = largest;
    }
    Ok(())
}

fn binary_heap_push_value(
    xs: &mut Vec<Value>,
    value: Value,
    order: HeapOrder,
) -> RuntimeResult<()> {
    xs.push(value);
    let start = xs.len() - 1;
    binary_heap_sift_up(xs, start, order)
}

fn binary_heap_pop_value(xs: &mut Vec<Value>, order: HeapOrder) -> RuntimeResult<Option<Value>> {
    if xs.is_empty() {
        return Ok(None);
    }
    let root = xs[0].clone();
    let last = xs.pop().unwrap_or_else(|| root.clone());
    if !xs.is_empty() {
        xs[0] = last;
        binary_heap_sift_down(xs, 0, order)?;
    }
    Ok(Some(root))
}

fn heap_extract_values(v: &Value) -> Vec<Value> {
    crate::stdlib_builtins::encoding_pem::collect_array(v)
}

pub(crate) fn heap_extract_i64s(v: &Value) -> Vec<i64> {
    match v {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|e| match e {
                Value::Int(n) => Some(*n),
                Value::Uint(n) => Some(*n as i64),
                _ => None,
            })
            .collect(),
        Value::IntArray(arr) => arr.to_vec(),
        _ => Vec::new(),
    }
}

pub(crate) fn heap_to_value(xs: Vec<i64>) -> Value {
    Value::IntArray(Arc::new(xs))
}

fn binary_heap_new(owner: &'static str, order: HeapOrder) -> Value {
    let id = next_binary_heap_handle();
    BINARY_HEAP_REGISTRY.with(|r| {
        r.borrow_mut().insert(
            id,
            HeapState {
                owner,
                order,
                values: Vec::new(),
            },
        );
    });
    binary_heap_handle(owner, id)
}

fn binary_heap_from(args: &[Value], owner: &'static str, order: HeapOrder) -> RuntimeResult<Value> {
    let id = next_binary_heap_handle();
    let mut heap = Vec::new();
    for value in heap_extract_values(args.first().unwrap_or(&Value::Unit)) {
        binary_heap_push_value(&mut heap, value, order)?;
    }
    BINARY_HEAP_REGISTRY.with(|r| {
        r.borrow_mut().insert(
            id,
            HeapState {
                owner,
                order,
                values: heap,
            },
        );
    });
    Ok(binary_heap_handle(owner, id))
}

fn builtin_max_heap_new(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(binary_heap_new("MaxHeap", HeapOrder::Max))
}

fn builtin_min_heap_new(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(binary_heap_new("MinHeap", HeapOrder::Min))
}

fn builtin_max_heap_from(args: &[Value]) -> RuntimeResult<Value> {
    binary_heap_from(args, "MaxHeap", HeapOrder::Max)
}

fn builtin_min_heap_from(args: &[Value]) -> RuntimeResult<Value> {
    binary_heap_from(args, "MinHeap", HeapOrder::Min)
}

fn builtin_binary_heap_push(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args.first().cloned().unwrap_or(Value::Unit);
    let Some(id) = binary_heap_id_of(&handle) else {
        return Ok(Value::Unit);
    };
    let value = args.get(1).cloned().unwrap_or(Value::Unit);
    BINARY_HEAP_REGISTRY.with(|r| {
        if let Some(state) = r.borrow_mut().get_mut(&id) {
            binary_heap_push_value(&mut state.values, value, state.order)
        } else {
            Ok(())
        }
    })?;
    Ok(Value::Unit)
}

fn builtin_binary_heap_pop(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(binary_heap_id_of) else {
        return Ok(none_variant());
    };
    let result = BINARY_HEAP_REGISTRY.with(|r| {
        let mut registry = r.borrow_mut();
        match registry.get_mut(&id) {
            Some(state) => binary_heap_pop_value(&mut state.values, state.order),
            None => Ok(None),
        }
    })?;
    Ok(result.map_or_else(none_variant, some_variant))
}

fn builtin_binary_heap_peek(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(binary_heap_id_of) else {
        return Ok(none_variant());
    };
    let result = BINARY_HEAP_REGISTRY.with(|r| {
        r.borrow()
            .get(&id)
            .and_then(|state| state.values.first().cloned())
    });
    Ok(result.map_or_else(none_variant, some_variant))
}

fn builtin_binary_heap_len(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(binary_heap_id_of) else {
        return Ok(Value::Int(0));
    };
    let len =
        BINARY_HEAP_REGISTRY.with(|r| r.borrow().get(&id).map_or(0, |state| state.values.len()));
    Ok(Value::Int(len as i64))
}

fn builtin_binary_heap_is_empty(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(binary_heap_id_of) else {
        return Ok(Value::Bool(true));
    };
    let empty = BINARY_HEAP_REGISTRY.with(|r| {
        r.borrow()
            .get(&id)
            .is_none_or(|state| state.values.is_empty())
    });
    Ok(Value::Bool(empty))
}

fn builtin_binary_heap_clear(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(binary_heap_id_of) else {
        return Ok(Value::Unit);
    };
    // The elements are dropped after the registry is released: one may hold
    // the last handle of another heap.
    let elements = BINARY_HEAP_REGISTRY.with(|r| {
        r.borrow_mut()
            .get_mut(&id)
            .map(|state| std::mem::take(&mut state.values))
    });
    drop(elements);
    Ok(Value::Unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_container_results_keep_typed_storage() {
        let value = heap_to_value(vec![1, 3, 2]);
        assert!(matches!(&value, Value::IntArray(xs) if xs.as_slice() == [1, 3, 2]));
        assert_eq!(heap_extract_i64s(&value), [1, 3, 2]);
    }
}
