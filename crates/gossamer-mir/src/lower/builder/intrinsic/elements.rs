//! Element access and typing for sequence traversals: slots, floats, and eager results.

use super::*;

impl<'a> Builder<'a> {
    /// `xs.map(f)` over a word-slot element and a word-slot result, with `f`
    /// a body the compiler can name: emits the traversal here so the element
    /// is read, transformed, and pushed without a runtime shim in between.
    /// `None` leaves the call to the general lowering.
    #[allow(
        clippy::too_many_arguments,
        reason = "one parameter per shape the specialisation is gated on"
    )]
    pub(super) fn try_lower_direct_map(
        &mut self,
        callback: &HirExpr,
        source: &HirExpr,
        in_ty: Ty,
        in_abi: ElemAbi,
        out_ty: Ty,
        out_abi: ElemAbi,
        result_ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        if in_abi != ElemAbi::Word || out_abi != ElemAbi::Word {
            return None;
        }
        // The traversal below reads and writes one 8-byte slot per element,
        // so a narrower or wider element keeps the shim that knows its stride.
        if self.elem_bytes_of(in_ty) != 8 || self.elem_bytes_of(out_ty) != 8 {
            return None;
        }
        // The output vec's elements are plain words this loop owns outright;
        // an element that carries a heap child needs the shim's element-kind
        // bookkeeping instead.
        if !matches!(
            self.tcx.kind_of(out_ty),
            TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_)
        ) {
            return None;
        }
        if !matches!(
            self.tcx.kind_of(result_ty),
            TyKind::Vec(_) | TyKind::Slice(_)
        ) {
            return None;
        }
        let body = self.direct_callback_body(callback)?;
        let vec_local = self.lower_iter_vec_arg(source)?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();

        let len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(vec_local))],
            i64_ty,
            span,
        );
        let out = self.emit_combinator_call(
            "gos_rt_vec_with_capacity_typed",
            vec![
                Operand::Const(ConstValue::Int(8)),
                Operand::Copy(Place::local(len)),
                Operand::Const(ConstValue::Int(0)),
            ],
            result_ty,
            span,
        );
        let index = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let more = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(more),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(len)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(more)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        let elem = self.emit_combinator_call(
            "gos_rt_vec_get_i64",
            vec![
                Operand::Copy(Place::local(vec_local)),
                Operand::Copy(Place::local(index)),
            ],
            in_ty,
            span,
        );
        let mapped = self.fresh(out_ty);
        let after_call = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(body)),
            args: vec![Operand::Copy(Place::local(elem))],
            destination: Place::local(mapped),
            target: Some(after_call),
        });
        self.set_current(after_call);
        let unit_ty = self.tcx.unit();
        let _ = self.emit_combinator_call(
            "gos_rt_vec_push_i64",
            vec![
                Operand::Copy(Place::local(out)),
                Operand::Copy(Place::local(mapped)),
            ],
            unit_ty,
            span,
        );
        self.emit_assign(
            Place::local(index),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Const(ConstValue::Int(1)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(out)
    }

    /// The callback parameter type and element class for a combinator reading
    /// `vec_local`.
    ///
    /// An `f64` element reaches its callback in an SSE register, an element
    /// wider than one slot by the address of its storage, and everything else
    /// as the word its slot spells. The shim to call and the type the callback
    /// is built with both follow from this, so they are decided together and
    /// cannot drift apart.
    pub(crate) fn iter_callback_shape(&mut self, vec_local: Local) -> (Ty, ElemAbi) {
        let seq_ty = self.locals[vec_local.0 as usize].ty;
        let (elem, abi) = self.iter_elem_abi(seq_ty);
        match abi {
            ElemAbi::Word => (self.tcx.int_ty(gossamer_types::IntTy::I64), abi),
            ElemAbi::Float | ElemAbi::Ptr => (elem, abi),
        }
    }

    /// The ordering descriptor to compare two of `elem` through, or an empty
    /// string when comparing the element's bytes is already its equality.
    ///
    /// A scalar's slot spells its value, so its bytes are its identity. A
    /// String, a struct or tuple holding one, and an enum reached through its
    /// node are all values two of which can be equal at different addresses,
    /// and those are compared through the descriptor instead.
    pub(crate) fn elem_equality_descriptor(&mut self, elem: Ty) -> String {
        use gossamer_types::TyKind;
        let scalar = matches!(
            self.tcx.kind_of(elem),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
        );
        if scalar {
            return String::new();
        }
        self.ordering_stream(elem)
            .map(|tags| tags.iter().map(|&b| b as char).collect())
            .unwrap_or_default()
    }

    /// The element a combinator's result sequence carries: the expected type's
    /// element when the call site names a sequence, and otherwise the element
    /// of the sequence that was read.
    ///
    /// A combinator that answers what it read carries the element with it. The
    /// word fallback stands only for a sequence whose element neither side
    /// names, and reaching it for a wider element is what renders that element
    /// as its raw slot bits.
    pub(crate) fn iter_result_elem_ty(&mut self, expected: Ty, source: Local) -> Ty {
        if let Some(elem) = self.sequence_elem_ty_of(expected) {
            return elem;
        }
        let src_ty = self.locals[source.0 as usize].ty;
        self.sequence_elem_ty_of(src_ty)
            .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64))
    }

    /// `Vec<T>` over [`Self::iter_result_elem_ty`].
    pub(crate) fn iter_result_vec_ty(&mut self, expected: Ty, source: Local) -> Ty {
        let elem = self.iter_result_elem_ty(expected, source);
        self.tcx.intern(gossamer_types::TyKind::Vec(elem))
    }

    /// The class a combinator's sequence argument is read as, from the local
    /// holding it. The address-taken classes are what a callback receives by
    /// slot address rather than as a loaded word.
    pub(crate) fn iter_local_elem_abi(&mut self, vec_local: Local) -> ElemAbi {
        let ty = self.locals[vec_local.0 as usize].ty;
        self.iter_elem_abi(ty).1
    }

    /// Whether a map of type `ty` declares its keys `u64` / `usize`, whose
    /// stored words walk in unsigned order.
    pub(crate) fn map_keys_unsigned(&self, ty: Ty) -> bool {
        self.hash_map_kv_tys(ty).is_some_and(|(key, _)| {
            matches!(
                self.tcx.kind_of(self.peel_ref_ty(key)),
                gossamer_types::TyKind::Int(
                    gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize
                )
            )
        })
    }

    /// Whether a set of type `ty` declares its elements `u64` / `usize`,
    /// whose stored words walk in unsigned order.
    pub(crate) fn set_elems_unsigned(&self, ty: Ty) -> bool {
        self.first_generic_of(self.peel_ref_ty(ty))
            .is_some_and(|elem| {
                matches!(
                    self.tcx.kind_of(self.peel_ref_ty(elem)),
                    gossamer_types::TyKind::Int(
                        gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize
                    )
                )
            })
    }

    pub(crate) fn iter_elem_abi(&mut self, seq_ty: Ty) -> (Ty, ElemAbi) {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let Some(elem) = self.iter_element_kind(seq_ty).map(|k| self.tcx.intern(k)) else {
            return (i64_ty, ElemAbi::Word);
        };
        if matches!(self.tcx.kind_of(elem), TyKind::Float(_)) {
            return (
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
                ElemAbi::Float,
            );
        }
        // An element naming a type parameter belongs to a generic template.
        // Each instantiation is lowered again with the concrete element, and a
        // template that stays reachable is rejected before code generation, so
        // the template has no layout to choose and keeps the word class.
        if self.ty_mentions_param(elem) {
            return (elem, ElemAbi::Word);
        }
        // A struct, tuple, or array element is inline slot data the body
        // reaches through its address, whatever its width; a one-field struct
        // fits a slot but its field is still read at an offset, not from the
        // slot's own bits.
        if self.elem_bytes_of(elem) > 8 || self.elem_is_slot_addressed(elem) {
            return (elem, ElemAbi::Ptr);
        }
        (elem, ElemAbi::Word)
    }

    /// Whether an element's storage is read through its address rather than
    /// as the value its slot spells. An enum stays a word: its value is the
    /// inline tag or the RC node pointer the variant decoding reads directly.
    pub(crate) fn elem_is_slot_addressed(&mut self, elem: Ty) -> bool {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(elem) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            TyKind::Adt { def, .. } => {
                let def = *def;
                self.tcx.enum_variant_tys(def).is_none() && self.tcx.struct_field_tys(def).is_some()
            }
            _ => false,
        }
    }

    /// ABI class of a combinator's result element (`map`'s output, `fold`'s
    /// accumulator, `sum_by`'s projection): float or word.
    pub(crate) fn scalar_abi_of(&mut self, ty: Ty) -> ElemAbi {
        use gossamer_types::TyKind;
        if matches!(self.tcx.kind_of(ty), TyKind::Float(_)) {
            ElemAbi::Float
        } else {
            ElemAbi::Word
        }
    }

    /// Element type of an iterated sequence local when that element is wider
    /// than one slot, which a combinator's callback receives by slot address
    /// rather than as a loaded word.
    pub(super) fn iter_wide_elem_ty(&self, vec_local: Local) -> Option<Ty> {
        use gossamer_types::TyKind;
        let elem = match self.tcx.kind_of(self.locals[vec_local.0 as usize].ty) {
            TyKind::Vec(elem) | TyKind::Slice(elem) => *elem,
            _ => return None,
        };
        // What decides the callback's shape is the element's width, not
        // whether it was written as a struct: a tuple of that width is stored
        // and reached exactly the same way.
        self.aggr_lazy_elem(elem).then_some(elem)
    }

    /// True when the iterated sequence's elements are `f64`, whose slot bits a
    /// combinator's callback must receive reinterpreted as a float.
    pub(super) fn iter_elem_is_float(&self, vec_local: Local) -> bool {
        use gossamer_types::TyKind;
        matches!(
            self.tcx.kind_of(self.locals[vec_local.0 as usize].ty),
            TyKind::Vec(elem) | TyKind::Slice(elem) if matches!(self.tcx.kind_of(*elem), TyKind::Float(_))
        )
    }

    /// `carrier.unwrap_or(v)` / `carrier.unwrap_or_else(f)` for an aggregate
    /// payload, as the branch it stands for: the `Ok` / `Some` payload copied
    /// out of the carrier, or else the fallback. Both arms are copies the frame
    /// owns, so the answer takes its own shares of the value's children the
    /// way a `match` binding does, where a runtime helper would hand back the
    /// payload's address with no share behind it.
    pub(crate) fn lower_unwrap_or_inline(
        &mut self,
        recv: Local,
        fallback: CarrierFallback,
        receiver_ty: Ty,
        is_option: bool,
        dest_ty: Ty,
        span: Span,
    ) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let payload_ty = self.enum_payload_ty(receiver_ty, 0).unwrap_or(dest_ty);
        let dest = self.fresh(dest_ty);
        let disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(disc),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_disc",
                args: vec![Operand::Copy(Place::local(recv))],
            },
            span,
        );
        let present = self.new_block(span);
        let otherwise = self.new_block(span);
        let join = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(disc)),
            arms: vec![(0, present)],
            default: otherwise,
        });
        self.set_current(present);
        if self.carrier_payload_is_carrier(receiver_ty) {
            // A payload that is itself a carrier is boxed, so the reader
            // answers the two words the box holds. The destination takes its
            // own share of them where that call answers, which is a block only
            // this arm reaches.
            let reader = if is_option {
                "gos_rt_option_unwrap_carrier"
            } else {
                "gos_rt_result_unwrap_carrier"
            };
            let answered = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(reader.to_string())),
                args: vec![Operand::Copy(Place::local(recv))],
                destination: Place::local(dest),
                target: Some(answered),
            });
            self.set_current(answered);
        } else {
            let payload = self.fresh(payload_ty);
            self.emit_assign(
                Place::local(payload),
                Rvalue::CallIntrinsic {
                    name: "gos_rt_result_payload",
                    args: vec![Operand::Copy(Place::local(recv))],
                },
                span,
            );
            self.emit_assign(
                Place::local(dest),
                Rvalue::Use(Operand::Copy(Place::local(payload))),
                span,
            );
        }
        self.terminate(Terminator::Goto { target: join });
        self.set_current(otherwise);
        let closure = match fallback {
            CarrierFallback::Value(value) => {
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::Use(Operand::Copy(Place::local(value))),
                    span,
                );
                self.terminate(Terminator::Goto { target: join });
                self.set_current(join);
                return dest;
            }
            CarrierFallback::Call(closure) => closure,
        };
        let args = if is_option {
            Vec::new()
        } else {
            let err_ty = self.enum_payload_ty(receiver_ty, 1).unwrap_or(i64_ty);
            let err = self.fresh(err_ty);
            self.emit_assign(
                Place::local(err),
                Rvalue::CallIntrinsic {
                    name: "gos_rt_result_payload",
                    args: vec![Operand::Copy(Place::local(recv))],
                },
                span,
            );
            vec![Operand::Copy(Place::local(err))]
        };
        self.terminate(Terminator::Call {
            callee: Operand::Copy(Place::local(closure)),
            args,
            destination: Place::local(dest),
            target: Some(join),
        });
        self.set_current(join);
        dest
    }

    /// Lowers `carrier.map(f)` whose mapped payload is an aggregate stored by
    /// address, as the match it stands for.
    ///
    /// The closure's aggregate answer is written into a local of this frame
    /// and boxed by the `Some` / `Ok` constructor, which gives it the copy-blob
    /// layout every holder of the carrier releases. An empty or `Err` receiver
    /// is the answer as it stands.
    pub(crate) fn lower_map_inline(
        &mut self,
        recv: Local,
        closure: Local,
        receiver_ty: Ty,
        mapped_ty: Ty,
        dest_ty: Ty,
        span: Span,
    ) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let payload_ty = self.enum_payload_ty(receiver_ty, 0).unwrap_or(i64_ty);
        let dest = self.fresh(dest_ty);
        let disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(disc),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_disc",
                args: vec![Operand::Copy(Place::local(recv))],
            },
            span,
        );
        let present = self.new_block(span);
        let otherwise = self.new_block(span);
        let mapped_block = self.new_block(span);
        let join = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(disc)),
            arms: vec![(0, present)],
            default: otherwise,
        });
        self.set_current(present);
        let payload = self.fresh(payload_ty);
        self.emit_assign(
            Place::local(payload),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_payload",
                args: vec![Operand::Copy(Place::local(recv))],
            },
            span,
        );
        let mapped = self.fresh(mapped_ty);
        self.terminate(Terminator::Call {
            callee: Operand::Copy(Place::local(closure)),
            args: vec![Operand::Copy(Place::local(payload))],
            destination: Place::local(mapped),
            target: Some(mapped_block),
        });
        self.set_current(mapped_block);
        let present_disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(present_disc),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        self.lower_result_ctor_into(dest, present_disc, mapped, span);
        self.terminate(Terminator::Goto { target: join });
        self.set_current(otherwise);
        self.emit_assign(
            Place::local(dest),
            Rvalue::Use(Operand::Copy(Place::local(recv))),
            span,
        );
        self.terminate(Terminator::Goto { target: join });
        self.set_current(join);
        dest
    }

    /// Lowers `opt.filter(p)` as the match it stands for: the receiver when it
    /// holds a value the predicate accepts, `None` otherwise.
    pub(crate) fn lower_filter_inline(
        &mut self,
        recv: Local,
        predicate: Local,
        receiver_ty: Ty,
        span: Span,
    ) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let payload_ty = self.enum_payload_ty(receiver_ty, 0).unwrap_or(i64_ty);
        let dest = self.fresh(receiver_ty);
        let disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(disc),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_disc",
                args: vec![Operand::Copy(Place::local(recv))],
            },
            span,
        );
        let present = self.new_block(span);
        let judged = self.new_block(span);
        let kept = self.new_block(span);
        let empty = self.new_block(span);
        let join = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(disc)),
            arms: vec![(0, present)],
            default: empty,
        });
        self.set_current(present);
        let payload = self.fresh(payload_ty);
        self.emit_assign(
            Place::local(payload),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_payload",
                args: vec![Operand::Copy(Place::local(recv))],
            },
            span,
        );
        let accepted = self.fresh(bool_ty);
        self.terminate(Terminator::Call {
            callee: Operand::Copy(Place::local(predicate)),
            args: vec![Operand::Copy(Place::local(payload))],
            destination: Place::local(accepted),
            target: Some(judged),
        });
        self.set_current(judged);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(accepted)),
            arms: vec![(0, empty)],
            default: kept,
        });
        self.set_current(kept);
        self.emit_assign(
            Place::local(dest),
            Rvalue::Use(Operand::Copy(Place::local(recv))),
            span,
        );
        self.terminate(Terminator::Goto { target: join });
        self.set_current(empty);
        self.emit_assign(
            Place::local(dest),
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
        dest
    }

    pub(crate) fn lower_iter_closure(
        &mut self,
        closure_arg: &HirExpr,
        inputs: &[Ty],
        output: Ty,
        span: Span,
    ) -> Option<Local> {
        // A shim that reads the callback's result as a word still calls a body
        // that answers its own narrower scalar, so the callable keeps that
        // result type and its thunk widens it to the word the shim reads.
        let output = match self.callable_output_of(closure_arg) {
            Some(real)
                if output == self.tcx.int_ty(gossamer_types::IntTy::I64)
                    && matches!(
                        self.tcx.kind_of(real),
                        gossamer_types::TyKind::Bool
                            | gossamer_types::TyKind::Char
                            | gossamer_types::TyKind::Int(
                                gossamer_types::IntTy::I8
                                    | gossamer_types::IntTy::U8
                                    | gossamer_types::IntTy::I16
                                    | gossamer_types::IntTy::U16
                                    | gossamer_types::IntTy::I32
                                    | gossamer_types::IntTy::U32
                            )
                    ) =>
            {
                real
            }
            _ => output,
        };
        let raw = self.lower_expr(closure_arg)?;
        // A combinator hands a wide element to its callback by the address of
        // the element's storage. A struct, tuple, or array parameter IS that
        // address, so the element type is what the callback takes. An
        // `Option` / `Result` / inline-enum element is two words held BY
        // VALUE, so a parameter of that type would be read out of registers
        // the shim never filled: the callback takes a reference to it
        // instead, which is the address the shim passes, and the
        // carrier-reference pass rewrites the body's reads to go through it.
        let inputs: Vec<Ty> = inputs
            .iter()
            .map(|&input| {
                if crate::lower::carrier_ref::is_two_word_carrier(self.tcx, input)
                    && self.elem_bytes_of(input) > 8
                {
                    self.tcx.intern(gossamer_types::TyKind::Ref {
                        mutability: gossamer_types::Mutbl::Not,
                        inner: input,
                    })
                } else {
                    input
                }
            })
            .collect();
        let cb_sig = gossamer_types::FnSig { inputs, output };
        let cb_trait_ty = self.tcx.intern(gossamer_types::TyKind::FnTrait(cb_sig));
        Some(self.coerce_to_fn_trait_if_needed(raw, cb_trait_ty, span))
    }

    /// Lowers an iterator source to a `GosVec` handle the eager combinator
    /// shims can index. A fixed array widens to a Vec; lazy iterator state is
    /// a distinct runtime object, so it is drained into a snapshot Vec first.
    pub(crate) fn lower_iter_vec_arg(&mut self, arg: &HirExpr) -> Option<Local> {
        use gossamer_types::TyKind;
        // An eager traversal of `m.iter()` reads the materialised pairs
        // directly: a cursor over them would only be drained back into a
        // second copy of the same sequence.
        if let HirExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &arg.kind
            && name.name == "iter"
            && args.is_empty()
        {
            let mut recv_ty = self
                .receiver_local_from_path(receiver)
                .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv_ty) {
                recv_ty = *inner;
            }
            if matches!(self.tcx.kind_of(recv_ty), TyKind::HashMap { .. }) {
                return self.materialize_hashmap_entries(receiver, recv_ty, arg.span);
            }
        }
        let raw = self.lower_expr(arg)?;
        let raw_ty = self.locals[raw.0 as usize].ty;
        match self.tcx.kind_of(raw_ty).clone() {
            TyKind::Array { elem, len } => Some(self.coerce_array_to_vec(raw, elem, len, arg.span)),
            // Lazy state, whether it carries its element in a word slot or by
            // address. An eager combinator reads a `GosVec` header, so the
            // handle drains into the sequence it stands for first; handing the
            // handle over unchanged has it read as a vec of its own fields.
            TyKind::Iterator(elem)
                if self.lazy_iter_carries_elem(elem) || self.local_aggr_iter.contains(&raw) =>
            {
                let vec_ty = self.tcx.intern(TyKind::Vec(elem));
                let collect_symbol = self.lazy_collect_symbol_for(raw, elem);
                let drained = self.emit_combinator_call(
                    collect_symbol,
                    vec![Operand::Copy(Place::local(raw))],
                    vec_ty,
                    arg.span,
                );
                if collect_symbol == "gos_rt_lazy_iter_collect_aggr" {
                    self.tag_owned_elements(drained, elem, arg.span);
                }
                Some(drained)
            }
            _ => Some(raw),
        }
    }

    /// Runtime symbol that drains iterator state into a `Vec` of `elem`.
    /// A two-`i64` tuple element rides its own shim because the snapshot
    /// stores pairs rather than scalar slots.
    /// Collect helper for the state `local` holds. An element wider than one
    /// slot reaches a terminal either as a pair of words or as an address, and
    /// only the producing site knows which, so the marker decides before the
    /// element type does.
    pub(crate) fn lazy_collect_symbol_for(&mut self, local: Local, elem: Ty) -> &'static str {
        if self.local_aggr_iter.contains(&local) {
            return "gos_rt_lazy_iter_collect_aggr";
        }
        self.lazy_collect_symbol(elem)
    }

    pub(crate) fn lazy_collect_symbol(&mut self, elem: Ty) -> &'static str {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        match self.tcx.kind_of(elem) {
            TyKind::Tuple(fields)
                if fields.len() == 2 && fields.iter().all(|field| *field == i64_ty) =>
            {
                "gos_rt_lazy_iter_collect_pair_i64"
            }
            _ => "gos_rt_lazy_iter_collect_i64",
        }
    }

    /// Element family a lazy-combinator input can supply, whether it is
    /// iterator state already or a sequence about to be borrowed as one.
    /// `None` for a source the lazy slot cannot carry, which keeps the caller
    /// on the eager sequence surface - including the pair shape, whose state
    /// only `zip` and `enumerate` produce.
    pub(crate) fn lazy_iter_source_family(&self, arg_ty: Ty) -> Option<LazyElemFamily> {
        use gossamer_types::TyKind;
        let mut peeled = arg_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        let elem = match self.tcx.kind_of(peeled) {
            TyKind::Iterator(elem)
            | TyKind::Range(elem)
            | TyKind::Vec(elem)
            | TyKind::Slice(elem)
            | TyKind::Array { elem, .. } => *elem,
            _ => return None,
        };
        match self.lazy_iter_elem_family(elem) {
            // The pair state is built by `zip` / `enumerate`, never borrowed
            // from a sequence, so a pair-shaped element borrows through the
            // address form like any other multi-slot element.
            Some(LazyElemFamily::PairWord) | None => self
                .lazy_addressed_elem(elem)
                .then_some(LazyElemFamily::Aggr),
            family => family,
        }
    }

    /// Source family for a combinator that reads each slot as an element
    /// value: an address-carrying stream is not one, so such a source stays on
    /// the eager surface where the element is read at its real width.
    pub(crate) fn lazy_iter_source_family_word(&self, arg_ty: Ty) -> Option<LazyElemFamily> {
        self.lazy_iter_source_family(arg_ty)
            .filter(|family| *family != LazyElemFamily::Aggr)
    }

    /// Whether a sequence of `elem` can be borrowed as a stream of element
    /// addresses: the element is stored inline, wider than the word slot a
    /// value-carrying stream reads, and its own storage is what a consumer
    /// reads through.
    pub(crate) fn aggr_lazy_elem(&self, elem: Ty) -> bool {
        use gossamer_types::TyKind;
        matches!(
            self.tcx.kind_of(elem),
            TyKind::Tuple(_) | TyKind::Adt { .. }
        ) && self.elem_bytes_of(elem) > 8
    }

    /// Whether a lazy stream of `elem` carries each element as the address of
    /// its storage: an element wider than one slot, or a struct, whose storage
    /// is reached through its address whatever its width.
    pub(crate) fn lazy_addressed_elem(&self, elem: Ty) -> bool {
        use gossamer_types::TyKind;
        self.aggr_lazy_elem(elem)
            || matches!(
                self.tcx.kind_of(elem),
                TyKind::Adt { def, .. }
                    if self.tcx.enum_variant_tys(*def).is_none()
                        && self.tcx.struct_field_tys(*def).is_some()
            )
    }

    /// Result type for an adapter that answered eagerly. A surface type of
    /// `Iterator<T>` describes state the eager shim does not build, so the
    /// value is typed as the `Vec<T>` it really is.
    pub(crate) fn eager_seq_result_ty(&mut self, surface: Ty, elem: Ty) -> Ty {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(surface) {
            TyKind::Iterator(_) => self.tcx.intern(TyKind::Vec(elem)),
            _ => surface,
        }
    }

    /// Lazy family of the state a lowered local actually holds.
    ///
    /// A combinator's result type says what the surface promises; the local's
    /// MIR type says what the producing arm built. Only the second decides
    /// whether a value is an iterator handle or a `GosVec`, so every consumer
    /// asks this rather than re-deriving the producer's choice from types.
    pub(crate) fn lowered_lazy_family(&self, local: Local) -> Option<LazyElemFamily> {
        if self.local_aggr_iter.contains(&local) {
            return Some(LazyElemFamily::Aggr);
        }
        self.lazy_iter_ty_family(self.locals[local.0 as usize].ty)
    }

    /// Wraps a materialised `Vec<(K, V)>` of map entries as lazy iterator
    /// state, so `m.iter()` answers the cursor its type names. `None` when the
    /// entry shape has no lazy state to carry it, leaving the caller with the
    /// vec it already built.
    pub(crate) fn entries_cursor(&mut self, entries: Local, span: Span) -> Option<Local> {
        let (handle, _family) = self.borrow_lazy_state(entries, span, true)?;
        Some(handle)
    }

    /// `xs.enumerate()` where the element is wider than one slot.
    ///
    /// The pair helper writes an index and one slot per element, which for a
    /// wider element keeps only its first field. Walking the source and
    /// building each `(index, element)` pair here gives every pair the
    /// element's own width.
    /// Whether an element's 8-byte slot is the element's own value, which is
    /// what lets the word-slot combinator shims copy it verbatim.
    pub(super) fn zip_slot_is_the_element(&mut self, elem: Ty) -> bool {
        use gossamer_types::TyKind;
        matches!(
            self.tcx.kind_of(elem),
            TyKind::Int(_) | TyKind::Bool | TyKind::Char
        )
    }

    /// `iter::zip(a, b)` for any pair of element types: reads element `i` from
    /// each side through the ordinary element path, so each pair owns its
    /// halves and carries their declared types.
    pub(super) fn lower_zip_general(
        &mut self,
        a: Local,
        b: Local,
        a_elem: Ty,
        b_elem: Ty,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let pair_ty = self.tcx.intern(TyKind::Tuple(vec![a_elem, b_elem]));
        let out_ty = self.tcx.intern(TyKind::Vec(pair_ty));
        let _ = self.ensure_aggr_copy_meta(pair_ty);
        let elem_bytes = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                self.type_slot_bytes(pair_ty).max(1),
            )))),
            span,
        );
        let a_len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(a))],
            i64_ty,
            span,
        );
        let b_len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(b))],
            i64_ty,
            span,
        );
        // The pairing stops at the shorter input.
        let shorter = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(shorter),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(a_len)),
                rhs: Operand::Copy(Place::local(b_len)),
            },
            span,
        );
        let len = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(len),
            Rvalue::Use(Operand::Copy(Place::local(b_len))),
            span,
        );
        let take_a = self.new_block(span);
        let after_len = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(shorter)),
            arms: vec![(0, after_len)],
            default: take_a,
        });
        self.set_current(take_a);
        self.emit_assign(
            Place::local(len),
            Rvalue::Use(Operand::Copy(Place::local(a_len))),
            span,
        );
        self.terminate(Terminator::Goto { target: after_len });
        self.set_current(after_len);

        let out = self.emit_combinator_call(
            "gos_rt_vec_with_capacity",
            vec![
                Operand::Copy(Place::local(elem_bytes)),
                Operand::Copy(Place::local(len)),
            ],
            out_ty,
            span,
        );
        let index = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let more = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(more),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(len)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(more)),
            arms: vec![(0, exit)],
            default: body,
        });

        self.set_current(body);
        let left = self.zip_read_element(a, index, a_elem, span);
        let right = self.zip_read_element(b, index, b_elem, span);
        let pair = self.fresh(pair_ty);
        self.emit_assign(
            Place::local(pair),
            Rvalue::Aggregate {
                kind: crate::ir::AggregateKind::Tuple,
                operands: vec![
                    Operand::Copy(Place::local(left)),
                    Operand::Copy(Place::local(right)),
                ],
            },
            span,
        );
        let unit_ty = self.tcx.unit();
        let _ = self.emit_combinator_call(
            "gos_rt_vec_push",
            vec![
                Operand::Copy(Place::local(out)),
                Operand::Copy(Place::local(pair)),
            ],
            unit_ty,
            span,
        );
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let next_index = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(next_index),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Copy(Place::local(next_index))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        out
    }

    /// Element `index` of `source`, read through its slot address so the copy
    /// carries the element's declared type and ownership.
    pub(crate) fn element_slot_ptr(
        &mut self,
        source: Local,
        index: Local,
        elem_ty: Ty,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let ref_ty = self.tcx.intern(TyKind::Ref {
            mutability: gossamer_types::Mutbl::Not,
            inner: elem_ty,
        });
        self.emit_combinator_call(
            "gos_rt_vec_get_ptr",
            vec![
                Operand::Copy(Place::local(source)),
                Operand::Copy(Place::local(index)),
            ],
            ref_ty,
            span,
        )
    }

    /// Element `index` of `source` copied out of the sequence's own storage,
    /// the way `let x = xs[i]` reads one. An aggregate element is inline slot
    /// data, so the indexed place is what carries its width; reading it
    /// through a raw slot pointer left a one-slot struct as the pointer's own
    /// bits on the JIT.
    pub(crate) fn read_element_place(
        &mut self,
        source: Local,
        index: Local,
        elem_ty: Ty,
        span: Span,
    ) -> Local {
        let mut place = Place::local(source);
        place.projection.push(crate::ir::Projection::Index(index));
        let element = self.fresh(elem_ty);
        self.emit_assign(
            Place::local(element),
            Rvalue::Use(Operand::Copy(place)),
            span,
        );
        element
    }

    pub(crate) fn zip_read_element(
        &mut self,
        source: Local,
        index: Local,
        elem_ty: Ty,
        span: Span,
    ) -> Local {
        // An aggregate is read from the indexed place, which carries its
        // width; everything else is one slot the pointer path reads directly.
        if self.elem_is_slot_addressed(elem_ty) {
            return self.read_element_place(source, index, elem_ty, span);
        }
        let slot = self.element_slot_ptr(source, index, elem_ty, span);
        let mut slot_place = Place::local(slot);
        slot_place.projection.push(crate::ir::Projection::Deref);
        let element = self.fresh(elem_ty);
        self.emit_assign(
            Place::local(element),
            Rvalue::Use(Operand::Copy(slot_place)),
            span,
        );
        element
    }

    pub(super) fn lower_enumerate_wide_elem(
        &mut self,
        source: Local,
        elem_ty: Ty,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let pair_ty = self.tcx.intern(TyKind::Tuple(vec![i64_ty, elem_ty]));
        let out_ty = self.tcx.intern(TyKind::Vec(pair_ty));
        let _ = self.ensure_aggr_copy_meta(pair_ty);
        let elem_bytes = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                self.type_slot_bytes(pair_ty).max(1),
            )))),
            span,
        );
        let len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(source))],
            i64_ty,
            span,
        );
        let out = self.emit_combinator_call(
            "gos_rt_vec_with_capacity",
            vec![
                Operand::Copy(Place::local(elem_bytes)),
                Operand::Copy(Place::local(len)),
            ],
            out_ty,
            span,
        );
        let index = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let more = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(more),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(len)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(more)),
            arms: vec![(0, exit)],
            default: body,
        });

        self.set_current(body);
        let ref_ty = self.tcx.intern(TyKind::Ref {
            mutability: gossamer_types::Mutbl::Not,
            inner: elem_ty,
        });
        let slot = self.emit_combinator_call(
            "gos_rt_vec_get_ptr",
            vec![
                Operand::Copy(Place::local(source)),
                Operand::Copy(Place::local(index)),
            ],
            ref_ty,
            span,
        );
        let mut slot_place = Place::local(slot);
        slot_place.projection.push(crate::ir::Projection::Deref);
        let element = self.fresh(elem_ty);
        self.emit_assign(
            Place::local(element),
            Rvalue::Use(Operand::Copy(slot_place)),
            span,
        );
        let pair = self.fresh(pair_ty);
        self.emit_assign(
            Place::local(pair),
            Rvalue::Aggregate {
                kind: crate::ir::AggregateKind::Tuple,
                operands: vec![
                    Operand::Copy(Place::local(index)),
                    Operand::Copy(Place::local(element)),
                ],
            },
            span,
        );
        let unit_ty = self.tcx.unit();
        let _ = self.emit_combinator_call(
            "gos_rt_vec_push",
            vec![
                Operand::Copy(Place::local(out)),
                Operand::Copy(Place::local(pair)),
            ],
            unit_ty,
            span,
        );
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let next_index = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(next_index),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Copy(Place::local(next_index))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        out
    }

    /// `xs.min()` / `xs.max()` where the element is wider than one slot.
    ///
    /// The word-slot terminals compare the first slot of each element, which
    /// is one field of it. Ordering a copy structurally - the comparison
    /// `sort` already uses - puts the answer at a known end, and the element
    /// there becomes the payload.
    /// `min` / `max` over a `String` sequence, which orders by text. The
    /// word shims would order the slots' addresses.
    pub(super) fn lower_minmax_string_elem(
        &mut self,
        seq_arg: &HirExpr,
        want_max: bool,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let mut seq_ty = seq_arg.ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(seq_ty) {
            seq_ty = *inner;
        }
        let (TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) =
            self.tcx.kind_of(seq_ty).clone()
        else {
            return None;
        };
        if !matches!(self.tcx.kind_of(elem), TyKind::String) {
            return None;
        }
        let vec_local = self.lower_iter_vec_arg(seq_arg)?;
        let opt_ty = self.option_payload_adt_ty(elem);
        let helper = if want_max {
            "gos_rt_iter_max_str"
        } else {
            "gos_rt_iter_min_str"
        };
        Some(self.emit_combinator_call(
            helper,
            vec![Operand::Copy(Place::local(vec_local))],
            opt_ty,
            span,
        ))
    }

    pub(super) fn lower_minmax_wide_elem(
        &mut self,
        seq_arg: &HirExpr,
        elem_ty: Ty,
        ty: Ty,
        want_max: bool,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let (count, tags) = self.tuple_element_stream(elem_ty)?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let vec_local = self.lower_iter_vec_arg(seq_arg)?;
        let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        // The caller's order is its own; the ordering runs on a copy.
        let sorted = self.emit_combinator_call(
            "gos_rt_vec_clone",
            vec![Operand::Copy(Place::local(vec_local))],
            vec_ty,
            span,
        );
        let count_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(count_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(
                i128::try_from(count).unwrap_or(0),
            ))),
            span,
        );
        let tag_text: String = tags.iter().map(|&b| b as char).collect();
        let string_ty = self.tcx.string_ty();
        let tags_local = self.fresh(string_ty);
        self.emit_assign(
            Place::local(tags_local),
            Rvalue::Use(Operand::Const(ConstValue::Str(tag_text))),
            span,
        );
        let unit_ty = self.tcx.unit();
        let _ = self.emit_combinator_call(
            "gos_rt_vec_sort_tuple",
            vec![
                Operand::Copy(Place::local(sorted)),
                Operand::Copy(Place::local(count_local)),
                Operand::Copy(Place::local(tags_local)),
            ],
            unit_ty,
            span,
        );
        let index = self.fresh(i64_ty);
        if want_max {
            let len = self.emit_combinator_call(
                "gos_rt_vec_len",
                vec![Operand::Copy(Place::local(sorted))],
                i64_ty,
                span,
            );
            let one = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(one),
                Rvalue::Use(Operand::Const(ConstValue::Int(1))),
                span,
            );
            self.emit_assign(
                Place::local(index),
                Rvalue::BinaryOp {
                    op: BinOp::Sub,
                    lhs: Operand::Copy(Place::local(len)),
                    rhs: Operand::Copy(Place::local(one)),
                },
                span,
            );
        } else {
            self.emit_assign(
                Place::local(index),
                Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                span,
            );
        }
        Some(self.option_from_vec_element(sorted, index, elem_ty, ty, span))
    }

    /// `xs.find(p)` where the element is wider than one slot.
    ///
    /// The word-slot form carries the found element in the `Option` payload
    /// itself, which an element of this width has no room for. Keeping the
    /// matches first gives storage of the element's own shape to answer from,
    /// and the payload is then minted exactly as a `Some(elem)` in source is:
    /// a heap copy the drop pass reclaims.
    pub(super) fn lower_find_wide_elem(
        &mut self,
        closure_local: Local,
        seq_arg: &HirExpr,
        elem_ty: Ty,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let vec_local = self.lower_iter_vec_arg(seq_arg)?;
        let kept_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let kept = self.emit_iter_combinator_call(
            "filter",
            ElemAbi::Ptr,
            None,
            vec![
                Operand::Copy(Place::local(closure_local)),
                Operand::Copy(Place::local(vec_local)),
            ],
            kept_ty,
            span,
        );
        let zero = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(zero),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let _ = bool_ty;
        Some(self.option_from_vec_element(kept, zero, elem_ty, ty, span))
    }

    /// `Some(v[index])` when `v` has an element to answer with, `None`
    /// otherwise, for an element wider than one slot.
    ///
    /// The payload is minted the way a `Some(elem)` written in source is: the
    /// backend heap-copies the element and the guarded layout registered here
    /// is what reclaims it, so the answer outlives the storage it was read
    /// from.
    pub(super) fn option_from_vec_element(
        &mut self,
        vec_local: Local,
        index: Local,
        elem_ty: Ty,
        ty: Ty,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(vec_local))],
            i64_ty,
            span,
        );
        let found = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(found),
            Rvalue::BinaryOp {
                op: BinOp::Gt,
                lhs: Operand::Copy(Place::local(len)),
                rhs: Operand::Copy(Place::local(index)),
            },
            span,
        );
        let kept = vec_local;
        let zero = index;
        // The payload is the element by construction, so where the call site
        // names no `Option` the carrier takes its payload from the element
        // rather than falling back to a word.
        let rty = if self.option_payload_of(ty).is_some() {
            self.result_repr_ty(ty)
        } else {
            self.option_payload_adt_ty(elem_ty)
        };
        let dest = self.fresh(rty);
        let some_block = self.new_block(span);
        let none_block = self.new_block(span);
        let join = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(found)),
            arms: vec![(0, none_block)],
            default: some_block,
        });

        self.set_current(some_block);
        let ref_ty = self.tcx.intern(TyKind::Ref {
            mutability: gossamer_types::Mutbl::Not,
            inner: elem_ty,
        });
        let slot = self.emit_combinator_call(
            "gos_rt_vec_get_ptr",
            vec![
                Operand::Copy(Place::local(kept)),
                Operand::Copy(Place::local(zero)),
            ],
            ref_ty,
            span,
        );
        let mut slot_place = Place::local(slot);
        slot_place.projection.push(crate::ir::Projection::Deref);
        let payload = self.fresh(elem_ty);
        self.emit_assign(
            Place::local(payload),
            Rvalue::Use(Operand::Copy(slot_place)),
            span,
        );
        // An aggregate payload outlives the vec it was read from, so the
        // backend's heap copy needs this element's guarded layout to reclaim it.
        if self.is_inline_aggregate_ty(elem_ty) {
            let _ = self.ensure_aggr_copy_meta(elem_ty);
        }
        let some_disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(some_disc),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Copy(Place::local(some_disc)),
                    Operand::Copy(Place::local(payload)),
                ],
            },
            span,
        );
        self.terminate(Terminator::Goto { target: join });

        self.set_current(none_block);
        let none_disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(none_disc),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Copy(Place::local(none_disc)),
                    Operand::Copy(Place::local(zero)),
                ],
            },
            span,
        );
        self.terminate(Terminator::Goto { target: join });

        self.set_current(join);
        dest
    }

    /// Carries the address-carrying marker from an adapter's upstream state to
    /// the state it produced, for an adapter that hands its slots through
    /// unchanged.
    pub(super) fn propagate_aggr_state(&mut self, upstream: Local, dest: Local) {
        if self.local_aggr_iter.contains(&upstream) {
            self.local_aggr_iter.insert(dest);
        }
    }

    /// Drains address-carrying state into a `Vec` of its elements, so a
    /// consumer that reads elements at their real width has storage to read.
    pub(super) fn drain_aggr_state(&mut self, state: Local, span: Span) -> Local {
        use gossamer_types::TyKind;
        let elem = self
            .sequence_elem_ty_of(self.locals[state.0 as usize].ty)
            .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
        let vec_ty = self.tcx.intern(TyKind::Vec(elem));
        let drained = self.emit_combinator_call(
            "gos_rt_lazy_iter_collect_aggr",
            vec![Operand::Copy(Place::local(state))],
            vec_ty,
            span,
        );
        self.tag_owned_elements(drained, elem, span);
        drained
    }

    /// Declares that the vec in `vec`, filled by a runtime shim with elements
    /// whose shares the shim handed over, owns what each `elem` element holds:
    /// a `String`, a nested container, a payload enum node, or the heap fields
    /// of an inline struct or tuple. A vec already tagged from its source keeps
    /// its layout; the tag records ownership and takes no shares itself.
    pub(crate) fn tag_owned_elements(&mut self, vec: Local, elem: Ty, span: Span) {
        use crate::lower::helpers::ElemOwnership;
        let (symbol, meta) = if matches!(self.tcx.kind_of(elem), gossamer_types::TyKind::String) {
            ("gos_rt_vec_mark_str_elems", None)
        } else {
            match crate::lower::helpers::elem_ownership(self.tcx, elem) {
                Some(
                    ownership @ (ElemOwnership::Owned(_)
                    | ElemOwnership::VecElems
                    | ElemOwnership::RcElems),
                ) => (ownership.symbol(), ownership.meta().map(str::to_string)),
                _ => return,
            }
        };
        let mut args = vec![Operand::Copy(Place::local(vec))];
        if let Some(meta) = meta {
            args.push(Operand::Const(ConstValue::Str(meta)));
        }
        let unit = self.tcx.unit();
        let sink = self.fresh(unit);
        self.emit_assign(
            Place::local(sink),
            Rvalue::CallIntrinsic { name: symbol, args },
            span,
        );
    }

    /// Lowers a combinator's sequence argument once, reporting whether the
    /// lowered value is genuinely lazy iterator state.
    ///
    /// Unlike [`Self::lower_iter_vec_arg`] this keeps iterator state as state;
    /// the caller decides between the lazy and the eager surface from the
    /// family this reports.
    pub(crate) fn lower_iter_seq_arg(
        &mut self,
        arg: &HirExpr,
    ) -> Option<(Local, Option<LazyElemFamily>)> {
        let (local, family) = self.lower_iter_seq_arg_raw(arg)?;
        // Address-carrying state reaches only the arms that pass slots to a
        // callback; everywhere else it is drained first, so a consumer never
        // reads an address as an element.
        if family == Some(LazyElemFamily::Aggr) {
            let drained = self.drain_aggr_state(local, arg.span);
            return Some((drained, None));
        }
        Some((local, family))
    }

    /// Like [`Self::lower_iter_seq_arg`], keeping address-carrying state as
    /// state for a caller that hands each slot to a callback.
    pub(crate) fn lower_iter_seq_arg_raw(
        &mut self,
        arg: &HirExpr,
    ) -> Option<(Local, Option<LazyElemFamily>)> {
        use gossamer_types::TyKind;
        let raw = self.lower_expr(arg)?;
        let raw_ty = self.locals[raw.0 as usize].ty;
        let local = match self.tcx.kind_of(raw_ty).clone() {
            TyKind::Array { elem, len } => self.coerce_array_to_vec(raw, elem, len, arg.span),
            _ => raw,
        };
        let family = self.lowered_lazy_family(local);
        // The pair state `zip` and `enumerate` build advances through a helper
        // of its own that no word adapter reads, so its pairs ride on as the
        // counted blobs an address-carrying stream hands out.
        if family == Some(LazyElemFamily::PairWord) {
            let ty = self.locals[local.0 as usize].ty;
            let handle = self.emit_combinator_call(
                "gos_rt_lazy_iter_pair_blobs",
                vec![Operand::Copy(Place::local(local))],
                ty,
                arg.span,
            );
            self.local_aggr_iter.insert(handle);
            return Some((handle, Some(LazyElemFamily::Aggr)));
        }
        Some((local, family))
    }

    /// Borrows a lowered `GosVec` as lazy state tagged with its own element
    /// family, or returns `None` when the elements are too wide for the slot.
    pub(super) fn borrow_lazy_state(
        &mut self,
        source: Local,
        span: Span,
        allow_aggr: bool,
    ) -> Option<(Local, LazyElemFamily)> {
        use gossamer_types::TyKind;
        let family = self.lazy_iter_source_family(self.locals[source.0 as usize].ty)?;
        // An address-carrying stream is only for a consumer that hands each
        // slot to a callback; every other one reads elements at their real
        // width from storage, which is the eager surface.
        if family == LazyElemFamily::Aggr && !allow_aggr {
            return None;
        }
        let source_elem = self.sequence_elem_ty_of(self.locals[source.0 as usize].ty);
        let elem_ty = match family {
            LazyElemFamily::Float => self.tcx.float_ty(gossamer_types::FloatTy::F64),
            // The address rides the slot, but what the consumer reads through
            // it is the element, so the state names the element type.
            LazyElemFamily::Aggr => source_elem?,
            // A counted element keeps its type, so every consumer downstream
            // picks the helpers that account for the share each pull carries.
            LazyElemFamily::Ptr => self.tcx.string_ty(),
            _ => self.tcx.int_ty(gossamer_types::IntTy::I64),
        };
        let iter_ty = self.tcx.intern(TyKind::Iterator(elem_ty));
        let helper = family.vec_source_symbol();
        let handle = self.emit_combinator_call(
            helper,
            vec![Operand::Copy(Place::local(source))],
            iter_ty,
            span,
        );
        if family == LazyElemFamily::Aggr {
            self.local_aggr_iter.insert(handle);
        }
        Some((handle, family))
    }

    /// Lowers an edition-2027 scalar iterator input. Existing iterator state
    /// passes through unchanged; Vec, slice, and fixed-array sources become a
    /// retained borrowed runtime iterator handle before an adapter consumes
    /// them.
    ///
    /// The borrow helper is chosen from the source's own element family, so
    /// the handle the adapter receives is tagged with the class its elements
    /// really have.
    pub(super) fn lower_lazy_iter_source(&mut self, arg: &HirExpr) -> Option<Local> {
        self.lower_lazy_iter_source_classed(arg, false)
    }

    /// Lowers a lazy source for a combinator that hands each slot to a
    /// callback, so a multi-slot element can ride the stream as its address.
    pub(super) fn lower_lazy_iter_source_aggr(&mut self, arg: &HirExpr) -> Option<Local> {
        self.lower_lazy_iter_source_classed(arg, true)
    }
}
