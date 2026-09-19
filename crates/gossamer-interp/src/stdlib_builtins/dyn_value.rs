//! `DynValue` on the bytecode VM.
//!
//! A dynamic value is carried as the ordinary `Value` it stands for - an
//! integer as `Value::Int`, a named arm as `Value::Variant` - so it renders,
//! compares, and travels exactly as any other value the interpreter holds.
//! The compiled tiers keep the same shapes behind their own node, which is
//! what makes the two agree line for line.

use std::sync::Arc;

use super::*;
use crate::builtins::BuiltinFnPub;
use crate::value::{MapKey, RuntimeResult, SmolStr, Value};

/// The kind name a value answers for `DynValue::kind()`.
fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Unit | Value::Void => "nil",
        Value::Bool(_) => "bool",
        Value::Int(_) | Value::Uint(_) => "int",
        Value::Float(_) => "float",
        Value::Char(_) => "char",
        Value::String(_) => "string",
        Value::IntArray(_)
        | Value::ByteArray(_)
        | Value::ByteVec(_)
        | Value::InlineByteArray(_) => "bytes",
        Value::Array(_) | Value::Tuple(_) | Value::FloatVec(_) => "list",
        Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_) => "map",
        Value::Variant(_) => "tagged",
        _ => "nil",
    }
}

/// The positional children a value carries: an arm's payload, a list's
/// elements, or a byte buffer's bytes.
fn children(value: &Value) -> Vec<Value> {
    match value {
        Value::Variant(v) => v.fields.to_vec(),
        Value::Array(items) | Value::Tuple(items) => items.as_ref().clone(),
        Value::IntArray(items) => items.iter().map(|n| Value::Int(*n)).collect(),
        Value::ByteVec(items) => items.iter().map(|b| Value::Int(i64::from(*b))).collect(),
        Value::ByteArray(items) => items.iter().map(|b| Value::Int(i64::from(*b))).collect(),
        Value::InlineByteArray(items) => items.iter().map(|b| Value::Int(i64::from(*b))).collect(),
        Value::FloatVec(items) => items.iter().map(|f| Value::Float(*f)).collect(),
        Value::Map(m) => m.lock().iter().map(|(_, v)| v.clone()).collect(),
        _ => Vec::new(),
    }
}

fn map_keys(value: &Value) -> Vec<Value> {
    match value {
        Value::Map(m) => m.lock().keys().map(MapKey::to_value).collect(),
        _ => Vec::new(),
    }
}

fn arg(args: &[Value], index: usize) -> Value {
    args.get(index).cloned().unwrap_or(Value::Unit)
}

fn some(value: Value) -> Value {
    Value::variant("Some", vec![value])
}

fn none() -> Value {
    Value::variant("None", Vec::new())
}

fn builtin_dyn_nil(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Unit)
}

fn builtin_dyn_identity(args: &[Value]) -> RuntimeResult<Value> {
    Ok(arg(args, 0))
}

fn builtin_dyn_bytes(args: &[Value]) -> RuntimeResult<Value> {
    let bytes: Vec<i64> = children(&arg(args, 0))
        .iter()
        .map(|v| match v {
            Value::Int(n) => *n & 0xff,
            _ => 0,
        })
        .collect();
    Ok(Value::IntArray(Arc::new(bytes)))
}

fn builtin_dyn_list(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Array(Arc::new(children(&arg(args, 0)))))
}

fn builtin_dyn_map(args: &[Value]) -> RuntimeResult<Value> {
    let keys = children(&arg(args, 0));
    let values = children(&arg(args, 1));
    let mut out = crate::value::dense_map_with_capacity(keys.len());
    for (key, value) in keys.iter().zip(values) {
        out.insert(MapKey::from_value(key), value);
    }
    Ok(Value::Map(Arc::new(parking_lot::Mutex::new(out.into()))))
}

fn builtin_dyn_tagged(args: &[Value]) -> RuntimeResult<Value> {
    let name = match arg(args, 0) {
        Value::String(s) => s.as_str().to_string(),
        other => other.to_string(),
    };
    Ok(Value::variant(
        crate::value::intern_field_name(&name),
        children(&arg(args, 1)),
    ))
}

fn builtin_dyn_kind(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(SmolStr::from(kind_name(&arg(args, 0)))))
}

fn builtin_dyn_name(args: &[Value]) -> RuntimeResult<Value> {
    Ok(match arg(args, 0) {
        Value::Variant(v) => Value::String(SmolStr::from(v.name.as_str())),
        _ => Value::String(SmolStr::from("")),
    })
}

fn builtin_dyn_len(args: &[Value]) -> RuntimeResult<Value> {
    let value = arg(args, 0);
    let count = match &value {
        // Text counts its own scalars, the same length a `String` reports.
        Value::String(s) => s.len(),
        other => children(other).len(),
    };
    Ok(Value::Int(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn builtin_dyn_at(args: &[Value]) -> RuntimeResult<Value> {
    let index = match arg(args, 1) {
        Value::Int(n) => n,
        _ => -1,
    };
    let items = children(&arg(args, 0));
    Ok(usize::try_from(index)
        .ok()
        .and_then(|i| items.get(i).cloned())
        .unwrap_or(Value::Unit))
}

fn builtin_dyn_key_at(args: &[Value]) -> RuntimeResult<Value> {
    let index = match arg(args, 1) {
        Value::Int(n) => n,
        _ => -1,
    };
    let keys = map_keys(&arg(args, 0));
    Ok(usize::try_from(index)
        .ok()
        .and_then(|i| keys.get(i).cloned())
        .unwrap_or(Value::Unit))
}

fn builtin_dyn_as_i64(args: &[Value]) -> RuntimeResult<Value> {
    Ok(match arg(args, 0) {
        Value::Int(n) => some(Value::Int(n)),
        _ => none(),
    })
}

fn builtin_dyn_as_f64(args: &[Value]) -> RuntimeResult<Value> {
    Ok(match arg(args, 0) {
        Value::Float(f) => some(Value::Float(f)),
        _ => none(),
    })
}

fn builtin_dyn_as_bool(args: &[Value]) -> RuntimeResult<Value> {
    Ok(match arg(args, 0) {
        Value::Bool(b) => some(Value::Bool(b)),
        _ => none(),
    })
}

fn builtin_dyn_as_char(args: &[Value]) -> RuntimeResult<Value> {
    Ok(match arg(args, 0) {
        Value::Char(c) => some(Value::Char(c)),
        _ => none(),
    })
}

fn builtin_dyn_as_str(args: &[Value]) -> RuntimeResult<Value> {
    Ok(match arg(args, 0) {
        Value::String(s) => some(Value::String(s)),
        _ => none(),
    })
}

fn builtin_dyn_as_bytes(args: &[Value]) -> RuntimeResult<Value> {
    let value = arg(args, 0);
    Ok(match kind_name(&value) {
        "bytes" => Value::IntArray(Arc::new(
            children(&value)
                .iter()
                .map(|v| match v {
                    Value::Int(n) => *n,
                    _ => 0,
                })
                .collect(),
        )),
        _ => Value::IntArray(Arc::new(Vec::new())),
    })
}

/// Registers the `DynValue::*` constructors and the reader methods.
pub(crate) fn install(globals: &mut Vec<(&'static str, Value)>) {
    // Everything is registered under the qualified `DynValue::` spelling.
    // `kind`, `name`, `at`, and `key_at` also get their bare name, which no
    // other receiver answers to; `len` and the `as_*` family do not, so a
    // bare call keeps routing by receiver shape.
    let qualified: &[(&str, BuiltinFnPub)] = &[
        ("nil", builtin_dyn_nil),
        ("bool", builtin_dyn_identity),
        ("int", builtin_dyn_identity),
        ("float", builtin_dyn_identity),
        ("char", builtin_dyn_identity),
        ("string", builtin_dyn_identity),
        ("bytes", builtin_dyn_bytes),
        ("list", builtin_dyn_list),
        ("map", builtin_dyn_map),
        ("tagged", builtin_dyn_tagged),
        ("len", builtin_dyn_len),
        ("as_i64", builtin_dyn_as_i64),
        ("as_f64", builtin_dyn_as_f64),
        ("as_bool", builtin_dyn_as_bool),
        ("as_str", builtin_dyn_as_str),
        ("as_bytes", builtin_dyn_as_bytes),
    ];
    for (short, call) in qualified {
        let joined: &'static str = Box::leak(format!("DynValue::{short}").into_boxed_str());
        globals.push((joined, crate::builtins::builtin_pub(joined, *call)));
    }
    let bare: &[(&str, BuiltinFnPub)] = &[
        ("kind", builtin_dyn_kind),
        ("name", builtin_dyn_name),
        ("at", builtin_dyn_at),
        ("key_at", builtin_dyn_key_at),
        ("as_char", builtin_dyn_as_char),
    ];
    install_module_pub("DynValue", bare, globals);
}
