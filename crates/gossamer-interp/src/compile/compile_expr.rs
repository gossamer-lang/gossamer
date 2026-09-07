#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

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
                match (dst_kind, src_kind) {
                    (RegKind::F64, RegKind::I64) => {
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
                        if !matches!(
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
            HirExprKind::Path { segments, def, .. } => self.compile_path(segments, *def),
            HirExprKind::Unary { op, operand } => self.compile_unary(*op, operand),
            HirExprKind::Binary { op, lhs, rhs } => self.compile_binary(*op, lhs, rhs),
            HirExprKind::Assign { place, value } => self.compile_assign(place, value),
            // Route through `_ex` so intrinsic-style calls
            // (`math::sqrt(x)`, etc.) get lowered to dedicated
            // opcodes when the arg kind is concrete f64, even
            // inside functions whose bodies are compiled via
            // the regular path (e.g. `fn fsqrt(x) { math::sqrt(x) }`).
            HirExprKind::Call { callee, args } => {
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

    /// Native `match` compilation. Emits the scrutinee once, then a
    /// test-and-branch chain per arm: each arm's pattern lowers to a
    /// sequence of shape tests (`VariantIs` / `StructIs` / literal
    /// `Eq` / range compares) that branch to the next arm on failure
    /// and extract sub-values into freshly-bound registers on
    /// success. Every pattern shape - including or-patterns that bind -
    /// lowers natively via [`Self::emit_pattern_test`].
    pub(crate) fn compile_match(
        &mut self,
        scrutinee: &HirExpr,
        arms: &[gossamer_hir::HirMatchArm],
        _whole: &HirExpr,
    ) -> RuntimeResult<Reg> {
        let scrut = self.compile_expr(scrutinee)?;
        let result = self.alloc_reg();
        let mut end_jumps: Vec<InstrIdx> = Vec::new();
        let scrut_ty = scrutinee.ty;
        // Move-on-last-use: a guard-free `match` may drain the matched
        // payload out of a uniquely-owned scrutinee instead of cloning
        // it. Eligible when the scrutinee is a consumable local or a
        // fresh temporary (e.g. the `?` desugar's `match parse(x) { ...
        // }`) - both leave a register nothing reads after the match.
        // Guards are excluded because a failed guard would fall through
        // and re-extract the drained scrutinee.
        let consume_eligible =
            self.value_consumable_here(scrutinee) && arms.iter().all(|arm| arm.guard.is_none());
        for arm in arms {
            self.push_scope();
            let mut fails: Vec<InstrIdx> = Vec::new();
            // Draining the scrutinee is only safe when this arm cannot fail
            // a refutable sub-test *after* extracting (and emptying) a field
            // and then fall through to a later arm that re-reads it. See
            // `pattern_consume_safe`.
            let arm_consume =
                consume_eligible && crate::compile::consume::pattern_consume_safe(&arm.pattern);
            self.emit_pattern_test_ex(scrut, &arm.pattern, &mut fails, arm_consume)?;
            // Tag collection-typed pattern bindings (`Some(arr)`, …) so a
            // `for x in <binding>` in the arm body iterates by index. The
            // binding's own `Path` carries an unresolved var when inferred,
            // so the type comes from the resolved scrutinee type instead.
            let mut coll_names: Vec<String> = Vec::new();
            self.collect_collection_binding_names(&arm.pattern, Some(scrut_ty), &mut coll_names);
            for name in coll_names {
                if let Some(tr) = self.lookup_local(&name) {
                    self.collection_locals.insert(tr.reg);
                }
            }
            if let Some(guard) = &arm.guard {
                let g = self.compile_expr(guard)?;
                fails.push(self.emit(Op::BranchIfNot { cond: g, target: 0 }));
            }
            // Hand the arm's result to the match register instead of
            // cloning when its last use is here (the `?` desugar's
            // `Ok(__try_value) => __try_value` keeps the unwrapped value
            // uniquely owned this way).
            let consume_body = self.value_consumable_here(&arm.body);
            let body_reg = self.compile_expr(&arm.body)?;
            if consume_body {
                self.emit(Op::MoveConsume {
                    dst: result,
                    src: body_reg,
                });
            } else {
                self.emit(Op::Move {
                    dst: result,
                    src: body_reg,
                });
            }
            end_jumps.push(self.emit(Op::Jump { target: 0 }));
            self.pop_scope();
            let next = self.cur_idx();
            for f in fails {
                self.patch_jump(f, next);
            }
        }
        // No arm matched. Exhaustiveness covers well-typed programs, but a
        // checker blind spot (e.g. an unenumerable integer payload like
        // `Some(2)` against `Some(0) | Some(1) | None`) can reach here.
        // Panic cleanly with the same message the compiled tiers emit
        // rather than falling through with a zero/default value.
        let msg = self.const_idx(
            ConstKey::String(NON_EXHAUSTIVE_MATCH_MESSAGE.to_string()),
            Value::String(SmolStr::from(NON_EXHAUSTIVE_MATCH_MESSAGE)),
        );
        self.emit(Op::Panic { msg });
        let end = self.cur_idx();
        for j in end_jumps {
            self.patch_jump(j, end);
        }
        Ok(result)
    }

    /// Compiles a `match` whose value is discarded (statement position).
    /// Each arm body is compiled in statement context via
    /// `compile_expr_discarded`, so a tail-position in-place mutation
    /// (`v.push(x)`) lowers to its dedicated op rather than the
    /// value-returning builtin path that deep-copies the whole
    /// collection per call. Mirrors `compile_match` minus the result
    /// register and per-arm `Move`.
    pub(crate) fn compile_match_discarded(
        &mut self,
        scrutinee: &HirExpr,
        arms: &[gossamer_hir::HirMatchArm],
        _whole: &HirExpr,
    ) -> RuntimeResult<()> {
        let scrut = self.compile_expr(scrutinee)?;
        let mut end_jumps: Vec<InstrIdx> = Vec::new();
        let scrut_ty = scrutinee.ty;
        for arm in arms {
            self.push_scope();
            let mut fails: Vec<InstrIdx> = Vec::new();
            self.emit_pattern_test(scrut, &arm.pattern, &mut fails)?;
            let mut coll_names: Vec<String> = Vec::new();
            self.collect_collection_binding_names(&arm.pattern, Some(scrut_ty), &mut coll_names);
            for name in coll_names {
                if let Some(tr) = self.lookup_local(&name) {
                    self.collection_locals.insert(tr.reg);
                }
            }
            if let Some(guard) = &arm.guard {
                let g = self.compile_expr(guard)?;
                fails.push(self.emit(Op::BranchIfNot { cond: g, target: 0 }));
            }
            self.compile_expr_discarded(&arm.body)?;
            end_jumps.push(self.emit(Op::Jump { target: 0 }));
            self.pop_scope();
            let next = self.cur_idx();
            for f in fails {
                self.patch_jump(f, next);
            }
        }
        let msg = self.const_idx(
            ConstKey::String(NON_EXHAUSTIVE_MATCH_MESSAGE.to_string()),
            Value::String(SmolStr::from(NON_EXHAUSTIVE_MATCH_MESSAGE)),
        );
        self.emit(Op::Panic { msg });
        let end = self.cur_idx();
        for j in end_jumps {
            self.patch_jump(j, end);
        }
        Ok(())
    }

    /// Native `select { … }` compilation. Evaluates each arm's
    /// channel (and value, for sends) into registers up front, emits a
    /// single [`Op::Select`] referencing a contiguous range of
    /// [`crate::bytecode::SelectArmMeta`] entries, then compiles every
    /// arm body as a basic block the op jumps to. Recv arms destructure
    /// the received value (written into the arm's `bind_reg` by the
    /// handler) at the top of their block. The handler's poll/park loop
    /// operates over `Value::Channel`.
    pub(crate) fn compile_select(
        &mut self,
        arms: &[gossamer_hir::HirSelectArm],
    ) -> RuntimeResult<Reg> {
        use crate::bytecode::{SelectArmKind, SelectArmMeta};
        use gossamer_hir::HirSelectOp;

        // An empty `select {}` blocks forever in Go; degrade it to Unit
        // rather than emitting an arm-less op that would spin in the
        // park loop.
        if arms.is_empty() {
            return Ok(self.load_unit());
        }

        let result = self.alloc_reg();
        let first = u32::try_from(self.select_arms.len())
            .map_err(|_| RuntimeError::Unsupported("too many select arms in one function"))?;

        // Pass 1: evaluate operand expressions (channels / send values)
        // and pre-allocate recv binding registers. The `body_block`
        // index is filled in pass 2 once each body is laid down.
        for arm in arms {
            let meta = match &arm.op {
                HirSelectOp::Recv { channel, .. } => {
                    let channel_reg = self.compile_expr(channel)?;
                    let bind_reg = self.alloc_reg();
                    SelectArmMeta {
                        kind: SelectArmKind::Recv,
                        channel_reg,
                        value_reg: 0,
                        bind_reg,
                        body_block: 0,
                    }
                }
                HirSelectOp::Send { channel, value } => {
                    let channel_reg = self.compile_expr(channel)?;
                    let value_reg = self.compile_expr(value)?;
                    SelectArmMeta {
                        kind: SelectArmKind::Send,
                        channel_reg,
                        value_reg,
                        bind_reg: 0,
                        body_block: 0,
                    }
                }
                HirSelectOp::Default => SelectArmMeta {
                    kind: SelectArmKind::Default,
                    channel_reg: 0,
                    value_reg: 0,
                    bind_reg: 0,
                    body_block: 0,
                },
            };
            self.select_arms.push(meta);
        }
        let count = u16::try_from(arms.len())
            .map_err(|_| RuntimeError::Unsupported("too many select arms in one select"))?;
        self.emit(Op::Select { first, count });

        // Pass 2: each arm body becomes a basic block. The handler
        // jumps to one of them; every block moves its result into the
        // shared `result` register and jumps to the continuation.
        let mut end_jumps: Vec<InstrIdx> = Vec::new();
        for (i, arm) in arms.iter().enumerate() {
            let meta_idx = first as usize + i;
            self.select_arms[meta_idx].body_block = self.cur_idx();
            self.push_scope();
            if let HirSelectOp::Recv { pattern, .. } = &arm.op {
                let bind_reg = self.select_arms[meta_idx].bind_reg;
                self.bind_pattern_locals(pattern, bind_reg)?;
            }
            let body_reg = self.compile_expr(&arm.body)?;
            self.emit(Op::Move {
                dst: result,
                src: body_reg,
            });
            end_jumps.push(self.emit(Op::Jump { target: 0 }));
            self.pop_scope();
        }
        let end = self.cur_idx();
        for j in end_jumps {
            self.patch_jump(j, end);
        }
        Ok(result)
    }

    /// Emits the shape-test + binding-extraction sequence for one
    /// pattern against the value in `scrut`. Pushes a branch index
    /// onto `fails` for every test that must jump to the next arm on
    /// mismatch; on the fall-through (match) path the pattern's
    /// bindings are live in the current scope.
    pub(crate) fn emit_pattern_test(
        &mut self,
        scrut: Reg,
        pat: &HirPat,
        fails: &mut Vec<InstrIdx>,
    ) -> RuntimeResult<()> {
        self.emit_pattern_test_ex(scrut, pat, fails, false)
    }

    /// Like [`Self::emit_pattern_test`] but with a `consume` flag: when
    /// set, the scrutinee is a uniquely-owned value the arm may drain
    /// (guard-free `match` on a consumable local), so variant-field and
    /// binding extraction move instead of clone. The runtime
    /// `Arc::get_mut` guard on `VariantFieldConsume` degrades a
    /// still-shared scrutinee to a safe clone.
    pub(crate) fn emit_pattern_test_ex(
        &mut self,
        scrut: Reg,
        pat: &HirPat,
        fails: &mut Vec<InstrIdx>,
        consume: bool,
    ) -> RuntimeResult<()> {
        match &pat.kind {
            HirPatKind::Wildcard | HirPatKind::Rest => {}
            HirPatKind::Binding { name, .. } => {
                // Copy into a fresh reg so a `let mut`-style rebind in
                // the arm body can't clobber the scrutinee register.
                // Under `consume` the scrutinee is a read-once uniquely
                // owned value, so hand it over instead of cloning.
                let r = self.alloc_reg();
                if consume {
                    self.emit(Op::MoveConsume { dst: r, src: scrut });
                } else {
                    self.emit(Op::Move { dst: r, src: scrut });
                }
                self.bind_local(
                    &name.name,
                    TypedReg {
                        reg: r,
                        kind: RegKind::Value,
                    },
                );
            }
            HirPatKind::Literal(lit) => {
                let lit_reg = self.compile_literal(lit)?;
                let eq = self.alloc_reg();
                self.emit(Op::Eq {
                    dst: eq,
                    lhs: scrut,
                    rhs: lit_reg,
                });
                fails.push(self.emit(Op::BranchIfNot {
                    cond: eq,
                    target: 0,
                }));
            }
            HirPatKind::Variant { name, fields } => {
                let name_idx = self.shape_name_idx(name.name.as_str());
                let arity = u16::try_from(fields.len())
                    .map_err(|_| RuntimeError::Unsupported("variant arity exceeds 65535"))?;
                let test = self.alloc_reg();
                self.emit(Op::VariantIs {
                    dst: test,
                    src: scrut,
                    name_idx,
                    arity,
                });
                fails.push(self.emit(Op::BranchIfNot {
                    cond: test,
                    target: 0,
                }));
                for (i, fp) in fields.iter().enumerate() {
                    let fr = self.alloc_reg();
                    let idx = u16::try_from(i).expect("field index overflow");
                    if consume {
                        self.emit(Op::VariantFieldConsume {
                            dst: fr,
                            src: scrut,
                            idx,
                        });
                    } else {
                        self.emit(Op::VariantField {
                            dst: fr,
                            src: scrut,
                            idx,
                        });
                    }
                    // A drained field is uniquely owned, so propagate
                    // `consume` into its sub-pattern.
                    self.emit_pattern_test_ex(fr, fp, fails, consume)?;
                }
            }
            HirPatKind::Struct { name, fields, .. } => {
                let name_idx = self.shape_name_idx(name.name.as_str());
                let test = self.alloc_reg();
                self.emit(Op::StructIs {
                    dst: test,
                    src: scrut,
                    name_idx,
                });
                fails.push(self.emit(Op::BranchIfNot {
                    cond: test,
                    target: 0,
                }));
                for fp in fields {
                    let fname_idx = self.const_idx(
                        ConstKey::String(fp.name.name.clone()),
                        Value::String(SmolStr::from(fp.name.name.as_str())),
                    );
                    let fr = self.alloc_reg();
                    let cache_idx = self.alloc_field_cache_idx();
                    self.emit(Op::FieldGet {
                        dst: fr,
                        receiver: scrut,
                        name_idx: fname_idx,
                        cache_idx,
                    });
                    if let Some(sub) = &fp.pattern {
                        self.emit_pattern_test(fr, sub, fails)?;
                    } else {
                        // `Struct { field }` shorthand binds `field`.
                        self.bind_local(
                            &fp.name.name,
                            TypedReg {
                                reg: fr,
                                kind: RegKind::Value,
                            },
                        );
                    }
                }
            }
            HirPatKind::Ref { inner, .. } => {
                self.emit_pattern_test_ex(scrut, inner, fails, consume)?;
            }
            HirPatKind::At { name, sub, .. } => {
                self.emit_pattern_test(scrut, sub, fails)?;
                let r = self.alloc_reg();
                self.emit(Op::Move { dst: r, src: scrut });
                self.bind_local(
                    &name.name,
                    TypedReg {
                        reg: r,
                        kind: RegKind::Value,
                    },
                );
            }
            HirPatKind::Range { lo, hi, inclusive } => {
                let lo_reg = self.compile_literal(lo)?;
                let ge = self.alloc_reg();
                self.emit(Op::Ge {
                    dst: ge,
                    lhs: scrut,
                    rhs: lo_reg,
                });
                fails.push(self.emit(Op::BranchIfNot {
                    cond: ge,
                    target: 0,
                }));
                let hi_reg = self.compile_literal(hi)?;
                let cmp = self.alloc_reg();
                if *inclusive {
                    self.emit(Op::Le {
                        dst: cmp,
                        lhs: scrut,
                        rhs: hi_reg,
                    });
                } else {
                    self.emit(Op::Lt {
                        dst: cmp,
                        lhs: scrut,
                        rhs: hi_reg,
                    });
                }
                fails.push(self.emit(Op::BranchIfNot {
                    cond: cmp,
                    target: 0,
                }));
            }
            HirPatKind::Tuple(parts) => {
                let rest_pos = parts
                    .iter()
                    .position(|p| matches!(p.kind, HirPatKind::Rest));
                for (i, part) in parts.iter().enumerate() {
                    if matches!(part.kind, HirPatKind::Rest) {
                        continue;
                    }
                    let elem = self.alloc_reg();
                    match rest_pos {
                        Some(rp) if i > rp => {
                            // Element after `..` - index from the end.
                            let from_end = parts.len() - 1 - i;
                            self.emit(Op::TupleTailIndex {
                                dst: elem,
                                receiver: scrut,
                                offset_from_end: u32::try_from(from_end)
                                    .expect("tuple tail index overflow"),
                            });
                        }
                        _ => {
                            self.emit(Op::TupleIndex {
                                dst: elem,
                                receiver: scrut,
                                index: u32::try_from(i).expect("tuple index overflow"),
                            });
                        }
                    }
                    self.emit_pattern_test(elem, part, fails)?;
                }
            }
            HirPatKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                // `len = scrut.len()` as a boxed `Value::Int`.
                let len_reg = self.alloc_reg();
                let len_name = self.global_idx("len");
                let cache_idx = self.alloc_cache_idx();
                self.emit(Op::MethodCall {
                    dst: len_reg,
                    receiver: scrut,
                    name_idx: len_name,
                    args: 0,
                    argc: 0,
                    cache_idx,
                });
                let n_prefix = i64::try_from(prefix.len())
                    .map_err(|_| RuntimeError::Unsupported("slice prefix too long"))?;
                let n_suffix = i64::try_from(suffix.len())
                    .map_err(|_| RuntimeError::Unsupported("slice suffix too long"))?;
                // Length guard: `len >= n_prefix + n_suffix` with a `..`,
                // `len == n_prefix` for a fixed-length slice pattern.
                let bound = if rest.is_some() {
                    n_prefix + n_suffix
                } else {
                    n_prefix
                };
                let bound_reg = self.load_int_value(bound);
                let test = self.alloc_reg();
                if rest.is_some() {
                    self.emit(Op::Ge {
                        dst: test,
                        lhs: len_reg,
                        rhs: bound_reg,
                    });
                } else {
                    self.emit(Op::Eq {
                        dst: test,
                        lhs: len_reg,
                        rhs: bound_reg,
                    });
                }
                fails.push(self.emit(Op::BranchIfNot {
                    cond: test,
                    target: 0,
                }));
                // Prefix elements: `scrut[i]`.
                for (i, sub) in prefix.iter().enumerate() {
                    let idx_reg = self.load_int_value(i as i64);
                    let elem = self.alloc_reg();
                    self.emit(Op::IndexGet {
                        dst: elem,
                        base: scrut,
                        index: idx_reg,
                    });
                    self.emit_pattern_test(elem, sub, fails)?;
                }
                // Suffix elements: `scrut[len - n_suffix + j]`.
                for (j, sub) in suffix.iter().enumerate() {
                    let off_reg = self.load_int_value(j as i64 - n_suffix);
                    let idx_reg = self.alloc_reg();
                    let add_cache = self.next_arith_cache();
                    self.emit(Op::AddInt {
                        dst: idx_reg,
                        lhs: len_reg,
                        rhs: off_reg,
                        cache_idx: add_cache,
                    });
                    let elem = self.alloc_reg();
                    self.emit(Op::IndexGet {
                        dst: elem,
                        base: scrut,
                        index: idx_reg,
                    });
                    self.emit_pattern_test(elem, sub, fails)?;
                }
                // `..rest` binding: `scrut.slice(n_prefix, len - n_suffix)`
                // yields `Ok(sub)`; extract the payload and bind it.
                if let Some(rest) = rest {
                    if let HirPatKind::Binding { name, .. } = &rest.kind {
                        let lo_reg = self.load_int_value(n_prefix);
                        let neg_suffix = self.load_int_value(-n_suffix);
                        let hi_reg = self.alloc_reg();
                        let hi_cache = self.next_arith_cache();
                        self.emit(Op::AddInt {
                            dst: hi_reg,
                            lhs: len_reg,
                            rhs: neg_suffix,
                            cache_idx: hi_cache,
                        });
                        let args_start = self.next_reg;
                        self.next_reg = self
                            .next_reg
                            .checked_add(2)
                            .expect("register overflow reserving slice args");
                        self.emit(Op::Move {
                            dst: args_start,
                            src: lo_reg,
                        });
                        self.emit(Op::Move {
                            dst: args_start + 1,
                            src: hi_reg,
                        });
                        let slice_res = self.alloc_reg();
                        let slice_name = self.global_idx("slice");
                        let slice_cache = self.alloc_cache_idx();
                        self.emit(Op::MethodCall {
                            dst: slice_res,
                            receiver: scrut,
                            name_idx: slice_name,
                            args: args_start,
                            argc: 2,
                            cache_idx: slice_cache,
                        });
                        let sub = self.alloc_reg();
                        self.emit(Op::VariantField {
                            dst: sub,
                            src: slice_res,
                            idx: 0,
                        });
                        self.bind_local(
                            &name.name,
                            TypedReg {
                                reg: sub,
                                kind: RegKind::Value,
                            },
                        );
                    }
                }
            }
            HirPatKind::Or(alts) if !pattern_has_binding(pat) => {
                // No alternative binds, so each alt is a pure test.
                // Emit them as a short-circuit OR: the first alt that
                // matches jumps past the rest to the shared
                // continuation; if every alt fails, fall through to
                // the arm-fail branch.
                let mut matched: Vec<InstrIdx> = Vec::new();
                for alt in alts {
                    let mut alt_fails: Vec<InstrIdx> = Vec::new();
                    self.emit_pattern_test(scrut, alt, &mut alt_fails)?;
                    // This alt matched - jump to the continuation.
                    matched.push(self.emit(Op::Jump { target: 0 }));
                    // This alt failed - next alt starts here.
                    let next_alt = self.cur_idx();
                    for f in alt_fails {
                        self.patch_jump(f, next_alt);
                    }
                }
                // All alternatives failed: jump to the arm-fail target.
                fails.push(self.emit(Op::Jump { target: 0 }));
                // Matched continuation.
                let cont = self.cur_idx();
                for m in matched {
                    self.patch_jump(m, cont);
                }
            }
            HirPatKind::Or(alts) => {
                // Binding or-pattern: every alternative binds the same
                // set of names (a typecheck invariant). One shared
                // register per name is the single home the arm body
                // reads, regardless of which alternative won - so each
                // alternative copies its freshly-extracted bindings
                // into those shared registers on its match path before
                // jumping to the continuation. Mirrors the MIR lowering
                // (`gossamer-mir/.../ctrl.rs`), which writes every
                // alternative's bindings into common slots.
                let mut names: Vec<String> = Vec::new();
                collect_pattern_binding_names(pat, &mut names);
                let shared: Vec<(String, Reg)> = names
                    .into_iter()
                    .map(|name| (name, self.alloc_reg()))
                    .collect();

                let mut matched: Vec<InstrIdx> = Vec::new();
                for alt in alts {
                    debug_assert!(
                        {
                            let mut alt_names = Vec::new();
                            collect_pattern_binding_names(alt, &mut alt_names);
                            alt_names.len() == shared.len()
                                && alt_names.iter().all(|n| shared.iter().any(|(s, _)| s == n))
                        },
                        "or-pattern alternatives must bind the same set of names"
                    );
                    let mut alt_fails: Vec<InstrIdx> = Vec::new();
                    // Compile the alternative in an inner scope so its
                    // leaf bindings land in fresh registers that this
                    // scope owns; relocate each into its shared home,
                    // then discard the scope.
                    self.push_scope();
                    self.emit_pattern_test(scrut, alt, &mut alt_fails)?;
                    for (name, dst) in &shared {
                        if let Some(src) = self.lookup_local(name) {
                            let src_v = self.as_value(src);
                            self.emit(Op::Move {
                                dst: *dst,
                                src: src_v,
                            });
                        }
                    }
                    self.pop_scope();
                    // This alt matched - jump to the continuation.
                    matched.push(self.emit(Op::Jump { target: 0 }));
                    // This alt failed - next alt starts here.
                    let next_alt = self.cur_idx();
                    for f in alt_fails {
                        self.patch_jump(f, next_alt);
                    }
                }
                // All alternatives failed: jump to the arm-fail target.
                fails.push(self.emit(Op::Jump { target: 0 }));
                // Matched continuation: expose each shared register to
                // the arm body (and any guard) under its bound name.
                let cont = self.cur_idx();
                for m in matched {
                    self.patch_jump(m, cont);
                }
                for (name, reg) in &shared {
                    self.bind_local(
                        name,
                        TypedReg {
                            reg: *reg,
                            kind: RegKind::Value,
                        },
                    );
                }
            }
        }
        Ok(())
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
        if matches!(op, HirBinaryOp::And | HirBinaryOp::Or) {
            let reg = self.compile_short_circuit(op, lhs, rhs)?;
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
            let overflow_ty = [lhs.ty, rhs.ty].into_iter().find_map(|ty| {
                match self.tcx.kind(self.unwrap_ref(ty)) {
                    Some(TyKind::Int(int_ty)) => Some(*int_ty),
                    _ => None,
                }
            });
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
        if matches!(
            op,
            HirBinaryOp::Lt | HirBinaryOp::Le | HirBinaryOp::Gt | HirBinaryOp::Ge
        ) {
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
    fn compile_struct_binop(
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
        match (op, kind) {
            (HirUnaryOp::Neg, RegKind::F64) => {
                let tr = self.compile_expr_ex(operand)?;
                let src_f = self.as_f64(tr);
                let dst = self.alloc_float();
                self.emit(Op::NegF64 { dst_f: dst, src_f });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::F64,
                })
            }
            (HirUnaryOp::Neg, RegKind::I64) => {
                let tr = self.compile_expr_ex(operand)?;
                let src_i = self.as_i64(tr);
                let dst = self.alloc_int();
                self.emit(Op::NegI64 { dst_i: dst, src_i });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                })
            }
            _ => {
                let reg = self.compile_unary(op, operand)?;
                Ok(TypedReg {
                    reg,
                    kind: RegKind::Value,
                })
            }
        }
    }

    pub(crate) fn compile_literal_ex(
        &mut self,
        lit: &HirLiteral,
        _ty: Ty,
    ) -> RuntimeResult<TypedReg> {
        match lit {
            HirLiteral::Float(text) => {
                let value = strip_float_suffix(text).parse::<f64>().unwrap_or(0.0);
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
                self.emit(Op::VecPush {
                    receiver: target_reg,
                    value,
                });
                Ok(true)
            }
            ("insert", 2) => {
                let index = self.compile_expr(&args[0])?;
                let value = self.compile_expr(&args[1])?;
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
        if !matches!(name.name.as_str(), "wrapping_add" | "wrapping_mul") || args.len() != 1 {
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
        if name.name == "wrapping_add"
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
                kind: if name.name == "wrapping_add" {
                    ImmArithKind::Add
                } else {
                    ImmArithKind::Mul
                },
                dst_i,
                lhs_i,
                imm,
            });
        } else {
            let rhs_tr = self.compile_expr_ex(&args[0])?;
            let rhs_i = self.as_i64(rhs_tr);
            if name.name == "wrapping_add" {
                self.emit(Op::AddI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                });
            } else {
                self.emit(Op::MulI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                });
            }
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

    pub(crate) fn compile_method_call(
        &mut self,
        receiver: &HirExpr,
        name: &Ident,
        args: &[HirExpr],
        owner: Option<&Ident>,
    ) -> RuntimeResult<Reg> {
        if name.name == "downgrade" && args.is_empty() {
            return self.compile_downgrade(receiver);
        }
        // Keep explicit wrapping arithmetic on the unboxed integer register
        // path. Routing these methods through generic dynamic dispatch makes
        // an intentional opt-out from debug overflow checks substantially
        // slower than the original arithmetic operation.
        if let Some(result) = self.try_compile_i64_wrapping_method(receiver, name, args)? {
            return Ok(self.as_value(result));
        }
        // A `&mut self` user method on a writeback place rides the cell
        // protocol so its mutation of `self` reaches the caller's
        // binding - the mechanism `for x in <custom iterator>` and every
        // stateful `obj.advance()` depend on. Tried first so a user
        // struct whose `&mut self` method shadows a builtin name (`pop`,
        // `swap`, `insert`) routes to the user method.
        if let Some(reg) = self.try_compile_mut_self_method(receiver, name, args)? {
            return Ok(reg);
        }
        // `xs.join(sep)` renders each element the way `{}` does, so an
        // element type that supplies its own rendering answers through that
        // method rather than the synthesized shape.
        if name.name == "join"
            && args.len() == 1
            && let Some(reg) = self.try_compile_rendered_join(receiver, &args[0])?
        {
            return Ok(reg);
        }
        // Vec::insert is fallible and returns a Result independently from the
        // updated receiver. Keep those two values separate so an `Ok` or
        // `Err` can never replace the Vec binding in expression position.
        if name.name == "insert" && args.len() == 2 {
            let mut kind = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = kind {
                kind = self.tcx.kind(inner).cloned();
            }
            if matches!(kind, Some(TyKind::Vec(_))) {
                let receiver_reg = self.compile_expr(receiver)?;
                let index = self.compile_expr(&args[0])?;
                let value = self.compile_expr(&args[1])?;
                let dst = self.alloc_reg();
                self.emit(Op::VecInsert {
                    dst,
                    receiver: receiver_reg,
                    index,
                    value,
                });
                self.compile_place_store(receiver, receiver_reg)?;
                return Ok(dst);
            }
        }
        // Vec::remove is fallible and returns the removed value independently
        // from the updated receiver.
        if name.name == "remove" && args.len() == 1 {
            let mut kind = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = kind {
                kind = self.tcx.kind(inner).cloned();
            }
            if matches!(kind, Some(TyKind::Vec(_))) {
                let receiver_reg = self.compile_expr(receiver)?;
                let index = self.compile_expr(&args[0])?;
                let dst = self.alloc_reg();
                self.emit(Op::VecRemoveAt {
                    dst,
                    receiver: receiver_reg,
                    index,
                });
                self.compile_place_store(receiver, receiver_reg)?;
                return Ok(dst);
            }
        }
        // `s.byte_at(i)` on a statically-`String` receiver: emit the
        // dedicated `Op::StrByteAt` rather than routing through the
        // generic `MethodCall` machinery. The static-type guard means a
        // non-string receiver (e.g. a user type with its own `byte_at`)
        // never reaches this op and keeps the name-global dispatch.
        if name.name.as_str() == "byte_at" && args.len() == 1 {
            let mut k = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = k {
                k = self.tcx.kind(inner).cloned();
            }
            if matches!(k, Some(TyKind::String)) {
                let recv_reg = self.compile_expr(receiver)?;
                let idx_reg = self.compile_expr(&args[0])?;
                let dst = self.alloc_reg();
                self.emit(Op::StrByteAt {
                    dst,
                    recv: recv_reg,
                    idx: idx_reg,
                });
                return Ok(dst);
            }
        }
        // Character pushes on a local String mutate its SmolStr directly.
        // SmolStr uses copy-on-write for shared heap strings, preserving value
        // semantics while reusing unique storage whenever possible.
        if matches!(name.name.as_str(), "push" | "push_char" | "push_byte") && args.len() == 1 {
            let mut k = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = k {
                k = self.tcx.kind(inner).cloned();
            }
            if matches!(k, Some(TyKind::String))
                && let HirExprKind::Path { segments, .. } = &receiver.kind
                && let [seg] = segments.as_slice()
                && let Some(target) = self.lookup_local(&seg.name)
                && target.kind == RegKind::Value
            {
                let value = self.compile_expr(&args[0])?;
                self.emit(Op::StrPush {
                    receiver: target.reg,
                    value,
                    byte: name.name.as_str() == "push_byte",
                });
                return Ok(self.load_unit());
            }
        }
        // `s.push_str(x)` on a local String can use the same in-place append
        // op as `s += x`. The generic mutating-method route clones the
        // receiver into the builtin argument list and then writes the returned
        // String back, which preserves semantics but turns builder-style loops
        // into repeated whole-string copies.
        if name.name.as_str() == "push_str" && args.len() == 1 {
            let mut k = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = k {
                k = self.tcx.kind(inner).cloned();
            }
            if matches!(k, Some(TyKind::String)) {
                if let HirExprKind::Path { segments, .. } = &receiver.kind {
                    if let [seg] = segments.as_slice() {
                        if let Some(target) = self.lookup_local(&seg.name) {
                            if target.kind == RegKind::Value {
                                let suffix = self.compile_expr(&args[0])?;
                                self.emit(Op::StrAppend {
                                    receiver: target.reg,
                                    value: suffix,
                                });
                                return Ok(self.load_unit());
                            }
                        }
                    }
                }
            }
        }
        // `d.as_millis()` / `d.as_secs()` / `d.as_micros()` - method form
        // of the `time::Duration` accessors. A Duration value is a bare
        // `Value::Int` at runtime with no qualified-key receiver, so the
        // generic `MethodCall` dispatch cannot reach the accessor by name.
        // Resolve it statically from the receiver's Duration type and emit
        // a direct call to the `time::Duration::<accessor>` global.
        if matches!(name.name.as_str(), "as_millis" | "as_secs" | "as_micros") && args.is_empty() {
            let mut k = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = k {
                k = self.tcx.kind(inner).cloned();
            }
            // A `flag::Set` duration cell (`fs.duration(...)`) carries no
            // Duration tag on its HIR type (an unresolved inference var),
            // so dispatch on the compile-time `duration_cell_locals` tag.
            // The cell is a `__Cell` handle at runtime; auto-derefing it at
            // the call boundary yields its backing `Value::Int`-of-ms, so
            // the accessor receives the same shape as a bare Duration.
            let is_duration_cell = self.receiver_is_duration_cell(receiver);
            if matches!(k, Some(TyKind::Duration)) || is_duration_cell {
                let qual = format!("time::Duration::{}", name.name);
                let idx = self.global_idx(&qual);
                let callee_reg = self.alloc_reg();
                self.emit(Op::LoadGlobal {
                    dst: callee_reg,
                    idx,
                });
                let args_start = self.next_reg;
                self.next_reg = self
                    .next_reg
                    .checked_add(1)
                    .expect("register overflow reserving duration accessor arg");
                let recv_reg = self.compile_expr(receiver)?;
                self.emit(Op::Move {
                    dst: args_start,
                    src: recv_reg,
                });
                let dst = self.alloc_reg();
                let cache_idx = self.alloc_cache_idx();
                self.emit(Op::Call {
                    dst,
                    callee: callee_reg,
                    args: args_start,
                    argc: 1,
                    cache_idx,
                    may_have_cells: is_duration_cell,
                });
                return Ok(dst);
            }
        }
        // `inst.elapsed_ms()` - method form of the `time::Instant`
        // accessor. An Instant value is a bare `Value::Int` of monotonic
        // ms at runtime with no qualified-key receiver, so resolve it
        // statically from the receiver's Instant type and emit a direct
        // call to the `time::Instant::elapsed_ms` global.
        if name.name.as_str() == "elapsed_ms" && args.is_empty() {
            let mut k = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = k {
                k = self.tcx.kind(inner).cloned();
            }
            if matches!(k, Some(TyKind::Instant)) {
                let idx = self.global_idx("time::Instant::elapsed_ms");
                let callee_reg = self.alloc_reg();
                self.emit(Op::LoadGlobal {
                    dst: callee_reg,
                    idx,
                });
                let args_start = self.next_reg;
                self.next_reg = self
                    .next_reg
                    .checked_add(1)
                    .expect("register overflow reserving instant accessor arg");
                let recv_reg = self.compile_expr(receiver)?;
                self.emit(Op::Move {
                    dst: args_start,
                    src: recv_reg,
                });
                let dst = self.alloc_reg();
                let cache_idx = self.alloc_cache_idx();
                self.emit(Op::Call {
                    dst,
                    callee: callee_reg,
                    args: args_start,
                    argc: 1,
                    cache_idx,
                    may_have_cells: false,
                });
                return Ok(dst);
            }
        }
        // Super-instruction fast path for the canonical
        // `m.insert(k, m.get_or(k, 0) + by)` counter-bump.
        // Detected here (before compiling args) so the inner
        // `get_or` call is never lowered.
        if name.name == "insert" && args.len() == 2 {
            if let Some((key_expr, by_expr)) = match_map_inc_pattern(receiver, &args[0], &args[1]) {
                // `StrIntMap` has no typed counter-bump op; let it fall
                // through to the generic `get_or` + `insert` builtins,
                // which dispatch on its storage. The `Op::MapInc` /
                // `Op::IntMapInc` super-instructions only cover the boxed
                // `Map` and the `IntMap`.
                if matches!(self.tcx.kind(receiver.ty), Some(TyKind::HashMap { .. }))
                    && !self.is_str_int_map_ty(receiver.ty)
                {
                    // Typed `HashMap<i64, i64>` route: use
                    // `Op::IntMapInc` so the key + delta stay in
                    // the i64 register file the whole time.
                    if self.is_int_map_ty(receiver.ty) {
                        let map_reg = self.compile_expr(receiver)?;
                        let key_tr = self.compile_expr_ex(key_expr)?;
                        let key_i = self.as_i64(key_tr);
                        let by_tr = self.compile_expr_ex(by_expr)?;
                        let by_i = self.as_i64(by_tr);
                        let dst_i = self.alloc_int();
                        self.emit(Op::IntMapInc {
                            dst_i,
                            map_reg,
                            key_i,
                            by_i,
                        });
                        // Caller wants a `Value` register; box the
                        // post-increment value back so the existing
                        // statement-context code keeps working.
                        let dst = self.alloc_reg();
                        self.emit(Op::BoxI64 {
                            dst_v: dst,
                            src_i: dst_i,
                        });
                        return Ok(dst);
                    }
                    let map_reg = self.compile_expr(receiver)?;
                    let key_reg = self.compile_expr(key_expr)?;
                    let by_reg = self.compile_expr(by_expr)?;
                    let dst = self.alloc_reg();
                    self.emit(Op::MapInc {
                        dst,
                        map_reg,
                        key_reg,
                        by_reg,
                    });
                    return Ok(dst);
                }
            }
        }
        // `m.inc_at(seq, start, len, by)` super-instruction for a
        // string-keyed integer-valued `HashMap`. Inlines the
        // slice-hash + entry-increment so a sliding-window
        // counter update doesn't pay the generic builtin-call
        // dispatch on each iteration.
        if name.name == "inc_at"
            && args.len() == 4
            && matches!(self.tcx.kind(receiver.ty), Some(TyKind::HashMap { .. }))
            && !self.is_str_int_map_ty(receiver.ty)
        {
            let map_reg = self.compile_expr(receiver)?;
            let seq_reg = self.compile_expr(&args[0])?;
            let start_reg = self.compile_expr(&args[1])?;
            let len_reg = self.compile_expr(&args[2])?;
            let by_reg = self.compile_expr(&args[3])?;
            let dst = self.alloc_reg();
            let wide_idx = u16::try_from(self.wide_ops.len()).expect("wide_ops index overflow");
            self.wide_ops.push(crate::bytecode::WideOp::MapIncAt {
                dst,
                map_reg,
                seq_reg,
                start_reg,
                len_reg,
                by_reg,
            });
            self.emit(Op::Wide { idx: wide_idx });
            return Ok(dst);
        }
        if name.name == "swap" && args.len() == 2 {
            let receiver_reg = self.compile_expr(receiver)?;
            let a = self.compile_expr(&args[0])?;
            let b = self.compile_expr(&args[1])?;
            let dst = self.alloc_reg();
            self.emit(Op::VecSwap {
                dst,
                receiver: receiver_reg,
                a,
                b,
            });
            self.compile_place_store(receiver, receiver_reg)?;
            return Ok(dst);
        }
        // Typed-IntMap method dispatch fast paths. Skip the
        // generic builtin-IC route for the handful of HashMap
        // methods that hot counter loops drive.
        if self.is_int_map_ty(receiver.ty) {
            if let Some(reg) = self.try_compile_int_map_method(receiver, &name.name, args)? {
                return Ok(reg);
            }
        }
        let receiver_reg = self.compile_expr(receiver)?;
        // A rendering method answers the text `{}` answers, and a `Vec`
        // and a fixed array share one runtime representation; the
        // descriptor built from the static type is what tells them
        // apart, so it travels with the renderer's copy here as it does
        // with a format argument.
        // A user `impl` of the channel answers with the receiver itself, so
        // the descriptor - which the built-in renderer reads and a written
        // body cannot - is not put in its way.
        let user_answers_channel = self.has_user_rendering(receiver.ty, &name.name);
        let receiver_desc = if user_answers_channel {
            None
        } else {
            self.render_receiver_desc(receiver.ty, &name.name, args.len())
        };
        let receiver_reg = match receiver_desc {
            Some(desc) => {
                let dst = self.alloc_reg();
                let desc_idx = self.const_idx(
                    ConstKey::String(desc.clone()),
                    Value::String(desc.as_str().into()),
                );
                self.emit(Op::UintLeaves {
                    dst,
                    src: receiver_reg,
                    desc_idx,
                });
                dst
            }
            None => receiver_reg,
        };
        // `xs.pop()` evaluates to `Option<last>` while shortening the
        // receiver. `Op::VecPop` does both in one in-place step: it
        // returns `Some(last)` / `None` and shrinks the receiver
        // register's backing storage without copying it. A bare-local
        // receiver's register is the local's own slot, so the mutation
        // persists; a temporary receiver's mutation is discarded, which
        // matches the compiled tiers (pop on a temporary is a no-op).
        // Unresolved receiver types (`Var` / missing) still take this
        // path: the dominant producer is a stdlib call like
        // `os::read_file` whose Vec result type the checker leaves open;
        // user receivers with their own `pop` are `Adt`-typed and excluded.
        if name.name == "pop"
            && args.is_empty()
            && matches!(
                self.tcx.kind(receiver.ty),
                None | Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Var(_))
            )
        {
            let opt_dst = self.alloc_reg();
            self.emit(Op::VecPop {
                dst: opt_dst,
                receiver: receiver_reg,
            });
            // A field or element receiver (`self.idle.pop()`,
            // `groups[i].pop()`) has its own storage, so the shortened
            // vector is spliced back through the place-store protocol -
            // the same contract `remove` / `insert` / `swap` follow, and
            // what the compiled tiers do by mutating in place.
            if !matches!(receiver.kind, HirExprKind::Path { .. }) {
                self.compile_place_store(receiver, receiver_reg)?;
            }
            return Ok(opt_dst);
        }
        // Super-instruction fast path for `<stream>.write_byte(<b>)`.
        // The runtime handler in `vm.rs::Op::StreamWriteByte`
        // verifies the receiver is a Stream and the byte is an
        // integer; if not, it falls through to a normal MethodCall
        // dispatch. Skipping the args-buf + IC + builtin-extract
        // chain saves the dominant per-character overhead in
        // fasta's hot output loop. Mirrors CPython 3.11's
        // `CALL_NO_KW_BUILTIN_O` specialisation.
        if name.name == "write_byte" && args.len() == 1 {
            // Use the typed compile path so a typed-i64 result (from
            // e.g. `Op::IntArrayGetI64`) can flow through an
            // explicit `BoxI64` rather than being re-fetched as a
            // boxed `Value::Int`. The handler still expects a
            // `Value` register, but `BoxI64` is a single op.
            let byte_tr = self.compile_expr_ex(&args[0])?;
            let byte_reg = self.as_value(byte_tr);
            let dst = self.alloc_reg();
            self.emit(Op::StreamWriteByte {
                dst,
                stream_reg: receiver_reg,
                byte_reg,
            });
            return Ok(dst);
        }
        // Mirror super-instruction for `<u8vec>.set_byte(<idx>, <byte>)`.
        // fasta's per-byte buffer fill drives this op millions of
        // times per phase; the inline handler skips the
        // MethodCall + IC + `&[Value]` round-trip.
        if name.name == "set_byte" && args.len() == 2 {
            let idx_tr = self.compile_expr_ex(&args[0])?;
            let idx_reg = self.as_value(idx_tr);
            let byte_tr = self.compile_expr_ex(&args[1])?;
            let byte_reg = self.as_value(byte_tr);
            let dst = self.alloc_reg();
            self.emit(Op::U8VecSetByte {
                dst,
                u8vec_reg: receiver_reg,
                idx_reg,
                byte_reg,
            });
            return Ok(dst);
        }
        // Mirror super-instruction for `<u8vec>.get_byte(<idx>) -> i64`.
        // The handler writes into a typed `i64` register, so a
        // downstream `Op::Add` etc. picks the result up without an
        // intermediate `Value::Int` round-trip. Caller still
        // expects a `Value` register, so we box back through
        // `Op::BoxI64` - the register allocator and downstream
        // typed-arith specialisation usually elide that pair.
        if name.name == "get_byte" && args.len() == 1 {
            let idx_tr = self.compile_expr_ex(&args[0])?;
            let idx_reg = self.as_value(idx_tr);
            let dst_i = self.alloc_int();
            self.emit(Op::U8VecGetByte {
                dst_i,
                u8vec_reg: receiver_reg,
                idx_reg,
            });
            let dst = self.alloc_reg();
            self.emit(Op::BoxI64 {
                dst_v: dst,
                src_i: dst_i,
            });
            return Ok(dst);
        }
        // Mirror super-instruction for `<str>.substring(<start>, <end>)`.
        // The sliding-window k-mer counter calls this once per position;
        // the inline handler skips the MethodCall + IC + receiver clone +
        // `&[Value]` round-trip. Non-string receivers fall back at runtime.
        if name.name == "substring" && args.len() == 2 {
            let start_reg = self.compile_expr(&args[0])?;
            let end_reg = self.compile_expr(&args[1])?;
            let dst = self.alloc_reg();
            self.emit(Op::StrSubstring {
                dst,
                recv_reg: receiver_reg,
                start_reg,
                end_reg,
            });
            return Ok(dst);
        }
        // Fused `m.inc(key[, by])` counter increment for a HashMap
        // receiver. The sliding-window counter calls this once per
        // k-mer; the inline handler acquires the map lock once and
        // skips the MethodCall + IC + map-handle clone round-trip.
        if name.name == "inc"
            && (args.len() == 1 || args.len() == 2)
            && matches!(self.tcx.kind(receiver.ty), Some(TyKind::HashMap { .. }))
        {
            let key_reg = self.compile_expr(&args[0])?;
            let by_reg = if args.len() == 2 {
                self.compile_expr(&args[1])?
            } else {
                self.load_int_value(1)
            };
            let dst = self.alloc_reg();
            self.emit(Op::MapIncMethod {
                dst,
                map_reg: receiver_reg,
                key_reg,
                by_reg,
            });
            return Ok(dst);
        }
        // `s.push_utf8(buf, start, end)` mutates the receiver AND answers a
        // bool, so the receiver crosses as a write-back cell: the replacement
        // protocol below has only the return value to thread back, and here
        // that value is the flag rather than the new string.
        if name.name.as_str() == "push_utf8"
            && args.len() == 3
            && {
                let mut peeled = receiver.ty;
                while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
                    peeled = *inner;
                }
                matches!(self.tcx.kind(peeled), Some(TyKind::String))
            }
            && self.place_root_is_local(receiver)
        {
            let cell = self.alloc_reg();
            self.emit(Op::CellNew {
                dst: cell,
                src: receiver_reg,
            });
            let cell_args_start = self.next_reg;
            self.next_reg = self
                .next_reg
                .checked_add(3)
                .expect("register overflow reserving push_utf8 args");
            for (i, arg) in args.iter().enumerate() {
                let a = self.compile_expr(arg)?;
                let slot = cell_args_start
                    .checked_add(u16::try_from(i).expect("argc overflow"))
                    .expect("reg overflow");
                self.ensure_reg_slot(slot);
                self.emit(Op::Move { dst: slot, src: a });
            }
            let name_idx = self.global_idx("String::push_utf8");
            let dst = self.alloc_reg();
            let cache_idx = self.alloc_cache_idx();
            self.emit(Op::MethodCall {
                dst,
                receiver: cell,
                name_idx,
                args: cell_args_start,
                argc: 3,
                cache_idx,
            });
            let updated = self.alloc_reg();
            self.emit(Op::CellTake { dst: updated, cell });
            self.compile_place_store(receiver, updated)?;
            return Ok(dst);
        }
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(u16::try_from(args.len()).map_err(|_| RuntimeError::Arity {
                expected: u16::MAX as usize,
                found: args.len(),
            })?)
            .expect("register overflow reserving method args");
        let mut cell_takes: Vec<(Reg, Reg)> = Vec::new();
        let mut place_takes: Vec<(&HirExpr, Reg)> = Vec::new();
        let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            if let Some(home) = self.mut_ref_arg_home(arg, None) {
                let cell = self.alloc_reg();
                if Self::mut_ref_place_name(arg)
                    .is_some_and(|name| Self::mut_arg_move_safe(args, i, name))
                {
                    self.emit(Op::CellNewMove {
                        dst: cell,
                        src: home,
                    });
                } else {
                    self.emit(Op::CellNew {
                        dst: cell,
                        src: home,
                    });
                }
                cell_takes.push((home, cell));
                arg_regs.push(cell);
            } else if let Some(place) = Self::mut_ref_writeback_place(self.tcx, arg, None) {
                let place_reg = self.compile_expr(place)?;
                let cell = self.alloc_reg();
                let local_home = Self::path_single_seg_name(place).and_then(|name| {
                    self.lookup_local(name)
                        .filter(|tr| tr.kind == RegKind::Value)
                        .map(|_| name)
                });
                if local_home.is_some_and(|name| Self::mut_arg_move_safe(args, i, name)) {
                    self.emit(Op::CellNewMove {
                        dst: cell,
                        src: place_reg,
                    });
                } else {
                    self.emit(Op::CellNew {
                        dst: cell,
                        src: place_reg,
                    });
                }
                if local_home.is_some() {
                    cell_takes.push((place_reg, cell));
                } else {
                    place_takes.push((place, cell));
                }
                arg_regs.push(cell);
            } else {
                arg_regs.push(self.compile_expr(arg)?);
            }
        }
        for (i, r) in arg_regs.iter().enumerate() {
            let slot = args_start
                .checked_add(u16::try_from(i).expect("argc overflow"))
                .expect("reg overflow");
            self.ensure_reg_slot(slot);
            self.emit(Op::Move { dst: slot, src: *r });
        }
        let argc = u16::try_from(args.len()).map_err(|_| RuntimeError::Arity {
            expected: u16::MAX as usize,
            found: args.len(),
        })?;
        // `m.pop(k)` on a HashMap mutates the map in place (it is an
        // `Arc<Mutex<..>>`) and returns `Option<V>`. The name-global `pop`
        // resolves to the Vec pop builtin, and the mutating-writeback below
        // would then overwrite the map binding with that result; route to
        // the qualified map builtin and suppress the writeback instead.
        let mut resolved_receiver_ty = receiver.ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved_receiver_ty) {
            resolved_receiver_ty = *inner;
        }
        let is_map_pop = name.name == "pop"
            && args.len() == 1
            && matches!(
                self.tcx.kind(resolved_receiver_ty),
                Some(TyKind::HashMap { .. })
            );
        // A traversal on a map or set answers eagerly from that container's own
        // surface; the bare name would reach the variant or Vec builtin.
        let traversal_owner = match self.tcx.kind(resolved_receiver_ty) {
            _ if !gossamer_types::is_collection_traversal_method(name.name.as_str()) => None,
            Some(TyKind::Adt { def, .. }) if def.local == HASH_SET_DEF_LOCAL => Some("Set"),
            Some(TyKind::Adt { def, .. }) if def.local == BTREE_SET_DEF_LOCAL => Some("BTreeSet"),
            _ => None,
        };
        let qualified_collection_method = match self.tcx.kind(resolved_receiver_ty) {
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL)
                    && matches!(
                        name.name.as_str(),
                        "insert"
                            | "remove"
                            | "contains"
                            | "len"
                            | "is_empty"
                            | "clear"
                            | "to_vec"
                            | "iter"
                            | "union"
                            | "intersection"
                            | "difference"
                            | "symmetric_difference"
                            | "is_subset"
                            | "is_superset"
                            | "is_disjoint"
                    ) =>
            {
                let owner = if def.local == BTREE_SET_DEF_LOCAL {
                    "BTreeSet"
                } else {
                    "Set"
                };
                Some(format!("{owner}::{}", name.name))
            }
            Some(TyKind::Adt { def, .. })
                if def.local == VEC_DEQUE_DEF_LOCAL
                    && matches!(
                        name.name.as_str(),
                        "push_back"
                            | "push_front"
                            | "pop_back"
                            | "pop_front"
                            | "peek_back"
                            | "peek_front"
                            | "len"
                            | "is_empty"
                            | "clear"
                    ) =>
            {
                Some(format!("Deque::{}", name.name))
            }
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, VEC_QUEUE_DEF_LOCAL | VEC_STACK_DEF_LOCAL)
                    && matches!(
                        name.name.as_str(),
                        "push" | "pop" | "peek" | "len" | "is_empty" | "clear"
                    ) =>
            {
                let owner = if def.local == VEC_STACK_DEF_LOCAL {
                    "Stack"
                } else {
                    "Queue"
                };
                Some(format!("{owner}::{}", name.name))
            }
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, BINARY_HEAP_DEF_LOCAL | MIN_HEAP_DEF_LOCAL)
                    && matches!(
                        name.name.as_str(),
                        "push" | "pop" | "peek" | "len" | "is_empty" | "clear"
                    ) =>
            {
                let owner = if def.local == MIN_HEAP_DEF_LOCAL {
                    "MinHeap"
                } else {
                    "MaxHeap"
                };
                Some(format!("{owner}::{}", name.name))
            }
            _ => None,
        };
        // An `impl` method wins over a builtin of the same name. An enum
        // value carries only its variant name at run time, so the receiver's
        // own type cannot be recovered there; naming the method by its
        // declaring type here is what reaches the user's body.
        let recorded_owner = owner
            .map(|owner| format!("{}::{}", owner.name, name.name))
            .filter(|qualified| self.fn_param_tys.contains_key(qualified));
        let user_impl_method = match self.tcx.kind(resolved_receiver_ty) {
            Some(TyKind::Adt { def, .. }) => self
                .tcx
                .def_name(*def)
                .map(|type_name| format!("{type_name}::{}", name.name))
                .filter(|qualified| self.fn_param_tys.contains_key(qualified)),
            // A non-`Adt` receiver whose type is known reaches its `impl`
            // block through the owner the checker recorded, and a name the
            // type already carries is answered by the type. Guessing an owner
            // from the name here would have an `impl Trait for String`
            // declaring `len` take every `len` call on a string.
            _ if !self.ty_is_unresolved(resolved_receiver_ty) => None,
            // An open receiver - a type parameter, an inference variable -
            // has no recorded owner, because one body serves every
            // instantiation. The name is all there is to bind to.
            _ => self
                .impl_target_names(resolved_receiver_ty)
                .into_iter()
                .map(|type_name| format!("{type_name}::{}", name.name))
                .find(|qualified| self.fn_param_tys.contains_key(qualified))
                // A payload binding extracted from a generic enum keeps an
                // unresolved type, which is exactly the receiver of a
                // recursive method's inner call. One `impl` declaring the
                // name settles it without a type: there is nothing else the
                // call could reach. A receiver whose type *is* known owns its
                // method surface, so the guess is confined to receivers whose
                // type is genuinely open.
                .or_else(|| {
                    self.ty_is_unresolved(resolved_receiver_ty)
                        .then(|| self.sole_impl_method(&name.name))
                        .flatten()
                }),
        };
        let dispatch_name = if is_map_pop {
            "Map::pop"
        } else if matches!(name.name.as_str(), "wrapping_add" | "wrapping_mul") {
            match self.tcx.kind(resolved_receiver_ty) {
                Some(TyKind::Int(int_ty)) => match int_ty {
                    gossamer_types::IntTy::I8 => {
                        if name.name == "wrapping_add" {
                            "i8::wrapping_add"
                        } else {
                            "i8::wrapping_mul"
                        }
                    }
                    gossamer_types::IntTy::I16 => {
                        if name.name == "wrapping_add" {
                            "i16::wrapping_add"
                        } else {
                            "i16::wrapping_mul"
                        }
                    }
                    gossamer_types::IntTy::I32 => {
                        if name.name == "wrapping_add" {
                            "i32::wrapping_add"
                        } else {
                            "i32::wrapping_mul"
                        }
                    }
                    gossamer_types::IntTy::I64 => {
                        if name.name == "wrapping_add" {
                            "i64::wrapping_add"
                        } else {
                            "i64::wrapping_mul"
                        }
                    }
                    gossamer_types::IntTy::Isize => {
                        if name.name == "wrapping_add" {
                            "isize::wrapping_add"
                        } else {
                            "isize::wrapping_mul"
                        }
                    }
                    gossamer_types::IntTy::U8 => {
                        if name.name == "wrapping_add" {
                            "u8::wrapping_add"
                        } else {
                            "u8::wrapping_mul"
                        }
                    }
                    gossamer_types::IntTy::U16 => {
                        if name.name == "wrapping_add" {
                            "u16::wrapping_add"
                        } else {
                            "u16::wrapping_mul"
                        }
                    }
                    gossamer_types::IntTy::U32 => {
                        if name.name == "wrapping_add" {
                            "u32::wrapping_add"
                        } else {
                            "u32::wrapping_mul"
                        }
                    }
                    gossamer_types::IntTy::U64 => {
                        if name.name == "wrapping_add" {
                            "u64::wrapping_add"
                        } else {
                            "u64::wrapping_mul"
                        }
                    }
                    gossamer_types::IntTy::Usize => {
                        if name.name == "wrapping_add" {
                            "usize::wrapping_add"
                        } else {
                            "usize::wrapping_mul"
                        }
                    }
                    gossamer_types::IntTy::I128 | gossamer_types::IntTy::U128 => &name.name,
                },
                _ => &name.name,
            }
        } else {
            match traversal_owner {
                Some(owner) => format!("{owner}::{}", name.name).leak(),
                // The `impl` block the checker resolved this call to answers
                // first: the receiver's type decided it there, while here a
                // container and a structural type are one runtime shape.
                None => recorded_owner
                    .as_deref()
                    .or(qualified_collection_method.as_deref())
                    .or(user_impl_method.as_deref())
                    .unwrap_or(&name.name),
            }
        };
        let name_idx = self.global_idx(dispatch_name);
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        self.emit(Op::MethodCall {
            dst,
            receiver: receiver_reg,
            name_idx,
            args: args_start,
            argc,
            cache_idx,
        });
        for (home, cell) in cell_takes {
            self.emit(Op::CellTake { dst: home, cell });
        }
        for (place, cell) in place_takes {
            let tmp = self.alloc_reg();
            self.emit(Op::CellTake { dst: tmp, cell });
            self.compile_place_store(place, tmp)?;
        }
        // Mutating-method writeback. The builtins for `push` /
        // `insert` / etc. return the *new* aggregate rather than
        // mutating in place, so the VM has to thread the result back
        // into the receiver's storage. A bare local receiver is the
        // common case (one `Op::Move`); an index / field place rooted
        // at a local (`groups[i].push(x)`, `bag.items.push(x)`) splices
        // the result back through the place-store protocol so the
        // mutation persists - matching the compiled tiers, which mutate
        // the backing storage in place.
        let replacement_writeback = match self.tcx.kind(resolved_receiver_ty) {
            Some(TyKind::String | TyKind::Vec(_) | TyKind::Slice(_)) => true,
            Some(TyKind::Array { .. }) => matches!(
                name.name.as_str(),
                "sort" | "sort_by" | "sort_by_key" | "reverse" | "swap" | "fill"
            ),
            _ => false,
        };
        if !is_map_pop && replacement_writeback && Self::is_mutating_method_name(name.name.as_str())
        {
            match &receiver.kind {
                HirExprKind::Path { segments, .. } if segments.len() == 1 => {
                    if let Some(target) = self.lookup_local(&segments[0].name) {
                        if target.kind == RegKind::Value && target.reg == receiver_reg {
                            self.emit(Op::Move {
                                dst: target.reg,
                                src: dst,
                            });
                        }
                    }
                }
                HirExprKind::Index { .. }
                | HirExprKind::Field { .. }
                | HirExprKind::TupleIndex { .. }
                    if self.place_root_is_local(receiver) =>
                {
                    self.compile_place_store(receiver, dst)?;
                }
                // `m.or_insert(k, d).push(v)`: the entry the receiver came
                // from is where the mutation belongs, so the updated
                // aggregate goes back under the same key. The compiled tiers
                // hand back the stored value itself and mutate it in place.
                HirExprKind::MethodCall {
                    receiver: map_expr,
                    name: entry_name,
                    args: entry_args,
                    owner: None,
                } if entry_name.name.as_str() == "or_insert" && entry_args.len() == 2 => {
                    let map_reg = self.compile_expr(map_expr)?;
                    let key_reg = self.compile_expr(&entry_args[0])?;
                    let scratch = self.alloc_reg();
                    self.emit(Op::MapInsert {
                        dst: scratch,
                        map_reg,
                        key_reg,
                        value_reg: dst,
                    });
                    self.compile_place_store(map_expr, map_reg)?;
                }
                _ => {}
            }
        }
        let returns_unit = match self.tcx.kind(resolved_receiver_ty) {
            Some(TyKind::String) => matches!(
                name.name.as_str(),
                "push" | "push_str" | "push_char" | "push_byte" | "clear" | "truncate"
            ),
            Some(TyKind::Vec(_) | TyKind::Slice(_)) => matches!(
                name.name.as_str(),
                "push"
                    | "insert"
                    | "clear"
                    | "extend"
                    | "extend_from_slice"
                    | "truncate"
                    | "sort"
                    | "sort_by"
                    | "sort_by_key"
                    | "reverse"
                    | "retain"
                    | "drain"
                    | "swap"
                    | "fill"
                    | "resize"
                    | "copy_within"
                    | "copy_from_slice"
            ),
            Some(TyKind::Array { .. }) => matches!(
                name.name.as_str(),
                "sort" | "sort_by" | "sort_by_key" | "reverse" | "swap" | "fill"
            ),
            Some(TyKind::HashMap { .. }) => name.name == "clear",
            Some(TyKind::Adt { def, .. }) if def.local == VEC_DEQUE_DEF_LOCAL => {
                matches!(name.name.as_str(), "push_back" | "push_front")
            }
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, VEC_QUEUE_DEF_LOCAL | VEC_STACK_DEF_LOCAL) =>
            {
                matches!(name.name.as_str(), "push" | "clear")
            }
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, BINARY_HEAP_DEF_LOCAL | MIN_HEAP_DEF_LOCAL) =>
            {
                matches!(name.name.as_str(), "push" | "clear")
            }
            _ => false,
        };
        if returns_unit {
            Ok(self.load_unit())
        } else {
            Ok(dst)
        }
    }

    /// Lowers a `&mut self` user-method call (`obj.bump()`,
    /// `(&mut __for_iter).next()`) through the write-back cell protocol
    /// so the method's mutation of `self` persists in the caller's
    /// binding. Returns `Some(result_reg)` when the receiver resolves to
    /// a local-rooted place whose `Type::method` is a known `&mut self`
    /// method; otherwise `None`, leaving the generic dispatch to handle
    /// it (a temporary receiver's mutation is discarded, matching the
    /// compiled tiers). The receiver crosses as a `MutCell`; the callee
    /// unwraps it (its `self` register is a `mut_ref_param`) and
    /// publishes the post-call `self` on return, which `Op::CellTake` +
    /// `compile_place_store` write back into the receiver place.
    fn try_compile_mut_self_method(
        &mut self,
        receiver: &HirExpr,
        name: &Ident,
        args: &[HirExpr],
    ) -> RuntimeResult<Option<Reg>> {
        let place = peel_ref_wrappers_expr(receiver);
        // Direct locals, fields, and indexed elements rooted at locals are
        // writable places. Temporaries remain values and receive no writeback.
        if !self.place_root_is_local(place) {
            return Ok(None);
        }
        let qual = self
            .impl_target_names(place.ty)
            .into_iter()
            .map(|type_name| format!("{type_name}::{}", name.name))
            .find(|qual| self.method_muts.contains(qual))
            .or_else(|| {
                // Name-only fallback, for a receiver whose type is not
                // resolved here at all. A receiver whose type IS known owns
                // its method surface: a `&mut self` method of that name on
                // some other type is not a candidate for it, and binding to
                // one would call a body the receiver's type never declared.
                if !self.ty_is_unresolved(place.ty) {
                    return None;
                }
                let suffix = format!("::{}", name.name);
                let mut matches = self
                    .method_muts
                    .iter()
                    .filter(|qual| qual.ends_with(&suffix));
                let qual = matches.next()?.clone();
                matches.next().is_none().then_some(qual)
            });
        let Some(qual) = qual else {
            return Ok(None);
        };
        let total = args.len() + 1;
        let argc = u16::try_from(total).map_err(|_| RuntimeError::Arity {
            expected: u16::MAX as usize,
            found: total,
        })?;
        // `Type::method` is registered for every user `impl` method, so
        // the global resolves; loading it yields the callee identity the
        // `Op::Call` inline cache keys on.
        let global_idx = self.global_idx(&qual);
        let callee_reg = self.alloc_reg();
        self.emit(Op::LoadGlobal {
            dst: callee_reg,
            idx: global_idx,
        });
        // Reserve the contiguous arg block (receiver cell + declared
        // args) before compiling any operand, so an operand whose
        // compile allocates fresh registers can't clobber the span.
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(argc)
            .expect("register overflow reserving mut-self method args");
        let place_reg = self.compile_expr(place)?;
        // Evaluate every argument BEFORE capturing the receiver, so an argument
        // that reads the receiver (`c.bump(c.value)`) still sees its live value
        // - `CellNewMove` empties the receiver's register immediately after.
        //
        // A `&mut` argument rides the same write-back cell the generic call
        // path gives it: a `&mut self` method may take an out-parameter
        // (`conn.next_row(&mut stream)`), and passing that by value would
        // leave every mutation on a copy.
        let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len());
        let mut arg_cell_takes: Vec<(Reg, Reg)> = Vec::new();
        let mut arg_place_takes: Vec<(&HirExpr, Reg)> = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if let Some(home) = self.mut_ref_arg_home(arg, None) {
                let arg_cell = self.alloc_reg();
                if Self::mut_ref_place_name(arg)
                    .is_some_and(|name| Self::mut_arg_move_safe(args, i, name))
                {
                    self.emit(Op::CellNewMove {
                        dst: arg_cell,
                        src: home,
                    });
                } else {
                    self.emit(Op::CellNew {
                        dst: arg_cell,
                        src: home,
                    });
                }
                arg_cell_takes.push((home, arg_cell));
                arg_regs.push(arg_cell);
            } else if let Some(place) = Self::mut_ref_writeback_place(self.tcx, arg, None) {
                let place_reg = self.compile_expr(place)?;
                let arg_cell = self.alloc_reg();
                let local_home = Self::path_single_seg_name(place).and_then(|name| {
                    self.lookup_local(name)
                        .filter(|tr| tr.kind == RegKind::Value)
                        .map(|_| name)
                });
                if local_home.is_some_and(|name| Self::mut_arg_move_safe(args, i, name)) {
                    self.emit(Op::CellNewMove {
                        dst: arg_cell,
                        src: place_reg,
                    });
                } else {
                    self.emit(Op::CellNew {
                        dst: arg_cell,
                        src: place_reg,
                    });
                }
                if local_home.is_some() {
                    arg_cell_takes.push((place_reg, arg_cell));
                } else {
                    arg_place_takes.push((place, arg_cell));
                }
                arg_regs.push(arg_cell);
            } else {
                arg_regs.push(self.compile_expr(arg)?);
            }
        }
        let cell = self.alloc_reg();
        // Move (not clone) the receiver into the cell. `CellTake` below
        // republishes the post-call `self` into the same place, and moving
        // keeps the receiver's refcount at one so the callee's first field
        // write mutates in place instead of forcing a copy-on-write clone.
        self.emit(Op::CellNewMove {
            dst: cell,
            src: place_reg,
        });
        self.emit(Op::Move {
            dst: args_start,
            src: cell,
        });
        for (i, r) in arg_regs.iter().enumerate() {
            let slot = args_start
                .checked_add(u16::try_from(i + 1).expect("argc overflow"))
                .expect("register overflow");
            self.emit(Op::Move { dst: slot, src: *r });
        }
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        // The receiver `MutCell` passes through `auto_deref_cell`
        // untouched (it only resolves `flag::Cell` handles); a declared
        // arg that is a `flag::Cell` still needs the auto-deref, so gate
        // it on a non-scalar arg being present.
        let may_have_cells = !args.iter().all(|a| {
            matches!(
                self.tcx.kind(a.ty),
                Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char)
            )
        });
        self.emit(Op::Call {
            dst,
            callee: callee_reg,
            args: args_start,
            argc,
            cache_idx,
            may_have_cells,
        });
        let tmp = self.alloc_reg();
        self.emit(Op::CellTake { dst: tmp, cell });
        self.compile_place_store(place, tmp)?;
        for (home, arg_cell) in arg_cell_takes {
            self.emit(Op::CellTake {
                dst: home,
                cell: arg_cell,
            });
        }
        for (arg_place, arg_cell) in arg_place_takes {
            let taken = self.alloc_reg();
            self.emit(Op::CellTake {
                dst: taken,
                cell: arg_cell,
            });
            self.compile_place_store(arg_place, taken)?;
        }
        Ok(Some(dst))
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

    pub(crate) fn compile_call_ex(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
        result_ty: Ty,
    ) -> RuntimeResult<Reg> {
        // Qualified Vec mutators use the same in-place contract as method
        // calls. Compile the referenced place directly so the legacy
        // Result-returning builtins cannot leak through this Rust-style API.
        if let HirExprKind::Path { segments, .. } = &callee.kind
            && segments.len() >= 2
            && let Some(method) = segments.last()
            && let Some(owner) = segments.get(segments.len() - 2)
            && owner.name == "Vec"
            && matches!(
                (method.name.as_str(), args.len()),
                ("insert", 3) | ("remove", 2)
            )
        {
            let place = peel_ref_wrappers_expr(&args[0]);
            let receiver = self.compile_expr(place)?;
            let index = self.compile_expr(&args[1])?;
            if method.name == "insert" {
                let value = self.compile_expr(&args[2])?;
                let dst = self.alloc_reg();
                self.emit(Op::VecInsert {
                    dst,
                    receiver,
                    index,
                    value,
                });
                self.compile_place_store(place, receiver)?;
                return Ok(dst);
            }
            let dst = self.alloc_reg();
            self.emit(Op::VecRemoveAt {
                dst,
                receiver,
                index,
            });
            self.compile_place_store(place, receiver)?;
            return Ok(dst);
        }
        // Rust UFCS syntax is semantically the same call as method syntax.
        // Route supported built-in mutators through the method compiler so
        // its receiver writeback and public return contract are identical.
        if let HirExprKind::Path { segments, .. } = &callee.kind
            && segments.len() >= 2
            && let Some(method) = segments.last()
            && let Some(owner) = segments.get(segments.len() - 2)
            && matches!(owner.name.as_str(), "String" | "Vec")
            && matches!(
                method.name.as_str(),
                "push"
                    | "push_str"
                    | "push_char"
                    | "push_byte"
                    | "clear"
                    | "truncate"
                    | "sort"
                    | "sort_by"
                    | "sort_by_key"
                    | "reverse"
                    | "swap"
                    | "fill"
                    | "insert"
                    | "remove"
            )
            && let Some((receiver, method_args)) = args.split_first()
        {
            return self.compile_method_call(
                peel_ref_wrappers_expr(receiver),
                method,
                method_args,
                None,
            );
        }
        // Qualified `Map` / `Set` mutators carry the same implicit
        // mutable-receiver contract as their method-call form (enforced by
        // the type checker's `check_mutating_qualified_call`), so route
        // them through the method compiler too. Left to the generic
        // by-value argument path below, the receiver would go through
        // `Op::CloneMapLike` - the independent-copy semantics an ordinary
        // function parameter needs - and every mutation would land on a
        // throwaway clone instead of the caller's binding.
        if let HirExprKind::Path { segments, .. } = &callee.kind
            && segments.len() >= 2
            && let Some(method) = segments.last()
            && let Some(owner) = segments.get(segments.len() - 2)
            && matches!(owner.name.as_str(), "Map" | "Set" | "BTreeSet")
            && matches!(
                method.name.as_str(),
                "insert"
                    | "remove"
                    | "clear"
                    | "inc"
                    | "inc_at"
                    | "inc_batch"
                    | "or_insert"
                    | "pop"
            )
            && let Some((receiver, method_args)) = args.split_first()
        {
            return self.compile_method_call(
                peel_ref_wrappers_expr(receiver),
                method,
                method_args,
                None,
            );
        }
        // A payload-less enum constructor is already represented by its
        // immutable global sentinel. Its HIR callee has the enum value type,
        // unlike a zero-argument function returning that enum, whose callee
        // has a function type. Return the loaded sentinel directly and avoid
        // generic call dispatch on every leaf construction.
        if args.is_empty()
            && let Some(TyKind::Adt { def, .. }) = self.tcx.kind(callee.ty)
            && let HirExprKind::Path { segments, .. } = &callee.kind
            && let Some(variant_name) = segments.last()
            && self
                .tcx
                .enum_variant_names(*def)
                .is_some_and(|variants| variants.iter().any(|name| name == &variant_name.name))
        {
            return self.compile_expr(callee);
        }
        if let Some(reg) = self.try_compile_variant_constructor(callee, args)? {
            return Ok(reg);
        }
        if let Some(reg) = self.try_compile_struct2_i64(callee, args, result_ty)? {
            return Ok(reg);
        }
        // Typed-IntMap construction fast path: when the callee is
        // `HashMap::new` and the result type is `HashMap<i64, i64>`,
        // emit a dedicated `Op::BuildIntMap` so the receiver lands
        // as `Value::IntMap` and downstream typed ops fire.
        if args.len() <= 1 {
            if let HirExprKind::Path { segments, .. } = &callee.kind {
                let segs: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
                // `BTreeMap` resolves to `TyKind::HashMap` and shares the map
                // runtime, so an i64-keyed `BTreeMap::new` lands as a typed
                // `IntMap` / `StrIntMap` exactly like `HashMap::new`. IntMap
                // iteration is key-sorted, matching BTreeMap's ordering.
                let is_map_new = args.is_empty()
                    && matches!(
                        segs.as_slice(),
                        ["Map" | "BTreeMap", "new"] | ["collections", "Map" | "BTreeMap", "new"]
                    );
                let is_empty_map_from = args.len() == 1
                    && matches!(self.tcx.kind(args[0].ty), Some(TyKind::Unit))
                    && matches!(
                        segs.as_slice(),
                        ["Map" | "BTreeMap", "from"] | ["collections", "Map" | "BTreeMap", "from"]
                    );
                if (is_map_new || is_empty_map_from) && self.is_int_map_ty(result_ty) {
                    let dst = self.alloc_reg();
                    self.emit(Op::BuildIntMap { dst_v: dst });
                    return Ok(dst);
                }
                if (is_map_new || is_empty_map_from) && self.is_str_int_map_ty(result_ty) {
                    let dst = self.alloc_reg();
                    self.emit(Op::BuildStrIntMap { dst_v: dst });
                    return Ok(dst);
                }
                // `{1: 0, 2: 5}` (an int-keyed, int-valued map literal)
                // desugars to `Map::from([(1, 0), (2, 5)])`, an
                // explicit-entry-list array argument rather than the
                // empty/unit form above. Without this arm the call fell
                // through to the generic `Map::from` builtin, which builds
                // a boxed `Value::Map` - but `is_int_map_ty(result_ty)`
                // still reports this binding as the typed shape, so a later
                // `.insert()` / `.len()` / `.contains_key()` / `.get_or()`
                // emits the dedicated `IntMap*` op, which requires
                // `Value::IntMap` and errors "receiver lost typed
                // invariant" against the mismatched boxed map. Build the
                // typed map directly and unroll the (compile-time-known)
                // entry count into individual typed inserts instead, the
                // same representation `Map::new()` + `.insert()` produces.
                if args.len() == 1
                    && matches!(
                        segs.as_slice(),
                        ["Map" | "BTreeMap", "from"] | ["collections", "Map" | "BTreeMap", "from"]
                    )
                    && let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(entries)) =
                        &args[0].kind
                    && entries.iter().all(
                        |entry| matches!(&entry.kind, HirExprKind::Tuple(pair) if pair.len() == 2),
                    )
                {
                    if self.is_int_map_ty(result_ty) {
                        let dst = self.alloc_reg();
                        self.emit(Op::BuildIntMap { dst_v: dst });
                        for entry in entries {
                            let HirExprKind::Tuple(pair) = &entry.kind else {
                                unreachable!("filtered to 2-element tuples above")
                            };
                            let key_tr = self.compile_expr_ex(&pair[0])?;
                            let key_i = self.as_i64(key_tr);
                            let val_tr = self.compile_expr_ex(&pair[1])?;
                            let value_i = self.as_i64(val_tr);
                            let insert_dst = self.alloc_reg();
                            self.emit(Op::IntMapInsert {
                                dst_v: insert_dst,
                                map_reg: dst,
                                key_i,
                                value_i,
                            });
                        }
                        return Ok(dst);
                    }
                }
            }
        }
        if Self::callee_is_concat(callee)
            && args.len() == 2
            && matches!(self.tcx.kind(args[0].ty), Some(TyKind::String))
            && let HirExprKind::Call {
                callee: pad_callee,
                args: pad_args,
            } = &args[1].kind
            && let HirExprKind::Path {
                segments: pad_segments,
                ..
            } = &pad_callee.kind
            && pad_segments
                .last()
                .is_some_and(|segment| segment.name == "__fmt_pad")
            && pad_args.len() == 4
            && let HirExprKind::Call {
                callee: rendered_callee,
                args: rendered_args,
            } = &pad_args[0].kind
            && Self::callee_is_concat(rendered_callee)
            && rendered_args.len() == 1
            && matches!(self.tcx.kind(rendered_args[0].ty), Some(TyKind::Int(_)))
            && !self.expr_has_uint_display_provenance(&rendered_args[0])
        {
            let prefix = self.compile_expr(&args[0])?;
            let value = self.compile_expr(&rendered_args[0])?;
            let width = self.compile_expr(&pad_args[1])?;
            let fill = self.compile_expr(&pad_args[2])?;
            let align = self.compile_expr(&pad_args[3])?;
            let dst = self.alloc_reg();
            let idx = u16::try_from(self.wide_ops.len()).map_err(|_| {
                RuntimeError::Panic("too many wide bytecode operations".to_string())
            })?;
            self.wide_ops
                .push(crate::bytecode::WideOp::StrConcatPadI64 {
                    dst,
                    prefix,
                    value,
                    width,
                    fill,
                    align,
                });
            self.emit(Op::Wide { idx });
            return Ok(dst);
        }
        if Self::callee_is_concat(callee)
            && args.len() == 2
            && matches!(self.tcx.kind(args[0].ty), Some(TyKind::String))
            && matches!(self.tcx.kind(args[1].ty), Some(TyKind::Int(_)))
            && !self.expr_has_uint_display_provenance(&args[1])
        {
            let prefix = self.compile_expr(&args[0])?;
            let value = self.compile_expr_ex(&args[1])?;
            let value_i = self.as_i64(value);
            let dst = self.alloc_reg();
            self.emit(Op::StrConcatI64 {
                dst,
                prefix,
                value_i,
            });
            return Ok(dst);
        }
        let direct_global_idx = if let HirExprKind::Path { segments, def } = &callee.kind {
            let local = segments.len() == 1 && self.lookup_local(&segments[0].name).is_some();
            let module_const = def.is_some_and(|def| self.module_consts.contains_key(&def));
            if local || module_const {
                None
            } else {
                let stripped = strip_module_relative(segments);
                let name = stripped
                    .iter()
                    .map(|segment| segment.name.as_str())
                    .collect::<Vec<_>>()
                    .join("::");
                Some(self.global_idx(&name))
            }
        } else {
            None
        };
        let callee_reg = if direct_global_idx.is_none() {
            Some(self.compile_expr(callee)?)
        } else {
            None
        };
        let argc = u16::try_from(args.len()).map_err(|_| RuntimeError::Arity {
            expected: u16::MAX as usize,
            found: args.len(),
        })?;
        // Reserve `argc` contiguous Value-register slots for the call's
        // argument vector before compiling any arg expression. Without
        // this, an arg whose `compile_expr` allocates a fresh register
        // (e.g. a literal or call result) lands inside the not-yet-
        // populated args region, and the subsequent `Move dst=slot
        // src=arg_reg` clobbers earlier args before they reach the
        // callee.
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(argc)
            .expect("register overflow reserving call args");
        // `&mut Vec<T>` / `&mut [T]` / `&mut <scalar>` arguments ride the
        // write-back cell protocol: wrap the current value in a cell, pass
        // the cell (the callee unwraps it via `mut_ref_params`), and read
        // the callee's final value back after the call. A `&mut <local
        // Vec>` writes straight back into the local's register
        // (`cell_takes`); a non-local place (`&mut arr[i]`, `&mut
        // obj.field`, `&mut <scalar local>`) takes the cell's inner into a
        // temp and re-stores it through the place (`place_takes`).
        let mut cell_takes: Vec<(Reg, Reg)> = Vec::new();
        let mut place_takes: Vec<(&HirExpr, Reg)> = Vec::new();
        let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len());
        // Renderer calls need unsigned-64 arguments boxed as `Value::Uint` so
        // values above `i64::MAX` render as large positive decimals. This
        // mirrors the compiled tiers' printer choice from declared type and
        // MIR cast provenance.
        let render_call = Self::callee_renders_args(callee);
        // `__debug` is the `{:?}` channel and answers through `impl Debug`;
        // every other rendering callee is `{}` and answers through
        // `impl Display`.
        let render_method = if Self::callee_is_debug(callee) {
            "fmt"
        } else {
            "to_string"
        };
        let callee_param_tys = self.callee_param_tys(callee);
        for (i, arg) in args.iter().enumerate() {
            let expected_ty = callee_param_tys
                .as_ref()
                .and_then(|params| params.get(i))
                .copied();
            if let Some(home) = self.mut_ref_arg_home(arg, expected_ty) {
                // `&mut <local Vec>`: move the local into the cell when no
                // sibling argument reads it, giving the callee unique
                // ownership so its first mutation grows in place instead of
                // copy-on-writing the whole buffer. A read elsewhere keeps
                // the clone, matching the compiled tiers' by-pointer
                // snapshot semantics (see the `&mut self` precedent).
                let cell = self.alloc_reg();
                if Self::mut_ref_place_name(arg)
                    .is_some_and(|name| Self::mut_arg_move_safe(args, i, name))
                {
                    self.emit(Op::CellNewMove {
                        dst: cell,
                        src: home,
                    });
                } else {
                    self.emit(Op::CellNew {
                        dst: cell,
                        src: home,
                    });
                }
                cell_takes.push((home, cell));
                arg_regs.push(cell);
            } else if let Some(place) = Self::mut_ref_writeback_place(self.tcx, arg, expected_ty) {
                let place_reg = self.compile_expr(place)?;
                let cell = self.alloc_reg();
                // A bare-local place (`&mut s` for a `String` / scalar /
                // struct local) is the local's own home register: it can be
                // moved into the cell (no-sibling-reads) and written back
                // with a direct `CellTake` into that register. A field /
                // index place keeps the clone (its value was copied out of
                // an aggregate that still holds a share) and re-stores
                // through the place expression.
                let local_home = Self::path_single_seg_name(place).and_then(|name| {
                    self.lookup_local(name)
                        .filter(|tr| tr.kind == RegKind::Value)
                        .map(|_| name)
                });
                if local_home.is_some_and(|name| Self::mut_arg_move_safe(args, i, name)) {
                    self.emit(Op::CellNewMove {
                        dst: cell,
                        src: place_reg,
                    });
                } else {
                    self.emit(Op::CellNew {
                        dst: cell,
                        src: place_reg,
                    });
                }
                if local_home.is_some() {
                    // `place_reg` is the local's home register; publish the
                    // post-call value straight back into it. The temp +
                    // place-store form would leave a lingering clone in the
                    // temp register, forcing copy-on-write on every later
                    // mutation through a repeatedly-called `&mut <local>`.
                    cell_takes.push((place_reg, cell));
                } else {
                    place_takes.push((place, cell));
                }
                arg_regs.push(cell);
            } else if render_call
                && let Some(reg) = self.compile_user_rendering(arg, render_method)?
            {
                arg_regs.push(reg);
            } else if render_call && self.expr_has_uint_display_provenance(arg) {
                let tr = self.compile_expr_ex(arg)?;
                let src_i = self.as_i64(tr);
                let dst_v = self.alloc_reg();
                self.emit(Op::I64ToUint { dst_v, src_i });
                arg_regs.push(dst_v);
            } else if render_call && let Some(desc) = self.uint_leaves_desc(arg.ty) {
                // An integer the type declared `u64` / `usize` reads as
                // unsigned wherever it sits, exactly as the compiled tiers'
                // element, payload, and slot tags render it.
                let src = self.compile_expr(arg)?;
                let dst = self.alloc_reg();
                let desc_idx = self.const_idx(
                    ConstKey::String(desc.clone()),
                    Value::String(desc.as_str().into()),
                );
                self.emit(Op::UintLeaves { dst, src, desc_idx });
                arg_regs.push(dst);
            } else {
                arg_regs.push(self.compile_expr(arg)?);
            }
        }
        for (i, arg_reg) in arg_regs.iter().enumerate() {
            let slot = args_start
                .checked_add(u16::try_from(i).unwrap())
                .expect("register overflow");
            // Move-on-last-use: when the argument is a consumable local
            // whose home register we are reading directly, hand the value
            // over instead of cloning so the callee receives unique
            // ownership and the caller's input frees as it is consumed.
            let consume = self
                .consumable_path(&args[i])
                .and_then(|name| self.lookup_local(name))
                .is_some_and(|tr| tr.kind == RegKind::Value && tr.reg == *arg_reg);
            if consume {
                self.emit(Op::MoveConsume {
                    dst: slot,
                    src: *arg_reg,
                });
            } else if is_path_expr(&args[i])
                && (self.expr_is_map(&args[i])
                    || self.expr_is_hashset(&args[i])
                    || self.expr_is_slot_container(&args[i])
                    // A struct or tuple carrying a container field shares
                    // that field's storage through a plain register copy, so
                    // the callee's value takes a clone the way a bare
                    // container argument does - which is what the compiled
                    // tiers give an aggregate argument through their
                    // struct-copy retain.
                    || self.expr_is_aggregate_with_container(&args[i]))
                // A callee that only reads the container, and lets nothing
                // derived from it outlive the call, sees the caller's storage:
                // copying it would be unobservable, and its cost is the
                // container's size on every call.
                && !self.callee_only_reads_param(callee, i)
                // A reference is an alias by construction: forwarding an
                // existing `&mut Map` / `&mut Set` parameter must reach the
                // callee as the same container, or the callee's `insert` /
                // `pop` lands on a copy and the caller sees nothing.
                && !matches!(self.tcx.kind(args[i].ty), Some(TyKind::Ref { .. }))
            {
                // A `Map` / `Set` local passed by value must reach the callee
                // as an independent value, not an `Arc<Mutex<_>>` alias the
                // callee could mutate out from under the caller - see
                // `Op::CloneMapLike`.
                self.emit(Op::CloneMapLike {
                    dst: slot,
                    src: *arg_reg,
                });
            } else {
                self.emit(Op::Move {
                    dst: slot,
                    src: *arg_reg,
                });
            }
        }
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        // A `flag::Cell` handle is a `Value::Struct("__Cell")`; a
        // primitive-scalar argument can never be one, so a call whose
        // every argument is scalar-typed needs no per-argument
        // auto-deref check.
        let may_have_cells = !args.iter().all(|a| {
            matches!(
                self.tcx.kind(a.ty),
                Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char)
            )
        });
        if let Some(global_idx) = direct_global_idx {
            self.emit(Op::CallGlobal {
                dst,
                global_idx,
                args: args_start,
                argc,
                cache_idx,
                may_have_cells,
            });
        } else {
            self.emit(Op::Call {
                dst,
                callee: callee_reg.expect("dynamic call has a callee register"),
                args: args_start,
                argc,
                cache_idx,
                may_have_cells,
            });
        }
        for (home, cell) in cell_takes {
            self.emit(Op::CellTake { dst: home, cell });
        }
        for (place, cell) in place_takes {
            let tmp = self.alloc_reg();
            self.emit(Op::CellTake { dst: tmp, cell });
            self.compile_place_store(place, tmp)?;
        }
        Ok(dst)
    }

    /// Lowers common payload enum constructors directly. The generic call
    /// path has to load a constructor sentinel, materialize an argument span,
    /// and rediscover that sentinel at runtime. The variant identity is
    /// already known from HIR, so one typed construction opcode is enough.
    fn try_compile_variant_constructor(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
    ) -> RuntimeResult<Option<Reg>> {
        if !matches!(args.len(), 1 | 2) {
            return Ok(None);
        }
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(callee.ty) else {
            return Ok(None);
        };
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return Ok(None);
        };
        let Some(variant) = segments.last() else {
            return Ok(None);
        };
        if !self
            .tcx
            .enum_variant_names(*def)
            .is_some_and(|names| names.iter().any(|name| name == &variant.name))
        {
            return Ok(None);
        }

        let name_idx = self.const_idx(
            ConstKey::Variant(variant.name.clone()),
            Value::variant(variant.name.as_str(), Vec::new()),
        );
        let first = self.compile_expr(&args[0])?;
        let take_first = matches!(args[0].kind, HirExprKind::Call { .. })
            || self.consumable_path(&args[0]).is_some();
        let dst = self.alloc_reg();
        if args.len() == 1 {
            self.emit(Op::BuildVariant1 {
                dst,
                name_idx,
                field: first,
                take_field: take_first,
            });
        } else {
            let second = self.compile_expr(&args[1])?;
            let take_second = matches!(args[1].kind, HirExprKind::Call { .. })
                || self.consumable_path(&args[1]).is_some();
            self.emit(Op::BuildVariant2 {
                dst,
                name_idx,
                first,
                second,
                take_first,
                take_second,
            });
        }
        Ok(Some(dst))
    }

    /// `true` when `callee` spells the struct's own positional constructor
    /// (`Pair(a, b)`), rather than an associated function reached through
    /// the type (`Pair::new(a, b)`). Both carry the struct's `DefId`, and
    /// only the constructor spelling may be turned into a direct field
    /// packing - consuming the other shape would drop the callee's body.
    fn path_spells_struct_ctor(&self, callee: &HirExpr) -> bool {
        let HirExprKind::Path { segments, def, .. } = &callee.kind else {
            return false;
        };
        let (Some(def), Some(last)) = (def.as_ref(), segments.last()) else {
            return false;
        };
        self.tcx
            .def_name(*def)
            .is_some_and(|name| name.rsplit("::").next() == Some(last.name.as_str()))
    }

    /// Recognises the HIR lowering of `Pair(a, b)` / a two-field positional
    /// struct constructor and keeps both scalar operands in the integer
    /// register file. The generic `__struct` builtin remains the fallback for
    /// named fields, non-integer payloads, and every other arity.
    fn try_compile_struct2_i64(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
        result_ty: Ty,
    ) -> RuntimeResult<Option<Reg>> {
        if let HirExprKind::Path { def: Some(def), .. } = &callee.kind
            && self.path_spells_struct_ctor(callee)
            && args.len() == 2
            && args
                .iter()
                .all(|arg| matches!(self.tcx.kind(arg.ty), Some(TyKind::Int(_))))
            && self.tcx.struct_field_tys(*def).is_some_and(|tys| {
                tys.len() == 2
                    && tys
                        .iter()
                        .all(|ty| matches!(self.tcx.kind(*ty), Some(TyKind::Int(_))))
            })
            && let Some(type_name) = self.tcx.def_name(*def)
            && matches!(self.tcx.kind(result_ty), Some(TyKind::Adt { def: result_def, .. }) if *result_def == *def)
        {
            let first = self.compile_expr_ex(&args[0])?;
            let first_i = self.as_i64(first);
            let second = self.compile_expr_ex(&args[1])?;
            let second_i = self.as_i64(second);
            let dst = self.alloc_reg();
            let type_name = self.shape_name_idx(type_name);
            let field0 = self.shape_name_idx("0");
            let field1 = self.shape_name_idx("1");
            self.emit(Op::Struct2I64 {
                dst,
                type_name,
                field0,
                field1,
                first_i,
                second_i,
            });
            return Ok(Some(dst));
        }
        if let HirExprKind::Path { segments, .. } = &callee.kind
            && let [segment] = segments.as_slice()
            && args.len() == 2
            && args
                .iter()
                .all(|arg| matches!(self.tcx.kind(arg.ty), Some(TyKind::Int(_))))
            && let Some((def, field_names)) = self.layouts.iter().find(|(def, names)| {
                names.len() == 2 && self.tcx.def_name(**def) == Some(segment.name.as_str())
            })
            && self.tcx.struct_field_tys(*def).is_some_and(|tys| {
                tys.len() == 2
                    && tys
                        .iter()
                        .all(|ty| matches!(self.tcx.kind(*ty), Some(TyKind::Int(_))))
            })
            && matches!(self.tcx.kind(result_ty), Some(TyKind::Adt { def: result_def, .. }) if *result_def == *def)
        {
            let type_name = segment.name.clone();
            let field0 = field_names[0].clone();
            let field1 = field_names[1].clone();
            let first = self.compile_expr_ex(&args[0])?;
            let first_i = self.as_i64(first);
            let second = self.compile_expr_ex(&args[1])?;
            let second_i = self.as_i64(second);
            let dst = self.alloc_reg();
            let type_name = self.shape_name_idx(&type_name);
            let field0 = self.shape_name_idx(&field0);
            let field1 = self.shape_name_idx(&field1);
            self.emit(Op::Struct2I64 {
                dst,
                type_name,
                field0,
                field1,
                first_i,
                second_i,
            });
            return Ok(Some(dst));
        }
        if let HirExprKind::Path { def: Some(def), .. } = &callee.kind
            && self.path_spells_struct_ctor(callee)
            && let Some(field_tys) = self.tcx.struct_field_tys(*def)
            && field_tys.len() == 2
            && field_tys
                .iter()
                .all(|ty| matches!(self.tcx.kind(*ty), Some(TyKind::Int(_))))
            && args.len() == 2
            && args
                .iter()
                .all(|arg| matches!(self.tcx.kind(arg.ty), Some(TyKind::Int(_))))
            && let Some(field_names) = self.layouts.get(def).filter(|names| names.len() == 2)
            && let Some(type_name) = self.tcx.def_name(*def)
            && matches!(self.tcx.kind(result_ty), Some(TyKind::Adt { def: result_def, .. }) if result_def == def)
        {
            let field0 = field_names[0].clone();
            let field1 = field_names[1].clone();
            let first = self.compile_expr_ex(&args[0])?;
            let first_i = self.as_i64(first);
            let second = self.compile_expr_ex(&args[1])?;
            let second_i = self.as_i64(second);
            let dst = self.alloc_reg();
            let type_name = self.shape_name_idx(type_name);
            let field0 = self.shape_name_idx(&field0);
            let field1 = self.shape_name_idx(&field1);
            self.emit(Op::Struct2I64 {
                dst,
                type_name,
                field0,
                field1,
                first_i,
                second_i,
            });
            return Ok(Some(dst));
        }
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return Ok(None);
        };
        if !matches!(segments.as_slice(), [segment] if segment.name == "__struct")
            || args.len() != 5
        {
            return Ok(None);
        }
        let (
            HirExprKind::Literal(HirLiteral::String(type_name)),
            HirExprKind::Literal(HirLiteral::String(field0)),
            HirExprKind::Literal(HirLiteral::String(field1)),
        ) = (&args[0].kind, &args[1].kind, &args[3].kind)
        else {
            return Ok(None);
        };
        if !matches!(self.tcx.kind(args[2].ty), Some(TyKind::Int(_)))
            || !matches!(self.tcx.kind(args[4].ty), Some(TyKind::Int(_)))
        {
            return Ok(None);
        }
        let first = self.compile_expr_ex(&args[2])?;
        let first_i = self.as_i64(first);
        let second = self.compile_expr_ex(&args[4])?;
        let second_i = self.as_i64(second);
        let dst = self.alloc_reg();
        let type_name = self.shape_name_idx(type_name);
        let field0 = self.shape_name_idx(field0);
        let field1 = self.shape_name_idx(field1);
        self.emit(Op::Struct2I64 {
            dst,
            type_name,
            field0,
            field1,
            first_i,
            second_i,
        });
        Ok(Some(dst))
    }

    /// True when a call renders its arguments through `Display`.
    fn callee_renders_args(callee: &HirExpr) -> bool {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return false;
        };
        segments.last().is_some_and(|s| {
            matches!(
                s.name.as_str(),
                "__concat" | "__debug" | "println" | "print" | "eprintln" | "format"
            )
        })
    }

    /// Whether `callee` is the `{:?}` rendering channel.
    fn callee_is_debug(callee: &HirExpr) -> bool {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return false;
        };
        segments.last().is_some_and(|s| s.name == "__debug")
    }

    fn callee_is_concat(callee: &HirExpr) -> bool {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return false;
        };
        segments.last().is_some_and(|s| s.name == "__concat")
    }

    /// The [`Op::UintLeaves`] descriptor a rendering method's receiver
    /// needs, or `None` when the method renders nothing the value alone
    /// cannot say. `join` renders the elements without the brackets, so
    /// its receiver takes the element-only descriptor.
    fn render_receiver_desc(&self, ty: Ty, method: &str, argc: usize) -> Option<String> {
        match (method, argc) {
            ("to_string" | "fmt", 0) => crate::value::render_descriptor(self.tcx, ty),
            ("join", 1) => crate::value::element_render_descriptor(self.tcx, ty),
            _ => None,
        }
    }

    /// The [`Op::UintLeaves`] descriptor for a rendered argument of type `ty`:
    /// where the type declared its integers `u64` / `usize`. `None` when it
    /// declared none, which is every value that renders as it always has.
    ///
    /// The shape mirrors what the compiled tiers' element, payload, and slot
    /// tags render unsigned, so all three tiers read one value the same way.
    pub(crate) fn uint_leaves_desc(&self, ty: Ty) -> Option<String> {
        crate::value::render_descriptor(self.tcx, ty)
    }

    pub(crate) fn expr_has_uint_display_provenance(&self, expr: &HirExpr) -> bool {
        if self.is_unsigned64_ty(expr.ty) {
            return true;
        }
        match &expr.kind {
            HirExprKind::Cast { ty, .. } => self.is_unsigned64_ty(*ty),
            HirExprKind::Path { segments, .. } if segments.len() == 1 => self
                .lookup_local(&segments[0].name)
                .is_some_and(|tr| self.uint_display_locals.contains(&tr.reg)),
            _ => false,
        }
    }

    /// Returns the home register of a `&mut Vec<T>` / `&mut [T]`
    /// call argument when the argument is a plain local place -
    /// either `&mut x` over a local or a bare path forwarding a
    /// `&mut` parameter. Non-local places (fields, indexes) and
    /// non-`&mut`-vec types return `None` and take the ordinary
    /// pass-by-value path.
    /// The single-segment local name of a bare-local place expression
    /// (`s`), or `None` for any other shape.
    fn path_single_seg_name(place: &HirExpr) -> Option<&str> {
        if let HirExprKind::Path { segments, .. } = &place.kind {
            if let [seg] = segments.as_slice() {
                return Some(seg.name.as_str());
            }
        }
        None
    }

    /// The single-segment local name a `&mut <local>` argument refers
    /// to - the `RefMut` operand (`&mut s`) or a bare path forwarding a
    /// `&mut` parameter (`s`).
    fn mut_ref_place_name(arg: &HirExpr) -> Option<&str> {
        let place = match &arg.kind {
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand,
            } => operand.as_ref(),
            _ => arg,
        };
        Self::path_single_seg_name(place)
    }

    /// `true` when moving (rather than cloning) the `&mut <local>`
    /// argument at `self_idx` into its write-back cell is safe: no other
    /// argument in the call reads the same local. A sibling read forces
    /// the clone so it observes the local's pre-call value, matching the
    /// compiled tiers, which pass `&mut` by pointer and evaluate the
    /// reading argument against the live binding.
    fn mut_arg_move_safe(args: &[HirExpr], self_idx: usize, name: &str) -> bool {
        let bound: std::collections::HashSet<String> = std::collections::HashSet::new();
        let shadowed: std::collections::HashSet<String> =
            gossamer_hir::shadowed_global_names(|candidate| candidate == name);
        args.iter().enumerate().all(|(j, other)| {
            j == self_idx
                || !gossamer_hir::collect_free_vars(other, &bound, &shadowed)
                    .iter()
                    .any(|v| v == name)
        })
    }

    fn mut_ref_arg_home(&self, arg: &HirExpr, expected_ty: Option<Ty>) -> Option<Reg> {
        // The callee's declared parameter decides, exactly as it does in
        // `mut_ref_writeback_place`: a `&Vec<T>` parameter reads the vector
        // and unwraps no cell, so a `&mut Vec<T>` argument reborrows as the
        // bare value.
        let expects_mut_vec = match expected_ty {
            Some(expected) => crate::compile::is_mut_ref_vec(self.tcx, expected),
            None => crate::compile::is_mut_ref_vec(self.tcx, arg.ty),
        };
        if !expects_mut_vec {
            return None;
        }
        let place = match &arg.kind {
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand,
            } => operand,
            HirExprKind::Path { .. } => arg,
            _ => return None,
        };
        let HirExprKind::Path { segments, .. } = &place.kind else {
            return None;
        };
        let [seg] = segments.as_slice() else {
            return None;
        };
        let tr = self.lookup_local(seg.name.as_str())?;
        (tr.kind == RegKind::Value).then_some(tr.reg)
    }

    /// Returns the lvalue place of a `&mut Vec<T>` / `&mut [T]` /
    /// `&mut <scalar>` call argument that is *not* a plain local Vec
    /// (`&mut s.field`, `&mut grid[i]`, `&mut <scalar local>`, or a bare
    /// path forwarding a `&mut` parameter). The caller wraps the place in
    /// a write-back cell and re-stores the callee's final value through it
    /// after the call. The plain-local-Vec case is handled separately by
    /// [`Self::mut_ref_arg_home`]; everything that isn't a write-through
    /// place (a temporary, a deref of a call result) returns `None`.
    fn mut_ref_writeback_place<'a>(
        tcx: &TyCtxt,
        arg: &'a HirExpr,
        expected_ty: Option<Ty>,
    ) -> Option<&'a HirExpr> {
        let typed_as_mut_ref = crate::compile::is_mut_ref_writeback(tcx, arg.ty);
        let (place, explicit_mut_place) = match &arg.kind {
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand,
            } => (operand.as_ref(), true),
            _ => (arg, false),
        };
        // The callee unwraps an incoming cell for exactly the parameters
        // its own declared type marks as write-back, so the declared type
        // decides here too; the argument's own shape answers only when the
        // callee is unknown at this call site.
        let participates = match expected_ty {
            Some(expected) => crate::compile::is_mut_ref_writeback(tcx, expected),
            None => {
                typed_as_mut_ref
                    || (explicit_mut_place && crate::compile::is_writeback_pointee(tcx, place.ty))
            }
        };
        if !participates {
            return None;
        }
        matches!(
            place.kind,
            HirExprKind::Path { .. }
                | HirExprKind::Field { .. }
                | HirExprKind::TupleIndex { .. }
                | HirExprKind::Index { .. }
        )
        .then_some(place)
    }

    /// The single `Type::method` key matching bare `method`, when exactly
    /// one `impl` in the program declares it. Two or more is genuinely
    /// ambiguous without the receiver's type, so the caller keeps the
    /// by-name dispatch.
    /// `xs.join(sep)` where the sequence's element type supplies its own
    /// rendering: the separator and that method's qualified name travel to
    /// the runtime, which dispatches per element.
    fn try_compile_rendered_join(
        &mut self,
        receiver: &HirExpr,
        separator: &HirExpr,
    ) -> RuntimeResult<Option<Reg>> {
        let mut seq_ty = receiver.ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(seq_ty) {
            seq_ty = *inner;
        }
        let elem_ty = match self.tcx.kind(seq_ty) {
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => *elem,
            _ => return Ok(None),
        };
        let Some(method) = self.user_rendering_qualified_name(elem_ty, "to_string") else {
            return Ok(None);
        };
        let receiver_reg = self.compile_expr(receiver)?;
        let sep_reg = self.compile_expr(separator)?;
        let method_reg = self.alloc_reg();
        let const_idx = self.const_idx(
            ConstKey::String(method.clone()),
            Value::String(method.as_str().into()),
        );
        self.emit(Op::LoadConst {
            dst: method_reg,
            idx: const_idx,
        });
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(2)
            .expect("register overflow reserving join args");
        self.ensure_reg_slot(args_start + 1);
        self.emit(Op::Move {
            dst: args_start,
            src: sep_reg,
        });
        self.emit(Op::Move {
            dst: args_start + 1,
            src: method_reg,
        });
        let dst = self.alloc_reg();
        let name_idx = self.global_idx("__join_rendered");
        let cache_idx = self.alloc_cache_idx();
        self.emit(Op::MethodCall {
            dst,
            receiver: receiver_reg,
            name_idx,
            args: args_start,
            argc: 2,
            cache_idx,
        });
        Ok(Some(dst))
    }

    /// The fully qualified `Type::method` a user `impl` supplies to render
    /// values of `ty` on `method`'s channel, or `None` when nothing overrides
    /// the synthesized form. `method` is `to_string` for `Display` (`{}`) and
    /// `fmt` for `Debug` (`{:?}`); the two channels never borrow each other's
    /// method, exactly as `Display` and `Debug` stay distinct traits.
    fn user_rendering_qualified_name(&self, ty: Ty, method: &str) -> Option<String> {
        let mut resolved = ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = *inner;
        }
        let type_name = match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, .. }) => self.tcx.def_name(*def)?.to_string(),
            // A sequence and a map are their own type kinds rather than named
            // Adts, so their source name is the one an `impl` for them
            // registers under - without this an `impl Display for Vec` was
            // compiled and never called.
            Some(TyKind::Vec(_) | TyKind::Slice(_)) => "Vec".to_string(),
            Some(TyKind::HashMap { ordered, .. }) => {
                if *ordered { "BTreeMap" } else { "Map" }.to_string()
            }
            _ => return None,
        };
        let qualified = format!("{type_name}::{method}");
        self.fn_param_tys
            .contains_key(&qualified)
            .then_some(qualified)
    }

    /// Whether a user `impl` supplies `method` for `ty`, so a value of it
    /// renders through that body rather than the synthesized form.
    pub(crate) fn has_user_rendering(&self, ty: Ty, method: &str) -> bool {
        self.user_rendering_qualified_name(ty, method).is_some()
    }

    /// Renders `arg` through its type's own method for this channel when one
    /// exists, so `{}` shows what `impl Display` says and `{:?}` what
    /// `impl Debug` says, rather than the synthesized shape. `Ok(None)` when
    /// nothing overrides.
    fn compile_user_rendering(
        &mut self,
        arg: &HirExpr,
        method: &str,
    ) -> RuntimeResult<Option<Reg>> {
        if self.has_user_rendering(arg.ty, method) {
            let name = Ident {
                name: method.to_string(),
            };
            return self.compile_method_call(arg, &name, &[], None).map(Some);
        }
        // A container, tuple, or `Option` holding such a type renders its
        // elements the same way, at any depth. The value carries its type
        // name at run time, so the walk resolves each element's method
        // itself; this only decides whether the walk is worth entering.
        if !self.ty_contains_user_rendering(arg.ty, 0, method) {
            return Ok(None);
        }
        let idx = self.global_idx("__render_display");
        let callee_reg = self.alloc_reg();
        self.emit(Op::LoadGlobal {
            dst: callee_reg,
            idx,
        });
        let compiled = self.compile_expr(arg)?;
        // The walk sees values, and a `Vec` and a fixed array share one
        // runtime representation while a `u64` shares a slot with an
        // `i64`. The descriptor built from the static type travels with
        // the renderer's private copy so the walk spells both the way a
        // program that wrote them spells them.
        let value = match self.uint_leaves_desc(arg.ty) {
            Some(desc) => {
                let dst = self.alloc_reg();
                let desc_idx = self.const_idx(
                    ConstKey::String(desc.clone()),
                    Value::String(desc.as_str().into()),
                );
                self.emit(Op::UintLeaves {
                    dst,
                    src: compiled,
                    desc_idx,
                });
                dst
            }
            None => compiled,
        };
        // An enum value carries only its variant name at run time, so the
        // walk cannot name the type whose `impl` renders it. The compiler
        // knows both, and hands over `Variant=Type::method` lines for every
        // enum nested in the operand.
        let mut aliases = String::new();
        self.collect_variant_rendering_aliases(arg.ty, 0, method, &mut aliases);
        let alias_reg = self.alloc_reg();
        let const_idx = self.const_idx(
            ConstKey::String(aliases.clone()),
            Value::String(aliases.as_str().into()),
        );
        self.emit(Op::LoadConst {
            dst: alias_reg,
            idx: const_idx,
        });
        let method_reg = self.alloc_reg();
        let method_idx = self.const_idx(
            ConstKey::String(method.to_string()),
            Value::String(method.into()),
        );
        self.emit(Op::LoadConst {
            dst: method_reg,
            idx: method_idx,
        });
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(3)
            .expect("register overflow reserving render args");
        self.ensure_reg_slot(args_start + 2);
        self.emit(Op::Move {
            dst: args_start,
            src: value,
        });
        self.emit(Op::Move {
            dst: args_start + 1,
            src: alias_reg,
        });
        self.emit(Op::Move {
            dst: args_start + 2,
            src: method_reg,
        });
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        self.emit(Op::Call {
            dst,
            callee: callee_reg,
            args: args_start,
            argc: 3,
            cache_idx,
            may_have_cells: true,
        });
        Ok(Some(dst))
    }

    /// Appends one `Variant=Type::method` line per variant of every enum
    /// nested in `ty` whose own type supplies a rendering, so the runtime
    /// walk can resolve a variant value back to its enum.
    fn collect_variant_rendering_aliases(
        &self,
        ty: Ty,
        depth: u32,
        method: &str,
        out: &mut String,
    ) {
        if depth > 8 {
            return;
        }
        let mut resolved = ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = *inner;
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs }) => {
                let (def, substs) = (*def, substs.clone());
                if let Some(qualified) = self.user_rendering_qualified_name(resolved, method)
                    && let Some(names) = self.tcx.enum_variant_names(def)
                {
                    for variant in names {
                        out.push_str(variant);
                        out.push('=');
                        out.push_str(&qualified);
                        out.push('\n');
                    }
                }
                for arg in substs.types() {
                    self.collect_variant_rendering_aliases(arg, depth + 1, method, out);
                }
            }
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
                self.collect_variant_rendering_aliases(*elem, depth + 1, method, out);
            }
            Some(TyKind::Tuple(elems)) => {
                for elem in elems.clone() {
                    self.collect_variant_rendering_aliases(elem, depth + 1, method, out);
                }
            }
            Some(TyKind::HashMap { key, value, .. }) => {
                let (key, value) = (*key, *value);
                self.collect_variant_rendering_aliases(key, depth + 1, method, out);
                self.collect_variant_rendering_aliases(value, depth + 1, method, out);
            }
            _ => {}
        }
    }

    /// Whether `ty`, or a type nested inside it, supplies its own rendering
    /// for this channel.
    fn ty_contains_user_rendering(&self, ty: Ty, depth: u32, method: &str) -> bool {
        // A recursive type would otherwise walk forever; a rendering method
        // that only appears below this many levels is rare enough that the
        // synthesized form is the honest answer.
        if depth > 8 {
            return false;
        }
        let mut resolved = ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = *inner;
        }
        if self.has_user_rendering(resolved, method) {
            return true;
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
                self.ty_contains_user_rendering(*elem, depth + 1, method)
            }
            Some(TyKind::Tuple(elems)) => elems
                .clone()
                .iter()
                .any(|elem| self.ty_contains_user_rendering(*elem, depth + 1, method)),
            Some(TyKind::HashMap { key, value, .. }) => {
                let (key, value) = (*key, *value);
                self.ty_contains_user_rendering(key, depth + 1, method)
                    || self.ty_contains_user_rendering(value, depth + 1, method)
            }
            Some(TyKind::Adt { substs, .. }) => substs
                .types()
                .clone()
                .into_iter()
                .any(|arg| self.ty_contains_user_rendering(arg, depth + 1, method)),
            _ => false,
        }
    }

    fn sole_impl_method(&self, method: &str) -> Option<String> {
        let suffix = format!("::{method}");
        let mut found: Option<&String> = None;
        for key in self.fn_param_tys.keys() {
            if !key.ends_with(&suffix) {
                continue;
            }
            // Only a method answers a method call. A module's free function
            // is filed under `module::name` too, and binding one here would
            // hand `value.name(..)` to a function that never took a receiver.
            if !self.impl_methods.contains(key.as_str()) {
                continue;
            }
            // `mod::Type::method` and `Type::method` name one method.
            if found.is_some_and(|prev| !prev.ends_with(key.as_str()) && !key.ends_with(prev)) {
                return None;
            }
            if found.is_none_or(|prev| key.len() < prev.len()) {
                found = Some(key);
            }
        }
        found.cloned()
    }

    fn callee_param_tys(&self, callee: &HirExpr) -> Option<Vec<Ty>> {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return None;
        };
        // A call through a function-typed local is indirect: the value the
        // binding holds decides the parameters, and a global that happens to
        // share the binding's name says nothing about them. Reading that
        // global's types here lowers the argument for a parameter the callee
        // does not have - a `&mut` one wraps it in a cell, and the callback
        // then receives a reference where it declared a value.
        if segments.len() == 1 && self.lookup_local(&segments[0].name).is_some() {
            return None;
        }
        let key = segments
            .iter()
            .map(|seg| seg.name.as_str())
            .collect::<Vec<_>>()
            .join("::");
        self.fn_param_tys.get(&key).cloned()
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
