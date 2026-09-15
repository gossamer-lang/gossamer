#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::cognitive_complexity,
    clippy::unused_io_amount,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
#![forbid(unsafe_code)]
use super::*;
use std::collections::HashMap;
use std::fmt::Write as _;

use crate::BuildError;
use crate::ty::packed_byte_array_len;
use anyhow::Result;
use gossamer_abi as abi;
use gossamer_mir::{
    BasicBlock, BinOp, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, Statement,
    StatementKind, Terminator, UnOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};

fn repeated_vec_child_words(tcx: &TyCtxt, ty: Ty) -> Vec<u32> {
    fn walk(tcx: &TyCtxt, ty: Ty, base: u32, out: &mut Vec<u32>, depth: u8) {
        if depth > 16 {
            return;
        }
        match tcx.kind_of(ty) {
            TyKind::Vec(_) => out.push(base),
            TyKind::Tuple(fields) => {
                let mut word = base;
                for field in fields {
                    walk(tcx, *field, word, out, depth + 1);
                    word = word.saturating_add(slot_count(tcx, *field).unwrap_or(1));
                }
            }
            TyKind::Adt { def, substs } if !tcx.is_inline_enum_ty(ty) => {
                if let Some(fields) = tcx.adt_field_tys(*def, substs) {
                    let mut word = base;
                    for field in fields {
                        walk(tcx, *field, word, out, depth + 1);
                        word = word.saturating_add(slot_count(tcx, *field).unwrap_or(1));
                    }
                }
            }
            TyKind::Array { elem, len } => {
                let stride = slot_count(tcx, *elem).unwrap_or(1);
                if let Ok(count) = u32::try_from(len.to_usize()) {
                    for index in 0..count {
                        walk(
                            tcx,
                            *elem,
                            base.saturating_add(index.saturating_mul(stride)),
                            out,
                            depth + 1,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    walk(tcx, ty, 0, &mut out, 0);
    out
}

impl<'a> Lowerer<'a> {
    /// Populates an aggregate stack slot (the destination
    /// `place`'s flat layout) with each operand in order.
    /// Each operand occupies one i64-wide slot for scalar
    /// fields; nested aggregates add their own slot count.
    pub(crate) fn emit_aggregate_store(
        &mut self,
        place: &Place,
        operands: &[Operand],
    ) -> Result<(), BuildError> {
        let base = if place.projection.is_empty() {
            local_slot(place.local)
        } else {
            self.lower_place_address(place)
        };
        let place_ty = self.place_leaf_ty(place);
        let packed_bytes = packed_byte_array_len(self.tcx, place_ty).is_some();
        let tbaa = self.aggregate_dest_tbaa(place);
        if let Some(layout) = self.tcx.packed_layout(place_ty) {
            return self.emit_packed_struct_store(&base, place_ty, &layout, operands, tbaa);
        }
        let mut slot_idx = 0u32;
        for operand in operands {
            let op_ty = self.operand_ty(operand);
            let mut op_slots = slot_count(self.tcx, op_ty).unwrap_or(1);
            // `operand_ty` falls back to the return-slot type for
            // `Const(Str)` and FnRef operands when their own type
            // isn't directly representable. When the function
            // returns a multi-slot aggregate (struct return), that
            // fallback inflates `op_slots` to the return type's
            // slot count, sending a 1-slot string-literal operand
            // through the multi-slot memcpy branch which then
            // bails because the source is a Const, not a place.
            // Every Const* and FnRef value is exactly 1 slot in the
            // flat ABI (the underlying constant produces a single
            // i64/double/ptr). Force `op_slots = 1` to match.
            if matches!(operand, Operand::Const(_) | Operand::FnRef { .. }) {
                op_slots = 1;
            }
            if op_slots == 0 {
                // A zero-width element still occupies the slot the
                // aggregate's layout reserves for it - `slot_count` and
                // `field_slot_offset` floor every element at one word - so
                // the slot is zeroed and the next operand lands past it.
                let dst = self.fresh();
                if packed_bytes {
                    writeln!(
                        self.out,
                        "  {dst} = getelementptr i8, ptr {base}, i64 {slot_idx}"
                    )
                    .unwrap();
                    writeln!(self.out, "  store i8 0, ptr {dst}{tbaa}").unwrap();
                } else {
                    writeln!(
                        self.out,
                        "  {dst} = getelementptr i64, ptr {base}, i64 {slot_idx}"
                    )
                    .unwrap();
                    writeln!(self.out, "  store i64 0, ptr {dst}{tbaa}").unwrap();
                }
                slot_idx += 1;
                continue;
            }
            // A struct/tuple field whose own value is a single-slot
            // aggregate (e.g. a nested `struct Inner { tag: i64 }`) must be
            // copied by value into the parent's slot - `lower_operand` of an
            // aggregate place yields its *address*, so the scalar-store branch
            // would embed a pointer and the parent would read it back inline as
            // garbage. Route 1-slot aggregate copies through the memcpy branch.
            let op_is_aggregate_copy =
                matches!(operand, Operand::Copy(_)) && is_aggregate(self.tcx, op_ty);
            if op_slots == 1 && !op_is_aggregate_copy {
                let v = self.lower_operand(operand)?;
                let op_llvm = self.operand_llvm_ty(operand);
                let dst = self.fresh();
                if packed_bytes {
                    writeln!(
                        self.out,
                        "  {dst} = getelementptr i8, ptr {base}, i64 {slot_idx}"
                    )
                    .unwrap();
                    let byte = self.fresh();
                    writeln!(self.out, "  {byte} = trunc {op_llvm} {v} to i8").unwrap();
                    writeln!(self.out, "  store i8 {byte}, ptr {dst}{tbaa}").unwrap();
                } else {
                    writeln!(
                        self.out,
                        "  {dst} = getelementptr i64, ptr {base}, i64 {slot_idx}"
                    )
                    .unwrap();
                    // A slot is a whole word and is read back as one, so a
                    // scalar narrower than that is widened to fill it: a
                    // `char` or a `bool` written at its own width would leave
                    // the rest of the slot holding whatever the frame did.
                    match self.slot_word_of(operand, &op_llvm, &v) {
                        Some(word) => {
                            writeln!(self.out, "  store i64 {word}, ptr {dst}{tbaa}").unwrap();
                        }
                        None => {
                            writeln!(self.out, "  store {op_llvm} {v}, ptr {dst}{tbaa}").unwrap();
                        }
                    }
                }
            } else {
                // Nested aggregate. The operand may be either a
                // bare-local copy (`Operand::Copy(p)` with empty
                // projection - the original supported shape) or
                // a projected place (`base.inner`, `tuple.0`,
                // etc.). For both, the source is an in-memory
                // place whose address we compute through
                // `lower_place_address`, then memcpy `op_slots *
                // 8` bytes into the destination slot. The
                // memcpy expands at `llc` time to the platform's
                // best sequence.
                let src_place = match operand {
                    Operand::Copy(p) => p,
                    Operand::Const(_) | Operand::FnRef { .. } => {
                        // Genuinely multi-slot const / FnRef are
                        // not surfaced by the current MIR: every
                        // multi-slot constructor is materialised
                        // into a temp local first, and FnRef is
                        // always 1-slot. The Unit-typed
                        // mis-classification is handled by the
                        // `op_slots == 0` recovery above so this
                        // branch is unreachable on well-formed
                        // input; we surface it as a hard error
                        // rather than a silent miscompile.
                        return Err(BuildError::InternalLoweringBug(
                            "nested aggregate operand must be a Copy(place); \
                             multi-slot constants and FnRef values are not \
                             materialised through the aggregate-store path",
                        ));
                    }
                };
                let src = self.lower_place_address(src_place);
                let dst = self.fresh();
                writeln!(
                    self.out,
                    "  {dst} = getelementptr i64, ptr {base}, i64 {slot_idx}"
                )
                .unwrap();
                let bytes = u64::from(op_slots) * 8;
                writeln!(
                    self.out,
                    "  call void @llvm.memcpy.p0.p0.i64(ptr {dst}, ptr {src}, i64 {bytes}, i1 false)"
                )
                .unwrap();
            }
            slot_idx += op_slots;
        }
        Ok(())
    }

    /// Stores a packed struct literal: every word zeroed, so padding between
    /// narrow fields is part of an equal value's bytes, then each field at its
    /// own offset and width, and a nested struct copied as its bytes.
    fn emit_packed_struct_store(
        &mut self,
        base: &str,
        place_ty: Ty,
        layout: &gossamer_types::PackedLayout,
        operands: &[Operand],
        tbaa: &str,
    ) -> Result<(), BuildError> {
        for word in 0..layout.size / 8 {
            let dst = self.fresh();
            writeln!(
                self.out,
                "  {dst} = getelementptr i64, ptr {base}, i64 {word}"
            )
            .unwrap();
            writeln!(self.out, "  store i64 0, ptr {dst}{tbaa}").unwrap();
        }
        let field_tys: Vec<Ty> = match self.tcx.kind(place_ty) {
            Some(TyKind::Adt { def, substs }) => self
                .tcx
                .adt_field_tys(*def, substs)
                .map(<[Ty]>::to_vec)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        for (index, operand) in operands.iter().enumerate() {
            let offset = layout.field_offsets.get(index).copied().unwrap_or(0);
            let bytes = layout.field_bytes.get(index).copied().unwrap_or(8);
            let dst = self.fresh();
            writeln!(
                self.out,
                "  {dst} = getelementptr i8, ptr {base}, i64 {offset}"
            )
            .unwrap();
            let op_ty = self.operand_ty(operand);
            if let Operand::Copy(src_place) = operand
                && is_aggregate(self.tcx, op_ty)
            {
                let src = self.lower_place_address(src_place);
                writeln!(
                    self.out,
                    "  call void @llvm.memcpy.p0.p0.i64(ptr {dst}, ptr {src}, i64 {bytes}, i1 false)"
                )
                .unwrap();
                continue;
            }
            let value = self.lower_operand(operand)?;
            let op_llvm = self.operand_llvm_ty(operand);
            let narrow = match field_tys.get(index).and_then(|ty| self.tcx.kind(*ty)) {
                Some(TyKind::Int(IntTy::I8 | IntTy::U8)) => Some("i8"),
                Some(TyKind::Int(IntTy::I16 | IntTy::U16)) => Some("i16"),
                Some(TyKind::Int(IntTy::I32 | IntTy::U32)) => Some("i32"),
                _ => None,
            };
            match narrow {
                Some(storage) if op_llvm == "i64" => {
                    let cut = self.fresh();
                    writeln!(self.out, "  {cut} = trunc i64 {value} to {storage}").unwrap();
                    writeln!(self.out, "  store {storage} {cut}, ptr {dst}{tbaa}").unwrap();
                }
                _ => {
                    writeln!(self.out, "  store {op_llvm} {value}, ptr {dst}{tbaa}").unwrap();
                }
            }
        }
        Ok(())
    }

    /// The whole-word form of a scalar operand narrower than a slot, or
    /// `None` when the value already fills one. A signed integer keeps its
    /// sign so the word reads back as the value written; every other narrow
    /// scalar - `char`, `bool`, an unsigned integer - is a magnitude.
    fn slot_word_of(&mut self, operand: &Operand, op_llvm: &str, value: &str) -> Option<String> {
        let width: u32 = op_llvm.strip_prefix('i')?.parse().ok()?;
        if width >= 64 {
            return None;
        }
        // Only a signed integer carries a sign into the word. A `bool` and a
        // `char` are magnitudes, and `i1` is always the former whatever the
        // operand's type lookup answers.
        let signed = op_llvm != "i1"
            && !matches!(
                operand,
                Operand::Const(ConstValue::Bool(_) | ConstValue::Char(_))
            )
            && matches!(
                self.tcx.kind(self.operand_ty(operand)),
                Some(TyKind::Int(int_ty)) if !int_ty_is_unsigned_llvm(*int_ty)
            );
        let op = if signed { "sext" } else { "zext" };
        let word = self.fresh();
        writeln!(self.out, "  {word} = {op} {op_llvm} {value} to i64").unwrap();
        Some(word)
    }

    /// `[value; count]` - fills `count` slots with the same
    /// scalar `value`. Small counts are unrolled (`llc -O3`
    /// later SLP-vectorises); larger counts drop into a
    /// tight loop to keep module text small.
    pub(crate) fn emit_repeat_store(
        &mut self,
        place: &Place,
        value: &Operand,
        count: u64,
    ) -> Result<(), BuildError> {
        let base = if place.projection.is_empty() {
            local_slot(place.local)
        } else {
            self.lower_place_address(place)
        };
        let place_ty = self.place_leaf_ty(place);
        let packed_bytes = packed_byte_array_len(self.tcx, place_ty).is_some();
        let tbaa = self.aggregate_dest_tbaa(place);
        let op_ty = self.operand_ty(value);
        let mut op_slots = slot_count(self.tcx, op_ty).unwrap_or(1);
        if matches!(value, Operand::Const(_) | Operand::FnRef { .. }) {
            op_slots = 1;
        }
        let op_is_aggregate_copy =
            matches!(value, Operand::Copy(_)) && is_aggregate(self.tcx, op_ty);
        if op_slots > 1 || op_is_aggregate_copy {
            // An aggregate element occupies `op_slots` inline words per
            // repetition ([[0; 3]; 3] is 9 contiguous words): copy the
            // element's flat payload into each repetition's slot range,
            // never a single scalar store of its address.
            let src_place = match value {
                Operand::Copy(p) => p,
                Operand::Const(_) | Operand::FnRef { .. } => {
                    // Multi-slot constructors are materialised into a temp
                    // local before Repeat, so a non-place element here is
                    // malformed input; fail loudly over a silent miscompile.
                    return Err(BuildError::InternalLoweringBug(
                        "aggregate repeat element must be a Copy(place)",
                    ));
                }
            };
            let src = self.lower_place_address(src_place);
            let bytes = u64::from(op_slots) * 8;
            let vec_child_words = repeated_vec_child_words(self.tcx, op_ty);
            if !vec_child_words.is_empty() {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_clone");
            }
            if count <= 16 {
                for i in 0..count {
                    let dst = self.fresh();
                    let slot = i * u64::from(op_slots);
                    writeln!(
                        self.out,
                        "  {dst} = getelementptr i64, ptr {base}, i64 {slot}"
                    )
                    .unwrap();
                    writeln!(
                        self.out,
                        "  call void @llvm.memcpy.p0.p0.i64(ptr {dst}, ptr {src}, i64 {bytes}, i1 false)"
                    )
                    .unwrap();
                    for child_word in &vec_child_words {
                        let child_src = self.fresh();
                        let child = self.fresh();
                        let cloned = self.fresh();
                        let child_dst = self.fresh();
                        writeln!(
                            self.out,
                            "  {child_src} = getelementptr i64, ptr {src}, i64 {child_word}"
                        )
                        .unwrap();
                        writeln!(self.out, "  {child} = load ptr, ptr {child_src}").unwrap();
                        writeln!(
                            self.out,
                            "  {cloned} = call ptr @gos_rt_vec_clone(ptr {child})"
                        )
                        .unwrap();
                        writeln!(
                            self.out,
                            "  {child_dst} = getelementptr i64, ptr {dst}, i64 {child_word}"
                        )
                        .unwrap();
                        writeln!(self.out, "  store ptr {cloned}, ptr {child_dst}{tbaa}").unwrap();
                    }
                }
            } else {
                let head = self.fresh_label("repeat_head");
                let body = self.fresh_label("repeat_body");
                let done = self.fresh_label("repeat_done");
                let counter = self.entry_alloca("i64");
                writeln!(self.out, "  store i64 0, ptr {counter}").unwrap();
                writeln!(self.out, "  br label %{head}").unwrap();
                writeln!(self.out, "{head}:").unwrap();
                let cur = self.fresh();
                writeln!(self.out, "  {cur} = load i64, ptr {counter}").unwrap();
                let cond = self.fresh();
                writeln!(self.out, "  {cond} = icmp ult i64 {cur}, {count}").unwrap();
                writeln!(self.out, "  br i1 {cond}, label %{body}, label %{done}").unwrap();
                writeln!(self.out, "{body}:").unwrap();
                let slot = self.fresh();
                writeln!(
                    self.out,
                    "  {slot} = mul i64 {cur}, {slots}",
                    slots = u64::from(op_slots)
                )
                .unwrap();
                let dst = self.fresh();
                writeln!(
                    self.out,
                    "  {dst} = getelementptr i64, ptr {base}, i64 {slot}"
                )
                .unwrap();
                writeln!(
                    self.out,
                    "  call void @llvm.memcpy.p0.p0.i64(ptr {dst}, ptr {src}, i64 {bytes}, i1 false)"
                )
                .unwrap();
                for child_word in &vec_child_words {
                    let child_src = self.fresh();
                    let child = self.fresh();
                    let cloned = self.fresh();
                    let child_dst = self.fresh();
                    writeln!(
                        self.out,
                        "  {child_src} = getelementptr i64, ptr {src}, i64 {child_word}"
                    )
                    .unwrap();
                    writeln!(self.out, "  {child} = load ptr, ptr {child_src}").unwrap();
                    writeln!(
                        self.out,
                        "  {cloned} = call ptr @gos_rt_vec_clone(ptr {child})"
                    )
                    .unwrap();
                    writeln!(
                        self.out,
                        "  {child_dst} = getelementptr i64, ptr {dst}, i64 {child_word}"
                    )
                    .unwrap();
                    writeln!(self.out, "  store ptr {cloned}, ptr {child_dst}{tbaa}").unwrap();
                }
                let next = self.fresh();
                writeln!(self.out, "  {next} = add i64 {cur}, 1").unwrap();
                writeln!(self.out, "  store i64 {next}, ptr {counter}").unwrap();
                writeln!(self.out, "  br label %{head}").unwrap();
                writeln!(self.out, "{done}:").unwrap();
            }
            return Ok(());
        }
        let v = self.lower_operand(value)?;
        let v_llvm = self.operand_llvm_ty(value);
        if packed_bytes {
            let byte = self.fresh();
            writeln!(self.out, "  {byte} = trunc {v_llvm} {v} to i8").unwrap();
            if count == 0 {
                return Ok(());
            }
            if matches!(value, Operand::Const(ConstValue::Int(0))) {
                writeln!(
                    self.out,
                    "  call void @llvm.memset.p0.i64(ptr {base}, i8 0, i64 {count}, i1 false)"
                )
                .unwrap();
            } else {
                let head = self.fresh_label("repeat_byte_head");
                let body = self.fresh_label("repeat_byte_body");
                let done = self.fresh_label("repeat_byte_done");
                let counter = self.entry_alloca("i64");
                writeln!(self.out, "  store i64 0, ptr {counter}").unwrap();
                writeln!(self.out, "  br label %{head}").unwrap();
                writeln!(self.out, "{head}:").unwrap();
                let cur = self.fresh();
                writeln!(self.out, "  {cur} = load i64, ptr {counter}").unwrap();
                let cond = self.fresh();
                writeln!(self.out, "  {cond} = icmp ult i64 {cur}, {count}").unwrap();
                writeln!(self.out, "  br i1 {cond}, label %{body}, label %{done}").unwrap();
                writeln!(self.out, "{body}:").unwrap();
                let dst = self.fresh();
                writeln!(
                    self.out,
                    "  {dst} = getelementptr i8, ptr {base}, i64 {cur}"
                )
                .unwrap();
                writeln!(self.out, "  store i8 {byte}, ptr {dst}{tbaa}").unwrap();
                let next = self.fresh();
                writeln!(self.out, "  {next} = add i64 {cur}, 1").unwrap();
                writeln!(self.out, "  store i64 {next}, ptr {counter}").unwrap();
                writeln!(self.out, "  br label %{head}").unwrap();
                writeln!(self.out, "{done}:").unwrap();
            }
            return Ok(());
        }
        // A slot is a whole word and is read back as one, so the scalar being
        // repeated fills each slot it lands in.
        let (v_llvm, v) = match self.slot_word_of(value, &v_llvm, &v) {
            Some(word) => ("i64".to_string(), word),
            None => (v_llvm, v),
        };
        if count <= 16 {
            for i in 0..count {
                let dst = self.fresh();
                writeln!(self.out, "  {dst} = getelementptr i64, ptr {base}, i64 {i}").unwrap();
                writeln!(self.out, "  store {v_llvm} {v}, ptr {dst}{tbaa}").unwrap();
            }
        } else {
            let head = self.fresh_label("repeat_head");
            let body = self.fresh_label("repeat_body");
            let done = self.fresh_label("repeat_done");
            let counter = self.entry_alloca("i64");
            writeln!(self.out, "  store i64 0, ptr {counter}").unwrap();
            writeln!(self.out, "  br label %{head}").unwrap();
            writeln!(self.out, "{head}:").unwrap();
            let cur = self.fresh();
            writeln!(self.out, "  {cur} = load i64, ptr {counter}").unwrap();
            let cond = self.fresh();
            writeln!(self.out, "  {cond} = icmp ult i64 {cur}, {count}").unwrap();
            writeln!(self.out, "  br i1 {cond}, label %{body}, label %{done}").unwrap();
            writeln!(self.out, "{body}:").unwrap();
            let dst = self.fresh();
            writeln!(
                self.out,
                "  {dst} = getelementptr i64, ptr {base}, i64 {cur}"
            )
            .unwrap();
            writeln!(self.out, "  store {v_llvm} {v}, ptr {dst}{tbaa}").unwrap();
            let next = self.fresh();
            writeln!(self.out, "  {next} = add i64 {cur}, 1").unwrap();
            writeln!(self.out, "  store i64 {next}, ptr {counter}").unwrap();
            writeln!(self.out, "  br label %{head}").unwrap();
            writeln!(self.out, "{done}:").unwrap();
        }
        Ok(())
    }

    /// Renders an aggregate / variant `value` to a `*c_char`
    /// suitable for `gos_rt_print_str` / `gos_rt_concat_str`.
    /// Each kind is routed through a runtime helper that walks
    /// the value's layout and produces a Display string. The Arr*
    /// kinds carry the static length so the helper knows the
    /// flat buffer's bounds.
    pub(crate) fn emit_aggregate_format(&mut self, kind: ConcatKind, value: &str) -> String {
        self.emit_aggregate_format_spelled(kind, value, false)
    }

    /// The same rendering under a chosen sequence spelling: a `Vec` writes
    /// `#[..]` and a fixed array or slice `[..]`. The two share one runtime
    /// representation, so the static type at the call site is what decides,
    /// and it says so through `bare`.
    pub(crate) fn emit_aggregate_format_spelled(
        &mut self,
        kind: ConcatKind,
        value: &str,
        bare: bool,
    ) -> String {
        if let ConcatKind::Seq(inner) = kind {
            return self.emit_aggregate_format_spelled(*inner, value, true);
        }
        let bare = i32::from(bare);
        let dest = self.fresh();
        match kind {
            ConcatKind::VecI64 => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_i64");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_i64(ptr {value}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::VecUint => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_u64");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_u64(ptr {value}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::VecF64 => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_f64");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_f64(ptr {value}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::VecBool => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_bool");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_bool(ptr {value}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::VecString => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_string");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_string(ptr {value}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::VecChar => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_char");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_char(ptr {value}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::VecAdt(ref fmt, by_ref) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_adt");
                let by_ref = i32::from(by_ref);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_adt(ptr {value}, ptr @\"{fmt}\", i32 {by_ref}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::MapTagged(key_tag, val_tag) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_map_format_tagged");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_map_format_tagged(ptr {value}, i64 {key_tag}, i64 {val_tag}, ptr null, i64 0)"
                )
                .unwrap();
            }
            ConcatKind::MapAdt(ref fmt) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_map_format_tagged");
                let adt = i64::from(gossamer_abi::DEBUG_PAYLOAD_ADT);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_map_format_tagged(ptr {value}, i64 0, i64 {adt}, ptr @\"{fmt}\", i64 0)"
                )
                .unwrap();
            }
            ConcatKind::MapTuple(ref tags, arity) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_map_format_tagged");
                let tag_str: String = tags.iter().map(|&b| b as char).collect();
                let (tags_global, _) = self.strings.borrow_mut().intern(&tag_str);
                let nested = i64::from(gossamer_abi::TUPLE_TAG_NESTED);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_map_format_tagged(ptr {value}, i64 0, i64 {nested}, ptr {tags_global}, i64 {arity})"
                )
                .unwrap();
            }
            ConcatKind::VecDesc(ref desc) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_desc");
                let global = self.intern_value_descriptor(desc);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_desc(ptr {value}, ptr {global}, i64 0, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::MapDesc(ref desc, val_at) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_map_format_desc");
                let global = self.intern_value_descriptor(desc);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_map_format_desc(ptr {value}, ptr {global}, i64 0, i64 {val_at})"
                )
                .unwrap();
            }
            ConcatKind::VecMap => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_map");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_map(ptr {value}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::VecTuple(ref tags, arity) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_tuple");
                let tag_str: String = tags.iter().map(|&b| b as char).collect();
                let (tags_global, _) = self.strings.borrow_mut().intern(&tag_str);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_tuple(ptr {value}, i64 {arity}, ptr {tags_global}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::VecVecI64 => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_vec_i64");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_vec_i64(ptr {value}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::VecVecF64 => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_vec_f64");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_vec_f64(ptr {value}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::VecVecString => {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_format_vec_string");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_vec_format_vec_string(ptr {value}, i32 {bare})"
                )
                .unwrap();
            }
            ConcatKind::ArrI64(n) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_arr_format_i64");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_arr_format_i64(ptr {value}, i64 {n})"
                )
                .unwrap();
            }
            ConcatKind::ArrU8(n) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_arr_format_u8");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_arr_format_u8(ptr {value}, i64 {n})"
                )
                .unwrap();
            }
            ConcatKind::ArrF64(n) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_arr_format_f64");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_arr_format_f64(ptr {value}, i64 {n})"
                )
                .unwrap();
            }
            ConcatKind::ArrBool(n) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_arr_format_bool");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_arr_format_bool(ptr {value}, i64 {n})"
                )
                .unwrap();
            }
            ConcatKind::ArrChar(n) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_arr_format_char");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_arr_format_char(ptr {value}, i64 {n})"
                )
                .unwrap();
            }
            ConcatKind::ArrAdt(n, stride, ref fmt, by_ref) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_arr_format_adt");
                let by_ref = i32::from(by_ref);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_arr_format_adt(ptr {value}, i64 {n}, i64 {stride}, ptr @\"{fmt}\", i32 {by_ref})"
                )
                .unwrap();
            }
            ConcatKind::ArrString(n) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_arr_format_string");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_arr_format_string(ptr {value}, i64 {n})"
                )
                .unwrap();
            }
            ConcatKind::ArrArrI64(n, m) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_arr_format_arr_i64");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_arr_format_arr_i64(ptr {value}, i64 {n}, i64 {m})"
                )
                .unwrap();
            }
            ConcatKind::ArrArrF64(n, m) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_arr_format_arr_f64");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_arr_format_arr_f64(ptr {value}, i64 {n}, i64 {m})"
                )
                .unwrap();
            }
            ConcatKind::ArrArrBool(n, m) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_arr_format_arr_bool");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_arr_format_arr_bool(ptr {value}, i64 {n}, i64 {m})"
                )
                .unwrap();
            }
            ConcatKind::JsonValue => {
                declare_rt(&mut self.runtime_refs, "gos_rt_json_display");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_json_display(ptr {value})"
                )
                .unwrap();
            }
            ConcatKind::DynValue => {
                declare_rt(&mut self.runtime_refs, "gos_rt_dyn_format");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_dyn_format(ptr {value})"
                )
                .unwrap();
            }
            ConcatKind::ErrorMessage => {
                // Display renders the colon-joined cause chain;
                // `.message()` keeps `gos_rt_error_message`.
                declare_rt(&mut self.runtime_refs, "gos_rt_error_display");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_error_display(ptr {value})"
                )
                .unwrap();
            }
            ConcatKind::Map => {
                declare_rt(&mut self.runtime_refs, "gos_rt_map_format");
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_map_format(ptr {value})"
                )
                .unwrap();
            }
            ConcatKind::HandleFormat(sym) => {
                declare_rt(&mut self.runtime_refs, sym);
                writeln!(self.out, "  {dest} = call ptr @{sym}(ptr {value})").unwrap();
            }
            ConcatKind::HandleFormatDesc(sym, ref desc) => {
                let desc = desc.clone();
                let global = self.intern_value_descriptor(&desc);
                declare_rt(&mut self.runtime_refs, sym);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @{sym}(ptr {value}, ptr {global})"
                )
                .unwrap();
            }
            ConcatKind::SetI64(is_btree) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_set_format_i64");
                let ordered = i32::from(is_btree);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_set_format_i64(ptr {value}, i32 {ordered})"
                )
                .unwrap();
            }
            ConcatKind::SetDesc(ref desc, is_btree) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_set_format_desc");
                let ordered = i32::from(is_btree);
                let global = self.intern_value_descriptor(desc);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_set_format_desc(ptr {value}, i32 {ordered}, ptr {global})"
                )
                .unwrap();
            }
            ConcatKind::SetEkey(ref desc, is_btree) => {
                let desc = desc.clone();
                let global = self.intern_value_descriptor(&desc);
                declare_rt(&mut self.runtime_refs, "gos_rt_set_format_ekey");
                let ordered = i32::from(is_btree);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_set_format_ekey(ptr {value}, i32 {ordered}, ptr {global})"
                )
                .unwrap();
            }
            ConcatKind::SetTagged(tag, is_btree) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_set_format_tagged");
                let ordered = i32::from(is_btree);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_set_format_tagged(ptr {value}, i32 {ordered}, i32 {tag})"
                )
                .unwrap();
            }
            ConcatKind::SetUint(is_btree) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_set_format_u64");
                let ordered = i32::from(is_btree);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_set_format_u64(ptr {value}, i32 {ordered})"
                )
                .unwrap();
            }
            ConcatKind::SetString(is_btree) => {
                declare_rt(&mut self.runtime_refs, "gos_rt_set_format_string");
                let ordered = i32::from(is_btree);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_set_format_string(ptr {value}, i32 {ordered})"
                )
                .unwrap();
            }
            // The operand is the by-value `i128` enum (disc + payload), not a
            // buffer pointer. It crosses the `extern "C"` boundary through
            // `fat_i128_call_arg`, which on Win64 spills it to a 16-byte slot
            // and passes `ptr` (matching the runtime's `__int128` ABI) and on
            // SysV passes the bare `i128`.
            ConcatKind::Option(ref payload) => {
                let tag = payload.tag();
                let opt_arg = self.fat_i128_call_arg(value);
                match payload {
                    DebugPayload::Tag(_) => {
                        declare_rt(&mut self.runtime_refs, "gos_rt_debug_option");
                        writeln!(
                            self.out,
                            "  {dest} = call ptr @gos_rt_debug_option({opt_arg}, i64 {tag})"
                        )
                        .unwrap();
                    }
                    DebugPayload::Fmt(_) | DebugPayload::Tuple(_) | DebugPayload::Desc(_) => {
                        declare_rt(&mut self.runtime_refs, "gos_rt_debug_option_fmt");
                        let operand = self.debug_fmt_operand(payload);
                        writeln!(
                            self.out,
                            "  {dest} = call ptr @gos_rt_debug_option_fmt({opt_arg}, i64 {tag}, ptr {operand})"
                        )
                        .unwrap();
                    }
                }
            }
            ConcatKind::Result(ref ok, ref err) => {
                let (ok_tag, err_tag) = (ok.tag(), err.tag());
                let res_arg = self.fat_i128_call_arg(value);
                match (ok, err) {
                    (DebugPayload::Tag(_), DebugPayload::Tag(_)) => {
                        declare_rt(&mut self.runtime_refs, "gos_rt_debug_result");
                        writeln!(
                            self.out,
                            "  {dest} = call ptr @gos_rt_debug_result({res_arg}, i64 {ok_tag}, i64 {err_tag})"
                        )
                        .unwrap();
                    }
                    _ => {
                        declare_rt(&mut self.runtime_refs, "gos_rt_debug_result_fmt");
                        let ok_fmt = self.debug_fmt_operand(ok);
                        let err_fmt = self.debug_fmt_operand(err);
                        writeln!(
                            self.out,
                            "  {dest} = call ptr @gos_rt_debug_result_fmt({res_arg}, i64 {ok_tag}, i64 {err_tag}, ptr {ok_fmt}, ptr {err_fmt})"
                        )
                        .unwrap();
                    }
                }
            }
            _ => unreachable!("emit_aggregate_format called with non-aggregate kind"),
        }
        dest
    }

    /// Emits the module-level global that carries a descriptor: an `i64`
    /// count of the per-type `fmt` pointers, those pointers, then the tag
    /// bytes. The runtime reads the header to reach both halves, so a
    /// `DESC_ADT` byte anywhere in the stream resolves to a real formatter.
    /// Returns the global's name.
    fn intern_value_descriptor(&mut self, desc: &ValueDesc) -> String {
        let mut bytes = String::with_capacity(desc.bytes.len() * 4);
        let mut key = String::with_capacity(desc.bytes.len() * 2);
        for byte in &desc.bytes {
            let _ = write!(bytes, "\\{byte:02X}");
            let _ = write!(key, "{byte:02x}");
        }
        for name in &desc.fns {
            let _ = write!(key, "_{name}");
        }
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in key.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        let name = format!("@\"__gos_desc_{hash:016x}\"");
        let table: String = desc
            .fns
            .iter()
            .map(|f| format!("ptr @\"{f}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let n = desc.fns.len();
        let len = desc.bytes.len();
        self.runtime_refs.insert(format!(
            "{name} = private constant {{ i64, [{n} x ptr], [{len} x i8] }} \
             {{ i64 {n}, [{n} x ptr] [{table}], [{len} x i8] c\"{bytes}\" }}"
        ));
        name
    }

    /// The `ptr` operand naming a payload's derived `fmt`, or `null` for a
    /// payload the runtime renders from its tag alone.
    fn debug_fmt_operand(&mut self, payload: &DebugPayload) -> String {
        match payload {
            DebugPayload::Tag(_) => "null".to_string(),
            DebugPayload::Fmt(sym) => format!("@\"{sym}\""),
            DebugPayload::Tuple(tags) => {
                let stream: String = tags.iter().map(|&b| b as char).collect();
                let (global, _) = self.strings.borrow_mut().intern(&stream);
                global
            }
            DebugPayload::Desc(desc) => {
                let desc = desc.clone();
                self.intern_value_descriptor(&desc)
            }
        }
    }

    /// Routes an aggregate-print operand to its runtime formatter.
    /// `Tuple` needs the operand to recompute its per-element tags;
    /// every other aggregate kind is fully described by `kind` +
    /// `value` and delegates to [`Self::emit_aggregate_format`].
    pub(crate) fn emit_concat_aggregate(
        &mut self,
        arg: &Operand,
        kind: ConcatKind,
        value: &str,
    ) -> Result<String, BuildError> {
        match kind {
            ConcatKind::Tuple => self.emit_tuple_format(arg, value),
            ConcatKind::TupleDesc(ref desc) => {
                let desc = desc.clone();
                let global = self.intern_value_descriptor(&desc);
                declare_rt(&mut self.runtime_refs, "gos_rt_tuple_format_desc");
                let dest = self.fresh();
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_tuple_format_desc(ptr {value}, ptr {global})"
                )
                .unwrap();
                Ok(dest)
            }
            ConcatKind::SetI64(_)
            | ConcatKind::SetUint(_)
            | ConcatKind::SetString(_)
            | ConcatKind::SetDesc(..)
            | ConcatKind::SetTagged(..)
            | ConcatKind::SetEkey(..)
            | ConcatKind::HandleFormat(_)
            | ConcatKind::HandleFormatDesc(..) => {
                let ptr = self.coerce_llvm_value(value, &self.operand_llvm_ty(arg), "ptr");
                Ok(self.emit_aggregate_format(kind, &ptr))
            }
            _ => Ok(self.emit_aggregate_format(kind, value)),
        }
    }

    /// Emits the `gos_rt_tuple_format(buf, n, tags)` call for a tuple
    /// operand. `value` is the address of the tuple's flat `[N x i64]`
    /// slot buffer; the tag array is interned as a module constant of
    /// raw bytes (one per element) and its body pointer passed as
    /// `tags`.
    fn emit_tuple_format(&mut self, arg: &Operand, value: &str) -> Result<String, BuildError> {
        let Operand::Copy(p) = arg else {
            return Err(BuildError::InternalLoweringBug(
                "tuple format expects a place operand",
            ));
        };
        let leaf = self.unwrap_ref(self.place_leaf_ty(p));
        let Some(TyKind::Tuple(elems)) = self.tcx.kind(leaf) else {
            return Err(BuildError::InternalLoweringBug(
                "tuple format on a non-tuple operand",
            ));
        };
        let elems: Vec<Ty> = elems.clone();
        let mut tags: Vec<u8> = Vec::with_capacity(elems.len());
        for e in &elems {
            match self.tuple_elem_tags(*e) {
                Some(t) => tags.extend(t),
                None => {
                    return Err(BuildError::InternalLoweringBug(
                        "tuple element type is not formattable on the compiled tier",
                    ));
                }
            }
        }
        let n = elems.len();
        // The tag bytes are all < 0x80, so each maps to a single UTF-8
        // byte: the interned constant's body is exactly the tag array.
        let tag_str: String = tags.iter().map(|&b| b as char).collect();
        let (tags_global, _) = self.strings.borrow_mut().intern(&tag_str);
        declare_rt(&mut self.runtime_refs, "gos_rt_tuple_format");
        let dest = self.fresh();
        writeln!(
            self.out,
            "  {dest} = call ptr @gos_rt_tuple_format(ptr {value}, i64 {n}, ptr {tags_global})"
        )
        .unwrap();
        Ok(dest)
    }
}
