//! Substituting an instantiation's types for a generic item's parameters.

use crate::{FnSig, GenericArg, Substs, Ty, TyCtxt, TyKind};

/// Substitutes a specialisation's concrete types for the template's type
/// parameters within `ty`, recursing through composite types. A `Param` whose
/// position holds a const argument (`subst_tys[i] == None`) is left unchanged.
pub fn subst_param_ty(tcx: &mut TyCtxt, ty: Ty, subst_tys: &[Option<Ty>]) -> Ty {
    let kind = tcx.kind_of(ty).clone();
    match kind {
        TyKind::Param { idx, .. } => subst_tys
            .get(idx.0 as usize)
            .copied()
            .flatten()
            .unwrap_or(ty),
        TyKind::Ref { inner, mutability } => {
            let inner = subst_param_ty(tcx, inner, subst_tys);
            tcx.intern(TyKind::Ref { inner, mutability })
        }
        TyKind::Vec(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Vec(elem))
        }
        TyKind::Slice(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Slice(elem))
        }
        TyKind::Array { elem, len } => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Array { elem, len })
        }
        TyKind::Tuple(elems) => {
            let elems = elems
                .iter()
                .map(|&e| subst_param_ty(tcx, e, subst_tys))
                .collect();
            tcx.intern(TyKind::Tuple(elems))
        }
        TyKind::HashMap {
            key,
            value,
            ordered,
        } => {
            let key = subst_param_ty(tcx, key, subst_tys);
            let value = subst_param_ty(tcx, value, subst_tys);
            tcx.intern(TyKind::HashMap {
                key,
                value,
                ordered,
            })
        }
        TyKind::Iterator(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Iterator(elem))
        }
        TyKind::Range(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Range(elem))
        }
        // A channel endpoint and a join handle name the payload they carry,
        // and the send, receive, and join lowering picks its value
        // representation from that payload type.
        TyKind::Sender(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Sender(elem))
        }
        TyKind::Receiver(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Receiver(elem))
        }
        TyKind::JoinHandle(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::JoinHandle(elem))
        }
        // A callable parameter carries the template's parameters inside
        // its signature, and the compiled tiers build the call from that
        // signature: an unsubstituted `Fn(T) -> T` on an `f64`
        // instantiation puts the argument in an integer register and
        // reads the result out of one.
        TyKind::FnPtr(sig) => {
            let sig = subst_param_sig(tcx, &sig, subst_tys);
            tcx.intern(TyKind::FnPtr(sig))
        }
        TyKind::FnTrait(sig) => {
            let sig = subst_param_sig(tcx, &sig, subst_tys);
            tcx.intern(TyKind::FnTrait(sig))
        }
        TyKind::Adt { def, substs } => {
            let substs = subst_param_substs(tcx, &substs, subst_tys);
            tcx.intern(TyKind::Adt { def, substs })
        }
        TyKind::Alias { def, substs } => {
            let substs = subst_param_substs(tcx, &substs, subst_tys);
            tcx.intern(TyKind::Alias { def, substs })
        }
        // A generic function named inside a generic body carries the body's
        // parameters as its own type arguments, and those say which
        // instantiation the call reaches.
        TyKind::FnDef { def, substs } => {
            let substs = subst_param_substs(tcx, &substs, subst_tys);
            tcx.intern(TyKind::FnDef { def, substs })
        }
        _ => ty,
    }
}

/// [`subst_param_ty`] over every type argument of `substs`.
pub fn subst_param_substs(tcx: &mut TyCtxt, substs: &Substs, subst_tys: &[Option<Ty>]) -> Substs {
    let new_args = substs
        .as_slice()
        .iter()
        .map(|a| match a {
            GenericArg::Type(t) => GenericArg::Type(subst_param_ty(tcx, *t, subst_tys)),
            other @ (GenericArg::Const(_) | GenericArg::ConstParam(_)) => other.clone(),
        })
        .collect();
    Substs::from_args(new_args)
}

/// [`subst_param_ty`] over every type a callable signature names.
pub fn subst_param_sig(tcx: &mut TyCtxt, sig: &FnSig, subst_tys: &[Option<Ty>]) -> FnSig {
    FnSig {
        inputs: sig
            .inputs
            .iter()
            .map(|input| subst_param_ty(tcx, *input, subst_tys))
            .collect(),
        output: subst_param_ty(tcx, sig.output, subst_tys),
    }
}
