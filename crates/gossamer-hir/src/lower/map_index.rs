//! Desugaring map index reads and writes and `entry`-style updates.

use gossamer_ast::{AssignOp, Expr as AstExpr, ExprKind as AstExprKind, Ident};
use gossamer_lex::Span;
use gossamer_types::Ty;

use crate::tree::{
    HirBlock, HirExpr, HirExprKind, HirLiteral, HirMatchArm, HirPat, HirPatKind, HirStmt,
    HirStmtKind,
};

use super::Lowerer;

impl Lowerer<'_> {
    /// `m[k]` read: the stored value, or a panic naming the missing key.
    pub(super) fn map_index_read(
        &mut self,
        map: HirExpr,
        key: HirExpr,
        value_ty: Ty,
        span: Span,
    ) -> HirExpr {
        let key_ty = key.ty;
        let string_ty = self.tcx.string_ty();
        let never = self.tcx.never();
        let option_value = self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs: gossamer_types::Substs::from_types([value_ty]),
        });
        let bind_key = self.entry_let_stmt(span, "__gos_map_key", key_ty, false, key);
        let key_arg = self.entry_path(span, "__gos_map_key", key_ty);
        let found = self.method_call(map, "get", vec![key_arg], option_value, span);
        let value_pat = HirPat {
            id: self.fresh(),
            span,
            ty: option_value,
            kind: HirPatKind::Variant {
                name: Ident::new("Some"),
                fields: vec![HirPat {
                    id: self.fresh(),
                    span,
                    ty: value_ty,
                    kind: HirPatKind::Binding {
                        name: Ident::new("__gos_map_value"),
                        mutable: false,
                    },
                }],
            },
        };
        let value = self.entry_path(span, "__gos_map_value", value_ty);
        let missing_pat = HirPat {
            id: self.fresh(),
            span,
            ty: option_value,
            kind: HirPatKind::Variant {
                name: Ident::new("None"),
                fields: Vec::new(),
            },
        };
        let key_again = self.entry_path(span, "__gos_map_key", key_ty);
        let shown = self.builtin_call("__debug", vec![key_again], string_ty, span);
        let prefix = HirExpr {
            id: self.fresh(),
            span,
            ty: string_ty,
            kind: HirExprKind::Literal(HirLiteral::String("key ".to_string())),
        };
        let suffix = HirExpr {
            id: self.fresh(),
            span,
            ty: string_ty,
            kind: HirExprKind::Literal(HirLiteral::String(" is not in the map".to_string())),
        };
        let message = self.builtin_call("__concat", vec![prefix, shown, suffix], string_ty, span);
        let missing = self.builtin_call("panic", vec![message], never, span);
        let lookup = HirExpr {
            id: self.fresh(),
            span,
            ty: value_ty,
            kind: HirExprKind::Match {
                scrutinee: Box::new(found),
                arms: vec![
                    HirMatchArm {
                        pattern: value_pat,
                        guard: None,
                        body: value,
                    },
                    HirMatchArm {
                        pattern: missing_pat,
                        guard: None,
                        body: missing,
                    },
                ],
            },
        };
        HirExpr {
            id: self.fresh(),
            span,
            ty: value_ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts: vec![bind_key],
                tail: Some(Box::new(lookup)),
                ty: value_ty,
                is_comptime: false,
            }),
        }
    }

    /// `m[k] = v`, `m[k] op= v`, and `m[k].field op= v`: the key evaluates
    /// once, the stored value is read (a missing key panics, except for a
    /// plain `=`, which inserts), updated, and stored back.
    pub(super) fn lower_map_index_assign(
        &mut self,
        op: AssignOp,
        place: &AstExpr,
        value: &AstExpr,
        span: Span,
    ) -> Option<HirExprKind> {
        let mut root = place;
        let mut projections: Vec<&AstExpr> = Vec::new();
        while let AstExprKind::FieldAccess { receiver, .. } = &root.kind {
            projections.push(root);
            root = receiver;
        }
        let AstExprKind::Index { base, index } = &root.kind else {
            return None;
        };
        let base_ty = self.ty_of(base.id);
        if !self.is_map_ty(base_ty) {
            return None;
        }
        let unit = self.unit();
        let value_ty = self.ty_of(root.id);
        let key = self.lower_expr(index);
        let key_ty = key.ty;
        let bind_key = self.entry_let_stmt(span, "__gos_map_slot_key", key_ty, false, key);
        let mut stmts = vec![bind_key];
        let stored = if projections.is_empty() && matches!(op, AssignOp::Assign) {
            self.lower_expr(value)
        } else {
            let map = self.lower_expr(base);
            let key_arg = self.entry_path(span, "__gos_map_slot_key", key_ty);
            let current = self.map_index_read(map, key_arg, value_ty, span);
            stmts.push(self.entry_let_stmt(span, "__gos_map_slot", value_ty, true, current));
            let slot = self.entry_path(span, "__gos_map_slot", value_ty);
            let target = self.project_entry_value(slot, &projections, span);
            let written = self.lower_expr(value);
            let kind = self.assign_kind(op, target, written, span);
            stmts.push(HirStmt {
                id: self.fresh(),
                span,
                kind: HirStmtKind::Expr {
                    expr: HirExpr {
                        id: self.fresh(),
                        span,
                        ty: unit,
                        kind,
                    },
                    has_semi: true,
                },
            });
            self.entry_path(span, "__gos_map_slot", value_ty)
        };
        let map = self.lower_expr(base);
        let key_arg = self.entry_path(span, "__gos_map_slot_key", key_ty);
        let insert = self.method_call(map, "insert", vec![key_arg, stored], unit, span);
        stmts.push(HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr: insert,
                has_semi: true,
            },
        });
        Some(HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts,
            tail: None,
            ty: unit,
            is_comptime: false,
        }))
    }

    fn entry_let_stmt(
        &mut self,
        span: Span,
        name: &str,
        ty: gossamer_types::Ty,
        mutable: bool,
        init: HirExpr,
    ) -> HirStmt {
        HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern: HirPat {
                    id: self.fresh(),
                    span,
                    ty,
                    kind: HirPatKind::Binding {
                        name: Ident::new(name),
                        mutable,
                    },
                },
                ty,
                init: Some(init),
            },
        }
    }

    /// A bare path expression referencing one of the entry bindings.
    pub(super) fn entry_path(&mut self, span: Span, name: &str, ty: gossamer_types::Ty) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        }
    }

    /// The shared prelude of the entry desugars:
    /// `let __entry_k = k; let mut __entry_v = option::unwrap_or(d, m.get(__entry_k))`.
    /// The get-or-default shape materialises an inline aggregate local
    /// on every tier (the `or_insert` shims carry scalar values only).
    fn entry_prelude(
        &mut self,
        span: Span,
        key: HirExpr,
        default: HirExpr,
        map: HirExpr,
        value_ty: gossamer_types::Ty,
    ) -> (HirStmt, HirStmt) {
        let key_ty = key.ty;
        let k_let = self.entry_let_stmt(span, "__entry_k", key_ty, false, key);
        let k_for_get = self.entry_path(span, "__entry_k", key_ty);
        let value_substs = gossamer_types::Substs::from_types([value_ty]);
        let option_value_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs: value_substs,
        });
        let get_call = HirExpr {
            id: self.fresh(),
            span,
            ty: option_value_ty,
            kind: HirExprKind::MethodCall {
                receiver: Box::new(map),
                name: Ident::new("get"),
                args: vec![k_for_get],
                owner: None,
            },
        };
        let default_callee = HirExpr {
            id: self.fresh(),
            span,
            ty: value_ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new("option"), Ident::new("unwrap_or")],
                def: None,
            },
        };
        let get_or_default = HirExpr {
            id: self.fresh(),
            span,
            ty: value_ty,
            kind: HirExprKind::Call {
                callee: Box::new(default_callee),
                args: vec![default, get_call],
            },
        };
        let v_let = self.entry_let_stmt(span, "__entry_v", value_ty, true, get_or_default);
        (k_let, v_let)
    }

    /// The write-back call of the entry desugars:
    /// `m.insert(__entry_k, __entry_v)`.
    fn entry_insert_call(
        &mut self,
        span: Span,
        map: HirExpr,
        key_ty: gossamer_types::Ty,
        value_ty: gossamer_types::Ty,
    ) -> HirExpr {
        let k = self.entry_path(span, "__entry_k", key_ty);
        let v = self.entry_path(span, "__entry_v", value_ty);
        let unit_ty = self.unit();
        HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::MethodCall {
                receiver: Box::new(map),
                name: Ident::new("insert"),
                args: vec![k, v],
                owner: None,
            },
        }
    }

    /// Desugars a value-position `m.or_insert(k, d)` whose VALUE type is
    /// an aggregate into `{ let __k = k; let mut __v = get-or-default;
    /// m.insert(__k, __v); __v }`. The scalar shims store an 8-byte
    /// value word; an aggregate default's stack word stored raw leaves
    /// the map pointing at a dead frame slot, while get / default /
    /// insert all carry aggregates correctly on every tier.
    /// The ordered surface of a `BTreeMap` in terms of two primitives every
    /// tier implements: `__window(lo, hi, take)`, a new map of the entries
    /// ranked `lo..hi` (a negative `lo` counts back from the end; `take`
    /// moves them out of the receiver), and `__range(lo, hi, mode)`, a new
    /// map of the entries between two keys. Each walks the result with the
    /// map's own `iter()`, so the receiver is evaluated once.
    pub(super) fn desugar_btree_map_method(
        &mut self,
        expr: &AstExpr,
        receiver: &AstExpr,
        method: &str,
        args: &[AstExpr],
    ) -> Option<HirExprKind> {
        use gossamer_types::{IntTy, TyKind};
        let mut map_ty = self.ty_of(receiver.id);
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(map_ty) {
            map_ty = *inner;
        }
        // A `BTreeMap` walks `(K, V)` pairs; a `BTreeSet` walks its elements.
        let entry = match self.tcx.kind_of(map_ty) {
            TyKind::HashMap {
                key,
                value,
                ordered: true,
            } => {
                let (key, value) = (*key, *value);
                self.tcx.intern(TyKind::Tuple(vec![key, value]))
            }
            TyKind::Adt { def, substs }
                if self.tcx.def_name(*def) == Some("BTreeSet")
                    && matches!(
                        method,
                        "first" | "last" | "pop_first" | "pop_last" | "range"
                    ) =>
            {
                substs.types().first().copied()?
            }
            _ => return None,
        };
        let span = expr.span;
        let ty = self.ty_of(expr.id);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let iter_ty = self.tcx.intern(TyKind::Iterator(entry));
        let (window, take) = match (method, args) {
            ("first_key_value" | "first", []) => ((0, 1), 0),
            ("last_key_value" | "last", []) => ((-1, i64::MAX), 0),
            ("pop_first", []) => ((0, 1), 1),
            ("pop_last", []) => ((-1, i64::MAX), 1),
            ("range", [arg]) => {
                let AstExprKind::Range { start, end, kind } = &arg.kind else {
                    return None;
                };
                let recv = self.lower_expr(receiver);
                let inclusive = matches!(kind, gossamer_ast::RangeKind::Inclusive);
                let slice = match (start, end) {
                    (None, None) => recv,
                    (Some(lo), Some(hi)) => {
                        let (lo, hi) = (self.lower_expr(lo), self.lower_expr(hi));
                        let mode = self.int_lit(3 | if inclusive { 4 } else { 0 }, i64_ty, span);
                        self.method_call(recv, "__range", vec![lo, hi, mode], map_ty, span)
                    }
                    (Some(bound), None) | (None, Some(bound)) => {
                        let bound = self.lower_expr(bound);
                        let mode = if start.is_some() {
                            1
                        } else {
                            2 | if inclusive { 4 } else { 0 }
                        };
                        let bound_ty = bound.ty;
                        let bind =
                            self.entry_let_stmt(span, "__range_bound", bound_ty, false, bound);
                        let lo = self.entry_path(span, "__range_bound", bound_ty);
                        let hi = self.entry_path(span, "__range_bound", bound_ty);
                        let mode = self.int_lit(mode, i64_ty, span);
                        let call =
                            self.method_call(recv, "__range", vec![lo, hi, mode], map_ty, span);
                        let walk = self.method_call(call, "iter", Vec::new(), iter_ty, span);
                        return Some(HirExprKind::Block(HirBlock {
                            id: self.fresh(),
                            span,
                            stmts: vec![bind],
                            tail: Some(Box::new(walk)),
                            ty: iter_ty,
                            is_comptime: false,
                        }));
                    }
                };
                return Some(
                    self.method_call(slice, "iter", Vec::new(), iter_ty, span)
                        .kind,
                );
            }
            _ => return None,
        };
        let recv = self.lower_expr(receiver);
        let window_args = vec![
            self.int_lit(window.0, i64_ty, span),
            self.int_lit(window.1, i64_ty, span),
            self.int_lit(take, i64_ty, span),
        ];
        let slice = self.method_call(recv, "__window", window_args, map_ty, span);
        let walk = self.method_call(slice, "iter", Vec::new(), iter_ty, span);
        Some(self.method_call(walk, "next", Vec::new(), ty, span).kind)
    }

    /// `receiver.name(args)` typed `ty`.
    fn method_call(
        &mut self,
        receiver: HirExpr,
        name: &str,
        args: Vec<HirExpr>,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::MethodCall {
                receiver: Box::new(receiver),
                name: Ident::new(name),
                args,
                owner: None,
            },
        }
    }

    /// An integer literal typed `ty`.
    fn int_lit(&mut self, value: i64, ty: Ty, span: Span) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Literal(HirLiteral::Int(value.to_string())),
        }
    }

    pub(super) fn desugar_or_insert_value(&mut self, expr: &AstExpr) -> Option<HirExpr> {
        let AstExprKind::MethodCall {
            receiver: map_expr,
            name,
            args,
            ..
        } = &expr.kind
        else {
            return None;
        };
        if name.name.as_str() != "or_insert" || args.len() != 2 {
            return None;
        }
        if !matches!(map_expr.kind, AstExprKind::Path(_)) {
            return None;
        }
        let map_ty = self.ty_of(map_expr.id);
        let gossamer_types::TyKind::HashMap { value, .. } = self.tcx.kind_of(map_ty) else {
            return None;
        };
        // Struct / tuple values only: the scalar shims store their
        // stack word raw (a dead frame slot once the statement ends).
        // Vec-valued maps keep the engineered borrow path (`or_insert`
        // returns an alias of the stored vec, marked borrowed so
        // teardown frees it exactly once); scalars keep the fast shims.
        let value_kind = self.tcx.kind_of(*value);
        if !matches!(
            value_kind,
            gossamer_types::TyKind::Adt { .. } | gossamer_types::TyKind::Tuple(_)
        ) {
            return None;
        }
        let span = expr.span;
        let value_ty = self.ty_of(expr.id);
        let key = self.lower_expr(&args[0]);
        let default = self.lower_expr(&args[1]);
        // Each receiver mention lowers separately so no two tree
        // positions share a HirId.
        let map = self.lower_expr(map_expr);
        let map_again = self.lower_expr(map_expr);
        let key_ty = key.ty;
        let (k_let, v_let) = self.entry_prelude(span, key, default, map, value_ty);
        let insert_call = self.entry_insert_call(span, map_again, key_ty, value_ty);
        let insert_stmt = HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr: insert_call,
                has_semi: true,
            },
        };
        let v_tail = self.entry_path(span, "__entry_v", value_ty);
        Some(HirExpr {
            id: self.fresh(),
            span,
            ty: value_ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts: vec![k_let, v_let, insert_stmt],
                tail: Some(Box::new(v_tail)),
                ty: value_ty,
                is_comptime: false,
            }),
        })
    }

    /// Desugars the statement `m.or_insert(k, d).method(args)`, the same
    /// through a field path (`m.or_insert(k, d).items.push(x)`), or an
    /// assignment to a field path (`m.or_insert(k, d).n += 1`), on a
    /// HashMap-typed simple-place receiver into an explicit write-back:
    ///
    /// ```text
    /// { let __entry_k = k; let mut __entry_v = get-or-default;
    ///   __entry_v.method(args); m.insert(__entry_k, __entry_v) }
    /// ```
    ///
    /// so the mutation lands in the map's stored value (the map's value
    /// semantics hand `or_insert` callers a copy). The key evaluates
    /// once; the receiver must be a bare path so its re-evaluation is
    /// the same place. `None` leaves the statement to the normal
    /// lowering.
    pub(super) fn desugar_or_insert_mutation(&mut self, expr: &AstExpr) -> Option<HirExpr> {
        // `m.or_insert(k, d).method(..)`, the same through a field path
        // (`m.or_insert(k, d).items.push(x)`), or an assignment to a field
        // path (`m.or_insert(k, d).n += 1`): the projections between the
        // entry and the mutated place, outermost first.
        let (mut entry, mut projections): (&AstExpr, Vec<&AstExpr>) = match &expr.kind {
            AstExprKind::MethodCall { receiver, .. } => (receiver, Vec::new()),
            AstExprKind::Assign { place, .. }
                if matches!(place.kind, AstExprKind::FieldAccess { .. }) =>
            {
                (place, Vec::new())
            }
            _ => return None,
        };
        while let AstExprKind::FieldAccess { receiver, .. } = &entry.kind {
            projections.push(entry);
            entry = receiver;
        }
        let AstExprKind::MethodCall {
            receiver: map_expr,
            name: inner_name,
            args: inner_args,
            ..
        } = &entry.kind
        else {
            return None;
        };
        if inner_name.name.as_str() != "or_insert" || inner_args.len() != 2 {
            return None;
        }
        if !matches!(map_expr.kind, AstExprKind::Path(_)) {
            return None;
        }
        let map_ty = self.ty_of(map_expr.id);
        let gossamer_types::TyKind::HashMap { value, .. } = self.tcx.kind_of(map_ty) else {
            return None;
        };
        // Struct / tuple values only - the copy-then-lose shape this
        // write-back exists for. Vec values keep the engineered borrow
        // path; scalar values have no mutating methods to chain.
        if !matches!(
            self.tcx.kind_of(*value),
            gossamer_types::TyKind::Adt { .. } | gossamer_types::TyKind::Tuple(_)
        ) {
            return None;
        }
        let span = expr.span;
        let key = self.lower_expr(&inner_args[0]);
        let default = self.lower_expr(&inner_args[1]);
        // Each receiver mention lowers separately so no two tree
        // positions share a HirId.
        let map = self.lower_expr(map_expr);
        let map_again = self.lower_expr(map_expr);
        let key_ty = key.ty;
        let value_ty = self.ty_of(entry.id);
        let outer_ty = self.ty_of(expr.id);
        let unit_ty = self.unit();
        let (k_let, v_let) = self.entry_prelude(span, key, default, map, value_ty);
        let entry_v = self.entry_path(span, "__entry_v", value_ty);
        let v_for_call = self.project_entry_value(entry_v, &projections, span);
        let mutate_kind = match &expr.kind {
            AstExprKind::MethodCall { name, args, .. } => HirExprKind::MethodCall {
                receiver: Box::new(v_for_call),
                name: name.clone(),
                args: args.iter().map(|a| self.lower_expr(a)).collect(),
                owner: None,
            },
            AstExprKind::Assign { op, value, .. } => {
                let value = self.lower_expr(value);
                self.assign_kind(*op, v_for_call, value, span)
            }
            _ => return None,
        };
        let mutate_call = HirExpr {
            id: self.fresh(),
            span,
            ty: outer_ty,
            kind: mutate_kind,
        };
        let mutate_stmt = HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr: mutate_call,
                has_semi: true,
            },
        };
        let insert_call = self.entry_insert_call(span, map_again, key_ty, value_ty);
        Some(HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts: vec![k_let, v_let, mutate_stmt],
                tail: Some(Box::new(insert_call)),
                ty: unit_ty,
                is_comptime: false,
            }),
        })
    }

    /// Desugars the statement `m[k].method(args)`, or the same through a
    /// field path (`m[k].items.push(x)`), into a read of the stored value
    /// (a missing key panics), the call on it, and a store back, so the
    /// mutation lands in the map.
    pub(super) fn desugar_map_index_mutation(&mut self, expr: &AstExpr) -> Option<HirExpr> {
        let AstExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &expr.kind
        else {
            return None;
        };
        let mut root = &**receiver;
        let mut projections: Vec<&AstExpr> = Vec::new();
        while let AstExprKind::FieldAccess { receiver, .. } = &root.kind {
            projections.push(root);
            root = receiver;
        }
        let AstExprKind::Index { base, index } = &root.kind else {
            return None;
        };
        let base_ty = self.ty_of(base.id);
        if !self.is_map_ty(base_ty) {
            return None;
        }
        let span = expr.span;
        let unit = self.unit();
        let value_ty = self.ty_of(root.id);
        let outer_ty = self.ty_of(expr.id);
        let key = self.lower_expr(index);
        let key_ty = key.ty;
        let bind_key = self.entry_let_stmt(span, "__gos_map_slot_key", key_ty, false, key);
        let map = self.lower_expr(base);
        let key_arg = self.entry_path(span, "__gos_map_slot_key", key_ty);
        let current = self.map_index_read(map, key_arg, value_ty, span);
        let bind_slot = self.entry_let_stmt(span, "__gos_map_slot", value_ty, true, current);
        let slot = self.entry_path(span, "__gos_map_slot", value_ty);
        let target = self.project_entry_value(slot, &projections, span);
        let lowered_args: Vec<HirExpr> = args.iter().map(|a| self.lower_expr(a)).collect();
        let call = self.method_call(target, name.name.as_str(), lowered_args, outer_ty, span);
        let slot = self.entry_path(span, "__gos_map_slot", value_ty);
        let map = self.lower_expr(base);
        let key_arg = self.entry_path(span, "__gos_map_slot_key", key_ty);
        let insert = self.method_call(map, "insert", vec![key_arg, slot], unit, span);
        let stmt = |this: &mut Self, expr: HirExpr| HirStmt {
            id: this.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr,
                has_semi: true,
            },
        };
        let call = stmt(self, call);
        let insert = stmt(self, insert);
        Some(HirExpr {
            id: self.fresh(),
            span,
            ty: unit,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts: vec![bind_key, bind_slot, call, insert],
                tail: None,
                ty: unit,
                is_comptime: false,
            }),
        })
    }

    /// `value` reached through `projections` (outermost first), the field
    /// path an entry mutation names below `m.or_insert(k, d)`.
    fn project_entry_value(
        &mut self,
        value: HirExpr,
        projections: &[&AstExpr],
        span: Span,
    ) -> HirExpr {
        let mut projected = value;
        for projection in projections.iter().rev() {
            let AstExprKind::FieldAccess { receiver, field } = &projection.kind else {
                continue;
            };
            let tuple_struct = matches!(field, gossamer_ast::FieldSelector::Index(_))
                && self.receiver_is_tuple_struct(receiver);
            let kind = match field {
                gossamer_ast::FieldSelector::Named(name) => HirExprKind::Field {
                    receiver: Box::new(projected),
                    name: name.clone(),
                },
                gossamer_ast::FieldSelector::Index(idx) if tuple_struct => HirExprKind::Field {
                    receiver: Box::new(projected),
                    name: gossamer_ast::Ident::new(idx.to_string()),
                },
                gossamer_ast::FieldSelector::Index(idx) => HirExprKind::TupleIndex {
                    receiver: Box::new(projected),
                    index: *idx,
                },
            };
            projected = HirExpr {
                id: self.fresh(),
                span,
                ty: self.ty_of(projection.id),
                kind,
            };
        }
        projected
    }

    pub(super) fn placeholder_expr(&mut self, span: Span) -> HirExpr {
        let ty = self.unit();
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Placeholder,
        }
    }
}
