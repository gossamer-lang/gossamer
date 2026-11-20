//! Operators on `math::big::Int` and `math::big::Uint` become calls of the
//! `math::big` functions that compute them, which every tier implements.

use gossamer_ast::{AssignOp, BinaryOp as AstBinOp, Expr as AstExpr, NodeId};
use gossamer_lex::Span;
use gossamer_types::IntTy;
use gossamer_types::big_integers::{
    big_int_operator, big_uint_operator, compound_binary, is_comparison,
};

use crate::tree::{HirExpr, HirExprKind};

use super::{Lowerer, lower_binary_op};

impl Lowerer<'_> {
    /// Whether the expression at `node` is a big `Int` (`Some(true)`), a big
    /// `Uint` (`Some(false)`), or neither.
    fn big_integer_at(&self, node: NodeId) -> Option<bool> {
        self.table
            .get(node)
            .and_then(|ty| self.tcx.big_integer_signed(ty))
    }

    /// `a op b` on big integers as a call of the function computing it; an
    /// ordering compares `int_cmp`'s answer with zero. Equality needs no
    /// call: a big integer's text is canonical, so equal values have equal
    /// text. `None` for operands that are not big integers.
    pub(super) fn lower_big_binary(
        &mut self,
        op: AstBinOp,
        lhs: &AstExpr,
        rhs: &AstExpr,
        span: Span,
    ) -> Option<HirExprKind> {
        let signed = self
            .big_integer_at(lhs.id)
            .or_else(|| self.big_integer_at(rhs.id))?;
        if matches!(op, AstBinOp::Eq | AstBinOp::Ne) {
            return None;
        }
        let function = if signed {
            big_int_operator(op)
        } else {
            big_uint_operator(op)
        }?;
        let lhs = self.lower_expr(lhs);
        let rhs = self.lower_expr(rhs);
        if !is_comparison(op) {
            return Some(self.big_arithmetic(function, lhs, rhs, span)?.kind);
        }
        let mut order = self.big_call(function, vec![lhs, rhs], span);
        order.ty = self.tcx.int_ty(IntTy::I64);
        let i64_ty = order.ty;
        let zero = self.conv_int_literal(false, 0, i64_ty, span);
        Some(HirExprKind::Binary {
            op: lower_binary_op(op),
            lhs: Box::new(order),
            rhs: Box::new(zero),
        })
    }

    /// `-x` on a big `Int` as `int_neg(x)`; `None` for any other operand.
    pub(super) fn lower_big_neg(&mut self, operand: &AstExpr, span: Span) -> Option<HirExprKind> {
        if self.big_integer_at(operand.id) != Some(true) {
            return None;
        }
        let operand = self.lower_expr(operand);
        Some(self.big_call("int_neg", vec![operand], span).kind)
    }

    /// The `math::big` function a compound assignment to a big integer
    /// applies, or `None` when `place` is not one.
    pub(super) fn big_compound_function(
        &self,
        op: AssignOp,
        place: &AstExpr,
    ) -> Option<&'static str> {
        let signed = self.big_integer_at(place.id)?;
        let binary = compound_binary(op)?;
        if signed {
            big_int_operator(binary)
        } else {
            big_uint_operator(binary)
        }
    }

    /// `place op= value` on a big integer as `place = function(place, value)`.
    pub(super) fn big_compound_assign(
        &mut self,
        function: &str,
        lowered_place: HirExpr,
        lowered_value: HirExpr,
        span: Span,
    ) -> Option<HirExprKind> {
        let combined = self.big_arithmetic(function, lowered_place.clone(), lowered_value, span)?;
        Some(HirExprKind::Assign {
            place: Box::new(lowered_place),
            value: Box::new(combined),
        })
    }

    /// `function(lhs, rhs)` answering the big integer. A quotient or
    /// remainder answers a `Result`, whose `Err` (a zero divisor) panics as
    /// an integer division by zero does.
    fn big_arithmetic(
        &mut self,
        function: &str,
        lhs: HirExpr,
        rhs: HirExpr,
        span: Span,
    ) -> Option<HirExpr> {
        let mut call = self.big_call(function, vec![lhs, rhs], span);
        if !matches!(function, "int_div" | "int_rem") {
            return Some(call);
        }
        let string = self.tcx.string_ty();
        let error = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([string, error]);
        let result = self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs: substs.clone(),
        });
        call.ty = result;
        let message = self.conv_text("attempt to divide by zero".to_string(), span);
        let unwrapped = self.expect_match(call, (result, string), false, &substs, message, span)?;
        Some(HirExpr {
            id: self.fresh(),
            span,
            ty: string,
            kind: unwrapped,
        })
    }

    /// A call of `math::big::<function>`, answering a big integer, which
    /// every tier holds as its decimal text.
    fn big_call(&mut self, function: &str, args: Vec<HirExpr>, span: Span) -> HirExpr {
        let callee = self.conv_path_of(&["math", "big", function], span);
        HirExpr {
            id: self.fresh(),
            span,
            ty: self.tcx.string_ty(),
            kind: HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        }
    }
}
