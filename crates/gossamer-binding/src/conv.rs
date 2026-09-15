//! Conversions between Gossamer [`Value`]s and Rust types.
//!
//! [`FromGos`] marshals an argument out of `&[Value]`; [`ToGos`]
//! boxes a return value back into a `Value`. The
//! `register_module!` macro derives the call-site wrappers from
//! these traits, so binding authors write idiomatic Rust
//! signatures and never touch `Value` directly.

use std::collections::HashMap;
use std::sync::Arc;

use gossamer_interp::value::{
    MapKey, RuntimeError, RuntimeResult, SmolStr, Value, dense_map_with_capacity,
};

/// Materialises a typed Rust value out of a Gossamer [`Value`].
pub trait FromGos: Sized {
    /// Performs the conversion or returns a typed `RuntimeError`.
    fn from_gos(value: &Value) -> RuntimeResult<Self>;
}

/// Boxes a Rust value into a Gossamer [`Value`].
pub trait ToGos {
    /// Performs the conversion (infallible - panics on
    /// representation overflow, which can only happen if a
    /// binding violates its declared signature).
    fn to_gos(self) -> Value;
}

fn type_err<T>(expected: &str, found: &Value) -> RuntimeResult<T> {
    Err(RuntimeError::Type(format!(
        "expected {expected}, found {}",
        describe(found)
    )))
}

/// `describe` for a value that must be cloned out of a lock first.
fn describe_owned(v: &Value) -> &'static str {
    describe(v)
}

fn describe(v: &Value) -> &'static str {
    match v {
        Value::NativeEnum(o) => o.shape.enum_name,
        // A capture cell is transparent: it describes whatever it holds.
        Value::CaptureCell(cell) => describe_owned(&cell.lock().clone()),
        Value::Unit => "()",
        Value::Bool(_) => "bool",
        Value::Int(_) => "i64",
        Value::Float(_) => "f64",
        Value::Char(_) => "char",
        Value::String(_) => "String",
        Value::Json(_) => "json",
        Value::Tuple(_) => "tuple",
        Value::Array(_)
        | Value::IntArray(_)
        | Value::ByteArray(_)
        | Value::InlineByteArray(_)
        | Value::ByteVec(_)
        | Value::FloatVec(_)
        | Value::FloatArray(_) => "vec",
        Value::Variant(_) => "enum variant",
        Value::Struct(_) => "struct",
        Value::Closure(_) | Value::Builtin(_) | Value::Native(_) => "callable",
        Value::Channel(_) => "channel",
        Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_) => "map",
        Value::LazyIter(_) => "iterator",
        Value::Weak(_) => "weak",
        Value::Void => "void",
        Value::Uint(_) => "u64",
        // &mut-param writeback cell; never escapes the call protocol,
        // so reaching a binding boundary means describing its payload.
        Value::MutCell(_) => "mut-ref cell",
    }
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

impl FromGos for u64 {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        let i = i64::from_gos(value)?;
        u64::try_from(i).map_err(|_| RuntimeError::Type(format!("expected u64, found {i}")))
    }
}

impl ToGos for u64 {
    fn to_gos(self) -> Value {
        Value::Int(i64::try_from(self).unwrap_or(i64::MAX))
    }
}

impl FromGos for usize {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        let i = i64::from_gos(value)?;
        usize::try_from(i).map_err(|_| RuntimeError::Type(format!("expected usize, found {i}")))
    }
}

impl ToGos for usize {
    fn to_gos(self) -> Value {
        Value::Int(i64::try_from(self).unwrap_or(i64::MAX))
    }
}

impl FromGos for u16 {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        let i = i64::from_gos(value)?;
        u16::try_from(i).map_err(|_| RuntimeError::Type(format!("expected u16, found {i}")))
    }
}

impl ToGos for u16 {
    fn to_gos(self) -> Value {
        Value::Int(i64::from(self))
    }
}

impl FromGos for u8 {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        let i = i64::from_gos(value)?;
        u8::try_from(i).map_err(|_| RuntimeError::Type(format!("expected u8, found {i}")))
    }
}

impl ToGos for u8 {
    fn to_gos(self) -> Value {
        Value::Int(i64::from(self))
    }
}

impl FromGos for f64 {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Float(f) => Ok(*f),
            // Lossy for |i| > 2^53; user code that passes such an
            // i64 to an f64 binding param has accepted that, the
            // explicit conversion just makes it visible.
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
        Value::String(SmolStr::from_string(self))
    }
}

impl ToGos for &str {
    fn to_gos(self) -> Value {
        Value::String(SmolStr::from_str(self))
    }
}

impl<T: FromGos> FromGos for Option<T> {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Variant(inner) => {
                if inner.name == "None" {
                    Ok(None)
                } else if inner.name == "Some" {
                    let payload = inner
                        .fields
                        .first()
                        .ok_or_else(|| RuntimeError::Type("Some(_) without payload".to_string()))?;
                    Ok(Some(T::from_gos(payload)?))
                } else {
                    type_err("Option<T>", value)
                }
            }
            other => type_err("Option<T>", other),
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
        match value {
            Value::Variant(inner) => {
                let first = inner.fields.first().ok_or_else(|| {
                    RuntimeError::Type("Result<_, _> without payload".to_string())
                })?;
                if inner.name == "Ok" {
                    Ok(Ok(T::from_gos(first)?))
                } else if inner.name == "Err" {
                    Ok(Err(E::from_gos(first)?))
                } else {
                    type_err("Result<T, E>", value)
                }
            }
            other => type_err("Result<T, E>", other),
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

// --- Tuple FromGos / ToGos (Phase 1) ----------------------------------
//
// Materialises a fixed-arity Rust tuple from a `Value::Tuple`
// payload. Round-trips through the same `Arc<Vec<Value>>` carrier
// the existing `Vec<T>` impl uses, so user code that produces a
// Gossamer tuple via the language's tuple literal flows directly
// into the binding param.

impl<A: FromGos, B: FromGos> FromGos for (A, B) {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Tuple(arc) | Value::Array(arc) => {
                if arc.len() != 2 {
                    return Err(RuntimeError::Type(format!(
                        "expected 2-tuple, found {}-element list",
                        arc.len()
                    )));
                }
                Ok((A::from_gos(&arc[0])?, B::from_gos(&arc[1])?))
            }
            other => type_err("(A, B)", other),
        }
    }
}

impl<A: ToGos, B: ToGos> ToGos for (A, B) {
    fn to_gos(self) -> Value {
        Value::Tuple(Arc::new(vec![self.0.to_gos(), self.1.to_gos()]))
    }
}

impl<A: FromGos, B: FromGos, C: FromGos> FromGos for (A, B, C) {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Tuple(arc) | Value::Array(arc) => {
                if arc.len() != 3 {
                    return Err(RuntimeError::Type(format!(
                        "expected 3-tuple, found {}-element list",
                        arc.len()
                    )));
                }
                Ok((
                    A::from_gos(&arc[0])?,
                    B::from_gos(&arc[1])?,
                    C::from_gos(&arc[2])?,
                ))
            }
            other => type_err("(A, B, C)", other),
        }
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
        match value {
            Value::Tuple(arc) | Value::Array(arc) => {
                if arc.len() != 4 {
                    return Err(RuntimeError::Type(format!(
                        "expected 4-tuple, found {}-element list",
                        arc.len()
                    )));
                }
                Ok((
                    A::from_gos(&arc[0])?,
                    B::from_gos(&arc[1])?,
                    C::from_gos(&arc[2])?,
                    D::from_gos(&arc[3])?,
                ))
            }
            other => type_err("(A, B, C, D)", other),
        }
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
        let items: &[Value] = match value {
            Value::Array(arc) | Value::Tuple(arc) => arc.as_slice(),
            Value::IntArray(arc) => {
                return arc.iter().map(|i| T::from_gos(&Value::Int(*i))).collect();
            }
            Value::ByteArray(arc) => {
                return arc
                    .iter()
                    .map(|byte| T::from_gos(&Value::Int(i64::from(*byte))))
                    .collect();
            }
            Value::InlineByteArray(arc) => {
                return arc
                    .iter()
                    .map(|byte| T::from_gos(&Value::Int(i64::from(*byte))))
                    .collect();
            }
            Value::ByteVec(arc) => {
                return arc
                    .iter()
                    .map(|byte| T::from_gos(&Value::Int(i64::from(*byte))))
                    .collect();
            }
            other => return type_err("[T]", other),
        };
        items.iter().map(T::from_gos).collect()
    }
}

impl<T: ToGos> ToGos for Vec<T> {
    fn to_gos(self) -> Value {
        let items: Vec<Value> = self.into_iter().map(ToGos::to_gos).collect();
        Value::Array(Arc::new(items))
    }
}

// --- ABI 0.4: Bytes ----------------------------------------------------

/// Transparent newtype around `Vec<u8>` that the binding system
/// recognises as the [`crate::Type::Bytes`] shape.
///
/// Rust authors prefer this when their binding fn takes or
/// returns a byte payload; the macro picks `Type::Bytes` for the
/// signature instead of `Type::Vec(&Type::I64)`.
///
/// # ABI invariants
///
/// - **Ownership**: the binding owns the inner `Vec<u8>`. Returned
///   from a binding fn, the bytes are copied into a fresh
///   `Value::IntArray` (interp tier) or a fresh
///   [`crate::native::GosBytes`] (compiled tier). The original
///   `Vec` is moved into the conversion and then dropped.
/// - **Lifetime**: a `Bytes` materialised via [`FromGos`] /
///   [`crate::native::BindingAbi::from_input`] outlives the
///   binding call. Binding authors may keep it across goroutine
///   boundaries (it is `Send + Sync`).
/// - **Pinning**: the underlying buffer is heap-allocated and
///   does NOT pin; binding authors must not pass the inner
///   `&[u8]` to FFI consumers that require a stable address
///   beyond the borrow.
/// - **GC**: the buffer is GC-tracked when stored as a
///   `Value::IntArray` (interp tier) - the same as `Vec<i64>`.
///   On the compiled tier, the [`crate::native::GosBytes`]
///   header lives on the arena and is reclaimed at the next
///   `gos_rt_gc_reset` tick.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    /// Wraps an existing buffer.
    #[must_use]
    pub const fn new(buf: Vec<u8>) -> Self {
        Self(buf)
    }

    /// Consumes `self` and returns the inner buffer.
    #[must_use]
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }

    /// Returns a borrow of the inner buffer.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Returns the buffer length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if the buffer is empty.
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
            Value::ByteArray(arc) => Ok(Bytes(arc.to_vec())),
            Value::InlineByteArray(arc) => Ok(Bytes(arc.to_vec())),
            Value::ByteVec(arc) => Ok(Bytes(arc.as_ref().clone())),
            Value::IntArray(arc) => {
                let mut out = Vec::with_capacity(arc.len());
                for v in arc.iter() {
                    let b = u8::try_from(*v).map_err(|_| {
                        RuntimeError::Type(format!("Bytes element out of u8 range: {v}"))
                    })?;
                    out.push(b);
                }
                Ok(Bytes(out))
            }
            Value::Array(arc) => {
                let mut out = Vec::with_capacity(arc.len());
                for v in arc.iter() {
                    let i = match v {
                        Value::Int(i) => *i,
                        Value::Uint(u) => i64::try_from(*u).unwrap_or(i64::MAX),
                        other => return type_err("Bytes element (u8)", other),
                    };
                    let b = u8::try_from(i).map_err(|_| {
                        RuntimeError::Type(format!("Bytes element out of u8 range: {i}"))
                    })?;
                    out.push(b);
                }
                Ok(Bytes(out))
            }
            // Common ergonomic: bindings that get a String where
            // Bytes was declared receive its UTF-8 bytes.
            Value::String(s) => Ok(Bytes(s.as_str().as_bytes().to_vec())),
            other => type_err("Bytes", other),
        }
    }
}

impl ToGos for Bytes {
    fn to_gos(self) -> Value {
        Value::ByteVec(Arc::new(self.0))
    }
}

// --- ABI 0.4: HashMap --------------------------------------------------
//
// The HashMap impls below are intentionally specialised to
// `RandomState` (the default hasher). The ABI shape is fixed and
// the binding receives a freshly-constructed HashMap; we don't
// want to thread a hasher type parameter through every binding
// signature. The `implicit_hasher` lint is allowed on each impl
// with this rationale.

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
            Value::Map(m) => {
                let guard = m.lock();
                let mut out = HashMap::with_capacity(guard.len());
                for (k, v) in guard.iter() {
                    let key_val = map_key_to_value(k);
                    out.insert(K::from_gos(&key_val)?, V::from_gos(v)?);
                }
                Ok(out)
            }
            Value::IntMap(m) => {
                let guard = m.lock();
                let mut out = HashMap::with_capacity(guard.len());
                for (k, v) in guard.iter() {
                    out.insert(K::from_gos(&Value::Int(*k))?, V::from_gos(&Value::Int(*v))?);
                }
                Ok(out)
            }
            // Accept Vec<(K, V)> as a builder-friendly alias.
            Value::Array(arr) => {
                let mut out = HashMap::with_capacity(arr.len());
                for entry in arr.iter() {
                    let Value::Tuple(pair) = entry else {
                        return type_err("Map<K, V> as [(K, V)]", entry);
                    };
                    if pair.len() != 2 {
                        return Err(RuntimeError::Type(
                            "Map<K, V> tuple entry must have 2 elements".to_string(),
                        ));
                    }
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
    K: ToGos + Clone,
    V: ToGos,
{
    fn to_gos(self) -> Value {
        let mut out = dense_map_with_capacity(self.len());
        for (k, v) in self {
            let key_value = k.to_gos();
            let key = value_to_map_key(&key_value);
            out.insert(key, v.to_gos());
        }
        Value::Map(Arc::new(parking_lot::Mutex::new(out)))
    }
}

fn map_key_to_value(k: &MapKey) -> Value {
    match k {
        MapKey::NonHashable => Value::Unit,
        MapKey::Bool(b) => Value::Bool(*b),
        MapKey::Int(i) => Value::Int(*i),
        // A rendered map's unsigned key carries the same bits the live map
        // keyed by, so it converts back to that integer.
        MapKey::Uint(n) => Value::Int(*n as i64),
        MapKey::Char(c) => Value::Char(*c),
        // A float key holds the value's bit pattern, so it reads back as the
        // float those bits spell.
        MapKey::Float(bits) => Value::Float(f64::from_bits(*bits)),
        MapKey::Str(s) => Value::String(s.clone()),
        // Aggregate keys don't round-trip to their typed shape (field names /
        // element types aren't retained in the key).
        MapKey::Agg(..) => Value::Unit,
    }
}

fn value_to_map_key(v: &Value) -> MapKey {
    match v {
        Value::Bool(b) => MapKey::Bool(*b),
        Value::Int(i) => MapKey::Int(*i),
        Value::Uint(u) => MapKey::Int(i64::try_from(*u).unwrap_or(i64::MAX)),
        Value::Char(c) => MapKey::Char(*c),
        Value::Float(f) => MapKey::Float(f.to_bits()),
        Value::String(s) => MapKey::Str(s.clone()),
        _ => MapKey::NonHashable,
    }
}

// --- ABI 0.4: DynValue (tagged-union returns) --------------------------

/// Type-erased value used for binding fns whose return shape is
/// dynamically variant (Redis RESP, Postgres typed columns,
/// `OpenTelemetry` attribute values).
///
/// Bindings authoring a [`crate::Type::Variant`] return hand back
/// a `DynValue` whose runtime tag picks the live arm. The
/// macro-generated thunk routes it to a [`Value::Variant`] (interp
/// tier) or a [`crate::native::GosVariant`] (compiled tier).
///
/// # Invariants
///
/// - `DynValue::Tagged { name, payload }` must declare a `name`
///   that matches one of the [`crate::types::VariantArm::name`]s
///   in the binding signature. Names mismatching the declaration
///   are accepted at the boundary but the type checker will not
///   know about them; downstream Gossamer code will pattern-match
///   on the literal arm name string.
/// - All payload elements in a `Tagged` arm carry their concrete
///   runtime type; the boundary does not validate them against
///   the declared payload types. Authors are responsible for
///   matching their declared signature.
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
        Value::NativeEnum(o) => value_to_dyn(&gossamer_interp::native_enum_to_variant(o)),
        // &mut-param writeback cell; convert through its current payload.
        Value::MutCell(cell) => value_to_dyn(&cell.lock().clone()),
        // A closure capture cell is transparent to conversion.
        Value::CaptureCell(cell) => value_to_dyn(&cell.lock().clone()),
        Value::Unit | Value::Void | Value::Weak(_) => DynValue::Nil,
        Value::Bool(b) => DynValue::Bool(*b),
        Value::Int(i) => DynValue::Int(*i),
        Value::Uint(u) => DynValue::Int(i64::try_from(*u).unwrap_or(i64::MAX)),
        Value::Float(f) => DynValue::Float(*f),
        Value::Char(c) => DynValue::Char(*c),
        Value::String(s) => DynValue::String(s.as_str().to_string()),
        Value::Json(json) => json_to_dyn(json.as_value()),
        Value::IntArray(arc) => {
            // Heuristic: an IntArray whose every element is in
            // `u8` range is treated as Bytes (the natural Gossamer
            // representation for byte payloads). Otherwise it's a
            // List of Ints.
            if arc.iter().all(|v| (0..=255).contains(v)) {
                DynValue::Bytes(arc.iter().map(|v| *v as u8).collect())
            } else {
                DynValue::List(arc.iter().map(|v| DynValue::Int(*v)).collect())
            }
        }
        Value::ByteArray(arc) => DynValue::Bytes(arc.to_vec()),
        Value::InlineByteArray(arc) => DynValue::Bytes(arc.to_vec()),
        Value::ByteVec(arc) => DynValue::Bytes(arc.as_ref().clone()),
        Value::FloatVec(arc) => DynValue::List(arc.iter().map(|v| DynValue::Float(*v)).collect()),
        Value::Array(arc) | Value::Tuple(arc) => {
            DynValue::List(arc.iter().map(value_to_dyn).collect())
        }
        Value::FloatArray(_) => DynValue::Nil,
        Value::Map(m) => {
            let guard = m.lock();
            DynValue::Map(
                guard
                    .iter()
                    .map(|(k, v)| (value_to_dyn(&map_key_to_value(k)), value_to_dyn(v)))
                    .collect(),
            )
        }
        Value::IntMap(m) => {
            let guard = m.lock();
            DynValue::Map(
                guard
                    .iter()
                    .map(|(k, v)| (DynValue::Int(*k), DynValue::Int(*v)))
                    .collect(),
            )
        }
        Value::StrIntMap(m) => {
            let guard = m.lock();
            DynValue::Map(
                guard
                    .iter()
                    .map(|(k, v)| (DynValue::String(k.as_str().to_string()), DynValue::Int(*v)))
                    .collect(),
            )
        }
        Value::Variant(inner) => DynValue::Tagged {
            name: inner.name.to_string(),
            payload: inner.fields.iter().map(value_to_dyn).collect(),
        },
        Value::Struct(_)
        | Value::Closure(_)
        | Value::Builtin(_)
        | Value::Native(_)
        | Value::Channel(_)
        | Value::LazyIter(_) => DynValue::Nil,
    }
}

fn json_to_dyn(value: &gossamer_std::json::Value) -> DynValue {
    match value {
        gossamer_std::json::Value::Null => DynValue::Nil,
        gossamer_std::json::Value::Bool(b) => DynValue::Bool(*b),
        gossamer_std::json::Value::Int(i) => DynValue::Int(*i),
        // `DynValue` carries integers as `i64`, the way `value_to_dyn` hands
        // over an interpreter `Uint`.
        gossamer_std::json::Value::Uint(u) => DynValue::Int(i64::try_from(*u).unwrap_or(i64::MAX)),
        gossamer_std::json::Value::Number(f) => DynValue::Float(*f),
        gossamer_std::json::Value::String(s) => DynValue::String(s.clone()),
        gossamer_std::json::Value::Array(items) => {
            DynValue::List(items.iter().map(json_to_dyn).collect())
        }
        gossamer_std::json::Value::Object(entries) => DynValue::Map(
            entries
                .iter()
                .map(|(key, value)| (DynValue::String(key.clone()), json_to_dyn(value)))
                .collect(),
        ),
    }
}

fn dyn_to_value(d: DynValue) -> Value {
    match d {
        DynValue::Nil => Value::Unit,
        DynValue::Bool(b) => Value::Bool(b),
        DynValue::Int(i) => Value::Int(i),
        DynValue::Float(f) => Value::Float(f),
        DynValue::Char(c) => Value::Char(c),
        DynValue::String(s) => Value::String(SmolStr::from_string(s)),
        DynValue::Bytes(buf) => {
            let widened: Vec<i64> = buf.into_iter().map(i64::from).collect();
            Value::IntArray(Arc::new(widened))
        }
        DynValue::List(items) => {
            let inner: Vec<Value> = items.into_iter().map(dyn_to_value).collect();
            Value::Array(Arc::new(inner))
        }
        DynValue::Map(entries) => {
            let mut out = dense_map_with_capacity(entries.len());
            for (k, v) in entries {
                let key_value = dyn_to_value(k);
                let key = value_to_map_key(&key_value);
                out.insert(key, dyn_to_value(v));
            }
            Value::Map(Arc::new(parking_lot::Mutex::new(out)))
        }
        DynValue::Tagged { name, payload } => {
            let fields: Vec<Value> = payload.into_iter().map(dyn_to_value).collect();
            // We need a `&'static str` for the variant name slot;
            // intern it through a process-global string pool.
            Value::variant(intern_arm_name(&name), fields)
        }
    }
}

// `Value::variant` takes `&'static str` for the arm name. The
// binding can produce arbitrary runtime strings via
// `DynValue::Tagged { name, .. }`, so we intern them in a
// process-global pool.
//
// the pool is **bounded at
// `INTERN_ARM_NAME_LIMIT = 1024`** entries. The previous design
// was a one-way unbounded `Box::leak`; a binding that returned
// `DynValue::Tagged { name: format!("Item-{n}"), .. }` from a
// loop would have OOM'd the process. Past the cap, the function
// returns a static `"<arm-name-pool-exhausted>"` sentinel and
// eprintln's a clear diagnostic on the first overflow so the
// binding author can switch to a stable arm-name set.
const INTERN_ARM_NAME_LIMIT: usize = 1024;

fn intern_arm_name(name: &str) -> &'static str {
    use parking_lot::RwLock;
    use std::collections::HashMap;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    static POOL: OnceLock<RwLock<HashMap<String, &'static str>>> = OnceLock::new();
    static WARNED: AtomicBool = AtomicBool::new(false);
    let pool = POOL.get_or_init(|| RwLock::new(HashMap::new()));
    if let Some(s) = pool.read().get(name) {
        return s;
    }
    let mut guard = pool.write();
    if let Some(s) = guard.get(name) {
        return s;
    }
    if guard.len() >= INTERN_ARM_NAME_LIMIT {
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "gossamer-binding: intern_arm_name pool reached its {INTERN_ARM_NAME_LIMIT}-entry \
                cap. Subsequent unseen arm names return the `<arm-name-pool-exhausted>` \
                sentinel. Variant arm names must be a small, stable set - bindings that \
                synthesise names dynamically (e.g. `format(\"Item-{{n}}\")`) leak \
                unboundedly. Switch to `Type::Opaque` or a stable name set."
            );
        }
        return "<arm-name-pool-exhausted>";
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    guard.insert(name.to_string(), leaked);
    leaked
}

// --- ABI 0.4: BindingCallback (Gossamer-side callable) -----------------

/// Handle to a Gossamer-side callable the binding may invoke
/// during its call.
///
/// # Lifetime
///
/// **Call-scoped.** The handle is valid only for the duration of
/// the binding fn that received it. Storing it past the return
/// (e.g. spawning a background goroutine that later invokes it)
/// is forbidden and will yield a typed runtime error on
/// `invoke()`. Long-lived callbacks ride on `Type::Opaque` with a
/// binding-specific registry.
///
/// # Coroutine safety
///
/// `invoke` re-enters the interpreter through the same
/// [`crate::NativeDispatch`] the binding received. The
/// interpreter's preemption + scheduler integration handles
/// goroutine yielding inside the callback. Re-entrancy depth is
/// bounded only by the interpreter's call-stack limit.
#[derive(Debug, Clone)]
pub struct BindingCallback {
    inner: Value,
}

impl BindingCallback {
    /// Builds a `BindingCallback` from the underlying [`Value`].
    /// Used by the macro-generated thunk; binding authors should
    /// not call this directly.
    #[must_use]
    pub fn from_value(v: Value) -> Self {
        Self { inner: v }
    }

    /// Returns the wrapped callable, useful for stashing into an
    /// opaque registry.
    #[must_use]
    pub fn into_value(self) -> Value {
        self.inner
    }

    /// Invokes the callback with `args`, re-entering the
    /// interpreter through `dispatch`. The interp side blocks
    /// until the callback returns; goroutine yielding inside the
    /// callback is handled by the interpreter's scheduler hooks.
    pub fn invoke(
        &self,
        dispatch: &mut dyn gossamer_interp::value::NativeDispatch,
        args: Vec<Value>,
    ) -> RuntimeResult<Value> {
        dispatch.call_value(&self.inner, args)
    }

    /// Returns a reference to the wrapped callable for
    /// inspection. Binding authors should prefer `invoke`.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.inner
    }
}

impl FromGos for BindingCallback {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Closure(_) | Value::Native(_) | Value::Builtin(_) => Ok(Self {
                inner: value.clone(),
            }),
            other => type_err("Fn(...)", other),
        }
    }
}

impl ToGos for BindingCallback {
    fn to_gos(self) -> Value {
        self.inner
    }
}

// --- Persistent callbacks (Phase 3) ----------------------------------

/// Long-lived callable handle. Outlives the binding fn that
/// produced it.
///
/// Where [`BindingCallback`] is call-scoped (the wrapped `Value`
/// reference is borrowed from the dispatcher's argument slice),
/// `PersistentCallback` owns a strong reference to the underlying
/// callable. The binding may store one on a registry-tracked
/// `static`, hand the handle back to Gossamer code, and re-invoke
/// it later from a different goroutine.
///
/// # Lifetime / GC
///
/// The wrapped `Value` is an `Arc`-shared closure / native - the
/// underlying callable stays alive as long as the
/// `PersistentCallback` does. The Gossamer-side GC scans the
/// binding's `Registry` so the captured environment is kept
/// reachable transitively.
///
/// # Coroutine safety
///
/// `invoke` re-enters the interpreter through the supplied
/// [`gossamer_interp::value::NativeDispatch`]. Goroutine yielding
/// works the same as any other Gossamer call.
///
/// # Release
///
/// Drop the `PersistentCallback` (or call [`Self::release`]) to
/// stop pinning the callable. Bindings that store handles on a
/// long-lived registry MUST publish a Gossamer-side "release"
/// function so user code can avoid leaks.
#[derive(Debug, Clone)]
pub struct PersistentCallback {
    inner: Value,
}

impl PersistentCallback {
    /// Wraps the underlying callable.
    #[must_use]
    pub fn from_value(v: Value) -> Self {
        Self { inner: v }
    }

    /// Invokes the callable with `args`, re-entering the interp
    /// through `dispatch`.
    pub fn invoke(
        &self,
        dispatch: &mut dyn gossamer_interp::value::NativeDispatch,
        args: Vec<Value>,
    ) -> RuntimeResult<Value> {
        dispatch.call_value(&self.inner, args)
    }

    /// Borrow the underlying callable for inspection.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.inner
    }

    /// Consumes `self`, returning the wrapped callable. Useful
    /// when handing it back to Gossamer code as a stored value.
    #[must_use]
    pub fn into_value(self) -> Value {
        self.inner
    }

    /// Explicitly drop the strong reference. After this the
    /// underlying callable is no longer pinned by `self`.
    pub fn release(self) {
        drop(self);
    }
}

impl FromGos for PersistentCallback {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Closure(_) | Value::Native(_) | Value::Builtin(_) => Ok(Self {
                inner: value.clone(),
            }),
            other => type_err("Fn(...) [persistent]", other),
        }
    }
}

impl ToGos for PersistentCallback {
    fn to_gos(self) -> Value {
        self.inner
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

    // --- ABI 0.4 conv tests --------------------------------------

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
    fn hash_map_i64_i64_round_trip_via_intmap() {
        let mut intmap = dense_map_with_capacity(2);
        intmap.insert(1, 100);
        intmap.insert(2, 200);
        let v = Value::IntMap(Arc::new(parking_lot::Mutex::new(intmap)));
        let back: HashMap<i64, i64> = HashMap::from_gos(&v).unwrap();
        assert_eq!(back.get(&1), Some(&100));
        assert_eq!(back.get(&2), Some(&200));
    }

    #[test]
    fn hash_map_accepts_array_of_tuples() {
        let entries = vec![
            Value::Tuple(Arc::new(vec![
                Value::String(SmolStr::from_str("k1")),
                Value::Int(1),
            ])),
            Value::Tuple(Arc::new(vec![
                Value::String(SmolStr::from_str("k2")),
                Value::Int(2),
            ])),
        ];
        let v = Value::Array(Arc::new(entries));
        let back: HashMap<String, i64> = HashMap::from_gos(&v).unwrap();
        assert_eq!(back.get("k1"), Some(&1));
        assert_eq!(back.get("k2"), Some(&2));
    }

    #[test]
    fn dyn_value_int_round_trip() {
        let v: Value = DynValue::Int(42).to_gos();
        let back = DynValue::from_gos(&v).unwrap();
        assert_eq!(back, DynValue::Int(42));
    }

    #[test]
    fn dyn_value_tagged_round_trip() {
        let original = DynValue::Tagged {
            name: "Integer".to_string(),
            payload: vec![DynValue::Int(7)],
        };
        let v: Value = original.clone().to_gos();
        let back = DynValue::from_gos(&v).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn dyn_value_bytes_via_intarray() {
        let original = DynValue::Bytes(b"hi".to_vec());
        let v: Value = original.clone().to_gos();
        let back = DynValue::from_gos(&v).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn dyn_value_list_of_mixed() {
        let original = DynValue::List(vec![
            DynValue::Int(1),
            DynValue::String("two".to_string()),
            DynValue::Bool(true),
        ]);
        let v: Value = original.clone().to_gos();
        let back = DynValue::from_gos(&v).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn binding_callback_rejects_non_callable() {
        let v: Value = "not a fn".to_gos();
        let err = BindingCallback::from_gos(&v).unwrap_err();
        assert!(matches!(err, RuntimeError::Type(_)));
    }
}
