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
    BasicBlock, BinOp, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, Statement,
    StatementKind, Terminator, UnOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};

impl<'a> Lowerer<'a> {
    pub(crate) fn emit_cleanup_call(&mut self, entry: &gossamer_mir::CleanupEntry) {
        declare_rt(&mut self.runtime_refs, entry.free_fn);
        let tmp = self.fresh();
        writeln!(
            self.out,
            "  {tmp} = load ptr, ptr {slot}",
            slot = local_slot(entry.local)
        )
        .unwrap();
        writeln!(
            self.out,
            "  call void @{free}(ptr {tmp})",
            free = entry.free_fn
        )
        .unwrap();
    }

    fn successor_blocks(term: &Terminator) -> Vec<u32> {
        match term {
            Terminator::Goto { target } => vec![target.as_u32()],
            Terminator::SwitchInt { arms, default, .. } => arms
                .iter()
                .map(|(_, target)| target.as_u32())
                .chain(std::iter::once(default.as_u32()))
                .collect(),
            Terminator::Call { target, .. } => target.iter().map(|b| b.as_u32()).collect(),
            Terminator::Assert { target, .. } | Terminator::Drop { target, .. } => {
                vec![target.as_u32()]
            }
            Terminator::Return | Terminator::Unreachable | Terminator::Panic { .. } => Vec::new(),
        }
    }

    /// A cycle edge is a back edge only when its target dominates its source.
    /// Test dominance directly by asking whether the source remains reachable
    /// from entry after removing the proposed target from the CFG.
    pub(crate) fn is_cfg_back_edge(&self, source: u32, target: u32) -> bool {
        if source as usize >= self.body.blocks.len() || target as usize >= self.body.blocks.len() {
            return false;
        }
        if target == 0 {
            return true;
        }
        let mut seen = vec![false; self.body.blocks.len()];
        let mut pending = vec![0u32];
        seen[0] = true;
        while let Some(block) = pending.pop() {
            if block == source {
                return false;
            }
            for next in Self::successor_blocks(&self.body.blocks[block as usize].terminator) {
                if next != target && !seen[next as usize] {
                    seen[next as usize] = true;
                    pending.push(next);
                }
            }
        }
        true
    }

    /// Returns the natural loop formed by a dominating target and its source.
    fn natural_loop_blocks(&self, source: u32, target: u32) -> Vec<u32> {
        let mut predecessors = vec![Vec::new(); self.body.blocks.len()];
        for (block, data) in self.body.blocks.iter().enumerate() {
            for successor in Self::successor_blocks(&data.terminator) {
                predecessors[successor as usize].push(block as u32);
            }
        }
        let mut included = vec![false; self.body.blocks.len()];
        included[target as usize] = true;
        included[source as usize] = true;
        let mut pending = if source == target {
            Vec::new()
        } else {
            vec![source]
        };
        while let Some(block) = pending.pop() {
            for &pred in &predecessors[block as usize] {
                if !included[pred as usize] {
                    included[pred as usize] = true;
                    pending.push(pred);
                }
            }
        }
        included
            .into_iter()
            .enumerate()
            .filter_map(|(block, yes)| yes.then_some(block as u32))
            .collect()
    }

    /// Estimates the work represented by a back edge. Calls dominate the
    /// charge because they can hide allocation, hashing, or collection work;
    /// statement count distinguishes tiny arithmetic loops from larger loop
    /// bodies without requiring a target-specific instruction cost model.
    fn preempt_charge(&self, target: u32) -> i32 {
        let source = self.current_block.unwrap_or(target);
        let mut statements = 0usize;
        let mut calls = 0usize;
        for block in self.natural_loop_blocks(source, target) {
            let block = &self.body.blocks[block as usize];
            statements += block.stmts.len();
            if matches!(block.terminator, Terminator::Call { .. }) {
                calls += 1;
            }
        }
        // A maximum charge of 16 preserves the old 1024-iteration interval
        // for expensive loops. A tiny loop charges one and polls every 16384
        // iterations, which is still well below the scheduler watchdog slice.
        (1 + statements / 8 + calls * 2).clamp(1, 16) as i32
    }

    /// Back-edge safepoint for cooperative preemption.
    ///
    /// Native loop polling is intentionally disabled for now. The opaque
    /// runtime call and its countdown state block the optimizers on the exact
    /// numeric loops this backend is meant to recover.
    pub(crate) fn emit_preempt_check(&mut self, target: u32) {
        let _ = target;
        let _ = self.preempt_seq;
    }

    pub(crate) fn fresh_label(&mut self, prefix: &str) -> String {
        let n = self.next_ssa;
        self.next_ssa += 1;
        format!("{prefix}_{n}")
    }

    /// Audit C6 - dynamic array-index bounds check.
    ///
    /// Emits a compare + conditional branch around the
    /// `getelementptr` for `arr[i]`. The compare is unsigned
    /// (`uge`) so negative indices (which wrap to large `u64`)
    /// trip the check without a separate sign branch. On
    /// out-of-bounds we land in a side block that calls
    /// `gos_rt_panic_oob` and falls through to `unreachable`,
    /// keeping the rest of the function well-formed.
    ///
    /// Only fires when `ty` (after peeling `Ref` wrappers)
    /// resolves to `TyKind::Array { len, .. }`. Vec / Slice
    /// indexing reaches element storage through
    /// `gos_rt_vec_get_*` intrinsics which check internally -
    /// the projection path only needs to cover flat fixed
    /// arrays.
    ///
    /// Skipped entirely when `GOSSAMER_DISABLE_BOUNDS_CHECK=1`
    /// is set in the build environment.
    pub(crate) fn emit_array_bounds_check(&mut self, ty: Ty, idx_ssa: &str) {
        if std::env::var_os("GOSSAMER_DISABLE_BOUNDS_CHECK").is_some() {
            return;
        }
        let mut peeled = ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
            peeled = *inner;
        }
        let Some(TyKind::Array { len, .. }) = self.tcx.kind(peeled) else {
            return;
        };
        let len_val = len.to_usize();
        declare_rt(&mut self.runtime_refs, "gos_rt_panic_oob");
        // Intern the `"array index"` label through the shared
        // module-wide string pool so multiple checks (in this
        // body and across the rest of the module) collapse to a
        // single `@.gstr_*` global.
        let (label_global, _) = self.strings.borrow_mut().intern("array index");
        let cond = self.fresh();
        writeln!(self.out, "  {cond} = icmp uge i64 {idx_ssa}, {len_val}").unwrap();
        let oob_label = self.fresh_label("oob");
        let ok_label = self.fresh_label("oob_ok");
        writeln!(
            self.out,
            "  br i1 {cond}, label %{oob_label}, label %{ok_label}"
        )
        .unwrap();
        let cold_start = self.out.len();
        writeln!(self.out, "{oob_label}:").unwrap();
        self.emit_panic_site_line();
        writeln!(
            self.out,
            "  call void @gos_rt_panic_oob(ptr {label_global}, i64 {idx_ssa}, i64 {len_val})"
        )
        .unwrap();
        writeln!(self.out, "  unreachable").unwrap();
        self.mark_cold(cold_start);
        writeln!(self.out, "{ok_label}:").unwrap();
    }

    /// Emits one runtime print call per argument, dispatching
    /// by the operand's MIR type. When `separator` is non-empty,
    /// emits a `gos_rt_print_str(separator)` call between each
    /// pair of args (used by `println(a, b, c)` for the
    /// space-separated form; empty for `__concat`'s tight join).
    pub(crate) fn emit_per_arg_print(
        &mut self,
        args: &[Operand],
        separator: &str,
    ) -> Result<(), BuildError> {
        for sym in [
            "gos_rt_print_str",
            "gos_rt_print_i64",
            "gos_rt_print_u64",
            "gos_rt_print_f64",
            "gos_rt_print_bool",
            "gos_rt_print_char",
        ] {
            declare_rt(&mut self.runtime_refs, sym);
        }
        let sep_name = if separator.is_empty() {
            None
        } else {
            Some(self.strings.borrow_mut().intern(separator).0)
        };
        for (idx, arg) in args.iter().enumerate() {
            if idx > 0 {
                if let Some(name) = &sep_name {
                    writeln!(self.out, "  call void @gos_rt_print_str(ptr {name})").unwrap();
                }
            }
            let kind = self.concat_print_kind(arg, "to_string");
            if matches!(kind, ConcatKind::Unsupported) {
                // Surface a generic "unsupported" so the driver
                // routes this body to Cranelift, whose `bail!`
                // emits a user-facing message naming the
                // specific operand kind (tuple, Vec, struct, …).
                return Err(BuildError::InternalLoweringBug(
                    "println/format of aggregate or variant types",
                ));
            }
            let value = self.lower_operand(arg)?;
            match kind {
                ConcatKind::StrPtr => {
                    writeln!(self.out, "  call void @gos_rt_print_str(ptr {value})").unwrap();
                }
                ConcatKind::Int => {
                    let widened = self.widen_to_i64(arg, &value);
                    writeln!(self.out, "  call void @gos_rt_print_i64(i64 {widened})").unwrap();
                }
                ConcatKind::Uint => {
                    let widened = self.widen_to_u64(arg, &value);
                    writeln!(self.out, "  call void @gos_rt_print_u64(i64 {widened})").unwrap();
                }
                ConcatKind::Float => {
                    let widened = self.widen_to_f64(arg, &value);
                    writeln!(self.out, "  call void @gos_rt_print_f64(double {widened})").unwrap();
                }
                ConcatKind::Bool => {
                    let widened = self.widen_bool_to_i32(arg, &value);
                    writeln!(self.out, "  call void @gos_rt_print_bool(i32 {widened})").unwrap();
                }
                ConcatKind::Char => {
                    let widened = self.widen_char_to_i32(arg, &value);
                    writeln!(self.out, "  call void @gos_rt_print_char(i32 {widened})").unwrap();
                }
                ConcatKind::Unsupported => unreachable!("checked above"),
                // Every remaining kind is an aggregate its runtime
                // formatter renders to a c-string.
                kind => {
                    let str_ptr = self.emit_concat_aggregate(arg, kind, &value)?;
                    writeln!(self.out, "  call void @gos_rt_print_str(ptr {str_ptr})").unwrap();
                    // The formatter answers a fresh String this frame is the
                    // only holder of, and the write copies its bytes out.
                    declare_rt(&mut self.runtime_refs, "gos_rt_str_free_typed");
                    writeln!(
                        self.out,
                        "  call void @gos_rt_str_free_typed(ptr {str_ptr})"
                    )
                    .unwrap();
                }
            }
        }
        Ok(())
    }

    /// Builds a single concatenated c-string from every argument
    /// and stores its pointer in `dest_ssa`. Each arg is
    /// converted through `gos_rt_*_to_str` (or passed through for
    /// strings); pieces are joined with `separator` via
    /// `gos_rt_str_concat`. Used by multi-arg `panic(...)` where
    /// the runtime takes a single message pointer.
    pub(crate) fn emit_args_to_concat_string(
        &mut self,
        args: &[Operand],
        separator: &str,
    ) -> Result<String, BuildError> {
        declare_rt(&mut self.runtime_refs, "gos_rt_str_concat");
        let (empty_name, _) = self.strings.borrow_mut().intern("");
        if args.is_empty() {
            return Ok(empty_name);
        }
        let sep_name = if separator.is_empty() {
            None
        } else {
            Some(self.strings.borrow_mut().intern(separator).0)
        };
        let mut acc = self.lower_arg_to_str_ptr(&args[0])?;
        for arg in &args[1..] {
            if let Some(name) = &sep_name {
                let next = self.fresh();
                writeln!(
                    self.out,
                    "  {next} = call ptr @gos_rt_str_concat(ptr {acc}, ptr {name})"
                )
                .unwrap();
                acc = next;
            }
            let piece = self.lower_arg_to_str_ptr(arg)?;
            let next = self.fresh();
            writeln!(
                self.out,
                "  {next} = call ptr @gos_rt_str_concat(ptr {acc}, ptr {piece})"
            )
            .unwrap();
            acc = next;
        }
        Ok(acc)
    }

    /// Emits the result of a variant constructor call without
    /// going through a real function symbol. `Ok(v)`, `Some(v)`,
    /// and `Err(e)` write the inner value to the destination;
    /// payload-less variants write zero. Coerces the inner
    /// value's LLVM type to the destination's slot type so the
    /// emitted store is well-formed even when the wrapper Adt
    /// renders as `ptr` and the inner value is a plain `i64` /
    /// `double`.
    pub(crate) fn emit_variant_stub(
        &mut self,
        name: &str,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let dest_ty_mir = self.place_leaf_ty(destination);
        let dest_ty = render_ty(self.tcx, dest_ty_mir);
        if dest_ty == "void" || is_unit(self.tcx, dest_ty_mir) {
            emit_terminator_branch(&mut self.out, target);
            return Ok(());
        }
        let (value, value_ty): (String, String) =
            if matches!(name, "Ok" | "Some" | "Err") && !args.is_empty() {
                let v = self.lower_operand(&args[0])?;
                let vt = self.operand_llvm_ty(&args[0]);
                (v, vt)
            } else {
                let zero = match dest_ty.as_str() {
                    "ptr" => "null".to_string(),
                    "double" | "float" => "0.0".to_string(),
                    _ => "0".to_string(),
                };
                (zero, dest_ty.clone())
            };
        let coerced = self.coerce_llvm_value(&value, &value_ty, &dest_ty);
        self.store_value_to_place(destination, &dest_ty, &coerced);
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Direct call to a named symbol: renders args, emits the
    /// `call`, stores the result if non-unit, and writes the
    /// outgoing branch / unreachable.
    pub(crate) fn emit_named_call(
        &mut self,
        symbol: &str,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let expected_param_tys: Vec<Option<Ty>> = self
            .param_tys_by_name
            .get(symbol)
            .map(|tys| tys.iter().map(|t| Some(*t)).collect())
            .unwrap_or_default();
        // For `gos_rt_*` symbols, the runtime registry gives us
        // canonical LLVM parameter types - drive the emission off
        // those rather than the per-call operand types so that a
        // Unit-typed operand still lands as a valid i64 / ptr at
        // the call site (matches the dedup'd canonical declare
        // emitted above).
        let registry_param_llvm: Vec<String> = if symbol.starts_with("gos_rt_") {
            gossamer_abi::lookup(symbol)
                .map(|e| {
                    e.sig
                        .params
                        .iter()
                        .map(|p| p.llvm_ir().to_string())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // `gos_rt_chan_send` / `gos_rt_chan_try_send` expect their
        // second argument to be `*const u8` - a pointer to a
        // memory slot holding the value bytes (the runtime
        // memcpys `chan.elem_bytes` from there). Stack-spill the
        // value and pass the slot address, matching the
        // Cranelift backend; a bare `inttoptr i64 N to ptr`
        // produces a wild pointer that segfaults inside the
        // runtime's `push_back`.
        let chan_send_spill = matches!(symbol, "gos_rt_chan_send" | "gos_rt_chan_try_send");
        // See `lower_runtime_call_intrinsic` for rationale: heap-copy
        // aggregate payloads passed to `gos_rt_result_new` so the
        // caller's stack alloca doesn't go dangling after return.
        let result_new_heap_copy = matches!(symbol, "gos_rt_result_new");
        // An ordered container's push takes the address of the element's
        // slots; the store copies its own stride from it.
        let heap_push_by_address = matches!(
            symbol,
            "gos_rt_bheap_max_push_desc" | "gos_rt_bheap_min_push_desc"
        );
        // A content-keyed map or set reads the key's slots through the
        // descriptor that travels with the call, so the key is passed by
        // address - an aggregate's own storage, or a fresh slot holding a
        // two-word `Option` carrier.
        let skey_by_address = matches!(
            symbol,
            "gos_rt_map_insert_skey_opt"
                | "gos_rt_map_insert_skey"
                | "gos_rt_map_get_skey_opt"
                | "gos_rt_map_get_or_skey"
                | "gos_rt_map_contains_skey"
                | "gos_rt_map_remove_skey"
                | "gos_rt_map_or_insert_skey"
                | "gos_rt_map_inc_skey"
                | "gos_rt_set_insert_skey"
                | "gos_rt_set_contains_skey"
                | "gos_rt_set_remove_skey"
        );

        // HashMap insert with a struct value: the value arg is the
        // stack address of an Rvalue::Aggregate local that goes out
        // of scope when the inserting frame returns. Heap-copy so
        // subsequent `m.get(&k)` calls read live memory rather than
        // stale stack slots. Applies to `_i64_i64` (i64-key) and
        // `_str_i64` (str-key) - the `_*_str` variants already pass
        // a c_char ptr to a heap-allocated string, no copy needed.
        // A map insert whose value is an aggregate copies it into a
        // reference-counted blob at the call site, at strong 1 - the frame's
        // own share, which the MIR passes cannot see to pair a release for.
        // Recorded here so it is given back right after the call stores it.
        let mut minted_map_blob: Option<String> = None;
        let map_insert_heap_copy = matches!(
            symbol,
            "gos_rt_map_insert_i64_i64"
                | "gos_rt_map_insert_str_i64"
                | "gos_rt_map_insert_typed_str_i64"
                | "gos_rt_map_insert_i64_i64_opt"
                | "gos_rt_map_insert_str_i64_opt"
                | "gos_rt_map_insert_typed_str_i64_opt"
        );
        // The same rationale for a content-keyed insert, whose value is
        // one argument later because the key's layout descriptor travels
        // between them. A stored value still pointing at the inserting
        // frame's slot is overwritten by the next iteration, so every
        // entry reads back as the last one inserted.
        let skey_insert_heap_copy = matches!(
            symbol,
            "gos_rt_map_insert_skey"
                | "gos_rt_map_insert_skey_opt"
                // `or_insert` stores its default when the slot is
                // absent, so that default is a stored value too.
                | "gos_rt_map_or_insert_skey"
        );
        // Win64: the runtime invokes a two-word spawn callable as
        // `extern "C-unwind" fn(usize) -> i128` and reads the result from
        // xmm0, but the callable is a gossamer `ret i128` (GP-register
        // pair). Hand it the `<16 x i8>` forwarding thunk in place of the
        // callable's own address; the thunk reads slot 0 of the same env
        // this call already passes, so a capturing closure, a
        // non-capturing one, and a bare fn all route the same way.
        let spawn_wide_cabi = symbol == crate::emit::CABI_SPAWN_SHIM
            && crate::emit::target_is_windows()
            && args
                .get(crate::emit::CABI_SPAWN_RET_WORDS_ARG)
                .and_then(|w| crate::emit::operand_const_int(self.body, w))
                == Some(crate::emit::SPAWN_RET_TWO_WORDS);
        let mut arg_text = String::new();
        let mut arg_tys_for_decl: Vec<String> = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                arg_text.push_str(", ");
            }
            if spawn_wide_cabi && i == 0 {
                let code = self.fresh();
                writeln!(
                    self.out,
                    "  {code} = ptrtoint ptr @\"{}\" to i64",
                    crate::emit::SPAWN_WIDE_CABI_THUNK
                )
                .unwrap();
                let _ = write!(arg_text, "i64 {code}");
                continue;
            }
            if (heap_push_by_address || skey_by_address) && i == 1 {
                let addr = if skey_by_address {
                    self.skey_slot_address(arg)?
                } else {
                    self.elem_slot_address(arg)?
                };
                let _ = write!(arg_text, "ptr {addr}");
                arg_tys_for_decl.push("ptr".to_string());
                continue;
            }
            let want = expected_param_tys.get(i).copied().flatten();
            let (a_v, mut a_ty) = self.lower_call_arg(arg, want)?;
            if result_new_heap_copy
                && i == 1
                && let Some(heap_v) = self
                    .maybe_heap_copy_value_enum(arg)
                    .or_else(|| self.maybe_heap_copy_aggregate(arg))
            {
                let _ = write!(arg_text, "i64 {heap_v}");
                continue;
            }
            if map_insert_heap_copy
                && i == 2
                && let Some(heap_v) = self
                    .maybe_heap_copy_value_enum(arg)
                    .or_else(|| self.maybe_heap_copy_aggregate_for_map(arg))
            {
                let _ = write!(arg_text, "i64 {heap_v}");
                minted_map_blob = Some(heap_v);
                continue;
            }
            if skey_insert_heap_copy
                && i == 3
                && let Some(heap_v) = self
                    .maybe_heap_copy_value_enum(arg)
                    .or_else(|| self.maybe_heap_copy_aggregate_for_map(arg))
            {
                let _ = write!(arg_text, "i64 {heap_v}");
                minted_map_blob = Some(heap_v);
                continue;
            }
            if chan_send_spill && i == 1 {
                // A by-value aggregate (struct / tuple / array) lives in
                // the sending frame's stack alloca; storing its address
                // into the channel hands the receiver - on its own
                // goroutine stack - a pointer that dangles the moment the
                // sender's frame is reused. Heap-copy it (RC-aware) so the
                // channel carries a stable pointer the receiver owns,
                // matching the `gos_rt_result_new` Ok-payload path.
                // An `Option` or `Result` element is a two-word carrier, which
                // no 8-byte channel slot holds. It boxes the way a value enum
                // does everywhere else a single word must carry one, so the
                // element word is the address of the carrier.
                if let Some(heap_v) = self
                    .maybe_heap_copy_aggregate(arg)
                    .or_else(|| self.maybe_heap_copy_value_enum(arg))
                {
                    let slot = self.entry_alloca("i64");
                    writeln!(self.out, "  store i64 {heap_v}, ptr {slot}").unwrap();
                    let _ = write!(arg_text, "ptr {slot}");
                    continue;
                }
                let slot = self.entry_alloca("i64");
                let stored_ty;
                let stored_val;
                if a_ty == "ptr" {
                    stored_ty = "ptr".to_string();
                    stored_val = a_v.clone();
                } else if a_ty == "double" || a_ty == "float" {
                    stored_ty = "double".to_string();
                    stored_val = a_v.clone();
                } else if a_ty.starts_with('i') && a_ty != "i64" {
                    let widened = self.fresh();
                    writeln!(self.out, "  {widened} = zext {a_ty} {a_v} to i64").unwrap();
                    stored_ty = "i64".to_string();
                    stored_val = widened;
                } else {
                    stored_ty = "i64".to_string();
                    stored_val = a_v.clone();
                }
                writeln!(self.out, "  store {stored_ty} {stored_val}, ptr {slot}").unwrap();
                let _ = write!(arg_text, "ptr {slot}");
                continue;
            }
            // If `a_ty` came back as `void` (Unit operand) or as
            // a different ABI-compatible shape than the registry
            // declares, coerce so the call instruction's argument
            // types match the dedup'd canonical declaration.
            if let Some(want_ty) = registry_param_llvm.get(i) {
                // Win64: a 2-word `i128` (Fat) argument crosses the
                // `extern "C"` boundary by pointer (rustc's `__int128` ABI),
                // not in a GP register pair. Spill into a 16-byte-aligned slot
                // and pass its address; the registry declaration renders the
                // param as `ptr` to match. No-op on SysV. (Mirrors
                // `lower_runtime_call_intrinsic`.)
                if crate::emit::target_is_windows() && want_ty == "i128" {
                    let v = if a_ty == "i128" {
                        a_v.clone()
                    } else {
                        self.coerce_llvm_value(&a_v, &a_ty, "i128")
                    };
                    let fat = self.fat_i128_call_arg(&v);
                    let _ = write!(arg_text, "{fat}");
                    continue;
                }
                if a_ty == "void" || a_ty.is_empty() {
                    let zero = match want_ty.as_str() {
                        "ptr" => "null".to_string(),
                        "double" => "0.0".to_string(),
                        _ => "0".to_string(),
                    };
                    let _ = write!(arg_text, "{want_ty} {zero}");
                    continue;
                }
                if &a_ty != want_ty {
                    let coerced = self.coerce_llvm_value(&a_v, &a_ty, want_ty);
                    let _ = write!(arg_text, "{want_ty} {coerced}");
                    a_ty.clone_from(want_ty);
                    let _ = a_ty;
                    continue;
                }
            }
            arg_tys_for_decl.push(a_ty.clone());
            let _ = write!(arg_text, "{a_ty} {a_v}");
        }
        let dest_ty_mir = self.body.local_ty(destination.local);
        let dest_ty = render_ty(self.tcx, dest_ty_mir);

        // Every `gos_rt_*` declaration MUST come from the typed
        // ABI registry - that's the single source of truth for the
        // LLVM IR shape. `declare_rt` panics on an unknown symbol,
        // which is the correct behaviour: the prior synthesised
        // path invented a signature from operand types at the call
        // site, and a stale or mistyped operand would silently
        // emit a `declare` whose params didn't match the runtime's
        // C-ABI definition, producing miscompiles instead of a
        // build-time error. User-defined functions are defined in
        // this module and need no `declare`.
        if symbol.starts_with("gos_rt_") {
            declare_rt(&mut self.runtime_refs, symbol);
        }

        // Look up the registry's canonical return type for
        // `gos_rt_*` symbols. The call instruction's return type
        // drives the x86_64 calling convention's read register
        // (rax for integer / ptr, xmm0 for double on both SysV
        // and Win64). When the surrounding code writes a runtime
        // helper's result into a destination slot whose type
        // differs from the helper's declared return (e.g. reading
        // an f64 element out of a `Vec<f64>` via the i64-shaped
        // `gos_rt_vec_get_i64`), the call MUST be typed with the
        // helper's actual return so the caller reads the right
        // register. Linux SysV LLVM 18 happens to normalise the
        // mismatch through the `declare i64` line and a memory
        // spill/reload pair; Windows mingw-w64-x86_64-llvm 18
        // does NOT - it honours the call-site type literally, so
        // the caller reads xmm0 while the function wrote rax and
        // the load yields stale FP state instead of the i64 we
        // returned. The visible symptom on Windows was
        // `halved[0] = 2` instead of `0.5` for any `Fn(f64) ->
        // f64` closure result threaded through
        // `gos_rt_vec_get_i64` (the closure result bytes never
        // made it through the read).
        let registry_ret: Option<String> = if symbol.starts_with("gos_rt_") {
            gossamer_abi::lookup(symbol).map(|e| e.sig.ret.llvm_ir().to_string())
        } else {
            None
        };
        let dest_is_void = dest_ty == "void" || is_unit(self.tcx, dest_ty_mir);
        let registry_says_void = registry_ret.as_deref() == Some("void");
        if symbol.starts_with("gos_binding_") {
            // External `[rust-bindings]` symbols are defined in the
            // linked per-project staticlib, not in this module -
            // synthesize the `declare` from the call-site types (the
            // MIR binding lowering typed the args from the binding's
            // signature metadata, so every call site agrees). Only
            // the default arg path runs for binding symbols, so
            // `arg_tys_for_decl` covers every argument.
            let ret = if dest_is_void {
                "void"
            } else {
                dest_ty.as_str()
            };
            self.runtime_refs.insert(format!(
                "declare {ret} @{symbol}({})",
                arg_tys_for_decl.join(", ")
            ));
        }
        if dest_is_void || registry_says_void {
            // Either the destination is unit-typed (caller
            // discards the return) or the registry declares the
            // helper as `void`. Emit a void call; if the dest is
            // non-unit but the helper returns void, fill the
            // slot with a zero of the dest's shape so any
            // accidental read doesn't see undefined memory.
            writeln!(self.out, "  call void @\"{symbol}\"({arg_text})").unwrap();
            self.release_minted_map_blob(minted_map_blob.as_deref());
            if !dest_is_void {
                self.store_zero_to_place(destination, &dest_ty);
            }
        } else {
            let tmp = self.fresh();
            // Aggregate destinations come back as a heap pointer the
            // subsequent memcpy / store dereferences. User functions
            // heap-copy their return (see the Return lowering), so the
            // call return is `ptr` and matches `dest_ty`. A `gos_rt_*`
            // helper, however, may return a *scalar* `i64` that IS the
            // box pointer (e.g. `gos_rt_map_get_or_i64` over a
            // struct-valued map hands back the stored i64 handle, not a
            // pointer to fresh storage). Typing that call `ptr` against
            // the registry's `i64` declaration is an LLVM IR
            // return-type mismatch: Linux SysV LLVM coerces the
            // same-width integer/pointer return register and hides it,
            // but Windows x64-64 LLVM honours the call-site type
            // literally, so the caller reads the wrong register and the
            // program crashes before any output. Honour the registry
            // return for `gos_rt_*` symbols, then inttoptr the scalar
            // into the pointer the memcpy expects. Non-aggregate
            // destinations already take the registry return.
            let call_ret_ty = if is_aggregate(self.tcx, dest_ty_mir) {
                if symbol.starts_with("gos_rt_") {
                    registry_ret.clone().unwrap_or_else(|| dest_ty.clone())
                } else {
                    dest_ty.clone()
                }
            } else {
                registry_ret.clone().unwrap_or_else(|| dest_ty.clone())
            };
            // Win64: a 2-word `i128` (Fat) return comes back in a 16-byte
            // vector register (`<16 x i8>`), matching rustc; call as that wire
            // type then `bitcast` back to the `i128` the rest of the body uses.
            // `call_ret_ty` (the logical type) is unchanged for the downstream
            // store/coerce. Gated on the ABI-registry return so it fires ONLY
            // for `gos_rt_*` shims, never a user function's bare-`i128` return
            // (which is `define i128`/`ret i128` in this same module). No-op on
            // SysV.
            let win_fat_ret = super::misc::needs_win64_fat_ret(
                crate::emit::target_is_windows(),
                registry_ret.as_deref(),
            );
            let wire_ret_ty = if win_fat_ret {
                "<16 x i8>"
            } else {
                &call_ret_ty
            };
            writeln!(
                self.out,
                "  {tmp} = call {wire_ret_ty} @\"{symbol}\"({arg_text})"
            )
            .unwrap();
            self.release_minted_map_blob(minted_map_blob.as_deref());
            let tmp = if win_fat_ret {
                let unwrapped = self.fresh();
                writeln!(self.out, "  {unwrapped} = bitcast <16 x i8> {tmp} to i128").unwrap();
                unwrapped
            } else {
                tmp
            };
            // A `gos_rt_*` helper returning a scalar `i64` box pointer into
            // an aggregate destination handed us an integer; the memcpy /
            // store below needs a `ptr`. Widening the integer to a pointer
            // is a no-op bit-reinterpret on every target (both are the
            // 64-bit GP return register); the preceding call is already
            // typed with the registry's `i64` so the caller reads the
            // correct register on Windows x64.
            let tmp = if is_aggregate(self.tcx, dest_ty_mir) && call_ret_ty == "i64" {
                let ptr_val = self.fresh();
                writeln!(self.out, "  {ptr_val} = inttoptr i64 {tmp} to ptr").unwrap();
                ptr_val
            } else {
                tmp
            };
            let slot = if destination.projection.is_empty() {
                local_slot(destination.local)
            } else {
                self.lower_place_address(destination)
            };
            if is_aggregate(self.tcx, dest_ty_mir) {
                if call_ret_ty == "i128" && slot_count(self.tcx, dest_ty_mir) == Some(2) {
                    // Result and Option use an inline two-word carrier. Runtime
                    // helpers return that carrier by value, so store it into the
                    // aggregate slot directly instead of treating it as a heap
                    // pointer and memcpying through the packed integer.
                    writeln!(self.out, "  store i128 {tmp}, ptr {slot}, align 8").unwrap();
                } else if let Some(slots) = slot_count(self.tcx, dest_ty_mir) {
                    // Inline aggregate (struct / tuple / array with a
                    // known field layout): the callee handed us a heap
                    // pointer to fresh storage. Copy the known slots
                    // into our destination's inline alloca so
                    // subsequent field reads use the same flat-slot
                    // shape that locally built aggregates use.
                    let bytes = u64::from(slots.max(1)) * 8;
                    writeln!(
                        self.out,
                        "  call void @llvm.memcpy.p0.p0.i64(ptr {slot}, ptr {tmp}, i64 {bytes}, i1 false)"
                    )
                    .unwrap();
                    // A user function returning an inline aggregate heap-copies
                    // it (see the Return lowering) so the pointer outlives the
                    // callee frame. We've now copied its slots into our own
                    // destination, so that buffer is dead - free it (a shallow
                    // dealloc; any RC field pointers it held now live in the
                    // destination slot). Without this, every struct/tuple/array
                    // returned by value leaks its buffer. Runtime accessors
                    // (`gos_rt_vec_get_ptr`, …) instead return a BORROWED
                    // pointer into a container, which must never be freed here.
                    if !symbol.starts_with("gos_rt_") {
                        declare_rt(&mut self.runtime_refs, "gos_rt_aggr_free");
                        writeln!(
                            self.out,
                            "  call void @\"gos_rt_aggr_free\"(ptr {tmp}, i64 {bytes})"
                        )
                        .unwrap();
                    }
                } else {
                    // Handle-Adt with no inline layout (recursive enum,
                    // opaque sentinel struct): the slot holds an 8-byte
                    // heap handle and the runtime returned that handle
                    // value. Store it directly - memcpy'ing would copy
                    // the cell's first word (the discriminant) into the
                    // slot, so the next discriminant / field read would
                    // double-indirect through it and crash. Mirrors the
                    // store-the-handle shape used by enum construction.
                    self.store_value_to_place(destination, "ptr", &tmp);
                }
            } else if call_ret_ty != dest_ty {
                // Registry-typed call result differs from the
                // destination slot's MIR-derived shape. The
                // runtime helpers (gos_rt_vec_get_i64,
                // gos_rt_arr_get, …) store and return values as
                // raw 8-byte slots regardless of source type, so
                // the i64 → double / i64 → ptr conversion at the
                // boundary is a bit-reinterpret, NOT a numeric
                // conversion. Reach for `bitcast` (and ptr-int
                // detours) directly instead of going through
                // `coerce_llvm_value`, whose i64↔double path uses
                // `sitofp` / `fptosi` semantics which would turn
                // the bit pattern of `0.5` (0x3FE0…) into the
                // f64 value `4.5e18`.
                let coerced = if call_ret_ty == "i64" && (dest_ty == "double" || dest_ty == "float")
                {
                    let mid = self.fresh();
                    writeln!(self.out, "  {mid} = bitcast i64 {tmp} to {dest_ty}").unwrap();
                    mid
                } else if (call_ret_ty == "double" || call_ret_ty == "float") && dest_ty == "i64" {
                    let mid = self.fresh();
                    writeln!(self.out, "  {mid} = bitcast {call_ret_ty} {tmp} to i64").unwrap();
                    mid
                } else {
                    self.coerce_llvm_value(&tmp, &call_ret_ty, &dest_ty)
                };
                self.store_value_to_place(destination, &dest_ty, &coerced);
            } else {
                self.store_value_to_place(destination, &dest_ty, &tmp);
            }
        }
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }
}
