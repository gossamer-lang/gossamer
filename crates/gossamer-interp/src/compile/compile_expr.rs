#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

mod free_call;
mod matching;
mod method_call;
mod operators;
mod rendering;

const HASH_SET_DEF_LOCAL: u32 = u32::MAX - 7;
const BTREE_SET_DEF_LOCAL: u32 = u32::MAX - 18;
const VEC_DEQUE_DEF_LOCAL: u32 = u32::MAX - 19;
const BINARY_HEAP_DEF_LOCAL: u32 = u32::MAX - 28;
const MIN_HEAP_DEF_LOCAL: u32 = u32::MAX - 30;
const VEC_QUEUE_DEF_LOCAL: u32 = u32::MAX - 31;
const VEC_STACK_DEF_LOCAL: u32 = u32::MAX - 32;

/// Peels any `&expr` / `&mut expr` borrow wrappers off an expression,
/// returning the underlying place. The for-loop desugar emits
/// `(&mut __for_iter).next()`, so the `&mut self` writeback target is
/// the value behind the borrow, not the borrow itself.
fn peel_ref_wrappers_expr(expr: &HirExpr) -> &HirExpr {
    let mut cur = expr;
    while let HirExprKind::Unary {
        op: HirUnaryOp::RefShared | HirUnaryOp::RefMut,
        operand,
    } = &cur.kind
    {
        cur = operand;
    }
    cur
}

fn diagnostic_expr(expr: &HirExpr) -> String {
    match &expr.kind {
        HirExprKind::Literal(HirLiteral::Int(text) | HirLiteral::Float(text)) => text.clone(),
        HirExprKind::Path { segments, .. } => segments
            .iter()
            .map(|segment| segment.name.as_str())
            .collect::<Vec<_>>()
            .join("::"),
        HirExprKind::Binary { op, lhs, rhs } => format!(
            "{} {} {}",
            diagnostic_expr(lhs),
            diagnostic_binary_op(*op),
            diagnostic_expr(rhs)
        ),
        _ => "<expression>".to_string(),
    }
}

fn diagnostic_binary_op(op: HirBinaryOp) -> &'static str {
    match op {
        HirBinaryOp::Add => "+",
        HirBinaryOp::Sub => "-",
        HirBinaryOp::Mul => "*",
        HirBinaryOp::Div => "/",
        HirBinaryOp::Rem => "%",
        HirBinaryOp::BitAnd => "&",
        HirBinaryOp::BitOr => "|",
        HirBinaryOp::BitXor => "^",
        HirBinaryOp::Shl => "<<",
        HirBinaryOp::Shr => ">>",
        HirBinaryOp::Eq => "==",
        HirBinaryOp::Ne => "!=",
        HirBinaryOp::Lt => "<",
        HirBinaryOp::Le => "<=",
        HirBinaryOp::Gt => ">",
        HirBinaryOp::Ge => ">=",
        HirBinaryOp::And => "&&",
        HirBinaryOp::Or => "||",
    }
}

/// Collects, in source order, the distinct names a pattern binds.
/// The or-pattern lowering uses this to allocate one shared register
/// per name so every alternative writes the same destinations. For
/// nested or-patterns only the first alternative is walked, since all
/// alternatives bind the same set of names by typecheck invariant.
pub(crate) fn collect_pattern_binding_names(pat: &HirPat, out: &mut Vec<String>) {
    fn push_unique(out: &mut Vec<String>, name: &str) {
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    match &pat.kind {
        HirPatKind::Binding { name, .. } => push_unique(out, &name.name),
        HirPatKind::At { name, sub, .. } => {
            push_unique(out, &name.name);
            collect_pattern_binding_names(sub, out);
        }
        HirPatKind::Tuple(ps) | HirPatKind::Variant { fields: ps, .. } => {
            for p in ps {
                collect_pattern_binding_names(p, out);
            }
        }
        HirPatKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for p in prefix {
                collect_pattern_binding_names(p, out);
            }
            if let Some(rest) = rest {
                collect_pattern_binding_names(rest, out);
            }
            for p in suffix {
                collect_pattern_binding_names(p, out);
            }
        }
        HirPatKind::Struct { fields, .. } => {
            for f in fields {
                match &f.pattern {
                    Some(p) => collect_pattern_binding_names(p, out),
                    None => push_unique(out, &f.name.name),
                }
            }
        }
        HirPatKind::Ref { inner, .. } => collect_pattern_binding_names(inner, out),
        HirPatKind::Or(alts) => {
            if let Some(first) = alts.first() {
                collect_pattern_binding_names(first, out);
            }
        }
        HirPatKind::Wildcard
        | HirPatKind::Rest
        | HirPatKind::Literal(_)
        | HirPatKind::Range { .. } => {}
    }
}

impl<'tcx> FnBuilder<'tcx> {
    fn emit_static_binary_type_error(
        &mut self,
        lhs: &HirExpr,
        lhs_kind: RegKind,
        rhs: &HirExpr,
        rhs_kind: RegKind,
    ) -> TypedReg {
        let type_name = |kind| match kind {
            RegKind::I64 => "i64",
            RegKind::F64 => "f64",
            RegKind::Value => "value",
        };
        let message = format!(
            "incompatible types: `{}` (`{}`) and `{}` (`{}`)",
            type_name(lhs_kind),
            diagnostic_expr(lhs),
            type_name(rhs_kind),
            diagnostic_expr(rhs),
        );
        let msg = self.const_idx(
            ConstKey::String(message.clone()),
            Value::String(SmolStr::from(message)),
        );
        self.emit(Op::TypeError { msg });
        TypedReg {
            reg: self.alloc_reg(),
            kind: RegKind::Value,
        }
    }

    /// Typed counterpart to [`Self::compile_expr`]. Returns
    /// whatever kind the expression naturally produces,
    /// skipping the `BoxF64` / `BoxI64` round-trip when the
    /// result feeds into another typed consumer. Callers that
    /// need a `Value` register invoke [`Self::compile_expr`],
    /// which wraps this method and coerces via `as_value`.
    pub(crate) fn compile_expr_ex(&mut self, expr: &HirExpr) -> RuntimeResult<TypedReg> {
        let start = self.instrs.len();
        let result = self.compile_expr_ex_inner(expr);
        if result.is_ok() {
            self.annotate_instructions(start, expr.span);
        }
        result
    }

    fn compile_expr_ex_inner(&mut self, expr: &HirExpr) -> RuntimeResult<TypedReg> {
        match &expr.kind {
            // Numeric literals land in their typed reg file so
            // adjacent typed ops can consume them directly.
            HirExprKind::Literal(lit) => self.compile_literal_ex(lit, expr.ty),
            // Single-segment paths resolve to locals; the local
            // already carries its `TypedReg`, so we return it
            // as-is without boxing.
            HirExprKind::Path { segments, def, .. } if segments.len() == 1 => {
                if let Some(tr) = self.lookup_local(&segments[0].name) {
                    return Ok(tr);
                }
                let reg = self.compile_path(segments, *def)?;
                Ok(TypedReg {
                    reg,
                    kind: RegKind::Value,
                })
            }
            HirExprKind::Binary { op, lhs, rhs } => self.compile_binary_ex(*op, lhs, rhs),
            HirExprKind::Unary { op, operand } => self.compile_unary_ex(*op, operand),
            HirExprKind::Call { callee, args } => {
                let dispatched = self.dispatched_path(callee);
                let callee: &HirExpr = dispatched.as_ref().unwrap_or(callee);
                if let Some(tr) = self.try_intrinsic_call(callee, args)? {
                    return Ok(tr);
                }
                if let Some(tr) = self.try_build_empty_typed_vec(callee, args, expr.ty)? {
                    return Ok(tr);
                }
                if let Some(tr) = self.try_inline_user_call(callee, args)? {
                    return Ok(tr);
                }
                let reg = self.compile_call_ex(callee, args, expr.ty)?;
                Ok(TypedReg {
                    reg,
                    kind: RegKind::Value,
                })
            }
            HirExprKind::Field { receiver, name } => self.compile_field_ex(receiver, name, expr.ty),
            // Typed numeric cast. We classify the result and the
            // source by the existing `expr_kind` helper. The four
            // tractable combinations land directly in the right
            // typed register file:
            //
            //   i64 → f64              →  IntToFloatF64
            //   f64 → i64              →  FloatToIntI64
            //   i64 → narrow int       →  TruncCastI64 (wrapping semantics)
            //   i64 → i64/u64/isize    →  identity (same bit width)
            //   f64 → f64              →  identity
            //
            // Anything else (refs, custom From impls, trait dyn
            // casts) defers via the catch-all in `compile_expr`.
            HirExprKind::Cast {
                value,
                ty: target_ty,
            } => {
                let dst_kind = self.expr_kind(expr);
                let src_kind = self.expr_kind(value);
                let unsigned_word = |kind: Option<&TyKind>| {
                    matches!(
                        kind,
                        Some(TyKind::Int(
                            gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize
                        ))
                    )
                };
                let src_unsigned_word = unsigned_word(self.tcx.kind(value.ty));
                let dst_unsigned_word = unsigned_word(self.tcx.kind(*target_ty));
                match (dst_kind, src_kind) {
                    (RegKind::F64, RegKind::I64) if !src_unsigned_word => {
                        let src_tr = self.compile_expr_ex(value)?;
                        let src_i = self.as_i64(src_tr);
                        let dst_f = self.alloc_float();
                        self.emit(Op::IntToFloatF64 { dst_f, src_i });
                        Ok(TypedReg {
                            reg: dst_f,
                            kind: RegKind::F64,
                        })
                    }
                    // A float reaching a full-width integer saturates at the
                    // machine word, which this op already does. A narrow
                    // target saturates at its own range instead, so it takes
                    // the general cast rather than a second op here.
                    (RegKind::I64, RegKind::F64)
                        if !dst_unsigned_word
                            && !matches!(
                                self.tcx.kind(*target_ty),
                                Some(TyKind::Int(
                                    gossamer_types::IntTy::I8
                                        | gossamer_types::IntTy::I16
                                        | gossamer_types::IntTy::I32
                                        | gossamer_types::IntTy::U8
                                        | gossamer_types::IntTy::U16
                                        | gossamer_types::IntTy::U32
                                ))
                            ) =>
                    {
                        let src_tr = self.compile_expr_ex(value)?;
                        let src_f = self.as_f64(src_tr);
                        let dst_i = self.alloc_int();
                        self.emit(Op::FloatToIntI64 { dst_i, src_f });
                        Ok(TypedReg {
                            reg: dst_i,
                            kind: RegKind::I64,
                        })
                    }
                    (RegKind::F64, RegKind::F64) => self.compile_expr_ex(value),
                    (RegKind::I64, RegKind::I64) => {
                        // Narrowing casts (e.g. `x as i32`, `x as u8`) must
                        // truncate + sign/zero-extend, not pass through the
                        // source value unchanged.
                        let target_kind = self.tcx.kind(*target_ty);
                        // u64/usize: produce Value::Uint for unsigned display.
                        if matches!(
                            target_kind,
                            Some(TyKind::Int(
                                gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize
                            ))
                        ) {
                            let src_tr = self.compile_expr_ex(value)?;
                            let src_i = self.as_i64(src_tr);
                            let dst_v = self.alloc_reg();
                            self.emit(Op::I64ToUint { dst_v, src_i });
                            return Ok(TypedReg {
                                reg: dst_v,
                                kind: RegKind::Value,
                            });
                        }
                        let (shift, signed) = match target_kind {
                            Some(TyKind::Int(gossamer_types::IntTy::I8)) => (56u8, true),
                            Some(TyKind::Int(gossamer_types::IntTy::I16)) => (48u8, true),
                            Some(TyKind::Int(gossamer_types::IntTy::I32)) => (32u8, true),
                            Some(TyKind::Int(gossamer_types::IntTy::U8)) => (56u8, false),
                            Some(TyKind::Int(gossamer_types::IntTy::U16)) => (48u8, false),
                            Some(TyKind::Int(gossamer_types::IntTy::U32)) => (32u8, false),
                            // i64/isize: same bit width, so the bits carry
                            // over - but a source that reached here as a
                            // `Uint` (from an earlier `as u64` / `as usize`)
                            // must land in a typed i64 register, since that is
                            // what makes the value render and compare signed.
                            // Unboxing an i64-kinded register is already a
                            // no-op, so the signed case costs nothing.
                            _ => {
                                let src_tr = self.compile_expr_ex(value)?;
                                let src_i = self.as_i64(src_tr);
                                return Ok(TypedReg {
                                    reg: src_i,
                                    kind: RegKind::I64,
                                });
                            }
                        };
                        let src_tr = self.compile_expr_ex(value)?;
                        let src_i = self.as_i64(src_tr);
                        let dst_i = self.alloc_int();
                        self.emit(Op::TruncCastI64 {
                            dst_i,
                            src_i,
                            shift,
                            signed,
                        });
                        Ok(TypedReg {
                            reg: dst_i,
                            kind: RegKind::I64,
                        })
                    }
                    _ => {
                        // Remaining whitelisted combos - f32 / bool /
                        // char sources, `char` / `f32` targets - lower
                        // to the generic scalar-cast op so every
                        // GT0005-whitelisted cast is handled natively.
                        let target = self
                            .tcx
                            .kind(*target_ty)
                            .and_then(crate::cast::CastTarget::of)
                            .map(|target| target.read_from(self.tcx.kind(value.ty)))
                            // `as` only typechecks (passes the GT0005
                            // whitelist) for scalar targets, so a resolved
                            // cast always maps to a `CastTarget`. Reaching
                            // here means the target type never resolved - a
                            // frontend invariant violation, surfaced as a
                            // compile error.
                            .ok_or(RuntimeError::Unsupported(
                                "cast target type did not resolve to a scalar",
                            ))?;
                        let src_tr = self.compile_expr_ex(value)?;
                        let src = self.as_value(src_tr);
                        let dst = self.alloc_reg();
                        self.emit(Op::CastScalar { dst, src, target });
                        Ok(TypedReg {
                            reg: dst,
                            kind: RegKind::Value,
                        })
                    }
                }
            }
            // Typed flat-i64 indexed read fast path. When the base
            // resolves to a local register marked as a
            // `Value::IntArray` (built via `try_build_int_array`)
            // and the parent expects an i64, we emit
            // `Op::IntArrayGetI64` which feeds the typed `i64`
            // register file directly - no `Value::Int` box/unbox.
            HirExprKind::Index { base, index }
                if matches!(self.tcx.kind(expr.ty), Some(TyKind::Int(_))) =>
            {
                let base_reg = self.compile_expr(base)?;
                if self.flat_int_locals.contains(&base_reg) {
                    let idx_tr = self.compile_expr_ex(index)?;
                    let idx_i = self.as_i64(idx_tr);
                    let dst_i = self.alloc_int();
                    self.emit(Op::IntArrayGetI64 {
                        dst_i,
                        base: base_reg,
                        index_i: idx_i,
                    });
                    return Ok(TypedReg {
                        reg: dst_i,
                        kind: RegKind::I64,
                    });
                }
                // Slow path: generic IndexGet → boxed Value reg.
                let idx_reg = self.compile_expr(index)?;
                let dst = self.alloc_reg();
                self.emit(Op::IndexGet {
                    dst,
                    base: base_reg,
                    index: idx_reg,
                });
                self.release_chain_temp(base, base_reg);
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            // Typed flat-f64 indexed read fast path. Same shape as
            // the flat-i64 path above but for `Value::FloatVec` -
            // the inner-loop scratch arrays in nbody-style code
            // ride this branch.
            HirExprKind::Index { base, index }
                if matches!(self.tcx.kind(expr.ty), Some(TyKind::Float(FloatTy::F64))) =>
            {
                let base_reg = self.compile_expr(base)?;
                if self.flat_float_locals.contains(&base_reg) {
                    let idx_tr = self.compile_expr_ex(index)?;
                    let idx_i = self.as_i64(idx_tr);
                    let dst_f = self.alloc_float();
                    self.emit(Op::FloatVecGetF64 {
                        dst_f,
                        base: base_reg,
                        index_i: idx_i,
                    });
                    return Ok(TypedReg {
                        reg: dst_f,
                        kind: RegKind::F64,
                    });
                }
                // Slow path: generic IndexGet → boxed Value reg.
                let idx_reg = self.compile_expr(index)?;
                let dst = self.alloc_reg();
                self.emit(Op::IndexGet {
                    dst,
                    base: base_reg,
                    index: idx_reg,
                });
                self.release_chain_temp(base, base_reg);
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                owner,
            } => {
                if let Some(call) = self.dispatched_method_call(expr, receiver, args) {
                    return self.compile_expr_ex(&call);
                }
                if let Some(result) = self.try_compile_i64_wrapping_method(receiver, name, args)? {
                    return Ok(result);
                }
                // Keep conversion methods on their typed direct path too.
                // `compile_expr_ex` handles typed call operands and must not
                // fall through to a runtime method named only `into` or
                // `try_into`, which has no standalone global binding.
                if name.name == "into"
                    && args.is_empty()
                    && matches!(self.tcx.kind(expr.ty), Some(TyKind::Vec(_)))
                    && matches!(self.tcx.kind(receiver.ty), Some(TyKind::Array { .. }))
                {
                    let source = self.compile_expr_ex(receiver)?;
                    return Ok(self.bind_to_fresh(source));
                }
                if name.name == "into"
                    && args.is_empty()
                    && let Some(bname) = self.adt_type_name(expr.ty)
                {
                    return self.compile_struct_unary(&bname, "from", receiver);
                }
                if name.name == "try_into"
                    && args.is_empty()
                    && let Some(bname) = self.result_ok_adt_name(expr.ty)
                {
                    return self.compile_struct_unary(&bname, "try_from", receiver);
                }
                if matches!(name.name.as_str(), "byte_at" | "len") {
                    let mut kind = self.tcx.kind(receiver.ty).cloned();
                    while let Some(TyKind::Ref { inner, .. }) = kind {
                        kind = self.tcx.kind(inner).cloned();
                    }
                    if matches!(kind, Some(TyKind::String)) {
                        let recv = self.compile_expr(receiver)?;
                        let dst_i = self.alloc_int();
                        if name.name == "len" && args.is_empty() {
                            self.emit(Op::StrLenI64 { dst_i, recv });
                            return Ok(TypedReg {
                                reg: dst_i,
                                kind: RegKind::I64,
                            });
                        }
                        if name.name == "byte_at" && args.len() == 1 {
                            let index = self.compile_expr_ex(&args[0])?;
                            let idx_i = self.as_i64(index);
                            self.emit(Op::StrByteAtI64 { dst_i, recv, idx_i });
                            return Ok(TypedReg {
                                reg: dst_i,
                                kind: RegKind::I64,
                            });
                        }
                    }
                }
                let reg = self.compile_method_call(receiver, name, args, owner.as_ref())?;
                Ok(TypedReg {
                    reg,
                    kind: RegKind::Value,
                })
            }
            HirExprKind::Array(gossamer_hir::HirArrayExpr::List(elems)) => {
                if let Some(tr) = self.try_build_float_array(expr.ty, elems.as_slice())? {
                    return Ok(tr);
                }
                if let Some(tr) =
                    self.try_build_float_array_from_structs(expr.ty, elems.as_slice())?
                {
                    return Ok(tr);
                }
                if let Some(tr) = self.try_build_int_array(expr.ty, elems.as_slice())? {
                    return Ok(tr);
                }
                if let Some(tr) = self.try_build_float_vec(expr.ty, elems.as_slice())? {
                    return Ok(tr);
                }
                let reg = self.compile_array_list(elems)?;
                Ok(TypedReg {
                    reg,
                    kind: RegKind::Value,
                })
            }
            HirExprKind::Array(gossamer_hir::HirArrayExpr::Repeat { value, count }) => {
                if let Some(tr) = self.try_build_float_vec_repeat(expr.ty, value, count)? {
                    return Ok(tr);
                }
                if let Some(tr) = self.try_build_int_array_repeat(expr.ty, value, count)? {
                    return Ok(tr);
                }
                let reg = self.compile_array_repeat(value, count)?;
                Ok(TypedReg {
                    reg,
                    kind: RegKind::Value,
                })
            }
            // Everything else goes through the generic path,
            // which always yields a `Value` register.
            _ => {
                let reg = self.compile_expr(expr)?;
                Ok(TypedReg {
                    reg,
                    kind: RegKind::Value,
                })
            }
        }
    }

    pub(crate) fn compile_expr(&mut self, expr: &HirExpr) -> RuntimeResult<Reg> {
        let start = self.instrs.len();
        let result = self.compile_expr_inner(expr);
        if result.is_ok() {
            self.annotate_instructions(start, expr.span);
        }
        result
    }

    fn compile_expr_inner(&mut self, expr: &HirExpr) -> RuntimeResult<Reg> {
        match &expr.kind {
            HirExprKind::Literal(lit) => self.compile_literal(lit),
            HirExprKind::Path { segments, def, .. } => match self.dispatched_path(expr) {
                Some(HirExpr {
                    kind:
                        HirExprKind::Path {
                            segments: target, ..
                        },
                    ..
                }) => self.compile_path(&target, None),
                _ => self.compile_path(segments, *def),
            },
            // `-x` and `!x` wrap at the operand's declared width, which the
            // typed path applies.
            HirExprKind::Unary {
                op: op @ (HirUnaryOp::Neg | HirUnaryOp::Not),
                operand,
            } => {
                let tr = self.compile_unary_ex(*op, operand)?;
                Ok(self.as_value(tr))
            }
            HirExprKind::Unary { op, operand } => self.compile_unary(*op, operand),
            HirExprKind::Binary { op, lhs, rhs } => self.compile_binary(*op, lhs, rhs),
            HirExprKind::Assign { place, value } => self.compile_assign(place, value),
            // Route through `_ex` so intrinsic-style calls
            // (`math::sqrt(x)`, etc.) get lowered to dedicated
            // opcodes when the arg kind is concrete f64, even
            // inside functions whose bodies are compiled via
            // the regular path (e.g. `fn fsqrt(x) { math::sqrt(x) }`).
            HirExprKind::Call { callee, args } => {
                let dispatched = self.dispatched_path(callee);
                let callee: &HirExpr = dispatched.as_ref().unwrap_or(callee);
                let tr = {
                    let intr = self.try_intrinsic_call(callee, args)?;
                    if let Some(tr) = intr {
                        tr
                    } else if let Some(tr) = self.try_inline_user_call(callee, args)? {
                        tr
                    } else {
                        let reg = self.compile_call_ex(callee, args, expr.ty)?;
                        TypedReg {
                            reg,
                            kind: RegKind::Value,
                        }
                    }
                };
                Ok(self.as_value(tr))
            }
            HirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.compile_if(condition, then_branch, else_branch.as_deref()),
            HirExprKind::While {
                condition,
                body,
                label,
            } => {
                self.pending_loop_label.clone_from(label);
                self.compile_while(condition, body)
            }
            // `Loop { body }` - native register-VM lowering. The typed
            // for-loop fast paths (`for i in a..b`, `for x in xs.iter()`,
            // `for (k, v) in map.iter()`) are tried first as an
            // allocation-free index walk; otherwise the generic loop
            // emitter runs. A `for x in <custom iterator>` desugar
            // (`loop { match (&mut __for_iter).next() { … } }`) compiles
            // through the generic emitter: its `next()` call rides the
            // `&mut self` write-back path in `compile_method_call`, so
            // the iterator's state advances natively each iteration.
            HirExprKind::Loop { body, label } => {
                // Re-arm the pending label at each fast-path attempt: a
                // fast path that fires takes it at its own `LoopCtx`
                // push; one that bails leaves the next attempt to set
                // it again.
                self.pending_loop_label.clone_from(label);
                if let Some(reg) = self.try_compile_for_loop_range(body)? {
                    return Ok(reg);
                }
                self.pending_loop_label.clone_from(label);
                if let Some(reg) = self.try_compile_for_loop_vec_iter(body)? {
                    return Ok(reg);
                }
                self.pending_loop_label.clone_from(label);
                self.compile_loop(body)
            }
            HirExprKind::Block(block) => {
                let result = self.compile_block(block)?;
                Ok(match result {
                    BlockResult::Unit | BlockResult::Diverges => self.load_unit(),
                    BlockResult::ValueIn(reg) => reg,
                })
            }
            HirExprKind::Return(value) => self.compile_return(value.as_deref()),
            HirExprKind::Break { value, label } => {
                self.compile_break(value.as_deref(), label.as_deref())
            }
            // Native `continue` - emit a forward jump that the
            // enclosing loop emitter patches once it knows the
            // address of its per-iteration step op. Routing through
            // a patch list (rather than jumping straight to
            // `loop_start`) lets the for-range / for-vec-iter fast
            // paths advance their typed counter on `continue`; a
            // direct jump-to-header bypasses the counter
            // increment that lives at the bottom of the body and
            // produces a livelock.
            HirExprKind::Continue { label } => {
                let idx = self
                    .resolve_loop_target(label.as_deref())
                    .ok_or(RuntimeError::Unsupported("continue outside of loop"))?;
                let defer_depth = self.loop_stack[idx].defer_depth;
                // Run the defers of the blocks nested inside the loop body
                // before jumping to the next iteration; the loop's own
                // enclosing frames stay pending.
                self.emit_defers_above(defer_depth)?;
                let patch = self.emit(Op::Jump { target: 0 });
                self.loop_stack[idx].continue_patches.push(patch);
                Ok(self.load_unit())
            }
            // Native method dispatch - emits an `Op::MethodCall`
            // for the most common hot-path shape
            // (fasta's inner `out.write_byte(…)` etc.).
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                owner,
            } => {
                if let Some(call) = self.dispatched_method_call(expr, receiver, args) {
                    return self.compile_expr(&call);
                }
                // `x.into()` converts to the inferred target `B` (the call's
                // result type) via `B::from(x)`.
                if name.name == "into"
                    && args.is_empty()
                    && matches!(self.tcx.kind(expr.ty), Some(TyKind::Vec(_)))
                    && matches!(self.tcx.kind(receiver.ty), Some(TyKind::Array { .. }))
                {
                    let source = self.compile_expr(receiver)?;
                    let destination = self.alloc_reg();
                    self.emit(Op::Move {
                        dst: destination,
                        src: source,
                    });
                    return Ok(destination);
                }
                if name.name == "into"
                    && args.is_empty()
                    && let Some(bname) = self.adt_type_name(expr.ty)
                {
                    return Ok(self.compile_struct_unary(&bname, "from", receiver)?.reg);
                }
                // `x.try_into()` -> `B::try_from(x)`, where `B` is the `Ok`
                // payload of the `Result<B, E>` result type.
                if name.name == "try_into"
                    && args.is_empty()
                    && let Some(bname) = self.result_ok_adt_name(expr.ty)
                {
                    return Ok(self.compile_struct_unary(&bname, "try_from", receiver)?.reg);
                }
                self.compile_method_call(receiver, name, args, owner.as_ref())
            }
            // Native indexed read.
            HirExprKind::Index { base, index } => {
                // `a[i]` on a user struct / enum routes to its `index` impl
                // method. The checker accepts ADT indexing only when that
                // method exists, so the route is always present here.
                if let Some(sname) = self.adt_type_name(base.ty) {
                    return Ok(self.compile_struct_binop(&sname, "index", base, index)?.reg);
                }
                let base_reg = self.compile_expr(base)?;
                let idx_reg = self.compile_expr(index)?;
                let dst = self.alloc_reg();
                // Source indexing is always checked. APIs that intentionally
                // probe a collection use their explicit `get`-style form;
                // indexing must not turn a bug into a scalar zero value.
                self.emit(Op::IndexGetChecked {
                    dst,
                    base: base_reg,
                    index: idx_reg,
                });
                self.release_chain_temp(base, base_reg);
                Ok(dst)
            }
            // Native struct-field read.
            HirExprKind::Field { receiver, name } => {
                let recv_reg = self.compile_expr(receiver)?;
                let name_idx = self.const_idx(
                    ConstKey::String(name.name.clone()),
                    Value::String(SmolStr::from(name.name.clone())),
                );
                let dst = self.alloc_reg();
                let cache_idx = self.alloc_field_cache_idx();
                self.emit(Op::FieldGet {
                    dst,
                    receiver: recv_reg,
                    name_idx,
                    cache_idx,
                });
                self.release_chain_temp(receiver, recv_reg);
                Ok(dst)
            }
            // Native tuple / positional-field read.
            HirExprKind::TupleIndex { receiver, index } => {
                let recv_reg = self.compile_expr(receiver)?;
                let dst = self.alloc_reg();
                self.emit(Op::TupleIndex {
                    dst,
                    receiver: recv_reg,
                    index: *index,
                });
                self.release_chain_temp(receiver, recv_reg);
                Ok(dst)
            }
            // Cast - delegate to the typed compile path so the
            // typed-numeric arms fire, then box back into a
            // Value reg for whoever asked for one.
            HirExprKind::Cast { .. } => {
                let tr = self.compile_expr_ex(expr)?;
                Ok(self.as_value(tr))
            }
            // Native tuple literal - `(a, b, c)` lands in
            // `count` consecutive value registers, then
            // `Op::BuildTuple` packs them.
            HirExprKind::Tuple(elems) => {
                let n = elems.len();
                if n == 0 {
                    // Empty tuple is unit-shaped; just emit
                    // `Value::Tuple(Arc::from(vec![]))` via
                    // BuildTuple with count 0 to keep semantics
                    // honest.
                    let dst = self.alloc_reg();
                    self.emit(Op::BuildTuple {
                        dst,
                        first: 0,
                        count: 0,
                    });
                    return Ok(dst);
                }
                // Allocate a contiguous block of value registers
                // up front, then compile each elem into its
                // pre-assigned slot via Move. Doing it this way
                // (rather than naively `compile_expr` per elem
                // and hoping they land contiguously) keeps the
                // BuildTuple op's first-reg invariant.
                let first = self.alloc_reg();
                for _ in 1..n {
                    let _ = self.alloc_reg();
                }
                for (i, elem) in elems.iter().enumerate() {
                    let r = self.compile_expr(elem)?;
                    let r = self.stored_value_reg(elem, r);
                    let slot = first + i as u16;
                    if r != slot {
                        self.emit(Op::Move { dst: slot, src: r });
                    }
                }
                let dst = self.alloc_reg();
                let count = u16::try_from(n).map_err(|_| {
                    RuntimeError::Unsupported("tuple literal exceeds 65535 elements")
                })?;
                self.emit(Op::BuildTuple { dst, first, count });
                Ok(dst)
            }
            // Native `match` - test-and-branch chain per arm, including
            // or-patterns that bind (shared binding registers).
            HirExprKind::Match { scrutinee, arms } => self.compile_match(scrutinee, arms, expr),
            // Native closure: compile the body to its own `FnChunk`
            // with the captured upvalues as leading parameters and
            // emit `Op::MakeClosure`.
            HirExprKind::Closure { params, body, .. } => self.compile_closure(params, body),
            // Native `select`: evaluate each arm's channel/value into
            // registers, dispatch via `Op::Select`, and run the winning
            // arm's body block.
            HirExprKind::Select { arms } => self.compile_select(arms),
            // Native `go` in expression position. Call shapes
            // (`go f(args)` / `go obj.method(args)`) lower to
            // `Op::Spawn` / `Op::SpawnMethod`; non-call shapes lift
            // the spawned expression into a zero-arg closure and spawn
            // that. The expression yields `()`.
            // Native array literal. In a `Value` context assemble a
            // generic `Value::Array` (or `[v; n]` repeat) directly. The
            // typed-storage specialisations (`Value::IntArray` /
            // `Value::FloatVec`) are reserved for the typed `_ex` entry,
            // where a known flat-storage consumer asks for them; a plain
            // `Value` consumer (a call argument, a struct field) gets the
            // uniform `Value::Array` the runtime builtins expect.
            HirExprKind::Array(gossamer_hir::HirArrayExpr::List(elems)) => {
                self.compile_array_list(elems)
            }
            HirExprKind::Array(gossamer_hir::HirArrayExpr::Repeat { value, count }) => {
                self.compile_array_repeat(value, count)
            }
            // Native standalone range value (`a..b` / `a..=b`): an eager
            // `Value::Array` of `Value::Int`. For-range loops never reach
            // here - they ride the desugar fast paths above.
            HirExprKind::Range {
                start,
                end,
                inclusive,
            } => self.compile_range_value(start.as_deref(), end.as_deref(), *inclusive),
            // `Placeholder` is the resolver's sentinel for forms it could
            // not rewrite; a well-typed program never carries one to
            // lowering. `LiftedClosure` exists only on the MIR-bound
            // build path (the `lift_closures` pass), which `gos`
            // never runs. Either reaching here is a frontend invariant
            // violation, surfaced as a compile error.
            HirExprKind::Placeholder => Err(RuntimeError::Unsupported(
                "placeholder expression reached bytecode lowering",
            )),
            HirExprKind::LiftedClosure { .. } => Err(RuntimeError::Unsupported(
                "lifted closure reached the bytecode VM (lift runs only on the MIR path)",
            )),
        }
    }

    /// Lowers a generic array literal `[a, b, c]` to `Op::BuildArray`.
    /// Each element compiles into a pre-reserved contiguous value-register
    /// slot, then the op `Arc`-wraps them into a `Value::Array`. The
    /// typed-storage specialisations (`Value::IntArray` / `Value::FloatVec`)
    /// are tried first by the callers; this is the fallback for every
    /// other element type.
    pub(crate) fn compile_array_list(&mut self, elems: &[HirExpr]) -> RuntimeResult<Reg> {
        let n = elems.len();
        if n == 0 {
            let dst = self.alloc_reg();
            self.emit(Op::BuildArray {
                dst,
                first: 0,
                count: 0,
            });
            return Ok(dst);
        }
        // Reserve a contiguous block of value registers before compiling
        // any element, so an element whose compile allocates fresh
        // registers can't land inside the not-yet-populated span and
        // clobber an earlier element. Mirrors the `BuildTuple` lowering.
        let first = self.alloc_reg();
        for _ in 1..n {
            let _ = self.alloc_reg();
        }
        for (i, elem) in elems.iter().enumerate() {
            let r = self.compile_expr(elem)?;
            let r = self.stored_value_reg(elem, r);
            let slot = first + i as u16;
            if r != slot {
                self.emit(Op::Move { dst: slot, src: r });
            }
        }
        let dst = self.alloc_reg();
        let count = u16::try_from(n)
            .map_err(|_| RuntimeError::Unsupported("array literal exceeds 65535 elements"))?;
        self.emit(Op::BuildArray { dst, first, count });
        Ok(dst)
    }

    /// Lowers a generic `[value; count]` repeat to `Op::BuildArrayRepeat`,
    /// which clones `value` `count` times into a `Value::Array`. The count
    /// is read at runtime (`Value::Int`).
    pub(crate) fn compile_array_repeat(
        &mut self,
        value: &HirExpr,
        count: &HirExpr,
    ) -> RuntimeResult<Reg> {
        let value_reg = self.compile_expr(value)?;
        let count_reg = self.compile_expr(count)?;
        let dst = self.alloc_reg();
        self.emit(Op::BuildArrayRepeat {
            dst,
            value: value_reg,
            count: count_reg,
        });
        Ok(dst)
    }

    /// Lowers a standalone range value to a lazy integer iterator. An omitted
    /// lower bound starts at zero; an omitted upper bound stays open-ended.
    pub(crate) fn compile_range_value(
        &mut self,
        start: Option<&HirExpr>,
        end: Option<&HirExpr>,
        inclusive: bool,
    ) -> RuntimeResult<Reg> {
        let start_reg = match start {
            Some(e) => self.compile_expr(e)?,
            None => {
                let idx = self.const_idx(ConstKey::Int(0), Value::Int(0));
                let r = self.alloc_reg();
                self.emit(Op::LoadConst { dst: r, idx });
                r
            }
        };
        let end_reg = match end {
            Some(e) => self.compile_expr(e)?,
            None => {
                let idx = self.const_idx(ConstKey::Int(i64::MAX), Value::Int(i64::MAX));
                let r = self.alloc_reg();
                self.emit(Op::LoadConst { dst: r, idx });
                r
            }
        };
        let dst = self.alloc_reg();
        self.emit(Op::BuildRange {
            dst,
            start: start_reg,
            end: end_reg,
            inclusive: inclusive || end.is_none(),
            start_open: start.is_none(),
            end_open: end.is_none(),
        });
        Ok(dst)
    }

    /// Publishes back into `scrut` every variant-payload binding the arm body
    /// wrote through, then into the place the scrutinee names.
    ///
    /// A payload matched through a mutable place is the enum's own value, but
    /// the interpreter's aggregates are copy-on-write, so a `&mut self` method
    /// mutates the binding's copy. Writing that copy back is what makes the
    /// enum see it.
    fn writeback_variant_payload(
        &mut self,
        scrutinee: &HirExpr,
        scrut: Reg,
        arm: &gossamer_hir::HirMatchArm,
    ) -> RuntimeResult<()> {
        if !self.scrutinee_is_writable_place(scrutinee) {
            return Ok(());
        }
        let HirPatKind::Variant { fields, .. } = &arm.pattern.kind else {
            return Ok(());
        };
        // A body that replaces the scrutinee itself has already decided what
        // the place holds; publishing the old payload over it would undo that.
        if self
            .scrutinee_root_name(scrutinee)
            .is_some_and(|root| self.name_is_written(&arm.body, &root))
        {
            return Ok(());
        }
        let mut wrote_any = false;
        for (i, field) in fields.iter().enumerate() {
            // A `mut` binding declared a value of its own, which the arm's
            // writes stay in; only a name the checker let the arm write
            // without `mut` - one reached through a `&mut` scrutinee - is the
            // enum's own payload.
            let HirPatKind::Binding {
                name,
                mutable: false,
            } = &field.kind
            else {
                continue;
            };
            if !self.name_is_written(&arm.body, &name.name) {
                continue;
            }
            let Some(bound) = self.lookup_local(&name.name) else {
                continue;
            };
            let src = self.as_value(bound);
            let idx = match u16::try_from(i) {
                Ok(idx) => idx,
                Err(_) => continue,
            };
            self.emit(Op::VariantFieldSet {
                receiver: scrut,
                idx,
                src,
            });
            wrote_any = true;
        }
        if wrote_any {
            self.compile_place_store(scrutinee, scrut)?;
        }
        Ok(())
    }

    /// The single-segment binding a scrutinee place is rooted at.
    fn scrutinee_root_name(&self, scrutinee: &HirExpr) -> Option<String> {
        let mut cur = scrutinee;
        loop {
            match &cur.kind {
                HirExprKind::Path { segments, .. } => {
                    return match segments.as_slice() {
                        [seg] => Some(seg.name.clone()),
                        _ => None,
                    };
                }
                HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                    cur = receiver
                }
                HirExprKind::Index { base, .. } => cur = base,
                HirExprKind::Unary { operand, .. } => cur = operand,
                _ => return None,
            }
        }
    }

    /// Whether a scrutinee names storage this frame can write back into.
    fn scrutinee_is_writable_place(&self, scrutinee: &HirExpr) -> bool {
        matches!(
            &scrutinee.kind,
            HirExprKind::Path { .. }
                | HirExprKind::Field { .. }
                | HirExprKind::TupleIndex { .. }
                | HirExprKind::Index { .. }
                | HirExprKind::Unary { .. }
        ) && self
            .scrutinee_root_name(scrutinee)
            .is_some_and(|root| self.lookup_local(&root).is_some())
    }

    pub(crate) fn compile_literal(&mut self, lit: &HirLiteral) -> RuntimeResult<Reg> {
        let (key, value) = literal_const(lit);
        let idx = self.const_idx(key, value);
        let dst = self.alloc_reg();
        self.emit(Op::LoadConst { dst, idx });
        Ok(dst)
    }

    /// Loads an `i64` constant into a fresh boxed `Value::Int` register.
    pub(crate) fn load_int_value(&mut self, value: i64) -> Reg {
        let idx = self.const_idx(ConstKey::Int(value), Value::Int(value));
        let dst = self.alloc_reg();
        self.emit(Op::LoadConst { dst, idx });
        dst
    }

    pub(crate) fn compile_literal_ex(
        &mut self,
        lit: &HirLiteral,
        _ty: Ty,
    ) -> RuntimeResult<TypedReg> {
        match lit {
            HirLiteral::Float(text) => {
                let value = float_literal_value(text);
                let idx = self.f64_const_idx(value);
                let dst = self.alloc_float();
                self.emit(Op::LoadConstF64 { dst_f: dst, idx });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::F64,
                })
            }
            HirLiteral::Int(text) => {
                if let Some(n) = parse_int(text) {
                    let idx = self.i64_const_idx(n);
                    let dst = self.alloc_int();
                    self.emit(Op::LoadConstI64 { dst_i: dst, idx });
                    return Ok(TypedReg {
                        reg: dst,
                        kind: RegKind::I64,
                    });
                }
                let reg = self.compile_literal(lit)?;
                Ok(TypedReg {
                    reg,
                    kind: RegKind::Value,
                })
            }
            _ => {
                let reg = self.compile_literal(lit)?;
                Ok(TypedReg {
                    reg,
                    kind: RegKind::Value,
                })
            }
        }
    }

    /// Phase-2 field-read fast path. When the field's own
    /// type is `f64`, emit `IndexedFieldGetF64` /
    /// `FieldGetF64` so the scalar skips a `Value::Float`
    /// wrap and lands directly in the float register file -
    /// critical for nbody's inner loop, where every
    /// `bodies[i].x` read feeds straight into f64 math.
    /// Releases `reg` once the read that consumed it has run, when `expr` is a
    /// field, index, or tuple read whose value is a temporary of this chain.
    ///
    /// Each step of a read like `st.tables[t].slots.len()` leaves the level it
    /// read in a register. Held there, those levels are shared with the place
    /// they came from, and the next write through that place would copy them.
    pub(crate) fn release_chain_temp(&mut self, expr: &HirExpr, reg: Reg) {
        if matches!(
            expr.kind,
            HirExprKind::Field { .. } | HirExprKind::Index { .. } | HirExprKind::TupleIndex { .. }
        ) {
            self.emit(Op::ClearRegs {
                start: reg,
                count: 1,
            });
        }
    }

    pub(crate) fn compile_field_ex(
        &mut self,
        receiver: &HirExpr,
        name: &Ident,
        field_ty: Ty,
    ) -> RuntimeResult<TypedReg> {
        let field_is_f64 = matches!(self.tcx.kind(field_ty), Some(TyKind::Float(FloatTy::F64)));
        let field_is_i64 = matches!(self.tcx.kind(field_ty), Some(TyKind::Int(IntTy::I64)));
        // Try to resolve the receiver's struct field layout
        // for a compile-time offset. When present, emit an
        // offset-based op so the runtime skips the field-name
        // scan entirely.
        let elem_ty = match &receiver.kind {
            HirExprKind::Index { base, .. } => self.array_elem_ty(base.ty),
            _ => Some(self.unwrap_ref(receiver.ty)),
        };
        let offset = elem_ty.and_then(|t| self.resolve_struct_field_offset(t, name.name.as_str()));
        let name_idx = self.const_idx(
            ConstKey::String(name.name.clone()),
            Value::String(SmolStr::from(name.name.clone())),
        );
        // Fused `base[i].field` - avoids cloning the inner
        // struct `Arc`.
        if let HirExprKind::Index { base, index } = &receiver.kind {
            let base_reg = self.compile_expr(base)?;
            // Compile the index in its native register file. When it
            // lands in the int bank (the common loop-counter case) the
            // flat read can consume it directly, skipping the per-access
            // `BoxI64` that a `Value`-register index would force.
            let idx_tr = self.compile_expr_ex(index)?;
            if field_is_f64 {
                if let Some(offset) = offset {
                    // Known-flat local: emit the dedicated
                    // FloatArray-only read that skips the
                    // discriminant check.
                    if let Some(&stride) = self.flat_locals.get(&base_reg) {
                        let dst = self.alloc_float();
                        if idx_tr.kind == RegKind::I64 {
                            self.emit(Op::FlatGetF64I {
                                dst_f: dst,
                                base: base_reg,
                                index_i: idx_tr.reg,
                                stride,
                                offset,
                            });
                        } else {
                            let idx_reg = self.as_value(idx_tr);
                            self.emit(Op::FlatGetF64 {
                                dst_f: dst,
                                base: base_reg,
                                index: idx_reg,
                                stride,
                                offset,
                            });
                        }
                        self.release_chain_temp(base, base_reg);
                        return Ok(TypedReg {
                            reg: dst,
                            kind: RegKind::F64,
                        });
                    }
                    let idx_reg = self.as_value(idx_tr);
                    let dst = self.alloc_float();
                    self.emit(Op::IndexedFieldGetF64ByOffset {
                        dst_f: dst,
                        base: base_reg,
                        index: idx_reg,
                        offset,
                    });
                    self.release_chain_temp(base, base_reg);
                    return Ok(TypedReg {
                        reg: dst,
                        kind: RegKind::F64,
                    });
                }
                let idx_reg = self.as_value(idx_tr);
                let dst = self.alloc_float();
                self.emit(Op::IndexedFieldGetF64 {
                    dst_f: dst,
                    base: base_reg,
                    index: idx_reg,
                    name_idx,
                });
                self.release_chain_temp(base, base_reg);
                return Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::F64,
                });
            }
            let idx_reg = self.as_value(idx_tr);
            let dst = self.alloc_reg();
            self.emit(Op::IndexedFieldGet {
                dst,
                base: base_reg,
                index: idx_reg,
                name_idx,
            });
            self.release_chain_temp(base, base_reg);
            return Ok(TypedReg {
                reg: dst,
                kind: RegKind::Value,
            });
        }
        // Plain `value.field` - the receiver itself is a
        // single value, so we already avoid the indexed
        // clone. The remaining win is unboxing the scalar
        // into a float reg.
        let recv_reg = self.compile_expr(receiver)?;
        if field_is_i64 {
            let dst = self.alloc_int();
            if let Some(offset) = offset {
                self.emit(Op::FieldGetI64ByOffset {
                    dst_i: dst,
                    receiver: recv_reg,
                    offset,
                });
            } else {
                self.emit(Op::FieldGetI64 {
                    dst_i: dst,
                    receiver: recv_reg,
                    name_idx,
                });
            }
            self.release_chain_temp(receiver, recv_reg);
            return Ok(TypedReg {
                reg: dst,
                kind: RegKind::I64,
            });
        }
        if field_is_f64 {
            if let Some(offset) = offset {
                let dst = self.alloc_float();
                self.emit(Op::FieldGetF64ByOffset {
                    dst_f: dst,
                    receiver: recv_reg,
                    offset,
                });
                self.release_chain_temp(receiver, recv_reg);
                return Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::F64,
                });
            }
            let dst = self.alloc_float();
            self.emit(Op::FieldGetF64 {
                dst_f: dst,
                receiver: recv_reg,
                name_idx,
            });
            self.release_chain_temp(receiver, recv_reg);
            return Ok(TypedReg {
                reg: dst,
                kind: RegKind::F64,
            });
        }
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_field_cache_idx();
        self.emit(Op::FieldGet {
            dst,
            receiver: recv_reg,
            name_idx,
            cache_idx,
        });
        self.release_chain_temp(receiver, recv_reg);
        Ok(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        })
    }

    pub(crate) fn compile_short_circuit(
        &mut self,
        op: HirBinaryOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> RuntimeResult<Reg> {
        let result = self.alloc_reg();
        let lhs_reg = self.compile_expr(lhs)?;
        self.emit(Op::Move {
            dst: result,
            src: lhs_reg,
        });
        let branch_idx = match op {
            HirBinaryOp::And => self.emit(Op::BranchIfNot {
                cond: result,
                target: 0,
            }),
            HirBinaryOp::Or => self.emit(Op::BranchIf {
                cond: result,
                target: 0,
            }),
            _ => unreachable!(),
        };
        let rhs_reg = self.compile_expr(rhs)?;
        self.emit(Op::Move {
            dst: result,
            src: rhs_reg,
        });
        let after = self.cur_idx();
        self.patch_jump(branch_idx, after);
        Ok(result)
    }

    /// `a & b`, `a | b`, and `a ^ b` on `bool`s: both sides evaluated, as a
    /// bitwise operator evaluates them, then combined - `^` as `!=`, the
    /// others by keeping the left value unless it decides nothing.
    pub(crate) fn compile_bool_bitwise(
        &mut self,
        op: HirBinaryOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> RuntimeResult<Reg> {
        let lhs_reg = self.compile_expr(lhs)?;
        let rhs_reg = self.compile_expr(rhs)?;
        let result = self.alloc_reg();
        if op == HirBinaryOp::BitXor {
            self.emit(Op::Ne {
                dst: result,
                lhs: lhs_reg,
                rhs: rhs_reg,
            });
            return Ok(result);
        }
        self.emit(Op::Move {
            dst: result,
            src: lhs_reg,
        });
        let branch_idx = if op == HirBinaryOp::BitAnd {
            self.emit(Op::BranchIfNot {
                cond: result,
                target: 0,
            })
        } else {
            self.emit(Op::BranchIf {
                cond: result,
                target: 0,
            })
        };
        self.emit(Op::Move {
            dst: result,
            src: rhs_reg,
        });
        let after = self.cur_idx();
        self.patch_jump(branch_idx, after);
        Ok(result)
    }

    /// [`Self::try_compile_inplace_vec_stmt`] for a Vec reached through a
    /// field or an index rooted at a local (`st.tables[t].slots.resize(n, 0)`).
    /// The Vec is grown where it lies, each level on the way made unique from
    /// the root, where the generic builtin would rebuild every element on each
    /// call.
    fn try_compile_inplace_vec_place_stmt(
        &mut self,
        receiver: &HirExpr,
        name: &Ident,
        args: &[HirExpr],
    ) -> RuntimeResult<bool> {
        if !matches!(
            receiver.kind,
            HirExprKind::Field { .. } | HirExprKind::Index { .. } | HirExprKind::TupleIndex { .. }
        ) || !self.place_root_is_local(receiver)
            || !matches!(self.tcx.kind(receiver.ty), Some(TyKind::Vec(_)))
        {
            return Ok(false);
        }
        match (name.name.as_str(), args.len()) {
            ("resize", 2) => {
                let len = self.compile_expr(&args[0])?;
                let fill = self.compile_expr(&args[1])?;
                if let Some((root, path)) = self.compile_place_path(receiver, 1)? {
                    let idx = u16::try_from(self.wide_ops.len()).expect("wide_ops index overflow");
                    self.wide_ops.push(crate::bytecode::WideOp::PlaceVecResize {
                        root,
                        path,
                        len,
                        fill,
                    });
                    self.emit(Op::Wide { idx });
                    return Ok(true);
                }
                let vec_reg = self.compile_expr(receiver)?;
                self.emit(Op::VecResize {
                    receiver: vec_reg,
                    len,
                    fill,
                });
                self.compile_place_store(receiver, vec_reg)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Emits a dedicated in-place Vec op (`VecPush` / `VecInsert` /
    /// `VecRemove`) for a bare-local Vec receiver whose mutating
    /// method's result is discarded (statement position). Returns
    /// `true` when it handled the call. The op mutates the receiver
    /// register's backing storage directly via `Arc::make_mut`, so a
    /// `push` loop grows in amortized O(1) instead of deep-copying the
    /// whole Vec per call. Other receiver shapes (index / field /
    /// temporary) and expression-position uses keep the generic
    /// builtin + writeback path, which produces the same final state.
    pub(crate) fn try_compile_inplace_vec_stmt(
        &mut self,
        receiver: &HirExpr,
        name: &Ident,
        args: &[HirExpr],
    ) -> RuntimeResult<bool> {
        if !matches!(receiver.kind, HirExprKind::Path { .. }) {
            return self.try_compile_inplace_vec_place_stmt(receiver, name, args);
        }
        let HirExprKind::Path { segments, .. } = &receiver.kind else {
            return Ok(false);
        };
        let [seg] = segments.as_slice() else {
            return Ok(false);
        };
        // Concrete Vec-like receivers only. `String` / `bytes::Builder`
        // also carry `push` / `insert` / `remove` but back onto different
        // storage, and an unresolved `Var` receiver (e.g. `String::new()`)
        // can be any of them - those keep the generic dispatch, which
        // selects the right builtin by the runtime value's type.
        if matches!(self.tcx.kind(receiver.ty), Some(TyKind::Array { .. })) {
            return Ok(false);
        }
        let is_concrete_vec = matches!(self.tcx.kind(receiver.ty), Some(TyKind::Vec(_)));
        if matches!(self.tcx.kind(receiver.ty), Some(TyKind::HashMap { .. })) {
            return Ok(false);
        }
        let target_reg = match self.lookup_local(&seg.name) {
            Some(target) if target.kind == RegKind::Value => target.reg,
            _ => return Ok(false),
        };
        // A flat typed-storage local (`IntArray` / `FloatVec`) or a local
        // tagged at `let` time as a Vec constructor / array literal is a
        // Vec even when its static type stayed an inference var.
        if !is_concrete_vec
            && !self.flat_int_locals.contains(&target_reg)
            && !self.flat_float_locals.contains(&target_reg)
            && !self.collection_locals.contains(&target_reg)
        {
            return Ok(false);
        }
        match (name.name.as_str(), args.len()) {
            ("push", 1) => {
                let value = self.compile_expr(&args[0])?;
                let value = self.stored_value_reg(&args[0], value);
                self.emit(Op::VecPush {
                    receiver: target_reg,
                    value,
                });
                Ok(true)
            }
            ("insert", 2) => {
                let index = self.compile_expr(&args[0])?;
                let value = self.compile_expr(&args[1])?;
                let value = self.stored_value_reg(&args[1], value);
                let dst = self.alloc_reg();
                self.emit(Op::VecInsert {
                    dst,
                    receiver: target_reg,
                    index,
                    value,
                });
                Ok(true)
            }
            ("remove", 1) => {
                let index = self.compile_expr(&args[0])?;
                self.emit(Op::VecRemove {
                    receiver: target_reg,
                    index,
                });
                Ok(true)
            }
            ("resize", 2) => {
                let len = self.compile_expr(&args[0])?;
                let fill = self.compile_expr(&args[1])?;
                self.emit(Op::VecResize {
                    receiver: target_reg,
                    len,
                    fill,
                });
                Ok(true)
            }
            ("swap", 2) if self.flat_int_locals.contains(&target_reg) => {
                let i = self.compile_expr_ex(&args[0])?;
                let i_i = self.as_i64(i);
                let j = self.compile_expr_ex(&args[1])?;
                let j_i = self.as_i64(j);
                self.emit(Op::IntArraySwap {
                    base: target_reg,
                    i_i,
                    j_i,
                });
                Ok(true)
            }
            ("swap", 2) if self.flat_float_locals.contains(&target_reg) => {
                let i = self.compile_expr_ex(&args[0])?;
                let i_i = self.as_i64(i);
                let j = self.compile_expr_ex(&args[1])?;
                let j_i = self.as_i64(j);
                self.emit(Op::FloatVecSwap {
                    base: target_reg,
                    i_i,
                    j_i,
                });
                Ok(true)
            }
            ("swap", 2) => {
                let a = self.compile_expr(&args[0])?;
                let b = self.compile_expr(&args[1])?;
                self.emit(Op::VecSwapDiscard {
                    receiver: target_reg,
                    a,
                    b,
                });
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn try_compile_i64_wrapping_method(
        &mut self,
        receiver: &HirExpr,
        name: &Ident,
        args: &[HirExpr],
    ) -> RuntimeResult<Option<TypedReg>> {
        let arith = match name.name.as_str() {
            "__gos_wrapping_add" => ImmArithKind::Add,
            "__gos_wrapping_sub" => ImmArithKind::Sub,
            "__gos_wrapping_mul" => ImmArithKind::Mul,
            _ => return Ok(None),
        };
        if args.len() != 1 {
            return Ok(None);
        }
        let mut kind = self.tcx.kind(receiver.ty).cloned();
        while let Some(TyKind::Ref { inner, .. }) = kind {
            kind = self.tcx.kind(inner).cloned();
        }
        if !matches!(
            kind,
            Some(TyKind::Int(
                gossamer_types::IntTy::I64 | gossamer_types::IntTy::Isize
            ))
        ) {
            return Ok(None);
        }

        let lhs_tr = self.compile_expr_ex(receiver)?;
        let lhs_i = self.as_i64(lhs_tr);
        let dst_i = self.alloc_int();
        if arith == ImmArithKind::Add
            && let HirExprKind::MethodCall {
                receiver: byte_receiver,
                name: byte_name,
                args: byte_args,
                ..
            } = &args[0].kind
            && byte_name.name == "byte_at"
            && byte_args.len() == 1
        {
            let mut byte_receiver_kind = self.tcx.kind(byte_receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = byte_receiver_kind {
                byte_receiver_kind = self.tcx.kind(inner).cloned();
            }
            if matches!(byte_receiver_kind, Some(TyKind::String)) {
                let recv = self.compile_expr(byte_receiver)?;
                let index = self.compile_expr_ex(&byte_args[0])?;
                let idx_i = self.as_i64(index);
                self.emit(Op::StrByteAtAddI64 {
                    dst_i,
                    lhs_i,
                    recv,
                    idx_i,
                });
                return Ok(Some(TypedReg {
                    reg: dst_i,
                    kind: RegKind::I64,
                }));
            }
        }
        let immediate = match &args[0].kind {
            HirExprKind::Literal(HirLiteral::Int(text)) => parse_int(text),
            HirExprKind::Unary {
                op: HirUnaryOp::Neg,
                operand,
            } => match &operand.kind {
                HirExprKind::Literal(HirLiteral::Int(text)) => {
                    parse_int(text).and_then(i64::checked_neg)
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(value) = immediate
            && let Ok(imm) = i32::try_from(value)
        {
            self.emit(Op::ArithImmI64 {
                kind: arith,
                dst_i,
                lhs_i,
                imm,
            });
        } else {
            let rhs_tr = self.compile_expr_ex(&args[0])?;
            let rhs_i = self.as_i64(rhs_tr);
            self.emit(match arith {
                ImmArithKind::Add => Op::AddI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                },
                ImmArithKind::Sub => Op::SubI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                },
                _ => Op::MulI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                },
            });
        }
        Ok(Some(TypedReg {
            reg: dst_i,
            kind: RegKind::I64,
        }))
    }

    /// True when a `.downgrade()` receiver of this type names an allocation a
    /// weak can observe: a user struct, enum, tuple, or array. Mirrors the MIR
    /// lowering's rule so both tiers agree on which receivers yield a weak
    /// that can ever upgrade.
    fn weak_referent_is_observable(&self, ty: gossamer_types::Ty) -> bool {
        let mut ty = ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
            ty = *inner;
        }
        match self.tcx.kind(ty) {
            // Stdlib sentinel Adts (`u32::MAX - 16 ..= u32::MAX`) are opaque
            // runtime handles; inline enums are by-value words.
            Some(TyKind::Adt { def, .. }) => {
                def.local < u32::MAX - 16 && !self.tcx.is_inline_enum_ty(ty)
            }
            Some(TyKind::Tuple(_) | TyKind::Array { .. }) => true,
            // An unresolved receiver keeps the general path; the checker
            // rejects the by-value cases it can prove.
            other => other.is_none(),
        }
    }

    /// `x.downgrade()` - the weak observes a strong reference pinned in a
    /// frame-lifetime register belonging to this call site. Liveness of a
    /// `Weak` is observable through `upgrade`, so the referent must stay
    /// reachable for the rest of the frame however the source binding is
    /// consumed, cleared at its last use, or overwritten. Re-executing the
    /// site (a downgrade in a loop) overwrites the pin, which releases the
    /// previous referent - the same schedule the compiled tiers keep.
    fn compile_downgrade(&mut self, receiver: &HirExpr) -> RuntimeResult<Reg> {
        let source = if self.weak_referent_is_observable(receiver.ty) {
            self.compile_expr(receiver)?
        } else {
            // An opaque runtime handle (a `Set`, a socket, an io stream) is
            // owned by the runtime and has no reference count of its own, so
            // its weak can never upgrade. The receiver is still evaluated for
            // its effects; downgrading unit is the dead-weak handle.
            self.compile_expr(receiver)?;
            self.load_unit()
        };
        let pin = self.alloc_reg();
        self.emit(Op::Move {
            dst: pin,
            src: source,
        });
        self.escaped_reference_reg_floor =
            self.escaped_reference_reg_floor.max(pin.saturating_add(1));
        let args_start = self.next_reg;
        self.ensure_reg_slot(args_start);
        let dst = self.alloc_reg();
        let name_idx = self.global_idx("downgrade");
        let cache_idx = self.alloc_cache_idx();
        self.emit(Op::MethodCall {
            dst,
            receiver: pin,
            name_idx,
            args: args_start,
            argc: 0,
            cache_idx,
        });
        Ok(dst)
    }

    /// Extended call compiler that takes the call's **result** type.
    /// Used by callers that have it on hand (for example
    /// `HirExprKind::Call`'s `expr.ty`) so the typed
    /// `HashMap<i64, i64>` construction can route to
    /// `Op::BuildIntMap` instead of the generic `builtin_map_new`
    /// path.
    /// Whether `callee` names a function whose parameter `idx` it only reads,
    /// per the summary the compiled tiers lower against.
    pub(crate) fn callee_only_reads_param(&self, callee: &HirExpr, idx: usize) -> bool {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return false;
        };
        let joined = segments
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("::");
        let bare = segments.last().map(|s| s.name.as_str()).unwrap_or_default();
        self.fn_param_shareable
            .get(&joined)
            .or_else(|| self.fn_param_shareable.get(bare))
            .and_then(|flags| flags.get(idx).copied())
            .unwrap_or(false)
    }

    /// The path `expr` names under this chunk's instantiation, when dispatch
    /// resolved one: a trait function reached through a type parameter, or a
    /// call into a function compiled per instantiation. The returned path
    /// spells the global to load as a single segment.
    pub(crate) fn dispatched_path(&self, expr: &HirExpr) -> Option<HirExpr> {
        let table = self.dispatch?;
        if !matches!(expr.kind, HirExprKind::Path { .. }) {
            return None;
        }
        let target = table.target(self.dispatch_key, expr.id)?;
        Some(HirExpr {
            id: expr.id,
            span: expr.span,
            ty: expr.ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(target.to_string())],
                def: None,
            },
        })
    }

    /// A method call dispatch pointed at a per-instantiation chunk, spelled as
    /// the direct call it is: the instance's global with the receiver as its
    /// first argument. A `&mut self` receiver is passed as `&mut receiver`, so
    /// the call's write-back protocol publishes the method's mutation to the
    /// caller's place exactly as the method-call path does.
    fn dispatched_method_call(
        &self,
        expr: &HirExpr,
        receiver: &HirExpr,
        args: &[HirExpr],
    ) -> Option<HirExpr> {
        let table = self.dispatch?;
        let target = table.target(self.dispatch_key, expr.id)?;
        let mutable_receiver = self
            .fn_param_tys
            .get(target)
            .and_then(|params| params.first())
            .is_some_and(|ty| {
                matches!(
                    self.tcx.kind(*ty),
                    Some(TyKind::Ref {
                        mutability: gossamer_types::Mutbl::Mut,
                        ..
                    })
                )
            });
        let receiver_arg = if mutable_receiver {
            HirExpr {
                id: receiver.id,
                span: receiver.span,
                ty: receiver.ty,
                kind: HirExprKind::Unary {
                    op: HirUnaryOp::RefMut,
                    operand: Box::new(receiver.clone()),
                },
            }
        } else {
            receiver.clone()
        };
        let mut call_args = Vec::with_capacity(args.len() + 1);
        call_args.push(receiver_arg);
        call_args.extend(args.iter().cloned());
        Some(HirExpr {
            id: expr.id,
            span: expr.span,
            ty: expr.ty,
            kind: HirExprKind::Call {
                callee: Box::new(HirExpr {
                    id: expr.id,
                    span: expr.span,
                    ty: expr.ty,
                    kind: HirExprKind::Path {
                        segments: vec![Ident::new(target.to_string())],
                        def: None,
                    },
                }),
                args: call_args,
            },
        })
    }
}

/// Operator-overload impl-method name for an arithmetic binary operator
/// (`+` -> `add`, `-` -> `sub`, `*` -> `mul`, `/` -> `div`), or `None` when
/// the operator does not dispatch to a user method.
fn arith_overload_method(op: HirBinaryOp) -> Option<&'static str> {
    match op {
        HirBinaryOp::Add => Some("add"),
        HirBinaryOp::Sub => Some("sub"),
        HirBinaryOp::Mul => Some("mul"),
        HirBinaryOp::Div => Some("div"),
        HirBinaryOp::Rem => Some("rem"),
        HirBinaryOp::BitAnd => Some("bitand"),
        HirBinaryOp::BitOr => Some("bitor"),
        HirBinaryOp::BitXor => Some("bitxor"),
        HirBinaryOp::Shl => Some("shl"),
        HirBinaryOp::Shr => Some("shr"),
        _ => None,
    }
}

/// The width-specific builtin a `wrapping_*` method call on an integer of
/// type `int_ty` dispatches to, so the result wraps at that type's width.
fn wrapping_dispatch_name(int_ty: gossamer_types::IntTy, method: &str) -> Option<&'static str> {
    use gossamer_types::IntTy;
    let names: [&'static str; 3] = match int_ty {
        IntTy::I8 => [
            "i8::__gos_wrapping_add",
            "i8::__gos_wrapping_sub",
            "i8::__gos_wrapping_mul",
        ],
        IntTy::I16 => [
            "i16::__gos_wrapping_add",
            "i16::__gos_wrapping_sub",
            "i16::__gos_wrapping_mul",
        ],
        IntTy::I32 => [
            "i32::__gos_wrapping_add",
            "i32::__gos_wrapping_sub",
            "i32::__gos_wrapping_mul",
        ],
        IntTy::I64 => [
            "i64::__gos_wrapping_add",
            "i64::__gos_wrapping_sub",
            "i64::__gos_wrapping_mul",
        ],
        IntTy::Isize => [
            "isize::__gos_wrapping_add",
            "isize::__gos_wrapping_sub",
            "isize::__gos_wrapping_mul",
        ],
        IntTy::U8 => [
            "u8::__gos_wrapping_add",
            "u8::__gos_wrapping_sub",
            "u8::__gos_wrapping_mul",
        ],
        IntTy::U16 => [
            "u16::__gos_wrapping_add",
            "u16::__gos_wrapping_sub",
            "u16::__gos_wrapping_mul",
        ],
        IntTy::U32 => [
            "u32::__gos_wrapping_add",
            "u32::__gos_wrapping_sub",
            "u32::__gos_wrapping_mul",
        ],
        IntTy::U64 => [
            "u64::__gos_wrapping_add",
            "u64::__gos_wrapping_sub",
            "u64::__gos_wrapping_mul",
        ],
        IntTy::Usize => [
            "usize::__gos_wrapping_add",
            "usize::__gos_wrapping_sub",
            "usize::__gos_wrapping_mul",
        ],
        IntTy::I128 | IntTy::U128 => return None,
    };
    let index = match method {
        "__gos_wrapping_add" => 0,
        "__gos_wrapping_sub" => 1,
        "__gos_wrapping_mul" => 2,
        _ => return None,
    };
    Some(names[index])
}
