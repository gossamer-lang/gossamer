//! Method calls: receiver dispatch and each built-in receiver's surface.

use super::{
    AUTOMATIC_METHODS, AstGenericArg, BINARY_HEAP_DEF_LOCAL, BTREE_MAP_ONLY_METHODS,
    BTREE_SET_DEF_LOCAL, BTREE_SET_ONLY_METHODS, COLLECTION_TRAVERSAL_METHODS, DEQUE_METHODS,
    DURATION_METHODS, DataPosition, DefId, DeferredMutatingReceiver, DeferredStructuralKind,
    Expectation, Expr, ExprKind, FloatTy, FnSig, HANDLE_SENTINEL_SPAN, HASH_SET_DEF_LOCAL, HashSet,
    INSTANT_METHODS, IntTy, MIN_HEAP_DEF_LOCAL, MethodCallSite, Mutbl, NodeId, OPTION_DEF_LOCAL,
    OPTION_METHODS, OwnGenericMethodSig, PUSH_POP_METHODS, PlaceMut, RESULT_DEF_LOCAL,
    RESULT_METHODS, Resolution, SET_METHODS, Span, Ty, TyKind, TypeChecker, TypeError,
    VEC_DEQUE_DEF_LOCAL, VEC_QUEUE_DEF_LOCAL, VEC_STACK_DEF_LOCAL, Visibility,
    builtin_trait_methods, core_type_own_method_names, is_array_sequence_method,
    is_btree_map_method, is_definitely_not_callable_value, is_free_call_only_traversal,
    is_map_method, is_slice_sequence_method, is_soft_for_structural_use, is_string_method,
    is_tuple_rejected_method, is_vec_only_sequence_method, iterator_receiver_accepts_method,
    render_array_len, render_ty, wrapping_method_operator, wrapping_operator_rewrite,
};

impl TypeChecker<'_> {
    /// `recv` / `try_recv` on a `Receiver<T>` yields `Option<T>`, and `send`
    /// / `try_send` on a `Sender<T>` consumes a `T`. Pinning the element type
    /// here sizes a `while let Some(p) = rx.recv()` binding by `T`'s real slot
    /// count, so a struct sent over a channel materialises as its inline
    /// fields rather than a single pointer word. Returns `None` for any
    /// non-channel receiver so the caller continues normal dispatch.
    pub(super) fn check_channel_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
    ) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Receiver(elem)) if matches!(method, "recv" | "try_recv" | "recv_ctx") => {
                let elem = *elem;
                for arg in args {
                    self.check_expr(arg);
                }
                Some(self.option_adt_ty(elem))
            }
            // `join` on a `JoinHandle<T>` yields `Result<T, String>`: the
            // goroutine's value, or the message it panicked with. Pinning
            // it here is what lets `handle.join()?` propagate, and what
            // gives the binding `T`'s real shape rather than a bare word.
            Some(TyKind::JoinHandle(elem)) if method == "join" && args.is_empty() => {
                let elem = *elem;
                let message = self.tcx.string_ty();
                Some(self.result_adt_ty(elem, message))
            }
            Some(TyKind::Sender(elem)) if matches!(method, "send" | "try_send") => {
                let elem = *elem;
                for arg in args {
                    let v = self.check_expr(arg);
                    self.unify(elem, v, arg.span);
                    let v = self.infer.resolve(self.tcx, v);
                    if self.ty_contains_reference(v) {
                        self.emit(
                            TypeError::ReferenceEscapeUnsupported {
                                context: "be stored in a channel".to_string(),
                            },
                            arg.span,
                        );
                    }
                }
                Some(self.tcx.unit())
            }
            _ => None,
        }
    }

    /// Phase 1 `VecDeque` support is backed by the native `i64` deque ABI.
    /// Keep method typing aligned with that runtime until the handle is made
    /// fully generic.
    pub(super) fn check_deque_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
    ) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved)
            && def.local == VEC_DEQUE_DEF_LOCAL
        {
            let elem = self.slot_collection_elem_as_written(substs.types().first().copied());
            return match method {
                "push_back" | "push_front" if args.len() == 1 => {
                    let v = self.check_expr_expecting(&args[0], Expectation::HasType(elem));
                    self.unify(elem, v, args[0].span);
                    Some(self.tcx.unit())
                }
                "pop_back" | "pop_front" | "peek_back" | "peek_front" if args.is_empty() => {
                    Some(self.option_adt_ty(elem))
                }
                "len" if args.is_empty() => Some(self.tcx.int_ty(IntTy::I64)),
                "is_empty" if args.is_empty() => Some(self.tcx.bool_ty()),
                "clear" if args.is_empty() => Some(self.tcx.unit()),
                _ => None,
            };
        }
        None
    }

    /// The ordered surface only a `BTreeMap` has: its first and last entries,
    /// taking them, and the entries between two keys. `range` reads its
    /// bounds from the range written in the call, each typed as a key.
    pub(super) fn check_btree_map_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
    ) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if BTREE_SET_ONLY_METHODS.contains(&method)
            && let Some((owner, elem)) = self.set_elem_ty(resolved)
            && owner == "BTreeSet"
        {
            return Some(self.check_ordered_surface(method, elem, elem, args));
        }
        if !BTREE_MAP_ONLY_METHODS.contains(&method) {
            return None;
        }
        let Some(TyKind::HashMap {
            key,
            value,
            ordered: true,
        }) = self.tcx.kind(resolved)
        else {
            return None;
        };
        let (key, value) = (*key, *value);
        let pair = self.tcx.intern(TyKind::Tuple(vec![key, value]));
        Some(self.check_ordered_surface(method, key, pair, args))
    }

    /// Types one of the ordered surface's methods over keys `key` whose
    /// entries read as `entry`: the first / last entry and taking it, and a
    /// range of keys.
    pub(super) fn check_ordered_surface(
        &mut self,
        method: &str,
        key: Ty,
        entry: Ty,
        args: &[Expr],
    ) -> Ty {
        let owner = if key == entry { "BTreeSet" } else { "BTreeMap" };
        match (method, args) {
            (
                "first_key_value" | "last_key_value" | "first" | "last" | "pop_first" | "pop_last",
                [],
            ) => self.option_adt_ty(entry),
            ("range", [arg]) => {
                if let ExprKind::Range { start, end, .. } = &arg.kind {
                    for bound in [start, end].into_iter().flatten() {
                        let ty = self.check_expr_expecting(bound, Expectation::HasType(key));
                        self.unify(key, ty, bound.span);
                    }
                    let range = self.tcx.intern(TyKind::Range(key));
                    self.record(arg.id, range);
                } else {
                    self.check_expr(arg);
                    let key = self.render_public_ty(key);
                    self.emit(TypeError::OrderedRangeArgument { key }, arg.span);
                }
                self.tcx.intern(TyKind::Iterator(entry))
            }
            _ => {
                self.emit(
                    TypeError::CallArityMismatch {
                        callee: format!("{owner}::{method}"),
                        expected: usize::from(method == "range"),
                        found: args.len(),
                    },
                    args.first().map_or(Span::default(), |a| a.span),
                );
                self.tcx.error_ty()
            }
        }
    }

    pub(super) fn check_queue_or_stack_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
    ) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved)
            && matches!(def.local, VEC_QUEUE_DEF_LOCAL | VEC_STACK_DEF_LOCAL)
        {
            let elem = self.slot_collection_elem_as_written(substs.types().first().copied());
            return match method {
                "push" if args.len() == 1 => {
                    let v = self.check_expr_expecting(&args[0], Expectation::HasType(elem));
                    self.unify(elem, v, args[0].span);
                    Some(self.tcx.unit())
                }
                "pop" | "peek" if args.is_empty() => Some(self.option_adt_ty(elem)),
                "len" if args.is_empty() => Some(self.tcx.int_ty(IntTy::I64)),
                "is_empty" if args.is_empty() => Some(self.tcx.bool_ty()),
                "clear" if args.is_empty() => Some(self.tcx.unit()),
                _ => None,
            };
        }
        None
    }

    pub(super) fn check_binary_heap_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
        span: gossamer_lex::Span,
    ) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let elem_ty = match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs })
                if matches!(def.local, BINARY_HEAP_DEF_LOCAL | MIN_HEAP_DEF_LOCAL) =>
            {
                substs.types().first().copied()
            }
            _ => return None,
        };
        let elem_ty = self.slot_collection_elem_as_written(elem_ty);
        match method {
            "push" if args.len() == 1 => {
                let got = self.check_expr_expecting(&args[0], Expectation::HasType(elem_ty));
                self.unify(elem_ty, got, args[0].span);
                Some(self.tcx.unit())
            }
            "pop" | "peek" if args.is_empty() => Some(self.option_adt_ty(elem_ty)),
            "len" if args.is_empty() => Some(self.tcx.int_ty(IntTy::I64)),
            "is_empty" if args.is_empty() => Some(self.tcx.bool_ty()),
            "clear" if args.is_empty() => Some(self.tcx.unit()),
            "push" | "pop" | "peek" | "len" | "is_empty" | "clear" => {
                let expected = usize::from(method == "push");
                let owner = self.render_public_ty(resolved);
                self.emit(
                    TypeError::CallArityMismatch {
                        callee: format!("{owner}::{method}"),
                        expected,
                        found: args.len(),
                    },
                    span,
                );
                Some(self.tcx.error_ty())
            }
            _ => None,
        }
    }

    /// Expected closure parameter types for a `Vec`/slice/array
    /// closure-combinator method (`xs.sort_by(cmp)`, `xs.map(f)`), or
    /// `None` when the method is not such a combinator or the receiver is
    /// not a sequence. Comparator-shaped methods take two element
    /// parameters; the rest take one.
    pub(super) fn vec_combinator_closure_inputs(
        &mut self,
        method: &str,
        receiver_ty: Ty,
    ) -> Option<Vec<Ty>> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        // An `Option` / `Result` receiver hands its payload to the closure the
        // same way a sequence hands over its element. Without this the closure
        // body is checked against an unconstrained parameter, so a projection
        // out of the payload never resolves and the mapped payload stays a
        // free variable.
        if let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved) {
            let payload_family = match def.local {
                d if d == u32::MAX - 1 => Some(0),
                d if d == u32::MAX => Some(0),
                _ => None,
            };
            if let Some(index) = payload_family {
                let payloads = substs.types();
                let ok = payloads.get(index).copied();
                let err = payloads.get(1).copied();
                if let Some(ok) = ok {
                    return match method {
                        "map" | "and_then" | "filter" | "is_some_and" | "inspect" => Some(vec![ok]),
                        "map_err" | "or_else" => err.map(|e| vec![e]),
                        _ => None,
                    };
                }
            }
        }
        let elem = match self.tcx.kind(resolved) {
            Some(
                TyKind::Vec(elem)
                | TyKind::Slice(elem)
                | TyKind::Array { elem, .. }
                | TyKind::Iterator(elem),
            ) => *elem,
            // A map hands the closure its key/value pair, a set its value.
            Some(TyKind::HashMap { key, value, .. }) => {
                let (key, value) = (*key, *value);
                self.tcx.intern(TyKind::Tuple(vec![key, value]))
            }
            _ => self.set_elem_ty(resolved).map(|(_owner, elem)| elem)?,
        };
        match method {
            "sort_by" | "min_by" | "max_by" => Some(vec![elem, elem]),
            "sort_by_key" | "min_by_key" | "max_by_key" | "map" | "filter" | "filter_map"
            | "flat_map" | "for_each" | "any" | "all" | "find" | "position" | "find_map"
            | "take_while" | "skip_while" | "partition" | "chunk_by" | "count_by" | "sum_by"
            | "product_by" | "count" | "retain" => Some(vec![elem]),
            _ => None,
        }
    }

    /// Types a parallel adapter call: `par_map`, `par_filter`, `par_reduce`,
    /// `par_sum`, `par_min`, or `par_max` on a sequence or an integer range,
    /// or `par_chunks_mut` on a writable sequence.
    /// Each answers what its sequential twin answers, and a range answers a
    /// `Vec` where its lazy `map` would answer an iterator.
    pub(super) fn check_parallel_adapter(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        let arity = match method {
            "par_map" | "par_filter" => 1,
            "par_reduce" | "par_chunks_mut" => 2,
            "par_sum" | "par_min" | "par_max" => 0,
            _ => return None,
        };
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let elem = match self.tcx.kind(resolved).cloned() {
            Some(TyKind::Range(elem)) if method != "par_chunks_mut" => elem,
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => elem,
            Some(TyKind::Var(_)) => {
                let elem = self.fresh();
                let shaped = self.tcx.intern(TyKind::Vec(elem));
                self.unify(shaped, resolved, span);
                elem
            }
            Some(TyKind::Error) => {
                for arg in args {
                    self.check_expr(arg);
                }
                return Some(self.tcx.error_ty());
            }
            _ => {
                // A lazy iterator keeps its sequential walk, and nothing else
                // is a sequence: the adapters are not part of its surface.
                for arg in args {
                    self.check_expr(arg);
                }
                let owner = self.render_public_ty(resolved);
                let error = self.unresolved_method(owner, method, resolved);
                self.emit(error, span);
                return Some(self.tcx.error_ty());
            }
        };
        if args.len() != arity {
            for arg in args {
                self.check_expr(arg);
            }
            let owner = self.render_public_ty(resolved);
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: arity,
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        let answer = self.parallel_adapter_answer(method, elem, args, span);
        // Every worker reads the callback's captures and a fixed-array
        // receiver's elements at once, so they obey the rule a goroutine's
        // captures do.
        for arg in args {
            self.reject_unshareable_goroutine_captures(arg);
        }
        if matches!(self.tcx.kind(resolved), Some(TyKind::Array { .. }))
            && self.ty_is_unshareable_across_goroutines(resolved)
        {
            let rendered = self.render_public_ty(resolved);
            self.emit(
                TypeError::ConcurrentCaptureUnsupported {
                    name: "the receiver".to_string(),
                    ty: rendered,
                },
                span,
            );
        }
        Some(answer)
    }

    /// The type an admitted parallel adapter call answers, with its callback
    /// and seed checked against the element type `elem`.
    pub(super) fn parallel_adapter_answer(
        &mut self,
        method: &str,
        elem: Ty,
        args: &[Expr],
        span: Span,
    ) -> Ty {
        let bool_ty = self.tcx.bool_ty();
        match method {
            "par_map" => {
                let out = self.check_parallel_callback(&args[0], &[elem]);
                self.tcx.intern(TyKind::Vec(out))
            }
            "par_filter" => {
                let kept = self.check_parallel_callback(&args[0], &[elem]);
                self.unify(bool_ty, kept, args[0].span);
                self.tcx.intern(TyKind::Vec(elem))
            }
            "par_chunks_mut" => {
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                let size = self.check_expr_expecting(&args[0], Expectation::HasType(i64_ty));
                self.unify(i64_ty, size, args[0].span);
                let slice = self.tcx.intern(TyKind::Slice(elem));
                let chunk = self.tcx.intern(TyKind::Ref {
                    mutability: Mutbl::Mut,
                    inner: slice,
                });
                let unit = self.tcx.unit();
                let answered = self.check_parallel_callback(&args[1], &[i64_ty, chunk]);
                self.unify(unit, answered, args[1].span);
                unit
            }
            "par_reduce" => {
                let init = self.check_expr_expecting(&args[0], Expectation::HasType(elem));
                self.unify(elem, init, args[0].span);
                let combined = self.check_parallel_callback(&args[1], &[elem, elem]);
                self.unify(elem, combined, args[1].span);
                elem
            }
            "par_sum" => {
                let resolved_elem = self.infer.resolve(self.tcx, elem);
                if !matches!(
                    self.tcx.kind(resolved_elem),
                    Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::Var(_) | TyKind::Error)
                ) {
                    let found = self.render_public_ty(resolved_elem);
                    self.emit(
                        TypeError::TypeMismatch {
                            expected: "a sequence of numbers".to_string(),
                            found,
                        },
                        span,
                    );
                }
                elem
            }
            _ => self.option_adt_ty(elem),
        }
    }

    /// Checks a parallel adapter's callback against the parameter types it
    /// is handed, answering its return type.
    pub(super) fn check_parallel_callback(&mut self, arg: &Expr, inputs: &[Ty]) -> Ty {
        let got = match &arg.kind {
            ExprKind::Closure { params, .. } if params.len() == inputs.len() => {
                let output = self.fresh();
                let sig = FnSig {
                    inputs: inputs.to_vec(),
                    output,
                };
                let want = self.tcx.intern(TyKind::FnPtr(sig));
                self.check_expr_expecting(arg, Expectation::HasType(want))
            }
            _ => self.check_expr(arg),
        };
        self.callable_output(got, inputs, arg.span)
    }

    /// The call's argument types with the piped value appended.
    /// `x |> recv.m(a)` desugars to `recv.m(a, x)`, so the built-in
    /// receiver surface sees the piped value as the trailing argument:
    /// it counts toward the method's arity and its type is checked
    /// against the slot it lands in.
    pub(super) fn arg_tys_with_piped(&self, call_id: NodeId, arg_tys: &[Ty]) -> Vec<Ty> {
        let mut tys = arg_tys.to_vec();
        if let Some(piped) = self.pipe_stage_arg_tys.get(&call_id).copied() {
            tys.push(piped);
        }
        tys
    }

    /// Types a `wrapping_add` / `wrapping_sub` / `wrapping_mul` call on an
    /// integer and reports it with the operator that replaces it.
    pub(super) fn check_retired_wrapping_method(
        &mut self,
        resolved: Ty,
        method: &str,
        (receiver, args): (&Expr, &[Expr]),
        all_arg_tys: &[Ty],
        call_span: Span,
    ) -> Ty {
        let arg_count = all_arg_tys.len();
        if arg_count != 1 {
            let owner = self.render_public_ty(resolved);
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: 1,
                    found: arg_count,
                },
                receiver.span,
            );
            return self.tcx.error_ty();
        }
        let arg_ty = self.peel_refs(all_arg_tys[0]);
        let arg_span = args.first().map_or(receiver.span, |arg| arg.span);
        self.unify(resolved, arg_ty, arg_span);
        // Wrapping arithmetic has one spelling, the operator; the call is
        // still typed so what follows it checks as it would once fixed.
        let Some(operator) = wrapping_method_operator(method) else {
            return resolved;
        };
        let replacement = args
            .first()
            .and_then(|arg| wrapping_operator_rewrite(receiver, arg, operator));
        self.emit(
            TypeError::WrappingMethodRetired {
                method: method.to_string(),
                operator: operator.to_string(),
                replacement,
            },
            call_span,
        );
        resolved
    }

    /// A method on a channel, `Weak`, `Deque`, `BTreeMap`, `Queue`, `Stack`,
    /// or heap receiver, each of which has its own surface.
    pub(super) fn check_container_family_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
        receiver_span: Span,
    ) -> Option<Ty> {
        self.check_channel_method(method, receiver_ty, args)
            .or_else(|| self.check_weak_method(method, receiver_ty, args, receiver_span))
            .or_else(|| self.check_deque_method(method, receiver_ty, args))
            .or_else(|| self.check_btree_map_method(method, receiver_ty, args))
            .or_else(|| self.check_queue_or_stack_method(method, receiver_ty, args))
            .or_else(|| self.check_binary_heap_method(method, receiver_ty, args, receiver_span))
    }

    pub(super) fn check_method_call(
        &mut self,
        site: MethodCallSite<'_>,
        receiver: &Expr,
        args: &[Expr],
        expected: Expectation,
    ) -> Ty {
        let ty = self.check_method_call_inner(site, receiver, args, expected);
        self.check_by_value_argument_aliases(args);
        ty
    }

    #[allow(
        clippy::too_many_lines,
        reason = "receiver dispatch is intentionally kept in source order"
    )]
    fn check_method_call_inner(
        &mut self,
        site: MethodCallSite<'_>,
        receiver: &Expr,
        args: &[Expr],
        expected: Expectation,
    ) -> Ty {
        let MethodCallSite {
            call_id,
            call_span,
            method,
            name_span,
            generics,
        } = site;
        self.check_overlapping_mutable_call_args(args);
        let receiver_expected = self.method_receiver_expectation(method, receiver, expected);
        let receiver_ty = self.check_expr_expecting(receiver, receiver_expected);
        let receiver_ty = self.window_receiver_ty(receiver, receiver_ty, method, args);
        self.note_receiver_owner(call_id, receiver_ty, method);
        if method == "par_chunks_mut" {
            self.check_chunk_receiver(receiver, receiver_ty, args);
        }
        if let Some(ty) = self
            .check_simd_method(method, receiver_ty, args, generics, call_span)
            .or_else(|| self.check_parallel_adapter(method, receiver_ty, args, receiver.span))
        {
            return ty;
        }
        if self.reject_invalid_builtin_receiver_call(receiver_ty, method, args, call_id, name_span)
        {
            return self.tcx.error_ty();
        }
        self.check_mutating_method_receiver(receiver, receiver_ty, method);
        self.reject_private_method_call(receiver_ty, method, receiver.span);
        if let Some(ty) = self.check_param_receiver_method(receiver_ty, method, args, receiver.span)
        {
            return ty;
        }
        if let Some(ty) = self.reject_method_on_receiver(receiver_ty, method, args, receiver.span) {
            return ty;
        }
        // Which `impl` block this call reaches is decided by the receiver's
        // type, which is known here and nowhere later: a container and a
        // structural type both reach a method as an untyped handle below.
        self.record_method_owner(call_id, receiver_ty, method);
        self.record_method_const_generic_args(call_id, receiver_ty, method);
        // A method an `impl` block declared for a built-in type. Its own
        // surface is dispatched below and answers first, so an impl adds
        // names to a type rather than replacing any it already had.
        if let Some(ty) = self.user_impl_method_on_builtin(receiver_ty, method, args, receiver.span)
        {
            return ty;
        }
        // `wg.wait_ctx(ctx)` answers whether the group completed. A sync
        // handle's receiver stays an inference variable by design, so the
        // name carries the return type; no other receiver declares it.
        if method == "wait_ctx" && args.len() == 1 {
            for arg in args {
                self.check_expr(arg);
            }
            return self.tcx.bool_ty();
        }
        // `x.into()` converts to an inferred target `B` via `B::from`, and
        // `x.try_into()` to `Result<B, E>` via `B::try_from`. The target is
        // fixed by the use site (a `let B` / `let Result<B, E>`, a parameter,
        // a return), so type it as a fresh variable here and let unification
        // bind it; lowering reads the resolved type and routes accordingly.
        if matches!(method, "into" | "try_into") && args.is_empty() {
            return self.check_conversion_method(method, receiver_ty, receiver.span);
        }
        if let Some(ty) =
            self.check_container_family_method(method, receiver_ty, args, receiver.span)
        {
            return ty;
        }
        // Result/Option combinator methods (`r.map_err(f)`,
        // `o.map(f)`) have known signatures: type them through the
        // std combinator table so closure params pin to the payload
        // type instead of falling through unresolved.
        if let Some(ty) =
            self.check_payload_combinator_method(method, receiver_ty, receiver.span, args)
        {
            return ty;
        }
        let arg_tys = self.check_method_call_arg_tys(method, receiver_ty, args);
        let all_arg_tys = self.arg_tys_with_piped(call_id, &arg_tys);
        let arg_count = all_arg_tys.len();
        // When the receiver resolves to a non-generic Adt with a
        // recorded method return type, use it: a fresh var here
        // leaves chained results (`sel.params()`) untyped all the
        // way into codegen.
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if method == "clone" && args.is_empty() {
            return resolved;
        }
        if let Some(ty) = self.check_display_to_string(method, resolved, args) {
            return ty;
        }
        if matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Int(_) | TyKind::Var(_))
        ) && matches!(method, "wrapping_add" | "wrapping_sub" | "wrapping_mul")
        {
            return self.check_retired_wrapping_method(
                resolved,
                method,
                (receiver, args),
                &all_arg_tys,
                call_span,
            );
        }
        if self.reject_collection_method_arity(resolved, method, arg_count, name_span) {
            return self.tcx.error_ty();
        }
        if self.reject_unknown_deque_method(resolved, method, arg_count, name_span) {
            return self.tcx.error_ty();
        }
        if let Some(ty) =
            self.validate_handle_method_ret(method, args, &arg_tys, resolved, receiver.span)
        {
            return ty;
        }
        let explicit = self.turbofish_types(generics);
        let method_substs =
            self.check_user_method_args(resolved, (method, call_span), args, &arg_tys, &explicit);
        if method == "where_eq"
            && args.len() == 2
            && let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved)
            && self.tcx.def_name(*def) == Some("__gos_sql_Select")
            && let Some(value_ty) = self.tcx.enum_ty_by_name("__gos_sql_Value")
        {
            self.unify(value_ty, arg_tys[1], args[1].span);
        }
        if let Some(ty) = self.time_accessor_method_ret(resolved, method, args, &arg_tys) {
            return ty;
        }
        if let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved)
            && substs.types().is_empty()
            && let Some(name) = self.tcx.def_name(*def)
            && let Some(&ret) =
                self.method_ret_types
                    .get(&(name.to_string(), method.to_string(), arg_count))
        {
            return ret;
        }
        if let Some(ret) = self.own_generic_method_ret(
            resolved,
            method,
            (args, &arg_tys),
            (arg_count, call_span),
            &explicit,
        ) {
            return ret;
        }
        // A generic-instantiation receiver (`Wrap<f64>`) types the call
        // from the generic impl's return with the instantiation's
        // arguments substituted, so chained uses resolve concretely.
        if let Some(ret) = self.generic_recv_method_ret(resolved, method, arg_count, &method_substs)
        {
            return ret;
        }
        if let Some(ty) = self.vec_method_ret(method, &all_arg_tys, resolved, receiver.span) {
            return ty;
        }
        if let Some(ty) = self.seq_combinator_method_ret(method, &all_arg_tys, resolved, name_span)
        {
            // `next` advances the state in place and leaves it with its holder.
            if method != "next" {
                self.mark_consumed_iterator_expr(method, receiver, resolved);
            }
            return ty;
        }
        if let Some(ty) = self.set_method_ret(method, &all_arg_tys, resolved, receiver.span) {
            return ty;
        }
        if self.reject_unknown_set_method(resolved, method, arg_count, name_span) {
            return self.tcx.error_ty();
        }
        if let Some(ty) = self.map_method_ret(method, &all_arg_tys, resolved, receiver.span) {
            return ty;
        }
        if let Some(ty) = self.flag_set_method_ret(method, resolved) {
            return ty;
        }
        if let Some(ty) = self.shared_method_ret(method, resolved, args, receiver.span) {
            return ty;
        }
        if let Some(ty) = self.http_client_method_ret(method, resolved) {
            return ty;
        }
        if let Some(ty) = self.handle_family_method_ret(method, args, &arg_tys, resolved, receiver)
        {
            return ty;
        }
        if let Some(ty) = self.net_handle_method_ret(method, resolved, arg_count, receiver.span) {
            return ty;
        }
        // A `json::Value` answers the same surface in method form that
        // `json::` does as free functions, so it is typed from the same
        // table. Falling through to a fresh variable would strip the
        // JsonValue tag - leaving a chained `.set(..).set(..)` receiver
        // untagged for the compiled tiers - and would let a document read
        // bind to any annotation the caller wrote.
        if let Some(ty) = self.opaque_value_method(resolved, method, arg_count, receiver.span) {
            return ty;
        }
        if method != "clone"
            && self.reject_unknown_sequence_method(resolved, method, arg_count, name_span)
        {
            return self.tcx.error_ty();
        }
        if let Some(ty) = self.check_string_receiver_method(
            call_id,
            method,
            generics,
            expected,
            resolved,
            name_span,
            args,
            &all_arg_tys,
        ) {
            return ty;
        }
        self.check_unsurfaced_method(
            call_id,
            method,
            (receiver, resolved),
            (args, &all_arg_tys),
            name_span,
        )
    }

    /// Types a call the surfaced-receiver arms did not claim: the `math`
    /// surface reached on a scalar, then the reports for a name no
    /// receiver of this type answers. A fresh variable is the historical
    /// fallback for a receiver the checker has no surface for at all.
    pub(super) fn check_unsurfaced_method(
        &mut self,
        call_id: NodeId,
        method: &str,
        (receiver, resolved): (&Expr, Ty),
        (args, arg_tys): (&[Expr], &[Ty]),
        span: Span,
    ) -> Ty {
        let arg_count = arg_tys.len();
        if let Some(ty) = self.float_mul_add_method_ret(method, resolved, args) {
            return ty;
        }
        if let Some(ty) = self.float_bits_method_ret(method, resolved, arg_count) {
            return ty;
        }
        if let Some(ty) = self.check_numeric_receiver_method(method, resolved, arg_count) {
            if matches!(method, "min" | "max" | "clamp") {
                self.unify_bound_arguments(method, (receiver, resolved), args, arg_tys);
            }
            return ty;
        }
        if let Some(name) = self.payload_adt_method_owner(resolved)
            && !matches!(method, "clone")
        {
            let error = self.unresolved_method(name.to_string(), method, resolved);
            self.emit(error, span);
            return self.tcx.error_ty();
        }
        self.check_method_arity(call_id, resolved, method, args, span);
        self.maybe_reject_unknown_adt_method(resolved, method, span);
        if self.reject_unknown_scalar_method(resolved, method, span)
            || self.reject_unknown_time_method(resolved, method, span)
        {
            return self.tcx.error_ty();
        }
        if self.reject_unknown_stdlib_handle_method(resolved, method, span) {
            return self.tcx.error_ty();
        }
        self.fresh()
    }

    /// Rejects `req.nowhere()` on a handle whose method surface the checker
    /// owns in full. Most runtime handles resolve their methods in the tier
    /// lowerings, so an unknown name on one has to stay open; the handles
    /// listed here answer a closed table in [`Self::http_client_method_ret`],
    /// and a name that table does not claim has no binding on any tier. The
    /// VM refuses such a call and the native build ends with an undefined
    /// `@name` symbol once the whole program has compiled, so naming it at
    /// the call site is what shows the reader the receiver's own surface.
    pub(super) fn reject_unknown_stdlib_handle_method(
        &mut self,
        resolved: Ty,
        method: &str,
        span: Span,
    ) -> bool {
        const CLOSED_HANDLE_SURFACES: &[&str] = &[
            "http::Client",
            "http::ClientBuilder",
            "http::Request",
            "http::ResponseStream",
        ];
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return false;
        };
        let def = *def;
        if def.local < u32::MAX - HANDLE_SENTINEL_SPAN {
            return false;
        }
        let Some(name) = self.tcx.def_name(def).map(str::to_string) else {
            return false;
        };
        if !CLOSED_HANDLE_SURFACES.contains(&name.as_str()) {
            return false;
        }
        // The conversions every value answers, and any name a user impl or
        // trait declares for this receiver, keep their own resolution.
        if matches!(method, "clone" | "into" | "try_into" | "to_string")
            || self.user_method_owners.contains_key(method)
        {
            return false;
        }
        let error = self.unresolved_method(name, method, resolved);
        self.emit(error, span);
        true
    }

    /// Rejects `x.nowhere()` on a scalar receiver. A scalar declares no
    /// methods of its own: the surface it answers is the `math` row set,
    /// the conversions, and a `use` that binds the name as a free value.
    /// Anything else has no binding on any tier and can only fail at run
    /// time, so it is named here instead.
    pub(super) fn reject_unknown_time_method(
        &mut self,
        resolved: Ty,
        method: &str,
        span: Span,
    ) -> bool {
        let surface = match self.tcx.kind(resolved) {
            Some(TyKind::Duration) => DURATION_METHODS,
            Some(TyKind::Instant) => INSTANT_METHODS,
            _ => return false,
        };
        if surface.contains(&method)
            || self.user_method_owners.contains_key(method)
            || matches!(method, "clone" | "to_string")
        {
            return false;
        }
        let ty = self.render_public_ty(resolved);
        let error = self.unresolved_method(ty, method, resolved);
        self.emit(error, span);
        true
    }

    pub(super) fn reject_unknown_scalar_method(
        &mut self,
        resolved: Ty,
        method: &str,
        span: Span,
    ) -> bool {
        // An unsuffixed literal is still an inference variable here: its
        // width is pinned by defaulting once every item is checked, so the
        // report waits until the scalar it names is known.
        let literal = self.infer.is_integer_constrained_var(self.tcx, resolved)
            || self.infer.is_float_literal_var(self.tcx, resolved);
        if !literal
            && !matches!(
                self.tcx.kind(resolved),
                Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char)
            )
        {
            return false;
        }
        // A user `impl` reaches a scalar only when it names that scalar's own
        // type: a method some struct declares - or the `cmp` / `eq` every
        // struct is given - is not one an integer answers. A literal's type is
        // known once defaulting has run, so its owner is checked then.
        let declared = (!literal && self.user_impl_declares(resolved, method))
            || gossamer_resolve::is_prelude_value(method)
            || self.import_binds_free_name(method)
            // The conversions every value answers, which no signature row
            // describes. `cmp`, `eq`, `fmt`, and `hash` are absent on
            // purpose: a scalar orders with `<`, compares with `==`, renders
            // through `{}`, and keys a map by value, so no tier binds a
            // method spelling for them - the same surface `Vec`, `Map`,
            // `Set`, and a tuple present.
            || matches!(
                method,
                "clone"
                    | "into"
                    | "to_string"
                    | "try_into"
            );
        if declared {
            return false;
        }
        if literal {
            self.deferred_scalar_method_rejections
                .push((resolved, method.to_string(), span));
            return false;
        }
        let ty = self.render_public_ty(resolved);
        let error = self.unresolved_method(ty, method, resolved);
        self.emit(error, span);
        true
    }

    /// Reports a closure handed to a binding callback whose parameter or
    /// result type nothing in the program decides. A compiled program calls
    /// the closure's code through the register class of each type, so each
    /// has to be known.
    pub(super) fn check_deferred_binding_callbacks(&mut self) {
        for (ty, callee, span) in std::mem::take(&mut self.deferred_binding_callbacks) {
            let resolved = self.deep_resolve(ty);
            let (TyKind::FnPtr(sig) | TyKind::FnTrait(sig)) = self.tcx.kind_of(resolved).clone()
            else {
                continue;
            };
            let untyped = sig
                .inputs
                .iter()
                .chain(std::iter::once(&sig.output))
                .any(|t| {
                    let resolved = self.deep_resolve(*t);
                    matches!(self.tcx.kind(resolved), Some(TyKind::Var(_)))
                });
            if untyped {
                self.emit(TypeError::BindingCallbackUntyped { callee }, span);
            }
        }
    }

    /// Records the owner of each method call on a numeric literal, once
    /// defaulting has given the literal its type.
    /// Records, for a call the named-argument rewrite waits on, the type
    /// `receiver_ty` names, or the bound trait declaring `method` for a
    /// type-parameter receiver, by its declared name.
    fn note_receiver_owner(&mut self, call_id: NodeId, receiver_ty: Ty, method: &str) {
        if !self.receiver_owner_watch.contains(&call_id) {
            return;
        }
        let resolved = self.peel_refs(self.infer.resolve(self.tcx, receiver_ty));
        let owner = if let Some(TyKind::Param { idx, .. }) = self.tcx.kind(resolved) {
            let idx = idx.0 as usize;
            self.current_param_bounds.get(idx).and_then(|bounds| {
                bounds
                    .iter()
                    .find(|bound| {
                        self.trait_method_ret
                            .contains_key(&((*bound).clone(), method.to_string()))
                    })
                    .cloned()
            })
        } else {
            self.impl_owner_of(resolved)
        };
        if let Some(owner) = owner {
            let head = owner.rsplit("::").next().unwrap_or(&owner).to_string();
            self.receiver_owners.insert(call_id, head);
        }
    }

    pub(super) fn record_deferred_method_owners(&mut self) {
        for (call_id, receiver_ty, method) in std::mem::take(&mut self.deferred_method_owners) {
            let resolved = self.deep_resolve(receiver_ty);
            self.record_method_owner(call_id, resolved, &method);
        }
    }

    /// Reports `x.name()` on a numeric literal, once defaulting has given
    /// the literal the width its diagnostic names.
    pub(super) fn check_deferred_scalar_method_rejections(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_scalar_method_rejections);
        for (resolved, method, span) in deferred {
            let resolved = self.deep_resolve(resolved);
            if !matches!(
                self.tcx.kind(resolved),
                Some(TyKind::Int(_) | TyKind::Float(_))
            ) || self.user_impl_declares(resolved, &method)
            {
                continue;
            }
            let ty = self.render_public_ty(resolved);
            let error = self.unresolved_method(ty, &method, resolved);
            self.emit(error, span);
        }
    }

    /// Whether a `use` in this file binds `name` as an unqualified value,
    /// which is what `x.name()` on a scalar needs: the call is the free call
    /// `name(x)`, and a std function is reachable only through its module
    /// path or an item import of its own.
    pub(super) fn import_binds_free_name(&self, name: &str) -> bool {
        self.import_targets
            .iter()
            .filter(|((_, bound), _)| bound == name)
            .any(|(_, full)| {
                full.first().map(String::as_str) != Some("std")
                    || gossamer_resolve::is_stdlib_item_path(&full.join("::"))
            })
    }

    /// Return type of `f64::to_bits` / `f64::from_bits` and their `f32`
    /// siblings, written as associated functions on the primitive.
    pub(super) fn float_bits_assoc_ret(&mut self, module: &[&str], last: &str) -> Option<Ty> {
        match (module, last) {
            (["f64"], "to_bits") => Some(self.tcx.int_ty(IntTy::U64)),
            (["f64"], "from_bits") => Some(self.tcx.float_ty(FloatTy::F64)),
            (["f32"], "to_bits") => Some(self.tcx.int_ty(IntTy::U32)),
            (["f32"], "from_bits") => Some(self.tcx.float_ty(FloatTy::F32)),
            (["f64"], "mul_add") => Some(self.tcx.float_ty(FloatTy::F64)),
            (["f32"], "mul_add") => Some(self.tcx.float_ty(FloatTy::F32)),
            _ => None,
        }
    }

    /// `x.mul_add(a, b)` on a float receiver: `x * a + b` rounded once, in
    /// the receiver's own width, with both arguments of that type.
    pub(super) fn float_mul_add_method_ret(
        &mut self,
        method: &str,
        resolved: Ty,
        args: &[Expr],
    ) -> Option<Ty> {
        if method != "mul_add" || args.len() != 2 {
            return None;
        }
        let float = matches!(self.tcx.kind(resolved), Some(TyKind::Float(_)))
            || (matches!(self.tcx.kind(resolved), Some(TyKind::Var(_)))
                && self.infer.is_float_literal_var(self.tcx, resolved));
        if !float {
            return None;
        }
        for arg in args {
            let got = self.check_expr_expecting(arg, Expectation::HasType(resolved));
            self.unify(resolved, got, arg.span);
        }
        Some(resolved)
    }

    /// `x.to_bits()` on a float receiver: the method spelling of
    /// [`Self::float_bits_assoc_ret`], answering the unsigned integer of
    /// the receiver's own width.
    pub(super) fn float_bits_method_ret(
        &mut self,
        method: &str,
        resolved: Ty,
        arg_count: usize,
    ) -> Option<Ty> {
        if method != "to_bits" || arg_count != 0 {
            return None;
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Float(FloatTy::F32)) => Some(self.tcx.int_ty(IntTy::U32)),
            Some(TyKind::Float(FloatTy::F64)) => Some(self.tcx.int_ty(IntTy::U64)),
            Some(TyKind::Var(_)) if self.infer.is_float_literal_var(self.tcx, resolved) => {
                Some(self.tcx.int_ty(IntTy::U64))
            }
            _ => None,
        }
    }

    /// Gives each argument of `a.min(b)`, `a.max(b)`, or `a.clamp(lo, hi)`
    /// the receiver's type, which the call answers in; an integer argument
    /// of another integer type is the GT0001 that names the cast.
    fn unify_bound_arguments(
        &mut self,
        method: &str,
        (receiver, receiver_ty): (&Expr, Ty),
        args: &[Expr],
        arg_tys: &[Ty],
    ) {
        for (index, (arg, arg_ty)) in args.iter().zip(arg_tys).enumerate() {
            let receiver_res = self.infer.resolve(self.tcx, receiver_ty);
            let arg_res = self.infer.resolve(self.tcx, *arg_ty);
            if let (Some(TyKind::Int(receiver_int)), Some(TyKind::Int(arg_int))) =
                (self.tcx.kind(receiver_res), self.tcx.kind(arg_res))
                && receiver_int != arg_int
            {
                let error = super::operators::integer_method_mismatch(
                    method,
                    (receiver, *receiver_int),
                    (args, index, *arg_int),
                );
                self.emit(error, arg.span);
                continue;
            }
            self.unify(receiver_ty, *arg_ty, arg.span);
        }
    }

    /// Types `x.sqrt()`, `(-2).abs()`, `a.pow(b)` and the rest of the
    /// `math` surface reached in method position on a numeric receiver.
    ///
    /// The receiver is the function's first argument, so the arity and
    /// the answer both come from the `math` signature row. `abs`, `min`,
    /// `max`, and `clamp` answer in the receiver's own type; every other
    /// row computes in floating point whatever it was handed.
    pub(super) fn check_numeric_receiver_method(
        &mut self,
        method: &str,
        resolved: Ty,
        arg_count: usize,
    ) -> Option<Ty> {
        // An unsuffixed literal's width is pinned at the end of
        // inference, so a numeric receiver reached from one is still a
        // variable here - constrained to a family, which is all this
        // needs to answer in.
        let receiver_is_float = match self.tcx.kind(resolved) {
            Some(TyKind::Float(_)) => true,
            Some(TyKind::Int(_)) => false,
            Some(TyKind::Var(_)) => {
                if self.infer.is_float_literal_var(self.tcx, resolved) {
                    true
                } else if self.infer.is_integer_constrained_var(self.tcx, resolved) {
                    false
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        let shape = crate::stdlib_signatures::function_shape_for_path(&["math"], method)?;
        if shape.params.len() != arg_count + 1 {
            return None;
        }
        if shape.return_ty == "bool" {
            return Some(self.tcx.bool_ty());
        }
        if receiver_is_float || matches!(method, "abs" | "min" | "max" | "clamp") {
            return Some(resolved);
        }
        Some(self.tcx.float_ty(FloatTy::F64))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the helper preserves the method-call checking context"
    )]
    pub(super) fn check_string_receiver_method(
        &mut self,
        call_id: NodeId,
        method: &str,
        generics: &[AstGenericArg],
        expected: Expectation,
        resolved: Ty,
        span: Span,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        if !matches!(self.tcx.kind(resolved), Some(TyKind::String)) {
            return None;
        }
        let expected_arity = match method {
            // The intrinsic String surface: the rest of the catalogue is
            // shared with the `strings::` free functions and gets its arity
            // from `check_strings_arity`.
            "clear" | "len" | "is_empty" | "as_bytes" => Some(0),
            "truncate" | "push" | "push_str" | "push_char" | "push_byte" => Some(1),
            "push_utf8" | "push_json_quoted" => Some(3),
            _ => None,
        };
        if let Some(expected_arity) = expected_arity
            && arg_tys.len() != expected_arity
        {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("String::{method}"),
                    expected: expected_arity,
                    found: arg_tys.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        if let Some(arg_ty) = arg_tys.first().copied() {
            let want = match method {
                "push" | "push_char" => Some(self.tcx.intern(TyKind::Char)),
                "push_str" => Some(self.tcx.string_ty()),
                "push_byte" | "truncate" => Some(self.tcx.int_ty(IntTy::I64)),
                "push_utf8" | "push_json_quoted" => {
                    let u8_ty = self.tcx.int_ty(IntTy::U8);
                    Some(self.tcx.intern(TyKind::Vec(u8_ty)))
                }
                _ => None,
            };
            if let Some(want) = want {
                let found = self.peel_refs(arg_ty);
                let arg_span = args.first().map_or(span, |arg| arg.span);
                self.unify(want, found, arg_span);
            }
        }
        self.check_string_method_args_if_needed(call_id, method, span, args, arg_tys);
        Some(self.string_method_ret(method, generics, expected, span))
    }

    /// Method names the checker resolves on `resolved`, in the order a
    /// diagnostic lists them. Empty for a receiver with no tabled surface,
    /// which leaves the diagnostic without a did-you-mean.
    /// The owner key an `impl` block for `resolved` registers its methods
    /// under, when the type has a spelling an impl header can name.
    ///
    /// An impl's owner is the last segment of the path it is written for, so
    /// a type reachable only as a structural spelling - `[T]`, `[T; N]`,
    /// `(A, B)` - has no key here and registers its methods by name alone.
    pub(super) fn builtin_impl_owner(&self, resolved: Ty) -> Option<&'static str> {
        match self.tcx.kind(resolved)? {
            TyKind::String => Some("String"),
            TyKind::Vec(_) => Some("Vec"),
            TyKind::HashMap { ordered, .. } => Some(if *ordered { "BTreeMap" } else { "Map" }),
            TyKind::Bool => Some("bool"),
            TyKind::Char => Some("char"),
            TyKind::Int(int) => Some(int.as_str()),
            TyKind::Float(float) => Some(float.as_str()),
            _ => None,
        }
    }

    /// Records the `impl` block `method` reaches on `receiver_ty`, so the
    /// lowering below calls that block's body rather than re-deriving the
    /// owner from a type that no longer names it.
    pub(super) fn record_method_owner(&mut self, call_id: NodeId, receiver_ty: Ty, method: &str) {
        let resolved = self.peel_refs(self.infer.resolve(self.tcx, receiver_ty));
        if self.infer.is_integer_constrained_var(self.tcx, resolved)
            || self.infer.is_float_literal_var(self.tcx, resolved)
        {
            self.deferred_method_owners
                .push((call_id, receiver_ty, method.to_string()));
            return;
        }
        // A receiver whose type is a parameter has one type per instantiation,
        // and the bytecode VM runs one body for all of them, so there is no
        // single block to name. Such a call is dispatched on the value in hand.
        if self.ty_mentions_generic_param(resolved) {
            return;
        }
        let Some(owner) = self.impl_owner_of(resolved) else {
            return;
        };
        if !self
            .user_method_owners
            .get(method)
            .is_some_and(|owners| owners.contains(&owner))
        {
            return;
        }
        // A type's own surface answers first: an `impl` block adds names to a
        // type rather than replacing any it already had, so a method the
        // receiver already carries is not redirected to a block that happens
        // to spell the same name. Checked last, so it costs nothing for the
        // calls that reach no user block at all.
        if self
            .tabled_method_names(resolved)
            .iter()
            .any(|name| name == method)
        {
            return;
        }
        self.table.insert_method_owner(call_id, owner);
    }

    /// Types a call to a method an `impl` block declared for a built-in
    /// receiver, when the receiver's own surface does not carry that name.
    ///
    /// Method resolution reads a tabled surface per built-in type, so without
    /// this a program's `impl Trait for String` is rejected at every call
    /// site even though the compiled tiers resolve and run it.
    pub(super) fn user_impl_method_on_builtin(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        receiver_span: Span,
    ) -> Option<Ty> {
        let mut resolved = self.peel_refs(self.infer.resolve(self.tcx, receiver_ty));
        // An unsuffixed literal takes the type whose `impl` declares the
        // method, as a binding of that type would: `17.isqrt()` reaches
        // `impl i64`. The literal's default type is preferred when several do.
        if let Some(pinned) = self.literal_receiver_impl_type(resolved, method) {
            self.unify(pinned, resolved, receiver_span);
            resolved = pinned;
        }
        let owner = self.impl_owner_of(resolved)?;
        if self.user_type_decls.contains(&owner) {
            // A type the program declares resolves its methods through its own
            // identity, which is checked against what that type owns.
            return None;
        }
        if !self
            .user_method_owners
            .get(method)
            .is_some_and(|owners| owners.contains(&owner))
        {
            return None;
        }
        if self
            .tabled_method_names(resolved)
            .iter()
            .any(|name| name == method)
        {
            return None;
        }
        // A generic `impl<T> Vec<T>` names the receiver's type arguments as
        // its own parameters, so its signature is read with those substituted.
        let substs = self.builtin_receiver_substs(resolved);
        let sig_key = (owner.clone(), method.to_string());
        let params: Vec<Ty> = match self.method_param_types.get(&sig_key) {
            Some(params) => params.clone(),
            None => self
                .generic_method_param_types
                .get(&sig_key)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|param| self.subst_generics_in_ty(param, &substs, &[]))
                .collect(),
        };
        // Each argument is checked against the parameter it fills, as a call
        // to a method on a declared type is; a closure is checked expecting
        // the callable the parameter names, so its parameters are typed.
        for (index, arg) in args.iter().enumerate() {
            let arg_ty = match params.get(index) {
                Some(param) => self.check_expr_expecting(arg, Expectation::HasType(*param)),
                None => self.check_expr(arg),
            };
            if let Some(param) = params.get(index) {
                self.check_sig_param_arg(*param, arg_ty, arg);
            }
        }
        let key = (owner, method.to_string(), args.len());
        if let Some(ret) = self.method_ret_types.get(&key).copied() {
            return Some(ret);
        }
        let ret = match self.generic_method_ret_types.get(&key).copied() {
            Some(ret) => self.subst_generics_in_ty(ret, &substs, &[]),
            None => self.fresh(),
        };
        Some(ret)
    }

    /// The type arguments a built-in receiver supplies to a generic `impl`
    /// written for its type: `Vec<T>`'s `T`, `Map<K, V>`'s `K` and `V`.
    pub(super) fn builtin_receiver_substs(&self, resolved: Ty) -> Vec<Ty> {
        match self.tcx.kind(resolved) {
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
                vec![*elem]
            }
            Some(TyKind::HashMap { key, value, .. }) => vec![*key, *value],
            _ => Vec::new(),
        }
    }

    /// The primitive an unsuffixed literal receiver `resolved` takes because
    /// an `impl` block for it declares `method`; `None` for any other
    /// receiver, or when no such block exists.
    pub(super) fn literal_receiver_impl_type(&mut self, resolved: Ty, method: &str) -> Option<Ty> {
        let owners = self.user_method_owners.get(method)?;
        let candidates: Vec<Ty> = if self.infer.is_integer_constrained_var(self.tcx, resolved) {
            [
                IntTy::I64,
                IntTy::I8,
                IntTy::I16,
                IntTy::I32,
                IntTy::Isize,
                IntTy::U8,
                IntTy::U16,
                IntTy::U32,
                IntTy::U64,
                IntTy::Usize,
            ]
            .into_iter()
            .filter(|int| owners.contains(int.as_str()))
            .map(|int| self.tcx.int_ty(int))
            .collect()
        } else if self.infer.is_float_literal_var(self.tcx, resolved) {
            [FloatTy::F64, FloatTy::F32]
                .into_iter()
                .filter(|float| owners.contains(float.as_str()))
                .map(|float| self.tcx.float_ty(float))
                .collect()
        } else {
            return None;
        };
        candidates.first().copied()
    }

    /// The name an `impl` block for `resolved` registers its methods under.
    ///
    /// A structural spelling - `[T]`, `[T; N]`, `(A, B)` - is not a path an
    /// impl header can name, so it has no owner here and its methods register
    /// by name alone.
    pub(super) fn impl_owner_of(&mut self, resolved: Ty) -> Option<String> {
        if let Some(builtin) = self.builtin_impl_owner(resolved) {
            return Some(builtin.to_string());
        }
        match self.tcx.kind(resolved)? {
            TyKind::Adt { def, .. } => {
                let def = *def;
                self.tcx.def_name(def).map(str::to_string)
            }
            // A tuple has no path to name it by, so it registers under the
            // spelling every layer shares for one.
            TyKind::Tuple(_) => {
                // The parts are what tell one structural type from another, so
                // each has to be resolved before it is named: a tuple whose
                // elements are still inference variables names them, and no
                // impl registered under that spelling.
                let settled = self.deep_resolve(resolved);
                crate::printer::structural_impl_owner(self.tcx, settled)
            }
            _ => None,
        }
    }

    /// Whether an `impl` block declared `method` for `resolved`'s type.
    pub(super) fn user_impl_declares(&mut self, resolved: Ty, method: &str) -> bool {
        self.impl_owner_of(resolved).is_some_and(|owner| {
            self.user_method_owners
                .get(method)
                .is_some_and(|owners| owners.contains(&owner))
        })
    }

    /// Method names user `impl` blocks declared for the type named `owner`.
    pub(super) fn user_methods_for_owner(&self, owner: &str) -> Vec<String> {
        self.user_method_owners
            .iter()
            .filter(|(_, owners)| owners.contains(owner))
            .map(|(method, _)| method.clone())
            .collect()
    }

    pub(super) fn known_method_names(&self, resolved: Ty) -> Vec<String> {
        // A type's own surface is what it always carried; an impl block adds
        // to it, so a diagnostic lists both. Only the nominal owners are read
        // here: rendering a structural one needs the interner mutably, and a
        // diagnostic is a listing rather than a decision.
        let mut out = self.tabled_method_names(resolved);
        let owner = self
            .builtin_impl_owner(resolved)
            .map(str::to_string)
            .or_else(|| match self.tcx.kind(resolved) {
                Some(TyKind::Adt { def, .. }) => self.tcx.def_name(*def).map(str::to_string),
                _ => None,
            });
        if let Some(owner) = owner {
            for method in self.user_methods_for_owner(&owner) {
                if !out.contains(&method) {
                    out.push(method);
                }
            }
        }
        out
    }

    /// The method surface a receiver carries on its own, before any `impl`
    /// block a program writes for it.
    pub(super) fn tabled_method_names(&self, resolved: Ty) -> Vec<String> {
        let owner = match self.tcx.kind(resolved) {
            Some(TyKind::String) => "String",
            Some(TyKind::Vec(_)) => "Vec",
            Some(TyKind::Slice(_)) => "Slice",
            Some(TyKind::Array { .. }) => "Array",
            Some(TyKind::HashMap { .. }) => "Map",
            Some(TyKind::Iterator(_) | TyKind::Range(_)) => "Iterator",
            Some(TyKind::Tuple(_)) => "Tuple",
            Some(TyKind::Adt { def, .. }) => return self.adt_method_names(*def),
            Some(TyKind::Duration) => {
                return DURATION_METHODS.iter().map(|m| (*m).to_string()).collect();
            }
            Some(TyKind::Instant) => {
                return INSTANT_METHODS.iter().map(|m| (*m).to_string()).collect();
            }
            _ => return Vec::new(),
        };
        let mut names = core_type_own_method_names(owner).unwrap_or_default();
        // A fixed array answers a copy and a conversion to the `Vec` of its
        // element; neither is part of the sequence surface it shares with a
        // view over one.
        if owner == "Array" {
            names.extend(["clone", "into"]);
        }
        let mut seen = HashSet::new();
        names
            .into_iter()
            .filter(|name| seen.insert(*name))
            .map(str::to_string)
            .collect()
    }

    /// Method names of an `Adt` receiver: the tabled surface for the
    /// checker's sentinel collections, and the user impl and trait methods
    /// for a declared type.
    pub(super) fn adt_method_names(&self, def: gossamer_resolve::DefId) -> Vec<String> {
        let tabled = match def.local {
            HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL => Some(SET_METHODS),
            VEC_DEQUE_DEF_LOCAL => Some(DEQUE_METHODS),
            VEC_QUEUE_DEF_LOCAL
            | VEC_STACK_DEF_LOCAL
            | BINARY_HEAP_DEF_LOCAL
            | MIN_HEAP_DEF_LOCAL => Some(PUSH_POP_METHODS),
            RESULT_DEF_LOCAL => Some(RESULT_METHODS),
            OPTION_DEF_LOCAL => Some(OPTION_METHODS),
            _ => None,
        };
        if let Some(names) = tabled {
            return names.iter().map(|name| (*name).to_string()).collect();
        }
        let Some(owner) = self.tcx.def_name(def) else {
            return Vec::new();
        };
        let mut methods: Vec<String> = self
            .user_method_owners
            .iter()
            .filter(|(_, owners)| owners.contains(owner))
            .map(|(method, _)| method.clone())
            .collect();
        // The methods written for this type lead the listing; the surface
        // every type carries says nothing about what the reader declared.
        methods
            .sort_by_key(|method| (AUTOMATIC_METHODS.contains(&method.as_str()), method.clone()));
        methods
    }

    /// The spelling to name in a diagnostic about `method`: what the source
    /// wrote, which differs only where a parse-time desugar renamed the call.
    pub(super) fn written_method_name(&self, method: &str) -> String {
        if self.written_method.is_empty() {
            return method.to_string();
        }
        self.written_method.clone()
    }

    /// Builds the GT0002 diagnostic for `method` on `resolved`, carrying
    /// the receiver's method surface so the reader gets a did-you-mean.
    pub(super) fn unresolved_method(&self, ty: String, method: &str, resolved: Ty) -> TypeError {
        TypeError::UnresolvedMethod {
            ty,
            name: self.written_method_name(method),
            available: self.known_method_names(resolved),
            field_of_same_name: self.adt_declares_field(resolved, method),
            free_fn_of_same_name: self.user_fn_names.contains(method),
        }
    }

    /// Whether the struct `resolved` names declares a field called
    /// `name`, which a call spelling would have missed.
    pub(super) fn adt_declares_field(&self, resolved: Ty, name: &str) -> bool {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return false;
        };
        self.struct_fields
            .get(def)
            .is_some_and(|fields| fields.iter().any(|(field, _)| field == name))
    }

    /// The diagnostic for a method call that did not resolve, given how many
    /// arguments it was written with. A name the receiver does declare failed
    /// on its argument count rather than its spelling, so it reports the count
    /// the method takes instead of claiming the method does not exist.
    pub(super) fn unresolved_method_call(
        &self,
        ty: String,
        method: &str,
        resolved: Ty,
        found_args: usize,
    ) -> TypeError {
        let available = self.known_method_names(resolved);
        // A declared method's parameters come from its signature; a built-in
        // sequence or iterator combinator has no `FnDecl`, so its count comes
        // from the same table the combinator's own typing reads (total arity,
        // receiver included).
        if available.iter().any(|name| name == method)
            && let Some(expected) = (0..=8)
                .find(|arity| {
                    self.method_arg_sigs
                        .contains_key(&(method.to_string(), *arity))
                })
                .or_else(|| {
                    Self::std_combinator_arity("iter", method).and_then(|a| a.checked_sub(1))
                })
                .filter(|expected| *expected != found_args)
        {
            return TypeError::CallArityMismatch {
                callee: method.to_string(),
                expected,
                found: found_args,
            };
        }
        TypeError::UnresolvedMethod {
            ty,
            name: self.written_method_name(method),
            available,
            field_of_same_name: self.adt_declares_field(resolved, method),
            free_fn_of_same_name: self.user_fn_names.contains(method),
        }
    }

    pub(super) fn reject_unknown_sequence_method(
        &mut self,
        resolved: Ty,
        method: &str,
        found_args: usize,
        span: Span,
    ) -> bool {
        if !matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. })
        ) {
            return false;
        }
        let ty = self.render_public_ty(resolved);
        let error = self.unresolved_method_call(ty, method, resolved, found_args);
        self.emit(error, span);
        true
    }

    pub(super) fn reject_unknown_set_method(
        &mut self,
        resolved: Ty,
        method: &str,
        found_args: usize,
        span: Span,
    ) -> bool {
        let is_hash_set = matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Adt { def, .. }) if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL)
        );
        if !is_hash_set {
            return false;
        }
        let ty = self.render_public_ty(resolved);
        let error = self.unresolved_method_call(ty, method, resolved, found_args);
        self.emit(error, span);
        true
    }

    pub(super) fn reject_unknown_deque_method(
        &mut self,
        resolved: Ty,
        method: &str,
        found_args: usize,
        span: Span,
    ) -> bool {
        let is_vec_deque = matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Adt { def, .. })
                if matches!(
                    def.local,
                    VEC_DEQUE_DEF_LOCAL | VEC_QUEUE_DEF_LOCAL | VEC_STACK_DEF_LOCAL
                )
        );
        if !is_vec_deque || method == "clone" {
            return false;
        }
        let ty = self.render_public_ty(resolved);
        let error = self.unresolved_method_call(ty, method, resolved, found_args);
        self.emit(error, span);
        true
    }

    /// Rejects a call on a built-in handle receiver whose argument count
    /// is not the one that method takes. These receivers dispatch by name
    /// to a runtime shim that reads a fixed number of slots, so an extra
    /// argument is dropped and a missing one is read as zero.
    pub(super) fn reject_handle_method_arity(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        pipe_extra: usize,
        span: Span,
    ) -> bool {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let found = args.len() + pipe_extra;
        let (owner, expected) = match self.tcx.kind(resolved) {
            Some(TyKind::Sender(_)) => (
                "Sender",
                match method {
                    "send" | "try_send" => Some(1),
                    "close" => Some(0),
                    _ => None,
                },
            ),
            Some(TyKind::Receiver(_)) => (
                "Receiver",
                match method {
                    "recv_ctx" => Some(1),
                    "recv" | "try_recv" | "close" => Some(0),
                    _ => None,
                },
            ),
            Some(TyKind::JoinHandle(_)) => ("JoinHandle", (method == "join").then_some(0)),
            Some(TyKind::Instant) => (
                "time::Instant",
                match method {
                    "elapsed_ms" | "elapsed" => Some(0),
                    "duration_since" => Some(1),
                    _ => None,
                },
            ),
            Some(TyKind::Duration) => (
                "time::Duration",
                DURATION_METHODS.contains(&method).then_some(0),
            ),
            Some(TyKind::DynError) => (
                "errors::Error",
                match method {
                    "with_field" => Some(2),
                    "is" | "field" => Some(1),
                    "message" | "cause" | "chain" | "fields" => Some(0),
                    _ => None,
                },
            ),
            Some(TyKind::JsonValue) => (
                "json::Value",
                match method {
                    "set" => Some(2),
                    "get" | "at" => Some(1),
                    "keys" | "len" | "is_null" | "as_str" | "as_i64" | "as_u64" | "as_f64"
                    | "as_bool" | "as_array" => Some(0),
                    _ => None,
                },
            ),
            _ => return false,
        };
        let Some(expected) = expected.filter(|expected| *expected != found) else {
            return false;
        };
        for arg in args {
            self.check_expr(arg);
        }
        self.emit(
            TypeError::CallArityMismatch {
                callee: format!("{owner}::{method}"),
                expected,
                found,
            },
            span,
        );
        true
    }

    pub(super) fn reject_collection_method_arity(
        &mut self,
        resolved: Ty,
        method: &str,
        found: usize,
        span: Span,
    ) -> bool {
        let expected = match self.tcx.kind(resolved) {
            Some(TyKind::HashMap { .. }) => match method {
                "insert" | "get_or" | "or_insert" => Some(2),
                "get" | "remove" | "pop" | "contains" | "contains_key" => Some(1),
                "clear" | "len" | "is_empty" | "keys" | "values" | "iter" => Some(0),
                _ => None,
            },
            // A tuple's surface is whole-value operations plus positional
            // access; nothing else reaches a tuple receiver, so every name
            // here has one arity.
            Some(TyKind::Tuple(_)) => match method {
                "get" => Some(1),
                "len" | "is_empty" | "clone" | "to_string" | "into" | "try_into" => Some(0),
                _ => None,
            },
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL) =>
            {
                match method {
                    "insert" | "remove" | "contains" => Some(1),
                    "clear" | "len" | "is_empty" | "to_vec" | "iter" => Some(0),
                    "union"
                    | "intersection"
                    | "difference"
                    | "symmetric_difference"
                    | "is_subset"
                    | "is_superset"
                    | "is_disjoint" => Some(1),
                    _ => None,
                }
            }
            Some(TyKind::Adt { def, .. }) if def.local == VEC_DEQUE_DEF_LOCAL => match method {
                "push_back" | "push_front" => Some(1),
                "pop_back" | "pop_front" | "peek_back" | "peek_front" | "len" | "is_empty"
                | "clear" => Some(0),
                _ => None,
            },
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, VEC_QUEUE_DEF_LOCAL | VEC_STACK_DEF_LOCAL) =>
            {
                match method {
                    "push" => Some(1),
                    "pop" | "peek" | "len" | "is_empty" | "clear" => Some(0),
                    _ => None,
                }
            }
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, BINARY_HEAP_DEF_LOCAL | MIN_HEAP_DEF_LOCAL) =>
            {
                match method {
                    "push" => Some(1),
                    "pop" | "peek" | "len" | "is_empty" | "clear" => Some(0),
                    _ => None,
                }
            }
            _ => None,
        };
        let Some(expected) = expected.filter(|expected| *expected != found) else {
            return false;
        };
        let owner = self.render_public_ty(resolved);
        self.emit(
            TypeError::CallArityMismatch {
                callee: format!("{owner}::{method}"),
                expected,
                found,
            },
            span,
        );
        true
    }

    /// Rejects `for x in t` where `t` is a generic parameter no bound makes
    /// iterable.
    ///
    /// A parameter iterates through `.next()`, so a bound has to guarantee
    /// that method exists. Without one the loop lowers against whatever
    /// shape each instantiation happens to have, which the compiled tier
    /// cannot resolve.
    pub(super) fn reject_unbounded_generic_iteration(&mut self, iter_ty: Ty, span: Span) {
        let mut t = self.infer.resolve(self.tcx, iter_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(t) {
            t = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Param { idx, name }) = self.tcx.kind(t) else {
            return;
        };
        let param = name.to_string();
        let bounds = self
            .current_param_bounds
            .get(idx.0 as usize)
            .cloned()
            .unwrap_or_default();
        let iterable = bounds.iter().any(|bound| {
            matches!(bound.as_str(), "Iterator" | "IntoIterator")
                || self
                    .trait_method_ret
                    .contains_key(&(bound.clone(), "next".to_string()))
        });
        if iterable {
            return;
        }
        self.emit(
            TypeError::MethodNotOnBound {
                param,
                method: "next".to_string(),
                bounds,
            },
            span,
        );
    }

    /// Rejects a method on a generic-parameter receiver that none of the
    /// parameter's bounds declares.
    ///
    /// A parameter stands for every type a caller may supply, so its bounds
    /// are the whole of what it can do. Without this the call fell through
    /// to a name-global lookup and bound an unrelated type's body, reading
    /// the receiver at that type's field layout.
    pub(super) fn reject_method_off_bound(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> bool {
        // Every value answers these regardless of its bounds.
        if AUTOMATIC_METHODS.contains(&method) {
            return false;
        }
        let mut t = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(t) {
            t = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Param { idx, name }) = self.tcx.kind(t) else {
            return false;
        };
        let param = name.to_string();
        let bounds = self
            .current_param_bounds
            .get(idx.0 as usize)
            .cloned()
            .unwrap_or_default();
        // A bound whose method surface is unknown cannot say whether this
        // call is valid, so it is left alone rather than guessed at.
        if bounds.iter().any(|bound| {
            !self.declared_trait_names.contains(bound) && builtin_trait_methods(bound).is_none()
        }) {
            return false;
        }
        // A built-in bound answers for the methods it licenses.
        if bounds
            .iter()
            .filter_map(|bound| builtin_trait_methods(bound))
            .any(|surface| surface.contains(&method))
        {
            return false;
        }
        for arg in args {
            self.check_expr(arg);
        }
        self.emit(
            TypeError::MethodNotOnBound {
                param,
                method: method.to_string(),
                bounds,
            },
            span,
        );
        true
    }

    pub(super) fn check_param_receiver_method(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        // A method on a bound type-parameter receiver (`s.area()` where
        // `s: &T`, `T: Shape`) resolves to the trait method's declared
        // return type, so a `String`-returning trait method is not left to
        // default to i64 and render its pointer bits on the compiled tiers.
        let (ret, params) = self.param_method_sig(receiver_ty, method, span)?;
        let arg_tys: Vec<Ty> = args.iter().map(|arg| self.check_expr(arg)).collect();
        if params.len() == args.len() {
            for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
                self.check_sig_param_arg(*param, *arg_ty, arg);
            }
        } else {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: method.to_string(),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
        }
        Some(ret)
    }

    pub(super) fn check_string_method_args_if_needed(
        &mut self,
        call_id: NodeId,
        method: &str,
        span: Span,
        args: &[Expr],
        arg_tys: &[Ty],
    ) {
        // `s.contains(x)` dispatches to the same `strings::` shim as
        // the free function with the receiver as the implicit first
        // argument; validate the explicit args so an integer in a
        // string slot is rejected here too. Skipped under `|>`,
        // which appends the piped value as a trailing argument.
        if !self.pipe_stage_callees.contains(&call_id) {
            self.check_strings_method_call_args(method, args, arg_tys, span);
        }
    }

    pub(super) fn method_receiver_expectation(
        &mut self,
        method: &str,
        receiver: &Expr,
        expected: Expectation,
    ) -> Expectation {
        if !Self::is_string_parse_expr(receiver) {
            return Expectation::None;
        }
        match method {
            "map" | "map_err" | "and_then" | "or_else" => {
                if let Some((ok, _)) = self.result_payload_expectation(expected) {
                    let err = self.fresh();
                    return Expectation::HasType(self.result_adt_ty(ok, err));
                }
            }
            "ok" => {
                if let Some(payload) = self.option_payload_expectation(expected) {
                    let err = self.fresh();
                    return Expectation::HasType(self.result_adt_ty(payload, err));
                }
            }
            "unwrap_or" | "unwrap_or_else" => {
                if let Some(payload) = self.non_result_expectation_target(expected) {
                    let err = self.fresh();
                    return Expectation::HasType(self.result_adt_ty(payload, err));
                }
            }
            _ => {}
        }
        Expectation::None
    }

    pub(super) fn is_string_parse_expr(expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::MethodCall { name, .. } => name.name == "parse",
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Path(path) => path
                    .segments
                    .last()
                    .is_some_and(|segment| segment.name.name == "parse"),
                _ => false,
            },
            _ => false,
        }
    }

    /// The receiver type a method on `seq[a..b]` sees. A mutating method acts
    /// on the window `[T]` the range names, so the sequence itself changes and
    /// a resizing method is rejected; any other method reads the copy.
    pub(super) fn window_receiver_ty(
        &mut self,
        receiver: &Expr,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
    ) -> Ty {
        if !crate::is_mutating_method_name(method) {
            return receiver_ty;
        }
        let Some(elem) = self.mutable_window_elem(receiver) else {
            return receiver_ty;
        };
        if let Some(root) = Self::place_root_name(receiver) {
            let roots = HashSet::from([root]);
            for arg in args {
                if let Some((root, borrower)) = self.closure_alias_of(arg, &roots) {
                    self.emit(
                        TypeError::MutableReferenceConflict { root, borrower },
                        arg.span,
                    );
                }
            }
        }
        self.tcx.intern(TyKind::Slice(elem))
    }

    /// `seq.par_chunks_mut(size, f)` writes `seq` from many workers at once:
    /// the receiver must be writable, and no argument may reach it otherwise.
    fn check_chunk_receiver(&mut self, receiver: &Expr, receiver_ty: Ty, args: &[Expr]) {
        self.check_mutating_method_receiver(receiver, receiver_ty, "par_chunks_mut");
        let Some(root) = Self::place_root_name(receiver) else {
            return;
        };
        let roots = HashSet::from([root]);
        for arg in args {
            if let Some((root, borrower)) = self.closure_alias_of(arg, &roots) {
                self.emit(
                    TypeError::MutableReferenceConflict { root, borrower },
                    arg.span,
                );
            }
        }
    }

    /// Enforces writable receivers for user `&mut self` methods and built-in
    /// methods whose execution path writes a replacement value back into the
    /// receiver place.
    pub(super) fn check_mutating_method_receiver(
        &mut self,
        receiver: &Expr,
        receiver_ty: Ty,
        method: &str,
    ) {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if matches!(self.tcx.kind(resolved), Some(TyKind::Var(_))) {
            self.deferred_mutating_receivers
                .push(DeferredMutatingReceiver {
                    ty: receiver_ty,
                    method: method.to_string(),
                    place: self.auto_deref_place_mutability(receiver),
                    name: Self::place_root_name(receiver).unwrap_or_else(|| "value".to_string()),
                    span: receiver.span,
                });
            return;
        }
        if !self.method_requires_mut_receiver(receiver_ty, method) {
            return;
        }
        self.check_mutating_receiver_place(receiver);
    }

    pub(super) fn reject_non_vec_resizing_method(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> bool {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if !matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Array { .. } | TyKind::Slice(_))
        ) || !is_vec_only_sequence_method(method)
        {
            return false;
        }
        for arg in args {
            self.check_expr(arg);
        }
        let ty = self.render_public_ty(resolved);
        self.emit(
            TypeError::SequenceResizeRequiresVec {
                ty,
                method: method.to_string(),
            },
            span,
        );
        true
    }

    /// Rejects a call a built-in receiver cannot take: a method its
    /// surface does not carry, or one it carries at a different arity.
    pub(super) fn reject_invalid_builtin_receiver_call(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        call_id: NodeId,
        span: Span,
    ) -> bool {
        let pipe_extra = usize::from(self.pipe_stage_callees.contains(&call_id));
        self.reject_non_vec_resizing_method(receiver_ty, method, args, span)
            || self.reject_unavailable_non_vec_sequence_method(receiver_ty, method, args, span)
            || self.reject_handle_method_arity(receiver_ty, method, args, pipe_extra, span)
    }

    pub(super) fn reject_unavailable_non_vec_sequence_method(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> bool {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        // A resizing method on a fixed-size sequence gets the more specific
        // `SequenceResizeRequiresVec` diagnostic from the sibling check, so
        // it is left alone here. An iterator has no buffer to resize, so
        // its surface is decided by the combinator list alone.
        // Every value `{}` renders answers `to_string`, whatever other surface
        // its receiver declares. A lazy cursor is not a value, so it keeps the
        // rejection its own surface gives it.
        if method == "to_string" && args.is_empty() && self.is_displayable_value(resolved) {
            return false;
        }
        // A method an `impl` block declared for this receiver is part of its
        // surface, so the tabled list is not the whole answer.
        if self.user_impl_declares(resolved, method) {
            return false;
        }
        let (available, resize_reported_separately) = match self.tcx.kind(resolved) {
            Some(TyKind::Array { .. }) => (is_array_sequence_method(method), true),
            Some(TyKind::Slice(_)) => (is_slice_sequence_method(method), true),
            // A tuple is not iterable: its elements may differ in type, so
            // there is no element type to hand a loop or a combinator.
            // Positional access (`t.0`, `t.get(i)`) stays available.
            Some(TyKind::Tuple(_)) => (!is_tuple_rejected_method(method), false),
            // Iterator state addresses elements through the combinator
            // surface; a buffer method has no length or storage to act on
            // and would read as a silent no-op.
            Some(TyKind::Iterator(_) | TyKind::Range(_)) => {
                (iterator_receiver_accepts_method(method), false)
            }
            // A map is keyed, not ordered by position: the sequence surface
            // has nothing to index, reorder, or slice on it.
            Some(TyKind::HashMap { ordered: true, .. }) => (is_btree_map_method(method), false),
            Some(TyKind::HashMap { .. }) => (is_map_method(method), false),
            _ => return false,
        };
        if available || (resize_reported_separately && is_vec_only_sequence_method(method)) {
            return false;
        }
        for arg in args {
            self.check_expr(arg);
        }
        let ty = self.render_public_ty(resolved);
        let error = self.unresolved_method_call(ty, method, resolved, args.len());
        self.emit(error, span);
        true
    }

    pub(super) fn render_public_ty(&mut self, ty: Ty) -> String {
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved).cloned() {
            Some(TyKind::Bool) => "bool".to_string(),
            Some(TyKind::Char) => "char".to_string(),
            Some(TyKind::String) => "String".to_string(),
            Some(TyKind::Int(int)) => int.as_str().to_string(),
            Some(TyKind::Float(float)) => float.as_str().to_string(),
            Some(TyKind::Unit) => "()".to_string(),
            Some(TyKind::Never) => "!".to_string(),
            Some(TyKind::DynValue) => "DynValue".to_string(),
            Some(TyKind::Array { elem, len }) => {
                format!(
                    "[{}; {}]",
                    self.render_public_ty(elem),
                    render_array_len(len)
                )
            }
            Some(TyKind::Simd { elem, lanes }) => self.render_simd_ty(elem, lanes),
            Some(TyKind::Slice(elem)) => {
                format!("[{}]", self.render_public_ty(elem))
            }
            Some(TyKind::Vec(elem)) => {
                format!("Vec<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::Iterator(elem)) => {
                format!("Iterator<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::Range(elem)) => {
                format!("Range<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::HashMap {
                key,
                value,
                ordered,
            }) => {
                format!(
                    "{}<{}, {}>",
                    if ordered { "BTreeMap" } else { "Map" },
                    self.render_public_ty(key),
                    self.render_public_ty(value)
                )
            }
            Some(TyKind::Sender(elem)) => {
                format!("Sender<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::Receiver(elem)) => {
                format!("Receiver<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::JoinHandle(elem)) => {
                format!("JoinHandle<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::Tuple(parts)) => {
                let rendered = parts
                    .iter()
                    .map(|part| self.render_public_ty(*part))
                    .collect::<Vec<_>>();
                if rendered.len() == 1 {
                    format!("({},)", rendered[0])
                } else {
                    format!("({})", rendered.join(", "))
                }
            }
            Some(TyKind::Ref { mutability, inner }) => {
                format!("{}{}", mutability.prefix(), self.render_public_ty(inner))
            }
            Some(TyKind::FnPtr(sig)) => self.render_public_fn_sig("fn", &sig),
            Some(TyKind::FnTrait(sig)) => self.render_public_fn_sig("Fn", &sig),
            Some(TyKind::FnDef { def, substs }) => {
                self.render_public_def("fn", def.local, substs.as_slice())
            }
            Some(TyKind::Closure { def, .. }) => format!("<closure #{}>", def.local),
            Some(TyKind::Adt { def, substs }) => {
                self.render_public_def("adt", def.local, substs.as_slice())
            }
            Some(TyKind::Alias { def, substs }) => {
                self.render_public_def("alias", def.local, substs.as_slice())
            }
            Some(TyKind::Nominal { def, .. }) => self.render_public_def("alias", def.local, &[]),
            Some(TyKind::Dyn(trait_ref)) => {
                self.render_public_def("trait", trait_ref.def.local, trait_ref.substs.as_slice())
            }
            Some(TyKind::Duration) => "time::Duration".to_string(),
            Some(TyKind::Instant) => "time::Instant".to_string(),
            Some(TyKind::JsonValue) => "json::Value".to_string(),
            Some(TyKind::DynError) => "errors::Error".to_string(),
            Some(TyKind::Var(vid)) if self.infer.is_unresolved_integer_var(vid) => {
                "i64".to_string()
            }
            Some(TyKind::Var(vid)) if self.infer.is_unresolved_float_var(vid) => "f64".to_string(),
            Some(TyKind::Var(_)) => "_".to_string(),
            Some(TyKind::Param { name, .. }) => name.to_string(),
            Some(TyKind::Error) => "<error>".to_string(),
            None => format!("<ty:{}>", resolved.as_u32()),
        }
    }

    pub(super) fn render_public_fn_sig(&mut self, prefix: &str, sig: &FnSig) -> String {
        let inputs = sig
            .inputs
            .iter()
            .map(|ty| self.render_public_ty(*ty))
            .collect::<Vec<_>>()
            .join(", ");
        let output = self.infer.resolve(self.tcx, sig.output);
        if matches!(self.tcx.kind(output), Some(TyKind::Unit)) {
            format!("{prefix}({inputs})")
        } else {
            format!("{prefix}({inputs}) -> {}", self.render_public_ty(output))
        }
    }

    pub(super) fn render_public_def(
        &mut self,
        fallback: &str,
        local: u32,
        substs: &[crate::GenericArg],
    ) -> String {
        let mut out = self
            .tcx
            .def_name(gossamer_resolve::DefId::local(local))
            .map_or_else(
                || format!("{fallback}#{local}"),
                |name| crate::printer::public_type_name(name).into_owned(),
            );
        if !substs.is_empty() {
            let args = substs
                .iter()
                .map(|arg| match arg {
                    crate::GenericArg::Type(ty) => self.render_public_ty(*ty),
                    crate::GenericArg::Const(value) => value.to_string(),
                    crate::GenericArg::ConstParam(idx) => self
                        .const_generic_param_name(*idx)
                        .unwrap_or_else(|| format!("N{}", idx.0)),
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.push('<');
            out.push_str(&args);
            out.push('>');
        }
        out
    }

    /// Qualified user-method calls (`Type::method(receiver, ...)`) and the
    /// qualified map/set mutation surface do not pass through
    /// `check_method_call`, so enforce the same receiver capability here.
    pub(super) fn check_mutating_qualified_call(&mut self, callee: &Expr, args: &[Expr]) {
        let ExprKind::Path(path) = &callee.kind else {
            return;
        };
        let segments = &path.segments;
        if segments.len() < 2 {
            return;
        }
        let owner = segments[segments.len() - 2].name.name.as_str();
        let method = segments[segments.len() - 1].name.name.as_str();
        let key = (owner.to_string(), method.to_string());
        let user_requirement = self
            .inherent_method_requires_mut
            .get(&key)
            .or_else(|| self.trait_impl_method_requires_mut.get(&key))
            .copied();
        let requires_mut = user_requirement.unwrap_or_else(|| {
            if self.user_type_decls.contains(owner)
                || matches!(
                    self.resolutions.get(callee.id),
                    Some(Resolution::Def { .. })
                )
            {
                return false;
            }
            matches!(owner, "Map" | "Set" | "BTreeSet") && crate::is_mutating_method_name(method)
        });
        if requires_mut && let Some(receiver) = args.first() {
            if user_requirement == Some(true) {
                match self.expr_ref_mutbl(receiver) {
                    Some(Mutbl::Mut) => {}
                    Some(Mutbl::Not) => self.check_mutating_receiver_place(receiver),
                    None => self.emit(
                        TypeError::MutableArgumentRequiresReference {
                            argument: Self::place_display(receiver),
                        },
                        receiver.span,
                    ),
                }
            } else {
                self.check_mutating_receiver_place(receiver);
            }
        }
    }

    pub(super) fn check_mutating_receiver_place(&mut self, receiver: &Expr) {
        let name = Self::written_place_name(receiver);
        self.emit_mutating_place_error(
            self.auto_deref_place_mutability(receiver),
            name,
            receiver.span,
        );
    }

    pub(super) fn emit_mutating_place_error(&mut self, place: PlaceMut, name: String, span: Span) {
        match place {
            PlaceMut::ImmutableBinding => {
                self.emit(TypeError::AssignToImmutable { name }, span);
            }
            PlaceMut::SharedReference => {
                self.emit(TypeError::AssignThroughSharedReference { name }, span);
            }
            PlaceMut::NotAReference => {
                self.emit(TypeError::DerefWriteToNonReference { name }, span);
            }
            PlaceMut::Writable | PlaceMut::Unknown => {}
        }
    }

    pub(super) fn receiver_method_owner_name(&self, resolved: Ty) -> Option<String> {
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, .. }) => self.tcx.def_name(*def).map(str::to_string),
            Some(
                TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Vec(_)
                | TyKind::Iterator(_)
                | TyKind::Range(_)
                | TyKind::HashMap { .. }
                | TyKind::Sender(_)
                | TyKind::Receiver(_)
                | TyKind::JoinHandle(_)
                | TyKind::Duration
                | TyKind::Instant
                | TyKind::JsonValue
                | TyKind::DynValue
                | TyKind::DynError,
            ) => {
                let rendered = render_ty(self.tcx, resolved);
                let bare = rendered.split('<').next().unwrap_or(&rendered);
                bare.rsplit("::").next().map(str::to_string)
            }
            _ => None,
        }
    }

    pub(super) fn method_requires_mut_receiver(&mut self, receiver_ty: Ty, method: &str) -> bool {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::Param { idx, .. }) = self.tcx.kind(resolved) {
            return self
                .current_param_bounds
                .get(idx.0 as usize)
                .is_some_and(|bounds| {
                    bounds.iter().any(|bound| {
                        self.trait_method_requires_mut
                            .get(&(bound.clone(), method.to_string()))
                            .copied()
                            .unwrap_or(false)
                    })
                });
        }
        if let Some(owner) = self.receiver_method_owner_name(resolved) {
            let key = (owner.clone(), method.to_string());
            // A user method named `push` or `remove` must follow its declared
            // receiver, not inherit the built-in writeback policy by name.
            if self.user_type_decls.contains(&owner) {
                return self
                    .inherent_method_requires_mut
                    .get(&key)
                    .or_else(|| self.trait_impl_method_requires_mut.get(&key))
                    .copied()
                    .unwrap_or(false);
            }
            // Counter-like `inc` methods use interior mutability. The
            // write-back variants are specific to HashMap receivers.
            if matches!(method, "inc" | "inc_at" | "inc_batch") {
                return matches!(owner.as_str(), "Map");
            }
            if let Some(requires_mut) = self
                .inherent_method_requires_mut
                .get(&key)
                .or_else(|| self.trait_impl_method_requires_mut.get(&key))
            {
                return *requires_mut;
            }
        }
        if matches!(method, "inc" | "inc_at" | "inc_batch") {
            return false;
        }
        crate::is_mutating_method_name(method)
    }

    pub(super) fn user_method_params_for(
        &mut self,
        receiver_ty: Ty,
        method: &str,
    ) -> Option<Vec<Ty>> {
        let TyKind::Adt { def, substs } = self.tcx.kind(receiver_ty)?.clone() else {
            return None;
        };
        let name = self.tcx.def_name(def)?.to_string();
        let key = (name, method.to_string());
        if let Some(params) = self.method_param_types.get(&key) {
            return Some(params.clone());
        }
        let params = self.generic_method_param_types.get(&key)?.clone();
        let (subst_tys, subst_consts) = self.adt_subst_vectors(&substs);
        Some(
            params
                .into_iter()
                .map(|param| self.subst_generics_in_ty(param, &subst_tys, &subst_consts))
                .collect(),
        )
    }

    /// The return of a call to a method whose own type parameters reach its
    /// return, with those parameters instantiated from this call's arguments.
    /// `None` when `resolved` declares no such method.
    pub(super) fn own_generic_method_ret(
        &mut self,
        resolved: Ty,
        method: &str,
        (args, arg_tys): (&[Expr], &[Ty]),
        (arity, span): (usize, Span),
        explicit: &[Ty],
    ) -> Option<Ty> {
        let (owner, sig) = match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs }) if substs.types().is_empty() => {
                self.tcx.def_name(*def).and_then(|name| {
                    self.own_generic_method_sigs
                        .get(&(name.to_string(), method.to_string(), arity))
                        .cloned()
                        .map(|sig| (name.to_string(), sig))
                })
            }
            _ => None,
        }?;
        Some(self.instantiate_own_generic_call(
            &sig,
            (&owner, method, span),
            (args, arg_tys),
            explicit,
        ))
    }

    /// The types a turbofish (`::<A, B>`) names, in order; its const
    /// arguments are read elsewhere.
    pub(super) fn turbofish_types(&mut self, generics: &[AstGenericArg]) -> Vec<Ty> {
        generics
            .iter()
            .filter_map(|arg| match arg {
                AstGenericArg::Type(ty) => Some(self.type_from_ast(ty)),
                AstGenericArg::Const(_) => None,
            })
            .collect()
    }

    /// Checks one call to a method whose own type parameters each take the
    /// type the call's turbofish names in their position, or a fresh
    /// variable, and answers its return with them substituted. `args` are
    /// the declared parameters' arguments, without a receiver.
    pub(super) fn instantiate_own_generic_call(
        &mut self,
        sig: &OwnGenericMethodSig,
        (owner, method, span): (&str, &str, Span),
        (args, arg_tys): (&[Expr], &[Ty]),
        explicit: &[Ty],
    ) -> Ty {
        let vars: Vec<Ty> = (0..sig.generics)
            .map(|position| {
                explicit
                    .get(position)
                    .copied()
                    .unwrap_or_else(|| self.fresh())
            })
            .collect();
        let params: Vec<Ty> = sig
            .params
            .iter()
            .map(|param| self.subst_params_in_ty(*param, &vars))
            .collect();
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        if let Some(constraints) = self
            .method_assoc_constraints
            .get(&(owner.to_string(), method.to_string()))
            .cloned()
        {
            self.apply_assoc_constraints(&constraints, &vars, span);
        }
        self.subst_params_in_ty(sig.ret, &vars)
    }

    /// Checks a user method call's arguments against the method's parameters
    /// and answers the type each of the method's own type parameters stands
    /// for at this call, by position (empty when it declares none).
    pub(super) fn check_user_method_args(
        &mut self,
        receiver_ty: Ty,
        (method, span): (&str, Span),
        args: &[Expr],
        arg_tys: &[Ty],
        explicit: &[Ty],
    ) -> Vec<Ty> {
        let Some(params) = self.user_method_params_for(receiver_ty, method) else {
            return Vec::new();
        };
        // The receiver's type arguments are already in `params`; what is left
        // is the method's own, which this call's turbofish names or its
        // arguments instantiate. The method's own take the positions after
        // the impl block's.
        let impl_slots = match self
            .tcx
            .kind(self.infer.resolve(self.tcx, receiver_ty))
            .cloned()
        {
            Some(TyKind::Adt { substs, .. }) => self.adt_subst_vectors(&substs).0.len(),
            _ => 0,
        };
        let slots = params
            .iter()
            .map(|param| self.param_slots(*param))
            .max()
            .unwrap_or(0)
            .max(impl_slots + explicit.len());
        let mut bound = vec![None; slots];
        for (position, ty) in explicit.iter().enumerate() {
            bound[impl_slots + position] = Some(*ty);
        }
        for (param, arg_ty) in params.iter().zip(arg_tys) {
            self.bind_type_params(*param, *arg_ty, &mut bound);
        }
        let mut method_substs: Vec<Ty> = bound
            .into_iter()
            .map(|ty| ty.unwrap_or_else(|| self.fresh()))
            .collect();
        self.apply_method_assoc_constraints(receiver_ty, (method, span), &mut method_substs);
        // Arity has its own receiver-aware diagnostic below. Validate every
        // explicit leading argument here; a pipeline supplies the final slot
        // later in `pipe_result_ty`.
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            let param = self.subst_params_in_ty(*param, &method_substs);
            self.check_sig_param_arg(param, *arg_ty, arg);
        }
        method_substs
    }

    /// Applies a user method's `Name = Type` constraints to one call: the
    /// receiver's type arguments fill the `impl` block's slots and
    /// `method_substs` the method's own, which a constraint may decide.
    pub(super) fn apply_method_assoc_constraints(
        &mut self,
        receiver_ty: Ty,
        (method, span): (&str, Span),
        method_substs: &mut Vec<Ty>,
    ) {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved).cloned() else {
            return;
        };
        let Some(name) = self.tcx.def_name(def).map(str::to_string) else {
            return;
        };
        let Some(constraints) = self
            .method_assoc_constraints
            .get(&(name, method.to_string()))
            .cloned()
        else {
            return;
        };
        let (receiver_tys, _) = self.adt_subst_vectors(&substs);
        let slots = constraints
            .iter()
            .map(|(position, _, target)| (*position + 1).max(self.param_slots(*target)))
            .max()
            .unwrap_or(0);
        while method_substs.len() < slots {
            method_substs.push(self.fresh());
        }
        let mut vars = method_substs.clone();
        for (slot, ty) in vars.iter_mut().zip(receiver_tys) {
            *slot = ty;
        }
        self.apply_assoc_constraints(&constraints, &vars, span);
    }

    /// Position of the `http::Handler` argument a stdlib handle method takes,
    /// so a closure written there is typed as the handler it stands for.
    pub(super) fn stdlib_handler_arg_slot(
        &self,
        receiver_ty: Ty,
        method: &str,
        n_args: usize,
    ) -> Option<usize> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        match (self.tcx.def_name(*def)?, method, n_args) {
            ("http::Server", "serve", 1) => Some(0),
            (
                "http::Router",
                "get" | "post" | "put" | "delete" | "patch" | "head" | "options",
                2,
            ) => Some(1),
            _ => None,
        }
    }

    /// Types a method call's explicit arguments, shaping each by the
    /// method's declared parameter. A closure argument to a Vec/slice
    /// combinator (`xs.sort_by`, `xs.map`) is pinned to the element type
    /// so a field access in its body resolves to the struct projection
    /// rather than the dynamic JSON path; a container-literal argument is
    /// coerced (never unified) toward the sole unambiguous candidate.
    pub(super) fn check_method_call_arg_tys(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
    ) -> Vec<Ty> {
        let candidates = self
            .method_arg_sigs
            .get(&(method.to_string(), args.len()))
            .cloned()
            .unwrap_or_default();
        let closure_combinator_inputs = self.vec_combinator_closure_inputs(method, receiver_ty);
        let is_closure = |arg: &Expr| matches!(arg.kind, ExprKind::Closure { .. });
        // A closure handed to a user method takes its parameter types from the
        // method's signature, once the other arguments have said what the
        // method's own type parameters stand for.
        let user_params = if closure_combinator_inputs.is_none() && args.iter().any(is_closure) {
            self.user_method_param_templates(receiver_ty, method, args.len())
        } else {
            None
        };
        let error_ty = self.tcx.error_ty();
        let mut arg_tys: Vec<Ty> = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            if user_params.is_some() && is_closure(arg) {
                arg_tys.push(error_ty);
                continue;
            }
            // A fold's callback reads the accumulator its seed argument names
            // and the receiver's element, and a reduce's reads two elements, so
            // both are its parameter types before its body is checked.
            let accumulator_inputs = match (method, i, &arg.kind) {
                ("fold" | "scan", 1, ExprKind::Closure { params, .. }) if params.len() == 2 => self
                    .vec_combinator_closure_inputs("map", receiver_ty)
                    .zip(arg_tys.first().copied())
                    .map(|(elem, seed)| vec![seed, elem[0]]),
                ("reduce", 0, ExprKind::Closure { params, .. }) if params.len() == 2 => self
                    .vec_combinator_closure_inputs("map", receiver_ty)
                    .map(|elem| vec![elem[0], elem[0]]),
                _ => None,
            };
            let handler_slot = is_closure(arg)
                && self.stdlib_handler_arg_slot(receiver_ty, method, args.len()) == Some(i);
            let exp = match (
                accumulator_inputs
                    .as_ref()
                    .or(closure_combinator_inputs.as_ref()),
                &arg.kind,
            ) {
                _ if handler_slot => Expectation::Coerce(self.http_handler_ty()),
                (Some(inputs), ExprKind::Closure { params, .. })
                    if params.len() == inputs.len() =>
                {
                    let output = self.fresh();
                    let sig = FnSig {
                        inputs: inputs.clone(),
                        output,
                    };
                    Expectation::HasType(self.tcx.intern(TyKind::FnPtr(sig)))
                }
                _ => match self.unique_container_expectation(&candidates, i) {
                    // A fixed-array literal never stands in for a `Vec`
                    // parameter: the spellings name different containers, so
                    // the mismatch is reported here rather than coerced into
                    // an argument the callee then treats as a Vec.
                    Some(want)
                        if matches!(&arg.kind, ExprKind::FixedArray(_))
                            && matches!(
                                self.tcx.kind(self.infer.resolve(self.tcx, want)),
                                Some(TyKind::Vec(_))
                            ) =>
                    {
                        Expectation::HasType(want)
                    }
                    Some(want) => Expectation::Coerce(want),
                    None => Expectation::None,
                },
            };
            arg_tys.push(self.check_expr_expecting(arg, exp));
        }
        if let Some(templates) = user_params {
            let slots = templates
                .iter()
                .map(|t| self.param_slots(*t))
                .max()
                .unwrap_or(0);
            self.check_closures_against_params(&templates, vec![None; slots], args, &mut arg_tys);
        }
        arg_tys
    }

    /// Rejects a method call whose argument count does not match the
    /// receiver method's declared arity. A call on the right of `|>`
    /// receives the piped value as an implicit trailing argument, so it
    /// is counted toward the supplied arity. Mirrors the free-call
    /// GT0018 check, which method calls never reached.
    pub(super) fn check_method_arity(
        &mut self,
        call_id: NodeId,
        resolved: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return;
        };
        let Some(name) = self.tcx.def_name(*def).map(str::to_string) else {
            return;
        };
        let Some(&expected) = self.method_arities.get(&(name.clone(), method.to_string())) else {
            return;
        };
        let pipe_extra = usize::from(self.pipe_stage_callees.contains(&call_id));
        let effective = args.len() + pipe_extra;
        if effective != expected {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{name}::{method}"),
                    expected,
                    found: effective,
                },
                span,
            );
        }
    }

    /// Return type of a method on a `HashSet` receiver (sentinel `Adt`,
    /// def `u32::MAX - 7`). Without this the set-algebra methods are left
    /// a fresh `Var`, so iterating their result (`for e in a.union(&b)`)
    /// could not recover the set kind and read the handle as a vec.
    pub(super) fn set_method_ret(
        &mut self,
        method: &str,
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let (_owner, elem) = self.set_elem_ty(resolved)?;
        // The value a set is asked about is one of its elements, so a set
        // built empty (`Set::new()`) learns its element type from the first
        // such call, and a value of another type is a mismatch. The queried
        // value is read through any borrow: `s.contains(&k)` asks about `k`.
        if matches!(method, "insert" | "remove" | "contains")
            && let [value] = arg_tys
        {
            let value = self.peel_refs(*value);
            self.unify(elem, value, span);
        }
        match method {
            // New sets - same element type as the receiver.
            "union" | "intersection" | "difference" | "symmetric_difference" => Some(resolved),
            // `to_vec` snapshots into a Vec; `iter` starts a pipeline, and
            // answers with an iterator the way every other sequence does.
            "to_vec" => Some(self.tcx.intern(TyKind::Vec(elem))),
            "iter" => Some(self.tcx.intern(TyKind::Iterator(elem))),
            "insert" | "remove" | "contains" | "is_empty" | "is_subset" | "is_superset"
            | "is_disjoint" => Some(self.tcx.bool_ty()),
            "len" => Some(self.tcx.int_ty(IntTy::I64)),
            "clear" => Some(self.tcx.unit()),
            _ => None,
        }
    }

    /// Type of `xs.count(pred)` - the accepted-element count - pinning the
    /// predicate to one that takes an element and answers a bool. Every
    /// receiver that traverses reaches this, so the predicate's parameter is
    /// the element type wherever the call is written; a parameter left
    /// unresolved reaches codegen with no type to project a field against.
    pub(super) fn pred_count_ty(&mut self, pred_ty: Ty, elem: Ty, span: Span) -> Ty {
        let out = self.callable_output(pred_ty, &[elem], span);
        let bool_ty = self.tcx.bool_ty();
        self.unify(bool_ty, out, span);
        self.tcx.int_ty(IntTy::I64)
    }

    /// Return type of an `iter::` combinator called in method form on a
    /// sequence receiver (`xs.map(f)`, `xs.filter(f)`, `xs.sum()`, …):
    /// the same typing as the data-last free form, with the receiver as
    /// the data argument. `None` for non-sequence receivers and
    /// non-combinator names, so `Result::map` / `Option::map` / the
    /// String surface keep their own dispatch.
    pub(super) fn seq_combinator_method_ret(
        &mut self,
        method: &str,
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        // A map holds its pairs, so a traversal on one answers eagerly with a
        // sequence, the way one on a Vec does. A free-call-only traversal is
        // declined here for the same reason `Vec` declines it: no receiver
        // form exists to reach.
        if COLLECTION_TRAVERSAL_METHODS.contains(&method) && !is_free_call_only_traversal(method) {
            // A map's element is its key/value pair. A set has no order for a
            // traversal to read its elements in, so a set answers these
            // through the iterator `iter()` gives, not on the collection.
            let elem = match self.tcx.kind(resolved) {
                Some(TyKind::HashMap { key, value, .. }) => {
                    let (key, value) = (*key, *value);
                    Some(self.tcx.intern(TyKind::Tuple(vec![key, value])))
                }
                _ => None,
            };
            if let Some(elem) = elem {
                // The predicate form of `count` has no data-last free
                // spelling for `std_combinator_ty` to answer from, so it is
                // typed here the way a sequence receiver types it.
                if method == "count" && arg_tys.len() == 1 {
                    return Some(self.pred_count_ty(arg_tys[0], elem, span));
                }
                let seq = self.tcx.intern(TyKind::Vec(elem));
                return self.std_combinator_ty_at(
                    "iter",
                    method,
                    arg_tys,
                    seq,
                    DataPosition::Receiver,
                    span,
                );
            }
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Iterator(_) | TyKind::Range(_)) => {
                // The receiver is the combinator's data argument, so the
                // declared arity leaves one slot for the explicit
                // arguments. A count the surface does not declare is
                // reported here: dispatch past this point has no iterator
                // entry to reach, so it would answer an unconstrained
                // variable and the call would run as a silent no-op.
                let accepted = Self::iterator_method_arities(method)?;
                if !accepted.contains(&arg_tys.len()) {
                    self.emit(
                        TypeError::CallArityMismatch {
                            callee: method.to_string(),
                            expected: accepted[0],
                            found: arg_tys.len(),
                        },
                        span,
                    );
                    return Some(self.tcx.error_ty());
                }
                if method == "next" {
                    let elem = self.sequence_elem_ty(resolved, span)?;
                    return Some(self.option_adt_ty(elem));
                }
                // The predicate form of `count` has no data-last free
                // spelling, so it is typed here: the predicate answers a
                // bool for an element and the count is an integer.
                if method == "count" && arg_tys.len() == 1 {
                    let elem = self.sequence_elem_ty(resolved, span)?;
                    return Some(self.pred_count_ty(arg_tys[0], elem, span));
                }
                return self.std_combinator_ty_at(
                    "iter",
                    method,
                    arg_tys,
                    resolved,
                    DataPosition::Receiver,
                    span,
                );
            }
            // A collection already holds its values, so traversing it answers
            // eagerly with a materialised result. `iter()` is how a caller
            // asks for the lazy walk that never holds the whole sequence.
            Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }) => {}
            _ => return None,
        }
        match (method, arg_tys.len()) {
            ("sum" | "product", 0) => self.sequence_elem_ty(resolved, span),
            ("min" | "max", 0) => {
                let elem = self.sequence_elem_ty(resolved, span)?;
                Some(self.option_adt_ty(elem))
            }
            ("count", 0) => Some(self.tcx.int_ty(IntTy::I64)),
            // `xs.count(f)`: the accepted-element count - the predicate
            // takes an element and yields bool.
            ("count", 1) => {
                let elem = self.sequence_elem_ty(resolved, span)?;
                Some(self.pred_count_ty(arg_tys[0], elem, span))
            }
            (
                m @ ("map" | "filter" | "for_each" | "any" | "all" | "find" | "position"
                | "max_by_key" | "min_by_key" | "take_while" | "skip_while" | "skip" | "chain"
                | "zip" | "windows" | "chunks" | "filter_map" | "find_map" | "flat_map"
                | "chunk_by" | "count_by" | "partition" | "product_by" | "sum_by" | "min_by"
                | "max_by" | "reduce"),
                1,
            )
            | (m @ ("enumerate" | "rev" | "dedup" | "flatten" | "pairwise" | "unzip"), 0)
            | (m @ ("fold" | "scan"), 2) => self.std_combinator_ty_at(
                "iter",
                m,
                arg_tys,
                resolved,
                DataPosition::Receiver,
                span,
            ),
            _ => None,
        }
    }

    /// Return type of a method on a `HashMap` / `BTreeMap` receiver whose
    /// result depends on the key/value types. Without this `m.iter()` is a
    /// fresh `Var`, so the for-vec lowering can't see the `(K, V)` element
    /// type and mis-sizes the element (especially when a destructure slot
    /// is `_`). Key/value-shaped arguments unify against the map's generics
    /// so an unannotated `HashMap::new()` is grounded by its first
    /// `insert` / `get` and native dispatch picks the right keyed symbol.
    /// Returns `None` for a non-map receiver so dispatch continues.
    pub(super) fn map_method_ret(
        &mut self,
        method: &str,
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let (key, value) = match self.tcx.kind(resolved) {
            Some(TyKind::HashMap { key, value, .. }) => (*key, *value),
            _ => return None,
        };
        // `set` is json's field-update helper, not a map method; the
        // bare-name dispatch would route it there and the write would
        // vanish (VM) or the symbol would not link (native), so reject
        // it uniformly here.
        if method == "set" {
            let ty = self.render_public_ty(resolved);
            let error = self.unresolved_method(ty, "set", resolved);
            self.emit(error, span);
            return Some(self.tcx.error_ty());
        }
        let (key_arg, value_arg) = match (method, arg_tys.len()) {
            ("insert" | "get_or" | "or_insert", 2) => {
                (arg_tys.first().copied(), arg_tys.get(1).copied())
            }
            ("get" | "remove" | "pop" | "contains" | "contains_key", 1) => {
                (arg_tys.first().copied(), None)
            }
            // `inc` is the integer-counter idiom: it pins the value to
            // i64 so an unannotated `HashMap::new()` grounded only by
            // `inc` still classifies for the counter lowering.
            ("inc", 1 | 2) => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.unify(value, i, span);
                (arg_tys.first().copied(), None)
            }
            _ => (None, None),
        };
        if let Some(arg_ty) = key_arg {
            let key_peeled = self.peel_refs(key);
            let arg_peeled = self.peel_refs(arg_ty);
            self.unify(key_peeled, arg_peeled, span);
        }
        if let Some(arg_ty) = value_arg {
            let value_peeled = self.peel_refs(value);
            let arg_peeled = self.peel_refs(arg_ty);
            self.unify(value_peeled, arg_peeled, span);
        }
        match method {
            // `m.iter()` yields `(K, V)` pairs, lazily like any other `iter`;
            // `collect` on that walk is how they are materialised.
            "iter" => {
                let pair = self.tcx.intern(TyKind::Tuple(vec![key, value]));
                Some(self.tcx.intern(TyKind::Iterator(pair)))
            }
            "keys" => {
                let key = self.peel_refs(key);
                Some(self.tcx.intern(TyKind::Vec(key)))
            }
            "values" => Some(self.tcx.intern(TyKind::Vec(value))),
            "get" | "pop" | "insert" | "remove" => Some(self.option_adt_ty(value)),
            "get_or" | "or_insert" => Some(value),
            "contains" | "contains_key" | "is_empty" => Some(self.tcx.bool_ty()),
            "len" => Some(self.tcx.int_ty(IntTy::I64)),
            "clear" => Some(self.tcx.unit()),
            _ => None,
        }
    }

    /// Whether the `Vec` surface accepts `name` at `arity` arguments. Used to
    /// name the count a method takes when a call supplies a different one.
    pub(super) fn vec_method_arity_exists(name: &str, arity: usize) -> bool {
        match name {
            "len" | "is_empty" | "first" | "last" | "to_vec" | "iter" | "sort" | "reverse"
            | "enumerate" | "rev" | "dedup" | "flatten" | "pairwise" | "sum" | "product"
            | "min" | "max" | "unzip" | "pop" | "clear" | "capacity" | "shrink_to_fit" => {
                arity == 0
            }
            "join" | "take" | "skip" | "step_by" | "chunks" | "windows" | "map" | "filter"
            | "filter_map" | "find_map" | "flat_map" | "take_while" | "skip_while" | "for_each"
            | "any" | "all" | "find" | "position" | "max_by_key" | "min_by_key" | "chunk_by"
            | "count_by" | "max_by" | "min_by" | "partition" | "product_by" | "reduce"
            | "sum_by" | "get" | "contains" | "index_of" | "count_of" | "insert" | "remove" => {
                arity == 1
            }
            "count" => arity <= 1,
            "slice" | "swap" | "fold" | "scan" => arity == 2,
            _ => false,
        }
    }

    /// Reports the argument count `name` takes when the receiver declares it
    /// but no arity accepted `found`. `None` leaves the call to the caller's
    /// ordinary unresolved-method path.
    pub(super) fn sequence_arity_mismatch(
        &mut self,
        name: &str,
        found: usize,
        span: Span,
    ) -> Option<Ty> {
        if !is_slice_sequence_method(name) && !is_vec_only_sequence_method(name) {
            return None;
        }
        // A count the surface accepts is typed by the combinator path.
        if Self::vec_method_arity_exists(name, found) {
            return None;
        }
        let expected =
            (0..=8).find(|arity| *arity != found && Self::vec_method_arity_exists(name, *arity))?;
        self.emit(
            TypeError::CallArityMismatch {
                callee: name.to_string(),
                expected,
                found,
            },
            span,
        );
        Some(self.tcx.error_ty())
    }

    /// Return type of a method on a `Vec` / slice / fixed-array receiver
    /// whose result is a function of the element type. Without this the
    /// checker falls through to a fresh `Var`, so a chained `.first()` /
    /// `.index_of(..).map(..)` reaches codegen with an untyped payload and
    /// the native tier mis-represents it. Also checks the `push` / `insert`
    /// argument against the element type (a `[i64]` accepting a `String`
    /// pointer word is a silent memory hazard on the native backend).
    /// Returns `None` for a non-sequence receiver so dispatch continues.
    #[allow(
        clippy::too_many_lines,
        reason = "method dispatch table stays readable as one row set"
    )]
    pub(super) fn vec_method_ret(
        &mut self,
        method: &str,
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let elem = match self.tcx.kind(resolved) {
            Some(TyKind::Vec(e) | TyKind::Slice(e)) => *e,
            Some(TyKind::Array { elem, .. }) => *elem,
            _ => return None,
        };
        let expected_arity = match method {
            "push" | "remove" | "truncate" | "extend" | "extend_from_slice" | "reserve"
            | "reserve_exact" | "get" | "fill" | "copy_from_slice" | "binary_search" => Some(1),
            "insert" | "swap" | "resize" => Some(2),
            "pop" | "clear" | "sort" | "reverse" | "capacity" | "iter" => Some(0),
            "sort_by" | "sort_by_key" => Some(1),
            "copy_within" => Some(3),
            _ => None,
        };
        if let Some(expected) = expected_arity
            && arg_tys.len() != expected
        {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("Vec::{method}"),
                    expected,
                    found: arg_tys.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        // References are layout-transparent (the runtime owns memory), so
        // peel them before comparing the pushed element to the slot type.
        let push_arg = match (method, arg_tys.len()) {
            ("push" | "fill", 1) => arg_tys.first().copied(),
            ("insert" | "resize", 2) => arg_tys.get(1).copied(),
            ("binary_search" | "contains" | "index_of" | "count_of", 1) => arg_tys.first().copied(),
            _ => None,
        };
        if let Some(arg_ty) = push_arg {
            let elem_peeled = self.peel_refs(elem);
            let arg_peeled = self.peel_refs(arg_ty);
            self.unify(elem_peeled, arg_peeled, span);
        }
        // `xs.extend(ys)` appends a sequence of the receiver's own element
        // type. Unifying it pins a literal argument to that element type, so
        // `Vec<u8>.extend(#[4, 5])` appends bytes rather than leaving the
        // literal at the default integer width.
        if matches!(method, "extend" | "extend_from_slice" | "copy_from_slice")
            && let Some(arg_ty) = arg_tys.first().copied()
        {
            let sequence = self.tcx.intern(TyKind::Vec(elem));
            let arg_peeled = self.peel_refs(arg_ty);
            if matches!(
                self.tcx.kind(self.infer.resolve(self.tcx, arg_peeled)),
                Some(TyKind::Vec(_) | TyKind::Var(_))
            ) {
                self.unify(sequence, arg_peeled, span);
            }
        }
        match (method, arg_tys.len()) {
            (
                "push" | "clear" | "truncate" | "extend" | "extend_from_slice" | "reserve"
                | "reserve_exact" | "sort" | "sort_by" | "sort_by_key" | "reverse" | "fill"
                | "swap" | "resize" | "copy_within" | "copy_from_slice",
                _,
            ) => Some(self.tcx.unit()),
            // `Ok(i)` is the found index and `Err(i)` the position an
            // insert would keep sorted, so both arms carry an index.
            ("binary_search", 1) => {
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                Some(self.result_adt_ty(i64_ty, i64_ty))
            }
            ("insert", 2) => {
                let error_ty = self.tcx.dyn_error_ty();
                let unit_ty = self.tcx.unit();
                Some(self.result_adt_ty(unit_ty, error_ty))
            }
            ("remove", 1) => {
                let error_ty = self.tcx.dyn_error_ty();
                Some(self.result_adt_ty(elem, error_ty))
            }
            ("capacity" | "len", 0) => Some(self.tcx.int_ty(IntTy::I64)),
            ("iter", 0) => {
                // Gossamer iteration yields managed values, not references.
                // Scalar elements copy and RC-backed elements gain their own
                // managed share. This keeps an iterator independent of a raw
                // element address while its runtime state retains the source.
                Some(self.tcx.intern(TyKind::Iterator(elem)))
            }
            ("is_empty", 0) => Some(self.tcx.bool_ty()),
            ("pop", 0) => Some(self.option_adt_ty(elem)),
            ("first" | "last", 0) => Some(self.option_adt_ty(elem)),
            ("get", 1) => {
                if let Some(arg_ty) = arg_tys.first() {
                    let i = self.tcx.int_ty(IntTy::I64);
                    let arg_peeled = self.peel_refs(*arg_ty);
                    self.unify(i, arg_peeled, span);
                }
                Some(self.option_adt_ty(elem))
            }
            // `dedup` describes the collection: it removes adjacent repeats
            // in place. `collect` and `rev` are traversals and belong to the
            // iterator, so they fall through to the collection-traversal
            // rejection.
            ("dedup", 0) => Some(self.tcx.intern(TyKind::Vec(elem))),
            // `to_vec` copies a borrowed or fixed-length sequence into an
            // owned one. A `Vec` is already that, so it does not carry the
            // conversion to itself.
            ("to_vec", 0) if !matches!(self.tcx.kind(resolved), Some(TyKind::Vec(_))) => {
                Some(self.tcx.intern(TyKind::Vec(elem)))
            }
            ("index_of", 1) => {
                let i = self.tcx.int_ty(IntTy::I64);
                Some(self.option_adt_ty(i))
            }
            ("count_of", 1) => Some(self.tcx.int_ty(IntTy::I64)),
            ("contains", 1) => Some(self.tcx.bool_ty()),
            ("slice", _) => {
                if arg_tys.len() != 2 {
                    self.emit(
                        TypeError::CallArityMismatch {
                            callee: "Vec::slice".to_string(),
                            expected: 2,
                            found: arg_tys.len(),
                        },
                        span,
                    );
                    return Some(self.tcx.error_ty());
                }
                let i = self.tcx.int_ty(IntTy::I64);
                for arg_ty in arg_tys {
                    let arg_peeled = self.peel_refs(*arg_ty);
                    self.unify(i, arg_peeled, span);
                }
                let vec = self.tcx.intern(TyKind::Vec(elem));
                let err = self.tcx.dyn_error_ty();
                Some(self.result_adt_ty(vec, err))
            }
            ("windows" | "chunks", 1) => {
                if let Some(arg_ty) = arg_tys.first() {
                    let i = self.tcx.int_ty(IntTy::I64);
                    let arg_peeled = self.peel_refs(*arg_ty);
                    self.unify(i, arg_peeled, span);
                }
                let window = self.tcx.intern(TyKind::Vec(elem));
                Some(self.tcx.intern(TyKind::Vec(window)))
            }
            ("pairwise", 0) => {
                let pair = self.tcx.intern(TyKind::Tuple(vec![elem, elem]));
                Some(self.tcx.intern(TyKind::Vec(pair)))
            }
            ("flatten", 0) => {
                let inner = self
                    .sequence_elem_ty(elem, span)
                    .unwrap_or_else(|| self.fresh());
                Some(self.tcx.intern(TyKind::Vec(inner)))
            }
            // `xs.join(sep)`: Display-renders scalar / String elements,
            // separator unifies with String. An aggregate element has no
            // joinable rendering and is rejected here so it can never
            // reach a shim that would join pointer words.
            ("join", 1) => {
                if let Some(arg_ty) = arg_tys.first() {
                    let s = self.tcx.string_ty();
                    let arg_peeled = self.peel_refs(*arg_ty);
                    self.unify(s, arg_peeled, span);
                }
                let elem_resolved = self.infer.resolve(self.tcx, elem);
                let elem_peeled = self.peel_refs(elem_resolved);
                if let Some((ty, class)) = self.not_displayable(elem_peeled) {
                    self.emit(TypeError::ValueNotDisplayable { ty, class }, span);
                    return Some(self.tcx.error_ty());
                }
                Some(self.tcx.string_ty())
            }
            // `xs.take(n)` / `xs.step_by(s)`: fresh Vec of the same
            // element type; the count/stride argument is an integer.
            ("take" | "step_by", 1) => {
                if let Some(arg_ty) = arg_tys.first() {
                    let i = self.tcx.int_ty(IntTy::I64);
                    let arg_peeled = self.peel_refs(*arg_ty);
                    self.unify(i, arg_peeled, span);
                }
                Some(self.tcx.intern(TyKind::Vec(elem)))
            }
            // A name this receiver does declare, written with an argument
            // count no arm accepts, failed on its arity rather than its
            // spelling.
            (name, found) => self.sequence_arity_mismatch(name, found, span),
        }
    }

    /// Return type of a method on a `String` receiver, with precise types
    /// for the commonly-chained methods (so `s.rfind(&"/").map(|i| i as
    /// i64)` types the `Option<i64>` payload rather than leaving it an
    /// untyped var the native tier mis-represents - the P0-4 shape).
    /// A method outside the `String` surface is the name-global dispatch
    /// leak (a `unicode::*` char predicate like `"abc".is_letter()`, or a
    /// typo like `"abc".bogus()`): it runs the wrong global body on the
    /// VM and fails to lower native, so it is rejected (P0-6).
    pub(super) fn string_method_ret(
        &mut self,
        method: &str,
        generics: &[AstGenericArg],
        expected: Expectation,
        span: Span,
    ) -> Ty {
        match method {
            "split" | "splitn" | "split_whitespace" | "lines" => {
                let s = self.tcx.string_ty();
                self.tcx.intern(TyKind::Vec(s))
            }
            // A cursor over the encoded text: walking a String holds the
            // text and a position, not a slot per scalar. `collect`
            // materialises, and `as_bytes` is the owned byte sequence.
            "chars" => {
                let c = self.tcx.intern(TyKind::Char);
                self.tcx.intern(TyKind::Iterator(c))
            }
            "bytes" => {
                let u8_ty = self.tcx.int_ty(IntTy::U8);
                self.tcx.intern(TyKind::Vec(u8_ty))
            }
            // `Option<i64>` Unicode scalar offsets.
            "find" | "rfind" | "find_any" | "rfind_any" | "index_rune" => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.option_adt_ty(i)
            }
            // Strict full-string parses: `"42".to_i64() -> Option<i64>`.
            "to_i64" => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.option_adt_ty(i)
            }
            "to_f64" => {
                let f = self.tcx.float_ty(FloatTy::F64);
                self.option_adt_ty(f)
            }
            "to_bool" => {
                let b = self.tcx.bool_ty();
                self.option_adt_ty(b)
            }
            "contains" | "contains_any" | "contains_rune" | "starts_with" | "ends_with"
            | "equal_fold" | "is_empty" => self.tcx.bool_ty(),
            "len" | "count" | "byte_at" | "byte_len" => self.tcx.int_ty(IntTy::I64),
            "clone" => self.tcx.string_ty(),
            "clear" | "truncate" | "push" | "push_str" | "push_char" | "push_byte" => {
                self.tcx.unit()
            }
            // Answers whether the window was valid UTF-8 and therefore appended.
            "push_utf8" | "push_json_quoted" => self.tcx.bool_ty(),
            // Methods that return a fresh `String` (runtime `*mut c_char`):
            // pinning the result type so chained calls (`s.trim().len()`) and
            // typed bindings lower from a known type instead of an inference
            // var carrying an untyped heap payload into MIR.
            "trim" | "trim_start" | "trim_end" | "trim_matches" | "trim_start_matches"
            | "trim_end_matches" | "to_uppercase" | "to_lowercase" | "to_title" | "replace"
            | "replacen" | "repeat" | "pad_left" | "pad_right" | "center" | "substring" => {
                self.tcx.string_ty()
            }
            // `as_bytes` -> `[u8]` (runtime `*mut GosVec` of bytes).
            "as_bytes" => {
                let u8_ty = self.tcx.int_ty(IntTy::U8);
                self.tcx.intern(TyKind::Vec(u8_ty))
            }
            // `split_once` / `rsplit_once` -> `Option<(String, String)>`.
            "split_once" | "rsplit_once" => {
                let s = self.tcx.string_ty();
                let pair = self.tcx.intern(TyKind::Tuple(vec![s, s]));
                self.option_adt_ty(pair)
            }
            // `strip_prefix` / `strip_suffix` -> `Option<String>`.
            "strip_prefix" | "strip_suffix" => {
                let s = self.tcx.string_ty();
                self.option_adt_ty(s)
            }
            // `slice(a, b) -> Result<String, errors::Error>` (out-of-range Err).
            "slice" => {
                let s = self.tcx.string_ty();
                let e = self.tcx.dyn_error_ty();
                self.result_adt_ty(s, e)
            }
            // One string reaches one parse: `to_i64` / `to_f64` / `to_bool`
            // are the strict, full-string forms, and each answers an
            // `Option<T>`. `parse` was a second surface with a second carrier
            // type for the same operation, so it reports with the rewrite and
            // still types as it did, leaving the rest of the body diagnosed on
            // its own terms.
            "parse" => {
                let suggestion = self.string_parse_replacement(expected);
                self.emit(TypeError::StringParseRetired { suggestion }, span);
                self.string_parse_ret("String::parse", generics, expected, span)
            }
            _ if is_string_method(method) => self.fresh(),
            _ => {
                let string_ty = self.tcx.string_ty();
                let error = self.unresolved_method("String".to_string(), method, string_ty);
                self.emit(error, span);
                self.tcx.error_ty()
            }
        }
    }

    pub(super) fn first_type_generic_arg(&mut self, generics: &[AstGenericArg]) -> Option<Ty> {
        generics.iter().find_map(|arg| match arg {
            AstGenericArg::Type(ty) => Some(self.type_from_ast(ty)),
            AstGenericArg::Const(_) => None,
        })
    }

    /// The `to_T()` spelling that replaces a `parse()` at this site, chosen
    /// from the type the surrounding code expects.
    pub(super) fn string_parse_replacement(&mut self, expected: Expectation) -> String {
        let payload = self.expectation_target(expected).and_then(|target| {
            let resolved = self.infer.resolve(self.tcx, target);
            match self.tcx.kind(resolved) {
                Some(TyKind::Adt { def, substs }) if def.local == RESULT_DEF_LOCAL => {
                    substs.types().first().copied()
                }
                _ => Some(resolved),
            }
        });
        match payload.map(|ty| self.tcx.kind_of(ty)) {
            Some(TyKind::Float(_)) => "to_f64",
            Some(TyKind::Bool) => "to_bool",
            _ => "to_i64",
        }
        .to_string()
    }

    pub(super) fn string_parse_ret(
        &mut self,
        callable: &str,
        generics: &[AstGenericArg],
        expected: Expectation,
        span: Span,
    ) -> Ty {
        let payload = if let Some(ty) = self.first_type_generic_arg(generics) {
            Some(ty)
        } else if let Some(ty) = self.result_ok_expectation(expected) {
            Some(ty)
        } else if let Some(target) = self.non_result_expectation_target(expected) {
            Some(target)
        } else {
            self.emit(
                TypeError::GenericReturnTypeUninferred {
                    callable: callable.to_string(),
                    param: "T".to_string(),
                },
                span,
            );
            None
        };
        let Some(payload) = payload else {
            return self.tcx.error_ty();
        };
        let e = self.tcx.dyn_error_ty();
        self.result_adt_ty(payload, e)
    }

    /// The `Ok` payload of a resolved `Result<T, E>`, or `None` for any
    /// other type.
    pub(super) fn result_ok_payload(&self, ty: Ty) -> Option<Ty> {
        let TyKind::Adt { def, substs } = self.tcx.kind(ty)? else {
            return None;
        };
        if def.local != RESULT_DEF_LOCAL && self.tcx.def_name(*def) != Some("Result") {
            return None;
        }
        match substs.as_slice().first()? {
            crate::GenericArg::Type(ok) => Some(*ok),
            crate::GenericArg::Const(_) | crate::GenericArg::ConstParam(_) => None,
        }
    }

    pub(super) fn result_ok_expectation(&mut self, expected: Expectation) -> Option<Ty> {
        self.result_payload_expectation(expected).map(|(ok, _)| ok)
    }

    pub(super) fn result_payload_expectation(&mut self, expected: Expectation) -> Option<(Ty, Ty)> {
        let ty = self.expectation_target(expected)?;
        let TyKind::Adt { def, substs } = self.tcx.kind(ty)? else {
            return None;
        };
        if def.local != u32::MAX && self.tcx.def_name(*def) != Some("Result") {
            return None;
        }
        let args = substs.as_slice();
        match (args.first()?, args.get(1)?) {
            (crate::GenericArg::Type(ok), crate::GenericArg::Type(err)) => Some((*ok, *err)),
            _ => None,
        }
    }

    pub(super) fn option_payload_expectation(&mut self, expected: Expectation) -> Option<Ty> {
        let ty = self.expectation_target(expected)?;
        let TyKind::Adt { def, substs } = self.tcx.kind(ty)? else {
            return None;
        };
        if def.local != u32::MAX - 1 && self.tcx.def_name(*def) != Some("Option") {
            return None;
        }
        match substs.as_slice().first()? {
            crate::GenericArg::Type(payload) => Some(*payload),
            crate::GenericArg::Const(_) | crate::GenericArg::ConstParam(_) => None,
        }
    }

    pub(super) fn non_result_expectation_target(&mut self, expected: Expectation) -> Option<Ty> {
        let target = self.expectation_target(expected)?;
        match self.tcx.kind(target)? {
            TyKind::Adt { def, .. }
                if def.local == u32::MAX || self.tcx.def_name(*def) == Some("Result") =>
            {
                None
            }
            TyKind::Var(_) | TyKind::Error => None,
            _ => Some(target),
        }
    }

    /// Rejects a method call on a concrete user struct / enum receiver
    /// when the method demonstrably belongs to a *different* user type -
    /// the name-global dispatch soundness hole (P0-6: `b.label()` runs
    /// `A`'s body against `B`'s memory on the VM and fails to lower on
    /// the native tier). Conservative on purpose: it fires only when the
    /// method name is owned by some user type but not this one, so an
    /// unknown method (a genuine typo with no owner anywhere) or a
    /// builtin / derived method (`clone`) still falls through.
    /// Re-validates deferred structural uses (`value[i]` / `value(args)`
    /// / `value.N`) after integer/float defaulting has given unsuffixed
    /// literals their concrete type. An operand that resolved to a
    /// concrete non-indexable / non-callable / non-tuple type is
    /// rejected here, so `let x = 5; x[0]` - whose `x` was an inference
    /// var at first check - is caught instead of faulting on the
    /// compiled tier.
    pub(super) fn check_deferred_structural(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_structural);
        for d in deferred {
            let mut resolved = self.infer.resolve(self.tcx, d.ty);
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(resolved).clone() {
                resolved = self.infer.resolve(self.tcx, inner);
            }
            let kind = self.tcx.kind_of(resolved).clone();
            match d.kind {
                DeferredStructuralKind::Index => {
                    let indexable = matches!(
                        kind,
                        TyKind::Array { .. }
                            | TyKind::Slice(_)
                            | TyKind::Vec(_)
                            | TyKind::String
                            | TyKind::HashMap { .. }
                    );
                    if !indexable && !is_soft_for_structural_use(&kind) {
                        let ty = self.render_public_ty(resolved);
                        self.emit(TypeError::NotIndexable { ty }, d.span);
                    }
                    if let Some(result) = d.result {
                        let element = match &kind {
                            TyKind::Array { elem, .. }
                            | TyKind::Slice(elem)
                            | TyKind::Vec(elem) => Some(*elem),
                            TyKind::String => Some(self.tcx.char_ty()),
                            TyKind::HashMap { value, .. } => Some(*value),
                            _ => None,
                        };
                        if let Some(element) = element {
                            self.unify(result, element, d.span);
                        }
                    }
                }
                DeferredStructuralKind::Call => {
                    if is_definitely_not_callable_value(&kind) {
                        let ty = self.render_public_ty(resolved);
                        self.emit(TypeError::NotCallable { ty }, d.span);
                    }
                }
                DeferredStructuralKind::TupleField(idx) => match &kind {
                    TyKind::Tuple(elems) => {
                        if idx as usize >= elems.len() {
                            let ty = self.render_public_ty(resolved);
                            self.emit(TypeError::NoTupleField { ty, index: idx }, d.span);
                        } else if let Some(result) = d.result {
                            let field = elems[idx as usize];
                            self.unify(result, field, d.span);
                        }
                    }
                    other => {
                        let is_tuple_struct = u32::try_from(idx)
                            .ok()
                            .is_some_and(|i| self.tuple_struct_field_ty(resolved, i).is_some());
                        if !is_tuple_struct && !is_soft_for_structural_use(other) {
                            let ty = self.render_public_ty(resolved);
                            self.emit(TypeError::NoTupleField { ty, index: idx }, d.span);
                        }
                    }
                },
                DeferredStructuralKind::Downgrade => {
                    if self.downgrade_receiver_is_non_rc(resolved) {
                        let ty = self.render_public_ty(resolved);
                        self.emit(TypeError::WeakDowngradeNonRc { ty }, d.span);
                    }
                }
            }
        }
    }

    pub(super) fn check_deferred_mutating_receivers(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_mutating_receivers);
        for receiver in deferred {
            if !self.method_requires_mut_receiver(receiver.ty, &receiver.method) {
                continue;
            }
            // The place verdict was taken while the receiver's type was still
            // an inference variable, so a `&mut` binding read as an
            // undeclared-`mut` local. Now that the type is known, a receiver
            // that crosses a mutable reference is a writable place.
            let mut resolved = self.infer.resolve(self.tcx, receiver.ty);
            let mut crossed_mutable_reference = false;
            while let Some(TyKind::Ref { mutability, inner }) = self.tcx.kind(resolved) {
                if *mutability == Mutbl::Mut {
                    crossed_mutable_reference = true;
                }
                resolved = self.infer.resolve(self.tcx, *inner);
            }
            if crossed_mutable_reference {
                continue;
            }
            self.emit_mutating_place_error(receiver.place, receiver.name, receiver.span);
        }
    }

    /// Emits mismatches held until numeric literal defaulting has made their
    /// type names stable. This keeps a rejected `r = [2, 3]` diagnostic
    /// readable as `&[i64; 2]` versus `[i64; 2]`, not inference variables.
    /// Runs the receiver-shape rejections that share one outcome: the
    /// method does not exist on this receiver, so the arguments are
    /// checked for their own errors and the call types as `error`.
    ///
    /// Returns the error type when one of them reported.
    pub(super) fn reject_method_on_receiver(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        if self.reject_method_off_bound(receiver_ty, method, args, span) {
            return Some(self.tcx.error_ty());
        }
        // A callable carries no method surface, so a method reached on one is
        // rejected before the conversion and representation paths below: those
        // would otherwise type `into` / `to_string` against a code address.
        if let Some(ty) = self.reject_method_on_callable(receiver_ty, method, args, span) {
            return Some(ty);
        }
        // `into` / `try_into` are conversions rather than surface the
        // receiver has to declare, and are typed further down. An opaque
        // alias formats as its representation does, so `to_string` is its own
        // Display surface rather than a representation method.
        if matches!(method, "into" | "try_into" | "to_string") && args.is_empty() {
            return None;
        }
        self.reject_nominal_repr_method(receiver_ty, method, args, span)
    }

    /// Rejects any method reached on a function, closure, or `Fn(..)` value.
    ///
    /// A callable is a code address: it declares no methods of its own and
    /// inherits none, so every such call is unresolved. Naming it here keeps
    /// the receiver from reaching the generic method paths, which would treat
    /// an unconstrained callable as text and answer the function's own name.
    /// Returns the error type when a diagnostic was emitted.
    pub(super) fn reject_method_on_callable(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        let mut r = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(r) {
            r = self.infer.resolve(self.tcx, *inner);
        }
        if !matches!(
            self.tcx.kind(r),
            Some(
                TyKind::FnDef { .. }
                    | TyKind::FnPtr(_)
                    | TyKind::FnTrait(_)
                    | TyKind::Closure { .. }
            )
        ) {
            return None;
        }
        for arg in args {
            self.check_expr(arg);
        }
        // `FnDef` and `Closure` print with an internal def index, which says
        // nothing to a reader; name the callable by what it is instead.
        let ty = match self.tcx.kind(r) {
            Some(TyKind::FnDef { def, .. }) => {
                let def = *def;
                match self.tcx.def_name(def) {
                    Some(name) => format!("fn {name}"),
                    None => "fn".to_string(),
                }
            }
            Some(TyKind::Closure { .. }) => "closure".to_string(),
            _ => self.render_public_ty(r),
        };
        self.emit(
            TypeError::UnresolvedMethod {
                ty,
                name: self.written_method_name(method),
                available: Vec::new(),
                field_of_same_name: false,
                free_fn_of_same_name: self.user_fn_names.contains(method),
            },
            span,
        );
        Some(self.tcx.error_ty())
    }

    /// Rejects a method reached on an opaque alias that only its
    /// representation declares.
    ///
    /// The alias exists to hide what it is made of, so the
    /// representation's surface is not part of it: `type Name = new
    /// String` gets no `len()` unless its own `impl` provides one.
    /// Converting to the representation is how that surface is reached.
    /// Returns the error type when a diagnostic was emitted.
    pub(super) fn reject_nominal_repr_method(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        let mut r = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(r) {
            r = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Nominal { def, .. }) = self.tcx.kind(r) else {
            return None;
        };
        let name = self.tcx.def_name(*def).map(str::to_string)?;
        if self
            .user_method_owners
            .get(method)
            .is_some_and(|owners| owners.contains(&name))
        {
            return None;
        }
        for arg in args {
            self.check_expr(arg);
        }
        let mut available: Vec<String> = self
            .user_method_owners
            .iter()
            .filter(|(_, owners)| owners.contains(&name))
            .map(|(m, _)| m.clone())
            .collect();
        available.sort();
        self.emit(
            TypeError::UnresolvedMethod {
                ty: name,
                name: self.written_method_name(method),
                available,
                field_of_same_name: false,
                free_fn_of_same_name: self.user_fn_names.contains(method),
            },
            span,
        );
        Some(self.tcx.error_ty())
    }

    /// Types `x.into()` / `x.try_into()`, whose target is fixed by the use
    /// site rather than by the call, and records `into` for the conversion
    /// audit once unification has pinned that target.
    pub(super) fn check_conversion_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        span: Span,
    ) -> Ty {
        let result = self.fresh();
        if method == "into" {
            self.deferred_into_conversions
                .push((receiver_ty, result, span));
            self.deferred_conversion_targets
                .push((result, "into", span));
        } else if method == "try_into" {
            self.deferred_conversion_targets
                .push((result, "try_into", span));
        }
        result
    }

    /// Reports `.into()` / `.try_into()` written where no use site fixes
    /// the target.
    ///
    /// The target of a conversion never comes from the receiver, so a call
    /// nothing constrains leaves its result an inference variable. Such a
    /// call has no `From` impl to reach and lowers to a bare `into`, which
    /// no tier binds; naming it here keeps the failure at `check`.
    pub(super) fn check_deferred_conversion_targets(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_conversion_targets);
        for (result, method, span) in deferred {
            let result = self.deep_resolve(result);
            let open = match self.tcx.kind(result) {
                Some(TyKind::Var(_)) => true,
                // `try_into` answers `Result<B, E>`; the target is `B`.
                _ => self
                    .result_ok_payload(result)
                    .map(|ok| self.deep_resolve(ok))
                    .is_some_and(|ok| matches!(self.tcx.kind(ok), Some(TyKind::Var(_)))),
            };
            if !open {
                continue;
            }
            self.emit(
                TypeError::ConversionTargetUnknown {
                    method: method.to_string(),
                },
                span,
            );
        }
    }

    /// Reports `.into()` across an opaque alias boundary with nothing
    /// behind it.
    ///
    /// An alias and its representation convert for free in both
    /// directions - one runtime value, so the conversion is the identity.
    /// Every other pair, including two aliases that happen to erase to the
    /// same representation, needs a `From` impl, and saying so here keeps
    /// the failure at `check` instead of at run time.
    pub(super) fn check_deferred_into_conversions(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_into_conversions);
        for (recv, result, span) in deferred {
            // The built-in array conversion answers for the value itself, so
            // it reads the receiver as written; a `From` impl is reached
            // through a reference just as it is through the value.
            let written = self.deep_resolve(recv);
            let mut recv = written;
            while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(recv) {
                recv = self.deep_resolve(*inner);
            }
            let result = self.deep_resolve(result);
            // An unresolved side has nothing to audit yet; an ambiguous
            // `.into()` with no target is caught where its type stays open.
            if matches!(self.tcx.kind(recv), Some(TyKind::Var(_)))
                || matches!(self.tcx.kind(result), Some(TyKind::Var(_)))
            {
                continue;
            }
            // A repr pair converts in both directions.
            if self.is_nominal_repr_pair(recv, result) {
                continue;
            }
            // `From<[T; N]> for Vec<T>` is built in, and lowers to the same
            // buffer copy `Vec::from(array)` does on every tier.
            if self.is_array_to_vec(written, result) {
                continue;
            }
            // An opaque alias converts to itself for free - one runtime
            // value. A primitive `.into()` to its own type has no `From`
            // behind it and lowers to an unbound `into`, so identity is a
            // conversion only for a nominal type.
            let recv_nominal = matches!(self.tcx.kind(recv), Some(TyKind::Nominal { .. }));
            if recv == result && recv_nominal {
                continue;
            }
            // A user `From` impl on the target answers for the pair.
            let target = self.render_public_ty(result);
            if self
                .user_method_owners
                .get("from")
                .is_some_and(|owners| owners.contains(&target))
            {
                continue;
            }
            let borrowed_sequence = self.is_array_to_vec(recv, result);
            let from = self.render_public_ty(if borrowed_sequence { written } else { recv });
            self.emit(
                TypeError::NoConversion {
                    from,
                    to: target,
                    borrowed_sequence,
                },
                span,
            );
        }
    }

    /// Whether `from` is a fixed array and `to` the `Vec` of its element.
    pub(super) fn is_array_to_vec(&self, from: Ty, to: Ty) -> bool {
        let (Some(TyKind::Array { elem, .. }), Some(TyKind::Vec(target))) =
            (self.tcx.kind(from), self.tcx.kind(to))
        else {
            return false;
        };
        elem == target
    }

    /// Whether one of `a` / `b` is an opaque alias whose representation is
    /// the other.
    pub(super) fn is_nominal_repr_pair(&self, a: Ty, b: Ty) -> bool {
        let over = |outer: Ty, inner: Ty| matches!(self.tcx.kind(outer), Some(TyKind::Nominal { repr, .. }) if *repr == inner);
        over(a, b) || over(b, a)
    }

    pub(super) fn check_deferred_type_mismatches(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_type_mismatches);
        for (expected, found, span) in deferred {
            let expected = self.deep_resolve(expected);
            let found = self.deep_resolve(found);
            let expected = self.render_public_ty(expected);
            let found = self.render_public_ty(found);
            self.emit(TypeError::TypeMismatch { expected, found }, span);
        }
    }

    /// Emits literal-constraint mismatches after defaulting has resolved every
    /// nested component of the expected type. This prevents diagnostics such
    /// as `&mut ?0` when the referent is known to be `i64`.
    pub(super) fn check_deferred_literal_type_mismatches(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_literal_type_mismatches);
        for (expected, found, span) in deferred {
            let expected = self.deep_resolve(expected);
            let expected = self.render_public_ty(expected);
            self.emit(
                TypeError::TypeMismatch {
                    expected,
                    found: found.to_string(),
                },
                span,
            );
        }
    }

    pub(super) fn maybe_reject_unknown_adt_method(
        &mut self,
        resolved: Ty,
        method: &str,
        span: Span,
    ) {
        if matches!(method, "clone") {
            return;
        }
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return;
        };
        let Some(name) = self.tcx.def_name(*def).map(str::to_string) else {
            return;
        };
        // Only genuine user-declared struct / enum receivers: sentinel
        // Adts (Result / Option / http::Response / VecDeque) are not in
        // `user_type_decls`.
        if !self.user_type_decls.contains(&name) {
            return;
        }
        // `user_method_owners` records every impl and trait method, so a
        // name this receiver does not own is a typo or a method of another
        // type; both would reach the compiled tier as an undefined
        // `@Type::method` symbol.
        let owned_here = self
            .user_method_owners
            .get(method)
            .is_some_and(|owners| owners.contains(&name));
        if !owned_here {
            let error = self.unresolved_method(name, method, resolved);
            self.emit(error, span);
        }
    }

    /// As [`Self::reject_private_method`], resolving the receiver's own
    /// identity first. Runs for every method call, including the ones a
    /// later pass resolves a return type for.
    pub(super) fn reject_private_method_call(&mut self, receiver_ty: Ty, method: &str, span: Span) {
        let mut peeled = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
            peeled = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(peeled) else {
            return;
        };
        let Some(name) = self.tcx.def_name(*def).map(str::to_string) else {
            return;
        };
        self.reject_private_method(&name, method, span);
    }

    /// Rejects a reference to a field declared without `pub` from outside
    /// the module its struct was declared in. A `pub` struct may keep
    /// private fields: the type is API, its representation need not be.
    pub(super) fn reject_private_field(&mut self, receiver_ty: Ty, field: &str, span: Span) {
        let mut peeled = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
            peeled = self.infer.resolve(self.tcx, *inner);
        }
        if self.synthesized_depth == 0 && matches!(self.tcx.kind(peeled), Some(TyKind::Var(_))) {
            self.deferred_field_receivers
                .push((receiver_ty, field.to_string(), span));
        }
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(peeled) else {
            // The receiver's type is not known yet; a struct it later
            // resolves to still has to satisfy the rule.
            self.deferred_private_fields.push((
                receiver_ty,
                field.to_string(),
                span,
                self.current_module.clone(),
            ));
            return;
        };
        let def = *def;
        self.reject_private_field_of(def, field, span);
    }

    /// Re-runs the field-visibility rule for accesses whose receiver type
    /// only became known after inference finished.
    pub(super) fn check_deferred_private_fields(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_private_fields);
        for (receiver_ty, field, span, module) in deferred {
            let mut peeled = self.infer.resolve(self.tcx, receiver_ty);
            while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
                peeled = self.infer.resolve(self.tcx, *inner);
            }
            let Some(TyKind::Adt { def, .. }) = self.tcx.kind(peeled) else {
                continue;
            };
            let def = *def;
            let prior = std::mem::replace(&mut self.current_module, module);
            self.reject_private_field_of(def, &field, span);
            self.current_module = prior;
        }
    }

    /// Reports a field read from an unannotated closure parameter no part of
    /// the program gave a type, which no tier can lay out. Other values of
    /// undecided type - a flag cell, a binding's struct - are read by name at
    /// run time.
    pub(super) fn check_deferred_field_receivers(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_field_receivers);
        let params: Vec<Ty> = self
            .unannotated_closure_params
            .iter()
            .map(|ty| self.infer.resolve(self.tcx, *ty))
            .filter(|ty| matches!(self.tcx.kind(*ty), Some(TyKind::Var(_))))
            .collect();
        for (receiver_ty, field, span) in deferred {
            let resolved = self.infer.resolve(self.tcx, receiver_ty);
            if params.contains(&resolved) {
                self.emit(TypeError::FieldReceiverUninferred { field }, span);
            }
        }
    }

    /// As [`Self::reject_private_field`], with the owning struct already
    /// resolved.
    pub(super) fn reject_private_field_of(&mut self, def: DefId, field: &str, span: Span) {
        if self.synthesized_depth > 0 {
            return;
        }
        let Some((home, visibility)) = self.field_homes.get(&(def, field.to_string())) else {
            return;
        };
        let reachable = self.current_module.starts_with(home.as_slice())
            || match visibility {
                Visibility::Public => true,
                Visibility::Package => self.resolutions.same_package(home, &self.current_module),
                Visibility::Inherited => false,
            };
        if reachable {
            return;
        }
        let module = home.join("::");
        let ty = self
            .tcx
            .def_name(def)
            .map_or_else(|| "?".to_string(), str::to_string);
        self.emit(
            TypeError::PrivateField {
                ty,
                name: field.to_string(),
                module,
            },
            span,
        );
    }

    /// Rejects a call to a method declared without `pub` from outside the
    /// module its `impl` was written in. This is the rule the resolver
    /// applies to a free function: the declaring module and its
    /// descendants keep access, so a `pub` wrapper always reaches the
    /// private helpers declared beside it.
    pub(super) fn reject_private_method(&mut self, ty: &str, method: &str, span: Span) {
        let Some((home, visibility)) = self.method_homes.get(&(ty.to_string(), method.to_string()))
        else {
            return;
        };
        // The same rule a field and a free function get: the declaring module
        // and its descendants always reach it, `pub` reaches everywhere, and
        // `pub(package)` reaches the rest of its own package.
        let reachable = self.current_module.starts_with(home.as_slice())
            || match visibility {
                Visibility::Public => true,
                Visibility::Package => self.resolutions.same_package(home, &self.current_module),
                Visibility::Inherited => false,
            };
        if reachable {
            return;
        }
        let error = TypeError::PrivateMethod {
            ty: ty.to_string(),
            name: method.to_string(),
            module: home.join("::"),
        };
        self.emit(error, span);
    }

    /// The single container-shaped (`Vec` / `Slice` / `Tuple`, ref
    /// transparent) parameter type at position `i` across the
    /// candidate signatures, or `None` when absent or ambiguous.
    pub(super) fn unique_container_expectation(
        &mut self,
        candidates: &[Vec<Ty>],
        i: usize,
    ) -> Option<Ty> {
        let mut found: Option<(Ty, String)> = None;
        for sig in candidates {
            let Some(&ty) = sig.get(i) else { continue };
            // A parameter typed by the method's own type parameters says
            // nothing about a literal until the call instantiates them.
            if self.ty_mentions_generic_param(ty) {
                continue;
            }
            let mut peeled = self.infer.resolve(self.tcx, ty);
            while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
                peeled = self.infer.resolve(self.tcx, *inner);
            }
            if !matches!(
                self.tcx.kind(peeled),
                Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Tuple(_))
            ) {
                continue;
            }
            let rendered = render_ty(self.tcx, ty);
            match &found {
                Some((_, existing)) if *existing == rendered => {}
                Some(_) => return None,
                None => found = Some((ty, rendered)),
            }
        }
        found.map(|(ty, _)| ty)
    }

    pub(super) fn result_adt_ty(&mut self, ok: Ty, err: Ty) -> Ty {
        let substs = crate::Substs::from_types([ok, err]);
        let def = gossamer_resolve::DefId::local(u32::MAX);
        self.tcx.register_def_name(def, "Result");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    pub(super) fn raw_stdlib_helper_ret(&mut self, name: &str) -> Option<Ty> {
        let ok = match name {
            "__gos_pem_decode_raw" => self.tuple_str_bytes_ty(),
            "__gos_pem_decode_all_raw" => {
                let entry = self.tuple_str_bytes_ty();
                self.tcx.intern(TyKind::Vec(entry))
            }
            "__gos_fs_metadata_raw" => self.tuple_fs_metadata_ty(),
            "__gos_fs_read_dir_raw" => {
                let entry = self.tuple_dir_entry_ty();
                self.tcx.intern(TyKind::Vec(entry))
            }
            "__gos_fs_walk_dir_raw" => self.tcx.unit(),
            "__gos_process_run_raw"
            | "__gos_process_run_in_raw"
            | "__gos_process_pipeline_run_raw" => self.tuple_process_output_ty(),
            "__gos_x509_parse_pem_raw" => self.tuple_cert_info_ty(),
            "__gos_tar_read_raw" | "__gos_zip_read_raw" => {
                let entry = self.tuple_archive_entry_ty();
                self.tcx.intern(TyKind::Vec(entry))
            }
            "__gos_time_location_raw" | "__gos_time_fixed_location_raw" => self.tcx.string_ty(),
            "__gos_time_civil_raw" => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.tcx.intern(TyKind::Tuple(vec![i; 9]))
            }
            "__gos_time_resolve_raw" => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.tcx.intern(TyKind::Tuple(vec![i; 3]))
            }
            "__gos_time_format_in_raw" => self.tcx.string_ty(),
            "__gos_time_add_date_raw" | "__gos_fd_wait_raw" => self.tcx.int_ty(IntTy::I64),
            _ => return None,
        };
        let err = self.tcx.dyn_error_ty();
        Some(self.result_adt_ty(ok, err))
    }

    pub(super) fn tuple_cert_info_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let i = self.tcx.int_ty(IntTy::I64);
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        let vec_u8 = self.tcx.intern(TyKind::Vec(u8_ty));
        let vec_str = self.tcx.intern(TyKind::Vec(s));
        self.tcx
            .intern(TyKind::Tuple(vec![s, s, vec_u8, i, i, vec_str, vec_u8]))
    }

    /// `(name, path, is_file, is_dir, is_symlink, size, modified_ms)`, one
    /// `fs::read_dir` entry.
    pub(super) fn tuple_dir_entry_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let i = self.tcx.int_ty(IntTy::I64);
        let b = self.tcx.bool_ty();
        self.tcx.intern(TyKind::Tuple(vec![s, s, b, b, b, i, i]))
    }

    /// `(stdout, stderr, code)`, a finished `process::run`.
    pub(super) fn tuple_process_output_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let i = self.tcx.int_ty(IntTy::I64);
        self.tcx.intern(TyKind::Tuple(vec![s, s, i]))
    }

    pub(super) fn tuple_fs_metadata_ty(&mut self) -> Ty {
        let i = self.tcx.int_ty(IntTy::I64);
        let b = self.tcx.bool_ty();
        self.tcx.intern(TyKind::Tuple(vec![i, b, b, b, b, i]))
    }

    pub(super) fn tuple_archive_entry_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        let vec_u8 = self.tcx.intern(TyKind::Vec(u8_ty));
        let b = self.tcx.bool_ty();
        self.tcx.intern(TyKind::Tuple(vec![s, vec_u8, b]))
    }

    pub(super) fn tuple_str_bytes_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        let vec_u8 = self.tcx.intern(TyKind::Vec(u8_ty));
        self.tcx.intern(TyKind::Tuple(vec![s, vec_u8]))
    }

    pub(super) fn payload_adt_method_owner(&mut self, ty: Ty) -> Option<&'static str> {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX => Some("Result"),
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX - 1 => Some("Option"),
            _ => None,
        }
    }

    /// Explicit argument counts a method on a built-in iterator receiver
    /// accepts: the combinator's declared arity less the data slot the
    /// receiver fills. `None` for a name the iterator surface does not
    /// declare, which the unresolved-method path reports instead.
    pub(super) fn iterator_method_arities(name: &str) -> Option<&'static [usize]> {
        match name {
            "next" => Some(&[0]),
            // `count` answers the length, or the accepted-element count
            // when handed a predicate.
            "count" => Some(&[0, 1]),
            _ => match Self::std_combinator_arity("iter", name)?.checked_sub(1)? {
                0 => Some(&[0]),
                1 => Some(&[1]),
                2 => Some(&[2]),
                _ => None,
            },
        }
    }

    /// Full argument arity (closure/seed args plus the trailing data
    /// arg) of a std data-last combinator the checker can type, or
    /// `None` for names it has no signature row for.
    pub(super) fn std_combinator_arity(module: &str, name: &str) -> Option<usize> {
        let arity = match (module, name) {
            (
                "result",
                "map" | "map_err" | "and_then" | "or_else" | "unwrap_or" | "unwrap_or_else",
            ) => 2,
            ("result", "expect") => 2,
            ("result", "ok" | "err" | "is_ok" | "is_err" | "unwrap") => 1,
            (
                "option",
                "map" | "and_then" | "filter" | "or" | "or_else" | "unwrap_or" | "unwrap_or_else"
                | "zip" | "ok_or" | "ok_or_else",
            ) => 2,
            ("option", "expect") => 2,
            ("option", "flatten" | "is_some" | "is_none" | "iter" | "unwrap") => 1,
            (
                "iter",
                "collect" | "count" | "sum" | "product" | "min" | "max" | "once" | "range"
                | "range_inclusive" | "repeat",
            ) => {
                if matches!(name, "range" | "range_inclusive" | "repeat") {
                    2
                } else {
                    1
                }
            }
            ("iter", "fold" | "scan") => 3,
            ("iter", "take" | "skip" | "step_by" | "chain" | "zip" | "windows" | "chunks") => 2,
            ("iter", "enumerate" | "rev" | "dedup" | "flatten" | "pairwise" | "unzip") => 1,
            ("iter", "empty") => 0,
            (
                "iter",
                "for_each" | "map" | "filter" | "filter_map" | "flat_map" | "reduce" | "sum_by"
                | "product_by" | "any" | "all" | "find" | "position" | "find_map" | "take_while"
                | "skip_while" | "partition" | "sort_by" | "sort_by_key" | "min_by" | "max_by"
                | "min_by_key" | "max_by_key" | "chunk_by" | "count_by",
            ) => 2,
            _ => return None,
        };
        Some(arity)
    }

    pub(super) fn iter_adapter_result_ty(&mut self, item: Ty, lazy_result: bool) -> Ty {
        if lazy_result {
            self.tcx.iterator_ty(item)
        } else {
            self.tcx.intern(TyKind::Vec(item))
        }
    }
}
