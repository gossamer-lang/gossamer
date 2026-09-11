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
    BasicBlock, BinOp, Body, ConstValue, Local, Operand, Place, Projection, RawIntrinsic, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};

impl<'a> Lowerer<'a> {
    pub(crate) fn lower_rvalue(
        &mut self,
        rvalue: &Rvalue,
        dest_local: Local,
    ) -> Result<String, BuildError> {
        match rvalue {
            Rvalue::Use(op) => self.lower_operand(op),
            Rvalue::UnaryOp { op, operand } => self.lower_unary(*op, operand, dest_local),
            Rvalue::BinaryOp { op, lhs, rhs } => self.lower_binary(*op, lhs, rhs, dest_local),
            Rvalue::Cast { operand, target } => self.lower_cast(operand, *target, dest_local),
            Rvalue::CallIntrinsic { name, args } => {
                self.lower_call_intrinsic(name, args, dest_local)
            }
            Rvalue::Ref { place, mutable } => {
                // `&place` returns the address of the projection walk.
                // A bare reference local already stores the referent pointer,
                // so forward that value instead of taking the address of the
                // stack slot that happens to hold the reference.
                let leaf_is_heap_value = matches!(
                    self.tcx.kind(self.place_leaf_ty(place)),
                    Some(
                        TyKind::String | TyKind::Slice(_) | TyKind::Vec(_) | TyKind::HashMap { .. }
                    )
                );
                let bare_mut_string = *mutable
                    && place.projection.is_empty()
                    && matches!(
                        self.tcx.kind(self.body.local_ty(place.local)),
                        Some(TyKind::String)
                    );
                // A local whose slot holds a pointer to its value - which is
                // what the place-read walk auto-derefs - names the referent
                // through that pointer, not through the slot holding it.
                let pointer_local = place.projection.is_empty()
                    && Self::is_pointer_local_ty(self.tcx, self.body.local_ty(place.local));
                // A `&mut` payload enum is the receiver a callee rebinds whole
                // (`*self = Variant(..)`), so it names the caller's slot and
                // the store lands where the caller reads its binding back.
                let bare_mut_enum = *mutable
                    && place.projection.is_empty()
                    && self.tcx.is_payload_enum(self.body.local_ty(place.local));
                if !bare_mut_string
                    && !bare_mut_enum
                    && (leaf_is_heap_value
                        || pointer_local
                        || place.projection.is_empty()
                            && self.tcx.is_payload_enum(self.body.local_ty(place.local)))
                {
                    Ok(self.lower_place_read(place))
                } else if place.projection.is_empty() {
                    Ok(local_slot(place.local))
                } else {
                    Ok(self.lower_place_address(place))
                }
            }
            Rvalue::Len(place) => {
                // `Rvalue::Len` reports the length of a
                // runtime-managed sequence. Stack-allocated
                // arrays have static lengths the compiler
                // folds from the type; heap-backed
                // `Vec`/`Slice`/`String` values go through
                // `gos_rt_len`.
                let ty = self.place_leaf_ty(place);
                if let Some(TyKind::Array { len, .. }) = self.tcx.kind(ty) {
                    return Ok(format!("{}", len.to_usize()));
                }
                // A bare local of Vec/Slice type with a non-String
                // element reads its length straight from the leading
                // i64 of the GosVec header (NULL -> 0), which removes
                // a per-iteration FFI call from every
                // `while i < xs.len()` loop. `Vec<String>` stays on
                // `gos_rt_len`: `env::args()` hands out a sentinel
                // pointer whose length lives in `ARGS_LEN`, not at
                // `*p`. Projected places keep the call as well.
                if place.projection.is_empty() {
                    let mut peeled = ty;
                    while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
                        peeled = *inner;
                    }
                    let elem = match self.tcx.kind(peeled) {
                        Some(TyKind::Vec(e) | TyKind::Slice(e)) => Some(*e),
                        _ => None,
                    };
                    let inlineable =
                        elem.is_some_and(|e| !matches!(self.tcx.kind(e), Some(TyKind::String)));
                    if inlineable {
                        let ptr = self.fresh();
                        writeln!(
                            self.out,
                            "  {ptr} = load ptr, ptr {slot}",
                            slot = local_slot(place.local),
                        )
                        .unwrap();
                        let s = self.next_ssa;
                        self.next_ssa += 1;
                        let (lz, ll, lc) = (
                            format!("len_z_{s}"),
                            format!("len_l_{s}"),
                            format!("len_c_{s}"),
                        );
                        let isnull = self.fresh();
                        writeln!(self.out, "  {isnull} = icmp eq ptr {ptr}, null").unwrap();
                        writeln!(self.out, "  br i1 {isnull}, label %{lz}, label %{ll}").unwrap();
                        writeln!(self.out, "{ll}:").unwrap();
                        let n = self.fresh();
                        writeln!(self.out, "  {n} = load i64, ptr {ptr}").unwrap();
                        writeln!(self.out, "  br label %{lc}").unwrap();
                        writeln!(self.out, "{lz}:").unwrap();
                        writeln!(self.out, "  br label %{lc}").unwrap();
                        writeln!(self.out, "{lc}:").unwrap();
                        let res = self.fresh();
                        writeln!(self.out, "  {res} = phi i64 [ {n}, %{ll} ], [ 0, %{lz} ]")
                            .unwrap();
                        return Ok(res);
                    }
                }
                // For heap-backed shapes the operand is the
                // opaque pointer; call the runtime.
                declare_rt(&mut self.runtime_refs, "gos_rt_len");
                let ptr = if place.projection.is_empty() {
                    let tmp = self.fresh();
                    writeln!(
                        self.out,
                        "  {tmp} = load ptr, ptr {slot}",
                        slot = local_slot(place.local),
                    )
                    .unwrap();
                    tmp
                } else {
                    self.lower_place_address(place)
                };
                let tmp = self.fresh();
                writeln!(self.out, "  {tmp} = call i64 @gos_rt_len(ptr {ptr})").unwrap();
                Ok(tmp)
            }
            Rvalue::Aggregate { .. } | Rvalue::Repeat { .. } => {
                // These rvalues are only legal on the right-hand
                // side of an `Assign` statement, and
                // `lower_assign` routes them directly to the
                // dedicated in-place aggregate store. Reaching
                // them here means the MIR used them as an
                // operand, which the MVP doesn't cover.
                Err(BuildError::InternalLoweringBug(
                    "Aggregate / Repeat as non-assignment rvalue",
                ))
            }
            Rvalue::StaticLoad(sref) => {
                let llvm_ty = render_ty(self.tcx, sref.ty);
                self.register_static_global(sref, &llvm_ty);
                let tmp = self.fresh();
                writeln!(
                    self.out,
                    "  {tmp} = load {llvm_ty}, ptr @{sym}",
                    sym = sref.symbol,
                )
                .unwrap();
                Ok(tmp)
            }
        }
    }

    /// Registers the backing module global for a `static mut`. The
    /// definition uses `linkonce_odr` linkage so the same global emitted
    /// from every object that references the static coalesces to one
    /// shared cell at link time. `runtime_refs` is a `BTreeSet`, so the
    /// duplicate definitions a single module emits dedup to one line.
    pub(crate) fn register_static_global(&mut self, sref: &gossamer_mir::StaticRef, llvm_ty: &str) {
        // A pointer-shaped cell starts empty; the entry's prologue writes the
        // storage the declaration names before anything reads it.
        let init = if llvm_ty == "ptr" {
            "null".to_string()
        } else {
            render_const(&sref.init)
        };
        self.runtime_refs.insert(format!(
            "@{sym} = linkonce_odr global {llvm_ty} {init}",
            sym = sref.symbol,
        ));
    }

    /// MIR's `CallIntrinsic` is used for stdlib math and
    /// conversion calls the lowerer wants inline (no separate
    /// Call terminator). The MVP covers the single-argument
    /// f64 functions the nbody-shape programs call through
    /// `std::math::sqrt` etc. - each maps to an LLVM intrinsic
    /// (`llvm.sqrt.f64`, `llvm.sin.f64`, …) which `llc -O3`
    /// lowers to the matching SSE/AVX instruction.
    /// Generic `gos_rt_*` runtime-call intrinsic in `Rvalue`
    /// position. Mirrors the Cranelift backend's behaviour:
    /// emit a typed `call` against the named runtime symbol,
    /// returning the result as the destination local's value.
    /// Inlines the pure Result/Option i128 carrier bit-ops
    /// (`gos_rt_result_new` / `_disc` / `_payload` / `_payload_f64`). Returns
    /// `Some(ssa)` (coerced to `dest_ty`) when `name` is one of them, else
    /// `None` so the caller emits the ordinary runtime call. The aggregate
    /// payload extractor `gos_rt_result_payload_i128` and the constructor
    /// `gos_rt_result_new_f64` are left on the call path (the former dereferences
    /// a heap pointer; the latter is rare).
    fn try_inline_result_carrier(
        &mut self,
        name: &str,
        args: &[Operand],
        dest_ty: &str,
    ) -> Result<Option<String>, BuildError> {
        // Coerce an SSA value to `dest_ty` (a no-op when it already matches,
        // or when the result is discarded into a `void` / unit destination -
        // coercing to `void` would emit an invalid `bitcast .. to void`).
        let coerce_dest = |this: &mut Self, val: String, natural: &str| -> String {
            if dest_ty == natural || dest_ty == "void" || dest_ty.is_empty() {
                val
            } else {
                this.coerce_llvm_value(&val, natural, dest_ty)
            }
        };
        match name {
            "gos_rt_result_new" | "gos_rt_result_new_owned" if args.len() == 2 => {
                // A Unit / `void` operand (`Ok(())`, `Some(())`) carries no
                // bits: pack it as 0, matching the runtime-call arg path's
                // void guard. Coercing a `void` value would emit an invalid
                // `bitcast void to i64`.
                let to_i64_word = |this: &mut Self, arg: &Operand| -> Result<String, BuildError> {
                    let ty = this.operand_llvm_ty(arg);
                    if ty == "void" || ty.is_empty() {
                        return Ok("0".to_string());
                    }
                    let raw = this.lower_operand(arg)?;
                    Ok(if ty == "i64" {
                        raw
                    } else {
                        this.coerce_llvm_value(&raw, &ty, "i64")
                    })
                };
                let disc = to_i64_word(self, &args[0])?;
                // Aggregate payloads (by-value struct / tuple / inline enum)
                // must be heap-copied so the packed pointer outlives the
                // constructing frame - identical to the runtime-call arg path.
                // The owning spelling says the payload's own walk over its
                // children was removed, so the box takes the share those words
                // already carried instead of minting one.
                let owned = name == "gos_rt_result_new_owned";
                let payload = if let Some(hv) = self
                    .maybe_heap_copy_value_enum(&args[1])
                    .or_else(|| self.maybe_heap_copy_aggregate_moved(&args[1], owned))
                {
                    hv
                } else {
                    to_i64_word(self, &args[1])?
                };
                let disc128 = self.fresh();
                writeln!(self.out, "  {disc128} = zext i64 {disc} to i128").unwrap();
                let pay128 = self.fresh();
                writeln!(self.out, "  {pay128} = zext i64 {payload} to i128").unwrap();
                let payhi = self.fresh();
                writeln!(self.out, "  {payhi} = shl i128 {pay128}, 64").unwrap();
                let packed = self.fresh();
                writeln!(self.out, "  {packed} = or i128 {payhi}, {disc128}").unwrap();
                Ok(Some(coerce_dest(self, packed, "i128")))
            }
            // `gos_rt_weak_opt_payload` is the payload extract of a
            // `w.upgrade()` result pinned in the frame's shadow local:
            // identical bit-op to `gos_rt_result_payload` (the None payload
            // word is 0, so the extract already reads as null for None); the
            // distinct MIR name only marks the destination as owned in the
            // drop pass.
            "gos_rt_result_disc"
            | "gos_rt_result_payload"
            | "gos_result_payload_owned"
            | "gos_rt_weak_opt_payload"
            | "gos_rt_result_payload_f64"
                if args.len() == 1 =>
            {
                let r_ty = self.operand_llvm_ty(&args[0]);
                let r_raw = self.lower_operand(&args[0])?;
                let r = if r_ty == "i128" {
                    r_raw
                } else {
                    self.coerce_llvm_value(&r_raw, &r_ty, "i128")
                };
                if name == "gos_rt_result_disc" {
                    // Low 64 bits.
                    let d = self.fresh();
                    writeln!(self.out, "  {d} = trunc i128 {r} to i64").unwrap();
                    return Ok(Some(coerce_dest(self, d, "i64")));
                }
                // High 64 bits.
                let hi = self.fresh();
                writeln!(self.out, "  {hi} = lshr i128 {r}, 64").unwrap();
                let p64 = self.fresh();
                writeln!(self.out, "  {p64} = trunc i128 {hi} to i64").unwrap();
                if name == "gos_rt_result_payload_f64" {
                    // Reinterpret the payload bits as an f64.
                    let f = self.fresh();
                    writeln!(self.out, "  {f} = bitcast i64 {p64} to double").unwrap();
                    return Ok(Some(coerce_dest(self, f, "double")));
                }
                Ok(Some(coerce_dest(self, p64, "i64")))
            }
            // `.unwrap()` is the discriminant test and the payload word, with
            // a panic on the empty arm. The shim reaching it costs an FFI
            // call and a two-register i128 argument for that, which a walk
            // over a linked structure pays once per step.
            "gos_rt_option_unwrap" | "gos_rt_result_unwrap" if args.len() == 1 => {
                let r_ty = self.operand_llvm_ty(&args[0]);
                let r_raw = self.lower_operand(&args[0])?;
                let r = if r_ty == "i128" {
                    r_raw
                } else {
                    self.coerce_llvm_value(&r_raw, &r_ty, "i128")
                };
                let message = if name == "gos_rt_option_unwrap" {
                    "called `Option::unwrap()` on a `None` value"
                } else {
                    "called `Result::unwrap()` on an `Err` value"
                };
                let p64 = self.emit_carrier_unwrap(&r, message);
                Ok(Some(coerce_dest(self, p64, "i64")))
            }
            _ => Ok(None),
        }
    }

    /// Emits the payload of a carrier that must hold one, panicking with
    /// `message` on the empty arm exactly as the shim does.
    fn emit_carrier_unwrap(&mut self, carrier: &str, message: &str) -> String {
        declare_rt(&mut self.runtime_refs, "gos_rt_panic");
        let s = self.next_ssa;
        self.next_ssa += 1;
        let (empty, held) = (format!("uw_empty_{s}"), format!("uw_held_{s}"));
        let disc = self.fresh();
        writeln!(self.out, "  {disc} = trunc i128 {carrier} to i64").unwrap();
        let bad = self.fresh();
        writeln!(self.out, "  {bad} = icmp ne i64 {disc}, 0").unwrap();
        writeln!(self.out, "  br i1 {bad}, label %{empty}, label %{held}").unwrap();
        let cold_start = self.out.len();
        writeln!(self.out, "{empty}:").unwrap();
        self.emit_panic_site_line();
        let (label, _) = self.strings.borrow_mut().intern(message);
        writeln!(self.out, "  call void @gos_rt_panic(ptr {label})").unwrap();
        writeln!(self.out, "  unreachable").unwrap();
        self.mark_cold(cold_start);
        writeln!(self.out, "{held}:").unwrap();
        let hi = self.fresh();
        writeln!(self.out, "  {hi} = lshr i128 {carrier}, 64").unwrap();
        let p64 = self.fresh();
        writeln!(self.out, "  {p64} = trunc i128 {hi} to i64").unwrap();
        p64
    }

    pub(crate) fn lower_runtime_call_intrinsic(
        &mut self,
        name: &str,
        args: &[Operand],
        dest_local: Local,
    ) -> Result<String, BuildError> {
        let name = if name == "gos_rt_bytearr_slice_result" {
            "gos_rt_packed_bytearr_slice_result"
        } else {
            name
        };
        let dest_ty = render_ty(self.tcx, self.body.local_ty(dest_local));
        // Inline the Result/Option i128 carrier bit-ops. The runtime encodes a
        // `Result<T, E>` / `Option<T>` as a 2-word by-value i128 with the
        // discriminant in the low 64 bits and the payload in the high 64 bits
        // (`c_abi/vec.rs::pack_result`). The `new`/`disc`/`payload` shims are
        // pure register pack/unpack, so `?`-heavy code stops paying an FFI call
        // per propagation. Inlining also sidesteps the Win64 i128 by-pointer /
        // `<16 x i8>` calling convention entirely - no ABI boundary is crossed.
        if let Some(inlined) = self.try_inline_result_carrier(name, args, &dest_ty)? {
            return Ok(inlined);
        }
        // Pull canonical parameter types from the runtime registry
        // when the symbol is registered. This sidesteps two related
        // miscompiles: (a) a Unit / `void` operand becoming a `void`
        // call-site argument (LLVM rejects this), and (b) two
        // distinct calls for the same symbol producing two divergent
        // `declare` lines (LLVM rejects redefinition).
        let registry_param_llvm: Vec<String> = gossamer_abi::lookup(name)
            .map(|e| {
                e.sig
                    .params
                    .iter()
                    .map(|p| p.llvm_ir().to_string())
                    .collect()
            })
            .unwrap_or_default();
        // `gos_rt_chan_send` / `gos_rt_chan_try_send` expect their
        // second argument to be `*const u8` - a pointer to a
        // memory slot holding the value bytes (the runtime
        // memcpys `chan.elem_bytes` from there). A naive
        // `inttoptr i64 N to ptr` produces a wild pointer that
        // segfaults inside `push_back`. Stack-spill the value
        // and pass the slot address, matching the Cranelift
        // backend.
        let chan_send_spill = matches!(name, "gos_rt_chan_send" | "gos_rt_chan_try_send");
        // `gos_rt_result_new(disc, payload)` stores `payload` as an
        // i64. For aggregate payloads (struct literals, tuples,
        // arrays built on this function's stack), the operand's
        // value in the flat-slot ABI is its stack alloca address -
        // which becomes dangling the moment the caller's frame
        // pops. Heap-copy the aggregate before passing so the
        // pointer outlives the function return.
        let result_new_heap_copy = matches!(name, "gos_rt_result_new" | "gos_rt_result_new_owned");
        // HashMap insert with struct value - same rationale as
        // `gos_rt_result_new`: the value lives on the inserting
        // frame's stack and goes dangling once that frame returns.
        let map_insert_heap_copy = matches!(
            name,
            "gos_rt_map_insert_i64_i64"
                | "gos_rt_map_insert_str_i64"
                | "gos_rt_map_insert_typed_str_i64"
                | "gos_rt_map_insert_i64_i64_opt"
                | "gos_rt_map_insert_str_i64_opt"
                | "gos_rt_map_insert_typed_str_i64_opt"
        );
        // The same rationale for the content-keyed inserts, whose value
        // is one argument later because the key's layout descriptor
        // travels between them. A stored value that still points at the
        // inserting frame's slot is re-written by the next iteration, so
        // every entry reads as the last one inserted.
        let skey_insert_heap_copy = matches!(
            name,
            "gos_rt_map_insert_skey"
                | "gos_rt_map_insert_skey_opt"
                // `or_insert` stores its default when the slot is
                // absent, so that default is a stored value too.
                | "gos_rt_map_or_insert_skey"
        );
        // `gos_rt_enum_box_aggr(size, meta, src)` and its weak-cell sibling
        // `gos_rt_rc_weak_cell`: the second argument names a module-global
        // `RC_KIND_STRUCT` meta blob (the cell's child layout), and the third
        // is the source aggregate's stack slot ADDRESS - not the value its
        // first word holds.
        let enum_box_aggr = matches!(name, "gos_rt_enum_box_aggr" | "gos_rt_rc_weak_cell");
        // An ordered container's push takes the address of the element's
        // slots: the store copies its own stride from it, and the sift
        // compares through the descriptor that follows.
        let heap_push_by_address = matches!(
            name,
            "gos_rt_bheap_max_push_desc" | "gos_rt_bheap_min_push_desc"
        );
        // A content-keyed map or set reads the key's slots through the
        // descriptor that travels with the call, so the key is passed by
        // address - an aggregate's own storage, or a fresh slot holding a
        // two-word `Option` carrier.
        let skey_by_address = matches!(
            name,
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

        let mut arg_text = String::new();
        // A map insert whose value is an aggregate copies it into a
        // reference-counted blob at the call site, at strong 1 - the frame's
        // own share, which the MIR passes cannot see to pair a release for.
        // Recorded here so it is given back right after the call stores it.
        let mut minted_blob: Option<String> = None;
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                arg_text.push_str(", ");
            }
            if enum_box_aggr && i == 1 {
                let meta = match arg {
                    Operand::Const(ConstValue::Str(sym)) if !sym.is_empty() => {
                        format!("@\"{sym}\"")
                    }
                    _ => "null".to_string(),
                };
                let _ = write!(arg_text, "ptr {meta}");
                continue;
            }
            if enum_box_aggr
                && i == 2
                && let Operand::Copy(place) = arg
                && place.projection.is_empty()
            {
                let slot = local_slot(place.local);
                let _ = write!(arg_text, "ptr {slot}");
                continue;
            }
            if (heap_push_by_address || skey_by_address) && i == 1 {
                let addr = if skey_by_address {
                    self.skey_slot_address(arg)?
                } else {
                    self.elem_slot_address(arg)?
                };
                let _ = write!(arg_text, "ptr {addr}");
                continue;
            }
            let a_ty = self.operand_llvm_ty(arg);
            let a_v = self.lower_operand(arg)?;
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
                minted_blob = Some(heap_v);
                continue;
            }
            if skey_insert_heap_copy
                && i == 3
                && let Some(heap_v) = self
                    .maybe_heap_copy_value_enum(arg)
                    .or_else(|| self.maybe_heap_copy_aggregate_for_map(arg))
            {
                let _ = write!(arg_text, "i64 {heap_v}");
                minted_blob = Some(heap_v);
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
                // Spill the value into a fresh 8-byte stack slot
                // (the channel element width is at most one
                // word in the current runtime ABI) and pass the
                // slot address. The `a_ty` could be ptr / i64 /
                // double / i32 / i1; widen scalars to i64 so the
                // slot's 8 bytes are fully initialised.
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
            if let Some(want_ty) = registry_param_llvm.get(i) {
                // Win64: a 2-word `i128` (by-value Result/Option) crosses
                // the `extern "C"` boundary by pointer, not in a register
                // pair - that is how rustc lowers an `i128` parameter on
                // `x86_64-pc-windows`. Spill the value into a 16-byte-aligned
                // slot and pass its address, matching the runtime's ABI. On
                // SysV (Linux/macOS) `i128` is register-passed identically by
                // llc and rustc, so this branch is skipped there.
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
                    continue;
                }
            }
            let _ = write!(arg_text, "{a_ty} {a_v}");
        }
        // Build a `declare` stub matching the call's actual
        // shape so the module header carries a single coherent
        // declaration. The runtime fn's signature is whatever
        // we just emitted at the call site - record it here.
        // Prefer the registry signature when available so two
        // different call sites for the same symbol always agree.
        let mut decl_args = String::new();
        if !registry_param_llvm.is_empty() {
            // Win64: declare i128 (Fat) params as `ptr` so the declaration
            // matches the by-pointer call emitted above (and rustc's ABI).
            decl_args = registry_param_llvm
                .iter()
                .map(|t| {
                    if crate::emit::target_is_windows() && t == "i128" {
                        "ptr".to_string()
                    } else {
                        t.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
        } else {
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    decl_args.push_str(", ");
                }
                let t = self.operand_llvm_ty(arg);
                let t = if t == "void" || t.is_empty() {
                    "i64".to_string()
                } else {
                    t
                };
                let _ = write!(decl_args, "{t}");
            }
        }
        let registry_ret_llvm: Option<String> =
            gossamer_abi::lookup(name).map(|e| e.sig.ret.llvm_ir().to_string());
        // Win64: an `i128` (Fat) return crosses the boundary in a 16-byte
        // vector register, which rustc models as `<16 x i8>`; llc returns a
        // bare `i128` in a GP register pair instead, so the two disagree. We
        // declare + call the runtime symbol as `<16 x i8>` to match rustc,
        // then `bitcast` the result back to the `i128` the rest of the body
        // expects. Skipped on SysV, where bare `i128` already agrees.
        let win_fat_ret = super::misc::needs_win64_fat_ret(
            crate::emit::target_is_windows(),
            registry_ret_llvm.as_deref(),
        );
        // The logical return type the surrounding code consumes (always the
        // registry/MIR type); `decl_ret` below is the *wire* type used for the
        // declaration and call instruction.
        let logical_ret = if let Some(r) = &registry_ret_llvm {
            r.clone()
        } else if dest_ty == "void" || is_unit(self.tcx, self.body.local_ty(dest_local)) {
            "void".to_string()
        } else {
            dest_ty.clone()
        };
        let decl_ret = if win_fat_ret {
            "<16 x i8>".to_string()
        } else {
            logical_ret.clone()
        };
        // Always declare using call-site types so the declaration matches
        // the call instruction LLVM sees. Registry types (via declare_rt)
        // may differ - e.g. gos_rt_result_payload is I64 in the registry
        // but called as ptr in compiled MIR because the payload is a heap
        // pointer reinterpreted as i64 in the C ABI. On x86-64 both share
        // the rax register so the call is correct; the declaration must
        // agree with the call site or opt miscompiles with the wrong type.
        //
        // De-duplicate by function name: if any prior call site already
        // produced a declaration for this symbol, keep that one and
        // skip this insertion. Multiple distinct signatures for the
        // same name make `opt` reject the IR with `invalid
        // redefinition of function`, which then drops the whole module
        // into an LLVM verifier failure. Result/Option constructors with
        // mixed `i64` / `ptr` / `void` payload args are the canonical
        // trigger.
        let needle = format!("@{name}(");
        if !self.runtime_refs.iter().any(|d| d.contains(&needle)) {
            self.runtime_refs
                .insert(format!("declare {decl_ret} @{name}({decl_args})"));
        }
        if decl_ret == "void" {
            writeln!(self.out, "  call void @{name}({arg_text})").unwrap();
            self.release_minted_blob(minted_blob.as_deref());
            // Rvalue-position void call: synthesise a sentinel value
            // matching the destination slot's type. Normally the dest
            // is unit-typed (a no-op store), but the drop pass may assign
            // a free call to a local whose type is the function's return
            // type (e.g. ptr). LLVM 18 rejects `store ptr 0`; use `null`.
            let sentinel = match dest_ty.as_str() {
                "ptr" => "null",
                "double" | "float" => "0.0",
                _ => "0",
            };
            Ok(sentinel.to_string())
        } else {
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = call {decl_ret} @{name}({arg_text})").unwrap();
            self.release_minted_blob(minted_blob.as_deref());

            // Win64 Fat return: unwrap the `<16 x i8>` wire value back to the
            // `i128` the rest of the body manipulates.
            let tmp = if win_fat_ret {
                let unwrapped = self.fresh();
                writeln!(self.out, "  {unwrapped} = bitcast <16 x i8> {tmp} to i128").unwrap();
                unwrapped
            } else {
                tmp
            };
            // The declaration is canonical, but the destination
            // slot may expect a different but ABI-compatible
            // shape (e.g. `gos_rt_result_payload` returns `i64`
            // per registry, but the call site stores it into a
            // ptr-typed slot when the payload was a heap pointer
            // reinterpreted as an integer). Coerce so the
            // surrounding store / use is well-typed. Compare against the
            // logical return type, not the `<16 x i8>` wire type.
            if logical_ret != dest_ty && dest_ty != "void" && !dest_ty.is_empty() {
                let coerced = self.coerce_llvm_value(&tmp, &logical_ret, &dest_ty);
                Ok(coerced)
            } else {
                Ok(tmp)
            }
        }
    }

    pub(crate) fn lower_call_intrinsic(
        &mut self,
        name: &str,
        args: &[Operand],
        dest_local: Local,
    ) -> Result<String, BuildError> {
        let Some(intrinsic) = RawIntrinsic::from_name(name) else {
            return Err(BuildError::InternalLoweringBug(
                "unknown CallIntrinsic name",
            ));
        };
        if !intrinsic.arity_for_name(name).accepts(args.len()) {
            return Err(BuildError::InternalLoweringBug(
                "CallIntrinsic arity mismatch",
            ));
        }
        let dest_ty = render_ty(self.tcx, self.body.local_ty(dest_local));
        if let Some(inlined) = self.try_inline_result_carrier(name, args, &dest_ty)? {
            return Ok(inlined);
        }
        let llvm_intrinsic = match intrinsic {
            RawIntrinsic::F64Math(math) => {
                // `abs` is shared by the integer and floating-point method
                // surfaces. MIR can retain the `f64.abs` spelling for an
                // integer call while its typed destination is i64. Only a
                // double result can use LLVM's `fabs` intrinsic; routing an
                // integer receiver through it produces malformed IR
                // (`call i64 @llvm.fabs.f64(double i64)`).
                if math == gossamer_mir::F64MathIntrinsic::Abs
                    && render_ty(self.tcx, self.body.local_ty(dest_local)) != "double"
                {
                    return self.lower_runtime_call_intrinsic(
                        "gos_rt_math_abs_i64",
                        args,
                        dest_local,
                    );
                }
                math.llvm_name()
            }
            // Inline `buf.set_byte(i, x)` as a branchless bounds-guarded store
            // instead of a runtime call. `GosU8Vec` is `{ i64 len, ptr data }`;
            // an out-of-range / null access redirects to a scratch byte, which
            // reproduces `gos_rt_heap_u8_set`'s no-op-on-OOB semantics without
            // the per-byte call overhead (fasta's hot inner loop).
            RawIntrinsic::Runtime if name == "gos_rt_heap_u8_set" => {
                let v = self.lower_operand(&args[0])?;
                let idx = self.lower_operand(&args[1])?;
                let val = self.lower_operand(&args[2])?;
                self.emit_heap_u8_set_branchless(&v, &idx, &val);
                return Ok(match dest_ty.as_str() {
                    "ptr" => "null",
                    "double" | "float" => "0.0",
                    _ => "0",
                }
                .to_string());
            }
            RawIntrinsic::Runtime => {
                // Generic runtime-call intrinsic: emit a regular
                // call against the named runtime symbol. Mirrors
                // how the Cranelift backend's `lower_intrinsic_call`
                // falls through to a named-call dispatch when the
                // intrinsic isn't a recognised inline shape.
                return self.lower_runtime_call_intrinsic(name, args, dest_local);
            }
            RawIntrinsic::EnumLoad
            | RawIntrinsic::EnumSlotPtr
            | RawIntrinsic::EnumTag
            | RawIntrinsic::EnumDiscTag
            | RawIntrinsic::EnumUntag
            | RawIntrinsic::EnumDisc
            | RawIntrinsic::EnumSetDisc
            | RawIntrinsic::Load
            | RawIntrinsic::Store
            | RawIntrinsic::StoreI128
            | RawIntrinsic::Alloc
            | RawIntrinsic::RcAlloc
            | RawIntrinsic::RcAllocTagged
            | RawIntrinsic::RcAllocReuse
            | RawIntrinsic::EnumStructEq
            | RawIntrinsic::MapEnumKey
            | RawIntrinsic::FnAddr
            | RawIntrinsic::WeakOptPayload
            | RawIntrinsic::OwnedAggregatePayload
            | RawIntrinsic::JitUnsupportedUserIterator => {
                return Err(BuildError::InternalLoweringBug(
                    "raw pointer intrinsic reached rvalue lowering",
                ));
            }
        };
        // Ensure a `declare` for this intrinsic lands in the
        // module header.
        self.runtime_refs
            .insert(format!("declare double @{llvm_intrinsic}(double)"));
        let arg_v = self.lower_operand(&args[0])?;
        let arg_llvm = self.operand_llvm_ty(&args[0]);
        // An integer argument computes in floating point, the way
        // `3.sqrt()` reads: widen it rather than reinterpret it.
        let arg_v = if arg_llvm.starts_with('i') {
            let d = self.fresh();
            writeln!(self.out, "  {d} = sitofp {arg_llvm} {arg_v} to double").unwrap();
            d
        } else {
            arg_v
        };
        let dest_llvm = dest_ty;
        let tmp = self.fresh();
        writeln!(
            self.out,
            "  {tmp} = call {dest_llvm} @{llvm_intrinsic}(double {arg_v})"
        )
        .unwrap();
        Ok(tmp)
    }
}
