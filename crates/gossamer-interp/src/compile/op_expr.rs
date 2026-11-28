#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

impl<'tcx> FnBuilder<'tcx> {
    pub(crate) fn compile_unary(
        &mut self,
        op: HirUnaryOp,
        operand: &HirExpr,
    ) -> RuntimeResult<Reg> {
        if matches!(op, HirUnaryOp::Neg | HirUnaryOp::Not)
            && let Some(unsigned) = self.wide_int_ty(self.static_ty(operand))
        {
            // `-170141183460469231731687303715884105728` is `i128::MIN`, a
            // constant whose magnitude alone does not fit.
            if op == HirUnaryOp::Neg
                && let HirExprKind::Literal(gossamer_hir::HirLiteral::Int(text)) = &operand.kind
            {
                let negative = gossamer_hir::HirLiteral::Int(format!("-{text}"));
                return self.compile_wide_literal(&negative, unsigned);
            }
            let src = self.compile_expr(operand)?;
            let dst = self.alloc_reg();
            self.emit(Op::WideUnary {
                dst,
                operand: src,
                neg: op == HirUnaryOp::Neg,
                unsigned,
            });
            return Ok(dst);
        }
        // `-x` / `!x` on a user struct / enum route to its `neg` / `not`
        // impl method.
        if let Some(method) = match op {
            HirUnaryOp::Neg => Some("neg"),
            HirUnaryOp::Not => Some("not"),
            _ => None,
        } && let Some(sname) = self.adt_type_name(operand.ty)
        {
            return Ok(self.compile_struct_unary(&sname, method, operand)?.reg);
        }
        let operand_reg = self.compile_expr(operand)?;
        let dst = self.alloc_reg();
        let instr = match op {
            HirUnaryOp::Neg => Op::Neg {
                dst,
                operand: operand_reg,
            },
            HirUnaryOp::Not => Op::Not {
                dst,
                operand: operand_reg,
            },
            HirUnaryOp::RefShared | HirUnaryOp::RefMut => Op::Move {
                dst,
                src: operand_reg,
            },
            HirUnaryOp::Deref => Op::Deref {
                dst,
                src: operand_reg,
            },
        };
        self.emit(instr);
        Ok(dst)
    }

    pub(crate) fn compile_binary(
        &mut self,
        op: HirBinaryOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> RuntimeResult<Reg> {
        if matches!(op, HirBinaryOp::And | HirBinaryOp::Or) {
            return self.compile_short_circuit(op, lhs, rhs);
        }
        // Route through `_ex` so two-f64 / two-i64 binary
        // trees stay in the typed register file end-to-end.
        // The result gets boxed only if the caller needs a
        // `Value`.
        let tr = self.compile_binary_ex(op, lhs, rhs)?;
        Ok(self.as_value(tr))
    }
}
