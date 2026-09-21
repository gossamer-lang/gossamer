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
    BuiltinFnPub, array_as_values, as_str, err_variant, install_module_pub, none_variant,
    ok_variant, some_variant, value_to_int,
};
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeResult, Value};

/// One set's elements, in the order they were added. A `Set` traverses in
/// that order and gives it no meaning beyond being the same on every tier; a
/// `BTreeSet` sorts on the way out.
/// A set's elements keyed by value: insertion-ordered for a `Set`, in the
/// ordered tree for a `BTreeSet`.
pub(crate) type SetEntries = crate::vm_map::VmMap;

/// Entry point invoked from `builtins::install`.
use super::*;

pub(crate) fn install_set(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("Set::new", builtin_set_new),
        ("Set::from", builtin_set_from),
        ("Set::insert", builtin_set_insert),
        ("Set::remove", builtin_set_remove),
        ("Set::contains", builtin_set_contains),
        ("Set::len", builtin_set_len),
        ("Set::is_empty", builtin_set_is_empty),
        ("Set::clear", builtin_set_clear),
        ("Set::to_vec", builtin_set_to_vec),
        ("Set::iter", builtin_set_to_vec),
        ("Set::union", builtin_set_union),
        ("Set::intersection", builtin_set_intersection),
        ("Set::difference", builtin_set_difference),
        (
            "Set::symmetric_difference",
            builtin_set_symmetric_difference,
        ),
        ("Set::is_subset", builtin_set_is_subset),
        ("Set::is_superset", builtin_set_is_superset),
        ("Set::is_disjoint", builtin_set_is_disjoint),
        ("collections::Set::new", builtin_set_new),
        ("collections::Set::from", builtin_set_from),
        ("collections::Set::insert", builtin_set_insert),
        ("collections::Set::remove", builtin_set_remove),
        ("collections::Set::contains", builtin_set_contains),
        ("collections::Set::len", builtin_set_len),
        ("collections::Set::is_empty", builtin_set_is_empty),
        ("collections::Set::clear", builtin_set_clear),
        ("collections::Set::to_vec", builtin_set_to_vec),
        ("collections::Set::iter", builtin_set_to_vec),
        ("collections::Set::union", builtin_set_union),
        ("collections::Set::intersection", builtin_set_intersection),
        ("collections::Set::difference", builtin_set_difference),
        (
            "collections::Set::symmetric_difference",
            builtin_set_symmetric_difference,
        ),
        ("collections::Set::is_subset", builtin_set_is_subset),
        ("collections::Set::is_superset", builtin_set_is_superset),
        ("collections::Set::is_disjoint", builtin_set_is_disjoint),
        (
            "HashSet::symmetric_difference",
            builtin_set_symmetric_difference,
        ),
        ("BTreeSet::new", builtin_btreeset_new),
        ("BTreeSet::from", builtin_btreeset_from),
        ("BTreeSet::insert", builtin_set_insert),
        ("BTreeSet::remove", builtin_set_remove),
        ("BTreeSet::contains", builtin_set_contains),
        ("BTreeSet::len", builtin_set_len),
        ("BTreeSet::is_empty", builtin_set_is_empty),
        ("BTreeSet::clear", builtin_set_clear),
        ("BTreeSet::to_vec", builtin_set_to_vec),
        ("BTreeSet::iter", builtin_set_to_vec),
        ("BTreeSet::union", builtin_set_union),
        ("BTreeSet::intersection", builtin_set_intersection),
        ("BTreeSet::difference", builtin_set_difference),
        (
            "BTreeSet::symmetric_difference",
            builtin_set_symmetric_difference,
        ),
        ("BTreeSet::is_subset", builtin_set_is_subset),
        ("BTreeSet::is_superset", builtin_set_is_superset),
        ("BTreeSet::is_disjoint", builtin_set_is_disjoint),
        ("collections::BTreeSet::new", builtin_btreeset_new),
        ("collections::BTreeSet::from", builtin_btreeset_from),
    ];
    for (name, call) in entries {
        globals.push((*name, crate::builtins::builtin_pub(name, *call)));
    }
}

pub(crate) fn next_set_handle() -> i64 {
    NEXT_SET_HANDLE.with(|c| {
        let mut v = c.borrow_mut();
        let id = *v;
        *v += 1;
        id
    })
}

pub(crate) fn set_handle(id: i64) -> Value {
    set_handle_named("Set", id)
}

pub(crate) fn set_handle_named(name: &'static str, id: i64) -> Value {
    Value::struct_(
        name,
        Arc::unwrap_or_clone(Arc::new(vec![("__set", Value::Int(id))])),
    )
}

/// Deep-clones a `Set` / `BTreeSet` handle: mints a fresh registry id,
/// copies the source id's entries into it, and returns a handle to the
/// copy. A `Value::Struct` handle clone (`.clone()`, `Op::Move`) only
/// copies the `__set` id, so the clone and the source alias the same
/// `SET_REGISTRY` slot - inserting through one is visible through the
/// other. `Op::CloneMapLike` calls this for a `let` / by-value-argument
/// binding so a `Set` gets the same independent-copy semantics `Map`
/// gets from cloning its `Arc<Mutex<DenseMap>>` contents. Returns `value`
/// unchanged if it isn't a recognised set handle.
pub(crate) fn set_deep_clone(value: &Value) -> Value {
    let Value::Struct(inner) = value else {
        return value.clone();
    };
    if !matches!(inner.name.as_str(), "Set" | "BTreeSet") {
        return value.clone();
    }
    let Some(id) = set_id_of(value) else {
        return value.clone();
    };
    let new_id = next_set_handle();
    let entries = SET_REGISTRY.with(|r| r.borrow().get(&id).cloned().unwrap_or_default());
    SET_REGISTRY.with(|r| {
        r.borrow_mut().insert(new_id, entries);
    });
    let name: &'static str = if inner.name.as_str() == "BTreeSet" {
        "BTreeSet"
    } else {
        "Set"
    };
    let handle = set_handle_named(name, new_id);
    // A rendered copy carries its element descriptor; the clone a `let` or a
    // by-value argument takes must keep it, or the elements lose the spelling
    // their type gives them on the way out.
    if let Some((_, elem_desc)) = inner
        .fields
        .iter()
        .find(|(field, _)| **field == crate::value::ELEM_DESC_MARKER)
        && let Value::Struct(cloned) = &handle
    {
        let mut fields = cloned.fields.to_vec();
        fields.push((crate::value::ELEM_DESC_MARKER, elem_desc.clone()));
        return Value::struct_(name, fields);
    }
    handle
}

/// The elements a set handle stands for, keyed the way the set keys them.
/// `None` when `value` is not a set handle.
pub(crate) fn set_entries_of(value: &Value) -> Option<SetEntries> {
    let id = set_id_of(value)?;
    SET_REGISTRY.with(|r| r.borrow().get(&id).cloned())
}

pub(crate) fn set_id_of(value: &Value) -> Option<i64> {
    if let Value::Struct(inner) = value {
        if matches!(inner.name.as_str(), "Set" | "BTreeSet") {
            for (i, v) in &inner.fields {
                if (*i) == "__set" {
                    if let Value::Int(n) = v {
                        return Some(*n);
                    }
                }
            }
        }
    }
    None
}

pub(crate) fn builtin_set_new(_args: &[Value]) -> RuntimeResult<Value> {
    builtin_set_new_named("Set")
}

pub(crate) fn builtin_btreeset_new(_args: &[Value]) -> RuntimeResult<Value> {
    builtin_set_new_named("BTreeSet")
}

fn builtin_set_new_named(name: &'static str) -> RuntimeResult<Value> {
    let id = next_set_handle();
    let entries = if name == "BTreeSet" {
        SetEntries::ordered(crate::vm_map::KeyOrder::Natural)
    } else {
        SetEntries::default()
    };
    SET_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, entries);
    });
    Ok(set_handle_named(name, id))
}

fn builtin_set_from(args: &[Value]) -> RuntimeResult<Value> {
    builtin_set_from_named("Set", args)
}

fn builtin_btreeset_from(args: &[Value]) -> RuntimeResult<Value> {
    builtin_set_from_named("BTreeSet", args)
}

fn builtin_set_from_named(name: &'static str, args: &[Value]) -> RuntimeResult<Value> {
    let handle = builtin_set_new_named(name)?;
    let Some(values) = args.first().and_then(array_as_values) else {
        return Ok(handle);
    };
    for value in values {
        builtin_set_insert(&[handle.clone(), value])?;
    }
    Ok(handle)
}

pub(crate) fn builtin_set_insert(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(set_id_of) else {
        return Ok(Value::Bool(false));
    };
    let Some(value) = args.get(1) else {
        return Ok(Value::Bool(false));
    };
    let key = MapKey::from_value(value);
    let inserted = SET_REGISTRY.with(|r| {
        if let Some(s) = r.borrow_mut().get_mut(&id) {
            s.insert(key, value.clone()).is_none()
        } else {
            false
        }
    });
    Ok(Value::Bool(inserted))
}

pub(crate) fn builtin_set_remove(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(set_id_of) else {
        return Ok(Value::Bool(false));
    };
    let Some(value) = args.get(1) else {
        return Ok(Value::Bool(false));
    };
    let key = MapKey::from_value(value);
    let removed = SET_REGISTRY.with(|r| {
        if let Some(s) = r.borrow_mut().get_mut(&id) {
            s.shift_remove(&key).is_some()
        } else {
            false
        }
    });
    Ok(Value::Bool(removed))
}

pub(crate) fn builtin_set_contains(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(set_id_of) else {
        return Ok(Value::Bool(false));
    };
    let Some(value) = args.get(1) else {
        return Ok(Value::Bool(false));
    };
    let key = MapKey::from_value(value);
    let has = SET_REGISTRY.with(|r| r.borrow().get(&id).is_some_and(|s| s.contains_key(&key)));
    Ok(Value::Bool(has))
}

pub(crate) fn builtin_set_len(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(set_id_of) else {
        return Ok(Value::Int(0));
    };
    let n = SET_REGISTRY.with(|r| r.borrow().get(&id).map_or(0, SetEntries::len));
    Ok(Value::Int(n as i64))
}

pub(crate) fn builtin_set_is_empty(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(set_id_of) else {
        return Ok(Value::Bool(true));
    };
    let empty = SET_REGISTRY.with(|r| r.borrow().get(&id).is_none_or(SetEntries::is_empty));
    Ok(Value::Bool(empty))
}

pub(crate) fn builtin_set_clear(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(id) = args.first().and_then(set_id_of) {
        SET_REGISTRY.with(|r| {
            if let Some(set) = r.borrow_mut().get_mut(&id) {
                set.clear();
            }
        });
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_set_to_vec(args: &[Value]) -> RuntimeResult<Value> {
    let values = args.first().and_then(set_snapshot).unwrap_or_default();
    Ok(Value::Array(Arc::new(values)))
}

/// The stored values in the order a walk sees them: sorted for a `BTreeSet`,
/// and otherwise the order the elements were added, which a `Set` gives no
/// meaning to beyond being the same on every tier.
pub(crate) fn set_snapshot(value: &Value) -> Option<Vec<Value>> {
    set_values(value, set_handle_name(Some(value)) == "BTreeSet")
}

/// The stored values sorted by key, for rendering and serialization: printed
/// and encoded output stays stable whatever order the elements went in.
pub(crate) fn set_display_snapshot(value: &Value) -> Option<Vec<Value>> {
    set_values(value, true)
}

fn set_values(value: &Value, sorted: bool) -> Option<Vec<Value>> {
    let id = set_id_of(value)?;
    // An element type that writes its own `cmp` seats each element by that
    // body as it arrives, so the walk is already the set's order.
    let (values, by_element_order) = SET_REGISTRY.with(|r| {
        r.borrow()
            .get(&id)
            .map(|s| {
                let by_element_order =
                    matches!(s.key_order(), Some(crate::vm_map::KeyOrder::User(_)));
                let mut entries: Vec<(&MapKey, &Value)> = s.iter().collect();
                if sorted && !by_element_order {
                    entries.sort_by_key(|(key, _)| (*key).clone());
                }
                let values = entries
                    .into_iter()
                    .map(|(_, value)| value.clone())
                    .collect();
                (values, by_element_order)
            })
            .unwrap_or_default()
    });
    // A stored key orders an element the way the language orders it, except
    // one declared `u64` / `usize`, whose bits order unsigned. A handle the
    // compiler described carries that element descriptor.
    Some(if sorted && !by_element_order {
        crate::value::described_set_order(value, values)
    } else {
        values
    })
}

fn set_pair_ids(args: &[Value]) -> Option<(i64, i64)> {
    Some((
        args.first().and_then(set_id_of)?,
        args.get(1).and_then(set_id_of)?,
    ))
}

fn set_handle_name(value: Option<&Value>) -> &'static str {
    match value {
        Some(Value::Struct(inner)) if inner.name == "BTreeSet" => "BTreeSet",
        _ => "Set",
    }
}

/// Runs a binary set operation over the two operand handles and stores the
/// result under a fresh handle.
fn set_binary_op(
    args: &[Value],
    op: impl Fn(&SetEntries, &SetEntries) -> SetEntries,
) -> RuntimeResult<Value> {
    let result = match set_pair_ids(args) {
        Some((a, b)) => SET_REGISTRY.with(|r| {
            let r = r.borrow();
            let sa = r.get(&a).cloned().unwrap_or_default();
            let sb = r.get(&b).cloned().unwrap_or_default();
            op(&sa, &sb)
        }),
        None => SetEntries::default(),
    };
    let id = next_set_handle();
    SET_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, result);
    });
    Ok(set_handle_named(set_handle_name(args.first()), id))
}

fn set_predicate(
    args: &[Value],
    pred: impl Fn(&SetEntries, &SetEntries) -> bool,
) -> RuntimeResult<Value> {
    let result = match set_pair_ids(args) {
        Some((a, b)) => SET_REGISTRY.with(|r| {
            let r = r.borrow();
            let sa = r.get(&a).cloned().unwrap_or_default();
            let sb = r.get(&b).cloned().unwrap_or_default();
            pred(&sa, &sb)
        }),
        None => false,
    };
    Ok(Value::Bool(result))
}

pub(crate) fn builtin_set_union(args: &[Value]) -> RuntimeResult<Value> {
    set_binary_op(args, |a, b| {
        let mut result = a.clone();
        for (key, value) in b {
            if !result.contains_key(key) {
                result.insert(key.clone(), value.clone());
            }
        }
        result
    })
}

pub(crate) fn builtin_set_intersection(args: &[Value]) -> RuntimeResult<Value> {
    set_binary_op(args, |a, b| {
        let mut result = a.empty_like();
        result.extend(
            a.iter()
                .filter(|(key, _)| b.contains_key(key))
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        result
    })
}

pub(crate) fn builtin_set_difference(args: &[Value]) -> RuntimeResult<Value> {
    set_binary_op(args, |a, b| {
        let mut result = a.empty_like();
        result.extend(
            a.iter()
                .filter(|(key, _)| !b.contains_key(key))
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        result
    })
}

pub(crate) fn builtin_set_symmetric_difference(args: &[Value]) -> RuntimeResult<Value> {
    set_binary_op(args, |a, b| {
        let mut result = a.empty_like();
        result.extend(
            a.iter()
                .filter(|(key, _)| !b.contains_key(key))
                .chain(b.iter().filter(|(key, _)| !a.contains_key(key)))
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        result
    })
}

pub(crate) fn builtin_set_is_subset(args: &[Value]) -> RuntimeResult<Value> {
    set_predicate(args, |a, b| a.keys().all(|key| b.contains_key(key)))
}

pub(crate) fn builtin_set_is_superset(args: &[Value]) -> RuntimeResult<Value> {
    set_predicate(args, |a, b| b.keys().all(|key| a.contains_key(key)))
}

pub(crate) fn builtin_set_is_disjoint(args: &[Value]) -> RuntimeResult<Value> {
    set_predicate(args, |a, b| a.keys().all(|key| !b.contains_key(key)))
}

// ----------------------------------------------------------------------
// sync extras (atomics + mutex + once)

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool as StdAtomicBool, AtomicI64 as StdAtomicI64, Ordering};

/// Process-global registry shared across goroutines. The sync
/// primitives (atomics, mutex, once, map) hand user code an integer
/// handle; the real `Arc<...>` lives here keyed by that handle. This
/// MUST be global rather than `thread_local!`: goroutines run on an OS
/// worker-thread pool, so a handle minted on one thread has to resolve
/// on another (a `thread_local!` registry silently lost every
/// cross-goroutine update). A `ReentrantMutex<RefCell<_>>` keeps the
/// old `.with(|r| r.borrow()/.borrow_mut())` call sites unchanged while
/// serializing cross-thread access; same-thread reentrancy still hits
/// RefCell's borrow checks exactly as the previous thread_local did.
pub(crate) struct GlobalReg<T: 'static>(LazyLock<parking_lot::ReentrantMutex<RefCell<T>>>);

impl<T: 'static> GlobalReg<T> {
    pub(crate) const fn new(init: fn() -> parking_lot::ReentrantMutex<RefCell<T>>) -> Self {
        Self(LazyLock::new(init))
    }

    pub(crate) fn with<R>(&self, f: impl FnOnce(&RefCell<T>) -> R) -> R {
        let guard = self.0.lock();
        f(&guard)
    }
}

pub(crate) static NEXT_ATOMIC_ID: GlobalReg<i64> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(1)));
pub(crate) static ATOMIC_I64_REGISTRY: GlobalReg<StdHashMap<i64, Arc<StdAtomicI64>>> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
pub(crate) static ATOMIC_BOOL_REGISTRY: GlobalReg<StdHashMap<i64, Arc<StdAtomicBool>>> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
pub(crate) static MUTEX_REGISTRY: GlobalReg<StdHashMap<i64, Arc<MutexCell>>> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));

// Process-global (not `thread_local!`): goroutines run on an OS
// worker-thread pool, so a set handle minted on one thread must
// resolve on another after the goroutine migrates between workers.
// Mirrors the `sync::*` registries above.
pub(crate) static NEXT_SET_HANDLE: GlobalReg<i64> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(1)));
pub(crate) static SET_REGISTRY: GlobalReg<StdHashMap<i64, SetEntries>> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));

/// Backing state for an interpreter `sync::Mutex`. The lock is held
/// from a `lock()` call until the matching `unlock()`, so a
/// read-modify-write on shared state performed by user code between
/// the two is serialized across goroutines - mirroring the compiled
/// tier's `gos_rt_mutex_lock`/`gos_rt_mutex_unlock`. Acquisition parks
/// the contending goroutine's worker thread on the condvar (the same
/// blocking discipline `Channel::recv` uses) rather than spinning.
pub(crate) struct MutexCell {
    /// `true` while a goroutine holds the lock.
    held: parking_lot::Mutex<bool>,
    /// Signalled by `unlock()` so one parked acquirer can proceed.
    available: parking_lot::Condvar,
    /// Protected payload, readable on `lock()` and writable via
    /// `store()`. Guarded by its own short-held lock so it never
    /// contends with the held mutual-exclusion lock.
    value: parking_lot::Mutex<Value>,
}

impl MutexCell {
    /// Creates an unlocked cell protecting `value`.
    pub(crate) fn new(value: Value) -> Self {
        Self {
            held: parking_lot::Mutex::new(false),
            available: parking_lot::Condvar::new(),
            value: parking_lot::Mutex::new(value),
        }
    }

    /// Whether a `lock` entered now would have to park.
    pub(crate) fn would_block(&self) -> bool {
        *self.held.lock()
    }

    /// Acquires the lock, parking until it is free, and returns a
    /// clone of the protected value.
    pub(crate) fn lock(&self) -> Value {
        let mut held = self.held.lock();
        while *held {
            self.available.wait(&mut held);
        }
        *held = true;
        drop(held);
        self.value.lock().clone()
    }

    /// Releases the lock and wakes one parked acquirer.
    pub(crate) fn unlock(&self) {
        let mut held = self.held.lock();
        *held = false;
        drop(held);
        self.available.notify_one();
    }

    /// Overwrites the protected value.
    pub(crate) fn store(&self, value: Value) {
        *self.value.lock() = value;
    }
}
pub(crate) static ONCE_REGISTRY: GlobalReg<StdHashMap<i64, Arc<parking_lot::Once>>> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
#[allow(
    clippy::type_complexity,
    reason = "the registry's type is the shared map it holds, and naming it would hide that"
)]
pub(crate) static SYNC_MAP_REGISTRY: GlobalReg<
    StdHashMap<i64, Arc<parking_lot::RwLock<StdHashMap<String, String>>>>,
> = GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));

pub(crate) fn next_atomic_id() -> i64 {
    NEXT_ATOMIC_ID.with(|c| {
        let mut v = c.borrow_mut();
        let id = *v;
        *v += 1;
        id
    })
}

pub(crate) fn atomic_handle(name: &'static str, id: i64) -> Value {
    Value::struct_(
        name,
        Arc::unwrap_or_clone(Arc::new(vec![("__atomic", Value::Int(id))])),
    )
}

pub(crate) fn atomic_id_of(value: &Value, expected: &str) -> Option<i64> {
    if let Value::Struct(inner) = value {
        if inner.name == expected {
            for (i, v) in &inner.fields {
                if (*i) == "__atomic" {
                    if let Value::Int(n) = v {
                        return Some(*n);
                    }
                }
            }
        }
    }
    None
}

/// `s.__window(lo, hi, take)` on a `BTreeSet`: a set of its elements ranked
/// `lo..hi`, the primitive its `first` / `pop_first` / `pop_last` desugar to.
/// `None` when the receiver is not a set.
pub(crate) fn set_window(args: &[Value]) -> Option<Value> {
    let id = set_id_of(args.first()?)?;
    let int = |i: usize| args.get(i).and_then(value_to_int).unwrap_or(0);
    let window = SET_REGISTRY.with(|r| {
        r.borrow_mut()
            .get_mut(&id)
            .map(|entries| entries.window(int(1), int(2), int(3) != 0))
    })?;
    Some(register_btree_set(window))
}

/// `s.__range(lo, hi, mode)` on a `BTreeSet`: a set of its elements between
/// two bounds, under the same mode word a `BTreeMap` range takes.
pub(crate) fn set_range(args: &[Value]) -> Option<Value> {
    let id = set_id_of(args.first()?)?;
    let mode = args.get(3).and_then(value_to_int).unwrap_or(0);
    let bound = |i: usize| MapKey::from_value(args.get(i).unwrap_or(&Value::Unit));
    let window = SET_REGISTRY.with(|r| {
        r.borrow_mut().get_mut(&id).map(|entries| {
            let lo = if mode & 1 != 0 {
                entries.rank(&bound(1), false) as i64
            } else {
                0
            };
            let hi = if mode & 2 != 0 {
                entries.rank(&bound(2), mode & 4 != 0) as i64
            } else {
                i64::MAX
            };
            entries.window(lo, hi, false)
        })
    })?;
    Some(register_btree_set(window))
}

fn register_btree_set(entries: SetEntries) -> Value {
    let id = next_set_handle();
    SET_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, entries);
    });
    set_handle_named("BTreeSet", id)
}

#[cfg(test)]
mod set_registry_tests {
    use super::*;
    use std::thread;

    // A HashSet handle minted on one worker thread must stay usable
    // from another: goroutines migrate across the OS worker pool, so the
    // backing registry has to be process-global, not thread-local. A
    // thread-local registry would leave the handle's entry invisible (an
    // empty map) on the second thread, so the insert would no-op and
    // `len` / `contains` would read 0 / false.
    #[test]
    fn set_handle_survives_thread_boundary() {
        let handle = thread::spawn(|| builtin_set_new(&[]).unwrap())
            .join()
            .unwrap();
        builtin_set_insert(&[handle.clone(), Value::Int(7)]).unwrap();
        builtin_set_insert(&[handle.clone(), Value::Int(9)]).unwrap();
        match builtin_set_len(std::slice::from_ref(&handle)).unwrap() {
            Value::Int(n) => assert_eq!(n, 2),
            other => panic!("expected Int, got {other:?}"),
        }
        match builtin_set_contains(&[handle.clone(), Value::Int(7)]).unwrap() {
            Value::Bool(b) => assert!(b),
            other => panic!("expected Bool, got {other:?}"),
        }
    }

    #[test]
    fn set_snapshot_preserves_aggregate_values() {
        let handle = builtin_set_new(&[]).unwrap();
        let first = Value::struct_("Point", vec![("x", Value::Int(1)), ("y", Value::Int(2))]);
        let equal = Value::struct_("Point", vec![("x", Value::Int(1)), ("y", Value::Int(2))]);

        assert!(matches!(
            builtin_set_insert(&[handle.clone(), first.clone()]).unwrap(),
            Value::Bool(true)
        ));
        assert!(matches!(
            builtin_set_insert(&[handle.clone(), equal]).unwrap(),
            Value::Bool(false)
        ));
        let Value::Array(snapshot) = builtin_set_to_vec(std::slice::from_ref(&handle)).unwrap()
        else {
            panic!("expected array snapshot");
        };
        assert_eq!(snapshot.len(), 1);
        assert!(matches!(snapshot.first(), Some(Value::Struct(_))));
        assert_eq!(
            snapshot.first().map(MapKey::from_value),
            Some(MapKey::from_value(&first))
        );
        assert_eq!(handle.repr(), "#{Point { x: 1, y: 2 }}");
    }
}
