//! Compiling binary and unary operators, including struct comparisons and operator overloads.

use super::*;

impl<'tcx> FnBuilder<'tcx> {
    /// Fuses an i64 arith whose right operand is an integer literal
    /// fitting `i32` into one `Op::ArithImmI64`, skipping the literal's
    /// `LoadConstI64`. Declined for a zero `Div`/`Rem` divisor (the
    /// two-op form owns the divide-by-zero panic) and for unsigned
    /// operands (handled by the caller's guard).
    fn try_compile_i64_arith_imm(
        &mut self,
        op: HirBinaryOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> RuntimeResult<Option<TypedReg>> {
        let kind = match op {
            HirBinaryOp::Div => ImmArithKind::Div,
            HirBinaryOp::Rem => ImmArithKind::Rem,
            _ => return Ok(None),
        };
        let HirExprKind::Literal(HirLiteral::Int(text)) = &rhs.kind else {
            return Ok(None);
        };
        let Some(n) = parse_int(text) else {
            return Ok(None);
        };
        let Ok(imm) = i32::try_from(n) else {
            return Ok(None);
        };
        if imm == 0 && matches!(kind, ImmArithKind::Div | ImmArithKind::Rem) {
            return Ok(None);
        }
        let lhs_tr = self.compile_expr_ex(lhs)?;
        if matches!(lhs_tr.kind, RegKind::F64) {
            return Ok(Some(self.emit_static_binary_type_error(
                lhs,
                lhs_tr.kind,
                rhs,
                RegKind::I64,
            )));
        }
        let rhs_peer = (lhs_tr.kind != RegKind::I64).then(|| self.load_int_value(n));
        let lhs_i = self.as_i64_with_peer(lhs_tr, rhs_peer);
        let dst = self.alloc_int();
        self.emit(Op::ArithImmI64 {
            kind,
            dst_i: dst,
            lhs_i,
            imm,
        });
        Ok(Some(TypedReg {
            reg: dst,
            kind: RegKind::I64,
        }))
    }

    /// Typed binary-op compile. Emits `AddF64` / `LtI64` /
    /// etc. when both operands share a concrete numeric kind;
    /// otherwise falls back to the generic `binary_op` path
    /// (which operates on `Value` regs).
    pub(crate) fn compile_binary_ex(
        &mut self,
        op: HirBinaryOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> RuntimeResult<TypedReg> {
        let result = self.compile_binary_ex_wide(op, lhs, rhs)?;
        // A narrow signed `MIN / -1` answers `2^(n-1)` at i64 width, one past
        // the type's range. The quotient wraps at the declared width, as the
        // i64 case does, so every division path is narrowed back here.
        if !matches!(op, HirBinaryOp::Div) {
            return Ok(result);
        }
        let signed_narrow =
            [lhs.ty, rhs.ty]
                .into_iter()
                .find_map(|ty| match self.tcx.kind(self.unwrap_ref(ty)) {
                    Some(TyKind::Int(
                        int_ty @ (gossamer_types::IntTy::I8
                        | gossamer_types::IntTy::I16
                        | gossamer_types::IntTy::I32),
                    )) => super::fast_paths::narrow_int_trunc(*int_ty),
                    _ => None,
                });
        let Some((shift, signed)) = signed_narrow else {
            return Ok(result);
        };
        let src_i = self.as_i64(result);
        let dst = self.alloc_int();
        self.emit(Op::TruncCastI64 {
            dst_i: dst,
            src_i,
            shift,
            signed,
        });
        Ok(TypedReg {
            reg: dst,
            kind: RegKind::I64,
        })
    }

    fn compile_binary_ex_wide(
        &mut self,
        op: HirBinaryOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> RuntimeResult<TypedReg> {
        if matches!(op, HirBinaryOp::And | HirBinaryOp::Or) {
            let reg = self.compile_short_circuit(op, lhs, rhs)?;
            return Ok(TypedReg {
                reg,
                kind: RegKind::Value,
            });
        }
        if matches!(
            op,
            HirBinaryOp::BitAnd | HirBinaryOp::BitOr | HirBinaryOp::BitXor
        ) && matches!(self.tcx.kind(self.unwrap_ref(lhs.ty)), Some(TyKind::Bool))
        {
            let reg = self.compile_bool_bitwise(op, lhs, rhs)?;
            return Ok(TypedReg {
                reg,
                kind: RegKind::Value,
            });
        }
        if let Some(reg) = self.try_compile_described_comparison(op, lhs, rhs)? {
            return Ok(TypedReg {
                reg,
                kind: RegKind::Value,
            });
        }
        let lk = self.expr_kind(lhs);
        let rk = self.expr_kind(rhs);
        // Both operands f64 - emit a typed f64 op. For `+-*/`
        // the result is also f64; for comparisons it's a
        // `Bool` Value.
        if lk == RegKind::F64 && rk == RegKind::F64 {
            // Peephole fuse `a * b + c` / `c + a * b` /
            // `c - a * b` into `MulAddF64` / `MulSubF64`
            // before touching operand evaluation. Halves the
            // op count on any vector-math-style expression
            // tree (`x + dt * vx`, `vx - dx * mag`, ...).
            if let Some(tr) = self.try_compile_fma(op, lhs, rhs)? {
                return Ok(tr);
            }
            let lhs_tr = self.compile_expr_ex(lhs)?;
            let rhs_tr = self.compile_expr_ex(rhs)?;
            if matches!(lhs_tr.kind, RegKind::I64 | RegKind::F64)
                && matches!(rhs_tr.kind, RegKind::I64 | RegKind::F64)
                && lhs_tr.kind != rhs_tr.kind
            {
                return Ok(self.emit_static_binary_type_error(lhs, lhs_tr.kind, rhs, rhs_tr.kind));
            }
            let lhs_peer = (rhs_tr.kind != RegKind::F64).then(|| self.as_value(lhs_tr));
            let rhs_peer = (lhs_tr.kind != RegKind::F64).then(|| self.as_value(rhs_tr));
            let lhs_f = self.as_f64_with_peer(lhs_tr, rhs_peer);
            let rhs_f = self.as_f64_with_peer(rhs_tr, lhs_peer);
            return self.emit_binary_f64(op, lhs_f, rhs_f);
        }
        if lk == RegKind::I64 && rk == RegKind::I64 {
            let lhs_unsigned = self.is_unsigned64_ty(lhs.ty);
            let rhs_unsigned = self.is_unsigned64_ty(rhs.ty);
            if !lhs_unsigned
                && !rhs_unsigned
                && let Some(tr) = self.try_compile_i64_arith_imm(op, lhs, rhs)?
            {
                return Ok(tr);
            }
            let lhs_tr = self.compile_expr_ex(lhs)?;
            let rhs_tr = self.compile_expr_ex(rhs)?;
            // Literal inference may retain an integer expectation on a
            // binary expression even though both operands are float
            // literals. The registers are the authoritative lowering
            // contract: two floats are valid f64 arithmetic, not an i64
            // mismatch between two f64 values.
            if lhs_tr.kind == RegKind::F64 && rhs_tr.kind == RegKind::F64 {
                let lhs_f = self.as_f64(lhs_tr);
                let rhs_f = self.as_f64(rhs_tr);
                return self.emit_binary_f64(op, lhs_f, rhs_f);
            }
            if matches!(lhs_tr.kind, RegKind::I64 | RegKind::F64)
                && matches!(rhs_tr.kind, RegKind::I64 | RegKind::F64)
                && lhs_tr.kind != rhs_tr.kind
            {
                return Ok(self.emit_static_binary_type_error(lhs, lhs_tr.kind, rhs, rhs_tr.kind));
            }
            let lhs_peer = (rhs_tr.kind != RegKind::I64).then(|| self.as_value(lhs_tr));
            let rhs_peer = (lhs_tr.kind != RegKind::I64).then(|| self.as_value(rhs_tr));
            let lhs_i = self.as_i64_with_peer(lhs_tr, rhs_peer);
            let rhs_i = self.as_i64_with_peer(rhs_tr, lhs_peer);
            let int_ty_of = |this: &Self, ty| match this.tcx.kind(this.unwrap_ref(ty)) {
                Some(TyKind::Int(int_ty)) => Some(*int_ty),
                _ => None,
            };
            // A shift's result carries the shifted operand's type; the count
            // has a type of its own. Every other op takes the pair's.
            let overflow_ty = if matches!(op, HirBinaryOp::Shl | HirBinaryOp::Shr) {
                int_ty_of(self, lhs.ty)
            } else {
                [lhs.ty, rhs.ty]
                    .into_iter()
                    .find_map(|ty| int_ty_of(self, ty))
            };
            return self.emit_binary_i64(op, lhs_i, rhs_i, lhs_unsigned, rhs_unsigned, overflow_ty);
        }
        // Struct `==` / `!=` routes to the derived `<Type>::eq` method,
        // which the bytecode `Op::Eq` (scalar / structural) can't express
        // for a `Value::Struct`. `==` only typechecks on a struct that
        // derives or implements `PartialEq`, so the method is present.
        // Enums fall through to the structural `Op::Eq` below.
        if matches!(op, HirBinaryOp::Eq | HirBinaryOp::Ne) {
            if let Some(sname) = self
                .struct_eq_dispatch_name(lhs.ty)
                .or_else(|| self.struct_eq_dispatch_name(rhs.ty))
            {
                return self.compile_struct_eq(&sname, op, lhs, rhs);
            }
        }
        // Struct / enum ordering routes `<` `<=` `>` `>=` to a `Type::cmp`
        // method (synthesized for a by-value-comparable type or hand-written),
        // testing its -1/0/1 result against 0. `adt_type_name` covers structs
        // and enums; the checker has confirmed the method exists.
        // An `Option` / `Result` has no `cmp` of its own: it orders by arm and
        // then by payload, which the structural compare below already does.
        let carrier = [lhs.ty, rhs.ty].into_iter().any(|ty| {
            matches!(
                self.tcx.kind(self.unwrap_ref(ty)),
                Some(TyKind::Adt { def, .. }) if def.local == u32::MAX || def.local == u32::MAX - 1
            )
        });
        if matches!(
            op,
            HirBinaryOp::Lt | HirBinaryOp::Le | HirBinaryOp::Gt | HirBinaryOp::Ge
        ) && !carrier
        {
            if let Some(sname) = self
                .adt_type_name(lhs.ty)
                .or_else(|| self.adt_type_name(rhs.ty))
            {
                return self.compile_struct_cmp(&sname, op, lhs, rhs);
            }
        }
        // Arithmetic / bitwise operator overloading: `a + b` on a user
        // struct or enum routes to its `add`/`sub`/... impl method. The
        // checker rejects ADT operands with no such impl, so the method
        // global is present. `adt_type_name` covers enums and generic
        // instantiations (`Wrap<f64>` -> `Wrap`), unlike the layout-keyed
        // struct-`==` route above.
        if let Some(method) = arith_overload_method(op) {
            if let Some(sname) = self
                .adt_type_name(lhs.ty)
                .or_else(|| self.adt_type_name(rhs.ty))
            {
                return self.compile_struct_binop(&sname, method, lhs, rhs);
            }
        }
        // Fallback: generic path on Value regs.
        let lhs_reg = self.compile_expr(lhs)?;
        let rhs_reg = self.compile_expr(rhs)?;
        let dst = self.alloc_reg();
        let instr = self
            .binary_op(op, dst, lhs_reg, rhs_reg)
            .ok_or(RuntimeError::Unsupported("binary op kind"))?;
        self.emit(instr);
        Ok(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        })
    }

    /// Lowers a struct / enum ordering `a <op> b` to `<sname>::cmp(a, b) <op>
    /// 0` - the synthesized / user `cmp` returns -1 / 0 / 1, tested against a
    /// zero literal with the original operator. Mirrors [`Self::compile_struct_eq`].
    fn compile_struct_cmp(
        &mut self,
        sname: &str,
        op: HirBinaryOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> RuntimeResult<TypedReg> {
        let key = format!("{sname}::cmp");
        let idx = self.global_idx(&key);
        let callee_reg = self.alloc_reg();
        self.emit(Op::LoadGlobal {
            dst: callee_reg,
            idx,
        });
        // Compile both operands into temporaries first, then lay them into a
        // fresh contiguous argument span above them. An aggregate operand
        // (enum / struct) whose construction allocates its own registers must
        // not overlap the span, so the span is allocated only after both
        // operands are built - the canonical call lowering's shape.
        let lhs_reg = self.compile_expr(lhs)?;
        let rhs_reg = self.compile_expr(rhs)?;
        let args_start = self.next_reg;
        self.ensure_reg_slot(args_start);
        self.emit(Op::Move {
            dst: args_start,
            src: lhs_reg,
        });
        self.ensure_reg_slot(args_start + 1);
        self.emit(Op::Move {
            dst: args_start + 1,
            src: rhs_reg,
        });
        let cmpres = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        // Aggregate operands are non-scalar, so the callee must auto-deref any
        // `flag::Cell` arguments - mirrors the free-call path's `may_have_cells`.
        self.emit(Op::Call {
            dst: cmpres,
            callee: callee_reg,
            args: args_start,
            argc: 2,
            cache_idx,
            may_have_cells: true,
        });
        let zero = self.load_int_value(0);
        let dst = self.alloc_reg();
        let instr = self
            .binary_op(op, dst, cmpres, zero)
            .ok_or(RuntimeError::Unsupported("cmp-to-zero op kind"))?;
        self.emit(instr);
        Ok(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        })
    }

    /// Lowers a unary operator overload `<op> a` (currently `-a` -> `neg`) to
    /// a call of the user `<sname>::<method>(a)` impl method.
    pub(crate) fn compile_struct_unary(
        &mut self,
        sname: &str,
        method: &str,
        operand: &HirExpr,
    ) -> RuntimeResult<TypedReg> {
        let key = format!("{sname}::{method}");
        let idx = self.global_idx(&key);
        let callee_reg = self.alloc_reg();
        self.emit(Op::LoadGlobal {
            dst: callee_reg,
            idx,
        });
        let operand_reg = self.compile_expr(operand)?;
        let args_start = self.next_reg;
        self.ensure_reg_slot(args_start);
        self.emit(Op::Move {
            dst: args_start,
            src: operand_reg,
        });
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        self.emit(Op::Call {
            dst,
            callee: callee_reg,
            args: args_start,
            argc: 1,
            cache_idx,
            may_have_cells: true,
        });
        Ok(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        })
    }

    /// Lowers an arithmetic operator overload `a <op> b` to a call of the
    /// user `<sname>::<method>(lhs, rhs)` impl method. Mirrors
    /// [`Self::compile_struct_eq`] without the `!=` negation.
    pub(super) fn compile_struct_binop(
        &mut self,
        sname: &str,
        method: &str,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> RuntimeResult<TypedReg> {
        let key = format!("{sname}::{method}");
        let idx = self.global_idx(&key);
        let callee_reg = self.alloc_reg();
        self.emit(Op::LoadGlobal {
            dst: callee_reg,
            idx,
        });
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(2)
            .expect("register overflow reserving operator-overload args");
        let lhs_reg = self.compile_expr(lhs)?;
        self.emit(Op::Move {
            dst: args_start,
            src: lhs_reg,
        });
        let rhs_reg = self.compile_expr(rhs)?;
        self.emit(Op::Move {
            dst: args_start + 1,
            src: rhs_reg,
        });
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        self.emit(Op::Call {
            dst,
            callee: callee_reg,
            args: args_start,
            argc: 2,
            cache_idx,
            may_have_cells: false,
        });
        Ok(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        })
    }

    /// Lowers a struct `==` / `!=` to a call of the derived
    /// `<sname>::eq(lhs, rhs)` method, negating the result for `!=`.
    /// Mirrors the compiled tiers, which route aggregate equality to the
    /// same synthesized method.
    fn compile_struct_eq(
        &mut self,
        sname: &str,
        op: HirBinaryOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> RuntimeResult<TypedReg> {
        let key = format!("{sname}::eq");
        let idx = self.global_idx(&key);
        let callee_reg = self.alloc_reg();
        self.emit(Op::LoadGlobal {
            dst: callee_reg,
            idx,
        });
        // Reserve the two argument slots before compiling either operand
        // so an operand whose compile allocates fresh registers can't land
        // inside the not-yet-populated span. Mirrors `compile_call_ex`.
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(2)
            .expect("register overflow reserving eq args");
        let lhs_reg = self.compile_expr(lhs)?;
        self.emit(Op::Move {
            dst: args_start,
            src: lhs_reg,
        });
        let rhs_reg = self.compile_expr(rhs)?;
        self.emit(Op::Move {
            dst: args_start + 1,
            src: rhs_reg,
        });
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        self.emit(Op::Call {
            dst,
            callee: callee_reg,
            args: args_start,
            argc: 2,
            cache_idx,
            may_have_cells: false,
        });
        if matches!(op, HirBinaryOp::Ne) {
            let neg = self.alloc_reg();
            self.emit(Op::Not {
                dst: neg,
                operand: dst,
            });
            return Ok(TypedReg {
                reg: neg,
                kind: RegKind::Value,
            });
        }
        Ok(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        })
    }

    pub(crate) fn compile_unary_ex(
        &mut self,
        op: HirUnaryOp,
        operand: &HirExpr,
    ) -> RuntimeResult<TypedReg> {
        let kind = self.expr_kind(operand);
        // Negation and complement run at i64 width; a narrower type takes its
        // value back from the wide result.
        let narrow = match self.tcx.kind(operand.ty) {
            Some(TyKind::Int(int_ty)) => super::fast_paths::narrow_int_trunc(*int_ty),
            _ => None,
        };
        let wide = match (op, kind) {
            (HirUnaryOp::Neg, RegKind::F64) => {
                let tr = self.compile_expr_ex(operand)?;
                let src_f = self.as_f64(tr);
                let dst = self.alloc_float();
                self.emit(Op::NegF64 { dst_f: dst, src_f });
                return Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::F64,
                });
            }
            (HirUnaryOp::Neg, RegKind::I64) => {
                let tr = self.compile_expr_ex(operand)?;
                let src_i = self.as_i64(tr);
                let dst = self.alloc_int();
                self.emit(Op::NegI64 { dst_i: dst, src_i });
                TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                }
            }
            _ => {
                let reg = self.compile_unary(op, operand)?;
                TypedReg {
                    reg,
                    kind: RegKind::Value,
                }
            }
        };
        let Some((shift, signed)) =
            narrow.filter(|_| matches!(op, HirUnaryOp::Neg | HirUnaryOp::Not))
        else {
            return Ok(wide);
        };
        let src_i = self.as_i64(wide);
        let dst = self.alloc_int();
        self.emit(Op::TruncCastI64 {
            dst_i: dst,
            src_i,
            shift,
            signed,
        });
        Ok(TypedReg {
            reg: dst,
            kind: RegKind::I64,
        })
    }
}
