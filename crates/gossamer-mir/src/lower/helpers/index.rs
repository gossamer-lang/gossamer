#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::if_not_else)]
#![allow(clippy::single_match_else)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::redundant_else)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::single_match)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::let_and_return)]
#![allow(clippy::needless_collect)]

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_hir::{
    HirAdtKind, HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind,
    HirLiteral, HirMatchArm, HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind, HirUnaryOp,
};
use gossamer_lex::Span;
use gossamer_types::{Ty, TyCtxt};

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};

use super::*;

pub(crate) enum MapValueKind {
    I64,
    String,
    Bytes,
    Other,
}

pub(crate) enum MapKeyKind {
    I64,
    String,
    Other,
}

pub(crate) fn map_value_kind_from(tcx: &gossamer_types::TyCtxt, ty: Ty) -> MapValueKind {
    use gossamer_types::TyKind;
    match tcx.kind_of(ty) {
        TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_) => MapValueKind::I64,
        TyKind::String => MapValueKind::String,
        TyKind::Vec(elem) | TyKind::Slice(elem)
            if matches!(tcx.kind_of(*elem), TyKind::Int(gossamer_types::IntTy::U8)) =>
        {
            MapValueKind::Bytes
        }
        _ => MapValueKind::Other,
    }
}

pub(crate) fn map_key_kind_from(tcx: &gossamer_types::TyCtxt, ty: Ty) -> MapKeyKind {
    use gossamer_types::TyKind;
    match tcx.kind_of(ty) {
        TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_) => MapKeyKind::I64,
        TyKind::String => MapKeyKind::String,
        // An enum whose variants all carry nothing is a discriminant and
        // nothing else, so it keys a map the way any other integer does. A
        // payload-bearing enum is a node, and content-hashes through its
        // structural descriptor instead.
        TyKind::Adt { .. } if is_unit_only_enum(tcx, ty) => MapKeyKind::I64,
        _ => MapKeyKind::Other,
    }
}

/// Whether `ty` is a user enum represented as a bare discriminant word.
fn is_unit_only_enum(tcx: &gossamer_types::TyCtxt, ty: Ty) -> bool {
    use gossamer_types::TyKind;
    let TyKind::Adt { def, .. } = tcx.kind_of(ty) else {
        return false;
    };
    tcx.enum_variant_names(*def).is_some() && !tcx.is_rc_managed(ty) && !tcx.is_inline_enum_ty(ty)
}

pub(crate) struct EnumIndex {
    pub(crate) by_enum: HashMap<String, Vec<String>>,
    pub(crate) variant_index: HashMap<String, (String, usize)>,
    /// `variant_name -> [field_name]` for struct-payload variants.
    /// Lets `__struct("Rect", "w", v, "h", v)` calls resolve their
    /// declaration order even when `Rect` is an enum variant rather
    /// than a free struct.
    pub(crate) variant_fields: HashMap<String, Vec<String>>,
    /// `variant_name -> [field_ty]`, parallel to `variant_fields`.
    /// Used by struct-pattern matching so `Shape::Rect { w, h }`
    /// declares `w` / `h` MIR locals at the right MIR type - the
    /// generic `gos_load` helper returns i64 and a missing
    /// f64-typed binding made the cranelift codegen skip the
    /// I64→F64 bitcast in `define_var_to_with`.
    pub(crate) variant_field_tys: HashMap<String, Vec<Ty>>,
    /// The same field types keyed by `Enum::Variant`, so a variant name two
    /// enums both declare resolves to the one the value's type names.
    pub(crate) variant_field_tys_owned: HashMap<String, Vec<Ty>>,
    /// `variant_name -> bool` - true when the variant carries any
    /// payload (struct fields OR tuple-payload constructor calls
    /// observed elsewhere in the program). Match dispatch keys off
    /// this to decide whether the scrutinee is a heap pointer.
    pub(crate) variant_has_payload: HashMap<String, bool>,
    /// `enum_name -> bool` - true when a variant OF THAT ENUM carries a
    /// payload. A variant name may be declared by several enums, so the
    /// per-variant map above cannot answer this question for one of them.
    pub(crate) enum_any_payload: HashMap<String, bool>,
}

impl EnumIndex {
    /// Resolves an enum-variant path / bare name to `(enum_name,
    /// variant_index)`. Accepts the bare name `Green` when the variant
    /// name is unambiguous across the program, and any path ending in
    /// `Color::Green` - so a module-qualified `shapes::Shape::Circle`
    /// resolves to the same variant the in-module `Shape::Circle` does.
    /// The enum index is keyed by bare enum name, so a path whose
    /// second-to-last segment names no enum still resolves to `None`.
    pub(crate) fn lookup(&self, segments: &[Ident]) -> Option<(String, usize)> {
        match segments {
            [single] => self.variant_index.get(&single.name).cloned(),
            [leading @ .., enum_seg, variant_seg] => {
                // An enum's identity carries the modules containing it, so a
                // written `a::Tag::Two` names the enum `a::Tag`. Prefer that
                // key, then the bare spelling for a root-level enum reached
                // through some other path.
                let qualified: String = leading
                    .iter()
                    .map(|segment| segment.name.as_str())
                    .filter(|segment| !matches!(*segment, "crate" | "self" | "super" | "root"))
                    .chain(std::iter::once(enum_seg.name.as_str()))
                    .collect::<Vec<_>>()
                    .join("::");
                let (enum_name, variants) = match self.by_enum.get(&qualified) {
                    Some(variants) => (qualified, variants),
                    None => (enum_seg.name.clone(), self.by_enum.get(&enum_seg.name)?),
                };
                let idx = variants.iter().position(|v| v == &variant_seg.name)?;
                Some((enum_name, idx))
            }
            [] => None,
        }
    }

    /// The variant's index within the named enum. A variant name can be
    /// declared by more than one enum, so a caller that knows the value's
    /// type resolves through this rather than through the program-wide
    /// variant map.
    pub(crate) fn variant_of_enum(&self, enum_name: &str, variant: &str) -> Option<usize> {
        self.by_enum
            .get(enum_name)?
            .iter()
            .position(|v| v == variant)
    }

    /// The declared field types of one variant, resolved through its own
    /// enum when the caller knows it.
    pub(crate) fn field_tys_of(&self, enum_name: &str, variant: &str) -> Option<Vec<Ty>> {
        self.variant_field_tys_owned
            .get(&format!("{enum_name}::{variant}"))
            .or_else(|| self.variant_field_tys.get(variant))
            .cloned()
    }

    /// True when any variant of the named enum carries a payload.
    pub(crate) fn enum_has_any_payload(&self, enum_name: &str) -> bool {
        self.enum_any_payload
            .get(enum_name)
            .copied()
            .unwrap_or(false)
    }

    /// Returns true when ANY variant of the enum that contains
    /// `segments` carries fields. Used by match dispatch to decide
    /// whether the scrutinee is a heap pointer (load disc from
    /// offset 0) or a flat i64 (variant index inline).
    pub(crate) fn has_any_payload(&self, segments: &[Ident]) -> bool {
        let Some((enum_name, _)) = self.lookup(segments) else {
            return false;
        };
        let Some(variants) = self.by_enum.get(&enum_name) else {
            return false;
        };
        variants
            .iter()
            .any(|v| self.variant_has_payload.get(v).copied().unwrap_or(false))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VecElemKind {
    /// Element / map-key type is `String` (or `&String`).
    Str,
    /// Default - anything else; treated as i64-shaped at the FFI.
    Int,
}

pub(crate) fn vec_element_kind(tcx: &gossamer_types::TyCtxt, ty: Ty) -> VecElemKind {
    use gossamer_types::TyKind;
    // See through `&` / `&mut`: a sequence reached through a reference has
    // the same element type it does directly. Reading the reference itself
    // answers `Int` for a `Vec<String>`, which routes `contains` / `index_of`
    // / `count_of` to the integer helper - and that compares the slot words,
    // so it matches only when both sides are the same interned literal and
    // reports a string built at run time as absent.
    let mut cur = ty;
    let inner = loop {
        match tcx.kind_of(cur) {
            TyKind::Ref { inner, .. } => cur = *inner,
            TyKind::Vec(inner) | TyKind::Slice(inner) => break *inner,
            TyKind::Array { elem, .. } => break *elem,
            _ => return VecElemKind::Int,
        }
    };
    if matches!(tcx.kind_of(inner), TyKind::String) {
        VecElemKind::Str
    } else {
        VecElemKind::Int
    }
}

pub(crate) fn hashmap_key_kind(tcx: &gossamer_types::TyCtxt, ty: Ty) -> VecElemKind {
    use gossamer_types::TyKind;
    // See through `&` / `&mut`: a map reached through a reference has the
    // same key type it does directly, and reading the reference itself
    // answers `Int` for a string-keyed map - which routes the call to the
    // integer-key runtime helper, so the lookup finds nothing.
    let mut cur = ty;
    loop {
        match tcx.kind_of(cur) {
            TyKind::Ref { inner, .. } => cur = *inner,
            TyKind::HashMap { key, .. } => {
                return if matches!(tcx.kind_of(*key), TyKind::String) {
                    VecElemKind::Str
                } else {
                    VecElemKind::Int
                };
            }
            _ => return VecElemKind::Int,
        }
    }
}

pub(crate) fn arg_is_float(tcx: &gossamer_types::TyCtxt, expr: &HirExpr) -> bool {
    use gossamer_types::TyKind;
    matches!(tcx.kind_of(expr.ty), TyKind::Float(_))
}

/// True when `expr` types as `u64` / `usize` (peeling references). Used by
/// the `min`/`max`/`clamp` dispatch: those words order unsigned, so they take
/// the unsigned helpers and keep their own type.
pub(crate) fn arg_is_unsigned64(tcx: &gossamer_types::TyCtxt, expr: &HirExpr) -> bool {
    use gossamer_types::{IntTy, TyKind};
    let mut walk = expr.ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(walk) {
        walk = *inner;
    }
    matches!(tcx.kind_of(walk), TyKind::Int(IntTy::U64 | IntTy::Usize))
}

/// True when `expr` types as `char` (peeling references). Used by the
/// `min`/`max`/`clamp` dispatch to keep the result `char`-typed - the
/// codepoint compares correctly as an i64, but the result must print as a
/// character, not its codepoint integer.
pub(crate) fn arg_is_char(tcx: &gossamer_types::TyCtxt, expr: &HirExpr) -> bool {
    use gossamer_types::TyKind;
    let mut walk = expr.ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(walk) {
        walk = *inner;
    }
    matches!(tcx.kind_of(walk), TyKind::Char)
}

/// True when `expr` types as `Vec<u8>` / `&Vec<u8>` / `&[u8]` /
/// `[u8]` - used by the `os::write_file` dispatcher to pick the
/// bytes-shaped runtime helper for binary writes (preserves NUL
/// bytes that the c-string variant would truncate at).
pub(crate) fn is_vec_u8_arg(tcx: &gossamer_types::TyCtxt, expr: &HirExpr) -> bool {
    use gossamer_types::{IntTy, TyKind};
    let mut walk = expr.ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(walk) {
        walk = *inner;
    }
    let inner = match tcx.kind_of(walk) {
        TyKind::Vec(t) | TyKind::Slice(t) => *t,
        _ => return false,
    };
    matches!(tcx.kind_of(inner), TyKind::Int(IntTy::U8))
}
