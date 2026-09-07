//! Pretty-printer used by diagnostics to render interned types in a
//! way that matches the SPEC's surface syntax.

#![forbid(unsafe_code)]

use std::fmt::Write;

use crate::context::TyCtxt;
use crate::subst::{GenericArg, Substs};
use crate::traits::TraitRef;
use crate::ty::{FnSig, Ty, TyKind};

/// Renders `ty` as a human-readable string. Handles that are not owned
/// by `tcx` render as `<ty:?>`.
#[must_use]
pub fn render_ty(tcx: &TyCtxt, ty: Ty) -> String {
    let mut out = String::new();
    write_ty(tcx, ty, &mut out);
    out
}

/// Renders a type for user-facing output without exposing compiler inference
/// variable identifiers.
#[must_use]
pub fn render_public_ty(tcx: &TyCtxt, ty: Ty) -> String {
    render_ty(tcx, ty)
        .split_inclusive(|c: char| !c.is_ascii_alphanumeric() && c != '?')
        .map(|part| {
            if part.starts_with('?')
                && part
                    .trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                    .get(1..)
                    .is_some_and(|digits| {
                        !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
                    })
            {
                let suffix = part.trim_start_matches(|c: char| c == '?' || c.is_ascii_digit());
                format!("_{suffix}")
            } else {
                part.to_string()
            }
        })
        .collect()
}

fn write_ty(tcx: &TyCtxt, ty: Ty, out: &mut String) {
    let Some(kind) = tcx.kind(ty) else {
        let _ = write!(out, "<ty:{}>", ty.as_u32());
        return;
    };
    write_kind(tcx, kind, out);
}

fn write_kind(tcx: &TyCtxt, kind: &TyKind, out: &mut String) {
    match kind {
        TyKind::Bool => out.push_str("bool"),
        TyKind::Char => out.push_str("char"),
        TyKind::String => out.push_str("String"),
        TyKind::Int(int) => out.push_str(int.as_str()),
        TyKind::Float(float) => out.push_str(float.as_str()),
        TyKind::Unit => out.push_str("()"),
        TyKind::Never => out.push('!'),
        TyKind::Tuple(parts) => write_tuple(tcx, parts, out),
        TyKind::Array { elem, len } => write_array(tcx, *elem, *len, out),
        TyKind::Slice(elem) => write_slice(tcx, *elem, out),
        TyKind::Vec(elem) => write_named(tcx, "Vec", &[*elem], out),
        TyKind::Iterator(elem) => write_named(tcx, "Iterator", &[*elem], out),
        TyKind::Range(elem) => write_named(tcx, "Range", &[*elem], out),
        TyKind::HashMap {
            key,
            value,
            ordered,
        } => {
            let name = if *ordered { "BTreeMap" } else { "Map" };
            write_named(tcx, name, &[*key, *value], out);
        }
        TyKind::Sender(elem) => write_named(tcx, "Sender", &[*elem], out),
        TyKind::Receiver(elem) => write_named(tcx, "Receiver", &[*elem], out),
        TyKind::JoinHandle(elem) => write_named(tcx, "JoinHandle", &[*elem], out),
        TyKind::Duration => out.push_str("time::Duration"),
        TyKind::Instant => out.push_str("time::Instant"),
        TyKind::JsonValue => out.push_str("json::Value"),
        TyKind::DynValue => out.push_str("DynValue"),
        TyKind::DynError => out.push_str("errors::Error"),
        TyKind::Ref { mutability, inner } => {
            out.push_str(mutability.prefix());
            write_ty(tcx, *inner, out);
        }
        TyKind::FnPtr(sig) => write_fn_ptr(tcx, sig, "fn", out),
        TyKind::FnTrait(sig) => write_fn_ptr(tcx, sig, "Fn", out),
        TyKind::FnDef { def, substs } => write_def("fn", tcx, def.local, substs, out),
        TyKind::Closure { def, .. } => {
            let _ = write!(out, "<closure #{}>", def.local);
        }
        TyKind::Adt { def, substs } => write_def("adt", tcx, def.local, substs, out),
        TyKind::Alias { def, substs } => write_def("alias", tcx, def.local, substs, out),
        // A nominal alias is its own type, so it prints under its own
        // name. Falling back to the representation would make the two
        // sides of a mismatch read identically.
        TyKind::Nominal { def, repr } => {
            if let Some(name) = tcx.def_name(*def) {
                out.push_str(name);
            } else {
                write_ty(tcx, *repr, out);
            }
        }
        TyKind::Dyn(trait_ref) => write_dyn(tcx, trait_ref, out),
        TyKind::Var(vid) => {
            let _ = write!(out, "?{}", vid.as_u32());
        }
        TyKind::Param { name, .. } => out.push_str(name),
        TyKind::Error => out.push_str("<error>"),
    }
}

fn write_tuple(tcx: &TyCtxt, parts: &[Ty], out: &mut String) {
    out.push('(');
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_ty(tcx, *part, out);
    }
    if parts.len() == 1 {
        out.push(',');
    }
    out.push(')');
}

fn write_array(tcx: &TyCtxt, elem: Ty, len: crate::ArrayLen, out: &mut String) {
    out.push('[');
    write_ty(tcx, elem, out);
    match len {
        crate::ArrayLen::Concrete(n) => {
            let _ = write!(out, "; {n}]");
        }
        crate::ArrayLen::Param(idx) => {
            let _ = write!(out, "; N{}]", idx.as_u32());
        }
    }
}

fn write_slice(tcx: &TyCtxt, elem: Ty, out: &mut String) {
    out.push('[');
    write_ty(tcx, elem, out);
    out.push(']');
}

fn write_named(tcx: &TyCtxt, name: &str, args: &[Ty], out: &mut String) {
    out.push_str(name);
    out.push('<');
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_ty(tcx, *arg, out);
    }
    out.push('>');
}

fn write_fn_ptr(tcx: &TyCtxt, sig: &FnSig, prefix: &str, out: &mut String) {
    out.push_str(prefix);
    out.push('(');
    for (i, input) in sig.inputs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_ty(tcx, *input, out);
    }
    out.push(')');
    if !matches!(tcx.kind(sig.output), Some(TyKind::Unit)) {
        out.push_str(" -> ");
        write_ty(tcx, sig.output, out);
    }
}

fn write_def(prefix: &str, tcx: &TyCtxt, local: u32, substs: &Substs, out: &mut String) {
    let def = gossamer_resolve::DefId::local(local);
    if let Some(name) = tcx.def_name(def) {
        out.push_str(name);
    } else {
        let _ = write!(out, "{prefix}#{local}");
    }
    if !substs.is_empty() {
        write_substs(tcx, substs, out);
    }
}

fn write_substs(tcx: &TyCtxt, substs: &Substs, out: &mut String) {
    out.push('<');
    for (i, arg) in substs.as_slice().iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        match arg {
            GenericArg::Type(ty) => write_ty(tcx, *ty, out),
            GenericArg::Const(value) => {
                let _ = write!(out, "{value}");
            }
        }
    }
    out.push('>');
}

fn write_dyn(tcx: &TyCtxt, trait_ref: &TraitRef, out: &mut String) {
    let _ = write!(out, "dyn trait#{}", trait_ref.def.local);
    if !trait_ref.substs.is_empty() {
        write_substs(tcx, &trait_ref.substs, out);
    }
}

/// The name an `impl` block for a structural type registers its methods under.
///
/// A tuple has no path to name it by, so it is named by the arity every tier
/// identifies a receiver of it by: the bytecode VM does not specialise a
/// generic, so a method reached through a type parameter is dispatched there on
/// the value in hand, and that value carries its arity. Two tuple `impl` blocks
/// of one arity are the same block to such a dispatch and are reported as
/// conflicting rather than resolved by guess.
///
/// An array and a slice have no name here. A fixed array is stored inline on
/// the compiled tiers and behind a handle on the bytecode VM, so no one name
/// identifies a receiver of one on every tier; a sequence method is written on
/// `Vec` instead.
#[must_use]
pub fn structural_impl_owner(tcx: &TyCtxt, ty: Ty) -> Option<String> {
    match tcx.kind_of(ty) {
        TyKind::Tuple(parts) => Some(format!("tuple_{}", parts.len())),
        _ => None,
    }
}
