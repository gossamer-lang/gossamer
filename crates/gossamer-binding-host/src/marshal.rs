//! VM values to and from the [`AbiValue`]s of a VM call, and the table of
//! VM callables a binding holds by handle.

#![allow(unsafe_code, reason = "reads the C values a binding library answers")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gossamer_binding_abi::{AbiValue, tag};
use gossamer_interp::value::{
    FloatBits, MapKey, RuntimeError, RuntimeResult, SmolStr, Value, dense_map_with_capacity,
};
use gossamer_resolve::BindingType;
use parking_lot::Mutex;

/// A VM callable a binding holds, and how many shares of its handle are
/// held: the call's own, and one per `PersistentCallback`.
struct Held {
    callable: Value,
    shares: usize,
}

fn callables() -> &'static Mutex<HashMap<u64, Held>> {
    static CALLABLES: std::sync::OnceLock<Mutex<HashMap<u64, Held>>> = std::sync::OnceLock::new();
    CALLABLES.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

/// Registers `callable` with one share, answering its handle.
fn register_callable(callable: Value) -> u64 {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    callables().lock().insert(
        handle,
        Held {
            callable,
            shares: 1,
        },
    );
    handle
}

/// Takes another share of `handle`.
pub(crate) fn retain_callable(handle: u64) {
    if let Some(held) = callables().lock().get_mut(&handle) {
        held.shares += 1;
    }
}

/// Gives back a share of `handle`; the last share drops the callable.
pub(crate) fn release_callable(handle: u64) {
    let dropped = {
        let mut table = callables().lock();
        let Some(held) = table.get_mut(&handle) else {
            return;
        };
        held.shares -= 1;
        if held.shares > 0 {
            return;
        }
        table.remove(&handle)
    };
    // The callable drops outside the table's lock: dropping a closure may
    // release values whose own drops reach back here.
    drop(dropped);
}

/// The callable `handle` names.
pub(crate) fn callable(handle: u64) -> Option<Value> {
    callables()
        .lock()
        .get(&handle)
        .map(|held| held.callable.clone())
}

/// The storage the encoded values of one call point into, and the callables
/// the call registered, whose call shares end with the call.
#[derive(Default)]
pub(crate) struct Encoded {
    arrays: Vec<Box<[AbiValue]>>,
    texts: Vec<Box<[u8]>>,
    callbacks: Vec<u64>,
}

impl Drop for Encoded {
    fn drop(&mut self) {
        for handle in self.callbacks.drain(..) {
            release_callable(handle);
        }
    }
}

fn type_err<T>(what: &str) -> RuntimeResult<T> {
    Err(RuntimeError::Type(format!(
        "a {what} cannot cross to a binding"
    )))
}

impl Encoded {
    fn owned_text(&mut self, tag: u32, bytes: Box<[u8]>) -> AbiValue {
        let value = AbiValue::pointing(tag, bytes.as_ptr(), bytes.len());
        self.texts.push(bytes);
        value
    }

    fn array(&mut self, tag: u32, items: Vec<AbiValue>, count: usize) -> AbiValue {
        let items = items.into_boxed_slice();
        let value = AbiValue::pointing(tag, items.as_ptr().cast(), count);
        self.arrays.push(items);
        value
    }

    fn callback(&mut self, callable: Value) -> AbiValue {
        let handle = register_callable(callable);
        self.callbacks.push(handle);
        AbiValue::words(tag::CALLBACK, handle, 0)
    }

    fn list(&mut self, tag: u32, items: &[Value]) -> RuntimeResult<AbiValue> {
        let encoded = items
            .iter()
            .map(|v| self.encode(v, None))
            .collect::<RuntimeResult<Vec<_>>>()?;
        Ok(self.array(tag, encoded, items.len()))
    }

    fn map(&mut self, entries: Vec<(Value, Value)>) -> RuntimeResult<AbiValue> {
        let count = entries.len();
        let mut encoded = Vec::with_capacity(count * 2);
        for (k, v) in &entries {
            encoded.push(self.encode(k, None)?);
            encoded.push(self.encode(v, None)?);
        }
        Ok(self.array(tag::MAP, encoded, count))
    }

    /// Encodes `value` for a parameter of `ty`. A string borrows the text of
    /// `value`, which the caller keeps alive for the call.
    pub(crate) fn encode(
        &mut self,
        value: &Value,
        ty: Option<&BindingType>,
    ) -> RuntimeResult<AbiValue> {
        if matches!(ty, Some(BindingType::Callback(..))) {
            return match value {
                Value::Closure(_) | Value::Native(_) | Value::Builtin(_) | Value::String(_) => {
                    Ok(self.callback(value.clone()))
                }
                _ => type_err(value_kind(value)),
            };
        }
        Ok(match value {
            Value::Unit | Value::Void => AbiValue::UNIT,
            Value::Bool(b) => AbiValue::words(tag::BOOL, u64::from(*b), 0),
            Value::Int(i) => AbiValue::words(tag::INT, *i as u64, 0),
            Value::Uint(u) => AbiValue::words(tag::INT, *u, 0),
            Value::Float(f) => AbiValue::words(tag::FLOAT, f.to_bits(), 0),
            Value::Char(c) => AbiValue::words(tag::CHAR, u64::from(*c), 0),
            Value::String(s) => {
                let text = s.as_str();
                AbiValue::pointing(tag::STRING, text.as_ptr(), text.len())
            }
            Value::ByteVec(_)
            | Value::ByteArray(_)
            | Value::InlineByteArray(_)
            | Value::IntArray(_)
            | Value::FloatVec(_) => self.encode_flat(value, ty)?,
            Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_) => self.encode_map(value)?,
            Value::Variant(_) | Value::NativeEnum(_) | Value::Struct(_) => {
                self.encode_named(value, ty)?
            }
            Value::FloatArray(_) => {
                let rehydrated = value.float_array_to_value_array();
                self.encode(&rehydrated, ty)?
            }
            Value::Array(items) => {
                let elem = match ty {
                    Some(BindingType::Vec(elem)) => Some(&**elem),
                    _ => None,
                };
                let encoded = items
                    .iter()
                    .map(|v| self.encode(v, elem))
                    .collect::<RuntimeResult<Vec<_>>>()?;
                self.array(tag::LIST, encoded, items.len())
            }
            Value::Tuple(items) => {
                let elems = match ty {
                    Some(BindingType::Tuple(elems)) => Some(elems.as_slice()),
                    _ => None,
                };
                let encoded = items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| self.encode(v, elems.and_then(|e| e.get(i))))
                    .collect::<RuntimeResult<Vec<_>>>()?;
                self.array(tag::TUPLE, encoded, items.len())
            }
            Value::Json(json) => self.encode(&json_to_value(json.as_value()), ty)?,
            Value::MutCell(cell) => {
                let inner = cell.lock().clone();
                self.encode_owned(inner, ty)?
            }
            Value::CaptureCell(cell) => {
                let inner = cell.lock().clone();
                self.encode_owned(inner, ty)?
            }
            Value::Closure(_) | Value::Native(_) | Value::Builtin(_) => {
                self.callback(value.clone())
            }
            Value::Channel(_) | Value::Opaque(_) | Value::LazyIter(_) | Value::Weak(_) => {
                return type_err(value_kind(value));
            }
        })
    }

    /// Encodes a flat sequence of bytes or numbers.
    fn encode_flat(&mut self, value: &Value, ty: Option<&BindingType>) -> RuntimeResult<AbiValue> {
        Ok(match value {
            Value::ByteVec(bytes) => AbiValue::pointing(tag::BYTES, bytes.as_ptr(), bytes.len()),
            Value::ByteArray(bytes) => {
                if matches!(ty, Some(BindingType::Bytes)) {
                    AbiValue::pointing(tag::BYTES, bytes.as_ptr(), bytes.len())
                } else {
                    let items: Vec<Value> =
                        bytes.iter().map(|b| Value::Int(i64::from(*b))).collect();
                    self.list(tag::LIST, &items)?
                }
            }
            Value::InlineByteArray(bytes) => {
                if matches!(ty, Some(BindingType::Bytes)) {
                    self.owned_text(tag::BYTES, bytes.to_vec().into_boxed_slice())
                } else {
                    let items: Vec<Value> =
                        bytes.iter().map(|b| Value::Int(i64::from(*b))).collect();
                    self.list(tag::LIST, &items)?
                }
            }
            Value::IntArray(items) => {
                if matches!(ty, Some(BindingType::Bytes))
                    && items.iter().all(|v| (0..=255).contains(v))
                {
                    let bytes: Vec<u8> = items.iter().map(|v| *v as u8).collect();
                    self.owned_text(tag::BYTES, bytes.into_boxed_slice())
                } else {
                    let items: Vec<Value> = items.iter().map(|i| Value::Int(*i)).collect();
                    self.list(tag::LIST, &items)?
                }
            }
            Value::FloatVec(items) => {
                let items: Vec<Value> = items.iter().map(|f| Value::Float(*f)).collect();
                self.list(tag::LIST, &items)?
            }
            _ => unreachable!("`encode` routes only the flat sequence shapes here"),
        })
    }

    /// Encodes one of the map shapes.
    fn encode_map(&mut self, value: &Value) -> RuntimeResult<AbiValue> {
        Ok(match value {
            Value::Map(map) => {
                let entries: Vec<(Value, Value)> = map
                    .lock()
                    .iter()
                    .map(|(k, v)| (map_key_to_value(k), v.clone()))
                    .collect();
                self.map(entries)?
            }
            Value::IntMap(map) => {
                let entries: Vec<(Value, Value)> = map
                    .lock()
                    .iter()
                    .map(|(k, v)| (Value::Int(*k), Value::Int(*v)))
                    .collect();
                self.map(entries)?
            }
            Value::StrIntMap(map) => {
                let entries: Vec<(Value, Value)> = map
                    .lock()
                    .iter()
                    .map(|(k, v)| (Value::String(k.clone()), Value::Int(*v)))
                    .collect();
                self.map(entries)?
            }
            _ => unreachable!("`encode` routes only the map shapes here"),
        })
    }

    /// Encodes a variant or a struct, which carry their names.
    fn encode_named(&mut self, value: &Value, ty: Option<&BindingType>) -> RuntimeResult<AbiValue> {
        Ok(match value {
            Value::Variant(inner) => {
                let name = inner.name.to_string();
                let mut encoded =
                    vec![self.owned_text(tag::STRING, name.into_bytes().into_boxed_slice())];
                for field in &inner.fields {
                    encoded.push(self.encode(field, None)?);
                }
                let count = encoded.len();
                self.array(tag::VARIANT, encoded, count)
            }
            Value::NativeEnum(owner) => {
                let variant = gossamer_interp::native_enum_to_variant(owner);
                self.encode(&variant, ty)?
            }
            Value::Struct(inner) => {
                let name = inner.name.to_string();
                let mut encoded =
                    vec![self.owned_text(tag::STRING, name.into_bytes().into_boxed_slice())];
                for (field, v) in inner.fields.iter() {
                    encoded.push(AbiValue::pointing(tag::STRING, field.as_ptr(), field.len()));
                    encoded.push(self.encode(v, None)?);
                }
                let count = encoded.len();
                self.array(tag::STRUCT, encoded, count)
            }
            _ => unreachable!("`encode` routes only variants and structs here"),
        })
    }

    /// Encodes a value this call owns, keeping the text it borrows alive.
    fn encode_owned(&mut self, value: Value, ty: Option<&BindingType>) -> RuntimeResult<AbiValue> {
        match &value {
            Value::String(s) => Ok(self.owned_text(tag::STRING, s.as_str().as_bytes().into())),
            Value::ByteVec(bytes) => Ok(self.owned_text(tag::BYTES, bytes.as_slice().into())),
            Value::ByteArray(bytes) => {
                if matches!(ty, Some(BindingType::Bytes)) {
                    Ok(self.owned_text(tag::BYTES, bytes.to_vec().into_boxed_slice()))
                } else {
                    self.encode(&value, ty)
                }
            }
            _ => self.encode(&value, ty),
        }
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Channel(_) => "channel",
        Value::Opaque(_) => "runtime handle",
        Value::LazyIter(_) => "lazy iterator",
        Value::Weak(_) => "weak reference",
        _ => "value of this kind",
    }
}

fn json_to_value(json: &gossamer_std::json::Value) -> Value {
    use gossamer_std::json::Value as J;
    match json {
        J::Null => Value::Unit,
        J::Bool(b) => Value::Bool(*b),
        J::Int(i) => Value::Int(*i),
        J::Uint(u) => Value::Int(i64::try_from(*u).unwrap_or(i64::MAX)),
        J::Number(f) => Value::Float(*f),
        J::String(s) => Value::String(SmolStr::from_str(s)),
        J::Array(items) => Value::Array(Arc::new(items.iter().map(json_to_value).collect())),
        J::Object(entries) => {
            let mut out = dense_map_with_capacity(entries.len());
            for (key, value) in entries {
                out.insert(MapKey::Str(SmolStr::from_str(key)), json_to_value(value));
            }
            Value::Map(Arc::new(parking_lot::Mutex::new(out.into())))
        }
    }
}

fn map_key_to_value(k: &MapKey) -> Value {
    match k {
        MapKey::NonHashable | MapKey::Agg(..) => Value::Unit,
        MapKey::Bool(b) => Value::Bool(*b),
        MapKey::Int(i) => Value::Int(*i),
        MapKey::Uint(n) => Value::Int(*n as i64),
        MapKey::Char(c) => Value::Char(*c),
        MapKey::Float(bits) => Value::Float(f64::from_bits(bits.0)),
        MapKey::Str(s) => Value::String(s.clone()),
    }
}

fn value_to_map_key(v: &Value) -> MapKey {
    match v {
        Value::Bool(b) => MapKey::Bool(*b),
        Value::Int(i) => MapKey::Int(*i),
        Value::Uint(u) => MapKey::Int(*u as i64),
        Value::Char(c) => MapKey::Char(*c),
        Value::Float(f) => MapKey::Float(FloatBits(f.to_bits())),
        Value::String(s) => MapKey::Str(s.clone()),
        _ => MapKey::NonHashable,
    }
}

/// Reads a value a binding answered.
///
/// # Safety
/// `value` and everything it points at stay alive for the read.
pub(crate) unsafe fn decode(value: &AbiValue) -> RuntimeResult<Value> {
    // SAFETY: the caller's contract covers every nested value.
    let decode_all = |items: &[AbiValue]| -> RuntimeResult<Vec<Value>> {
        items.iter().map(|v| unsafe { decode(v) }).collect()
    };
    let text = |value: &AbiValue| -> RuntimeResult<String> {
        // SAFETY: as above.
        let bytes = unsafe { value.bytes() };
        String::from_utf8(bytes.to_vec()).map_err(|_| {
            RuntimeError::Type("a binding answered text that is not UTF-8".to_string())
        })
    };
    // SAFETY: as above.
    let elements = || unsafe { value.elements() };
    Ok(match value.tag {
        tag::UNIT => Value::Unit,
        tag::BOOL => Value::Bool(value.a != 0),
        tag::INT => Value::Int(value.a as i64),
        tag::FLOAT => Value::Float(f64::from_bits(value.a)),
        tag::CHAR => Value::Char(
            u32::try_from(value.a)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| {
                    RuntimeError::Type(format!("a binding answered char {}", value.a))
                })?,
        ),
        tag::STRING => Value::String(SmolStr::from_string(text(value)?)),
        // SAFETY: as above.
        tag::BYTES => Value::ByteVec(Arc::new(unsafe { value.bytes() }.to_vec())),
        tag::LIST => Value::Array(Arc::new(decode_all(elements())?)),
        tag::TUPLE => Value::Tuple(Arc::new(decode_all(elements())?)),
        tag::MAP => {
            let items = decode_all(elements())?;
            let mut out = dense_map_with_capacity(items.len() / 2);
            let mut iter = items.into_iter();
            while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
                out.insert(value_to_map_key(&k), v);
            }
            Value::Map(Arc::new(parking_lot::Mutex::new(out.into())))
        }
        tag::VARIANT => {
            let items = elements();
            let (name, payload) = items.split_first().ok_or_else(|| {
                RuntimeError::Type("a binding answered a nameless variant".to_string())
            })?;
            Value::variant(text(name)?, decode_all(payload)?)
        }
        tag::STRUCT => {
            let items = elements();
            let (name, rest) = items.split_first().ok_or_else(|| {
                RuntimeError::Type("a binding answered a nameless struct".to_string())
            })?;
            let mut fields = Vec::with_capacity(rest.len() / 2);
            for pair in rest.chunks_exact(2) {
                let field = gossamer_interp::value::intern_field_name(&text(&pair[0])?);
                // SAFETY: as above.
                fields.push((field, unsafe { decode(&pair[1]) }?));
            }
            Value::struct_(text(name)?, fields)
        }
        tag::CALLBACK => callable(value.a).ok_or_else(|| {
            RuntimeError::Type(format!(
                "a binding answered callback {} it no longer holds",
                value.a
            ))
        })?,
        other => {
            return Err(RuntimeError::Type(format!(
                "a binding answered value tag {other}"
            )));
        }
    })
}

/// The text of a failed call's message.
pub(crate) fn message_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.as_str().to_string(),
        other => format!("{other:?}"),
    }
}
