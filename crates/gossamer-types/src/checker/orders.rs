//! Ordering a user type that declares no order.
//!
//! A struct or enum is ordered only when it says how: `#[derive(PartialOrd,
//! Ord)]` orders it by its fields in declaration order, and a `cmp` of its
//! own orders it however that `cmp` answers. Equality stays structural for
//! every type, but sorting, `<`, `min` / `max`, an ordered container's key,
//! and an `Ord` bound each need an order the type chose.

use gossamer_ast::{Item, ItemKind};
use gossamer_lex::Span;

use super::{BTREE_SET_DEF_LOCAL, DefId, TypeChecker};
use crate::{Ty, TyKind, TypeError};

impl TypeChecker<'_> {
    /// Records where a user struct or enum is declared, and whether its
    /// derives ask for an order.
    pub(super) fn note_type_decl(&mut self, item: &Item) {
        let Some(def) = self.resolutions.definition_of(item.id) else {
            return;
        };
        self.type_decl_spans.insert(def, item.span);
        let derives_order = item.attrs.outer.iter().any(|attr| {
            attr.path.segments.len() == 1
                && attr.path.segments[0].name.name == "derive"
                && attr.tokens.as_deref().is_some_and(|tokens| {
                    tokens
                        .split(',')
                        .any(|name| matches!(name.trim(), "PartialOrd" | "Ord"))
                })
        });
        if derives_order && matches!(item.kind, ItemKind::Struct(_) | ItemKind::Enum(_)) {
            self.derived_order_defs.push((def, item.span));
        }
    }

    /// Notes that `what` orders values of `ty`, checked once inference has
    /// settled the type.
    pub(super) fn defer_order(&mut self, ty: Ty, what: &str, span: Span) {
        self.deferred_orders.push((ty, what.to_string(), span));
    }

    /// Notes an expression whose type seats its contents by order - a
    /// `BTreeMap`, a `BTreeSet`, a heap - so its key or element is checked
    /// for one once inference settles.
    pub(super) fn note_ordered_container(&mut self, ty: Ty, span: Span) {
        let resolved = self.infer.resolve(self.tcx, ty);
        let (key, what) = match self.tcx.kind(resolved) {
            Some(TyKind::HashMap {
                key, ordered: true, ..
            }) => (*key, "a `BTreeMap` key"),
            Some(TyKind::Adt { def, substs }) => {
                let what = if def.local == BTREE_SET_DEF_LOCAL {
                    "a `BTreeSet` element"
                } else if matches!(self.tcx.def_name(*def), Some("MinHeap" | "MaxHeap")) {
                    "a heap element"
                } else {
                    return;
                };
                let Some(elem) = substs.types().first().copied() else {
                    return;
                };
                (elem, what)
            }
            _ => return,
        };
        if self.noted_ordered_containers.insert(resolved) {
            self.deferred_orders.push((key, what.to_string(), span));
        }
    }

    /// Notes the order a method call needs: the key a `*_by_key` call sorts
    /// or picks by, and the receiver of a two-value `min` / `max` / `clamp` /
    /// `cmp`.
    pub(super) fn note_method_order(
        &mut self,
        method: &str,
        receiver: &gossamer_ast::Expr,
        args: &[gossamer_ast::Expr],
        span: Span,
    ) {
        match (method, args) {
            ("sort_by_key" | "min_by_key" | "max_by_key", [key]) => {
                if let gossamer_ast::ExprKind::Closure { body, .. } = &key.kind
                    && let Some(ty) = self.table.get(body.id)
                {
                    self.defer_order(ty, &format!("`{method}`"), span);
                }
            }
            ("min" | "max" | "cmp" | "partial_cmp", [_]) | ("clamp", [_, _]) => {
                if let Some(ty) = self.table.get(receiver.id) {
                    self.defer_order(ty, &format!("`{method}`"), span);
                }
            }
            _ => {}
        }
    }

    /// Whether the user type `def` declares an order: a derived or written
    /// `cmp`.
    pub(super) fn user_type_has_order(&self, def: DefId) -> bool {
        if self.custom_cmp_defs.contains(&def) {
            return true;
        }
        self.tcx.def_name(def).is_some_and(|name| {
            self.method_homes
                .contains_key(&(name.to_string(), "cmp".to_string()))
        })
    }

    /// The first user type inside `ty` that declares no order, looking
    /// through references, sequences, tuples, `Option`, `Result`, and the
    /// type arguments of an ordered user type.
    pub(super) fn unordered_user_type(&mut self, ty: Ty) -> Option<DefId> {
        self.unordered_user_type_within(ty, &mut Vec::new())
    }

    fn unordered_user_type_within(&mut self, ty: Ty, seen: &mut Vec<Ty>) -> Option<DefId> {
        let ty = self.infer.resolve(self.tcx, ty);
        if seen.contains(&ty) {
            return None;
        }
        seen.push(ty);
        let inner: Vec<Ty> = match self.tcx.kind(ty)?.clone() {
            TyKind::Ref { inner, .. }
            | TyKind::Vec(inner)
            | TyKind::Slice(inner)
            | TyKind::Array { elem: inner, .. } => vec![inner],
            TyKind::Tuple(elems) => elems,
            TyKind::Adt { def, substs } => {
                let user = self.user_type_defs.values().any(|d| *d == def);
                if user && !self.user_type_has_order(def) {
                    return Some(def);
                }
                substs.types()
            }
            _ => return None,
        };
        inner
            .into_iter()
            .find_map(|t| self.unordered_user_type_within(t, seen))
    }

    /// GT0143 for each recorded ordering of a user type with no order, and
    /// for each derived order over a field that has none.
    pub(super) fn check_deferred_orders(&mut self) {
        let mut reported: std::collections::HashSet<(DefId, Span)> =
            std::collections::HashSet::new();
        for (ty, what, span) in std::mem::take(&mut self.deferred_orders) {
            let Some(def) = self.unordered_user_type(ty) else {
                continue;
            };
            if reported.insert((def, span)) {
                self.report_missing_order(def, what, span);
            }
        }
        for (def, span) in std::mem::take(&mut self.derived_order_defs) {
            let fields: Vec<Ty> = if let Some(fields) = self.tcx.struct_field_tys(def) {
                fields.to_vec()
            } else if let Some(variants) = self.tcx.enum_variant_tys(def) {
                variants.iter().flatten().copied().collect()
            } else {
                continue;
            };
            let owner = self
                .tcx
                .def_name(def)
                .map(crate::printer::public_type_name)
                .map(std::borrow::Cow::into_owned)
                .unwrap_or_default();
            for field in fields {
                if let Some(missing) = self.unordered_user_type(field)
                    && missing != def
                {
                    self.report_missing_order(
                        missing,
                        format!("the order `#[derive(PartialOrd, Ord)]` gives `{owner}`"),
                        span,
                    );
                    break;
                }
            }
        }
    }

    pub(super) fn report_missing_order(&mut self, def: DefId, what: String, span: Span) {
        let ty = self
            .tcx
            .def_name(def)
            .map(crate::printer::public_type_name)
            .map(std::borrow::Cow::into_owned)
            .unwrap_or_default();
        let decls = self
            .types_needing_order(def)
            .into_iter()
            .filter_map(|def| self.type_decl_spans.get(&def).copied())
            .collect();
        self.emit(TypeError::MissingOrder { ty, what, decls }, span);
    }

    /// `def` and every user type with no order its fields reach, each of
    /// which a derived order on `def` needs ordered too.
    fn types_needing_order(&mut self, def: DefId) -> Vec<DefId> {
        let mut out = vec![def];
        let mut next = 0;
        while let Some(&current) = out.get(next) {
            next += 1;
            let fields: Vec<Ty> = if let Some(fields) = self.tcx.struct_field_tys(current) {
                fields.to_vec()
            } else if let Some(variants) = self.tcx.enum_variant_tys(current) {
                variants.iter().flatten().copied().collect()
            } else {
                Vec::new()
            };
            for field in fields {
                let mut seen = Vec::new();
                self.collect_unordered(field, &mut seen, &mut out);
            }
        }
        out
    }

    fn collect_unordered(&mut self, ty: Ty, seen: &mut Vec<Ty>, out: &mut Vec<DefId>) {
        let ty = self.infer.resolve(self.tcx, ty);
        if seen.contains(&ty) {
            return;
        }
        seen.push(ty);
        let inner: Vec<Ty> = match self.tcx.kind(ty).cloned() {
            Some(
                TyKind::Ref { inner, .. }
                | TyKind::Vec(inner)
                | TyKind::Slice(inner)
                | TyKind::Array { elem: inner, .. },
            ) => vec![inner],
            Some(TyKind::Tuple(elems)) => elems,
            Some(TyKind::Adt { def, substs }) => {
                let user = self.user_type_defs.values().any(|d| *d == def);
                if user && !self.user_type_has_order(def) && !out.contains(&def) {
                    out.push(def);
                }
                substs.types()
            }
            _ => Vec::new(),
        };
        for t in inner {
            self.collect_unordered(t, seen, out);
        }
    }
}
