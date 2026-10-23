//! Conversions between binding [`Value`]s and Rust types.
//!
//! [`FromGos`] reads a Rust value out of a [`Value`]; [`ToGos`] builds one.
//! The VM entry of every binding item decodes its arguments and encodes its
//! answer through these traits, so binding authors write idiomatic Rust
//! signatures and rarely touch `Value` directly.

use std::collections::HashMap;
use std::sync::Arc;

use crate::value::{
    CallbackOrigin, CallbackRef, NativeDispatch, RuntimeError, RuntimeResult, Str, Value,
};

/// Materialises a typed Rust value out of a Gossamer [`Value`].
pub trait FromGos: Sized {
    /// Performs the conversion or returns a typed `RuntimeError`.
    fn from_gos(value: &Value) -> RuntimeResult<Self>;
}

/// Builds a Gossamer [`Value`] from a Rust value.
pub trait ToGos {
    /// Performs the conversion.
    fn to_gos(self) -> Value;
}

pub(crate) fn type_err<T>(expected: &str, found: &Value) -> RuntimeResult<T> {
    Err(RuntimeError::Type(format!(
        "expected {expected}, found {}",
        found.kind()
    )))
}

impl FromGos for () {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Unit => Ok(()),
            other => type_err("()", other),
        }
    }
}

impl ToGos for () {
    fn to_gos(self) -> Value {
        Value::Unit
    }
}

impl FromGos for bool {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Bool(b) => Ok(*b),
            other => type_err("bool", other),
        }
    }
}

impl ToGos for bool {
    fn to_gos(self) -> Value {
        Value::Bool(self)
    }
}

impl FromGos for i64 {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Int(i) => Ok(*i),
            other => type_err("i64", other),
        }
    }
}

impl ToGos for i64 {
    fn to_gos(self) -> Value {
        Value::Int(self)
    }
}

macro_rules! narrow_int {
    ($($ty:ty => $name:literal),* $(,)?) => {
        $(
            impl FromGos for $ty {
                fn from_gos(value: &Value) -> RuntimeResult<Self> {
                    let i = i64::from_gos(value)?;
                    <$ty>::try_from(i).map_err(|_| {
                        RuntimeError::Type(format!(concat!("expected ", $name, ", found {}"), i))
                    })
                }
            }

            impl ToGos for $ty {
                fn to_gos(self) -> Value {
                    Value::Int(i64::try_from(self).unwrap_or(i64::MAX))
                }
            }
        )*
    };
}

narrow_int!(u64 => "u64", usize => "usize", u16 => "u16", u8 => "u8");

impl FromGos for f64 {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Float(f) => Ok(*f),
            // Lossy for |i| > 2^53; a program that passes such an integer
            // to an f64 parameter accepted that.
            Value::Int(i) => Ok(*i as f64),
            other => type_err("f64", other),
        }
    }
}

impl ToGos for f64 {
    fn to_gos(self) -> Value {
        Value::Float(self)
    }
}

impl FromGos for char {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Char(c) => Ok(*c),
            other => type_err("char", other),
        }
    }
}

impl ToGos for char {
    fn to_gos(self) -> Value {
        Value::Char(self)
    }
}

impl FromGos for String {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::String(s) => Ok(s.as_str().to_string()),
            other => type_err("String", other),
        }
    }
}

impl ToGos for String {
    fn to_gos(self) -> Value {
        Value::String(Str::from(self))
    }
}

impl ToGos for &str {
    fn to_gos(self) -> Value {
        Value::String(Str::from(self))
    }
}

impl<T: FromGos> FromGos for Option<T> {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        let Value::Variant(inner) = value else {
            return type_err("Option<T>", value);
        };
        match inner.name.as_str() {
            "None" => Ok(None),
            "Some" => {
                let payload = inner
                    .fields
                    .first()
                    .ok_or_else(|| RuntimeError::Type("Some(_) without payload".to_string()))?;
                Ok(Some(T::from_gos(payload)?))
            }
            _ => type_err("Option<T>", value),
        }
    }
}

impl<T: ToGos> ToGos for Option<T> {
    fn to_gos(self) -> Value {
        match self {
            None => Value::variant("None", Vec::new()),
            Some(t) => Value::variant("Some", vec![t.to_gos()]),
        }
    }
}

impl<T: FromGos, E: FromGos> FromGos for Result<T, E> {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        let Value::Variant(inner) = value else {
            return type_err("Result<T, E>", value);
        };
        let first = inner
            .fields
            .first()
            .ok_or_else(|| RuntimeError::Type("Result<_, _> without payload".to_string()))?;
        match inner.name.as_str() {
            "Ok" => Ok(Ok(T::from_gos(first)?)),
            "Err" => Ok(Err(E::from_gos(first)?)),
            _ => type_err("Result<T, E>", value),
        }
    }
}

impl<T: ToGos, E: ToGos> ToGos for Result<T, E> {
    fn to_gos(self) -> Value {
        match self {
            Ok(t) => Value::variant("Ok", vec![t.to_gos()]),
            Err(e) => Value::variant("Err", vec![e.to_gos()]),
        }
    }
}

impl FromGos for Value {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        Ok(value.clone())
    }
}

impl ToGos for Value {
    fn to_gos(self) -> Value {
        self
    }
}

/// The elements of a tuple of `arity`, which a sequence of that length
/// also supplies.
fn tuple_items(value: &Value, arity: usize, expected: &str) -> RuntimeResult<Arc<Vec<Value>>> {
    match value {
        Value::Tuple(items) | Value::Array(items) => {
            if items.len() == arity {
                Ok(Arc::clone(items))
            } else {
                Err(RuntimeError::Type(format!(
                    "expected {arity}-tuple, found {}-element list",
                    items.len()
                )))
            }
        }
        other => type_err(expected, other),
    }
}

impl<A: FromGos, B: FromGos> FromGos for (A, B) {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        let items = tuple_items(value, 2, "(A, B)")?;
        Ok((A::from_gos(&items[0])?, B::from_gos(&items[1])?))
    }
}

impl<A: ToGos, B: ToGos> ToGos for (A, B) {
    fn to_gos(self) -> Value {
        Value::Tuple(Arc::new(vec![self.0.to_gos(), self.1.to_gos()]))
    }
}

impl<A: FromGos, B: FromGos, C: FromGos> FromGos for (A, B, C) {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        let items = tuple_items(value, 3, "(A, B, C)")?;
        Ok((
            A::from_gos(&items[0])?,
            B::from_gos(&items[1])?,
            C::from_gos(&items[2])?,
        ))
    }
}

impl<A: ToGos, B: ToGos, C: ToGos> ToGos for (A, B, C) {
    fn to_gos(self) -> Value {
        Value::Tuple(Arc::new(vec![
            self.0.to_gos(),
            self.1.to_gos(),
            self.2.to_gos(),
        ]))
    }
}

impl<A: FromGos, B: FromGos, C: FromGos, D: FromGos> FromGos for (A, B, C, D) {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        let items = tuple_items(value, 4, "(A, B, C, D)")?;
        Ok((
            A::from_gos(&items[0])?,
            B::from_gos(&items[1])?,
            C::from_gos(&items[2])?,
            D::from_gos(&items[3])?,
        ))
    }
}

impl<A: ToGos, B: ToGos, C: ToGos, D: ToGos> ToGos for (A, B, C, D) {
    fn to_gos(self) -> Value {
        Value::Tuple(Arc::new(vec![
            self.0.to_gos(),
            self.1.to_gos(),
            self.2.to_gos(),
            self.3.to_gos(),
        ]))
    }
}

impl<T: FromGos> FromGos for Vec<T> {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Array(items) | Value::Tuple(items) => items.iter().map(T::from_gos).collect(),
            Value::Bytes(bytes) => bytes
                .iter()
                .map(|byte| T::from_gos(&Value::Int(i64::from(*byte))))
                .collect(),
            other => type_err("[T]", other),
        }
    }
}

impl<T: ToGos> ToGos for Vec<T> {
    fn to_gos(self) -> Value {
        Value::Array(Arc::new(self.into_iter().map(ToGos::to_gos).collect()))
    }
}

/// Transparent newtype around `Vec<u8>` that the binding system
/// recognises as the [`crate::Type::Bytes`] shape.
///
/// Rust authors prefer this when their binding fn takes or returns a byte
/// payload; the signature then says `Type::Bytes` instead of
/// `Type::Vec(&Type::I64)`. The binding owns the buffer: an argument is a
/// copy that may outlive the call, and an answer is copied into the
/// caller's own buffer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    /// Wraps an existing buffer.
    #[must_use]
    pub const fn new(buf: Vec<u8>) -> Self {
        Self(buf)
    }

    /// Unwraps into the inner `Vec<u8>`.
    #[must_use]
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }

    /// Borrows the bytes as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Number of bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl From<&[u8]> for Bytes {
    fn from(s: &[u8]) -> Self {
        Self(s.to_vec())
    }
}

impl From<Bytes> for Vec<u8> {
    fn from(b: Bytes) -> Self {
        b.0
    }
}

impl std::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl FromGos for Bytes {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Bytes(bytes) => Ok(Bytes(bytes.clone())),
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items.iter() {
                    let Value::Int(i) = item else {
                        return type_err("Bytes element (u8)", item);
                    };
                    let byte = u8::try_from(*i).map_err(|_| {
                        RuntimeError::Type(format!("Bytes element out of u8 range: {i}"))
                    })?;
                    out.push(byte);
                }
                Ok(Bytes(out))
            }
            // A binding that declared Bytes and receives a String reads its
            // UTF-8 bytes.
            Value::String(s) => Ok(Bytes(s.as_bytes().to_vec())),
            other => type_err("Bytes", other),
        }
    }
}

impl ToGos for Bytes {
    fn to_gos(self) -> Value {
        Value::Bytes(self.0)
    }
}

// The HashMap impls are specialised to the default hasher: a binding
// receives a freshly built map, so a hasher parameter would only thread
// through every signature.

#[allow(
    clippy::implicit_hasher,
    reason = "ABI surface; bindings receive freshly-built HashMaps."
)]
impl<K, V> FromGos for HashMap<K, V>
where
    K: FromGos + std::hash::Hash + Eq,
    V: FromGos,
{
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Map(entries) => {
                let mut out = HashMap::with_capacity(entries.len());
                for (k, v) in entries.iter() {
                    out.insert(K::from_gos(k)?, V::from_gos(v)?);
                }
                Ok(out)
            }
            // A sequence of pairs builds a map too.
            Value::Array(items) => {
                let mut out = HashMap::with_capacity(items.len());
                for entry in items.iter() {
                    let pair = tuple_items(entry, 2, "Map<K, V> as [(K, V)]")?;
                    out.insert(K::from_gos(&pair[0])?, V::from_gos(&pair[1])?);
                }
                Ok(out)
            }
            other => type_err("Map<K, V>", other),
        }
    }
}

#[allow(
    clippy::implicit_hasher,
    reason = "ABI surface; bindings receive freshly-built HashMaps."
)]
impl<K, V> ToGos for HashMap<K, V>
where
    K: ToGos,
    V: ToGos,
{
    fn to_gos(self) -> Value {
        Value::Map(Arc::new(
            self.into_iter()
                .map(|(k, v)| (k.to_gos(), v.to_gos()))
                .collect(),
        ))
    }
}

/// Type-erased value used for binding fns whose answer's shape the data
/// decides (Redis RESP, Postgres typed columns, `OpenTelemetry` attribute
/// values).
///
/// A [`crate::Type::Variant`] answer is a `DynValue` whose tag picks the
/// arm. `DynValue::Tagged { name, .. }` should name one of the arms the
/// signature declares: the program matches on the enum that spells them,
/// and the boundary does not validate the payload against the declaration.
#[derive(Debug, Clone, PartialEq)]
pub enum DynValue {
    /// `Nil` / absent value.
    Nil,
    /// `bool`.
    Bool(bool),
    /// `i64`.
    Int(i64),
    /// `f64`.
    Float(f64),
    /// `char`.
    Char(char),
    /// `String`.
    String(String),
    /// Byte buffer.
    Bytes(Vec<u8>),
    /// Heterogeneous list.
    List(Vec<DynValue>),
    /// Heterogeneous key/value pairs (preserves insertion order).
    Map(Vec<(DynValue, DynValue)>),
    /// Tagged arm - name + positional payload.
    Tagged {
        /// Variant arm name.
        name: String,
        /// Positional payload values.
        payload: Vec<DynValue>,
    },
}

impl DynValue {
    /// Returns `true` when `self` is the `Nil` sentinel.
    #[must_use]
    pub fn is_nil(&self) -> bool {
        matches!(self, Self::Nil)
    }
}

impl FromGos for DynValue {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        Ok(value_to_dyn(value))
    }
}

impl ToGos for DynValue {
    fn to_gos(self) -> Value {
        dyn_to_value(self)
    }
}

fn value_to_dyn(value: &Value) -> DynValue {
    match value {
        Value::Unit | Value::Callback(_) => DynValue::Nil,
        Value::Bool(b) => DynValue::Bool(*b),
        Value::Int(i) => DynValue::Int(*i),
        Value::Float(f) => DynValue::Float(*f),
        Value::Char(c) => DynValue::Char(*c),
        Value::String(s) => DynValue::String(s.as_str().to_string()),
        Value::Bytes(bytes) => DynValue::Bytes(bytes.clone()),
        // A vector of integers that all fit a byte reads as bytes, as the
        // compiled wire reads the same vector.
        Value::Array(items) if items.iter().all(|v| matches!(v, Value::Int(0..=255))) => {
            DynValue::Bytes(
                items
                    .iter()
                    .filter_map(|v| match v {
                        Value::Int(i) => u8::try_from(*i).ok(),
                        _ => None,
                    })
                    .collect(),
            )
        }
        Value::Array(items) | Value::Tuple(items) => {
            DynValue::List(items.iter().map(value_to_dyn).collect())
        }
        Value::Map(entries) => DynValue::Map(
            entries
                .iter()
                .map(|(k, v)| (value_to_dyn(k), value_to_dyn(v)))
                .collect(),
        ),
        Value::Variant(inner) => DynValue::Tagged {
            name: inner.name.clone(),
            payload: inner.fields.iter().map(value_to_dyn).collect(),
        },
        // A struct reads as the tagged arm the compiled wire spells it as:
        // its name, then its fields in declaration order.
        Value::Struct(inner) => DynValue::Tagged {
            name: inner.name.clone(),
            payload: inner.fields.iter().map(|(_, v)| value_to_dyn(v)).collect(),
        },
    }
}

fn dyn_to_value(d: DynValue) -> Value {
    match d {
        DynValue::Nil => Value::Unit,
        DynValue::Bool(b) => Value::Bool(b),
        DynValue::Int(i) => Value::Int(i),
        DynValue::Float(f) => Value::Float(f),
        DynValue::Char(c) => Value::Char(c),
        DynValue::String(s) => Value::String(Str::from(s)),
        DynValue::Bytes(buf) => Value::Bytes(buf),
        DynValue::List(items) => {
            Value::Array(Arc::new(items.into_iter().map(dyn_to_value).collect()))
        }
        DynValue::Map(entries) => Value::Map(Arc::new(
            entries
                .into_iter()
                .map(|(k, v)| (dyn_to_value(k), dyn_to_value(v)))
                .collect(),
        )),
        DynValue::Tagged { name, payload } => {
            Value::variant(name, payload.into_iter().map(dyn_to_value).collect())
        }
    }
}

/// Handle to a Gossamer-side callable the binding may invoke during its
/// call.
///
/// **Call-scoped**: the handle is valid only while the binding fn that
/// received it runs. A binding that keeps a callable past its call takes a
/// [`PersistentCallback`] instead.
#[derive(Debug, Clone)]
pub struct BindingCallback {
    inner: Value,
}

impl BindingCallback {
    /// Wraps a callable value.
    #[must_use]
    pub fn from_value(v: Value) -> Self {
        Self { inner: v }
    }

    /// A callable a compiled caller registered under `handle`.
    #[must_use]
    pub fn from_handle(handle: u64) -> Self {
        Self {
            inner: Value::Callback(CallbackRef {
                handle,
                origin: CallbackOrigin::Compiled,
            }),
        }
    }

    /// The handle the caller registered the callable under.
    #[must_use]
    pub fn handle(&self) -> Option<u64> {
        match self.inner {
            Value::Callback(callback) => Some(callback.handle),
            _ => None,
        }
    }

    /// Consumes `self`, answering the callable value.
    #[must_use]
    pub fn into_value(self) -> Value {
        self.inner
    }

    /// Calls the callable with `args` through `dispatch`.
    pub fn invoke(
        &self,
        dispatch: &mut dyn NativeDispatch,
        args: Vec<Value>,
    ) -> RuntimeResult<Value> {
        match self.inner {
            Value::Callback(callback) => dispatch.invoke_callback(callback, args),
            ref other => type_err("Fn(...)", other),
        }
    }

    /// Borrows the callable value.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.inner
    }
}

impl FromGos for BindingCallback {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Callback(_) => Ok(Self::from_value(value.clone())),
            other => type_err("Fn(...)", other),
        }
    }
}

impl ToGos for BindingCallback {
    fn to_gos(self) -> Value {
        self.inner
    }
}

/// A Gossamer-side callable a binding keeps past the call that received
/// it, for a registry, an event loop, or a later call.
///
/// Each `PersistentCallback` holds a share of its handle: the callable
/// stays alive until every clone is dropped (or [`Self::release`]d). A
/// binding that stores callables on a long-lived registry should publish a
/// Gossamer-side "release" function so programs can let them go.
#[derive(Debug)]
pub struct PersistentCallback {
    callback: CallbackRef,
}

impl PersistentCallback {
    /// Takes a share of `callback`.
    #[must_use]
    pub fn new(callback: CallbackRef) -> Self {
        retain(callback);
        Self { callback }
    }

    /// Takes a share of the callable a compiled caller registered under
    /// `handle`.
    #[must_use]
    pub fn from_handle(handle: u64) -> Self {
        Self::new(CallbackRef {
            handle,
            origin: CallbackOrigin::Compiled,
        })
    }

    /// The handle the caller registered the callable under.
    #[must_use]
    pub fn handle(&self) -> Option<u64> {
        Some(self.callback.handle)
    }

    /// Calls the callable with `args` through `dispatch`.
    pub fn invoke(
        &self,
        dispatch: &mut dyn NativeDispatch,
        args: Vec<Value>,
    ) -> RuntimeResult<Value> {
        dispatch.invoke_callback(self.callback, args)
    }

    /// The callable value.
    #[must_use]
    pub fn as_value(&self) -> Value {
        Value::Callback(self.callback)
    }

    /// Gives back this share of the callable.
    pub fn release(self) {
        drop(self);
    }
}

fn retain(callback: CallbackRef) {
    match callback.origin {
        CallbackOrigin::Vm => crate::host::vm_callback_retain(callback.handle),
        CallbackOrigin::Compiled => crate::host::callback_retain(callback.handle),
    }
}

impl Clone for PersistentCallback {
    fn clone(&self) -> Self {
        Self::new(self.callback)
    }
}

impl Drop for PersistentCallback {
    fn drop(&mut self) {
        match self.callback.origin {
            CallbackOrigin::Vm => crate::host::vm_callback_release(self.callback.handle),
            CallbackOrigin::Compiled => crate::host::callback_release(self.callback.handle),
        }
    }
}

impl FromGos for PersistentCallback {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Callback(callback) => Ok(Self::new(*callback)),
            other => type_err("Fn(...) [persistent]", other),
        }
    }
}

impl ToGos for PersistentCallback {
    fn to_gos(self) -> Value {
        self.as_value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_i64() {
        let v: Value = 42_i64.to_gos();
        assert_eq!(i64::from_gos(&v).unwrap(), 42);
    }

    #[test]
    fn round_trip_string() {
        let v: Value = "hello".to_gos();
        assert_eq!(String::from_gos(&v).unwrap(), "hello");
    }

    #[test]
    fn round_trip_option_some_none() {
        let v: Value = Some(5_i64).to_gos();
        assert_eq!(Option::<i64>::from_gos(&v).unwrap(), Some(5));
        let v: Value = Option::<i64>::None.to_gos();
        assert_eq!(Option::<i64>::from_gos(&v).unwrap(), None);
    }

    #[test]
    fn round_trip_result_ok_err() {
        let v: Value = Ok::<_, String>(7_i64).to_gos();
        assert_eq!(Result::<i64, String>::from_gos(&v).unwrap(), Ok(7));
        let v: Value = Err::<i64, _>("bad".to_string()).to_gos();
        assert_eq!(
            Result::<i64, String>::from_gos(&v).unwrap(),
            Err("bad".to_string())
        );
    }

    #[test]
    fn round_trip_vec() {
        let v: Value = vec![1_i64, 2, 3].to_gos();
        assert_eq!(Vec::<i64>::from_gos(&v).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn type_mismatch_returns_typed_error() {
        let v: Value = "hello".to_gos();
        let err = i64::from_gos(&v).unwrap_err();
        assert!(matches!(err, RuntimeError::Type(_)));
    }

    #[test]
    fn a_narrow_integer_out_of_range_is_a_type_error() {
        assert_eq!(u8::from_gos(&Value::Int(255)).unwrap(), 255);
        assert!(matches!(
            u8::from_gos(&Value::Int(256)),
            Err(RuntimeError::Type(_))
        ));
    }

    #[test]
    fn bytes_to_gos_then_from() {
        let payload: Vec<u8> = b"hello world".to_vec();
        let v: Value = Bytes::new(payload.clone()).to_gos();
        let back = Bytes::from_gos(&v).unwrap();
        assert_eq!(back.as_slice(), payload.as_slice());
    }

    #[test]
    fn bytes_accepts_string() {
        let v: Value = "hello".to_gos();
        let back = Bytes::from_gos(&v).unwrap();
        assert_eq!(back.as_slice(), b"hello");
    }

    #[test]
    fn bytes_rejects_oversize_element() {
        let v = Value::Array(Arc::new(vec![Value::Int(300)]));
        let err = Bytes::from_gos(&v).unwrap_err();
        assert!(matches!(err, RuntimeError::Type(_)));
    }

    #[test]
    fn hash_map_string_string_round_trip() {
        let mut m: HashMap<String, String> = HashMap::new();
        m.insert("a".into(), "1".into());
        m.insert("b".into(), "2".into());
        let v: Value = m.clone().to_gos();
        let back: HashMap<String, String> = HashMap::from_gos(&v).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn hash_map_accepts_array_of_tuples() {
        let entries = vec![
            Value::Tuple(Arc::new(vec!["k1".to_gos(), Value::Int(1)])),
            Value::Tuple(Arc::new(vec!["k2".to_gos(), Value::Int(2)])),
        ];
        let v = Value::Array(Arc::new(entries));
        let back: HashMap<String, i64> = HashMap::from_gos(&v).unwrap();
        assert_eq!(back.get("k1"), Some(&1));
        assert_eq!(back.get("k2"), Some(&2));
    }

    #[test]
    fn dyn_value_tagged_round_trip() {
        let original = DynValue::Tagged {
            name: "Integer".to_string(),
            payload: vec![DynValue::Int(7)],
        };
        let v: Value = original.clone().to_gos();
        assert_eq!(DynValue::from_gos(&v).unwrap(), original);
    }

    #[test]
    fn dyn_value_list_of_mixed() {
        let original = DynValue::List(vec![
            DynValue::Int(1),
            DynValue::String("two".to_string()),
            DynValue::Bool(true),
            DynValue::Bytes(b"hi".to_vec()),
        ]);
        let v: Value = original.clone().to_gos();
        assert_eq!(DynValue::from_gos(&v).unwrap(), original);
    }

    #[test]
    fn a_vector_of_byte_sized_integers_reads_as_bytes() {
        let small = Value::Array(Arc::new(vec![Value::Int(1), Value::Int(255)]));
        assert_eq!(
            DynValue::from_gos(&small).unwrap(),
            DynValue::Bytes(vec![1, 255])
        );
        let wide = Value::Array(Arc::new(vec![Value::Int(1), Value::Int(256)]));
        assert_eq!(
            DynValue::from_gos(&wide).unwrap(),
            DynValue::List(vec![DynValue::Int(1), DynValue::Int(256)])
        );
    }

    #[test]
    fn a_struct_reads_as_its_tagged_fields() {
        let v = Value::struct_(
            "Point",
            vec![("x".into(), Value::Int(1)), ("y".into(), Value::Int(2))],
        );
        assert_eq!(
            DynValue::from_gos(&v).unwrap(),
            DynValue::Tagged {
                name: "Point".to_string(),
                payload: vec![DynValue::Int(1), DynValue::Int(2)],
            }
        );
    }

    #[test]
    fn binding_callback_rejects_non_callable() {
        let v: Value = 7_i64.to_gos();
        assert!(matches!(
            BindingCallback::from_gos(&v),
            Err(RuntimeError::Type(_))
        ));
        assert!(matches!(
            PersistentCallback::from_gos(&v),
            Err(RuntimeError::Type(_))
        ));
    }
}
