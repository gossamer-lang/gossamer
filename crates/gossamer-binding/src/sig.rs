//! Compile-time type advertisement.
//!
//! Each Rust type usable in a binding signature implements
//! [`SigType`] with an associated `const TYPE` that the
//! `register_module!` macro picks up to populate
//! [`crate::Signature::params`] / [`crate::Signature::ret`].

use crate::types::Type;

/// Lifts a Rust type into the binding's [`Type`] vocabulary.
pub trait SigType {
    /// Static [`Type`] tag identifying `Self`.
    const TYPE: Type;
}

impl SigType for () {
    const TYPE: Type = Type::Unit;
}

impl SigType for bool {
    const TYPE: Type = Type::Bool;
}

impl SigType for i64 {
    const TYPE: Type = Type::I64;
}

impl SigType for u64 {
    const TYPE: Type = Type::I64;
}

impl SigType for usize {
    const TYPE: Type = Type::I64;
}

impl SigType for u16 {
    const TYPE: Type = Type::I64;
}

impl SigType for u8 {
    const TYPE: Type = Type::I64;
}

impl SigType for f64 {
    const TYPE: Type = Type::F64;
}

impl SigType for char {
    const TYPE: Type = Type::Char;
}

impl SigType for String {
    const TYPE: Type = Type::String;
}

impl<T: SigType> SigType for Option<T> {
    const TYPE: Type = Type::Option(&T::TYPE);
}

impl<T: SigType, E: SigType> SigType for Result<T, E> {
    const TYPE: Type = Type::Result(&T::TYPE, &E::TYPE);
}

impl<T: SigType> SigType for Vec<T> {
    const TYPE: Type = Type::Vec(&T::TYPE);
}

impl SigType for crate::Value {
    /// `Value` is the universal pass-through; the type checker
    /// accepts anything in this slot.
    const TYPE: Type = Type::Any;
}

// --- ABI 0.4 new shapes ------------------------------------------------

// `Bytes` is a transparent `Vec<u8>` newtype; binding authors
// reach for it when they want `Type::Bytes` instead of
// `Type::Vec(&Type::I64)`. Rust stable doesn't have
// specialization, so we can't have `Vec<u8>` → `Bytes` automatic;
// the newtype is the explicit opt-in.
impl SigType for crate::conv::Bytes {
    const TYPE: Type = Type::Bytes;
}

impl SigType for crate::conv::BindingCallback {
    /// A callable, valid for the binding call that received it.
    const TYPE: Type = Type::Callback(&[], &Type::Any);
}

impl SigType for crate::conv::PersistentCallback {
    /// Persistent callbacks ride on the same `Callback` shape as
    /// `BindingCallback`. The lifetime distinction is enforced
    /// runtime-side; the type checker sees a callable.
    const TYPE: Type = Type::Callback(&[], &Type::Any);
}

impl SigType for crate::conv::DynValue {
    /// `DynValue` is the variant-erasure type. Its `Type` slot
    /// is filled by the binding author at call site via a const
    /// `VariantArm` table; the default `TYPE` is a permissive
    /// `Variant(&[])` (accepts any arm). Bindings that want
    /// strict typechecking should declare a concrete signature
    /// via a custom `SigType` wrapper.
    const TYPE: Type = Type::Variant(&[]);
}

#[allow(
    clippy::implicit_hasher,
    reason = "ABI surface - declared signature is hasher-agnostic."
)]
impl<K: SigType, V: SigType> SigType for std::collections::HashMap<K, V> {
    const TYPE: Type = Type::Map(&K::TYPE, &V::TYPE);
}

// --- Tuple SigType impls (Phase 1) ------------------------------------
//
// Lower N-tuples to `Type::Tuple(&[T1::TYPE, ...])`. Compile-time
// element-type validation comes for free - each `Ti: SigType` bound
// is checked at impl-instantiation. The compiled-tier `BindingAbi`
// for tuples is hand-listed per shape in `native.rs`.

impl<A: SigType, B: SigType> SigType for (A, B) {
    const TYPE: Type = Type::Tuple(&[A::TYPE, B::TYPE]);
}

impl<A: SigType, B: SigType, C: SigType> SigType for (A, B, C) {
    const TYPE: Type = Type::Tuple(&[A::TYPE, B::TYPE, C::TYPE]);
}

impl<A: SigType, B: SigType, C: SigType, D: SigType> SigType for (A, B, C, D) {
    const TYPE: Type = Type::Tuple(&[A::TYPE, B::TYPE, C::TYPE, D::TYPE]);
}
