fn builtin_len(args: &[Value]) -> RuntimeResult<Value> {
    let count = match args.first() {
        // String indexing and length use Unicode scalar positions. Explicit
        // byte access remains available through `as_bytes`, `bytes`, and
        // `byte_at`.
        Some(Value::String(s)) => s.len(),
        Some(Value::Array(parts)) => parts.len(),
        Some(Value::Tuple(parts)) => parts.len(),
        Some(Value::IntArray(data)) => data.len(),
        Some(Value::ByteArray(data)) => data.len(),
        Some(Value::InlineByteArray(data)) => data.len(),
        Some(Value::ByteVec(data)) => data.len(),
        Some(Value::FloatVec(data)) => data.len(),
        Some(Value::Map(m)) => m.lock().len(),
        Some(Value::IntMap(m)) => m.lock().len(),
        Some(Value::StrIntMap(m)) => m.lock().len(),
        // A named arm counts the payload it carries, which is what
        // `DynValue::len` reads. Every other enum reaches `len` only through
        // that type, so no user enum gains a length by it.
        Some(Value::Variant(v)) => v.fields.len(),
        _ => return Ok(Value::Int(0)),
    };
    Ok(Value::Int(i64::try_from(count).unwrap_or(i64::MAX)))
}

/// `x.is_empty()` for a `String` / `Vec` / `[T]` / tuple receiver - the
/// bare-name method that mirrors the compiled tier's `gos_rt_len_is_zero`.
/// Maps dispatch through their qualified `HashMap::is_empty`, but this also
/// handles them so a bare-name fallthrough stays correct.
fn builtin_is_empty(args: &[Value]) -> RuntimeResult<Value> {
    let empty = match args.first() {
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(parts)) => parts.is_empty(),
        Some(Value::Tuple(parts)) => parts.is_empty(),
        Some(Value::IntArray(data)) => data.is_empty(),
        Some(Value::ByteArray(data)) => data.is_empty(),
        Some(Value::InlineByteArray(data)) => data.is_empty(),
        Some(Value::ByteVec(data)) => data.is_empty(),
        Some(Value::FloatVec(data)) => data.is_empty(),
        Some(Value::Map(m)) => m.lock().is_empty(),
        Some(Value::IntMap(m)) => m.lock().is_empty(),
        Some(Value::StrIntMap(m)) => m.lock().is_empty(),
        _ => true,
    };
    Ok(Value::Bool(empty))
}

fn builtin_to_string(args: &[Value]) -> RuntimeResult<Value> {
    let rendered: String = match args.first() {
        Some(Value::String(s)) => s.as_str().to_string(),
        Some(other) => format!("{other}"),
        None => String::new(),
    };
    Ok(Value::String(rendered.into()))
}

/// `s.split(delim)` → `[String]`. Matches Rust's `str::split` when
/// `delim` is a single character or a literal substring. Returns the
/// original string as a one-element array on an empty or non-string
/// receiver so downstream `.len()` / indexing stays well-defined.
fn builtin_split(args: &[Value]) -> RuntimeResult<Value> {
    let receiver: String = match args.first() {
        Some(Value::String(s)) => s.as_str().to_string(),
        _ => return Ok(Value::empty_array()),
    };
    let delim: String = match args.get(1) {
        Some(Value::String(s)) => s.as_str().to_string(),
        Some(Value::Char(c)) => c.to_string(),
        _ => return Ok(Value::Array(Arc::new(vec![Value::String(receiver.into())]))),
    };
    let parts: Vec<Value> = if delim.is_empty() {
        receiver
            .chars()
            .map(|c| Value::String(SmolStr::from(c.to_string())))
            .collect()
    } else {
        receiver
            .split(&delim)
            .map(|p| Value::String(SmolStr::from(p.to_string())))
            .collect()
    };
    Ok(Value::Array(Arc::new(parts)))
}

/// `s.trim()` → `String`. Strips ASCII / Unicode whitespace from
/// both ends - matches Rust's `str::trim`.
fn builtin_trim(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::String(s)) = args.first() else {
        return Ok(Value::String(SmolStr::from(String::new())));
    };
    Ok(Value::String(SmolStr::from(s.trim().to_string())))
}

/// `s.as_bytes()` -> `[i64]`. Returns the UTF-8 bytes of `s` as an
/// integer array so callers can iterate or index without a manual
/// `for i in 0..s.len()` loop. On a non-string receiver, falls
/// through to an empty array.
fn builtin_as_bytes(args: &[Value]) -> RuntimeResult<Value> {
    // A value that already holds bytes answers itself - the shape a
    // `DynValue` of bytes carries.
    if let Some(
        value @ (Value::IntArray(_)
        | Value::ByteArray(_)
        | Value::InlineByteArray(_)
        | Value::ByteVec(_)),
    ) = args.first()
    {
        return Ok(value.clone());
    }
    let Some(Value::String(s)) = args.first() else {
        return Ok(Value::empty_array());
    };
    let parts: Vec<Value> = s
        .as_bytes()
        .iter()
        .map(|b| Value::Int(i64::from(*b)))
        .collect();
    Ok(Value::Array(Arc::new(parts)))
}

fn builtin_to_uppercase(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::String(s)) = args.first() else {
        return Ok(Value::String(SmolStr::from(String::new())));
    };
    Ok(Value::String(SmolStr::to_uppercase_from(s.as_str())))
}

fn builtin_to_lowercase(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::String(s)) = args.first() else {
        return Ok(Value::String(SmolStr::from(String::new())));
    };
    Ok(Value::String(SmolStr::from(s.to_lowercase())))
}

/// `String::new()` - a fresh empty owned String.
fn builtin_str_new(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(SmolStr::from(String::new())))
}

/// `String::with_capacity(n)` reserves mutable VM string storage.
fn builtin_str_with_capacity(args: &[Value]) -> RuntimeResult<Value> {
    let capacity = match args.first().and_then(value_to_int) {
        Some(n) if n < 0 => {
            return Err(RuntimeError::Type(
                "String::with_capacity: capacity must be non-negative".to_string(),
            ))
        }
        Some(n) => usize::try_from(n).map_err(|_| {
            RuntimeError::Type("String::with_capacity: capacity is too large".to_string())
        })?,
        None => 0,
    };
    Ok(Value::String(SmolStr::with_capacity(capacity)))
}

/// `String::from(s)` is identity for a string argument - gos `String` is
/// already the owned representation. Mirrors the compiled tier's identity
/// passthrough; matches Rust's `String::from(&str)`.
fn builtin_str_from(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::String(s)) => Ok(Value::String(s.clone())),
        Some(Value::Char(c)) => Ok(Value::String(SmolStr::from(c.to_string()))),
        Some(other) => Ok(Value::String(SmolStr::from(format!("{other}")))),
        None => Ok(Value::String(SmolStr::from(String::new()))),
    }
}

/// `String::from_utf8(bytes)` decodes a byte vector and returns
/// `Result<String, errors::Error>`.
fn builtin_str_from_utf8(args: &[Value]) -> RuntimeResult<Value> {
    let Some(bytes_value) = args.first() else {
        return Ok(err_variant("String::from_utf8: missing byte vector"));
    };
    let Some(items) = array_as_values(bytes_value) else {
        return Ok(err_variant("String::from_utf8: expected byte vector"));
    };
    let mut bytes = Vec::with_capacity(items.len());
    for item in &items {
        let Some(n) = value_to_int(item) else {
            return Ok(err_variant("String::from_utf8: expected integer byte"));
        };
        bytes.push((n & 0xff) as u8);
    }
    match String::from_utf8(bytes) {
        Ok(s) => Ok(ok_variant(Value::String(SmolStr::from(s)))),
        Err(e) => Ok(err_variant(format!("String::from_utf8: {e}"))),
    }
}

/// `s.push(ch)` - append a char, returning the new String. Returning
/// the new value (not Unit) keeps the VM's mutating-method writeback
/// idempotent: the writeback move assigns the appended String back into
/// the receiver binding, so `let mut s = …; s.push('x')` leaves `s` a
/// String rather than clobbering it with `()`.
fn builtin_str_push(args: &[Value]) -> RuntimeResult<Value> {
    let mut out = args
        .first()
        .and_then(|value| match value {
            Value::String(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    match args.get(1) {
        Some(Value::Char(c)) => out.push(*c),
        other => {
            if let Some(c) = other
                .and_then(value_to_int)
                .and_then(|n| char::from_u32(n as u32))
            {
                out.push(c);
            }
        }
    }
    Ok(Value::String(out))
}

/// `s.push_char(c)` - append a char, returning the new String.
/// Identical write-back contract as `builtin_str_push`.
fn builtin_str_push_char(args: &[Value]) -> RuntimeResult<Value> {
    let mut out = args
        .first()
        .and_then(|value| match value {
            Value::String(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    match args.get(1) {
        Some(Value::Char(c)) => out.push(*c),
        other => {
            if let Some(c) = other
                .and_then(value_to_int)
                .and_then(|n| char::from_u32(n as u32))
            {
                out.push(c);
            }
        }
    }
    Ok(Value::String(out))
}

/// `s.push_byte(b)` - append a byte (0-255) as its Unicode codepoint,
/// returning the new String. Write-back contract matches `builtin_str_push`.
fn builtin_str_push_byte(args: &[Value]) -> RuntimeResult<Value> {
    let byte = args.get(1).and_then(value_to_int).unwrap_or(0) as u8;
    let ch = char::from(byte);
    let mut out = args
        .first()
        .and_then(|value| match value {
            Value::String(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    out.push(ch);
    Ok(Value::String(out))
}

/// `s.push_utf8(buf, start, end)` - append the `[start, end)` byte window of
/// `buf` when it is valid UTF-8, answering whether it was appended.
///
/// The receiver crosses as a write-back cell because the call's value is the
/// flag rather than the new string; an out-of-range or non-UTF-8 window leaves
/// the receiver untouched.
fn builtin_str_push_utf8(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::MutCell(cell)) = args.first() else {
        return Ok(Value::Bool(false));
    };
    // The window is read through `byte_window`, which answers whichever
    // representation the VM chose for the buffer, so a window is appended the
    // same way whether the caller built the buffer by literal, by push, or out
    // of a file - and costs the window rather than the buffer, which is what a
    // caller appending one field of a resident file pays per call.
    let start = args.get(2).and_then(value_to_int).unwrap_or(-1);
    let end = args.get(3).and_then(value_to_int).unwrap_or(-1);
    if start < 0 || end < start {
        return Ok(Value::Bool(false));
    }
    let Some(bytes) = args
        .get(1)
        .and_then(|buf| buf.byte_window(start as usize, end as usize))
    else {
        return Ok(Value::Bool(false));
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(Value::Bool(false));
    };
    let mut guard = cell.lock();
    let mut out = match &*guard {
        Value::String(existing) => existing.clone(),
        _ => return Ok(Value::Bool(false)),
    };
    out.push_str(text);
    *guard = Value::String(out);
    Ok(Value::Bool(true))
}

/// `s.push_json_quoted(buf, start, end)` - append the `[start, end)` byte
/// window of `buf` as a quoted, escaped JSON string when it is valid UTF-8,
/// answering whether it was appended. The receiver crosses as a write-back
/// cell, as for `push_utf8`.
fn builtin_str_push_json_quoted(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::MutCell(cell)) = args.first() else {
        return Ok(Value::Bool(false));
    };
    let start = args.get(2).and_then(value_to_int).unwrap_or(-1);
    let end = args.get(3).and_then(value_to_int).unwrap_or(-1);
    if start < 0 || end < start {
        return Ok(Value::Bool(false));
    }
    let Some(bytes) = args
        .get(1)
        .and_then(|buf| buf.byte_window(start as usize, end as usize))
    else {
        return Ok(Value::Bool(false));
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(Value::Bool(false));
    };
    let mut guard = cell.lock();
    let mut out = match &*guard {
        Value::String(existing) => existing.clone(),
        _ => return Ok(Value::Bool(false)),
    };
    let mut quoted = Vec::with_capacity(text.len() + 2);
    quoted.push(b'"');
    gossamer_runtime::c_abi::json_escape_into(text.as_bytes(), &mut quoted);
    quoted.push(b'"');
    // Escaping keeps the text UTF-8: it only replaces ASCII bytes.
    out.push_str(&String::from_utf8_lossy(&quoted));
    *guard = Value::String(out);
    Ok(Value::Bool(true))
}

/// `s.push_str(t)` - append a string slice, returning the new String.
/// See `builtin_str_push` for the writeback contract.
fn builtin_str_push_str(args: &[Value]) -> RuntimeResult<Value> {
    let suffix = args.get(1).and_then(as_str).unwrap_or("");
    let mut out = args
        .first()
        .and_then(|value| match value {
            Value::String(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    out.push_str(suffix);
    Ok(Value::String(out))
}

/// `s.chars()` - the Unicode scalars of `s` as a `Vec<char>`, so
/// `for ch in s.chars()` binds each `Value::Char` and the elements
/// work with `out.push(ch)`. Mirrors the compiled `gos_rt_str_chars`.
fn builtin_str_chars(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    let items: Vec<Value> = s.chars().map(Value::Char).collect();
    Ok(Value::Array(Arc::new(items)))
}

pub(crate) fn builtin_contains(args: &[Value]) -> RuntimeResult<Value> {
    // `HashMap`/`BTreeMap`::contains(k) is the `contains_key` alias the
    // compiled tier exposes; route map receivers to the key-membership
    // builtin so a `Value::Map`/`Value::IntMap` does not fall through to
    // the always-false tail.
    if matches!(
        args.first(),
        Some(Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_))
    ) {
        return builtin_map_contains_key(args);
    }
    // String::contains(substr) when both args are strings; otherwise
    // Vec::contains(&v) - element membership by value equality. The
    // compiled tier exposes both shapes under the same `contains`
    // method, so the interp must too.
    if let Some(Value::String(s)) = args.first() {
        match args.get(1) {
            Some(Value::String(needle)) => return Ok(Value::Bool(s.contains(needle.as_str()))),
            // `s.contains('e')` - a char needle is the single-codepoint
            // substring, matching the compiled tiers' char→String coercion.
            Some(Value::Char(c)) => return Ok(Value::Bool(s.as_str().contains(*c))),
            _ => {}
        }
    }
    if let (Some(recv), Some(needle)) = (args.first(), args.get(1)) {
        if let Some(items) = array_as_values(recv) {
            return Ok(Value::Bool(
                items.iter().any(|e| values_equal_for_assertion(e, needle)),
            ));
        }
    }
    Ok(Value::Bool(false))
}

/// Normalises any array-shaped `Value` (boxed `Array`, flat `IntArray`
/// / `FloatVec`, or all-f64 `FloatArray`) into a `Vec<Value>` for the
/// read-only collection helpers below.
pub(crate) fn array_as_values(recv: &Value) -> Option<Vec<Value>> {
    match recv {
        Value::Array(items) | Value::Tuple(items) => Some(items.as_ref().clone()),
        Value::IntArray(items) => Some(items.iter().map(|n| Value::Int(*n)).collect()),
        Value::ByteArray(items) => Some(
            items
                .iter()
                .map(|n| Value::Int(i64::from(*n)))
                .collect(),
        ),
        Value::InlineByteArray(items) => Some(
            items
                .iter()
                .map(|n| Value::Int(i64::from(*n)))
                .collect(),
        ),
        Value::ByteVec(items) => Some(
            items
                .iter()
                .map(|n| Value::Int(i64::from(*n)))
                .collect(),
        ),
        Value::FloatVec(items) => Some(items.iter().map(|x| Value::Float(*x)).collect()),
        rx @ Value::FloatArray(_) => match rx.float_array_to_value_array() {
            Value::Array(items) => Some(items.as_ref().clone()),
            _ => None,
        },
        _ => None,
    }
}

fn builtin_first(args: &[Value]) -> RuntimeResult<Value> {
    match args.first().and_then(array_as_values) {
        Some(items) if !items.is_empty() => Ok(Value::variant("Some", vec![items[0].clone()])),
        _ => Ok(Value::variant("None", vec![])),
    }
}

fn builtin_last(args: &[Value]) -> RuntimeResult<Value> {
    match args.first().and_then(array_as_values) {
        Some(items) if !items.is_empty() => {
            Ok(Value::variant("Some", vec![items[items.len() - 1].clone()]))
        }
        _ => Ok(Value::variant("None", vec![])),
    }
}

fn builtin_get(args: &[Value]) -> RuntimeResult<Value> {
    let (Some(recv), Some(idx)) = (args.first(), args.get(1)) else {
        return Ok(Value::variant("None", vec![]));
    };
    let Some(idx) = value_to_int(idx) else {
        return Ok(Value::variant("None", vec![]));
    };
    if idx < 0 {
        return Ok(Value::variant("None", vec![]));
    }
    match array_as_values(recv).and_then(|items| items.get(idx as usize).cloned()) {
        Some(value) => Ok(Value::variant("Some", vec![value])),
        None => Ok(Value::variant("None", vec![])),
    }
}

fn builtin_reversed(args: &[Value]) -> RuntimeResult<Value> {
    match args.first().and_then(array_as_values) {
        Some(mut items) => {
            items.reverse();
            Ok(Value::Array(Arc::new(items)))
        }
        None => Ok(args.first().cloned().unwrap_or(Value::Unit)),
    }
}

fn builtin_index_of(args: &[Value]) -> RuntimeResult<Value> {
    let (Some(recv), Some(needle)) = (args.first(), args.get(1)) else {
        return Ok(Value::variant("None", vec![]));
    };
    if let Some(items) = array_as_values(recv) {
        if let Some(idx) = items
            .iter()
            .position(|e| values_equal_for_assertion(e, needle))
        {
            return Ok(Value::variant("Some", vec![Value::Int(idx as i64)]));
        }
    }
    Ok(Value::variant("None", vec![]))
}

fn builtin_count_of(args: &[Value]) -> RuntimeResult<Value> {
    let (Some(recv), Some(needle)) = (args.first(), args.get(1)) else {
        return Ok(Value::Int(0));
    };
    if let Some(items) = array_as_values(recv) {
        let n = items
            .iter()
            .filter(|e| values_equal_for_assertion(e, needle))
            .count();
        return Ok(Value::Int(n as i64));
    }
    Ok(Value::Int(0))
}

/// Legacy internal bounds-checked insert helper.
fn builtin_vec_insert_safe(args: &[Value]) -> RuntimeResult<Value> {
    let idx = args.get(1).and_then(value_to_int).unwrap_or(0);
    let Some(mut items) = args.first().and_then(array_as_values) else {
        return Ok(slice_err("insert expects a Vec receiver".to_string()));
    };
    let len = items.len() as i64;
    if idx < 0 || idx > len {
        return Ok(slice_err(format!(
            "insert: index {idx} out of bounds for length {len}"
        )));
    }
    items.insert(idx as usize, args.get(2).cloned().unwrap_or(Value::Unit));
    Ok(ok_variant(Value::Array(Arc::new(items))))
}

/// Legacy internal bounds-checked remove helper.
fn builtin_vec_remove_safe(args: &[Value]) -> RuntimeResult<Value> {
    let idx = args.get(1).and_then(value_to_int).unwrap_or(0);
    let Some(mut items) = args.first().and_then(array_as_values) else {
        return Ok(slice_err("remove expects a Vec receiver".to_string()));
    };
    let len = items.len() as i64;
    if idx < 0 || idx >= len {
        return Ok(slice_err(format!(
            "remove: index {idx} out of bounds for length {len}"
        )));
    }
    Ok(ok_variant(items.remove(idx as usize)))
}

/// `Map::pop(m, k) -> Option<V>` - remove and return the previous
/// value Python-style.
fn builtin_map_pop(args: &[Value]) -> RuntimeResult<Value> {
    let Some(key_val) = args.get(1) else {
        return Ok(none_variant());
    };
    match args.first() {
        Some(Value::Map(m)) => {
            let key = MapKey::from_value(key_val);
            match m.lock().remove(&key) {
                Some(v) => Ok(some_variant(v)),
                None => Ok(none_variant()),
            }
        }
        Some(Value::IntMap(m)) => {
            let key = value_to_int(key_val).unwrap_or(0);
            match m.lock().swap_remove(&key) {
                Some(v) => Ok(some_variant(Value::Int(v))),
                None => Ok(none_variant()),
            }
        }
        Some(Value::StrIntMap(m)) => {
            let Value::String(k) = key_val else {
                return Ok(none_variant());
            };
            match m.lock().swap_remove(k) {
                Some(v) => Ok(some_variant(Value::Int(v))),
                None => Ok(none_variant()),
            }
        }
        _ => Ok(none_variant()),
    }
}

fn builtin_starts_with(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::String(s)) = args.first() else {
        return Ok(Value::Bool(false));
    };
    let Some(Value::String(prefix)) = args.get(1) else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(s.starts_with(prefix.as_str())))
}

fn builtin_ends_with(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::String(s)) = args.first() else {
        return Ok(Value::Bool(false));
    };
    let Some(Value::String(suffix)) = args.get(1) else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(s.ends_with(suffix.as_str())))
}

/// `String::slice(s, a, b) -> Result<String, errors::Error>` - the
/// non-panicking character-range slice contract: `a > b` or `b > len`
/// returns Err.
fn builtin_str_slice(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::String(s)) = args.first() else {
        return Ok(slice_err(
            "String::slice expects a String receiver".to_string(),
        ));
    };
    let start = args.get(1).and_then(value_to_int).unwrap_or(0);
    let end = args.get(2).and_then(value_to_int).unwrap_or(0);
    let len = s.len() as i64;
    // Match the compiled `gos_rt_str_slice` bounds policy + message
    // verbatim so `gos` and `gos build` agree byte-for-byte.
    if start < 0 || end < 0 || start > end || end > len {
        return Ok(slice_err(format!(
            "slice: range [{start}, {end}) out of bounds for length {len}"
        )));
    }
    Ok(ok_variant(Value::String(str_substring_inline(
        s, start, end,
    ))))
}

/// Dual entry for bare `slice` reaching method dispatch when the
/// receiver type isn't yet known. Routes by receiver kind to the
/// String or Vec slice helper. Accepts every flat-storage Vec
/// variant the interp uses (`Array`, `IntArray`, `FloatArray`).
pub(crate) fn builtin_str_or_vec_slice(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::String(_)) => builtin_str_slice(args),
        Some(
            Value::Array(_)
            | Value::IntArray(_)
            | Value::ByteArray(_)
            | Value::InlineByteArray(_)
            | Value::ByteVec(_)
            | Value::FloatArray(_)
            | Value::FloatVec(_),
        ) => {
            builtin_vec_slice(args)
        }
        _ => Ok(slice_err(
            "slice expects a String or Vec receiver".to_string(),
        )),
    }
}

pub(crate) fn slice_err(msg: String) -> Value {
    Value::variant(
        "Err",
        vec![errors_struct(msg, Value::variant("None", vec![]))],
    )
}

fn slice_bounds(args: &[Value], label: &str) -> Result<(usize, usize), Value> {
    let start = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => return Err(slice_err(format!("{label} expects an i64 start"))),
    };
    let end = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => return Err(slice_err(format!("{label} expects an i64 end"))),
    };
    if start < 0 || end < 0 {
        return Err(slice_err(format!("{label} bounds must be non-negative")));
    }
    if start > end {
        return Err(slice_err(format!("{label} start exceeds end")));
    }
    Ok((start as usize, end as usize))
}

/// `Vec::slice(xs, a, b) -> Result<Vec<T>, errors::Error>` -
/// non-panicking element-range slice; same bounds policy as
/// [`builtin_str_slice`]. Handles each flat-storage Vec variant.
fn builtin_vec_slice(args: &[Value]) -> RuntimeResult<Value> {
    let (lo, hi) = match slice_bounds(args, "Vec::slice") {
        Ok(bounds) => bounds,
        Err(err) => return Ok(err),
    };
    // Length of the receiver, for the bounds message.
    let len = match args.first() {
        Some(Value::Array(arr)) => arr.len() as i64,
        Some(Value::IntArray(arr)) => arr.len() as i64,
        Some(Value::ByteArray(arr)) => arr.len() as i64,
        Some(Value::InlineByteArray(arr)) => arr.len() as i64,
        Some(Value::ByteVec(arr)) => arr.len() as i64,
        Some(Value::FloatVec(arr)) => arr.len() as i64,
        Some(rx @ Value::FloatArray(_)) => match rx.float_array_to_value_array() {
            Value::Array(v) => v.len() as i64,
            _ => 0,
        },
        _ => return Ok(slice_err("Vec::slice expects a Vec receiver".to_string())),
    };
    // Mirror the compiled `gos_rt_vec_slice` bounds policy + message.
    if hi > len as usize {
        return Ok(slice_err(format!(
            "slice: range [{lo}, {hi}) out of bounds for length {len}"
        )));
    }
    match args.first() {
        Some(Value::Array(arr)) => Ok(ok_variant(Value::Array(Arc::new(arr[lo..hi].to_vec())))),
        Some(Value::IntArray(arr)) => {
            Ok(ok_variant(Value::IntArray(Arc::new(arr[lo..hi].to_vec()))))
        }
        Some(Value::ByteArray(arr)) => {
            Ok(ok_variant(Value::ByteArray(Arc::new(
                crate::value::PackedBytes::from(arr[lo..hi].to_vec()),
            ))))
        }
        Some(Value::InlineByteArray(arr)) if lo == 0 && hi == arr.len() => {
            Ok(ok_variant(Value::InlineByteArray(Arc::clone(arr))))
        }
        Some(Value::InlineByteArray(arr)) => {
            Ok(ok_variant(Value::ByteArray(Arc::new(
                crate::value::PackedBytes::from(arr[lo..hi].to_vec()),
            ))))
        }
        Some(Value::ByteVec(arr)) => {
            Ok(ok_variant(Value::ByteArray(Arc::new(
                crate::value::PackedBytes::from(arr[lo..hi].to_vec()),
            ))))
        }
        Some(Value::FloatVec(arr)) => {
            Ok(ok_variant(Value::FloatVec(Arc::new(arr[lo..hi].to_vec()))))
        }
        Some(rx @ Value::FloatArray(_)) => {
            let Value::Array(view) = rx.float_array_to_value_array() else {
                return Ok(slice_err("Vec::slice cannot view FloatArray".to_string()));
            };
            Ok(ok_variant(Value::Array(Arc::new(view[lo..hi].to_vec()))))
        }
        _ => Ok(slice_err("Vec::slice expects a Vec receiver".to_string())),
    }
}

/// Clamping byte-range substring shared by the `substring` builtin and the
/// VM's fused `Op::StrSubstring`. Out-of-range bounds clamp and inverted
/// bounds yield "", mirroring the compiled tier's `gos_rt_str_substring`.
/// Bounds inside a multibyte scalar advance to the next UTF-8 boundary.
/// Builds the `SmolStr` directly from the validated slice: substrings within
/// the inline capacity carry no heap allocation, so materialising an
/// intermediate owned `String` first would add a redundant heap alloc + free
/// per call - the dominant cost when slicing many short substrings.
pub(crate) fn str_substring_inline(s: &SmolStr, start: i64, end: i64) -> SmolStr {
    let byte_len = s.byte_len();
    let lo = next_char_boundary(s.as_str(), (start.max(0) as usize).min(byte_len));
    let hi = next_char_boundary(s.as_str(), (end.max(0) as usize).min(byte_len)).max(lo);
    SmolStr::from_str(&s.as_str()[lo..hi])
}

fn next_char_boundary(s: &str, mut index: usize) -> usize {
    while index < s.len() && !s.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn builtin_str_substring(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::String(s)) = args.first() else {
        return Ok(Value::String(SmolStr::new()));
    };
    let start = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let end = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => s.byte_len() as i64,
    };
    Ok(Value::String(str_substring_inline(s, start, end)))
}

fn builtin_str_byte_len(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::String(s)) = args.first() else {
        return Ok(Value::Int(0));
    };
    Ok(Value::Int(i64::try_from(s.byte_len()).unwrap_or(i64::MAX)))
}

/// `String::byte_at(s, i) -> i64` - the UTF-8 byte at index `i`, or 0
/// when `i` is negative or at/past the end. Mirrors the compiled-tier
/// `gos_rt_str_byte_at` (which reads the null terminator as 0).
fn builtin_str_byte_at(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::String(s)) = args.first() else {
        return Ok(Value::Int(0));
    };
    let i = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => return Ok(Value::Int(0)),
    };
    let bytes = s.as_str().as_bytes();
    let byte = if i < 0 || (i as usize) >= bytes.len() {
        0
    } else {
        bytes[i as usize]
    };
    Ok(Value::Int(i64::from(byte)))
}

fn builtin_str_replace(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::String(s)) = args.first() else {
        return Ok(Value::String(SmolStr::from(String::new())));
    };
    let from = match args.get(1) {
        Some(Value::String(f)) => f.as_str(),
        _ => return Ok(Value::String(s.clone())),
    };
    let to = match args.get(2) {
        Some(Value::String(t)) => t.as_str(),
        _ => "",
    };
    Ok(Value::String(SmolStr::from(s.replace(from, to))))
}

fn builtin_str_find(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::String(s)) = args.first() else {
        return Ok(Value::Int(-1));
    };
    let needle = match args.get(1) {
        Some(Value::String(n)) => n.as_str(),
        _ => return Ok(Value::Int(-1)),
    };
    match s.find(needle) {
        Some(idx) => Ok(Value::Int(
            i64::try_from(s[..idx].chars().count()).unwrap_or(-1),
        )),
        None => Ok(Value::Int(-1)),
    }
}

fn builtin_push(args: &[Value]) -> RuntimeResult<Value> {
    let extra = args.get(1);
    match args.first() {
        Some(Value::Array(parts)) => {
            // First scalar push into an empty generic array switches it to
            // flat typed storage (`IntArray` / `FloatVec`, 8 bytes per
            // element instead of a 16-byte boxed `Value`) - the same
            // routing as `Op::VecPush`.
            match extra {
                Some(Value::Int(n)) if parts.is_empty() => {
                    let mut data = Vec::with_capacity(parts.capacity().max(1));
                    data.push(*n);
                    return Ok(Value::IntArray(Arc::new(data)));
                }
                Some(Value::Float(f)) if parts.is_empty() => {
                    let mut data = Vec::with_capacity(parts.capacity().max(1));
                    data.push(*f);
                    return Ok(Value::FloatVec(Arc::new(data)));
                }
                _ => {}
            }
            let mut owned = crate::value::copy_with_capacity(parts);
            if let Some(extra) = extra {
                owned.push(extra.clone());
            }
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(parts)) => {
            // A float push means the receiver is an `[f64]` whose elements
            // so far were integer-valued: widen to flat float storage.
            if let Some(Value::Float(f)) = extra {
                let mut wide = Vec::with_capacity(parts.capacity().max(parts.len() + 1));
                wide.extend(parts.iter().map(|n| *n as f64));
                wide.push(*f);
                return Ok(Value::FloatVec(Arc::new(wide)));
            }
            let mut owned = crate::value::copy_with_capacity(parts);
            if let Some(Value::Int(n)) = extra {
                owned.push(*n);
            }
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::ByteArray(parts)) => {
            let mut owned = parts.to_vec();
            if let Some(Value::Int(n)) = extra {
                owned.push(*n as u8);
            }
            Ok(Value::ByteVec(Arc::new(owned)))
        }
        Some(Value::InlineByteArray(parts)) => {
            let mut owned = parts.to_vec();
            if let Some(Value::Int(n)) = extra {
                owned.push(*n as u8);
            }
            Ok(Value::ByteVec(Arc::new(owned)))
        }
        Some(Value::ByteVec(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            if let Some(Value::Int(n)) = extra {
                owned.push(*n as u8);
            }
            Ok(Value::ByteVec(Arc::new(owned)))
        }
        Some(Value::FloatVec(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            if let Some(Value::Float(f)) = args.get(1) {
                owned.push(*f);
            }
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        _ => Ok(Value::Unit),
    }
}

fn builtin_pop(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Array(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.pop();
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(data)) => {
            // Typed-storage path: `let mut xs: [i64] = [..]` lands
            // here as `Value::IntArray`. Without this arm, the
            // generic-Array branch above missed the type and
            // returned `Value::empty_array()`, which the bytecode
            // VM's writeback then moved into `xs` - clobbering
            // every element instead of shortening by one.
            let mut owned = crate::value::copy_with_capacity(data);
            owned.pop();
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::FloatVec(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.pop();
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        _ => Ok(Value::empty_array()),
    }
}

fn builtin_map_new(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Map(Arc::new(parking_lot::Mutex::new(crate::vm_map::VmMap::with_capacity(16)))))
}

fn builtin_map_from(args: &[Value]) -> RuntimeResult<Value> {
    map_from(args, crate::vm_map::VmMap::with_capacity(16))
}

/// `Map::from` / `BTreeMap::from`: `empty` filled from the array of pairs.
fn map_from(args: &[Value], empty: crate::vm_map::VmMap) -> RuntimeResult<Value> {
    let map = Arc::new(parking_lot::Mutex::new(empty));
    if let Some(source) = args.first() {
        let Some(entries) = array_as_values(source) else {
            return Err(RuntimeError::Type(
                "HashMap::from expects a fixed array of key-value tuples".to_string(),
            ));
        };
        let mut output = map.lock();
        for entry in entries {
            let Some(parts) = array_as_values(&entry) else {
                return Err(RuntimeError::Type(
                    "HashMap::from expects key-value tuple entries".to_string(),
                ));
            };
            let [key, value] = parts.as_slice() else {
                return Err(RuntimeError::Type(
                    "HashMap::from expects key-value tuple entries".to_string(),
                ));
            };
            output.insert(MapKey::from_value(key), value.clone());
        }
    } else {
        return Err(RuntimeError::Type(
            "HashMap::from expects a fixed array of key-value tuples".to_string(),
        ));
    }
    Ok(Value::Map(map))
}

/// `HashMap::with_capacity(cap)`: pre-sizes generic map storage so the
/// doubling chain doesn't fire on a hot insert loop. The VM cannot infer the
/// key/value specialization from this constructor call, so it must preserve
/// the same general `Map` representation as `HashMap::new`; returning an
/// `IntMap` here silently made every `HashMap<String, i64>` lookup miss.
fn builtin_map_with_capacity(args: &[Value]) -> RuntimeResult<Value> {
    let cap = match arg_int(args, 0) {
        Some(n) => crate::value::map_capacity(n)?,
        None => 0,
    };
    Ok(Value::Map(Arc::new(parking_lot::Mutex::new(crate::vm_map::VmMap::with_capacity(cap)))))
}

fn builtin_map_get(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Map(map)) => {
            let Some(v) = args.get(1) else {
                return Ok(none_variant());
            };
            let key = MapKey::from_value(v);
            match map.lock().get(&key) {
                Some(v) => Ok(some_variant(v.clone())),
                None => Ok(none_variant()),
            }
        }
        Some(Value::IntMap(map)) => {
            let Some(Value::Int(k)) = args.get(1) else {
                return Ok(none_variant());
            };
            match map.lock().get(k).copied() {
                Some(v) => Ok(some_variant(Value::Int(v))),
                None => Ok(none_variant()),
            }
        }
        Some(Value::StrIntMap(map)) => {
            let Some(Value::String(k)) = args.get(1) else {
                return Ok(none_variant());
            };
            match map.lock().get(k).copied() {
                Some(v) => Ok(some_variant(Value::Int(v))),
                None => Ok(none_variant()),
            }
        }
        _ => Ok(none_variant()),
    }
}

fn builtin_map_get_or(args: &[Value]) -> RuntimeResult<Value> {
    let default = args.get(2).cloned().unwrap_or(Value::Unit);
    match args.first() {
        Some(Value::Map(map)) => {
            let Some(v) = args.get(1) else {
                return Ok(default);
            };
            let key = MapKey::from_value(v);
            match map.lock().get(&key).cloned() {
                Some(v) => Ok(v),
                None => Ok(default),
            }
        }
        Some(Value::IntMap(map)) => {
            let Some(Value::Int(k)) = args.get(1) else {
                return Ok(default);
            };
            let fallback = if let Value::Int(d) = &default { *d } else { 0 };
            Ok(Value::Int(map.lock().get(k).copied().unwrap_or(fallback)))
        }
        Some(Value::StrIntMap(map)) => {
            let Some(Value::String(k)) = args.get(1) else {
                return Ok(default);
            };
            let fallback = if let Value::Int(d) = &default { *d } else { 0 };
            Ok(Value::Int(map.lock().get(k).copied().unwrap_or(fallback)))
        }
        _ => Ok(default),
    }
}

/// `m.inc(k)` / `m.inc(k, by)` - counter-style increment for an
/// integer-valued `HashMap` or `IntMap`. Returns the post-increment
/// value. Equivalent to `*m.entry(k).or_insert(0) += by` in Rust.
fn builtin_map_inc(args: &[Value]) -> RuntimeResult<Value> {
    let by = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => 1,
    };
    match args.first() {
        Some(Value::Map(map)) => {
            let Some(key_value) = args.get(1) else {
                return Ok(Value::Int(0));
            };
            let key = MapKey::from_value(key_value);
            let mut guard = map.lock();
            let new_val = match guard.get(&key) {
                Some(Value::Int(v)) => v + by,
                _ => by,
            };
            guard.insert(key, Value::Int(new_val));
            Ok(Value::Int(new_val))
        }
        Some(Value::IntMap(map)) => {
            let Some(Value::Int(k)) = args.get(1) else {
                return Ok(Value::Int(0));
            };
            let mut guard = map.lock();
            let new_val = guard.get(k).copied().unwrap_or(0) + by;
            guard.insert(*k, new_val);
            Ok(Value::Int(new_val))
        }
        Some(Value::StrIntMap(map)) => {
            let Some(Value::String(k)) = args.get(1) else {
                return Ok(Value::Int(0));
            };
            let mut guard = map.lock();
            // Borrowed-slice probe first: a repeated key (the common
            // case when counting) updates in place with no key clone;
            // only a first insert pays the `SmolStr` clone.
            if let Some(slot) = guard.get_mut(k) {
                *slot += by;
                return Ok(Value::Int(*slot));
            }
            guard.insert(k.clone(), by);
            Ok(Value::Int(by))
        }
        _ => Ok(Value::Int(0)),
    }
}

/// `m.or_insert(k, default)` - returns the existing value for `k`,
/// inserting `default` first if missing. The Gossamer-shaped
/// equivalent of Rust's `entry().or_insert()`.
fn builtin_map_or_insert(args: &[Value]) -> RuntimeResult<Value> {
    let default = args.get(2).cloned().unwrap_or(Value::Unit);
    match args.first() {
        Some(Value::Map(map)) => {
            let Some(key_value) = args.get(1) else {
                return Ok(default);
            };
            let key = MapKey::from_value(key_value);
            let mut guard = map.lock();
            if let Some(existing) = guard.get(&key) {
                return Ok(existing.clone());
            }
            guard.insert(key, default.clone());
            Ok(default)
        }
        Some(Value::IntMap(map)) => {
            let Some(Value::Int(k)) = args.get(1) else {
                return Ok(default);
            };
            let fallback = if let Value::Int(d) = &default { *d } else { 0 };
            let mut guard = map.lock();
            if let Some(existing) = guard.get(k).copied() {
                return Ok(Value::Int(existing));
            }
            guard.insert(*k, fallback);
            Ok(Value::Int(fallback))
        }
        Some(Value::StrIntMap(map)) => {
            let Some(Value::String(k)) = args.get(1) else {
                return Ok(default);
            };
            let fallback = if let Value::Int(d) = &default { *d } else { 0 };
            let mut guard = map.lock();
            if let Some(existing) = guard.get(k).copied() {
                return Ok(Value::Int(existing));
            }
            guard.insert(k.clone(), fallback);
            Ok(Value::Int(fallback))
        }
        _ => Ok(default),
    }
}

/// `m.inc_at(seq, start, len, by)` for `HashMap<String, i64>`.
///
/// DEPRECATED. This fuses substring-extraction with the counter
/// increment into one call - a shape no other language expresses as a
/// single stdlib primitive, so it does not belong in head-to-head
/// benchmarks. Idiomatic code extracts the substring and calls
/// `m.inc(key, by)`; the general `HashMap<String, i64>` path is now
/// compact (`Value::StrIntMap`), so the fusion buys no allocation win.
/// Retained for source compatibility; handles both the boxed `Map` and
/// the typed `StrIntMap` storage so it never silently no-ops.
fn builtin_map_inc_at(args: &[Value]) -> RuntimeResult<Value> {
    let by = match args.get(4) {
        Some(Value::Int(n)) => *n,
        _ => 1,
    };
    let start = match args.get(2) {
        Some(Value::Int(n)) if *n < 0 => {
            return Err(RuntimeError::Type(
                "HashMap::inc_at: start must be non-negative".to_string(),
            ))
        }
        Some(Value::Int(n)) => usize::try_from(*n).unwrap_or(0),
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(n)) if *n < 0 => {
            return Err(RuntimeError::Type(
                "HashMap::inc_at: length must be non-negative".to_string(),
            ))
        }
        Some(Value::Int(n)) => usize::try_from(*n).unwrap_or(0),
        _ => 0,
    };
    if len == 0 {
        return Ok(Value::Int(0));
    }
    // Build the SmolStr directly from the &str slice - skips the
    // intermediate String allocation that the prior shape (`.to_string()`
    // then `SmolStr::from(String)`) paid per k-mer. For k <= 22 the
    // SmolStr is stored inline (no heap alloc), so every k-nucleotide
    // input shape (k = 1, 2, 3, 4, 6, 12, 18) lands in inline storage.
    let kmer = match args.get(1) {
        Some(Value::String(s)) => {
            let bytes = s.as_bytes();
            if start + len > bytes.len() {
                return Ok(Value::Int(0));
            }
            match std::str::from_utf8(&bytes[start..start + len]) {
                Ok(slice) => SmolStr::from_str(slice),
                Err(_) => return Ok(Value::Int(0)),
            }
        }
        _ => return Ok(Value::Int(0)),
    };
    match args.first() {
        Some(Value::Map(map)) => {
            let key = MapKey::Str(kmer);
            let mut guard = map.lock();
            let new_val = match guard.get(&key) {
                Some(Value::Int(v)) => v + by,
                _ => by,
            };
            guard.insert(key, Value::Int(new_val));
            Ok(Value::Int(new_val))
        }
        Some(Value::StrIntMap(map)) => {
            let mut guard = map.lock();
            let entry = guard.entry(kmer).or_insert(0);
            *entry += by;
            Ok(Value::Int(*entry))
        }
        _ => Ok(Value::Int(0)),
    }
}

fn builtin_map_insert(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Map(map)) => {
            let Some(v) = args.get(1) else {
                return Ok(none_variant());
            };
            let key = MapKey::from_value(v);
            let value = args.get(2).cloned().unwrap_or(Value::Unit);
            Ok(match map.lock().insert(key, value) {
                Some(previous) => some_variant(previous),
                None => none_variant(),
            })
        }
        Some(Value::IntMap(map)) => {
            let Some(Value::Int(k)) = args.get(1) else {
                return Ok(none_variant());
            };
            let v = match args.get(2) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            Ok(match map.lock().insert(*k, v) {
                Some(previous) => some_variant(Value::Int(previous)),
                None => none_variant(),
            })
        }
        Some(Value::StrIntMap(map)) => {
            let Some(Value::String(k)) = args.get(1) else {
                return Ok(none_variant());
            };
            let v = match args.get(2) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            Ok(match map.lock().insert(k.clone(), v) {
                Some(previous) => some_variant(Value::Int(previous)),
                None => none_variant(),
            })
        }
        _ => Ok(none_variant()),
    }
}

fn builtin_map_remove(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Map(map)) => {
            let Some(v) = args.get(1) else {
                return Ok(none_variant());
            };
            let key = MapKey::from_value(v);
            Ok(match map.lock().remove(&key) {
                Some(previous) => some_variant(previous),
                None => none_variant(),
            })
        }
        Some(Value::IntMap(map)) => {
            let Some(Value::Int(k)) = args.get(1) else {
                return Ok(none_variant());
            };
            Ok(match map.lock().swap_remove(k) {
                Some(previous) => some_variant(Value::Int(previous)),
                None => none_variant(),
            })
        }
        Some(Value::StrIntMap(map)) => {
            let Some(Value::String(k)) = args.get(1) else {
                return Ok(none_variant());
            };
            Ok(match map.lock().swap_remove(k) {
                Some(previous) => some_variant(Value::Int(previous)),
                None => none_variant(),
            })
        }
        _ => Ok(none_variant()),
    }
}

fn builtin_map_contains_key(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Map(map)) => {
            let Some(v) = args.get(1) else {
                return Ok(Value::Bool(false));
            };
            let key = MapKey::from_value(v);
            Ok(Value::Bool(map.lock().contains_key(&key)))
        }
        Some(Value::IntMap(map)) => {
            let Some(Value::Int(k)) = args.get(1) else {
                return Ok(Value::Bool(false));
            };
            Ok(Value::Bool(map.lock().contains_key(k)))
        }
        Some(Value::StrIntMap(map)) => {
            let Some(Value::String(k)) = args.get(1) else {
                return Ok(Value::Bool(false));
            };
            Ok(Value::Bool(map.lock().contains_key(k)))
        }
        _ => Ok(Value::Bool(false)),
    }
}

fn builtin_map_len(args: &[Value]) -> RuntimeResult<Value> {
    let n = match args.first() {
        Some(Value::Map(m)) => m.lock().len() as i64,
        Some(Value::IntMap(m)) => m.lock().len() as i64,
        Some(Value::StrIntMap(m)) => m.lock().len() as i64,
        _ => 0,
    };
    Ok(Value::Int(n))
}

fn builtin_map_keys(args: &[Value]) -> RuntimeResult<Value> {
    let mut out: Vec<Value> = Vec::new();
    match args.first() {
        Some(Value::Map(map)) => {
            // Sort by key for deterministic order that matches `iter()`
            // and the compiled tier's implementation-defined native map
            // order.
            out.extend(map.lock().sorted().into_iter().map(|(k, _)| k.to_value()));
        }
        Some(Value::IntMap(map)) => {
            let mut keys: Vec<i64> = map.lock().keys().copied().collect();
            keys.sort_unstable();
            out.extend(keys.into_iter().map(Value::Int));
        }
        Some(Value::StrIntMap(map)) => {
            let mut keys: Vec<SmolStr> = map.lock().keys().cloned().collect();
            keys.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            out.extend(keys.into_iter().map(Value::String));
        }
        _ => {}
    }
    Ok(Value::Array(Arc::new(out)))
}

/// Bare-name `keys` router. The bytecode VM dispatches `m.keys()` /
/// `obj.keys()` against the bare-name builtin (qualified keys like
/// `json::keys` only fire when callers spell them out). Without this
/// router, `install_module("json", …)` overrode the
/// `install_module("HashMap", …)` registration of `"keys"` with
/// `builtin_json_keys`, which returns `None` for `Map` / `IntMap`
/// receivers and silently produced an empty `Vec` for every
/// `HashMap.keys()` call. The router dispatches by receiver shape
/// so both surfaces work without a registration-order foot-gun.
fn builtin_keys_router(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_)) => builtin_map_keys(args),
        Some(Value::Struct(_)) => builtin_json_keys(args),
        _ => builtin_map_keys(args),
    }
}

/// Receiver-dispatching router for bare `get()`. `HashMap` /
/// `IntMap` receivers route to the map getter (Option-returning);
/// struct / json receivers keep the json field getter. Prevents the
/// `install_module("json", …)` bare-name push from shadowing the
/// `HashMap` getter - the bug that made `match m.get(&k) { Some(v) =>
/// … }` always take the `None` arm under native scrutinee
/// evaluation.
fn builtin_get_router(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_)) => builtin_map_get(args),
        Some(Value::Struct(_) | Value::Json(_)) => builtin_json_get(args),
        _ => builtin_get(args),
    }
}

/// Companion router for bare `values()` - same shape collision as
/// `keys()` above. Without this, future stdlib registrations could
/// re-introduce the silent override.
fn builtin_values_router(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_)) => builtin_map_values(args),
        _ => builtin_map_values(args),
    }
}

fn builtin_map_values(args: &[Value]) -> RuntimeResult<Value> {
    let mut out: Vec<Value> = Vec::new();
    match args.first() {
        Some(Value::Map(map)) => {
            // Emit values in key-sorted order so `keys()` / `values()` /
            // `iter()` agree on ordering and it is deterministic across
            // tiers.
            let entries: Vec<(MapKey, Value)> = map
                .lock()
                .sorted()
                .into_iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            out.extend(entries.into_iter().map(|(_, v)| v));
        }
        Some(Value::IntMap(map)) => {
            let mut entries: Vec<(i64, i64)> = map.lock().iter().map(|(k, v)| (*k, *v)).collect();
            entries.sort_by_key(|(k, _)| *k);
            out.extend(entries.into_iter().map(|(_, v)| Value::Int(v)));
        }
        Some(Value::StrIntMap(map)) => {
            let mut entries: Vec<(SmolStr, i64)> =
                map.lock().iter().map(|(k, v)| (k.clone(), *v)).collect();
            entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            out.extend(entries.into_iter().map(|(_, v)| Value::Int(v)));
        }
        _ => {}
    }
    Ok(Value::Array(Arc::new(out)))
}

/// `m.iter()` / `m.entries()` - yields a `[(K, V)]` array of tuples
/// suitable for direct destructuring in `for (k, v) in m.iter()`.
/// Snapshots the map under the lock so the caller's iteration is
/// safe even if other goroutines are mutating concurrently.
///
/// For non-map receivers (`Array`, `IntArray`, `FloatVec`, etc.)
/// returns the receiver unchanged so `arr.iter()` continues to work
/// as a no-op pass-through to the for-loop.
/// A map's `iter()` answers iterator state over its sorted entries, so an
/// adapter downstream runs its callback as each entry is pulled.
fn map_entries_cursor(entries: Vec<Value>) -> Value {
    crate::stdlib_builtins::iter::lazy_source(&Value::Array(Arc::new(entries)))
}

pub(crate) fn builtin_map_iter(args: &[Value]) -> RuntimeResult<Value> {
    // Sort by key on every call so `BTreeMap` users get deterministic
    // iteration order. The VM uses one runtime value shape for both
    // `HashMap` and `BTreeMap`, so sorting unifies observed order across
    // the two.
    match args.first() {
        Some(Value::Map(map)) => {
            let entries: Vec<(MapKey, Value)> = map
                .lock()
                .sorted()
                .into_iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let out: Vec<Value> = entries
                .into_iter()
                .map(|(k, v)| Value::Tuple(Arc::from(vec![k.to_value(), v])))
                .collect();
            Ok(map_entries_cursor(out))
        }
        Some(Value::IntMap(map)) => {
            let mut entries: Vec<(i64, i64)> = map.lock().iter().map(|(k, v)| (*k, *v)).collect();
            entries.sort_by_key(|(k, _)| *k);
            let out: Vec<Value> = entries
                .into_iter()
                .map(|(k, v)| Value::Tuple(Arc::from(vec![Value::Int(k), Value::Int(v)])))
                .collect();
            Ok(map_entries_cursor(out))
        }
        Some(Value::StrIntMap(map)) => {
            let mut entries: Vec<(SmolStr, i64)> =
                map.lock().iter().map(|(k, v)| (k.clone(), *v)).collect();
            entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            let out: Vec<Value> = entries
                .into_iter()
                .map(|(k, v)| Value::Tuple(Arc::from(vec![Value::String(k), Value::Int(v)])))
                .collect();
            Ok(map_entries_cursor(out))
        }
        // A sequence answers an iterator, so a combinator downstream of
        // `iter()` runs when the pipeline is drained rather than here.
        Some(other) => Ok(crate::stdlib_builtins::iter::lazy_source(other)),
        None => Ok(Value::Unit),
    }
}

/// `m.inc_batch(keys, by)` - typed batch counter increment for
/// `Value::IntMap`. Takes the map's mutex once and applies the
/// `+= by` to every i64 key in the input vec, amortising the
/// `parking_lot::Mutex` acquisition that `Op::IntMapInc` would
/// pay per call. Returns the map handle to mirror `insert`'s
/// shape.
///
/// Falls through to a no-op for non-IntMap receivers and for
/// keys-vec shapes the runtime can't index as `i64` (the audit
/// flagged the per-op lock cost as the gap; this is the
/// minimum-viable amortisation primitive).
fn builtin_map_inc_batch(args: &[Value]) -> RuntimeResult<Value> {
    let by = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => 1,
    };
    match args.first() {
        Some(Value::IntMap(map)) => {
            let mut locked = map.lock();
            match args.get(1) {
                Some(Value::IntArray(keys)) => {
                    for k in keys.iter() {
                        *locked.entry(*k).or_insert(0) += by;
                    }
                }
                Some(Value::Array(items)) => {
                    for v in items.iter() {
                        if let Value::Int(k) = v {
                            *locked.entry(*k).or_insert(0) += by;
                        }
                    }
                }
                _ => {}
            }
            drop(locked);
            Ok(Value::IntMap(Arc::clone(map)))
        }
        Some(Value::Map(map)) => {
            let mut locked = map.lock();
            if let Some(Value::Array(items)) = args.get(1) {
                for v in items.iter() {
                    let key = MapKey::from_value(v);
                    let entry = locked.get_or_insert_with(key, || Value::Int(0));
                    if let Value::Int(n) = entry {
                        *n += by;
                    }
                }
            }
            drop(locked);
            Ok(Value::Map(Arc::clone(map)))
        }
        _ => Ok(args.first().cloned().unwrap_or(Value::Unit)),
    }
}

fn builtin_map_clear(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Map(map)) => {
            map.lock().clear();
            Ok(Value::Map(Arc::clone(map)))
        }
        Some(Value::IntMap(map)) => {
            map.lock().clear();
            Ok(Value::IntMap(Arc::clone(map)))
        }
        Some(Value::StrIntMap(map)) => {
            map.lock().clear();
            Ok(Value::StrIntMap(Arc::clone(map)))
        }
        _ => Ok(args.first().cloned().unwrap_or(Value::Unit)),
    }
}

fn builtin_map_is_empty(args: &[Value]) -> RuntimeResult<Value> {
    let empty = match args.first() {
        Some(Value::Map(m)) => m.lock().is_empty(),
        Some(Value::IntMap(m)) => m.lock().is_empty(),
        Some(Value::StrIntMap(m)) => m.lock().is_empty(),
        _ => false,
    };
    Ok(Value::Bool(empty))
}

fn builtin_insert(args: &[Value]) -> RuntimeResult<Value> {
    // Map dispatch: `m.insert(k, v)` - keyed insert, no index.
    if matches!(
        args.first(),
        Some(Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_))
    ) {
        return builtin_map_insert(args);
    }
    let idx = args
        .get(1)
        .ok_or(RuntimeError::Type("index must be integer".to_string()))?;
    let idx = crate::vm::index_value(idx)?;
    let value = args.get(2).cloned().unwrap_or(Value::Unit);
    match args.first() {
        Some(Value::Array(parts)) => {
            let len = parts.len() as i64;
            if idx < 0 || idx > len {
                return Err(RuntimeError::Panic(format!(
                    "insert: index {idx} out of bounds for length {len}"
                )));
            }
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.insert(idx as usize, value);
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(data)) => {
            let len = data.len() as i64;
            if idx < 0 || idx > len {
                return Err(RuntimeError::Panic(format!(
                    "insert: index {idx} out of bounds for length {len}"
                )));
            }
            let mut owned = crate::value::copy_with_capacity(data);
            if let Value::Int(n) = value {
                owned.insert(idx as usize, n);
            }
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::FloatVec(data)) => {
            let len = data.len() as i64;
            if idx < 0 || idx > len {
                return Err(RuntimeError::Panic(format!(
                    "insert: index {idx} out of bounds for length {len}"
                )));
            }
            let mut owned = crate::value::copy_with_capacity(data);
            let f = match value {
                Value::Float(f) => Some(f),
                Value::Int(n) => Some(n as f64),
                _ => None,
            };
            if let Some(f) = f {
                owned.insert(idx as usize, f);
            }
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        _ => Ok(args.first().cloned().unwrap_or(Value::Unit)),
    }
}
