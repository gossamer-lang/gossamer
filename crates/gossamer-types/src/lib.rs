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

mod arena_escape;
pub mod builtin_traits;
mod checker;
mod context;
pub mod data_first;
mod error;
mod exhaustiveness;
mod infer;
mod normalize;
pub mod printer;
pub mod std_fn_eta;
pub mod std_fn_values;
pub mod stdlib_signatures;
mod subst;
mod table;
mod trait_index;
mod traits;
mod ty;

pub use arena_escape::{
    ArenaEscapeDiagnostic, ArenaEscapeError, ArenaEscapeKind, check_arena_escapes,
};
pub use builtin_traits::{
    BUILTIN_TRAITS, BuiltinTrait, BuiltinTraitKind, ITERATOR_BOUND_METHODS, builtin_trait,
    render_builtin_traits_markdown,
};
pub use checker::{
    STDLIB_TRAIT_NAMES, core_type_accepts_method, core_type_declares_method,
    is_array_sequence_method, is_collection_traversal_method, is_free_call_only_traversal,
    is_iterator_method, is_map_method, is_set_method, is_slice_sequence_method, is_tuple_method,
    is_tuple_rejected_method, is_vec_only_sequence_method, iterator_adapter_is_lazy,
    iterator_receiver_accepts_method, typecheck_source_file,
    typecheck_source_file_for_repl_inspection,
};
pub use context::TyCtxt;
pub use error::{NotDisplayableClass, TypeDiagnostic, TypeError};
pub use exhaustiveness::{ExhaustivenessDiagnostic, ExhaustivenessError, check_exhaustiveness};
pub use infer::{InferCtxt, UnifyError};
pub use normalize::normalize_caller_side_spellings;
pub use printer::{render_public_ty, render_ty};
pub use stdlib_signatures::{
    STD_FUNCTION_SIGNATURES, StdFunctionSignature, function_shape as stdlib_function_shape,
    function_signature as stdlib_function_signature,
};
pub use subst::{GenericArg, Substs};
pub use table::TypeTable;
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
            | "retain"
            | "shrink_to_fit"
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
/// representation.
#[must_use]
pub fn normalize_for_lowering(tcx: &mut TyCtxt, ty: Ty) -> Ty {
    let ty = match tcx.kind(ty) {
        Some(TyKind::Range(item)) => {
            let item = *item;
            tcx.iterator_ty(item)
        }
        _ => ty,
    };
    erase_nominal(tcx, ty)
}

/// Replaces every [`TyKind::Nominal`] in `ty` with its representation,
/// rebuilding the containers around it. Returns `ty` untouched when it
/// holds no nominal alias, so the common case interns nothing.
#[must_use]
pub fn erase_nominal(tcx: &mut TyCtxt, ty: Ty) -> Ty {
    let Some(kind) = tcx.kind(ty).cloned() else {
        return ty;
    };
    match kind {
        TyKind::Nominal { repr, .. } => erase_nominal(tcx, repr),
        TyKind::Tuple(elems) => {
            let mapped: Vec<Ty> = elems.iter().map(|e| erase_nominal(tcx, *e)).collect();
            if mapped == elems {
                ty
            } else {
                tcx.intern(TyKind::Tuple(mapped))
            }
        }
        TyKind::Array { elem, len } => {
            let mapped = erase_nominal(tcx, elem);
            if mapped == elem {
                ty
            } else {
                tcx.intern(TyKind::Array { elem: mapped, len })
            }
        }
        TyKind::HashMap { key, value, .. } => {
            let k = erase_nominal(tcx, key);
            let v = erase_nominal(tcx, value);
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
            let mapped = erase_nominal(tcx, inner);
            if mapped == inner {
                ty
            } else {
                tcx.intern(TyKind::Ref {
                    mutability,
                    inner: mapped,
                })
            }
        }
        TyKind::FnPtr(ref sig) | TyKind::FnTrait(ref sig) => {
            let inputs: Vec<Ty> = sig.inputs.iter().map(|i| erase_nominal(tcx, *i)).collect();
            let output = erase_nominal(tcx, sig.output);
            if inputs == sig.inputs && output == sig.output {
                return ty;
            }
            let mapped = FnSig { inputs, output };
            tcx.intern(if matches!(kind, TyKind::FnPtr(_)) {
                TyKind::FnPtr(mapped)
            } else {
                TyKind::FnTrait(mapped)
            })
        }
        TyKind::Slice(inner)
        | TyKind::Vec(inner)
        | TyKind::Iterator(inner)
        | TyKind::Range(inner)
        | TyKind::Sender(inner)
        | TyKind::Receiver(inner)
        | TyKind::JoinHandle(inner) => {
            let mapped = erase_nominal(tcx, inner);
            if mapped == inner {
                return ty;
            }
            tcx.intern(match kind {
                TyKind::Slice(_) => TyKind::Slice(mapped),
                TyKind::Vec(_) => TyKind::Vec(mapped),
                TyKind::Iterator(_) => TyKind::Iterator(mapped),
                TyKind::Range(_) => TyKind::Range(mapped),
                TyKind::Sender(_) => TyKind::Sender(mapped),
                TyKind::Receiver(_) => TyKind::Receiver(mapped),
                _ => TyKind::JoinHandle(mapped),
            })
        }
        TyKind::Adt { def, substs } => match erase_nominal_substs(tcx, &substs) {
            Some(mapped) => tcx.intern(TyKind::Adt {
                def,
                substs: mapped,
            }),
            None => ty,
        },
        _ => ty,
    }
}

/// Erases nominal aliases inside a substitution list, returning `None`
/// when nothing changed.
fn erase_nominal_substs(tcx: &mut TyCtxt, substs: &Substs) -> Option<Substs> {
    let mut changed = false;
    let args: Vec<GenericArg> = substs
        .as_slice()
        .iter()
        .map(|arg| match arg {
            GenericArg::Type(t) => {
                let mapped = erase_nominal(tcx, *t);
                changed |= mapped != *t;
                GenericArg::Type(mapped)
            }
            other @ GenericArg::Const(_) => other.clone(),
        })
        .collect();
    changed.then(|| Substs::from_args(args))
}
