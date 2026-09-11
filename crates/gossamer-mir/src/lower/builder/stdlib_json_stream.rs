use gossamer_hir::HirExpr;
use gossamer_lex::Span;
use gossamer_types::{Ty, TyKind};

use crate::ir::{BinOp, ConstValue, Local, Operand, Place, Projection, Rvalue, Terminator};

use super::Builder;

impl<'a> Builder<'a> {
    /// Lowers a compact `json::encode(value)` into token writes: the walk the
    /// tree builder performs, writing each scalar where the tree builder
    /// would box it. Answers `None` for a value whose shape that walk does
    /// not cover, so the caller keeps its own lowering for it. The member
    /// order and every byte match the tree renderer's.
    pub(crate) fn lower_json_encode_stream(
        &mut self,
        args: &[HirExpr],
        value_ty: Ty,
        span: Span,
        pretty: bool,
    ) -> Option<Local> {
        if !self.json_streamable_ty(value_ty) {
            return None;
        }
        let value_local = self.lower_expr(&args[0])?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let ctor = if pretty {
            "gos_rt_json_writer_new_pretty"
        } else {
            "gos_rt_json_writer_new"
        };
        let writer = self.emit_rt_call(ctor, vec![], i64_ty, span);
        self.stream_json_value(writer, value_local, value_ty, span);
        let string_ty = self.tcx.string_ty();
        Some(self.emit_rt_call("gos_rt_json_writer_finish", vec![writer], string_ty, span))
    }

    /// Whether the tree builder walks a value of this type, so the token
    /// writer can too: the two agree on which shapes render and which
    /// render as `null`.
    fn json_streamable_ty(&self, ty: Ty) -> bool {
        let ty = self.peel_ref_ty(ty);
        match self.tcx.kind_of(ty) {
            TyKind::JsonValue
            | TyKind::Int(_)
            | TyKind::Bool
            | TyKind::Float(_)
            | TyKind::String => true,
            TyKind::Adt { def, .. } => {
                if self.is_result_or_option_adt(ty) {
                    return self.carrier_payload_tys(ty).iter().all(|&payload| {
                        matches!(self.tcx.kind_of(payload), TyKind::Var(_) | TyKind::Error)
                            || self.json_streamable_ty(payload)
                    });
                }
                if let Some(variants) = self.json_enum_variants(ty) {
                    return variants
                        .iter()
                        .all(|(_, tys)| tys.iter().all(|&t| self.json_streamable_ty(t)));
                }
                self.json_struct_fields(*def).is_some()
            }
            TyKind::HashMap { key, .. } => matches!(self.tcx.kind_of(*key), TyKind::String),
            TyKind::Tuple(elems) => elems.iter().all(|t| self.json_streamable_ty(*t)),
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => {
                self.json_streamable_ty(*elem)
            }
            _ => false,
        }
    }

    /// The fields of a struct the tree builder walks, as `(index, name,
    /// type)` in the order its members render: the tree keeps members by
    /// name, so they leave in byte order of the name.
    fn json_struct_fields(&self, def: gossamer_resolve::DefId) -> Option<Vec<(u32, String, Ty)>> {
        let struct_name = self.struct_defs.get(&def)?;
        let field_names = self.structs.get(struct_name)?;
        let field_tys = self.tcx.struct_field_tys(def)?;
        if field_names.len() != field_tys.len() || field_names.is_empty() {
            return None;
        }
        let mut fields: Vec<(u32, String, Ty)> = field_names
            .iter()
            .zip(field_tys.iter())
            .enumerate()
            .map(|(i, (name, ty))| (i as u32, name.clone(), *ty))
            .collect();
        fields.sort_by(|a, b| a.1.as_bytes().cmp(b.1.as_bytes()));
        Some(fields)
    }

    /// One runtime call whose result lands in a fresh local of `dest_ty`.
    fn emit_rt_call(&mut self, name: &str, args: Vec<Local>, dest_ty: Ty, span: Span) -> Local {
        let dest = self.fresh(dest_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name.to_string())),
            args: args
                .into_iter()
                .map(|l| Operand::Copy(Place::local(l)))
                .collect(),
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }

    fn emit_writer_call(&mut self, name: &str, args: Vec<Local>, span: Span) {
        let unit_ty = self.tcx.unit();
        self.emit_rt_call(name, args, unit_ty, span);
    }

    /// Writes the value in `local` as the next token(s). `ty` is a shape
    /// [`Self::json_streamable_ty`] accepts.
    fn stream_json_value(&mut self, writer: Local, local: Local, ty: Ty, span: Span) {
        let ty = self.peel_ref_ty(ty);
        match self.tcx.kind_of(ty).clone() {
            TyKind::JsonValue => {
                self.emit_writer_call("gos_rt_json_writer_value", vec![writer, local], span)
            }
            TyKind::Int(_) => {
                self.emit_writer_call("gos_rt_json_writer_i64", vec![writer, local], span)
            }
            TyKind::Bool => {
                self.emit_writer_call("gos_rt_json_writer_bool", vec![writer, local], span)
            }
            TyKind::Float(_) => {
                self.emit_writer_call("gos_rt_json_writer_f64", vec![writer, local], span)
            }
            TyKind::String => {
                self.emit_writer_call("gos_rt_json_writer_str", vec![writer, local], span)
            }
            TyKind::Adt { def, .. } => {
                if self.is_result_or_option_adt(ty) {
                    self.stream_json_carrier(writer, local, ty, span);
                } else if self.json_enum_variants(ty).is_some() {
                    self.stream_json_enum(writer, local, ty, span);
                } else {
                    self.stream_json_struct(writer, local, def, span);
                }
            }
            TyKind::HashMap { value, .. } => self.stream_json_map(writer, local, value, span),
            TyKind::Tuple(elem_tys) => {
                self.emit_writer_call("gos_rt_json_writer_begin_array", vec![writer], span);
                for (i, elem_ty) in elem_tys.iter().enumerate() {
                    let field_local = self.fresh(*elem_ty);
                    self.emit_assign(
                        Place::local(field_local),
                        Rvalue::Use(Operand::Copy(Place {
                            local,
                            projection: vec![Projection::Field(i as u32)],
                        })),
                        span,
                    );
                    self.stream_json_value(writer, field_local, *elem_ty, span);
                }
                self.emit_writer_call("gos_rt_json_writer_end_array", vec![writer], span);
            }
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => {
                self.stream_json_seq(writer, local, ty, elem, span);
            }
            _ => self.emit_writer_call("gos_rt_json_writer_null", vec![writer], span),
        }
    }

    /// The payload types a `Result<T, E>` or `Option<T>` carries, in
    /// discriminant order: `[T, E]` for a result, `[T]` for an option.
    fn carrier_payload_tys(&self, ty: Ty) -> Vec<Ty> {
        let first = self.adt_generic_at(ty, 0);
        let second = if self.is_option_adt(ty) {
            None
        } else {
            self.adt_generic_at(ty, 1)
        };
        first.into_iter().chain(second).collect()
    }

    /// The variants of a user enum as `(name, payload types)` in declaration
    /// order, or `None` when `ty` names no enum this lowering can walk.
    fn json_enum_variants(&self, ty: Ty) -> Option<Vec<(String, Vec<Ty>)>> {
        let enum_name = self.enum_index_name_of(ty)?;
        let names = self.enums.by_enum.get(&enum_name)?;
        if names.is_empty() {
            return None;
        }
        Some(
            names
                .iter()
                .map(|variant| {
                    let tys = self
                        .enums
                        .field_tys_of(&enum_name, variant)
                        .unwrap_or_default();
                    (variant.clone(), tys)
                })
                .collect(),
        )
    }

    /// Writes a `Result` / `Option`: the payload of the arm the discriminant
    /// names, and `null` for `None`. The two-word carrier holds the
    /// discriminant beside the payload, so one read answers which arm it is.
    fn stream_json_carrier(&mut self, writer: Local, local: Local, ty: Ty, span: Span) {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let payload_tys = self.carrier_payload_tys(ty);
        let is_option = self.is_option_adt(ty);
        let disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(disc),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_disc",
                args: vec![Operand::Copy(Place::local(local))],
            },
            span,
        );
        let ok_block = self.new_block(span);
        let else_block = self.new_block(span);
        let join = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(disc)),
            arms: vec![(0, ok_block)],
            default: else_block,
        });

        self.set_current(ok_block);
        match payload_tys.first().copied() {
            Some(payload_ty) => {
                let payload = self.read_carrier_payload(local, payload_ty, span);
                self.stream_json_member(writer, payload, payload_ty, span);
            }
            None => self.emit_writer_call("gos_rt_json_writer_null", vec![writer], span),
        }
        self.terminate(Terminator::Goto { target: join });

        self.set_current(else_block);
        // `None` is the absent value JSON spells `null`; an `Err` carries a
        // payload of its own, which renders the way the `Ok` side does.
        match payload_tys.get(1).copied().filter(|_| !is_option) {
            Some(err_ty) => {
                let payload = self.read_carrier_payload(local, err_ty, span);
                self.stream_json_member(writer, payload, err_ty, span);
            }
            None => self.emit_writer_call("gos_rt_json_writer_null", vec![writer], span),
        }
        self.terminate(Terminator::Goto { target: join });
        self.set_current(join);
    }

    /// Reads a two-word carrier's payload as `payload_ty`, through the
    /// extractor that type's representation needs.
    fn read_carrier_payload(&mut self, carrier: Local, payload_ty: Ty, span: Span) -> Local {
        let getter = if matches!(self.tcx.kind_of(payload_ty), TyKind::Float(_)) {
            "gos_rt_result_payload_f64"
        } else if self.is_by_value_enum_ty(payload_ty) {
            "gos_rt_result_payload_i128"
        } else {
            "gos_rt_result_payload"
        };
        let payload = self.fresh(payload_ty);
        self.emit_assign(
            Place::local(payload),
            Rvalue::CallIntrinsic {
                name: getter,
                args: vec![Operand::Copy(Place::local(carrier))],
            },
            span,
        );
        payload
    }

    /// Writes a user enum: a variant carrying nothing is its own name, one
    /// carrying a single value is that value, and one carrying several is
    /// the array of them.
    fn stream_json_enum(&mut self, writer: Local, local: Local, ty: Ty, span: Span) {
        let Some(variants) = self.json_enum_variants(ty) else {
            self.emit_writer_call("gos_rt_json_writer_null", vec![writer], span);
            return;
        };
        let Some(enum_name) = self.enum_index_name_of(ty) else {
            self.emit_writer_call("gos_rt_json_writer_null", vec![writer], span);
            return;
        };
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let string_ty = self.tcx.string_ty();
        let has_payload = self.enums.enum_has_any_payload(&enum_name);

        // A payload-bearing enum's value points at `[disc, p0, ..]`; a
        // unit-only enum's value is the discriminant itself.
        let disc = self.fresh(i64_ty);
        if has_payload {
            let intrinsic = if self.enum_repr_tagged(&enum_name) {
                "gos_enum_disc_tag"
            } else {
                "gos_enum_disc"
            };
            self.emit_assign(
                Place::local(disc),
                Rvalue::CallIntrinsic {
                    name: intrinsic,
                    args: vec![Operand::Copy(Place::local(local))],
                },
                span,
            );
        } else {
            self.emit_assign(
                Place::local(disc),
                Rvalue::Use(Operand::Copy(Place::local(local))),
                span,
            );
        }

        let join = self.new_block(span);
        let arm_blocks: Vec<_> = variants.iter().map(|_| self.new_block(span)).collect();
        let default = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(disc)),
            arms: arm_blocks
                .iter()
                .enumerate()
                .map(|(i, block)| (i as i128, *block))
                .collect(),
            default,
        });

        self.set_current(default);
        self.emit_writer_call("gos_rt_json_writer_null", vec![writer], span);
        self.terminate(Terminator::Goto { target: join });

        for (block, (name, payload_tys)) in arm_blocks.into_iter().zip(variants) {
            self.set_current(block);
            let offsets = self.variant_payload_offsets(&payload_tys, payload_tys.len());
            match payload_tys.len() {
                0 => {
                    let name_local = self.fresh(string_ty);
                    self.emit_assign(
                        Place::local(name_local),
                        Rvalue::Use(Operand::Const(ConstValue::Str(name))),
                        span,
                    );
                    self.emit_writer_call("gos_rt_json_writer_str", vec![writer, name_local], span);
                }
                1 => {
                    let payload = self.read_enum_payload(local, payload_tys[0], offsets[0], span);
                    self.stream_json_member(writer, payload, payload_tys[0], span);
                }
                _ => {
                    self.emit_writer_call("gos_rt_json_writer_begin_array", vec![writer], span);
                    for (i, payload_ty) in payload_tys.iter().enumerate() {
                        let payload = self.read_enum_payload(local, *payload_ty, offsets[i], span);
                        self.stream_json_member(writer, payload, *payload_ty, span);
                    }
                    self.emit_writer_call("gos_rt_json_writer_end_array", vec![writer], span);
                }
            }
            self.terminate(Terminator::Goto { target: join });
        }
        self.set_current(join);
    }

    /// Reads one payload word of a heap enum value at its byte offset.
    fn read_enum_payload(&mut self, enum_local: Local, ty: Ty, offset: i64, span: Span) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let off_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(off_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(offset)))),
            span,
        );
        let payload = self.fresh(ty);
        self.emit_assign(
            Place::local(payload),
            Rvalue::CallIntrinsic {
                name: "gos_enum_load",
                args: vec![
                    Operand::Copy(Place::local(enum_local)),
                    Operand::Copy(Place::local(off_local)),
                ],
            },
            span,
        );
        payload
    }

    /// A field or member whose shape the walk covers is written; any other
    /// renders as `null`, as it does in the tree.
    fn stream_json_member(&mut self, writer: Local, local: Local, ty: Ty, span: Span) {
        if self.json_streamable_ty(ty) {
            self.stream_json_value(writer, local, ty, span);
        } else {
            self.emit_writer_call("gos_rt_json_writer_null", vec![writer], span);
        }
    }

    fn stream_json_struct(
        &mut self,
        writer: Local,
        struct_local: Local,
        def: gossamer_resolve::DefId,
        span: Span,
    ) {
        let Some(fields) = self.json_struct_fields(def) else {
            self.emit_writer_call("gos_rt_json_writer_null", vec![writer], span);
            return;
        };
        let string_ty = self.tcx.string_ty();
        self.emit_writer_call("gos_rt_json_writer_begin_object", vec![writer], span);
        for (index, name, fty) in fields {
            let name_local = self.fresh(string_ty);
            self.emit_assign(
                Place::local(name_local),
                Rvalue::Use(Operand::Const(ConstValue::Str(name))),
                span,
            );
            self.emit_writer_call("gos_rt_json_writer_key", vec![writer, name_local], span);
            let field_local = self.fresh(fty);
            self.emit_assign(
                Place::local(field_local),
                Rvalue::Use(Operand::Copy(Place {
                    local: struct_local,
                    projection: vec![Projection::Field(index)],
                })),
                span,
            );
            self.stream_json_member(writer, field_local, fty, span);
        }
        self.emit_writer_call("gos_rt_json_writer_end_object", vec![writer], span);
    }

    /// Emits `for counter in 0..len { body(counter) }` over a length the
    /// caller has read, leaving the current block after the loop.
    fn emit_counted_loop(
        &mut self,
        len_local: Local,
        span: Span,
        body: impl FnOnce(&mut Self, Local),
    ) {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
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
        body(self, counter);
        self.terminate(Terminator::Goto { target: step_block });

        self.set_current(step_block);
        self.emit_assign(
            Place::local(counter),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Const(ConstValue::Int(1)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });
        self.set_current(exit);
    }

    fn stream_json_seq(
        &mut self,
        writer: Local,
        seq_local: Local,
        seq_ty: Ty,
        elem_ty: Ty,
        span: Span,
    ) {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let elem_ty = self.peel_ref_ty(elem_ty);
        let seq_local = if let TyKind::Array { elem, len } = self.tcx.kind_of(seq_ty).clone() {
            self.coerce_array_to_vec(seq_local, elem, len, span)
        } else {
            seq_local
        };
        let len_local = self.emit_rt_call("gos_rt_vec_len", vec![seq_local], i64_ty, span);
        // An inline aggregate element is its slot address, and its field
        // reads take their offsets from there; every other element the walk
        // reaches occupies one slot, read as a word.
        let inline_aggregate = matches!(
            self.tcx.kind_of(elem_ty),
            TyKind::Tuple(_) | TyKind::Array { .. }
        ) || matches!(
            self.tcx.kind_of(elem_ty),
            TyKind::Adt { def, .. } if self.tcx.struct_field_tys(*def).is_some()
        );
        self.emit_writer_call("gos_rt_json_writer_begin_array", vec![writer], span);
        self.emit_counted_loop(len_local, span, |b, counter| {
            let (reader, read_ty) = if inline_aggregate {
                ("gos_rt_vec_get_ptr", i64_ty)
            } else {
                ("gos_rt_vec_get_i64", elem_ty)
            };
            let elem_slot = b.emit_rt_call(reader, vec![seq_local, counter], read_ty, span);
            let elem_local = if inline_aggregate {
                let l = b.fresh(elem_ty);
                b.emit_assign(
                    Place::local(l),
                    Rvalue::Use(Operand::Copy(Place::local(elem_slot))),
                    span,
                );
                l
            } else {
                elem_slot
            };
            b.stream_json_value(writer, elem_local, elem_ty, span);
        });
        self.emit_writer_call("gos_rt_json_writer_end_array", vec![writer], span);
    }

    /// A string-keyed map's members, one per key in byte order of the key,
    /// which is the order the tree keeps them in.
    fn stream_json_map(&mut self, writer: Local, map_local: Local, value_ty: Ty, span: Span) {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let string_ty = self.tcx.string_ty();
        let keys_ty = self.tcx.intern(TyKind::Vec(string_ty));
        let value_ty = self.peel_ref_ty(value_ty);
        let keys = self.emit_rt_call("gos_rt_map_keys_vec", vec![map_local], keys_ty, span);
        self.emit_writer_call("gos_rt_vec_sort_str", vec![keys], span);
        let len_local = self.emit_rt_call("gos_rt_vec_len", vec![keys], i64_ty, span);
        let value_reader = if matches!(self.tcx.kind_of(value_ty), TyKind::String) {
            "gos_rt_map_get_str_str"
        } else {
            "gos_rt_map_get_str_i64"
        };
        self.emit_writer_call("gos_rt_json_writer_begin_object", vec![writer], span);
        self.emit_counted_loop(len_local, span, |b, counter| {
            // The key word is the member's name text, read as a word so the
            // writer copies the bytes it points at.
            let key_local = b.emit_rt_call("gos_rt_vec_get_i64", vec![keys, counter], i64_ty, span);
            b.emit_writer_call("gos_rt_json_writer_key", vec![writer, key_local], span);
            let value_local =
                b.emit_rt_call(value_reader, vec![map_local, key_local], value_ty, span);
            b.stream_json_member(writer, value_local, value_ty, span);
        });
        self.emit_writer_call("gos_rt_json_writer_end_object", vec![writer], span);
        self.emit_writer_call("gos_rt_vec_free", vec![keys], span);
        // The sentinel keeps the drop-at-return pass from freeing the keys a
        // second time.
        self.emit_assign(
            Place::local(keys),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
    }
}
