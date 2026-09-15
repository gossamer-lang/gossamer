//! Core type representation shared across the type-checker, trait
//! solver, and later IR passes.
//! The [`Ty`] handle is a cheap `Copy` wrapper around an index into the
//! [`crate::TyCtxt`] interner. Structural type data lives in [`TyKind`].
//! Two semantically identical types always intern to the same [`Ty`],
//! so pointer-equality can stand in for structural equality.

#![forbid(unsafe_code)]

use gossamer_resolve::DefId;

use crate::subst::Substs;
use crate::traits::TraitRef;

/// Interner handle for a type. Cheap to copy; meaningful only when
/// paired with the [`crate::TyCtxt`] that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Ty(pub(crate) u32);

impl Ty {
    /// Raw numeric index into the interner, useful for stable sort
    /// orders and debug output.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Width tag for signed and unsigned integer types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IntTy {
    /// Signed 8-bit integer.
    I8,
    /// Signed 16-bit integer.
    I16,
    /// Signed 32-bit integer.
    I32,
    /// Signed 64-bit integer.
    I64,
    /// Signed 128-bit integer.
    I128,
    /// Signed pointer-sized integer.
    Isize,
    /// Unsigned 8-bit integer.
    U8,
    /// Unsigned 16-bit integer.
    U16,
    /// Unsigned 32-bit integer.
    U32,
    /// Unsigned 64-bit integer.
    U64,
    /// Unsigned 128-bit integer.
    U128,
    /// Unsigned pointer-sized integer.
    Usize,
}

impl IntTy {
    /// Returns the source-level name of this integer type (e.g. `i32`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::I128 => "i128",
            Self::Isize => "isize",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::U128 => "u128",
            Self::Usize => "usize",
        }
    }

    /// Returns `true` when this integer type is signed.
    #[must_use]
    pub const fn is_signed(self) -> bool {
        matches!(
            self,
            Self::I8 | Self::I16 | Self::I32 | Self::I64 | Self::I128 | Self::Isize
        )
    }
}

/// Width tag for floating-point types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FloatTy {
    /// 32-bit IEEE-754 binary32.
    F32,
    /// 64-bit IEEE-754 binary64.
    F64,
}

impl FloatTy {
    /// Returns the source-level name of this float type (e.g. `f64`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }
}

/// Reference mutability marker used by [`TyKind::Ref`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Mutbl {
    /// `&T` - shared GC reference.
    Not,
    /// `&mut T` - exclusive GC reference.
    Mut,
}

impl Mutbl {
    /// Returns the keyword form used when printing reference types.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Not => "&",
            Self::Mut => "&mut ",
        }
    }
}

/// Type-inference variable identifier produced by
/// [`crate::InferCtxt::fresh_var`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TyVid(pub u32);

impl TyVid {
    /// Returns the raw numeric index of this variable.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Zero-based index of a bound generic parameter within its defining
/// item's generics list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ParamIdx(pub u32);

impl ParamIdx {
    /// Returns the raw numeric index of this parameter.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Length of a fixed-size array `[T; N]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ArrayLen {
    /// A statically-known element count.
    Concrete(usize),
    /// A const generic parameter, referenced by its position in the
    /// defining item's generics list. Every instantiation is
    /// monomorphised first, replacing this with a `Concrete` length, so
    /// codegen only ever observes concrete array lengths.
    Param(ParamIdx),
}

impl ArrayLen {
    /// The concrete element count, or `0` for a const generic parameter
    /// that was never substituted (only reachable in an un-instantiated
    /// generic template, which never executes).
    #[must_use]
    pub const fn to_usize(self) -> usize {
        match self {
            Self::Concrete(n) => n,
            Self::Param(_) => 0,
        }
    }

    /// The concrete element count, or `None` for a const generic
    /// parameter that has not yet been substituted.
    #[must_use]
    pub const fn concrete(self) -> Option<usize> {
        match self {
            Self::Concrete(n) => Some(n),
            Self::Param(_) => None,
        }
    }
}

/// Signature of a bare function pointer or `fn`-typed item.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FnSig {
    /// Parameter types in source order.
    pub inputs: Vec<Ty>,
    /// Return type (use the interned unit type for `()`).
    pub output: Ty,
}

/// Closure-trait kind attached to a [`TyKind::Closure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ClosureKind {
    /// Non-mutating closure (`Fn`).
    Fn,
    /// Mutating closure (`FnMut`).
    FnMut,
    /// Owning closure (`FnOnce`).
    FnOnce,
}

impl ClosureKind {
    /// Returns the source-level trait spelling of this closure kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fn => "Fn",
            Self::FnMut => "FnMut",
            Self::FnOnce => "FnOnce",
        }
    }
}

/// Structural payload of an interned type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TyKind {
    /// `bool`.
    Bool,
    /// `char`.
    Char,
    /// `String` - GC-backed UTF-8 string.
    String,
    /// Signed or unsigned integer types `i8`..`usize`.
    Int(IntTy),
    /// Floating-point types `f32` / `f64`.
    Float(FloatTy),
    /// `()` - the unit type.
    Unit,
    /// `!` - the never type.
    Never,
    /// Tuple type `(T1, ..., Tn)` with two or more elements.
    Tuple(Vec<Ty>),
    /// Fixed-size array `[T; N]`.
    Array {
        /// Element type.
        elem: Ty,
        /// Element count - concrete, or a const generic parameter.
        len: ArrayLen,
    },
    /// `Simd<T, N>` - a fixed-width vector of `N` lanes of `T` with lane-wise
    /// operations; `Mask<N>` is its `bool`-laned form. The checker keeps the
    /// kind so operators and methods resolve against it;
    /// [`crate::normalize_for_lowering`] erases it to the `[T; N]` it is laid
    /// out as, so no backend observes it.
    Simd {
        /// Lane type.
        elem: Ty,
        /// Lane count - concrete, or a const generic parameter.
        lanes: ArrayLen,
    },
    /// Unsized slice `[T]`, always seen through a reference at runtime.
    Slice(Ty),
    /// `Vec<T>` - built-in growable sequence.
    Vec(Ty),
    /// `Iterator<T>` - linear lazy sequence state, the type an adapter
    /// chain produces.
    Iterator(Ty),
    /// `Range<T>` - the bounded sequence a range expression produces.
    ///
    /// It shares `Iterator<T>`'s representation and method surface and
    /// converts to it, so a range is accepted wherever an iterator is
    /// required. The distinction exists so a range reports the type the
    /// reader wrote. Lowering never sees it: [`crate::normalize_for_lowering`]
    /// maps it to `Iterator` at the boundary into HIR.
    Range(Ty),
    /// `Map<K, V>` / `BTreeMap<K, V>` - the built-in map. The two
    /// spellings name distinct types over one representation, so a value
    /// carries which container it is.
    HashMap {
        /// Key type.
        key: Ty,
        /// Value type.
        value: Ty,
        /// True for the `BTreeMap` spelling.
        ordered: bool,
    },
    /// `Sender<T>` - channel send endpoint.
    Sender(Ty),
    /// `Receiver<T>` - channel receive endpoint.
    Receiver(Ty),
    /// `JoinHandle<T>` - handle returned by `spawn(f)`; `.join()`
    /// blocks for the goroutine's `Result<T, String>` outcome.
    /// Carried as a one-shot channel pointer at runtime.
    JoinHandle(Ty),
    /// `time::Duration` - a transparent `i64`-of-milliseconds newtype.
    /// The runtime representation is exactly an `i64`; the distinct
    /// kind exists only so method-form accessors (`d.as_millis()`)
    /// resolve against the receiver's static type. MIR lowering
    /// normalizes it back to `i64`, so codegen never observes it.
    Duration,
    /// `time::Instant` - a transparent `i64`-of-monotonic-milliseconds
    /// newtype. Like `Duration`, the runtime value is exactly an `i64`
    /// (the monotonic-ms reading at `Instant::now()`); the distinct
    /// kind only steers the method-form accessor (`inst.elapsed_ms()`)
    /// against the receiver's static type. MIR lowering normalizes it
    /// back to `i64`, so codegen never observes it.
    Instant,
    /// `json::Value` - opaque dynamic JSON node. Carries no
    /// generic parameters; the runtime backs every node with a
    /// boxed `serde_json::Value`. Field access on a `JsonValue`
    /// receiver is rewritten by MIR lowering into a runtime
    /// `gos_rt_json_get(receiver, "field")` call.
    JsonValue,
    /// `DynValue` - a value whose shape is decided by the data rather than
    /// by a declaration: `Nil | Bool | Int | Float | Char | String | Bytes |
    /// List | Map | Tagged { name, payload }`, where a tagged arm's name is a
    /// runtime string. A decoder, a database column typed by its own
    /// metadata, and a Rust binding returning an arm set it names at run time
    /// all produce one. The runtime backs every value with a shared node.
    DynValue,
    /// `errors::Error` - opaque heap error value with a message
    /// string and optional cause chain. Used as the default Err
    /// type for `Result<T>` so the `?` operator and error
    /// propagation work across the standard library without
    /// explicit `map_err` calls.
    DynError,
    /// GC reference type `&T` or `&mut T`.
    Ref {
        /// Mutability of the reference.
        mutability: Mutbl,
        /// Pointee type.
        inner: Ty,
    },
    /// Reference to a specific function definition.
    FnDef {
        /// `DefId` of the function.
        def: DefId,
        /// Generic substitutions instantiating the function.
        substs: Substs,
    },
    /// Anonymous function-pointer type `fn(...) -> ...`.
    FnPtr(FnSig),
    /// Callable trait type `Fn(args) -> ret` - accepts both bare
    /// `fn` items and capturing closures via implicit coercion.
    /// Lowered as a `(env_ptr, code_ptr)` fat pointer (two
    /// consecutive `i64` slots) so the env that a capturing
    /// closure needs has a place to live, and so a bare item can
    /// still satisfy the type by setting `env` to null. Mirrors
    /// Rust's `dyn Fn(args) -> ret` shape; a single trait covers
    /// the common case (no `FnMut` / `FnOnce` split for v1.0.0
    /// since every captured value is GC-managed).
    FnTrait(FnSig),
    /// Anonymous closure type, tied to the expression that introduced it.
    Closure {
        /// `DefId` of the closure (normally the enclosing expression's
        /// synthetic item id).
        def: DefId,
        /// Captured-type substitutions.
        substs: Substs,
        /// Closure-trait kind.
        kind: ClosureKind,
    },
    /// Named ADT (struct or enum) instantiation.
    Adt {
        /// `DefId` of the ADT.
        def: DefId,
        /// Generic substitutions.
        substs: Substs,
    },
    /// Type alias reference `type Alias<G> = Target`.
    Alias {
        /// `DefId` of the alias.
        def: DefId,
        /// Generic substitutions.
        substs: Substs,
    },
    /// Opaque nominal alias `type Name = new Repr`.
    ///
    /// A type distinct from every other, including `repr`, and distinct
    /// from any other nominal alias over the same representation. Nothing
    /// converts between the two implicitly; `From` / `TryFrom` impls are
    /// how a program crosses the boundary.
    ///
    /// The runtime value is exactly `repr`, following `Duration` and
    /// `Instant`: the distinct kind exists only so the checker can keep
    /// the two apart and so method dispatch resolves against the alias.
    /// [`crate::normalize_for_lowering`] erases it at the boundary into
    /// HIR, so no backend observes it and the representation's ABI is
    /// unchanged.
    Nominal {
        /// `DefId` of the alias declaration.
        def: DefId,
        /// Runtime representation this alias is a distinct name for.
        repr: Ty,
    },
    /// Dynamic trait object `dyn Trait<Args>`.
    Dyn(TraitRef),
    /// Unresolved inference variable introduced during unification.
    Var(TyVid),
    /// Bound type parameter referring to the `ParamIdx`-th generic of
    /// its defining item.
    Param {
        /// Position in the parent generics list.
        idx: ParamIdx,
        /// Source-level name for diagnostics (`T`, `U`, ...). Owned
        /// so the previous `Box::leak`-into-`&'static str` pattern
        /// can't strand allocations on the heap when the surrounding
        /// `TyCtxt` is dropped - leaksanitizer flagged the strand
        /// on the fuzz harness.
        name: Box<str>,
    },
    /// A type that could not be resolved; diagnostics have already been
    /// produced.
    Error,
}

impl TyKind {
    /// Returns `true` for the primitive numeric, boolean, character,
    /// unit, or never kinds.
    #[must_use]
    pub const fn is_primitive(&self) -> bool {
        matches!(
            self,
            Self::Bool
                | Self::Char
                | Self::Int(_)
                | Self::Float(_)
                | Self::Unit
                | Self::Never
                | Self::String
                | Self::Duration
                | Self::Instant
        )
    }
}
