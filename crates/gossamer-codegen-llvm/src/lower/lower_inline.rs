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
use gossamer_mir::{BasicBlock, Body, ConstValue, Operand, Place, Projection, UnOp};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};

impl<'a> Lowerer<'a> {
    /// Inline fast path for `gos_rt_stream_write_byte(stream, b)`.
    ///
    /// The stdout case dominates the fasta benchmark (50M+
    /// calls). Going through an FFI call for every byte spends
    /// hundreds of millions of nanoseconds in PLT + stack-frame
    /// setup alone. Inlining the buffer-append (load len,
    /// bounds check, store byte, increment len) cuts those
    /// hot-loop calls down to ~5 instructions each.
    ///
    /// Shape:
    /// ```llvm
    ///   %fd = load i32, ptr %stream
    ///   %is_stdout = icmp eq i32 %fd, 1
    ///   br i1 %is_stdout, label %fast_check, label %slow
    /// fast_check:
    ///   %len = load i64, ptr @GOS_RT_STDOUT_LEN
    ///   %full = icmp uge i64 %len, 8192
    ///   br i1 %full, label %slow, label %append
    /// append:
    ///   %dst = getelementptr i8, ptr @GOS_RT_STDOUT_BYTES, i64 %len
    ///   %byte = trunc i64 %b to i8
    ///   store i8 %byte, ptr %dst
    ///   %newlen = add i64 %len, 1
    ///   store i64 %newlen, ptr @GOS_RT_STDOUT_LEN
    ///   br label %end
    /// slow:
    ///   call void @gos_rt_stream_write_byte_slow(ptr %stream, i64 %b)
    ///   br label %end
    /// end:
    /// ```
    pub(crate) fn lower_stream_write_byte_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        for sym in [
            "gos_rt_stdout_acquire",
            "gos_rt_stdout_release",
            "gos_rt_stream_write_byte",
        ] {
            declare_rt(&mut self.runtime_refs, sym);
        }
        let stream_v = self.lower_operand(&args[0])?;
        let byte_v = self.lower_operand(&args[1])?;
        // Suffix to keep block labels unique within a function.
        let suffix = self.next_ssa;
        self.next_ssa += 1;
        let fast_check = format!("wb_check_{suffix}");
        let append = format!("wb_append_{suffix}");
        let slow = format!("wb_slow_{suffix}");
        let end = format!("wb_end_{suffix}");

        // Read fd and route stdout (fd==1) to the fast path.
        // `!invariant.load` tells LLVM the fd field of a stream
        // never changes after construction (every stream the
        // runtime exposes is a static `&STREAM_*`), so the load
        // can be hoisted out of containing loops. Without the
        // hint LLVM keeps a per-iteration `cmpl $1, (%stream)`
        // which is the hot path of fasta's inner loop.
        let fd = self.fresh();
        writeln!(
            self.out,
            "  {fd} = load i32, ptr {stream_v}, !invariant.load !0"
        )
        .unwrap();
        let is_stdout = self.fresh();
        writeln!(self.out, "  {is_stdout} = icmp eq i32 {fd}, 1").unwrap();
        writeln!(
            self.out,
            "  br i1 {is_stdout}, label %{fast_check}, label %{slow}"
        )
        .unwrap();

        // fast_check: bounds-check the buffer. Take the
        // process-global stdout lock first so this thread's
        // load+store on `@GOS_RT_STDOUT_LEN` cannot tear against
        // a concurrent goroutine on another worker thread.
        // `gos_rt_stdout_acquire` / `_release` wrap a
        // `parking_lot::RawMutex`; uncontended cost is ~10 ns.
        writeln!(self.out, "{fast_check}:").unwrap();
        writeln!(self.out, "  call void @gos_rt_stdout_acquire()").unwrap();
        let len = self.fresh();
        writeln!(self.out, "  {len} = load i64, ptr @GOS_RT_STDOUT_LEN").unwrap();
        let full = self.fresh();
        writeln!(self.out, "  {full} = icmp uge i64 {len}, 8192").unwrap();
        // On overflow we still hold the lock - release before
        // routing to the slow call path so the slow path can
        // re-acquire through the safe Rust guard.
        let full_release = format!("wb_full_rel_{suffix}");
        writeln!(
            self.out,
            "  br i1 {full}, label %{full_release}, label %{append}"
        )
        .unwrap();
        writeln!(self.out, "{full_release}:").unwrap();
        writeln!(self.out, "  call void @gos_rt_stdout_release()").unwrap();
        writeln!(self.out, "  br label %{slow}").unwrap();

        // append: store the byte at bytes[len], bump len, release.
        writeln!(self.out, "{append}:").unwrap();
        let dst = self.fresh();
        writeln!(
            self.out,
            "  {dst} = getelementptr i8, ptr @GOS_RT_STDOUT_BYTES, i64 {len}"
        )
        .unwrap();
        let byte_8 = self.fresh();
        writeln!(self.out, "  {byte_8} = trunc i64 {byte_v} to i8").unwrap();
        writeln!(self.out, "  store i8 {byte_8}, ptr {dst}").unwrap();
        let newlen = self.fresh();
        writeln!(self.out, "  {newlen} = add i64 {len}, 1").unwrap();
        writeln!(self.out, "  store i64 {newlen}, ptr @GOS_RT_STDOUT_LEN").unwrap();
        writeln!(self.out, "  call void @gos_rt_stdout_release()").unwrap();
        writeln!(self.out, "  br label %{end}").unwrap();

        // slow: full-call path. The runtime helper acquires the
        // lock itself through the safe `StdoutGuard`.
        writeln!(self.out, "{slow}:").unwrap();
        writeln!(
            self.out,
            "  call void @gos_rt_stream_write_byte(ptr {stream_v}, i64 {byte_v})"
        )
        .unwrap();
        writeln!(self.out, "  br label %{end}").unwrap();

        // Merge.
        writeln!(self.out, "{end}:").unwrap();
        // Destination is `()`; nothing to store.
        let _ = destination;
        match target {
            Some(t) => writeln!(self.out, "  br label %bb{}", t.as_u32()).unwrap(),
            None => writeln!(self.out, "  unreachable").unwrap(),
        }
        Ok(())
    }

    /// Inline fast path for `gos_rt_heap_i64_set(v, idx,
    /// val)`. The `GosI64Vec` is laid out as
    /// `{ i64 len; ptr data }` (8-byte aligned); we load
    /// `data` from offset 8, index it by `idx`, store `val`.
    /// Null vec / out-of-range `idx` -> no-op (see body comment),
    /// matching the `gos_rt_heap_i64_set` shim.
    pub(crate) fn lower_heap_i64_set_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let v = self.lower_operand(&args[0])?;
        let idx = self.lower_operand(&args[1])?;
        // The inline bounds check and address math operate on i64; widen a
        // narrow-typed index (`u8`/`u16`/`u32`/`i8`/`i16`/`i32`) first so the
        // emitted `icmp i64` / `mul i64` don't reference an i32 SSA value.
        let idx = self.widen_to_i64(&args[1], &idx);
        let val = self.lower_operand(&args[2])?;
        // Null vec / out-of-range idx -> no-op, matching the
        // `gos_rt_heap_i64_set` shim. Without this guard an
        // out-of-range index stored into arbitrary heap memory.
        let s = self.next_ssa;
        self.next_ssa += 1;
        let (check, store_b, cont) = (
            format!("hs_check_{s}"),
            format!("hs_store_{s}"),
            format!("hs_cont_{s}"),
        );
        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {v}, null").unwrap();
        writeln!(self.out, "  br i1 {isnull}, label %{cont}, label %{check}").unwrap();
        writeln!(self.out, "{check}:").unwrap();
        let len = self.fresh();
        writeln!(self.out, "  {len} = load i64, ptr {v}{TBAA_HEADER}").unwrap();
        // One unsigned compare catches both `idx < 0` (wraps to a huge
        // unsigned value, >= len) and `idx >= len`. A `GosVec` length is
        // always non-negative, so `(idx as u64) >= (len as u64)` is exactly
        // `idx < 0 || idx >= len`. LLVM can't fold the two signed compares
        // into this itself - `len` is a runtime load it can't prove >= 0.
        let bad = self.fresh();
        writeln!(self.out, "  {bad} = icmp uge i64 {idx}, {len}").unwrap();
        writeln!(self.out, "  br i1 {bad}, label %{cont}, label %{store_b}").unwrap();
        writeln!(self.out, "{store_b}:").unwrap();
        let data_ptr_addr = self.fresh();
        writeln!(
            self.out,
            "  {data_ptr_addr} = getelementptr i8, ptr {v}, i64 8"
        )
        .unwrap();
        let data = self.fresh();
        writeln!(
            self.out,
            "  {data} = load ptr, ptr {data_ptr_addr}{TBAA_HEADER}"
        )
        .unwrap();
        let dst = self.fresh();
        writeln!(
            self.out,
            "  {dst} = getelementptr i64, ptr {data}, i64 {idx}"
        )
        .unwrap();
        writeln!(self.out, "  store i64 {val}, ptr {dst}{TBAA_DATA}").unwrap();
        writeln!(self.out, "  br label %{cont}").unwrap();
        writeln!(self.out, "{cont}:").unwrap();
        let _ = destination;
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline fast path for `gos_rt_heap_i64_get(v, idx) ->
    /// i64`. Mirror of `lower_heap_i64_set_inline`.
    pub(crate) fn lower_heap_i64_get_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let v = self.lower_operand(&args[0])?;
        let idx = self.lower_operand(&args[1])?;
        // The inline bounds check and address math operate on i64; widen a
        // narrow-typed index (`u8`/`u16`/`u32`/`i8`/`i16`/`i32`) first so the
        // emitted `icmp i64` / `mul i64` don't reference an i32 SSA value.
        let idx = self.widen_to_i64(&args[1], &idx);
        // Null vec / out-of-range idx -> 0, matching the
        // `gos_rt_heap_i64_get` shim. Without this guard an
        // out-of-range index read arbitrary heap memory.
        let is_unit_dest = is_unit(self.tcx, self.body.local_ty(destination.local));
        let slot = local_slot(destination.local);
        let s = self.next_ssa;
        self.next_ssa += 1;
        let (check, load_b, dflt, cont) = (
            format!("hg_check_{s}"),
            format!("hg_load_{s}"),
            format!("hg_dflt_{s}"),
            format!("hg_cont_{s}"),
        );
        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {v}, null").unwrap();
        writeln!(self.out, "  br i1 {isnull}, label %{dflt}, label %{check}").unwrap();
        writeln!(self.out, "{check}:").unwrap();
        let len = self.fresh();
        writeln!(self.out, "  {len} = load i64, ptr {v}{TBAA_HEADER}").unwrap();
        // One unsigned compare catches both `idx < 0` (wraps to a huge
        // unsigned value, >= len) and `idx >= len`. A `GosVec` length is
        // always non-negative, so `(idx as u64) >= (len as u64)` is exactly
        // `idx < 0 || idx >= len`. LLVM can't fold the two signed compares
        // into this itself - `len` is a runtime load it can't prove >= 0.
        let bad = self.fresh();
        writeln!(self.out, "  {bad} = icmp uge i64 {idx}, {len}").unwrap();
        writeln!(self.out, "  br i1 {bad}, label %{dflt}, label %{load_b}").unwrap();
        writeln!(self.out, "{load_b}:").unwrap();
        let data_ptr_addr = self.fresh();
        writeln!(
            self.out,
            "  {data_ptr_addr} = getelementptr i8, ptr {v}, i64 8"
        )
        .unwrap();
        let data = self.fresh();
        writeln!(
            self.out,
            "  {data} = load ptr, ptr {data_ptr_addr}{TBAA_HEADER}"
        )
        .unwrap();
        let src = self.fresh();
        writeln!(
            self.out,
            "  {src} = getelementptr i64, ptr {data}, i64 {idx}"
        )
        .unwrap();
        let val = self.fresh();
        writeln!(self.out, "  {val} = load i64, ptr {src}{TBAA_DATA}").unwrap();
        if !is_unit_dest {
            writeln!(self.out, "  store i64 {val}, ptr {slot}").unwrap();
        }
        writeln!(self.out, "  br label %{cont}").unwrap();
        writeln!(self.out, "{dflt}:").unwrap();
        if !is_unit_dest {
            writeln!(self.out, "  store i64 0, ptr {slot}").unwrap();
        }
        writeln!(self.out, "  br label %{cont}").unwrap();
        writeln!(self.out, "{cont}:").unwrap();
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Coerce a GosVec operand to an LLVM `ptr` value.
    fn vec_operand_ptr(&mut self, op: &Operand) -> Result<String, BuildError> {
        let v = self.lower_operand(op)?;
        let ty = self.operand_llvm_ty(op);
        if ty == "ptr" {
            Ok(v)
        } else {
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = inttoptr {ty} {v} to ptr").unwrap();
            Ok(tmp)
        }
    }

    /// True when `op` is a Vec/Slice whose element provably occupies an
    /// 8-byte stride in every construction path: word-width ints and
    /// `f64` (`elem_bytes_of` maps them to 8, and every runtime
    /// constructor that returns such a vec passes 8). Byte buffers
    /// (`Vec<u8>` from `fs::read` / `crypto::rand_bytes` / HTTP
    /// `raw_bytes`) and `Vec<bool>` are stride 1, so anything narrower
    /// keeps the header-driven element-size load in the get/set paths.
    pub(crate) fn vec_operand_has_word_elem(&self, op: &Operand) -> bool {
        let Operand::Copy(pl) = op else {
            return false;
        };
        let mut ty = self.place_leaf_ty(pl);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
            ty = *inner;
        }
        let elem = match self.tcx.kind(ty) {
            Some(TyKind::Vec(e) | TyKind::Slice(e)) => *e,
            _ => return false,
        };
        matches!(
            self.tcx.kind(elem),
            Some(
                TyKind::Char
                    | TyKind::Int(
                        IntTy::I8
                            | IntTy::I16
                            | IntTy::I32
                            | IntTy::I64
                            | IntTy::I128
                            | IntTy::Isize
                            | IntTy::U16
                            | IntTy::U32
                            | IntTy::U64
                            | IntTy::U128
                            | IntTy::Usize
                    )
                    | TyKind::Float(FloatTy::F64)
                    | TyKind::Vec(_)
                    | TyKind::Slice(_)
            )
        )
    }

    /// True when the operand is a `Vec`/`[T]` whose element word is a handle
    /// the vector owns, so a store has to give back the share the slot held.
    /// Such a store goes through the runtime helper rather than the inline
    /// write.
    pub(crate) fn vec_operand_elem_owns_word(&self, op: &Operand) -> bool {
        let Operand::Copy(pl) = op else {
            return false;
        };
        let mut ty = self.place_leaf_ty(pl);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
            ty = *inner;
        }
        let elem = match self.tcx.kind(ty) {
            Some(TyKind::Vec(e) | TyKind::Slice(e)) => *e,
            _ => return false,
        };
        matches!(
            self.tcx.kind(elem),
            Some(
                TyKind::String
                    | TyKind::Vec(_)
                    | TyKind::Slice(_)
                    | TyKind::HashMap { .. }
                    | TyKind::JsonValue
            )
        ) || self.tcx.is_rc_managed(elem)
    }

    /// True when the operand is a `Vec`/`[T]` whose element type is statically
    /// `bool` - the only primitive stored at a 1-byte stride. Lets the inline
    /// get/set emit a constant-stride byte access (load/store `i8`) instead of
    /// loading `elem_bytes` from the header and branching on it per access.
    /// Type-erased vecs (unknown element) keep the dynamic-stride fallback.
    pub(crate) fn vec_operand_has_byte_elem(&self, op: &Operand) -> bool {
        let Operand::Copy(pl) = op else {
            return false;
        };
        let mut ty = self.place_leaf_ty(pl);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
            ty = *inner;
        }
        let elem = match self.tcx.kind(ty) {
            Some(TyKind::Vec(e) | TyKind::Slice(e)) => *e,
            _ => return false,
        };
        matches!(
            self.tcx.kind(elem),
            Some(TyKind::Bool | TyKind::Int(IntTy::U8))
        )
    }

    /// True when `op` is a Vec/Slice whose element is itself a
    /// Vec/Slice - an 8-byte heap-pointer slot. Indexing one returns
    /// the borrowed inner-vec pointer, a plain word load with no
    /// retain or copy, so the inline get applies even though the
    /// destination is `ptr`-typed.
    pub(crate) fn vec_operand_elem_is_vec(&self, op: &Operand) -> bool {
        let Operand::Copy(pl) = op else {
            return false;
        };
        let mut ty = self.place_leaf_ty(pl);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
            ty = *inner;
        }
        let elem = match self.tcx.kind(ty) {
            Some(TyKind::Vec(e) | TyKind::Slice(e)) => *e,
            _ => return false,
        };
        matches!(self.tcx.kind(elem), Some(TyKind::Vec(_) | TyKind::Slice(_)))
    }

    /// Inline fast path for `gos_rt_vec_get_i64(vec, idx) -> i64`. The valid
    /// path loads directly from `ptr + idx*elem_bytes`; the null/out-of-range
    /// path tail-calls the checked runtime helper so diagnostics and panic
    /// behavior stay canonical without carrying the panic block in every hot
    /// function. GosVec layout: `len@0, elem_bytes@16, ptr@24` (mirrors
    /// `lower_vec_len_inline`).
    pub(crate) fn lower_vec_get_i64_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let word_elem = self.vec_operand_has_word_elem(&args[0]);
        let byte_elem = !word_elem && self.vec_operand_has_byte_elem(&args[0]);
        let vec_ptr = self.vec_operand_ptr(&args[0])?;
        let idx = self.lower_operand(&args[1])?;
        // The inline bounds check and address math operate on i64; widen a
        // narrow-typed index (`u8`/`u16`/`u32`/`i8`/`i16`/`i32`) first so the
        // emitted `icmp i64` / `mul i64` don't reference an i32 SSA value.
        let idx = self.widen_to_i64(&args[1], &idx);
        let dest_ty = render_ty(self.tcx, self.body.local_ty(destination.local));
        let dest_slot = local_slot(destination.local);
        let s = self.next_ssa;
        self.next_ssa += 1;
        let (check, load, slow_null, slow_oob, cont) = (
            format!("vg_check_{s}"),
            format!("vg_load_{s}"),
            format!("vg_slow_null_{s}"),
            format!("vg_slow_oob_{s}"),
            format!("vg_cont_{s}"),
        );
        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {vec_ptr}, null").unwrap();
        writeln!(
            self.out,
            "  br i1 {isnull}, label %{slow_null}, label %{check}"
        )
        .unwrap();
        writeln!(self.out, "{check}:").unwrap();
        let len = self.fresh();
        writeln!(self.out, "  {len} = load i64, ptr {vec_ptr}{TBAA_HEADER}").unwrap();
        // One unsigned compare catches both `idx < 0` (wraps to a huge
        // unsigned value, >= len) and `idx >= len`. A `GosVec` length is
        // always non-negative, so `(idx as u64) >= (len as u64)` is exactly
        // `idx < 0 || idx >= len`. LLVM can't fold the two signed compares
        // into this itself - `len` is a runtime load it can't prove >= 0.
        let bad = self.fresh();
        writeln!(self.out, "  {bad} = icmp uge i64 {idx}, {len}").unwrap();
        writeln!(self.out, "  br i1 {bad}, label %{slow_oob}, label %{load}").unwrap();
        writeln!(self.out, "{load}:").unwrap();
        // Word-stride elements skip the header `elem_bytes` load: the
        // index scales by a constant 8 that folds into the address
        // mode, instead of a dependent load + mul on every access.
        // Other vecs read the stride from the header and pick the
        // load width to match: shims like `fs::read` / `crypto::
        // rand_bytes` / HTTP `raw_bytes` hand out packed
        // `elem_bytes == 1` byte buffers, where an i64-wide load
        // would pull in neighbouring bytes (and read past the
        // buffer tail on the last elements).
        let loaded = if word_elem {
            let off = self.fresh();
            writeln!(self.out, "  {off} = mul i64 {idx}, 8").unwrap();
            let ea = self.vec_elem_addr(&vec_ptr, &off);
            let loaded = self.fresh();
            writeln!(self.out, "  {loaded} = load i64, ptr {ea}{TBAA_DATA}").unwrap();
            loaded
        } else if byte_elem {
            // Statically-bool element: 1-byte stride, so the offset is the
            // index itself. One `i8` load, no header `elem_bytes` load and no
            // `is_byte` branch.
            let ea = self.vec_elem_addr(&vec_ptr, &idx);
            let b8 = self.fresh();
            writeln!(self.out, "  {b8} = load i8, ptr {ea}{TBAA_DATA}").unwrap();
            let b64 = self.fresh();
            writeln!(self.out, "  {b64} = zext i8 {b8} to i64").unwrap();
            b64
        } else {
            let eb_addr = self.fresh();
            writeln!(
                self.out,
                "  {eb_addr} = getelementptr i8, ptr {vec_ptr}, i64 16"
            )
            .unwrap();
            let eb32 = self.fresh();
            writeln!(self.out, "  {eb32} = load i32, ptr {eb_addr}{TBAA_HEADER}").unwrap();
            let eb = self.fresh();
            writeln!(self.out, "  {eb} = zext i32 {eb32} to i64").unwrap();
            let off = self.fresh();
            writeln!(self.out, "  {off} = mul i64 {idx}, {eb}").unwrap();
            let ea = self.vec_elem_addr(&vec_ptr, &off);
            let (byte_b, word_b, join_b) = (
                format!("vg_byte_{s}"),
                format!("vg_word_{s}"),
                format!("vg_join_{s}"),
            );
            let is_byte = self.fresh();
            writeln!(self.out, "  {is_byte} = icmp eq i64 {eb}, 1").unwrap();
            writeln!(
                self.out,
                "  br i1 {is_byte}, label %{byte_b}, label %{word_b}"
            )
            .unwrap();
            writeln!(self.out, "{byte_b}:").unwrap();
            let b8 = self.fresh();
            writeln!(self.out, "  {b8} = load i8, ptr {ea}{TBAA_DATA}").unwrap();
            let b64 = self.fresh();
            writeln!(self.out, "  {b64} = zext i8 {b8} to i64").unwrap();
            writeln!(self.out, "  br label %{join_b}").unwrap();
            writeln!(self.out, "{word_b}:").unwrap();
            let w64 = self.fresh();
            writeln!(self.out, "  {w64} = load i64, ptr {ea}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  br label %{join_b}").unwrap();
            writeln!(self.out, "{join_b}:").unwrap();
            let loaded = self.fresh();
            writeln!(
                self.out,
                "  {loaded} = phi i64 [ {b64}, %{byte_b} ], [ {w64}, %{word_b} ]"
            )
            .unwrap();
            loaded
        };
        self.store_i64_as(&loaded, &dest_ty, &dest_slot);
        writeln!(self.out, "  br label %{cont}").unwrap();
        let cold_start = self.out.len();
        if matches!(crate::emit::opt_profile(), crate::emit::OptProfile::Release) {
            declare_rt(&mut self.runtime_refs, "gos_rt_panic_oob");
            let (label, _) = self.strings.borrow_mut().intern("vec index");
            writeln!(self.out, "{slow_null}:").unwrap();
            writeln!(
                self.out,
                "  call void @gos_rt_panic_oob(ptr {label}, i64 {idx}, i64 0)"
            )
            .unwrap();
            writeln!(self.out, "  unreachable").unwrap();
            writeln!(self.out, "{slow_oob}:").unwrap();
            writeln!(
                self.out,
                "  call void @gos_rt_panic_oob(ptr {label}, i64 {idx}, i64 {len})"
            )
            .unwrap();
            writeln!(self.out, "  unreachable").unwrap();
        } else {
            declare_rt(&mut self.runtime_refs, "gos_rt_vec_get_i64");
            for label in [&slow_null, &slow_oob] {
                writeln!(self.out, "{label}:").unwrap();
                self.emit_panic_site_line();
                let checked = self.fresh();
                writeln!(
                    self.out,
                    "  {checked} = call i64 @gos_rt_vec_get_i64(ptr {vec_ptr}, i64 {idx})"
                )
                .unwrap();
                self.store_i64_as(&checked, &dest_ty, &dest_slot);
                writeln!(self.out, "  br label %{cont}").unwrap();
            }
        }
        self.mark_cold(cold_start);
        writeln!(self.out, "{cont}:").unwrap();
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline fast path for `gos_rt_vec_get_i64_unchecked(vec, idx) -> i64`.
    /// Identical element load to [`Self::lower_vec_get_i64_inline`] but WITHOUT the
    /// null guard and bounds compare/branch: the MIR emits this call only from
    /// the counted-loop element read, where the index is a fresh `0..len`
    /// induction over this same vec and the loop header only branches into the
    /// body while `counter < len` - so the receiver is non-null and the index
    /// is provably in `[0, len)`. Dropping the guard leaves a straight load
    /// (branch-free for word-stride elements) that LLVM keeps in the inner
    /// loop.
    pub(crate) fn lower_vec_get_i64_unchecked_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let word_elem = self.vec_operand_has_word_elem(&args[0]);
        let byte_elem = !word_elem && self.vec_operand_has_byte_elem(&args[0]);
        let vec_ptr = self.vec_operand_ptr(&args[0])?;
        let idx = self.lower_operand(&args[1])?;
        // The address math operates on i64; widen a narrow-typed index
        // (`u8`/`u16`/`u32`/`i8`/`i16`/`i32`) first so the emitted `mul i64`
        // doesn't reference an i32 SSA value.
        let idx = self.widen_to_i64(&args[1], &idx);
        let dest_ty = render_ty(self.tcx, self.body.local_ty(destination.local));
        let dest_slot = local_slot(destination.local);
        let s = self.next_ssa;
        self.next_ssa += 1;
        // Word-stride elements scale by a constant 8 that folds into the
        // address mode; narrower vecs read the stride from the header and
        // pick the load width to match (a packed `elem_bytes == 1` byte
        // buffer must not be read i64-wide). Mirrors the checked reader's
        // load, minus the surrounding bounds control flow.
        let loaded = if word_elem {
            let off = self.fresh();
            writeln!(self.out, "  {off} = mul i64 {idx}, 8").unwrap();
            let ea = self.vec_elem_addr(&vec_ptr, &off);
            let loaded = self.fresh();
            writeln!(self.out, "  {loaded} = load i64, ptr {ea}{TBAA_DATA}").unwrap();
            loaded
        } else if byte_elem {
            // Statically-bool element: 1-byte stride, so the offset is the
            // index itself. One `i8` load, no header `elem_bytes` load and no
            // `is_byte` branch.
            let ea = self.vec_elem_addr(&vec_ptr, &idx);
            let b8 = self.fresh();
            writeln!(self.out, "  {b8} = load i8, ptr {ea}{TBAA_DATA}").unwrap();
            let b64 = self.fresh();
            writeln!(self.out, "  {b64} = zext i8 {b8} to i64").unwrap();
            b64
        } else {
            let eb_addr = self.fresh();
            writeln!(
                self.out,
                "  {eb_addr} = getelementptr i8, ptr {vec_ptr}, i64 16"
            )
            .unwrap();
            let eb32 = self.fresh();
            writeln!(self.out, "  {eb32} = load i32, ptr {eb_addr}{TBAA_HEADER}").unwrap();
            let eb = self.fresh();
            writeln!(self.out, "  {eb} = zext i32 {eb32} to i64").unwrap();
            let off = self.fresh();
            writeln!(self.out, "  {off} = mul i64 {idx}, {eb}").unwrap();
            let ea = self.vec_elem_addr(&vec_ptr, &off);
            let (byte_b, word_b, join_b) = (
                format!("vgu_byte_{s}"),
                format!("vgu_word_{s}"),
                format!("vgu_join_{s}"),
            );
            let is_byte = self.fresh();
            writeln!(self.out, "  {is_byte} = icmp eq i64 {eb}, 1").unwrap();
            writeln!(
                self.out,
                "  br i1 {is_byte}, label %{byte_b}, label %{word_b}"
            )
            .unwrap();
            writeln!(self.out, "{byte_b}:").unwrap();
            let b8 = self.fresh();
            writeln!(self.out, "  {b8} = load i8, ptr {ea}{TBAA_DATA}").unwrap();
            let b64 = self.fresh();
            writeln!(self.out, "  {b64} = zext i8 {b8} to i64").unwrap();
            writeln!(self.out, "  br label %{join_b}").unwrap();
            writeln!(self.out, "{word_b}:").unwrap();
            let w64 = self.fresh();
            writeln!(self.out, "  {w64} = load i64, ptr {ea}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  br label %{join_b}").unwrap();
            writeln!(self.out, "{join_b}:").unwrap();
            let loaded = self.fresh();
            writeln!(
                self.out,
                "  {loaded} = phi i64 [ {b64}, %{byte_b} ], [ {w64}, %{word_b} ]"
            )
            .unwrap();
            loaded
        };
        self.store_i64_as(&loaded, &dest_ty, &dest_slot);
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline `gos_rt_min_i64` / `gos_rt_max_i64` as a branchless
    /// `icmp`+`select`. Value-identical to the runtime `a.min(b)` /
    /// `a.max(b)` for `i64` (parity holds on every tier), but it drops the
    /// per-call FFI boundary from hot loops - the Levenshtein DP cell does
    /// two `min` per iteration - and, being branchless, no longer blocks the
    /// loop vectorizer the way an opaque call did.
    pub(crate) fn lower_scalar_minmax_i64_inline(
        &mut self,
        is_min: bool,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let a = self.lower_operand(&args[0])?;
        let a = self.widen_to_i64(&args[0], &a);
        let b = self.lower_operand(&args[1])?;
        let b = self.widen_to_i64(&args[1], &b);
        let dest_ty = render_ty(self.tcx, self.body.local_ty(destination.local));
        let dest_slot = local_slot(destination.local);
        let cmp = self.fresh();
        let pred = if is_min { "slt" } else { "sgt" };
        writeln!(self.out, "  {cmp} = icmp {pred} i64 {a}, {b}").unwrap();
        let r = self.fresh();
        writeln!(self.out, "  {r} = select i1 {cmp}, i64 {a}, i64 {b}").unwrap();
        self.store_i64_as(&r, &dest_ty, &dest_slot);
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline fast path for `gos_rt_vec_set_i64(vec, idx, val)`. Valid indices
    /// store directly; invalid indexing uses the checked runtime helper so the
    /// panic path stays canonical and out of the hot function body.
    pub(crate) fn lower_vec_set_i64_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let word_elem = self.vec_operand_has_word_elem(&args[0]);
        let byte_elem = !word_elem && self.vec_operand_has_byte_elem(&args[0]);
        let vec_ptr = self.vec_operand_ptr(&args[0])?;
        let idx = self.lower_operand(&args[1])?;
        // The inline bounds check and address math operate on i64; widen a
        // narrow-typed index (`u8`/`u16`/`u32`/`i8`/`i16`/`i32`) first so the
        // emitted `icmp i64` / `mul i64` don't reference an i32 SSA value.
        let idx = self.widen_to_i64(&args[1], &idx);
        let val_v = self.lower_operand(&args[2])?;
        let val_ty = self.operand_llvm_ty(&args[2]);
        let val = self.value_to_i64(&val_v, &val_ty);
        let s = self.next_ssa;
        self.next_ssa += 1;
        let (check, store_b, slow_null, slow_oob, cont) = (
            format!("vs_check_{s}"),
            format!("vs_store_{s}"),
            format!("vs_slow_null_{s}"),
            format!("vs_slow_oob_{s}"),
            format!("vs_cont_{s}"),
        );
        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {vec_ptr}, null").unwrap();
        writeln!(
            self.out,
            "  br i1 {isnull}, label %{slow_null}, label %{check}"
        )
        .unwrap();
        writeln!(self.out, "{check}:").unwrap();
        let len = self.fresh();
        writeln!(self.out, "  {len} = load i64, ptr {vec_ptr}{TBAA_HEADER}").unwrap();
        // One unsigned compare catches both `idx < 0` (wraps to a huge
        // unsigned value, >= len) and `idx >= len`. A `GosVec` length is
        // always non-negative, so `(idx as u64) >= (len as u64)` is exactly
        // `idx < 0 || idx >= len`. LLVM can't fold the two signed compares
        // into this itself - `len` is a runtime load it can't prove >= 0.
        let bad = self.fresh();
        writeln!(self.out, "  {bad} = icmp uge i64 {idx}, {len}").unwrap();
        writeln!(
            self.out,
            "  br i1 {bad}, label %{slow_oob}, label %{store_b}"
        )
        .unwrap();
        writeln!(self.out, "{store_b}:").unwrap();
        // Word-stride elements skip the header `elem_bytes` load: the
        // index scales by a constant 8 that folds into the address
        // mode, instead of a dependent load + mul on every access.
        // Other vecs match the store width to the header stride -
        // an i64-wide store into a packed `elem_bytes == 1` byte
        // buffer (`fs::read` / `crypto::rand_bytes` / HTTP
        // `raw_bytes`) would clobber the seven neighbouring bytes
        // and write past the buffer tail on the last elements.
        if word_elem {
            let off = self.fresh();
            writeln!(self.out, "  {off} = mul i64 {idx}, 8").unwrap();
            let ea = self.vec_elem_addr(&vec_ptr, &off);
            writeln!(self.out, "  store i64 {val}, ptr {ea}{TBAA_DATA}").unwrap();
        } else if byte_elem {
            // Statically-bool element: 1-byte stride, so the offset is the
            // index itself. One `i8` store, no header `elem_bytes` load and no
            // `is_byte` branch.
            let ea = self.vec_elem_addr(&vec_ptr, &idx);
            let v8 = self.fresh();
            writeln!(self.out, "  {v8} = trunc i64 {val} to i8").unwrap();
            writeln!(self.out, "  store i8 {v8}, ptr {ea}{TBAA_DATA}").unwrap();
        } else {
            let eb_addr = self.fresh();
            writeln!(
                self.out,
                "  {eb_addr} = getelementptr i8, ptr {vec_ptr}, i64 16"
            )
            .unwrap();
            let eb32 = self.fresh();
            writeln!(self.out, "  {eb32} = load i32, ptr {eb_addr}{TBAA_HEADER}").unwrap();
            let eb = self.fresh();
            writeln!(self.out, "  {eb} = zext i32 {eb32} to i64").unwrap();
            let off = self.fresh();
            writeln!(self.out, "  {off} = mul i64 {idx}, {eb}").unwrap();
            let ea = self.vec_elem_addr(&vec_ptr, &off);
            let (byte_b, word_b) = (format!("vs_byte_{s}"), format!("vs_word_{s}"));
            let is_byte = self.fresh();
            writeln!(self.out, "  {is_byte} = icmp eq i64 {eb}, 1").unwrap();
            writeln!(
                self.out,
                "  br i1 {is_byte}, label %{byte_b}, label %{word_b}"
            )
            .unwrap();
            writeln!(self.out, "{byte_b}:").unwrap();
            let v8 = self.fresh();
            writeln!(self.out, "  {v8} = trunc i64 {val} to i8").unwrap();
            writeln!(self.out, "  store i8 {v8}, ptr {ea}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  br label %{cont}").unwrap();
            writeln!(self.out, "{word_b}:").unwrap();
            writeln!(self.out, "  store i64 {val}, ptr {ea}{TBAA_DATA}").unwrap();
        }
        writeln!(self.out, "  br label %{cont}").unwrap();
        let cold_start = self.out.len();
        if matches!(crate::emit::opt_profile(), crate::emit::OptProfile::Release) {
            declare_rt(&mut self.runtime_refs, "gos_rt_panic_oob");
            let (label, _) = self.strings.borrow_mut().intern("vec index");
            writeln!(self.out, "{slow_null}:").unwrap();
            writeln!(
                self.out,
                "  call void @gos_rt_panic_oob(ptr {label}, i64 {idx}, i64 0)"
            )
            .unwrap();
            writeln!(self.out, "  unreachable").unwrap();
            writeln!(self.out, "{slow_oob}:").unwrap();
            writeln!(
                self.out,
                "  call void @gos_rt_panic_oob(ptr {label}, i64 {idx}, i64 {len})"
            )
            .unwrap();
            writeln!(self.out, "  unreachable").unwrap();
        } else {
            declare_rt(&mut self.runtime_refs, "gos_rt_vec_set_i64");
            for label in [&slow_null, &slow_oob] {
                writeln!(self.out, "{label}:").unwrap();
                self.emit_panic_site_line();
                writeln!(
                    self.out,
                    "  call void @gos_rt_vec_set_i64(ptr {vec_ptr}, i64 {idx}, i64 {val})"
                )
                .unwrap();
                writeln!(self.out, "  br label %{cont}").unwrap();
            }
        }
        self.mark_cold(cold_start);
        writeln!(self.out, "{cont}:").unwrap();
        let _ = destination;
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline fast path for `gos_rt_vec_set_i64_unchecked(vec, idx, val)`.
    /// Identical element store to [`Self::lower_vec_set_i64_inline`] but WITHOUT
    /// the null guard and bounds compare/branch: the MIR emits this call only
    /// from the bounds-check elision of a counted loop, where the index is a
    /// `0..len` induction over this same vec and the loop header only branches
    /// into the body while `counter < len` - so the receiver is non-null and the
    /// index is provably in `[0, len)`. Dropping the guard leaves a straight
    /// store (branch-free for word-stride elements).
    pub(crate) fn lower_vec_set_i64_unchecked_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let word_elem = self.vec_operand_has_word_elem(&args[0]);
        let byte_elem = !word_elem && self.vec_operand_has_byte_elem(&args[0]);
        let vec_ptr = self.vec_operand_ptr(&args[0])?;
        let idx = self.lower_operand(&args[1])?;
        // The address math operates on i64; widen a narrow-typed index first so
        // the emitted `mul i64` does not reference an i32 SSA value.
        let idx = self.widen_to_i64(&args[1], &idx);
        let val_v = self.lower_operand(&args[2])?;
        let val_ty = self.operand_llvm_ty(&args[2]);
        let val = self.value_to_i64(&val_v, &val_ty);
        let s = self.next_ssa;
        self.next_ssa += 1;
        // Word-stride elements scale by a constant 8 that folds into the address
        // mode; narrower vecs read the stride from the header and pick the store
        // width to match. Mirrors the checked writer's store, minus the
        // surrounding null/bounds control flow.
        if word_elem {
            let off = self.fresh();
            writeln!(self.out, "  {off} = mul i64 {idx}, 8").unwrap();
            let ea = self.vec_elem_addr(&vec_ptr, &off);
            writeln!(self.out, "  store i64 {val}, ptr {ea}{TBAA_DATA}").unwrap();
        } else if byte_elem {
            // Statically-bool element: 1-byte stride, so the offset is the
            // index itself. One `i8` store, no header `elem_bytes` load and no
            // `is_byte` branch.
            let ea = self.vec_elem_addr(&vec_ptr, &idx);
            let v8 = self.fresh();
            writeln!(self.out, "  {v8} = trunc i64 {val} to i8").unwrap();
            writeln!(self.out, "  store i8 {v8}, ptr {ea}{TBAA_DATA}").unwrap();
        } else {
            let eb_addr = self.fresh();
            writeln!(
                self.out,
                "  {eb_addr} = getelementptr i8, ptr {vec_ptr}, i64 16"
            )
            .unwrap();
            let eb32 = self.fresh();
            writeln!(self.out, "  {eb32} = load i32, ptr {eb_addr}{TBAA_HEADER}").unwrap();
            let eb = self.fresh();
            writeln!(self.out, "  {eb} = zext i32 {eb32} to i64").unwrap();
            let off = self.fresh();
            writeln!(self.out, "  {off} = mul i64 {idx}, {eb}").unwrap();
            let ea = self.vec_elem_addr(&vec_ptr, &off);
            let (byte_b, word_b, join_b) = (
                format!("vsu_byte_{s}"),
                format!("vsu_word_{s}"),
                format!("vsu_join_{s}"),
            );
            let is_byte = self.fresh();
            writeln!(self.out, "  {is_byte} = icmp eq i64 {eb}, 1").unwrap();
            writeln!(
                self.out,
                "  br i1 {is_byte}, label %{byte_b}, label %{word_b}"
            )
            .unwrap();
            writeln!(self.out, "{byte_b}:").unwrap();
            let v8 = self.fresh();
            writeln!(self.out, "  {v8} = trunc i64 {val} to i8").unwrap();
            writeln!(self.out, "  store i8 {v8}, ptr {ea}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  br label %{join_b}").unwrap();
            writeln!(self.out, "{word_b}:").unwrap();
            writeln!(self.out, "  store i64 {val}, ptr {ea}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  br label %{join_b}").unwrap();
            writeln!(self.out, "{join_b}:").unwrap();
        }
        let _ = destination;
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline fast path for `gos_rt_vec_swap_i64(vec, i, j)`. This preserves
    /// the scalar Vec semantics of the runtime helper: null receivers and
    /// out-of-range indices are no-ops, while in-range indices exchange one
    /// word- or byte-shaped element.
    pub(crate) fn lower_vec_swap_i64_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let word_elem = self.vec_operand_has_word_elem(&args[0]);
        let byte_elem = !word_elem && self.vec_operand_has_byte_elem(&args[0]);
        if !word_elem && !byte_elem {
            return Err(BuildError::InternalLoweringBug(
                "inline Vec::swap requires statically word- or byte-sized elements",
            ));
        }
        let vec_ptr = self.vec_operand_ptr(&args[0])?;
        let i_raw = self.lower_operand(&args[1])?;
        let i = self.widen_to_i64(&args[1], &i_raw);
        let j_raw = self.lower_operand(&args[2])?;
        let j = self.widen_to_i64(&args[2], &j_raw);
        let s = self.next_ssa;
        self.next_ssa += 1;
        let (check, check_j, swap, cont) = (
            format!("vsw_check_{s}"),
            format!("vsw_check_j_{s}"),
            format!("vsw_swap_{s}"),
            format!("vsw_cont_{s}"),
        );

        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {vec_ptr}, null").unwrap();
        writeln!(self.out, "  br i1 {isnull}, label %{cont}, label %{check}").unwrap();

        writeln!(self.out, "{check}:").unwrap();
        let len = self.fresh();
        writeln!(self.out, "  {len} = load i64, ptr {vec_ptr}{TBAA_HEADER}").unwrap();
        let i_bad = self.fresh();
        writeln!(self.out, "  {i_bad} = icmp uge i64 {i}, {len}").unwrap();
        writeln!(self.out, "  br i1 {i_bad}, label %{cont}, label %{check_j}").unwrap();

        writeln!(self.out, "{check_j}:").unwrap();
        let j_bad = self.fresh();
        writeln!(self.out, "  {j_bad} = icmp uge i64 {j}, {len}").unwrap();
        writeln!(self.out, "  br i1 {j_bad}, label %{cont}, label %{swap}").unwrap();

        writeln!(self.out, "{swap}:").unwrap();
        if word_elem {
            let i_off = self.fresh();
            writeln!(self.out, "  {i_off} = mul i64 {i}, 8").unwrap();
            let j_off = self.fresh();
            writeln!(self.out, "  {j_off} = mul i64 {j}, 8").unwrap();
            let i_addr = self.vec_elem_addr(&vec_ptr, &i_off);
            let j_addr = self.vec_elem_addr(&vec_ptr, &j_off);
            let a = self.fresh();
            writeln!(self.out, "  {a} = load i64, ptr {i_addr}{TBAA_DATA}").unwrap();
            let b = self.fresh();
            writeln!(self.out, "  {b} = load i64, ptr {j_addr}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  store i64 {b}, ptr {i_addr}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  store i64 {a}, ptr {j_addr}{TBAA_DATA}").unwrap();
        } else {
            let i_addr = self.vec_elem_addr(&vec_ptr, &i);
            let j_addr = self.vec_elem_addr(&vec_ptr, &j);
            let a = self.fresh();
            writeln!(self.out, "  {a} = load i8, ptr {i_addr}{TBAA_DATA}").unwrap();
            let b = self.fresh();
            writeln!(self.out, "  {b} = load i8, ptr {j_addr}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  store i8 {b}, ptr {i_addr}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  store i8 {a}, ptr {j_addr}{TBAA_DATA}").unwrap();
        }
        writeln!(self.out, "  br label %{cont}").unwrap();

        writeln!(self.out, "{cont}:").unwrap();
        if !is_unit(self.tcx, self.body.local_ty(destination.local)) {
            let dest_ty = render_ty(self.tcx, self.body.local_ty(destination.local));
            let dslot = local_slot(destination.local);
            let zero = match dest_ty.as_str() {
                "ptr" => "null",
                "double" | "float" => "0.0",
                _ => "0",
            };
            writeln!(self.out, "  store {dest_ty} {zero}, ptr {dslot}").unwrap();
        }
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline the in-bounds path of `gos_rt_vec_swap_safe(vec, i, j)`,
    /// retaining the runtime helper as the uncommon out-of-bounds path where
    /// it raises the bounds panic.
    pub(crate) fn lower_vec_swap_safe_inline(
        &mut self,
        args: &[Operand],
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let word_elem = self.vec_operand_has_word_elem(&args[0]);
        let byte_elem = !word_elem && self.vec_operand_has_byte_elem(&args[0]);
        if !word_elem && !byte_elem {
            return Err(BuildError::InternalLoweringBug(
                "inline Vec::swap requires statically word- or byte-sized elements",
            ));
        }

        let vec_ptr = self.vec_operand_ptr(&args[0])?;
        let i_raw = self.lower_operand(&args[1])?;
        let i = self.widen_to_i64(&args[1], &i_raw);
        let j_raw = self.lower_operand(&args[2])?;
        let j = self.widen_to_i64(&args[2], &j_raw);
        let s = self.next_ssa;
        self.next_ssa += 1;
        let (check, check_j, invalid, swap, join) = (
            format!("vss_check_{s}"),
            format!("vss_check_j_{s}"),
            format!("vss_invalid_{s}"),
            format!("vss_swap_{s}"),
            format!("vss_join_{s}"),
        );

        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {vec_ptr}, null").unwrap();
        writeln!(
            self.out,
            "  br i1 {isnull}, label %{invalid}, label %{check}"
        )
        .unwrap();

        writeln!(self.out, "{check}:").unwrap();
        let len = self.fresh();
        writeln!(self.out, "  {len} = load i64, ptr {vec_ptr}{TBAA_HEADER}").unwrap();
        let i_bad = self.fresh();
        writeln!(self.out, "  {i_bad} = icmp uge i64 {i}, {len}").unwrap();
        writeln!(
            self.out,
            "  br i1 {i_bad}, label %{invalid}, label %{check_j}"
        )
        .unwrap();

        writeln!(self.out, "{check_j}:").unwrap();
        let j_bad = self.fresh();
        writeln!(self.out, "  {j_bad} = icmp uge i64 {j}, {len}").unwrap();
        writeln!(self.out, "  br i1 {j_bad}, label %{invalid}, label %{swap}").unwrap();

        writeln!(self.out, "{invalid}:").unwrap();
        declare_rt(&mut self.runtime_refs, "gos_rt_vec_swap_safe");
        writeln!(
            self.out,
            "  call void @gos_rt_vec_swap_safe(ptr {vec_ptr}, i64 {i}, i64 {j})"
        )
        .unwrap();
        writeln!(self.out, "  br label %{join}").unwrap();

        writeln!(self.out, "{swap}:").unwrap();
        if word_elem {
            let i_off = self.fresh();
            writeln!(self.out, "  {i_off} = mul i64 {i}, 8").unwrap();
            let j_off = self.fresh();
            writeln!(self.out, "  {j_off} = mul i64 {j}, 8").unwrap();
            let i_addr = self.vec_elem_addr(&vec_ptr, &i_off);
            let j_addr = self.vec_elem_addr(&vec_ptr, &j_off);
            let a = self.fresh();
            writeln!(self.out, "  {a} = load i64, ptr {i_addr}{TBAA_DATA}").unwrap();
            let b = self.fresh();
            writeln!(self.out, "  {b} = load i64, ptr {j_addr}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  store i64 {b}, ptr {i_addr}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  store i64 {a}, ptr {j_addr}{TBAA_DATA}").unwrap();
        } else {
            let i_addr = self.vec_elem_addr(&vec_ptr, &i);
            let j_addr = self.vec_elem_addr(&vec_ptr, &j);
            let a = self.fresh();
            writeln!(self.out, "  {a} = load i8, ptr {i_addr}{TBAA_DATA}").unwrap();
            let b = self.fresh();
            writeln!(self.out, "  {b} = load i8, ptr {j_addr}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  store i8 {b}, ptr {i_addr}{TBAA_DATA}").unwrap();
            writeln!(self.out, "  store i8 {a}, ptr {j_addr}{TBAA_DATA}").unwrap();
        }
        writeln!(self.out, "  br label %{join}").unwrap();

        writeln!(self.out, "{join}:").unwrap();
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Emits the branchless inline body of `gos_rt_heap_u8_set(v, idx, val)`.
    /// `GosU8Vec` is `{ i64 len, ptr data }`; a null vec or out-of-range index
    /// redirects the store to a scratch byte, reproducing the runtime shim's
    /// no-op-on-OOB semantics without a per-byte FFI call. The len/data loads
    /// are header accesses ([`TBAA_HEADER`]) and the byte store is element data
    /// ([`TBAA_DATA`]), so `-O3` hoists the loop-invariant header loads out of
    /// the enclosing byte loop (fasta's hot inner loop).
    pub(crate) fn emit_heap_u8_set_branchless(&mut self, v: &str, idx: &str, val: &str) {
        self.runtime_refs
            .insert("@gos_u8_set_scratch = internal global [16 x i8] zeroinitializer".to_string());
        self.runtime_refs.insert(
            "@gos_u8_set_hdr = internal global { i64, ptr } { i64 0, ptr @gos_u8_set_scratch }"
                .to_string(),
        );
        let vnn = self.fresh();
        let vbase = self.fresh();
        let len = self.fresh();
        let dptr = self.fresh();
        let data = self.fresh();
        let ge0 = self.fresh();
        let lt = self.fresh();
        let inb = self.fresh();
        let elem = self.fresh();
        let target = self.fresh();
        let valb = self.fresh();
        writeln!(self.out, "  {vnn} = icmp ne ptr {v}, null").unwrap();
        writeln!(
            self.out,
            "  {vbase} = select i1 {vnn}, ptr {v}, ptr @gos_u8_set_hdr"
        )
        .unwrap();
        writeln!(self.out, "  {len} = load i64, ptr {vbase}{TBAA_HEADER}").unwrap();
        writeln!(
            self.out,
            "  {dptr} = getelementptr inbounds i8, ptr {vbase}, i64 8"
        )
        .unwrap();
        writeln!(self.out, "  {data} = load ptr, ptr {dptr}{TBAA_HEADER}").unwrap();
        writeln!(self.out, "  {ge0} = icmp sge i64 {idx}, 0").unwrap();
        writeln!(self.out, "  {lt} = icmp slt i64 {idx}, {len}").unwrap();
        writeln!(self.out, "  {inb} = and i1 {ge0}, {lt}").unwrap();
        writeln!(
            self.out,
            "  {elem} = getelementptr inbounds i8, ptr {data}, i64 {idx}"
        )
        .unwrap();
        writeln!(
            self.out,
            "  {target} = select i1 {inb}, ptr {elem}, ptr @gos_u8_set_scratch"
        )
        .unwrap();
        writeln!(self.out, "  {valb} = trunc i64 {val} to i8").unwrap();
        writeln!(self.out, "  store i8 {valb}, ptr {target}{TBAA_DATA}").unwrap();
    }

    /// Inline fast path for `gos_rt_heap_u8_set(v, idx, val)` on the
    /// `Terminator::Call` route (`buf.set_byte(i, x)`). Mirrors the
    /// `Rvalue::CallIntrinsic` inline; the destination is unit, so nothing is
    /// stored for it.
    pub(crate) fn lower_heap_u8_set_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let v = self.lower_operand(&args[0])?;
        let idx = self.lower_operand(&args[1])?;
        let idx = self.widen_to_i64(&args[1], &idx);
        let val = self.lower_operand(&args[2])?;
        self.emit_heap_u8_set_branchless(&v, &idx, &val);
        let _ = destination;
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline fast path for `gos_rt_vec_get_ptr(vec, idx) -> ptr` when the
    /// destination is a bare element *pointer* (a `&elem`-typed local that a
    /// following field projection dereferences, e.g. `table[j].1` /
    /// `bodies[i].field`). Reproduces the runtime shim exactly - null vec /
    /// out-of-range idx yields null, else `data + idx * elem_bytes`. Removes a
    /// per-probe FFI call from the linear-search hot loop and lets `-O3` hoist
    /// the loop-invariant len / elem_bytes / data-pointer header loads.
    ///
    /// Only applies when the destination is `ptr`-typed: a multi-slot aggregate
    /// destination copies the whole element out of the returned address (the
    /// generic call-result path's memcpy), which this inline does not do.
    pub(crate) fn lower_vec_get_ptr_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let vec_ptr = self.vec_operand_ptr(&args[0])?;
        let idx = self.lower_operand(&args[1])?;
        let idx = self.widen_to_i64(&args[1], &idx);
        let dest_slot = local_slot(destination.local);
        let s = self.next_ssa;
        self.next_ssa += 1;
        let (check, load, dflt, cont) = (
            format!("vgp_check_{s}"),
            format!("vgp_load_{s}"),
            format!("vgp_dflt_{s}"),
            format!("vgp_cont_{s}"),
        );
        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {vec_ptr}, null").unwrap();
        writeln!(self.out, "  br i1 {isnull}, label %{dflt}, label %{check}").unwrap();
        writeln!(self.out, "{check}:").unwrap();
        let len = self.fresh();
        writeln!(self.out, "  {len} = load i64, ptr {vec_ptr}{TBAA_HEADER}").unwrap();
        // One unsigned compare catches both `idx < 0` and `idx >= len`; a
        // GosVec length is always non-negative.
        let bad = self.fresh();
        writeln!(self.out, "  {bad} = icmp uge i64 {idx}, {len}").unwrap();
        writeln!(self.out, "  br i1 {bad}, label %{dflt}, label %{load}").unwrap();
        writeln!(self.out, "{load}:").unwrap();
        let eb_addr = self.fresh();
        writeln!(
            self.out,
            "  {eb_addr} = getelementptr i8, ptr {vec_ptr}, i64 16"
        )
        .unwrap();
        let eb32 = self.fresh();
        writeln!(self.out, "  {eb32} = load i32, ptr {eb_addr}{TBAA_HEADER}").unwrap();
        let eb = self.fresh();
        writeln!(self.out, "  {eb} = zext i32 {eb32} to i64").unwrap();
        let off = self.fresh();
        writeln!(self.out, "  {off} = mul i64 {idx}, {eb}").unwrap();
        let ea = self.vec_elem_addr(&vec_ptr, &off);
        writeln!(self.out, "  store ptr {ea}, ptr {dest_slot}").unwrap();
        writeln!(self.out, "  br label %{cont}").unwrap();
        writeln!(self.out, "{dflt}:").unwrap();
        writeln!(self.out, "  store ptr null, ptr {dest_slot}").unwrap();
        writeln!(self.out, "  br label %{cont}").unwrap();
        writeln!(self.out, "{cont}:").unwrap();
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Emits the element address for a GosVec: loads the data
    /// pointer from header offset 24 and offsets it by `off` bytes.
    fn vec_elem_addr(&mut self, vec_ptr: &str, off: &str) -> String {
        let dptr_addr = self.fresh();
        writeln!(
            self.out,
            "  {dptr_addr} = getelementptr i8, ptr {vec_ptr}, i64 24"
        )
        .unwrap();
        let dptr = self.fresh();
        writeln!(
            self.out,
            "  {dptr} = load ptr, ptr {dptr_addr}{TBAA_HEADER}"
        )
        .unwrap();
        let ea = self.fresh();
        writeln!(self.out, "  {ea} = getelementptr i8, ptr {dptr}, i64 {off}").unwrap();
        ea
    }

    /// Store an i64 SSA value into `dest_slot` coerced to `dest_ty`.
    fn store_i64_as(&mut self, val_i64: &str, dest_ty: &str, dest_slot: &str) {
        match dest_ty {
            "i64" => {
                writeln!(self.out, "  store i64 {val_i64}, ptr {dest_slot}").unwrap();
            }
            "i32" | "i16" | "i8" | "i1" => {
                let t = self.fresh();
                writeln!(self.out, "  {t} = trunc i64 {val_i64} to {dest_ty}").unwrap();
                writeln!(self.out, "  store {dest_ty} {t}, ptr {dest_slot}").unwrap();
            }
            "ptr" => {
                let t = self.fresh();
                writeln!(self.out, "  {t} = inttoptr i64 {val_i64} to ptr").unwrap();
                writeln!(self.out, "  store ptr {t}, ptr {dest_slot}").unwrap();
            }
            "double" => {
                let t = self.fresh();
                writeln!(self.out, "  {t} = bitcast i64 {val_i64} to double").unwrap();
                writeln!(self.out, "  store double {t}, ptr {dest_slot}").unwrap();
            }
            _ => {
                writeln!(self.out, "  store i64 {val_i64}, ptr {dest_slot}").unwrap();
            }
        }
    }

    /// Coerce an SSA value of `val_ty` to i64 (for storing into a vec slot).
    fn value_to_i64(&mut self, val: &str, val_ty: &str) -> String {
        match val_ty {
            "i64" => val.to_string(),
            // A `bool` element's stored byte is its canonical `0` / `1`, which
            // every reader compares against and passes on as a whole byte, so
            // it widens by zero-extension.
            "i1" => {
                let t = self.fresh();
                writeln!(self.out, "  {t} = zext i1 {val} to i64").unwrap();
                t
            }
            "i32" | "i16" | "i8" => {
                let t = self.fresh();
                writeln!(self.out, "  {t} = sext {val_ty} {val} to i64").unwrap();
                t
            }
            "ptr" => {
                let t = self.fresh();
                writeln!(self.out, "  {t} = ptrtoint ptr {val} to i64").unwrap();
                t
            }
            "double" => {
                let t = self.fresh();
                writeln!(self.out, "  {t} = bitcast double {val} to i64").unwrap();
                t
            }
            _ => val.to_string(),
        }
    }

    /// Renders a Fat (`i128`, the 2-word Result/Option) argument for a
    /// `gos_rt_*` call. On Win64 an `i128` crosses the `extern "C"` boundary
    /// by pointer (rustc's `__int128` ABI, matched by the `ptr` param that
    /// `RuntimeEntry::llvm_declare` renders there), so spill the value into a
    /// 16-byte slot and pass `ptr <slot>`; on SysV pass the bare `i128 <val>`.
    /// Every site that hands an `i128` to a runtime helper MUST route through
    /// this so the call instruction matches the declaration on Windows.
    pub(crate) fn fat_i128_call_arg(&mut self, val: &str) -> String {
        if crate::emit::target_is_windows() {
            let slot = self.entry_alloca("i128, align 16");
            writeln!(self.out, "  store i128 {val}, ptr {slot}, align 16").unwrap();
            format!("ptr {slot}")
        } else {
            format!("i128 {val}")
        }
    }

    /// Inline fast path for `gos_rt_vec_len(v) -> i64`. The
    /// `GosVec` heap struct stores `len: i64` at offset 0, so the
    /// runtime helper degenerates to one load. Inlining skips the
    /// FFI call entirely; LLVM then hoists the load when `v` is
    /// loop-invariant.
    pub(crate) fn lower_vec_len_inline(
        &mut self,
        arg: &Operand,
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let v = self.lower_operand(arg)?;
        // Null-guarded: the runtime returns 0 for a null vec (the
        // empty representation), so the inline load must too.
        let s_id = self.next_ssa;
        self.next_ssa += 1;
        let (ll, lz, lc) = (
            format!("vl_l_{s_id}"),
            format!("vl_z_{s_id}"),
            format!("vl_c_{s_id}"),
        );
        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {v}, null").unwrap();
        writeln!(self.out, "  br i1 {isnull}, label %{lz}, label %{ll}").unwrap();
        writeln!(self.out, "{ll}:").unwrap();
        let n = self.fresh();
        writeln!(self.out, "  {n} = load i64, ptr {v}{TBAA_HEADER}").unwrap();
        writeln!(self.out, "  br label %{lc}").unwrap();
        writeln!(self.out, "{lz}:").unwrap();
        writeln!(self.out, "  br label %{lc}").unwrap();
        writeln!(self.out, "{lc}:").unwrap();
        let tmp = self.fresh();
        writeln!(self.out, "  {tmp} = phi i64 [ {n}, %{ll} ], [ 0, %{lz} ]").unwrap();
        if !is_unit(self.tcx, self.body.local_ty(destination.local)) {
            let slot = local_slot(destination.local);
            writeln!(self.out, "  store i64 {tmp}, ptr {slot}").unwrap();
        }
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Emits the test that `s` is a compiler-typed Gossamer string whose
    /// header the caller may read inline, branching to `typed` or `slow`.
    ///
    /// A body pointer carries a fixed low-bit shape and is preceded by a tag
    /// byte, and both are checked before anything reads in front of it: a
    /// foreign C string reaching a `String` parameter has neither, and the
    /// bytes before it belong to whoever placed it.
    fn emit_typed_string_guard(&mut self, s: &str, typed: &str, slow: &str) {
        use gossamer_abi::string_layout as sl;
        let id = self.next_ssa;
        self.next_ssa += 1;
        let (shape_b, tag_b) = (format!("sg_shape_{id}"), format!("sg_tag_{id}"));
        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {s}, null").unwrap();
        writeln!(
            self.out,
            "  br i1 {isnull}, label %{slow}, label %{shape_b}"
        )
        .unwrap();
        writeln!(self.out, "{shape_b}:").unwrap();
        let addr = self.fresh();
        let low = self.fresh();
        let shaped = self.fresh();
        writeln!(self.out, "  {addr} = ptrtoint ptr {s} to i64").unwrap();
        writeln!(self.out, "  {low} = and i64 {addr}, {}", sl::BODY_ADDR_MASK).unwrap();
        writeln!(
            self.out,
            "  {shaped} = icmp eq i64 {low}, {}",
            sl::BODY_ADDR_TAG
        )
        .unwrap();
        writeln!(self.out, "  br i1 {shaped}, label %{tag_b}, label %{slow}").unwrap();
        writeln!(self.out, "{tag_b}:").unwrap();
        let tag_ptr = self.fresh();
        let tag = self.fresh();
        let tag_z = self.fresh();
        writeln!(
            self.out,
            "  {tag_ptr} = getelementptr i8, ptr {s}, i64 {}",
            sl::TAG_OFFSET
        )
        .unwrap();
        writeln!(self.out, "  {tag} = load i8, ptr {tag_ptr}{TBAA_HEADER}").unwrap();
        writeln!(self.out, "  {tag_z} = zext i8 {tag} to i32").unwrap();
        let mut acc: Option<String> = None;
        for candidate in sl::HEADER_TAGS {
            let eq = self.fresh();
            writeln!(self.out, "  {eq} = icmp eq i32 {tag_z}, {candidate}").unwrap();
            acc = Some(match acc {
                None => eq,
                Some(prev) => {
                    let or = self.fresh();
                    writeln!(self.out, "  {or} = or i1 {prev}, {eq}").unwrap();
                    or
                }
            });
        }
        let typed_flag = acc.unwrap_or_else(|| "false".to_string());
        writeln!(
            self.out,
            "  br i1 {typed_flag}, label %{typed}, label %{slow}"
        )
        .unwrap();
    }

    /// Loads a typed string's `len` field, its content's byte length.
    fn emit_typed_string_byte_len(&mut self, s: &str) -> String {
        use gossamer_abi::string_layout as sl;
        let len_ptr = self.fresh();
        let len32 = self.fresh();
        let len = self.fresh();
        writeln!(
            self.out,
            "  {len_ptr} = getelementptr i8, ptr {s}, i64 {}",
            sl::LEN_OFFSET
        )
        .unwrap();
        writeln!(
            self.out,
            "  {len32} = load i32, ptr {len_ptr}, align 1{TBAA_HEADER}"
        )
        .unwrap();
        writeln!(self.out, "  {len} = zext i32 {len32} to i64").unwrap();
        len
    }

    /// Loads the first word of a typed string's character index, which is the
    /// character count or [`INDEX_ASCII`] when a character index equals its
    /// byte offset.
    ///
    /// [`INDEX_ASCII`]: gossamer_abi::string_layout::INDEX_ASCII
    fn emit_typed_string_index_head(&mut self, s: &str) -> String {
        use gossamer_abi::string_layout as sl;
        let cap_ptr = self.fresh();
        let cap32 = self.fresh();
        let cap = self.fresh();
        let foot_off = self.fresh();
        let foot_ptr = self.fresh();
        let head = self.fresh();
        writeln!(
            self.out,
            "  {cap_ptr} = getelementptr i8, ptr {s}, i64 {}",
            sl::CAP_OFFSET
        )
        .unwrap();
        writeln!(
            self.out,
            "  {cap32} = load i32, ptr {cap_ptr}, align 1{TBAA_HEADER}"
        )
        .unwrap();
        writeln!(self.out, "  {cap} = zext i32 {cap32} to i64").unwrap();
        writeln!(self.out, "  {foot_off} = add i64 {cap}, 1").unwrap();
        writeln!(
            self.out,
            "  {foot_ptr} = getelementptr i8, ptr {s}, i64 {foot_off}"
        )
        .unwrap();
        writeln!(
            self.out,
            "  {head} = load i32, ptr {foot_ptr}, align 1{TBAA_HEADER}"
        )
        .unwrap();
        head
    }

    /// Inline fast path for `gos_rt_str_byte_at(s, i) -> i64`.
    ///
    /// The whole operation is a guarded byte load, and the shim is called once
    /// per input byte by any scanner, so the call itself is the cost. An index
    /// outside the content answers zero, as the shim does.
    pub(crate) fn lower_str_byte_at_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let s = self.lower_operand(&args[0])?;
        let i_raw = self.lower_operand(&args[1])?;
        let i = self.widen_to_i64(&args[1], &i_raw);
        let id = self.next_ssa;
        self.next_ssa += 1;
        let (typed_b, read_b, zero_b, slow_b, cont_b) = (
            format!("sba_t_{id}"),
            format!("sba_r_{id}"),
            format!("sba_z_{id}"),
            format!("sba_s_{id}"),
            format!("sba_c_{id}"),
        );
        self.emit_typed_string_guard(&s, &typed_b, &slow_b);
        writeln!(self.out, "{typed_b}:").unwrap();
        let len = self.emit_typed_string_byte_len(&s);
        let neg = self.fresh();
        let past = self.fresh();
        let oob = self.fresh();
        writeln!(self.out, "  {neg} = icmp slt i64 {i}, 0").unwrap();
        writeln!(self.out, "  {past} = icmp sge i64 {i}, {len}").unwrap();
        writeln!(self.out, "  {oob} = or i1 {neg}, {past}").unwrap();
        writeln!(self.out, "  br i1 {oob}, label %{zero_b}, label %{read_b}").unwrap();
        writeln!(self.out, "{read_b}:").unwrap();
        let byte_ptr = self.fresh();
        let byte = self.fresh();
        let byte_z = self.fresh();
        writeln!(
            self.out,
            "  {byte_ptr} = getelementptr i8, ptr {s}, i64 {i}"
        )
        .unwrap();
        writeln!(self.out, "  {byte} = load i8, ptr {byte_ptr}{TBAA_DATA}").unwrap();
        writeln!(self.out, "  {byte_z} = zext i8 {byte} to i64").unwrap();
        writeln!(self.out, "  br label %{cont_b}").unwrap();
        writeln!(self.out, "{zero_b}:").unwrap();
        writeln!(self.out, "  br label %{cont_b}").unwrap();
        writeln!(self.out, "{slow_b}:").unwrap();
        declare_rt(&mut self.runtime_refs, "gos_rt_str_byte_at");
        let slow = self.fresh();
        writeln!(
            self.out,
            "  {slow} = call i64 @gos_rt_str_byte_at(ptr {s}, i64 {i})"
        )
        .unwrap();
        writeln!(self.out, "  br label %{cont_b}").unwrap();
        writeln!(self.out, "{cont_b}:").unwrap();
        let out = self.fresh();
        writeln!(
            self.out,
            "  {out} = phi i64 [ {byte_z}, %{read_b} ], [ 0, %{zero_b} ], [ {slow}, %{slow_b} ]"
        )
        .unwrap();
        self.store_inline_i64_result(&out, destination);
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline fast path for `gos_rt_str_byte_len(s) -> i64`.
    pub(crate) fn lower_str_byte_len_inline(
        &mut self,
        arg: &Operand,
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let s = self.lower_operand(arg)?;
        let id = self.next_ssa;
        self.next_ssa += 1;
        let (typed_b, slow_b, cont_b) = (
            format!("sbl_t_{id}"),
            format!("sbl_s_{id}"),
            format!("sbl_c_{id}"),
        );
        self.emit_typed_string_guard(&s, &typed_b, &slow_b);
        writeln!(self.out, "{typed_b}:").unwrap();
        let len = self.emit_typed_string_byte_len(&s);
        writeln!(self.out, "  br label %{cont_b}").unwrap();
        writeln!(self.out, "{slow_b}:").unwrap();
        declare_rt(&mut self.runtime_refs, "gos_rt_str_byte_len");
        let slow = self.fresh();
        writeln!(
            self.out,
            "  {slow} = call i64 @gos_rt_str_byte_len(ptr {s})"
        )
        .unwrap();
        writeln!(self.out, "  br label %{cont_b}").unwrap();
        writeln!(self.out, "{cont_b}:").unwrap();
        let out = self.fresh();
        writeln!(
            self.out,
            "  {out} = phi i64 [ {len}, %{typed_b} ], [ {slow}, %{slow_b} ]"
        )
        .unwrap();
        self.store_inline_i64_result(&out, destination);
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline fast path for `gos_rt_str_char_at(s, i) -> i64`.
    ///
    /// Only the all-ASCII case is inline. There a character index is a byte
    /// offset, so the read is the same guarded byte load `byte_at` does;
    /// anything else needs the index blocks and the UTF-8 decode the shim
    /// already implements.
    pub(crate) fn lower_str_char_at_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        use gossamer_abi::string_layout as sl;
        let s = self.lower_operand(&args[0])?;
        let i_raw = self.lower_operand(&args[1])?;
        let i = self.widen_to_i64(&args[1], &i_raw);
        let id = self.next_ssa;
        self.next_ssa += 1;
        let (typed_b, ascii_b, read_b, slow_b, cont_b) = (
            format!("sca_t_{id}"),
            format!("sca_a_{id}"),
            format!("sca_r_{id}"),
            format!("sca_s_{id}"),
            format!("sca_c_{id}"),
        );
        self.emit_typed_string_guard(&s, &typed_b, &slow_b);
        writeln!(self.out, "{typed_b}:").unwrap();
        let head = self.emit_typed_string_index_head(&s);
        let is_ascii = self.fresh();
        writeln!(
            self.out,
            "  {is_ascii} = icmp eq i32 {head}, {}",
            sl::INDEX_ASCII
        )
        .unwrap();
        writeln!(
            self.out,
            "  br i1 {is_ascii}, label %{ascii_b}, label %{slow_b}"
        )
        .unwrap();
        writeln!(self.out, "{ascii_b}:").unwrap();
        let len = self.emit_typed_string_byte_len(&s);
        let neg = self.fresh();
        let past = self.fresh();
        let oob = self.fresh();
        writeln!(self.out, "  {neg} = icmp slt i64 {i}, 0").unwrap();
        writeln!(self.out, "  {past} = icmp sge i64 {i}, {len}").unwrap();
        writeln!(self.out, "  {oob} = or i1 {neg}, {past}").unwrap();
        // An index outside the content panics, so it leaves the fast path for
        // the shim that words and raises it.
        writeln!(self.out, "  br i1 {oob}, label %{slow_b}, label %{read_b}").unwrap();
        writeln!(self.out, "{read_b}:").unwrap();
        let byte_ptr = self.fresh();
        let byte = self.fresh();
        let byte_z = self.fresh();
        writeln!(
            self.out,
            "  {byte_ptr} = getelementptr i8, ptr {s}, i64 {i}"
        )
        .unwrap();
        writeln!(self.out, "  {byte} = load i8, ptr {byte_ptr}{TBAA_DATA}").unwrap();
        writeln!(self.out, "  {byte_z} = zext i8 {byte} to i64").unwrap();
        writeln!(self.out, "  br label %{cont_b}").unwrap();
        writeln!(self.out, "{slow_b}:").unwrap();
        declare_rt(&mut self.runtime_refs, "gos_rt_str_char_at");
        let slow = self.fresh();
        writeln!(
            self.out,
            "  {slow} = call i64 @gos_rt_str_char_at(ptr {s}, i64 {i})"
        )
        .unwrap();
        writeln!(self.out, "  br label %{cont_b}").unwrap();
        writeln!(self.out, "{cont_b}:").unwrap();
        let out = self.fresh();
        writeln!(
            self.out,
            "  {out} = phi i64 [ {byte_z}, %{read_b} ], [ {slow}, %{slow_b} ]"
        )
        .unwrap();
        self.store_inline_i64_result(&out, destination);
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Stores an inline fast path's `i64` result, skipping a unit destination.
    ///
    /// The destination's own type decides the store: a character index answers
    /// a `char`, which is narrower than the word the fast path computed.
    fn store_inline_i64_result(&mut self, value: &str, destination: &Place) {
        let dest_ty = self.body.local_ty(destination.local);
        if is_unit(self.tcx, dest_ty) {
            return;
        }
        let rendered = render_ty(self.tcx, dest_ty);
        let slot = local_slot(destination.local);
        self.store_i64_as(value, &rendered, &slot);
    }

    /// Inline fast path for `gos_rt_str_len(s) -> i64`.
    pub(crate) fn lower_str_len_inline(
        &mut self,
        arg: &Operand,
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        if let Some(len) = self.const_string_len(arg) {
            if !is_unit(self.tcx, self.body.local_ty(destination.local)) {
                let slot = local_slot(destination.local);
                writeln!(self.out, "  store i64 {len}, ptr {slot}").unwrap();
            }
            emit_terminator_branch(&mut self.out, target);
            return Ok(());
        }
        use gossamer_abi::string_layout as sl;
        let s_v = self.lower_operand(arg)?;
        // A runtime string carries its character count in the index that
        // follows its content, and the all-ASCII case states itself with a
        // sentinel so the count is the byte length. Reading both inline is
        // what lets `while i < s.len()` hoist its bound out of the loop: an
        // opaque call there is re-evaluated on every iteration.
        let id = self.next_ssa;
        self.next_ssa += 1;
        let (typed_b, ascii_b, slow_b, cont_b) = (
            format!("sl_t_{id}"),
            format!("sl_a_{id}"),
            format!("sl_s_{id}"),
            format!("sl_c_{id}"),
        );
        self.emit_typed_string_guard(&s_v, &typed_b, &slow_b);
        writeln!(self.out, "{typed_b}:").unwrap();
        let head = self.emit_typed_string_index_head(&s_v);
        let is_ascii = self.fresh();
        writeln!(
            self.out,
            "  {is_ascii} = icmp eq i32 {head}, {}",
            sl::INDEX_ASCII
        )
        .unwrap();
        writeln!(
            self.out,
            "  br i1 {is_ascii}, label %{ascii_b}, label %{slow_b}"
        )
        .unwrap();
        writeln!(self.out, "{ascii_b}:").unwrap();
        let len = self.emit_typed_string_byte_len(&s_v);
        writeln!(self.out, "  br label %{cont_b}").unwrap();
        writeln!(self.out, "{slow_b}:").unwrap();
        declare_rt(&mut self.runtime_refs, "gos_rt_str_len");
        let tmp = self.fresh();
        writeln!(self.out, "  {tmp} = call i64 @gos_rt_str_len(ptr {s_v})").unwrap();
        writeln!(self.out, "  br label %{cont_b}").unwrap();
        writeln!(self.out, "{cont_b}:").unwrap();
        let out = self.fresh();
        writeln!(
            self.out,
            "  {out} = phi i64 [ {len}, %{ascii_b} ], [ {tmp}, %{slow_b} ]"
        )
        .unwrap();
        self.store_inline_i64_result(&out, destination);
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Character count of a string operand whose text is known here.
    ///
    /// `String::len` counts Unicode scalars, so a literal folds to its
    /// character count rather than to the byte length its Rust `str` reports;
    /// the two differ for every literal outside ASCII.
    fn const_string_len(&self, arg: &Operand) -> Option<usize> {
        match arg {
            Operand::Const(gossamer_mir::ConstValue::Str(text)) => Some(text.chars().count()),
            Operand::Copy(place) if place.projection.is_empty() => {
                let decl = self.body.locals.get(place.local.0 as usize)?;
                if decl.mutable {
                    return None;
                }
                let mut found = None;
                for block in &self.body.blocks {
                    for stmt in &block.stmts {
                        let gossamer_mir::StatementKind::Assign {
                            place: assigned,
                            rvalue:
                                gossamer_mir::Rvalue::Use(Operand::Const(
                                    gossamer_mir::ConstValue::Str(text),
                                )),
                        } = &stmt.kind
                        else {
                            continue;
                        };
                        if assigned.local == place.local && assigned.projection.is_empty() {
                            if found.replace(text.chars().count()).is_some() {
                                return None;
                            }
                        }
                    }
                }
                found
            }
            _ => None,
        }
    }

    /// Inline fast path for
    /// `gos_rt_stream_write_byte_array(stream, arr, len)`.
    ///
    /// Pack the low byte of every i64 slot in `arr[..len]`
    /// directly into the stdout buffer. The stream-fd check is
    /// hoisted (via `!invariant.load !0`); when we know the fd
    /// is 1 we drop into a tight pack loop that LLVM unrolls
    /// when `len` is compile-time-known. For the fasta_block /
    /// fasta_mt programs `len` is `line_len + 1` â¤ 61 and the
    /// buffer is rarely full, so the slow path almost never
    /// fires.
    ///
    /// Layout summary:
    /// ```llvm
    ///   %fd = load i32, ptr %stream, !invariant.load !0
    ///   %is_stdout = icmp eq i32 %fd, 1
    ///   br i1 %is_stdout, label %fast_check, label %slow_call
    /// fast_check:
    ///   %len = load i64, ptr @GOS_RT_STDOUT_LEN
    ///   %sum = add i64 %len, %wlen
    ///   %fits = icmp ule i64 %sum, 8192
    ///   br i1 %fits, label %pack, label %slow_call
    /// pack:
    ///   %i = phi i64 [0, %fast_check], [%inext, %pack_body]
    ///   %done = icmp uge i64 %i, %wlen
    ///   br i1 %done, label %store_len, label %pack_body
    /// pack_body:
    ///   %src = getelementptr i64, ptr %arr, i64 %i
    ///   %v = load i64, ptr %src
    ///   %byte = trunc i64 %v to i8
    ///   %dst = getelementptr i8, ptr @GOS_RT_STDOUT_BYTES, i64 %newlen
    ///   store i8 %byte, ptr %dst
    ///   ; loop
    /// store_len:
    ///   store i64 %sum, ptr @GOS_RT_STDOUT_LEN
    ///   br label %end
    /// slow_call:
    ///   call void @gos_rt_stream_write_byte_array(...)
    ///   br label %end
    /// end:
    /// ```
    pub(crate) fn lower_stream_write_byte_array_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        for sym in [
            "gos_rt_stdout_acquire",
            "gos_rt_stdout_release",
            "gos_rt_stream_write_byte_array",
        ] {
            declare_rt(&mut self.runtime_refs, sym);
        }
        let stream_v = self.lower_operand(&args[0])?;
        let arr_v = self.lower_operand(&args[1])?;
        let len_v = self.lower_operand(&args[2])?;
        let suffix = self.next_ssa;
        self.next_ssa += 1;
        let fast_check = format!("wba_check_{suffix}");
        let pack_header = format!("wba_pack_{suffix}");
        let pack_body = format!("wba_body_{suffix}");
        let store_len_lbl = format!("wba_store_{suffix}");
        let slow = format!("wba_slow_{suffix}");
        let end = format!("wba_end_{suffix}");

        // fd check
        let fd = self.fresh();
        writeln!(
            self.out,
            "  {fd} = load i32, ptr {stream_v}, !invariant.load !0"
        )
        .unwrap();
        let is_stdout = self.fresh();
        writeln!(self.out, "  {is_stdout} = icmp eq i32 {fd}, 1").unwrap();
        writeln!(
            self.out,
            "  br i1 {is_stdout}, label %{fast_check}, label %{slow}"
        )
        .unwrap();

        // Capacity check. Acquire the stdout lock before the
        // `LEN` load so the read + `LEN` store on the inline
        // path are atomic with respect to other goroutines.
        // The lock is released along every exit (store_len,
        // and the slow-call branch).
        writeln!(self.out, "{fast_check}:").unwrap();
        writeln!(self.out, "  call void @gos_rt_stdout_acquire()").unwrap();
        let cur_len = self.fresh();
        writeln!(self.out, "  {cur_len} = load i64, ptr @GOS_RT_STDOUT_LEN").unwrap();
        let new_len = self.fresh();
        writeln!(self.out, "  {new_len} = add i64 {cur_len}, {len_v}").unwrap();
        let fits = self.fresh();
        writeln!(self.out, "  {fits} = icmp ule i64 {new_len}, 8192").unwrap();
        let fits_release = format!("wba_nofit_rel_{suffix}");
        writeln!(
            self.out,
            "  br i1 {fits}, label %{pack_header}, label %{fits_release}"
        )
        .unwrap();
        writeln!(self.out, "{fits_release}:").unwrap();
        writeln!(self.out, "  call void @gos_rt_stdout_release()").unwrap();
        writeln!(self.out, "  br label %{slow}").unwrap();

        // Pack loop header (PHI for the loop counter).
        writeln!(self.out, "{pack_header}:").unwrap();
        let i_phi = self.fresh();
        writeln!(
            self.out,
            "  {i_phi} = phi i64 [ 0, %{fast_check} ], [ %t_inext_{suffix}, %{pack_body} ]",
        )
        .unwrap();
        let done = self.fresh();
        writeln!(self.out, "  {done} = icmp uge i64 {i_phi}, {len_v}").unwrap();
        writeln!(
            self.out,
            "  br i1 {done}, label %{store_len_lbl}, label %{pack_body}"
        )
        .unwrap();

        // Pack body - read arr[i], pack into buf[cur_len + i].
        writeln!(self.out, "{pack_body}:").unwrap();
        let src = self.fresh();
        writeln!(
            self.out,
            "  {src} = getelementptr i64, ptr {arr_v}, i64 {i_phi}"
        )
        .unwrap();
        let raw = self.fresh();
        writeln!(self.out, "  {raw} = load i64, ptr {src}").unwrap();
        let byte = self.fresh();
        writeln!(self.out, "  {byte} = trunc i64 {raw} to i8").unwrap();
        let dst_off = self.fresh();
        writeln!(self.out, "  {dst_off} = add i64 {cur_len}, {i_phi}").unwrap();
        let dst = self.fresh();
        writeln!(
            self.out,
            "  {dst} = getelementptr i8, ptr @GOS_RT_STDOUT_BYTES, i64 {dst_off}"
        )
        .unwrap();
        writeln!(self.out, "  store i8 {byte}, ptr {dst}").unwrap();
        // increment counter - must use the exact name we
        // forward-referenced in the PHI above.
        writeln!(self.out, "  %t_inext_{suffix} = add i64 {i_phi}, 1").unwrap();
        writeln!(self.out, "  br label %{pack_header}").unwrap();

        // Store the new length once we've packed the whole block,
        // then release the stdout lock acquired in fast_check.
        writeln!(self.out, "{store_len_lbl}:").unwrap();
        writeln!(self.out, "  store i64 {new_len}, ptr @GOS_RT_STDOUT_LEN").unwrap();
        writeln!(self.out, "  call void @gos_rt_stdout_release()").unwrap();
        writeln!(self.out, "  br label %{end}").unwrap();

        // Slow path: fall back to the runtime helper.
        writeln!(self.out, "{slow}:").unwrap();
        writeln!(
            self.out,
            "  call void @gos_rt_stream_write_byte_array(ptr {stream_v}, ptr {arr_v}, i64 {len_v})"
        )
        .unwrap();
        writeln!(self.out, "  br label %{end}").unwrap();

        // End - destination is `()`; nothing to store.
        writeln!(self.out, "{end}:").unwrap();
        let _ = destination;
        match target {
            Some(t) => writeln!(self.out, "  br label %bb{}", t.as_u32()).unwrap(),
            None => writeln!(self.out, "  unreachable").unwrap(),
        }
        Ok(())
    }

    /// The address of a content key's slots. A reference to an aggregate
    /// already names the storage the descriptor reads - a snapshot slot, a
    /// borrowed struct - so the pointer travels as it is; anything else
    /// takes the element-slot form.
    pub(crate) fn skey_slot_address(&mut self, arg: &Operand) -> Result<String, BuildError> {
        if let Operand::Copy(p) = arg
            && let TyKind::Ref { inner, .. } = self.tcx.kind_of(self.place_leaf_ty(p))
            && is_aggregate(self.tcx, *inner)
            && slot_count(self.tcx, *inner).is_some()
        {
            let value = self.lower_operand(arg)?;
            let ty = self.operand_llvm_ty(arg);
            if ty == "ptr" {
                return Ok(value);
            }
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = inttoptr {ty} {value} to ptr").unwrap();
            return Ok(tmp);
        }
        self.elem_slot_address(arg)
    }

    /// The address of an element's slots: the operand's own storage when it
    /// is a multi-slot aggregate, else a fresh slot holding its word. The
    /// element store copies its own stride from whatever this addresses.
    pub(crate) fn elem_slot_address(&mut self, arg: &Operand) -> Result<String, BuildError> {
        if let Operand::Copy(p) = arg
            && is_aggregate(self.tcx, self.place_leaf_ty(p))
            && slot_count(self.tcx, self.place_leaf_ty(p)).is_some()
        {
            return Ok(if p.projection.is_empty() {
                local_slot(p.local)
            } else {
                self.lower_place_address(p)
            });
        }
        let val_v = self.lower_operand(arg)?;
        let val_ty = self.operand_llvm_ty(arg);
        let slot = self.entry_alloca("i128, align 16");
        let stored = if val_ty == "i128" {
            val_v
        } else {
            self.coerce_llvm_value(&val_v, &val_ty, "i128")
        };
        writeln!(self.out, "  store i128 {stored}, ptr {slot}").unwrap();
        Ok(slot)
    }

    /// A slot container's wide push: `gos_rt_deque_push_{back,front}_wide`
    /// copies the element store's own stride from the address handed over,
    /// so the element's slots are passed by address - its stack slot for an
    /// aggregate, a fresh 16-byte spill for a by-value `Option` / `Result`
    /// carrier.
    pub(crate) fn lower_container_push_wide(
        &mut self,
        sym: &str,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let recv_v = self.lower_operand(&args[0])?;
        let recv_ty = self.operand_llvm_ty(&args[0]);
        let recv_ptr = if recv_ty == "ptr" {
            recv_v
        } else {
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = inttoptr {recv_ty} {recv_v} to ptr").unwrap();
            tmp
        };
        let elem_addr = self.elem_slot_address(&args[1])?;
        declare_rt(&mut self.runtime_refs, sym);
        writeln!(
            self.out,
            "  call void @{sym}(ptr {recv_ptr}, ptr {elem_addr})"
        )
        .unwrap();
        if !is_unit(self.tcx, self.body.local_ty(destination.local)) {
            let dest_ty = render_ty(self.tcx, self.body.local_ty(destination.local));
            let dslot = local_slot(destination.local);
            let zero = match dest_ty.as_str() {
                "ptr" => "null".to_string(),
                "double" | "float" => "0.0".to_string(),
                _ => "0".to_string(),
            };
            writeln!(self.out, "  store {dest_ty} {zero}, ptr {dslot}").unwrap();
        }
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline `v.push(x)` for arbitrary element widths.
    /// `gos_rt_vec_push(*mut GosVec, *const u8)` reads the
    /// element through the second pointer; the i64 / ptr value
    /// needs to land on the stack first so we can hand the
    /// helper an `&value` instead of `value`. Mirrors the
    /// Cranelift backend's `lower_intrinsic_call` stack-slot
    /// dance for the same symbol.
    pub(crate) fn lower_vec_push_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let vec_v = self.lower_operand(&args[0])?;
        let vec_ty = self.operand_llvm_ty(&args[0]);
        let vec_ptr = if vec_ty == "ptr" {
            vec_v
        } else {
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = inttoptr {vec_ty} {vec_v} to ptr").unwrap();
            tmp
        };
        // Aggregate-element push (`xs.push((a, b))` where the
        // element type is a tuple/struct/array): the runtime
        // `gos_rt_vec_push(vec, ptr)` memcpys `vec.elem_bytes`
        // bytes from `ptr`. The scalar path below spills a
        // pointer-sized value into an `alloca i64` and would
        // copy `elem_bytes` from a too-small slot, clobbering
        // the vec's storage. Pass the operand's slot address
        // directly so the memcpy reads the full aggregate.
        // Only *inline* aggregates (structs / tuples / arrays with a
        // known multi-slot field layout) are pushed by address - the
        // runtime memcpys their `elem_bytes` of flat field data. A
        // handle-Adt (recursive enum, opaque sentinel; `slot_count ==
        // None`) holds an 8-byte heap pointer in its slot, like a
        // scalar, so it must go through the value path
        // (`gos_rt_vec_push_i64`) below. Taking its slot address and
        // memcpy'ing instead stored a stale pointer for a
        // function-returned enum (`xs.push(make_enum())`), decoding
        // the vec element as a garbage handle.
        if let Operand::Copy(p) = &args[1]
            && is_aggregate(self.tcx, self.place_leaf_ty(p))
            && slot_count(self.tcx, self.place_leaf_ty(p)).is_some()
        {
            let val_addr = if p.projection.is_empty() {
                local_slot(p.local)
            } else {
                self.lower_place_address(p)
            };
            declare_rt(&mut self.runtime_refs, "gos_rt_vec_push");
            writeln!(
                self.out,
                "  call void @gos_rt_vec_push(ptr {vec_ptr}, ptr {val_addr})"
            )
            .unwrap();
            if !is_unit(self.tcx, self.body.local_ty(destination.local)) {
                let dest_ty = render_ty(self.tcx, self.body.local_ty(destination.local));
                let dslot = local_slot(destination.local);
                let zero = match dest_ty.as_str() {
                    "ptr" => "null".to_string(),
                    "double" | "float" => "0.0".to_string(),
                    _ => "0".to_string(),
                };
                writeln!(self.out, "  store {dest_ty} {zero}, ptr {dslot}").unwrap();
            }
            emit_terminator_branch(&mut self.out, target);
            return Ok(());
        }
        let val_v = self.lower_operand(&args[1])?;
        let val_ty = self.operand_llvm_ty(&args[1]);
        // A 16-byte by-value `Result`/`Option` element pushes through the
        // dedicated `i128` helper (the vec's `elem_bytes` is 16) - coercing it
        // to i64 like the scalar path below would truncate the payload.
        if val_ty == "i128" {
            declare_rt(&mut self.runtime_refs, "gos_rt_vec_push_i128");
            let fat = self.fat_i128_call_arg(&val_v);
            writeln!(
                self.out,
                "  call void @gos_rt_vec_push_i128(ptr {vec_ptr}, {fat})"
            )
            .unwrap();
            if !is_unit(self.tcx, self.body.local_ty(destination.local)) {
                let dest_ty = render_ty(self.tcx, self.body.local_ty(destination.local));
                let dslot = local_slot(destination.local);
                let zero = match dest_ty.as_str() {
                    "ptr" => "null",
                    "double" | "float" => "0.0",
                    _ => "0",
                };
                writeln!(self.out, "  store {dest_ty} {zero}, ptr {dslot}").unwrap();
            }
            emit_terminator_branch(&mut self.out, target);
            return Ok(());
        }
        // Coerce the element to i64 and call gos_rt_vec_push_i64 directly.
        // This avoids emitting `alloca i64` inside the caller's basic block
        // (which would be a loop body for xs.push patterns), preventing stack
        // growth proportional to the loop iteration count.
        let val_i64 = match val_ty.as_str() {
            "i64" => val_v,
            "i1" => {
                let tmp = self.fresh();
                writeln!(self.out, "  {tmp} = zext i1 {val_v} to i64").unwrap();
                tmp
            }
            "i32" | "i16" | "i8" => {
                let tmp = self.fresh();
                writeln!(self.out, "  {tmp} = sext {val_ty} {val_v} to i64").unwrap();
                tmp
            }
            "double" => {
                let tmp = self.fresh();
                writeln!(self.out, "  {tmp} = bitcast double {val_v} to i64").unwrap();
                tmp
            }
            "float" => {
                let mid = self.fresh();
                writeln!(self.out, "  {mid} = fpext float {val_v} to double").unwrap();
                let tmp = self.fresh();
                writeln!(self.out, "  {tmp} = bitcast double {mid} to i64").unwrap();
                tmp
            }
            "ptr" => {
                let tmp = self.fresh();
                writeln!(self.out, "  {tmp} = ptrtoint ptr {val_v} to i64").unwrap();
                tmp
            }
            _ => val_v,
        };
        // Static element stride, derived from the operand type exactly as the
        // get/set paths do. A `Vec<i64/f64/ptr/Vec>` is word-stride, a
        // `Vec<bool>` is byte-stride; an erased element type stays unknown and
        // falls back to reading `elem_bytes` from the header at run time.
        let word_elem = self.vec_operand_has_word_elem(&args[0]);
        let byte_elem = !word_elem && self.vec_operand_has_byte_elem(&args[0]);
        // Inline no-grow fast path: when the vec is non-null, has spare
        // capacity, and the element stride is known, a push is one store
        // plus a len increment. Two stride cases get fast paths: 8-byte
        // (word, covering i64/f64/ptr/char/String/Vec) and 1-byte (bool
        // and byte buffers from fs::read / crypto::rand_bytes). The
        // runtime call remains the slow path for growth, null vecs, and
        // any other stride. RC retains happen at the push site via the
        // drop pass, so every path is semantically identical.
        declare_rt(&mut self.runtime_refs, "gos_rt_vec_push_i64");
        let s_id = self.next_ssa;
        self.next_ssa += 1;
        let (chk, chk2, chk3, word_fast, byte_fast, slow, cont) = (
            format!("vp_chk_{s_id}"),
            format!("vp_chk2_{s_id}"),
            format!("vp_chk3_{s_id}"),
            format!("vp_word_{s_id}"),
            format!("vp_byte_{s_id}"),
            format!("vp_slow_{s_id}"),
            format!("vp_cont_{s_id}"),
        );
        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {vec_ptr}, null").unwrap();
        writeln!(self.out, "  br i1 {isnull}, label %{slow}, label %{chk}").unwrap();
        writeln!(self.out, "{chk}:").unwrap();
        let len = self.fresh();
        writeln!(self.out, "  {len} = load i64, ptr {vec_ptr}{TBAA_HEADER}").unwrap();
        let cap_addr = self.fresh();
        writeln!(
            self.out,
            "  {cap_addr} = getelementptr i8, ptr {vec_ptr}, i64 8"
        )
        .unwrap();
        let cap = self.fresh();
        writeln!(self.out, "  {cap} = load i64, ptr {cap_addr}{TBAA_HEADER}").unwrap();
        let full = self.fresh();
        writeln!(self.out, "  {full} = icmp sge i64 {len}, {cap}").unwrap();
        // Full: must grow - delegate to the runtime. Otherwise reach the store
        // for this element stride. A statically-known stride branches straight
        // there, skipping the per-push `elem_bytes` header load and the two
        // runtime stride compares; an unknown stride keeps the dynamic dispatch.
        if word_elem {
            writeln!(
                self.out,
                "  br i1 {full}, label %{slow}, label %{word_fast}"
            )
            .unwrap();
        } else if byte_elem {
            writeln!(
                self.out,
                "  br i1 {full}, label %{slow}, label %{byte_fast}"
            )
            .unwrap();
        } else {
            let eb_addr = self.fresh();
            writeln!(
                self.out,
                "  {eb_addr} = getelementptr i8, ptr {vec_ptr}, i64 16"
            )
            .unwrap();
            let eb32 = self.fresh();
            writeln!(self.out, "  {eb32} = load i32, ptr {eb_addr}{TBAA_HEADER}").unwrap();
            writeln!(self.out, "  br i1 {full}, label %{slow}, label %{chk2}").unwrap();
            writeln!(self.out, "{chk2}:").unwrap();
            let is8 = self.fresh();
            writeln!(self.out, "  {is8} = icmp eq i32 {eb32}, 8").unwrap();
            writeln!(self.out, "  br i1 {is8}, label %{word_fast}, label %{chk3}").unwrap();
            writeln!(self.out, "{chk3}:").unwrap();
            let is1 = self.fresh();
            writeln!(self.out, "  {is1} = icmp eq i32 {eb32}, 1").unwrap();
            writeln!(self.out, "  br i1 {is1}, label %{byte_fast}, label %{slow}").unwrap();
        }
        // Word-stride (8-byte) fast path: store i64 directly. Emitted for a
        // statically word-strided vec and for the dynamic dispatch.
        if word_elem || !byte_elem {
            writeln!(self.out, "{word_fast}:").unwrap();
            let dptr_addr = self.fresh();
            writeln!(
                self.out,
                "  {dptr_addr} = getelementptr i8, ptr {vec_ptr}, i64 24"
            )
            .unwrap();
            let dptr = self.fresh();
            writeln!(
                self.out,
                "  {dptr} = load ptr, ptr {dptr_addr}{TBAA_HEADER}"
            )
            .unwrap();
            let off = self.fresh();
            writeln!(self.out, "  {off} = mul i64 {len}, 8").unwrap();
            let ea = self.fresh();
            writeln!(self.out, "  {ea} = getelementptr i8, ptr {dptr}, i64 {off}").unwrap();
            writeln!(self.out, "  store i64 {val_i64}, ptr {ea}{TBAA_DATA}").unwrap();
            let len1 = self.fresh();
            writeln!(self.out, "  {len1} = add i64 {len}, 1").unwrap();
            writeln!(self.out, "  store i64 {len1}, ptr {vec_ptr}{TBAA_HEADER}").unwrap();
            writeln!(self.out, "  br label %{cont}").unwrap();
        }
        // Byte-stride (1-byte) fast path for bool and byte-buffer elements.
        // Element address is data_ptr + len (stride == 1, no multiply). Emitted
        // for a statically byte-strided vec and for the dynamic dispatch.
        if byte_elem || !word_elem {
            writeln!(self.out, "{byte_fast}:").unwrap();
            let dptr_addr2 = self.fresh();
            writeln!(
                self.out,
                "  {dptr_addr2} = getelementptr i8, ptr {vec_ptr}, i64 24"
            )
            .unwrap();
            let dptr2 = self.fresh();
            writeln!(
                self.out,
                "  {dptr2} = load ptr, ptr {dptr_addr2}{TBAA_HEADER}"
            )
            .unwrap();
            let ea2 = self.fresh();
            writeln!(
                self.out,
                "  {ea2} = getelementptr i8, ptr {dptr2}, i64 {len}"
            )
            .unwrap();
            let val8 = self.fresh();
            writeln!(self.out, "  {val8} = trunc i64 {val_i64} to i8").unwrap();
            writeln!(self.out, "  store i8 {val8}, ptr {ea2}{TBAA_DATA}").unwrap();
            let len1b = self.fresh();
            writeln!(self.out, "  {len1b} = add i64 {len}, 1").unwrap();
            writeln!(self.out, "  store i64 {len1b}, ptr {vec_ptr}{TBAA_HEADER}").unwrap();
            writeln!(self.out, "  br label %{cont}").unwrap();
        }
        writeln!(self.out, "{slow}:").unwrap();
        writeln!(
            self.out,
            "  call void @gos_rt_vec_push_i64(ptr {vec_ptr}, i64 {val_i64})"
        )
        .unwrap();
        writeln!(self.out, "  br label %{cont}").unwrap();
        writeln!(self.out, "{cont}:").unwrap();
        if !is_unit(self.tcx, self.body.local_ty(destination.local)) {
            let dest_ty = render_ty(self.tcx, self.body.local_ty(destination.local));
            let dslot = local_slot(destination.local);
            let zero = match dest_ty.as_str() {
                "ptr" => "null".to_string(),
                "double" | "float" => "0.0".to_string(),
                _ => "0".to_string(),
            };
            writeln!(self.out, "  store {dest_ty} {zero}, ptr {dslot}").unwrap();
        }
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline fast path for `gos_rt_vec_pop_opt(v) -> Option<T>` when `T` is
    /// represented as one word or one byte. Null / empty returns `None`
    /// (`disc = 1`); otherwise the vector length is decremented and the last
    /// element is packed into the high word of the by-value `i128` Option.
    pub(crate) fn lower_vec_pop_opt_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let word_elem = self.vec_operand_has_word_elem(&args[0]);
        let byte_elem = !word_elem && self.vec_operand_has_byte_elem(&args[0]);
        if !word_elem && !byte_elem {
            return Err(BuildError::InternalLoweringBug(
                "inline Vec::pop requires statically word- or byte-sized elements",
            ));
        }
        let vec_ptr = self.vec_operand_ptr(&args[0])?;
        let s = self.next_ssa;
        self.next_ssa += 1;
        let (check, some, none, cont) = (
            format!("vpop_check_{s}"),
            format!("vpop_some_{s}"),
            format!("vpop_none_{s}"),
            format!("vpop_cont_{s}"),
        );

        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {vec_ptr}, null").unwrap();
        writeln!(self.out, "  br i1 {isnull}, label %{none}, label %{check}").unwrap();

        writeln!(self.out, "{check}:").unwrap();
        let len = self.fresh();
        writeln!(self.out, "  {len} = load i64, ptr {vec_ptr}{TBAA_HEADER}").unwrap();
        let empty = self.fresh();
        writeln!(self.out, "  {empty} = icmp sle i64 {len}, 0").unwrap();
        writeln!(self.out, "  br i1 {empty}, label %{none}, label %{some}").unwrap();

        writeln!(self.out, "{some}:").unwrap();
        let len1 = self.fresh();
        writeln!(self.out, "  {len1} = add i64 {len}, -1").unwrap();
        writeln!(self.out, "  store i64 {len1}, ptr {vec_ptr}{TBAA_HEADER}").unwrap();
        let payload = if word_elem {
            let off = self.fresh();
            writeln!(self.out, "  {off} = mul i64 {len1}, 8").unwrap();
            let ea = self.vec_elem_addr(&vec_ptr, &off);
            let loaded = self.fresh();
            writeln!(self.out, "  {loaded} = load i64, ptr {ea}{TBAA_DATA}").unwrap();
            loaded
        } else {
            let ea = self.vec_elem_addr(&vec_ptr, &len1);
            let b8 = self.fresh();
            writeln!(self.out, "  {b8} = load i8, ptr {ea}{TBAA_DATA}").unwrap();
            let b64 = self.fresh();
            writeln!(self.out, "  {b64} = zext i8 {b8} to i64").unwrap();
            b64
        };
        let payload128 = self.fresh();
        writeln!(self.out, "  {payload128} = zext i64 {payload} to i128").unwrap();
        let packed_some = self.fresh();
        writeln!(self.out, "  {packed_some} = shl i128 {payload128}, 64").unwrap();
        writeln!(self.out, "  br label %{cont}").unwrap();

        writeln!(self.out, "{none}:").unwrap();
        writeln!(self.out, "  br label %{cont}").unwrap();

        writeln!(self.out, "{cont}:").unwrap();
        let packed = self.fresh();
        writeln!(
            self.out,
            "  {packed} = phi i128 [ {packed_some}, %{some} ], [ 1, %{none} ]"
        )
        .unwrap();
        let slot = local_slot(destination.local);
        writeln!(self.out, "  store i128 {packed}, ptr {slot}, align 8").unwrap();
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    /// Inline fast path for `gos_rt_str_append_bytes(acc, ptr, len)`.
    /// Mirrors the runtime shim's in-place branch: when `acc` is a
    /// sole-owner growable builder (`STR_BUILDER_TAG`) with spare
    /// capacity, append `len` bytes via memcpy + length bump with no
    /// FFI call. Every other shape (null, non-builder, region builder,
    /// shared, capacity-exhausted) branches to the runtime shim, which
    /// owns those paths. Header layout matches `c_abi::string`:
    /// `rc@acc-13`, `cap@acc-9`, `len@acc-5`, `tag@acc-1`.
    pub(crate) fn lower_str_append_bytes_inline(
        &mut self,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let acc = self.lower_operand(&args[0])?;
        let piece = self.lower_operand(&args[1])?;
        let len_raw = self.lower_operand(&args[2])?;
        let len = self.widen_to_i64(&args[2], &len_raw);
        declare_rt(&mut self.runtime_refs, "gos_rt_str_append_bytes");

        let tagchk = self.fresh_label("ab_tag");
        let hdr = self.fresh_label("ab_hdr");
        let fast = self.fresh_label("ab_fast");
        let slow = self.fresh_label("ab_slow");
        let done = self.fresh_label("ab_done");

        let isnull = self.fresh();
        writeln!(self.out, "  {isnull} = icmp eq ptr {acc}, null").unwrap();
        writeln!(self.out, "  br i1 {isnull}, label %{slow}, label %{tagchk}").unwrap();

        writeln!(self.out, "{tagchk}:").unwrap();
        let tagp = self.fresh();
        writeln!(self.out, "  {tagp} = getelementptr i8, ptr {acc}, i64 -1").unwrap();
        let tag = self.fresh();
        writeln!(self.out, "  {tag} = load i8, ptr {tagp}{TBAA_HEADER}").unwrap();
        let isbuilder = self.fresh();
        // STR_BUILDER_TAG = 0xAB.
        writeln!(self.out, "  {isbuilder} = icmp eq i8 {tag}, -85").unwrap();
        writeln!(self.out, "  br i1 {isbuilder}, label %{hdr}, label %{slow}").unwrap();

        writeln!(self.out, "{hdr}:").unwrap();
        let rcp = self.fresh();
        writeln!(self.out, "  {rcp} = getelementptr i8, ptr {acc}, i64 -13").unwrap();
        let rc = self.fresh();
        writeln!(self.out, "  {rc} = load i32, ptr {rcp}{TBAA_HEADER}").unwrap();
        let capp = self.fresh();
        writeln!(self.out, "  {capp} = getelementptr i8, ptr {acc}, i64 -9").unwrap();
        let cap = self.fresh();
        writeln!(self.out, "  {cap} = load i32, ptr {capp}{TBAA_HEADER}").unwrap();
        let lenp = self.fresh();
        writeln!(self.out, "  {lenp} = getelementptr i8, ptr {acc}, i64 -5").unwrap();
        let curlen = self.fresh();
        writeln!(self.out, "  {curlen} = load i32, ptr {lenp}{TBAA_HEADER}").unwrap();
        let lentr = self.fresh();
        writeln!(self.out, "  {lentr} = trunc i64 {len} to i32").unwrap();
        let newlen = self.fresh();
        writeln!(self.out, "  {newlen} = add i32 {curlen}, {lentr}").unwrap();
        let fits = self.fresh();
        writeln!(self.out, "  {fits} = icmp ule i32 {newlen}, {cap}").unwrap();
        let sole = self.fresh();
        writeln!(self.out, "  {sole} = icmp eq i32 {rc}, 1").unwrap();
        let okc = self.fresh();
        writeln!(self.out, "  {okc} = and i1 {fits}, {sole}").unwrap();
        writeln!(self.out, "  br i1 {okc}, label %{fast}, label %{slow}").unwrap();

        writeln!(self.out, "{fast}:").unwrap();
        let curlen64 = self.fresh();
        writeln!(self.out, "  {curlen64} = zext i32 {curlen} to i64").unwrap();
        let dst = self.fresh();
        writeln!(
            self.out,
            "  {dst} = getelementptr i8, ptr {acc}, i64 {curlen64}"
        )
        .unwrap();
        writeln!(
            self.out,
            "  call void @llvm.memcpy.p0.p0.i64(ptr {dst}, ptr {piece}, i64 {len}, i1 false)"
        )
        .unwrap();
        let nulp = self.fresh();
        writeln!(
            self.out,
            "  {nulp} = getelementptr i8, ptr {dst}, i64 {len}"
        )
        .unwrap();
        writeln!(self.out, "  store i8 0, ptr {nulp}{TBAA_DATA}").unwrap();
        writeln!(self.out, "  store i32 {newlen}, ptr {lenp}{TBAA_HEADER}").unwrap();
        writeln!(self.out, "  br label %{done}").unwrap();

        writeln!(self.out, "{slow}:").unwrap();
        let r = self.fresh();
        writeln!(
            self.out,
            "  {r} = call ptr @gos_rt_str_append_bytes(ptr {acc}, ptr {piece}, i64 {len})"
        )
        .unwrap();
        writeln!(self.out, "  br label %{done}").unwrap();

        writeln!(self.out, "{done}:").unwrap();
        let res = self.fresh();
        writeln!(
            self.out,
            "  {res} = phi ptr [ {acc}, %{fast} ], [ {r}, %{slow} ]"
        )
        .unwrap();
        if !is_unit(self.tcx, self.body.local_ty(destination.local)) {
            let slot = local_slot(destination.local);
            writeln!(self.out, "  store ptr {res}, ptr {slot}").unwrap();
        }
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }
}
