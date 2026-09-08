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
use anyhow::Result;
use gossamer_abi as abi;
use gossamer_mir::{
    BasicBlock, BinOp, Body, ConstValue, IteratorAdapterKind, IteratorSourceKind, Local, Operand,
    Place, Projection, Rvalue, Statement, StatementKind, Terminator, UnOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};

impl<'a> Lowerer<'a> {
    fn store_typed_iter_value(&mut self, place: &Place, llvm_ty: &str, value: &str) {
        self.store_value_to_place(place, llvm_ty, value);
    }

    fn emit_typed_iter_call(
        &mut self,
        name: &str,
        ret_ty: &str,
        args: &[(&str, String)],
    ) -> String {
        declare_rt(&mut self.runtime_refs, name);
        let rendered = args
            .iter()
            .map(|(ty, value)| format!("{ty} {value}"))
            .collect::<Vec<_>>()
            .join(", ");
        let tmp = self.fresh();
        writeln!(self.out, "  {tmp} = call {ret_ty} @\"{name}\"({rendered})").unwrap();
        tmp
    }

    fn lower_typed_iterator_stmt(&mut self, stmt: &Statement) -> Result<bool, BuildError> {
        match &stmt.kind {
            StatementKind::IterSource {
                dst,
                source_kind,
                source,
                ..
            } => {
                let value = match source_kind {
                    IteratorSourceKind::Range => {
                        let end = self.lower_operand(source)?;
                        self.emit_typed_iter_call(
                            "gos_rt_lazy_iter_range_i64",
                            "ptr",
                            &[("i64", "0".to_string()), ("i64", end)],
                        )
                    }
                    IteratorSourceKind::Slice | IteratorSourceKind::VecInto => {
                        let source = self.lower_operand(source)?;
                        self.emit_typed_iter_call(
                            "gos_rt_lazy_iter_from_vec_i64",
                            "ptr",
                            &[("ptr", source)],
                        )
                    }
                };
                self.store_typed_iter_value(dst, "ptr", &value);
                Ok(true)
            }
            StatementKind::IterAdapter {
                dst,
                adapter_kind,
                upstream,
                closure_or_arg,
                ..
            } => {
                let upstream = self.lower_place_read(upstream);
                let (name, args): (&str, Vec<(&str, String)>) = match adapter_kind {
                    IteratorAdapterKind::Take | IteratorAdapterKind::Skip => {
                        let n = match closure_or_arg {
                            Some(arg) => self.lower_operand(arg)?,
                            None => "0".to_string(),
                        };
                        let name = if matches!(adapter_kind, IteratorAdapterKind::Take) {
                            "gos_rt_lazy_iter_take_i64"
                        } else {
                            "gos_rt_lazy_iter_skip_i64"
                        };
                        (name, vec![("i64", n), ("ptr", upstream)])
                    }
                    IteratorAdapterKind::Map | IteratorAdapterKind::Filter => {
                        let env = match closure_or_arg {
                            Some(arg) => self.lower_operand(arg)?,
                            None => "null".to_string(),
                        };
                        let name = if matches!(adapter_kind, IteratorAdapterKind::Map) {
                            "gos_rt_lazy_iter_map_i64"
                        } else {
                            "gos_rt_lazy_iter_filter_i64"
                        };
                        (name, vec![("ptr", env), ("ptr", upstream)])
                    }
                    IteratorAdapterKind::Enumerate => {
                        ("gos_rt_lazy_iter_enumerate_i64", vec![("ptr", upstream)])
                    }
                    IteratorAdapterKind::Chain | IteratorAdapterKind::Zip => {
                        let rhs = match closure_or_arg {
                            Some(arg) => self.lower_operand(arg)?,
                            None => "null".to_string(),
                        };
                        let name = if matches!(adapter_kind, IteratorAdapterKind::Chain) {
                            "gos_rt_lazy_iter_chain_i64"
                        } else {
                            "gos_rt_lazy_iter_zip_i64"
                        };
                        (name, vec![("ptr", upstream), ("ptr", rhs)])
                    }
                };
                let value = self.emit_typed_iter_call(name, "ptr", &args);
                self.store_typed_iter_value(dst, "ptr", &value);
                Ok(true)
            }
            StatementKind::IterNext {
                dst_option,
                iter_place,
                ..
            } => {
                let iter = self.lower_place_read(iter_place);
                let value = self.emit_typed_iter_call(
                    "gos_rt_lazy_iter_next_i64",
                    "i128",
                    &[("ptr", iter)],
                );
                self.store_typed_iter_value(dst_option, "i128", &value);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub(crate) fn lower_block(
        &mut self,
        block: &gossamer_mir::BasicBlock,
    ) -> Result<(), BuildError> {
        writeln!(self.out, "bb{}:", block.id.as_u32()).unwrap();
        // No loop-back-edge safepoint. A runtime call on every
        let cleanup = gossamer_mir::plan_cleanup_with_summary(self.body, &self.capture_summary);
        for entry in cleanup.at_block_entry(block.id) {
            self.emit_cleanup_call(entry);
        }
        for stmt in &block.stmts {
            self.emit_stack_frame_line(stmt.span.start);
            self.with_frame_line(|lowerer| lowerer.lower_stmt(stmt))?;
        }
        for entry in cleanup.at_block_exit(block.id) {
            self.emit_cleanup_call(entry);
        }
        self.current_block = Some(block.id.as_u32());
        self.with_frame_line(|lowerer| lowerer.lower_terminator(&block.terminator))?;
        self.current_block = None;
        Ok(())
    }

    pub(crate) fn lower_stmt(&mut self, stmt: &Statement) -> Result<(), BuildError> {
        if self.lower_typed_iterator_stmt(stmt)? {
            return Ok(());
        }
        match &stmt.kind {
            StatementKind::Assign { place, rvalue } => {
                self.lower_assign(place, rvalue)?;
            }
            StatementKind::StorageLive(local) => {
                // Hint to LLVM's register allocator that the
                // alloca's storage becomes live. Treat unit /
                // zero-sized locals as no-ops since they have no
                // alloca.
                if !is_unit(self.tcx, self.body.local_ty(*local)) {
                    let slot = local_slot(*local);
                    let bytes =
                        u64::from(slot_count(self.tcx, self.body.local_ty(*local)).unwrap_or(1))
                            * 8;
                    writeln!(
                        self.out,
                        "  call void @llvm.lifetime.start.p0(i64 {bytes}, ptr {slot})"
                    )
                    .unwrap();
                }
            }
            StatementKind::StorageDead(local) => {
                if !is_unit(self.tcx, self.body.local_ty(*local)) {
                    let slot = local_slot(*local);
                    let bytes =
                        u64::from(slot_count(self.tcx, self.body.local_ty(*local)).unwrap_or(1))
                            * 8;
                    writeln!(
                        self.out,
                        "  call void @llvm.lifetime.end.p0(i64 {bytes}, ptr {slot})"
                    )
                    .unwrap();
                }
            }
            StatementKind::StaticStore { target, value } => {
                // Store the value into the backing `static mut` global.
                let llvm_ty = render_ty(self.tcx, target.ty);
                self.register_static_global(target, &llvm_ty);
                let val = self.lower_operand(value)?;
                writeln!(
                    self.out,
                    "  store {llvm_ty} {val}, ptr @{sym}",
                    sym = target.symbol,
                )
                .unwrap();
            }
            StatementKind::IterSource { .. }
            | StatementKind::IterAdapter { .. }
            | StatementKind::IterNext { .. } => {
                unreachable!("typed iterator statements handled above")
            }
            StatementKind::Nop => {}
            StatementKind::SetDiscriminant { place, variant } => {
                // Stores the variant index at offset 0 of the
                // enum's backing place. Matches the Cranelift
                // convention: tag at slot 0, payload at +8.
                self.store_value_to_place(place, "i64", &variant.to_string());
            }
        }
        Ok(())
    }

    pub(crate) fn lower_assign(
        &mut self,
        place: &Place,
        rvalue: &Rvalue,
    ) -> Result<(), BuildError> {
        let dest_ty_mir = self.body.local_ty(place.local);
        if is_unit(self.tcx, dest_ty_mir) {
            // Even when the destination's MIR type is unit, the
            // rvalue may be a side-effecting intrinsic (gos_store
            // sinks, etc.). Funnel those into the raw-intrinsic
            // path so the IR records the side effect.
            if let Rvalue::CallIntrinsic { name, args } = rvalue
                && matches!(
                    *name,
                    "gos_load"
                        | "gos_store"
                        | "gos_store_i128"
                        | "gos_alloc"
                        | "gos_rc_alloc"
                        | "gos_rc_alloc_tagged"
                        | "gos_rc_alloc_reuse"
                        | "gos_fn_addr"
                        | "gos_enum_disc"
                        | "gos_enum_set_disc"
                        | "gos_enum_tag"
                        | "gos_enum_disc_tag"
                        | "gos_enum_untag"
                        | "gos_enum_load"
                )
            {
                return self.lower_raw_intrinsic(name, args, place, None);
            }
            // Guarded copy-blob walks from the aggregate drop pass:
            // arg0 is passed by SLOT ADDRESS (the walk reads the
            // aggregate's flat words in place) and arg1 names the
            // module-global meta blob - the generic runtime-call path
            // would lower both wrongly (value load; string constant).
            if let Rvalue::CallIntrinsic { name, args } = rvalue
                && matches!(
                    *name,
                    "gos_rt_aggr_release_children"
                        | "gos_rt_aggr_retain_children"
                        | "gos_rt_aggr_zero_guarded"
                        | "gos_rt_option_slot_retain"
                        | "gos_rt_option_slot_release"
                        | "gos_rt_vec_set_elem_meta"
                        | "gos_rt_vec_set_slot_children"
                        | "gos_rt_map_set_blob_values"
                        | "gos_rt_map_set_vec_values"
                        | "gos_rt_map_field_release"
                        | "gos_rt_map_field_clone"
                        | "gos_rt_set_field_release"
                        | "gos_rt_set_field_clone"
                        | "gos_rt_deque_field_release"
                        | "gos_rt_deque_field_clone"
                        | "gos_rt_bheap_field_release"
                        | "gos_rt_bheap_field_clone"
                )
            {
                return self.lower_guarded_walk_intrinsic(name, args);
            }
            // Drop-style intrinsic calls (`gos_rt_map_free`,
            // `gos_rt_vec_free`, etc.) emitted by the MIR cleanup
            // pass come through with a unit-typed destination
            // because their result is `()`. Without this branch
            // the call would be dropped on the floor and the
            // container would leak until process exit. Route any
            // `gos_rt_*` intrinsic at the runtime-call path so
            // the IR records the side effect.
            if let Rvalue::CallIntrinsic { name, args } = rvalue
                && name.starts_with("gos_rt_")
            {
                self.lower_runtime_call_intrinsic(name, args, place.local)?;
                return Ok(());
            }
            return Ok(());
        }
        // Aggregate constructions (`Aggregate`, `Repeat`) are
        // routed straight at the destination slot - they
        // populate the stack aggregate in-place rather than
        // producing a scalar value to store.
        match rvalue {
            Rvalue::Aggregate { operands, .. } => {
                return self.emit_aggregate_store(place, operands);
            }
            Rvalue::Repeat { value, count } => {
                return self.emit_repeat_store(place, value, *count);
            }
            // Rvalue-position raw heap intrinsics (the
            // `coerce_to_fn_trait_if_needed` MIR pass uses
            // these for the FnTrait env blob, and lifted
            // closures use them for env materialisation).
            // Reuse the same inline handler the terminator path
            // hits via `lower_call`.
            Rvalue::CallIntrinsic { name, args }
                if matches!(
                    *name,
                    "gos_load"
                        | "gos_store"
                        | "gos_store_i128"
                        | "gos_alloc"
                        | "gos_rc_alloc"
                        | "gos_rc_alloc_tagged"
                        | "gos_rc_alloc_reuse"
                        | "gos_fn_addr"
                        | "gos_enum_disc"
                        | "gos_enum_set_disc"
                        | "gos_enum_tag"
                        | "gos_enum_disc_tag"
                        | "gos_enum_untag"
                        | "gos_enum_load"
                ) =>
            {
                return self.lower_raw_intrinsic(name, args, place, None);
            }
            _ => {}
        }
        // Whole-aggregate copy: when the destination is an
        // aggregate local and the rvalue is a plain `Use(Copy)`
        // of another aggregate value (a bare local OR a
        // projected aggregate field - `let p = pts[i]`), memcpy
        // the flat storage rather than trying to load/store it
        // as a single scalar.
        let leaf_ty = self.place_leaf_ty(place);
        if is_aggregate(self.tcx, leaf_ty) {
            if let Rvalue::Use(Operand::Copy(src_place)) = rvalue {
                let src_leaf_ty = self.place_leaf_ty(src_place);
                if is_aggregate(self.tcx, src_leaf_ty) {
                    let bytes = aggregate_storage_bytes(self.tcx, leaf_ty).unwrap_or_else(|| {
                        u64::from(slot_count(self.tcx, leaf_ty).unwrap_or(1).max(1)) * 8
                    });
                    let src_addr = if src_place.projection.is_empty() {
                        local_slot(src_place.local)
                    } else {
                        self.lower_place_address(src_place)
                    };
                    let dst_addr = if place.projection.is_empty() {
                        local_slot(place.local)
                    } else {
                        self.lower_place_address(place)
                    };
                    writeln!(
                        self.out,
                        "  call void @llvm.memcpy.p0.p0.i64(ptr {dst_addr}, ptr {src_addr}, i64 {bytes}, i1 false)"
                    )
                    .unwrap();
                    return Ok(());
                }
            }
        }
        let leaf_llvm = render_ty(self.tcx, leaf_ty);
        let value = self.lower_rvalue(rvalue, place.local)?;
        // The rvalue's LLVM type may differ from the destination
        // slot's leaf type for several shapes:
        //
        //   * `Use(FnRef)` returns a `ptr` literal; when the
        //     destination is an `i64` slot (goroutine-spawn path
        //     stores fn addresses as i64), coerce ptr → i64.
        //   * `Use(Const(Int(n)))` returns the integer literal
        //     `n` as an `i64`; when the destination is a float
        //     slot (closure capturing a float, struct field of
        //     type `f64`, etc.), the bare `store double 16` IR
        //     is rejected by `opt`/`llc`. Coerce i64 → double.
        //   * `Use(Const(Float(...)))` returns a `0xH…` literal
        //     typed `double`; when the destination is integer-
        //     shaped (rare; format-precision args route this
        //     way), coerce double → i64.
        //
        // Strict LLVM verification surfaces these mismatches, so
        // coerce here.
        let rvalue_llvm = self.rvalue_llvm_ty(rvalue);
        // When the rvalue is void (e.g. `Use(Copy(_tmp))` where the
        // source local was assigned the result of a void-returning
        // runtime call), `lower_place_read` returns an empty string.
        // Coercing `bitcast void <empty> to ptr` is invalid IR.
        // Synthesise a null sentinel matching the destination's
        // leaf type so the slot has a well-defined bit pattern when
        // the return path or any later use reads it.
        let (rvalue_llvm, value) = if rvalue_llvm == "void" || value.is_empty() {
            let sentinel = match leaf_llvm.as_str() {
                "ptr" => "null".to_string(),
                "double" | "float" => "0.0".to_string(),
                _ => "0".to_string(),
            };
            (leaf_llvm.clone(), sentinel)
        } else {
            (rvalue_llvm, value)
        };
        let value = if rvalue_llvm != leaf_llvm && !rvalue_llvm.is_empty() && leaf_llvm != "void" {
            self.coerce_llvm_value(&value, &rvalue_llvm, &leaf_llvm)
        } else {
            value
        };
        let addr = if place.projection.is_empty() {
            local_slot(place.local)
        } else {
            self.lower_place_address(place)
        };
        // When a runtime call returns a heap pointer to an
        // aggregate (e.g. `gos_rt_result_payload` returning a
        // heap-allocated Bag / ExecOutput / tuple), the destination
        // is an inline `[N x i64]` alloca. A bare `store ptr` only
        // writes the blob address into slot 0; subsequent field
        // reads then load the blob pointer instead of the actual
        // field value. Memcpy the full struct instead. This applies
        // to every aggregate slot count, including N==1: a 1-slot
        // `Bag { items: Vec<String> }` value-semantically holds a
        // Vec ptr at offset 0, NOT the Bag's address itself.
        let dest_slots = slot_count(self.tcx, dest_ty_mir);
        // A single word stored into a *multi-slot* aggregate is a pointer to a
        // heap copy of that aggregate (a genuine multi-slot value is
        // materialised slot-by-slot, never as one word), so its contents are
        // copied in. Covers a boxed pointer carried as `i64` - e.g.
        // `gos_rt_select_value` for a struct channel payload.
        let heap_aggregate_store = place.projection.is_empty()
            && is_aggregate(self.tcx, dest_ty_mir)
            && dest_slots.is_some_and(|n| n >= 1)
            && (leaf_llvm == "ptr" || (leaf_llvm == "i64" && dest_slots.is_some_and(|n| n >= 2)));
        if heap_aggregate_store {
            let bytes = u64::from(dest_slots.unwrap_or(1).max(1)) * 8;
            let src_ptr = if leaf_llvm == "ptr" {
                value.clone()
            } else {
                let p = self.fresh();
                writeln!(self.out, "  {p} = inttoptr i64 {value} to ptr").unwrap();
                p
            };
            writeln!(
                self.out,
                "  call void @llvm.memcpy.p0.p0.i64(ptr {addr}, ptr {src_ptr}, i64 {bytes}, i1 false)"
            )
            .unwrap();
        } else if self.place_is_packed_byte_element(place) {
            self.store_value_to_place(place, &leaf_llvm, &value);
        } else {
            let tbaa = self.place_payload_tbaa(place);
            writeln!(self.out, "  store {leaf_llvm} {value}, ptr {addr}{tbaa}").unwrap();
        }
        Ok(())
    }

    pub(crate) fn lower_terminator(&mut self, term: &Terminator) -> Result<(), BuildError> {
        match term {
            Terminator::Return => {
                // The frame this body pushed on entry leaves the panic
                // report's call stack on every return path.
                self.emit_stack_frame_pop();
                // Emit cleanup calls for owning heap-typed locals before
                // the actual `ret`. Mirrors the Cranelift Return path -
                // see `gossamer_mir::plan_cleanup` for the analysis.
                let cleanup =
                    gossamer_mir::plan_cleanup_with_summary(self.body, &self.capture_summary);
                for entry in cleanup.at_return() {
                    self.emit_cleanup_call(entry);
                }
                let ret_ty = self.body.local_ty(Local::RETURN);
                let ret_llvm = render_ty(self.tcx, ret_ty);
                if is_unit(self.tcx, ret_ty) {
                    self.emit_heap_spill_frees();
                    writeln!(self.out, "  ret void").unwrap();
                } else if is_aggregate(self.tcx, ret_ty) {
                    if let Some(slots) = slot_count(self.tcx, ret_ty) {
                        // Inline aggregate (struct / tuple / array): the
                        // callee's `%l0` is a stack alloca whose storage
                        // dies when the frame pops. Heap-allocate so the
                        // returned pointer outlives the call, copy the
                        // inline field data over, and return the heap
                        // pointer.
                        let bytes = u64::from(slots.max(1)) * 8;
                        declare_rt(&mut self.runtime_refs, "gos_rt_gc_alloc");
                        // `noalias`: a fresh allocation, so the memcpy below
                        // cannot be writing through any other live pointer.
                        let heap = self.fresh();
                        writeln!(
                            self.out,
                            "  {heap} = call noalias ptr @gos_rt_gc_alloc(i64 {bytes})"
                        )
                        .unwrap();
                        writeln!(
                            self.out,
                            "  call void @llvm.memcpy.p0.p0.i64(ptr {heap}, ptr {slot}, i64 {bytes}, i1 false)",
                            slot = local_slot(Local::RETURN)
                        )
                        .unwrap();
                        self.emit_heap_spill_frees();
                        writeln!(self.out, "  ret ptr {heap}").unwrap();
                    } else {
                        // Handle-Adt (recursive enum, opaque sentinel
                        // struct): the RETURN slot already holds an
                        // 8-byte heap handle. Return it directly. The
                        // inline-aggregate path above would gc_alloc a
                        // copy of the slot and return a pointer *to* the
                        // handle (double indirection), so the caller
                        // decoded a wild discriminant - e.g. an enum
                        // produced by `fn f() -> E` then pushed into a
                        // Vec read back as garbage.
                        let tmp = self.fresh();
                        writeln!(
                            self.out,
                            "  {tmp} = load ptr, ptr {slot}",
                            slot = local_slot(Local::RETURN)
                        )
                        .unwrap();
                        self.emit_heap_spill_frees();
                        writeln!(self.out, "  ret ptr {tmp}").unwrap();
                    }
                } else {
                    let tmp = self.fresh();
                    writeln!(
                        self.out,
                        "  {tmp} = load {ret_llvm}, ptr {slot}",
                        slot = local_slot(Local::RETURN)
                    )
                    .unwrap();
                    self.emit_heap_spill_frees();
                    writeln!(self.out, "  ret {ret_llvm} {tmp}").unwrap();
                }
                Ok(())
            }
            Terminator::Goto { target } => {
                if self
                    .current_block
                    .is_some_and(|src| self.is_cfg_back_edge(src, target.as_u32()))
                {
                    self.emit_preempt_check(target.as_u32());
                }
                writeln!(self.out, "  br label %bb{}", target.as_u32()).unwrap();
                Ok(())
            }
            Terminator::SwitchInt {
                discriminant,
                arms,
                default,
            } => {
                let src = self.current_block.unwrap_or(u32::MAX);
                let back_target = arms
                    .iter()
                    .map(|(_, target)| target.as_u32())
                    .chain(std::iter::once(default.as_u32()))
                    .filter(|target| self.is_cfg_back_edge(src, *target))
                    .min();
                if let Some(target) = back_target {
                    self.emit_preempt_check(target);
                }
                let v = self.lower_operand(discriminant)?;
                let mut ty = render_ty(self.tcx, self.operand_ty(discriminant));
                let mut v = v;
                // A pointer discriminant (a truthiness / null check on a heap
                // handle, e.g. `if json_get(e, "is_file")`) cannot drive an
                // LLVM `switch` - its condition must be an integer. Reinterpret
                // the pointer's bits as i64; the case labels are integer
                // literals (`0` for the null / false arm) that compare directly.
                if ty == "ptr" {
                    let int_v = self.fresh();
                    writeln!(self.out, "  {int_v} = ptrtoint ptr {v} to i64").unwrap();
                    v = int_v;
                    ty = "i64".to_string();
                }
                writeln!(
                    self.out,
                    "  switch {ty} {v}, label %bb{default} [",
                    default = default.as_u32()
                )
                .unwrap();
                for (cst, target) in arms {
                    writeln!(self.out, "    {ty} {cst}, label %bb{}", target.as_u32()).unwrap();
                }
                writeln!(self.out, "  ]").unwrap();
                Ok(())
            }
            Terminator::Unreachable => {
                writeln!(self.out, "  unreachable").unwrap();
                Ok(())
            }
            Terminator::Panic { message } => {
                self.lower_panic(message);
                Ok(())
            }
            Terminator::Drop { target, .. } => {
                // Gossamer runtime manages drops through the GC
                // hooks; the MIR `Drop` terminator is a
                // sequencing point that the LLVM backend can
                // treat as a plain `Goto` without calling any
                // destructor (no-op drop).
                writeln!(self.out, "  br label %bb{}", target.as_u32()).unwrap();
                Ok(())
            }
            Terminator::Assert {
                cond,
                expected,
                target,
                msg,
            } => self.lower_assert(cond, *expected, *target, msg),
            Terminator::Call {
                callee,
                args,
                destination,
                target,
            } => self.lower_call(callee, args, destination, target.as_ref()),
        }
    }
}
