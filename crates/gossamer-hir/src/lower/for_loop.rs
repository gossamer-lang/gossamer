//! Lowering `for` loops over sequences, ranges, lazy chains, and user iterators.

use gossamer_ast::{Expr as AstExpr, Ident, NodeId, Pattern as AstPat};
use gossamer_lex::Span;

use crate::tree::{
    HirBlock, HirExpr, HirExprKind, HirLiteral, HirMatchArm, HirPat, HirPatKind, HirStmt,
    HirStmtKind,
};

use super::{BTREE_SET_DEF_LOCAL, HASH_SET_DEF_LOCAL, Lowerer};

impl Lowerer<'_> {
    pub(super) fn lower_for(
        &mut self,
        pattern: &AstPat,
        iter: &AstExpr,
        body: &AstExpr,
        label: Option<String>,
        span: Span,
    ) -> HirExprKind {
        let mut iter_expr = self.lower_expr(iter);
        // A reference to a `String` walks the text it names, so the cursor is
        // chosen from the referent's type. Reading the reference's own type
        // here left the loop on the generic sequence walk, which hands the
        // body each scalar as an integer rather than a `char`.
        let mut iter_referent = iter_expr.ty;
        while let Some(gossamer_types::TyKind::Ref { inner, .. }) = self.tcx.kind(iter_referent) {
            iter_referent = *inner;
        }
        if matches!(
            self.tcx.kind(iter_referent),
            Some(gossamer_types::TyKind::String)
        ) {
            let char_ty = self.tcx.char_ty();
            // `chars()` answers a cursor, and the loop drives it.
            let collection_ty = self.tcx.intern(gossamer_types::TyKind::Iterator(char_ty));
            iter_expr = HirExpr {
                id: self.fresh(),
                span: iter_expr.span,
                ty: collection_ty,
                kind: HirExprKind::MethodCall {
                    receiver: Box::new(iter_expr),
                    name: Ident::new("chars"),
                    args: Vec::new(),
                    owner: None,
                },
            };
        } else if let HirExprKind::Range { start, .. } = &mut iter_expr.kind
            && start.is_none()
        {
            let zero = HirExpr {
                id: self.fresh(),
                span: iter_expr.span,
                ty: self.tcx.int_ty(gossamer_types::IntTy::I64),
                kind: HirExprKind::Literal(HirLiteral::Int("0".to_string())),
            };
            *start = Some(Box::new(zero));
        }
        let iter_ty = iter_expr.ty;
        // For unknown / Adt iter types the canonical desugar
        // needs to bind the iter to a fresh slot and call
        // `.next()` on `&mut` of that slot - that's the
        // mechanism user `impl Iterator for T` relies on for
        // its state to persist across iterations. For built-in
        // iter shapes (ranges, arrays, vecs), the MIR fast paths
        // walk the receiver expression directly, so we keep the
        // inline shape that those detectors recognise.
        // Lazy iterator state is a cursor: it must be bound once and advanced,
        // since re-evaluating the expression that built it would hand the loop
        // a fresh cursor on every turn. An adapter chain is observable - its
        // callbacks run as elements are pulled, and a `break` ends the pulling
        // - so the loop drives it one element per turn through `next()`. A
        // syntactic range keeps its counted inline loop, and a bare `.iter()`
        // over a collection keeps the indexed walk over its source: neither
        // has an adapter whose work the walk could reorder.
        let lazy_state_route = self.iter_expr_is_lazy_chain(&iter_expr);
        let needs_state_binding = lazy_state_route
            || self.iter_needs_state_binding(iter_ty)
            || Self::iter_expr_is_temporary_sequence(&iter_expr);
        if needs_state_binding {
            return self.lower_for_user_iter(pattern, iter_expr, body, label, span);
        }
        self.lower_for_inline(pattern, iter_expr, body, label, span)
    }

    /// `true` when the loop's iterable is a freshly built sequence with
    /// no home to index - a literal, or an `iter()` / `enumerate()`
    /// chain over one. Such a value must be bound before the loop; the
    /// inline shape leaves the compiled tiers indexing a temporary that
    /// no longer exists.
    fn iter_expr_is_temporary_sequence(iter_expr: &HirExpr) -> bool {
        let mut cur = iter_expr;
        loop {
            match &cur.kind {
                HirExprKind::Array(_) => return true,
                HirExprKind::MethodCall {
                    receiver,
                    name,
                    args,
                    owner: None,
                } if args.is_empty() && (name.name == "iter" || name.name == "enumerate") => {
                    cur = receiver;
                }
                _ => return false,
            }
        }
    }

    /// Desugars `for x in iter` to the canonical `loop { match
    /// (&mut __for_iter).next() { Some(x) => body, None => break } }`.
    /// Used when the iter expression is a user struct / unknown
    /// type - those need state persistence across `next()` calls.
    fn lower_for_user_iter(
        &mut self,
        pattern: &AstPat,
        iter_expr: HirExpr,
        body: &AstExpr,
        label: Option<String>,
        span: Span,
    ) -> HirExprKind {
        let iter_ty = iter_expr.ty;
        let iter_local_id = self.fresh();
        let iter_pat = HirPat {
            id: self.fresh(),
            span,
            ty: iter_ty,
            kind: HirPatKind::Binding {
                name: Ident::new(crate::fuse::FOR_ITER),
                mutable: true,
            },
        };
        let iter_let = HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern: iter_pat,
                ty: iter_ty,
                init: Some(iter_expr),
            },
        };
        let iter_path = HirExpr {
            id: iter_local_id,
            span,
            ty: iter_ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(crate::fuse::FOR_ITER)],
                def: None,
            },
        };
        let iter_ref = HirExpr {
            id: self.fresh(),
            span,
            ty: iter_ty,
            kind: HirExprKind::Unary {
                op: crate::tree::HirUnaryOp::RefMut,
                operand: Box::new(iter_path),
            },
        };
        let next_call = HirExpr {
            id: self.fresh(),
            span,
            ty: self.error_ty(),
            kind: HirExprKind::MethodCall {
                receiver: Box::new(iter_ref),
                name: Ident::new("next"),
                args: Vec::new(),
                owner: None,
            },
        };
        let loop_expr = self.assemble_for_loop(pattern, next_call, body, label, span);
        let outer_block = HirBlock {
            id: self.fresh(),
            span,
            stmts: vec![iter_let],
            tail: Some(Box::new(loop_expr)),
            ty: self.unit(),
            is_comptime: false,
        };
        HirExprKind::Block(outer_block)
    }

    /// Inline shape - `loop { match <iter>.next() { ... } }`. The
    /// MIR / interp for-loop fast-paths inspect `<iter>` directly,
    /// so for built-in iterables (ranges, slices, vecs) we keep
    /// the receiver expression in place rather than introducing a
    /// `__for_iter` binding the detectors don't recognise.
    fn lower_for_inline(
        &mut self,
        pattern: &AstPat,
        iter_expr: HirExpr,
        body: &AstExpr,
        label: Option<String>,
        span: Span,
    ) -> HirExprKind {
        let next_call = HirExpr {
            id: self.fresh(),
            span,
            ty: self.error_ty(),
            kind: HirExprKind::MethodCall {
                receiver: Box::new(iter_expr),
                name: Ident::new("next"),
                args: Vec::new(),
                owner: None,
            },
        };
        self.assemble_for_loop(pattern, next_call, body, label, span)
            .kind
    }

    /// Splits a `for` pattern into the shape every backend's loop walks - a
    /// binding, `_`, or a tuple of those - and the `let` statements that
    /// destructure the rest at the top of the body. A struct, nested tuple,
    /// or other compound element is bound whole and destructured by an
    /// irrefutable `let`, which every tier already lowers.
    fn flatten_for_pattern(&mut self, pat: HirPat) -> (HirPat, Vec<HirStmt>) {
        let simple =
            |p: &HirPat| matches!(p.kind, HirPatKind::Binding { .. } | HirPatKind::Wildcard);
        let mut lets = Vec::new();
        match pat.kind {
            HirPatKind::Binding { .. } | HirPatKind::Wildcard => (pat, lets),
            HirPatKind::Tuple(elems) => {
                let mut flat = Vec::with_capacity(elems.len());
                for (index, elem) in elems.into_iter().enumerate() {
                    if simple(&elem) {
                        flat.push(elem);
                    } else {
                        let name = format!("{}{index}", crate::fuse::FOR_ELEM);
                        flat.push(self.for_elem_binding(&name, &elem));
                        lets.push(self.for_elem_let(&name, elem));
                    }
                }
                let kind = HirPatKind::Tuple(flat);
                (HirPat { kind, ..pat }, lets)
            }
            kind => {
                let whole = HirPat { kind, ..pat };
                let binding = self.for_elem_binding(crate::fuse::FOR_ELEM, &whole);
                lets.push(self.for_elem_let(crate::fuse::FOR_ELEM, whole));
                (binding, lets)
            }
        }
    }

    /// A binding named `name` of `like`'s type, for a `for` element taken
    /// whole.
    fn for_elem_binding(&mut self, name: &str, like: &HirPat) -> HirPat {
        HirPat {
            id: self.fresh(),
            span: like.span,
            ty: like.ty,
            kind: HirPatKind::Binding {
                name: Ident::new(name),
                mutable: false,
            },
        }
    }

    /// `let pattern = name`, destructuring a `for` element bound whole.
    fn for_elem_let(&mut self, name: &str, pattern: HirPat) -> HirStmt {
        let (ty, span) = (pattern.ty, pattern.span);
        let init = HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        };
        HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern,
                ty,
                init: Some(init),
            },
        }
    }

    /// Shared builder: wraps a `match scrutinee { Some(pat) =>
    /// body, None => break }` in a `loop` whose body is one Block.
    fn assemble_for_loop(
        &mut self,
        pattern: &AstPat,
        next_call: HirExpr,
        body: &AstExpr,
        label: Option<String>,
        span: Span,
    ) -> HirExpr {
        let written_pat = self.lower_pat(pattern);
        let (loop_pat, destructures) = self.flatten_for_pattern(written_pat);
        let pat_ty = loop_pat.ty;
        let some_pat = HirPat {
            id: self.fresh(),
            span,
            ty: pat_ty,
            kind: HirPatKind::Variant {
                name: Ident::new("Some"),
                fields: vec![loop_pat],
            },
        };
        let none_pat = HirPat {
            id: self.fresh(),
            span,
            ty: pat_ty,
            kind: HirPatKind::Variant {
                name: Ident::new("None"),
                fields: Vec::new(),
            },
        };
        let written_body = self.lower_expr(body);
        let body_expr = if destructures.is_empty() {
            written_body
        } else {
            let (body_ty, body_span) = (written_body.ty, written_body.span);
            HirExpr {
                id: self.fresh(),
                span: body_span,
                ty: body_ty,
                kind: HirExprKind::Block(HirBlock {
                    id: self.fresh(),
                    span: body_span,
                    stmts: destructures,
                    tail: Some(Box::new(written_body)),
                    ty: body_ty,
                    is_comptime: false,
                }),
            }
        };
        let unit_ty = self.unit();
        let break_expr = HirExpr {
            id: self.fresh(),
            span,
            ty: self.tcx.never(),
            kind: HirExprKind::Break {
                value: None,
                label: None,
            },
        };
        let match_expr = HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::Match {
                scrutinee: Box::new(next_call),
                arms: vec![
                    HirMatchArm {
                        pattern: some_pat,
                        guard: None,
                        body: body_expr,
                    },
                    HirMatchArm {
                        pattern: none_pat,
                        guard: None,
                        body: break_expr,
                    },
                ],
            },
        };
        let inner_block = HirBlock {
            id: self.fresh(),
            span,
            stmts: Vec::new(),
            tail: Some(Box::new(match_expr)),
            ty: unit_ty,
            is_comptime: false,
        };
        let body_block = HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::Block(inner_block),
        };
        HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::Loop {
                body: Box::new(body_block),
                label,
            },
        }
    }

    /// Returns `true` when an iter expression of type `ty` needs
    /// the `let mut __for_iter = ...` binding so `.next()` calls
    /// can persist state. `Adt` (user struct) and `Var(_)` shapes
    /// take the state path; ranges / arrays / vecs / slices /
    /// `HashMap`s stay inline so the MIR fast-paths can recognise
    /// the receiver expression directly.
    /// Whether a `for` iterable is an `Iterator` built in place by an adapter
    /// or a cursor-producing call, whose elements have to be pulled one per
    /// turn. A name already holds state the loop advances where it is, and a
    /// bare `.iter()` over a collection has no adapter to observe.
    fn iter_expr_is_lazy_chain(&self, iter_expr: &HirExpr) -> bool {
        use gossamer_types::TyKind;
        if !matches!(self.tcx.kind(iter_expr.ty), Some(TyKind::Iterator(_))) {
            return false;
        }
        match &iter_expr.kind {
            HirExprKind::Path { .. } | HirExprKind::Range { .. } | HirExprKind::Unary { .. } => {
                false
            }
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } if name.name == "iter" && args.is_empty() => {
                matches!(self.tcx.kind(receiver.ty), Some(TyKind::Iterator(_)))
            }
            _ => true,
        }
    }

    /// The type of a receiver that is an `i8`, `i16`, or `i32`.
    pub(super) fn narrow_signed_int_of(&mut self, receiver: NodeId) -> Option<gossamer_types::Ty> {
        use gossamer_types::{IntTy, TyKind};
        let mut ty = self.table.get(receiver)?;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
            ty = *inner;
        }
        matches!(
            self.tcx.kind(ty),
            Some(TyKind::Int(IntTy::I8 | IntTy::I16 | IntTy::I32))
        )
        .then_some(ty)
    }

    fn iter_needs_state_binding(&self, ty: gossamer_types::Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        for _ in 0..8 {
            match self.tcx.kind(cur) {
                Some(TyKind::Ref { inner, .. }) => cur = *inner,
                // `HashSet` / `BTreeSet` sentinels are not
                // stateful iterator: it snapshots to a sorted Vec on the
                // inline path (VM and compiled both materialise `to_vec`),
                // so keep it off the `&mut __for_iter.next()` desugar that
                // a real `impl Iterator` struct needs.
                Some(TyKind::Adt { def, .. }) => {
                    return !matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL);
                }
                // A type parameter's shape is only known per instantiation,
                // so it advances through `.next()`, the one protocol every
                // iterable answers. Its bound guarantees that method exists.
                Some(TyKind::Param { .. }) => return true,
                _ => return false,
            }
        }
        false
    }
}
