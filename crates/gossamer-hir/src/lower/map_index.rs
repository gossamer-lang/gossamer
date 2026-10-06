//! Desugaring map index reads and writes and `entry`-style updates.

use gossamer_ast::{AssignOp, Expr as AstExpr, ExprKind as AstExprKind, Ident};
use gossamer_lex::Span;
use gossamer_types::Ty;

use crate::tree::{
    HirBlock, HirExpr, HirExprKind, HirLiteral, HirMatchArm, HirPat, HirPatKind, HirStmt,
    HirStmtKind,
};

use super::Lowerer;

/// A map entry opened for a write: the place inside the entry's value the
/// write names, and one frame per entry on the path, outermost first.
pub(super) struct OpenEntry {
    pub(super) target: HirExpr,
    frames: Vec<EntryFrame>,
}

/// One map entry a write passes through.
struct EntryFrame {
    /// Binds the key, evaluated once, before the entry is taken.
    key_stmt: HirStmt,
    /// The map the entry lives in, as a place.
    map_place: HirExpr,
    key_name: String,
    key_ty: Ty,
    slot_name: String,
    value_ty: Ty,
    /// The new value for a plain store to the entry itself, which reads
    /// nothing; otherwise the stored value is taken out of the map.
    initial: Option<HirExpr>,
    /// Binds each index on the way down from the entry, evaluated once.
    index_stmts: Vec<HirStmt>,
}

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
        let option_value = self.option_of(value_ty);
        let bind_key = self.entry_let_stmt(span, "__gos_map_key", key_ty, false, key);
        let key_arg = self.entry_path(span, "__gos_map_key", key_ty);
        let found = self.method_call(map, "get", vec![key_arg], option_value, span);
        let value = self.entry_path(span, "__gos_map_value", value_ty);
        let missing = self.missing_key_panic("__gos_map_key", key_ty, span);
        let lookup = self.option_match(
            found,
            value_ty,
            "__gos_map_value",
            false,
            value,
            missing,
            span,
        );
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

    /// `Option<value_ty>`.
    fn option_of(&mut self, value_ty: Ty) -> Ty {
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs: gossamer_types::Substs::from_types([value_ty]),
        })
    }

    /// `match scrutinee { Some(binding) => present, None => missing }`,
    /// typed as `present`.
    #[allow(
        clippy::too_many_arguments,
        reason = "each argument is one part of the match the caller spells"
    )]
    fn option_match(
        &mut self,
        scrutinee: HirExpr,
        value_ty: Ty,
        binding: &str,
        mutable: bool,
        present: HirExpr,
        missing: HirExpr,
        span: Span,
    ) -> HirExpr {
        let option_value = self.option_of(value_ty);
        let ty = present.ty;
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
                        name: Ident::new(binding),
                        mutable,
                    },
                }],
            },
        };
        let missing_pat = HirPat {
            id: self.fresh(),
            span,
            ty: option_value,
            kind: HirPatKind::Variant {
                name: Ident::new("None"),
                fields: Vec::new(),
            },
        };
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms: vec![
                    HirMatchArm {
                        pattern: value_pat,
                        guard: None,
                        body: present,
                    },
                    HirMatchArm {
                        pattern: missing_pat,
                        guard: None,
                        body: missing,
                    },
                ],
            },
        }
    }

    /// The panic a read of a key the map lacks raises, naming the key bound
    /// to `key_name`.
    fn missing_key_panic(&mut self, key_name: &str, key_ty: Ty, span: Span) -> HirExpr {
        let string_ty = self.tcx.string_ty();
        let never = self.tcx.never();
        let key = self.entry_path(span, key_name, key_ty);
        let shown = self.builtin_call("__debug", vec![key], string_ty, span);
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
        self.builtin_call("panic", vec![message], never, span)
    }

    /// `m[k] = v`, `m[k] op= v`, and a write to any place below an entry
    /// (`m[k].field op= v`, `m[k][i] = v`, `m[a][b] = v`): the key evaluates
    /// once, the stored value is read (a missing key panics, except for a
    /// plain `=` on the entry itself, which inserts), updated, and stored
    /// back.
    pub(super) fn lower_map_index_assign(
        &mut self,
        op: AssignOp,
        place: &AstExpr,
        value: &AstExpr,
        span: Span,
    ) -> Option<HirExprKind> {
        let (_, _, _, steps) = self.split_map_place(place)?;
        let initial = (steps.is_empty() && matches!(op, AssignOp::Assign)).then_some(value);
        if initial.is_some() {
            let entry = self.open_entry(place, initial, span)?;
            return Some(self.close_entry(entry, Vec::new(), None, span).kind);
        }
        // The right-hand side is evaluated before the entry opens, so it reads
        // the map as it stands.
        let (bind, written) = self.bind_entry_operand(value, span);
        let entry = self.open_entry(place, None, span)?;
        let target = entry.target.clone();
        let kind = self.assign_kind(op, target, written, span);
        let write = self.unit_stmt(kind, span);
        let closed = self.close_entry(entry, vec![write], None, span);
        Some(self.prefixed(bind, closed, span).kind)
    }

    /// `{ stmts; expr }`.
    pub(super) fn prefixed(&mut self, stmts: Vec<HirStmt>, expr: HirExpr, span: Span) -> HirExpr {
        let ty = expr.ty;
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts,
                tail: Some(Box::new(expr)),
                ty,
                is_comptime: false,
            }),
        }
    }

    /// The map index nearest the written leaf of `place`, as the index
    /// expression itself, the map, the key, and the projections from that
    /// entry down to the leaf (outermost first). `None` when no map index is
    /// on the path.
    pub(super) fn split_map_place<'p>(
        &mut self,
        place: &'p AstExpr,
    ) -> Option<(&'p AstExpr, &'p AstExpr, &'p AstExpr, Vec<&'p AstExpr>)> {
        let mut cur = place;
        let mut steps = Vec::new();
        loop {
            match &cur.kind {
                AstExprKind::FieldAccess { receiver, .. } => {
                    steps.push(cur);
                    cur = receiver;
                }
                AstExprKind::Index { base, index } => {
                    if matches!(index.kind, AstExprKind::Range { .. }) {
                        return None;
                    }
                    let base_ty = self.ty_of(base.id);
                    if self.is_map_ty(base_ty) {
                        return Some((cur, base, index, steps));
                    }
                    steps.push(cur);
                    cur = base;
                }
                _ => return None,
            }
        }
    }

    /// Opens the map entry a write to `place` goes through, and every entry
    /// enclosing it: answers the place inside the entry's value the write
    /// names. [`Self::close_entry`] wraps the write in the code that takes
    /// each value out of its map and stores it back.
    pub(super) fn open_entry(
        &mut self,
        place: &AstExpr,
        initial: Option<&AstExpr>,
        span: Span,
    ) -> Option<OpenEntry> {
        let (entry, map, key, steps) = self.split_map_place(place)?;
        let value_ty = self.ty_of(entry.id);
        let tag = self.fresh().0;
        let key_name = format!("__gos_map_slot_key_{tag}");
        let slot_name = format!("__gos_map_slot_{tag}");
        let (mut frames, map_place) = match self.open_entry(map, None, span) {
            Some(outer) => (outer.frames, outer.target),
            None => (Vec::new(), self.lower_expr(map)),
        };
        let key = self.lower_expr(key);
        let key_ty = key.ty;
        let key_stmt = self.entry_let_stmt(span, &key_name, key_ty, false, key);
        let initial = initial.map(|value| self.lower_expr(value));
        let mut index_stmts = Vec::new();
        let mut target = self.entry_path(span, &slot_name, value_ty);
        for step in steps.iter().rev() {
            let ty = self.ty_of(step.id);
            target = match &step.kind {
                AstExprKind::Index { index, .. } => {
                    let index = self.lower_expr(index);
                    let index_ty = index.ty;
                    let index_name = format!("__gos_map_slot_index_{}", self.fresh().0);
                    index_stmts.push(self.entry_let_stmt(
                        span,
                        &index_name,
                        index_ty,
                        false,
                        index,
                    ));
                    let index = self.entry_path(span, &index_name, index_ty);
                    HirExpr {
                        id: self.fresh(),
                        span,
                        ty,
                        kind: HirExprKind::Index {
                            base: Box::new(target),
                            index: Box::new(index),
                        },
                    }
                }
                _ => self.project_entry_value(target, &[*step], span),
            };
        }
        frames.push(EntryFrame {
            key_stmt,
            map_place,
            key_name,
            key_ty,
            slot_name,
            value_ty,
            initial,
            index_stmts,
        });
        Some(OpenEntry { target, frames })
    }

    /// Runs `writes` against an opened entry, then `result`, and answers
    /// `result`'s value once every entry is back in its map.
    ///
    /// Each value leaves its map while the write runs (a missing key
    /// panics), bound in the arm that takes it, so the write lands in place
    /// rather than in a copy; the store back puts it where it was, since a
    /// map iterates in key order.
    pub(super) fn close_entry(
        &mut self,
        entry: OpenEntry,
        writes: Vec<HirStmt>,
        result: Option<HirExpr>,
        span: Span,
    ) -> HirExpr {
        let unit = self.unit();
        let mut stmts = writes;
        let mut tail = result;
        for frame in entry.frames.into_iter().rev() {
            let result_ty = tail.as_ref().map_or(unit, |t| t.ty);
            let result_name = format!("__gos_map_result_{}", self.fresh().0);
            let mut body = frame.index_stmts;
            body.extend(stmts);
            // A unit answer runs as a statement, so an in-place mutation
            // (`push`) lowers to its statement form on every tier.
            if let Some(value) = tail.take() {
                if result_ty == unit {
                    body.push(HirStmt {
                        id: self.fresh(),
                        span,
                        kind: HirStmtKind::Expr {
                            expr: value,
                            has_semi: true,
                        },
                    });
                } else {
                    body.push(self.entry_let_stmt(span, &result_name, result_ty, false, value));
                }
            }
            let key = self.entry_path(span, &frame.key_name, frame.key_ty);
            let slot = self.entry_path(span, &frame.slot_name, frame.value_ty);
            let insert = self.method_call(
                frame.map_place.clone(),
                "insert",
                vec![key, slot],
                unit,
                span,
            );
            body.push(self.unit_stmt(insert.kind, span));
            let answer =
                (result_ty != unit).then(|| self.entry_path(span, &result_name, result_ty));
            let arm = HirExpr {
                id: self.fresh(),
                span,
                ty: result_ty,
                kind: HirExprKind::Block(HirBlock {
                    id: self.fresh(),
                    span,
                    stmts: body,
                    tail: answer.map(Box::new),
                    ty: result_ty,
                    is_comptime: false,
                }),
            };
            let opened = if let Some(value) = frame.initial {
                let bind = self.entry_let_stmt(span, &frame.slot_name, frame.value_ty, true, value);
                self.prefixed(vec![bind], arm, span)
            } else {
                let option_value = self.option_of(frame.value_ty);
                let key = self.entry_path(span, &frame.key_name, frame.key_ty);
                let taken =
                    self.method_call(frame.map_place, "remove", vec![key], option_value, span);
                let missing = self.missing_key_panic(&frame.key_name, frame.key_ty, span);
                self.option_match(
                    taken,
                    frame.value_ty,
                    &frame.slot_name,
                    true,
                    arm,
                    missing,
                    span,
                )
            };
            stmts = vec![frame.key_stmt];
            tail = Some(opened);
        }
        let ty = tail.as_ref().map_or(unit, |t| t.ty);
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts,
                tail: tail.map(Box::new),
                ty,
                is_comptime: false,
            }),
        }
    }

    /// A unit-typed expression statement.
    fn unit_stmt(&mut self, kind: HirExprKind, span: Span) -> HirStmt {
        let unit = self.unit();
        HirStmt {
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
        }
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

    /// Desugars `m[k].method(args)`, or the same on any place below an
    /// entry (`m[k].items.push(x)`, `m[k][i].tags.push(x)`), into a read of
    /// the stored value (a missing key panics), the call on it, and a store
    /// back, so the mutation lands in the map.
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
        let span = expr.span;
        let outer_ty = self.ty_of(expr.id);
        self.split_map_place(receiver)?;
        // The arguments are evaluated before the entry opens, so they read
        // the map as it stands.
        let mut binds = Vec::new();
        let mut lowered_args = Vec::with_capacity(args.len());
        for arg in args {
            let (bind, value) = self.bind_entry_operand(arg, span);
            binds.extend(bind);
            lowered_args.push(value);
        }
        let entry = self.open_entry(receiver, None, span)?;
        let target = entry.target.clone();
        let call = self.method_call(target, name.name.as_str(), lowered_args, outer_ty, span);
        let closed = self.close_entry(entry, Vec::new(), Some(call), span);
        Some(self.prefixed(binds, closed, span))
    }

    /// Evaluates `operand` into a local of its own, answering the binding
    /// and a path naming it.
    pub(super) fn bind_entry_operand(
        &mut self,
        operand: &AstExpr,
        span: Span,
    ) -> (Vec<HirStmt>, HirExpr) {
        let lowered = self.lower_expr(operand);
        let ty = lowered.ty;
        let name = format!("__gos_map_operand_{}", self.fresh().0);
        let bind = self.entry_let_stmt(span, &name, ty, false, lowered);
        (vec![bind], self.entry_path(span, &name, ty))
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
