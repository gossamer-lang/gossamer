#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

impl<'tcx> FnBuilder<'tcx> {
    /// Emits a positional tuple-field read - draining
    /// (`TupleIndexConsume`) when `consume` is set, cloning
    /// (`TupleIndex`) otherwise.
    fn emit_tuple_index(&mut self, dst: Reg, receiver: Reg, index: u32, consume: bool) {
        if consume {
            self.emit(Op::TupleIndexConsume {
                dst,
                receiver,
                index,
            });
        } else {
            self.emit(Op::TupleIndex {
                dst,
                receiver,
                index,
            });
        }
    }

    pub(crate) fn bind_pattern_locals(
        &mut self,
        pattern: &HirPat,
        init_reg: Reg,
    ) -> RuntimeResult<()> {
        self.bind_pattern_locals_ex(pattern, init_reg, false)
    }

    /// Like [`Self::bind_pattern_locals`] but with a `consume` flag:
    /// when set, `init_reg` holds a uniquely-owned aggregate the
    /// destructure may drain (a for-loop element moved out of a
    /// consumable source), so tuple-field extraction moves instead of
    /// cloning. The `Arc::get_mut` guard on `TupleIndexConsume` keeps a
    /// still-shared aggregate correct.
    pub(crate) fn bind_pattern_locals_ex(
        &mut self,
        pattern: &HirPat,
        init_reg: Reg,
        consume: bool,
    ) -> RuntimeResult<()> {
        match &pattern.kind {
            HirPatKind::Binding { name, .. } => {
                self.bind_local(
                    &name.name,
                    TypedReg {
                        reg: init_reg,
                        kind: RegKind::Value,
                    },
                );
                Ok(())
            }
            HirPatKind::Tuple(elems) => {
                let rest_pos = elems
                    .iter()
                    .position(|p| matches!(p.kind, HirPatKind::Rest));
                let n_after = rest_pos.map_or(0, |r| elems.len() - r - 1);
                for (i, sub) in elems.iter().enumerate() {
                    if matches!(sub.kind, HirPatKind::Rest) {
                        continue;
                    }
                    let dst = self.alloc_reg();
                    match rest_pos {
                        None => {
                            self.emit_tuple_index(dst, init_reg, i as u32, consume);
                        }
                        Some(rest_idx) if i < rest_idx => {
                            self.emit_tuple_index(dst, init_reg, i as u32, consume);
                        }
                        Some(_) => {
                            // Tail-anchored: offset_from_end = n_after - (i - rest_idx - 1) - 1
                            let offset = n_after - (i - rest_pos.unwrap() - 1) - 1;
                            self.emit(Op::TupleTailIndex {
                                dst,
                                receiver: init_reg,
                                offset_from_end: offset as u32,
                            });
                        }
                    }
                    // A drained element is uniquely owned, so propagate
                    // `consume` into nested tuple sub-patterns.
                    self.bind_pattern_locals_ex(sub, dst, consume)?;
                }
                Ok(())
            }
            HirPatKind::Struct { fields, .. } => {
                for fp in fields {
                    let fname_idx = self.const_idx(
                        ConstKey::String(fp.name.name.clone()),
                        Value::String(SmolStr::from(fp.name.name.as_str())),
                    );
                    let dst = self.alloc_reg();
                    let cache_idx = self.alloc_field_cache_idx();
                    self.emit(Op::FieldGet {
                        dst,
                        receiver: init_reg,
                        name_idx: fname_idx,
                        cache_idx,
                    });
                    match &fp.pattern {
                        Some(sub) => self.bind_pattern_locals(sub, dst)?,
                        None => self.bind_local(
                            &fp.name.name,
                            TypedReg {
                                reg: dst,
                                kind: RegKind::Value,
                            },
                        ),
                    }
                }
                Ok(())
            }
            HirPatKind::Variant { fields, .. } => {
                for (i, fp) in fields.iter().enumerate() {
                    let dst = self.alloc_reg();
                    let idx = u16::try_from(i)
                        .map_err(|_| RuntimeError::Unsupported("variant arity exceeds 65535"))?;
                    self.emit(Op::VariantField {
                        dst,
                        src: init_reg,
                        idx,
                    });
                    self.bind_pattern_locals(fp, dst)?;
                }
                Ok(())
            }
            HirPatKind::At { name, sub, .. } => {
                self.bind_pattern_locals_ex(sub, init_reg, consume)?;
                let bound = self.alloc_reg();
                self.emit(Op::Move {
                    dst: bound,
                    src: init_reg,
                });
                self.bind_local(
                    &name.name,
                    TypedReg {
                        reg: bound,
                        kind: RegKind::Value,
                    },
                );
                Ok(())
            }
            HirPatKind::Or(alts) => {
                // Irrefutable let: the alternatives jointly cover the
                // value's type, so exactly one matches at runtime and
                // the final alternative is the guaranteed fallback. Every
                // alternative binds the same names; one shared register
                // per name is the single home the following code reads,
                // so each alternative copies its freshly-extracted
                // bindings into those shared registers before continuing.
                let mut names: Vec<String> = Vec::new();
                super::compile_expr::collect_pattern_binding_names(pattern, &mut names);
                let shared: Vec<(String, Reg)> = names
                    .into_iter()
                    .map(|name| (name, self.alloc_reg()))
                    .collect();
                let mut matched: Vec<InstrIdx> = Vec::new();
                let last = alts.len().saturating_sub(1);
                for (i, alt) in alts.iter().enumerate() {
                    let mut alt_fails: Vec<InstrIdx> = Vec::new();
                    self.push_scope();
                    if i == last {
                        self.bind_pattern_locals(alt, init_reg)?;
                    } else {
                        self.emit_pattern_test(init_reg, alt, &mut alt_fails)?;
                    }
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
                    if i != last {
                        matched.push(self.emit(Op::Jump { target: 0 }));
                        let next_alt = self.cur_idx();
                        for f in alt_fails {
                            self.patch_jump(f, next_alt);
                        }
                    }
                }
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
                Ok(())
            }
            HirPatKind::Wildcard | HirPatKind::Literal(_) | HirPatKind::Rest => Ok(()),
            // VM references preserve the underlying value representation.
            // Give the peeled value its own register before binding it so a
            // bare-path initializer (`let &x = value`) cannot be cleared as
            // the old name's last use while `x` still aliases that register.
            HirPatKind::Ref { inner, .. } => {
                let peeled = self.alloc_reg();
                self.emit(Op::Move {
                    dst: peeled,
                    src: init_reg,
                });
                self.bind_pattern_locals_ex(inner, peeled, consume)
            }
            _ => Err(RuntimeError::Unsupported(
                "this `let` pattern is not supported by the VM compiler",
            )),
        }
    }

    /// Allocates a fresh `arith_caches` slot for a Tier-C2
    /// adaptive op and returns its index. Each emit site gets
    /// its own slot so observed shapes don't bleed across call
    /// sites that happen to flow through the same handler.
    pub(crate) fn next_arith_cache(&mut self) -> u16 {
        let idx = self.next_arith_cache_idx;
        self.next_arith_cache_idx = self.next_arith_cache_idx.saturating_add(1);
        idx
    }

    /// Builds the boxed-`Value` op for `op` on `(lhs, rhs)`
    /// destined for `dst`. Adaptive arith variants (Add/Sub/Mul/
    /// Div/Rem) allocate a fresh cache slot here so the runtime
    /// has somewhere to record the observed shape (Tier C2).
    pub(crate) fn binary_op(
        &mut self,
        op: HirBinaryOp,
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
    ) -> Option<Op> {
        Some(match op {
            HirBinaryOp::Add => Op::AddInt {
                dst,
                lhs,
                rhs,
                cache_idx: self.next_arith_cache(),
            },
            HirBinaryOp::Sub => Op::SubInt {
                dst,
                lhs,
                rhs,
                cache_idx: self.next_arith_cache(),
            },
            HirBinaryOp::Mul => Op::MulInt {
                dst,
                lhs,
                rhs,
                cache_idx: self.next_arith_cache(),
            },
            HirBinaryOp::Div => Op::DivInt {
                dst,
                lhs,
                rhs,
                cache_idx: self.next_arith_cache(),
            },
            HirBinaryOp::Rem => Op::RemInt {
                dst,
                lhs,
                rhs,
                cache_idx: self.next_arith_cache(),
            },
            HirBinaryOp::Eq => Op::Eq { dst, lhs, rhs },
            HirBinaryOp::Ne => Op::Ne { dst, lhs, rhs },
            HirBinaryOp::Lt => Op::Lt { dst, lhs, rhs },
            HirBinaryOp::Le => Op::Le { dst, lhs, rhs },
            HirBinaryOp::Gt => Op::Gt { dst, lhs, rhs },
            HirBinaryOp::Ge => Op::Ge { dst, lhs, rhs },
            _ => return None,
        })
    }

    /// Matches `a * b + c`, `c + a * b`, or `c - a * b` in the
    /// HIR and emits a single fused-multiply-{add,sub} op
    /// instead of the two-op sequence. All three operands must
    /// resolve to concrete f64 kinds.
    pub(crate) fn try_compile_fma(
        &mut self,
        op: HirBinaryOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> RuntimeResult<Option<TypedReg>> {
        match op {
            HirBinaryOp::Add => {
                // a * b + c
                if let HirExprKind::Binary {
                    op: HirBinaryOp::Mul,
                    lhs: ma,
                    rhs: mb,
                } = &lhs.kind
                {
                    if self.expr_kind(ma) == RegKind::F64
                        && self.expr_kind(mb) == RegKind::F64
                        && self.expr_kind(rhs) == RegKind::F64
                    {
                        let a_tr = self.compile_expr_ex(ma)?;
                        let b_tr = self.compile_expr_ex(mb)?;
                        let c_tr = self.compile_expr_ex(rhs)?;
                        let a_f = self.as_f64(a_tr);
                        let b_f = self.as_f64(b_tr);
                        let c_f = self.as_f64(c_tr);
                        let dst = self.alloc_float();
                        self.emit(Op::MulAddF64 {
                            dst_f: dst,
                            a_f,
                            b_f,
                            c_f,
                        });
                        return Ok(Some(TypedReg {
                            reg: dst,
                            kind: RegKind::F64,
                        }));
                    }
                }
                // c + a * b
                if let HirExprKind::Binary {
                    op: HirBinaryOp::Mul,
                    lhs: ma,
                    rhs: mb,
                } = &rhs.kind
                {
                    if self.expr_kind(ma) == RegKind::F64
                        && self.expr_kind(mb) == RegKind::F64
                        && self.expr_kind(lhs) == RegKind::F64
                    {
                        let c_tr = self.compile_expr_ex(lhs)?;
                        let a_tr = self.compile_expr_ex(ma)?;
                        let b_tr = self.compile_expr_ex(mb)?;
                        let a_f = self.as_f64(a_tr);
                        let b_f = self.as_f64(b_tr);
                        let c_f = self.as_f64(c_tr);
                        let dst = self.alloc_float();
                        self.emit(Op::MulAddF64 {
                            dst_f: dst,
                            a_f,
                            b_f,
                            c_f,
                        });
                        return Ok(Some(TypedReg {
                            reg: dst,
                            kind: RegKind::F64,
                        }));
                    }
                }
            }
            HirBinaryOp::Sub => {
                // c - a * b
                if let HirExprKind::Binary {
                    op: HirBinaryOp::Mul,
                    lhs: ma,
                    rhs: mb,
                } = &rhs.kind
                {
                    if self.expr_kind(ma) == RegKind::F64
                        && self.expr_kind(mb) == RegKind::F64
                        && self.expr_kind(lhs) == RegKind::F64
                    {
                        let c_tr = self.compile_expr_ex(lhs)?;
                        let a_tr = self.compile_expr_ex(ma)?;
                        let b_tr = self.compile_expr_ex(mb)?;
                        let a_f = self.as_f64(a_tr);
                        let b_f = self.as_f64(b_tr);
                        let c_f = self.as_f64(c_tr);
                        let dst = self.alloc_float();
                        self.emit(Op::MulSubF64 {
                            dst_f: dst,
                            a_f,
                            b_f,
                            c_f,
                        });
                        return Ok(Some(TypedReg {
                            reg: dst,
                            kind: RegKind::F64,
                        }));
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    pub(crate) fn emit_binary_f64(
        &mut self,
        op: HirBinaryOp,
        lhs_f: Reg,
        rhs_f: Reg,
    ) -> RuntimeResult<TypedReg> {
        match op {
            HirBinaryOp::Add => {
                let dst = self.alloc_float();
                self.emit(Op::AddF64 {
                    dst_f: dst,
                    lhs_f,
                    rhs_f,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::F64,
                })
            }
            HirBinaryOp::Sub => {
                let dst = self.alloc_float();
                self.emit(Op::SubF64 {
                    dst_f: dst,
                    lhs_f,
                    rhs_f,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::F64,
                })
            }
            // (handled by the caller via `try_compile_fma` -
            // this arm is the non-fused fallback)
            HirBinaryOp::Mul => {
                let dst = self.alloc_float();
                self.emit(Op::MulF64 {
                    dst_f: dst,
                    lhs_f,
                    rhs_f,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::F64,
                })
            }
            HirBinaryOp::Div => {
                let dst = self.alloc_float();
                self.emit(Op::DivF64 {
                    dst_f: dst,
                    lhs_f,
                    rhs_f,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::F64,
                })
            }
            HirBinaryOp::Lt => {
                let dst = self.alloc_reg();
                self.emit(Op::LtF64 {
                    dst_v: dst,
                    lhs_f,
                    rhs_f,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirBinaryOp::Le => {
                let dst = self.alloc_reg();
                self.emit(Op::LeF64 {
                    dst_v: dst,
                    lhs_f,
                    rhs_f,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirBinaryOp::Gt => {
                let dst = self.alloc_reg();
                self.emit(Op::GtF64 {
                    dst_v: dst,
                    lhs_f,
                    rhs_f,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirBinaryOp::Ge => {
                let dst = self.alloc_reg();
                self.emit(Op::GeF64 {
                    dst_v: dst,
                    lhs_f,
                    rhs_f,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirBinaryOp::Eq => {
                let dst = self.alloc_reg();
                self.emit(Op::EqF64 {
                    dst_v: dst,
                    lhs_f,
                    rhs_f,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirBinaryOp::Ne => {
                let dst = self.alloc_reg();
                self.emit(Op::NeF64 {
                    dst_v: dst,
                    lhs_f,
                    rhs_f,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            _ => Err(RuntimeError::Unsupported("f64 binary op kind")),
        }
    }

    /// True when `ty` (through `&` / `&mut` layers) is `u64` or `usize` -
    /// the only ≤64-bit integer types whose value can exceed `i64::MAX`,
    /// so comparison and right-shift must use unsigned semantics. The
    /// narrower unsigned types (`u8`/`u16`/`u32`) mask to a value below
    /// `2^63`, where signed and unsigned ops coincide.
    pub(crate) fn is_unsigned64_ty(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(self.unwrap_ref(ty)),
            Some(TyKind::Int(
                gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize
            ))
        )
    }

    pub(crate) fn emit_binary_i64(
        &mut self,
        op: HirBinaryOp,
        lhs_i: Reg,
        rhs_i: Reg,
        lhs_unsigned: bool,
        rhs_unsigned: bool,
        overflow_ty: Option<gossamer_types::IntTy>,
    ) -> RuntimeResult<TypedReg> {
        // Relational comparisons treat the pair as unsigned when either
        // operand's declared type is unsigned 64-bit; a right-shift keys
        // off the shifted (left) operand only.
        let cmp_unsigned = lhs_unsigned || rhs_unsigned;
        match op {
            HirBinaryOp::Add => {
                let dst = self.alloc_int();
                self.emit(match overflow_ty {
                    Some(overflow_ty) => Op::CheckedAddI64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                        overflow_ty,
                    },
                    None => Op::AddI64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                    },
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                })
            }
            HirBinaryOp::Sub => {
                let dst = self.alloc_int();
                self.emit(match overflow_ty {
                    Some(overflow_ty) => Op::CheckedSubI64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                        overflow_ty,
                    },
                    None => Op::SubI64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                    },
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                })
            }
            HirBinaryOp::Mul => {
                let dst = self.alloc_int();
                self.emit(match overflow_ty {
                    Some(overflow_ty) => Op::CheckedMulI64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                        overflow_ty,
                    },
                    None => Op::MulI64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                    },
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                })
            }
            HirBinaryOp::Div => {
                let dst = self.alloc_int();
                self.emit(if cmp_unsigned {
                    Op::DivU64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                    }
                } else {
                    Op::DivI64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                    }
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                })
            }
            HirBinaryOp::Rem => {
                let dst = self.alloc_int();
                self.emit(if cmp_unsigned {
                    Op::RemU64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                    }
                } else {
                    Op::RemI64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                    }
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                })
            }
            HirBinaryOp::Lt => {
                let dst = self.alloc_reg();
                self.emit(if cmp_unsigned {
                    Op::LtU64 {
                        dst_v: dst,
                        lhs_i,
                        rhs_i,
                    }
                } else {
                    Op::LtI64 {
                        dst_v: dst,
                        lhs_i,
                        rhs_i,
                    }
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirBinaryOp::Le => {
                let dst = self.alloc_reg();
                self.emit(if cmp_unsigned {
                    Op::LeU64 {
                        dst_v: dst,
                        lhs_i,
                        rhs_i,
                    }
                } else {
                    Op::LeI64 {
                        dst_v: dst,
                        lhs_i,
                        rhs_i,
                    }
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirBinaryOp::Gt => {
                let dst = self.alloc_reg();
                self.emit(if cmp_unsigned {
                    Op::GtU64 {
                        dst_v: dst,
                        lhs_i,
                        rhs_i,
                    }
                } else {
                    Op::GtI64 {
                        dst_v: dst,
                        lhs_i,
                        rhs_i,
                    }
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirBinaryOp::Ge => {
                let dst = self.alloc_reg();
                self.emit(if cmp_unsigned {
                    Op::GeU64 {
                        dst_v: dst,
                        lhs_i,
                        rhs_i,
                    }
                } else {
                    Op::GeI64 {
                        dst_v: dst,
                        lhs_i,
                        rhs_i,
                    }
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirBinaryOp::Eq => {
                let dst = self.alloc_reg();
                self.emit(Op::EqI64 {
                    dst_v: dst,
                    lhs_i,
                    rhs_i,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirBinaryOp::Ne => {
                let dst = self.alloc_reg();
                self.emit(Op::NeI64 {
                    dst_v: dst,
                    lhs_i,
                    rhs_i,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::Value,
                })
            }
            HirBinaryOp::BitAnd => {
                let dst = self.alloc_int();
                self.emit(Op::BitAndI64 {
                    dst_i: dst,
                    lhs_i,
                    rhs_i,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                })
            }
            HirBinaryOp::BitOr => {
                let dst = self.alloc_int();
                self.emit(Op::BitOrI64 {
                    dst_i: dst,
                    lhs_i,
                    rhs_i,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                })
            }
            HirBinaryOp::BitXor => {
                let dst = self.alloc_int();
                self.emit(Op::BitXorI64 {
                    dst_i: dst,
                    lhs_i,
                    rhs_i,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                })
            }
            HirBinaryOp::Shl => {
                let dst = self.alloc_int();
                self.emit(Op::ShlI64 {
                    dst_i: dst,
                    lhs_i,
                    rhs_i,
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                })
            }
            HirBinaryOp::Shr => {
                let dst = self.alloc_int();
                self.emit(if lhs_unsigned {
                    Op::ShrU64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                    }
                } else {
                    Op::ShrI64 {
                        dst_i: dst,
                        lhs_i,
                        rhs_i,
                    }
                });
                Ok(TypedReg {
                    reg: dst,
                    kind: RegKind::I64,
                })
            }
            _ => Err(RuntimeError::Unsupported("i64 binary op kind")),
        }
    }

    /// Detects `[S; N]` array literals where `S` is a struct
    /// whose fields are all `f64`, and emits a flat-f64
    /// `Op::BuildFloatArray` instead of constructing the
    /// boxed `Value::Array<Value::Struct>` form. Subsequent
    /// indexed field access on the resulting local routes
    /// through the flat fast path in the VM.
    pub(crate) fn try_build_float_array(
        &mut self,
        array_ty: Ty,
        elems: &[HirExpr],
    ) -> RuntimeResult<Option<TypedReg>> {
        // Require a fixed-size `[S; N]` array shape. The flat
        // `FloatArray` representation has no growth path, so a
        // growable `Vec`/`Slice` of all-f64 structs must keep the
        // boxed `Array(Struct)` form that `push`/`pop` understand.
        let elem_ty = match self.tcx.kind(array_ty) {
            Some(TyKind::Array { elem, .. }) => *elem,
            _ => return Ok(None),
        };
        let (def, struct_name) = match self.tcx.kind(elem_ty) {
            Some(TyKind::Adt { def, .. }) => {
                let Some(layout) = self.layouts.get(def) else {
                    return Ok(None);
                };
                // Need a name for rehydration; grab it from
                // any layout key. We don't have a DefId→Name
                // table here, so rely on each element `__struct`
                // call carrying the name string.
                let _ = layout;
                (*def, "")
            }
            _ => return Ok(None),
        };
        let Some(field_names) = self.layouts.get(&def).cloned() else {
            return Ok(None);
        };
        // If the type context knows the declared field types,
        // require every one to be `f64`. When the types aren't
        // registered (e.g. for programs whose resolver didn't
        // populate `struct_field_tys`) we still try the fast
        // path as long as every element in the literal is
        // clearly the same struct - the `__struct` parse below
        // sees the actual field values, so a later type mismatch
        // would just fall back at runtime.
        if let Some(tys) = self.tcx.struct_field_tys(def) {
            let all_f64 = tys
                .iter()
                .all(|t| matches!(self.tcx.kind(*t), Some(TyKind::Float(FloatTy::F64))));
            if !all_f64 {
                return Ok(None);
            }
        }
        if field_names.is_empty() {
            return Ok(None);
        }
        let Ok(stride) = u16::try_from(field_names.len()) else {
            return Ok(None);
        };
        let Ok(elem_count) = u16::try_from(elems.len()) else {
            return Ok(None);
        };
        // Pick up the struct name from the first element's
        // `__struct(name, ...)` call; fall back to the layout
        // map if we've seen an explicit name before.
        let _ = struct_name;
        let mut struct_name_found: Option<String> = None;
        // Each element must be a `Call(__struct, args)` whose
        // arg layout matches `name, fname, value, fname, value, …`.
        // Collect the per-element field expressions, keyed by
        // field name.
        let mut per_elem: Vec<std::collections::HashMap<String, &HirExpr>> =
            Vec::with_capacity(elems.len());
        for elem in elems {
            let HirExprKind::Call { callee, args } = &elem.kind else {
                return Ok(None);
            };
            let HirExprKind::Path { segments, .. } = &callee.kind else {
                return Ok(None);
            };
            if segments.len() != 1 || segments[0].name != "__struct" {
                return Ok(None);
            }
            // args: [String(name), String(field1), Value1, ...]
            if args.is_empty() {
                return Ok(None);
            }
            if let HirExprKind::Literal(HirLiteral::String(s)) = &args[0].kind {
                if struct_name_found.is_none() {
                    struct_name_found = Some(s.clone());
                }
            }
            let mut map = std::collections::HashMap::new();
            let rest = &args[1..];
            let mut i = 0;
            while i + 1 < rest.len() {
                let HirExprKind::Literal(HirLiteral::String(fname)) = &rest[i].kind else {
                    return Ok(None);
                };
                map.insert(fname.clone(), &rest[i + 1]);
                i += 2;
            }
            per_elem.push(map);
        }
        let struct_name = struct_name_found.unwrap_or_default();
        // Allocate `stride * elem_count` contiguous float regs.
        let first_f = self.next_float_reg;
        let total = u32::from(stride) * u32::from(elem_count);
        if total > u32::from(u16::MAX - first_f) {
            return Ok(None);
        }
        self.next_float_reg = first_f + total as u16;
        // Compile each field's value expression into the matching
        // float slot.
        for (elem_idx, fields) in per_elem.iter().enumerate() {
            for (field_idx, fname) in field_names.iter().enumerate() {
                let target = first_f + elem_idx as u16 * stride + field_idx as u16;
                if let Some(value_expr) = fields.get(fname) {
                    let tr = self.compile_expr_ex(value_expr)?;
                    let src_f = self.as_f64(tr);
                    self.emit(Op::MoveF64 {
                        dst_f: target,
                        src_f,
                    });
                } else {
                    let idx = self.f64_const_idx(0.0);
                    self.emit(Op::LoadConstF64 { dst_f: target, idx });
                }
            }
        }
        // Intern the struct name + field-name metadata in the
        // const pool so the `BuildFloatArray` op can rehydrate
        // lazily.
        let name_idx = self.const_idx(
            ConstKey::String(struct_name.clone()),
            Value::String(struct_name.into()),
        );
        let fields_value = Value::Array(Arc::new(
            field_names
                .iter()
                .map(|n| Value::String(SmolStr::from(n.clone())))
                .collect::<Vec<_>>(),
        ));
        let fields_idx = self.const_idx(ConstKey::FieldNames(field_names.clone()), fields_value);
        let dst = self.alloc_reg();
        let wide_idx = u16::try_from(self.wide_ops.len()).expect("wide_ops index overflow");
        self.wide_ops
            .push(crate::bytecode::WideOp::BuildFloatArray {
                dst_v: dst,
                name_idx,
                fields_idx,
                stride,
                elem_count,
                first_f,
            });
        self.emit(Op::Wide { idx: wide_idx });
        // Record the register as known-flat so subsequent
        // indexed-field reads / writes can emit
        // `Flat{Get,Set}F64` and skip the runtime
        // discriminant check.
        self.flat_locals.insert(dst, stride);
        Ok(Some(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        }))
    }

    /// Packs a fixed array of all-`f64` structs whose elements are general
    /// expressions. Direct struct literals use [`Self::try_build_float_array`]
    /// and compile straight into float registers. This fallback evaluates
    /// each expression normally, then packs its struct fields once at array
    /// construction time. It covers constructor helpers and other call
    /// boundaries without leaving hot indexed-field loops on boxed storage.
    pub(crate) fn try_build_float_array_from_structs(
        &mut self,
        array_ty: Ty,
        elems: &[HirExpr],
    ) -> RuntimeResult<Option<TypedReg>> {
        let elem_ty = match self.tcx.kind(array_ty) {
            Some(TyKind::Array { elem, .. }) => *elem,
            _ => return Ok(None),
        };
        let def = match self.tcx.kind(elem_ty) {
            Some(TyKind::Adt { def, .. }) => *def,
            _ => return Ok(None),
        };
        let Some(field_names) = self.layouts.get(&def).cloned() else {
            return Ok(None);
        };
        if field_names.is_empty()
            || !self.tcx.struct_field_tys(def).is_some_and(|tys| {
                tys.iter()
                    .all(|ty| matches!(self.tcx.kind(*ty), Some(TyKind::Float(FloatTy::F64))))
            })
        {
            return Ok(None);
        }
        let Ok(stride) = u16::try_from(field_names.len()) else {
            return Ok(None);
        };
        let Some(struct_name) = self.tcx.def_name(def).map(str::to_owned) else {
            return Ok(None);
        };
        let elem_count = u16::try_from(elems.len())
            .map_err(|_| RuntimeError::Unsupported("array literal exceeds 65535 elements"))?;
        if elem_count == 0 {
            return Ok(None);
        }

        let first_v = self.alloc_reg();
        for _ in 1..elems.len() {
            let _ = self.alloc_reg();
        }
        for (index, elem) in elems.iter().enumerate() {
            let src = self.compile_expr(elem)?;
            let dst = first_v + index as u16;
            if src != dst {
                self.emit(Op::Move { dst, src });
            }
        }

        let name_idx = self.const_idx(
            ConstKey::String(struct_name.clone()),
            Value::String(struct_name.into()),
        );
        let fields_value = Value::Array(Arc::new(
            field_names
                .iter()
                .map(|name| Value::String(SmolStr::from(name.clone())))
                .collect(),
        ));
        let fields_idx = self.const_idx(ConstKey::FieldNames(field_names.clone()), fields_value);
        let dst_v = self.alloc_reg();
        let wide_idx = u16::try_from(self.wide_ops.len()).expect("wide_ops index overflow");
        self.wide_ops
            .push(crate::bytecode::WideOp::BuildFloatArrayFromStructs {
                dst_v,
                first_v,
                elem_count,
                name_idx,
                fields_idx,
            });
        self.emit(Op::Wide { idx: wide_idx });
        self.flat_locals.insert(dst_v, stride);
        Ok(Some(TypedReg {
            reg: dst_v,
            kind: RegKind::Value,
        }))
    }

    /// Lowers a numeric `Vec::new()` or `Vec::with_capacity(n)` to a flat empty `IntArray` /
    /// `FloatVec` (8 bytes per element) instead of the boxed
    /// `Value::Array` the generic `Vec::new` builtin returns. The local
    /// is tagged flat so a following `push` loop grows the flat backing
    /// store in place. The VM has historically treated capacity as a hint,
    /// but still evaluates the capacity expression exactly once.
    pub(crate) fn try_build_empty_typed_vec(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
        result_ty: Ty,
    ) -> RuntimeResult<Option<TypedReg>> {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return Ok(None);
        };
        let n = segments.len();
        if n < 2 || segments[n - 2].name.as_str() != "Vec" {
            return Ok(None);
        }
        let constructor = segments[n - 1].name.as_str();
        if !((constructor == "new" && args.is_empty())
            || (constructor == "with_capacity" && args.len() == 1))
        {
            return Ok(None);
        }
        let Some(elem) = self.array_elem_ty(result_ty) else {
            return Ok(None);
        };
        if constructor == "with_capacity" {
            let capacity = self.compile_expr_ex(&args[0])?;
            let capacity_i = self.as_i64(capacity);
            self.emit(Op::CheckNonNegativeCapacity { capacity_i });
        }
        let dst = self.alloc_reg();
        match self.tcx.kind(elem) {
            Some(TyKind::Int(IntTy::U8)) => {
                self.emit(Op::BuildByteArray {
                    dst_v: dst,
                    first_i: 0,
                    count: 0,
                });
                self.flat_int_locals.insert(dst);
            }
            Some(TyKind::Int(IntTy::I64 | IntTy::Isize | IntTy::Usize)) => {
                self.emit(Op::BuildIntArray {
                    dst_v: dst,
                    first_i: 0,
                    count: 0,
                });
                self.flat_int_locals.insert(dst);
            }
            Some(TyKind::Float(FloatTy::F64)) => {
                self.emit(Op::BuildFloatVec {
                    dst_v: dst,
                    first_f: 0,
                    count: 0,
                });
                self.flat_float_locals.insert(dst);
            }
            _ => return Ok(None),
        }
        Ok(Some(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        }))
    }

    /// Mirror of [`Self::try_build_float_array`] for the
    /// primitive `[i64; N]` shape. When the literal's element
    /// type is `i64` we emit `Op::BuildIntArray` (writing into
    /// the typed `i64` register file) instead of the
    /// general-purpose boxed-`Value::Array<Value::Int>` form.
    /// fasta's TWO/THREE inner loops index two such arrays
    /// (`iub_cut`, `iub_ch`) several times per output byte;
    /// keeping their storage as raw `Vec<i64>` lets
    /// [`Op::IntArrayGetI64`] feed the typed `i64` registers
    /// directly.
    pub(crate) fn try_build_int_array(
        &mut self,
        array_ty: Ty,
        elems: &[HirExpr],
    ) -> RuntimeResult<Option<TypedReg>> {
        let elem_kind = self
            .array_elem_ty(array_ty)
            .and_then(|elem| self.tcx.kind(elem));
        if !matches!(
            elem_kind,
            Some(TyKind::Int(
                IntTy::U8 | IntTy::I64 | IntTy::Isize | IntTy::Usize
            ))
        ) {
            return Ok(None);
        }
        let Ok(count) = u16::try_from(elems.len()) else {
            return Ok(None);
        };
        // Allocate `count` contiguous i64 registers. `compile_expr_ex`
        // on each element returns a TypedReg; we coerce to i64 via
        // `as_i64`.
        let first_i = self.next_int_reg;
        if u32::from(count) > u32::from(u16::MAX - first_i) {
            return Ok(None);
        }
        self.next_int_reg = first_i + count;
        for (i, elem) in elems.iter().enumerate() {
            let target = first_i + u16::try_from(i).expect("count overflow");
            let tr = self.compile_expr_ex(elem)?;
            let src_i = self.as_i64(tr);
            self.emit(Op::MoveI64 {
                dst_i: target,
                src_i,
            });
        }
        let dst = self.alloc_reg();
        if matches!(elem_kind, Some(TyKind::Int(IntTy::U8))) {
            self.emit(Op::BuildByteArray {
                dst_v: dst,
                first_i,
                count,
            });
        } else {
            self.emit(Op::BuildIntArray {
                dst_v: dst,
                first_i,
                count,
            });
        }
        // Track for the indexing fast path so subsequent
        // `arr[k]` reads route through `Op::IntArrayGetI64`.
        self.flat_int_locals.insert(dst);
        Ok(Some(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        }))
    }

    /// Repeat-form variant of [`Self::try_build_float_vec`] for
    /// `[value; count]` shapes. Evaluates `value` once, then lets
    /// `Op::BuildArrayRepeat` materialise the flat `FloatVec` at
    /// runtime.
    ///
    /// Do not expand this into one float register per element. A fixed
    /// `[0.0; 40000]` scratch buffer otherwise emits 40k `MoveF64` ops and
    /// forces validator/dataflow state into gigabytes before the program
    /// starts running.
    pub(crate) fn try_build_float_vec_repeat(
        &mut self,
        array_ty: Ty,
        value: &HirExpr,
        count: &HirExpr,
    ) -> RuntimeResult<Option<TypedReg>> {
        if !is_array_elem_kind(self.tcx, array_ty, Some(value.ty), |k| {
            matches!(k, TyKind::Float(FloatTy::F64))
        }) {
            return Ok(None);
        }
        let src_tr = self.compile_expr_ex(value)?;
        let value_reg = self.as_value(src_tr);
        let count_reg = self.compile_expr(count)?;
        let dst = self.alloc_reg();
        self.emit(Op::BuildArrayRepeat {
            dst,
            value: value_reg,
            count: count_reg,
        });
        self.flat_float_locals.insert(dst);
        Ok(Some(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        }))
    }

    /// Repeat-form mirror of [`Self::try_build_int_array`] for
    /// `[value; count]` `[i64]` literals - used by integer scratch
    /// buffers initialised at function entry.
    pub(crate) fn try_build_int_array_repeat(
        &mut self,
        array_ty: Ty,
        value: &HirExpr,
        count: &HirExpr,
    ) -> RuntimeResult<Option<TypedReg>> {
        let elem_kind = self
            .array_elem_ty(array_ty)
            .and_then(|elem| self.tcx.kind(elem));
        if !matches!(
            elem_kind,
            Some(TyKind::Int(
                IntTy::U8 | IntTy::I64 | IntTy::Isize | IntTy::Usize
            ))
        ) {
            return Ok(None);
        }
        let src_tr = self.compile_expr_ex(value)?;
        let dst = self.alloc_reg();
        if matches!(elem_kind, Some(TyKind::Int(IntTy::U8))) {
            let value_i = self.as_i64(src_tr);
            let count_v = self.compile_expr(count)?;
            self.emit(Op::BuildByteArrayRepeat {
                dst_v: dst,
                value_i,
                count_v,
            });
        } else {
            let value_reg = self.as_value(src_tr);
            let count_reg = self.compile_expr(count)?;
            self.emit(Op::BuildArrayRepeat {
                dst,
                value: value_reg,
                count: count_reg,
            });
        }
        self.flat_int_locals.insert(dst);
        Ok(Some(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        }))
    }

    /// Mirror of [`Self::try_build_int_array`] for `[f64; N]`
    /// literals. Compiles each element into a contiguous f64
    /// register span and emits [`Op::BuildFloatVec`], which wraps
    /// the span into a `Value::FloatVec`. Subsequent indexed reads
    /// / writes route through [`Op::FloatVecGetF64`] and
    /// [`Op::FloatVecSetF64`] so each element load lands directly
    /// in the typed-`f64` register file.
    pub(crate) fn try_build_float_vec(
        &mut self,
        array_ty: Ty,
        elems: &[HirExpr],
    ) -> RuntimeResult<Option<TypedReg>> {
        if !is_array_elem_kind(self.tcx, array_ty, elems.first().map(|e| e.ty), |k| {
            matches!(k, TyKind::Float(FloatTy::F64))
        }) {
            return Ok(None);
        }
        let Ok(count) = u16::try_from(elems.len()) else {
            return Ok(None);
        };
        let first_f = self.next_float_reg;
        if u32::from(count) > u32::from(u16::MAX - first_f) {
            return Ok(None);
        }
        self.next_float_reg = first_f + count;
        for (i, elem) in elems.iter().enumerate() {
            let target = first_f + u16::try_from(i).expect("count overflow");
            let tr = self.compile_expr_ex(elem)?;
            let src_f = self.as_f64(tr);
            self.emit(Op::MoveF64 {
                dst_f: target,
                src_f,
            });
        }
        let dst = self.alloc_reg();
        self.emit(Op::BuildFloatVec {
            dst_v: dst,
            first_f,
            count,
        });
        self.flat_float_locals.insert(dst);
        Ok(Some(TypedReg {
            reg: dst,
            kind: RegKind::Value,
        }))
    }

    /// Recognise pure single-arg f64 math intrinsics
    /// (`math::sqrt`, `math::sin`, …) and emit the dedicated
    /// typed opcode instead of going through `Op::Call`.
    /// Both the bare and `math::` spellings are accepted to
    /// match the stdlib's dual registration.
    pub(crate) fn try_intrinsic_call(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
    ) -> RuntimeResult<Option<TypedReg>> {
        if args.len() != 1 {
            return Ok(None);
        }
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return Ok(None);
        };
        let full: Vec<String> = segments.iter().map(|s| s.name.clone()).collect();
        // If the callee is a single-segment user function that
        // the prepass flagged as a trivial wrapper around an
        // intrinsic (`fn f(x) { math::sqrt(x) }`), redirect to
        // the intrinsic's path and inline directly.
        let effective_segs = if full.len() == 1 {
            match self.wrappers.get(&full[0]) {
                Some(target) => target.clone(),
                None => full.clone(),
            }
        } else {
            full.clone()
        };
        let segs_str: Vec<&str> = effective_segs
            .iter()
            .map(std::string::String::as_str)
            .collect();
        let kind = match segs_str.as_slice() {
            ["math", "sqrt"] | ["sqrt"] => "sqrt",
            ["math", "sin"] | ["sin"] => "sin",
            ["math", "cos"] | ["cos"] => "cos",
            ["math", "abs"] | ["abs"] => "abs",
            ["math", "floor"] | ["floor"] => "floor",
            ["math", "ceil"] | ["ceil"] => "ceil",
            ["math", "exp"] | ["exp"] => "exp",
            ["math", "ln" | "log"] | ["ln"] => "ln",
            _ => return Ok(None),
        };
        if self.expr_kind(&args[0]) != RegKind::F64 {
            return Ok(None);
        }
        let arg_tr = self.compile_expr_ex(&args[0])?;
        let src_f = self.as_f64(arg_tr);
        let dst = self.alloc_float();
        let op = match kind {
            "sqrt" => Op::SqrtF64 { dst_f: dst, src_f },
            "sin" => Op::SinF64 { dst_f: dst, src_f },
            "cos" => Op::CosF64 { dst_f: dst, src_f },
            "abs" => Op::AbsF64 { dst_f: dst, src_f },
            "floor" => Op::FloorF64 { dst_f: dst, src_f },
            "ceil" => Op::CeilF64 { dst_f: dst, src_f },
            "exp" => Op::ExpF64 { dst_f: dst, src_f },
            "ln" => Op::LnF64 { dst_f: dst, src_f },
            _ => unreachable!(),
        };
        self.emit(op);
        Ok(Some(TypedReg {
            reg: dst,
            kind: RegKind::F64,
        }))
    }

    /// In-place-mutation method names. Methods here have a "returns the
    /// new aggregate" builtin whose result the VM has to thread back
    /// into the receiver's slot.
    pub(crate) fn is_mutating_method_name(name: &str) -> bool {
        gossamer_types::is_mutating_method_name(name)
    }

    /// Routes the typed-`HashMap<i64, i64>` method-call surface
    /// through dedicated typed ops. Returns `Some(reg)` when the
    /// method is handled here; the caller falls through to the
    /// generic dispatch otherwise.
    pub(crate) fn try_compile_int_map_method(
        &mut self,
        receiver: &HirExpr,
        method: &str,
        args: &[HirExpr],
    ) -> RuntimeResult<Option<Reg>> {
        match (method, args.len()) {
            ("insert", 2) => {
                let map_reg = self.compile_expr(receiver)?;
                let key_tr = self.compile_expr_ex(&args[0])?;
                let key_i = self.as_i64(key_tr);
                let val_tr = self.compile_expr_ex(&args[1])?;
                let val_i = self.as_i64(val_tr);
                let dst = self.alloc_reg();
                self.emit(Op::IntMapInsert {
                    dst_v: dst,
                    map_reg,
                    key_i,
                    value_i: val_i,
                });
                Ok(Some(dst))
            }
            ("get_or", 2) => {
                let map_reg = self.compile_expr(receiver)?;
                let key_tr = self.compile_expr_ex(&args[0])?;
                let key_i = self.as_i64(key_tr);
                let def_tr = self.compile_expr_ex(&args[1])?;
                let def_i = self.as_i64(def_tr);
                let dst_i = self.alloc_int();
                self.emit(Op::IntMapGetOr {
                    dst_i,
                    map_reg,
                    key_i,
                    default_i: def_i,
                });
                let dst = self.alloc_reg();
                self.emit(Op::BoxI64 {
                    dst_v: dst,
                    src_i: dst_i,
                });
                Ok(Some(dst))
            }
            ("len", 0) => {
                let map_reg = self.compile_expr(receiver)?;
                let dst_i = self.alloc_int();
                self.emit(Op::IntMapLen { dst_i, map_reg });
                let dst = self.alloc_reg();
                self.emit(Op::BoxI64 {
                    dst_v: dst,
                    src_i: dst_i,
                });
                Ok(Some(dst))
            }
            ("contains_key", 1) => {
                let map_reg = self.compile_expr(receiver)?;
                let key_tr = self.compile_expr_ex(&args[0])?;
                let key_i = self.as_i64(key_tr);
                let dst = self.alloc_reg();
                self.emit(Op::IntMapContainsKey {
                    dst_v: dst,
                    map_reg,
                    key_i,
                });
                Ok(Some(dst))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn ensure_reg_slot(&mut self, slot: Reg) {
        if slot >= self.next_reg {
            self.next_reg = slot.checked_add(1).expect("register overflow");
        }
    }

    /// Hoists literal-and-local comparison operands out of
    /// `while` loops so the compare operands are evaluated
    /// once up front rather than per iteration. Returns
    /// `(lhs_reg, rhs_reg, op, kind)` when the condition
    /// has a hoistable shape - specifically
    /// `Path(local) <op> Literal` or `Literal <op> Path(local)`
    /// over typed numeric kinds.
    pub(crate) fn try_hoist_condition_literals(
        &mut self,
        condition: &HirExpr,
    ) -> RuntimeResult<Option<(Reg, Reg, HirBinaryOp, RegKind)>> {
        let HirExprKind::Binary { op, lhs, rhs } = &condition.kind else {
            return Ok(None);
        };
        if !matches!(
            op,
            HirBinaryOp::Lt | HirBinaryOp::Le | HirBinaryOp::Gt | HirBinaryOp::Ge
        ) {
            return Ok(None);
        }
        let lk = self.expr_kind(lhs);
        let rk = self.expr_kind(rhs);
        if lk != rk || lk == RegKind::Value {
            return Ok(None);
        }
        // The fused branch ops are signed-only; an unsigned-64 operand
        // must fall back to the unsigned compare + `BranchIfNot` path.
        if self.is_unsigned64_ty(lhs.ty) || self.is_unsigned64_ty(rhs.ty) {
            return Ok(None);
        }
        // Hoist only when neither operand would require an
        // `Unbox*` at evaluation: that would snapshot a
        // `Value::Int` local into a typed int reg once,
        // and subsequent writes back through the `Value`
        // reg wouldn't update it. Safe cases:
        //   * typed literals - always produce a typed reg
        //   * locals whose stored `TypedReg` already matches
        //     the operand kind - reads update through the
        //     same typed reg the compare uses.
        if !self.is_hoistable_operand(lhs, lk) {
            return Ok(None);
        }
        if !self.is_hoistable_operand(rhs, lk) {
            return Ok(None);
        }
        let lhs_tr = self.compile_expr_ex(lhs)?;
        let rhs_tr = self.compile_expr_ex(rhs)?;
        let (lhs_reg, rhs_reg) = match lk {
            RegKind::I64 => (self.as_i64(lhs_tr), self.as_i64(rhs_tr)),
            RegKind::F64 => (self.as_f64(lhs_tr), self.as_f64(rhs_tr)),
            RegKind::Value => unreachable!(),
        };
        Ok(Some((lhs_reg, rhs_reg, *op, lk)))
    }

    /// Returns `true` when `expr`'s operand register can be
    /// pre-computed before a loop body without going stale.
    /// Typed literals qualify (their reg is write-once), as do
    /// locals already bound in the matching typed register
    /// file. Anything else - most importantly a local bound as
    /// `Value` that would need an `Unbox*` snapshot - is
    /// rejected so the fused branch re-emits it each iteration.
    pub(crate) fn is_hoistable_operand(&self, expr: &HirExpr, kind: RegKind) -> bool {
        match &expr.kind {
            HirExprKind::Literal(_) => true,
            HirExprKind::Path { segments, .. } if segments.len() == 1 => {
                match self.lookup_local(&segments[0].name) {
                    Some(tr) => tr.kind == kind,
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// Emits the inverted compare-and-branch op that exits a
    /// loop when `lhs <op> rhs` is false. Callers have already
    /// computed the operand registers.
    pub(crate) fn emit_fused_exit_branch(
        &mut self,
        op: HirBinaryOp,
        kind: RegKind,
        lhs_reg: Reg,
        rhs_reg: Reg,
    ) -> InstrIdx {
        let op_emit = match (kind, op) {
            (RegKind::I64, HirBinaryOp::Lt) => Op::BranchIfGeI64 {
                lhs_i: lhs_reg,
                rhs_i: rhs_reg,
                target: 0,
            },
            (RegKind::I64, HirBinaryOp::Le) => Op::BranchIfLtI64 {
                lhs_i: rhs_reg,
                rhs_i: lhs_reg,
                target: 0,
            },
            (RegKind::I64, HirBinaryOp::Gt) => Op::BranchIfGeI64 {
                lhs_i: rhs_reg,
                rhs_i: lhs_reg,
                target: 0,
            },
            (RegKind::I64, HirBinaryOp::Ge) => Op::BranchIfLtI64 {
                lhs_i: lhs_reg,
                rhs_i: rhs_reg,
                target: 0,
            },
            (RegKind::F64, HirBinaryOp::Lt) => Op::BranchIfGeF64 {
                lhs_f: lhs_reg,
                rhs_f: rhs_reg,
                target: 0,
            },
            (RegKind::F64, HirBinaryOp::Le) => Op::BranchIfLtF64 {
                lhs_f: rhs_reg,
                rhs_f: lhs_reg,
                target: 0,
            },
            (RegKind::F64, HirBinaryOp::Gt) => Op::BranchIfGeF64 {
                lhs_f: rhs_reg,
                rhs_f: lhs_reg,
                target: 0,
            },
            (RegKind::F64, HirBinaryOp::Ge) => Op::BranchIfLtF64 {
                lhs_f: lhs_reg,
                rhs_f: rhs_reg,
                target: 0,
            },
            _ => unreachable!(),
        };
        self.emit(op_emit)
    }

    /// Recognises `while lhs <op> rhs { ... }` where `lhs` and
    /// `rhs` share a concrete numeric kind and emits a fused
    /// "branch to loop exit when the inverted predicate holds"
    /// op. Returns the patch index so the caller can fix up
    /// the target once the loop-end address is known.
    pub(crate) fn try_compile_fused_exit_branch(
        &mut self,
        condition: &HirExpr,
    ) -> RuntimeResult<Option<InstrIdx>> {
        let HirExprKind::Binary { op, lhs, rhs } = &condition.kind else {
            return Ok(None);
        };
        let lk = self.expr_kind(lhs);
        let rk = self.expr_kind(rhs);
        if lk != rk || lk == RegKind::Value {
            return Ok(None);
        }
        // The fused branch ops are signed-only; an unsigned-64 operand
        // must fall back to the unsigned compare + `BranchIfNot` path.
        if self.is_unsigned64_ty(lhs.ty) || self.is_unsigned64_ty(rhs.ty) {
            return Ok(None);
        }
        // Check supported op kinds BEFORE compiling operands -
        // otherwise we'd emit dead operand-evaluation ops when
        // the comparison falls back to the generic path.
        if !matches!(
            op,
            HirBinaryOp::Lt | HirBinaryOp::Le | HirBinaryOp::Gt | HirBinaryOp::Ge
        ) {
            return Ok(None);
        }
        if lk == RegKind::I64 {
            let lhs_tr = self.compile_expr_ex(lhs)?;
            let rhs_tr = self.compile_expr_ex(rhs)?;
            let lhs_i = self.as_i64(lhs_tr);
            let rhs_i = self.as_i64(rhs_tr);
            // Fire when the predicate is FALSE (the loop
            // wants to exit). `while lhs < rhs` → exit when
            // `lhs >= rhs`, etc.
            //   < → Ge(lhs, rhs)
            //   <= → Lt(rhs, lhs)      [NOT (lhs <= rhs) ⟺ rhs < lhs]
            //   > → Ge(rhs, lhs)       [NOT (lhs > rhs) ⟺ rhs >= lhs]
            //   >= → Lt(lhs, rhs)
            let op_emit = match op {
                HirBinaryOp::Lt => Op::BranchIfGeI64 {
                    lhs_i,
                    rhs_i,
                    target: 0,
                },
                HirBinaryOp::Le => Op::BranchIfLtI64 {
                    lhs_i: rhs_i,
                    rhs_i: lhs_i,
                    target: 0,
                },
                HirBinaryOp::Gt => Op::BranchIfGeI64 {
                    lhs_i: rhs_i,
                    rhs_i: lhs_i,
                    target: 0,
                },
                HirBinaryOp::Ge => Op::BranchIfLtI64 {
                    lhs_i,
                    rhs_i,
                    target: 0,
                },
                _ => unreachable!(),
            };
            return Ok(Some(self.emit(op_emit)));
        }
        if lk == RegKind::F64 {
            let lhs_tr = self.compile_expr_ex(lhs)?;
            let rhs_tr = self.compile_expr_ex(rhs)?;
            let lhs_f = self.as_f64(lhs_tr);
            let rhs_f = self.as_f64(rhs_tr);
            let op_emit = match op {
                HirBinaryOp::Lt => Op::BranchIfGeF64 {
                    lhs_f,
                    rhs_f,
                    target: 0,
                },
                HirBinaryOp::Le => Op::BranchIfLtF64 {
                    lhs_f: rhs_f,
                    rhs_f: lhs_f,
                    target: 0,
                },
                HirBinaryOp::Gt => Op::BranchIfGeF64 {
                    lhs_f: rhs_f,
                    rhs_f: lhs_f,
                    target: 0,
                },
                HirBinaryOp::Ge => Op::BranchIfLtF64 {
                    lhs_f,
                    rhs_f,
                    target: 0,
                },
                _ => unreachable!(),
            };
            return Ok(Some(self.emit(op_emit)));
        }
        Ok(None)
    }

    /// `for var in start..end { body }`, `for _ in start..end { body }`,
    /// and the inclusive variant `for var in start..=end { body }`.
    ///
    /// HIR lowers a `for` into the generic `loop { match iter.next() { Some(p)
    /// => body, None => break } }` shape so every iterable goes through
    /// the same machinery. For an integer range that path costs an
    /// `Option` allocation + a match dispatch *per iteration* - on
    /// nbody's nested `for a in 0..4 { for b in (a+1)..5 { ... } }` the
    /// overhead dominated everything inside.
    ///
    /// We pattern-match the desugar back out and emit a typed-i64
    /// counter loop:
    ///
    /// ```text
    ///     start_i = <start>
    ///     end_i   = <end>
    /// header:
    ///     if start_i >= end_i goto exit       (exclusive: BranchIfGeI64)
    ///     if start_i >  end_i goto exit       (inclusive: BranchIfGtI64)
    ///     <body>                              (binding sees var via I64 reg)
    ///     start_i = start_i + 1               (AddI64)
    ///     jump header
    /// exit:
    /// ```
    ///
    /// Falls through to the generic match-loop on:
    ///   - non-i64 range bounds (the typed file is i64-only; f64-step
    ///     ranges would need a separate fast path nobody currently writes)
    ///   - non-`Range` iterators with non-trivial state - those still
    ///     go through `next()`.
    pub(crate) fn try_compile_for_loop_range(
        &mut self,
        body: &HirExpr,
    ) -> RuntimeResult<Option<Reg>> {
        let HirExprKind::Block(block) = &body.kind else {
            return Ok(None);
        };
        if !block.stmts.is_empty() {
            return Ok(None);
        }
        let Some(tail) = block.tail.as_deref() else {
            return Ok(None);
        };
        let HirExprKind::Match { scrutinee, arms } = &tail.kind else {
            return Ok(None);
        };
        if arms.len() != 2 {
            return Ok(None);
        }
        let HirExprKind::MethodCall {
            receiver,
            name,
            args,
            owner: None,
        } = &scrutinee.kind
        else {
            return Ok(None);
        };
        if name.name != "next" || !args.is_empty() {
            return Ok(None);
        }
        let HirExprKind::Range {
            start: Some(start),
            end,
            inclusive,
        } = &receiver.kind
        else {
            return Ok(None);
        };
        if *inclusive && end.is_none() {
            return Ok(None);
        }
        // A range's bounds are integers by typecheck. Accept a bound
        // whose static kind is `i64` (the common case, driven by a typed
        // counter) or `Value` (an unresolved-typed bound such as
        // `0..xs.len()` where `len()`'s result stayed an inference var) -
        // `as_i64` unboxes the runtime `Value::Int`. Only a statically
        // float bound is rejected (no valid for-range produces one), so
        // it falls through to the general inline-iterable materialiser
        // rather than miscompiling.
        if self.expr_kind(start) == RegKind::F64
            || end
                .as_deref()
                .is_some_and(|end| self.expr_kind(end) == RegKind::F64)
        {
            return Ok(None);
        }
        let some_arm = &arms[0];
        let none_arm = &arms[1];
        let HirPatKind::Variant {
            name: some_name,
            fields: some_fields,
        } = &some_arm.pattern.kind
        else {
            return Ok(None);
        };
        if some_name.name != "Some" || some_fields.len() != 1 {
            return Ok(None);
        }
        let HirPatKind::Variant {
            name: none_name,
            fields: none_fields,
        } = &none_arm.pattern.kind
        else {
            return Ok(None);
        };
        if none_name.name != "None" || !none_fields.is_empty() {
            return Ok(None);
        }
        // Bind the Some-arm's pattern (an ident or `_`) so the body
        // can read it. The body is some_arm.body in the desugar.
        let loop_var = match &some_fields[0].kind {
            HirPatKind::Binding { name, .. } => Some(name.name.clone()),
            HirPatKind::Wildcard => None,
            _ => return Ok(None),
        };
        let inclusive = *inclusive;

        let start_tr = self.compile_expr_ex(start)?;
        let start_i = self.as_i64(start_tr);
        // The loop owns its counter. `as_i64` hands back the start
        // expression's own register when that expression is already a typed
        // i64 local, and the fused increment below writes through it, so a
        // bound named by a variable has to be copied into a register the loop
        // is free to advance.
        if self.next_int_reg == u16::MAX {
            return Ok(None);
        }
        let counter_i = self.next_int_reg;
        self.next_int_reg += 1;
        self.emit(Op::MoveI64 {
            dst_i: counter_i,
            src_i: start_i,
        });
        let end_i = if let Some(end) = end {
            let end_tr = self.compile_expr_ex(end)?;
            Some(self.as_i64(end_tr))
        } else {
            None
        };

        // A `for` expression without an explicit value-bearing `break`
        // evaluates to unit. Initialise the result before entering the loop
        // so an empty loop and normal exhaustion cannot expose a stale
        // register value to the REPL.
        let result = self.load_unit();
        self.push_scope();
        if let Some(name) = &loop_var {
            self.bind_local(
                name,
                TypedReg {
                    reg: counter_i,
                    kind: RegKind::I64,
                },
            );
        }

        // Layout: a header bounds-check (paid on loop entry only) +
        // the body + a fused `IncJumpIfLt(target=header+1)` at the
        // bottom that combines the per-iter AddI64 + bounds re-test +
        // Jump into one dispatch whose back-edge lands on the first
        // body instruction. The fall-through case after the fused op
        // (counter has reached end) lands directly on the post-loop
        // block.
        let header = self.cur_idx();
        let exit_branch_idx = end_i.map(|end_i| {
            self.emit(if inclusive {
                Op::BranchIfGtI64 {
                    lhs_i: counter_i,
                    rhs_i: end_i,
                    target: 0,
                }
            } else {
                Op::BranchIfGeI64 {
                    lhs_i: counter_i,
                    rhs_i: end_i,
                    target: 0,
                }
            })
        });

        self.loop_stack.push(LoopCtx {
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            result_reg: result,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        self.compile_loop_body(&some_arm.body)?;
        // `continue` jumps here: the same fused inc-and-test op
        // the body's bottom fall-through executes. Routing
        // `continue` directly to `header` would re-test the
        // bound without advancing the counter and livelock the
        // loop. The back-edge targets the first body instruction
        // (`header + 1`): the fused op already performed the
        // equivalent bounds test, so intermediate iterations skip
        // the header check and only loop entry pays it.
        let continue_target = self.cur_idx();
        if let Some(end_i) = end_i {
            self.emit(if inclusive {
                Op::IncJumpIfLeI64 {
                    counter_i,
                    end_i,
                    target: header + 1,
                }
            } else {
                Op::IncJumpIfLtI64 {
                    counter_i,
                    end_i,
                    target: header + 1,
                }
            });
        } else {
            self.emit(Op::ArithImmI64 {
                kind: ImmArithKind::Add,
                dst_i: counter_i,
                lhs_i: counter_i,
                imm: 1,
            });
            self.emit(Op::Jump { target: header });
        }
        let after = self.cur_idx();
        if let Some(exit_branch_idx) = exit_branch_idx {
            self.patch_jump(exit_branch_idx, after);
        }
        let ctx = self
            .loop_stack
            .pop()
            .expect("loop stack underflow on for-range");
        for patch in ctx.break_patches {
            self.patch_jump(patch, after);
        }
        for patch in ctx.continue_patches {
            self.patch_jump(patch, continue_target);
        }
        self.pop_scope();
        Ok(Some(result))
    }

    /// `for x in v.iter() { body }` and `for (i, x) in v.iter().enumerate() { body }`.
    ///
    /// Emits a typed counter loop that dereferences `v[counter]` via
    /// `Op::IndexGet`, sidestepping the per-iteration `Option`
    /// allocation + match dispatch the generic iterator path would
    /// pay. The iterator's length is captured once at loop entry
    /// from `v.len()`; the body sees the element binding through a
    /// regular Value register.
    ///
    /// Falls through to the generic match-loop when the receiver
    /// shape isn't a recognised `MethodCall` chain (e.g. a stateful
    /// custom iterator, a `HashMap::keys` view, or a chained
    /// `filter`/`map` whose bytecode shape isn't fixed).
    pub(crate) fn try_compile_for_loop_vec_iter(
        &mut self,
        body: &HirExpr,
    ) -> RuntimeResult<Option<Reg>> {
        let HirExprKind::Block(block) = &body.kind else {
            return Ok(None);
        };
        if !block.stmts.is_empty() {
            return Ok(None);
        }
        let Some(tail) = block.tail.as_deref() else {
            return Ok(None);
        };
        let HirExprKind::Match { scrutinee, arms } = &tail.kind else {
            return Ok(None);
        };
        if arms.len() != 2 {
            return Ok(None);
        }
        let HirExprKind::MethodCall {
            receiver: next_recv,
            name: next_name,
            args: next_args,
            owner: None,
        } = &scrutinee.kind
        else {
            return Ok(None);
        };
        if next_name.name != "next" || !next_args.is_empty() {
            return Ok(None);
        }
        // A range or adapter stored in a binding is a real lazy Iterator
        // state, not an indexable collection. Let generic method dispatch
        // call `Iterator::next`; treating it as a Vec gives it length zero
        // and silently skips the loop.
        let collection_iter_method = matches!(
            &next_recv.kind,
            HirExprKind::MethodCall { name, args, .. }
                if name.name == "iter" && args.is_empty()
        );
        let concrete_enumerate_method = matches!(
            &next_recv.kind,
            HirExprKind::MethodCall { receiver, name, args, owner: None }
                if name.name == "enumerate"
                    && args.is_empty()
                    && matches!(
                        &receiver.kind,
                        HirExprKind::MethodCall { name, args, .. }
                            if name.name == "iter" && args.is_empty()
                    )
        );
        // A range, or an adapter chain over one, carries live iterator state
        // instead of an indexable buffer, and its `next()` receiver is an
        // rvalue the generic loop re-evaluates on every pull. Snapshot it
        // once below and drive the snapshot by index - the shape the
        // compiled tiers lower to. A stateful `impl Iterator` receiver
        // (`(&mut __for_iter).next()`) keeps the `next()` protocol.
        let lazy_source = self.receiver_is_lazy_iterator(next_recv)
            && !collection_iter_method
            && !concrete_enumerate_method
            && !matches!(
                &next_recv.kind,
                HirExprKind::Unary {
                    op: HirUnaryOp::RefMut,
                    ..
                }
            );
        // Walk the iterator chain. Recognise:
        //   `vec.iter()`              → element binding, no enumerate
        //   `vec.iter().enumerate()`  → tuple binding (i, x)
        //   `vec` (plain collection)  → element binding, no enumerate
        // The plain shape is the `for x in xs` desugar, where the
        // receiver of `.next()` is the collection itself (no `.iter()`).
        // It is accepted only when the receiver's type is an
        // array / vec / slice the index-walk below can drive - a user
        // `impl Iterator` (Adt receiver) falls through to `None` so the
        // stateful `.next()` desugar keeps its own handling.
        let (vec_expr, is_enumerate, write_back_elem) = match &next_recv.kind {
            // A lazy pipeline yields its elements (pairs included) straight
            // from the snapshot, so it is driven as a plain sequence: the
            // `enumerate` index rides in the tuple the snapshot holds.
            _ if lazy_source => (next_recv.as_ref(), false, false),
            HirExprKind::MethodCall {
                receiver: chain_recv,
                name: chain_name,
                args: chain_args,
                owner: None,
            } if chain_name.name == "iter" && chain_args.is_empty() => {
                (chain_recv.as_ref(), false, false)
            }
            HirExprKind::MethodCall {
                receiver: enum_recv,
                name: enum_name,
                args: enum_args,
                owner: None,
            } if enum_name.name == "enumerate" && enum_args.is_empty() => {
                // `xs.iter().enumerate()` and the `.iter()`-less
                // `xs.enumerate()` walk the same collection; both drive
                // by index from `xs`. The generic loop would re-evaluate
                // the receiver on each `next()` pull, and `enumerate`
                // materialises a fresh sequence per call, so its walk
                // would restart at index zero forever.
                match &enum_recv.kind {
                    HirExprKind::MethodCall {
                        receiver: chain_recv,
                        name: chain_name,
                        args: chain_args,
                        owner: None,
                    } if chain_name.name == "iter" && chain_args.is_empty() => {
                        (chain_recv.as_ref(), true, false)
                    }
                    _ if self.receiver_is_collection(enum_recv) => {
                        (enum_recv.as_ref(), true, false)
                    }
                    _ => return Ok(None),
                }
            }
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand,
            } if self.receiver_is_collection(operand) => {
                // The `&mut` a user wrote names a sequence the body's stores
                // reach through, so each element is written back. The one the
                // `for` desugar mints over a temporary names its own state
                // binding: nothing observes that snapshot, and the body is
                // free to move the element out of its register, which the
                // write-back would then store over the element it read.
                let write_back = !Self::is_for_iter_state_path(operand);
                (operand.as_ref(), false, write_back)
            }
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                ..
            } => return Ok(None),
            // A `&mut [T]` / `&mut Vec<T>` binding iterated without a fresh
            // `&mut` in the header: the elements are still written through
            // to the source the reference names.
            _ if self.expr_is_mut_ref_collection(next_recv) => (next_recv.as_ref(), false, true),
            // Any other inline `for`-desugar receiver: a plain collection
            // (`for x in xs`), a method-result collection (`for (k, v) in
            // map.iter()`, `map.keys()`, `text.chars()`,
            // `text.as_bytes()`), a free-function iterator
            // (`iter::enumerate(xs)`), or a range value the typed
            // for-range path declined. Each evaluates to an indexable
            // `Value::Array` at runtime, so it materialises once and
            // drives by index. The stateful custom-iterator desugar is
            // excluded - its `.next()` receiver is a `&mut __for_iter`
            // borrow, driven by the generic loop emitter's `&mut self`
            // write-back instead - so it falls through to `compile_loop`.
            _ => (next_recv.as_ref(), false, false),
        };

        let some_arm = &arms[0];
        let none_arm = &arms[1];
        let HirPatKind::Variant {
            name: some_name,
            fields: some_fields,
        } = &some_arm.pattern.kind
        else {
            return Ok(None);
        };
        if some_name.name != "Some" || some_fields.len() != 1 {
            return Ok(None);
        }
        let HirPatKind::Variant {
            name: none_name,
            fields: none_fields,
        } = &none_arm.pattern.kind
        else {
            return Ok(None);
        };
        if none_name.name != "None" || !none_fields.is_empty() {
            return Ok(None);
        }
        let elem_pat = &some_fields[0];
        // Pattern shapes we accept:
        //   non-enumerate: Binding (or Wildcard)
        //   enumerate:     Tuple of two bindings (or wildcards)
        let (elem_binding, idx_binding): (Option<String>, Option<String>) = if is_enumerate {
            let HirPatKind::Tuple(parts) = &elem_pat.kind else {
                return Ok(None);
            };
            if parts.len() != 2 {
                return Ok(None);
            }
            let pick = |p: &HirPat| match &p.kind {
                HirPatKind::Binding { name, .. } => Some(Some(name.name.clone())),
                HirPatKind::Wildcard => Some(None),
                _ => None,
            };
            let i_b = pick(&parts[0]).ok_or(RuntimeError::Unsupported(
                "enumerate: index pat must be ident/_",
            ))?;
            let x_b = pick(&parts[1]).ok_or(RuntimeError::Unsupported(
                "enumerate: elem pat must be ident/_",
            ))?;
            (x_b, i_b)
        } else {
            let pick = match &elem_pat.kind {
                HirPatKind::Binding { name, .. } => Some(name.name.clone()),
                HirPatKind::Wildcard => None,
                // A destructuring element pattern (`for (a, b) in xs`) is
                // bound after the per-iteration `IndexGet` below; it has no
                // single pre-loop name. Decline shapes `bind_pattern_locals`
                // can't lower so they keep their own handling.
                HirPatKind::Tuple(_) => None,
                _ => return Ok(None),
            };
            (pick, None)
        };
        // For a non-enumerate destructuring element pattern, remember it so
        // each iteration re-binds it from the freshly-loaded element.
        let elem_destructure: Option<&HirPat> = match (is_enumerate, &elem_pat.kind) {
            (false, HirPatKind::Tuple(_)) => Some(elem_pat),
            _ => None,
        };

        // Pick the expression to materialise and drive by `len()` +
        // `IndexGet`. When the inner receiver is itself an indexable
        // collection (`xs.iter()`, a plain `xs`, or a pattern-bound
        // collection local), index it directly - no intermediate
        // `.iter()` allocation. An `xs.iter().enumerate()` chain needs
        // `xs` itself indexable; a non-collection enumerate base isn't
        // driven here. Otherwise materialise the iterator expression once
        // (`map.iter()`, `text.chars()`, `iter::enumerate(xs)`, a range
        // value): every non-stateful inline receiver yields an indexable
        // `Value::Array` at runtime.
        let source_expr: &HirExpr = if self.receiver_is_collection(vec_expr) {
            vec_expr
        } else if is_enumerate {
            // Inferred `let entries = source.iter().collect()` bindings can
            // retain a `Var` HIR type even though their runtime value is an
            // indexable array. Materialise the whole enumerate expression
            // once instead of falling into the generic `next()` desugar,
            // which reconstructs the iterator every trip and repeats item 0.
            if self.receiver_is_lazy_iterator(vec_expr) {
                return Ok(None);
            }
            next_recv
        } else {
            next_recv
        };

        // Move-on-last-use: when the iterated collection is a consumable
        // local (read exactly once here), drain each element out of it as
        // the loop advances instead of cloning, so the input frees as it
        // is consumed. Disabled for a `HashSet` source, whose elements
        // are read from a fresh sorted snapshot rather than the local.
        let source_is_set = self.expr_is_hashset(source_expr);
        let source_is_map = self.expr_is_map(source_expr);
        let consume_source =
            self.consumable_path(source_expr).is_some() && !source_is_set && !source_is_map;

        // Compile the iterable and capture it once.
        let mut vec_reg = self.compile_expr(source_expr)?;

        // A lazy pipeline's value is iterator state with no indexable
        // length. `collect` drains it into the snapshot the index walk
        // drives; on an already-materialized sequence it hands back the
        // same buffer.
        if lazy_source {
            let snap = self.alloc_reg();
            let collect_idx = self.global_idx("collect");
            let cache_idx = self.alloc_cache_idx();
            self.emit(Op::MethodCall {
                dst: snap,
                receiver: vec_reg,
                name_idx: collect_idx,
                args: 0,
                argc: 0,
                cache_idx,
            });
            vec_reg = snap;
        }

        // A bare `HashSet` iterand is not indexable; snapshot it to a
        // sorted `Vec` (the same order `set.to_vec()` / `.iter()` yield)
        // and drive that by index. The set handle would otherwise report
        // no indexable length and the loop would never run.
        if source_is_set {
            let snap = self.alloc_reg();
            let to_vec_idx = self.global_idx("to_vec");
            let cache_idx = self.alloc_cache_idx();
            self.emit(Op::MethodCall {
                dst: snap,
                receiver: vec_reg,
                name_idx: to_vec_idx,
                args: 0,
                argc: 0,
                cache_idx,
            });
            vec_reg = snap;
        } else if source_is_map {
            // Maps are iterable as `(key, value)` pairs, but their runtime
            // storage is keyed rather than numerically indexable. Snapshot
            // entries exactly as `m.iter()` does before using the common
            // indexed loop machinery. This covers both HashMap and BTreeMap.
            let snap = self.alloc_reg();
            let iter_idx = self.global_idx("iter");
            let cache_idx = self.alloc_cache_idx();
            self.emit(Op::MethodCall {
                dst: snap,
                receiver: vec_reg,
                name_idx: iter_idx,
                args: 0,
                argc: 0,
                cache_idx,
            });
            vec_reg = snap;
        }

        // Length: emit a `len()` MethodCall whose result we treat as
        // an i64 by unboxing through `as_i64`.
        let len_dst = self.alloc_reg();
        let len_name = self.global_idx("len");
        let cache_idx = self.alloc_cache_idx();
        self.emit(Op::MethodCall {
            dst: len_dst,
            receiver: vec_reg,
            name_idx: len_name,
            args: 0,
            argc: 0,
            cache_idx,
        });
        let len_tr = TypedReg {
            reg: len_dst,
            kind: RegKind::Value,
        };
        let len_i = self.as_i64(len_tr);

        // Counter starts at 0.
        let zero_idx = self.i64_const_idx(0);
        let counter_i = self.alloc_int();
        self.emit(Op::LoadConstI64 {
            dst_i: counter_i,
            idx: zero_idx,
        });
        // Normal exhaustion of a `for` loop yields unit. A value-bearing
        // `break` still overwrites this register through `LoopCtx`.
        let result = self.load_unit();
        self.push_scope();

        // Element register: a fresh Value reg refilled each iteration.
        let elem_reg = self.alloc_reg();
        if let Some(name) = &elem_binding {
            self.bind_local(
                name,
                TypedReg {
                    reg: elem_reg,
                    kind: RegKind::Value,
                },
            );
        }
        // Index binding (enumerate only) - alias the counter i64 reg.
        if let Some(name) = &idx_binding {
            self.bind_local(
                name,
                TypedReg {
                    reg: counter_i,
                    kind: RegKind::I64,
                },
            );
        }

        let header = self.cur_idx();
        let exit_branch_idx = self.emit(Op::BranchIfGeI64 {
            lhs_i: counter_i,
            rhs_i: len_i,
            target: 0,
        });

        // Refill element register from `vec[counter]`. We need a
        // Value register holding the index; `BoxI64` covers that.
        let idx_v = self.alloc_reg();
        self.emit(Op::BoxI64 {
            dst_v: idx_v,
            src_i: counter_i,
        });
        if consume_source {
            self.emit(Op::IndexGetConsume {
                dst: elem_reg,
                base: vec_reg,
                index: idx_v,
            });
        } else {
            self.emit(Op::IndexGet {
                dst: elem_reg,
                base: vec_reg,
                index: idx_v,
            });
        }
        // Destructure the loaded element (`for (a, b) in xs`) each
        // iteration; a simple binding was already aliased to `elem_reg`.
        // A drained element is uniquely owned, so destructure it by
        // moving its fields out rather than cloning.
        if let Some(pat) = elem_destructure {
            self.bind_pattern_locals_ex(pat, elem_reg, consume_source)?;
        }

        self.loop_stack.push(LoopCtx {
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            result_reg: result,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        self.compile_loop_body(&some_arm.body)?;
        if write_back_elem {
            self.emit(Op::IndexSet {
                base: vec_reg,
                index: idx_v,
                value: elem_reg,
            });
            self.compile_place_store(vec_expr, vec_reg)?;
        }
        // `continue` jumps here so the counter still advances before
        // the bound is re-tested (routing it to `header` would re-check
        // without incrementing and livelock). The fused op's back-edge
        // targets the element refill just past the header bounds check:
        // it already performed the equivalent `< len` test, so only the
        // loop's first entry pays the header check.
        let continue_target = self.cur_idx();
        self.emit(Op::IncJumpIfLtI64 {
            counter_i,
            end_i: len_i,
            target: header + 1,
        });
        let after = self.cur_idx();
        self.patch_jump(exit_branch_idx, after);
        let ctx = self
            .loop_stack
            .pop()
            .expect("loop stack underflow on for-vec-iter");
        for patch in ctx.break_patches {
            self.patch_jump(patch, after);
        }
        for patch in ctx.continue_patches {
            self.patch_jump(patch, continue_target);
        }
        self.pop_scope();
        Ok(Some(result))
    }

    pub(crate) fn load_unit(&mut self) -> Reg {
        let idx = self.const_idx(ConstKey::Unit, Value::Unit);
        let dst = self.alloc_reg();
        self.emit(Op::LoadConst { dst, idx });
        dst
    }
}
