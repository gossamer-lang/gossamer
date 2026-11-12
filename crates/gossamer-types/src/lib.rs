//! Type representation for the Gossamer compiler.
//! This crate models every type production in SPEC §3: primitives,
//! tuples, arrays, slices, built-in collections (`Vec`, `HashMap`),
//! channel endpoints, GC references, function pointers and closures,
//! named ADTs, type aliases, trait objects, inference variables, and
//! bound type parameters.
//! Type handles are issued by the [`TyCtxt`] interner. Two structurally
//! identical types always intern to the same [`Ty`], so later passes
//! can compare types with a single `u32` comparison. The [`InferCtxt`]
//! sits on top of the interner and provides Hindley-Milner unification
//! with an occurs check.
//! See SPEC §3 for the full type system.

#![forbid(unsafe_code)]

mod admissible;
mod arena_escape;
pub mod builtin_traits;
mod capture_writes;
mod checker;
mod context;
pub mod data_first;
mod error;
mod exhaustiveness;
mod foreign_layout;
mod foreign_policy;
mod frontend_checks;
mod infer;
mod normalize;
pub mod printer;
pub mod purity;
pub mod std_fn_eta;
pub mod std_fn_values;
pub mod stdlib_signatures;
mod subst;
mod table;
mod trait_index;
mod traits;
mod ty;

/// Element-kind tags a vector's header carries, naming what one element
/// owns. The runtime's `vec_elem_kind` module defines the same values.
pub mod vec_elem_kind {
    /// A value owning no other heap memory.
    pub const PRIMITIVE: u8 = 0;
    /// A counted runtime string.
    pub const STRING: u8 = 1;
    /// A vector handle.
    pub const VEC: u8 = 2;
    /// A map handle.
    pub const MAP: u8 = 3;
    /// An `errors::Error` handle.
    pub const ERROR: u8 = 4;
    /// A struct, tuple, or fixed array of a single slot, held inline.
    pub const AGGR_FLAT: u8 = 10;
    /// A `json::Value` handle.
    pub const JSON: u8 = 12;
    /// A narrow two's-complement integer (`i8`, `i16`, `i32`).
    pub const SIGNED: u8 = 18;
    /// A four-byte `f32`.
    pub const F32: u8 = 19;
}

pub use admissible::{AdmissibilityDiagnostic, Inadmissible, check_parallel_adapters};
pub use arena_escape::{
    ArenaEscapeDiagnostic, ArenaEscapeError, ArenaEscapeKind, check_arena_escapes,
};
pub use builtin_traits::{
    BUILTIN_TRAITS, BuiltinTrait, BuiltinTraitKind, ITERATOR_BOUND_METHODS, builtin_trait,
    render_builtin_traits_markdown,
};
pub use capture_writes::{CaptureWriteDiagnostic, CaptureWriteKind, check_capture_writes};
pub use checker::{
    STDLIB_TRAIT_NAMES, core_type_accepts_method, core_type_declares_method,
    is_array_sequence_method, is_btree_map_method, is_btree_set_method,
    is_collection_traversal_method, is_free_call_only_traversal, is_iterator_method, is_map_method,
    is_set_method, is_slice_sequence_method, is_tuple_method, is_tuple_rejected_method,
    is_vec_only_sequence_method, iterator_adapter_is_lazy, iterator_receiver_accepts_method,
    typecheck_source_file, typecheck_source_file_for_repl_inspection,
};
pub use context::{FlatEnumLayout, PackedLayout, PlainLayout, TyCtxt, UNION_TYPE_NAME};
pub use error::{
    ForeignError, ForeignLibrary, IntegerCastFix, NotDisplayableClass, TypeDiagnostic, TypeError,
};
pub use exhaustiveness::{ExhaustivenessDiagnostic, ExhaustivenessError, check_exhaustiveness};
pub use foreign_layout::{CLeaf, CStep, c_class_signed, c_class_width};
pub use foreign_policy::ForeignPolicy;
pub use frontend_checks::{
    CheckPhase, PhaseObserver, UnitChecks, check_resolved_unit, top_level_names,
};
pub use infer::{InferCtxt, UnifyError};
pub use normalize::normalize_caller_side_spellings;
pub use printer::{public_type_name, render_public_ty, render_ty, stable_type_name};
pub use stdlib_signatures::{
    STD_FUNCTION_SIGNATURES, StdFunctionSignature, function_shape as stdlib_function_shape,
    function_signature as stdlib_function_signature,
};
pub use subst::{GenericArg, Substs};
pub use table::{ConstGenericArg, TypeTable};
pub use trait_index::{
    ImplEntry, ImplFnId, ImplId, ImplIndex, ImplMethod, MethodResolution, TraitDiagnostic,
    TraitEntry, TraitError,
};
pub use traits::{Predicate, TraitRef};
pub use ty::{ArrayLen, ClosureKind, FloatTy, FnSig, IntTy, Mutbl, ParamIdx, Ty, TyKind, TyVid};

/// Method names whose built-in dispatch mutates and writes the receiver back
/// into its source place. The type checker and execution tiers share this
/// list so an immutable receiver cannot reach an in-place mutation through a
/// method call.
#[must_use]
pub fn is_mutating_method_name(name: &str) -> bool {
    matches!(
        name,
        "push"
            | "push_str"
            | "push_char"
            | "push_byte"
            | "push_utf8"
            | "push_json_quoted"
            | "push_back"
            | "push_front"
            | "pop"
            | "pop_back"
            | "pop_front"
            | "insert"
            | "or_insert"
            | "inc"
            | "inc_at"
            | "inc_batch"
            | "remove"
            | "clear"
            | "extend"
            | "extend_from_slice"
            | "truncate"
            | "reserve"
            | "reserve_exact"
            | "sort"
            | "sort_by"
            | "sort_by_key"
            | "reverse"
            | "swap"
            | "fill"
            | "copy_within"
            | "copy_from_slice"
            | "append"
            | "resize"
            | "resize_with"
            | "split_off"
            | "drain"
            | "shrink_to_fit"
            | "par_chunks_mut"
    )
}

/// Rewrites `Range<T>` to `Iterator<T>` and erases opaque nominal
/// aliases to their representation for the lowering pipeline.
///
/// `Range` exists so a range reports the type the reader wrote. The two
/// share one representation and one method surface, and only `Iterator`
/// has lowering behind it, so the boundary into HIR maps ranges onto it.
///
/// A `type Name = new Repr` alias is likewise a checker-only distinction
/// over an identical runtime value, so it erases here and no backend ever
/// sees one. The erasure is structural because a nominal alias can sit
/// anywhere a type can - `Vec<UserId>`, `Map<UserId, String>`, a function
/// signature - and every one of those positions must reach lowering as the
/// representation. Ranges are rewritten at every depth for the same reason,
/// a generic function's type arguments included.
///
/// Returns `ty` untouched when it holds neither, so the common case interns
/// nothing.
#[must_use]
pub fn normalize_for_lowering(tcx: &mut TyCtxt, ty: Ty) -> Ty {
    let Some(kind) = tcx.kind(ty).cloned() else {
        return ty;
    };
    match kind {
        TyKind::Nominal { repr, .. } => normalize_for_lowering(tcx, repr),
        TyKind::Range(item) => {
            let item = normalize_for_lowering(tcx, item);
            tcx.iterator_ty(item)
        }
        TyKind::Tuple(elems) => {
            let mapped: Vec<Ty> = elems
                .iter()
                .map(|e| normalize_for_lowering(tcx, *e))
                .collect();
            if mapped == elems {
                ty
            } else {
                tcx.intern(TyKind::Tuple(mapped))
            }
        }
        TyKind::Array { elem, len } => {
            let mapped = normalize_for_lowering(tcx, elem);
            if mapped == elem {
                ty
            } else {
                tcx.intern(TyKind::Array { elem: mapped, len })
            }
        }
        // A vector is laid out as the fixed array of its lanes.
        TyKind::Simd { elem, lanes } => {
            let mapped = normalize_for_lowering(tcx, elem);
            tcx.intern(TyKind::Array {
                elem: mapped,
                len: lanes,
            })
        }
        TyKind::HashMap { key, value, .. } => {
            let k = normalize_for_lowering(tcx, key);
            let v = normalize_for_lowering(tcx, value);
            if k == key && v == value {
                ty
            } else {
                tcx.intern(TyKind::HashMap {
                    key: k,
                    value: v,
                    ordered: false,
                })
            }
        }
        TyKind::Ref { mutability, inner } => {
            let mapped = normalize_for_lowering(tcx, inner);
            if mapped == inner {
                ty
            } else {
                tcx.intern(TyKind::Ref {
                    mutability,
                    inner: mapped,
                })
            }
        }
        TyKind::FnPtr(ref sig) | TyKind::FnTrait(ref sig) => match normalize_sig(tcx, sig) {
            Some(mapped) if matches!(kind, TyKind::FnPtr(_)) => tcx.intern(TyKind::FnPtr(mapped)),
            Some(mapped) => tcx.intern(TyKind::FnTrait(mapped)),
            None => ty,
        },
        TyKind::Slice(inner)
        | TyKind::Vec(inner)
        | TyKind::Iterator(inner)
        | TyKind::Sender(inner)
        | TyKind::Receiver(inner)
        | TyKind::JoinHandle(inner) => {
            let mapped = normalize_for_lowering(tcx, inner);
            if mapped == inner {
                return ty;
            }
            tcx.intern(match kind {
                TyKind::Slice(_) => TyKind::Slice(mapped),
                TyKind::Vec(_) => TyKind::Vec(mapped),
                TyKind::Iterator(_) => TyKind::Iterator(mapped),
                TyKind::Sender(_) => TyKind::Sender(mapped),
                TyKind::Receiver(_) => TyKind::Receiver(mapped),
                _ => TyKind::JoinHandle(mapped),
            })
        }
        TyKind::Adt { ref substs, .. }
        | TyKind::FnDef { ref substs, .. }
        | TyKind::Closure { ref substs, .. } => match normalize_substs(tcx, substs) {
            Some(mapped) => tcx.intern(with_substs(kind, mapped)),
            None => ty,
        },
        _ => ty,
    }
}

/// Normalizes a function signature's types for lowering, returning `None`
/// when nothing changed.
fn normalize_sig(tcx: &mut TyCtxt, sig: &FnSig) -> Option<FnSig> {
    let inputs: Vec<Ty> = sig
        .inputs
        .iter()
        .map(|i| normalize_for_lowering(tcx, *i))
        .collect();
    let output = normalize_for_lowering(tcx, sig.output);
    (inputs != sig.inputs || output != sig.output).then_some(FnSig { inputs, output })
}

/// `kind`, a type carrying a substitution list, with `substs` in its place.
fn with_substs(kind: TyKind, substs: Substs) -> TyKind {
    match kind {
        TyKind::Adt { def, .. } => TyKind::Adt { def, substs },
        TyKind::FnDef { def, .. } => TyKind::FnDef { def, substs },
        TyKind::Closure { def, kind, .. } => TyKind::Closure { def, substs, kind },
        other => other,
    }
}

/// Normalizes the types of a substitution list for lowering, returning
/// `None` when nothing changed.
fn normalize_substs(tcx: &mut TyCtxt, substs: &Substs) -> Option<Substs> {
    let mut changed = false;
    let args: Vec<GenericArg> = substs
        .as_slice()
        .iter()
        .map(|arg| match arg {
            GenericArg::Type(t) => {
                let mapped = normalize_for_lowering(tcx, *t);
                changed |= mapped != *t;
                GenericArg::Type(mapped)
            }
            other @ (GenericArg::Const(_) | GenericArg::ConstParam(_)) => other.clone(),
        })
        .collect();
    changed.then(|| Substs::from_args(args))
}
