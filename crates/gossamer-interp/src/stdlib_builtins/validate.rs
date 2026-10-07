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
//! `std::validate` builtins for the bytecode VM - the `FieldError`
//! and `Errors` data handles. Both handles are structs carrying an
//! `id`; the real state lives in a process-global registry keyed by
//! `id`, so `&mut self` methods mutate through the registry rather
//! than relying on the VM's receiver write-back (mirrors `sync::Map`
//! / `math::rand::Rng`).
//!
//! The `Validate` trait surface is not wired here: dispatching a
//! user-type `validate()` impl needs per-type vtable resolution that
//! the global-name method dispatch does not yet model. `FieldError` +
//! `Errors` are the usable data surface and are fully tier-parallel.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashMap as StdHashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, Ordering};

use gossamer_ast::Ident;

use crate::builtins::{BuiltinFnPub, as_str, value_to_int};
use crate::value::{RuntimeResult, Value};

#[derive(Clone, Default)]
struct FieldErrorData {
    path: String,
    message: String,
    code: String,
}

type FeMap = StdHashMap<i64, FieldErrorData>;
type ErrsMap = StdHashMap<i64, BTreeMap<String, Vec<FieldErrorData>>>;

static FE_REGISTRY: LazyLock<parking_lot::ReentrantMutex<RefCell<FeMap>>> =
    LazyLock::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
static ERRS_REGISTRY: LazyLock<parking_lot::ReentrantMutex<RefCell<ErrsMap>>> =
    LazyLock::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

fn with_fe<R>(f: impl FnOnce(&RefCell<FeMap>) -> R) -> R {
    let guard = FE_REGISTRY.lock();
    f(&guard)
}

fn with_errs<R>(f: impl FnOnce(&RefCell<ErrsMap>) -> R) -> R {
    let guard = ERRS_REGISTRY.lock();
    f(&guard)
}

pub(crate) fn install_validate(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        // Constructors (free path calls).
        ("FieldError::new", builtin_field_error_new),
        ("validate::FieldError::new", builtin_field_error_new),
        ("Errors::new", builtin_errors_new),
        ("validate::Errors::new", builtin_errors_new),
        // FieldError methods (dispatched via the `validate::FieldError`
        // struct-name key that `qualified_method_key` forms).
        ("validate::FieldError::path", builtin_field_error_path),
        ("validate::FieldError::message", builtin_field_error_message),
        ("validate::FieldError::code", builtin_field_error_code),
        // Errors methods.
        ("validate::Errors::add", builtin_errors_add),
        ("validate::Errors::is_empty", builtin_errors_is_empty),
        ("validate::Errors::len", builtin_errors_len),
        ("validate::Errors::count", builtin_errors_count),
        ("validate::Errors::get", builtin_errors_get),
        ("validate::Errors::collect", builtin_errors_collect),
    ];
    for (name, call) in entries {
        globals.push((*name, crate::builtins::builtin_pub(name, *call)));
    }
}

fn fe_handle(id: i64) -> Value {
    let key = super::set::registry_key(id, |id| {
        let removed = with_fe(|r| r.borrow_mut().remove(&id));
        drop(removed);
    });
    Value::struct_("validate::FieldError", vec![("__fe", key)])
}

fn errs_handle(id: i64) -> Value {
    let key = super::set::registry_key(id, |id| {
        let removed = with_errs(|r| r.borrow_mut().remove(&id));
        drop(removed);
    });
    Value::struct_("validate::Errors", vec![("__errs", key)])
}

fn id_of(value: &Value, ty: &str, field: &str) -> Option<i64> {
    if let Value::Struct(inner) = value {
        if inner.name == ty {
            for (i, v) in &inner.fields {
                if (*i) == field {
                    return super::set::registry_id(v);
                }
            }
        }
    }
    None
}

pub(crate) fn builtin_field_error_new(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("").to_string();
    let message = args.get(1).and_then(as_str).unwrap_or("").to_string();
    let code = args.get(2).and_then(as_str).unwrap_or("").to_string();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    with_fe(|r| {
        r.borrow_mut().insert(
            id,
            FieldErrorData {
                path,
                message,
                code,
            },
        );
    });
    Ok(fe_handle(id))
}

fn fe_field(args: &[Value], pick: impl Fn(&FieldErrorData) -> String) -> Value {
    let Some(id) = args
        .first()
        .and_then(|v| id_of(v, "validate::FieldError", "__fe"))
    else {
        return Value::String(String::new().into());
    };
    let out = with_fe(|r| r.borrow().get(&id).map(&pick)).unwrap_or_default();
    Value::String(out.into())
}

pub(crate) fn builtin_field_error_path(args: &[Value]) -> RuntimeResult<Value> {
    Ok(fe_field(args, |d| d.path.clone()))
}

pub(crate) fn builtin_field_error_message(args: &[Value]) -> RuntimeResult<Value> {
    Ok(fe_field(args, |d| d.message.clone()))
}

pub(crate) fn builtin_field_error_code(args: &[Value]) -> RuntimeResult<Value> {
    Ok(fe_field(args, |d| d.code.clone()))
}

pub(crate) fn builtin_errors_new(_args: &[Value]) -> RuntimeResult<Value> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    with_errs(|r| {
        r.borrow_mut().insert(id, BTreeMap::new());
    });
    Ok(errs_handle(id))
}

pub(crate) fn builtin_errors_add(args: &[Value]) -> RuntimeResult<Value> {
    let Some(eid) = args
        .first()
        .and_then(|v| id_of(v, "validate::Errors", "__errs"))
    else {
        return Ok(Value::Unit);
    };
    let field = args.get(1).and_then(as_str).unwrap_or("").to_string();
    let Some(fid) = args
        .get(2)
        .and_then(|v| id_of(v, "validate::FieldError", "__fe"))
    else {
        return Ok(Value::Unit);
    };
    let data = with_fe(|r| r.borrow().get(&fid).cloned()).unwrap_or_default();
    with_errs(|r| {
        if let Some(m) = r.borrow_mut().get_mut(&eid) {
            m.entry(field).or_default().push(data);
        }
    });
    Ok(Value::Unit)
}

pub(crate) fn builtin_errors_is_empty(args: &[Value]) -> RuntimeResult<Value> {
    let Some(eid) = args
        .first()
        .and_then(|v| id_of(v, "validate::Errors", "__errs"))
    else {
        return Ok(Value::Bool(true));
    };
    let empty = with_errs(|r| r.borrow().get(&eid).map(BTreeMap::is_empty)).unwrap_or(true);
    Ok(Value::Bool(empty))
}

pub(crate) fn builtin_errors_len(args: &[Value]) -> RuntimeResult<Value> {
    let Some(eid) = args
        .first()
        .and_then(|v| id_of(v, "validate::Errors", "__errs"))
    else {
        return Ok(Value::Int(0));
    };
    let n = with_errs(|r| {
        r.borrow()
            .get(&eid)
            .map(|m| m.values().map(Vec::len).sum::<usize>() as i64)
    })
    .unwrap_or(0);
    Ok(Value::Int(n))
}

pub(crate) fn builtin_errors_count(args: &[Value]) -> RuntimeResult<Value> {
    let Some(eid) = args
        .first()
        .and_then(|v| id_of(v, "validate::Errors", "__errs"))
    else {
        return Ok(Value::Int(0));
    };
    let field = args.get(1).and_then(as_str).unwrap_or("");
    let n = with_errs(|r| {
        r.borrow()
            .get(&eid)
            .and_then(|m| m.get(field))
            .map(|v| v.len() as i64)
    })
    .unwrap_or(0);
    Ok(Value::Int(n))
}

pub(crate) fn builtin_errors_get(args: &[Value]) -> RuntimeResult<Value> {
    let Some(eid) = args
        .first()
        .and_then(|v| id_of(v, "validate::Errors", "__errs"))
    else {
        return Ok(Value::String(String::new().into()));
    };
    let field = args.get(1).and_then(as_str).unwrap_or("");
    let joined = with_errs(|r| {
        r.borrow().get(&eid).map(|m| {
            m.get(field)
                .map(|v| {
                    v.iter()
                        .map(|e| e.message.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_default()
        })
    })
    .unwrap_or_default();
    Ok(Value::String(joined.into()))
}

pub(crate) fn builtin_errors_collect(args: &[Value]) -> RuntimeResult<Value> {
    let Some(eid) = args
        .first()
        .and_then(|v| id_of(v, "validate::Errors", "__errs"))
    else {
        return Ok(Value::String(String::new().into()));
    };
    let rendered = with_errs(|r| {
        r.borrow().get(&eid).map(|m| {
            let mut parts: Vec<String> = Vec::new();
            for (field, list) in m {
                for e in list {
                    parts.push(format!("{field}: {}", e.message));
                }
            }
            parts.join("; ")
        })
    })
    .unwrap_or_default();
    Ok(Value::String(rendered.into()))
}
