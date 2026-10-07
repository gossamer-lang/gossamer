//! `?`, combinators, pipes, operators, and assignment.

use super::{
    BinaryOp, BuiltinPatternFamily, DataPosition, DefId, Expectation, Expr, ExprKind, FnSig,
    HashSet, INT_SUFFIXES, IntTy, Literal, MatchArm, Mutbl, NodeId, Pattern, PatternKind, PlaceMut,
    Resolution, Span, TryFamily, Ty, TyKind, TypeChecker, TypeDiagnostic, TypeError, UnaryOp,
    arith_op_method, assign_op_method, branch_value_span, callee_display_name, cast_allowed,
    combinator_module_name, expr_display, is_iterator_method, is_plainly_not_callable,
    op_trait_name, operand_display, pipe_step_operation_name,
};

impl TypeChecker<'_> {
    pub(super) fn check_question_mark(&mut self, ty: Ty, span: Span) -> Ty {
        let Some((inner_family, payload)) = self.try_family_and_payload(ty) else {
            let ty = self.render_public_ty(ty);
            self.emit(
                TypeError::QuestionMarkUnsupported {
                    ty,
                    reason: "the operand is not a `Result` or `Option`".to_string(),
                },
                span,
            );
            return self.tcx.error_ty();
        };
        let Some(ret) = self.current_fn_ret else {
            let ty = self.render_public_ty(ty);
            self.emit(
                TypeError::QuestionMarkUnsupported {
                    ty,
                    reason: "`?` is only valid inside a function with a compatible return type"
                        .to_string(),
                },
                span,
            );
            return self.tcx.error_ty();
        };
        // A closure whose answer nothing has fixed yet takes the family its
        // `?` propagates, and a `Result` the error type the operand carries.
        let open_ret = self.infer.resolve(self.tcx, ret);
        if matches!(self.tcx.kind(open_ret), Some(TyKind::Var(_))) {
            let shaped = match inner_family {
                TryFamily::Result => {
                    let ok = self.fresh();
                    let err = self
                        .result_payload_tys(ty, span)
                        .map_or_else(|| self.fresh(), |(_, err)| err);
                    self.result_adt_ty(ok, err)
                }
                TryFamily::Option => {
                    let value = self.fresh();
                    self.option_adt_ty(value)
                }
            };
            self.unify(open_ret, shaped, span);
        }
        let Some((ret_family, _)) = self.try_family_and_payload(ret) else {
            let ty = self.render_public_ty(ret);
            self.emit(
                TypeError::QuestionMarkUnsupported {
                    ty,
                    reason: "the enclosing function does not return `Result` or `Option`"
                        .to_string(),
                },
                span,
            );
            return self.tcx.error_ty();
        };
        if inner_family != ret_family {
            let ty = self.render_public_ty(ty);
            self.emit(
                TypeError::QuestionMarkUnsupported {
                    ty,
                    reason: "the operand and enclosing function use different propagation types"
                        .to_string(),
                },
                span,
            );
            return self.tcx.error_ty();
        }
        if inner_family == TryFamily::Result
            && let (Some((_, from)), Some((_, to))) = (
                self.result_payload_tys(ty, span),
                self.result_payload_tys(ret, span),
            )
        {
            self.deferred_try_conversions.push((from, to, span));
        }
        payload
    }

    /// Reports a `?` whose operand's error type the enclosing function's
    /// cannot take. Decided once unification has settled both, since either
    /// may be pinned by code after the `?`.
    pub(super) fn check_deferred_try_conversions(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_try_conversions);
        for (from, to, span) in deferred {
            let from = self.deep_resolve(from);
            let to = self.deep_resolve(to);
            if from == to
                || matches!(self.tcx.kind(from), Some(TyKind::Var(_) | TyKind::Error))
                || matches!(self.tcx.kind(to), Some(TyKind::Var(_) | TyKind::Error))
                || matches!(self.tcx.kind(to), Some(TyKind::DynError))
                // A `String` error takes any error through its rendering.
                || matches!(self.tcx.kind(to), Some(TyKind::String))
            {
                continue;
            }
            let target = self.render_public_ty(to);
            let source = self
                .method_param_types
                .get(&(target.clone(), "from".to_string()))
                .and_then(|params| (params.len() == 1).then(|| params[0]));
            let converts = source.is_some_and(|source| self.deep_resolve(source) == from);
            if converts {
                continue;
            }
            let from = self.render_public_ty(from);
            self.emit(
                TypeError::QuestionMarkNoConversion { from, to: target },
                span,
            );
        }
    }

    pub(super) fn try_family_and_payload(&mut self, ty: Ty) -> Option<(TryFamily, Ty)> {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let TyKind::Adt { def, substs } = self.tcx.kind(resolved)? else {
            return None;
        };
        let payload = substs.types().first().copied()?;
        match def.local {
            u32::MAX => Some((TryFamily::Result, payload)),
            n if n == u32::MAX - 1 => Some((TryFamily::Option, payload)),
            _ => match self.tcx.def_name(*def)? {
                "Result" => Some((TryFamily::Result, payload)),
                "Option" => Some((TryFamily::Option, payload)),
                _ => None,
            },
        }
    }

    /// `(ok, err)` payload types of a Result-shaped `ty`. A still-free
    /// inference var is unified with a fresh `Result<?, ?>` so the
    /// payload slots exist for the combinator row to pin against.
    pub(super) fn result_payload_tys(&mut self, ty: Ty, span: Span) -> Option<(Ty, Ty)> {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs }) if def.local == u32::MAX => {
                let tys = substs.types();
                Some((tys.first().copied()?, tys.get(1).copied()?))
            }
            Some(TyKind::Var(_)) => {
                let ok = self.fresh();
                let err = self.fresh();
                let shaped = self.result_adt_ty(ok, err);
                self.unify(resolved, shaped, span);
                Some((ok, err))
            }
            _ => None,
        }
    }

    /// Payload type of an Option-shaped `ty`, unifying a free var
    /// with `Option<?>` the same way as [`Self::result_payload_tys`].
    pub(super) fn option_payload_ty(&mut self, ty: Ty, span: Span) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs }) if def.local == u32::MAX - 1 => {
                substs.types().first().copied()
            }
            Some(TyKind::Var(_)) => {
                let payload = self.fresh();
                let shaped = self.option_adt_ty(payload);
                self.unify(resolved, shaped, span);
                Some(payload)
            }
            _ => None,
        }
    }

    /// Element type of a sequence-shaped `ty` (`Vec`, `Iterator`, slice, or
    /// fixed array, ref-transparent), unifying a free var with `Vec<?>`.
    pub(super) fn sequence_elem_ty(&mut self, ty: Ty, span: Span) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(resolved) {
            Some(
                TyKind::Vec(elem)
                | TyKind::Iterator(elem)
                | TyKind::Range(elem)
                | TyKind::Slice(elem)
                | TyKind::Array { elem, .. },
            ) => Some(*elem),
            Some(TyKind::Var(_)) => {
                let elem = self.fresh();
                let shaped = self.tcx.intern(TyKind::Vec(elem));
                self.unify(resolved, shaped, span);
                Some(elem)
            }
            _ => None,
        }
    }

    /// Pins a callable argument's parameter types to `inputs` and
    /// returns its output type. This is the load-bearing step for
    /// lifted closures: binding the param inference vars here is what
    /// keeps the HIR lift pass from pinning an unresolved String/Error
    /// param to i64 (which renders the payload as a raw pointer on the
    /// compiled tiers).
    pub(super) fn callable_output(&mut self, callable_ty: Ty, inputs: &[Ty], span: Span) -> Ty {
        let resolved = self.infer.resolve(self.tcx, callable_ty);
        match self.tcx.kind(resolved).cloned() {
            Some(TyKind::FnPtr(sig) | TyKind::FnTrait(sig)) => {
                self.check_callable_arity(&sig, inputs, resolved, span);
                sig.output
            }
            // A generic function item takes fresh variables for its type
            // parameters at each use, so its callback slot binds them to the
            // element types this call supplies.
            Some(TyKind::FnDef { def, substs }) => {
                match self.instantiated_fn_item_sig(def, &substs) {
                    Some(sig) => {
                        self.check_callable_arity(&sig, inputs, resolved, span);
                        sig.output
                    }
                    None => self.fresh(),
                }
            }
            Some(TyKind::Var(_)) => {
                let output = self.fresh();
                let shaped = self.tcx.intern(TyKind::FnPtr(FnSig {
                    inputs: inputs.to_vec(),
                    output,
                }));
                // `unify` reads its first argument as the expected type: the
                // callable shape this slot declares, not what was passed.
                self.unify(shaped, resolved, span);
                output
            }
            // A callback slot given something that plainly cannot be called.
            // Left unreported, the call reached a runtime that read the
            // argument as a name and failed with an unrelated message.
            Some(kind) if is_plainly_not_callable(&kind) => {
                let output = self.fresh();
                let shaped = self.tcx.intern(TyKind::FnPtr(FnSig {
                    inputs: inputs.to_vec(),
                    output,
                }));
                let expected = self.render_public_ty(shaped);
                let found = self.render_public_ty(resolved);
                self.emit(TypeError::TypeMismatch { expected, found }, span);
                output
            }
            _ => self.fresh(),
        }
    }

    /// Unifies a callable's declared parameters with the slot's, and
    /// reports a callable whose parameter count does not match.
    ///
    /// A silent skip let a callback of the wrong arity reach a runtime
    /// that raised an argument-count error with no source position.
    pub(super) fn check_callable_arity(
        &mut self,
        sig: &FnSig,
        inputs: &[Ty],
        actual: Ty,
        span: Span,
    ) {
        if sig.inputs.len() == inputs.len() {
            for (have, want) in sig.inputs.iter().zip(inputs) {
                self.unify(*have, *want, span);
            }
            return;
        }
        let output = self.fresh();
        let shaped = self.tcx.intern(TyKind::FnPtr(FnSig {
            inputs: inputs.to_vec(),
            output,
        }));
        let expected = self.render_public_ty(shaped);
        let found = self.render_public_ty(actual);
        self.emit(TypeError::TypeMismatch { expected, found }, span);
    }

    /// Unifies `var` with `fallback` when it is still an unresolved
    /// inference variable. Used to default a combinator's unpinned
    /// payload slot to the receiver's payload type: an unresolved
    /// slot blocks `{:?}` lowering on the compiled tiers.
    pub(super) fn default_free_var_to(&mut self, var: Ty, fallback: Ty, span: Span) {
        let resolved = self.infer.resolve(self.tcx, var);
        if matches!(self.tcx.kind(resolved), Some(TyKind::Var(_))) {
            self.unify(resolved, fallback, span);
        }
    }

    /// Resolves `ty` to a Result if possible: an already-Result type
    /// is returned as-is, a free var is unified with
    /// `Result<ok, err>`, anything else degrades to a fresh var.
    pub(super) fn shape_result_like(&mut self, ty: Ty, ok: Ty, err: Ty, span: Span) -> Ty {
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs }) if def.local == u32::MAX => {
                // Pin the payload slots: a combinator closure's `Ok(v)`
                // body leaves the Err slot a free var, and an
                // unresolved payload blocks `{:?}` lowering on the
                // compiled tiers.
                let tys = substs.types();
                if let (Some(&have_ok), Some(&have_err)) = (tys.first(), tys.get(1)) {
                    self.unify(have_ok, ok, span);
                    self.unify(have_err, err, span);
                }
                resolved
            }
            Some(TyKind::Var(_)) => {
                let shaped = self.result_adt_ty(ok, err);
                self.unify(resolved, shaped, span);
                shaped
            }
            _ => self.fresh(),
        }
    }

    /// Option-shaped counterpart of [`Self::shape_result_like`].
    pub(super) fn shape_option_like(&mut self, ty: Ty, payload: Ty, span: Span) -> Ty {
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs }) if def.local == u32::MAX - 1 => {
                let tys = substs.types();
                if let Some(&have) = tys.first() {
                    self.unify(have, payload, span);
                }
                resolved
            }
            Some(TyKind::Var(_)) => {
                let shaped = self.option_adt_ty(payload);
                self.unify(resolved, shaped, span);
                shaped
            }
            _ => self.fresh(),
        }
    }

    /// Return type of a known std data-last combinator call
    /// (`result::*` / `option::*` / closure-taking `iter::*`, free,
    /// piped, or method form). `lead_tys` are the leading closure /
    /// seed argument types; `data_ty` is the trailing data argument
    /// (the method receiver, or the piped value). Unifies closure
    /// parameter vars with the data payload types so lifted closure
    /// bodies keep String/Error params instead of the i64 pin.
    #[allow(
        clippy::too_many_lines,
        reason = "one row per std combinator; splitting the table would obscure the signature catalog"
    )]
    pub(super) fn std_combinator_ty(
        &mut self,
        module: &str,
        name: &str,
        lead_tys: &[Ty],
        data_ty: Ty,
        span: Span,
    ) -> Option<Ty> {
        self.std_combinator_ty_at(module, name, lead_tys, data_ty, DataPosition::Last, span)
    }

    /// [`Self::std_combinator_ty`] with the data argument's position in the
    /// call as written. Only a combinator that pairs its two inputs - `zip` -
    /// reads differently between the two spellings.
    #[allow(
        clippy::too_many_lines,
        reason = "one row per std combinator; splitting the table would obscure the signature catalog"
    )]
    pub(super) fn std_combinator_ty_at(
        &mut self,
        module: &str,
        name: &str,
        lead_tys: &[Ty],
        data_ty: Ty,
        data_position: DataPosition,
        span: Span,
    ) -> Option<Ty> {
        if Self::std_combinator_arity(module, name)? != lead_tys.len() + 1 {
            return None;
        }
        match module {
            "result" => {
                let (ok, err) = self.result_payload_tys(data_ty, span)?;
                let ty = match name {
                    "map" => {
                        let mapped = self.callable_output(lead_tys[0], &[ok], span);
                        self.result_adt_ty(mapped, err)
                    }
                    "map_err" => {
                        let mapped = self.callable_output(lead_tys[0], &[err], span);
                        self.result_adt_ty(ok, mapped)
                    }
                    "and_then" => {
                        let out = self.callable_output(lead_tys[0], &[ok], span);
                        let next_ok = self.fresh();
                        let shaped = self.shape_result_like(out, next_ok, err, span);
                        // An `Err`-only handler leaves the next Ok type
                        // free; default it to the receiver's so the
                        // result is fully resolved for the compiled
                        // tiers' `{:?}` lowering.
                        self.default_free_var_to(next_ok, ok, span);
                        shaped
                    }
                    "or_else" => {
                        let out = self.callable_output(lead_tys[0], &[err], span);
                        let next_err = self.fresh();
                        let shaped = self.shape_result_like(out, ok, next_err, span);
                        self.default_free_var_to(next_err, err, span);
                        shaped
                    }
                    "unwrap_or" => {
                        self.unify(ok, lead_tys[0], span);
                        ok
                    }
                    "unwrap" => ok,
                    "expect" => {
                        let s = self.tcx.string_ty();
                        let message = self.peel_refs(lead_tys[0]);
                        self.unify(s, message, span);
                        ok
                    }
                    // `Ok(v)` yields `v` and `Err(e)` yields `f(e)`, so
                    // both arms answer the Ok payload and the handler
                    // is pinned to produce one. A handler that diverges
                    // contributes no value and is left alone.
                    "unwrap_or_else" => {
                        let out = self.callable_output(lead_tys[0], &[err], span);
                        let resolved_out = self.infer.resolve(self.tcx, out);
                        if !matches!(self.tcx.kind(resolved_out), Some(TyKind::Never)) {
                            self.unify(ok, out, span);
                        }
                        ok
                    }
                    "ok" => self.option_adt_ty(ok),
                    "err" => self.option_adt_ty(err),
                    "is_ok" | "is_err" => self.tcx.bool_ty(),
                    _ => return None,
                };
                Some(ty)
            }
            "option" => {
                let payload = self.option_payload_ty(data_ty, span)?;
                let ty = match name {
                    "map" => {
                        let mapped = self.callable_output(lead_tys[0], &[payload], span);
                        self.option_adt_ty(mapped)
                    }
                    "and_then" => {
                        let out = self.callable_output(lead_tys[0], &[payload], span);
                        let next = self.fresh();
                        let shaped = self.shape_option_like(out, next, span);
                        self.default_free_var_to(next, payload, span);
                        shaped
                    }
                    "filter" => {
                        let out = self.callable_output(lead_tys[0], &[payload], span);
                        let bool_ty = self.tcx.bool_ty();
                        self.unify(bool_ty, out, span);
                        self.option_adt_ty(payload)
                    }
                    "or" => {
                        let shaped = self.option_adt_ty(payload);
                        self.unify(shaped, lead_tys[0], span);
                        shaped
                    }
                    "or_else" => {
                        let out = self.callable_output(lead_tys[0], &[], span);
                        self.shape_option_like(out, payload, span)
                    }
                    "ok_or" => {
                        let err = self.peel_refs(lead_tys[0]);
                        self.result_adt_ty(payload, err)
                    }
                    "ok_or_else" => {
                        let err = self.callable_output(lead_tys[0], &[], span);
                        self.result_adt_ty(payload, err)
                    }
                    "unwrap_or" => {
                        self.unify(payload, lead_tys[0], span);
                        payload
                    }
                    "unwrap" => payload,
                    "expect" => {
                        let s = self.tcx.string_ty();
                        let message = self.peel_refs(lead_tys[0]);
                        self.unify(s, message, span);
                        payload
                    }
                    // Same mixed-type rationale as the Result row: both arms
                    // answer the payload, so the fallback is pinned to
                    // produce one. A fallback that diverges contributes no
                    // value and is left alone.
                    "unwrap_or_else" => {
                        let out = self.callable_output(lead_tys[0], &[], span);
                        let resolved_out = self.infer.resolve(self.tcx, out);
                        if !matches!(self.tcx.kind(resolved_out), Some(TyKind::Never)) {
                            self.unify(payload, out, span);
                        }
                        payload
                    }
                    "zip" => {
                        let other = match self.option_payload_ty(lead_tys[0], span) {
                            Some(other) => other,
                            None => self.fresh(),
                        };
                        let pair = match data_position {
                            DataPosition::Receiver => {
                                self.tcx.intern(TyKind::Tuple(vec![payload, other]))
                            }
                            DataPosition::Last => {
                                self.tcx.intern(TyKind::Tuple(vec![other, payload]))
                            }
                        };
                        self.option_adt_ty(pair)
                    }
                    "flatten" => {
                        let inner = self.fresh();
                        self.shape_option_like(payload, inner, span)
                    }
                    "is_some" | "is_none" => self.tcx.bool_ty(),
                    "iter" => self.tcx.intern(TyKind::Vec(payload)),
                    _ => return None,
                };
                Some(ty)
            }
            "iter" => {
                // A constructor or adapter answers an iterator only when it
                // was handed one; a collection traverses eagerly.
                let edition_lazy_result = false;
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                if matches!(name, "range" | "range_inclusive") {
                    self.unify(i64_ty, lead_tys[0], span);
                    self.unify(i64_ty, data_ty, span);
                    return Some(self.iter_adapter_result_ty(i64_ty, edition_lazy_result));
                }
                if name == "once" {
                    return Some(self.iter_adapter_result_ty(data_ty, edition_lazy_result));
                }
                if name == "repeat" {
                    self.unify(i64_ty, data_ty, span);
                    return Some(self.iter_adapter_result_ty(lead_tys[0], edition_lazy_result));
                }
                let all_tier_iterator_input = is_iterator_method(name);
                let data_is_iterator = matches!(
                    self.tcx.kind_of(self.infer.resolve(self.tcx, data_ty)),
                    TyKind::Iterator(_) | TyKind::Range(_)
                );
                // `enumerate` pairs an index with each element as it is
                // asked for, so it answers an iterator whatever it is
                // handed, in every edition.
                let lazy_result =
                    edition_lazy_result || data_is_iterator || matches!(name, "enumerate");
                if data_is_iterator && !all_tier_iterator_input {
                    let found = self.render_public_ty(data_ty);
                    self.emit(
                        TypeError::TypeMismatch {
                            expected: "Vec<T>".to_string(),
                            found,
                        },
                        span,
                    );
                    return Some(self.tcx.error_ty());
                }
                let elem = self.sequence_elem_ty(data_ty, span)?;
                let bool_ty = self.tcx.bool_ty();
                let ty = match name {
                    "collect" => self.tcx.intern(TyKind::Vec(elem)),
                    "count" => i64_ty,
                    // The sum of a sequence has its element's type.
                    "sum" | "product" => {
                        let elem = self.infer.resolve(self.tcx, elem);
                        match self.tcx.kind(elem) {
                            Some(TyKind::Float(float)) => self.tcx.float_ty(*float),
                            Some(TyKind::Int(int)) => self.tcx.int_ty(*int),
                            // An element the receiver has not pinned yet, or
                            // a generic body's parameter, settles together
                            // with the sum rather than fixing it to `i64`.
                            Some(TyKind::Var(_) | TyKind::Param { .. }) => elem,
                            _ => i64_ty,
                        }
                    }
                    "min" | "max" => self.option_adt_ty(elem),
                    "take" | "skip" | "step_by" => {
                        self.unify(i64_ty, lead_tys[0], span);
                        self.iter_adapter_result_ty(elem, lazy_result)
                    }
                    "enumerate" => {
                        let pair = self.tcx.intern(TyKind::Tuple(vec![i64_ty, elem]));
                        self.iter_adapter_result_ty(pair, lazy_result)
                    }
                    "rev" => self.iter_adapter_result_ty(elem, lazy_result),
                    "dedup" => self.tcx.intern(TyKind::Vec(elem)),
                    "chain" => {
                        let other = self.sequence_elem_ty(lead_tys[0], span).unwrap_or(elem);
                        self.unify(elem, other, span);
                        self.iter_adapter_result_ty(elem, lazy_result)
                    }
                    "zip" => {
                        let other = self
                            .sequence_elem_ty(lead_tys[0], span)
                            .unwrap_or_else(|| self.fresh());
                        // The pair carries the two sequences in the order the
                        // call writes them, which a receiver leads and a
                        // data-last free or piped call trails.
                        let pair = match data_position {
                            DataPosition::Receiver => {
                                self.tcx.intern(TyKind::Tuple(vec![elem, other]))
                            }
                            DataPosition::Last => self.tcx.intern(TyKind::Tuple(vec![other, elem])),
                        };
                        self.iter_adapter_result_ty(pair, lazy_result)
                    }
                    "flatten" => {
                        // Flattening needs an element that is itself a
                        // sequence. A scalar element has no inner type, and
                        // inventing a fresh one let the call through to a
                        // runtime that reads the scalar as a sequence header.
                        let Some(inner) = self.sequence_elem_ty(elem, span) else {
                            let found = self.render_public_ty(data_ty);
                            self.emit(
                                TypeError::TypeMismatch {
                                    expected: "a sequence of sequences".to_string(),
                                    found,
                                },
                                span,
                            );
                            return Some(self.tcx.error_ty());
                        };
                        self.tcx.intern(TyKind::Vec(inner))
                    }
                    "pairwise" => {
                        let pair = self.tcx.intern(TyKind::Tuple(vec![elem, elem]));
                        self.tcx.intern(TyKind::Vec(pair))
                    }
                    "unzip" => {
                        // Splitting pairs needs pairs. Without an element type
                        // to take apart the call went through untyped and each
                        // tier read the missing second slot differently.
                        let resolved_elem = self.infer.resolve(self.tcx, elem);
                        let Some([left, right]) =
                            self.tcx.kind(resolved_elem).and_then(|kind| match kind {
                                TyKind::Tuple(parts) if parts.len() == 2 => {
                                    Some([parts[0], parts[1]])
                                }
                                _ => None,
                            })
                        else {
                            let found = self.render_public_ty(data_ty);
                            self.emit(
                                TypeError::TypeMismatch {
                                    expected: "a sequence of two-element tuples".to_string(),
                                    found,
                                },
                                span,
                            );
                            return Some(self.tcx.error_ty());
                        };
                        let lefts = self.tcx.intern(TyKind::Vec(left));
                        let rights = self.tcx.intern(TyKind::Vec(right));
                        self.tcx.intern(TyKind::Tuple(vec![lefts, rights]))
                    }
                    "windows" | "chunks" => {
                        self.unify(i64_ty, lead_tys[0], span);
                        let window = self.tcx.intern(TyKind::Vec(elem));
                        self.tcx.intern(TyKind::Vec(window))
                    }
                    "for_each" => {
                        let _ = self.callable_output(lead_tys[0], &[elem], span);
                        self.tcx.unit()
                    }
                    "map" => {
                        let mapped = self.callable_output(lead_tys[0], &[elem], span);
                        self.iter_adapter_result_ty(mapped, lazy_result)
                    }
                    "filter" | "take_while" | "skip_while" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        self.unify(bool_ty, out, span);
                        self.iter_adapter_result_ty(elem, lazy_result)
                    }
                    "filter_map" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        let mapped = match self.option_payload_ty(out, span) {
                            Some(payload) => payload,
                            None => self.fresh(),
                        };
                        self.iter_adapter_result_ty(mapped, lazy_result)
                    }
                    "flat_map" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        let mapped = self.sequence_elem_ty(out, span).unwrap_or_else(|| {
                            // Non-sequence closure output is a real
                            // bug, but the runtime flattens anything;
                            // degrade to fresh instead of erroring.
                            self.fresh()
                        });
                        self.iter_adapter_result_ty(mapped, lazy_result)
                    }
                    "fold" | "scan" => {
                        let acc = lead_tys[0];
                        let out = self.callable_output(lead_tys[1], &[acc, elem], span);
                        self.unify(acc, out, span);
                        if name == "fold" {
                            acc
                        } else {
                            self.iter_adapter_result_ty(acc, lazy_result)
                        }
                    }
                    "reduce" => {
                        let out = self.callable_output(lead_tys[0], &[elem, elem], span);
                        self.unify(elem, out, span);
                        self.option_adt_ty(elem)
                    }
                    "sum_by" | "product_by" => self.callable_output(lead_tys[0], &[elem], span),
                    "any" | "all" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        self.unify(bool_ty, out, span);
                        bool_ty
                    }
                    "find" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        self.unify(bool_ty, out, span);
                        self.option_adt_ty(elem)
                    }
                    "position" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        self.unify(bool_ty, out, span);
                        let i64_ty = self.tcx.int_ty(IntTy::I64);
                        self.option_adt_ty(i64_ty)
                    }
                    "find_map" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        let mapped = match self.option_payload_ty(out, span) {
                            Some(payload) => payload,
                            None => self.fresh(),
                        };
                        self.option_adt_ty(mapped)
                    }
                    "partition" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        self.unify(bool_ty, out, span);
                        let vec_ty = self.tcx.intern(TyKind::Vec(elem));
                        self.tcx.intern(TyKind::Tuple(vec![vec_ty, vec_ty]))
                    }
                    // Comparator output is an ordering integer of any
                    // width; the row pins only the element params.
                    "sort_by" => {
                        let _ = self.callable_output(lead_tys[0], &[elem, elem], span);
                        self.tcx.intern(TyKind::Vec(elem))
                    }
                    "sort_by_key" => {
                        let _ = self.callable_output(lead_tys[0], &[elem], span);
                        self.tcx.intern(TyKind::Vec(elem))
                    }
                    "min_by" | "max_by" => {
                        let _ = self.callable_output(lead_tys[0], &[elem, elem], span);
                        self.option_adt_ty(elem)
                    }
                    "min_by_key" | "max_by_key" => {
                        let _ = self.callable_output(lead_tys[0], &[elem], span);
                        self.option_adt_ty(elem)
                    }
                    "chunk_by" => {
                        let key = self.callable_output(lead_tys[0], &[elem], span);
                        let value = self.tcx.intern(TyKind::Vec(elem));
                        self.tcx.intern(TyKind::HashMap {
                            key,
                            value,
                            ordered: false,
                        })
                    }
                    "count_by" => {
                        let key = self.callable_output(lead_tys[0], &[elem], span);
                        let value = self.tcx.int_ty(IntTy::I64);
                        self.tcx.intern(TyKind::HashMap {
                            key,
                            value,
                            ordered: false,
                        })
                    }
                    _ => return None,
                };
                Some(ty)
            }
            _ => None,
        }
    }

    /// Types a full-arity std combinator free call (`result::*` /
    /// `option::*` / `iter::*` data-last forms), or emits the loud
    /// uninferrable-closure error when the name has no signature row
    /// but a closure argument is present. `None` falls back to the
    /// existing stdlib heuristics (including partial applications,
    /// which the pipe site completes).
    pub(super) fn check_std_combinator_free_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
        module: Option<&'static str>,
        name: &str,
    ) -> Option<Ty> {
        let module = module?;
        match Self::std_combinator_arity(module, name) {
            // A source with no data argument stands outside the data-last
            // shape the rest of this table describes: there is no sequence to
            // split off, only the element type the call site expects.
            Some(0) if args.is_empty() => {
                let elem = self.fresh();
                Some(self.tcx.intern(TyKind::Vec(elem)))
            }
            Some(arity) if arity >= 1 && args.len() == arity => {
                let (lead, data) = arg_tys.split_at(arity - 1);
                let lead = lead.to_vec();
                let span = args.last().map_or(callee.span, |arg| arg.span);
                let ty = self.std_combinator_ty(module, name, &lead, data[0], span);
                // An iterator is consumed by the adapter that takes it in
                // every edition: reading it again yields nothing, so the
                // second read is reported rather than silently empty.
                if module == "iter" && ty.is_some() {
                    self.mark_consumed_iterator_args(name, args, arg_tys);
                }
                // A rowed option/result combinator at full arity whose
                // data argument is concretely non-payload-shaped (the
                // classic mistake is the swapped order, data first and
                // closure last) would run the closure slot as the data
                // value and yield the empty fallback; reject it.
                if ty.is_none() {
                    let shape = match module {
                        "option" => Some("Option"),
                        "result" => Some("Result"),
                        _ => None,
                    };
                    if let Some(shape) = shape {
                        self.emit(
                            TypeError::CombinatorDataArgMismatch {
                                combinator: format!("{module}::{name}"),
                                shape: shape.to_string(),
                            },
                            callee.span,
                        );
                        return Some(self.tcx.error_ty());
                    }
                }
                ty
            }
            // A step naming its slot (`xs |> iter::map(f, $)`) is typed at
            // the pipe site, where the data argument's type is known.
            Some(_) => None,
            // A std combinator the checker has no signature row for
            // cannot type its closure argument; the compiled tiers
            // would pin the param to i64 and print String payloads as
            // pointers. Reject loudly instead.
            None => {
                if args
                    .iter()
                    .any(|arg| matches!(arg.kind, ExprKind::Closure { .. }))
                {
                    self.emit(
                        TypeError::ClosureParamUninferred {
                            combinator: format!("{module}::{name}"),
                        },
                        callee.span,
                    );
                    return Some(self.tcx.error_ty());
                }
                None
            }
        }
    }

    pub(super) fn mark_consumed_iterator_args(
        &mut self,
        name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
    ) {
        for (arg, ty) in args.iter().zip(arg_tys.iter()) {
            self.mark_consumed_iterator_expr(name, arg, *ty);
        }
    }

    /// Records the left operand of a `|>` step as a spent iterator, named
    /// after the call the step makes so the diagnostic points at it.
    pub(super) fn mark_piped_iterator_consumed(&mut self, lhs: &Expr, lhs_ty: Ty, rhs: &Expr) {
        let operation = pipe_step_operation_name(rhs).unwrap_or_else(|| "|>".to_string());
        self.mark_consumed_iterator_labelled(&operation, lhs, lhs_ty);
    }

    pub(super) fn mark_consumed_iterator_expr(&mut self, name: &str, expr: &Expr, ty: Ty) {
        self.mark_consumed_iterator_labelled(&format!("iter::{name}"), expr, ty);
    }

    /// [`Self::mark_consumed_iterator_expr`] with the operation spelled out.
    pub(super) fn mark_consumed_iterator_labelled(&mut self, operation: &str, expr: &Expr, ty: Ty) {
        let resolved = self.infer.resolve(self.tcx, ty);
        if !matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Iterator(_) | TyKind::Range(_))
        ) {
            return;
        }
        let ExprKind::Path(path) = &expr.kind else {
            return;
        };
        if path.segments.len() != 1 {
            return;
        }
        let Some(Resolution::Local(binding)) = self.resolutions.get(expr.id) else {
            return;
        };
        if let Some(scope) = self.consumed_iterators.last_mut() {
            scope.insert(binding, operation.to_string());
        }
    }

    /// Types Result/Option combinator *method* calls
    /// (`r.map_err(f)`, `o.map(f)`) through the same signature table
    /// as the free `result::*` / `option::*` functions, with the
    /// receiver as the data argument. Returns `None` (leaving the
    /// generic method path untouched) when the receiver is not a
    /// resolved Result/Option or the name has no row.
    pub(super) fn check_payload_combinator_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        receiver_span: Span,
        args: &[Expr],
    ) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let module = match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX => "result",
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX - 1 => "option",
            _ => return None,
        };
        if Self::std_combinator_arity(module, method)? != args.len() + 1 {
            return None;
        }
        let span = args.first().map_or(receiver_span, |arg| arg.span);
        // Check a closure argument against the payload it will receive. Its
        // body is checked once, so an unconstrained parameter leaves any
        // projection out of the payload as a free variable that later unifies
        // with whatever context demands - and the mapped payload never gets
        // the closure's real return type.
        let closure_inputs = self.vec_combinator_closure_inputs(method, resolved);
        let lead_tys: Vec<Ty> = args
            .iter()
            .map(|arg| match (&closure_inputs, &arg.kind) {
                (Some(inputs), ExprKind::Closure { params, .. })
                    if params.len() == inputs.len() =>
                {
                    let output = self.fresh();
                    let sig = FnSig {
                        inputs: inputs.clone(),
                        output,
                    };
                    let want = self.tcx.intern(TyKind::FnPtr(sig));
                    self.check_expr_expecting(arg, Expectation::HasType(want))
                }
                _ => self.check_expr(arg),
            })
            .collect();
        self.std_combinator_ty_at(
            module,
            method,
            &lead_tys,
            resolved,
            DataPosition::Receiver,
            span,
        )
    }

    /// The type of `!operand`, whose type is `operand_ty` (`resolved` once
    /// resolved).
    fn check_not(&mut self, operand: &Expr, operand_ty: Ty, resolved: Ty, span: Span) -> Ty {
        if matches!(self.tcx.kind(resolved), Some(TyKind::Bool)) {
            self.tcx.bool_ty()
        } else if self.reject_operator_off_bound(resolved, "!", "not", span) {
            self.tcx.error_ty()
        } else if self.adt_name_of(resolved).is_some() {
            // `!x` on a user struct / enum routes to its `not` impl
            // (a zero-arg method on the receiver), the same way `-x`
            // routes to `neg`. The operand node is anchored to its
            // resolved nominal type so tier lowering dispatches the
            // call.
            self.record(operand.id, resolved);
            if let Some(ret) = self.adt_op_method_ret(resolved, "not", 0) {
                ret
            } else {
                let ty = self.render_public_ty(resolved);
                self.emit(
                    TypeError::UnresolvedOpImpl {
                        op: "!".to_string(),
                        trait_name: "Not".to_string(),
                        method: "not".to_string(),
                        ty,
                    },
                    span,
                );
                self.tcx.error_ty()
            }
        } else if self.is_concrete(resolved) && !self.is_integer(resolved) {
            let lhs = self.render_public_ty(resolved);
            self.emit(
                TypeError::UnresolvedOp {
                    op: "!".to_string(),
                    lhs,
                    rhs: String::new(),
                },
                span,
            );
            self.tcx.error_ty()
        } else {
            operand_ty
        }
    }

    /// The type of `-operand`, whose type is `operand_ty` (`resolved` once
    /// resolved).
    fn check_neg(&mut self, operand: &Expr, operand_ty: Ty, resolved: Ty, span: Span) -> Ty {
        // `-x` on a user struct / enum routes to its `neg` impl
        // (a zero-arg method on the receiver); the result is that
        // method's return type, and the operand node is anchored
        // to its resolved nominal type so tier lowering dispatches
        // the call. An ADT with no `impl Neg` is rejected here
        // rather than faulting at runtime. Scalars and lane vectors
        // keep the operand type.
        if matches!(self.tcx.kind(resolved), Some(TyKind::Simd { .. })) {
            operand_ty
        } else if self.reject_operator_off_bound(resolved, "-", "neg", span) {
            self.tcx.error_ty()
        } else if self.adt_name_of(resolved).is_some() {
            self.record(operand.id, resolved);
            if let Some(ret) = self.adt_op_method_ret(resolved, "neg", 0) {
                ret
            } else {
                let ty = self.render_public_ty(resolved);
                self.emit(
                    TypeError::UnresolvedOpImpl {
                        op: "-".to_string(),
                        trait_name: "Neg".to_string(),
                        method: "neg".to_string(),
                        ty,
                    },
                    span,
                );
                self.tcx.error_ty()
            }
        } else {
            // A negated literal is one constant, which the literal
            // range check judges whole.
            let literal = matches!(operand.kind, ExprKind::Literal(_));
            if !literal {
                self.deferred_integer_operands.push((
                    operand_ty,
                    "-",
                    false,
                    expr_display(operand),
                    span,
                ));
            }
            operand_ty
        }
    }

    pub(super) fn check_unary(
        &mut self,
        op: UnaryOp,
        operand: &Expr,
        span: Span,
        _expected: Expectation,
    ) -> Ty {
        // A borrow preserves the operand's concrete owned type. The resulting
        // reference may then unsize from an array or Vec reference to a slice.
        let operand_expected = match op {
            UnaryOp::RefShared | UnaryOp::RefMut => Expectation::None,
            _ => Expectation::None,
        };
        let previous_suppression = self.suppressed.borrow_read_conflict;
        if matches!(op, UnaryOp::RefShared | UnaryOp::RefMut) {
            self.suppressed.borrow_read_conflict = true;
        }
        let operand_ty = match (op, &operand.kind) {
            // A suffixed literal under `-` is one negative constant, so its
            // range is checked from the negative side: `-128i8` is `i8::MIN`.
            (UnaryOp::Neg, ExprKind::Literal(Literal::Int(text)))
                if INT_SUFFIXES
                    .iter()
                    .any(|(suffix, _)| text.ends_with(suffix)) =>
            {
                let ty = self.type_of_int_literal(&format!("-{text}"), operand.span);
                self.record(operand.id, ty);
                ty
            }
            _ => self.check_expr_expecting(operand, operand_expected),
        };
        self.suppressed.borrow_read_conflict = previous_suppression;
        let resolved = self.infer.resolve(self.tcx, operand_ty);
        match op {
            UnaryOp::Not => self.check_not(operand, operand_ty, resolved, span),
            UnaryOp::Neg => self.check_neg(operand, operand_ty, resolved, span),
            UnaryOp::RefShared | UnaryOp::RefMut => {
                self.check_reference_unary(op, operand, operand_ty)
            }
            UnaryOp::Deref => {
                // `*x` strips a single `&T` / `&mut T` wrapper.
                // For any other concrete operand shape the deref is
                // an identity (matches the interp's behaviour on
                // for-loop bound elements where the iterator hands
                // back values rather than references). Without
                // either pinning, downstream `println!("{}", *x)`
                // dispatches via `TyKind::Var → StrPtr` and tries
                // to dereference the value as a pointer - segv.
                let resolved = self.infer.resolve(self.tcx, operand_ty);
                match self.tcx.kind(resolved) {
                    Some(TyKind::Ref { inner, .. }) => *inner,
                    _ => operand_ty,
                }
            }
        }
    }

    pub(super) fn check_reference_unary(
        &mut self,
        op: UnaryOp,
        operand: &Expr,
        operand_ty: Ty,
    ) -> Ty {
        let root = Self::place_root_name(operand).unwrap_or_else(|| "value".to_string());
        if self.reject_range_borrow(op, operand) {
            return self.tcx.error_ty();
        }
        let mutability = if op == UnaryOp::RefMut {
            let conflict = self
                .active_mutable_borrower(&root)
                .or_else(|| self.active_shared_borrower(&root))
                .map(str::to_string);
            if let Some(borrower) = conflict {
                self.emit(
                    TypeError::MutableReferenceConflict {
                        root: root.clone(),
                        borrower,
                    },
                    operand.span,
                );
            }
            match self.place_mutability(operand) {
                PlaceMut::ImmutableBinding => self.emit(
                    TypeError::MutableReferenceToImmutable { name: root.clone() },
                    operand.span,
                ),
                PlaceMut::SharedReference => self.emit(
                    TypeError::AssignThroughSharedReference { name: root.clone() },
                    operand.span,
                ),
                PlaceMut::NotAReference => self.emit(
                    TypeError::DerefWriteToNonReference { name: root.clone() },
                    operand.span,
                ),
                PlaceMut::Writable | PlaceMut::Unknown => {}
            }
            Mutbl::Mut
        } else {
            if let Some(borrower) = self.active_mutable_borrower(&root).map(str::to_string) {
                self.emit(
                    TypeError::BorrowedPlaceConflict {
                        root,
                        borrower,
                        action: "read through a new shared reference to",
                    },
                    operand.span,
                );
            }
            Mutbl::Not
        };
        let inner = match self.mutable_window_elem(operand) {
            Some(elem) if mutability == Mutbl::Mut => self.tcx.intern(TyKind::Slice(elem)),
            _ => operand_ty,
        };
        self.tcx.intern(TyKind::Ref { mutability, inner })
    }

    /// If `expr` is a tuple-variant constructor call (`E::B(1)`), the `Adt`
    /// type of its enum. Used to anchor comparison operands - the constructor's
    /// result is otherwise a fresh variable unless used at a typed site, which
    /// leaves an inline `E::B(1) < E::B(2)` undispatchable. Scoped to the
    /// comparison arm so it does not retype constructors feeding a `let`
    /// destructure (whose compiled-tier payload extraction wants the bare form).
    /// Instantiation of a generic enum's parameters for the constructor
    /// call at `node`, allocating fresh variables on first use and returning
    /// the same ones afterwards.
    ///
    /// Returns `None` for a non-generic enum, whose payload types carry no
    /// parameters and whose `Adt` is cached whole in `enum_tys`.
    pub(super) fn variant_ctor_instantiation(
        &mut self,
        node: NodeId,
        enum_name: &str,
    ) -> Option<(DefId, Vec<Ty>)> {
        if let Some(found) = self.variant_ctor_substs.get(&node) {
            return Some(found.clone());
        }
        let def = *self.user_type_defs.get(enum_name)?;
        let arity = self.struct_generic_arity.get(&def).copied()?;
        let const_mask = self.fn_generic_const_mask_of(def);
        let placeholder = self.tcx.error_ty();
        let substs: Vec<Ty> = (0..arity)
            .map(|i| {
                if const_mask.get(i).copied().unwrap_or(false) {
                    placeholder
                } else {
                    self.fresh()
                }
            })
            .collect();
        self.variant_ctor_substs.insert(node, (def, substs.clone()));
        Some((def, substs))
    }

    pub(super) fn variant_ctor_enum_ty(&self, expr: &Expr) -> Option<Ty> {
        let ExprKind::Call { callee, .. } = &expr.kind else {
            return None;
        };
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let n = path.segments.len();
        if n < 2 {
            return None;
        }
        let enum_name = path.segments[n - 2].name.name.as_str();
        let var_name = path.segments[n - 1].name.name.as_str();
        if self
            .enum_variants
            .get(enum_name)
            .is_some_and(|vs| vs.contains(var_name))
        {
            self.enum_tys.get(enum_name).copied()
        } else {
            None
        }
    }

    /// Records what the right of `|>` is before its stage is checked: a
    /// bare path there is a callee, and a call or method call receives
    /// the piped value as its trailing argument during lowering, so the
    /// arity checks account for one argument the source does not spell.
    pub(super) fn record_pipe_stage(&mut self, op: BinaryOp, rhs: &Expr) {
        if op != BinaryOp::PipeGt {
            return;
        }
        match &rhs.kind {
            ExprKind::Path(_) => {
                self.callee_path_nodes.insert(rhs.id);
            }
            ExprKind::Call { callee, .. } => {
                self.pipe_stage_callees.insert(callee.id);
            }
            ExprKind::MethodCall { .. } => {
                self.pipe_stage_callees.insert(rhs.id);
            }
            _ => {}
        }
    }

    /// Checks a comparison whose operands are already typed; it answers `bool`.
    pub(super) fn check_comparison(
        &mut self,
        op: BinaryOp,
        (lhs, lhs_ty): (&Expr, Ty),
        (rhs, rhs_ty): (&Expr, Ty),
        span: Span,
    ) -> Ty {
        // Anchor a variant-constructor operand to its enum so an inline
        // same-variant comparison (`E::B(1) < E::B(2)`) can dispatch - both
        // sides are otherwise fresh variables.
        let lhs_ty = if let Some(e) = self.variant_ctor_enum_ty(lhs) {
            self.record(lhs.id, e);
            e
        } else {
            lhs_ty
        };
        let rhs_ty = if let Some(e) = self.variant_ctor_enum_ty(rhs) {
            self.record(rhs.id, e);
            e
        } else {
            rhs_ty
        };
        // A byte literal compares against any integer operand without an
        // explicit `as i64`: `s[i] == b'>'`. A byte literal is an `Int` value
        // on every tier, so re-typing its node to the integer operand's type
        // lets the comparison flow unchanged.
        if !self.coerce_byte_literal_cmp(lhs, lhs_ty, rhs, rhs_ty) {
            self.unify_operands(op, lhs, lhs_ty, rhs, rhs_ty, span);
        }
        self.tcx.bool_ty()
    }

    /// The type of `lhs op rhs` when an operand is a user type, answered by
    /// its operator impl (`+` -> `add`, `|` -> `bitor`, ...): receiver-first,
    /// so the left operand is `self`. `None` when neither operand is one.
    fn check_operator_impl(
        &mut self,
        op: BinaryOp,
        method: &'static str,
        (lhs, lhs_ty): (&Expr, Ty),
        (rhs, rhs_ty): (&Expr, Ty),
        span: Span,
    ) -> Option<Ty> {
        if self.reject_operands_off_bound(lhs_ty, rhs_ty, op.as_str(), method, span) {
            return Some(self.tcx.error_ty());
        }
        let lhs_res = self.infer.resolve(self.tcx, lhs_ty);
        let rhs_res = self.infer.resolve(self.tcx, rhs_ty);
        let lhs_adt = self.operand_nominal_name_of(lhs_res);
        let rhs_adt = self.operand_nominal_name_of(rhs_res);
        if lhs_adt.is_some() || rhs_adt.is_some() {
            // Anchor ADT operand nodes to their resolved
            // nominal type: tier lowering dispatches the
            // impl-method call off the operand node's type,
            // which otherwise may stay an inference var
            // (enum locals in particular).
            if lhs_adt.is_some() {
                self.record(lhs.id, lhs_res);
            }
            if rhs_adt.is_some() {
                self.record(rhs.id, rhs_res);
            }
            if lhs_adt.is_some()
                && let Some((chosen, ret)) =
                    self.rhs_typed_operator_method((lhs_res, rhs_res), method, span)
            {
                self.table.insert_operator_method(lhs.id, chosen);
                return Some(ret);
            }
            if lhs_adt.is_some()
                && let Some(ret) = self.adt_op_method_ret(lhs_res, method, 1)
            {
                // The right operand is the method's argument.
                if let Some(param) = self
                    .user_method_params_for(lhs_res, method)
                    .and_then(|params| params.first().copied())
                {
                    self.unify(param, rhs_ty, rhs.span);
                }
                return Some(ret);
            }
            let ty = if lhs_adt.is_some() { lhs_res } else { rhs_res };
            // An impl answers one right-hand type, so a right
            // operand of another type names the one to write.
            let trait_name = if lhs_adt.is_some() && lhs_res != rhs_res {
                let rhs_text = self.render_public_ty(rhs_res);
                format!("{}<{rhs_text}>", op_trait_name(method))
            } else {
                op_trait_name(method).to_string()
            };
            let ty = self.render_public_ty(ty);
            self.emit(
                TypeError::UnresolvedOpImpl {
                    op: op.as_str().to_string(),
                    trait_name,
                    method: method.to_string(),
                    ty,
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        None
    }

    /// The type of a shift: the amount is a count of bits, so it may be any
    /// integer type, and the result is the shifted operand's type.
    fn check_shift(
        &mut self,
        op: BinaryOp,
        (lhs, lhs_ty): (&Expr, Ty),
        (rhs, rhs_ty): (&Expr, Ty),
    ) -> Ty {
        if let Some(amount) = literal_int_spelling(rhs) {
            self.deferred_shift_amounts.push((lhs_ty, amount, rhs.span));
        }
        for (ty, operand) in [(lhs_ty, lhs), (rhs_ty, rhs)] {
            self.deferred_integer_operands
                .push((ty, op.as_str(), false, None, operand.span));
        }
        lhs_ty
    }

    pub(super) fn check_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, span: Span) -> Ty {
        self.record_pipe_stage(op, rhs);
        let lhs_ty = self.check_expr(lhs);
        if op == BinaryOp::PipeGt && matches!(rhs.kind, ExprKind::MethodCall { .. }) {
            self.pipe_stage_arg_tys.insert(rhs.id, lhs_ty);
        }
        let rhs_ty = self.check_pipe_rhs(op, lhs_ty, rhs);
        if let Some(ty) = self.check_simd_binary(op, lhs_ty, rhs_ty, span) {
            return ty;
        }
        match op {
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge => self.check_comparison(op, (lhs, lhs_ty), (rhs, rhs_ty), span),
            BinaryOp::And | BinaryOp::Or => {
                let bool_ty = self.tcx.bool_ty();
                self.unify(bool_ty, lhs_ty, lhs.span);
                self.unify(bool_ty, rhs_ty, rhs.span);
                bool_ty
            }
            BinaryOp::PipeGt => self.pipe_result_ty(lhs, lhs_ty, rhs, rhs_ty),
            BinaryOp::WrappingAdd | BinaryOp::WrappingSub | BinaryOp::WrappingMul => {
                self.check_wrapping_operands(op, lhs, lhs_ty, rhs, rhs_ty, span)
            }
            // A masking shift's amount is a count of any integer type, as a
            // plain shift's is; the result is the shifted operand's type.
            BinaryOp::WrappingShl | BinaryOp::WrappingShr => {
                self.require_wrapping_integer(op.as_str(), rhs_ty, lhs_ty, rhs.span);
                self.require_wrapping_integer(op.as_str(), lhs_ty, rhs_ty, span)
            }
            _ => {
                // String concatenation accepts a borrowed RHS:
                // `"hello, " + &name` (the documented spelling). Peel
                // the reference before unifying so the expression
                // stays `String` instead of failing `String != &T`.
                if op == BinaryOp::Add {
                    let l = self.infer.resolve(self.tcx, lhs_ty);
                    if matches!(self.tcx.kind_of(l), TyKind::String) {
                        let r = self.infer.resolve(self.tcx, rhs_ty);
                        if let TyKind::Ref { inner, .. } = self.tcx.kind_of(r) {
                            let inner = *inner;
                            self.unify(lhs_ty, inner, span);
                            return lhs_ty;
                        }
                    }
                }
                // Arithmetic / bitwise on a user struct/enum routes to its
                // operator impl (`+` -> `add`, `|` -> `bitor`, ...); the
                // result is that method's return type. Dispatch is
                // receiver-first (the left operand is `self`), so an ADT
                // operand with no such impl - or an ADT appearing only on
                // the right of a non-ADT left operand - is rejected here
                // rather than miscompiling to a runtime fault.
                if let Some(method) = arith_op_method(op)
                    && let Some(ty) =
                        self.check_operator_impl(op, method, (lhs, lhs_ty), (rhs, rhs_ty), span)
                {
                    return ty;
                }
                // A shift amount is a count of bits, so it may be any integer
                // type; the result is the shifted operand's type.
                if matches!(op, BinaryOp::Shl | BinaryOp::Shr) {
                    return self.check_shift(op, (lhs, lhs_ty), (rhs, rhs_ty));
                }
                // A byte literal joins integer arithmetic without an explicit
                // `as i64` - `s[i] - b'0'` - through the same node re-typing
                // the comparison arms apply; the result takes the integer
                // operand's type.
                let lhs_is_byte = matches!(&lhs.kind, ExprKind::Literal(Literal::Byte(_)));
                let result = if self.coerce_byte_literal_cmp(lhs, lhs_ty, rhs, rhs_ty) {
                    if lhs_is_byte { rhs_ty } else { lhs_ty }
                } else {
                    self.unify_operands(op, lhs, lhs_ty, rhs, rhs_ty, span);
                    lhs_ty
                };
                if let Some(allows_bool) = bit_operator_allows_bool(op) {
                    self.deferred_integer_operands.push((
                        result,
                        op.as_str(),
                        allows_bool,
                        None,
                        span,
                    ));
                }
                result
            }
        }
    }

    /// Types a wrapping arithmetic operator (`+%`, `-%`, `*%`). Its operands
    /// are one integer type, whose declared width the result wraps at; a byte
    /// literal joins an integer operand as it does for `+`.
    pub(super) fn check_wrapping_operands(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        lhs_ty: Ty,
        rhs: &Expr,
        rhs_ty: Ty,
        span: Span,
    ) -> Ty {
        let lhs_is_byte = matches!(&lhs.kind, ExprKind::Literal(Literal::Byte(_)));
        let result = if self.coerce_byte_literal_cmp(lhs, lhs_ty, rhs, rhs_ty) {
            if lhs_is_byte { rhs_ty } else { lhs_ty }
        } else {
            self.unify_operands(op, lhs, lhs_ty, rhs, rhs_ty, span);
            lhs_ty
        };
        self.require_wrapping_integer(op.as_str(), result, rhs_ty, span)
    }

    /// Reports GT0003 when a wrapping arithmetic operand settles on a type
    /// other than an integer: wrapping names a width, and only an integer has
    /// one. An operand still being inferred is left to unification.
    pub(super) fn require_wrapping_integer(
        &mut self,
        op: &'static str,
        ty: Ty,
        other: Ty,
        span: Span,
    ) -> Ty {
        let resolved = self.infer.resolve(self.tcx, ty);
        if self.is_integer(resolved) {
            return ty;
        }
        if !self.is_concrete(resolved) {
            self.deferred_wrapping_operands
                .push((resolved, other, op, span));
            return ty;
        }
        self.emit_wrapping_operand_error(op, resolved, other, span);
        self.tcx.error_ty()
    }

    /// Reports the deferred wrapping arithmetic operands whose type literal
    /// defaulting settled on something other than an integer.
    pub(super) fn check_deferred_wrapping_operands(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_wrapping_operands);
        for (ty, other, op, span) in deferred {
            let resolved = self.deep_resolve(ty);
            if self.is_integer(resolved)
                || !self.is_concrete(resolved)
                || matches!(self.tcx.kind(resolved), Some(TyKind::Error))
            {
                continue;
            }
            self.emit_wrapping_operand_error(op, resolved, other, span);
        }
    }

    pub(super) fn emit_wrapping_operand_error(&mut self, op: &str, ty: Ty, other: Ty, span: Span) {
        let other = self.deep_resolve(other);
        let lhs = self.render_public_ty(ty);
        let rhs = self.render_public_ty(other);
        self.emit(
            TypeError::UnresolvedOp {
                op: op.to_string(),
                lhs,
                rhs,
            },
            span,
        );
    }

    /// Reports each deferred operand whose settled type the operator cannot
    /// apply to: `-` on an unsigned integer (GT0001), and a bitwise or shift
    /// operator on anything but an integer or, for `&` `|` `^`, a `bool`
    /// (GT0003). An operand still unsettled, or already an error, is left
    /// to the diagnostics that unsettled it; a user type's operator impl was
    /// chosen before any operand was deferred.
    pub(super) fn check_deferred_integer_operands(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_integer_operands);
        for (ty, op, allows_bool, operand, span) in deferred {
            let resolved = self.deep_resolve(ty);
            let resolved = self.peel_refs(resolved);
            match self.tcx.kind(resolved).cloned() {
                Some(TyKind::Int(int_ty)) if op == "-" && !int_ty.is_signed() => {
                    self.emit(
                        TypeError::UnsignedNegation {
                            ty: int_ty.as_str().to_string(),
                            operand,
                        },
                        span,
                    );
                }
                Some(
                    TyKind::Int(_)
                    | TyKind::Var(_)
                    | TyKind::Error
                    | TyKind::Never
                    | TyKind::Param { .. },
                )
                | None => {}
                Some(TyKind::Bool) if allows_bool => {}
                Some(_) if op == "-" => {}
                Some(_) => {
                    let lhs = self.render_public_ty(resolved);
                    self.emit(
                        TypeError::UnresolvedOp {
                            op: op.to_string(),
                            lhs: lhs.clone(),
                            rhs: lhs,
                        },
                        span,
                    );
                }
            }
        }
    }

    /// Reports GT0116 for each `<<` / `>>` whose literal amount falls outside
    /// `0..BITS` of the shifted type inference settled on.
    pub(super) fn check_deferred_shift_amounts(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_shift_amounts);
        for (ty, amount, span) in deferred {
            let resolved = self.deep_resolve(ty);
            let Some(TyKind::Int(int_ty)) = self.tcx.kind(resolved).cloned() else {
                continue;
            };
            let bits = int_bits(int_ty);
            let in_range = super::literals::parse_int_magnitude(&amount)
                .is_some_and(|magnitude| !amount.starts_with('-') && magnitude < u128::from(bits));
            if !in_range {
                self.emit(
                    TypeError::ShiftAmountOutOfRange {
                        amount,
                        ty: int_ty.as_str().to_string(),
                        bits,
                    },
                    span,
                );
            }
        }
    }

    /// Unifies two operand types, reporting an integer paired with a float
    /// as the cast the reader has to write instead of a bare mismatch.
    pub(super) fn unify_operands(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        lhs_ty: Ty,
        rhs: &Expr,
        rhs_ty: Ty,
        span: Span,
    ) {
        if let Some(error) = self.numeric_operand_mismatch(op, lhs, lhs_ty, rhs, rhs_ty) {
            self.emit(error, span);
            return;
        }
        self.unify(lhs_ty, rhs_ty, span);
    }

    /// The GT0001 diagnostic for an integer operand paired with a float
    /// one, spelling the whole expression with the cast in place. `None`
    /// for every other operand pairing.
    pub(super) fn numeric_operand_mismatch(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        lhs_ty: Ty,
        rhs: &Expr,
        rhs_ty: Ty,
    ) -> Option<TypeError> {
        let left = self.infer.resolve(self.tcx, lhs_ty);
        let right = self.infer.resolve(self.tcx, rhs_ty);
        let (left_kind, right_kind) = (self.tcx.kind(left)?.clone(), self.tcx.kind(right)?.clone());
        let expected = self.render_public_ty(left);
        let found = self.render_public_ty(right);
        let op = op.as_str();
        let (left_text, right_text) = (operand_display(lhs), operand_display(rhs));
        let cast = match (left_kind, right_kind) {
            (TyKind::Int(left_int), TyKind::Int(right_int)) if left_int != right_int => {
                return Some(integer_operand_mismatch(
                    op,
                    (lhs, left_int),
                    (rhs, right_int),
                ));
            }
            (TyKind::Int(_), TyKind::Float(_)) => {
                format!("{left_text} as {found} {op} {right_text}")
            }
            (TyKind::Float(_), TyKind::Int(_)) => {
                format!("{left_text} {op} {right_text} as {expected}")
            }
            _ => return None,
        };
        Some(TypeError::NumericOperandMismatch {
            expected,
            found,
            cast,
        })
    }

    /// Type-checks a pipe RHS closure with its parameter shaped by the value
    /// flowing in. Other expressions retain ordinary expression checking.
    pub(super) fn check_pipe_rhs(&mut self, op: BinaryOp, lhs_ty: Ty, rhs: &Expr) -> Ty {
        // A pipe into a closure determines the closure's sole parameter before
        // checking its body. Delaying this until `pipe_result_ty` left method
        // calls in the body with an unresolved receiver, so a malformed
        // `s |> |s| s.slice(s, 1, 3)` skipped String's arity check and reached
        // the runtime shim with an ignored extra argument.
        if op == BinaryOp::PipeGt
            && let ExprKind::Closure { params, .. } = &rhs.kind
            && params.len() == 1
        {
            let output = self.fresh();
            let expected = self.tcx.intern(TyKind::FnPtr(FnSig {
                inputs: vec![lhs_ty],
                output,
            }));
            self.check_expr_expecting(rhs, Expectation::HasType(expected))
        } else if op != BinaryOp::PipeGt
            && matches!(
                self.tcx.kind(self.infer.resolve(self.tcx, lhs_ty)),
                Some(TyKind::Simd { .. })
            )
        {
            // A lane-wise operator's right operand is a vector of the left's
            // type, which a `Simd::splat` there takes its lanes from.
            self.check_expr_expecting(rhs, Expectation::HasType(lhs_ty))
        } else {
            self.check_expr(rhs)
        }
    }

    pub(super) fn check_direct_pipe_sig(
        &mut self,
        sig: &FnSig,
        lhs: &Expr,
        lhs_ty: Ty,
        rhs: &Expr,
    ) {
        if sig.inputs.len() == 1 {
            self.check_sig_param_arg(sig.inputs[0], lhs_ty, lhs);
        } else {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: callee_display_name(rhs),
                    expected: sig.inputs.len(),
                    found: 1,
                },
                rhs.span,
            );
        }
    }

    pub(super) fn check_piped_user_method_arg(&mut self, lhs: &Expr, lhs_ty: Ty, rhs: &Expr) {
        let ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &rhs.kind
        else {
            return;
        };
        let Some(receiver_ty) = self.table.get(receiver.id) else {
            return;
        };
        let receiver_ty = self.peel_refs(receiver_ty);
        if let Some(params) = self.user_method_params_for(receiver_ty, &name.name)
            && args.len() + 1 == params.len()
            && let Some(last) = params.last().copied()
        {
            self.check_sig_param_arg(last, lhs_ty, lhs);
        }
    }

    /// Returns the result type of a `lhs |> rhs` pipe expression.
    ///
    /// `|>` desugars to `rhs(lhs)` (or `rhs(partial_args…, lhs)` for
    /// partial-application RHS). The expression type is the callee's
    /// return type, not the callee's function type. Unifies `lhs_ty`
    /// with the callee's last parameter so that un-annotated closure
    /// params (`|x| x + 1`) are pinned from the piped value's type.
    pub(super) fn pipe_result_ty(&mut self, lhs: &Expr, lhs_ty: Ty, rhs: &Expr, rhs_ty: Ty) -> Ty {
        // Try to extract the callee's return type from rhs_ty first.
        let resolved = self.infer.resolve(self.tcx, rhs_ty);
        match self.tcx.kind_of(resolved).clone() {
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                if matches!(rhs.kind, ExprKind::Path(_) | ExprKind::Closure { .. }) {
                    self.check_direct_pipe_sig(&sig, lhs, lhs_ty, rhs);
                }
                // A step takes the piped value by value, so a lazy iterator
                // handed to one is spent: whatever the step does with it, the
                // next read of the binding sees a drained cursor.
                self.mark_piped_iterator_consumed(lhs, lhs_ty, rhs);
                return self.infer.resolve(self.tcx, sig.output);
            }
            TyKind::FnDef { def, substs } => {
                if let Some(sig) = self.instantiated_fn_item_sig(def, &substs) {
                    if matches!(rhs.kind, ExprKind::Path(_)) {
                        self.check_direct_pipe_sig(&sig, lhs, lhs_ty, rhs);
                    }
                    return sig.output;
                }
            }
            _ => {}
        }
        self.check_piped_user_method_arg(lhs, lhs_ty, rhs);
        // Data-last std combinators, partially applied through the
        // pipe (`xs |> iter::map(f, $)`, `r |> result::map_err(f, $)`,
        // `r |> result::ok`): the piped value is the data argument,
        // so its payload types pin the closure params here.
        let combinator: Option<(&gossamer_ast::PathExpr, &[Expr])> = match &rhs.kind {
            ExprKind::Call { callee, args } => match &callee.kind {
                // A callee that resolved to a user `FnDef` keeps its
                // own typing even under a std-module-shaped name.
                ExprKind::Path(path)
                    if !matches!(
                        self.table
                            .get(callee.id)
                            .map(|t| self.tcx.kind_of(t).clone()),
                        Some(TyKind::FnDef { .. })
                    ) =>
                {
                    Some((path, args.as_slice()))
                }
                _ => None,
            },
            ExprKind::Path(path) => Some((path, &[])),
            _ => None,
        };
        if let Some((path, lead_args)) = combinator {
            let names: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
            let (module, last) = names.split_at(names.len().saturating_sub(1));
            let comb = combinator_module_name(module);
            if let (Some(comb), Some(&last)) = (comb, last.first()) {
                if Self::std_combinator_arity(comb, last) == Some(lead_args.len() + 1) {
                    let lead_tys: Vec<Ty> = lead_args
                        .iter()
                        .map(|arg| {
                            self.table
                                .get(arg.id)
                                .unwrap_or_else(|| self.tcx.error_ty())
                        })
                        .collect();
                    if let Some(ret) =
                        self.std_combinator_ty(comb, last, &lead_tys, lhs_ty, lhs.span)
                    {
                        if comb == "iter" {
                            self.mark_consumed_iterator_args(last, lead_args, &lead_tys);
                            self.mark_consumed_iterator_expr(last, lhs, lhs_ty);
                        }
                        return ret;
                    }
                }
            }
        }
        // rhs_ty might be an unresolved Var when `rhs` is a call whose arity
        // guard fired in check_call. Recover by inspecting the call's inner
        // callee type directly.
        if let ExprKind::Call {
            callee: inner_callee,
            args,
        } = &rhs.kind
        {
            let inner_ty = self.table.get(inner_callee.id).unwrap_or(rhs_ty);
            let resolved_inner = self.infer.resolve(self.tcx, inner_ty);
            match self.tcx.kind_of(resolved_inner).clone() {
                TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                    if args.len() + 1 == sig.inputs.len()
                        && let Some(last) = sig.inputs.last().copied()
                    {
                        self.check_sig_param_arg(last, lhs_ty, lhs);
                    }
                    return self.infer.resolve(self.tcx, sig.output);
                }
                TyKind::FnDef { def, substs } => {
                    if let Some(sig) = self.instantiated_fn_item_sig(def, &substs) {
                        if args.len() + 1 == sig.inputs.len()
                            && let Some(last) = sig.inputs.last().copied()
                        {
                            self.check_sig_param_arg(last, lhs_ty, lhs);
                        }
                        return sig.output;
                    }
                }
                _ => {}
            }
        }
        rhs_ty
    }

    /// Resolved reference mutability of `expr`, or `None` when its type is
    /// not a reference. Lets a write through a `&mut T` succeed regardless
    /// of the reference binding's own declared mutability.
    pub(super) fn expr_ref_mutbl(&self, expr: &Expr) -> Option<Mutbl> {
        let ty = self.table.get(expr.id)?;
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved) {
            Some(TyKind::Ref { mutability, .. }) => Some(*mutability),
            _ => None,
        }
    }

    /// Rejects a call whose arguments reach one referent twice while a
    /// `&mut` argument holds it: a second `&mut` of the same root, or a
    /// closure that captures the root. The callee would observe the referent
    /// through an alias the `&mut` does not account for.
    pub(super) fn check_overlapping_mutable_call_args(&mut self, args: &[Expr]) {
        let mut borrowed: Vec<PlacePath> = Vec::new();
        let mut windows: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        for arg in args {
            let ExprKind::Unary {
                op: UnaryOp::RefMut,
                operand,
            } = &arg.kind
            else {
                continue;
            };
            let Some(place) = PlacePath::of(operand) else {
                continue;
            };
            let root = place.root.clone();
            // Windows of one place may share a call: their ranges are checked
            // disjoint before it runs.
            let window = window_place_key(operand);
            let first_window = windows
                .entry(root.clone())
                .or_insert_with(|| window.clone());
            let joins_windows = window.is_some() && *first_window == window;
            let overlaps = borrowed.iter().any(|earlier| earlier.overlaps(&place));
            borrowed.push(place);
            if overlaps && !joins_windows {
                self.emit(
                    TypeError::MutableReferenceConflict {
                        root,
                        borrower: "an earlier call argument".to_string(),
                    },
                    arg.span,
                );
            }
        }
        let roots: HashSet<String> = borrowed.into_iter().map(|place| place.root).collect();
        for arg in args {
            if let Some((root, borrower)) = self.closure_alias_of(arg, &roots) {
                self.emit(
                    TypeError::MutableReferenceConflict { root, borrower },
                    arg.span,
                );
            }
        }
    }

    /// Rejects a by-value argument that reads storage a `&mut` argument of
    /// the same call reaches - the same place, a place inside it, or one it
    /// sits inside: the callee would read the caller's storage through that
    /// value while it writes the same storage through the reference.
    /// Sibling fields of one root are disjoint. Runs once the arguments
    /// carry their types.
    pub(super) fn check_by_value_argument_aliases(&mut self, args: &[Expr]) {
        let borrowed: Vec<PlacePath> = args
            .iter()
            .filter_map(|arg| match &arg.kind {
                ExprKind::Unary {
                    op: UnaryOp::RefMut,
                    operand,
                } => PlacePath::of(operand),
                _ => None,
            })
            .collect();
        if borrowed.is_empty() {
            return;
        }
        let roots: HashSet<String> = borrowed.iter().map(|place| place.root.clone()).collect();
        for arg in args {
            if !matches!(
                arg.kind,
                ExprKind::Path(_) | ExprKind::FieldAccess { .. } | ExprKind::Index { .. }
            ) || self.closure_alias_of(arg, &roots).is_some()
            {
                continue;
            }
            if let Some((root, borrower)) = self.by_value_storage_alias(arg, &borrowed) {
                self.emit(
                    TypeError::MutableReferenceConflict { root, borrower },
                    arg.span,
                );
            }
        }
    }

    /// The `&mut` root among `roots` that a closure argument captures - a
    /// closure literal, or a local bound to one - with how it reaches it.
    pub(super) fn closure_alias_of(
        &self,
        arg: &Expr,
        roots: &HashSet<String>,
    ) -> Option<(String, String)> {
        match &arg.kind {
            ExprKind::Closure { params, body, .. } => {
                let captured = super::closure_outer_names(params, body);
                let root = roots.iter().find(|root| captured.contains(*root))?;
                Some((
                    root.clone(),
                    "a closure argument that captures it".to_string(),
                ))
            }
            ExprKind::Path(path) if path.segments.len() == 1 => {
                let name = path.segments[0].name.name.as_str();
                let captured = self.closure_binding_captures(name)?;
                let root = roots.iter().find(|root| captured.contains(*root))?;
                Some((
                    root.clone(),
                    format!("the closure `{name}` that captures it"),
                ))
            }
            _ => None,
        }
    }

    /// A by-value place argument rooted at one of `roots` whose type holds
    /// storage the callee would read through the caller's handle.
    fn by_value_storage_alias(
        &mut self,
        arg: &Expr,
        borrowed: &[PlacePath],
    ) -> Option<(String, String)> {
        let place = PlacePath::of(arg)?;
        if !borrowed.iter().any(|mutable| mutable.overlaps(&place)) {
            return None;
        }
        let root = place.root;
        let recorded = self.table.get(arg.id)?;
        let ty = self.peel_refs(recorded);
        let scalar = matches!(
            self.tcx.kind(ty),
            Some(
                TyKind::Bool
                    | TyKind::Char
                    | TyKind::Int(_)
                    | TyKind::Float(_)
                    | TyKind::String
                    | TyKind::Unit
                    | TyKind::Error
                    | TyKind::Var(_)
            )
        );
        (!scalar).then(|| {
            (
                root,
                "a by-value argument reading the same storage".to_string(),
            )
        })
    }

    /// Whether an assignment place is writable: writable when rooted at a
    /// `mut` binding or reached through a `&mut` reference; immutable when
    /// rooted at a non-`mut` binding or reached through a `&T`; unknown
    /// otherwise (module item, deref of a non-reference, complex base).
    /// Only a definitely-immutable place is rejected.
    pub(super) fn place_mutability(&self, place: &Expr) -> PlaceMut {
        match &place.kind {
            ExprKind::Path(path) => {
                if path.segments.len() == 1 && path.segments[0].generics.is_empty() {
                    if let Some(mutable) = self.lookup_local_mutability(&path.segments[0].name.name)
                    {
                        return if mutable {
                            PlaceMut::Writable
                        } else {
                            PlaceMut::ImmutableBinding
                        };
                    }
                    match self.resolutions.get(place.id) {
                        Some(Resolution::Def {
                            def,
                            kind: gossamer_resolve::DefKind::Static,
                        }) => match self.static_mutability.get(&def) {
                            Some(true) => PlaceMut::Writable,
                            Some(false) => PlaceMut::ImmutableBinding,
                            None => PlaceMut::Unknown,
                        },
                        Some(Resolution::Def {
                            kind: gossamer_resolve::DefKind::Const,
                            ..
                        }) => PlaceMut::ImmutableBinding,
                        _ => PlaceMut::Unknown,
                    }
                } else {
                    PlaceMut::Unknown
                }
            }
            ExprKind::FieldAccess { receiver, .. } => self.base_place_mutability(receiver),
            ExprKind::Index { base, .. } => self.base_place_mutability(base),
            ExprKind::Unary {
                op: UnaryOp::Deref,
                operand,
            } => match self.expr_ref_mutbl(operand) {
                Some(Mutbl::Mut) => PlaceMut::Writable,
                Some(Mutbl::Not) => PlaceMut::SharedReference,
                None if self.operand_is_concrete_non_reference(operand) => PlaceMut::NotAReference,
                None => PlaceMut::Unknown,
            },
            _ => PlaceMut::Unknown,
        }
    }

    /// Mutability of an auto-dereferenced projection or method receiver.
    /// Every crossed reference layer must be mutable. An outer `&mut` cannot
    /// tunnel through an inner shared reference in a `&mut &T` chain.
    pub(super) fn auto_deref_place_mutability(&self, base: &Expr) -> PlaceMut {
        let Some(ty) = self.table.get(base.id) else {
            return self.place_mutability(base);
        };
        let mut resolved = self.infer.resolve(self.tcx, ty);
        let mut crossed_mutable_reference = false;
        loop {
            match self.tcx.kind(resolved) {
                Some(TyKind::Ref {
                    mutability: Mutbl::Not,
                    ..
                }) => return PlaceMut::SharedReference,
                Some(TyKind::Ref {
                    mutability: Mutbl::Mut,
                    inner,
                }) => {
                    crossed_mutable_reference = true;
                    resolved = self.infer.resolve(self.tcx, *inner);
                }
                _ => break,
            }
        }
        if crossed_mutable_reference {
            PlaceMut::Writable
        } else {
            self.place_mutability(base)
        }
    }

    pub(super) fn base_place_mutability(&self, base: &Expr) -> PlaceMut {
        self.auto_deref_place_mutability(base)
    }

    /// Element type of `&mut seq[a..b]` when `operand` is a range index over
    /// a Vec, array, or slice: the borrow is a mutable window `&mut [T]`.
    pub(super) fn mutable_window_elem(&mut self, operand: &Expr) -> Option<Ty> {
        let ExprKind::Index { base, index } = &operand.kind else {
            return None;
        };
        if !matches!(index.kind, ExprKind::Range { .. }) {
            return None;
        }
        let recorded = self.table.get(base.id)?;
        let base_ty = self.infer.resolve(self.tcx, recorded);
        let base_ty = self.peel_refs(base_ty);
        match self.tcx.kind(base_ty) {
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
                Some(*elem)
            }
            _ => None,
        }
    }

    /// Rejects `&xs[a..b]` over a sequence and `&mut s[a..b]` over a
    /// `String`. Both indexes answer a fresh copy of the range, so the borrow
    /// would reference a temporary nothing owns; only `&mut seq[a..b]` is a
    /// window.
    pub(super) fn reject_range_borrow(&mut self, op: UnaryOp, operand: &Expr) -> bool {
        let ExprKind::Index { base, index } = &operand.kind else {
            return false;
        };
        if !matches!(index.kind, ExprKind::Range { .. }) {
            return false;
        }
        let Some(recorded) = self.table.get(base.id) else {
            return false;
        };
        let base_ty = self.infer.resolve(self.tcx, recorded);
        let base_ty = self.peel_refs(base_ty);
        let rejected = match self.tcx.kind(base_ty) {
            Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }) => {
                op == UnaryOp::RefShared
            }
            Some(TyKind::String) => op == UnaryOp::RefMut,
            _ => false,
        };
        if !rejected {
            return false;
        }
        let base_text = Self::place_root_name(base).unwrap_or_else(|| "xs".to_string());
        self.emit(
            TypeError::RangeBorrow {
                mutability: if op == UnaryOp::RefMut { "mut " } else { "" },
                base: base_text,
                range: "start..end".to_string(),
            },
            operand.span,
        );
        true
    }

    /// Leftmost path-segment name of a place, naming the root binding in
    /// the immutability diagnostic.
    pub(super) fn place_root_name(place: &Expr) -> Option<String> {
        match &place.kind {
            ExprKind::Path(path) => path.segments.first().map(|s| s.name.name.clone()),
            ExprKind::FieldAccess { receiver, .. } => Self::place_root_name(receiver),
            ExprKind::Index { base, .. } => Self::place_root_name(base),
            ExprKind::Unary { operand, .. } => Self::place_root_name(operand),
            _ => None,
        }
    }

    /// Names an invalid assignment target in its diagnostic: the literal as
    /// written where the expression is one, and the expression's category
    /// otherwise.
    pub(super) fn assign_target_display(target: &Expr) -> String {
        match &target.kind {
            ExprKind::Literal(
                gossamer_ast::Literal::Int(text) | gossamer_ast::Literal::Float(text),
            ) => text.clone(),
            ExprKind::Literal(gossamer_ast::Literal::Bool(value)) => value.to_string(),
            ExprKind::Literal(gossamer_ast::Literal::String(text)) => format!("{text:?}"),
            ExprKind::Literal(gossamer_ast::Literal::Char(value)) => format!("'{value}'"),
            ExprKind::Call { .. } => "a call result".to_string(),
            ExprKind::MethodCall { .. } => "a method result".to_string(),
            ExprKind::Binary { .. } | ExprKind::Unary { .. } => "an operator result".to_string(),
            ExprKind::Array(_) | ExprKind::FixedArray(_) => "a sequence literal".to_string(),
            ExprKind::MapLiteral(_) => "a map literal".to_string(),
            ExprKind::SetLiteral(_) => "a set literal".to_string(),
            ExprKind::Struct { .. } => "a struct literal".to_string(),
            ExprKind::Range { .. } => "a range".to_string(),
            _ => "this expression".to_string(),
        }
    }

    pub(super) fn place_display(place: &Expr) -> String {
        match &place.kind {
            ExprKind::Path(path) => path
                .segments
                .iter()
                .map(|segment| segment.name.name.as_str())
                .collect::<Vec<_>>()
                .join("::"),
            ExprKind::FieldAccess { receiver, field } => match field {
                gossamer_ast::FieldSelector::Named(name) => {
                    format!("{}.{}", Self::place_display(receiver), name.name)
                }
                gossamer_ast::FieldSelector::Index(index) => {
                    format!("{}.{}", Self::place_display(receiver), index)
                }
            },
            ExprKind::Index { base, .. } => format!("{}[...]", Self::place_display(base)),
            ExprKind::MethodCall { receiver, name, .. } => {
                format!("{}.{}(..)", Self::place_display(receiver), name.name)
            }
            ExprKind::Call { callee, .. } => format!("{}(..)", Self::place_display(callee)),
            ExprKind::Unary {
                op: gossamer_ast::UnaryOp::Deref,
                operand,
            } => format!("*{}", Self::place_display(operand)),
            _ => "value".to_string(),
        }
    }

    /// The name a write diagnostic gives `place`: its root binding, or the
    /// expression written under the `*` when no binding roots it.
    pub(super) fn written_place_name(place: &Expr) -> String {
        Self::place_root_name(place).unwrap_or_else(|| match &place.kind {
            ExprKind::Unary {
                op: gossamer_ast::UnaryOp::Deref,
                operand,
            } => Self::place_display(operand),
            _ => Self::place_display(place),
        })
    }

    pub(super) fn correct_map_lookup_assignment_result(
        &mut self,
        value: &Expr,
        mut value_ty: Ty,
    ) -> Ty {
        let ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &value.kind
        else {
            return value_ty;
        };
        if !matches!(name.name.as_str(), "get_or" | "or_insert") {
            return value_ty;
        }
        if let Some(default) = args.get(1)
            && let Some(default_ty) = self.table.get(default.id)
        {
            value_ty = self.infer.resolve(self.tcx, default_ty);
            self.record(value.id, value_ty);
        }
        let receiver_ty = self
            .table
            .get(receiver.id)
            .unwrap_or_else(|| self.check_expr(receiver));
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::HashMap {
            value: map_value, ..
        }) = self.tcx.kind(resolved)
        {
            let map_value = self.infer.resolve(self.tcx, *map_value);
            if matches!(self.tcx.kind(value_ty), Some(TyKind::Var(_))) {
                value_ty = map_value;
                self.record(value.id, value_ty);
            }
        }
        value_ty
    }

    /// Reports a place that cannot be written: one borrowed elsewhere, one
    /// rooted at an immutable binding, or one reached through a shared `&T`.
    pub(super) fn check_place_writable(&mut self, place: &Expr) {
        let name = Self::written_place_name(place);
        if let Some(borrower) = self
            .active_mutable_borrower(&name)
            .or_else(|| self.active_shared_borrower(&name))
            .map(str::to_string)
        {
            self.emit(
                TypeError::BorrowedPlaceConflict {
                    root: name.clone(),
                    borrower,
                    action: "mutate",
                },
                place.span,
            );
        }
        match self.place_mutability(place) {
            PlaceMut::ImmutableBinding => {
                self.emit(TypeError::AssignToImmutable { name }, place.span);
            }
            PlaceMut::SharedReference => {
                self.emit(TypeError::AssignThroughSharedReference { name }, place.span);
            }
            PlaceMut::NotAReference => {
                self.emit(TypeError::DerefWriteToNonReference { name }, place.span);
            }
            PlaceMut::Writable | PlaceMut::Unknown => {}
        }
    }

    /// True when `operand`'s type is known and is not a reference, so a
    /// `*operand` write names no place at all.
    pub(super) fn operand_is_concrete_non_reference(&self, operand: &Expr) -> bool {
        let Some(ty) = self.table.get(operand.id) else {
            return false;
        };
        let resolved = self.infer.resolve(self.tcx, ty);
        matches!(
            self.tcx.kind(resolved),
            Some(
                TyKind::Int(_)
                    | TyKind::Float(_)
                    | TyKind::Bool
                    | TyKind::Char
                    | TyKind::String
                    | TyKind::Unit
            )
        )
    }

    /// Types one destructuring-assignment target. A tuple recurses
    /// element-wise, `_` discards its element and answers a fresh variable,
    /// and every other element must name a writable place.
    pub(super) fn check_assign_target(&mut self, target: &Expr) -> Ty {
        if let ExprKind::Tuple(elems) = &target.kind {
            let tys: Vec<Ty> = elems
                .iter()
                .map(|elem| self.check_assign_target(elem))
                .collect();
            let ty = self.tcx.intern(TyKind::Tuple(tys));
            self.record(target.id, ty);
            return ty;
        }
        if target.is_wildcard() {
            let ty = self.fresh();
            self.record(target.id, ty);
            return ty;
        }
        if !target.is_place() {
            self.emit(
                TypeError::InvalidAssignTarget {
                    target: Self::assign_target_display(target),
                },
                target.span,
            );
            return self.tcx.error_ty();
        }
        let previous_suppression = self.suppressed.borrow_read_conflict;
        self.suppressed.borrow_read_conflict = true;
        let ty = self.check_expr(target);
        self.suppressed.borrow_read_conflict = previous_suppression;
        self.check_place_writable(target);
        ty
    }

    /// Type-checks `a, b.c, xs[i] = rhs`, whose targets are the elements of
    /// the left-hand list. The tuple of element types is the expectation the
    /// right-hand side is checked against, so a literal there is shaped by
    /// the destination exactly as in a scalar assignment. A compound operator
    /// pairs with the same element types, since each place is combined with
    /// its own element.
    pub(super) fn check_destructuring_assign(&mut self, place: &Expr, value: &Expr) -> Ty {
        let place_ty = self.check_assign_target(place);
        let value_ty = self.check_expr_expecting(value, Expectation::HasType(place_ty));
        self.unify(place_ty, value_ty, value.span);
        self.tcx.unit()
    }

    /// Checks rebinding a reference-typed place: the value must itself be a
    /// named borrow. Answers `false` when the value is not a reference, having
    /// deferred the mismatch.
    pub(super) fn check_reference_rebind(
        &mut self,
        place: &Expr,
        value: &Expr,
        place_resolved: Ty,
        value_ty: Ty,
    ) -> bool {
        if !self.rebind_named_borrow(place, value) {
            self.emit(
                TypeError::ReferenceEscapeUnsupported {
                    context: "be rebound through an alias or from a temporary".to_string(),
                },
                value.span,
            );
        }
        let value_resolved = self.infer.resolve(self.tcx, value_ty);
        let value_is_reference_or_unresolved = matches!(
            self.tcx.kind(value_resolved),
            Some(TyKind::Ref { .. } | TyKind::Var(_) | TyKind::Error | TyKind::Never)
        );
        if !value_is_reference_or_unresolved {
            self.deferred_type_mismatches
                .push((place_resolved, value_resolved, value.span));
        }
        value_is_reference_or_unresolved
    }

    /// `s += &t` / `s += &str`: String append accepts a borrowed operand on
    /// the right, mirroring the `+` concatenation operator. Only the compound
    /// `+=` form relaxes; plain `=` still requires an owned String.
    pub(super) fn is_string_append(&mut self, place_ty: Ty, value_ty: Ty) -> bool {
        let pr = self.infer.resolve(self.tcx, place_ty);
        if !matches!(self.tcx.kind(pr), Some(TyKind::String)) {
            return false;
        }
        let mut vr = self.infer.resolve(self.tcx, value_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(vr) {
            vr = self.infer.resolve(self.tcx, *inner);
        }
        matches!(self.tcx.kind(vr), Some(TyKind::String))
    }

    /// Checks the compound assignments only an integer place answers: the
    /// wrapping forms and the shifts. Whether `op` was one of them.
    fn check_integer_compound_assign(
        &mut self,
        op: gossamer_ast::AssignOp,
        (place, place_ty): (&Expr, Ty),
        (value, value_ty): (&Expr, Ty),
    ) -> bool {
        if matches!(
            op,
            gossamer_ast::AssignOp::WrappingAddAssign
                | gossamer_ast::AssignOp::WrappingSubAssign
                | gossamer_ast::AssignOp::WrappingMulAssign
        ) {
            self.unify(place_ty, value_ty, value.span);
            self.require_wrapping_integer(op.as_str(), place_ty, value_ty, place.span);
            return true;
        }
        // A shift amount is a count of bits of any integer type, so it is not
        // unified with the place it shifts.
        if matches!(
            op,
            gossamer_ast::AssignOp::WrappingShlAssign | gossamer_ast::AssignOp::WrappingShrAssign
        ) {
            self.require_wrapping_integer(op.as_str(), value_ty, place_ty, value.span);
            self.require_wrapping_integer(op.as_str(), place_ty, value_ty, place.span);
            return true;
        }
        if matches!(
            op,
            gossamer_ast::AssignOp::ShlAssign | gossamer_ast::AssignOp::ShrAssign
        ) && self
            .adt_name_of(self.infer.resolve(self.tcx, place_ty))
            .is_none()
        {
            if let Some(amount) = literal_int_spelling(value) {
                self.deferred_shift_amounts
                    .push((place_ty, amount, value.span));
            }
            for (ty, operand) in [(place_ty, place), (value_ty, value)] {
                self.deferred_integer_operands
                    .push((ty, op.as_str(), false, None, operand.span));
            }
            return true;
        }
        false
    }

    pub(super) fn check_assign(
        &mut self,
        place: &Expr,
        value: &Expr,
        op: gossamer_ast::AssignOp,
    ) -> Ty {
        if matches!(place.kind, ExprKind::Tuple(_)) {
            return self.check_destructuring_assign(place, value);
        }
        if !place.is_place() {
            self.emit(
                TypeError::InvalidAssignTarget {
                    target: Self::assign_target_display(place),
                },
                place.span,
            );
            let _ = self.check_expr(value);
            return self.tcx.unit();
        }
        let previous_suppression = self.suppressed.borrow_read_conflict;
        self.suppressed.borrow_read_conflict = true;
        let place_ty = self.check_expr(place);
        self.suppressed.borrow_read_conflict = previous_suppression;
        self.check_place_writable(place);
        self.check_ffi_field_write(place);
        let place_resolved = self.infer.resolve(self.tcx, place_ty);
        let place_is_reference = matches!(self.tcx.kind(place_resolved), Some(TyKind::Ref { .. }));
        // A reference binding must be rebound with another reference. Do not
        // pass its referent through as a literal-shaping expectation: doing so
        // would allow `let mut r = &value; r = value` to discard the `&`.
        // Other assignment destinations retain the expected-type flow that
        // shapes `[2, 3]` as a Vec for a `Vec<i64>` slot.
        let mut value_ty = if place_is_reference {
            self.check_expr(value)
        } else {
            self.check_expr_expecting(value, Expectation::HasType(place_ty))
        };
        // A map lookup-like method returns V, independently of the assignment
        // destination. When V is still inferred, feeding the destination
        // HashMap<K, V> expectation into `h = h.or_insert(k, default)` could
        // recursively bind V to the map itself and silently retype `h`.
        // Re-read the authoritative value type from the receiver after its
        // key/default arguments have grounded the map generics.
        value_ty = self.correct_map_lookup_assignment_result(value, value_ty);
        if place_is_reference
            && !self.check_reference_rebind(place, value, place_resolved, value_ty)
        {
            return self.tcx.unit();
        }
        if matches!(op, gossamer_ast::AssignOp::AddAssign)
            && self.is_string_append(place_ty, value_ty)
        {
            return self.tcx.unit();
        }
        if self.check_integer_compound_assign(op, (place, place_ty), (value, value_ty)) {
            return self.tcx.unit();
        }
        // Compound assignment on a user struct / enum desugars through the
        // binary operator, so it routes to the same operator impl
        // (`+=` -> `add`). The impl's return re-binds the place, so it
        // must be the place's own type. A place with no impl is rejected
        // here rather than faulting at runtime; the value operand keeps
        // the impl's declared parameter shape (a scalar for `v *= 2.0`),
        // so the place/value unification below is skipped on this path.
        if let Some(method) = assign_op_method(op) {
            let pr = self.infer.resolve(self.tcx, place_ty);
            if self.adt_name_of(pr).is_some() {
                self.record(place.id, pr);
                let vr = self.infer.resolve(self.tcx, value_ty);
                if self.adt_name_of(vr).is_some() {
                    self.record(value.id, vr);
                }
                if let Some((chosen, ret)) =
                    self.rhs_typed_operator_method((pr, vr), method, place.span)
                {
                    self.table.insert_operator_method(place.id, chosen);
                    self.unify(place_ty, ret, place.span);
                } else if let Some(ret) = self.adt_op_method_ret(pr, method, 1) {
                    if let Some(param) = self
                        .user_method_params_for(pr, method)
                        .and_then(|params| params.first().copied())
                    {
                        self.unify(param, value_ty, value.span);
                    }
                    self.unify(place_ty, ret, place.span);
                } else {
                    let ty = self.render_public_ty(pr);
                    self.emit(
                        TypeError::UnresolvedOpImpl {
                            op: op.as_str().to_string(),
                            trait_name: op_trait_name(method).to_string(),
                            method: method.to_string(),
                            ty,
                        },
                        place.span,
                    );
                }
                return self.tcx.unit();
            }
        }
        self.unify(place_ty, value_ty, value.span);
        self.tcx.unit()
    }

    /// Validates an `as` cast against the whitelist of permitted
    /// conversions: numeric ↔ numeric, `bool`/`char` → integer,
    /// `u8` → `char`, and same-type no-ops. Matches Rust's RFC 401.
    /// Fails soft when either side is still an inference variable -
    /// the unification pass will resolve it, and a later run can
    /// recheck; inventing an error on a not-yet-known type would
    /// cascade into noise.
    pub(super) fn check_cast(&mut self, from: Ty, to: Ty, span: Span) {
        let resolved_from = self.infer.resolve(self.tcx, from);
        let resolved_to = self.infer.resolve(self.tcx, to);
        let Some(from_kind) = self.tcx.kind(resolved_from).cloned() else {
            return;
        };
        let Some(to_kind) = self.tcx.kind(resolved_to).cloned() else {
            return;
        };
        if matches!(from_kind, TyKind::Var(_) | TyKind::Error)
            || matches!(to_kind, TyKind::Var(_) | TyKind::Error)
        {
            return;
        }
        if cast_allowed(&from_kind, &to_kind) {
            return;
        }
        let from = self.render_public_ty(resolved_from);
        let to = self.render_public_ty(resolved_to);
        self.diagnostics.push(TypeDiagnostic::new(
            TypeError::InvalidCast { from, to },
            span,
        ));
    }

    /// Checks an expression whose value the statement discards.
    ///
    /// An `if` here answers nothing, so each branch is checked on its own
    /// terms. Joining them would make two branches that merely differ - one
    /// answering an `Option<i64>`, the next an `Option<String>` - a type
    /// error about a value nothing reads.
    pub(super) fn check_discarded_expr(&mut self, expr: &Expr) -> Ty {
        // A block whose value is discarded discards its tail as well, so the
        // tail gets the same per-branch treatment a statement does.
        if let ExprKind::Block(block) | ExprKind::Unsafe(block) = &expr.kind
            && let Some(tail) = &block.tail
        {
            let unsafe_block = matches!(expr.kind, ExprKind::Unsafe(_));
            if unsafe_block {
                self.unsafe_depth += 1;
            }
            self.push_scope();
            for stmt in &block.stmts {
                self.check_stmt(stmt);
            }
            self.check_discarded_expr(tail);
            self.pop_scope();
            if unsafe_block {
                self.unsafe_depth -= 1;
            }
            let unit = self.tcx.unit();
            return self.record(expr.id, unit);
        }
        let ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } = &expr.kind
        else {
            return self.check_expr(expr);
        };
        let cond_ty = self.check_expr(condition);
        let bool_ty = self.tcx.bool_ty();
        self.unify(bool_ty, cond_ty, condition.span);
        self.check_discarded_expr(then_branch);
        if let Some(else_branch) = else_branch {
            self.check_discarded_expr(else_branch);
        }
        let unit = self.tcx.unit();
        self.record(expr.id, unit)
    }

    pub(super) fn check_if(
        &mut self,
        condition: &Expr,
        then_branch: &Expr,
        else_branch: Option<&Expr>,
        expected: Expectation,
    ) -> Ty {
        let cond_ty = self.check_expr(condition);
        let bool_ty = self.tcx.bool_ty();
        self.unify(bool_ty, cond_ty, condition.span);
        let then_ty = self.check_expr_expecting(then_branch, expected);
        if let Some(else_branch) = else_branch {
            let else_ty = self.check_expr_expecting(else_branch, expected);
            // An `else if` chain with no final `else` answers nothing on the
            // path where every condition fails. Like an else-less `if`, it is
            // typed `()` and each branch keeps its own type.
            if !Self::if_chain_has_final_else(else_branch) {
                return self.tcx.unit();
            }
            let joined = self.join_branch_tys(then_ty, else_ty, branch_value_span(else_branch));
            // When the branches joined to a Vec/slice, re-record each
            // array-literal branch to that shape so an unannotated
            // `let v = if c { [1, 2] } else { [3, 4, 5] }` lowers both
            // arms as a heap Vec, matching the joined result slot.
            self.adjust_literal_to_join(then_branch, joined);
            self.adjust_literal_to_join(else_branch, joined);
            joined
        } else {
            self.tcx.unit()
        }
    }

    /// Whether an `if` chain ends in a final `else`, so some branch runs on
    /// every path. `else_branch` is the node after an `else`.
    pub(super) fn if_chain_has_final_else(else_branch: &Expr) -> bool {
        match &else_branch.kind {
            ExprKind::If {
                else_branch: Some(next),
                ..
            } => Self::if_chain_has_final_else(next),
            ExprKind::If {
                else_branch: None, ..
            } => false,
            _ => true,
        }
    }

    /// Joins branch types without silently converting arrays or slices to Vec.
    /// As in Rust, both value-producing branches must have one compatible type.
    /// The element type of an unannotated sequence literal. A function item
    /// stored as an element is a callable value, so the elements share its
    /// signature rather than one item's own type.
    pub(super) fn join_element_tys(&mut self, elems: &[Expr]) -> Ty {
        let Some(first) = elems.first() else {
            return self.fresh();
        };
        let first_ty = self.check_expr(first);
        let elem_ty = match self.fn_item_value_ty(first_ty) {
            Some(callable) => {
                self.record_fn_item_coercion(first, callable);
                callable
            }
            None => first_ty,
        };
        for elem in elems.iter().skip(1) {
            let ty = self.check_expr(elem);
            self.record_fn_item_coercion(elem, elem_ty);
            self.unify(elem_ty, ty, elem.span);
        }
        elem_ty
    }

    /// The callable type a function item has as a stored value: its
    /// instantiated signature. `None` when `ty` is not a function item.
    pub(super) fn fn_item_value_ty(&mut self, ty: Ty) -> Option<Ty> {
        let resolved = self.infer.resolve(self.tcx, ty);
        let Some(TyKind::FnDef { def, substs }) = self.tcx.kind(resolved).cloned() else {
            return None;
        };
        let sig = self.instantiated_fn_item_sig(def, &substs)?;
        Some(self.tcx.intern(TyKind::FnPtr(sig)))
    }

    pub(super) fn join_branch_tys(&mut self, a: Ty, b: Ty, span: Span) -> Ty {
        self.unify(a, b, span);
        // A branch that never finishes answers nothing; the `if` has the
        // type of the one that does.
        let a_res = self.infer.resolve(self.tcx, a);
        if matches!(self.tcx.kind(a_res), Some(TyKind::Never)) {
            return b;
        }
        a
    }

    pub(super) fn check_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        expected: Expectation,
    ) -> Ty {
        let scrut_expectation = self.match_scrutinee_expectation(arms);
        let scrut_ty = self.check_expr_expecting(scrutinee, scrut_expectation);
        self.reject_constructor_scrutinee_mismatch(scrut_ty, arms);
        self.reject_json_value_variant_patterns(arms);
        let mut result_ty = self.fresh();
        for arm in arms {
            self.push_scope();
            let pat_ty = self.type_of_pattern(&arm.pattern);
            // String literal patterns compare by value through any leading `&`
            // on the scrutinee, so `match ref_str { "foo" => ... }` is valid.
            let effective_scrut_ty = if matches!(
                &arm.pattern.kind,
                PatternKind::Literal(Literal::String(_) | Literal::RawString { .. })
            ) {
                let resolved = self.infer.resolve(self.tcx, scrut_ty);
                match self.tcx.kind(resolved) {
                    Some(TyKind::Ref { inner, .. }) => *inner,
                    _ => scrut_ty,
                }
            } else {
                scrut_ty
            };
            // Unify BEFORE binding: the pattern's synthesized type carries
            // fresh payload vars, and binding resolves each binder through
            // the inference table - unifying first lets a variant pattern's
            // binders see the scrutinee's concrete payload types instead of
            // unresolved vars.
            self.unify(effective_scrut_ty, pat_ty, arm.pattern.span);
            self.bind_pattern(&arm.pattern, pat_ty);
            let resolved_scrut_ty = self.infer.resolve(self.tcx, scrut_ty);
            if matches!(self.tcx.kind(resolved_scrut_ty), Some(TyKind::Ref { .. }))
                && let Some(scrutinee_binding) = Self::place_root_name(scrutinee)
            {
                let origin = self
                    .reference_origin(&scrutinee_binding)
                    .unwrap_or(&scrutinee_binding)
                    .to_string();
                self.register_pattern_reference_origins(&arm.pattern, &origin);
            }
            if let Some(guard) = &arm.guard {
                let guard_ty = self.check_expr(guard);
                let bool_ty = self.tcx.bool_ty();
                self.unify(bool_ty, guard_ty, guard.span);
            }
            let body_ty = self.check_expr_expecting(&arm.body, expected);
            result_ty = self.join_branch_tys(result_ty, body_ty, branch_value_span(&arm.body));
            self.pop_scope();
        }
        // Second pass: if the arms joined to a Vec/slice, re-record every
        // array-literal arm body to that shape so an unannotated
        // `let v = match n { 0 => ["a"], _ => ["b", "c"] }` lowers each
        // arm as a heap Vec, matching the joined result slot.
        for arm in arms {
            self.adjust_literal_to_join(&arm.body, result_ty);
        }
        result_ty
    }

    pub(super) fn match_scrutinee_expectation(&mut self, arms: &[MatchArm]) -> Expectation {
        let mut wants_result = false;
        let mut wants_option = false;
        for arm in arms {
            match Self::builtin_enum_pattern_family(&arm.pattern) {
                Some(BuiltinPatternFamily::Result) => wants_result = true,
                Some(BuiltinPatternFamily::Option) => wants_option = true,
                None => {}
            }
        }
        match (wants_result, wants_option) {
            (true, false) => {
                let ok_ty = self.fresh();
                let err_ty = self.fresh();
                Expectation::HasType(self.result_adt_ty(ok_ty, err_ty))
            }
            (false, true) => {
                let payload = self.fresh();
                Expectation::HasType(self.option_adt_ty(payload))
            }
            _ => Expectation::None,
        }
    }

    pub(super) fn builtin_enum_pattern_family(pattern: &Pattern) -> Option<BuiltinPatternFamily> {
        match &pattern.kind {
            PatternKind::TupleStruct { path, .. } | PatternKind::Path(path) => {
                match Self::bare_result_option_ctor(path)? {
                    "Ok" | "Err" => Some(BuiltinPatternFamily::Result),
                    "Some" | "None" => Some(BuiltinPatternFamily::Option),
                    _ => None,
                }
            }
            PatternKind::Or(alts) => {
                let mut family = None;
                for alt in alts {
                    let Some(next) = Self::builtin_enum_pattern_family(alt) else {
                        continue;
                    };
                    if let Some(prev) = family
                        && prev != next
                    {
                        return None;
                    }
                    family = Some(next);
                }
                family
            }
            PatternKind::Ref { inner, .. } => Self::builtin_enum_pattern_family(inner),
            _ => None,
        }
    }

    /// Rejects `Ok` / `Err` / `Some` / `None` arms whose scrutinee's
    /// resolved head is not the matching `Result` / `Option`. The
    /// `unify` mismatch is suppressed for these arms because the
    /// synthesized pattern type carries unresolved payload vars, so the
    /// hole is closed with a direct GT0001 here. Skips a scrutinee whose
    /// head is still an inference variable so a not-yet-resolved shape is
    /// never flagged.
    pub(super) fn reject_constructor_scrutinee_mismatch(
        &mut self,
        scrut_ty: Ty,
        arms: &[MatchArm],
    ) {
        let resolved = self.infer.resolve(self.tcx, scrut_ty);
        let resolved = match self.tcx.kind(resolved) {
            Some(TyKind::Ref { inner, .. }) => self.infer.resolve(self.tcx, *inner),
            _ => resolved,
        };
        // Judge the container by the resolved head only: an unresolved
        // scrutinee (`Var`) or an error type carries no decision; a
        // partially-inferred `Result<_, Var>` still has a known `Result`
        // head, so a `Some` arm against it is correctly rejected.
        match self.tcx.kind(resolved) {
            Some(TyKind::Var(_) | TyKind::Error) | None => return,
            _ => {}
        }
        for arm in arms {
            let ctor = match &arm.pattern.kind {
                PatternKind::TupleStruct { path, .. } | PatternKind::Path(path) => {
                    Self::bare_result_option_ctor(path)
                }
                _ => None,
            };
            let Some(ctor) = ctor else { continue };
            // `Ok` / `Err` need the `Result` sentinel (`u32::MAX`);
            // `Some` / `None` need the `Option` sentinel (`u32::MAX - 1`).
            let want_def = match ctor {
                "Ok" | "Err" => u32::MAX,
                _ => u32::MAX - 1,
            };
            let matches_container = matches!(
                self.tcx.kind(resolved),
                Some(TyKind::Adt { def, .. }) if def.local == want_def
            );
            if matches_container {
                continue;
            }
            let expected = if want_def == u32::MAX {
                let fresh_ok = self.fresh();
                let fresh_err = self.fresh();
                self.result_adt_ty(fresh_ok, fresh_err)
            } else {
                let fresh = self.fresh();
                self.option_adt_ty(fresh)
            };
            let expected = self.render_public_ty(expected);
            let found = self.render_public_ty(resolved);
            self.emit(
                TypeError::TypeMismatch { expected, found },
                arm.pattern.span,
            );
        }
    }
}

/// The place a `base[lo..hi]` window names, as a key two windows of one place
/// share: a binding reached through fields and indexes that are bindings or
/// literals, which evaluating again names the same place.
/// A place as its root binding and the steps below it, for deciding whether
/// two call arguments reach the same storage.
struct PlacePath {
    root: String,
    steps: Vec<PlaceStep>,
}

/// One step from a place to a place inside it.
#[derive(PartialEq)]
enum PlaceStep {
    Field(String),
    /// An element by index or range; any two may be the same element.
    Element,
}

impl PlacePath {
    fn of(place: &Expr) -> Option<Self> {
        match &place.kind {
            ExprKind::Path(path) => Some(Self {
                root: path.segments.first()?.name.name.clone(),
                steps: Vec::new(),
            }),
            ExprKind::FieldAccess { receiver, field } => {
                let mut path = Self::of(receiver)?;
                path.steps.push(PlaceStep::Field(match field {
                    gossamer_ast::FieldSelector::Named(name) => name.name.clone(),
                    gossamer_ast::FieldSelector::Index(index) => index.to_string(),
                }));
                Some(path)
            }
            ExprKind::Index { base, .. } => {
                let mut path = Self::of(base)?;
                path.steps.push(PlaceStep::Element);
                Some(path)
            }
            ExprKind::Unary { operand, .. } => Self::of(operand),
            _ => None,
        }
    }

    /// Whether the two places share storage: one is the other or lies
    /// inside it.
    fn overlaps(&self, other: &Self) -> bool {
        self.root == other.root
            && self
                .steps
                .iter()
                .zip(&other.steps)
                .all(|(left, right)| left == right)
    }
}

fn window_place_key(operand: &Expr) -> Option<String> {
    let ExprKind::Index { base, index } = &operand.kind else {
        return None;
    };
    if !matches!(index.kind, ExprKind::Range { .. }) {
        return None;
    }
    place_key(base)
}

fn place_key(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Path(path) if path.segments.len() == 1 => {
            Some(path.segments[0].name.name.clone())
        }
        ExprKind::FieldAccess { receiver, field } => {
            let field = match field {
                gossamer_ast::FieldSelector::Named(name) => name.name.clone(),
                gossamer_ast::FieldSelector::Index(index) => index.to_string(),
            };
            Some(format!("{}.{field}", place_key(receiver)?))
        }
        ExprKind::Index { base, index } => {
            let index = match &index.kind {
                ExprKind::Path(path) if path.segments.len() == 1 => {
                    path.segments[0].name.name.clone()
                }
                ExprKind::Literal(gossamer_ast::Literal::Int(text)) => text.clone(),
                _ => return None,
            };
            Some(format!("{}[{index}]", place_key(base)?))
        }
        ExprKind::Unary {
            op: UnaryOp::Deref,
            operand,
        } => Some(format!("*{}", place_key(operand)?)),
        _ => None,
    }
}

/// Bit width of an integer type; `isize` and `usize` are a machine word,
/// which every Gossamer target holds in 64 bits.
const fn int_bits(ty: IntTy) -> u32 {
    match ty {
        IntTy::I8 | IntTy::U8 => 8,
        IntTy::I16 | IntTy::U16 => 16,
        IntTy::I32 | IntTy::U32 => 32,
        IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => 64,
        IntTy::I128 | IntTy::U128 => 128,
    }
}

/// Whether every value of `from` is a value of `to`.
const fn int_holds(to: IntTy, from: IntTy) -> bool {
    match (to.is_signed(), from.is_signed()) {
        (true, true) | (false, false) => int_bits(to) >= int_bits(from),
        (true, false) => int_bits(to) > int_bits(from),
        (false, true) => false,
    }
}

/// The narrowest signed type holding every value of both, when one exists.
fn int_common_type(a: IntTy, b: IntTy) -> Option<IntTy> {
    [IntTy::I16, IntTy::I32, IntTy::I64]
        .into_iter()
        .find(|candidate| int_holds(*candidate, a) && int_holds(*candidate, b))
}

/// An operand a postfix `as` binds to whole; any other needs parentheses.
fn is_cast_atom(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::Literal(_)
            | ExprKind::Path(_)
            | ExprKind::Call { .. }
            | ExprKind::MethodCall { .. }
            | ExprKind::FieldAccess { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
    )
}

/// The type holding every value of both integer types: one of them when it
/// already does, else the narrowest signed type wide enough.
fn int_target(left: IntTy, right: IntTy) -> Option<IntTy> {
    if int_holds(left, right) {
        Some(left)
    } else if int_holds(right, left) {
        Some(right)
    } else {
        int_common_type(left, right)
    }
}

/// The text inserted before and after an operand to cast it to `target`.
fn cast_insertions(operand: &Expr, target: IntTy) -> (String, String) {
    if is_cast_atom(operand) {
        (String::new(), format!(" as {}", target.as_str()))
    } else {
        ("(".to_string(), format!(") as {}", target.as_str()))
    }
}

/// `text` with the insertions around it.
fn with_insertions(text: &str, (before, after): &(String, String)) -> String {
    format!("{before}{text}{after}")
}

/// The GT0001 diagnostic for two integer operands of different types, with
/// the casts to the type that holds every value of both.
pub(super) fn integer_operand_mismatch(
    op: &str,
    (lhs, left): (&Expr, IntTy),
    (rhs, right): (&Expr, IntTy),
) -> TypeError {
    let target = int_target(left, right);
    let mut casts = Vec::new();
    let mut rendered = Vec::new();
    for (operand, ty) in [(lhs, left), (rhs, right)] {
        let text = expr_display(operand);
        match target {
            Some(target) if target != ty => {
                let insertions = cast_insertions(operand, target);
                rendered.push(text.map(|text| with_insertions(&text, &insertions)));
                casts.push((operand.span, insertions.0, insertions.1));
            }
            _ => rendered.push(text),
        }
    }
    let rewritten = match (&rendered[0], &rendered[1]) {
        (Some(left_text), Some(right_text)) if !casts.is_empty() => {
            Some(format!("{left_text} {op} {right_text}"))
        }
        _ => None,
    };
    TypeError::IntegerOperandMismatch {
        op: op.to_string(),
        lhs: left.as_str().to_string(),
        rhs: right.as_str().to_string(),
        fix: Box::new(crate::error::IntegerCastFix {
            target: target.map(|target| target.as_str().to_string()),
            casts,
            rewritten,
        }),
    }
}

/// The GT0001 diagnostic for an integer method (`a.min(b)`) whose argument
/// has another integer type than its receiver. A cast receiver is wrapped
/// whole, so the method still applies to the cast value.
pub(super) fn integer_method_mismatch(
    method: &str,
    (receiver, receiver_ty): (&Expr, IntTy),
    (args, mismatched, arg_ty): (&[Expr], usize, IntTy),
) -> TypeError {
    let target = int_target(receiver_ty, arg_ty);
    let mut casts = Vec::new();
    let mut receiver_text = expr_display(receiver);
    let mut arg_texts: Vec<Option<String>> = args.iter().map(expr_display).collect();
    if let Some(target) = target {
        if target != receiver_ty {
            let insertions = ("(".to_string(), format!(" as {})", target.as_str()));
            receiver_text = receiver_text.map(|text| with_insertions(&text, &insertions));
            casts.push((receiver.span, insertions.0, insertions.1));
        }
        if target != arg_ty {
            let arg = &args[mismatched];
            let insertions = cast_insertions(arg, target);
            arg_texts[mismatched] = arg_texts[mismatched]
                .take()
                .map(|text| with_insertions(&text, &insertions));
            casts.push((arg.span, insertions.0, insertions.1));
        }
    }
    let arg_texts: Option<Vec<String>> = arg_texts.into_iter().collect();
    let rewritten = match (receiver_text, arg_texts) {
        (Some(receiver), Some(args)) if !casts.is_empty() => {
            Some(format!("{receiver}.{method}({})", args.join(", ")))
        }
        _ => None,
    };
    TypeError::IntegerOperandMismatch {
        op: format!(".{method}"),
        lhs: receiver_ty.as_str().to_string(),
        rhs: arg_ty.as_str().to_string(),
        fix: Box::new(crate::error::IntegerCastFix {
            target: target.map(|target| target.as_str().to_string()),
            casts,
            rewritten,
        }),
    }
}

/// For a bitwise operator, whether it also applies to `bool` (it does);
/// `None` for every other operator.
fn bit_operator_allows_bool(op: BinaryOp) -> Option<bool> {
    match op {
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => Some(true),
        _ => None,
    }
}

/// The spelling of an integer literal, negated or not, with no suffix.
fn literal_int_spelling(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Literal(Literal::Int(text))
            if !INT_SUFFIXES
                .iter()
                .any(|(suffix, _)| text.ends_with(suffix)) =>
        {
            Some(text.clone())
        }
        ExprKind::Unary {
            op: UnaryOp::Neg,
            operand,
        } => literal_int_spelling(operand).map(|text| format!("-{text}")),
        _ => None,
    }
}
