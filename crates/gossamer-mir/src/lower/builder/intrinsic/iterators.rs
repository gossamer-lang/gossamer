//! Lowering iterator construction and advancement: wide elements, options, and the cursor protocol.

use super::*;

impl<'a> Builder<'a> {
    /// The one runtime form a value of an `Iterator` type over elements wider
    /// than a slot takes, built from the sequence `local` holds when an eager
    /// lowering answered one: the pair state for `(i64, i64)`, the
    /// address-carrying stream for any other element. A use site sees only
    /// the type, so every value of that type has to be the same object.
    pub(crate) fn canonical_wide_iterator(&mut self, local: Local, ty: Ty, span: Span) -> Local {
        use gossamer_types::TyKind;
        let TyKind::Iterator(elem) = self.tcx.kind_of(ty).clone() else {
            return local;
        };
        if !self.lazy_elem_is_addressed(elem)
            || !matches!(
                self.tcx.kind_of(self.locals[local.0 as usize].ty),
                TyKind::Vec(_) | TyKind::Slice(_)
            )
        {
            return local;
        }
        let state = self.emit_combinator_call(
            LazyElemFamily::Aggr.vec_source_symbol(),
            vec![Operand::Copy(Place::local(local))],
            ty,
            span,
        );
        if crate::lower::helpers::lazy_iter_is_pair_state(self.tcx, elem) {
            return self.emit_combinator_call(
                "gos_rt_lazy_iter_aggr_pairs",
                vec![Operand::Copy(Place::local(state))],
                ty,
                span,
            );
        }
        self.local_aggr_iter.insert(state);
        state
    }

    pub(super) fn try_lower_iter_call_raw(
        &mut self,
        joined: &str,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit_ty = self.tcx.unit();
        match (joined, args.len()) {
            // Non-closure constructors / accessors.
            ("iter::collect", 1) => {
                let (v, lazy) = self.lower_iter_seq_arg_raw(&args[0])?;
                if let Some(family) = lazy {
                    let elem = match self.tcx.kind_of(self.locals[v.0 as usize].ty) {
                        TyKind::Iterator(elem) => *elem,
                        _ => i64_ty,
                    };
                    let dest_ty = self.tcx.intern(TyKind::Vec(elem));
                    let helper = match family {
                        LazyElemFamily::PairWord => "gos_rt_lazy_iter_collect_pair_i64",
                        LazyElemFamily::Aggr => "gos_rt_lazy_iter_collect_aggr",
                        _ => "gos_rt_lazy_iter_collect_i64",
                    };
                    let collected = self.emit_combinator_call(
                        helper,
                        vec![Operand::Copy(Place::local(v))],
                        dest_ty,
                        span,
                    );
                    if family == LazyElemFamily::Aggr {
                        self.tag_owned_elements(collected, elem, span);
                    }
                    return Some(collected);
                }
                let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Vec(_) | TyKind::Slice(_)) {
                    ty
                } else {
                    self.tcx.intern(TyKind::Vec(i64_ty))
                };
                Some(self.emit_combinator_call(
                    "gos_rt_vec_clone",
                    vec![Operand::Copy(Place::local(v))],
                    dest_ty,
                    span,
                ))
            }
            ("iter::count", 1) => {
                let (v, lazy) = self.lower_iter_seq_arg_raw(&args[0])?;
                if let Some(family) = lazy {
                    let helper = if family == LazyElemFamily::PairWord {
                        "gos_rt_lazy_iter_count_pair_i64"
                    } else {
                        "gos_rt_lazy_iter_count_i64"
                    };
                    return Some(self.emit_combinator_call(
                        helper,
                        vec![Operand::Copy(Place::local(v))],
                        i64_ty,
                        span,
                    ));
                }
                let dest = self.fresh(i64_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_iter_count".to_string())),
                    args: vec![Operand::Copy(Place::local(v))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::empty", 0) => {
                let elem_ty = match self.tcx.kind_of(ty) {
                    TyKind::Vec(e) | TyKind::Slice(e) => *e,
                    _ => i64_ty,
                };
                let elem_bytes_val = i128::from(self.elem_bytes_of(elem_ty).max(1));
                let elem_bytes = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(elem_bytes),
                    Rvalue::Use(Operand::Const(ConstValue::Int(elem_bytes_val))),
                    span,
                );
                let cap = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(cap),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    span,
                );
                let dest = self.fresh(ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_vec_with_capacity".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(elem_bytes)),
                        Operand::Copy(Place::local(cap)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::once", 1) => {
                let v = self.lower_expr(&args[0])?;
                if let Some(family) = self.lazy_iter_ty_family(ty) {
                    let helper = format!("gos_rt_lazy_iter_once_{}", family.value_suffix());
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper)),
                        args: vec![Operand::Copy(Place::local(v))],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Vec(_) | TyKind::Slice(_)) {
                    ty
                } else {
                    self.tcx.intern(TyKind::Vec(i64_ty))
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_iter_repeat_i64".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(v)),
                        Operand::Const(ConstValue::Int(1)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::sum", 1) => {
                // Element-type dispatch: f64 vec → sum_f64, otherwise sum_i64.
                let (v, lazy) = self.lower_iter_seq_arg(&args[0])?;
                if let Some(family) = lazy {
                    let (helper, dest_ty) = match family {
                        LazyElemFamily::Float => (
                            "gos_rt_lazy_iter_sum_f64",
                            self.tcx.float_ty(gossamer_types::FloatTy::F64),
                        ),
                        _ => ("gos_rt_lazy_iter_sum_i64", i64_ty),
                    };
                    return Some(self.emit_combinator_call(
                        helper,
                        vec![Operand::Copy(Place::local(v))],
                        dest_ty,
                        span,
                    ));
                }
                let elem_is_f64 = self.iter_elem_abi(args[0].ty).1 == ElemAbi::Float;
                let helper = if elem_is_f64 {
                    "gos_rt_iter_sum_f64"
                } else {
                    "gos_rt_iter_sum_i64"
                };
                let dest_ty = if elem_is_f64 {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else {
                    i64_ty
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![Operand::Copy(Place::local(v))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::product", 1) => {
                let (v, lazy) = self.lower_iter_seq_arg(&args[0])?;
                if let Some(family) = lazy {
                    let (helper, dest_ty) = match family {
                        LazyElemFamily::Float => (
                            "gos_rt_lazy_iter_product_f64",
                            self.tcx.float_ty(gossamer_types::FloatTy::F64),
                        ),
                        _ => ("gos_rt_lazy_iter_product_i64", i64_ty),
                    };
                    return Some(self.emit_combinator_call(
                        helper,
                        vec![Operand::Copy(Place::local(v))],
                        dest_ty,
                        span,
                    ));
                }
                let elem_is_f64 = self.iter_elem_abi(args[0].ty).1 == ElemAbi::Float;
                let helper = if elem_is_f64 {
                    "gos_rt_iter_product_f64"
                } else {
                    "gos_rt_iter_product_i64"
                };
                let dest_ty = if elem_is_f64 {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else {
                    i64_ty
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![Operand::Copy(Place::local(v))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // Bare prelude `min(xs)` / `max(xs)` over a `Vec`/array return
            // `Option<T>`, exactly like `iter::min`/`iter::max`. The two-arg
            // scalar forms (`min(a, b)`) are handled separately; only the
            // single Vec/array argument reaches here.
            ("iter::min" | "min" | "math::min", 1) => {
                // An element the language orders structurally - a tuple, a
                // struct, a payload enum - is ordered through its descriptor.
                // The word path below is for elements whose slot spells their
                // own order, which is what the descriptor builder declines.
                let (wide_elem, _) = self.iter_elem_abi(args[0].ty);
                if let Some(local) =
                    self.lower_minmax_wide_elem(&args[0], wide_elem, ty, false, span)
                {
                    return Some(local);
                }
                if let Some(local) = self.lower_minmax_string_elem(&args[0], false, span) {
                    return Some(local);
                }
                if let Some(family) = self.lazy_iter_source_family_word(args[0].ty) {
                    let (iter, lazy) = self.lower_iter_seq_arg(&args[0])?;
                    if lazy.is_some() {
                        let helper =
                            format!("gos_rt_lazy_iter_min_{}", family.word_or_float_suffix());
                        return Some(self.emit_combinator_call(
                            &helper,
                            vec![Operand::Copy(Place::local(iter))],
                            ty,
                            span,
                        ));
                    }
                    return self.lower_iter_vec_opt_local(iter, "min", span);
                }
                self.lower_iter_simple_vec_opt("min", args, span)
            }
            ("iter::max" | "max" | "math::max", 1) => {
                // An element the language orders structurally - a tuple, a
                // struct, a payload enum - is ordered through its descriptor.
                // The word path below is for elements whose slot spells their
                // own order, which is what the descriptor builder declines.
                let (wide_elem, _) = self.iter_elem_abi(args[0].ty);
                if let Some(local) =
                    self.lower_minmax_wide_elem(&args[0], wide_elem, ty, true, span)
                {
                    return Some(local);
                }
                if let Some(local) = self.lower_minmax_string_elem(&args[0], true, span) {
                    return Some(local);
                }
                if let Some(family) = self.lazy_iter_source_family_word(args[0].ty) {
                    let (iter, lazy) = self.lower_iter_seq_arg(&args[0])?;
                    if lazy.is_some() {
                        let helper =
                            format!("gos_rt_lazy_iter_max_{}", family.word_or_float_suffix());
                        return Some(self.emit_combinator_call(
                            &helper,
                            vec![Operand::Copy(Place::local(iter))],
                            ty,
                            span,
                        ));
                    }
                    return self.lower_iter_vec_opt_local(iter, "max", span);
                }
                self.lower_iter_simple_vec_opt("max", args, span)
            }
            ("iter::range", 2) => {
                let a = self.lower_expr(&args[0])?;
                let b = self.lower_expr(&args[1])?;
                if matches!(self.tcx.kind_of(ty), TyKind::Iterator(e) if self.lazy_iter_carries_elem(*e))
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_range_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(a)),
                            Operand::Copy(Place::local(b)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let vec_i64 = self.tcx.intern(TyKind::Vec(i64_ty));
                let dest = self.fresh(vec_i64);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_iter_range".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(a)),
                        Operand::Copy(Place::local(b)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::range_inclusive", 2) => {
                let a = self.lower_expr(&args[0])?;
                let b = self.lower_expr(&args[1])?;
                if matches!(self.tcx.kind_of(ty), TyKind::Iterator(e) if self.lazy_iter_carries_elem(*e))
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_range_inclusive_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(a)),
                            Operand::Copy(Place::local(b)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let dest = self.fresh(ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(
                        "gos_rt_iter_range_inclusive".to_string(),
                    )),
                    args: vec![
                        Operand::Copy(Place::local(a)),
                        Operand::Copy(Place::local(b)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::repeat", 2) => {
                let v = self.lower_expr(&args[0])?;
                let n = self.lower_expr(&args[1])?;
                if let Some(family) = self.lazy_iter_ty_family(ty) {
                    let helper = format!("gos_rt_lazy_iter_repeat_{}", family.value_suffix());
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper)),
                        args: vec![
                            Operand::Copy(Place::local(v)),
                            Operand::Copy(Place::local(n)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let vec_i64 = self.tcx.intern(TyKind::Vec(i64_ty));
                let dest = self.fresh(vec_i64);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_iter_repeat_i64".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(v)),
                        Operand::Copy(Place::local(n)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::take", 2) => {
                let n = self.lower_expr(&args[0])?;
                if self.lazy_iter_result_ty(ty)
                    && let Some(iter) = self.lower_lazy_iter_source_aggr(&args[1])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_take_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(n)),
                            Operand::Copy(Place::local(iter)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    self.propagate_aggr_state(iter, dest);
                    return Some(dest);
                }
                let v = self.lower_iter_vec_arg(&args[1])?;
                let dest_ty = self.iter_result_vec_ty(ty, v);
                Some(self.emit_iter_combinator_call(
                    "take",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(n)),
                        Operand::Copy(Place::local(v)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::step_by", 2) => {
                let step = self.lower_expr(&args[0])?;
                if self.lazy_iter_result_ty(ty)
                    && let Some(iter) = self.lower_lazy_iter_source_aggr(&args[1])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_step_by_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(step)),
                            Operand::Copy(Place::local(iter)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    self.propagate_aggr_state(iter, dest);
                    return Some(dest);
                }
                let v = self.lower_iter_vec_arg(&args[1])?;
                let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Vec(_) | TyKind::Slice(_)) {
                    ty
                } else {
                    self.tcx.intern(TyKind::Vec(i64_ty))
                };
                Some(self.emit_combinator_call(
                    "gos_rt_vec_step_by",
                    vec![
                        Operand::Copy(Place::local(v)),
                        Operand::Copy(Place::local(step)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::skip", 2) => {
                let n = self.lower_expr(&args[0])?;
                if self.lazy_iter_result_ty(ty)
                    && let Some(iter) = self.lower_lazy_iter_source_aggr(&args[1])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_skip_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(n)),
                            Operand::Copy(Place::local(iter)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    self.propagate_aggr_state(iter, dest);
                    return Some(dest);
                }
                let v = self.lower_iter_vec_arg(&args[1])?;
                let dest_ty = self.iter_result_vec_ty(ty, v);
                Some(self.emit_iter_combinator_call(
                    "skip",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(n)),
                        Operand::Copy(Place::local(v)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::rev", 1) => {
                if self.lazy_iter_ty_family(ty).is_some()
                    && let Some(iter) = self.lower_lazy_iter_source(&args[0])
                {
                    // Reversal is only defined once the source's length is
                    // known, so the pipeline is snapshotted, reversed, and
                    // handed back as iterator state the rest of the chain
                    // (and the `for` desugar) can keep pulling from.
                    let elem = match self.tcx.kind_of(ty) {
                        TyKind::Iterator(elem) => *elem,
                        _ => i64_ty,
                    };
                    let vec_ty = self.tcx.intern(TyKind::Vec(elem));
                    let collect_symbol = self.lazy_collect_symbol(elem);
                    let collected = self.emit_combinator_call(
                        collect_symbol,
                        vec![Operand::Copy(Place::local(iter))],
                        vec_ty,
                        span,
                    );
                    let (_, snapshot_abi) = self.iter_elem_abi(vec_ty);
                    let reversed = self.emit_iter_combinator_call(
                        "rev",
                        snapshot_abi,
                        None,
                        vec![Operand::Copy(Place::local(collected))],
                        vec_ty,
                        span,
                    );
                    let from_vec = self.lazy_iter_elem_family(elem).map_or(
                        "gos_rt_lazy_iter_from_vec_i64",
                        LazyElemFamily::vec_source_symbol,
                    );
                    return Some(self.emit_combinator_call(
                        from_vec,
                        vec![Operand::Copy(Place::local(reversed))],
                        ty,
                        span,
                    ));
                }
                // The word-slot reversal moves one slot per element, which
                // for a wider element reorders its fields rather than the
                // elements; the vec form moves each element whole.
                let (_, rev_abi) = self.iter_elem_abi(args[0].ty);
                if rev_abi == ElemAbi::Ptr {
                    let source = self.lower_iter_vec_arg(&args[0])?;
                    let elem = self.sequence_elem_ty_of(self.locals[source.0 as usize].ty);
                    let dest_ty = elem.map_or(ty, |elem| self.tcx.intern(TyKind::Vec(elem)));
                    return Some(self.emit_combinator_call(
                        "gos_rt_vec_reversed",
                        vec![Operand::Copy(Place::local(source))],
                        dest_ty,
                        span,
                    ));
                }
                self.lower_iter_simple_vec_in_vec_out("rev", args, ty, span)
            }
            ("iter::chain", 2) => {
                if self.lazy_iter_ty_family(ty).is_some()
                    && let Some(a) = self.lower_lazy_iter_source(&args[0])
                    && let Some(b) = self.lower_lazy_iter_source(&args[1])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_chain_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(a)),
                            Operand::Copy(Place::local(b)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let a = self.lower_iter_vec_arg(&args[0])?;
                let b = self.lower_iter_vec_arg(&args[1])?;
                // The word-slot concatenation copies one slot per element; a
                // wider element is copied whole by the vec form, which also
                // gives the result its own share of each element's children.
                let (_, chain_abi) = self.iter_elem_abi(args[0].ty);
                if chain_abi == ElemAbi::Ptr {
                    let elem = self.sequence_elem_ty_of(self.locals[a.0 as usize].ty);
                    let dest_ty = elem.map_or(ty, |elem| self.tcx.intern(TyKind::Vec(elem)));
                    let joined_local = self.emit_combinator_call(
                        "gos_rt_vec_clone",
                        vec![Operand::Copy(Place::local(a))],
                        dest_ty,
                        span,
                    );
                    let unit_ty = self.tcx.unit();
                    let _ = self.emit_combinator_call(
                        "gos_rt_vec_extend",
                        vec![
                            Operand::Copy(Place::local(joined_local)),
                            Operand::Copy(Place::local(b)),
                        ],
                        unit_ty,
                        span,
                    );
                    return Some(joined_local);
                }
                let dest_ty = self.iter_result_vec_ty(ty, a);
                Some(self.emit_iter_combinator_call(
                    "chain",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(a)),
                        Operand::Copy(Place::local(b)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::dedup", 1) => {
                let v = self.lower_iter_vec_arg(&args[0])?;
                let vec_ty = self.iter_result_vec_ty(ty, v);
                // Consecutive equality is the element's own, so an element
                // whose slot word is not its value travels with the ordering
                // descriptor the runtime compares it through.
                let unit_ty = self.tcx.unit();
                let elem = self.iter_result_elem_ty(unit_ty, v);
                let desc = self.elem_equality_descriptor(elem);
                let has_desc = i128::from(!desc.is_empty());
                let string_ty = self.tcx.string_ty();
                let desc_local = self.fresh(string_ty);
                self.emit_assign(
                    Place::local(desc_local),
                    Rvalue::Use(Operand::Const(ConstValue::Str(desc))),
                    span,
                );
                Some(self.emit_iter_combinator_call(
                    "dedup",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(v)),
                        Operand::Copy(Place::local(desc_local)),
                        Operand::Const(ConstValue::Int(has_desc)),
                    ],
                    vec_ty,
                    span,
                ))
            }
            ("iter::flatten", 1) => {
                let vec_local = self.lower_iter_vec_arg(&args[0])?;
                // The flattened sequence carries the inner sequence's element,
                // which is the element of the element that was read.
                let inner_seq = self.iter_result_elem_ty(ty, vec_local);
                let elem = self.sequence_elem_ty_of(inner_seq).unwrap_or(inner_seq);
                let dest_ty = self.tcx.intern(TyKind::Vec(elem));
                Some(self.emit_iter_combinator_call(
                    "flatten",
                    ElemAbi::Word,
                    None,
                    vec![Operand::Copy(Place::local(vec_local))],
                    dest_ty,
                    span,
                ))
            }
            ("iter::enumerate", 1) => {
                if self.lazy_iter_ty_family(ty).is_some()
                    && let Some(iter_local) = self.lower_lazy_iter_source(&args[0])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_enumerate_i64".to_string(),
                        )),
                        args: vec![Operand::Copy(Place::local(iter_local))],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let vec_local = self.lower_iter_vec_arg(&args[0])?;
                // A wide element does not fit the word slot the pair helper
                // writes, and a counted one needs the pair to own a share of
                // it, so both are built element by element, each pair
                // carrying the element through the ordinary element path.
                let (wide_elem, wide_abi) = self.iter_elem_abi(args[0].ty);
                if wide_abi == ElemAbi::Ptr || self.tcx.is_rc_managed(wide_elem) {
                    return Some(self.lower_enumerate_wide_elem(vec_local, wide_elem, span));
                }
                let unit_ty = self.tcx.unit();
                let elem = self.iter_result_elem_ty(unit_ty, vec_local);
                let pair = self.tcx.intern(TyKind::Tuple(vec![i64_ty, elem]));
                let dest_ty = self.tcx.intern(TyKind::Vec(pair));
                Some(self.emit_iter_combinator_call(
                    "enumerate",
                    ElemAbi::Word,
                    None,
                    vec![Operand::Copy(Place::local(vec_local))],
                    dest_ty,
                    span,
                ))
            }
            ("iter::zip", 2) => {
                if self.lazy_iter_ty_family(ty).is_some()
                    && let Some(a) = self.lower_lazy_iter_source(&args[0])
                    && let Some(b) = self.lower_lazy_iter_source(&args[1])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_zip_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(a)),
                            Operand::Copy(Place::local(b)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let a = self.lower_iter_vec_arg(&args[0])?;
                let b = self.lower_iter_vec_arg(&args[1])?;
                let (a_elem, _) = self.iter_elem_abi(args[0].ty);
                let (b_elem, _) = self.iter_elem_abi(args[1].ty);
                // The word-slot shim copies each side's slot verbatim, which
                // is the element itself only for an integer-shaped scalar; a
                // float, a String, or an aggregate carries a bit pattern or a
                // managed address the pair has to take ownership of.
                let pairs = if self.zip_slot_is_the_element(a_elem)
                    && self.zip_slot_is_the_element(b_elem)
                {
                    let pair = self.tcx.intern(TyKind::Tuple(vec![a_elem, b_elem]));
                    let dest_ty = self.tcx.intern(TyKind::Vec(pair));
                    self.emit_iter_combinator_call(
                        "zip",
                        ElemAbi::Word,
                        None,
                        vec![
                            Operand::Copy(Place::local(a)),
                            Operand::Copy(Place::local(b)),
                        ],
                        dest_ty,
                        span,
                    )
                } else {
                    self.lower_zip_general(a, b, a_elem, b_elem, span)
                };
                // An iterator-typed zip answers a cursor over its pairs, the
                // shape `next` and the adapters advance, as a map's `iter()`
                // does over the pairs it reads out.
                if matches!(self.tcx.kind_of(ty), TyKind::Iterator(_)) {
                    return Some(self.entries_cursor(pairs, span).unwrap_or(pairs));
                }
                Some(pairs)
            }
            // Successive overlapping pairs are the sequence zipped against
            // itself advanced by one, which is the general pairing lowering
            // and carries any element through by its own width.
            ("iter::pairwise", 1) => {
                let vec_local = self.lower_iter_vec_arg(&args[0])?;
                let elem = self
                    .sequence_elem_ty_of(self.locals[vec_local.0 as usize].ty)
                    .unwrap_or(i64_ty);
                let vec_ty = self.locals[vec_local.0 as usize].ty;
                let one = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(one),
                    Rvalue::Use(Operand::Const(ConstValue::Int(1))),
                    span,
                );
                let tail = self.emit_iter_combinator_call(
                    "skip",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(one)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    vec_ty,
                    span,
                );
                Some(self.lower_zip_general(vec_local, tail, elem, elem, span))
            }
            ("iter::windows", 2) => {
                let n = self.lower_expr(&args[0])?;
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let unit_ty = self.tcx.unit();
                let inner = self.iter_result_vec_ty(unit_ty, vec_local);
                let dest_ty = self.tcx.intern(TyKind::Vec(inner));
                Some(self.emit_iter_combinator_call(
                    "windows",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(n)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::chunks", 2) => {
                let n = self.lower_expr(&args[0])?;
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let unit_ty = self.tcx.unit();
                let inner = self.iter_result_vec_ty(unit_ty, vec_local);
                let dest_ty = self.tcx.intern(TyKind::Vec(inner));
                Some(self.emit_iter_combinator_call(
                    "chunks",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(n)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::unzip", 1) => {
                let vec_local = self.lower_iter_vec_arg(&args[0])?;
                // Each side carries its own half of the pair that was read.
                let unit_ty = self.tcx.unit();
                let pair = self.iter_result_elem_ty(unit_ty, vec_local);
                let halves = match self.tcx.kind_of(pair) {
                    TyKind::Tuple(parts) if parts.len() == 2 => Some((parts[0], parts[1])),
                    _ => None,
                };
                let (left, right) = halves.unwrap_or((i64_ty, i64_ty));
                let left_vec = self.tcx.intern(TyKind::Vec(left));
                let right_vec = self.tcx.intern(TyKind::Vec(right));
                let dest_ty = self.tcx.intern(TyKind::Tuple(vec![left_vec, right_vec]));
                Some(self.emit_iter_combinator_call(
                    "unzip",
                    ElemAbi::Word,
                    None,
                    vec![Operand::Copy(Place::local(vec_local))],
                    dest_ty,
                    span,
                ))
            }
            // Closure-taking helpers. Args are `(f, ..., xs)`; coerce
            // the closure to its callback FnTrait shape so the
            // unified callable infra ships an env pointer with the
            // body address at env[0].
            ("iter::for_each", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (mut in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                // A String is a managed pointer word, so it rides the word
                // shim while keeping its own type at the callback.
                if let Some(elem) = self.sequence_elem_ty_of(self.locals[vec_local.0 as usize].ty)
                    && matches!(self.tcx.kind_of(elem), TyKind::String)
                {
                    in_ty = elem;
                }
                let closure_local = self.lower_iter_closure(&args[0], &[in_ty], i64_ty, span)?;
                let _ = self.emit_iter_combinator_call(
                    "for_each",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    unit_ty,
                    span,
                );
                Some(self.lower_unit(span))
            }
            ("iter::map", 2) => {
                // Route an f64-element map through the float-ABI shim + closure
                // so the element rides an SSE register; a hardcoded i64 sig
                // would hand the closure integer-register bits it reads as a
                // garbage double. The output shape stays the closure's own
                // return type so a `[f64] -> [i64]` map (or the reverse) is
                // typed correctly.
                let (in_ty, in_abi) = self.iter_elem_abi(args[1].ty);
                let out_ty = self
                    .iter_element_kind(ty)
                    .map_or(i64_ty, |k| self.tcx.intern(k));
                let out_abi = self.scalar_abi_of(out_ty);
                // The lazy state carries the SOURCE elements, so a lazy result
                // type alone does not qualify the call: the input's own family
                // decides whether a handle can exist at all. A mapped element
                // wider than a slot is answered as the address of storage the
                // callback owns, and the lazy state holds one word per element
                // with nowhere to copy that block to, so it stays eager.
                // A struct, tuple, or array result is answered as the address of
                // the block the callback built, so the lazy state carries that
                // address and the stream is marked as address-carrying. A float
                // source hands the callback its element in another register
                // file, which the address form's callback shape does not.
                // An `Option` / `Result` result is a two-word carrier the
                // callback answers by value; each one becomes a counted blob the
                // stream hands out by address, laid out as a one-field tuple.
                if self.is_result_or_option_adt(out_ty)
                    && matches!(self.tcx.kind_of(ty), TyKind::Iterator(_))
                    && self
                        .lazy_iter_source_family(args[1].ty)
                        .is_some_and(|family| family != LazyElemFamily::Float)
                {
                    let holder = self.tcx.intern(TyKind::Tuple(vec![out_ty]));
                    if let Some(meta) = self
                        .ensure_aggr_struct_meta(holder)
                        .or_else(|| self.ensure_aggr_copy_meta(holder))
                    {
                        let closure_local =
                            self.lower_iter_closure(&args[0], &[in_ty], out_ty, span)?;
                        let iter_local = self.lower_lazy_iter_source(&args[1])?;
                        let dest = self.fresh(ty);
                        let next = self.new_block(span);
                        self.terminate(Terminator::Call {
                            callee: Operand::Const(ConstValue::Str(
                                "gos_rt_lazy_iter_map_carrier".to_string(),
                            )),
                            args: vec![
                                Operand::Copy(Place::local(closure_local)),
                                Operand::Copy(Place::local(iter_local)),
                                Operand::Const(ConstValue::Str(meta)),
                            ],
                            destination: Place::local(dest),
                            target: Some(next),
                        });
                        self.set_current(next);
                        self.local_aggr_iter.insert(dest);
                        return Some(dest);
                    }
                }
                if self.elem_is_slot_addressed(out_ty)
                    && self.lazy_addressed_elem(out_ty)
                    && matches!(self.tcx.kind_of(ty), TyKind::Iterator(_))
                    && self
                        .lazy_iter_source_family(args[1].ty)
                        .is_some_and(|family| family != LazyElemFamily::Float)
                {
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[in_ty], out_ty, span)?;
                    let iter_local = self.lower_lazy_iter_source_aggr(&args[1])?;
                    let width = i128::from(self.elem_bytes_of(out_ty));
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_map_aggr".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                            Operand::Const(ConstValue::Int(width)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    // Each result is a block only the stream holds, so the
                    // stream hands it out as a counted blob this meta describes:
                    // the element's owning fields when it has any, the leaf
                    // copy meta when it does not.
                    if let Some(meta) = self
                        .ensure_aggr_struct_meta(out_ty)
                        .or_else(|| self.ensure_aggr_copy_meta(out_ty))
                    {
                        let unit = self.tcx.unit();
                        let sink = self.fresh(unit);
                        self.emit_assign(
                            Place::local(sink),
                            Rvalue::CallIntrinsic {
                                name: "gos_rt_lazy_iter_set_elem_meta",
                                args: vec![
                                    Operand::Copy(Place::local(dest)),
                                    Operand::Const(ConstValue::Str(meta)),
                                ],
                            },
                            span,
                        );
                    }
                    self.local_aggr_iter.insert(dest);
                    return Some(dest);
                }
                if let Some(source) = self.lazy_iter_source_family(args[1].ty)
                    && self.lazy_iter_ty_family(ty).is_some()
                    && !self.elem_is_slot_addressed(out_ty)
                    && self.elem_bytes_of(out_ty) <= 8
                {
                    // A `String` result is a fresh share the stream's consumers
                    // own, so it is produced by the helper that counts it.
                    let counted = self.lazy_iter_elem_family(out_ty) == Some(LazyElemFamily::Ptr);
                    let helper = match (source, out_abi) {
                        (LazyElemFamily::Float, ElemAbi::Float) => "gos_rt_lazy_iter_map_f64",
                        (LazyElemFamily::Float, _) if counted => "gos_rt_lazy_iter_map_f64_str",
                        (LazyElemFamily::Float, _) => "gos_rt_lazy_iter_map_f64_word",
                        (_, ElemAbi::Float) => "gos_rt_lazy_iter_map_word_f64",
                        _ if counted => "gos_rt_lazy_iter_map_str",
                        _ => "gos_rt_lazy_iter_map_i64",
                    };
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[in_ty], out_ty, span)?;
                    let iter_local = self.lower_lazy_iter_source_aggr(&args[1])?;
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper.to_string())),
                        args: vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                // A callback answering an `Option` or `Result` hands back the
                // two-word carrier by value, which the carrier shim reads and
                // stores whole.
                if matches!(self.tcx.kind_of(out_ty),
                    TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1)
                {
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[in_ty], out_ty, span)?;
                    let vec_local = self.lower_iter_vec_arg(&args[1])?;
                    let dest_ty = self.eager_seq_result_ty(ty, out_ty);
                    let mapped = self.emit_iter_combinator_call(
                        "map_carrier",
                        in_abi,
                        None,
                        vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(vec_local)),
                        ],
                        dest_ty,
                        span,
                    );
                    self.tag_owned_elements(mapped, out_ty, span);
                    return Some(mapped);
                }
                // A callback the compiler can name is called directly, one
                // element at a time, instead of through the runtime's
                // combinator shim: no closure environment, no indirect call
                // the optimiser has to see through, and no separate output
                // buffer built by the runtime.
                if let Some(local) = self.try_lower_direct_map(
                    &args[0], &args[1], in_ty, in_abi, out_ty, out_abi, ty, span,
                ) {
                    return Some(local);
                }
                // A word-result map changes the element type, so the output
                // vec cannot inherit the source's stride: the mapped element's
                // own declared width travels with the call.
                // The mapped element rides an integer register unless it is
                // an `f64`; an aggregate result travels as the address of its
                // slots, which is a word.
                let out_class = match out_abi {
                    ElemAbi::Float => ElemAbi::Float,
                    _ => ElemAbi::Word,
                };
                let declares_width = out_class == ElemAbi::Word;
                let closure_local = self.lower_iter_closure(&args[0], &[in_ty], out_ty, span)?;
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                // The eager shim answers with a `GosVec`, so the destination
                // carries a Vec type even where the surface promised iterator
                // state: downstream terminals read the value's real shape.
                let dest_ty = self.eager_seq_result_ty(ty, out_ty);
                let mut call_args = vec![
                    Operand::Copy(Place::local(closure_local)),
                    Operand::Copy(Place::local(vec_local)),
                ];
                if declares_width {
                    let width = i128::from(self.elem_bytes_of(out_ty));
                    call_args.push(Operand::Const(ConstValue::Int(width)));
                    // A mapped struct, tuple, or array is answered as the
                    // address of its slots whatever its width, so the shim
                    // copies the block rather than storing the word.
                    let by_block = i128::from(self.elem_is_slot_addressed(out_ty));
                    call_args.push(Operand::Const(ConstValue::Int(by_block)));
                }
                let mapped = self.emit_iter_combinator_call(
                    "map",
                    in_abi,
                    Some(out_class),
                    call_args,
                    dest_ty,
                    span,
                );
                // Each element the callback answered carries the shares its
                // fields own, which the result now holds.
                self.tag_owned_elements(mapped, out_ty, span);
                Some(mapped)
            }
            ("iter::filter", 2) => {
                let bool_ty = self.tcx.bool_ty();
                let (in_ty, in_abi) = self.iter_elem_abi(args[1].ty);
                if let Some(source) = self.lazy_iter_source_family(args[1].ty)
                    && self.lazy_iter_result_ty(ty)
                {
                    let helper =
                        format!("gos_rt_lazy_iter_filter_{}", source.word_or_float_suffix());
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[in_ty], bool_ty, span)?;
                    let iter_local = self.lower_lazy_iter_source_aggr(&args[1])?;
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper)),
                        args: vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    self.propagate_aggr_state(iter_local, dest);
                    return Some(dest);
                }
                let closure_local = self.lower_iter_closure(&args[0], &[in_ty], bool_ty, span)?;
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let dest_ty = self.eager_seq_result_ty(ty, in_ty);
                Some(self.emit_iter_combinator_call(
                    "filter",
                    in_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::fold", 3) => {
                // The accumulator's type is the fold's result and the
                // callback's first parameter; the element fills its second.
                let init_local = self.lower_expr(&args[0])?;
                let acc_ty = self.locals[init_local.0 as usize].ty;
                let (elem_ty, _) = self.iter_elem_abi(args[2].ty);
                let closure_local =
                    self.lower_iter_closure(&args[1], &[acc_ty, elem_ty], acc_ty, span)?;
                self.lower_fold_loop(
                    FoldSeed::Init(init_local),
                    closure_local,
                    &args[2],
                    acc_ty,
                    elem_ty,
                    span,
                )
            }
            ("iter::sum_by", 2) => {
                let (elem_ty, elem_abi) = self.iter_elem_abi(args[1].ty);
                let out_abi = self.scalar_abi_of(ty);
                let out_ty = match out_abi {
                    ElemAbi::Float => self.tcx.float_ty(gossamer_types::FloatTy::F64),
                    _ => i64_ty,
                };
                let out_class = match out_abi {
                    ElemAbi::Float => ElemAbi::Float,
                    _ => ElemAbi::Word,
                };
                let closure_local = self.lower_iter_closure(&args[0], &[elem_ty], out_ty, span)?;
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                Some(self.emit_iter_combinator_call(
                    "sum_by",
                    elem_abi,
                    Some(out_class),
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    out_ty,
                    span,
                ))
            }
            ("iter::any", 2) => {
                let bool_ty = self.tcx.bool_ty();
                if let Some(source) = self.lazy_iter_ty_family(args[1].ty) {
                    let (elem_ty, _) = self.iter_elem_abi(args[1].ty);
                    let helper = format!("gos_rt_lazy_iter_any_{}", source.word_or_float_suffix());
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[elem_ty], bool_ty, span)?;
                    let iter_local = self.lower_lazy_iter_source_aggr(&args[1])?;
                    let dest = self.fresh(bool_ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper)),
                        args: vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                // A wide element is handed to the predicate by slot address,
                // so the closure takes the element type and the by-pointer
                // shim feeds it; matches `iter::all`.
                let (elem_ty, elem_abi) = self.iter_elem_abi(args[1].ty);
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let closure_local = self.lower_iter_closure(&args[0], &[elem_ty], bool_ty, span)?;
                let combinator = "any";
                // Bool-typed destination so `{}` renders true/false
                // like the VM; the shim returns i64 0/1.
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    bool_ty,
                    span,
                ))
            }
            ("iter::all", 2) => {
                let bool_ty = self.tcx.bool_ty();
                if let Some(source) = self.lazy_iter_ty_family(args[1].ty) {
                    let (elem_ty, _) = self.iter_elem_abi(args[1].ty);
                    let helper = format!("gos_rt_lazy_iter_all_{}", source.word_or_float_suffix());
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[elem_ty], bool_ty, span)?;
                    let iter_local = self.lower_lazy_iter_source_aggr(&args[1])?;
                    let dest = self.fresh(bool_ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper)),
                        args: vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let (elem_ty, elem_abi) = self.iter_elem_abi(args[1].ty);
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let closure_local = self.lower_iter_closure(&args[0], &[elem_ty], bool_ty, span)?;
                let combinator = "all";
                // Bool-typed destination so `{}` renders true/false
                // like the VM; the shim returns i64 0/1.
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    bool_ty,
                    span,
                ))
            }
            ("iter::find", 2) => {
                // Build an Option<i64> from a (flag, value) pair so
                // pattern matching on the result keeps working.
                let bool_ty = self.tcx.bool_ty();
                let (elem_ty, elem_abi) = self.iter_elem_abi(args[1].ty);
                // A wide element has no single-word `Option` payload to carry,
                // so it keeps the eager word-slot path only when its slots fit
                // a word; the combinator surface rejects the rest upstream.
                let closure_local = self.lower_iter_closure(&args[0], &[elem_ty], bool_ty, span)?;
                // A wide element is searched over its own storage: the matches
                // are kept as elements of that shape, and the first of them
                // becomes the payload the way `Some(kept[0])` builds one.
                if elem_abi == ElemAbi::Ptr {
                    return self.lower_find_wide_elem(closure_local, &args[1], elem_ty, ty, span);
                }
                if let Some(source) = self.lazy_iter_ty_family(args[1].ty) {
                    let helper = format!("gos_rt_lazy_iter_find_{}", source.word_or_float_suffix());
                    let iter_local = self.lower_lazy_iter_source(&args[1])?;
                    return Some(self.emit_combinator_call(
                        &helper,
                        vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                        ],
                        ty,
                        span,
                    ));
                }
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let value = self.emit_iter_combinator_call(
                    "find",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    i64_ty,
                    span,
                );
                // The answer is one of the sequence's own elements, and the
                // `Option` that carries it owns its payload, so a counted
                // element takes a share of its own. An unmatched search
                // answers null, which a retain leaves alone.
                if self.tcx.is_rc_managed(elem_ty) {
                    let unit_ty = self.tcx.unit();
                    let share = self.fresh(unit_ty);
                    self.emit_assign(
                        Place::local(share),
                        Rvalue::CallIntrinsic {
                            name: if self.tcx.is_weak_ty(elem_ty) {
                                "gos_rt_rc_weak_retain"
                            } else {
                                "gos_rt_rc_retain"
                            },
                            args: vec![Operand::Copy(Place::local(value))],
                        },
                        span,
                    );
                }
                let flag = self.emit_iter_combinator_call(
                    "find_flag",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    i64_ty,
                    span,
                );
                // Convert flag (0/1) → disc (0 for Some, 1 for None).
                let disc = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(disc),
                    Rvalue::BinaryOp {
                        op: crate::BinOp::Sub,
                        lhs: Operand::Const(ConstValue::Int(1)),
                        rhs: Operand::Copy(Place::local(flag)),
                    },
                    span,
                );
                let rty = self.result_repr_ty(ty);
                let dest = self.fresh(rty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_new",
                        args: vec![
                            Operand::Copy(Place::local(disc)),
                            Operand::Copy(Place::local(value)),
                        ],
                    },
                    span,
                );
                Some(dest)
            }
            _ => None,
        }
    }

    /// Destination type for `option::unwrap_or` / `result::unwrap_or`:
    /// the call expression's HIR type when concrete, else the payload
    /// type (`substs[0]`) recovered from the scrutinee's HIR or MIR
    /// type, else i64. The call-expression type is often still an
    /// inference Var here, and the dest must keep a heap payload's
    /// real type so the drop / fmt machinery sees e.g. a String
    /// rather than a raw i64.
    pub(super) fn unwrap_default_dest_ty(
        &mut self,
        expr_ty: Ty,
        scrutinee_hir_ty: Ty,
        scrutinee_local: Local,
    ) -> Ty {
        use gossamer_types::TyKind;
        let concrete = |t: Ty| {
            !matches!(
                self.tcx.kind_of(t),
                TyKind::Var(_) | TyKind::Error | TyKind::Never
            )
        };
        if concrete(expr_ty) {
            return expr_ty;
        }
        let sources = [scrutinee_hir_ty, self.locals[scrutinee_local.0 as usize].ty];
        for src in sources {
            if let TyKind::Adt { substs, .. } = self.tcx.kind_of(src) {
                if let Some(payload) = substs.types().first().copied() {
                    if concrete(payload) {
                        return payload;
                    }
                }
            }
        }
        self.tcx.int_ty(gossamer_types::IntTy::I64)
    }

    /// The `option::unwrap` / `option::expect` helper for a payload of type
    /// `payload`: a carrier is boxed and loads back as two words.
    pub(super) fn option_unwrap_helper(&self, payload: Ty) -> &'static str {
        if self.is_result_or_option_adt(payload) {
            "gos_rt_option_unwrap_carrier"
        } else {
            "gos_rt_option_unwrap"
        }
    }

    pub(crate) fn try_lower_option_call(
        &mut self,
        joined: &str,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::IntTy;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        match (joined, args.len()) {
            ("option::is_some", 1) => {
                self.lower_combinator_pred_call("gos_rt_option_is_some", args, span)
            }
            ("option::is_none", 1) => {
                self.lower_combinator_pred_call("gos_rt_option_is_none", args, span)
            }
            ("option::unwrap", 1) => {
                let opt = self.lower_expr(&args[0])?;
                let dest_ty = self.unwrap_default_dest_ty(ty, args[0].ty, opt);
                let helper = self.option_unwrap_helper(dest_ty);
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![Operand::Copy(Place::local(opt))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("option::expect", 2) => {
                let _message = self.lower_expr(&args[0])?;
                let opt = self.lower_expr(&args[1])?;
                let dest_ty = self.unwrap_default_dest_ty(ty, args[1].ty, opt);
                let helper = self.option_unwrap_helper(dest_ty);
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![Operand::Copy(Place::local(opt))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("option::unwrap_or", 2) => {
                let fallback = self.lower_expr(&args[0])?;
                let opt = self.lower_expr(&args[1])?;
                let payload_ty = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                if self.tcx.elem_is_addressed_aggregate(payload_ty)
                    && !self.carrier_payload_is_carrier(args[1].ty)
                {
                    return Some(self.lower_unwrap_or_inline(
                        opt,
                        CarrierFallback::Value(fallback),
                        args[1].ty,
                        true,
                        payload_ty,
                        span,
                    ));
                }
                let dest_ty = self.unwrap_default_dest_ty(ty, args[1].ty, opt);
                // A carrier payload is boxed, so its helper loads the two words
                // back and takes the option first.
                let nested = self.is_result_or_option_adt(dest_ty);
                let helper = if nested {
                    "gos_rt_result_unwrap_or_carrier"
                } else if matches!(self.tcx.kind_of(dest_ty), gossamer_types::TyKind::Float(_)) {
                    "gos_rt_option_default_f64"
                } else {
                    "gos_rt_option_default_i64"
                };
                let call_args = if nested {
                    vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(fallback)),
                    ]
                } else {
                    vec![
                        Operand::Copy(Place::local(fallback)),
                        Operand::Copy(Place::local(opt)),
                    ]
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: call_args,
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("option::ok_or", 2) => {
                let err = self.lower_expr(&args[0])?;
                let opt = self.lower_expr(&args[1])?;
                let dest = self.fresh(ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_result_ok_or".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(err)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // `option::map(f, opt) -> Option<U>`. Closure-arg first
            // (Gossamer's data-last `|>` syntactic-sugar passes the
            // pipe value as the *trailing* arg), opt second. Builds
            // a fresh Option packed in `*mut GosResult` with disc=0
            // for Some(mapped) and disc=1 for None passthrough.
            ("option::map" | "result::map", 2)
                if self.enum_payload_ty(ty, 0).is_some_and(|mapped| {
                    !matches!(
                        self.tcx.kind_of(mapped),
                        gossamer_types::TyKind::Var(_)
                            | gossamer_types::TyKind::Error
                            | gossamer_types::TyKind::Never
                    )
                }) =>
            {
                // The closure is called in this frame, as the method form does.
                let mapped_ty = self.enum_payload_ty(ty, 0)?;
                let recv = self.lower_expr(&args[1])?;
                let recv_ty = self.locals[recv.0 as usize].ty;
                let payload_ty = self.enum_payload_ty(recv_ty, 0).unwrap_or(i64_ty);
                let closure = self.lower_iter_closure(&args[0], &[payload_ty], mapped_ty, span)?;
                Some(self.lower_map_inline(recv, closure, recv_ty, mapped_ty, ty, span))
            }
            ("option::map", 2) => {
                // Lower the option first so the closure's parameter can be
                // typed from the payload it actually receives. A payload wider
                // than a slot - a tuple or a struct - travels as one word, but
                // the closure must still see its real type or a destructuring
                // pattern reads that word as the wrong shape.
                let opt_local = self.lower_expr(&args[1])?;
                let payload_ty = self
                    .option_payload_of(self.locals[opt_local.0 as usize].ty)
                    .unwrap_or(i64_ty);
                let closure_local =
                    self.lower_iter_closure(&args[0], &[payload_ty], i64_ty, span)?;
                let opt_ty = if self.is_option_adt(ty) {
                    ty
                } else {
                    let substs = gossamer_types::Substs::from_types([i64_ty]);
                    self.tcx.intern(gossamer_types::TyKind::Adt {
                        def: gossamer_resolve::DefId::local(u32::MAX - 1),
                        substs,
                    })
                };
                let dest = self.fresh(opt_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_option_map_i64".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(opt_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // `result::unwrap_or(v, res) -> T`. Data-last pipe: the
            // fallback value is arg 0, the Result arg 1. Returns the
            // `Ok` payload, or the fallback when the Result is `Err`.
            ("result::unwrap_or", 2) => {
                let fallback = self.lower_expr(&args[0])?;
                let res_local = self.lower_expr(&args[1])?;
                let payload_ty = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                if self.tcx.elem_is_addressed_aggregate(payload_ty)
                    && !self.carrier_payload_is_carrier(args[1].ty)
                {
                    return Some(self.lower_unwrap_or_inline(
                        res_local,
                        CarrierFallback::Value(fallback),
                        args[1].ty,
                        false,
                        payload_ty,
                        span,
                    ));
                }
                let dest_ty = self.unwrap_default_dest_ty(ty, args[1].ty, res_local);
                let helper =
                    if matches!(self.tcx.kind_of(dest_ty), gossamer_types::TyKind::Float(_)) {
                        "gos_rt_result_default_f64"
                    } else {
                        "gos_rt_result_default"
                    };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![
                        Operand::Copy(Place::local(fallback)),
                        Operand::Copy(Place::local(res_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // `result::unwrap_or_else(f, res) -> T`. Data-last pipe: the
            // closure is arg 0, the Result arg 1. Returns the `Ok`
            // value, or the closure applied to the `Err` payload.
            ("result::unwrap_or_else", 2) => {
                let payload_ty = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                if self.tcx.elem_is_addressed_aggregate(payload_ty)
                    || self.carrier_payload_is_carrier(args[1].ty)
                    || matches!(
                        self.tcx.kind_of(payload_ty),
                        gossamer_types::TyKind::Float(_)
                    )
                {
                    let err_ty = self.enum_payload_ty(args[1].ty, 1).unwrap_or(i64_ty);
                    let closure = self.lower_iter_closure(&args[0], &[err_ty], payload_ty, span)?;
                    let res_local = self.lower_expr(&args[1])?;
                    return Some(self.lower_unwrap_or_inline(
                        res_local,
                        CarrierFallback::Call(closure),
                        args[1].ty,
                        false,
                        payload_ty,
                        span,
                    ));
                }
                let closure_local = self.lower_iter_closure(&args[0], &[i64_ty], i64_ty, span)?;
                let res_local = self.lower_expr(&args[1])?;
                let dest_ty = if matches!(
                    self.tcx.kind_of(ty),
                    gossamer_types::TyKind::Var(_)
                        | gossamer_types::TyKind::Error
                        | gossamer_types::TyKind::Never
                ) {
                    i64_ty
                } else {
                    ty
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(
                        "gos_rt_result_default_with".to_string(),
                    )),
                    args: vec![
                        Operand::Copy(Place::local(res_local)),
                        Operand::Copy(Place::local(closure_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // `result::map_err(f, res) -> Result<T, F>`. Data-last
            // pipe: closure first, Result second. Routes through the
            // same env-first shim the method form uses; Ok passes
            // through unchanged.
            ("result::map_err", 2) => {
                let closure_local = self.lower_iter_closure(&args[0], &[i64_ty], i64_ty, span)?;
                let res_local = self.lower_expr(&args[1])?;
                let dest_ty = if matches!(
                    self.tcx.kind_of(ty),
                    gossamer_types::TyKind::Var(_)
                        | gossamer_types::TyKind::Error
                        | gossamer_types::TyKind::Never
                ) {
                    let err_ty = self.tcx.dyn_error_ty();
                    let substs = gossamer_types::Substs::from_types([i64_ty, err_ty]);
                    self.tcx.intern(gossamer_types::TyKind::Adt {
                        def: gossamer_resolve::DefId::local(u32::MAX),
                        substs,
                    })
                } else {
                    ty
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_result_map_err".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(res_local)),
                        Operand::Copy(Place::local(closure_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // `result::map(f, res) -> Result<U, E>`. Same shape as
            // `option::map`; Err passes through unchanged.
            ("result::map", 2) => {
                let closure_local = self.lower_iter_closure(&args[0], &[i64_ty], i64_ty, span)?;
                let res_local = self.lower_expr(&args[1])?;
                let res_ty = if self.is_result_or_option_adt(ty) && !self.is_option_adt(ty) {
                    ty
                } else {
                    let err_ty = self.tcx.dyn_error_ty();
                    let substs = gossamer_types::Substs::from_types([i64_ty, err_ty]);
                    self.tcx.intern(gossamer_types::TyKind::Adt {
                        def: gossamer_resolve::DefId::local(u32::MAX),
                        substs,
                    })
                };
                let dest = self.fresh(res_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_result_map_i64".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(res_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            _ => None,
        }
    }

    /// Method-form chain combinators on a Result/Option receiver:
    /// `x.and_then(f)` / `or_else(f)` / `filter(p)` / `ok_or_else(f)`.
    /// Mirrors the data-last free forms below: the closure crosses the
    /// C-ABI as the env-blob `lower_iter_closure` builds.
    pub(crate) fn lower_variant_chain_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        closure_arg: &HirExpr,
        receiver_ty: Ty,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let is_option = self.is_option_adt(receiver_ty);
        // `unwrap_or_else` answers the payload rather than another carrier, so
        // it takes its own destination type instead of the carrier repr the
        // chain combinators below share.
        if method.name.as_str() == "unwrap_or_else" {
            let helper = if is_option {
                "gos_rt_option_default_with"
            } else {
                "gos_rt_result_default_with"
            };
            // Option's fallback is a nullary thunk; Result's receives the
            // `Err` payload word.
            let inputs: &[Ty] = if is_option { &[] } else { &[i64_ty] };
            let payload_ty = self.enum_payload_ty(receiver_ty, 0).unwrap_or(i64_ty);
            let dest_ty = if matches!(
                self.tcx.kind_of(ty),
                TyKind::Var(_) | TyKind::Error | TyKind::Never
            ) {
                payload_ty
            } else {
                ty
            };
            let recv = self.lower_expr(receiver)?;
            // The runtime helpers hand the fallback's answer back as an integer
            // word, so a float or a wider value is produced in this frame.
            if self.tcx.elem_is_addressed_aggregate(payload_ty)
                || self.carrier_payload_is_carrier(receiver_ty)
                || matches!(self.tcx.kind_of(payload_ty), TyKind::Float(_))
            {
                let err_ty = self.enum_payload_ty(receiver_ty, 1).unwrap_or(i64_ty);
                let inputs: Vec<Ty> = if is_option { Vec::new() } else { vec![err_ty] };
                let closure = self.lower_iter_closure(closure_arg, &inputs, payload_ty, span)?;
                return Some(self.lower_unwrap_or_inline(
                    recv,
                    CarrierFallback::Call(closure),
                    receiver_ty,
                    is_option,
                    dest_ty,
                    span,
                ));
            }
            let closure = self.lower_iter_closure(closure_arg, inputs, payload_ty, span)?;
            return Some(self.emit_combinator_call(
                helper,
                vec![
                    Operand::Copy(Place::local(recv)),
                    Operand::Copy(Place::local(closure)),
                ],
                dest_ty,
                span,
            ));
        }
        // A float payload reaches a runtime predicate in an integer register,
        // so the predicate runs in this frame instead.
        if method.name.as_str() == "filter"
            && is_option
            && let Some(payload_ty) = self.enum_payload_ty(receiver_ty, 0)
            && matches!(self.tcx.kind_of(payload_ty), TyKind::Float(_))
        {
            let recv = self.lower_expr(receiver)?;
            let bool_ty = self.tcx.bool_ty();
            let closure = self.lower_iter_closure(closure_arg, &[payload_ty], bool_ty, span)?;
            return Some(self.lower_filter_inline(recv, closure, receiver_ty, span));
        }
        let helper = match (method.name.as_str(), is_option) {
            ("and_then", true) => "gos_rt_option_and_then",
            ("and_then", false) => "gos_rt_result_and_then",
            ("or_else", true) => "gos_rt_option_or_else",
            ("or_else", false) => "gos_rt_result_or_else",
            ("filter", true) => "gos_rt_option_filter",
            ("ok_or_else", _) => "gos_rt_result_ok_or_else",
            _ => return None,
        };
        // Closure parameter shape: `and_then`/`filter` and Result's
        // `or_else` receive the payload word; Option's `or_else` and
        // `ok_or_else` are nullary thunks.
        let inputs: &[Ty] = match (method.name.as_str(), is_option) {
            ("or_else", true) | ("ok_or_else", _) => &[],
            _ => &[i64_ty],
        };
        let closure_out = match method.name.as_str() {
            "filter" => self.tcx.bool_ty(),
            "ok_or_else" => i64_ty,
            _ if is_option => {
                let payload = self.enum_payload_ty(receiver_ty, 0).unwrap_or(i64_ty);
                self.option_payload_adt_ty(payload)
            }
            _ => self.result_i64_error_adt_ty(),
        };
        let recv = self.lower_expr(receiver)?;
        let closure = self.lower_iter_closure(closure_arg, inputs, closure_out, span)?;
        let checked = if matches!(
            self.tcx.kind_of(ty),
            TyKind::Var(_) | TyKind::Error | TyKind::Never
        ) {
            receiver_ty
        } else {
            ty
        };
        let dest_ty = self.result_repr_ty(checked);
        Some(self.emit_combinator_call(
            helper,
            vec![
                Operand::Copy(Place::local(recv)),
                Operand::Copy(Place::local(closure)),
            ],
            dest_ty,
            span,
        ))
    }
}
