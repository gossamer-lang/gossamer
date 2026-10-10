//! Lazy iterator sources and map traversals.

use super::*;

impl<'a> Builder<'a> {
    pub(super) fn lower_lazy_iter_source_classed(
        &mut self,
        arg: &HirExpr,
        allow_aggr: bool,
    ) -> Option<Local> {
        let (local, family) = self.lower_iter_seq_arg_raw(arg)?;
        if let Some(family) = family {
            if family == LazyElemFamily::Aggr && !allow_aggr {
                return None;
            }
            return Some(local);
        }
        self.borrow_lazy_state(local, arg.span, allow_aggr)
            .map(|(h, _)| h)
    }

    pub(crate) fn try_lower_for_hashmap_iter(
        &mut self,
        for_loop: &ForLoopShape<'_>,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let HirExprKind::MethodCall { receiver, name, .. } = &for_loop.iter_expr.kind else {
            return None;
        };
        if name.name != "iter" {
            return None;
        }
        let mut recv_ty = self
            .receiver_local_from_path(receiver)
            .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
        // Peel `&` / `&mut` so `for (k, v) in m.iter()` over a `&HashMap`
        // parameter is recognised as a map receiver; otherwise it falls through
        // to the generic for-vec path, which reads the map handle as a Vec. The
        // downstream key / value kind helpers already peel, and the receiver
        // handle matches what `m.len()` / `m.get_or()` pass through a borrow.
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv_ty) {
            recv_ty = *inner;
        }
        if !matches!(self.tcx.kind_of(recv_ty), TyKind::HashMap { .. }) {
            return None;
        }
        self.lower_for_hashmap_pairs(receiver, recv_ty, for_loop, span)
    }

    pub(crate) fn try_lower_for_bare_hashmap_iter(
        &mut self,
        receiver: &HirExpr,
        for_loop: &ForLoopShape<'_>,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let mut recv_ty = self
            .receiver_local_from_path(receiver)
            .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv_ty) {
            recv_ty = *inner;
        }
        if !matches!(self.tcx.kind_of(recv_ty), TyKind::HashMap { .. }) {
            return None;
        }
        self.lower_for_hashmap_pairs(receiver, recv_ty, for_loop, span)
    }

    pub(super) fn lower_for_hashmap_pairs(
        &mut self,
        receiver: &HirExpr,
        recv_ty: Ty,
        for_loop: &ForLoopShape<'_>,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let HirPatKind::Tuple(elems) = &for_loop.loop_pat.kind else {
            return None;
        };
        if elems.len() != 2 {
            return None;
        }
        // Accept a `Binding` (bind the name) or `_` (read the slot but
        // bind no name) in either tuple position. The key is read either
        // way - the value lookup needs it - so a `_` key just suppresses
        // its user-visible binding. A literal or nested sub-pattern is
        // left to the generic for-vec path.
        let key_binding = match &elems[0].kind {
            HirPatKind::Binding { name, mutable } => Some((name.clone(), *mutable)),
            HirPatKind::Wildcard => None,
            _ => return None,
        };
        let val_binding = match &elems[1].kind {
            HirPatKind::Binding { name, mutable } => Some((name.clone(), *mutable)),
            HirPatKind::Wildcard => None,
            _ => return None,
        };
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let str_ty = self.tcx.string_ty();
        // An aggregate-keyed map stores its keys as flat content bytes under a
        // slot descriptor, so the snapshot rebuilds each key and the entry
        // lookup goes through the same content-keyed helper the inserts used.
        let skey = self
            .hash_map_kv_tys(recv_ty)
            .filter(|(key, _)| self.is_aggregate_key(*key))
            .and_then(|(key, _)| self.key_descriptor(key).map(|desc| (key, desc)));
        if let Some((key_struct_ty, descriptor)) = skey {
            return self.lower_for_skey_pairs(
                receiver,
                recv_ty,
                key_struct_ty,
                KeyedPairs::Aggregate(descriptor),
                key_binding,
                val_binding,
                for_loop,
                span,
            );
        }
        // A payload enum key is stored under its canonical bytes beside the
        // node itself, so the snapshot hands back the nodes and each value is
        // looked up through the enum's equality descriptor.
        if let Some((key_ty, _)) = self.hash_map_kv_tys(recv_ty)
            && self.struct_name_of(key_ty).is_none()
            && let Some(descriptor) = self.ensure_enum_eq_desc(key_ty)
        {
            return self.lower_for_skey_pairs(
                receiver,
                recv_ty,
                key_ty,
                KeyedPairs::Enum(descriptor),
                key_binding,
                val_binding,
                for_loop,
                span,
            );
        }
        // A key the runtime cannot rebuild cannot drive the loop. When the key
        // is unused (`_`) and the values are scalar, iterate the live values
        // directly - matching the VM, which yields each entry's value.
        if key_binding.is_none()
            && matches!(self.hash_map_key_kind(recv_ty), Some(MapKeyKind::Other))
            && matches!(self.hash_map_value_kind(recv_ty), Some(MapValueKind::I64))
        {
            return self.lower_for_struct_keyed_values(receiver, for_loop.loop_pat, for_loop, span);
        }
        let (key_ty, val_ty, keys_helper, get_or_helper) = {
            let key_kind = self.hash_map_key_kind(recv_ty);
            let value_kind = self.hash_map_value_kind(recv_ty);
            let key_ty = match key_kind {
                Some(MapKeyKind::String) => str_ty,
                _ => i64_ty,
            };
            let val_ty = match value_kind {
                Some(MapValueKind::String) => str_ty,
                // A struct value is stored as a boxed pointer; bind `v`
                // as a reference (a single box-pointer word) so field
                // access derefs the box. Typing the binding as the
                // by-value struct makes the drop pass treat the blob
                // pointer as an inline struct and release its RC fields
                // — a use-after-free (and on Windows a misaligned-RC
                // crash) once the map's own share is later released.
                // Non-struct aggregates keep the value type. Mirrors the
                // `for v in m.values()` binding in
                // [`try_lower_for_hashmap_iter`]'s sibling in ctrl.rs.
                Some(MapValueKind::Other) => {
                    let value_struct = self
                        .hash_map_kv_tys(recv_ty)
                        .map(|(_, v)| v)
                        .filter(|v| self.struct_name_of(*v).is_some());
                    match value_struct {
                        Some(v) => self.tcx.intern(TyKind::Ref {
                            mutability: gossamer_types::Mutbl::Not,
                            inner: v,
                        }),
                        None => self.hash_map_kv_tys(recv_ty).map_or(i64_ty, |(_, v)| v),
                    }
                }
                _ => i64_ty,
            };
            let keys_helper = match key_kind {
                Some(MapKeyKind::String) => "gos_rt_map_keys_str",
                _ if self.map_keys_unsigned(recv_ty) => "gos_rt_map_keys_u64",
                _ => "gos_rt_map_keys_i64",
            };
            let get_or_helper = match (key_kind, value_kind) {
                (Some(MapKeyKind::String), Some(MapValueKind::String)) => {
                    "gos_rt_map_get_or_str_str"
                }
                (Some(MapKeyKind::String), _) => "gos_rt_map_get_or_typed_str_i64",
                (_, Some(MapValueKind::String)) => "gos_rt_map_get_or_i64_str",
                _ => "gos_rt_map_get_or_i64",
            };
            (key_ty, val_ty, keys_helper, get_or_helper)
        };

        // A key and a value that each fit one word are read out of pairs the
        // runtime sorts once, with no lookup per entry, and each is bound as the
        // type the map declares: a scalar's slot holds its own bits.
        let pairs = match (
            self.hash_map_key_kind(recv_ty),
            self.hash_map_value_kind(recv_ty),
            self.hash_map_kv_tys(recv_ty),
        ) {
            (
                Some(MapKeyKind::String | MapKeyKind::I64),
                Some(MapValueKind::String | MapValueKind::I64),
                Some((declared_key, declared_val)),
            ) => {
                let pair_key = if key_ty == str_ty {
                    str_ty
                } else {
                    declared_key
                };
                let pair_val = if val_ty == str_ty {
                    str_ty
                } else {
                    declared_val
                };
                let pair = self.tcx.intern(TyKind::Tuple(vec![pair_key, pair_val]));
                (self.elem_bytes_of(pair) == 16).then_some((pair, pair_key, pair_val))
            }
            _ => None,
        };
        let (key_ty, val_ty) = pairs.map_or((key_ty, val_ty), |(_, k, v)| (k, v));

        let recv_local = self.lower_expr(receiver)?;
        let keys_vec = if let Some((pair, _, _)) = pairs {
            let elem_bytes = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(elem_bytes),
                Rvalue::Use(Operand::Const(ConstValue::Int(16))),
                span,
            );
            let pairs_ty = self.tcx.intern(TyKind::Vec(pair));
            let entries = self.emit_combinator_call_raw(
                "Vec::new",
                vec![Operand::Copy(Place::local(elem_bytes))],
                pairs_ty,
                span,
            );
            let unit_ty = self.tcx.unit();
            let entries_helper = if self.map_keys_unsigned(recv_ty) {
                "gos_rt_map_entries_into_u64"
            } else {
                "gos_rt_map_entries_into"
            };
            let _ = self.emit_combinator_call_raw(
                entries_helper,
                vec![
                    Operand::Copy(Place::local(recv_local)),
                    Operand::Copy(Place::local(entries)),
                ],
                unit_ty,
                span,
            );
            entries
        } else {
            let keys_vec_ty = self.tcx.intern(TyKind::Vec(key_ty));
            let keys_vec = self.fresh(keys_vec_ty);
            let after_keys = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(keys_helper.to_string())),
                args: vec![Operand::Copy(Place::local(recv_local))],
                destination: Place::local(keys_vec),
                target: Some(after_keys),
            });
            self.set_current(after_keys);
            keys_vec
        };

        let len_local = self.fresh(i64_ty);
        let after_len = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
            args: vec![Operand::Copy(Place::local(keys_vec))],
            destination: Place::local(len_local),
            target: Some(after_len),
        });
        self.set_current(after_len);

        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let step_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let bool_ty = self.tcx.bool_ty();
        let cmp = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(cmp),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(len_local)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cmp)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        self.push_scope();
        // ptr = gos_rt_vec_get_ptr(keys, counter); k = *ptr
        let ptr_local = self.fresh(i64_ty);
        let after_ptr = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_ptr".to_string())),
            args: vec![
                Operand::Copy(Place::local(keys_vec)),
                Operand::Copy(Place::local(counter)),
            ],
            destination: Place::local(ptr_local),
            target: Some(after_ptr),
        });
        self.set_current(after_ptr);
        let key_local = self.push_local(
            key_ty,
            key_binding.as_ref().map(|(n, _)| n.clone()),
            key_binding.as_ref().is_some_and(|(_, m)| *m),
        );
        if let Some((name, _)) = &key_binding {
            self.bind_local(&name.name, key_local);
        }
        let after_load = self.new_block(span);
        let zero_off = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(zero_off),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_load".to_string())),
            args: vec![
                Operand::Copy(Place::local(ptr_local)),
                Operand::Copy(Place::local(zero_off)),
            ],
            destination: Place::local(key_local),
            target: Some(after_load),
        });
        self.set_current(after_load);

        let val_local = self.push_local(
            val_ty,
            val_binding.as_ref().map(|(n, _)| n.clone()),
            val_binding.as_ref().is_some_and(|(_, m)| *m),
        );
        if let Some((name, _)) = &val_binding {
            self.bind_local(&name.name, val_local);
        }
        let after_val = self.new_block(span);
        if pairs.is_some() {
            // v = the pair's second word, read the way the key was.
            let value_off = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(value_off),
                Rvalue::Use(Operand::Const(ConstValue::Int(8))),
                span,
            );
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_load".to_string())),
                args: vec![
                    Operand::Copy(Place::local(ptr_local)),
                    Operand::Copy(Place::local(value_off)),
                ],
                destination: Place::local(val_local),
                target: Some(after_val),
            });
        } else {
            // v = m.get_or(k, default). Default-by-value-type: 0 for
            // i64-valued maps, an empty string for string-valued maps.
            let default_local = if val_ty == str_ty {
                let l = self.fresh(str_ty);
                self.emit_assign(
                    Place::local(l),
                    Rvalue::Use(Operand::Const(ConstValue::Str(String::new()))),
                    span,
                );
                l
            } else {
                let l = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(l),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    span,
                );
                l
            };
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(get_or_helper.to_string())),
                args: vec![
                    Operand::Copy(Place::local(recv_local)),
                    Operand::Copy(Place::local(key_local)),
                    Operand::Copy(Place::local(default_local)),
                ],
                destination: Place::local(val_local),
                target: Some(after_val),
            });
        }
        self.set_current(after_val);

        // Auto-region the body, exactly as the `for x in vec` path does: the
        // key/value bindings are read from the snapshot above (outside the
        // region), and only the body's per-iteration allocations are
        // bulk-freed at the iteration boundary. Eligibility rejects any
        // escape, so this can only speed the loop up, never change a result.
        let regioned = self.begin_loop_region(for_loop.body, span);
        // `continue` jumps to the counter bump, `break` leaves the loop.
        // Without this context the body's `break` / `continue` finds no
        // target: it reaches an outer loop's, or none at all.
        self.loop_stack.push(LoopContext {
            region: self.loop_region_slot(regioned),
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(for_loop.body);
        self.loop_stack.pop();
        self.pop_scope();
        self.end_auto_region(regioned, span);
        self.terminate(Terminator::Goto { target: step_block });

        self.set_current(step_block);
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let bumped = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(bumped),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Copy(Place::local(bumped))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        let unit_ty = self.tcx.unit();
        let unit = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(unit),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        Some(unit)
    }

    /// Lowers `for (k, v) in m.iter()` over an aggregate- or enum-keyed map.
    ///
    /// An aggregate key snapshot hands back the rebuilt aggregates as flat
    /// element slots, so the key binding observes each slot's address; an enum
    /// key snapshot hands back the nodes themselves. Either way the value comes
    /// from the same keyed lookup an explicit `m.get(k)` uses.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn lower_for_skey_pairs(
        &mut self,
        receiver: &HirExpr,
        recv_ty: Ty,
        key_struct_ty: Ty,
        keyed: KeyedPairs,
        key_binding: Option<(Ident, bool)>,
        val_binding: Option<(Ident, bool)>,
        for_loop: &ForLoopShape<'_>,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let val_ty = self.hash_map_kv_tys(recv_ty).map_or(i64_ty, |(_, v)| v);
        // The key binding names the element's storage, not a copy: an
        // aggregate slot is addressed in place, exactly as a struct-valued
        // binding is, so field reads deref the snapshot's own memory.
        let (key_ref_ty, keys_helper, key_reader, lookup_helper, descriptor) = match keyed {
            KeyedPairs::Aggregate(descriptor) => (
                self.tcx.intern(TyKind::Ref {
                    mutability: gossamer_types::Mutbl::Not,
                    inner: key_struct_ty,
                }),
                "gos_rt_map_keys_skey",
                "gos_rt_vec_get_ptr",
                "gos_rt_map_get_skey_opt",
                descriptor,
            ),
            KeyedPairs::Enum(descriptor) => (
                key_struct_ty,
                "gos_rt_map_keys_ekey",
                "gos_rt_vec_get_i64",
                "gos_rt_map_get_ekey_opt",
                descriptor,
            ),
        };
        let recv_local = self.lower_expr(receiver)?;
        let keys_vec_ty = self.tcx.intern(TyKind::Vec(key_struct_ty));
        let keys_vec = self.emit_combinator_call(
            keys_helper,
            vec![Operand::Copy(Place::local(recv_local))],
            keys_vec_ty,
            span,
        );
        let len_local = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(keys_vec))],
            i64_ty,
            span,
        );

        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let step_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let bool_ty = self.tcx.bool_ty();
        let cmp = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(cmp),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(len_local)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cmp)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        self.push_scope();
        let key_local = self.push_local(
            key_ref_ty,
            key_binding.as_ref().map(|(n, _)| n.clone()),
            key_binding.as_ref().is_some_and(|(_, m)| *m),
        );
        if let Some((name, _)) = &key_binding {
            self.bind_local(&name.name, key_local);
        }
        let after_ptr = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(key_reader.to_string())),
            args: vec![
                Operand::Copy(Place::local(keys_vec)),
                Operand::Copy(Place::local(counter)),
            ],
            destination: Place::local(key_local),
            target: Some(after_ptr),
        });
        self.set_current(after_ptr);

        // `m.get(k)` in its content-keyed form: an `Option<V>` whose payload
        // word is the stored value, so the binding takes the payload directly.
        let opt_ty = self.option_payload_adt_ty(val_ty);
        let entry = self.emit_combinator_call(
            lookup_helper,
            vec![
                Operand::Copy(Place::local(recv_local)),
                Operand::Copy(Place::local(key_local)),
                Operand::Const(ConstValue::Str(descriptor)),
            ],
            opt_ty,
            span,
        );
        let val_local = self.push_local(
            val_ty,
            val_binding.as_ref().map(|(n, _)| n.clone()),
            val_binding.as_ref().is_some_and(|(_, m)| *m),
        );
        if let Some((name, _)) = &val_binding {
            self.bind_local(&name.name, val_local);
        }
        let after_val = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_result_payload".to_string())),
            args: vec![Operand::Copy(Place::local(entry))],
            destination: Place::local(val_local),
            target: Some(after_val),
        });
        self.set_current(after_val);

        let regioned = self.begin_loop_region(for_loop.body, span);
        // `continue` jumps to the counter bump, `break` leaves the loop.
        // Without this context the body's `break` / `continue` finds no
        // target: it reaches an outer loop's, or none at all.
        self.loop_stack.push(LoopContext {
            region: self.loop_region_slot(regioned),
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(for_loop.body);
        self.loop_stack.pop();
        self.pop_scope();
        self.end_auto_region(regioned, span);
        self.terminate(Terminator::Goto { target: step_block });

        self.set_current(step_block);
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let bumped = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(bumped),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Copy(Place::local(bumped))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        let unit_ty = self.tcx.unit();
        let unit = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(unit),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        Some(unit)
    }

    /// Lowers `for (_, v) in m.iter()` over a struct / tuple-keyed map by
    /// driving the loop from the values snapshot. The key bytes a struct-keyed
    /// map stores are opaque (no `keys()` round-trip), so a key-driven loop
    /// would never iterate; reading the values directly yields each entry's
    /// value exactly as the VM's `m.iter()` does. The value binding's runtime
    /// kind selects the values helper and per-element getter.
    pub(super) fn lower_for_struct_keyed_values(
        &mut self,
        receiver: &HirExpr,
        loop_pat: &HirPat,
        for_loop: &ForLoopShape<'_>,
        span: Span,
    ) -> Option<Local> {
        let HirPatKind::Tuple(elems) = &loop_pat.kind else {
            return None;
        };
        let val_binding = match &elems[1].kind {
            HirPatKind::Binding { name, mutable } => Some((name.clone(), *mutable)),
            HirPatKind::Wildcard => None,
            _ => return None,
        };
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let str_ty = self.tcx.string_ty();
        let recv_ty = self
            .receiver_local_from_path(receiver)
            .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
        let (val_ty, values_helper, getter) = match self.hash_map_value_kind(recv_ty) {
            Some(MapValueKind::String) => (str_ty, "gos_rt_map_values_str", "gos_rt_vec_get_ptr"),
            Some(MapValueKind::Other) => {
                // Struct / aggregate values are boxed pointers in the snapshot.
                let v = self.hash_map_kv_tys(recv_ty).map_or(i64_ty, |(_, v)| v);
                (v, "gos_rt_map_values_vec", "gos_rt_vec_get_ptr")
            }
            _ => (
                i64_ty,
                if self.map_keys_unsigned(recv_ty) {
                    "gos_rt_map_values_u64"
                } else {
                    "gos_rt_map_values_i64"
                },
                "gos_rt_vec_get_i64_unchecked",
            ),
        };

        let recv_local = self.lower_expr(receiver)?;
        let vals_vec_ty = self.tcx.intern(gossamer_types::TyKind::Vec(val_ty));
        let vals_vec = self.fresh(vals_vec_ty);
        let after_vals = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(values_helper.to_string())),
            args: vec![Operand::Copy(Place::local(recv_local))],
            destination: Place::local(vals_vec),
            target: Some(after_vals),
        });
        self.set_current(after_vals);

        let len_local = self.fresh(i64_ty);
        let after_len = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
            args: vec![Operand::Copy(Place::local(vals_vec))],
            destination: Place::local(len_local),
            target: Some(after_len),
        });
        self.set_current(after_len);

        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let step_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let bool_ty = self.tcx.bool_ty();
        let cmp = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(cmp),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(len_local)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cmp)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        self.push_scope();
        let val_local = self.push_local(
            val_ty,
            val_binding.as_ref().map(|(n, _)| n.clone()),
            val_binding.as_ref().is_some_and(|(_, m)| *m),
        );
        if let Some((name, _)) = &val_binding {
            self.bind_local(&name.name, val_local);
        }
        let after_get = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(getter.to_string())),
            args: vec![
                Operand::Copy(Place::local(vals_vec)),
                Operand::Copy(Place::local(counter)),
            ],
            destination: Place::local(val_local),
            target: Some(after_get),
        });
        self.set_current(after_get);

        // Auto-region the body: the value binding is read from the values
        // snapshot above (outside the region), so only the body's
        // per-iteration allocations are arena-freed at the boundary.
        let regioned = self.begin_loop_region(for_loop.body, span);
        // `continue` jumps to the counter bump, `break` leaves the loop.
        // Without this context the body's `break` / `continue` finds no
        // target: it reaches an outer loop's, or none at all.
        self.loop_stack.push(LoopContext {
            region: self.loop_region_slot(regioned),
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(for_loop.body);
        self.loop_stack.pop();
        self.pop_scope();
        self.end_auto_region(regioned, span);
        self.terminate(Terminator::Goto { target: step_block });

        self.set_current(step_block);
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let bumped = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(bumped),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Copy(Place::local(bumped))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        let unit_ty = self.tcx.unit();
        let unit = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(unit),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        Some(unit)
    }

    /// Materialise a map `m.iter()` bound directly to a
    /// `Vec<(K, V)>` into a real heap vector of `(K, V)` tuples.
    /// Mirrors the `for (k, v) in m.iter()` lowering: snapshot the
    /// keys via `gos_rt_map_keys_*`, then `get_or` each value and
    /// push the `(k, v)` tuple. The for-loop form is handled earlier
    /// by `try_lower_for_hashmap_iter`; this covers the direct-bind
    /// form (`let entries = m.iter()`) that otherwise dispatched the
    /// map receiver through `gos_rt_arr_iter` and segfaulted on the
    /// compiled tiers.
    pub(crate) fn materialize_hashmap_entries(
        &mut self,
        receiver: &HirExpr,
        recv_ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        // A struct, tuple, array, or enum key is stored as content bytes the
        // runtime rebuilds into keys, so the pairs are the key snapshot and
        // the value snapshot - both in the one key order - zipped together.
        if let Some((key, value)) = self.hash_map_kv_tys(recv_ty)
            && (self.is_aggregate_key(key)
                || (self.struct_name_of(key).is_none() && self.ensure_enum_eq_desc(key).is_some()))
        {
            let recv_local = self.lower_expr(receiver)?;
            let recv = || vec![Operand::Copy(Place::local(recv_local))];
            let keys_ty = self.tcx.intern(TyKind::Vec(key));
            let keys = self.emit_combinator_call("gos_rt_map_keys_vec", recv(), keys_ty, span);
            let (values_helper, stored_value) = if self.map_value_is_carrier(recv_ty) {
                ("gos_rt_map_values_carrier", value)
            } else if self.struct_name_of(value).is_some() {
                let boxed = self.tcx.intern(TyKind::Ref {
                    mutability: gossamer_types::Mutbl::Not,
                    inner: value,
                });
                ("gos_rt_map_values_vec", boxed)
            } else {
                ("gos_rt_map_values_vec", value)
            };
            let values_ty = self.tcx.intern(TyKind::Vec(stored_value));
            let values = self.emit_combinator_call(values_helper, recv(), values_ty, span);
            return Some(self.lower_zip_general(keys, values, key, value, span));
        }
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let str_ty = self.tcx.string_ty();
        let key_kind = self.hash_map_key_kind(recv_ty);
        let value_kind = self.hash_map_value_kind(recv_ty);
        let pairs_fill_in_runtime = matches!(key_kind, Some(MapKeyKind::String | MapKeyKind::I64))
            && matches!(value_kind, Some(MapValueKind::String | MapValueKind::I64));
        let key_ty = match key_kind {
            Some(MapKeyKind::String) => str_ty,
            _ => i64_ty,
        };
        // A struct value is stored boxed, so the entry's word is the box's
        // address. The materialised pair is an owned element - it outlives the
        // walk that produced it and can be stored - so the struct is copied out
        // of the box into the slot, and the vec's slot-children meta retains
        // what the copy shares with the map's own value. The `for (k, v) in
        // m.iter()` binding keeps the reference instead: it names the entry for
        // the body's duration only.
        let struct_val_ty = match value_kind {
            Some(MapValueKind::Other) => self
                .hash_map_kv_tys(recv_ty)
                .map(|(_, v)| v)
                .filter(|v| self.struct_name_of(*v).is_some()),
            _ => None,
        };
        let val_ty = match (&value_kind, struct_val_ty) {
            (Some(MapValueKind::String), _) => str_ty,
            (_, Some(value)) => value,
            (Some(MapValueKind::Other), None) => {
                self.hash_map_kv_tys(recv_ty).map_or(i64_ty, |(_, v)| v)
            }
            _ => i64_ty,
        };
        let boxed_val_ty = struct_val_ty.map(|value| {
            self.tcx.intern(TyKind::Ref {
                mutability: gossamer_types::Mutbl::Not,
                inner: value,
            })
        });
        let keys_helper = match key_kind {
            Some(MapKeyKind::String) => "gos_rt_map_keys_str",
            _ if self.map_keys_unsigned(recv_ty) => "gos_rt_map_keys_u64",
            _ => "gos_rt_map_keys_i64",
        };
        let get_or_helper = {
            match (key_kind, value_kind) {
                (Some(MapKeyKind::String), Some(MapValueKind::String)) => {
                    "gos_rt_map_get_or_str_str"
                }
                (Some(MapKeyKind::String), _) => "gos_rt_map_get_or_typed_str_i64",
                (_, Some(MapValueKind::String)) => "gos_rt_map_get_or_i64_str",
                _ => "gos_rt_map_get_or_i64",
            }
        };

        let unit_ty = self.tcx.unit();
        let tuple_ty = self.tcx.intern(TyKind::Tuple(vec![key_ty, val_ty]));
        let result_vec_ty = self.tcx.intern(TyKind::Vec(tuple_ty));

        // The runtime copies a scalar's stored word into the pair as it is, so
        // the pair takes the key and value types the map declares: an `f64`,
        // `char`, or `bool` slot is read as itself, never as the integer its
        // bits spell.
        let (tuple_ty, result_vec_ty) = match self.hash_map_kv_tys(recv_ty) {
            Some((declared_key, declared_val)) if pairs_fill_in_runtime => {
                let pair_key = if key_ty == str_ty {
                    str_ty
                } else {
                    declared_key
                };
                let pair_val = if val_ty == str_ty {
                    str_ty
                } else {
                    declared_val
                };
                let pair = self.tcx.intern(TyKind::Tuple(vec![pair_key, pair_val]));
                (pair, self.tcx.intern(TyKind::Vec(pair)))
            }
            _ => (tuple_ty, result_vec_ty),
        };

        let recv_local = self.lower_expr(receiver)?;

        // A pair of two words - each a `String` or a scalar - is written by the
        // runtime in one pass under the map's lock, sorted once, rather than
        // snapshotting the keys and looking every one of them up again.
        if pairs_fill_in_runtime && self.elem_bytes_of(tuple_ty) == 16 {
            let elem_bytes = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(elem_bytes),
                Rvalue::Use(Operand::Const(ConstValue::Int(16))),
                span,
            );
            let result_vec = self.fresh(result_vec_ty);
            let after_new = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("Vec::new".to_string())),
                args: vec![Operand::Copy(Place::local(elem_bytes))],
                destination: Place::local(result_vec),
                target: Some(after_new),
            });
            self.set_current(after_new);
            let filled = self.fresh(unit_ty);
            let after_fill = self.new_block(span);
            let entries_helper = if self.map_keys_unsigned(recv_ty) {
                "gos_rt_map_entries_into_u64"
            } else {
                "gos_rt_map_entries_into"
            };
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(entries_helper.to_string())),
                args: vec![
                    Operand::Copy(Place::local(recv_local)),
                    Operand::Copy(Place::local(result_vec)),
                ],
                destination: Place::local(filled),
                target: Some(after_fill),
            });
            self.set_current(after_fill);
            return Some(result_vec);
        }

        // keys = m.keys() - a fresh real Vec<K> snapshot.
        let keys_vec_ty = self.tcx.intern(TyKind::Vec(key_ty));
        let keys_vec = self.fresh(keys_vec_ty);
        let after_keys = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(keys_helper.to_string())),
            args: vec![Operand::Copy(Place::local(recv_local))],
            destination: Place::local(keys_vec),
            target: Some(after_keys),
        });
        self.set_current(after_keys);

        // len = keys.len()
        let len_local = self.fresh(i64_ty);
        let after_len = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
            args: vec![Operand::Copy(Place::local(keys_vec))],
            destination: Place::local(len_local),
            target: Some(after_len),
        });
        self.set_current(after_len);

        // result = Vec::new(elem_bytes_of((K, V)))
        let elem_bytes_val = i128::from(self.elem_bytes_of(tuple_ty).max(8));
        let elem_bytes = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes),
            Rvalue::Use(Operand::Const(ConstValue::Int(elem_bytes_val))),
            span,
        );
        let result_vec = self.fresh(result_vec_ty);
        let after_new = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("Vec::new".to_string())),
            args: vec![Operand::Copy(Place::local(elem_bytes))],
            destination: Place::local(result_vec),
            target: Some(after_new),
        });
        self.set_current(after_new);

        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let step_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let bool_ty = self.tcx.bool_ty();
        let cmp = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(cmp),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(len_local)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cmp)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        // key = keys[counter]
        let ptr_local = self.fresh(i64_ty);
        let after_ptr = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_ptr".to_string())),
            args: vec![
                Operand::Copy(Place::local(keys_vec)),
                Operand::Copy(Place::local(counter)),
            ],
            destination: Place::local(ptr_local),
            target: Some(after_ptr),
        });
        self.set_current(after_ptr);
        let key_local = self.fresh(key_ty);
        let zero_off = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(zero_off),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let after_load = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_load".to_string())),
            args: vec![
                Operand::Copy(Place::local(ptr_local)),
                Operand::Copy(Place::local(zero_off)),
            ],
            destination: Place::local(key_local),
            target: Some(after_load),
        });
        self.set_current(after_load);

        // val = m.get_or(key, default)
        let default_local = if val_ty == str_ty {
            let l = self.fresh(str_ty);
            self.emit_assign(
                Place::local(l),
                Rvalue::Use(Operand::Const(ConstValue::Str(String::new()))),
                span,
            );
            l
        } else {
            let l = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(l),
                Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                span,
            );
            l
        };
        let val_local = self.fresh(boxed_val_ty.unwrap_or(val_ty));
        let after_val = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(get_or_helper.to_string())),
            args: vec![
                Operand::Copy(Place::local(recv_local)),
                Operand::Copy(Place::local(key_local)),
                Operand::Copy(Place::local(default_local)),
            ],
            destination: Place::local(val_local),
            target: Some(after_val),
        });
        self.set_current(after_val);
        // The pair's slot holds the struct's own words, so read them through
        // the box the entry named.
        let val_local = if boxed_val_ty.is_some() {
            let copied = self.fresh(val_ty);
            self.emit_assign(
                Place::local(copied),
                Rvalue::Use(Operand::Copy(Place {
                    local: val_local,
                    projection: vec![crate::ir::Projection::Deref],
                })),
                span,
            );
            copied
        } else {
            val_local
        };

        // tuple = (key, val); result.push(tuple)
        let tuple_local = self.fresh(tuple_ty);
        self.emit_assign(
            Place::local(tuple_local),
            Rvalue::Aggregate {
                kind: crate::ir::AggregateKind::Tuple,
                operands: vec![
                    Operand::Copy(Place::local(key_local)),
                    Operand::Copy(Place::local(val_local)),
                ],
            },
            span,
        );
        let push_dest = self.fresh(unit_ty);
        let after_push = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_push".to_string())),
            args: vec![
                Operand::Copy(Place::local(result_vec)),
                Operand::Copy(Place::local(tuple_local)),
            ],
            destination: Place::local(push_dest),
            target: Some(after_push),
        });
        self.set_current(after_push);
        self.terminate(Terminator::Goto { target: step_block });

        self.set_current(step_block);
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let bumped = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(bumped),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Copy(Place::local(bumped))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(result_vec)
    }
}
