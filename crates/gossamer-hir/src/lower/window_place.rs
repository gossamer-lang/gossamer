//! Writes through a range of a sequence: `xs[lo..hi][i] = v`.
//!
//! `xs[lo..hi]` read as a value is a copy, but written through it names the
//! elements of `xs` it covers, as `xs[lo..hi].sort()` sorts them in place.
//! A place that indexes, or reaches a field through, such a window is
//! rewritten to the element of the sequence itself: the window's bounds clamp
//! as a slice's do, an index past the window's end panics as indexing the
//! window would, and the write lands at the window's start plus the index.
//! Every tier then writes the same element.

use gossamer_lex::Span;
use gossamer_types::{IntTy, Ty, TyKind};

use crate::tree::{HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirStmt, HirStmtKind, HirUnaryOp};

use super::Lowerer;

impl Lowerer<'_> {
    /// Whether `place` writes through a range of a sequence.
    pub(super) fn writes_through_window(&self, place: &HirExpr) -> bool {
        match &place.kind {
            HirExprKind::Index { base, .. } => {
                self.window_parts(base).is_some() || self.writes_through_window(base)
            }
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                self.writes_through_window(receiver)
            }
            _ => false,
        }
    }

    /// `place` rewritten to name the sequence's own element, with the
    /// statements that bind the window bounds and check the index pushed to
    /// `stmts`. Every index expression on the way is bound first, so each
    /// runs once.
    pub(super) fn flatten_window_place(
        &mut self,
        place: HirExpr,
        stmts: &mut Vec<HirStmt>,
    ) -> HirExpr {
        let HirExpr { id, span, ty, kind } = place;
        let kind = match kind {
            HirExprKind::Index { base, index } => {
                if let Some((sequence, range)) = self.take_window(*base.clone()) {
                    let (sequence, offset, count) = self.window_view(sequence, range, stmts, span);
                    let index = self.bind_index(*index, stmts);
                    let int = self.tcx.int_ty(IntTy::I64);
                    stmts.push(self.window_bounds_guard(&index, &count, span));
                    let offset = self.conv_path(&offset, int, span);
                    let position = self.binary(HirBinaryOp::Add, offset, index, int, span);
                    return HirExpr {
                        id,
                        span,
                        ty,
                        kind: HirExprKind::Index {
                            base: Box::new(sequence),
                            index: Box::new(position),
                        },
                    };
                }
                let base = self.flatten_window_place(*base, stmts);
                let index = self.bind_index(*index, stmts);
                HirExprKind::Index {
                    base: Box::new(base),
                    index: Box::new(index),
                }
            }
            HirExprKind::Field { receiver, name } => HirExprKind::Field {
                receiver: Box::new(self.flatten_window_place(*receiver, stmts)),
                name,
            },
            HirExprKind::TupleIndex { receiver, index } => HirExprKind::TupleIndex {
                receiver: Box::new(self.flatten_window_place(*receiver, stmts)),
                index,
            },
            other => other,
        };
        HirExpr { id, span, ty, kind }
    }

    /// The sequence and range of `base` when it is `seq[range]` or
    /// `&mut seq[range]` over a `Vec`, a fixed array, or a slice.
    fn window_parts<'e>(&self, base: &'e HirExpr) -> Option<(&'e HirExpr, &'e HirExpr)> {
        let inner = match &base.kind {
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand,
            } => operand.as_ref(),
            _ => base,
        };
        let HirExprKind::Index {
            base: sequence,
            index,
        } = &inner.kind
        else {
            return None;
        };
        if !matches!(index.kind, HirExprKind::Range { .. }) {
            return None;
        }
        let mut ty = sequence.ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
            ty = *inner;
        }
        matches!(
            self.tcx.kind(ty),
            Some(TyKind::Vec(_) | TyKind::Array { .. } | TyKind::Slice(_))
        )
        .then_some((sequence.as_ref(), index.as_ref()))
    }

    fn take_window(&self, base: HirExpr) -> Option<(HirExpr, HirExpr)> {
        let (sequence, range) = self.window_parts(&base)?;
        Some((sequence.clone(), range.clone()))
    }

    /// `index` bound to a fresh name unless it is already a name or a
    /// literal, so the rewritten place evaluates it once.
    fn bind_index(&mut self, index: HirExpr, stmts: &mut Vec<HirStmt>) -> HirExpr {
        if matches!(
            index.kind,
            HirExprKind::Path { .. } | HirExprKind::Literal(_)
        ) {
            return index;
        }
        let name = format!("__window_index_{}", self.fresh().0);
        let ty = index.ty;
        let span = index.span;
        stmts.push(self.conv_let(&name, index, span));
        self.conv_path(&name, ty, span)
    }

    /// The window `sequence[range]` as the place of the sequence it covers,
    /// the name bound to the window's first position in it, and the name
    /// bound to the window's length. A window of a window composes: its
    /// bounds clamp to the outer window and its offset adds to the outer one.
    fn window_view(
        &mut self,
        sequence: HirExpr,
        range: HirExpr,
        stmts: &mut Vec<HirStmt>,
        span: Span,
    ) -> (HirExpr, String, String) {
        let int = self.tcx.int_ty(IntTy::I64);
        let (base, outer_offset, available) =
            if let Some((inner, inner_range)) = self.take_window(sequence.clone()) {
                let (base, offset, count) = self.window_view(inner, inner_range, stmts, span);
                (base, Some(offset), count)
            } else {
                let base = self.flatten_window_place(sequence, stmts);
                let len_name = format!("__window_seq_len_{}", self.fresh().0);
                let len = self.method_call(base.clone(), "len", Vec::new(), int, span);
                stmts.push(self.conv_let(&len_name, len, span));
                (base, None, len_name)
            };
        let HirExprKind::Range {
            start,
            end,
            inclusive,
        } = range.kind
        else {
            return (base, available.clone(), available);
        };
        let tag = self.fresh().0;
        let lo_name = format!("__window_lo_{tag}");
        let count_name = format!("__window_len_{tag}");
        let offset_name = format!("__window_at_{tag}");

        let zero = self.conv_int_literal(false, 0, int, span);
        let start = start.map_or(zero, |start| *start);
        let lo = self.clamp(start, &available, span);
        stmts.push(self.conv_let(&lo_name, lo, span));

        let hi = match end {
            Some(end) if inclusive => {
                let one = self.conv_int_literal(false, 1, int, span);
                self.binary(HirBinaryOp::Add, *end, one, int, span)
            }
            Some(end) => *end,
            None => self.conv_path(&available, int, span),
        };
        let lo_read = self.conv_path(&lo_name, int, span);
        let len_read = self.conv_path(&available, int, span);
        let hi = self.method_call(hi, "clamp", vec![lo_read, len_read], int, span);
        let lo_read = self.conv_path(&lo_name, int, span);
        let count = self.binary(HirBinaryOp::Sub, hi, lo_read, int, span);
        stmts.push(self.conv_let(&count_name, count, span));

        let lo_read = self.conv_path(&lo_name, int, span);
        let offset = match outer_offset {
            Some(outer) => {
                let outer = self.conv_path(&outer, int, span);
                self.binary(HirBinaryOp::Add, outer, lo_read, int, span)
            }
            None => lo_read,
        };
        stmts.push(self.conv_let(&offset_name, offset, span));
        (base, offset_name, count_name)
    }

    /// `value.clamp(0, len)`.
    fn clamp(&mut self, value: HirExpr, len_name: &str, span: Span) -> HirExpr {
        let int = self.tcx.int_ty(IntTy::I64);
        let least = self.conv_int_literal(false, 0, int, span);
        let len = self.conv_path(len_name, int, span);
        self.method_call(value, "clamp", vec![least, len], int, span)
    }

    fn binary(
        &mut self,
        op: HirBinaryOp,
        lhs: HirExpr,
        rhs: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        }
    }

    /// `if index < 0 || index >= count { panic(..) }`, worded as indexing
    /// the window itself panics.
    fn window_bounds_guard(&mut self, index: &HirExpr, count_name: &str, span: Span) -> HirStmt {
        let int = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let string_ty = self.tcx.string_ty();
        let never = self.tcx.never();
        let zero = self.conv_int_literal(false, 0, int, span);
        let below = self.conv_compare(HirBinaryOp::Lt, index.clone(), zero, span);
        let count = self.conv_path(count_name, int, span);
        let past = self.conv_compare(HirBinaryOp::Ge, index.clone(), count, span);
        let outside = self.conv_compare(HirBinaryOp::Or, below, past, span);

        let lead = self.conv_text("vec index out of bounds: the len is ".to_string(), span);
        let count = self.conv_path(count_name, int, span);
        let count = self.builtin_call("__debug", vec![count], string_ty, span);
        let middle = self.conv_text(" but the index is ".to_string(), span);
        let shown = self.builtin_call("__debug", vec![index.clone()], string_ty, span);
        let message = self.builtin_call(
            "__concat",
            vec![lead, count, middle, shown],
            string_ty,
            span,
        );
        let panic = self.builtin_call("panic", vec![message], never, span);
        let raise = HirExpr {
            id: self.fresh(),
            span,
            ty: unit,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts: vec![HirStmt {
                    id: self.fresh(),
                    span,
                    kind: HirStmtKind::Expr {
                        expr: panic,
                        has_semi: true,
                    },
                }],
                tail: None,
                ty: unit,
                is_comptime: false,
            }),
        };
        HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr: HirExpr {
                    id: self.fresh(),
                    span,
                    ty: unit,
                    kind: HirExprKind::If {
                        condition: Box::new(outside),
                        then_branch: Box::new(raise),
                        else_branch: None,
                    },
                },
                has_semi: true,
            },
        }
    }

    /// A call that writes through a window, by a mutating method's receiver
    /// or a `&mut` argument, rewritten to write the sequence's own element,
    /// with the window's bindings evaluated before the call.
    pub(super) fn hoist_window_writes(&mut self, mut call: HirExpr) -> HirExpr {
        let mut stmts = Vec::new();
        let span = call.span;
        match &mut call.kind {
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                if gossamer_types::is_mutating_method_name(name.name.as_str()) {
                    self.flatten_written_operand(receiver, &mut stmts);
                }
                for arg in args {
                    self.flatten_borrowed_arg(arg, &mut stmts);
                }
            }
            HirExprKind::Call { args, .. } => {
                for arg in args {
                    self.flatten_borrowed_arg(arg, &mut stmts);
                }
            }
            _ => {}
        }
        if stmts.is_empty() {
            return call;
        }
        self.prefixed(stmts, call, span)
    }

    /// A `&mut place` argument whose place writes through a window.
    fn flatten_borrowed_arg(&mut self, arg: &mut HirExpr, stmts: &mut Vec<HirStmt>) {
        if matches!(
            arg.kind,
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                ..
            }
        ) {
            self.flatten_written_operand(arg, stmts);
        }
    }

    /// `operand`, or the place under its `&mut`, rewritten when it writes
    /// through a window.
    fn flatten_written_operand(&mut self, operand: &mut HirExpr, stmts: &mut Vec<HirStmt>) {
        let place = match &mut operand.kind {
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand: inner,
            } => inner.as_mut(),
            _ => operand,
        };
        if self.writes_through_window(place) {
            let taken = std::mem::replace(
                place,
                HirExpr {
                    id: place.id,
                    span: place.span,
                    ty: place.ty,
                    kind: HirExprKind::Tuple(Vec::new()),
                },
            );
            *place = self.flatten_window_place(taken, stmts);
        }
    }

    /// `{ stmts; assign }` as the lowered assignment.
    pub(super) fn with_window_bindings(
        &mut self,
        stmts: Vec<HirStmt>,
        assign: HirExprKind,
        span: Span,
    ) -> HirExprKind {
        if stmts.is_empty() {
            return assign;
        }
        let unit = self.tcx.unit();
        let assign = HirExpr {
            id: self.fresh(),
            span,
            ty: unit,
            kind: assign,
        };
        let mut stmts = stmts;
        stmts.push(HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr: assign,
                has_semi: true,
            },
        });
        HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts,
            tail: None,
            ty: unit,
            is_comptime: false,
        })
    }
}
