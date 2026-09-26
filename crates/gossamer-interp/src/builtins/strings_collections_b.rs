/// Bytes taken from a packed byte-sequence receiver, paired with the
/// constructor that rebuilds that receiver's own representation.
type PackedBytes = (Vec<u8>, fn(Vec<u8>) -> Value);

/// Bytes of a packed byte-sequence receiver plus the constructor that
/// rebuilds its own representation, so a mutator answers the same shape it
/// was handed. `None` for a receiver that is not a byte sequence.
///
/// `Vec<u8>` and `[u8; N]` reach a builtin as one of these packed forms
/// rather than a boxed `Array`, so a mutator that matches only `Array` /
/// `IntArray` / `FloatVec` would answer its receiver unchanged.
fn packed_bytes_receiver(recv: &Value) -> Option<PackedBytes> {
    match recv {
        Value::ByteVec(data) => Some((crate::value::copy_with_capacity(data), |b| {
            Value::ByteVec(Arc::new(b))
        })),
        Value::ByteArray(data) => {
            Some((data.to_vec(), |b| Value::ByteArray(Arc::new(b.into()))))
        }
        Value::InlineByteArray(data) => Some((data.to_vec(), |b| {
            Value::InlineByteArray(Arc::new(smallvec::SmallVec::from_vec(b)))
        })),
        _ => None,
    }
}

/// Rebuilds a sequence in the receiver's own representation when the new
/// elements still fit it, so a packed `Vec<u8>` or `Vec<i64>` stays
/// packed instead of widening to boxed values on every bulk edit. The
/// rebuilt storage keeps the receiver's capacity, as an in-place edit would.
fn rebuild_sequence(receiver: &Value, values: Vec<Value>) -> Value {
    fn packed<T>(
        capacity: usize,
        values: &[Value],
        elem: impl Fn(&Value) -> Option<T>,
    ) -> Option<Vec<T>> {
        let mut out = Vec::with_capacity(capacity);
        for value in values {
            out.push(elem(value)?);
        }
        Some(out)
    }
    fn byte(value: &Value) -> Option<u8> {
        match value {
            Value::Int(n) => u8::try_from(*n).ok(),
            _ => None,
        }
    }
    let capacity = sequence_capacity(receiver).max(values.len());
    let boxed = |values: Vec<Value>| {
        let mut out = Vec::with_capacity(capacity);
        out.extend(values);
        Value::Array(Arc::new(out))
    };
    match receiver {
        Value::IntArray(_) => packed(capacity, &values, |v| match v {
            Value::Int(n) => Some(*n),
            Value::Uint(n) => Some(*n as i64),
            _ => None,
        })
        .map_or_else(|| boxed(values), |ints| Value::IntArray(Arc::new(ints))),
        Value::FloatVec(_) => packed(capacity, &values, |v| match v {
            Value::Float(f) => Some(*f),
            Value::Int(n) => Some(*n as f64),
            _ => None,
        })
        .map_or_else(|| boxed(values), |floats| Value::FloatVec(Arc::new(floats))),
        Value::ByteVec(_) => packed(capacity, &values, byte)
            .map_or_else(|| boxed(values), |bytes| Value::ByteVec(Arc::new(bytes))),
        Value::ByteArray(_) => packed(capacity, &values, byte).map_or_else(
            || boxed(values),
            |bytes| Value::ByteArray(Arc::new(bytes.into())),
        ),
        Value::InlineByteArray(_) => packed(capacity, &values, byte).map_or_else(
            || boxed(values),
            |bytes| Value::InlineByteArray(Arc::new(smallvec::SmallVec::from_vec(bytes))),
        ),
        _ => boxed(values),
    }
}

/// Element capacity of a sequence value's backing store; zero for a non-sequence.
fn sequence_capacity(value: &Value) -> usize {
    match value {
        Value::Array(parts) => parts.capacity(),
        Value::IntArray(data) => data.capacity(),
        Value::ByteArray(data) => data.len(),
        Value::InlineByteArray(data) => data.capacity(),
        Value::ByteVec(data) => data.capacity(),
        Value::FloatVec(data) => data.capacity(),
        Value::FloatArray(inner) => inner
            .data
            .capacity()
            .checked_div(usize::from(inner.stride))
            .unwrap_or(0),
        _ => 0,
    }
}

/// `xs.copy_within(src, dest, len)`. The ranges may overlap, which is the
/// operation's whole purpose, so the source is read before any write.
fn builtin_copy_within(args: &[Value]) -> RuntimeResult<Value> {
    let Some(receiver) = args.first() else {
        return Ok(Value::Unit);
    };
    let Some(mut values) = array_as_values(receiver) else {
        return Ok(receiver.clone());
    };
    let read = |idx: usize| -> RuntimeResult<i64> {
        args.get(idx)
            .and_then(crate::builtins::value_to_int)
            .ok_or_else(|| RuntimeError::Type("copy_within: indices must be integers".to_string()))
    };
    let (src, dest, len) = (read(1)?, read(2)?, read(3)?);
    let vec_len = values.len() as i64;
    if src < 0 || dest < 0 || len < 0 || src + len > vec_len || dest + len > vec_len {
        return Err(RuntimeError::Panic(
            "copy_within: range outside the vector".to_string(),
        ));
    }
    let staged: Vec<Value> = values[src as usize..(src + len) as usize].to_vec();
    values[dest as usize..(dest + len) as usize].clone_from_slice(&staged);
    Ok(rebuild_sequence(receiver, values))
}

/// `dst.copy_from_slice(src)`. Both sequences must have the same length.
fn builtin_copy_from_slice(args: &[Value]) -> RuntimeResult<Value> {
    let Some(receiver) = args.first() else {
        return Ok(Value::Unit);
    };
    let Some(values) = array_as_values(receiver) else {
        return Ok(receiver.clone());
    };
    let source = args.get(1).and_then(array_as_values).unwrap_or_default();
    if values.len() != source.len() {
        return Err(RuntimeError::Panic(
            "copy_from_slice: source and destination differ in length".to_string(),
        ));
    }
    Ok(rebuild_sequence(receiver, source))
}

/// `xs.resize(new_len, value)` - truncate, or append copies of `value`.
pub(crate) fn builtin_resize(args: &[Value]) -> RuntimeResult<Value> {
    let Some(receiver) = args.first() else {
        return Ok(Value::Unit);
    };
    let Some(mut values) = array_as_values(receiver) else {
        return Ok(receiver.clone());
    };
    let new_len = args
        .get(1)
        .and_then(crate::builtins::value_to_int)
        .ok_or_else(|| RuntimeError::Type("resize: length must be an integer".to_string()))?;
    if new_len < 0 {
        return Err(RuntimeError::Panic(
            "resize: length must be non-negative".to_string(),
        ));
    }
    let fill = args.get(2).cloned().unwrap_or(Value::Unit);
    values.resize(new_len as usize, fill);
    Ok(rebuild_sequence(receiver, values))
}

/// `xs.binary_search(needle)` over an already-ascending sequence:
/// `Ok(index)` when found, `Err(index)` at the position an insert would
/// keep sorted.
fn builtin_binary_search(args: &[Value]) -> RuntimeResult<Value> {
    let values = args
        .first()
        .and_then(array_as_values)
        .unwrap_or_default();
    let needle = args.get(1).cloned().unwrap_or(Value::Unit);
    let (mut lo, mut hi) = (0usize, values.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match crate::vm::value_ordering(&values[mid], &needle)? {
            std::cmp::Ordering::Less => lo = mid + 1,
            _ => hi = mid,
        }
    }
    let found = lo < values.len()
        && crate::vm::value_ordering(&values[lo], &needle)? == std::cmp::Ordering::Equal;
    let index = Value::Int(lo as i64);
    Ok(Value::variant(if found { "Ok" } else { "Err" }, vec![index]))
}

fn builtin_remove(args: &[Value]) -> RuntimeResult<Value> {
    if matches!(
        args.first(),
        Some(Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_))
    ) {
        return builtin_map_remove(args);
    }
    let idx = args
        .get(1)
        .ok_or(RuntimeError::Type("index must be integer".to_string()))?;
    let idx = crate::vm::index_value(idx)?;
    match args.first() {
        Some(Value::Array(parts)) => {
            let len = parts.len() as i64;
            if idx < 0 || idx >= len {
                return Err(RuntimeError::Panic(format!(
                    "remove: index {idx} out of bounds for length {len}"
                )));
            }
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.remove(idx as usize);
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(data)) => {
            let len = data.len() as i64;
            if idx < 0 || idx >= len {
                return Err(RuntimeError::Panic(format!(
                    "remove: index {idx} out of bounds for length {len}"
                )));
            }
            let mut owned = crate::value::copy_with_capacity(data);
            owned.remove(idx as usize);
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::FloatVec(data)) => {
            let len = data.len() as i64;
            if idx < 0 || idx >= len {
                return Err(RuntimeError::Panic(format!(
                    "remove: index {idx} out of bounds for length {len}"
                )));
            }
            let mut owned = crate::value::copy_with_capacity(data);
            owned.remove(idx as usize);
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        _ => Ok(args.first().cloned().unwrap_or(Value::Unit)),
    }
}

fn builtin_clear(args: &[Value]) -> RuntimeResult<Value> {
    if matches!(
        args.first(),
        Some(Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_))
    ) {
        return builtin_map_clear(args);
    }
    match args.first() {
        Some(Value::Array(parts)) => {
            Ok(Value::Array(Arc::new(Vec::with_capacity(parts.capacity()))))
        }
        Some(Value::IntArray(data)) => Ok(Value::IntArray(Arc::new(Vec::with_capacity(
            data.capacity(),
        )))),
        Some(Value::FloatVec(data)) => Ok(Value::FloatVec(Arc::new(Vec::with_capacity(
            data.capacity(),
        )))),
        Some(Value::ByteVec(data)) => Ok(Value::ByteVec(Arc::new(Vec::with_capacity(
            data.capacity(),
        )))),
        Some(Value::String(_)) => Ok(Value::String(SmolStr::from(String::new()))),
        Some(v) if packed_bytes_receiver(v).is_some() => Ok(Value::ByteVec(Arc::new(Vec::new()))),
        _ => Ok(args.first().cloned().unwrap_or(Value::Unit)),
    }
}

fn builtin_extend(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Array(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            if let Some(extra) = args.get(1).and_then(array_as_values) {
                owned.extend(extra);
            }
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            if let Some(extra) = args.get(1).and_then(array_as_values) {
                owned.extend(extra.into_iter().filter_map(|v| match v {
                    Value::Int(n) => Some(n),
                    _ => None,
                }));
            }
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::FloatVec(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            if let Some(extra) = args.get(1).and_then(array_as_values) {
                owned.extend(extra.into_iter().filter_map(|v| match v {
                    Value::Float(f) => Some(f),
                    Value::Int(n) => Some(n as f64),
                    _ => None,
                }));
            }
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        Some(v) => {
            let Some((mut owned, rebuild)) = packed_bytes_receiver(v) else {
                return Ok(v.clone());
            };
            if let Some(extra) = args.get(1).and_then(array_as_values) {
                owned.extend(
                    extra
                        .into_iter()
                        .filter_map(|v| crate::builtins::value_to_int(&v))
                        .map(|n| n as u8),
                );
            }
            Ok(rebuild(owned))
        }
        None => Ok(Value::Unit),
    }
}

fn builtin_truncate(args: &[Value]) -> RuntimeResult<Value> {
    let cap = match args.get(1) {
        Some(Value::Int(n)) if *n >= 0 => *n as usize,
        Some(Value::Int(_)) => {
            return Err(RuntimeError::Panic(
                "truncate: length must be non-negative".to_string(),
            ));
        }
        _ => return Ok(args.first().cloned().unwrap_or(Value::Unit)),
    };
    match args.first() {
        Some(Value::Array(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.truncate(cap);
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.truncate(cap);
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::FloatVec(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.truncate(cap);
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        Some(Value::String(s)) => {
            let end = s
                .char_indices()
                .map(|(idx, _)| idx)
                .chain(std::iter::once(s.as_str().len()))
                .take_while(|idx| *idx <= cap)
                .last()
                .unwrap_or(0);
            Ok(Value::String(SmolStr::from(&s[..end])))
        }
        Some(v) => {
            let Some((mut owned, rebuild)) = packed_bytes_receiver(v) else {
                return Ok(v.clone());
            };
            owned.truncate(cap);
            Ok(rebuild(owned))
        }
        None => Ok(Value::Unit),
    }
}

fn builtin_vec_reserve(args: &[Value]) -> RuntimeResult<Value> {
    let min_capacity = match args.get(1) {
        Some(Value::Int(n)) if *n >= 0 => *n as usize,
        Some(Value::Int(_)) => {
            return Err(RuntimeError::Type(
                "reserve: capacity must be non-negative".to_string(),
            ));
        }
        _ => return Ok(args.first().cloned().unwrap_or(Value::Unit)),
    };
    match args.first() {
        Some(Value::Array(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.reserve(min_capacity.saturating_sub(owned.len()));
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.reserve(min_capacity.saturating_sub(owned.len()));
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::ByteArray(data)) => {
            let mut owned = data.to_vec();
            owned.reserve(min_capacity.saturating_sub(owned.len()));
            Ok(Value::ByteVec(Arc::new(owned)))
        }
        Some(Value::InlineByteArray(data)) => {
            let mut owned = data.to_vec();
            owned.reserve(min_capacity.saturating_sub(owned.len()));
            Ok(Value::ByteVec(Arc::new(owned)))
        }
        Some(Value::ByteVec(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.reserve(min_capacity.saturating_sub(owned.len()));
            Ok(Value::ByteVec(Arc::new(owned)))
        }
        Some(Value::FloatVec(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.reserve(min_capacity.saturating_sub(owned.len()));
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        other => Ok(other.cloned().unwrap_or(Value::Unit)),
    }
}

fn builtin_vec_reserve_exact(args: &[Value]) -> RuntimeResult<Value> {
    let min_capacity = match args.get(1) {
        Some(Value::Int(n)) if *n >= 0 => *n as usize,
        Some(Value::Int(_)) => {
            return Err(RuntimeError::Type(
                "reserve_exact: capacity must be non-negative".to_string(),
            ));
        }
        _ => return Ok(args.first().cloned().unwrap_or(Value::Unit)),
    };
    match args.first() {
        Some(Value::Array(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.reserve_exact(min_capacity.saturating_sub(owned.len()));
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.reserve_exact(min_capacity.saturating_sub(owned.len()));
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::ByteArray(data)) => {
            let mut owned = data.to_vec();
            owned.reserve_exact(min_capacity.saturating_sub(owned.len()));
            Ok(Value::ByteVec(Arc::new(owned)))
        }
        Some(Value::InlineByteArray(data)) => {
            let mut owned = data.to_vec();
            owned.reserve_exact(min_capacity.saturating_sub(owned.len()));
            Ok(Value::ByteVec(Arc::new(owned)))
        }
        Some(Value::ByteVec(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.reserve_exact(min_capacity.saturating_sub(owned.len()));
            Ok(Value::ByteVec(Arc::new(owned)))
        }
        Some(Value::FloatVec(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.reserve_exact(min_capacity.saturating_sub(owned.len()));
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        other => Ok(other.cloned().unwrap_or(Value::Unit)),
    }
}

fn builtin_vec_capacity(args: &[Value]) -> RuntimeResult<Value> {
    let cap = args.first().map_or(0, sequence_capacity);
    Ok(Value::Int(i64::try_from(cap).unwrap_or(i64::MAX)))
}

fn builtin_sort(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Array(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.sort_by(crate::stdlib_builtins::iter::compare_values_total);
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.sort_unstable();
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::FloatVec(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            // The IEEE total order, which places NaN after every number, as
            // the compiled tiers sort.
            owned.sort_by(|a, b| gossamer_runtime::c_abi::sort::float_order(*a, *b));
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        Some(v) => {
            let Some((mut owned, rebuild)) = packed_bytes_receiver(v) else {
                return Ok(v.clone());
            };
            owned.sort_unstable();
            Ok(rebuild(owned))
        }
        None => Ok(Value::Unit),
    }
}

fn builtin_reverse(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Array(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.reverse();
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.reverse();
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::FloatVec(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.reverse();
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        Some(v) => {
            let Some((mut owned, rebuild)) = packed_bytes_receiver(v) else {
                return Ok(v.clone());
            };
            owned.reverse();
            Ok(rebuild(owned))
        }
        None => Ok(Value::Unit),
    }
}

fn builtin_swap(args: &[Value]) -> RuntimeResult<Value> {
    fn oob(i: i64, j: i64, len: usize) -> RuntimeError {
        RuntimeError::Panic(format!(
            "vec swap index out of bounds: the len is {len} but the index is {}",
            if i < 0 || i as usize >= len { i } else { j }
        ))
    }
    let raw_i = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => return Ok(args.first().cloned().unwrap_or(Value::Unit)),
    };
    let raw_j = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => return Ok(args.first().cloned().unwrap_or(Value::Unit)),
    };
    // `swap` is an indexed write, so an index outside `[0, len)` panics
    // exactly as `xs[i] = v` does rather than leaving the receiver untouched.
    let swapped = |len: usize| -> RuntimeResult<(usize, usize)> {
        if raw_i < 0 || raw_j < 0 || raw_i as usize >= len || raw_j as usize >= len {
            return Err(oob(raw_i, raw_j, len));
        }
        Ok((raw_i as usize, raw_j as usize))
    };
    match args.first() {
        Some(Value::Array(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            let (i, j) = swapped(owned.len())?;
            owned.swap(i, j);
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            let (i, j) = swapped(owned.len())?;
            owned.swap(i, j);
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::FloatVec(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            let (i, j) = swapped(owned.len())?;
            owned.swap(i, j);
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        Some(other) => Ok(other.clone()),
        None => Ok(Value::Unit),
    }
}

fn builtin_fill(args: &[Value]) -> RuntimeResult<Value> {
    let Some(value) = args.get(1) else {
        return Ok(args.first().cloned().unwrap_or(Value::Unit));
    };
    match args.first() {
        Some(Value::Array(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.fill(value.clone());
            Ok(Value::Array(Arc::new(owned)))
        }
        Some(Value::IntArray(parts)) => {
            let Value::Int(value) = value else {
                return Err(RuntimeError::Type(
                    "fill expects an integer element".to_string(),
                ));
            };
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.fill(*value);
            Ok(Value::IntArray(Arc::new(owned)))
        }
        Some(Value::FloatVec(parts)) => {
            let Value::Float(value) = value else {
                return Err(RuntimeError::Type(
                    "fill expects a float element".to_string(),
                ));
            };
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.fill(*value);
            Ok(Value::FloatVec(Arc::new(owned)))
        }
        Some(Value::ByteArray(parts)) => {
            let Value::Int(value) = value else {
                return Err(RuntimeError::Type("fill expects a byte element".to_string()));
            };
            let byte = u8::try_from(*value)
                .map_err(|_| {
                    RuntimeError::Type("fill byte must be in the range 0..=255".to_string())
                })?;
            let mut owned = parts.as_ref().clone();
            owned.fill(byte);
            Ok(Value::ByteArray(Arc::new(owned)))
        }
        Some(Value::InlineByteArray(parts)) => {
            let Value::Int(value) = value else {
                return Err(RuntimeError::Type("fill expects a byte element".to_string()));
            };
            let byte = u8::try_from(*value)
                .map_err(|_| {
                    RuntimeError::Type("fill byte must be in the range 0..=255".to_string())
                })?;
            let mut owned = parts.as_ref().clone();
            owned.fill(byte);
            Ok(Value::InlineByteArray(Arc::new(owned)))
        }
        Some(Value::ByteVec(parts)) => {
            let Value::Int(value) = value else {
                return Err(RuntimeError::Type("fill expects a byte element".to_string()));
            };
            let byte = u8::try_from(*value)
                .map_err(|_| {
                    RuntimeError::Type("fill byte must be in the range 0..=255".to_string())
                })?;
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.fill(byte);
            Ok(Value::ByteVec(Arc::new(owned)))
        }
        Some(other) => Ok(other.clone()),
        None => Ok(Value::Unit),
    }
}

/// `x.clone()` - a second value, independent of the receiver.
///
/// A `Map`'s table and a `Set`'s / slot container's element store sit behind
/// a handle that carries no count of its holders, so the copy takes storage
/// of its own wherever the receiver reaches one - at any depth through a
/// struct field, a tuple element, or a sequence slot. Everything else is a
/// cheap `Arc`/scalar copy, which the same walk answers.
fn builtin_clone(args: &[Value]) -> RuntimeResult<Value> {
    Ok(args
        .first()
        .map_or(Value::Unit, crate::vm::run::map_like_deep_clone))
}

/// `x.downgrade()` - produce a non-owning [`Value::Weak`] observing the
/// receiver's allocation.
fn builtin_downgrade(args: &[Value]) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    Ok(Value::Weak(crate::value::WeakValue::downgrade(&receiver)))
}

/// `w.upgrade()` - `Some(value)` while the referent is alive, else `None`.
fn builtin_upgrade(args: &[Value]) -> RuntimeResult<Value> {
    let some = |v: Value| Value::variant("Some", vec![v]);
    let none = Value::variant("None", vec![]);
    match args.first() {
        Some(Value::Weak(w)) => Ok(w.upgrade().map_or(none, some)),
        // A non-weak receiver can reach here only through an untyped
        // dispatch path; treat it as a live identity upgrade so the
        // value round-trips rather than vanishing.
        Some(other) => Ok(some(other.clone())),
        None => Ok(none),
    }
}

/// `it.next()` - returns `Some(first)` for non-empty
/// collection-shaped receivers and `None` for empty. The
/// for-loop fast paths handle real iterator state; this binding
/// covers user code that calls `.next()` once outside a
/// for-loop, and standalone `xs.iter().next()` shapes.
fn builtin_next(args: &[Value]) -> RuntimeResult<Value> {
    let none = Value::variant("None", vec![]);
    let some = |v: Value| Value::variant("Some", vec![v]);
    match args.first() {
        Some(Value::Array(items)) => {
            if let Some(first) = items.first() {
                Ok(some(first.clone()))
            } else {
                Ok(none)
            }
        }
        Some(Value::IntArray(items)) => {
            if let Some(first) = items.first() {
                Ok(some(Value::Int(*first)))
            } else {
                Ok(none)
            }
        }
        Some(Value::FloatVec(items)) => {
            if let Some(first) = items.first() {
                Ok(some(Value::Float(*first)))
            } else {
                Ok(none)
            }
        }
        Some(Value::Tuple(items)) => {
            if let Some(first) = items.first() {
                Ok(some(first.clone()))
            } else {
                Ok(none)
            }
        }
        Some(Value::String(s)) => {
            if let Some(c) = s.as_str().chars().next() {
                Ok(some(Value::Char(c)))
            } else {
                Ok(none)
            }
        }
        Some(value @ Value::LazyIter(_)) => {
            Ok(crate::stdlib_builtins::iter::lazy_iter_next_value(value)?.map_or_else(
                || none.clone(),
                some,
            ))
        }
        _ => Ok(none),
    }
}

fn builtin_path_join_v(args: &[Value]) -> RuntimeResult<Value> {
    let mut parts: Vec<String> = Vec::with_capacity(args.len());
    for a in args {
        if let Value::String(s) = a {
            parts.push(s.as_str().to_string());
        }
    }
    let mut p = std::path::PathBuf::new();
    for part in &parts {
        p.push(part);
    }
    Ok(Value::String(SmolStr::from(
        p.to_string_lossy().into_owned(),
    )))
}

fn builtin_btmap_new(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Map(Arc::new(parking_lot::Mutex::new(
        crate::vm_map::VmMap::ordered(crate::vm_map::KeyOrder::Natural),
    ))))
}

fn builtin_btmap_from(args: &[Value]) -> RuntimeResult<Value> {
    map_from(
        args,
        crate::vm_map::VmMap::ordered(crate::vm_map::KeyOrder::Natural),
    )
}

/// `m.__window(lo, hi, take)`: a `BTreeMap`'s entries ranked `lo..hi`, the
/// primitive its `first_key_value` / `pop_first` / `pop_last` desugar to.
fn builtin_btree_map_window(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(window) = crate::stdlib_builtins::set::set_window(args) {
        return Ok(window);
    }
    let Some(Value::Map(map)) = args.first() else {
        return Ok(Value::Map(Arc::new(parking_lot::Mutex::new(
            crate::vm_map::VmMap::ordered(crate::vm_map::KeyOrder::Natural),
        ))));
    };
    let int = |i: usize| args.get(i).and_then(value_to_int).unwrap_or(0);
    let window = map.lock().window(int(1), int(2), int(3) != 0);
    Ok(Value::Map(Arc::new(parking_lot::Mutex::new(window))))
}

/// `m.__range(lo, hi, mode)`: a `BTreeMap`'s entries between two keys. The
/// mode word says a lower bound is present (1), an upper bound is present
/// (2), and the upper bound is inclusive (4).
fn builtin_btree_map_range(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(window) = crate::stdlib_builtins::set::set_range(args) {
        return Ok(window);
    }
    let Some(Value::Map(map)) = args.first() else {
        return builtin_btree_map_window(args);
    };
    let mode = args.get(3).and_then(value_to_int).unwrap_or(0);
    let mut locked = map.lock();
    let bound = |i: usize| MapKey::from_value(args.get(i).unwrap_or(&Value::Unit));
    let lo = if mode & 1 != 0 { locked.rank(&bound(1), false) as i64 } else { 0 };
    let hi = if mode & 2 != 0 {
        locked.rank(&bound(2), mode & 4 != 0) as i64
    } else {
        i64::MAX
    };
    let window = locked.window(lo, hi, false);
    Ok(Value::Map(Arc::new(parking_lot::Mutex::new(window))))
}

/// `__btree_map_new(unsigned, [pairs])`: a `BTreeMap` whose integer keys
/// order unsigned when `unsigned` is set, filled from the optional pairs.
fn builtin_btree_map_new(args: &[Value]) -> RuntimeResult<Value> {
    let order = if matches!(args.first(), Some(Value::Int(n)) if *n != 0) {
        crate::vm_map::KeyOrder::Unsigned
    } else {
        crate::vm_map::KeyOrder::Natural
    };
    let empty = crate::vm_map::VmMap::ordered(order);
    match args.get(1) {
        Some(_) => map_from(&args[1..], empty),
        None => Ok(Value::Map(Arc::new(parking_lot::Mutex::new(empty)))),
    }
}

fn builtin_set_new(args: &[Value]) -> RuntimeResult<Value> {
    builtin_map_new(args)
}

fn builtin_duration_passthrough(args: &[Value]) -> RuntimeResult<Value> {
    Ok(args.first().cloned().unwrap_or(Value::Int(0)))
}

fn builtin_duration_secs_to_ms(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Int(n)) => Ok(Value::Int(n.saturating_mul(1000))),
        _ => Ok(Value::Int(0)),
    }
}

fn builtin_to_vec_v(args: &[Value]) -> RuntimeResult<Value> {
    Ok(args.first().cloned().unwrap_or(Value::Unit))
}

fn builtin_json_value_passthrough(args: &[Value]) -> RuntimeResult<Value> {
    Ok(args.first().cloned().unwrap_or(Value::Unit))
}

fn builtin_json_value_null(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Unit)
}

fn builtin_json_value_object(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Array(parts)) = args.first() else {
        return Ok(Value::struct_(
            "json::Object",
            Arc::unwrap_or_clone(Arc::new(Vec::new())),
        ));
    };
    let mut fields: Vec<(&'static str, Value)> = Vec::with_capacity(parts.len());
    for entry in parts.iter() {
        let Value::Tuple(pair) = entry else { continue };
        if pair.len() < 2 {
            continue;
        }
        let key = match &pair[0] {
            Value::String(s) => s.as_str().to_string(),
            other => format!("{other:?}"),
        };
        fields.push((crate::value::intern_type_name(&key), pair[1].clone()));
    }
    Ok(Value::struct_(
        "json::Object",
        Arc::unwrap_or_clone(Arc::new(fields)),
    ))
}

/// `json::set(obj, key, value) -> json::Value` - append or
/// replace the named field on a `json::Value::object()`-shaped
/// receiver and return the updated value. Non-object receivers
/// fall through unchanged so callers don't have to special-case
/// `Null` / arrays. Mirrors the surface documented in the SKILL
/// card and used by askq's chat-round assembly.
fn builtin_json_set(args: &[Value]) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let key = match args.get(1) {
        Some(Value::String(s)) => s.as_str().to_string(),
        Some(other) => format!("{other}"),
        None => return Ok(receiver),
    };
    let value = args.get(2).cloned().unwrap_or(Value::Unit);
    if let Value::Json(json) = &receiver {
        let json_std::Value::Object(entries) = json.as_value() else {
            return Ok(receiver);
        };
        let mut updated = entries.clone();
        updated.insert(key, gossamer_to_json_value(&value));
        return Ok(Value::Json(Arc::new(JsonInner::new(
            json_std::Value::Object(updated),
        ))));
    }
    let Value::Struct(inner) = &receiver else {
        return Ok(receiver);
    };
    // `json::parse` builds objects named `Object`; the `Value::object`
    // constructor builds `json::Object`. Both are object-shaped.
    if inner.name != "json::Object" && inner.name != "Object" {
        return Ok(receiver);
    }
    let mut fields: Vec<(&'static str, Value)> = inner.fields.to_vec();
    if let Some(slot) = fields.iter_mut().find(|(name, _)| *name == key) {
        slot.1 = value;
    } else {
        fields.push((crate::value::intern_type_name(&key), value));
    }
    Ok(Value::struct_(
        "json::Object",
        Arc::unwrap_or_clone(Arc::new(fields)),
    ))
}

fn builtin_variant_unwrap(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Variant(inner)) if inner.name == "Ok" || inner.name == "Some" => {
            inner.fields.first().cloned().ok_or_else(|| {
                RuntimeError::Panic(format!("unwrap on empty `{}` variant", inner.name))
            })
        }
        // `None` and `Err` are the two shapes `unwrap` refuses, and each
        // names itself the way the compiled tiers' shims do.
        Some(Value::Variant(inner)) if inner.name == "None" => Err(RuntimeError::Panic(
            "called `Option::unwrap()` on a `None` value".to_string(),
        )),
        Some(Value::Variant(inner)) => Err(RuntimeError::Panic(format!(
            "called `Result::unwrap()` on an `Err` value{}",
            inner
                .fields
                .first()
                .map(|v| format!(": {v}"))
                .unwrap_or_default()
        ))),
        Some(other) => Ok(other.clone()),
        None => Err(RuntimeError::Panic("unwrap without receiver".to_string())),
    }
}

fn builtin_variant_unwrap_or(args: &[Value]) -> RuntimeResult<Value> {
    let default = args.get(1).cloned().unwrap_or(Value::Unit);
    match args.first() {
        Some(Value::Variant(inner))
            if (inner.name == "Ok" || inner.name == "Some") && !inner.fields.is_empty() =>
        {
            Ok(inner.fields[0].clone())
        }
        _ => Ok(default),
    }
}

fn native_variant_unwrap_or_else(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let fallback = args.get(1).cloned().unwrap_or(Value::Unit);
    match &receiver {
        Value::Variant(inner)
            if (inner.name == "Ok" || inner.name == "Some") && !inner.fields.is_empty() =>
        {
            Ok(inner.fields[0].clone())
        }
        Value::Variant(inner) if inner.name == "Err" => {
            let err_value = inner.fields.first().cloned().unwrap_or(Value::Unit);
            invoke_callable(dispatch, &fallback, vec![err_value])
        }
        _ => invoke_callable(dispatch, &fallback, Vec::new()),
    }
}

fn builtin_variant_unwrap_or_default(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Variant(inner))
            if (inner.name == "Ok" || inner.name == "Some") && !inner.fields.is_empty() =>
        {
            Ok(inner.fields[0].clone())
        }
        _ => Ok(Value::Unit),
    }
}

fn builtin_variant_is<const TAG: char>(args: &[Value]) -> RuntimeResult<Value> {
    let want = match TAG {
        'S' => "Some",
        'N' => "None",
        'O' => "Ok",
        'E' => "Err",
        _ => return Ok(Value::Bool(false)),
    };
    let is = matches!(
        args.first(),
        Some(Value::Variant(inner)) if inner.name == want
    );
    Ok(Value::Bool(is))
}

fn builtin_variant_ok(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Variant(inner)) if inner.name == "Ok" && !inner.fields.is_empty() => {
            Ok(some_variant(inner.fields[0].clone()))
        }
        _ => Ok(none_variant()),
    }
}

fn builtin_variant_err(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Variant(inner)) if inner.name == "Err" && !inner.fields.is_empty() => {
            Ok(some_variant(inner.fields[0].clone()))
        }
        _ => Ok(none_variant()),
    }
}

/// `result.ok_or(new_err)` / `option.ok_or(new_err)`. Replaces a
/// missing-value variant (Err / None) with `Err(new_err)`; passes
/// the success variant through unchanged.
fn builtin_variant_ok_or(args: &[Value]) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let new_err = args.get(1).cloned().unwrap_or(Value::Unit);
    match &receiver {
        Value::Variant(inner)
            if (inner.name == "Ok" || inner.name == "Some") && !inner.fields.is_empty() =>
        {
            Ok(Value::variant("Ok", vec![inner.fields[0].clone()]))
        }
        _ => Ok(Value::variant("Err", vec![new_err])),
    }
}

/// `opt.and_then(f)` / `res.and_then(f)` - `f(payload)` when
/// Some/Ok (f returns the next Option/Result), None/Err passthrough.
fn native_variant_and_then(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let f = args.get(1).cloned().unwrap_or(Value::Unit);
    match &receiver {
        Value::Variant(inner)
            if (inner.name == "Some" || inner.name == "Ok") && !inner.fields.is_empty() =>
        {
            invoke_callable(dispatch, &f, vec![inner.fields[0].clone()])
        }
        other => Ok(other.clone()),
    }
}

/// `opt.or_else(f)` (f takes no argument) / `res.or_else(f)` (f takes
/// the Err payload) - Some/Ok passthrough.
fn native_variant_or_else(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let f = args.get(1).cloned().unwrap_or(Value::Unit);
    match &receiver {
        Value::Variant(inner) if inner.name == "None" => invoke_callable(dispatch, &f, Vec::new()),
        Value::Variant(inner) if inner.name == "Err" && !inner.fields.is_empty() => {
            invoke_callable(dispatch, &f, vec![inner.fields[0].clone()])
        }
        other => Ok(other.clone()),
    }
}

/// `opt.filter(p)` - keeps `Some(x)` only when `p(x)` holds.
fn native_variant_filter(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let p = args.get(1).cloned().unwrap_or(Value::Unit);
    match &receiver {
        Value::Variant(inner) if inner.name == "Some" && !inner.fields.is_empty() => {
            if matches!(
                invoke_callable(dispatch, &p, vec![inner.fields[0].clone()])?,
                Value::Bool(true)
            ) {
                Ok(receiver.clone())
            } else {
                Ok(none_variant())
            }
        }
        other => Ok(other.clone()),
    }
}

/// `opt.ok_or_else(f)` - `Ok(payload)` when Some, `Err(f())` when None.
fn native_variant_ok_or_else(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let f = args.get(1).cloned().unwrap_or(Value::Unit);
    match &receiver {
        Value::Variant(inner)
            if (inner.name == "Some" || inner.name == "Ok") && !inner.fields.is_empty() =>
        {
            Ok(Value::variant("Ok", vec![inner.fields[0].clone()]))
        }
        _ => Ok(Value::variant(
            "Err",
            vec![invoke_callable(dispatch, &f, Vec::new())?],
        )),
    }
}

fn native_variant_map(dispatch: &mut dyn NativeDispatch, args: &[Value]) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let transform = args.get(1).cloned().unwrap_or(Value::Unit);
    match &receiver {
        Value::Variant(inner)
            if (inner.name == "Some" || inner.name == "Ok") && !inner.fields.is_empty() =>
        {
            let mapped = invoke_callable(dispatch, &transform, vec![inner.fields[0].clone()])?;
            Ok(Value::variant(inner.name.clone(), vec![mapped]))
        }
        other => Ok(other.clone()),
    }
}

fn native_variant_map_or(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let default = args.get(1).cloned().unwrap_or(Value::Unit);
    let mapper = args.get(2).cloned().unwrap_or(Value::Unit);
    match &receiver {
        Value::Variant(inner)
            if (inner.name == "Some" || inner.name == "Ok") && !inner.fields.is_empty() =>
        {
            invoke_callable(dispatch, &mapper, vec![inner.fields[0].clone()])
        }
        _ => Ok(default),
    }
}

fn invoke_callable(
    dispatch: &mut dyn NativeDispatch,
    callable: &Value,
    args: Vec<Value>,
) -> RuntimeResult<Value> {
    dispatch.call_value(callable, args)
}

/// `arr.sort_by(|a, b| ordering)` - drives Rust's `sort_by` with a
/// Gossamer comparator. The comparator returns an i64 (negative
/// if a < b, zero if equal, positive if a > b), matching Rust's
/// `Ordering::cmp`. Falls back to identity when the receiver isn't
/// an array or the second arg isn't callable.
fn native_sort_by(dispatch: &mut dyn NativeDispatch, args: &[Value]) -> RuntimeResult<Value> {
    let comparator = args.get(1).cloned().unwrap_or(Value::Unit);
    let mut cmp_with = |a: Value, b: Value, sort_err: &mut Option<RuntimeError>| {
        if sort_err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match invoke_callable(dispatch, &comparator, vec![a, b]) {
            Ok(Value::Int(n)) => n.cmp(&0),
            Ok(Value::Float(f)) => f.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal),
            Ok(_) => std::cmp::Ordering::Equal,
            Err(e) => {
                *sort_err = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    };
    let mut sort_err: Option<RuntimeError> = None;
    let result = match args.first() {
        Some(Value::Array(parts)) => {
            let mut owned = crate::value::copy_with_capacity(parts);
            owned.sort_by(|a, b| cmp_with(a.clone(), b.clone(), &mut sort_err));
            Value::Array(Arc::new(owned))
        }
        Some(Value::IntArray(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.sort_by(|a, b| cmp_with(Value::Int(*a), Value::Int(*b), &mut sort_err));
            Value::IntArray(Arc::new(owned))
        }
        Some(Value::FloatVec(data)) => {
            let mut owned = crate::value::copy_with_capacity(data);
            owned.sort_by(|a, b| cmp_with(Value::Float(*a), Value::Float(*b), &mut sort_err));
            Value::FloatVec(Arc::new(owned))
        }
        Some(recv) if packed_bytes_receiver(recv).is_some() => {
            let (mut bytes, rebuild) =
                packed_bytes_receiver(recv).expect("packed receiver checked above");
            bytes.sort_by(|a, b| {
                cmp_with(
                    Value::Int(i64::from(*a)),
                    Value::Int(i64::from(*b)),
                    &mut sort_err,
                )
            });
            rebuild(bytes)
        }
        other => return Ok(other.cloned().unwrap_or(Value::Unit)),
    };
    if let Some(err) = sort_err {
        return Err(err);
    }
    Ok(result)
}

/// Renders `value` the way `{}` does, except that a struct or enum whose own
/// type supplies `method` answers through it - at any depth, since a value
/// carries its type name at run time. `method` is the channel's contract:
/// `to_string` for `Display` (`{}`), `fmt` for `Debug` (`{:?}`).
/// [`render_display`] for a value nested inside another: a string reads in
/// the quoted spelling that builds it, and a float in the form that shows it
/// is one, as every other nested rendering writes them.
fn render_nested(
    dispatch: &mut dyn NativeDispatch,
    value: &Value,
    aliases: &std::collections::HashMap<String, String>,
    method: &str,
) -> RuntimeResult<String> {
    match value {
        Value::String(text) => Ok(format!("{:?}", text.as_str())),
        Value::Char(ch) => Ok(format!("{ch:?}")),
        Value::Float(number) => Ok(gossamer_runtime::builtins::format_float_debug(*number)),
        other => render_display(dispatch, other, aliases, method),
    }
}

fn render_display(
    dispatch: &mut dyn NativeDispatch,
    value: &Value,
    aliases: &std::collections::HashMap<String, String>,
    method: &str,
) -> RuntimeResult<String> {
    let own_name = match value {
        Value::Struct(inner) => Some(inner.name.to_string()),
        Value::Variant(inner) => Some(inner.name.to_string()),
        _ => None,
    };
    let own_method = own_name.and_then(|name| {
        aliases.get(&name).cloned().or_else(|| {
            let qualified = format!("{name}::{method}");
            dispatch.has_fn(&qualified).then_some(qualified)
        })
    });
    if let Some(method) = own_method {
        return Ok(match dispatch.call_fn(&method, vec![value.clone()])? {
            Value::String(s) => s.as_str().to_string(),
            other => format!("{other}"),
        });
    }
    let joined = |dispatch: &mut dyn NativeDispatch, items: &[Value]| -> RuntimeResult<Vec<String>> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            // A float inside a sequence reads in the form that shows it is
            // one, the same text every other sequence rendering writes.
            out.push(render_nested(dispatch, item, aliases, method)?);
        }
        Ok(out)
    };
    match value {
        Value::Array(items) => Ok(format!("[{}]", joined(dispatch, items)?.join(", "))),
        Value::Tuple(items) => Ok(format!("({})", joined(dispatch, items)?.join(", "))),
        Value::Variant(inner) if inner.fields.is_empty() => Ok(inner.name.to_string()),
        Value::Variant(inner) => Ok(format!(
            "{}({})",
            inner.name,
            joined(dispatch, &inner.fields)?.join(", ")
        )),
        // A container keeps its elements in a runtime registry behind a
        // handle field, so the walk reads them out of it rather than
        // rendering the handle: `Queue [P { .. }]`, not the handle struct.
        Value::Struct(inner)
            if matches!(inner.name.as_str(), "Deque" | "Queue" | "Stack")
                && let Some(items) =
                    crate::stdlib_builtins::deque::deque_snapshot(value) =>
        {
            Ok(format!(
                "{} [{}]",
                inner.name,
                joined(dispatch, &items)?.join(", ")
            ))
        }
        Value::Struct(inner)
            if matches!(inner.name.as_str(), "MaxHeap" | "MinHeap")
                && let Some(items) =
                    crate::stdlib_builtins::container_heap::binary_heap_snapshot(value) =>
        {
            Ok(format!(
                "{} [{}]",
                inner.name,
                joined(dispatch, &items)?.join(", ")
            ))
        }
        // A set renders in the literal spelling that builds it, the same
        // one it has when nothing inside supplies its own rendering.
        Value::Struct(inner)
            if matches!(inner.name.as_str(), "Set" | "BTreeSet")
                && let Some(items) =
                    crate::stdlib_builtins::set::set_display_snapshot(value) =>
        {
            Ok(format!("#{{{}}}", joined(dispatch, &items)?.join(", ")))
        }
        // The renderer's copy of a `Vec` arrives inside a wrapper naming
        // it as one, since a `Vec` and a fixed array are one value shape.
        Value::Struct(inner) if let Some(items) = crate::value::vec_render_items(inner) => {
            match items.as_value_slice() {
                Some(parts) => Ok(format!("#[{}]", joined(dispatch, parts)?.join(", "))),
                // A flat typed storage holds no value this walk can reach,
                // so it renders as it does everywhere else.
                None => Ok(crate::value::vec_render_text(items)),
            }
        }
        // An error renders as its cause chain here too: a walk entered
        // because something else in the operand carries its own rendering
        // must not change how an error inside it shows.
        Value::Struct(_) if let Some(text) = crate::value::error_chain_text(value) => Ok(text),
        Value::Struct(inner) => {
            let mut parts = Vec::with_capacity(inner.fields.len());
            for (name, field) in &inner.fields {
                parts.push(format!("{name}: {}", render_nested(dispatch, field, aliases, method)?));
            }
            Ok(format!("{} {{ {} }}", inner.name, parts.join(", ")))
        }
        Value::Map(map) => {
            // Entries render in key order, as the plain formatter and both
            // compiled tiers' `gos_rt_map_format` render them.
            let entries: Vec<(crate::value::MapKey, Value)> = map
                .lock()
                .sorted()
                .into_iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let mut parts = Vec::with_capacity(entries.len());
            for (key, entry) in &entries {
                // A map renders a string key quoted, the way the synthesized
                // form and both compiled tiers show one.
                let key = match key.to_value() {
                    Value::String(text) => format!("{:?}", text.as_str()),
                    Value::Char(ch) => format!("{ch:?}"),
                    other => render_display(dispatch, &other, aliases, method)?,
                };
                parts.push(format!("{key}: {}", render_nested(dispatch, entry, aliases, method)?));
            }
            Ok(format!("{{{}}}", parts.join(", ")))
        }
        other => Ok(format!("{other}")),
    }
}

/// `{}` / `{:?}` over a value whose type, or one nested inside it, renders
/// itself. The third argument names the channel's contract - `to_string` for
/// `Display`, `fmt` for `Debug` - and defaults to `Display`.
fn native_render_display(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let Some(value) = args.first() else {
        return Ok(Value::String(String::new().into()));
    };
    let method = match args.get(2) {
        Some(Value::String(name)) => name.as_str().to_string(),
        _ => "to_string".to_string(),
    };
    let aliases: std::collections::HashMap<String, String> = match args.get(1) {
        Some(Value::String(text)) => text
            .as_str()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(variant, method)| (variant.to_string(), method.to_string()))
            .collect(),
        _ => std::collections::HashMap::new(),
    };
    Ok(Value::String(
        render_display(dispatch, value, &aliases, &method)?.into(),
    ))
}

/// `xs.join(sep)` where the elements' own type supplies the rendering: the
/// third argument names the method each element answers, so a user
/// `impl Display for T` shows through a join the way it shows through `{}`.
fn native_join_rendered(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let separator = match args.get(1) {
        Some(Value::String(s)) => s.as_str().to_string(),
        _ => String::new(),
    };
    let Some(Value::String(method)) = args.get(2) else {
        return crate::stdlib_builtins::strings::builtin_strings_join(args);
    };
    let method = method.as_str().to_string();
    let elements = args
        .first()
        .map(crate::stdlib_builtins::encoding_pem::collect_array)
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::with_capacity(elements.len());
    for element in elements {
        let rendered = dispatch.call_fn(&method, vec![element.clone()])?;
        parts.push(match rendered {
            Value::String(s) => s.as_str().to_string(),
            other => format!("{other}"),
        });
    }
    Ok(Value::String(parts.join(&separator).into()))
}

fn native_spawn(dispatch: &mut dyn NativeDispatch, args: &[Value]) -> RuntimeResult<Value> {
    let Some(callable) = args.first().cloned() else {
        return Ok(Value::Unit);
    };
    // `spawn(f, reason: "..")` carries a label the cohort reports name the
    // child by; the callable itself takes no arguments.
    let reason = match args.get(1) {
        Some(Value::String(text)) => text.to_string(),
        _ => String::new(),
    };
    // `spawn(f)` returns a join handle (a one-shot channel) whose
    // `.join()` blocks for the goroutine's `Result<T, String>`.
    dispatch.spawn_join_labelled(callable, Vec::new(), reason)
}

/// `handle.join() -> Result<T, String>` - blocks on the one-shot
/// handle channel for the spawned goroutine's outcome variant.
fn builtin_channel_join(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Channel(channel)) = args.first() else {
        return Ok(Value::variant(
            "Err",
            vec![Value::String("join on a non-handle value".into())],
        ));
    };
    let outcome = channel.recv();
    // Joining is how a child's outcome reaches the program, so a failure
    // read here is not one the root cohort reports as unobserved at exit.
    // Marked after the outcome arrives, by which point the child has
    // already recorded the failure.
    crate::stdlib_builtins::cohort::mark_handle_observed(channel.identity());
    match outcome {
        crate::value::RecvOutcome::Value(outcome) => Ok(outcome),
        crate::value::RecvOutcome::Closed => Ok(Value::variant(
            "Err",
            vec![Value::String("join: handle channel closed".into())],
        )),
        crate::value::RecvOutcome::Deadlocked => Err(crate::value::deadlock_error("join")),
    }
}

fn builtin_testing_check(args: &[Value]) -> RuntimeResult<Value> {
    let cond = matches!(args.first(), Some(Value::Bool(true)));
    let message = args.get(1).and_then(as_str).unwrap_or("check failed");
    let location = current_assertion_location()
        .map(|s| format!(" at {s}"))
        .unwrap_or_default();
    observe_assertion(cond, format!("check: {message}{location}"));
    if cond {
        Ok(ok_variant(Value::Unit))
    } else {
        Ok(err_variant(format!("assertion failed: {message}")))
    }
}

fn builtin_testing_check_eq(args: &[Value]) -> RuntimeResult<Value> {
    let left = args.first().cloned().unwrap_or(Value::Unit);
    let right = args.get(1).cloned().unwrap_or(Value::Unit);
    let message = args.get(2).and_then(as_str).unwrap_or("check_eq failed");
    let ok = values_equal_for_assertion(&left, &right);
    let location = current_assertion_location()
        .map(|s| format!(" at {s}"))
        .unwrap_or_default();
    // `{:?}` (Debug) wraps strings in quotes so a failing
    // `"foo "` vs `"foo"` (trailing space) is visible. Bare
    // `Display` would render them identically.
    observe_assertion(
        ok,
        format!("check_eq: {message}{location}: left={left:?}, right={right:?}"),
    );
    if ok {
        Ok(ok_variant(Value::Unit))
    } else {
        Ok(err_variant(format!(
            "{message}: left={left:?}, right={right:?}"
        )))
    }
}

fn builtin_testing_check_ok(args: &[Value]) -> RuntimeResult<Value> {
    let result = args.first().cloned().unwrap_or(Value::Unit);
    let message = args.get(1).and_then(as_str).unwrap_or("check_ok failed");
    match &result {
        Value::Variant(inner) if inner.name == "Ok" && !inner.fields.is_empty() => {
            observe_assertion(true, format!("check_ok: {message}"));
            Ok(ok_variant(inner.fields[0].clone()))
        }
        Value::Variant(inner) if inner.name == "Err" => {
            let msg = inner
                .fields
                .first()
                .map(|v| format!("{v}"))
                .unwrap_or_default();
            observe_assertion(false, format!("check_ok: {message}: {msg}"));
            Ok(err_variant(format!("{message}: {msg}")))
        }
        other => {
            observe_assertion(
                false,
                format!("check_ok: {message}: not a Result variant: {other}"),
            );
            Ok(err_variant(format!(
                "{message}: expected Result, got {other}"
            )))
        }
    }
}

fn builtin_testing_wait_for_scheduler_idle(args: &[Value]) -> RuntimeResult<Value> {
    let timeout_ms = match args.first().and_then(value_to_int) {
        Some(n) if n < 0 => {
            return Err(RuntimeError::Type(
                "testing::wait_for_scheduler_idle: timeout_ms must be non-negative".to_string(),
            ));
        }
        Some(n) => n,
        None => 1000,
    };
    let deadline = gossamer_runtime::platform::Instant::now()
        + std::time::Duration::from_millis(u64::try_from(timeout_ms).unwrap_or(0));
    let scheduler = gossamer_runtime::sched_global::scheduler();
    loop {
        let stats = scheduler.stats();
        if scheduler.live_goroutines() == 0 && stats.spawned == stats.finished {
            return Ok(Value::Bool(true));
        }
        if gossamer_runtime::platform::Instant::now() >= deadline {
            return Ok(Value::Bool(false));
        }
        gossamer_runtime::platform::sleep(std::time::Duration::from_millis(1));
    }
}

/// Structural equality check used by `testing::check_eq`; more
/// forgiving than `==` on values since it walks aggregates instead
/// of bailing out on type mismatch. Returns `false` rather than an
/// error on cross-kind operands.
fn values_equal_for_assertion(a: &Value, b: &Value) -> bool {
    // `assert_eq` reports what `==` reports. A sequence has several runtime
    // representations - a grown Vec and a literal of the same elements do not
    // share one - so the structural comparison behind `==` decides here too.
    if crate::vm::values_equal(a, b) {
        return true;
    }
    match (a, b) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Char(x), Value::Char(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Tuple(x), Value::Tuple(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|(a, b)| values_equal_for_assertion(a, b))
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|(a, b)| values_equal_for_assertion(a, b))
        }
        (Value::Variant(a), Value::Variant(b)) => {
            a.name == b.name
                && a.fields.len() == b.fields.len()
                && a.fields
                    .iter()
                    .zip(b.fields.iter())
                    .all(|(x, y)| values_equal_for_assertion(x, y))
        }
        _ => false,
    }
}

fn builtin_struct_new(args: &[Value]) -> RuntimeResult<Value> {
    let name: String = match args.first() {
        Some(Value::String(s)) => s.as_str().to_string(),
        _ => String::new(),
    };
    // Collect (field_name, value) pairs in source-literal order.
    // The synthetic `"__base"` key carries the functional-update
    // base value (from `Outer { n: 99, ..base }`); strip it out and
    // remember it so missing fields fill from `base.field` below.
    let mut pairs: Vec<(String, Value)> = Vec::with_capacity(args.len() / 2);
    let mut base: Option<Value> = None;
    let mut iter = args.iter().skip(1);
    while let (Some(key), Some(value)) = (iter.next(), iter.next()) {
        let Value::String(field_name) = key else {
            continue;
        };
        if field_name.as_str() == "__base" {
            base = Some(value.clone());
            continue;
        }
        pairs.push((field_name.to_string(), value.clone()));
    }
    let lookup_base_field = |field_name: &str| -> Option<Value> {
        match &base {
            Some(Value::Struct(inner)) => inner
                .fields
                .iter()
                .find(|(n, _)| (**n) == field_name)
                .map(|(_, v)| v.clone()),
            _ => None,
        }
    };
    // Reorder to declaration order when the struct's layout is
    // known. This makes every `Value::Struct { name: "Body" }`
    // share the same `fields[i]` layout, which lets the VM
    // emit compile-time offsets for field reads instead of
    // doing a linear name scan per read.
    let fields: Vec<(&'static str, Value)> = STRUCT_LAYOUTS.with(|cell| {
        let layouts = cell.borrow();
        if let Some(order) = layouts.get(&name) {
            let mut out: Vec<(&'static str, Value)> = Vec::with_capacity(order.len());
            for field_name in order {
                let value = pairs
                    .iter()
                    .find(|(n, _)| n.as_str() == *field_name)
                    .map(|(_, v)| v.clone())
                    .or_else(|| lookup_base_field(field_name))
                    .unwrap_or(Value::Unit);
                out.push((*field_name, value));
            }
            // Preserve any extra fields present in the literal
            // but not declared (should be rare; keeps program
            // state visible for debugging).
            for (n, v) in &pairs {
                // `order` holds `&'static str` but `n.as_str()` is a non-static
                // borrow, so `[T]::contains` (which wants `&&'static str`) does
                // not typecheck; the manual `any` is the only well-typed form.
                #[allow(clippy::manual_contains)]
                let declared = order.iter().any(|o| *o == n.as_str());
                if !declared {
                    out.push((crate::value::intern_type_name(n.as_str()), v.clone()));
                }
            }
            out
        } else if base.is_some() {
            // Layout unknown but a base is provided: start from the
            // base's fields and overlay the explicit overrides.
            let mut out: Vec<(&'static str, Value)> = match &base {
                Some(Value::Struct(inner)) => {
                    inner.fields.iter().map(|(n, v)| (*n, v.clone())).collect()
                }
                _ => Vec::new(),
            };
            for (n, v) in &pairs {
                if let Some(slot) = out.iter_mut().find(|(name, _)| *name == n.as_str()) {
                    slot.1 = v.clone();
                } else {
                    out.push((crate::value::intern_type_name(n.as_str()), v.clone()));
                }
            }
            out
        } else {
            pairs
                .into_iter()
                .map(|(n, v)| (crate::value::intern_type_name(n.as_str()), v))
                .collect()
        }
    });
    Ok(Value::struct_(name, Arc::unwrap_or_clone(Arc::new(fields))))
}

fn builtin_channel_new(args: &[Value]) -> RuntimeResult<Value> {
    // `channel()` / `channel(0)` is an unbuffered rendezvous channel,
    // matching Go's zero-capacity channel. `channel(N)` for positive N
    // is bounded. Use `channel::unbounded()` for the old queue form.
    let capacity = match args.first() {
        Some(Value::Int(n)) if *n < 0 => {
            return Err(RuntimeError::Type(
                "channel: capacity must be non-negative".to_string(),
            ));
        }
        Some(Value::Int(n)) if *n > 0 => *n as usize,
        _ => 0,
    };
    let channel = crate::value::Channel::with_capacity(capacity);
    let sender = Value::Channel(channel.clone());
    let receiver = Value::Channel(channel);
    Ok(Value::Tuple(Arc::from(vec![sender, receiver])))
}

fn builtin_channel_unbounded(_args: &[Value]) -> RuntimeResult<Value> {
    let channel = crate::value::Channel::unbounded();
    let sender = Value::Channel(channel.clone());
    let receiver = Value::Channel(channel);
    Ok(Value::Tuple(Arc::from(vec![sender, receiver])))
}

fn builtin_channel_send(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Channel(channel)) = args.first() else {
        return Err(RuntimeError::Type(
            "send: receiver must be a channel".to_string(),
        ));
    };
    // The receiver gets a value of its own: a table the sender still holds
    // is copied rather than shared, as a by-value argument's is.
    let value = args
        .get(1)
        .map_or(Value::Unit, crate::vm::run::map_like_deep_clone);
    match channel.send(value) {
        crate::value::SendOutcome::Sent => Ok(Value::Unit),
        crate::value::SendOutcome::Closed => Err(RuntimeError::Panic(
            "send on closed channel".to_string(),
        )),
        crate::value::SendOutcome::Deadlocked => Err(crate::value::deadlock_error("send")),
    }
}

