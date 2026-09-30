//! Lowering patterns.

use gossamer_ast::{
    FieldPattern as AstFieldPat, Ident, Literal as AstLiteral, Mutability, NodeId,
    Pattern as AstPat, PatternKind as AstPatKind,
};
use gossamer_lex::Span;
use gossamer_resolve::Resolution;

use crate::tree::{HirFieldPat, HirPat, HirPatKind};

use super::{Extreme, Lowerer, PatKindExt, RECURSION_LIMIT, int_extreme_literal, lower_literal};

impl Lowerer<'_> {
    pub(super) fn lower_pat(&mut self, pattern: &AstPat) -> HirPat {
        let ty = self.ty_of(pattern.id);
        self.lower_pat_with_ty(pattern, ty)
    }

    pub(super) fn lower_pat_with_ty(&mut self, pattern: &AstPat, ty: gossamer_types::Ty) -> HirPat {
        if self.recursion_depth >= RECURSION_LIMIT {
            return HirPat {
                id: self.fresh(),
                span: pattern.span,
                ty,
                kind: HirPatKind::Wildcard,
            };
        }
        self.recursion_depth += 1;
        let kind = self.lower_pat_kind(pattern, ty);
        self.recursion_depth = self.recursion_depth.saturating_sub(1);
        HirPat {
            id: self.fresh(),
            span: pattern.span,
            ty,
            kind,
        }
    }

    /// Lowers a tuple-variant / tuple-struct pattern's elements, expanding a
    /// `..` rest into the wildcards it stands for (`E::C(..)` -> two wildcards
    /// for a two-field variant). The matchers compare element-for-element, so
    /// without this a `[Rest]` pattern only matches a single-field variant.
    fn lower_variant_pat_fields(
        &mut self,
        elems: &[AstPat],
        arity: Option<usize>,
        span: Span,
        ty: gossamer_types::Ty,
    ) -> Vec<HirPat> {
        let rest_pos = elems
            .iter()
            .position(|p| matches!(p.kind, AstPatKind::Rest));
        match (rest_pos, arity) {
            (Some(pos), Some(n)) => {
                let explicit = elems.len() - 1;
                let fill = n.saturating_sub(explicit);
                let mut out = Vec::with_capacity(n.max(elems.len()));
                for (i, p) in elems.iter().enumerate() {
                    if i == pos {
                        for _ in 0..fill {
                            out.push(HirPat {
                                id: self.fresh(),
                                span,
                                ty,
                                kind: HirPatKind::Wildcard,
                            });
                        }
                    } else {
                        out.push(self.lower_pat(p));
                    }
                }
                out
            }
            // Unknown arity or no rest: lower element-for-element (a lone
            // `(..)` still matches a single-field variant, as before).
            _ => elems.iter().map(|p| self.lower_pat(p)).collect(),
        }
    }

    fn lower_pat_kind(&mut self, pattern: &AstPat, ty: gossamer_types::Ty) -> HirPatKind {
        match &pattern.kind {
            AstPatKind::Wildcard => HirPatKind::Wildcard,
            AstPatKind::Rest => HirPatKind::Rest,
            AstPatKind::Ident {
                name,
                mutability,
                subpattern,
            } => {
                let mutable = matches!(mutability, Mutability::Mutable);
                if let Some(sub) = subpattern {
                    HirPatKind::At {
                        name: name.clone(),
                        mutable,
                        sub: Box::new(self.lower_pat(sub)),
                    }
                } else {
                    HirPatKind::Binding {
                        name: name.clone(),
                        mutable,
                    }
                }
            }
            AstPatKind::Literal(lit) => HirPatKind::Literal(lower_literal(lit)),
            AstPatKind::Path(path) => self.lower_path_pat(path),
            AstPatKind::TupleStruct { path, elems } => {
                let name = path
                    .segments
                    .last()
                    .map_or_else(|| Ident::new("<error>"), |seg| seg.name.clone());
                let arity = self.ctor_arity.get(name.name.as_str()).copied();
                let fields = self.lower_variant_pat_fields(elems, arity, pattern.span, ty);
                HirPatKind::Variant { name, fields }
            }
            AstPatKind::Struct { path, fields, rest } => {
                self.lower_struct_pat(pattern.id, path, fields, *rest)
            }
            AstPatKind::Tuple(parts) => {
                HirPatKind::Tuple(parts.iter().map(|p| self.lower_pat(p)).collect())
            }
            AstPatKind::Slice {
                prefix,
                rest,
                suffix,
            } => HirPatKind::Slice {
                prefix: prefix.iter().map(|p| self.lower_pat(p)).collect(),
                rest: rest.as_ref().map(|r| Box::new(self.lower_pat(r))),
                suffix: suffix.iter().map(|p| self.lower_pat(p)).collect(),
            },
            AstPatKind::Or(alts) => {
                HirPatKind::Or(alts.iter().map(|p| self.lower_pat(p)).collect())
            }
            AstPatKind::Ref { inner, mutability } => HirPatKind::Ref {
                inner: Box::new(self.lower_pat(inner)),
                mutable: matches!(mutability, Mutability::Mutable),
            },
            AstPatKind::Range { lo, hi, kind } => {
                self.lower_range_pat(lo.as_ref(), hi.as_ref(), *kind, ty)
            }
            AstPatKind::Error => HirPatKind::Wildcard,
        }
        .erase_unused(ty)
    }

    /// Lowers a path pattern, resolving it to a const value, unit struct, or unit variant.
    fn lower_path_pat(&mut self, path: &gossamer_ast::TypePath) -> HirPatKind {
        let name = path
            .segments
            .last()
            .map_or_else(|| Ident::new("<error>"), |seg| seg.name.clone());
        // A `const` named in a pattern stands for its value, the way
        // a literal written there does. A unit variant or unit struct
        // of the same name is the nominal pattern and keeps it.
        if self.unit_structs.contains(name.name.as_str()) {
            // A unit struct has one value, so naming it is the
            // fieldless struct pattern its braced form spells.
            return HirPatKind::Struct {
                name,
                fields: Vec::new(),
                rest: false,
            };
        }
        match self.const_literals.get(name.name.as_str()) {
            Some(lit) if !self.ctor_arity.contains_key(name.name.as_str()) => {
                HirPatKind::Literal(lit.clone())
            }
            _ => HirPatKind::Variant {
                name,
                fields: Vec::new(),
            },
        }
    }

    /// Lowers a braced struct pattern, naming it by its promoted module path when it has one.
    fn lower_struct_pat(
        &mut self,
        id: NodeId,
        path: &gossamer_ast::TypePath,
        fields: &[AstFieldPat],
        rest: bool,
    ) -> HirPatKind {
        let mut name = path
            .segments
            .last()
            .map_or_else(|| Ident::new("<error>"), |seg| seg.name.clone());
        if let Some(Resolution::Def { def, .. }) = self.resolutions.get(id)
            && let Some(promoted) = self.module_fn_paths.get(&def)
            && let Some(promoted_name) = promoted.last()
        {
            name.clone_from(promoted_name);
        }
        HirPatKind::Struct {
            name,
            fields: fields.iter().map(|f| self.lower_field_pat(f)).collect(),
            rest,
        }
    }

    /// Lowers a range pattern, closing an open bound with the scrutinee type's extreme.
    fn lower_range_pat(
        &self,
        lo: Option<&AstLiteral>,
        hi: Option<&AstLiteral>,
        kind: gossamer_ast::RangeKind,
        ty: gossamer_types::Ty,
    ) -> HirPatKind {
        let inclusive = matches!(kind, gossamer_ast::RangeKind::Inclusive);
        // An open bound denotes the scrutinee type's extreme, so
        // synthesise a type-correct min/max literal and lower to a
        // closed `lo..=hi` / `lo..hi` predicate the compiled tiers
        // already handle. An open end always reaches the maximum,
        // hence inclusive of it.
        match (lo, hi) {
            (Some(lo), Some(hi)) => HirPatKind::Range {
                lo: lower_literal(lo),
                hi: lower_literal(hi),
                inclusive,
            },
            (None, Some(hi)) => HirPatKind::Range {
                lo: int_extreme_literal(self.tcx, ty, Extreme::Min),
                hi: lower_literal(hi),
                inclusive,
            },
            (Some(lo), None) => HirPatKind::Range {
                lo: lower_literal(lo),
                hi: int_extreme_literal(self.tcx, ty, Extreme::Max),
                inclusive: true,
            },
            (None, None) => HirPatKind::Wildcard,
        }
    }

    fn lower_field_pat(&mut self, field: &AstFieldPat) -> HirFieldPat {
        HirFieldPat {
            name: field.name.clone(),
            pattern: field.pattern.as_ref().map(|p| self.lower_pat(p)),
        }
    }
}
