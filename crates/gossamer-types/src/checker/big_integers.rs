//! Operators on `math::big::Int` and `math::big::Uint`.

use super::{BinaryOp, Expr, Span, Ty, TypeChecker, TypeError, UnaryOp};
use crate::big_integers::{big_int_operator, big_uint_operator, is_comparison};

impl TypeChecker<'_> {
    /// Types a binary operator with a `math::big` integer operand: both sides
    /// are the same big type, arithmetic answers that type and a comparison
    /// a `bool`. `None` when neither operand is a big integer.
    ///
    /// `Int` answers `+ - * / %` and every comparison; `Uint` answers `+`,
    /// `*`, `==`, and `!=`, the operations `math::big` defines on it. The
    /// lowering rewrites each into the `math::big` function that computes it.
    pub(super) fn check_big_integer_binary(
        &mut self,
        op: BinaryOp,
        (lhs, lhs_ty): (&Expr, Ty),
        (rhs, rhs_ty): (&Expr, Ty),
        span: Span,
    ) -> Option<Ty> {
        let lhs_resolved = self.infer.resolve(self.tcx, lhs_ty);
        let rhs_resolved = self.infer.resolve(self.tcx, rhs_ty);
        let signed = self
            .tcx
            .big_integer_signed(lhs_resolved)
            .or_else(|| self.tcx.big_integer_signed(rhs_resolved))?;
        let big = if signed {
            self.tcx.big_int_ty()
        } else {
            self.tcx.big_uint_ty()
        };
        self.unify(big, lhs_ty, lhs.span);
        self.unify(big, rhs_ty, rhs.span);
        let supported = if signed {
            big_int_operator(op).is_some()
        } else {
            big_uint_operator(op).is_some()
        };
        if !supported {
            let ty = self.render_public_ty(big);
            self.emit(
                TypeError::UnresolvedOp {
                    op: op.as_str().to_string(),
                    lhs: ty.clone(),
                    rhs: ty,
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        Some(if is_comparison(op) {
            self.tcx.bool_ty()
        } else {
            big
        })
    }

    /// Types `x op= y` on a big integer as `x = x op y`. Whether `place` was
    /// one.
    pub(super) fn check_big_integer_compound(
        &mut self,
        op: gossamer_ast::AssignOp,
        (place, place_ty): (&Expr, Ty),
        (value, value_ty): (&Expr, Ty),
    ) -> bool {
        let Some(binary) = crate::big_integers::compound_binary(op) else {
            return false;
        };
        let Some(result) =
            self.check_big_integer_binary(binary, (place, place_ty), (value, value_ty), place.span)
        else {
            return false;
        };
        self.unify(place_ty, result, place.span);
        true
    }

    /// Types `-x` on a `math::big::Int`; `None` for any other operand. A
    /// `Uint` has no negation.
    pub(super) fn check_big_integer_unary(
        &mut self,
        op: UnaryOp,
        operand_ty: Ty,
        span: Span,
    ) -> Option<Ty> {
        if !matches!(op, UnaryOp::Neg | UnaryOp::Not) {
            return None;
        }
        let resolved = self.infer.resolve(self.tcx, operand_ty);
        let signed = self.tcx.big_integer_signed(resolved)?;
        if op == UnaryOp::Neg && signed {
            return Some(resolved);
        }
        let ty = self.render_public_ty(resolved);
        self.emit(
            TypeError::UnresolvedOp {
                op: op.as_str().to_string(),
                lhs: ty.clone(),
                rhs: ty,
            },
            span,
        );
        Some(self.tcx.error_ty())
    }
}
