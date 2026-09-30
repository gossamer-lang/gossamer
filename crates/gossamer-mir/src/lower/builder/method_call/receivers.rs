//! Receiver-specific lowering: lazy iterators, weak references, JSON values, arrays, and receiver places.

use super::*;

impl<'a> Builder<'a> {
    /// Runtime symbol that advances lazy state yielding `elem` by one pull.
    /// The pair state is a distinct runtime object with its own advance.
    pub(crate) fn lazy_iter_next_symbol(&self, elem: Ty) -> Option<&'static str> {
        match self.lazy_iter_elem_family(elem) {
            Some(LazyElemFamily::PairWord) => Some("gos_rt_lazy_iter_next_pair_i64"),
            Some(
                LazyElemFamily::Word
                | LazyElemFamily::Ptr
                | LazyElemFamily::Float
                | LazyElemFamily::Aggr,
            ) => Some("gos_rt_lazy_iter_next_i64"),
            // An element wider than a slot rides an address-carrying stream,
            // which advances through the word shim with the element's address
            // as the `Some` payload.
            None if self.lazy_elem_is_addressed(elem) => Some("gos_rt_lazy_iter_next_i64"),
            None => None,
        }
    }

    /// Whether the lazy iterator runtime can carry `elem` in its 8-byte slot.
    pub(crate) fn lazy_iter_carries_elem(&self, elem: Ty) -> bool {
        self.lazy_iter_elem_family(elem).is_some()
    }

    /// Whether a sequence of `elem` can be borrowed as lazy state.
    ///
    /// The pair state is built only by `zip` and `enumerate`; it has no
    /// borrowed-sequence source, and a pair sequence stores two words per
    /// element, which the single-slot borrow cannot address. Such a sequence
    /// stays on the eager surface, where the element is read at its real width.
    pub(crate) fn lazy_iter_borrowable_elem(&self, elem: Ty) -> bool {
        // A borrowed source reads each element from a word-wide slot, so only
        // elements a sequence stores that way qualify. A narrower integer or a
        // `char` is packed at its own width in the buffer, and reading it as a
        // word would take the neighbouring bytes with it.
        matches!(
            self.tcx.kind_of(elem),
            TyKind::Int(gossamer_types::IntTy::I64)
                | TyKind::String
                | TyKind::Float(gossamer_types::FloatTy::F64)
        )
    }

    /// Whether an element-preserving adapter answering `ty` can answer lazy
    /// state: an element the slot carries, or one wider than a slot whose
    /// address the slot carries instead.
    pub(crate) fn lazy_iter_result_ty(&self, ty: Ty) -> bool {
        match self.tcx.kind_of(ty) {
            TyKind::Iterator(elem) => {
                self.lazy_iter_elem_family(*elem).is_some() || self.lazy_addressed_elem(*elem)
            }
            _ => false,
        }
    }

    /// Family of an `Iterator<T>`-typed value, or `None` when `ty` is not
    /// iterator state the lazy runtime carries.
    pub(crate) fn lazy_iter_ty_family(&self, ty: Ty) -> Option<LazyElemFamily> {
        match self.tcx.kind_of(ty) {
            TyKind::Iterator(elem) => self.lazy_iter_elem_family(*elem),
            _ => None,
        }
    }

    pub(super) fn lower_rc_weak_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        // `x.downgrade()` - create a `Weak<T>` from a strong RC value.
        // `gos_rt_rc_downgrade` bumps the weak count and returns the same
        // payload pointer, now typed `Weak<T>` so the drop pass releases
        // it through `gos_rt_rc_weak_release`. The referent it observes is
        // first pinned in a frame-owned local, so liveness follows the
        // downgrading scope rather than the source binding's last use -
        // the schedule the interpreter's downgrade pin keeps.
        if method.name.as_str() == "downgrade" && args.is_empty() {
            let Some(recv_local) = self.lower_expr(receiver) else {
                return MethodLowering::Handled(None);
            };
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let weak_ty = self.weak_adt_ty(recv_ty);
            let Some(pin_local) = self.pin_weak_referent(recv_local, recv_ty, span) else {
                // No RC allocation exists to observe (an opaque runtime
                // handle), so the weak is born dead: a null referent, which
                // every weak helper reads as `None`.
                let dest = self.fresh(weak_ty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    span,
                );
                return MethodLowering::Handled(Some(dest));
            };
            let dest = self.fresh(weak_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_rc_downgrade".to_string())),
                args: vec![Operand::Copy(Place::local(pin_local))],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return MethodLowering::Handled(Some(dest));
        }
        // `w.upgrade()` - turn a `Weak<T>` back into `Option<T>`.
        // `gos_rt_rc_weak_upgrade_opt` packs `Some(payload)` when the
        // referent is still alive (`strong > 0`) and `None` otherwise,
        // as the `{disc, payload}` pair the standard match / if-let
        // discriminant read works on, on every tier. The Some payload
        // carries a fresh strong reference taken atomically inside the
        // shim (a CAS from a non-zero count for shared referents), so an
        // upgrade racing another goroutine's final release can never hand
        // out a dead pointer. That reference is pinned in a frame-owned
        // shadow local (`gos_rt_weak_opt_payload` extracts the payload
        // word, null for `None`), which the drop pass releases at scope
        // exit / reassignment - mirroring the interpreter, whose
        // `Some(value)` holds an `Arc` clone until its binding dies.
        if method.name.as_str() == "upgrade" && args.is_empty() {
            let Some(recv_local) = self.lower_expr(receiver) else {
                return MethodLowering::Handled(None);
            };
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let payload_ty = self.weak_payload_ty(recv_ty).unwrap_or(ty);
            let opt_ty = self.option_payload_adt_ty(payload_ty);
            let dest = self.fresh(opt_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_rc_weak_upgrade_opt".to_string())),
                args: vec![Operand::Copy(Place::local(recv_local))],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            // The pin's type decides the release helper the drop pass emits
            // for it. A by-value aggregate payload lives in an RC cell, so
            // the pin holds the cell pointer, not aggregate slot data.
            let shadow_ty = if self.weak_referent_needs_cell(payload_ty) {
                self.weak_cell_adt_ty(payload_ty)
            } else {
                payload_ty
            };
            let shadow = self.fresh(shadow_ty);
            self.emit_assign(
                Place::local(shadow),
                Rvalue::CallIntrinsic {
                    name: "gos_rt_weak_opt_payload",
                    args: vec![Operand::Copy(Place::local(dest))],
                },
                span,
            );
            return MethodLowering::Handled(Some(dest));
        }
        MethodLowering::Pass
    }

    /// Materialises the referent a `Weak` observes and leaves it owned by the
    /// enclosing frame. An RC-managed receiver is pinned by an ordinary owning
    /// copy; a by-value aggregate has no RC header, so its slot bytes are
    /// copied into an RC cell that becomes the referent. Returns `None` for a
    /// receiver with no RC allocation to observe at all.
    pub(super) fn pin_weak_referent(
        &mut self,
        recv_local: Local,
        recv_ty: Ty,
        span: Span,
    ) -> Option<Local> {
        if self.tcx.is_rc_managed(recv_ty) && !self.tcx.is_weak_ty(recv_ty) {
            let pin = self.fresh(recv_ty);
            self.emit_assign(
                Place::local(pin),
                Rvalue::Use(Operand::Copy(Place::local(recv_local))),
                span,
            );
            return Some(pin);
        }
        if !self.weak_referent_needs_cell(recv_ty) {
            return None;
        }
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let size_bytes = i128::from(self.type_slot_bytes(recv_ty));
        let meta_sym = self.ensure_aggr_struct_meta(recv_ty).unwrap_or_default();
        let size_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(size_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(size_bytes))),
            span,
        );
        let cell_ty = self.weak_cell_adt_ty(recv_ty);
        let cell = self.fresh(cell_ty);
        self.emit_assign(
            Place::local(cell),
            Rvalue::CallIntrinsic {
                name: "gos_rt_rc_weak_cell",
                args: vec![
                    Operand::Copy(Place::local(size_local)),
                    Operand::Const(ConstValue::Str(meta_sym)),
                    Operand::Copy(Place::local(recv_local)),
                ],
            },
            span,
        );
        Some(cell)
    }

    /// `d.as_millis()`, `inst.elapsed()`, `later.duration_since(earlier)`:
    /// the `time::Duration` / `time::Instant` methods, each the runtime
    /// helper its qualified form calls with the receiver as the first
    /// argument.
    pub(super) fn lower_time_unit_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        let mut recv_kind = self.tcx.kind_of(receiver.ty).clone();
        while let TyKind::Ref { inner, .. } = recv_kind {
            recv_kind = self.tcx.kind_of(inner).clone();
        }
        // A `flag::Set` duration cell carries no Duration tag on its HIR
        // type (the typechecker leaves it an inference var); its MIR binding
        // is tagged `flag::Cell::Duration` and auto-derefs to the Duration.
        let is_duration_cell = self
            .receiver_local_from_path(receiver)
            .and_then(|l| self.local_runtime_kind.get(&l).copied())
            == Some("flag::Cell::Duration");
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let duration = is_duration_cell || matches!(recv_kind, TyKind::Duration);
        let (sym, ret) = match (&recv_kind, method.name.as_str(), args.len()) {
            (_, name, 0) if duration => match name {
                "as_nanos" => ("gos_rt_duration_as_nanos", i64_ty),
                "as_micros" => ("gos_rt_duration_as_micros", i64_ty),
                "as_millis" => ("gos_rt_duration_as_millis", i64_ty),
                "as_secs" => ("gos_rt_duration_as_secs", i64_ty),
                "as_secs_f64" => (
                    "gos_rt_duration_as_secs_f64",
                    self.tcx.float_ty(gossamer_types::FloatTy::F64),
                ),
                _ => return MethodLowering::Pass,
            },
            (TyKind::Instant, "elapsed_ms", 0) => ("gos_rt_instant_elapsed_ms", i64_ty),
            (TyKind::Instant, "elapsed", 0) => ("gos_rt_instant_elapsed", self.tcx.duration_ty()),
            (TyKind::Instant, "duration_since", 1) => {
                ("gos_rt_instant_duration_since", self.tcx.duration_ty())
            }
            _ => return MethodLowering::Pass,
        };
        let Some(recv_local) = self.lower_expr(receiver) else {
            return MethodLowering::Handled(None);
        };
        let recv_local = self.auto_deref_cell(recv_local, span);
        let mut call_args = vec![Operand::Copy(Place::local(recv_local))];
        for arg in args {
            let Some(local) = self.lower_expr(arg) else {
                return MethodLowering::Handled(None);
            };
            call_args.push(Operand::Copy(Place::local(local)));
        }
        let dest = self.fresh(ret);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(sym.to_string())),
            args: call_args,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        MethodLowering::Handled(Some(dest))
    }

    /// `h.join()` - block on a spawned goroutine's outcome.
    pub(super) fn lower_join_handle_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        // `h.join()` - block on a spawned goroutine's outcome.
        // `gos_rt_join` recvs the SpawnOutcome over the handle's
        // one-shot channel and packs it into `Result<T, String>` (Ok
        // value, or Err panic message). Gated on a `JoinHandle`
        // receiver so a same-named user method or the string / Vec
        // `.join(sep)` (which takes a separator argument) is never
        // shadowed. Peek the receiver type first so the receiver is
        // lowered only when this arm actually consumes it.
        if method.name.as_str() == "join"
            && args.is_empty()
            && self
                .peek_struct_type(receiver)
                .is_some_and(|t| matches!(self.tcx.kind_of(t), TyKind::JoinHandle(_)))
        {
            let Some(recv_local) = self.lower_expr(receiver) else {
                return MethodLowering::Handled(None);
            };
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let elem = match self.tcx.kind_of(recv_ty).clone() {
                TyKind::JoinHandle(e) => e,
                _ => self.tcx.int_ty(gossamer_types::IntTy::I64),
            };
            let result_ty = self.result_payload_string_error_ty(elem);
            let dest = self.fresh(result_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_join".to_string())),
                args: vec![Operand::Copy(Place::local(recv_local))],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return MethodLowering::Handled(Some(dest));
        }
        MethodLowering::Pass
    }

    /// `.clone()` on a `json::Value` receiver - identity copy keeping the tag.
    pub(super) fn lower_json_clone_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        // `.clone()` on a `json::Value` receiver. The generic
        // identity-copy arm walks `match self.tcx.kind_of(ty)` and
        // falls through to `_ =>` for `JsonValue`, then the MIR
        // receiver-kind probe defaulted to `receiver_ty` (a `Var`
        // for chained accesses like `tcs[k].clone()`). The cloned
        // local then lost its `JsonValue` tag and downstream
        // `json::get(&clone_local, ...)` missed the json runtime
        // helper, returning the empty string. Short-circuit clone
        // on a JsonValue receiver to a direct copy with the
        // receiver's MIR type preserved.
        // `.clone()` on a `json::Value` receiver short-circuits to a
        // direct copy with the receiver's MIR type preserved (the
        // generic identity-copy arm later falls through to a Var dest
        // for `tcs[k].clone()` shapes and downstream json helpers stop
        // dispatching). Only lower the receiver when we know we'll
        // consume it here - falling through after the lower would
        // leave behind the receiver's lowered Call as dead but live
        // MIR, and any heap-container result (e.g. `gos_rt_vec_get_i64`
        // producing a `Vec<T>`-typed dest) would be marked twice for
        // `gos_rt_vec_free`, producing a double free at scope end.
        // A query on a parsed document is the same operation whichever way
        // it is spelled, so `doc.keys()` lowers exactly as
        // `json::keys(doc)` does - one helper, one return type, both tiers.
        if self.is_json_value_ty(receiver.ty)
            && matches!(
                method.name.as_str(),
                "get"
                    | "at"
                    | "keys"
                    | "len"
                    | "is_null"
                    | "as_str"
                    | "as_i64"
                    | "as_u64"
                    | "as_f64"
                    | "as_bool"
                    | "as_array"
                    | "render"
                    | "encode"
                    | "encode_pretty"
            )
        {
            let mut query_args = Vec::with_capacity(args.len() + 1);
            query_args.push(receiver.clone());
            query_args.extend(args.iter().cloned());
            if let Some(local) = self.lower_json_query(method.name.as_str(), &query_args, span) {
                return MethodLowering::Handled(Some(local));
            }
        }
        if method.name.as_str() == "clone" && args.is_empty() && self.is_json_value_ty(receiver.ty)
        {
            let Some(recv_local) = self.lower_expr(receiver) else {
                return MethodLowering::Handled(None);
            };
            let recv_mir_ty = self.locals[recv_local.0 as usize].ty;
            let dest = self.fresh(recv_mir_ty);
            if let Some(rk) = self.local_runtime_kind.get(&recv_local).copied() {
                self.local_runtime_kind.insert(dest, rk);
            }
            self.emit_assign(
                Place::local(dest),
                Rvalue::Use(Operand::Copy(Place::local(recv_local))),
                span,
            );
            return MethodLowering::Handled(Some(dest));
        }
        MethodLowering::Pass
    }

    /// `result.map(_)` / `map_err(_)` when the lowered receiver is a Result Adt.
    pub(super) fn lower_result_map_eager_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        // `result.map_err(closure)` / `result.map(closure)` when the
        // HIR receiver type is unresolved but its lowered MIR type
        // turns out to be a Result Adt. Without this short-circuit
        // the generic dispatch sees the unresolved kind, falls
        // through to the identity-copy arm, and silently drops the
        // mapping (errors.gos `text.parse().map_err(|_| …)?`
        // reproducer).
        if matches!(method.name.as_str(), "map_err" | "map") && args.len() == 1 {
            let receiver_ty_for_kind = self
                .receiver_local_from_path(receiver)
                .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
            if matches!(self.tcx.kind_of(receiver_ty_for_kind), TyKind::Var(_)) {
                if let Some(local) =
                    self.try_lower_result_map_with_eager_recv(receiver, method, &args[0], ty, span)
                {
                    return MethodLowering::Handled(Some(local));
                }
            }
        }
        MethodLowering::Pass
    }

    /// `let entries = m.iter()` on a map - materialise `Vec<(K, V)>`.
    pub(super) fn lower_hashmap_iter_binding_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        // `let entries = m.iter()` on a HashMap or BTreeMap - materialise a real
        // `Vec<(K, V)>` of entries. The `for (k, v) in m.iter()` form
        // is lowered earlier in `try_lower_for_hashmap_iter`; this
        // direct-binding form would otherwise fall through to the
        // generic `gos_rt_arr_iter` dispatch, reinterpret the
        // `*mut GosMap` as a `*mut GosVec`, and segfault on the
        // compiled tiers. Materialising here makes both forms behave
        // identically across the VM, Cranelift, and LLVM tiers.
        if method.name.as_str() == "iter" && args.is_empty() {
            let mut recv_ty_for_kind = self
                .receiver_local_from_path(receiver)
                .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
            // Peel `&` / `&mut` so `m.iter()` on a `&HashMap` parameter is
            // recognised as a map receiver and materialised; otherwise it falls
            // through to the generic `gos_rt_arr_iter` path, which reads the map
            // handle as a `*mut GosVec`. The handle the runtime helpers receive
            // is the same value `m.len()` / `m.get_or()` already pass through a
            // borrow, so only the receiver-type check needs the peel.
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv_ty_for_kind) {
                recv_ty_for_kind = *inner;
            }
            if matches!(self.tcx.kind_of(recv_ty_for_kind), TyKind::HashMap { .. }) {
                let entries = self.materialize_hashmap_entries(receiver, recv_ty_for_kind, span);
                // The pairs are read out under the map's lock, and the cursor
                // over them is what `iter()` answers: the value carries its own
                // position, so a downstream adapter runs per element pulled
                // rather than over the whole snapshot.
                let cursor = entries.and_then(|entries| self.entries_cursor(entries, span));
                return MethodLowering::Handled(cursor.or(entries));
            }
        }
        MethodLowering::Pass
    }

    /// `[].to_vec()` / `[a, b].to_vec()` on array-literal receivers.
    pub(super) fn lower_array_to_vec_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        // `[].to_vec()` - the empty-array literal carries no
        // element type, so the generic `gos_rt_vec_clone` arm
        // produces a `GosVec { elem_bytes: 0, … }`. Subsequent
        // `.push(t)` allocates `0 * cap` bytes for the data
        // buffer; `xs[0]` then reads through a bogus offset
        // and segfaults. Detect the empty-array shape and pin
        // the dest's `elem_bytes` from the call's HIR return
        // type (`Vec<T>`) by emitting a direct
        // `gos_rt_vec_new(elem_bytes_for_T)` instead.
        if method.name.as_str() == "to_vec"
            && args.is_empty()
            && let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(elems)) = &receiver.kind
            && elems.is_empty()
        {
            let mut peeled = ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                peeled = *inner;
            }
            let elem_ty_opt = match self.tcx.kind_of(peeled) {
                TyKind::Vec(elem) | TyKind::Slice(elem) => Some(*elem),
                TyKind::Array { elem, .. } => Some(*elem),
                _ => None,
            };
            if let Some(elem_ty) = elem_ty_opt {
                let elem_bytes = self.elem_bytes_of(elem_ty);
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let elem_bytes_local = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(elem_bytes_local),
                    Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(elem_bytes)))),
                    span,
                );
                let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
                let dest = self.fresh(vec_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_vec_new".to_string())),
                    args: vec![Operand::Copy(Place::local(elem_bytes_local))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return MethodLowering::Handled(Some(dest));
            }
        }
        // `[a, b, c].to_vec()` on a non-empty literal-array
        // receiver. The default `to_vec` arm lowers to
        // `gos_rt_vec_clone(receiver)`, but `gos_rt_vec_clone`
        // expects a real `*const GosVec` header (len/cap/
        // elem_bytes/ptr). The lowered receiver is a stack
        // `[T; N]` aggregate whose first 24 bytes are the raw
        // payload - `gos_rt_vec_clone` then reads `elems[0]` as
        // `len`, `elems[1]` as `cap`, etc. and either segfaults
        // or panics with a bogus `memory allocation of <huge>
        // bytes failed` when the runtime tries to copy that
        // many bytes. Detect the literal-array shape, lower the
        // elements normally, and route through the existing
        // `gos_rt_vec_from_arr(elem_bytes, &arr, len)` shim that
        // builds a real `GosVec` header around the stack
        // payload. Mirrors `coerce_arg_for_binding`'s `[T; N] →
        // Vec<T>` fix for binding calls.
        if method.name.as_str() == "to_vec"
            && args.is_empty()
            && let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(elems)) = &receiver.kind
            && !elems.is_empty()
        {
            let dest = self.fresh(ty);
            return if self.lower_let_array_as_vec(dest, elems, span) {
                MethodLowering::Handled(Some(dest))
            } else {
                MethodLowering::Handled(None)
            };
        }
        MethodLowering::Pass
    }

    /// `arr.swap(i, j)` / `xs.sort_by(closure)` in-place array operations.
    pub(super) fn lower_array_mutation_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        // `arr.swap(i, j)` super-instruction. The generic Call
        // fallback at the end of this function would lower this as
        // `Call(Const(Str("swap")), …)` which the cranelift backend
        // can't resolve - JIT- and AOT-compiled bodies silently
        // produced a typed-zero stub, leaving the receiver
        // unmutated. Inlining as four index ops (read i, read j,
        // write j-into-i, write i-into-j) keeps the semantics
        // intact across every backend.
        if method.name.as_str() == "swap" && args.len() == 2 {
            if let Some(swap_local) =
                self.try_lower_array_swap(receiver, &args[0], &args[1], ty, span)
            {
                return MethodLowering::Handled(Some(swap_local));
            }
        }
        if method.name.as_str() == "sort"
            && args.is_empty()
            && let Some(local) = self.try_lower_tuple_sort(receiver, span)
        {
            return MethodLowering::Handled(Some(local));
        }
        if matches!(method.name.as_str(), "sort" | "reverse")
            && args.is_empty()
            && let Some(local) =
                self.try_lower_fixed_array_ordering(receiver, method.name.as_str(), span)
        {
            return MethodLowering::Handled(Some(local));
        }
        if method.name.as_str() == "fill"
            && let [value] = args
            && let Some(local) = self.try_lower_sequence_fill(receiver, value, span)
        {
            return MethodLowering::Handled(Some(local));
        }
        if method.name.as_str() == "resize"
            && let [new_len, value] = args
            && let Some(local) = self.try_lower_vec_resize(receiver, new_len, value, span)
        {
            return MethodLowering::Handled(Some(local));
        }
        if matches!(
            method.name.as_str(),
            "copy_within" | "copy_from_slice" | "binary_search"
        ) && let Some(local) = self.try_lower_vec_bulk_method(receiver, method, args, span)
        {
            return MethodLowering::Handled(Some(local));
        }
        // `xs.sort_by(closure)` for `[i64; N]` / `[i64]` / `Vec<i64>`.
        // Routes through one of two runtime helpers depending on
        // the receiver shape: fixed buffers go through
        // `gos_rt_arr_sort_by_i64(ptr, len, env)`; Vec receivers
        // through `gos_rt_vec_sort_by_i64(vec, env)`. Both load the
        // closure body address from `env[0]` and forward the
        // `(env, *const T, *const T) -> i64` callback.
        if method.name.as_str() == "sort_by" && args.len() == 1 {
            if let Some(local) = self.try_lower_sort_by(receiver, &args[0], ty, span) {
                return MethodLowering::Handled(Some(local));
            }
        }
        MethodLowering::Pass
    }

    /// Map counter idioms: fused `insert(k, get_or+by)`, `inc`, struct-keyed ops.
    pub(super) fn lower_map_idiom_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        // Fused-increment peephole: `m.insert(k, m.get_or(k, 0)
        // + by)` (or `… + 1`) on an i64-keyed map collapses into
        // a single `gos_rt_map_inc_i64(m, k, by)` call. Halves
        // the lock + hash work on every counter-style loop.
        if method.name.as_str() == "insert" && args.len() == 2 {
            if let Some(local) = self.try_lower_map_inc(receiver, &args[0], &args[1], ty, span) {
                return MethodLowering::Handled(Some(local));
            }
        }
        // `m.inc(key)` / `m.inc(key, by)` for `HashMap<String, i64>`.
        // The interpreter ships a dedicated counter idiom; the
        // compiled tier needs a matching dispatch or values stay
        // at zero. Default `by` to 1 when only the key is given.
        if method.name.as_str() == "inc" && (args.len() == 1 || args.len() == 2) {
            let recv_ty_local = self
                .receiver_local_from_path(receiver)
                .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
            let val_kind = self.hash_map_value_kind(recv_ty_local);
            let key_kind = self.hash_map_key_kind(recv_ty_local);
            if std::env::var("GOS_DEBUG_FALLBACK").is_ok() {
                let (key_dbg, val_dbg) = match self.tcx.kind_of(recv_ty_local) {
                    gossamer_types::TyKind::HashMap { key, value, .. } => (
                        format!("{:?}", self.tcx.kind_of(*key)),
                        format!("{:?}", self.tcx.kind_of(*value)),
                    ),
                    other => (format!("{other:?}"), String::new()),
                };
                eprintln!(
                    "inc gate: key={key_dbg} value={val_dbg} val_is_i64={} key_str={} key_i64={}",
                    matches!(val_kind, Some(MapValueKind::I64)),
                    matches!(key_kind, Some(MapKeyKind::String)),
                    matches!(key_kind, Some(MapKeyKind::I64)),
                );
            }
            if matches!(val_kind, Some(MapValueKind::I64)) {
                let (fn_name, key_kind_ok) = match key_kind {
                    Some(MapKeyKind::String) => ("gos_rt_map_inc_typed_str_i64", true),
                    Some(MapKeyKind::I64) => ("gos_rt_map_inc_i64", true),
                    _ => ("", false),
                };
                if key_kind_ok {
                    let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                    let Some(recv_local) = self.lower_expr(receiver) else {
                        return MethodLowering::Handled(None);
                    };
                    let Some(key_local) = self.lower_expr(&args[0]) else {
                        return MethodLowering::Handled(None);
                    };
                    let by_local = if args.len() == 2 {
                        match self.lower_expr(&args[1]) {
                            Some(v) => v,
                            None => return MethodLowering::Handled(None),
                        }
                    } else {
                        let l = self.fresh(i64_ty);
                        self.emit_assign(
                            Place::local(l),
                            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
                            span,
                        );
                        l
                    };
                    let dest = self.fresh(i64_ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(fn_name.to_string())),
                        args: vec![
                            Operand::Copy(Place::local(recv_local)),
                            Operand::Copy(Place::local(key_local)),
                            Operand::Copy(Place::local(by_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return MethodLowering::Handled(Some(dest));
                }
            }
        }
        // Map operations on a HashMap keyed by a flat aggregate: hash the
        // key's content bytes (the VM value-keys; the compiled tier would
        // otherwise use the key's pointer and miss on a distinct allocation
        // of an equal value).
        if matches!(
            method.name.as_str(),
            "insert"
                | "get"
                | "contains_key"
                | "contains"
                | "pop"
                | "remove"
                | "get_or"
                | "or_insert"
                | "inc"
                | "__range"
        ) && let Some(local) =
            self.try_lower_struct_key_map_op(receiver, method.name.as_str(), args, span)
        {
            return MethodLowering::Handled(Some(local));
        }
        MethodLowering::Pass
    }

    /// `true` for the methods that rebind a `String` receiver in place.
    /// They own their receiver's write-back, so the receiver reaches them
    /// as the reference it was written as.
    pub(super) fn rebinds_receiver_place(method: &Ident) -> bool {
        matches!(
            method.name.as_str(),
            "push_str"
                | "push"
                | "push_char"
                | "push_byte"
                | "push_utf8"
                | "push_json_quoted"
                | "clear"
                | "truncate"
        )
    }

    /// The pointee type when `receiver` denotes a `&mut <scalar / String>`
    /// place - the shapes a reference addresses by slot rather than by
    /// value. `None` for every other receiver, including a shared `&T`,
    /// which already holds the value's own pointer.
    pub(super) fn mut_slot_receiver_pointee(&self, receiver: &HirExpr) -> Option<Ty> {
        let ref_ty = if matches!(self.tcx.kind_of(receiver.ty), TyKind::Ref { .. }) {
            receiver.ty
        } else {
            let local = self.receiver_local_from_path(receiver)?;
            self.locals[local.0 as usize].ty
        };
        self.mut_slot_pointee(ref_ty)
    }

    /// The place a receiver's value lives in: the slot a
    /// `&mut <scalar / String>` addresses, and the local itself for every
    /// other receiver. Reading and writing the same place keeps the rebind
    /// in the shape the drop schedule already reasons about - the value a
    /// consuming helper answers with moves into the place it came from.
    pub(super) fn receiver_slot_place(&self, local: Local) -> Place {
        if self.mut_slot_pointee_of_local(local).is_some() {
            Place {
                local,
                projection: vec![crate::ir::Projection::Deref],
            }
        } else {
            Place::local(local)
        }
    }

    /// `Some((string_ty, projected))` when `receiver` names a `String` place:
    /// a binding, the slot a `&mut String` addresses, or the field or element
    /// a projection reaches. `projected` marks the last of those, whose
    /// storage belongs to the aggregate holding it rather than to a local.
    pub(super) fn string_receiver_shape(&self, receiver: &HirExpr) -> Option<(Ty, bool)> {
        let peel = |this: &Self, mut ty: Ty| {
            while let TyKind::Ref { inner, .. } = this.tcx.kind_of(ty) {
                ty = *inner;
            }
            ty
        };
        if let Some(local) = self.receiver_local_from_path(receiver) {
            let ty = peel(self, self.locals[local.0 as usize].ty);
            return matches!(self.tcx.kind_of(ty), TyKind::String).then_some((ty, false));
        }
        if !matches!(
            receiver.kind,
            HirExprKind::Field { .. } | HirExprKind::TupleIndex { .. } | HirExprKind::Index { .. }
        ) {
            return None;
        }
        let ty = peel(self, receiver.ty);
        matches!(self.tcx.kind_of(ty), TyKind::String).then_some((ty, true))
    }

    /// The place [`Self::string_receiver_shape`] described, materialised.
    pub(super) fn string_receiver_place(&mut self, receiver: &HirExpr) -> Option<Place> {
        if let Some(local) = self.receiver_local_from_path(receiver) {
            return Some(self.receiver_slot_place(local));
        }
        let place = self.lower_place_expr(receiver)?;
        (!place.projection.is_empty()).then_some(place)
    }

    /// Mints the share a consuming string helper takes off a place whose
    /// storage the holding aggregate owns. The rebinding store gives the
    /// aggregate back a share of the replacement, so without this the helper
    /// would spend the one the aggregate still names.
    pub(super) fn retain_string_place(&mut self, place: &Place, span: Span) {
        let unit_ty = self.tcx.unit();
        let retained = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(retained),
            Rvalue::CallIntrinsic {
                name: "gos_rt_str_retain_typed",
                args: vec![Operand::Copy(place.clone())],
            },
            span,
        );
    }

    /// Pointee of a local typed `&mut <scalar / String>`.
    pub(super) fn mut_slot_pointee_of_local(&self, local: Local) -> Option<Ty> {
        self.mut_slot_pointee(self.locals[local.0 as usize].ty)
    }

    /// Pointee of `&mut T` when `T` is a shape a reference addresses by
    /// slot: a scalar, a `String`, or one of the transparent `i64` time
    /// newtypes. `None` for a shared reference or any other referent.
    pub(super) fn mut_slot_pointee(&self, ty: Ty) -> Option<Ty> {
        let TyKind::Ref {
            inner,
            mutability: gossamer_types::Mutbl::Mut,
        } = self.tcx.kind_of(ty)
        else {
            return None;
        };
        let inner = *inner;
        matches!(
            self.tcx.kind_of(inner),
            TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Duration
                | TyKind::Instant
        )
        .then_some(inner)
    }

    /// `s.push_str/push/push_char/push_byte(_)` on an owned `String` receiver.
    pub(super) fn lower_string_push_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        // `b.push_str(s)` on an owned `String` receiver. The runtime takes
        // ownership of the accumulator and may grow its unique buffer in
        // place, returning the replacement pointer for receiver writeback.
        // This is materially different from lowering to `__concat`: callers
        // using `String::with_capacity` (streaming JSON, encoders, log
        // builders) must not copy the whole prefix for every append.
        if method.name.as_str() == "push_str"
            && args.len() == 1
            && let Some((peeled, projected)) = self.string_receiver_shape(receiver)
        {
            {
                let literal_len = match &args[0].kind {
                    HirExprKind::Literal(gossamer_hir::HirLiteral::String(text)) => {
                        Some(text.len() as i128)
                    }
                    _ => None,
                };
                // `s.push_str(t.substring(a, b))` copies the slice straight
                // out of `t`; the substring is never built.
                if let Some(slice) = self.substring_append_piece(&args[0]) {
                    let Some(operands) = self.lower_substring_operands(slice, span) else {
                        return MethodLowering::Handled(None);
                    };
                    let Some(recv_place) = self.string_receiver_place(receiver) else {
                        return MethodLowering::Handled(None);
                    };
                    if projected {
                        self.retain_string_place(&recv_place, span);
                    }
                    let dest = self.fresh(peeled);
                    self.emit_substring_append(
                        Operand::Copy(recv_place.clone()),
                        Place::local(dest),
                        operands,
                        span,
                    );
                    self.emit_assign(
                        recv_place,
                        Rvalue::Use(Operand::Copy(Place::local(dest))),
                        span,
                    );
                    return MethodLowering::Handled(Some(self.lower_unit(span)));
                }
                // `s.push_str(n.to_string())` appends the scalar's text in
                // place; the `String` the argument names is never built.
                let (append, appended) = match self.fused_append_piece(&args[0]) {
                    Some((symbol, value)) if symbol != "gos_rt_str_concat_drop_a" => {
                        (symbol, value)
                    }
                    _ => ("gos_rt_str_concat_drop_a", &args[0]),
                };
                let Some(arg_local) = self.lower_expr(appended) else {
                    return MethodLowering::Handled(None);
                };
                let Some(recv_place) = self.string_receiver_place(receiver) else {
                    return MethodLowering::Handled(None);
                };
                if projected {
                    self.retain_string_place(&recv_place, span);
                }
                let dest = self.fresh(peeled);
                let next = self.new_block(span);
                let (callee, call_args) = match literal_len {
                    Some(len) => (
                        "gos_rt_str_append_bytes",
                        vec![
                            Operand::Copy(recv_place.clone()),
                            Operand::Copy(Place::local(arg_local)),
                            Operand::Const(ConstValue::Int(len)),
                        ],
                    ),
                    None => (
                        append,
                        vec![
                            Operand::Copy(recv_place.clone()),
                            Operand::Copy(Place::local(arg_local)),
                        ],
                    ),
                };
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(callee.to_string())),
                    args: call_args,
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                self.emit_assign(
                    recv_place,
                    Rvalue::Use(Operand::Copy(Place::local(dest))),
                    span,
                );
                return MethodLowering::Handled(Some(self.lower_unit(span)));
            }
        }
        // `s.push_utf8(buf, start, end)` appends a validated UTF-8 window of a
        // byte buffer straight onto the receiver's storage, and
        // `s.push_json_quoted(..)` appends it as a quoted JSON string. The shim
        // answers a carrier: the payload is the pointer the receiver takes,
        // and the discriminant is the `bool` the call evaluates to.
        let window_append = match method.name.as_str() {
            "push_utf8" => Some("gos_rt_str_push_utf8"),
            "push_json_quoted" => Some("gos_rt_str_push_json_quoted"),
            _ => None,
        };
        if let Some(window_append) = window_append
            && args.len() == 3
            && let Some((peeled, projected)) = self.string_receiver_shape(receiver)
        {
            {
                let mut lowered = Vec::with_capacity(3);
                for arg in args {
                    let Some(a) = self.lower_expr(arg) else {
                        return MethodLowering::Handled(None);
                    };
                    lowered.push(a);
                }
                let Some(recv_place) = self.string_receiver_place(receiver) else {
                    return MethodLowering::Handled(None);
                };
                if projected {
                    self.retain_string_place(&recv_place, span);
                }
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                // The shim answers the two-word carrier: the payload is the
                // pointer the receiver takes and the discriminant is the flag.
                let carrier_ty = self.result_of(peeled);
                let carrier = self.fresh(carrier_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(window_append.to_string())),
                    args: vec![
                        Operand::Copy(recv_place.clone()),
                        Operand::Copy(Place::local(lowered[0])),
                        Operand::Copy(Place::local(lowered[1])),
                        Operand::Copy(Place::local(lowered[2])),
                    ],
                    destination: Place::local(carrier),
                    target: Some(next),
                });
                self.set_current(next);
                let updated = self.fresh(peeled);
                self.emit_assign(
                    Place::local(updated),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_payload",
                        args: vec![Operand::Copy(Place::local(carrier))],
                    },
                    span,
                );
                self.emit_assign(
                    recv_place,
                    Rvalue::Use(Operand::Copy(Place::local(updated))),
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
                let bool_ty = self.tcx.bool_ty();
                let ok = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(ok),
                    Rvalue::BinaryOp {
                        op: crate::ir::BinOp::Eq,
                        lhs: Operand::Copy(Place::local(disc)),
                        rhs: Operand::Const(ConstValue::Int(0)),
                    },
                    span,
                );
                return MethodLowering::Handled(Some(ok));
            }
        }
        // `s.push(ch)` on an owned `String` receiver uses the growable-string
        // mutation helper. It consumes the old receiver reference, mutates a
        // unique buffer when capacity permits, and returns the pointer to
        // write back. The unqualified `push` arm below routes Vec receivers
        // to `gos_rt_vec_push`; this block claims only String ones.
        if method.name.as_str() == "push"
            && args.len() == 1
            && let Some((peeled, projected)) = self.string_receiver_shape(receiver)
        {
            {
                let Some(arg_local) = self.lower_expr(&args[0]) else {
                    return MethodLowering::Handled(None);
                };
                let Some(recv_place) = self.string_receiver_place(receiver) else {
                    return MethodLowering::Handled(None);
                };
                if projected {
                    self.retain_string_place(&recv_place, span);
                }
                let dest = self.fresh(peeled);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_str_push_char".to_string())),
                    args: vec![
                        Operand::Copy(recv_place.clone()),
                        Operand::Copy(Place::local(arg_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                self.emit_assign(
                    recv_place,
                    Rvalue::Use(Operand::Copy(Place::local(dest))),
                    span,
                );
                return MethodLowering::Handled(Some(self.lower_unit(span)));
            }
        }
        // `s.push_char(c)` on a String receiver. Same receiver-rebind
        // contract as `push`; dispatches to `gos_rt_str_push_char` which
        // interprets the argument as a Unicode codepoint.
        if method.name.as_str() == "push_char"
            && args.len() == 1
            && let Some((peeled, projected)) = self.string_receiver_shape(receiver)
        {
            {
                let Some(arg_local) = self.lower_expr(&args[0]) else {
                    return MethodLowering::Handled(None);
                };
                let Some(recv_place) = self.string_receiver_place(receiver) else {
                    return MethodLowering::Handled(None);
                };
                if projected {
                    self.retain_string_place(&recv_place, span);
                }
                let dest = self.fresh(peeled);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_str_push_char".to_string())),
                    args: vec![
                        Operand::Copy(recv_place.clone()),
                        Operand::Copy(Place::local(arg_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                self.emit_assign(
                    recv_place,
                    Rvalue::Use(Operand::Copy(Place::local(dest))),
                    span,
                );
                return MethodLowering::Handled(Some(self.lower_unit(span)));
            }
        }
        // `s.push_byte(b)` on a String receiver. Same receiver-rebind
        // contract as `push`; dispatches to `gos_rt_str_push_byte` which
        // interprets the argument as a raw byte value.
        if method.name.as_str() == "push_byte"
            && args.len() == 1
            && let Some((peeled, projected)) = self.string_receiver_shape(receiver)
        {
            {
                let Some(arg_local) = self.lower_expr(&args[0]) else {
                    return MethodLowering::Handled(None);
                };
                let Some(recv_place) = self.string_receiver_place(receiver) else {
                    return MethodLowering::Handled(None);
                };
                if projected {
                    self.retain_string_place(&recv_place, span);
                }
                let dest = self.fresh(peeled);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_str_push_byte".to_string())),
                    args: vec![
                        Operand::Copy(recv_place.clone()),
                        Operand::Copy(Place::local(arg_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                self.emit_assign(
                    recv_place,
                    Rvalue::Use(Operand::Copy(Place::local(dest))),
                    span,
                );
                return MethodLowering::Handled(Some(self.lower_unit(span)));
            }
        }
        // `s.clear()` / `s.truncate(n)` on a String receiver. Same
        // receiver-rebind contract as the push family: the runtime consumes
        // the receiver, shortening it in place when it holds the buffer alone,
        // and the answer is written back.
        if matches!(method.name.as_str(), "clear" | "truncate")
            && (method.name.as_str() == "clear" && args.is_empty()
                || method.name.as_str() == "truncate" && args.len() == 1)
            && let Some((peeled, projected)) = self.string_receiver_shape(receiver)
        {
            {
                let arg_local = if method.name.as_str() == "truncate" {
                    let Some(arg_local) = self.lower_expr(&args[0]) else {
                        return MethodLowering::Handled(None);
                    };
                    Some(arg_local)
                } else {
                    None
                };
                let Some(recv_place) = self.string_receiver_place(receiver) else {
                    return MethodLowering::Handled(None);
                };
                if projected {
                    self.retain_string_place(&recv_place, span);
                }
                let mut call_args = vec![Operand::Copy(recv_place.clone())];
                let rt = match arg_local {
                    Some(arg_local) => {
                        call_args.push(Operand::Copy(Place::local(arg_local)));
                        "gos_rt_str_truncate"
                    }
                    None => "gos_rt_str_clear",
                };
                let dest = self.fresh(peeled);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(rt.to_string())),
                    args: call_args,
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                self.emit_assign(
                    recv_place,
                    Rvalue::Use(Operand::Copy(Place::local(dest))),
                    span,
                );
                return MethodLowering::Handled(Some(self.lower_unit(span)));
            }
        }
        MethodLowering::Pass
    }

    /// Ground-truth type of a `<parent>.<field>` receiver whose `parent` is a
    /// bound local of struct type: the field's declared type from the struct
    /// definition, with the instantiation's generic arguments applied. The HIR
    /// type of such a field access can be left degraded - a match-payload
    /// binding (`match r { Ok(m) => m.tags... }`) loses the field's generic
    /// substitution - which map key/value dispatch then reads as `i64`, sending
    /// `HashMap<String, _>` accessors to the integer-keyed helpers. The parent
    /// struct's declared field type is ground truth. `None` for non-field
    /// receivers, an unresolvable parent, or a non-concrete declared type (a
    /// generic template's rigid `Param`, where the HIR type is more specific).
    pub(crate) fn field_declared_ty(&self, receiver: &HirExpr) -> Option<Ty> {
        let HirExprKind::Field {
            receiver: parent,
            name: field,
        } = &receiver.kind
        else {
            return None;
        };
        let parent_local = self.receiver_local_from_path(parent)?;
        let mut pty = self.locals[parent_local.0 as usize].ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(pty) {
            pty = *inner;
        }
        let TyKind::Adt { def, substs } = self.tcx.kind_of(pty).clone() else {
            return None;
        };
        let sname = self.struct_defs.get(&def).cloned()?;
        let order = self.structs.get(&sname).cloned()?;
        let pos = order.iter().position(|f| f == &field.name)?;
        let field_ty = self
            .tcx
            .adt_field_tys(def, &substs)
            .and_then(|t| t.get(pos).copied())?;
        if matches!(
            self.tcx.kind_of(field_ty),
            TyKind::Var(_) | TyKind::Error | TyKind::Param { .. }
        ) {
            return None;
        }
        Some(field_ty)
    }

    /// Recover the receiver's flattened dispatch kind and its ground type.
    pub(super) fn receiver_dispatch_kinds(&mut self, receiver: &HirExpr) -> (Ty, TyKind) {
        let mut receiver_ty = self
            .receiver_local_from_path(receiver)
            .map_or(receiver.ty, |local| self.locals[local.0 as usize].ty);
        let receiver_kind = self.tcx.kind_of(receiver_ty).clone();
        // Unwrap a leading `&T` so `s.len()` on a `&String`
        // parameter lowers the same as on an owned `String`.
        let mut receiver_kind_flat = match &receiver_kind {
            TyKind::Ref { inner, .. } => self.tcx.kind_of(*inner).clone(),
            other => other.clone(),
        };
        // `(*flags.<long>).method(...)` - the HIR receiver type is
        // an unresolved inference variable, but the underlying cell
        // kind is known statically from `local_define_layout`.
        // Promote the receiver kind so method dispatch (`to_string`,
        // `len`, …) picks the right runtime helper.
        if matches!(receiver_kind_flat, TyKind::Var(_)) {
            if let Some(kind) = self.peek_define_deref_kind(receiver) {
                receiver_kind_flat = kind;
            }
        }
        // `<chain>.method().to_string()` - when the chain ends in
        // a call whose return shape is pinned (`len`, `parse`,
        // `to_string`, integer-yielding helpers), surface that
        // shape so downstream `.to_string()` dispatches through
        // the i64/f64 runtime formatter instead of the
        // identity-copy arm. Expression-only walk; emits no MIR.
        if matches!(receiver_kind_flat, TyKind::Var(_)) {
            if let Some(kind) = self.peek_method_chain_kind(receiver) {
                receiver_kind_flat = kind;
            }
        }
        // `r.query.len()` / other methods on a field of an opaque
        // runtime-kind struct (`http::Request` / `http::Response`):
        // the field expression's HIR type is an inference Var (the
        // structs are checker-opaque), but the field-accessor table
        // knows the static type. Without this, `.len()` falls to the
        // len-prefixed `gos_rt_len` and dereferences a c-string -
        // a misaligned-pointer abort on the first proxied request.
        if matches!(receiver_kind_flat, TyKind::Var(_))
            && let HirExprKind::Field {
                receiver: obj,
                name: fname,
            } = &receiver.kind
            && let Some(rk) = self
                .receiver_local_from_path(obj)
                .and_then(|l| self.local_runtime_kind.get(&l).copied())
                .or_else(|| self.expr_runtime_kind(obj))
            && let Some(field_ty) = self.runtime_field_static_ty(rk, fname.name.as_str())
        {
            receiver_kind_flat = self.tcx.kind_of(field_ty).clone();
        }
        // `args[i].method()` - when typeck resolves the Index
        // expression to its base collection (Vec / Slice / Array)
        // instead of the element type (a multi-module typeck
        // regression: single-file builds correctly type
        // `args[i]` as `String`, but with `mod util;` the HIR
        // node retains the base `Vec<String>`), prefer the
        // element kind taken from the base local's MIR type.
        // Without this, `.len()` on `args[i]` lands on the
        // collection arm `gos_rt_arr_len`, which then crashes
        // inside `mov (%rdi),%rax` reading a Vec header out of a
        // `*const c_char` string pointer.
        let needs_index_fixup = matches!(
            receiver_kind_flat,
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } | TyKind::Var(_)
        );
        if needs_index_fixup {
            if let HirExprKind::Index { base, .. } = &receiver.kind {
                if let Some(base_ty) = self.peek_collection_type(base) {
                    let elem_ty = match self.tcx.kind_of(base_ty) {
                        TyKind::Vec(elem) | TyKind::Slice(elem) => Some(*elem),
                        TyKind::Array { elem, .. } => Some(*elem),
                        _ => None,
                    };
                    if let Some(elem_ty) = elem_ty {
                        let mut elem_kind = self.tcx.kind_of(elem_ty).clone();
                        while let TyKind::Ref { inner, .. } = elem_kind {
                            elem_kind = self.tcx.kind_of(inner).clone();
                        }
                        if !matches!(elem_kind, TyKind::Var(_)) {
                            receiver_kind_flat = elem_kind;
                        }
                    }
                }
            }
        }

        // `<recv>.<field>.method()` - the field-access HIR type can
        // be wrongly resolved to `String` (e.g. a `match Ok(q) =>
        // q.bytes.len()` binding where the field came back as
        // `String` instead of `[u8]`, sending `.len()` to strlen and
        // reading the i64-per-element Vec as a c-string), or left with
        // a degraded generic substitution (a `HashMap<String, _>`
        // field reached through a match binding, whose key/value substs
        // are lost). The parent struct's *declared* field type is
        // ground truth - recover the full type so both the receiver
        // kind AND `receiver_ty` (which map key/value dispatch reads the
        // substitution from) are correct. Ungated (the HIR type may be
        // concrete-but-wrong, not just `Var`).
        if let Some(field_ty) = self.field_declared_ty(receiver) {
            receiver_ty = field_ty;
            let mut k = self.tcx.kind_of(field_ty).clone();
            while let TyKind::Ref { inner, .. } = k {
                k = self.tcx.kind_of(inner).clone();
            }
            receiver_kind_flat = k;
        }
        (receiver_ty, receiver_kind_flat)
    }

    pub(super) fn stdlib_runtime_kind_from_kind(kind: &TyKind) -> Option<&'static str> {
        match kind {
            TyKind::Adt { def, .. } if def.local == HASH_SET_DEF_LOCAL => {
                Some("collections::HashSet")
            }
            TyKind::Adt { def, .. } if def.local == BTREE_SET_DEF_LOCAL => {
                Some("collections::BTreeSet")
            }
            TyKind::Adt { def, .. } if def.local == BINARY_HEAP_DEF_LOCAL => {
                Some("collections::MaxHeap")
            }
            TyKind::Adt { def, .. } if def.local == MIN_HEAP_DEF_LOCAL => {
                Some("collections::MinHeap")
            }
            TyKind::Adt { def, .. } if def.local == VEC_QUEUE_DEF_LOCAL => {
                Some("collections::VecQueue")
            }
            TyKind::Adt { def, .. } if def.local == VEC_STACK_DEF_LOCAL => {
                Some("collections::VecStack")
            }
            TyKind::Adt { def, .. } if def.local == VALIDATE_ERRORS_DEF_LOCAL => {
                Some("validate::Errors")
            }
            TyKind::Adt { def, .. } if def.local == VALIDATE_FIELD_ERROR_DEF_LOCAL => {
                Some("validate::FieldError")
            }
            // A handle that arrives as a parameter carries no construction
            // site to recover the kind from, so the sentinel itself has to
            // answer it - otherwise `fn f(p: &regex::Pattern)` dispatches by
            // bare name, which the VM has and the compiled tiers do not.
            TyKind::Adt { def, .. } if def.local == u32::MAX - 26 => Some("regex::Pattern"),
            _ => None,
        }
    }

    /// Fold `recv.headers.insert/get(...)` into a single runtime header call.
    pub(super) fn lower_headers_fold_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        if let HirExprKind::Field {
            receiver: inner,
            name: field_name,
        } = &receiver.kind
        {
            if field_name.name.as_str() == "headers" {
                let inner_local_for_kind = self.receiver_local_from_path(inner);
                let inner_kind = inner_local_for_kind
                    .and_then(|l| self.local_runtime_kind.get(&l).copied())
                    .or_else(|| {
                        let inner_ty =
                            inner_local_for_kind.map_or(inner.ty, |l| self.locals[l.0 as usize].ty);
                        match self.tcx.kind_of(inner_ty) {
                            TyKind::Ref { inner: i, .. } => self.struct_name_of(*i),
                            _ => self.struct_name_of(inner_ty),
                        }
                        .and_then(|s| match s.as_str() {
                            "Response" => Some("http::Response"),
                            "Request" => Some("http::Request"),
                            _ => None,
                        })
                    });
                if matches!(inner_kind, Some("http::Response" | "http::Request")) {
                    let helper = match (inner_kind, method.name.as_str()) {
                        (Some("http::Response"), "insert") => {
                            Some(("gos_rt_http_response_set_header", self.tcx.unit(), 2usize))
                        }
                        (Some("http::Response"), "get") => Some((
                            "gos_rt_http_response_get_header",
                            self.tcx.string_ty(),
                            1usize,
                        )),
                        (Some("http::Request"), "insert") => {
                            Some(("gos_rt_http_request_set_header", self.tcx.unit(), 2usize))
                        }
                        (Some("http::Request"), "get") => Some((
                            "gos_rt_http_request_get_header",
                            self.tcx.string_ty(),
                            1usize,
                        )),
                        _ => None,
                    };
                    if let Some((rt, ret_ty, want_args)) = helper {
                        if args.len() == want_args {
                            let Some(inner_local) = self.lower_expr(inner) else {
                                return MethodLowering::Handled(None);
                            };
                            let mut ops = Vec::with_capacity(args.len() + 1);
                            ops.push(Operand::Copy(Place::local(inner_local)));
                            for a in args {
                                let Some(al) = self.lower_expr(a) else {
                                    return MethodLowering::Handled(None);
                                };
                                ops.push(Operand::Copy(Place::local(al)));
                            }
                            let dest = self.fresh(ret_ty);
                            let next = self.new_block(span);
                            self.terminate(Terminator::Call {
                                callee: Operand::Const(ConstValue::Str(rt.to_string())),
                                args: ops,
                                destination: Place::local(dest),
                                target: Some(next),
                            });
                            self.set_current(next);
                            return MethodLowering::Handled(Some(dest));
                        }
                    }
                }
            }
        }
        MethodLowering::Pass
    }

    /// `.len()` on a fixed-size `[T; N]` array - a compile-time constant.
    pub(super) fn lower_fixed_array_len_method(
        &mut self,
        method: &Ident,
        args: &[HirExpr],
        receiver_kind_flat: &TyKind,
        span: Span,
    ) -> MethodLowering {
        let receiver_kind_flat = receiver_kind_flat.clone();
        // A tuple's element count is part of its type, exactly like a fixed
        // array's length, so both fold to a constant here. Without the
        // tuple arm the dispatch fell through to the generic vec-header
        // read, which reported 1.
        let constant_len = match &receiver_kind_flat {
            TyKind::Array { len, .. } => Some(len.to_usize()),
            TyKind::Tuple(elems) => Some(elems.len()),
            _ => None,
        };
        if method.name.as_str() == "len"
            && args.is_empty()
            && let Some(n) = constant_len
        {
            let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            let dest = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(dest),
                Rvalue::Use(Operand::Const(ConstValue::Int(n as i128))),
                span,
            );
            return MethodLowering::Handled(Some(dest));
        }
        MethodLowering::Pass
    }

    /// True when the receiver's own type declares `method` in an `impl`.
    ///
    /// Keyed by the type's registered identity, so a method on a type
    /// declared inside a module is found under the same name its `impl`
    /// registered.
    pub(super) fn user_impl_method_exists(
        &self,
        receiver_ty: Ty,
        receiver: &HirExpr,
        method: &str,
    ) -> bool {
        [
            self.adt_dispatch_name(receiver_ty),
            self.struct_name_from_expr(receiver),
            self.builtin_impl_owner_name(receiver_ty),
        ]
        .into_iter()
        .flatten()
        .any(|owner| {
            // A core type's own surface answers before an `impl` block that
            // spells one of its names, so such a block adds names to the type
            // rather than replacing any it already had. A declared type has no
            // surface of its own here, and a primitive's is not one an `impl`
            // can collide with, so both keep the block they name.
            !gossamer_types::core_type_declares_method(&owner, method)
                && self
                    .impl_methods
                    .contains_key(&format!("{owner}::{method}"))
        })
    }
}
