//! A call of a `[rust-bindings]` item whose types nest composites passes
//! and takes JSON text, which the generated walk writes and reads.
//!
//! The library states which positions cross as text
//! (`ExternalItem::json`); every tier calls the item with the text, so the
//! walk here is the one conversion each tier runs.

use gossamer_types::{IntTy, Ty};

use super::{Emit, Gen, READER, WRITER};
use crate::tree::{
    HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind, HirStmtKind,
    HirUnaryOp, for_each_child_expr_mut,
};

impl Gen<'_> {
    /// Rewrites every call under `item` of a binding item with a position
    /// that crosses as text.
    pub(super) fn wire_binding_calls(&mut self, item: &mut HirItem) {
        match &mut item.kind {
            HirItemKind::Fn(f) => self.wire_in_fn(f),
            HirItemKind::Impl(imp) => {
                for m in &mut imp.methods {
                    self.wire_in_fn(m);
                }
            }
            HirItemKind::Trait(t) => {
                for m in &mut t.methods {
                    self.wire_in_fn(m);
                }
            }
            HirItemKind::Const(c) => self.wire_in_expr(&mut c.value),
            HirItemKind::Static(s) => self.wire_in_expr(&mut s.value),
            HirItemKind::Adt(_) => {}
        }
    }

    fn wire_in_fn(&mut self, f: &mut HirFn) {
        if let Some(body) = &mut f.body {
            self.wire_in_block(&mut body.block);
        }
    }

    fn wire_in_block(&mut self, block: &mut HirBlock) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { init, .. } => {
                    if let Some(e) = init {
                        self.wire_in_expr(e);
                    }
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                    self.wire_in_expr(expr);
                }
                HirStmtKind::Item(item) => self.wire_binding_calls(item),
            }
        }
        if let Some(tail) = &mut block.tail {
            self.wire_in_expr(tail);
        }
    }

    fn wire_in_expr(&mut self, expr: &mut HirExpr) {
        if let HirExprKind::Block(block) = &mut expr.kind {
            self.wire_in_block(block);
        } else {
            for_each_child_expr_mut(expr, &mut |child| self.wire_in_expr(child));
        }
        if let Some(wired) = self.wired_call(expr) {
            *expr = wired;
        }
    }

    /// `expr` with its text positions written and read, when it calls a
    /// binding item that has any.
    fn wired_call(&mut self, expr: &HirExpr) -> Option<HirExpr> {
        let HirExprKind::Call { callee, args } = &expr.kind else {
            return None;
        };
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return None;
        };
        let qualified = segments
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("::");
        let item = gossamer_resolve::lookup_external_item(&qualified)?;
        if item.json == 0 {
            return None;
        }
        let span = expr.span;
        let string_ty = self.tcx.string_ty();
        let outer = std::mem::replace(&mut self.wire, true);
        let wired_args = args
            .iter()
            .enumerate()
            .map(|(index, arg)| {
                if item.json_param(index) {
                    self.wire_text(arg, span)
                } else {
                    arg.clone()
                }
            })
            .collect();
        let call_ty = if item.json_ret() { string_ty } else { expr.ty };
        let call = self.expr(
            call_ty,
            span,
            HirExprKind::Call {
                callee: callee.clone(),
                args: wired_args,
            },
        );
        let wired = if item.json_ret() {
            self.wire_value(call, expr.ty, &qualified, span)
        } else {
            call
        };
        self.wire = outer;
        Some(wired)
    }

    /// The text `arg` crosses as.
    fn wire_text(&mut self, arg: &HirExpr, span: gossamer_lex::Span) -> HirExpr {
        let mut value = arg;
        while let HirExprKind::Unary {
            op: HirUnaryOp::RefShared | HirUnaryOp::RefMut,
            operand,
        } = &value.kind
        {
            value = operand;
        }
        let ty = value.ty;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let string_ty = self.tcx.string_ty();
        let mut out = Emit::default();
        let writer = self.step("wr_new", Vec::new(), i64_ty, span);
        out.stmt(self.let_stmt(WRITER, false, i64_ty, writer, span));
        let held = self.fresh("arg");
        out.stmt(self.let_stmt(&held, false, ty, value.clone(), span));
        let held_value = self.local(&held, ty, span);
        self.encode(&mut out, held_value, ty, span);
        let w = self.local(WRITER, i64_ty, span);
        let finished = self.step("wr_finish", vec![w], string_ty, span);
        let stmts = self.finish(out, span);
        self.block(stmts, Some(finished), string_ty, span)
    }

    /// The value of `ty` the text `call` answers spells. The library wrote
    /// it, so text the walk cannot read is a broken library, which panics.
    fn wire_value(
        &mut self,
        call: HirExpr,
        ty: Ty,
        qualified: &str,
        span: gossamer_lex::Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let unit = self.tcx.unit();
        let never = self.tcx.never();
        let mut out = Emit::default();
        let reader = self.step("rd_new", vec![call], i64_ty, span);
        out.stmt(self.let_stmt(READER, false, i64_ty, reader, span));
        let value = self.decode(&mut out, ty, qualified, span);
        let held = self.fresh("answer");
        out.stmt(self.let_stmt(&held, false, ty, value, span));
        let r = self.local(READER, i64_ty, span);
        let failed = self.step("rd_failed", vec![r], bool_ty, span);
        let message = self.string_lit(
            &format!("binding `{qualified}` answered text its type does not read"),
            span,
        );
        let raised = self.call_named("panic", vec![message], never, span);
        let raised = self.expr_stmt(raised, span);
        let raised = self.block(vec![raised], None, unit, span);
        let nothing = self.block(Vec::new(), None, unit, span);
        let checked = self.if_else(failed, raised, nothing, unit, span);
        out.stmt(self.expr_stmt(checked, span));
        let r = self.local(READER, i64_ty, span);
        let free = self.step("rd_free", vec![r], unit, span);
        out.stmt(self.expr_stmt(free, span));
        let stmts = self.finish(out, span);
        let answer = self.local(&held, ty, span);
        self.block(stmts, Some(answer), ty, span)
    }

    /// An `Option` as the wire spells it: `[]`, or `[v]`.
    pub(super) fn decode_wire_option(
        &mut self,
        out: &mut Emit,
        ty: Ty,
        inner: Ty,
        path: &str,
        span: gossamer_lex::Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let unit = self.tcx.unit();
        let count = self.fresh("n");
        let r = self.reader(span);
        let message = self.string_lit(&format!("{path}: expected option"), span);
        let begun = self.step("rd_array", vec![r, message], i64_ty, span);
        out.stmt(self.let_stmt(&count, false, i64_ty, begun, span));
        let held = self.local(&count, i64_ty, span);
        let zero = self.int_lit(0, i64_ty, span);
        let empty = self.expr(
            bool_ty,
            span,
            HirExprKind::Binary {
                op: HirBinaryOp::Eq,
                lhs: Box::new(held),
                rhs: Box::new(zero),
            },
        );
        let none = self.none(ty, span);
        let mut some = Emit::default();
        let r = self.reader(span);
        let first = self.int_lit(0, i64_ty, span);
        let enter = self.step("rd_elem", vec![r, first], unit, span);
        some.stmt(self.expr_stmt(enter, span));
        let value = self.decode(&mut some, inner, &format!("{path}?"), span);
        let local = self.fresh("x");
        some.stmt(self.let_stmt(&local, false, inner, value, span));
        some.stmt(self.leave_stmt(span));
        let value = self.local(&local, inner, span);
        let wrapped = self.ctor("Some", vec![value], ty, span);
        let some_stmts = self.finish(some, span);
        let some_block = self.block(some_stmts, Some(wrapped), ty, span);
        self.if_else(empty, none, some_block, ty, span)
    }

    /// One arm of a wire `Result`: the payload, wrapped by `ctor`.
    fn decode_wire_arm(
        &mut self,
        ctor: &str,
        arm_ty: Ty,
        ty: Ty,
        path: &str,
        span: gossamer_lex::Span,
    ) -> HirExpr {
        let mut arm = Emit::default();
        let value = self.decode(&mut arm, arm_ty, &format!("{path}.{ctor}"), span);
        let wrapped = self.ctor(ctor, vec![value], ty, span);
        let stmts = self.finish(arm, span);
        self.block(stmts, Some(wrapped), ty, span)
    }

    /// One arm of a wire `Result` written: the payload after `tag`, in the
    /// binding the match names.
    pub(super) fn encode_wire_arm(
        &mut self,
        tag: &str,
        arm_ty: Ty,
        span: gossamer_lex::Span,
    ) -> (String, HirExpr) {
        let unit = self.tcx.unit();
        let held = self.fresh("x");
        let mut arm = Emit::default();
        arm.raw(tag);
        let held_value = self.local(&held, arm_ty, span);
        self.encode(&mut arm, held_value, arm_ty, span);
        arm.raw("]");
        let stmts = self.finish(arm, span);
        (held, self.block(stmts, None, unit, span))
    }

    /// A `Result` as the wire spells it: `[0, v]`, or `[1, e]`.
    pub(super) fn decode_wire_result(
        &mut self,
        out: &mut Emit,
        ty: Ty,
        ok_ty: Ty,
        err_ty: Ty,
        path: &str,
        span: gossamer_lex::Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let unit = self.tcx.unit();
        let r = self.reader(span);
        let two = self.int_lit(2, i64_ty, span);
        let not_array = self.string_lit(&format!("{path}: expected result"), span);
        let wrong_len = self.string_lit(&format!("{path}: expected result"), span);
        let begin = self.step("rd_tuple", vec![r, two, not_array, wrong_len], unit, span);
        out.stmt(self.expr_stmt(begin, span));
        let r = self.reader(span);
        let first = self.int_lit(0, i64_ty, span);
        let enter = self.step("rd_elem", vec![r, first], unit, span);
        out.stmt(self.expr_stmt(enter, span));
        let r = self.reader(span);
        let message = self.string_lit(&format!("{path}: expected result tag"), span);
        let tag = self.step("rd_i64", vec![r, message], i64_ty, span);
        let tag_local = self.fresh("tag");
        out.stmt(self.let_stmt(&tag_local, false, i64_ty, tag, span));
        out.stmt(self.leave_stmt(span));
        let r = self.reader(span);
        let second = self.int_lit(1, i64_ty, span);
        let enter = self.step("rd_elem", vec![r, second], unit, span);
        out.stmt(self.expr_stmt(enter, span));
        let on_ok = self.decode_wire_arm("Ok", ok_ty, ty, path, span);
        let on_err = self.decode_wire_arm("Err", err_ty, ty, path, span);
        let held = self.local(&tag_local, i64_ty, span);
        let zero = self.int_lit(0, i64_ty, span);
        let is_ok = self.expr(
            bool_ty,
            span,
            HirExprKind::Binary {
                op: HirBinaryOp::Eq,
                lhs: Box::new(held),
                rhs: Box::new(zero),
            },
        );
        let decided = self.if_else(is_ok, on_ok, on_err, ty, span);
        let local = self.fresh("result");
        out.stmt(self.let_stmt(&local, false, ty, decided, span));
        out.stmt(self.leave_stmt(span));
        self.local(&local, ty, span)
    }
}
