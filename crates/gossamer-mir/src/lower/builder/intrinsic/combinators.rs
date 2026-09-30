//! Lowering combinator calls and the callback bodies they run.

use super::*;

impl<'a> Builder<'a> {
    /// Lowers the closure-taking std combinators wired natively in the
    /// Task-22 pass: `result::and_then`/`or_else`/`ok`/`err`/`is_ok`/
    /// `is_err`, the remaining `option::*` family, and the newer
    /// closure-taking `iter::*` entries. Free data-last call shapes
    /// only; returns `None` for any other name so the generic call
    /// path keeps running.
    pub(crate) fn try_lower_combinator_call(
        &mut self,
        joined: &str,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        match (joined, args.len()) {
            ("result::and_then" | "result::or_else", 2) => {
                let out_ty = self.result_i64_error_adt_ty();
                let closure = self.lower_iter_closure(&args[0], &[i64_ty], out_ty, span)?;
                let res = self.lower_expr(&args[1])?;
                let dest_ty = self.result_repr_ty(
                    if matches!(
                        self.tcx.kind_of(ty),
                        TyKind::Var(_) | TyKind::Error | TyKind::Never
                    ) {
                        args[1].ty
                    } else {
                        ty
                    },
                );
                let helper = if joined == "result::and_then" {
                    "gos_rt_result_and_then"
                } else {
                    "gos_rt_result_or_else"
                };
                Some(self.emit_combinator_call(
                    helper,
                    vec![
                        Operand::Copy(Place::local(res)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("result::ok" | "result::err", 1) => {
                let res = self.lower_expr(&args[0])?;
                let slot = usize::from(joined == "result::err");
                let payload = self.enum_payload_ty(args[0].ty, slot).unwrap_or(i64_ty);
                let dest_ty = self.option_payload_adt_ty(payload);
                let helper = if joined == "result::ok" {
                    "gos_rt_result_to_opt_ok"
                } else {
                    "gos_rt_result_to_opt_err"
                };
                Some(self.emit_combinator_call(
                    helper,
                    vec![Operand::Copy(Place::local(res))],
                    dest_ty,
                    span,
                ))
            }
            ("result::is_ok", 1) => {
                self.lower_combinator_pred_call("gos_rt_result_is_ok", args, span)
            }
            ("result::is_err", 1) => {
                self.lower_combinator_pred_call("gos_rt_result_is_err", args, span)
            }
            ("option::and_then" | "option::or_else", 2) => {
                let payload = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                let out_ty = self.option_payload_adt_ty(payload);
                let inputs: &[Ty] = if joined == "option::and_then" {
                    &[i64_ty]
                } else {
                    &[]
                };
                let closure = self.lower_iter_closure(&args[0], inputs, out_ty, span)?;
                let opt = self.lower_expr(&args[1])?;
                let helper = if joined == "option::and_then" {
                    "gos_rt_option_and_then"
                } else {
                    "gos_rt_option_or_else"
                };
                Some(self.emit_combinator_call(
                    helper,
                    vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    out_ty,
                    span,
                ))
            }
            ("option::filter", 2) => {
                let closure = self.lower_iter_closure(&args[0], &[i64_ty], bool_ty, span)?;
                let opt = self.lower_expr(&args[1])?;
                let payload = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                let dest_ty = self.option_payload_adt_ty(payload);
                Some(self.emit_combinator_call(
                    "gos_rt_option_filter",
                    vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("option::or", 2) => {
                let alt = self.lower_expr(&args[0])?;
                let opt = self.lower_expr(&args[1])?;
                let payload = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                let dest_ty = self.option_payload_adt_ty(payload);
                Some(self.emit_combinator_call(
                    "gos_rt_option_or",
                    vec![
                        Operand::Copy(Place::local(alt)),
                        Operand::Copy(Place::local(opt)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("option::ok_or_else", 2) => {
                let closure = self.lower_iter_closure(&args[0], &[], i64_ty, span)?;
                let opt = self.lower_expr(&args[1])?;
                let dest_ty = if matches!(
                    self.tcx.kind_of(ty),
                    TyKind::Var(_) | TyKind::Error | TyKind::Never
                ) {
                    let payload = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                    let substs = gossamer_types::Substs::from_types([payload, i64_ty]);
                    self.tcx.intern(TyKind::Adt {
                        def: gossamer_resolve::DefId::local(u32::MAX),
                        substs,
                    })
                } else {
                    ty
                };
                Some(self.emit_combinator_call(
                    "gos_rt_result_ok_or_else",
                    vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("sync::Once::call" | "Once::call", 2) => {
                // `Once::call(o, || ...)` - handle first, nullary closure
                // second. The closure crosses the C-ABI through the same
                // env-thunk convention as `option::unwrap_or_else`; the run
                // body's value is ignored (the i64 result is the ran flag).
                let handle = self.lower_expr(&args[0])?;
                let closure = self.lower_iter_closure(&args[1], &[], i64_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_once_call",
                    vec![
                        Operand::Copy(Place::local(handle)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("middleware::bearer_ok" | "http::middleware::bearer_ok", 2) => {
                // `bearer_ok(req, verify)` - request first, a
                // String-taking verify closure second. Mirrors the
                // VM-native `native_bearer_ok`; the closure runs on the
                // extracted Bearer token and its bool result is returned
                // (false, without calling verify, when no Bearer header
                // is present).
                let string_ty = self.tcx.string_ty();
                let req = self.lower_expr(&args[0])?;
                let verify = self.lower_iter_closure(&args[1], &[string_ty], bool_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_http_bearer_ok",
                    vec![
                        Operand::Copy(Place::local(req)),
                        Operand::Copy(Place::local(verify)),
                    ],
                    bool_ty,
                    span,
                ))
            }
            ("sync::Shared::with" | "Shared::with", 2) => {
                // `Shared::with(shared, |v| ...)` - handle first, the
                // callback second. Mirrors the VM-native
                // `native_shared_with`: the lock is held across the call
                // and the guarded value is left as it was.
                let handle = self.lower_expr(&args[0])?;
                let closure = self.lower_iter_closure(&args[1], &[i64_ty], i64_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_shared_with",
                    vec![
                        Operand::Copy(Place::local(handle)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("sync::Shared::update" | "Shared::update", 2) => {
                // The callback's answer becomes the guarded value, and the
                // lock spans both the read and the write.
                let handle = self.lower_expr(&args[0])?;
                let closure = self.lower_iter_closure(&args[1], &[i64_ty], i64_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_shared_update",
                    vec![
                        Operand::Copy(Place::local(handle)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("sync::RwLock::with_read" | "RwLock::with_read", 2) => {
                // `RwLock::with_read(lock, |v| ...)` - handle first, an
                // i64-taking closure second. Mirrors the VM-native
                // `native_rwlock_with_read`; the callback runs under a
                // read lock and its result is returned unchanged.
                let handle = self.lower_expr(&args[0])?;
                let closure = self.lower_iter_closure(&args[1], &[i64_ty], i64_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_rwlock_with_read",
                    vec![
                        Operand::Copy(Place::local(handle)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("sync::RwLock::with_write" | "RwLock::with_write", 2) => {
                // `RwLock::with_write(lock, |v| ...)` - the callback runs
                // under a write lock and its result becomes the new
                // guarded value, which is also returned.
                let handle = self.lower_expr(&args[0])?;
                let closure = self.lower_iter_closure(&args[1], &[i64_ty], i64_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_rwlock_with_write",
                    vec![
                        Operand::Copy(Place::local(handle)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("option::unwrap_or_else", 2) => {
                let payload_ty = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                if self.tcx.elem_is_addressed_aggregate(payload_ty)
                    || self.carrier_payload_is_carrier(args[1].ty)
                    || matches!(
                        self.tcx.kind_of(payload_ty),
                        gossamer_types::TyKind::Float(_)
                    )
                {
                    let closure = self.lower_iter_closure(&args[0], &[], payload_ty, span)?;
                    let opt = self.lower_expr(&args[1])?;
                    return Some(self.lower_unwrap_or_inline(
                        opt,
                        CarrierFallback::Call(closure),
                        args[1].ty,
                        true,
                        payload_ty,
                        span,
                    ));
                }
                let closure = self.lower_iter_closure(&args[0], &[], i64_ty, span)?;
                let opt = self.lower_expr(&args[1])?;
                let dest_ty = self.unwrap_default_dest_ty(ty, args[1].ty, opt);
                Some(self.emit_combinator_call(
                    "gos_rt_option_default_with",
                    vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("option::zip", 2) => {
                // The pair is built here and wrapped the way any `Some((a, b))`
                // is, so the carrier holds a copy its own release reclaims.
                let first = self.lower_expr(&args[0])?;
                let second = self.lower_expr(&args[1])?;
                let a = self.enum_payload_ty(args[0].ty, 0).unwrap_or(i64_ty);
                let b = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                let pair = self.tcx.intern(TyKind::Tuple(vec![a, b]));
                let dest_ty = self.option_payload_adt_ty(pair);
                let dest_ty = self.result_repr_ty(dest_ty);
                let result = self.fresh(dest_ty);
                let both = self.new_block(span);
                let none = self.new_block(span);
                let join = self.new_block(span);
                let check = |this: &mut Self, carrier: Local, next: BlockId| {
                    let disc = this.fresh(i64_ty);
                    this.emit_assign(
                        Place::local(disc),
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_disc",
                            args: vec![Operand::Copy(Place::local(carrier))],
                        },
                        span,
                    );
                    this.terminate(Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::local(disc)),
                        arms: vec![(0, next)],
                        default: none,
                    });
                };
                let second_check = self.new_block(span);
                check(self, first, second_check);
                self.set_current(second_check);
                check(self, second, both);
                self.set_current(both);
                let payload_of = |this: &mut Self, carrier: Local, payload_ty: Ty| {
                    let extractor = if matches!(this.tcx.kind_of(payload_ty), TyKind::Float(_)) {
                        "gos_rt_result_payload_f64"
                    } else if this.is_by_value_enum_ty(payload_ty) {
                        "gos_rt_result_payload_i128"
                    } else {
                        "gos_rt_result_payload"
                    };
                    let payload = this.fresh(payload_ty);
                    this.emit_assign(
                        Place::local(payload),
                        Rvalue::CallIntrinsic {
                            name: extractor,
                            args: vec![Operand::Copy(Place::local(carrier))],
                        },
                        span,
                    );
                    payload
                };
                let left = payload_of(self, first, a);
                let right = payload_of(self, second, b);
                let tuple = self.fresh(pair);
                self.emit_assign(
                    Place::local(tuple),
                    Rvalue::Aggregate {
                        kind: crate::ir::AggregateKind::Tuple,
                        operands: vec![
                            Operand::Copy(Place::local(left)),
                            Operand::Copy(Place::local(right)),
                        ],
                    },
                    span,
                );
                let _ = self.ensure_aggr_copy_meta(pair);
                let some_disc = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(some_disc),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    span,
                );
                self.lower_result_ctor_into(result, some_disc, tuple, span);
                self.terminate(Terminator::Goto { target: join });
                self.set_current(none);
                self.emit_assign(
                    Place::local(result),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_new",
                        args: vec![
                            Operand::Const(ConstValue::Int(1)),
                            Operand::Const(ConstValue::Int(0)),
                        ],
                    },
                    span,
                );
                self.terminate(Terminator::Goto { target: join });
                self.set_current(join);
                Some(result)
            }
            ("option::flatten", 1) => {
                let opt = self.lower_expr(&args[0])?;
                let inner = self.enum_payload_ty(args[0].ty, 0).unwrap_or(i64_ty);
                let dest_ty = self.result_repr_ty(inner);
                Some(self.emit_combinator_call(
                    "gos_rt_option_flatten",
                    vec![Operand::Copy(Place::local(opt))],
                    dest_ty,
                    span,
                ))
            }
            ("option::iter", 1) => {
                let opt = self.lower_expr(&args[0])?;
                let payload = self.enum_payload_ty(args[0].ty, 0).unwrap_or(i64_ty);
                let dest_ty = self.tcx.intern(TyKind::Vec(payload));
                Some(self.emit_combinator_call(
                    "gos_rt_option_iter",
                    vec![Operand::Copy(Place::local(opt))],
                    dest_ty,
                    span,
                ))
            }
            ("iter::filter_map" | "iter::find_map", 2) => {
                // Over a lazy stream the callback runs one element per pull, so
                // a consumer that stops early stops the calls with it.
                if joined == "iter::filter_map"
                    && matches!(self.tcx.kind_of(ty), TyKind::Iterator(_))
                    && let Some(source) = self.lazy_iter_source_family_word(args[1].ty)
                    && source != LazyElemFamily::Float
                    && let Some(payload_ty) = self
                        .callable_output_of(&args[0])
                        .and_then(|out| self.option_payload_of(out))
                    && let Some(helper) = match self.lazy_iter_elem_family(payload_ty) {
                        Some(LazyElemFamily::Word) => Some("gos_rt_lazy_iter_filter_map_i64"),
                        Some(LazyElemFamily::Ptr) => Some("gos_rt_lazy_iter_filter_map_str"),
                        _ => None,
                    }
                {
                    let (in_ty, _) = self.iter_elem_abi(args[1].ty);
                    let opt_payload = self.option_payload_adt_ty(payload_ty);
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[in_ty], opt_payload, span)?;
                    let iter_local = self.lower_lazy_iter_source(&args[1])?;
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
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                // The kept payloads are the callback's results, so the result
                // element is the callback's Some payload, not the input's.
                let payload_ty = self
                    .callable_output_of(&args[0])
                    .and_then(|out| self.option_payload_of(out))
                    .unwrap_or(i64_ty);
                let opt_payload = self.option_payload_adt_ty(payload_ty);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], opt_payload, span)?;
                if joined == "iter::filter_map" {
                    let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Vec(_)) {
                        ty
                    } else {
                        self.tcx.intern(TyKind::Vec(payload_ty))
                    };
                    // A kept payload wider than a slot is answered as the
                    // address of its storage, so the shim copies the block
                    // rather than storing the word.
                    let width = i128::from(self.elem_bytes_of(payload_ty));
                    let by_block = i128::from(self.elem_is_slot_addressed(payload_ty));
                    let kept = self.emit_iter_combinator_call(
                        "filter_map",
                        elem_abi,
                        None,
                        vec![
                            Operand::Copy(Place::local(closure)),
                            Operand::Copy(Place::local(vec_local)),
                            Operand::Const(ConstValue::Int(width)),
                            Operand::Const(ConstValue::Int(by_block)),
                        ],
                        dest_ty,
                        span,
                    );
                    // Each kept payload is the share its `Some` carried, which
                    // the result now holds.
                    self.tag_owned_elements(kept, payload_ty, span);
                    return Some(kept);
                }
                Some(self.emit_iter_combinator_call(
                    "find_map",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    opt_payload,
                    span,
                ))
            }
            ("iter::flat_map", 2) => {
                let vec_i64 = self.tcx.intern(TyKind::Vec(i64_ty));
                // A callback returning a fixed-size array hands back a
                // raw slot buffer with no GosVec header; route those
                // through the arr-variant shim with the static length.
                let arr_len = match self.callable_output_of(&args[0]) {
                    Some(out) => match self.tcx.kind_of(out) {
                        TyKind::Array { len, .. } => Some(*len),
                        _ => None,
                    },
                    None => None,
                };
                let cb_out = match arr_len {
                    Some(_) => self
                        .callable_output_of(&args[0])
                        .expect("arr_len derived from callable output"),
                    None => vec_i64,
                };
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], cb_out, span)?;
                // The concatenation carries the callback's element, not the
                // one that was read.
                let inner = self
                    .callable_output_of(&args[0])
                    .and_then(|out| self.sequence_elem_ty_of(out));
                let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Vec(_)) {
                    ty
                } else {
                    inner.map_or(vec_i64, |elem| self.tcx.intern(TyKind::Vec(elem)))
                };
                let mut call_args = vec![
                    Operand::Copy(Place::local(closure)),
                    Operand::Copy(Place::local(vec_local)),
                ];
                // A callback answering a fixed array hands back a raw slot
                // buffer with no header rather than a sequence, so the array
                // variant reads that buffer directly. The element it was
                // called on still crosses in its own class.
                if let Some(len) = arr_len {
                    let len_local = self.fresh(i64_ty);
                    let len_i128 = i128::try_from(len.to_usize()).unwrap_or(0);
                    self.emit_assign(
                        Place::local(len_local),
                        Rvalue::Use(Operand::Const(ConstValue::Int(len_i128))),
                        span,
                    );
                    call_args.push(Operand::Copy(Place::local(len_local)));
                    return Some(self.emit_iter_combinator_call(
                        "flat_map_arr",
                        elem_abi,
                        None,
                        call_args,
                        dest_ty,
                        span,
                    ));
                }
                Some(self.emit_iter_combinator_call(
                    "flat_map", elem_abi, None, call_args, dest_ty, span,
                ))
            }
            ("iter::reduce", 2) => {
                // The accumulator is an element, so the callback takes two of
                // them and answers one, and the Option carries that element.
                let (elem_ty, _) = self.iter_elem_abi(args[1].ty);
                let closure =
                    self.lower_iter_closure(&args[0], &[elem_ty, elem_ty], elem_ty, span)?;
                let result_ty = self.option_payload_adt_ty(elem_ty);
                self.lower_fold_loop(
                    FoldSeed::FirstElement(result_ty),
                    closure,
                    &args[1],
                    elem_ty,
                    elem_ty,
                    span,
                )
            }
            ("iter::scan", 3) => {
                // Each accumulator the callback answers is kept in the result,
                // so the loop pushes it there and replaces its own copy.
                let init = self.lower_expr(&args[0])?;
                let acc_ty = self.locals[init.0 as usize].ty;
                let (elem_ty, _) = self.iter_elem_abi(args[2].ty);
                let closure =
                    self.lower_iter_closure(&args[1], &[acc_ty, elem_ty], acc_ty, span)?;
                self.lower_scan_loop(init, closure, &args[2], acc_ty, elem_ty, span)
            }
            ("iter::product_by", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], i64_ty, span)?;
                Some(self.emit_iter_combinator_call(
                    "product_by",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("iter::position", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], bool_ty, span)?;
                // The answer is an index whatever the element was.
                let dest_ty = self.option_payload_adt_ty(i64_ty);
                Some(self.emit_iter_combinator_call(
                    "position",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::take_while" | "iter::skip_while", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], bool_ty, span)?;
                let dest_ty = self.iter_result_vec_ty(ty, vec_local);
                let combinator = if joined == "iter::take_while" {
                    "take_while"
                } else {
                    "skip_while"
                };
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::partition", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], bool_ty, span)?;
                let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Tuple(_)) {
                    ty
                } else {
                    let unit_ty = self.tcx.unit();
                    let side = self.iter_result_vec_ty(unit_ty, vec_local);
                    self.tcx.intern(TyKind::Tuple(vec![side, side]))
                };
                Some(self.emit_iter_combinator_call(
                    "partition",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::sort_by" | "iter::min_by" | "iter::max_by", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                // The comparator takes two elements and answers an ordering,
                // so both operands ride the element's own register class.
                let closure = self.lower_iter_closure(&args[0], &[in_ty, in_ty], i64_ty, span)?;
                let unit_ty = self.tcx.unit();
                let elem = self.iter_result_elem_ty(unit_ty, vec_local);
                let (combinator, dest_ty) = match joined {
                    "iter::sort_by" => {
                        let dest = self.iter_result_vec_ty(ty, vec_local);
                        ("sort_by", dest)
                    }
                    "iter::min_by" => ("min_by", self.option_payload_adt_ty(elem)),
                    _ => ("max_by", self.option_payload_adt_ty(elem)),
                };
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::sort_by_key" | "iter::min_by_key" | "iter::max_by_key", 2) => {
                if joined != "iter::sort_by_key"
                    && let Some(dest) = self.try_lower_map_select_by_key(
                        joined == "iter::max_by_key",
                        &args[0],
                        &args[1],
                        span,
                    )
                {
                    return Some(dest);
                }
                if joined != "iter::sort_by_key"
                    && let Some(key_ty) = self.callable_output_of(&args[0])
                    && !self.key_orders_as_word(key_ty)
                {
                    let (elem_ty, _) = self.iter_elem_abi(args[1].ty);
                    let closure = self.lower_iter_closure(&args[0], &[elem_ty], key_ty, span)?;
                    return self.lower_select_by_key_loop(
                        closure,
                        &args[1],
                        elem_ty,
                        key_ty,
                        joined == "iter::max_by_key",
                        span,
                    );
                }
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                // The element and the key each pick their own register class,
                // so the callback must be built with, and called through, the
                // exact pair: a float in either position rides an SSE register
                // that an integer-shaped signature never fills.
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let key_ty = self.callable_output_of(&args[0]).unwrap_or(i64_ty);
                let key_is_f64 = matches!(self.tcx.kind_of(key_ty), TyKind::Float(_));
                let closure_ret = if key_is_f64 {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else {
                    i64_ty
                };
                let closure = self.lower_iter_closure(&args[0], &[in_ty], closure_ret, span)?;
                let unit_ty = self.tcx.unit();
                let elem = self.iter_result_elem_ty(unit_ty, vec_local);
                let (combinator, dest_ty) = match joined {
                    "iter::sort_by_key" => {
                        let dest = self.iter_result_vec_ty(ty, vec_local);
                        ("sort_by_key", dest)
                    }
                    "iter::min_by_key" => ("min_by_key", self.option_payload_adt_ty(elem)),
                    _ => ("max_by_key", self.option_payload_adt_ty(elem)),
                };
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                        Operand::Const(ConstValue::Int(i128::from(key_is_f64))),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::chunk_by" | "iter::count_by", 2) => {
                // The runtime shims group by the key's word, which is the key
                // for every scalar; a `String`, an aggregate, and a payload enum
                // group by value.
                if let Some(key_ty) = self.callable_output_of(&args[0])
                    && (self.is_aggregate_key(key_ty)
                        || (self.struct_name_of(key_ty).is_none()
                            && self.ensure_enum_eq_desc(key_ty).is_some())
                        || matches!(self.tcx.kind_of(key_ty), TyKind::String))
                {
                    let (elem_ty, _) = self.iter_elem_abi(args[1].ty);
                    let closure = self.lower_iter_closure(&args[0], &[elem_ty], key_ty, span)?;
                    return self.lower_group_by_key_loop(
                        closure,
                        &args[1],
                        elem_ty,
                        key_ty,
                        joined == "iter::count_by",
                        span,
                    );
                }
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], i64_ty, span)?;
                let (combinator, dest_ty) = if joined == "iter::chunk_by" {
                    let dest = if matches!(self.tcx.kind_of(ty), TyKind::HashMap { .. }) {
                        ty
                    } else {
                        // The groups hold the source's own elements.
                        let unit_ty = self.tcx.unit();
                        let group = self.iter_result_vec_ty(unit_ty, vec_local);
                        self.tcx.intern(TyKind::HashMap {
                            key: i64_ty,
                            value: group,
                            ordered: false,
                        })
                    };
                    ("chunk_by", dest)
                } else {
                    let dest = if matches!(self.tcx.kind_of(ty), TyKind::HashMap { .. }) {
                        ty
                    } else {
                        self.tcx.intern(TyKind::HashMap {
                            key: i64_ty,
                            value: i64_ty,
                            ordered: false,
                        })
                    };
                    ("count_by", dest)
                };
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            _ => None,
        }
    }

    /// Declared output type of a callable-shaped argument expression
    /// (`FnPtr` / `FnTrait` sig, or a lifted closure's registered
    /// return type), or `None` when the shape is unknown.
    pub(super) fn callable_output_of(&self, arg: &HirExpr) -> Option<Ty> {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(arg.ty) {
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => Some(sig.output),
            TyKind::FnDef { def, .. } => self.fn_returns.get(def).copied(),
            _ => None,
        }
    }

    /// `substs[idx]` of a Result/Option-shaped `ty`, ref-transparent;
    /// `None` when the type is not a resolved enum Adt or the payload
    /// slot is still an inference Var.
    pub(crate) fn enum_payload_ty(&self, ty: Ty, idx: usize) -> Option<Ty> {
        use gossamer_types::TyKind;
        let mut resolved = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(resolved) {
            resolved = *inner;
        }
        match self.tcx.kind_of(resolved) {
            TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => {
                let payload = substs.types().get(idx).copied()?;
                if matches!(
                    self.tcx.kind_of(payload),
                    TyKind::Var(_) | TyKind::Error | TyKind::Never
                ) {
                    None
                } else {
                    Some(payload)
                }
            }
            _ => None,
        }
    }

    /// Emits a call to `helper` and returns the destination local.
    pub(crate) fn emit_combinator_call(
        &mut self,
        helper: &str,
        args: Vec<Operand>,
        dest_ty: Ty,
        span: Span,
    ) -> Local {
        // A shim that declares an element crossing has a family of twins its
        // C-ABI signature cannot tell apart, so the one to call follows from
        // the element class rather than from a name written at the call site.
        debug_assert!(
            gossamer_abi::combinator_abi_of(helper).is_none(),
            "{helper} declares an element crossing, so it must be reached \
             through emit_iter_combinator_call, which derives the symbol from \
             the class present at the call site"
        );
        self.emit_combinator_call_raw(helper, args, dest_ty, span)
    }

    /// Emits a sequence-combinator call, choosing the shim from the element
    /// crossing present rather than from a name spelled at the call site.
    ///
    /// `elem` is the class of the sequence being read and `result` the class
    /// of the element produced, for the combinators whose symbol distinguishes
    /// one. A crossing the registry declares no shim for is a gap in the shim
    /// family, and it is named here rather than reaching the linker.
    /// `m.iter().min_by_key(f)` / `max_by_key(f)` over a map of scalar values,
    /// with `f` a lifted closure whose body has no observable effect: the
    /// runtime finds the entry in one pass over the table, ties going to the
    /// smallest map key, which is the entry a walk in key order keeps. The
    /// winner lands in a one-pair vec that the ordinary combinator turns into
    /// the `Option`, so the result is built exactly as the general path builds
    /// it. `None`, having emitted nothing, leaves the call to that path.
    pub(super) fn try_lower_map_select_by_key(
        &mut self,
        want_max: bool,
        callback: &HirExpr,
        source: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let HirExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &source.kind
        else {
            return None;
        };
        if name.name != "iter" || !args.is_empty() {
            return None;
        }
        // A closure that captures nothing reaches here as the path to its
        // lifted body; one lowered before lifting settled still names it.
        let body_name = match &callback.kind {
            HirExprKind::Path { segments, .. } if segments.len() == 1 => &segments[0].name,
            HirExprKind::LiftedClosure { name, captures } if captures.is_empty() => &name.name,
            _ => return None,
        };
        let reads_key = *self.effect_free_pair_keys.get(body_name)?;
        let mut recv_ty = self
            .receiver_local_from_path(receiver)
            .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv_ty) {
            recv_ty = *inner;
        }
        if !matches!(self.tcx.kind_of(recv_ty), TyKind::HashMap { .. }) {
            return None;
        }
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let (declared_key, declared_val) = self.hash_map_kv_tys(recv_ty)?;
        // A string key the callback never reads is handed over as a null word,
        // which is only sound when the body is known not to touch it.
        let key_ty = match self.hash_map_key_kind(recv_ty) {
            Some(MapKeyKind::String) if !reads_key => self.tcx.string_ty(),
            Some(MapKeyKind::I64) => declared_key,
            _ => return None,
        };
        if !matches!(self.hash_map_value_kind(recv_ty), Some(MapValueKind::I64)) {
            return None;
        }
        let tuple_ty = self.tcx.intern(TyKind::Tuple(vec![key_ty, declared_val]));
        if self.elem_bytes_of(tuple_ty) != 16 {
            return None;
        }
        let vec_ty = self.tcx.intern(TyKind::Vec(tuple_ty));
        let (_, elem_abi) = self.iter_elem_abi(vec_ty);
        if !matches!(elem_abi, ElemAbi::Ptr) {
            return None;
        }
        let key_out = self.callable_output_of(callback).unwrap_or(i64_ty);
        let key_is_f64 = matches!(self.tcx.kind_of(key_out), TyKind::Float(_));
        let closure_ret = if key_is_f64 {
            self.tcx.float_ty(gossamer_types::FloatTy::F64)
        } else {
            i64_ty
        };

        let recv_local = self.lower_expr(receiver)?;
        let elem_bytes = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes),
            Rvalue::Use(Operand::Const(ConstValue::Int(16))),
            span,
        );
        let winner = self.emit_combinator_call_raw(
            "Vec::new",
            vec![Operand::Copy(Place::local(elem_bytes))],
            vec_ty,
            span,
        );
        let (in_ty, _) = self.iter_callback_shape(winner);
        let closure = self.lower_iter_closure(callback, &[in_ty], closure_ret, span)?;
        let unit_ty = self.tcx.unit();
        let _ = self.emit_combinator_call_raw(
            "gos_rt_map_select_by_key_into",
            vec![
                Operand::Copy(Place::local(recv_local)),
                Operand::Copy(Place::local(winner)),
                Operand::Copy(Place::local(closure)),
                Operand::Const(ConstValue::Int(i128::from(key_is_f64))),
                Operand::Const(ConstValue::Int(i128::from(want_max))),
            ],
            unit_ty,
            span,
        );
        let elem = self.iter_result_elem_ty(unit_ty, winner);
        let dest_ty = self.option_payload_adt_ty(elem);
        Some(self.emit_iter_combinator_call(
            if want_max { "max_by_key" } else { "min_by_key" },
            elem_abi,
            None,
            vec![
                Operand::Copy(Place::local(closure)),
                Operand::Copy(Place::local(winner)),
                Operand::Const(ConstValue::Int(i128::from(key_is_f64))),
            ],
            dest_ty,
            span,
        ))
    }

    pub(crate) fn emit_iter_combinator_call(
        &mut self,
        combinator: &str,
        elem: ElemAbi,
        result: Option<ElemAbi>,
        args: Vec<Operand>,
        dest_ty: Ty,
        span: Span,
    ) -> Local {
        let helper = iter_combinator_helper(combinator, elem, result);
        self.emit_combinator_call_raw(helper, args, dest_ty, span)
    }

    pub(super) fn emit_combinator_call_raw(
        &mut self,
        helper: &str,
        args: Vec<Operand>,
        dest_ty: Ty,
        span: Span,
    ) -> Local {
        let dest = self.fresh(dest_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(helper.to_string())),
            args,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }

    /// Lowers a 1-arg Result/Option predicate (`is_ok` / `is_some` /
    /// ...) with a bool-typed destination so `{}` prints true/false on
    /// every tier.
    pub(super) fn lower_combinator_pred_call(
        &mut self,
        helper: &'static str,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        let v = self.lower_expr(&args[0])?;
        let bool_ty = self.tcx.bool_ty();
        Some(self.emit_combinator_call(helper, vec![Operand::Copy(Place::local(v))], bool_ty, span))
    }

    pub(crate) fn lower_iter_simple_vec_i64(
        &mut self,
        helper: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let v = self.lower_iter_vec_arg(&args[0])?;
        let dest = self.fresh(i64_ty);
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

    /// Like [`Self::lower_iter_simple_vec_i64`] but pins the dest as a
    /// boxed `Option<i64>` (the 16-byte Result/Option ABI), for
    /// terminals such as `iter::min` / `iter::max` whose Gossamer type
    /// is `Option<i64>`. The matching shim returns an i128-packed
    /// Option (None = 1, Some(m) = `gos_rt_result_new(0, m)`).
    /// Lowers `iter::min` / `iter::max` over a sequence to the shim matching
    /// the element type, so a float payload comes back as a float rather than
    /// as the integer its bits spell.
    pub(crate) fn lower_iter_simple_vec_opt(
        &mut self,
        combinator: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        let v = self.lower_iter_vec_arg(&args[0])?;
        self.lower_iter_vec_opt_local(v, combinator, span)
    }

    /// `min` / `max` over an already-lowered sequence local, returning
    /// `Option<T>` typed from the local's own element type.
    pub(crate) fn lower_iter_vec_opt_local(
        &mut self,
        v: Local,
        combinator: &str,
        span: Span,
    ) -> Option<Local> {
        let seq_ty = self.locals[v.0 as usize].ty;
        let (elem_ty, elem_abi) = self.iter_elem_abi(seq_ty);
        // An `f32` keeps its own type: its slot is double width either way,
        // and the type is what renders its single-precision digits.
        let payload_ty = match elem_abi {
            ElemAbi::Float
                if matches!(
                    self.iter_element_kind(seq_ty),
                    Some(gossamer_types::TyKind::Float(gossamer_types::FloatTy::F32))
                ) =>
            {
                self.tcx.float_ty(gossamer_types::FloatTy::F32)
            }
            ElemAbi::Float => self.tcx.float_ty(gossamer_types::FloatTy::F64),
            _ => elem_ty,
        };
        let opt_ty = self.option_payload_adt_ty(payload_ty);
        Some(self.emit_iter_combinator_call(
            combinator,
            elem_abi,
            None,
            vec![Operand::Copy(Place::local(v))],
            opt_ty,
            span,
        ))
    }

    pub(crate) fn lower_iter_simple_vec_in_vec_out(
        &mut self,
        combinator: &str,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let v = self.lower_iter_vec_arg(&args[0])?;
        // Pin the dest to `Vec<elem>` (never the call's raw Array/Var
        // type): the shim returns a heap `*mut GosVec`, so an
        // unannotated `iter::rev(xs)[i]` would otherwise take the
        // stack-array index path on a heap pointer and SIGSEGV.
        let vec_ty = self.iter_result_vec_ty(ty, v);
        Some(self.emit_iter_combinator_call(
            combinator,
            ElemAbi::Word,
            None,
            vec![Operand::Copy(Place::local(v))],
            vec_ty,
            span,
        ))
    }

    /// How a combinator's callback receives one element of an eager sequence.
    ///
    /// The runtime reads a slot either as a word, as a double, or - when the
    /// element is wider than one slot - as the address of its storage. The
    /// helper name and the callback's parameter type both follow from this,
    /// so they are decided together and never drift apart.
    /// The body a callback argument names when the compiler can see it: a
    /// plain function, or a lifted closure that captured nothing and so needs
    /// no environment. `None` for anything reached through a value.
    pub(super) fn direct_callback_body(&mut self, callback: &HirExpr) -> Option<String> {
        match &callback.kind {
            HirExprKind::LiftedClosure { name, captures } if captures.is_empty() => {
                Some(name.name.clone())
            }
            _ => None,
        }
    }

    /// `fold` and `reduce` as a loop in this body: each element is pulled from
    /// the source, handed to the callback with the accumulator, and the
    /// callback's answer replaces the accumulator.
    ///
    /// The accumulator is an ordinary local, so every value it holds is owned
    /// the way any local's is: the seed is copied in, each replaced
    /// accumulator is released when the next one is assigned, and the result
    /// leaves the loop as the local's value. A lazy source is pulled one
    /// element per turn, so its adapters run interleaved with the callback.
    pub(super) fn lower_fold_loop(
        &mut self,
        seed: FoldSeed,
        closure: Local,
        source: &HirExpr,
        acc_ty: Ty,
        elem_ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let pull = self.fold_source_pull(source, elem_ty, span)?;
        let acc = self.push_local(acc_ty, None, true);
        let exit = self.new_block(span);
        let (result, empty) = match seed {
            FoldSeed::Init(init) => {
                self.seed_accumulator(acc, init, span);
                (None, exit)
            }
            FoldSeed::FirstElement(result_ty) => {
                let result_ty = self.result_repr_ty(result_ty);
                let result = self.fresh(result_ty);
                let empty = self.new_block(span);
                // A pulled element is a share of its own, so it becomes the
                // accumulator where it lands; a container keeps the copy
                // `seed_accumulator` gives it.
                let into = self
                    .accumulator_clone_symbol(acc_ty)
                    .is_none()
                    .then_some(acc);
                let first = self.emit_fold_pull(&pull, elem_ty, empty, into, span);
                if first != acc {
                    self.seed_accumulator(acc, first, span);
                }
                (Some(result), empty)
            }
        };
        let header = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });
        self.set_current(header);
        let elem = self.emit_fold_pull(&pull, elem_ty, exit, None, span);
        let acc_arg = self.fold_callback_arg(acc, span);
        let elem_arg = self.fold_callback_arg(elem, span);
        let replaced = self.fresh(acc_ty);
        let after_call = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Copy(Place::local(closure)),
            args: vec![
                Operand::Copy(Place::local(acc_arg)),
                Operand::Copy(Place::local(elem_arg)),
            ],
            destination: Place::local(replaced),
            target: Some(after_call),
        });
        self.set_current(after_call);
        self.emit_assign(
            Place::local(acc),
            Rvalue::Use(Operand::Copy(Place::local(replaced))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        let Some(result) = result else {
            self.fresh_loop_results.insert(acc);
            return Some(acc);
        };
        // Both arms build the carrier straight into the result, so the
        // payload it carries is the result's own to hand over.
        let join = self.new_block(span);
        let some_disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(some_disc),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        self.lower_result_ctor_into(result, some_disc, acc, span);
        self.terminate(Terminator::Goto { target: join });

        self.set_current(empty);
        self.emit_assign(
            Place::local(result),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Const(ConstValue::Int(1)),
                    Operand::Const(ConstValue::Int(0)),
                ],
            },
            span,
        );
        self.terminate(Terminator::Goto { target: join });
        self.set_current(join);
        Some(result)
    }

    /// Lowers a loop's element source: lazy state pulled one element per turn,
    /// or a sequence read by index.
    pub(super) fn fold_source_pull(
        &mut self,
        source: &HirExpr,
        elem_ty: Ty,
        span: Span,
    ) -> Option<FoldPull> {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        if self.lazy_iter_source_family(source.ty).is_some()
            && matches!(self.tcx.kind_of(source.ty), TyKind::Iterator(_))
        {
            let state = self.lower_lazy_iter_source_aggr(source)?;
            let next_symbol = if self.local_aggr_iter.contains(&state) {
                "gos_rt_lazy_iter_next_i64"
            } else {
                self.lazy_iter_next_symbol(elem_ty)?
            };
            return Some(FoldPull::Lazy { state, next_symbol });
        }
        let vec = self.lower_iter_vec_arg(source)?;
        let len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(vec))],
            i64_ty,
            span,
        );
        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        Some(FoldPull::Vec { vec, len, counter })
    }

    /// Whether a key's slot word is its own order, which is what the word-key
    /// combinator shims compare. A `String`, an aggregate, a sequence, a
    /// payload enum, and an unsigned word order by value instead.
    pub(super) fn key_orders_as_word(&self, key_ty: Ty) -> bool {
        use gossamer_types::{IntTy, TyKind};
        match self.tcx.kind_of(key_ty) {
            TyKind::Int(IntTy::U64 | IntTy::Usize) => false,
            TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_) => true,
            TyKind::Adt { def, .. } => {
                self.tcx.is_inline_enum_ty(key_ty)
                    && self
                        .tcx
                        .enum_variant_tys(*def)
                        .is_some_and(|variants| variants.iter().all(Vec::is_empty))
            }
            _ => false,
        }
    }

    /// Calls a key callback on `elem`, answering the key.
    pub(super) fn emit_key_call(
        &mut self,
        closure: Local,
        elem: Local,
        key_ty: Ty,
        span: Span,
    ) -> Local {
        let arg = self.fold_callback_arg(elem, span);
        let key = self.fresh(key_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Copy(Place::local(closure)),
            args: vec![Operand::Copy(Place::local(arg))],
            destination: Place::local(key),
            target: Some(next),
        });
        self.set_current(next);
        key
    }

    /// `min_by_key` / `max_by_key` over a key that orders by value: each
    /// element's key is compared with the best one so far through the key
    /// type's ordering descriptor, and the first element holding the least (or
    /// greatest) key is answered.
    pub(super) fn lower_select_by_key_loop(
        &mut self,
        closure: Local,
        source: &HirExpr,
        elem_ty: Ty,
        key_ty: Ty,
        greatest: bool,
        span: Span,
    ) -> Option<Local> {
        let desc: String = self
            .ordering_stream(key_ty)?
            .iter()
            .map(|&b| b as char)
            .collect();
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let pull = self.fold_source_pull(source, elem_ty, span)?;
        let result_ty = self.option_payload_adt_ty(elem_ty);
        let result_ty = self.result_repr_ty(result_ty);
        let result = self.fresh(result_ty);
        let best = self.push_local(elem_ty, None, true);
        let best_key = self.push_local(key_ty, None, true);
        let empty = self.new_block(span);
        let exit = self.new_block(span);
        let first = self.emit_fold_pull(&pull, elem_ty, empty, None, span);
        self.emit_assign(
            Place::local(best),
            Rvalue::Use(Operand::Copy(Place::local(first))),
            span,
        );
        let first_key = self.emit_key_call(closure, best, key_ty, span);
        self.emit_assign(
            Place::local(best_key),
            Rvalue::Use(Operand::Copy(Place::local(first_key))),
            span,
        );
        let header = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });
        self.set_current(header);
        let elem = self.emit_fold_pull(&pull, elem_ty, exit, None, span);
        let key = self.emit_key_call(closure, elem, key_ty, span);
        let key_slots = self.ordered_value_slots(key, key_ty, span);
        let best_slots = self.ordered_value_slots(best_key, key_ty, span);
        let order = self.emit_combinator_call(
            "gos_rt_desc_cmp",
            vec![
                Operand::Copy(Place::local(key_slots)),
                Operand::Copy(Place::local(best_slots)),
                Operand::Const(ConstValue::Str(desc)),
            ],
            i64_ty,
            span,
        );
        let replaces = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(replaces),
            Rvalue::BinaryOp {
                op: if greatest { BinOp::Gt } else { BinOp::Lt },
                lhs: Operand::Copy(Place::local(order)),
                rhs: Operand::Const(ConstValue::Int(0)),
            },
            span,
        );
        let replace = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(replaces)),
            arms: vec![(0, header)],
            default: replace,
        });
        self.set_current(replace);
        self.emit_assign(
            Place::local(best),
            Rvalue::Use(Operand::Copy(Place::local(elem))),
            span,
        );
        match self.accumulator_clone_symbol(key_ty) {
            // The comparison read this turn's container key, which stays this
            // turn's to reclaim, so the best key takes storage of its own.
            Some(symbol) => {
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(symbol.to_string())),
                    args: vec![Operand::Copy(Place::local(key))],
                    destination: Place::local(best_key),
                    target: Some(next),
                });
                self.set_current(next);
            }
            None => self.emit_assign(
                Place::local(best_key),
                Rvalue::Use(Operand::Copy(Place::local(key))),
                span,
            ),
        }
        self.terminate(Terminator::Goto { target: header });

        let join = self.new_block(span);
        self.set_current(exit);
        let some_disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(some_disc),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        self.lower_result_ctor_into(result, some_disc, best, span);
        self.terminate(Terminator::Goto { target: join });
        self.set_current(empty);
        self.emit_assign(
            Place::local(result),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Const(ConstValue::Int(1)),
                    Operand::Const(ConstValue::Int(0)),
                ],
            },
            span,
        );
        self.terminate(Terminator::Goto { target: join });
        self.set_current(join);
        Some(result)
    }

    /// `count_by` / `chunk_by` over a key that is hashed by value: each
    /// element's key reaches the map through the key type's descriptor, so
    /// equal keys built at different allocations name one entry.
    pub(super) fn lower_group_by_key_loop(
        &mut self,
        closure: Local,
        source: &HirExpr,
        elem_ty: Ty,
        key_ty: Ty,
        counting: bool,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        // A `String` key is its text, which the typed string-keyed entries hash
        // directly; an aggregate or enum key reaches the map through its
        // descriptor.
        let (suffix, desc) = if matches!(self.tcx.kind_of(key_ty), TyKind::String) {
            ("typed_str_i64", None)
        } else {
            match self.ensure_enum_eq_desc(key_ty) {
                Some(desc) if self.struct_name_of(key_ty).is_none() => ("ekey", Some(desc)),
                _ => ("skey", Some(self.key_descriptor(key_ty)?)),
            }
        };
        let group_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let value_ty = if counting { i64_ty } else { group_ty };
        let map_ty = self.tcx.intern(TyKind::HashMap {
            key: key_ty,
            value: value_ty,
            ordered: false,
        });
        let map = self.emit_combinator_call("Map::new", Vec::new(), map_ty, span);
        if !counting {
            let unit = self.tcx.unit();
            let sink = self.fresh(unit);
            self.emit_assign(
                Place::local(sink),
                Rvalue::CallIntrinsic {
                    name: "gos_rt_map_set_vec_values",
                    args: vec![Operand::Copy(Place::local(map))],
                },
                span,
            );
        }
        let pull = self.fold_source_pull(source, elem_ty, span)?;
        let exit = self.new_block(span);
        let header = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });
        self.set_current(header);
        let elem = self.emit_fold_pull(&pull, elem_ty, exit, None, span);
        let key = self.emit_key_call(closure, elem, key_ty, span);
        let desc_op = desc.map(|desc| Operand::Const(ConstValue::Str(desc)));
        if counting {
            let mut inc_args = vec![
                Operand::Copy(Place::local(map)),
                Operand::Copy(Place::local(key)),
            ];
            inc_args.extend(desc_op);
            inc_args.push(Operand::Const(ConstValue::Int(1)));
            self.emit_combinator_call(&format!("gos_rt_map_inc_{suffix}"), inc_args, i64_ty, span);
        } else {
            let bytes = i128::from(self.elem_bytes_of(elem_ty).max(1));
            let fresh_group = self.emit_combinator_call(
                "Vec::new",
                vec![Operand::Const(ConstValue::Int(bytes))],
                group_ty,
                span,
            );
            let mut insert_args = vec![
                Operand::Copy(Place::local(map)),
                Operand::Copy(Place::local(key)),
            ];
            insert_args.extend(desc_op);
            insert_args.push(Operand::Copy(Place::local(fresh_group)));
            let group = self.emit_combinator_call(
                &format!("gos_rt_map_or_insert_{suffix}"),
                insert_args,
                group_ty,
                span,
            );
            let unit = self.tcx.unit();
            self.emit_combinator_call(
                "gos_rt_vec_push",
                vec![
                    Operand::Copy(Place::local(group)),
                    Operand::Copy(Place::local(elem)),
                ],
                unit,
                span,
            );
        }
        self.terminate(Terminator::Goto { target: header });
        self.set_current(exit);
        Some(map)
    }

    /// Gives a fold's accumulator `acc` its starting value `seed`. A container
    /// is copied into storage of its own, so replacing the accumulator frees
    /// only what the fold itself holds.
    pub(super) fn seed_accumulator(&mut self, acc: Local, seed: Local, span: Span) {
        let ty = self.locals[acc.0 as usize].ty;
        match self.accumulator_clone_symbol(ty) {
            Some(symbol) => {
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(symbol.to_string())),
                    args: vec![Operand::Copy(Place::local(seed))],
                    destination: Place::local(acc),
                    target: Some(next),
                });
                self.set_current(next);
            }
            None => self.emit_assign(
                Place::local(acc),
                Rvalue::Use(Operand::Copy(Place::local(seed))),
                span,
            ),
        }
    }

    /// The runtime copy a container accumulator of type `ty` starts from.
    pub(super) fn accumulator_clone_symbol(&self, ty: Ty) -> Option<&'static str> {
        if matches!(
            self.tcx.kind_of(ty),
            gossamer_types::TyKind::Vec(_) | gossamer_types::TyKind::Slice(_)
        ) {
            return Some("gos_rt_vec_clone");
        }
        self.map_or_set_clone_symbol(ty)
    }

    /// `scan`: a fold whose every accumulator is also pushed onto the result,
    /// in order.
    pub(super) fn lower_scan_loop(
        &mut self,
        init: Local,
        closure: Local,
        source: &HirExpr,
        acc_ty: Ty,
        elem_ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let pull = self.fold_source_pull(source, elem_ty, span)?;
        let out_ty = self.tcx.intern(TyKind::Vec(acc_ty));
        let bytes = i128::from(self.elem_bytes_of(acc_ty).max(1));
        let out = self.emit_combinator_call(
            "Vec::new",
            vec![Operand::Const(ConstValue::Int(bytes))],
            out_ty,
            span,
        );
        let acc = self.push_local(acc_ty, None, true);
        self.seed_accumulator(acc, init, span);
        let exit = self.new_block(span);
        let header = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });
        self.set_current(header);
        let elem = self.emit_fold_pull(&pull, elem_ty, exit, None, span);
        let acc_arg = self.fold_callback_arg(acc, span);
        let elem_arg = self.fold_callback_arg(elem, span);
        let replaced = self.fresh(acc_ty);
        let after_call = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Copy(Place::local(closure)),
            args: vec![
                Operand::Copy(Place::local(acc_arg)),
                Operand::Copy(Place::local(elem_arg)),
            ],
            destination: Place::local(replaced),
            target: Some(after_call),
        });
        self.set_current(after_call);
        self.emit_assign(
            Place::local(acc),
            Rvalue::Use(Operand::Copy(Place::local(replaced))),
            span,
        );
        let unit = self.tcx.unit();
        self.emit_combinator_call(
            "gos_rt_vec_push",
            vec![
                Operand::Copy(Place::local(out)),
                Operand::Copy(Place::local(acc)),
            ],
            unit,
            span,
        );
        self.terminate(Terminator::Goto { target: header });
        self.set_current(exit);
        Some(out)
    }

    /// Pulls the next element of a fold's source, branching to `done` when
    /// there is none, and answers the element in the shape a callback reads.
    /// A lazily pulled element lands in `into` when one is given.
    pub(super) fn emit_fold_pull(
        &mut self,
        pull: &FoldPull,
        elem_ty: Ty,
        done: BlockId,
        into: Option<Local>,
        span: Span,
    ) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let body = self.new_block(span);
        match *pull {
            FoldPull::Vec { vec, len, counter } => {
                let more = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(more),
                    Rvalue::BinaryOp {
                        op: BinOp::Lt,
                        lhs: Operand::Copy(Place::local(counter)),
                        rhs: Operand::Copy(Place::local(len)),
                    },
                    span,
                );
                self.terminate(Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::local(more)),
                    arms: vec![(0, done)],
                    default: body,
                });
                self.set_current(body);
                let (elem, _) = self.load_vec_element(vec, counter, elem_ty, false, span);
                self.emit_assign(
                    Place::local(counter),
                    Rvalue::BinaryOp {
                        op: BinOp::Add,
                        lhs: Operand::Copy(Place::local(counter)),
                        rhs: Operand::Const(ConstValue::Int(1)),
                    },
                    span,
                );
                elem
            }
            FoldPull::Lazy { state, next_symbol } => {
                let carrier_ty = self.option_payload_adt_ty(elem_ty);
                let carrier = self.emit_combinator_call(
                    next_symbol,
                    vec![Operand::Copy(Place::local(state))],
                    carrier_ty,
                    span,
                );
                let disc = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(disc),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_disc",
                        args: vec![Operand::Copy(Place::local(carrier))],
                    },
                    span,
                );
                self.terminate(Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::local(disc)),
                    arms: vec![(0, body)],
                    default: done,
                });
                self.set_current(body);
                let extractor =
                    if matches!(self.tcx.kind_of(elem_ty), gossamer_types::TyKind::Float(_)) {
                        "gos_rt_result_payload_f64"
                    } else if self.is_by_value_enum_ty(elem_ty) {
                        "gos_rt_result_payload_i128"
                    } else {
                        "gos_rt_result_payload"
                    };
                let elem = into.unwrap_or_else(|| self.fresh(elem_ty));
                self.emit_assign(
                    Place::local(elem),
                    Rvalue::CallIntrinsic {
                        name: extractor,
                        args: vec![Operand::Copy(Place::local(carrier))],
                    },
                    span,
                );
                elem
            }
        }
    }

    /// The operand a combinator callback takes for `value`: the value itself,
    /// or its address for a two-word carrier the callback reads by reference
    /// (see [`Self::lower_iter_closure`]).
    pub(super) fn fold_callback_arg(&mut self, value: Local, span: Span) -> Local {
        let ty = self.locals[value.0 as usize].ty;
        if !(crate::lower::carrier_ref::is_two_word_carrier(self.tcx, ty)
            && self.elem_bytes_of(ty) > 8)
        {
            return value;
        }
        let ref_ty = self.tcx.intern(gossamer_types::TyKind::Ref {
            mutability: gossamer_types::Mutbl::Not,
            inner: ty,
        });
        let reference = self.fresh(ref_ty);
        self.emit_assign(
            Place::local(reference),
            Rvalue::Ref {
                mutable: false,
                place: Place::local(value),
            },
            span,
        );
        reference
    }
}
