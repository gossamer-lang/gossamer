#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::doc_markdown,
    clippy::needless_pass_by_value,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::single_call_fn,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
//! Gossamer-callable `std::sort` builtins: the explicit stable-order and
//! sorted-sequence search half of the sequence surface. `Vec`'s inherent
//! `sort` is unstable, so `sort::sort_stable` is the spelling that
//! guarantees equal elements keep their input order.

use std::sync::Arc;

use crate::builtins::{BuiltinFnPub, none_variant, some_variant};
use crate::stdlib_builtins::encoding_pem::collect_array;
use crate::value::{NativeDispatch, RuntimeResult, Value};

use super::*;

/// Entry point invoked from `stdlib_builtins::install`.
pub(crate) fn install_sort(globals: &mut Vec<(&'static str, Value)>) {
    for (name, call) in [
        ("sort::sort_stable", builtin_sort_stable as BuiltinFnPub),
        ("sort::binary_search", builtin_sort_binary_search),
        ("sort::partition_point", builtin_sort_partition_point),
    ] {
        globals.push((name, crate::builtins::builtin_pub(name, call)));
    }
}

/// The language's ordering, the one `<` and `sort` use: a float by total
/// order, and a tuple, struct, sequence, or enum structurally.
fn cmp_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => gossamer_runtime::c_abi::sort::float_order(*x, *y),
        (Value::String(x), Value::String(y)) => x.as_str().cmp(y.as_str()),
        _ => crate::stdlib_builtins::iter::compare_values_total(a, b),
    }
}

// ----------------------------------------------------------------------
// Described ordering. A `u64` / `usize` reaches the VM as an `Int` as often
// as a `Uint`, since its bits are the same, so no value-level rule can order
// it. The compiler hands these builtins the static type's ordering
// descriptor, and each compares the copy [`crate::value::uint_leaves`]
// re-boxes under it, while answering with the values themselves.

/// The ordering descriptor the compiler passed at `at`.
fn desc_arg(args: &[Value], at: usize) -> Vec<u8> {
    match args.get(at) {
        Some(Value::String(text)) => text.as_str().as_bytes().to_vec(),
        _ => Vec::new(),
    }
}

/// `value` as the ordering compares it: its unsigned integers re-boxed as
/// `Value::Uint`, which the VM orders unsigned.
fn ordering_key(value: &Value, desc: &[u8]) -> Value {
    crate::value::uint_leaves(value, desc)
}

fn described_order(a: &Value, b: &Value, desc: &[u8]) -> std::cmp::Ordering {
    crate::stdlib_builtins::iter::compare_values_total(
        &ordering_key(a, desc),
        &ordering_key(b, desc),
    )
}

/// The elements of `source` in ascending described order.
fn described_sorted(source: &Value, desc: &[u8]) -> Vec<Value> {
    let items = collect_array(source);
    let keys: Vec<Value> = items.iter().map(|item| ordering_key(item, desc)).collect();
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| crate::stdlib_builtins::iter::compare_values_total(&keys[a], &keys[b]));
    order.into_iter().map(|at| items[at].clone()).collect()
}

/// Index of the first element of a described-sorted `items` not ordered
/// before `target`.
fn described_lower_bound(items: &[Value], target: &Value, desc: &[u8]) -> usize {
    let target_key = ordering_key(target, desc);
    let (mut lo, mut hi) = (0usize, items.len());
    while lo < hi {
        let mid = usize::midpoint(lo, hi);
        let key = ordering_key(&items[mid], desc);
        if crate::stdlib_builtins::iter::compare_values_total(&key, &target_key)
            == std::cmp::Ordering::Less
        {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// `xs.sort()` over described elements: the sorted sequence, in the
/// receiver's own storage shape.
pub(crate) fn builtin_described_sort(args: &[Value]) -> RuntimeResult<Value> {
    let Some(source) = args.first() else {
        return Ok(Value::Unit);
    };
    let sorted = described_sorted(source, &desc_arg(args, 1));
    Ok(rebuild_like(Some(source), sorted))
}

/// `xs.binary_search(target)` over described elements.
pub(crate) fn builtin_described_binary_search(args: &[Value]) -> RuntimeResult<Value> {
    let items = elements(args.first());
    let target = args.get(1).cloned().unwrap_or(Value::Unit);
    let desc = desc_arg(args, 2);
    let at = described_lower_bound(&items, &target, &desc);
    let found = at < items.len()
        && described_order(&items[at], &target, &desc) == std::cmp::Ordering::Equal;
    Ok(Value::variant(
        if found { "Ok" } else { "Err" },
        vec![Value::Int(at as i64)],
    ))
}

/// `sort::sort_stable(xs)` over described elements.
pub(crate) fn builtin_described_sort_stable(args: &[Value]) -> RuntimeResult<Value> {
    let Some(source) = args.first() else {
        return Ok(Value::Unit);
    };
    let sorted = described_sorted(source, &desc_arg(args, 1));
    Ok(rebuild_like(Some(source), sorted))
}

/// `sort::binary_search(xs, target)` over described elements.
pub(crate) fn builtin_described_search(args: &[Value]) -> RuntimeResult<Value> {
    let items = elements(args.first());
    let Some(target) = args.get(1) else {
        return Ok(none_variant());
    };
    let desc = desc_arg(args, 2);
    let at = described_lower_bound(&items, target, &desc);
    if at < items.len() && described_order(&items[at], target, &desc) == std::cmp::Ordering::Equal {
        Ok(some_variant(Value::Int(at as i64)))
    } else {
        Ok(none_variant())
    }
}

/// `sort::partition_point(xs, pivot)` over described elements.
pub(crate) fn builtin_described_partition_point(args: &[Value]) -> RuntimeResult<Value> {
    let items = elements(args.first());
    let Some(pivot) = args.get(1) else {
        return Ok(Value::Int(0));
    };
    let desc = desc_arg(args, 2);
    Ok(Value::Int(
        described_lower_bound(&items, pivot, &desc) as i64
    ))
}

/// The first element that no later one orders `want` of, as `Some`, or
/// `None` for an empty sequence. An iterator is drained first.
fn described_extreme(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
    want: std::cmp::Ordering,
) -> RuntimeResult<Value> {
    let source = args.first().unwrap_or(&Value::Unit);
    let items = if matches!(source, Value::LazyIter(_)) {
        crate::stdlib_builtins::iter::drain_iter_with_dispatch(source, dispatch)?
    } else {
        collect_array(source)
    };
    let desc = desc_arg(args, 1);
    let mut best: Option<(Value, Value)> = None;
    for item in items {
        let key = ordering_key(&item, &desc);
        let replace = match &best {
            None => true,
            Some((_, best_key)) => {
                crate::stdlib_builtins::iter::compare_values_total(&key, best_key) == want
            }
        };
        if replace {
            best = Some((item, key));
        }
    }
    Ok(best.map_or_else(none_variant, |(item, _)| some_variant(item)))
}

/// `min(xs)` / `iter::min(xs)` / `xs.min()` over described elements.
pub(crate) fn native_described_min(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    described_extreme(dispatch, args, std::cmp::Ordering::Less)
}

/// `max(xs)` / `iter::max(xs)` / `xs.max()` over described elements.
pub(crate) fn native_described_max(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    described_extreme(dispatch, args, std::cmp::Ordering::Greater)
}

/// Scalar `min(a, b)` over described operands.
pub(crate) fn builtin_described_min2(args: &[Value]) -> RuntimeResult<Value> {
    let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
        return Ok(Value::Unit);
    };
    let desc = desc_arg(args, 2);
    Ok(
        if described_order(b, a, &desc) == std::cmp::Ordering::Less {
            b.clone()
        } else {
            a.clone()
        },
    )
}

/// Scalar `max(a, b)` over described operands.
pub(crate) fn builtin_described_max2(args: &[Value]) -> RuntimeResult<Value> {
    let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
        return Ok(Value::Unit);
    };
    let desc = desc_arg(args, 2);
    Ok(
        if described_order(b, a, &desc) == std::cmp::Ordering::Greater {
            b.clone()
        } else {
            a.clone()
        },
    )
}

/// Scalar `clamp(x, lo, hi)` over described operands.
pub(crate) fn builtin_described_clamp(args: &[Value]) -> RuntimeResult<Value> {
    let (Some(x), Some(lo), Some(hi)) = (args.first(), args.get(1), args.get(2)) else {
        return Ok(Value::Unit);
    };
    let desc = desc_arg(args, 3);
    Ok(
        if described_order(x, lo, &desc) == std::cmp::Ordering::Less {
            lo.clone()
        } else if described_order(x, hi, &desc) == std::cmp::Ordering::Greater {
            hi.clone()
        } else {
            x.clone()
        },
    )
}

/// `a < b` / `<=` / `>` / `>=` over described operands. The operator code
/// counts `<`, `<=`, `>`, `>=` from zero.
pub(crate) fn builtin_described_compare(args: &[Value]) -> RuntimeResult<Value> {
    let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
        return Ok(Value::Bool(false));
    };
    let ordering = described_order(a, b, &desc_arg(args, 2));
    Ok(Value::Bool(match args.get(3).and_then(Value::as_i64) {
        Some(0) => ordering.is_lt(),
        Some(1) => ordering.is_le(),
        Some(2) => ordering.is_gt(),
        _ => ordering.is_ge(),
    }))
}

fn elements(v: Option<&Value>) -> Vec<Value> {
    v.map(collect_array).unwrap_or_default()
}

/// Rebuild a sequence in the receiver's storage shape so a packed
/// `IntArray` / `FloatVec` input yields the same representation back.
fn rebuild_like(source: Option<&Value>, items: Vec<Value>) -> Value {
    match source {
        Some(Value::IntArray(_)) => Value::IntArray(Arc::new(
            items
                .iter()
                .map(|v| match v {
                    Value::Int(n) => *n,
                    _ => 0,
                })
                .collect(),
        )),
        Some(Value::FloatVec(_)) => Value::FloatVec(Arc::new(
            items
                .iter()
                .map(|v| match v {
                    Value::Float(f) => *f,
                    Value::Int(n) => *n as f64,
                    _ => 0.0,
                })
                .collect(),
        )),
        _ => Value::Array(Arc::new(items)),
    }
}

/// `sort::sort_stable(xs) -> [T]` - a fresh ascending sequence in which
/// equal elements keep their input order.
pub(crate) fn builtin_sort_stable(args: &[Value]) -> RuntimeResult<Value> {
    let mut items = elements(args.first());
    items.sort_by(cmp_values);
    Ok(rebuild_like(args.first(), items))
}

/// Index of the first element not ordered before `target` in a sorted
/// sequence.
fn lower_bound(items: &[Value], target: &Value) -> usize {
    let (mut lo, mut hi) = (0usize, items.len());
    while lo < hi {
        let mid = usize::midpoint(lo, hi);
        if cmp_values(&items[mid], target) == std::cmp::Ordering::Less {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// `sort::binary_search(xs, target) -> Option<i64>` over a sorted
/// sequence.
pub(crate) fn builtin_sort_binary_search(args: &[Value]) -> RuntimeResult<Value> {
    let items = elements(args.first());
    let Some(target) = args.get(1) else {
        return Ok(none_variant());
    };
    let at = lower_bound(&items, target);
    if at < items.len() && cmp_values(&items[at], target) == std::cmp::Ordering::Equal {
        Ok(some_variant(Value::Int(at as i64)))
    } else {
        Ok(none_variant())
    }
}

/// `sort::partition_point(xs, pivot) -> i64` - the count of elements
/// strictly less than `pivot` in a sorted sequence, which is also the
/// insertion index that keeps it sorted.
pub(crate) fn builtin_sort_partition_point(args: &[Value]) -> RuntimeResult<Value> {
    let items = elements(args.first());
    let Some(pivot) = args.get(1) else {
        return Ok(Value::Int(0));
    };
    Ok(Value::Int(lower_bound(&items, pivot) as i64))
}
