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

pub(crate) fn install_sync_extras(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("AtomicI64::new", builtin_atomic_i64_new),
        ("AtomicI64::load", builtin_atomic_i64_load),
        ("AtomicI64::store", builtin_atomic_i64_store),
        ("AtomicI64::fetch_add", builtin_atomic_i64_fetch_add),
        ("AtomicI64::fetch_sub", builtin_atomic_i64_fetch_sub),
        ("AtomicI64::compare_exchange", builtin_atomic_i64_cas),
        ("AtomicI32::new", builtin_atomic_i32_new),
        ("AtomicI32::load", builtin_atomic_i32_load),
        ("AtomicI32::store", builtin_atomic_i32_store),
        ("AtomicI32::fetch_add", builtin_atomic_i32_fetch_add),
        ("AtomicI32::fetch_sub", builtin_atomic_i32_fetch_sub),
        ("AtomicI32::compare_exchange", builtin_atomic_i32_cas),
        ("AtomicBool::new", builtin_atomic_bool_new),
        ("AtomicBool::load", builtin_atomic_bool_load),
        ("AtomicBool::store", builtin_atomic_bool_store),
        ("AtomicBool::compare_exchange", builtin_atomic_bool_cas),
        ("Mutex::new", builtin_mutex_new),
        ("Mutex::lock", builtin_mutex_lock),
        ("Mutex::unlock", builtin_mutex_unlock),
        ("Once::new", builtin_once_new),
        ("Map::new", builtin_sync_map_new),
        ("Map::insert", builtin_sync_map_set),
        ("Map::get", builtin_sync_map_get),
        ("Map::remove", builtin_sync_map_delete),
        ("Map::len", builtin_sync_map_len),
        ("Map::contains_key", builtin_sync_map_contains),
        ("Map::keys", builtin_sync_map_keys),
    ];
    for (name, call) in entries {
        let qualified: &'static str = Box::leak(format!("sync::{name}").into_boxed_str());
        globals.push((qualified, crate::builtins::builtin_pub(qualified, *call)));
        // The bare `Map::*` spellings name the collections map, so the
        // concurrent map is reachable only through `sync::Map::*`.
        if !name.starts_with("Map::") {
            globals.push((*name, crate::builtins::builtin_pub(name, *call)));
        }
    }

    // `Once::call(o, || ...)` runs a closure, so it must be a `native`
    // builtin with access to the interpreter dispatcher (a plain
    // `BuiltinFnPub` cannot invoke the callback). Register both the
    // bare and `sync::`-qualified spellings.
    let once_call_entries: &[&str] = &["sync::Once::call", "Once::call"];
    for name in once_call_entries {
        globals.push((*name, Value::native(name, native_once_call)));
    }
}

fn sync_map_id_of(value: &Value) -> Option<i64> {
    if let Value::Struct(inner) = value {
        if inner.name == "sync::Map" {
            for (i, v) in &inner.fields {
                if (*i) == "__map" {
                    if let Value::Int(n) = v {
                        return Some(*n);
                    }
                }
            }
        }
    }
    None
}

pub(crate) fn builtin_sync_map_new(_args: &[Value]) -> RuntimeResult<Value> {
    let id = next_atomic_id();
    SYNC_MAP_REGISTRY.with(|r| {
        r.borrow_mut()
            .insert(id, Arc::new(parking_lot::RwLock::new(StdHashMap::new())));
    });
    Ok(Value::struct_(
        "sync::Map",
        Arc::unwrap_or_clone(Arc::new(vec![("__map", Value::Int(id))])),
    ))
}

pub(crate) fn builtin_sync_map_set(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(sync_map_id_of) else {
        return Ok(Value::Unit);
    };
    let key = args.get(1).and_then(as_str).unwrap_or("").to_string();
    let val = args.get(2).and_then(as_str).unwrap_or("").to_string();
    let arc = SYNC_MAP_REGISTRY.with(|r| r.borrow().get(&id).cloned());
    if let Some(m) = arc {
        m.write().insert(key, val);
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_sync_map_get(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(sync_map_id_of) else {
        return Ok(none_variant());
    };
    let key = args.get(1).and_then(as_str).unwrap_or("");
    let arc = SYNC_MAP_REGISTRY.with(|r| r.borrow().get(&id).cloned());
    Ok(match arc {
        Some(m) => match m.read().get(key) {
            Some(v) => some_variant(Value::String(v.clone().into())),
            None => none_variant(),
        },
        None => none_variant(),
    })
}

pub(crate) fn builtin_sync_map_delete(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(sync_map_id_of) else {
        return Ok(Value::Unit);
    };
    let key = args.get(1).and_then(as_str).unwrap_or("");
    let arc = SYNC_MAP_REGISTRY.with(|r| r.borrow().get(&id).cloned());
    if let Some(m) = arc {
        m.write().remove(key);
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_sync_map_len(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(sync_map_id_of) else {
        return Ok(Value::Int(0));
    };
    let arc = SYNC_MAP_REGISTRY.with(|r| r.borrow().get(&id).cloned());
    Ok(Value::Int(match arc {
        Some(m) => m.read().len() as i64,
        None => 0,
    }))
}

pub(crate) fn builtin_sync_map_contains(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(sync_map_id_of) else {
        return Ok(Value::Bool(false));
    };
    let key = args.get(1).and_then(as_str).unwrap_or("");
    let arc = SYNC_MAP_REGISTRY.with(|r| r.borrow().get(&id).cloned());
    Ok(Value::Bool(match arc {
        Some(m) => m.read().contains_key(key),
        None => false,
    }))
}

pub(crate) fn builtin_sync_map_keys(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(sync_map_id_of) else {
        return Ok(Value::Array(Arc::new(Vec::new())));
    };
    let arc = SYNC_MAP_REGISTRY.with(|r| r.borrow().get(&id).cloned());
    Ok(Value::Array(Arc::new(match arc {
        Some(m) => m
            .read()
            .keys()
            .map(|k| Value::String(k.clone().into()))
            .collect(),
        None => Vec::new(),
    })))
}

pub(crate) fn builtin_atomic_i64_new(args: &[Value]) -> RuntimeResult<Value> {
    let init = args.first().and_then(value_to_int).unwrap_or(0);
    let id = next_atomic_id();
    ATOMIC_I64_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, Arc::new(StdAtomicI64::new(init)));
    });
    Ok(atomic_handle("sync::AtomicI64", id))
}

pub(crate) fn with_atomic_i64<R>(
    value: &Value,
    f: impl FnOnce(&Arc<StdAtomicI64>) -> R,
) -> Option<R> {
    let id = atomic_id_of(value, "sync::AtomicI64")?;
    ATOMIC_I64_REGISTRY.with(|r| r.borrow().get(&id).map(f))
}

pub(crate) fn builtin_atomic_i64_load(args: &[Value]) -> RuntimeResult<Value> {
    let n = args
        .first()
        .and_then(|v| with_atomic_i64(v, |a| a.load(Ordering::SeqCst)))
        .unwrap_or(0);
    Ok(Value::Int(n))
}

pub(crate) fn builtin_atomic_i64_store(args: &[Value]) -> RuntimeResult<Value> {
    let val = args.get(1).and_then(value_to_int).unwrap_or(0);
    if let Some(handle) = args.first() {
        let _ = with_atomic_i64(handle, |a| a.store(val, Ordering::SeqCst));
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_atomic_i64_fetch_add(args: &[Value]) -> RuntimeResult<Value> {
    let delta = args.get(1).and_then(value_to_int).unwrap_or(0);
    let prev = args
        .first()
        .and_then(|v| with_atomic_i64(v, |a| a.fetch_add(delta, Ordering::SeqCst)))
        .unwrap_or(0);
    Ok(Value::Int(prev))
}

pub(crate) fn builtin_atomic_i64_fetch_sub(args: &[Value]) -> RuntimeResult<Value> {
    let delta = args.get(1).and_then(value_to_int).unwrap_or(0);
    let prev = args
        .first()
        .and_then(|v| with_atomic_i64(v, |a| a.fetch_sub(delta, Ordering::SeqCst)))
        .unwrap_or(0);
    Ok(Value::Int(prev))
}

pub(crate) fn builtin_atomic_i64_cas(args: &[Value]) -> RuntimeResult<Value> {
    let current = args.get(1).and_then(value_to_int).unwrap_or(0);
    let new = args.get(2).and_then(value_to_int).unwrap_or(0);
    let ok = args
        .first()
        .and_then(|v| {
            with_atomic_i64(v, |a| {
                a.compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            })
        })
        .unwrap_or(false);
    Ok(Value::Bool(ok))
}

/// `AtomicI32::new(v)`: an i64 cell holding an i32, under a handle name of
/// its own so its arithmetic wraps at 32 bits.
pub(crate) fn builtin_atomic_i32_new(args: &[Value]) -> RuntimeResult<Value> {
    let init = args.first().and_then(value_to_int).unwrap_or(0);
    let id = next_atomic_id();
    ATOMIC_I64_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, Arc::new(StdAtomicI64::new(init)));
    });
    Ok(atomic_handle("sync::AtomicI32", id))
}

fn with_atomic_i32<R>(value: &Value, f: impl FnOnce(&Arc<StdAtomicI64>) -> R) -> Option<R> {
    let id = atomic_id_of(value, "sync::AtomicI32")?;
    ATOMIC_I64_REGISTRY.with(|r| r.borrow().get(&id).map(f))
}

pub(crate) fn builtin_atomic_i32_load(args: &[Value]) -> RuntimeResult<Value> {
    let n = args
        .first()
        .and_then(|v| with_atomic_i32(v, |a| a.load(Ordering::SeqCst)))
        .unwrap_or(0);
    Ok(Value::Int(n))
}

pub(crate) fn builtin_atomic_i32_store(args: &[Value]) -> RuntimeResult<Value> {
    let val = args.get(1).and_then(value_to_int).unwrap_or(0);
    if let Some(handle) = args.first() {
        let _ = with_atomic_i32(handle, |a| a.store(val, Ordering::SeqCst));
    }
    Ok(Value::Unit)
}

/// Applies `step` to the i32 the handle's cell holds, answering the prior
/// value.
fn atomic_i32_update(args: &[Value], step: impl Fn(i32, i32) -> i32) -> RuntimeResult<Value> {
    let delta = args.get(1).and_then(value_to_int).unwrap_or(0) as i32;
    let prev = args
        .first()
        .and_then(|v| {
            with_atomic_i32(v, |a| {
                a.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                    Some(i64::from(step(cur as i32, delta)))
                })
                .unwrap_or_else(|cur| cur)
            })
        })
        .unwrap_or(0);
    Ok(Value::Int(i64::from(prev as i32)))
}

pub(crate) fn builtin_atomic_i32_fetch_add(args: &[Value]) -> RuntimeResult<Value> {
    atomic_i32_update(args, i32::wrapping_add)
}

pub(crate) fn builtin_atomic_i32_fetch_sub(args: &[Value]) -> RuntimeResult<Value> {
    atomic_i32_update(args, i32::wrapping_sub)
}

pub(crate) fn builtin_atomic_i32_cas(args: &[Value]) -> RuntimeResult<Value> {
    let current = args.get(1).and_then(value_to_int).unwrap_or(0);
    let new = args.get(2).and_then(value_to_int).unwrap_or(0);
    let ok = args
        .first()
        .and_then(|v| {
            with_atomic_i32(v, |a| {
                a.compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            })
        })
        .unwrap_or(false);
    Ok(Value::Bool(ok))
}

pub(crate) fn builtin_atomic_bool_new(args: &[Value]) -> RuntimeResult<Value> {
    let init = matches!(args.first(), Some(Value::Bool(true)));
    let id = next_atomic_id();
    ATOMIC_BOOL_REGISTRY.with(|r| {
        r.borrow_mut()
            .insert(id, Arc::new(StdAtomicBool::new(init)));
    });
    Ok(atomic_handle("sync::AtomicBool", id))
}

pub(crate) fn with_atomic_bool<R>(
    value: &Value,
    f: impl FnOnce(&Arc<StdAtomicBool>) -> R,
) -> Option<R> {
    let id = atomic_id_of(value, "sync::AtomicBool")?;
    ATOMIC_BOOL_REGISTRY.with(|r| r.borrow().get(&id).map(f))
}

pub(crate) fn builtin_atomic_bool_load(args: &[Value]) -> RuntimeResult<Value> {
    let v = args
        .first()
        .and_then(|v| with_atomic_bool(v, |a| a.load(Ordering::SeqCst)))
        .unwrap_or(false);
    Ok(Value::Bool(v))
}

pub(crate) fn builtin_atomic_bool_store(args: &[Value]) -> RuntimeResult<Value> {
    let val = matches!(args.get(1), Some(Value::Bool(true)));
    if let Some(handle) = args.first() {
        let _ = with_atomic_bool(handle, |a| a.store(val, Ordering::SeqCst));
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_atomic_bool_cas(args: &[Value]) -> RuntimeResult<Value> {
    let current = matches!(args.get(1), Some(Value::Bool(true)));
    let new = matches!(args.get(2), Some(Value::Bool(true)));
    let ok = args
        .first()
        .and_then(|v| {
            with_atomic_bool(v, |a| {
                a.compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            })
        })
        .unwrap_or(false);
    Ok(Value::Bool(ok))
}

pub(crate) fn builtin_mutex_new(_args: &[Value]) -> RuntimeResult<Value> {
    let id = next_atomic_id();
    MUTEX_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, Arc::new(MutexCell::default()));
    });
    Ok(Value::struct_(
        "sync::Mutex",
        Arc::unwrap_or_clone(Arc::new(vec![("__mutex", Value::Int(id))])),
    ))
}

pub(crate) fn mutex_id_of(value: &Value) -> Option<i64> {
    if let Value::Struct(inner) = value {
        if inner.name == "sync::Mutex" {
            for (i, v) in &inner.fields {
                if (*i) == "__mutex" {
                    if let Value::Int(n) = v {
                        return Some(*n);
                    }
                }
            }
        }
    }
    None
}

pub(crate) fn builtin_mutex_lock(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(mutex_id_of) else {
        return Ok(Value::Unit);
    };
    // Clone the handle out of the registry before acquiring, so the
    // registry lock is not held while this goroutine parks on the
    // mutex's condvar.
    let arc = MUTEX_REGISTRY.with(|r| r.borrow().get(&id).cloned());
    if let Some(cell) = arc {
        // A held mutex stays held on the browser build: the goroutine that
        // would unlock it has already run to completion.
        if !gossamer_runtime::platform::CAN_BLOCK && cell.would_block() {
            return Err(crate::value::RuntimeError::WouldNeverWake("Mutex::lock"));
        }
        cell.lock();
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_mutex_unlock(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(mutex_id_of) else {
        return Ok(Value::Unit);
    };
    let arc = MUTEX_REGISTRY.with(|r| r.borrow().get(&id).cloned());
    if let Some(cell) = arc {
        cell.unlock();
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_once_new(_args: &[Value]) -> RuntimeResult<Value> {
    let id = next_atomic_id();
    ONCE_REGISTRY.with(|r| {
        r.borrow_mut()
            .insert(id, Arc::new(parking_lot::Once::new()));
    });
    Ok(Value::struct_(
        "sync::Once",
        Arc::unwrap_or_clone(Arc::new(vec![("__once", Value::Int(id))])),
    ))
}

/// `Once::call(o, f)` - run `f` exactly once across all callers of the
/// handle. Native so the closure can be invoked through the interpreter
/// dispatcher; returns `true` on the call that executed the body, mirror
/// of the compiled `gos_rt_once_call`.
pub(crate) fn native_once_call(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let id = match args.first() {
        Some(Value::Struct(inner)) if inner.name == "sync::Once" => inner
            .fields
            .iter()
            .find_map(|(i, v)| {
                if (*i) == "__once" {
                    if let Value::Int(n) = v {
                        Some(*n)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .unwrap_or(0),
        _ => return Ok(Value::Bool(false)),
    };
    let Some(f) = args.get(1).cloned() else {
        return Ok(Value::Bool(false));
    };
    let arc = ONCE_REGISTRY.with(|r| r.borrow().get(&id).cloned());
    let mut ran = false;
    let mut call_result: RuntimeResult<Value> = Ok(Value::Unit);
    if let Some(once) = arc {
        once.call_once(|| {
            ran = true;
            call_result = dispatch.call_value(&f, Vec::new());
        });
    }
    call_result?;
    Ok(Value::Bool(ran))
}

// ----------------------------------------------------------------------
// math
