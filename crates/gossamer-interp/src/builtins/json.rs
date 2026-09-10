fn builtin_json_parse(args: &[Value]) -> RuntimeResult<Value> {
    let Some(source) = args.first().and_then(as_str) else {
        return Ok(err_variant("json::parse: argument must be a string"));
    };
    match json_std::parse(source) {
        Ok(value) => Ok(ok_variant(Value::Json(Arc::new(JsonInner::new(value))))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

fn builtin_json_render(args: &[Value]) -> RuntimeResult<Value> {
    let Some(value) = args.first() else {
        return Ok(Value::String(SmolStr::from(String::from("null"))));
    };
    if let Value::Json(json_value) = value {
        return Ok(Value::String(SmolStr::from(json_std::encode(
            json_value.as_value(),
        ))));
    }
    let json_value = gossamer_to_json_value(value);
    Ok(Value::String(SmolStr::from(json_std::encode(&json_value))))
}

/// `json::get(value, key)` → object lookup wrapped in `Option`.
/// Returns `Some(child)` when `value` is an object and `key` is
/// present, otherwise `None`. Tests pattern-match the result, so
/// we must always return a real `Option` variant (never the bare
/// child or `Value::Unit`).
fn builtin_json_get(args: &[Value]) -> RuntimeResult<Value> {
    let Some(receiver) = args.first() else {
        return Ok(none_variant());
    };
    let Some(key) = args.get(1).and_then(as_str) else {
        return Ok(none_variant());
    };
    if let Value::Json(value) = receiver {
        if let json_std::Value::Object(entries) = value.as_value() {
            if let Some(child) = entries.get(key) {
                return Ok(some_variant(json_child_to_lazy_value(value, child)));
            }
        }
        return Ok(none_variant());
    }
    if let Value::Struct(inner) = receiver {
        for (field_name, value) in &inner.fields {
            if (*field_name) == key {
                return Ok(some_variant(value.clone()));
            }
        }
    }
    Ok(none_variant())
}

/// A JSON `null`, which is what a document states and what an element
/// the document does not have reads as.
fn json_null() -> Value {
    Value::Json(Arc::new(JsonInner::new(json_std::Value::Null)))
}

/// `json::at(array, idx)` → array index. Answers JSON `null` when the
/// receiver isn't an array or the index is out of bounds, which is what
/// the compiled tiers answer and what `json::is_null` reports on.
fn builtin_json_at(args: &[Value]) -> RuntimeResult<Value> {
    let Some(receiver) = args.first() else {
        return Ok(json_null());
    };
    let idx = args.get(1).and_then(|v| match v {
        Value::Int(n) => Some(*n),
        Value::Uint(n) => Some(*n as i64),
        _ => None,
    });
    let Some(idx) = idx else {
        return Ok(json_null());
    };
    if idx < 0 {
        return Ok(json_null());
    }
    if let Value::Json(value) = receiver {
        if let json_std::Value::Array(items) = value.as_value() {
            if let Some(child) = items.get(idx as usize) {
                return Ok(json_child_to_lazy_value(value, child));
            }
        }
        return Ok(json_null());
    }
    if let Value::Array(arr) = receiver {
        if let Some(v) = arr.get(idx as usize) {
            return Ok(v.clone());
        }
    }
    Ok(json_null())
}

/// `json::keys(object)` → `Some([String])` of every key in sorted
/// order, or `None` when the receiver isn't an object. Tests
/// pattern-match the result so we always emit an Option variant.
fn builtin_json_keys(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        if let json_std::Value::Object(entries) = value.as_value() {
            let keys = entries
                .keys()
                .map(|name| Value::String(SmolStr::from(name.as_str())))
                .collect();
            return Ok(some_variant(Value::Array(Arc::new(keys))));
        }
        return Ok(none_variant());
    }
    if let Some(Value::Struct(inner)) = args.first() {
        let mut out: Vec<Value> = Vec::new();
        for (name, _) in &inner.fields {
            out.push(Value::String(SmolStr::from(*name)));
        }
        return Ok(some_variant(Value::Array(Arc::new(out))));
    }
    Ok(none_variant())
}

/// `json::len(value)` → element / pair / byte count, 0 for scalar.
fn builtin_json_len(args: &[Value]) -> RuntimeResult<Value> {
    let n: i64 = match args.first() {
        Some(Value::Json(value)) => match value.as_value() {
            json_std::Value::Array(items) => items.len() as i64,
            json_std::Value::Object(entries) => entries.len() as i64,
            json_std::Value::String(text) => text.len() as i64,
            _ => 0,
        },
        Some(Value::Array(a)) => a.len() as i64,
        Some(Value::Struct(s)) => s.fields.len() as i64,
        Some(Value::String(s)) => s.len() as i64,
        _ => 0,
    };
    Ok(Value::Int(n))
}

/// `json::is_null(value)` → `true` when the value is the `null` shape.
fn builtin_json_is_null(args: &[Value]) -> RuntimeResult<Value> {
    let is_null = matches!(args.first(), Some(Value::Unit | Value::Void) | None)
        || matches!(args.first(), Some(Value::Json(value)) if matches!(value.as_value(), json_std::Value::Null));
    Ok(Value::Bool(is_null))
}

/// `json::as_str(value)` → `Option<String>`.
fn builtin_json_as_str(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        if let json_std::Value::String(text) = value.as_value() {
            return Ok(some_variant(Value::String(SmolStr::from(text.as_str()))));
        }
    }
    if let Some(Value::String(s)) = args.first() {
        return Ok(some_variant(Value::String(s.clone())));
    }
    Ok(none_variant())
}

/// `json::as_i64(value)` → `Option<i64>`.
fn builtin_json_as_i64(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        if let Some(n) = json_std::as_i64(value.as_value()) {
            return Ok(some_variant(Value::Int(n)));
        }
    }
    if let Some(Value::Int(n)) = args.first() {
        return Ok(some_variant(Value::Int(*n)));
    }
    if let Some(Value::Float(f)) = args.first() {
        if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
            return Ok(some_variant(Value::Int(*f as i64)));
        }
    }
    Ok(none_variant())
}

/// `json::as_f64(value)` → `Option<f64>`.
fn builtin_json_as_f64(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        if let Some(n) = json_std::as_f64(value.as_value()) {
            return Ok(some_variant(Value::Float(n)));
        }
    }
    if let Some(Value::Float(f)) = args.first() {
        return Ok(some_variant(Value::Float(*f)));
    }
    if let Some(Value::Int(n)) = args.first() {
        return Ok(some_variant(Value::Float(*n as f64)));
    }
    Ok(none_variant())
}

/// `json::as_bool(value)` → `Option<bool>`.
fn builtin_json_as_bool(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        if let json_std::Value::Bool(b) = value.as_value() {
            return Ok(some_variant(Value::Bool(*b)));
        }
    }
    if let Some(Value::Bool(b)) = args.first() {
        return Ok(some_variant(Value::Bool(*b)));
    }
    Ok(none_variant())
}

/// `json::as_array(value)` → `Some([T])` when the receiver is an
/// array, otherwise `None`. Tests pattern-match the result, so we
/// always emit an Option variant - `unwrap()` on a non-array now
/// fails loudly instead of returning a misleading empty array.
fn builtin_json_as_array(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        if let json_std::Value::Array(items) = value.as_value() {
            return Ok(some_variant(Value::Array(Arc::new(
                items
                    .iter()
                    .map(|child| json_child_to_lazy_value(value, child))
                    .collect(),
            ))));
        }
    }
    if let Some(Value::Array(a)) = args.first() {
        return Ok(some_variant(Value::Array(Arc::clone(a))));
    }
    Ok(none_variant())
}

fn builtin_json_decode(args: &[Value]) -> RuntimeResult<Value> {
    let Some(text) = args.first().and_then(as_str) else {
        return Ok(err_variant("json::decode: expected string argument"));
    };
    match json_std::decode(text) {
        Ok(value) => Ok(ok_variant(json_value_to_gossamer(&value))),
        Err(err) => Ok(err_variant(err.to_string())),
    }
}

/// Ordered field schema attached to a user struct. Built at
/// `Vm::load` time from the typechecker's `struct_field_tys` map and
/// the HIR's declaration-order field name list. Drives strict
/// `<Type>::from_json(text)` deserialization.
#[derive(Debug, Clone)]
pub(crate) struct JsonStructSchema {
    /// Source-order `(field_name, expected_kind)` pairs.
    pub fields: Vec<(String, JsonSchemaKind)>,
}

/// Expected shape for one JSON-decoded field. Mirrors the subset of
/// `TyKind` the JSON decoder validates against - primitives, the
/// growable / fixed sequence shapes, tuples, and nested named ADTs
/// resolved by struct name.
#[derive(Debug, Clone)]
pub(crate) enum JsonSchemaKind {
    /// `i8`..`u128` / `isize` / `usize`.
    Int,
    /// `f32` / `f64`.
    Float,
    /// `bool`.
    Bool,
    /// `String` (and `char`, encoded as one-rune string).
    String,
    /// `Vec<T>` or `[T]` (growable / slice).
    Vec(Box<JsonSchemaKind>),
    /// `[T; N]` fixed-size array.
    Array(Box<JsonSchemaKind>, usize),
    /// `(A, B, ...)`. Matched as JSON array of the same arity.
    Tuple(Vec<JsonSchemaKind>),
    /// Nested user struct referenced by source name.
    Struct(String),
    /// `Option<T>`. `null` decodes to `None`; any other JSON value
    /// runs through the inner kind and wraps in `Some`.
    Option(Box<JsonSchemaKind>),
    /// `HashMap<String, V>` - JSON object with arbitrary string keys.
    Map(Box<JsonSchemaKind>),
    /// `json::Value` - accept any well-formed JSON value untouched.
    Json,
    /// Unknown / unsupported leaf. The decoder accepts whatever the
    /// parser produced and does not validate further.
    Any,
}

#[allow(
    clippy::missing_const_for_thread_local,
    reason = "HashMap::new with default RandomState is not const on MSRV"
)]
mod json_schema_registry {
    use super::{JsonStructSchema, RefCell};

    thread_local! {
        pub(crate) static STRUCT_SCHEMAS: RefCell<std::collections::HashMap<String, JsonStructSchema>> =
            RefCell::new(std::collections::HashMap::new());
    }
}

pub(crate) use json_schema_registry::STRUCT_SCHEMAS;

/// Installs the struct schema table consulted by
/// `<Type>::from_json(text)` deserialization. Invoked once per
/// `Vm::load`. Replaces any prior table so tests that load multiple
/// programs see only the current program's structs.
#[allow(
    clippy::implicit_hasher,
    reason = "stored verbatim in a RandomState-typed thread-local; generic hasher would force the thread-local to be generic too"
)]
pub(crate) fn set_json_struct_schemas(
    schemas: std::collections::HashMap<String, JsonStructSchema>,
) {
    STRUCT_SCHEMAS.with(|cell| *cell.borrow_mut() = schemas);
}

/// Returns `true` when `type_name` has a registered JSON schema -
/// i.e. it is a user struct in the currently-loaded program. Used
/// by the interpreter to decide whether to intercept
/// `<Type>::from_json(text)` / `<Type>::to_json(value)` calls.
#[must_use]
pub(crate) fn has_json_schema(type_name: &str) -> bool {
    STRUCT_SCHEMAS.with(|cell| cell.borrow().contains_key(type_name))
}

/// `<Type>::to_json(value)` - render `value` as a JSON string,
/// returning `Result<String, errors::Error>`. Pairs with
/// `<Type>::from_json`; the receiver may be either an already-typed
/// `Value::Struct` or any other shape `json::render` can flatten.
pub(crate) fn json_to_string_for_type(_type_name: &str, value: &Value) -> Value {
    let rendered = gossamer_to_json_value(value);
    ok_variant(Value::String(SmolStr::from(json_std::encode(&rendered))))
}

/// `<Type>::from_json(text)` - parse and validate strictly against
/// the registered schema for `type_name`. Returns the same shape as
/// `json::decode` (`Result<T, errors::Error>`) so `?` propagates.
pub(crate) fn json_from_str_for_type(type_name: &str, text: &str) -> Value {
    let parsed = match json_std::parse(text) {
        Ok(v) => v,
        Err(e) => return err_variant(format!("{type_name}::from_json: {e}")),
    };
    match coerce_json_to_named_struct(&parsed, type_name) {
        Ok(value) => ok_variant(value),
        Err(msg) => err_variant(format!("{type_name}::from_json: {msg}")),
    }
}

fn coerce_json_to_named_struct(value: &json_std::Value, type_name: &str) -> Result<Value, String> {
    let schema = STRUCT_SCHEMAS.with(|cell| cell.borrow().get(type_name).cloned());
    let Some(schema) = schema else {
        return Err(format!("unknown struct `{type_name}`"));
    };
    let json_std::Value::Object(map) = value else {
        return Err(format!("expected JSON object for `{type_name}`"));
    };
    let mut fields: Vec<(&'static str, Value)> = Vec::with_capacity(schema.fields.len());
    for (field_name, kind) in &schema.fields {
        let Some(child) = map.get(field_name) else {
            return Err(format!("missing field `{field_name}`"));
        };
        let coerced =
            coerce_json_to_kind(child, kind).map_err(|m| format!("field `{field_name}`: {m}"))?;
        fields.push((crate::value::intern_type_name(field_name.as_str()), coerced));
    }
    Ok(Value::struct_(
        type_name,
        Arc::unwrap_or_clone(Arc::new(fields)),
    ))
}

fn coerce_json_to_kind(value: &json_std::Value, kind: &JsonSchemaKind) -> Result<Value, String> {
    use JsonSchemaKind as K;
    match (value, kind) {
        (_, K::Any | K::Json) => Ok(json_value_to_gossamer(value)),
        (json_std::Value::Null, K::Option(_)) => Ok(none_variant()),
        (other, K::Option(inner)) => coerce_json_to_kind(other, inner).map(some_variant),
        (json_std::Value::Number(n), K::Int) => {
            if !n.is_finite() || n.fract() != 0.0 {
                return Err(format!("expected integer, got {n}"));
            }
            Ok(Value::Int(*n as i64))
        }
        (json_std::Value::Number(n), K::Float) => Ok(Value::Float(*n)),
        (json_std::Value::Bool(b), K::Bool) => Ok(Value::Bool(*b)),
        (json_std::Value::String(s), K::String) => Ok(Value::String(SmolStr::from(s.clone()))),
        (json_std::Value::Array(items), K::Vec(elem)) => {
            let mut out: Vec<Value> = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                out.push(coerce_json_to_kind(item, elem).map_err(|m| format!("[{i}]: {m}"))?);
            }
            Ok(Value::Array(Arc::new(out)))
        }
        (json_std::Value::Array(items), K::Array(elem, expected_len)) => {
            if items.len() != *expected_len {
                return Err(format!(
                    "expected array of length {expected_len}, got {}",
                    items.len()
                ));
            }
            let mut out: Vec<Value> = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                out.push(coerce_json_to_kind(item, elem).map_err(|m| format!("[{i}]: {m}"))?);
            }
            Ok(Value::Array(Arc::new(out)))
        }
        (json_std::Value::Array(items), K::Tuple(elems)) => {
            if items.len() != elems.len() {
                return Err(format!(
                    "expected tuple of arity {}, got {}",
                    elems.len(),
                    items.len()
                ));
            }
            let mut out: Vec<Value> = Vec::with_capacity(items.len());
            for (i, (item, elem_kind)) in items.iter().zip(elems.iter()).enumerate() {
                out.push(coerce_json_to_kind(item, elem_kind).map_err(|m| format!(".{i}: {m}"))?);
            }
            Ok(Value::Tuple(Arc::from(out)))
        }
        (json_std::Value::Object(_), K::Struct(name)) => coerce_json_to_named_struct(value, name),
        (json_std::Value::Object(map), K::Map(value_kind)) => {
            let mut storage = dense_map_with_capacity(map.len());
            for (k, v) in map {
                let coerced =
                    coerce_json_to_kind(v, value_kind).map_err(|m| format!("[{k:?}]: {m}"))?;
                storage.insert(MapKey::Str(SmolStr::from(k.clone())), coerced);
            }
            Ok(Value::Map(Arc::new(parking_lot::Mutex::new(storage))))
        }
        (got, want) => Err(format!(
            "expected {}, got {}",
            describe_kind(want),
            describe_json_value(got)
        )),
    }
}

fn describe_kind(kind: &JsonSchemaKind) -> String {
    use JsonSchemaKind as K;
    match kind {
        K::Int => "integer".to_string(),
        K::Float => "number".to_string(),
        K::Bool => "bool".to_string(),
        K::String => "string".to_string(),
        K::Vec(inner) => format!("array of {}", describe_kind(inner)),
        K::Array(inner, n) => format!("array of {} length {n}", describe_kind(inner)),
        K::Tuple(elems) => format!("tuple of arity {}", elems.len()),
        K::Struct(name) => name.clone(),
        K::Option(inner) => format!("optional {}", describe_kind(inner)),
        K::Map(inner) => format!("map of {}", describe_kind(inner)),
        K::Json => "any json value".to_string(),
        K::Any => "any value".to_string(),
    }
}

fn describe_json_value(value: &json_std::Value) -> &'static str {
    match value {
        json_std::Value::Null => "null",
        json_std::Value::Bool(_) => "bool",
        json_std::Value::Int(_) | json_std::Value::Number(_) => "number",
        json_std::Value::String(_) => "string",
        json_std::Value::Array(_) => "array",
        json_std::Value::Object(_) => "object",
    }
}

fn json_value_to_gossamer(value: &json_std::Value) -> Value {
    match value {
        json_std::Value::Null => Value::Unit,
        json_std::Value::Bool(b) => Value::Bool(*b),
        // `Int` and `Number` are kept distinct so an integer round-trips
        // as an integer and a float (including an integer-valued one like
        // `2.0`) round-trips as a float, matching the serde-backed
        // compiled tier.
        json_std::Value::Int(n) => Value::Int(*n),
        json_std::Value::Number(n) => Value::Float(*n),
        json_std::Value::String(s) => Value::String(SmolStr::from(s.clone())),
        json_std::Value::Array(items) => {
            Value::Array(Arc::new(items.iter().map(json_value_to_gossamer).collect()))
        }
        json_std::Value::Object(entries) => {
            let fields: Vec<(&'static str, Value)> = entries
                .iter()
                .map(|(k, v)| (crate::value::intern_type_name(k), json_value_to_gossamer(v)))
                .collect();
            Value::struct_("Object", Arc::unwrap_or_clone(Arc::new(fields)))
        }
    }
}

/// Exposes JSON scalars as their natural interpreter values and keeps nested
/// documents canonical. This is the lazy boundary for JSON query operations:
/// direct parse-render never crosses it, while callers that inspect a child
/// retain the pre-existing scalar behavior without rebuilding unrelated
/// branches of the document.
fn json_value_to_lazy_value(value: &json_std::Value) -> Value {
    match value {
        json_std::Value::Null => json_null(),
        json_std::Value::Bool(b) => Value::Bool(*b),
        json_std::Value::Int(n) => Value::Int(*n),
        json_std::Value::Number(n) => Value::Float(*n),
        json_std::Value::String(text) => Value::String(SmolStr::from(text.as_str())),
        json_std::Value::Array(_) | json_std::Value::Object(_) => {
            Value::Json(Arc::new(JsonInner::new(value.clone())))
        }
    }
}

fn json_child_to_lazy_value(parent: &JsonInner, child: &json_std::Value) -> Value {
    match child {
        json_std::Value::Null => Value::Json(Arc::new(parent.child(child))),
        json_std::Value::Bool(b) => Value::Bool(*b),
        json_std::Value::Int(n) => Value::Int(*n),
        json_std::Value::Number(n) => Value::Float(*n),
        json_std::Value::String(text) => Value::String(SmolStr::from(text.as_str())),
        json_std::Value::Array(_) | json_std::Value::Object(_) => {
            Value::Json(Arc::new(parent.child(child)))
        }
    }
}

#[allow(clippy::too_many_lines, reason = "one match whose length is the value set")]
fn gossamer_to_json_value(value: &Value) -> json_std::Value {
    match value {
        Value::Json(value) => value.to_owned_value(),
        Value::NativeEnum(o) => gossamer_to_json_value(&crate::value::native_enum_to_variant(o)),
        Value::MutCell(c) => {
            let inner = c.lock().clone();
            gossamer_to_json_value(&inner)
        }
        Value::CaptureCell(c) => {
            let inner = c.lock().clone();
            gossamer_to_json_value(&inner)
        }
        Value::Unit | Value::Void | Value::Weak(_) => json_std::Value::Null,
        Value::Bool(b) => json_std::Value::Bool(*b),
        Value::Int(n) => json_std::Value::Int(*n),
        Value::Float(f) => json_std::Value::Number(*f),
        Value::Char(c) => json_std::Value::String(c.to_string()),
        Value::String(s) => json_std::Value::String(s.as_str().to_string()),
        Value::Tuple(parts) => {
            json_std::Value::Array(parts.iter().map(gossamer_to_json_value).collect())
        }
        Value::Array(parts) => {
            json_std::Value::Array(parts.iter().map(gossamer_to_json_value).collect())
        }
        Value::Struct(inner) => {
            let mut map = std::collections::BTreeMap::new();
            for (ident, v) in &inner.fields {
                map.insert((*ident).to_string(), gossamer_to_json_value(v));
            }
            json_std::Value::Object(map)
        }
        Value::Variant(inner) => {
            let name = inner.name.clone();
            let fields = &inner.fields;
            if fields.is_empty() {
                json_std::Value::String(name.to_string())
            } else if fields.len() == 1 {
                gossamer_to_json_value(&fields[0])
            } else {
                json_std::Value::Array(fields.iter().map(gossamer_to_json_value).collect())
            }
        }
        Value::Closure(_)
        | Value::Builtin(_)
        | Value::Native(_)
        | Value::Channel(_)
        | Value::LazyIter(_) => json_std::Value::Null,
        Value::Map(map) => {
            let mut out = std::collections::BTreeMap::new();
            for (k, v) in map.lock().iter() {
                let key_string = match k.to_value() {
                    Value::String(s) => s.as_str().to_string(),
                    other => other.to_string(),
                };
                out.insert(key_string, gossamer_to_json_value(v));
            }
            json_std::Value::Object(out)
        }
        Value::FloatArray { .. } => {
            let fallback = value.float_array_to_value_array();
            gossamer_to_json_value(&fallback)
        }
        Value::IntArray(data) => {
            let arr: Vec<json_std::Value> =
                data.iter().copied().map(json_std::Value::Int).collect();
            json_std::Value::Array(arr)
        }
        Value::ByteArray(data) => {
            let arr = data
                .iter()
                .map(|value| json_std::Value::Int(i64::from(*value)))
                .collect();
            json_std::Value::Array(arr)
        }
        Value::InlineByteArray(data) => {
            let arr = data
                .iter()
                .map(|value| json_std::Value::Int(i64::from(*value)))
                .collect();
            json_std::Value::Array(arr)
        }
        Value::ByteVec(data) => {
            let arr = data
                .iter()
                .map(|value| json_std::Value::Int(i64::from(*value)))
                .collect();
            json_std::Value::Array(arr)
        }
        Value::FloatVec(data) => {
            let arr: Vec<json_std::Value> =
                data.iter().copied().map(json_std::Value::Number).collect();
            json_std::Value::Array(arr)
        }
        Value::IntMap(map) => {
            let mut out = std::collections::BTreeMap::new();
            for (k, v) in map.lock().iter() {
                out.insert(k.to_string(), json_std::Value::Int(*v));
            }
            json_std::Value::Object(out)
        }
        Value::StrIntMap(map) => {
            let mut out = std::collections::BTreeMap::new();
            for (k, v) in map.lock().iter() {
                out.insert(k.as_str().to_string(), json_std::Value::Int(*v));
            }
            json_std::Value::Object(out)
        }
        // An unsigned word past `i64::MAX` has no exact integer rendering,
        // so only a value the signed form holds keeps its digits.
        Value::Uint(n) => i64::try_from(*n)
            .map_or_else(|_| json_std::Value::Number(*n as f64), json_std::Value::Int),
    }
}
