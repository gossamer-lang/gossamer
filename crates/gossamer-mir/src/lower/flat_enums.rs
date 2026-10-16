//! Which user enums are stored by value, and where their fields live.

use gossamer_hir::{HirAdtKind, HirItemKind, HirProgram};
use gossamer_resolve::DefId;
use gossamer_types::{FlatEnumLayout, Ty, TyCtxt, TyKind};

/// Registers every user enum that is stored by value in `tcx`.
///
/// An enum whose variants each hold at most one word already travels as two
/// words. Any other enum is stored by value unless it is generic, holds a
/// field of a shape that has no by-value layout, or contains itself by value,
/// directly or through other types: a recursive enum keeps its heap node,
/// whose pointer is what makes its size finite. An enum a map or set is keyed
/// by is stored by value only when its words key by content; otherwise its
/// node keys through its structural descriptor.
pub(crate) fn register_flat_enums(program: &HirProgram, tcx: &mut TyCtxt) {
    let keys = tcx.keyed_tys();
    let mut layouts = Vec::new();
    for item in &program.items {
        let HirItemKind::Adt(adt) = &item.kind else {
            continue;
        };
        if !matches!(adt.kind, HirAdtKind::Enum(_)) {
            continue;
        }
        let Some(def) = enum_def(tcx, item.def, adt.self_ty) else {
            continue;
        };
        if tcx.is_inline_enum_def(def.local) {
            continue;
        }
        let Some(variants) = tcx.enum_variant_tys(def) else {
            continue;
        };
        // An enum of unit variants is its discriminant already.
        if variants.iter().all(Vec::is_empty) {
            continue;
        }
        let variants: Vec<Vec<Ty>> = variants
            .iter()
            .map(|fields| fields.iter().map(|ty| tcx.peel_nominal(*ty)).collect())
            .collect();
        if variants
            .iter()
            .flatten()
            .any(|ty| !has_value_layout(tcx, *ty, 0))
        {
            continue;
        }
        if variants
            .iter()
            .flatten()
            .any(|ty| contains_by_value(tcx, *ty, def, &mut Vec::new()))
        {
            continue;
        }
        let keyed = keys
            .iter()
            .any(|key| contains_by_value(tcx, *key, def, &mut Vec::new()));
        if keyed
            && !variants
                .iter()
                .flatten()
                .all(|ty| keys_by_content(tcx, *ty, 0))
        {
            continue;
        }
        layouts.push((def, variants));
    }
    let word = tcx.int_ty(gossamer_types::IntTy::I64);
    let float = tcx.float_ty(gossamer_types::FloatTy::F64);
    let layouts: Vec<_> = layouts
        .into_iter()
        .map(|(def, variants)| (def, layout_of(tcx, variants, word, float)))
        .collect();
    for (def, layout) in layouts {
        tcx.register_flat_enum(def, layout);
    }
}

/// The enum an item declares: the definition the resolver gave the item, or
/// the one its type names. A package's items reach lowering with the
/// definition alone, their type left for the bodies that use them.
pub(crate) fn enum_def(tcx: &TyCtxt, def: Option<DefId>, self_ty: Ty) -> Option<DefId> {
    match tcx.kind_of(self_ty) {
        TyKind::Adt { def, substs } if substs.as_slice().is_empty() => Some(*def),
        TyKind::Adt { .. } => None,
        _ => def,
    }
}

/// Places each variant's fields in regions keyed by storage type: a field
/// takes the first region of its storage type its own variant has not taken
/// yet.
fn layout_of(tcx: &TyCtxt, variants: Vec<Vec<Ty>>, word: Ty, float: Ty) -> FlatEnumLayout {
    let mut regions: Vec<Ty> = Vec::new();
    let mut variant_regions = Vec::with_capacity(variants.len());
    for fields in &variants {
        let mut taken: Vec<u32> = Vec::with_capacity(fields.len());
        for ty in fields {
            let storage = storage_ty(tcx, *ty, word, float);
            let free = (0..regions.len() as u32)
                .find(|&region| regions[region as usize] == storage && !taken.contains(&region));
            let region = free.unwrap_or_else(|| {
                regions.push(storage);
                regions.len() as u32 - 1
            });
            taken.push(region);
        }
        variant_regions.push(taken);
    }
    FlatEnumLayout {
        regions,
        variant_regions,
        variant_field_tys: variants,
    }
}

/// The type a field of type `ty` is stored as: a one-word integer, `bool`,
/// `char`, or unit-only enum in a shared `i64`, a float in a shared `f64`,
/// and anything else as itself.
fn storage_ty(tcx: &TyCtxt, ty: Ty, word: Ty, float: Ty) -> Ty {
    match tcx.kind_of(ty) {
        TyKind::Int(_) | TyKind::Bool | TyKind::Char => word,
        TyKind::Float(_) => float,
        _ if is_unit_only_enum(tcx, ty) => word,
        _ => ty,
    }
}

/// Whether `ty` is a two-word enum none of whose variants carries a field,
/// so its value is its discriminant.
pub(crate) fn is_unit_only_enum(tcx: &TyCtxt, ty: Ty) -> bool {
    let TyKind::Adt { def, .. } = tcx.kind_of(ty) else {
        return false;
    };
    tcx.is_inline_enum_ty(ty)
        && tcx
            .enum_variant_tys(*def)
            .is_some_and(|variants| variants.iter().all(Vec::is_empty))
}

/// Whether a field of type `ty` has a by-value layout every tier agrees on.
fn has_value_layout(tcx: &TyCtxt, ty: Ty, depth: u32) -> bool {
    if depth > 16 {
        return false;
    }
    match tcx.kind_of(tcx.peel_nominal(ty)) {
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Float(_)
        | TyKind::String
        | TyKind::Slice(_)
        | TyKind::Vec(_)
        | TyKind::HashMap { .. }
        | TyKind::Sender(_)
        | TyKind::Receiver(_)
        | TyKind::JoinHandle(_)
        | TyKind::FnPtr(_)
        | TyKind::JsonValue
        | TyKind::DynError => true,
        TyKind::Tuple(elems) => {
            !elems.is_empty() && elems.iter().all(|e| has_value_layout(tcx, *e, depth + 1))
        }
        TyKind::Array { elem, .. } => has_value_layout(tcx, *elem, depth + 1),
        TyKind::Adt { def, substs } => {
            // `Option` and `Result` are two words over any payload.
            if def.local == u32::MAX || def.local == u32::MAX - 1 {
                return substs.types().iter().all(|t| {
                    matches!(tcx.kind_of(*t), TyKind::Unit) || has_value_layout(tcx, *t, depth + 1)
                });
            }
            if def.local >= u32::MAX - 16 || !substs.as_slice().is_empty() {
                return false;
            }
            if tcx.enum_variant_tys(*def).is_some() {
                // A user enum: two words, a heap node, or a value of its own.
                return true;
            }
            tcx.struct_field_tys(*def)
                .is_some_and(|fields| fields.iter().all(|f| has_value_layout(tcx, *f, depth + 1)))
        }
        _ => false,
    }
}

/// Whether a field of type `ty` keys a map by its words alone: scalars,
/// strings and sequences of scalars by content, and aggregates of those.
fn keys_by_content(tcx: &TyCtxt, ty: Ty, depth: u32) -> bool {
    if depth > 16 {
        return false;
    }
    let next = depth + 1;
    match tcx.kind_of(tcx.peel_nominal(ty)) {
        TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Float(_) | TyKind::String => true,
        TyKind::Vec(elem) | TyKind::Slice(elem) => matches!(
            tcx.kind_of(*elem),
            TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Float(_)
        ),
        TyKind::Tuple(elems) => elems.iter().all(|e| keys_by_content(tcx, *e, next)),
        TyKind::Array { elem, .. } => keys_by_content(tcx, *elem, next),
        TyKind::Adt { def, substs } => {
            // `Option` keys by its tag and payload.
            if def.local == u32::MAX - 1 {
                return substs
                    .types()
                    .iter()
                    .all(|t| keys_by_content(tcx, *t, next));
            }
            if def.local >= u32::MAX - 64 || !substs.as_slice().is_empty() {
                return false;
            }
            match tcx.enum_variant_tys(*def) {
                Some(variants) => variants
                    .iter()
                    .flatten()
                    .all(|f| keys_by_content(tcx, *f, next)),
                None => tcx
                    .struct_field_tys(*def)
                    .is_some_and(|fields| fields.iter().all(|f| keys_by_content(tcx, *f, next))),
            }
        }
        _ => false,
    }
}

/// Whether a value of `ty` holds a value of the enum `target` in place,
/// rather than behind a pointer.
fn contains_by_value(tcx: &TyCtxt, ty: Ty, target: DefId, visiting: &mut Vec<DefId>) -> bool {
    match tcx.kind_of(tcx.peel_nominal(ty)) {
        TyKind::Tuple(elems) => {
            let elems = elems.clone();
            elems
                .iter()
                .any(|e| contains_by_value(tcx, *e, target, visiting))
        }
        TyKind::Array { elem, .. } => contains_by_value(tcx, *elem, target, visiting),
        TyKind::Adt { def, substs } => {
            let (def, substs) = (*def, substs.clone());
            if def.local == u32::MAX || def.local == u32::MAX - 1 {
                return substs
                    .types()
                    .iter()
                    .any(|t| contains_by_value(tcx, *t, target, visiting));
            }
            if def == target {
                return true;
            }
            if visiting.contains(&def) {
                return false;
            }
            visiting.push(def);
            let inner: Vec<Ty> = match tcx.enum_variant_tys(def) {
                Some(variants) => variants.iter().flatten().copied().collect(),
                None => tcx
                    .struct_field_tys(def)
                    .map(<[Ty]>::to_vec)
                    .unwrap_or_default(),
            };
            inner
                .iter()
                .any(|t| contains_by_value(tcx, *t, target, visiting))
        }
        _ => false,
    }
}
