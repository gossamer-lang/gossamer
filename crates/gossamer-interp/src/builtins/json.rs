fn builtin_json_parse(args: &[Value]) -> RuntimeResult<Value> {
    let Some(source) = args.first().and_then(as_str) else {
        return Ok(err_variant("json::parse: argument must be a string"));
    };
    // `json::__parse_with(text, policy)` is `json::parse(text, duplicate_keys:)`.
    let duplicates = match args.get(1) {
        Some(Value::Int(n)) => json_std::DuplicateKeys::from_index(*n),
        _ => json_std::DuplicateKeys::Error,
    };
    // A document reads into its tape, as native code reads it. Every read of
    // a tape takes the last of equal keys, so one that repeats a key and keeps
    // the first, or is too long for a tape, is read as a tree.
    match json_std::parse_to_tape(source.as_bytes(), duplicates) {
        Ok((text, Some(tape)))
            if !(duplicates == json_std::DuplicateKeys::UseFirst
                && json_std::has_duplicate_key(text, Some(&tape))) =>
        {
            let doc = JsonInner::parsed(json_std::ParsedTape::new(text, tape));
            let root = json_child_to_lazy_value(&doc, doc.get());
            Ok(ok_variant(root))
        }
        Ok(_) => match json_std::parse_with(source, duplicates) {
            Ok(value) => Ok(ok_variant(json_root_to_lazy_value(value))),
            Err(e) => Ok(err_variant(format!("{e}"))),
        },
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

/// A parsed document as the VM carries it: a scalar as the plain value, the
/// way a scalar read out of a document travels, and anything else as a JSON
/// handle.
fn json_root_to_lazy_value(value: json_std::Value) -> Value {
    match value {
        json_std::Value::Bool(b) => Value::Bool(b),
        json_std::Value::Int(n) => Value::Int(n),
        json_std::Value::Uint(n) => Value::Uint(n),
        json_std::Value::Number(n) => Value::Float(n),
        json_std::Value::String(text) => Value::String(SmolStr::from(text)),
        json_std::Value::Null | json_std::Value::Array(_) | json_std::Value::Object(_) => {
            Value::Json(Arc::new(JsonInner::new(value)))
        }
    }
}

fn builtin_json_render(args: &[Value]) -> RuntimeResult<Value> {
    let Some(value) = args.first() else {
        return Ok(Value::String(SmolStr::from(String::from("null"))));
    };
    if let Value::Json(json_value) = value {
        return Ok(Value::String(SmolStr::from(json_std::encode(
            &json_value.as_value(),
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
        return Ok(value
            .get()
            .get(key)
            .map_or_else(none_variant, |child| some_variant(json_child_to_lazy_value(value, child))));
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

/// `json::pointer(value, pointer)`: the value an RFC 6901 JSON Pointer names.
pub(crate) fn builtin_json_pointer(args: &[Value]) -> RuntimeResult<Value> {
    let pointer = args.get(1).and_then(as_str).unwrap_or("");
    match args.first() {
        Some(Value::Json(value)) => {
            Ok(value.get().pointer(pointer).map_or_else(none_variant, |child| {
                some_variant(json_child_to_lazy_value(value, child))
            }))
        }
        // A scalar read out of a document: only the empty pointer names it.
        Some(other) if pointer.is_empty() => Ok(some_variant(other.clone())),
        _ => Ok(none_variant()),
    }
}



/// `json::keys(object)` → `Some([String])` of every key in sorted
/// order, or `None` when the receiver isn't an object. Tests
/// pattern-match the result so we always emit an Option variant.
fn builtin_json_keys(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        return Ok(value.get().entries().map_or_else(none_variant, |entries| {
            let keys = entries
                .into_iter()
                .map(|(name, _)| Value::String(SmolStr::from(&*name)))
                .collect();
            some_variant(Value::Array(Arc::new(keys)))
        }));
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
    // `v.len()` on a `json::Value` answers the `Option` `json::len` does.
    if let Some(Value::Json(_)) = args.first() {
        return builtin_json_len_opt(args);
    }
    let n: i64 = match args.first() {
        Some(Value::Array(a)) => a.len() as i64,
        Some(Value::Struct(s)) => s.fields.len() as i64,
        Some(Value::String(s)) => s.byte_len() as i64,
        _ => 0,
    };
    Ok(Value::Int(n))
}

/// `json::at(value, index)` → `Option<json::Value>`: the element, or `None`
/// past the end and for a value that is not an array.
pub(crate) fn builtin_json_at_opt(args: &[Value]) -> RuntimeResult<Value> {
    let index = match args.get(1) {
        Some(Value::Int(n)) => usize::try_from(*n).ok(),
        Some(Value::Uint(n)) => usize::try_from(*n).ok(),
        _ => None,
    };
    if let (Some(Value::Json(value)), Some(index)) = (args.first(), index)
        && let Some(child) = value.get().at(index)
    {
        return Ok(some_variant(json_child_to_lazy_value(value, child)));
    }
    Ok(none_variant())
}

/// `json::len(value)` → `Option<i64>`: the count of an array or object, and
/// `None` for any other value, as `json::keys` answers.
pub(crate) fn builtin_json_len_opt(args: &[Value]) -> RuntimeResult<Value> {
    let count = match args.first() {
        Some(Value::Json(value)) => {
            let node = value.get();
            (node.is_array() || node.is_object()).then(|| node.len())
        }
        _ => None,
    };
    Ok(count.map_or_else(none_variant, |n| {
        some_variant(Value::Int(i64::try_from(n).unwrap_or(i64::MAX)))
    }))
}

/// `json::is_null(value)` → `true` when the value is the `null` shape.
fn builtin_json_is_null(args: &[Value]) -> RuntimeResult<Value> {
    let is_null = matches!(args.first(), Some(Value::Unit | Value::Void) | None)
        || matches!(args.first(), Some(Value::Json(value)) if value.get().is_null());
    Ok(Value::Bool(is_null))
}

/// `json::as_str(value)` → `Option<String>`.
fn builtin_json_as_str(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        if let Some(text) = value.get().string() {
            return Ok(some_variant(Value::String(SmolStr::from(&*text))));
        }
    }
    if let Some(Value::String(s)) = args.first() {
        return Ok(some_variant(Value::String(s.clone())));
    }
    Ok(none_variant())
}

/// `json::set_max_size(bytes)`.
pub(crate) fn builtin_json_set_max_size(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Int(n)) = args.first() {
        json_std::set_max_size(gossamer_core_cap(*n));
    }
    Ok(Value::Unit)
}

/// `json::set_max_depth(levels)`.
pub(crate) fn builtin_json_set_max_depth(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Int(n)) = args.first() {
        json_std::set_max_depth(gossamer_core_cap(*n));
    }
    Ok(Value::Unit)
}

fn gossamer_core_cap(n: i64) -> usize {
    json_std::cap_from_program(n)
}

/// `json::as_i64(value)` → `Option<i64>`.
fn builtin_json_as_i64(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        if let Some(n) = value.get().number().as_ref().and_then(json_std::as_i64) {
            return Ok(some_variant(Value::Int(n)));
        }
    }
    if let Some(Value::Int(n)) = args.first() {
        return Ok(some_variant(Value::Int(*n)));
    }
    if let Some(Value::Uint(n)) = args.first() {
        return Ok(i64::try_from(*n).map_or_else(|_| none_variant(), |n| some_variant(Value::Int(n))));
    }
    if let Some(Value::Float(f)) = args.first() {
        if let Some(n) = json_std::integral_i64(*f) {
            return Ok(some_variant(Value::Int(n)));
        }
    }
    Ok(none_variant())
}

/// `json::as_u64(value)` → `Option<u64>`: the integer when it is non-negative
/// and fits a `u64`.
fn builtin_json_as_u64(args: &[Value]) -> RuntimeResult<Value> {
    let n = match args.first() {
        Some(Value::Json(value)) => value.get().number().as_ref().and_then(json_std::as_u64),
        Some(Value::Uint(n)) => Some(*n),
        Some(Value::Int(n)) => u64::try_from(*n).ok(),
        Some(Value::Float(f)) => json_std::as_u64(&json_std::Value::Number(*f)),
        _ => None,
    };
    Ok(n.map_or_else(none_variant, |n| some_variant(Value::Uint(n))))
}

/// `json::as_f64(value)` → `Option<f64>`.
fn builtin_json_as_f64(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        if let Some(n) = value.get().number().as_ref().and_then(json_std::as_f64) {
            return Ok(some_variant(Value::Float(n)));
        }
    }
    if let Some(Value::Float(f)) = args.first() {
        return Ok(some_variant(Value::Float(*f)));
    }
    if let Some(Value::Int(n)) = args.first() {
        return Ok(some_variant(Value::Float(*n as f64)));
    }
    if let Some(Value::Uint(n)) = args.first() {
        return Ok(some_variant(Value::Float(*n as f64)));
    }
    Ok(none_variant())
}

/// `json::as_bool(value)` → `Option<bool>`.
fn builtin_json_as_bool(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Json(value)) = args.first() {
        if let Some(b) = value.get().as_bool() {
            return Ok(some_variant(Value::Bool(b)));
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
        if let Some(items) = value.get().items() {
            return Ok(some_variant(Value::Array(Arc::new(
                items
                    .into_iter()
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


/// A node a read of `parent` answered, as the VM carries it: a scalar as the
/// plain value, and `null`, an array, or an object as a handle on the same
/// document.
fn json_child_to_lazy_value(parent: &JsonInner, child: json_std::ValueRef<'_>) -> Value {
    if let Some(b) = child.as_bool() {
        return Value::Bool(b);
    }
    if let Some(text) = child.string() {
        return Value::String(SmolStr::from(&*text));
    }
    match child.number() {
        Some(json_std::Value::Int(n)) => Value::Int(n),
        Some(json_std::Value::Uint(n)) => Value::Uint(n),
        Some(json_std::Value::Number(n)) => Value::Float(n),
        _ => Value::Json(Arc::new(parent.child(child))),
    }
}

#[allow(clippy::too_many_lines, reason = "one match whose length is the value set")]
pub(crate) fn gossamer_to_json_value(value: &Value) -> json_std::Value {
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
        Value::CapturePlace(place) => gossamer_to_json_value(&place.read()),
        Value::Unit | Value::Void | Value::Weak(_) => json_std::Value::Null,
        Value::Bool(b) => json_std::Value::Bool(*b),
        Value::Int(n) => json_std::Value::Int(*n),
        // A 128-bit integer past 64 bits has no JSON number every reader
        // holds exactly, so it is written as its decimal digits in a string.
        Value::Wide(wide) => json_std::Value::String(wide.to_string()),
        Value::WideInline(..) => json_std::Value::String(
            value.wide_int().map(|w| w.to_string()).unwrap_or_default(),
        ),
        // JSON has no NaN or infinity, so such a float is `null`, as the
        // compiled tiers build it.
        Value::Float(f) if !f.is_finite() => json_std::Value::Null,
        Value::Float(f) => json_std::Value::Number(*f),
        // An `f32` encodes as its single-precision digits, as the compiled
        // tiers write it.
        Value::Struct(_) if let Some(f) = crate::value::f32_render_slot(value) => {
            if f.is_finite() {
                json_std::Value::Number(gossamer_runtime::builtins::f32_as_decimal_double(f))
            } else {
                json_std::Value::Null
            }
        }
        Value::Char(c) => json_std::Value::String(c.to_string()),
        Value::String(s) => json_std::Value::String(s.as_str().to_string()),
        Value::Tuple(parts) => {
            json_std::Value::Array(parts.iter().map(gossamer_to_json_value).collect())
        }
        Value::Array(parts) => {
            json_std::Value::Array(parts.iter().map(gossamer_to_json_value).collect())
        }
        // A `Vec` carries its spelling on a rendering copy; JSON has one
        // array form, so the elements are what encode.
        Value::Struct(inner) if crate::value::vec_render_items(inner).is_some() => {
            crate::value::vec_render_items(inner).map_or(json_std::Value::Null, gossamer_to_json_value)
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
                // The absent value is what JSON spells `null`; every other
                // payload-less variant is named by its own spelling.
                if name.as_str() == "None" {
                    json_std::Value::Null
                } else {
                    json_std::Value::String(name.to_string())
                }
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
        | Value::Opaque(_)
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
        Value::Uint(n) => json_std::from_u64(*n),
    }
}
