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
    BasicBlock, BinOp, Body, ConstValue, Local, Operand, Place, Projection, RawIntrinsic, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};

struct LoweredMapSlot {
    llvm_ty: &'static str,
    value: String,
}

/// Zero byte a discriminant read is steered at when the payload is null.
const ENUM_DISC_NULL_PAD: &str = ".gos_enum_disc_null";

impl<'a> Lowerer<'a> {
    /// Indirect call lowering for `f(args…)` where `f` is a
    /// local variable holding either a plain function pointer
    /// or a closure-environment record. The callee classifier
    /// follows what the Cranelift backend does in its
    /// `call_indirect` arm:
    ///   1. `FnDef` / `FnPtr`-typed local → value is the fn
    ///      address; call directly with the plain arg list.
    ///   2. Closure env (other reference / opaque ptr local) →
    ///      load fn pointer from `env[0]`, then call with `env`
    ///      as the implicit first arg followed by the user
    ///      args.
    pub(crate) fn lower_indirect_call(
        &mut self,
        place: &Place,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let callee_ty = self.body.local_ty(place.local);
        // Mirrors the Cranelift narrowing: only `FnDef`-typed
        // locals (the result of `Operand::FnRef`) hold a raw
        // function address. `FnPtr` / `FnTrait` locals carry an
        // env pointer post the MIR's let / return / assign
        // coercion, so they share the closure dispatch shape.
        let is_plain_fn = matches!(self.tcx.kind(callee_ty), Some(TyKind::FnDef { .. }));
        // Read the local's value: for a function pointer the
        // load yields the callable address; for a closure env
        // it yields the env pointer.
        let callee_llvm = render_ty(self.tcx, callee_ty);
        let raw_value = self.lower_place_read(place);
        // When a closure was stored through a Vec<i64> or similar
        // integer-typed container, the loaded value is an i64 holding
        // a pointer bit-pattern. Convert to ptr so it can be used as
        // a memory address for vtable + env dispatch.
        let env_value = if callee_llvm == "i64" {
            let p = self.fresh();
            writeln!(self.out, "  {p} = inttoptr i64 {raw_value} to ptr").unwrap();
            p
        } else {
            raw_value
        };
        let fn_ptr = if is_plain_fn {
            env_value.clone()
        } else {
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = load ptr, ptr {env_value}").unwrap();
            tmp
        };
        let dest_ty_mir = self.place_leaf_ty(destination);
        let dest_llvm = render_ty(self.tcx, dest_ty_mir);
        let mut arg_text = String::new();
        if !is_plain_fn {
            // Closure: env is the first arg.
            arg_text.push_str("ptr ");
            arg_text.push_str(&env_value);
        }
        for arg in args {
            if !arg_text.is_empty() {
                arg_text.push_str(", ");
            }
            let a_ty = self.operand_llvm_ty(arg);
            let a_v = self.lower_operand(arg)?;
            let _ = write!(arg_text, "{a_ty} {a_v}");
        }
        if dest_llvm == "void" || is_unit(self.tcx, dest_ty_mir) {
            writeln!(self.out, "  call void {fn_ptr}({arg_text})").unwrap();
        } else if is_aggregate(self.tcx, dest_ty_mir) {
            // A closure / fn-value returning a multi-slot aggregate heap-copies
            // it (like any user fn - see the Return lowering) and returns the
            // box pointer. Copy its slots into the destination's inline alloca,
            // then free the box; storing the bare pointer (the old behavior)
            // left every field past the first reading uninitialised memory.
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = call ptr {fn_ptr}({arg_text})").unwrap();
            let slot = if destination.projection.is_empty() {
                local_slot(destination.local)
            } else {
                self.lower_place_address(destination)
            };
            if let Some(slots) = slot_count(self.tcx, dest_ty_mir) {
                let bytes = u64::from(slots.max(1)) * 8;
                writeln!(
                    self.out,
                    "  call void @llvm.memcpy.p0.p0.i64(ptr {slot}, ptr {tmp}, i64 {bytes}, i1 false)"
                )
                .unwrap();
                declare_rt(&mut self.runtime_refs, "gos_rt_aggr_free");
                writeln!(
                    self.out,
                    "  call void @\"gos_rt_aggr_free\"(ptr {tmp}, i64 {bytes})"
                )
                .unwrap();
            } else {
                self.store_value_to_place(destination, "ptr", &tmp);
            }
        } else {
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = call {dest_llvm} {fn_ptr}({arg_text})").unwrap();
            self.store_value_to_place(destination, &dest_llvm, &tmp);
        }
        match target {
            Some(t) => {
                writeln!(self.out, "  br label %bb{}", t.as_u32()).unwrap();
            }
            None => {
                writeln!(self.out, "  unreachable").unwrap();
            }
        }
        Ok(())
    }

    /// Lowers a single operand to a `ptr` SSA holding the
    /// argument's stringification. Strings pass through; numeric
    /// types route through their `gos_rt_*_to_str` helper.
    pub(crate) fn lower_arg_to_str_ptr(&mut self, arg: &Operand) -> Result<String, BuildError> {
        for sym in [
            "gos_rt_i64_to_str",
            "gos_rt_u64_to_str",
            "gos_rt_f64_to_str",
            "gos_rt_bool_to_str",
            "gos_rt_char_to_str",
        ] {
            declare_rt(&mut self.runtime_refs, sym);
        }
        let kind = self.concat_print_kind(arg, "to_string");
        if matches!(kind, ConcatKind::Unsupported) {
            return Err(BuildError::InternalLoweringBug(
                "stringify of aggregate or variant types",
            ));
        }
        let value = self.lower_operand(arg)?;
        let dest = self.fresh();
        match kind {
            ConcatKind::StrPtr => Ok(value),
            ConcatKind::Int => {
                let widened = self.widen_to_i64(arg, &value);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_i64_to_str(i64 {widened})"
                )
                .unwrap();
                Ok(dest)
            }
            ConcatKind::Uint => {
                let widened = self.widen_to_u64(arg, &value);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_u64_to_str(i64 {widened})"
                )
                .unwrap();
                Ok(dest)
            }
            ConcatKind::Float => {
                let widened = self.widen_to_f64(arg, &value);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_f64_to_str(double {widened})"
                )
                .unwrap();
                Ok(dest)
            }
            ConcatKind::Bool => {
                let widened = self.widen_bool_to_i32(arg, &value);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_bool_to_str(i32 {widened})"
                )
                .unwrap();
                Ok(dest)
            }
            ConcatKind::Char => {
                let widened = self.widen_char_to_i32(arg, &value);
                writeln!(
                    self.out,
                    "  {dest} = call ptr @gos_rt_char_to_str(i32 {widened})"
                )
                .unwrap();
                Ok(dest)
            }
            ConcatKind::Unsupported => unreachable!("checked above"),
            // Every remaining kind is an aggregate its runtime formatter
            // renders to a c-string.
            kind => self.emit_concat_aggregate(arg, kind, &value),
        }
    }

    /// Direct-call lowering for `Operand::FnRef` and simple
    /// prelude-name calls (the MIR lowerer leaves prelude targets
    /// as `ConstValue::Str("println")` etc.). Indirect closure
    /// calls go through [`Self::lower_indirect_call`] via the
    /// `Operand::Copy` branch.
    pub(crate) fn lower_call(
        &mut self,
        callee: &Operand,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let target_name: Option<String> = match callee {
            Operand::FnRef { def, .. } => {
                // Resolve through the per-module `DefId.local` →
                // name map populated by the emitter. Unknown
                // `def.local` means the referenced function isn't
                // in this MIR module - typically a stdlib helper
                // the frontend was expected to monomorphise but
                // didn't. This is a hard backend error so the
                // missing monomorphisation surfaces at compile time.
                self.fn_name_by_def.get(&def.local).cloned()
            }
            Operand::Const(ConstValue::Str(name)) => Some(name.clone()),
            Operand::Copy(place) => {
                // `Copy(local)` callee: indirect call through a
                // function pointer. Two shapes:
                //   1. `FnDef`/`FnPtr` typed local - the value
                //      *is* the callable address.
                //   2. Closure env pointer - first heap word is
                //      the function pointer; env doubles as the
                //      implicit first argument.
                // Mirror Cranelift's `call_indirect` handling.
                self.lower_indirect_call(place, args, destination, target)?;
                return Ok(());
            }
            Operand::Const(_) => None,
        };
        let Some(name) = target_name else {
            // 0.8.0: this used to be a silent route-to-Cranelift
            // fallback. Now it's a hard error: report what was
            // unresolved so the frontend / MIR side has something
            // actionable to chase.
            let kind_label = match callee {
                Operand::FnRef { def, .. } => format!("FnRef def.local={:?}", def.local),
                Operand::Const(c) => format!("Const({c:?})"),
                Operand::Copy(_) => "Copy".to_string(),
            };
            return Err(BuildError::InternalLoweringBug(Box::leak(
                format!(
                    "indirect / closure call not lowered: callee shape {kind_label} \
                    has no resolvable name in fn_name_by_def - frontend monomorphisation \
                    bug or missing stdlib registration"
                )
                .into_boxed_str(),
            )));
        };
        let name = resolve_external_binding_symbol(&name, args.len()).unwrap_or(name);
        let name = if name == "gos_rt_bytearr_slice_result" {
            "gos_rt_packed_bytearr_slice_result".to_string()
        } else {
            name
        };
        // `__concat` is the parser's lowering of `println!`-style
        // formatted output: it takes a heterogeneous arg list,
        // prints each piece directly to stdout, and produces an
        // empty-string pointer for the surrounding `println` call
        // to consume. Mirror the Cranelift backend's per-arg
        // dispatch (one runtime print call per operand keyed off
        // the operand's MIR kind).
        if name == "__concat" {
            self.lower_concat_call(args, destination, target, false)?;
            return Ok(());
        }
        // `__debug` is the `{:?}` channel: identical to `__concat` except a
        // float always renders with a fractional part or an exponent.
        if name == "__debug" {
            self.lower_concat_call(args, destination, target, true)?;
            return Ok(());
        }
        // `__fmt_prec(value, prec)` - emitted by macro expansion for
        // `{:.N}` specs. Routes through `gos_rt_f64_prec_to_str` so
        // the result is a heap String that the surrounding `__concat`
        // pipeline consumes like any other string operand.
        if name == "__fmt_prec" {
            self.lower_fmt_prec_call(args, destination, target)?;
            return Ok(());
        }
        // `println` / `print` / `eprintln` / `eprint` route to
        // the runtime's `gos_rt_print_str` for each arg, plus a
        // trailing `gos_rt_println()` for the `*ln` variants.
        // This mirrors what the Cranelift backend does in
        // `lower_intrinsic_call` - the runtime's println is
        // arity-0, so an inline `gos_rt_print_str(arg)` then
        // `gos_rt_println()` reproduces the user-level
        // `println(s)` semantics.
        if matches!(name.as_str(), "println" | "print" | "eprintln" | "eprint") {
            self.lower_print_call(&name, args, destination, target)?;
            return Ok(());
        }
        // `panic(args...)` builds a single concatenated message
        // (space-joined to mirror the interpreter's
        // `render_args`) and routes it through `gos_rt_panic`,
        // which is `noreturn` and emits the GX0005 prefix +
        // aborts. Fall back to an empty-string pointer when no
        // args were given.
        if name == "panic" {
            // `panic(args...)` builds a single space-joined
            // message via `gos_rt_str_concat` over the
            // per-arg `to_str` helpers, then calls
            // `gos_rt_panic` (noreturn). Empty arg list panics
            // with an empty message - `gos_rt_panic` then
            // emits its default "panic" string.
            declare_rt(&mut self.runtime_refs, "gos_rt_panic");
            let msg = self.emit_args_to_concat_string(args, " ")?;
            writeln!(self.out, "  call void @gos_rt_panic(ptr {msg})").unwrap();
            writeln!(self.out, "  unreachable").unwrap();
            return Ok(());
        }
        // Integer `.abs()` calls can still reach the Terminator::Call route
        // with an `f64.abs`-style name. Dispatch from the destination type,
        // which is the authoritative MIR representation, before selecting a
        // floating-point LLVM intrinsic.
        if name.rsplit("::").next().is_some_and(|tail| tail == "abs")
            && args.len() == 1
            && render_ty(self.tcx, self.body.local_ty(destination.local)) != "double"
        {
            self.emit_named_call("gos_rt_math_abs_i64", args, destination, target)?;
            return Ok(());
        }
        // Recognise `math::*` calls and emit a direct
        // LLVM intrinsic invocation instead of routing
        // through an undefined `@"math::sqrt"` symbol. These
        // lower to the host's SSE/AVX instruction via `llc`.
        if let Some(intrinsic_name) = math_intrinsic(&name)
            && args.len() == 1
        {
            self.lower_math_intrinsic(intrinsic_name, &args[0], destination, target)?;
            return Ok(());
        }
        // Hot path inlining for byte-at-a-time stdout writes.
        // The runtime exposes the stdout buffer as a pair of
        // global symbols (`@GOS_RT_STDOUT_BYTES` / `@GOS_RT_STDOUT_LEN`);
        // we emit the buffer-append fast path directly so that
        // fasta-style inner loops (50M+ calls) don't pay one
        // FFI call per byte. Only the slow path (full buffer)
        // falls through to `gos_rt_stream_write_byte`.
        if name == "gos_rt_stream_write_byte" && args.len() == 2 {
            self.lower_stream_write_byte_inline(args, destination, target)?;
            return Ok(());
        }
        // Bulk byte-array write for stdout: pack `len` low-bytes
        // of an `[i64; N]` array into the global stdout buffer
        // inline. The fasta_block / fasta_mt programs call
        // `out.write_byte_array(&line, line_len + 1)` once per
        // 60-char line; without inlining each line pays one
        // FFI call. With inlining the loop bound is usually
        // a compile-time-known small integer, so LLVM unrolls
        // and the per-byte pack drops to one `mov` + `inc`.
        if name == "gos_rt_stream_write_byte_array" && args.len() == 3 {
            self.lower_stream_write_byte_array_inline(args, destination, target)?;
            return Ok(());
        }
        // String append must update both byte-length and Unicode-index
        // metadata. Keep that ownership-aware operation in the runtime shim.
        // `vec.len()` reads the `len: i64` field at offset 0 of the
        // `GosVec` heap struct. One FFI call per loop iteration
        // would otherwise dominate any `for i in 0..xs.len() { … }`
        // shape that doesn't pre-cache the length.
        if name == "gos_rt_vec_len" && args.len() == 1 {
            self.lower_vec_len_inline(&args[0], destination, target)?;
            return Ok(());
        }
        // A string scan calls these once per character or byte, and each is a
        // guarded header read that costs less than the call reaching it. The
        // length shims matter twice over: an opaque call in `while i <
        // s.len()` is re-evaluated every iteration and cannot be hoisted.
        if name == "gos_rt_str_byte_at" && args.len() == 2 {
            self.lower_str_byte_at_inline(args, destination, target)?;
            return Ok(());
        }
        if name == "gos_rt_str_char_at" && args.len() == 2 {
            self.lower_str_char_at_inline(args, destination, target)?;
            return Ok(());
        }
        if name == "gos_rt_str_byte_len" && args.len() == 1 {
            self.lower_str_byte_len_inline(&args[0], destination, target)?;
            return Ok(());
        }
        if name == "gos_rt_str_len" && args.len() == 1 {
            self.lower_str_len_inline(&args[0], destination, target)?;
            return Ok(());
        }
        // `.len()` on a Vec/Slice that routed through the generic
        // `gos_rt_len` dispatcher: same null-guarded header load, but
        // only when the static element type pins the receiver as a
        // real word-stride GosVec - never `Vec<String>`, whose
        // `env::args()` sentinel pointer keeps its length in
        // `ARGS_LEN` rather than at `*p`.
        if name == "gos_rt_len" && args.len() == 1 && self.vec_operand_has_word_elem(&args[0]) {
            self.lower_vec_len_inline(&args[0], destination, target)?;
            return Ok(());
        }
        // `s.split(c)` where `c` is a char: the runtime takes `sep: *const c_char`
        // (a C string pointer), but MIR emits the char code as `i32`. Convert
        // via `gos_rt_char_to_str` first, mirroring the Cranelift backend.
        if name == "gos_rt_str_split" && args.len() == 2 {
            if matches!(
                self.concat_print_kind(&args[1], "to_string"),
                ConcatKind::Char
            ) {
                declare_rt(&mut self.runtime_refs, "gos_rt_char_to_str");
                declare_rt(&mut self.runtime_refs, "gos_rt_str_split");
                let s = self.lower_operand(&args[0])?;
                let c_raw = self.lower_operand(&args[1])?;
                let c_widened = self.widen_char_to_i32(&args[1], &c_raw);
                let sep_ptr = self.fresh();
                let tmp = self.fresh();
                writeln!(
                    self.out,
                    "  {sep_ptr} = call ptr @gos_rt_char_to_str(i32 {c_widened})"
                )
                .unwrap();
                writeln!(
                    self.out,
                    "  {tmp} = call ptr @gos_rt_str_split(ptr {s}, ptr {sep_ptr})"
                )
                .unwrap();
                self.store_value_to_place(destination, "ptr", &tmp);
                emit_terminator_branch(&mut self.out, target);
                return Ok(());
            }
        }
        // Heap-Vec inline fast paths. The runtime returns a
        // `*mut GosI64Vec { len: i64, data: *mut i64 }`; user
        // code accesses elements via `vec.set_at(i, v)` and
        // `vec.get_at(i)`. Without inlining each access pays
        // one FFI call (~5-20 ns), which dominates the
        // multi-threaded fasta hot loop. The inline shape
        // skips the runtime's bounds check (caller is
        // expected to keep `i` in range, same convention as
        // `str_byte_at`).
        if name == "gos_rt_heap_i64_set" && args.len() == 3 {
            self.lower_heap_i64_set_inline(args, destination, target)?;
            return Ok(());
        }
        if name == "gos_rt_heap_i64_get" && args.len() == 2 {
            self.lower_heap_i64_get_inline(args, destination, target)?;
            return Ok(());
        }
        // Raw heap intrinsics that the cranelift tier handles
        // inline. Lower them to a direct LLVM `getelementptr +
        // load/store` here so the LLVM tier doesn't bail and
        // route the body to cranelift just for these calls.
        if matches!(
            name.as_str(),
            "gos_load"
                | "gos_store"
                | "gos_store_i128"
                | "gos_alloc"
                | "gos_rc_alloc"
                | "gos_rc_alloc_reuse"
                | "gos_fn_addr"
                | "gos_enum_disc"
                | "gos_enum_set_disc"
                | "gos_rt_enum_struct_eq"
                | "gos_rt_map_insert_ekey_opt"
                | "gos_rt_map_get_ekey_opt"
                | "gos_rt_map_contains_ekey"
                | "gos_rt_map_pop_ekey"
                | "gos_rt_map_get_or_ekey"
                | "gos_rt_map_or_insert_ekey"
                | "gos_rt_map_inc_ekey"
                | "gos_rt_set_insert_ekey"
                | "gos_rt_set_contains_ekey"
                | "gos_rt_set_remove_ekey"
        ) {
            self.lower_raw_intrinsic(&name, args, destination, target)?;
            return Ok(());
        }
        // `v.push(x)` on a Vec - the runtime helper takes
        // `(*mut GosVec, *const u8)` and `memcpy`s the value
        // through the second pointer. The MIR routes every
        // element type to the same generic `gos_rt_vec_push`
        // symbol, so we have to spill the i64 / ptr value into
        // an alloca and pass the alloca's address. Without this
        // the LLVM tier passed the i64 value as a pointer and
        // the helper segfaulted dereferencing it (`SEGV_MAPERR`
        // at `si_addr=value`). Mirrors the cranelift-side
        // stack-slot dance in `lower_intrinsic_call`.
        if name == "gos_rt_vec_push" && args.len() == 2 {
            self.lower_vec_push_inline(args, destination, target)?;
            return Ok(());
        }
        // A slot container's wide push takes the address of the element's
        // slots, exactly as the Vec push does: the runtime copies the
        // element store's own stride from it.
        if matches!(
            name.as_str(),
            "gos_rt_deque_push_back_wide" | "gos_rt_deque_push_front_wide"
        ) && args.len() == 2
        {
            self.lower_container_push_wide(&name, args, destination, target)?;
            return Ok(());
        }
        if name == "gos_rt_vec_pop_opt"
            && args.len() == 1
            && render_ty(self.tcx, self.body.local_ty(destination.local)) == "i128"
            && (self.vec_operand_has_word_elem(&args[0])
                || self.vec_operand_has_byte_elem(&args[0]))
        {
            self.lower_vec_pop_opt_inline(args, destination, target)?;
            return Ok(());
        }
        // Inline scalar `min`/`max` on i64 to a branchless `icmp`+`select`
        // (value-identical to the runtime `a.min(b)`/`a.max(b)`), removing a
        // per-call FFI boundary from tight numeric loops and unblocking
        // vectorization. `clamp` and the `f64` variants keep the runtime call.
        if (name == "gos_rt_min_i64" || name == "gos_rt_max_i64") && args.len() == 2 {
            self.lower_scalar_minmax_i64_inline(
                name == "gos_rt_min_i64",
                args,
                destination,
                target,
            )?;
            return Ok(());
        }
        // Inline primitive Vec index get/set/get_ptr. Checked scalar
        // get/set keep their runtime panic semantics on the slow path while
        // valid indices avoid a per-element FFI call in hot loops.
        // Inline when the element is a single-word scalar with no read-time
        // ownership: integer/bool and `f64` (the numeric-kernel case - a
        // `Vec<f64>` matvec read is bit-identical to the i64 path through the
        // bitcast in `store_i64_as`). A heap-pointer Adt element (e.g.
        // `Vec<DirInfo>`, where `&entries[i]` has reference-through-handle
        // semantics the generic call-result path handles) keeps the runtime
        // call.
        if name == "gos_rt_vec_get_i64"
            && args.len() == 2
            && (is_inline_vec_scalar_llvm(&render_ty(
                self.tcx,
                self.body.local_ty(destination.local),
            )) || self.vec_operand_elem_is_vec(&args[0]))
        {
            self.lower_vec_get_i64_inline(args, destination, target)?;
            return Ok(());
        }
        // The MIR emits this only for the counted-loop element read of a
        // primitive int/bool/char Vec, where the index is proven in
        // `[0, len)` and the receiver non-null. Inline it without the null /
        // bounds guard so the inner loop is a straight load.
        if name == "gos_rt_vec_get_i64_unchecked" && args.len() == 2 {
            self.lower_vec_get_i64_unchecked_inline(args, destination, target)?;
            return Ok(());
        }
        if name == "gos_rt_vec_set_i64"
            && args.len() == 3
            && is_inline_vec_scalar_llvm(&self.operand_llvm_ty(&args[2]))
        {
            self.lower_vec_set_i64_inline(args, destination, target)?;
            return Ok(());
        }
        // The MIR emits this only from the bounds-check elision of a
        // counted loop, where the index is proven in `[0, len)` and the
        // receiver non-null. Inline it without the null / bounds guard so
        // the inner loop is a straight store.
        if name == "gos_rt_vec_set_i64_unchecked" && args.len() == 3 {
            self.lower_vec_set_i64_unchecked_inline(args, destination, target)?;
            return Ok(());
        }
        if name == "gos_rt_vec_swap_safe"
            && args.len() == 3
            && (self.vec_operand_has_word_elem(&args[0])
                || self.vec_operand_has_byte_elem(&args[0]))
        {
            self.lower_vec_swap_safe_inline(args, target)?;
            return Ok(());
        }
        if name == "gos_rt_vec_swap_i64"
            && args.len() == 3
            && (self.vec_operand_has_word_elem(&args[0])
                || self.vec_operand_has_byte_elem(&args[0]))
        {
            self.lower_vec_swap_i64_inline(args, destination, target)?;
            return Ok(());
        }
        // `buf.set_byte(i, x)` on the Terminator::Call route (fasta's inner
        // loop). The branchless inline also fires on the Rvalue::CallIntrinsic
        // route (`lower_call_intrinsic`); route both to the same body so the
        // per-byte FFI call disappears regardless of how the call was lowered.
        if name == "gos_rt_heap_u8_set" && args.len() == 3 {
            self.lower_heap_u8_set_inline(args, destination, target)?;
            return Ok(());
        }
        // `gos_rt_vec_get_ptr` is inlined only when the destination is a bare
        // element pointer (`ptr`-typed) that a following field projection
        // dereferences - e.g. `table[j].1` on `Vec<(i64, f64)>`, or
        // `bodies[i].field`. A multi-slot aggregate destination copies the
        // whole element out of the returned address (the generic call-result
        // path's memcpy), which the inline does not reproduce, so that case
        // stays on the opaque call.
        if name == "gos_rt_vec_get_ptr"
            && args.len() == 2
            && render_ty(self.tcx, self.body.local_ty(destination.local)) == "ptr"
            && !is_aggregate(self.tcx, self.body.local_ty(destination.local))
        {
            self.lower_vec_get_ptr_inline(args, destination, target)?;
            return Ok(());
        }
        // Variant constructor stubs: `Ok(v)`, `Some(v)`, `Err(e)`
        // pass the wrapped value through unchanged (the compiled
        // tier flattens Option/Result, so `unwrap` is identity).
        // `None` and other no-payload variants resolve to a zero
        // value. Mirrors the Cranelift backend's variant-stub
        // branch so escaped Result/Option values don't end up
        // calling a non-existent `@"Ok"` symbol at link time.
        let is_variant_stub = matches!(name.as_str(), "Ok" | "Some" | "Err" | "None")
            || (name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && !name.contains("::"));
        if is_variant_stub {
            self.emit_variant_stub(&name, args, destination, target)?;
            return Ok(());
        }
        // `channel()` / `sync::channel()` - the std prelude shape
        // for unbuffered channels. MIR emits a 0-arg `Call("channel",
        // dest=tuple_local)`; we lower to `gos_rt_chan_new(8, 0)` and
        // mirror the cranelift backend's "write chan_ptr to both
        // tuple slots" trick so `pair.0` and `pair.1` (Sender +
        // Receiver) project to the same handle.
        if matches!(
            name.as_str(),
            "channel"
                | "channel::new"
                | "channel::unbounded"
                | "sync::channel"
                | "sync::channel_unbounded"
                | "std::sync::channel_unbounded"
                | "sync::Channel::new"
                | "gos_rt_chan_new"
                | "Channel::new"
        ) {
            declare_rt(&mut self.runtime_refs, "gos_rt_chan_new");
            let cap_arg = if let Some(a) = args.first() {
                Some(self.lower_operand(a)?)
            } else {
                None
            };
            let cap = if matches!(
                name.as_str(),
                "channel::unbounded" | "sync::channel_unbounded" | "std::sync::channel_unbounded"
            ) {
                "-1".to_string()
            } else {
                cap_arg.unwrap_or_else(|| "0".to_string())
            };
            let tmp = self.fresh();
            writeln!(
                self.out,
                "  {tmp} = call ptr @gos_rt_chan_new(i32 8, i64 {cap})"
            )
            .unwrap();
            // Materialise a fresh 16-byte tuple buffer so the
            // `(Sender, Receiver)` projections both observe the
            // same channel handle. The destination MIR local may
            // be a single-ptr alloca (typeck doesn't always
            // preserve the tuple shape through the channel-call
            // path), so writing slot 1 into the destination
            // directly would overflow the alloca and clobber
            // adjacent stack memory. Mirrors the Cranelift
            // backend's `create_sized_stack_slot(16, 3)` shape.
            // Block-scoped rather than hoisted to the entry block: the
            // buffer's lifetime is the destination local's, not the call's,
            // so each execution of this call must hand back a distinct
            // address for a `.0` / `.1` read that outlives the iteration.
            let pair_buf = self.fresh();
            writeln!(self.out, "  {pair_buf} = alloca [2 x i64]").unwrap();
            let slot0 = self.fresh();
            writeln!(
                self.out,
                "  {slot0} = getelementptr i64, ptr {pair_buf}, i64 0"
            )
            .unwrap();
            writeln!(self.out, "  store ptr {tmp}, ptr {slot0}").unwrap();
            let slot1 = self.fresh();
            writeln!(
                self.out,
                "  {slot1} = getelementptr i64, ptr {pair_buf}, i64 1"
            )
            .unwrap();
            writeln!(self.out, "  store ptr {tmp}, ptr {slot1}").unwrap();
            let dest_slot = if destination.projection.is_empty() {
                local_slot(destination.local)
            } else {
                self.lower_place_address(destination)
            };
            let dest_ty = self.place_leaf_ty(destination);
            if slot_count(self.tcx, dest_ty).is_some_and(|n| n >= 2) {
                // Typeck preserved the `(Sender, Receiver)` tuple shape, so the
                // destination is a multi-slot alloca that holds the handle
                // pair inline (`.0` / `.1` read its slots directly). Write the
                // channel handle into both slots - storing the buffer address
                // would leave `.0` reading the pointer bits instead of the
                // handle.
                let d0 = self.fresh();
                writeln!(
                    self.out,
                    "  {d0} = getelementptr i64, ptr {dest_slot}, i64 0"
                )
                .unwrap();
                writeln!(self.out, "  store ptr {tmp}, ptr {d0}").unwrap();
                let d1 = self.fresh();
                writeln!(
                    self.out,
                    "  {d1} = getelementptr i64, ptr {dest_slot}, i64 1"
                )
                .unwrap();
                writeln!(self.out, "  store ptr {tmp}, ptr {d1}").unwrap();
            } else {
                // Single-ptr destination (the tuple shape was erased): store
                // the buffer address so downstream `.0` / `.1` projections
                // lower as loads from `pair_buf + N*8`.
                writeln!(self.out, "  store ptr {pair_buf}, ptr {dest_slot}").unwrap();
            }
            emit_terminator_branch(&mut self.out, target);
            return Ok(());
        }
        // Avoid returning the channel Option<T> through the Rust/C `i128`
        // boundary on LLVM native builds. The runtime already exposes the
        // primitive status-plus-out-pointer calls; packing the 2-word Option
        // in this module keeps the ABI register convention entirely under
        // LLVM's control. This is especially important on non-x86_64 targets
        // where a closed recv misread as `Some` turns drain loops into
        // infinite loops.
        // The cancellation-aware receive carries a context handle alongside
        // the channel and reaches the same status-plus-out-pointer runtime
        // call, so it repacks here through the identical sequence.
        let ctx_recv = name.as_str() == "gos_rt_chan_recv_ctx_option" && args.len() == 2;
        if (matches!(
            name.as_str(),
            "gos_rt_chan_recv_option" | "gos_rt_chan_try_recv_option"
        ) && args.len() == 1
            || ctx_recv)
            && render_ty(self.tcx, self.body.local_ty(destination.local)) == "i128"
        {
            let chan = self.lower_operand(&args[0])?;
            let ctx = if ctx_recv {
                let handle = self.lower_operand(&args[1])?;
                // A context handle travels as an integer in the body; the
                // runtime takes it as an opaque pointer.
                if render_ty(self.tcx, self.operand_ty(&args[1])) == "ptr" {
                    Some(handle)
                } else {
                    let as_ptr = self.fresh();
                    writeln!(self.out, "  {as_ptr} = inttoptr i64 {handle} to ptr").unwrap();
                    Some(as_ptr)
                }
            } else {
                None
            };
            let out_slot = self.entry_alloca("i64");
            writeln!(self.out, "  store i64 0, ptr {out_slot}").unwrap();
            let status_fn = match name.as_str() {
                "gos_rt_chan_recv_option" => "gos_rt_chan_recv",
                "gos_rt_chan_recv_ctx_option" => "gos_rt_chan_recv_ctx",
                _ => "gos_rt_chan_try_recv",
            };
            declare_rt(&mut self.runtime_refs, status_fn);
            let status = self.fresh();
            if let Some(ctx) = ctx {
                writeln!(
                    self.out,
                    "  {status} = call i32 @\"{status_fn}\"(ptr {chan}, ptr {ctx}, ptr {out_slot})"
                )
                .unwrap();
            } else {
                writeln!(
                    self.out,
                    "  {status} = call i32 @\"{status_fn}\"(ptr {chan}, ptr {out_slot})"
                )
                .unwrap();
            }
            let status_i64 = self.fresh();
            writeln!(self.out, "  {status_i64} = sext i32 {status} to i64").unwrap();
            let disc = self.fresh();
            writeln!(self.out, "  {disc} = sub i64 1, {status_i64}").unwrap();
            let is_some = self.fresh();
            writeln!(self.out, "  {is_some} = icmp eq i32 {status}, 1").unwrap();
            let raw_payload = self.fresh();
            writeln!(self.out, "  {raw_payload} = load i64, ptr {out_slot}").unwrap();
            let payload = self.fresh();
            writeln!(
                self.out,
                "  {payload} = select i1 {is_some}, i64 {raw_payload}, i64 0"
            )
            .unwrap();
            let disc128 = self.fresh();
            writeln!(self.out, "  {disc128} = zext i64 {disc} to i128").unwrap();
            let payload128 = self.fresh();
            writeln!(self.out, "  {payload128} = zext i64 {payload} to i128").unwrap();
            let shifted = self.fresh();
            writeln!(self.out, "  {shifted} = shl i128 {payload128}, 64").unwrap();
            let packed = self.fresh();
            writeln!(self.out, "  {packed} = or i128 {shifted}, {disc128}").unwrap();
            self.store_value_to_place(destination, "i128", &packed);
            emit_terminator_branch(&mut self.out, target);
            return Ok(());
        }
        // `Vec::new(elem_bytes)` - the runtime helper signature is
        // `gos_rt_vec_new(elem_bytes: u32)`. MIR passes the element
        // width as `i64`, so we truncate to `i32` before the call.
        if matches!(name.as_str(), "Vec::new" | "gos_rt_vec_new") {
            let kind = llvm_vec_elem_kind_from_local(self.body, self.tcx, destination.local);
            let eb_i64 = if let Some(a) = args.first() {
                self.lower_operand(a)?
            } else {
                llvm_vec_elem_bytes_from_local(self.body, self.tcx, destination.local)
                    .unwrap_or(8)
                    .to_string()
            };
            let eb_i32 = self.fresh();
            writeln!(self.out, "  {eb_i32} = trunc i64 {eb_i64} to i32").unwrap();
            let tmp = self.fresh();
            if kind == vec_elem_kind_llvm::PRIMITIVE {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_new");
                writeln!(
                    self.out,
                    "  {tmp} = call noalias ptr @gos_rt_vec_new(i32 {eb_i32})"
                )
                .unwrap();
            } else {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_new_typed");
                writeln!(
                    self.out,
                    "  {tmp} = call noalias ptr @gos_rt_vec_new_typed(i32 {eb_i32}, i8 {kind})"
                )
                .unwrap();
            }
            self.store_value_to_place(destination, "ptr", &tmp);
            emit_terminator_branch(&mut self.out, target);
            return Ok(());
        }
        // `Vec::with_capacity(elem_bytes, cap)` mirrors `Vec::new` but
        // pre-allocates buffer space.
        if matches!(
            name.as_str(),
            "Vec::with_capacity" | "gos_rt_vec_with_capacity"
        ) {
            let kind = llvm_vec_elem_kind_from_local(self.body, self.tcx, destination.local);
            let eb_i64 = if let Some(a) = args.first() {
                self.lower_operand(a)?
            } else {
                llvm_vec_elem_bytes_from_local(self.body, self.tcx, destination.local)
                    .unwrap_or(8)
                    .to_string()
            };
            let cap_i64 = if let Some(a) = args.get(1) {
                self.lower_operand(a)?
            } else {
                "0".to_string()
            };
            let eb_i32 = self.fresh();
            writeln!(self.out, "  {eb_i32} = trunc i64 {eb_i64} to i32").unwrap();
            let tmp = self.fresh();
            if kind == vec_elem_kind_llvm::PRIMITIVE {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_with_capacity");
                writeln!(
                    self.out,
                    "  {tmp} = call noalias ptr @gos_rt_vec_with_capacity(i32 {eb_i32}, i64 {cap_i64})"
                )
                .unwrap();
            } else {
                declare_rt(&mut self.runtime_refs, "gos_rt_vec_with_capacity_typed");
                writeln!(
                    self.out,
                    "  {tmp} = call noalias ptr @gos_rt_vec_with_capacity_typed(i32 {eb_i32}, i64 {cap_i64}, i8 {kind})"
                )
                .unwrap();
            }
            self.store_value_to_place(destination, "ptr", &tmp);
            emit_terminator_branch(&mut self.out, target);
            return Ok(());
        }
        // HashMap / collection constructors: MIR emits a 0-arg call but the
        // runtime function takes (key_bytes: i32, val_bytes: i32). The widths
        // are ABI placeholders only: typed insertion selects storage lazily.
        if matches!(
            name.as_str(),
            "Map::new"
                | "collections::Map::new"
                | "std::collections::Map::new"
                | "HashMap::new"
                | "collections::HashMap::new"
                | "std::collections::HashMap::new"
                | "BTreeMap::new"
                | "collections::BTreeMap::new"
                | "std::collections::BTreeMap::new"
                | "gos_rt_map_new"
        ) {
            let tmp = self.fresh();
            if let Some((key_kind, val_kind)) = self
                .body
                .locals
                .get(destination.local.0 as usize)
                .and_then(|decl| match self.tcx.kind_of(decl.ty) {
                    TyKind::HashMap { key, value, .. } => self.hashmap_storage_kinds(*key, *value),
                    _ => None,
                })
            {
                declare_rt(&mut self.runtime_refs, "gos_rt_map_new_with_capacity_typed");
                writeln!(
                    self.out,
                    "  {tmp} = call ptr @gos_rt_map_new_with_capacity_typed(i32 {key_kind}, i32 {val_kind}, i64 0)"
                )
                .unwrap();
            } else {
                declare_rt(&mut self.runtime_refs, "gos_rt_map_new");
                writeln!(self.out, "  {tmp} = call ptr @gos_rt_map_new(i32 8, i32 8)").unwrap();
            }
            self.store_value_to_place(destination, "ptr", &tmp);
            if let Some(tgt) = target {
                writeln!(self.out, "  br label %bb{}", tgt.as_u32()).unwrap();
            }
            return Ok(());
        }
        if matches!(
            name.as_str(),
            "Map::with_capacity"
                | "collections::Map::with_capacity"
                | "std::collections::Map::with_capacity"
                | "HashMap::with_capacity"
                | "collections::HashMap::with_capacity"
                | "std::collections::HashMap::with_capacity"
                | "gos_rt_map_new_with_capacity"
        ) {
            let tmp = self.fresh();
            let typed_kinds = self
                .body
                .locals
                .get(destination.local.0 as usize)
                .and_then(|decl| match self.tcx.kind_of(decl.ty) {
                    TyKind::HashMap { key, value, .. } => {
                        let kind = |ty| match self.tcx.kind_of(ty) {
                            TyKind::Int(_) => Some(0),
                            TyKind::String => Some(1),
                            TyKind::Vec(elem) | TyKind::Slice(elem)
                                if matches!(
                                    self.tcx.kind_of(*elem),
                                    TyKind::Int(gossamer_types::IntTy::U8)
                                ) =>
                            {
                                Some(2)
                            }
                            _ => None,
                        };
                        Some((kind(*key)?, kind(*value)?))
                    }
                    _ => None,
                });
            if let Some((key_kind, val_kind)) = typed_kinds {
                declare_rt(&mut self.runtime_refs, "gos_rt_map_new_with_capacity_typed");
                let cap = if let Some(a) = args.first() {
                    self.lower_operand(a)?
                } else {
                    "0".to_string()
                };
                writeln!(
                    self.out,
                    "  {tmp} = call ptr @gos_rt_map_new_with_capacity_typed(i32 {key_kind}, i32 {val_kind}, i64 {cap})"
                )
                .unwrap();
            } else {
                // Aggregate layouts retain their lazy constructor until their
                // concrete storage descriptor is part of this ABI.
                declare_rt(&mut self.runtime_refs, "gos_rt_map_new");
                writeln!(self.out, "  {tmp} = call ptr @gos_rt_map_new(i32 8, i32 8)").unwrap();
            }
            self.store_value_to_place(destination, "ptr", &tmp);
            if let Some(tgt) = target {
                writeln!(self.out, "  br label %bb{}", tgt.as_u32()).unwrap();
            }
            return Ok(());
        }
        if matches!(
            name.as_str(),
            "Map::from"
                | "collections::Map::from"
                | "std::collections::Map::from"
                | "HashMap::from"
                | "collections::HashMap::from"
                | "std::collections::HashMap::from"
                | "BTreeMap::from"
                | "collections::BTreeMap::from"
                | "std::collections::BTreeMap::from"
        ) && args.len() == 1
        {
            self.lower_hashmap_from_array(&args[0], destination, target)?;
            return Ok(());
        }
        if matches!(
            name.as_str(),
            "gos_rt_vec_from_arr" | "gos_rt_vec_borrow_arr"
        ) && args.get(1).is_some_and(|arg| {
            matches!(
                arg,
                Operand::Copy(place)
                    if place.projection.is_empty()
                        && packed_byte_array_len(self.tcx, self.body.local_ty(place.local))
                            .is_some()
            )
        }) {
            let symbol = if name == "gos_rt_vec_borrow_arr" {
                "gos_rt_vec_borrow_packed_arr"
            } else {
                "gos_rt_vec_from_packed_arr"
            };
            declare_rt(&mut self.runtime_refs, symbol);
            let elem_bytes = if let Some(arg) = args.first() {
                self.lower_operand(arg)?
            } else {
                "1".to_string()
            };
            let elem_bytes_i32 = self.fresh();
            writeln!(
                self.out,
                "  {elem_bytes_i32} = trunc i64 {elem_bytes} to i32"
            )
            .unwrap();
            let data = self.lower_operand(&args[1])?;
            let data_ptr = self.coerce_llvm_value(&data, &self.operand_llvm_ty(&args[1]), "ptr");
            let len = if let Some(arg) = args.get(2) {
                self.lower_operand(arg)?
            } else {
                "0".to_string()
            };
            let len_i64 = self.coerce_llvm_value(
                &len,
                &args
                    .get(2)
                    .map_or("i64".to_string(), |arg| self.operand_llvm_ty(arg)),
                "i64",
            );
            let tmp = self.fresh();
            writeln!(
                self.out,
                "  {tmp} = call ptr @\"{symbol}\"(i32 {elem_bytes_i32}, ptr {data_ptr}, i64 {len_i64})"
            )
            .unwrap();
            self.store_value_to_place(destination, "ptr", &tmp);
            emit_terminator_branch(&mut self.out, target);
            return Ok(());
        }
        let mapped = map_prelude_symbol(&name);
        // `map_prelude_symbol` returns the name unchanged only for a
        // user-defined function (every runtime/prelude name maps to a
        // distinct `gos_rt_*` symbol). Route that case through the
        // mangler so the user symbol can't shadow a libc/runtime symbol.
        let mangled;
        let symbol = if mapped == name.as_str() {
            mangled = mangle_fn_name(&name);
            mangled.as_ref()
        } else {
            mapped
        };
        self.emit_named_call(symbol, args, destination, target)
    }

    fn lower_hashmap_from_array(
        &mut self,
        arg: &Operand,
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let arg_ty = self.unwrap_ref(self.operand_ty(arg));
        let Some(TyKind::Array { elem, len }) = self.tcx.kind(arg_ty).cloned() else {
            return Err(BuildError::InternalLoweringBug(
                "HashMap::from native lowering expects an array argument",
            ));
        };
        let Some(TyKind::Tuple(fields)) = self.tcx.kind(elem).cloned() else {
            return Err(BuildError::InternalLoweringBug(
                "HashMap::from native lowering expects tuple array elements",
            ));
        };
        let [key_ty, val_ty] = fields.as_slice() else {
            return Err(BuildError::InternalLoweringBug(
                "HashMap::from native lowering expects key/value tuple elements",
            ));
        };
        let Some((key_kind, val_kind)) = self.hashmap_storage_kinds(*key_ty, *val_ty) else {
            return Err(BuildError::InternalLoweringBug(
                "HashMap::from native lowering only supports i64/String keys and values",
            ));
        };
        let count = i64::try_from(len.to_usize()).unwrap_or(i64::MAX);
        declare_rt(&mut self.runtime_refs, "gos_rt_map_new_with_capacity_typed");
        let map = self.fresh();
        writeln!(
            self.out,
            "  {map} = call ptr @gos_rt_map_new_with_capacity_typed(i32 {key_kind}, i32 {val_kind}, i64 {count})"
        )
        .unwrap();
        if self.hashmap_value_is_vec(*val_ty) {
            declare_rt(&mut self.runtime_refs, "gos_rt_map_set_vec_values");
            writeln!(
                self.out,
                "  call void @gos_rt_map_set_vec_values(ptr {map})"
            )
            .unwrap();
        }
        let base = match arg {
            Operand::Copy(place) => self.lower_place_address(place),
            Operand::Const(_) | Operand::FnRef { .. } => {
                return Err(BuildError::InternalLoweringBug(
                    "HashMap::from native lowering expects a materialised array place",
                ));
            }
        };
        let stride = slot_count(self.tcx, elem).unwrap_or(2).max(1);
        let value_offset = slot_count(self.tcx, *key_ty).unwrap_or(1).max(1);
        for index in 0..len.to_usize() {
            let offset = u64::try_from(index)
                .unwrap_or(u64::MAX)
                .saturating_mul(u64::from(stride));
            let key = self.load_hashmap_from_slot(&base, offset, *key_ty)?;
            let val = self.load_hashmap_from_slot(
                &base,
                offset.saturating_add(u64::from(value_offset)),
                *val_ty,
            )?;
            let helper = match (key.llvm_ty, val.llvm_ty) {
                ("i64", "i64") => "gos_rt_map_insert_i64_i64",
                ("i64", "ptr") => "gos_rt_map_insert_i64_str",
                ("ptr", "i64") => "gos_rt_map_insert_str_i64",
                ("ptr", "ptr") => "gos_rt_map_insert_str_str",
                _ => {
                    return Err(BuildError::InternalLoweringBug(
                        "HashMap::from native lowering produced unsupported key/value ABI",
                    ));
                }
            };
            declare_rt(&mut self.runtime_refs, helper);
            writeln!(
                self.out,
                "  call void @\"{helper}\"(ptr {map}, {} {}, {} {})",
                key.llvm_ty, key.value, val.llvm_ty, val.value
            )
            .unwrap();
        }
        self.store_value_to_place(destination, "ptr", &map);
        emit_terminator_branch(&mut self.out, target);
        Ok(())
    }

    fn hashmap_storage_kinds(&self, key: Ty, value: Ty) -> Option<(i32, i32)> {
        // A key the runtime hashes by CONTENT - a container, an aggregate -
        // has no pre-typed storage: its first insert installs the
        // content-keyed table. Pre-typing the map as handle-keyed would leave
        // that insert with nowhere to go.
        if matches!(
            self.tcx.kind(self.unwrap_ref(key)),
            Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::HashMap { .. })
        ) {
            return None;
        }
        Some((
            self.hashmap_storage_kind(key)?,
            self.hashmap_storage_kind(value)?,
        ))
    }

    fn hashmap_storage_kind(&self, ty: Ty) -> Option<i32> {
        match self.tcx.kind(self.unwrap_ref(ty)) {
            // Every scalar occupies the same one word, and the runtime keeps
            // it as the bits the slot holds, so a float, a bool, a char, and
            // a narrow integer share the word-storage kind an `i64` names.
            Some(
                TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::Duration
                | TyKind::Instant,
            ) => Some(0),
            Some(TyKind::String) => Some(1),
            Some(TyKind::Vec(elem) | TyKind::Slice(elem))
                if matches!(
                    self.tcx.kind(*elem),
                    Some(TyKind::Int(gossamer_types::IntTy::U8))
                ) =>
            {
                Some(2)
            }
            // Any other container rides as the handle word its
            // constructor answered, the way `insert` stores one.
            Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::HashMap { .. }) => Some(0),
            _ => None,
        }
    }

    /// True when a map value is a `Vec` handle, so the map owns one Vec
    /// share per entry and releases it when an entry or the map dies.
    fn hashmap_value_is_vec(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(self.unwrap_ref(ty)),
            Some(TyKind::Vec(_) | TyKind::Slice(_))
        )
    }

    fn load_hashmap_from_slot(
        &mut self,
        base: &str,
        offset: u64,
        ty: Ty,
    ) -> Result<LoweredMapSlot, BuildError> {
        let slot = self.fresh();
        writeln!(
            self.out,
            "  {slot} = getelementptr i64, ptr {base}, i64 {offset}"
        )
        .unwrap();
        match self.tcx.kind(self.unwrap_ref(ty)) {
            // Read every scalar as the word its slot holds: the runtime keys
            // and stores it by those bits, and reading a float as a double
            // here would hand the insert a converted value instead.
            Some(
                TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::Duration
                | TyKind::Instant,
            ) => {
                let value = self.fresh();
                writeln!(self.out, "  {value} = load i64, ptr {slot}").unwrap();
                Ok(LoweredMapSlot {
                    llvm_ty: "i64",
                    value,
                })
            }
            Some(TyKind::String) => {
                let value = self.fresh();
                writeln!(self.out, "  {value} = load ptr, ptr {slot}").unwrap();
                Ok(LoweredMapSlot {
                    llvm_ty: "ptr",
                    value,
                })
            }
            Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::HashMap { .. }) => {
                let value = self.fresh();
                writeln!(self.out, "  {value} = load i64, ptr {slot}").unwrap();
                Ok(LoweredMapSlot {
                    llvm_ty: "i64",
                    value,
                })
            }
            _ => Err(BuildError::InternalLoweringBug(
                "HashMap::from native lowering cannot load key/value type",
            )),
        }
    }

    /// Reads the heap pointer value addressed by an operand
    /// passed as the `ptr` argument of `gos_load`/`gos_store`.
    /// Differs from the generic `lower_operand` because
    /// aggregate-typed locals (Adt / Tuple / Array) are stored
    /// as a heap pointer in their slot - `lower_place_read`
    /// returns the slot's *address* for those, but the raw
    /// heap intrinsics need the *contents* (the heap pointer
    /// the slot stores). Without this distinction the
    /// `getelementptr` walks from the local's stack alloca
    /// instead of the heap blob, corrupting the slot.
    pub(crate) fn lower_raw_ptr_arg(&mut self, op: &Operand) -> Result<String, BuildError> {
        if let Operand::Copy(place) = op
            && place.projection.is_empty()
            && is_aggregate(self.tcx, self.body.local_ty(place.local))
        {
            let slot = local_slot(place.local);
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = load ptr, ptr {slot}").unwrap();
            return Ok(tmp);
        }
        let raw = self.lower_operand(op)?;
        let raw_ty = self.operand_llvm_ty(op);
        if raw_ty == "ptr" {
            return Ok(raw);
        }
        let tmp = self.fresh();
        writeln!(self.out, "  {tmp} = inttoptr {raw_ty} {raw} to ptr").unwrap();
        Ok(tmp)
    }

    /// Lowers an argument operand for a regular fn call.
    ///
    /// Three storage shapes co-exist:
    ///
    /// 1. Callee expects a one-word heap-pointer aggregate (`slot_count`
    ///    is `None`: a recursive or tagged enum, `http::Response`, an
    ///    opaque blob handle): load the word its slot holds and pass that.
    ///    `emit_param_stores` binds such a parameter as the word.
    ///
    /// 2. Callee expects `Ref<T>`, caller has an enum-Adt local
    ///    (slot_count = None): the slot holds a GC heap pointer.
    ///    Load it and pass it directly so the callee GEPs into the
    ///    heap data without an extra indirection.
    ///
    /// 3. Callee expects a flat-slot aggregate parameter (Array, Tuple,
    ///    or a field-bearing Adt): pass the alloca ADDRESS.
    ///    `emit_param_stores` in the callee does
    ///    `memcpy(callee_slot, arg_ptr, bytes)` over the field data.
    ///
    /// 4. All other args: `lower_operand` → `lower_place_read`.
    ///
    /// The shape belongs to the parameter, not to the kind of body being
    /// called: a lifted closure a runtime combinator shim invokes through a
    /// C function pointer, a C-ABI handler, and a direct call all receive a
    /// parameter in the same shape, which is what lets one predicate
    /// (`param_is_by_pointer`) bind every callee.
    pub(crate) fn lower_call_arg(
        &mut self,
        op: &Operand,
        expected: Option<Ty>,
    ) -> Result<(String, String), BuildError> {
        if let Some(want) = expected
            && let Operand::Copy(place) = op
            && place.projection.is_empty()
        {
            let local_ty = self.body.local_ty(place.local);
            if is_aggregate(self.tcx, want) && slot_count(self.tcx, want).is_none() {
                // The callee binds a one-word heap-pointer aggregate as the
                // word its slot holds, so hand over that word rather than the
                // address of the slot holding it.
                let slot = local_slot(place.local);
                let tmp = self.fresh();
                writeln!(self.out, "  {tmp} = load ptr, ptr {slot}").unwrap();
                return Ok((tmp, "ptr".to_string()));
            }
            if matches!(self.tcx.kind(want), Some(TyKind::Ref { .. })) {
                // Case 1: &T param, enum-Adt slot → load heap ptr.
                if matches!(self.tcx.kind(local_ty), Some(TyKind::Adt { .. }))
                    && slot_count(self.tcx, local_ty).is_none()
                {
                    let slot = local_slot(place.local);
                    let tmp = self.fresh();
                    writeln!(self.out, "  {tmp} = load ptr, ptr {slot}").unwrap();
                    return Ok((tmp, "ptr".to_string()));
                }
            } else if is_aggregate(self.tcx, want) && is_aggregate(self.tcx, local_ty) {
                // Case 2: aggregate param → pass alloca address so
                // the callee's memcpy copies the right bytes.
                return Ok((local_slot(place.local), "ptr".to_string()));
            } else if is_aggregate(self.tcx, want)
                && slot_count(self.tcx, want) == Some(1)
                && matches!(
                    self.tcx.kind(local_ty),
                    Some(TyKind::Var(_) | TyKind::Error | TyKind::Int(_)) | None
                )
            {
                // Case 2b: the callee declares a one-slot flat aggregate
                // param (a bare-discriminant enum, a one-field struct),
                // but this call site's arg local is a one-word scalar
                // - either inference left it untyped (method-call arg
                // temporaries) or a unit-only enum lowered its value
                // to a bare `i64` discriminant. The callee memcpys 8
                // bytes from the arg address, so pass the slot address;
                // passing the loaded VALUE instead makes the callee
                // dereference the bits (read garbage or fault). A
                // heap-pointer param (`slot_count` `None`) takes the word
                // itself and is answered above.
                return Ok((local_slot(place.local), "ptr".to_string()));
            }
        }
        let v = self.lower_operand(op)?;
        let ty = self.operand_llvm_ty(op);
        Ok((v, ty))
    }

    /// Reads the value being stored by a raw heap intrinsic
    /// (`gos_store`'s third arg). Aggregate-typed locals hold
    /// the heap pointer in their slot - when one flows in as
    /// the value, return the *contents* (load ptr from slot)
    /// instead of the slot address. Returns `(value, llvm_ty)`.
    pub(crate) fn lower_raw_value_arg(
        &mut self,
        op: &Operand,
    ) -> Result<(String, String), BuildError> {
        if let Operand::Copy(place) = op
            && place.projection.is_empty()
            && is_aggregate(self.tcx, self.body.local_ty(place.local))
        {
            let local_ty = self.body.local_ty(place.local);
            let slot = local_slot(place.local);
            // A multi-slot inline aggregate (struct / tuple / array with
            // `slot_count == Some(n > 1)`) is address-is-value. A closure env
            // stores one word per capture and its body reads the capture back
            // as a pointer it dereferences (see `gos_load`), so copy the
            // aggregate to a stable heap box and store that pointer. Storing
            // the slot's first word (the old behaviour) made the body read the
            // capture as the pointer and every field past the first as
            // uninitialised slot memory; a stack slot address would dangle once
            // the capturing frame returns. Single-slot aggregates and
            // heap-pointer aggregates (enum / `Box`, `slot_count == None`)
            // already hold their one word inline / as a heap pointer: load it.
            if let Some(n) = slot_count(self.tcx, local_ty).filter(|&n| n > 1) {
                let bytes = n * 8;
                let boxed = self.fresh();
                writeln!(self.out, "  {boxed} = call ptr @malloc(i64 {bytes})").unwrap();
                writeln!(
                    self.out,
                    "  call void @llvm.memcpy.p0.p0.i64(ptr {boxed}, ptr {slot}, i64 {bytes}, i1 false)"
                )
                .unwrap();
                return Ok((boxed, "ptr".to_string()));
            }
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = load ptr, ptr {slot}").unwrap();
            return Ok((tmp, "ptr".to_string()));
        }
        let v = self.lower_operand(op)?;
        let ty = self.operand_llvm_ty(op);
        Ok((v, ty))
    }

    /// Lowers the raw-pointer intrinsics (`gos_load`,
    /// `gos_store`, `gos_alloc`, `gos_fn_addr`) directly to
    /// LLVM IR so the LLVM tier doesn't have to fall back to
    /// cranelift for closure envs / vec iteration / fn-pointer
    /// trampolines. Mirrors the cranelift-side handlers in
    /// `lower_intrinsic_outcome`.
    pub(crate) fn lower_raw_intrinsic(
        &mut self,
        name: &str,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        let Some(intrinsic) = RawIntrinsic::from_name(name) else {
            return Err(BuildError::InternalLoweringBug(
                "unrecognised raw intrinsic",
            ));
        };
        if !intrinsic.arity_for_name(name).accepts(args.len()) {
            return Err(BuildError::InternalLoweringBug("raw intrinsic arity"));
        }
        let dest_ty_mir = self.place_leaf_ty(destination);
        let dest_ty = render_ty(self.tcx, dest_ty_mir);
        match name {
            "gos_enum_load" => {
                // gos_enum_load(ptr, off) -> i64 at (ptr & !7) + off.
                // Enum payload read: the mask strips a tagged repr's disc
                // bits and is a no-op for aligned header-repr pointers.
                if args.len() < 2 {
                    return Err(BuildError::InternalLoweringBug("gos_enum_load arity"));
                }
                let pv = self.lower_operand(&args[0])?;
                let p_ty = self.operand_llvm_ty(&args[0]);
                let p64 = self.coerce_llvm_value(&pv, &p_ty, "i64");
                let off_v = self.lower_operand(&args[1])?;
                let off_ty = self.operand_llvm_ty(&args[1]);
                let off64 = self.coerce_llvm_value(&off_v, &off_ty, "i64");
                let m = self.fresh();
                writeln!(self.out, "  {m} = and i64 {p64}, -8").unwrap();
                // Unit variants of tagged enums are TAGGED NULLS (base
                // zero, no object): a payload load on one yields 0
                // instead of dereferencing address zero. Perfectly
                // predicted on payload-bearing values.
                let is_null = self.fresh();
                writeln!(self.out, "  {is_null} = icmp eq i64 {m}, 0").unwrap();
                let entry_l = self.fresh_label("enum_load_entry");
                let load_l = self.fresh_label("enum_load");
                let done_l = self.fresh_label("enum_load_done");
                writeln!(self.out, "  br label %{entry_l}").unwrap();
                writeln!(self.out, "{entry_l}:").unwrap();
                writeln!(
                    self.out,
                    "  br i1 {is_null}, label %{done_l}, label %{load_l}"
                )
                .unwrap();
                writeln!(self.out, "{load_l}:").unwrap();
                let mp = self.fresh();
                writeln!(self.out, "  {mp} = inttoptr i64 {m} to ptr").unwrap();
                let addr = self.fresh();
                writeln!(
                    self.out,
                    "  {addr} = getelementptr i8, ptr {mp}, i64 {off64}"
                )
                .unwrap();
                let lv = self.fresh();
                // A two-word `Option` / `Result` / inline-enum field occupies
                // both of its words in the node, so the read takes its own
                // width - a word-sized load keeps only the discriminant.
                let load_ty = if dest_ty == "i128" { "i128" } else { "i64" };
                // Payload memory: `gos_enum_load` is emitted only for a match
                // arm reading a variant payload word out of an enum node, and
                // a node's words are a flat slot slab - the same class the
                // projection walk tags, never a `GosVec` / string header word.
                writeln!(self.out, "  {lv} = load {load_ty}, ptr {addr}{TBAA_DATA}").unwrap();
                writeln!(self.out, "  br label %{done_l}").unwrap();
                writeln!(self.out, "{done_l}:").unwrap();
                let v = self.fresh();
                writeln!(
                    self.out,
                    "  {v} = phi {load_ty} [ 0, %{entry_l} ], [ {lv}, %{load_l} ]"
                )
                .unwrap();
                // A multi-slot aggregate payload (struct / tuple / array > 1
                // word) is stored as a POINTER to a heap-boxed copy. The
                // loaded word is that box pointer: materialise the binding by
                // value (memcpy the box's flat slots into the destination
                // alloca) and retain the box's RC children so the binding's
                // scope-end teardown release stays balanced. A null box (a
                // unit variant with no fields) is skipped - those have no
                // aggregate to read.
                let dest_slots = if is_aggregate(self.tcx, dest_ty_mir) {
                    slot_count(self.tcx, dest_ty_mir).filter(|&n| n > 1)
                } else {
                    None
                };
                if let Some(n) = dest_slots {
                    let bytes = u64::from(n) * 8;
                    let slot = if destination.projection.is_empty() {
                        local_slot(destination.local)
                    } else {
                        self.lower_place_address(destination)
                    };
                    let boxp = self.fresh();
                    writeln!(self.out, "  {boxp} = inttoptr i64 {v} to ptr").unwrap();
                    let nonnull = self.fresh();
                    writeln!(self.out, "  {nonnull} = icmp ne i64 {v}, 0").unwrap();
                    let copy_l = self.fresh_label("enum_aggr_copy");
                    let after_l = self.fresh_label("enum_aggr_done");
                    writeln!(
                        self.out,
                        "  br i1 {nonnull}, label %{copy_l}, label %{after_l}"
                    )
                    .unwrap();
                    writeln!(self.out, "{copy_l}:").unwrap();
                    writeln!(
                        self.out,
                        "  call void @llvm.memcpy.p0.p0.i64(ptr {slot}, ptr {boxp}, i64 {bytes}, i1 false)"
                    )
                    .unwrap();
                    declare_rt(&mut self.runtime_refs, "gos_rt_rc_retain_children");
                    writeln!(
                        self.out,
                        "  call void @gos_rt_rc_retain_children(ptr {boxp})"
                    )
                    .unwrap();
                    writeln!(self.out, "  br label %{after_l}").unwrap();
                    writeln!(self.out, "{after_l}:").unwrap();
                    return Ok(());
                }
                // Bit-preserving recovery, exactly as `gos_load`: float
                // payloads were stored as raw bits and must bitcast back.
                let coerced = if dest_ty == "double" || dest_ty == "float" {
                    let tmp = self.fresh();
                    writeln!(self.out, "  {tmp} = bitcast i64 {v} to {dest_ty}").unwrap();
                    tmp
                } else {
                    self.coerce_llvm_value(&v, load_ty, &dest_ty)
                };
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            "gos_enum_tag" => {
                // gos_enum_tag(ptr, disc) -> ptr | (disc << 1). Tagged-repr
                // enums (<= 4 variants) carry the discriminant in pointer
                // bits 1-2; bit 0 stays 0 (odd pointers are string bodies).
                if args.len() < 2 {
                    return Err(BuildError::InternalLoweringBug("gos_enum_tag arity"));
                }
                let pv = self.lower_operand(&args[0])?;
                let p_ty = self.operand_llvm_ty(&args[0]);
                let p64 = self.coerce_llvm_value(&pv, &p_ty, "i64");
                let d = self.lower_operand(&args[1])?;
                let d_ty = self.operand_llvm_ty(&args[1]);
                let d64 = self.coerce_llvm_value(&d, &d_ty, "i64");
                let sh = self.fresh();
                writeln!(self.out, "  {sh} = shl i64 {d64}, 1").unwrap();
                let or = self.fresh();
                writeln!(self.out, "  {or} = or i64 {p64}, {sh}").unwrap();
                let coerced = self.coerce_llvm_value(&or, "i64", &dest_ty);
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            "gos_enum_disc_tag" => {
                // gos_enum_disc_tag(ptr) -> (ptr >> 1) & 3.
                if args.is_empty() {
                    return Err(BuildError::InternalLoweringBug("gos_enum_disc_tag arity"));
                }
                let pv = self.lower_operand(&args[0])?;
                let p_ty = self.operand_llvm_ty(&args[0]);
                let p64 = self.coerce_llvm_value(&pv, &p_ty, "i64");
                let sh = self.fresh();
                writeln!(self.out, "  {sh} = lshr i64 {p64}, 1").unwrap();
                let m = self.fresh();
                writeln!(self.out, "  {m} = and i64 {sh}, 3").unwrap();
                let coerced = self.coerce_llvm_value(&m, "i64", &dest_ty);
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            "gos_enum_untag" => {
                // gos_enum_untag(ptr) -> ptr & !7 (payload base).
                if args.is_empty() {
                    return Err(BuildError::InternalLoweringBug("gos_enum_untag arity"));
                }
                let pv = self.lower_operand(&args[0])?;
                let p_ty = self.operand_llvm_ty(&args[0]);
                let p64 = self.coerce_llvm_value(&pv, &p_ty, "i64");
                let m = self.fresh();
                writeln!(self.out, "  {m} = and i64 {p64}, -8").unwrap();
                let coerced = self.coerce_llvm_value(&m, "i64", &dest_ty);
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            "gos_enum_disc" => {
                // gos_enum_disc(payload_ptr) -> i64. The discriminant is
                // the byte at payload-3 (inside the RC header: strong u32,
                // weak u8, disc u8, meta_id u16).
                if args.is_empty() {
                    return Err(BuildError::InternalLoweringBug("gos_enum_disc arity"));
                }
                let p = self.lower_raw_ptr_arg(&args[0])?;
                // A payload that is not there - the `None` half of an
                // `Option`, an unset carrier - has no header to read, so the
                // load is steered at a zero byte and answers a discriminant
                // no variant carries.
                self.runtime_refs.insert(format!(
                    "@{ENUM_DISC_NULL_PAD} = internal constant [8 x i8] zeroinitializer"
                ));
                let is_null = self.fresh();
                writeln!(self.out, "  {is_null} = icmp eq ptr {p}, null").unwrap();
                let header = self.fresh();
                writeln!(self.out, "  {header} = getelementptr i8, ptr {p}, i64 -3").unwrap();
                let addr = self.fresh();
                writeln!(
                    self.out,
                    "  {addr} = select i1 {is_null}, ptr @{ENUM_DISC_NULL_PAD}, ptr {header}"
                )
                .unwrap();
                let b = self.fresh();
                writeln!(self.out, "  {b} = load i8, ptr {addr}").unwrap();
                let raw = self.fresh();
                writeln!(self.out, "  {raw} = zext i8 {b} to i64").unwrap();
                let v = self.fresh();
                writeln!(self.out, "  {v} = select i1 {is_null}, i64 -1, i64 {raw}").unwrap();
                let coerced = self.coerce_llvm_value(&v, "i64", &dest_ty);
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            "gos_enum_set_disc" => {
                // gos_enum_set_disc(payload_ptr, disc_i64): store the low
                // byte of the discriminant at payload-3.
                if args.len() < 2 {
                    return Err(BuildError::InternalLoweringBug("gos_enum_set_disc arity"));
                }
                let p = self.lower_raw_ptr_arg(&args[0])?;
                let d = self.lower_operand(&args[1])?;
                let d_ty = self.operand_llvm_ty(&args[1]);
                let d64 = self.coerce_llvm_value(&d, &d_ty, "i64");
                let b = self.fresh();
                writeln!(self.out, "  {b} = trunc i64 {d64} to i8").unwrap();
                let addr = self.fresh();
                writeln!(self.out, "  {addr} = getelementptr i8, ptr {p}, i64 -3").unwrap();
                writeln!(self.out, "  store i8 {b}, ptr {addr}").unwrap();
            }
            "gos_load" => {
                // gos_load(ptr_i64, offset_i64) -> i64
                if args.len() < 2 {
                    return Err(BuildError::InternalLoweringBug("gos_load arity"));
                }
                let p = self.lower_raw_ptr_arg(&args[0])?;
                let off_v = self.lower_operand(&args[1])?;
                let off_ty = self.operand_llvm_ty(&args[1]);
                // gep i8, p, off → addr
                // `sext` is integer-only; use the type-aware
                // coercion so a `double` offset (closure capture
                // routed through gos_load) converts via `fptosi`
                // rather than the malformed `sext double to i64`
                // shape that `opt`'s verifier rejects.
                let off64 = self.coerce_llvm_value(&off_v, &off_ty, "i64");
                let addr = self.fresh();
                writeln!(
                    self.out,
                    "  {addr} = getelementptr i8, ptr {p}, i64 {off64}"
                )
                .unwrap();
                if crate::emit::want_race_instrumentation() {
                    declare_rt(&mut self.runtime_refs, "gos_rt_race_access");
                    let addr_int = self.fresh();
                    writeln!(self.out, "  {addr_int} = ptrtoint ptr {addr} to i64").unwrap();
                    writeln!(
                        self.out,
                        "  call void @gos_rt_race_access(i64 {addr_int}, i32 0)"
                    )
                    .unwrap();
                }
                let loaded = self.fresh();
                writeln!(self.out, "  {loaded} = load i64, ptr {addr}").unwrap();
                // An inline-aggregate capture (struct / tuple / array) is held
                // in the env as a pointer to its data (see `lower_raw_value_arg`).
                // Materialise the destination by copying the aggregate's bytes
                // out of that pointer - storing the bare pointer word would
                // leave every field past the first reading uninitialised slot
                // memory.
                if is_aggregate(self.tcx, dest_ty_mir)
                    && let Some(n) = slot_count(self.tcx, dest_ty_mir).filter(|&n| n > 1)
                {
                    let src = self.fresh();
                    writeln!(self.out, "  {src} = inttoptr i64 {loaded} to ptr").unwrap();
                    let slot = if destination.projection.is_empty() {
                        local_slot(destination.local)
                    } else {
                        self.lower_place_address(destination)
                    };
                    writeln!(
                        self.out,
                        "  call void @llvm.memcpy.p0.p0.i64(ptr {slot}, ptr {src}, i64 {}, i1 false)",
                        n * 8
                    )
                    .unwrap();
                    if target.is_some() {
                        emit_terminator_branch(&mut self.out, target);
                    }
                    return Ok(());
                }
                // Heap-load value recovery is bit-preserving:
                // a `double` capture stored via `gos_store` (which
                // bitcasts the float bits into the i64 slot)
                // must be read back via `bitcast i64 to double`,
                // not `sitofp` (which would interpret the bits
                // as an integer value). Mirrors the gos_store
                // path above.
                let coerced = if dest_ty == "double" || dest_ty == "float" {
                    let tmp = self.fresh();
                    writeln!(self.out, "  {tmp} = bitcast i64 {loaded} to {dest_ty}").unwrap();
                    tmp
                } else {
                    self.coerce_llvm_value(&loaded, "i64", &dest_ty)
                };
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            "gos_store" | "gos_store_i128" => {
                // gos_store(ptr, offset, value) writes 8 bytes;
                // gos_store_i128 writes both words of a two-word carrier.
                if args.len() < 3 {
                    return Err(BuildError::InternalLoweringBug("gos_store arity"));
                }
                let p = self.lower_raw_ptr_arg(&args[0])?;
                let off_v = self.lower_operand(&args[1])?;
                let off_ty = self.operand_llvm_ty(&args[1]);
                // The value being stored may itself be an
                // aggregate-typed Copy whose slot holds the
                // heap pointer (the recursive-enum case where
                // a `Box<List>` rest field lives in another
                // local's slot). Use the same heap-pointer
                // resolution as the ptr arg so we store the
                // heap pointer rather than the slot address.
                let (val_v, val_ty) = self.lower_raw_value_arg(&args[2])?;
                // `sext` is integer-only; use the type-aware
                // coercion so a `double` offset (closure capture
                // routed through gos_load) converts via `fptosi`
                // rather than the malformed `sext double to i64`
                // shape that `opt`'s verifier rejects.
                let off64 = self.coerce_llvm_value(&off_v, &off_ty, "i64");
                // Heap-store value-coercion is bit-preserving:
                // a `double` capture stored into an `i64` heap
                // slot must keep its IEEE-754 bit pattern intact
                // (so the matching `gos_load` can read it back
                // via `bitcast i64 to double`). `coerce_llvm_value`
                // uses `fptosi` for value-semantic conversions -
                // wrong here: `fptosi(0.5)` is `0`, losing the
                // capture. Emit `bitcast` explicitly for the
                // float-to-i64 store path.
                // The wide store is `gos_store_i128`; this one writes the word
                // every other slot in the language is.
                let store_ty = if name == "gos_store_i128" { "i128" } else { "i64" };
                let val64 = if val_ty == "double" || val_ty == "float" {
                    let tmp = self.fresh();
                    writeln!(self.out, "  {tmp} = bitcast {val_ty} {val_v} to i64").unwrap();
                    tmp
                } else {
                    self.coerce_llvm_value(&val_v, &val_ty, store_ty)
                };
                let addr = self.fresh();
                writeln!(
                    self.out,
                    "  {addr} = getelementptr i8, ptr {p}, i64 {off64}"
                )
                .unwrap();
                if crate::emit::want_race_instrumentation() {
                    declare_rt(&mut self.runtime_refs, "gos_rt_race_access");
                    let addr_int = self.fresh();
                    writeln!(self.out, "  {addr_int} = ptrtoint ptr {addr} to i64").unwrap();
                    writeln!(
                        self.out,
                        "  call void @gos_rt_race_access(i64 {addr_int}, i32 1)"
                    )
                    .unwrap();
                }
                writeln!(self.out, "  store {store_ty} {val64}, ptr {addr}").unwrap();
                if dest_ty != "void" && !is_unit(self.tcx, dest_ty_mir) {
                    // Sink stores: pick a zero-shaped literal that
                    // matches the destination's LLVM type so `opt`
                    // doesn't reject `store ptr 0` / `store double 0`.
                    self.store_zero_to_place(destination, &dest_ty);
                }
            }
            "gos_alloc" => {
                // gos_alloc(size_i64) -> ptr (via libc malloc).
                let size_v = if args.is_empty() {
                    "0".to_string()
                } else {
                    let v = self.lower_operand(&args[0])?;
                    let t = self.operand_llvm_ty(&args[0]);
                    if t == "i64" {
                        v
                    } else {
                        let tmp = self.fresh();
                        writeln!(self.out, "  {tmp} = sext {t} {v} to i64").unwrap();
                        tmp
                    }
                };
                let tmp = self.fresh();
                writeln!(self.out, "  {tmp} = call ptr @malloc(i64 {size_v})").unwrap();
                let coerced = self.coerce_llvm_value(&tmp, "ptr", &dest_ty);
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            "gos_rc_alloc_reuse" => {
                // gos_rc_alloc_reuse(token_ptr, size_i64, meta_symbol) -> ptr.
                // Perceus reuse: re-home `token` (a block from
                // gos_rt_rc_drop_reuse) into a fresh strong-1 object, or
                // allocate fresh when the token is null. Same meta-symbol
                // handling as `gos_rc_alloc`, with the recycled block as the
                // leading argument.
                let token = self.lower_operand(&args[0])?;
                let size_v = {
                    let v = self.lower_operand(&args[1])?;
                    let t = self.operand_llvm_ty(&args[1]);
                    if t == "i64" {
                        v
                    } else {
                        let tmp = self.fresh();
                        writeln!(self.out, "  {tmp} = sext {t} {v} to i64").unwrap();
                        tmp
                    }
                };
                let meta_ptr = match args.get(2) {
                    Some(Operand::Const(ConstValue::Str(sym))) if !sym.is_empty() => {
                        format!("@\"{sym}\"")
                    }
                    _ => "null".to_string(),
                };
                declare_rt(&mut self.runtime_refs, "gos_rt_rc_alloc_reuse");
                let tmp = self.fresh();
                writeln!(
                    self.out,
                    "  {tmp} = call ptr @gos_rt_rc_alloc_reuse(ptr {token}, i64 {size_v}, ptr {meta_ptr})"
                )
                .unwrap();
                let coerced = self.coerce_llvm_value(&tmp, "ptr", &dest_ty);
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            "gos_rt_enum_struct_eq" => {
                // gos_rt_enum_struct_eq(a_ptr, b_ptr, desc_symbol) -> i64. The
                // two enum node pointers lower normally; the third arg is a
                // const-string naming the module-global structural-eq
                // descriptor blob (a registered rc_meta), emitted as its
                // address like the rc_alloc meta symbol.
                let a = self.lower_raw_ptr_arg(&args[0])?;
                let b = self.lower_raw_ptr_arg(&args[1])?;
                let desc_ptr = match args.get(2) {
                    Some(Operand::Const(ConstValue::Str(sym))) if !sym.is_empty() => {
                        format!("@\"{sym}\"")
                    }
                    _ => "null".to_string(),
                };
                declare_rt(&mut self.runtime_refs, "gos_rt_enum_struct_eq");
                let tmp = self.fresh();
                writeln!(
                    self.out,
                    "  {tmp} = call i64 @gos_rt_enum_struct_eq(ptr {a}, ptr {b}, ptr {desc_ptr})"
                )
                .unwrap();
                let coerced = self.coerce_llvm_value(&tmp, "i64", &dest_ty);
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            // The enum-keyed map helpers all take `(map, key_node,
            // desc_symbol, [word])`. The third argument names the same
            // module-global structural descriptor blob `gos_rt_enum_struct_eq`
            // walks, so it lowers as that global's address rather than as a
            // string constant.
            "gos_rt_map_insert_ekey_opt"
            | "gos_rt_map_get_ekey_opt"
            | "gos_rt_map_contains_ekey"
            | "gos_rt_map_pop_ekey"
            | "gos_rt_map_get_or_ekey"
            | "gos_rt_map_or_insert_ekey"
            | "gos_rt_map_inc_ekey"
            // A set of enum elements keys by the same descriptor, so its
            // `(set, node, desc)` calls lower the same way.
            | "gos_rt_set_insert_ekey"
            | "gos_rt_set_contains_ekey"
            | "gos_rt_set_remove_ekey" => {
                let (ret_ty, has_word) = match name {
                    "gos_rt_map_insert_ekey_opt" => ("i128", true),
                    "gos_rt_map_get_ekey_opt" | "gos_rt_map_pop_ekey" => ("i128", false),
                    "gos_rt_map_contains_ekey" => ("i8", false),
                    "gos_rt_set_insert_ekey"
                    | "gos_rt_set_contains_ekey"
                    | "gos_rt_set_remove_ekey" => ("i64", false),
                    _ => ("i64", true),
                };
                let map = self.lower_raw_ptr_arg(&args[0])?;
                let key = self.lower_raw_ptr_arg(&args[1])?;
                let desc_ptr = match args.get(2) {
                    Some(Operand::Const(ConstValue::Str(sym))) if !sym.is_empty() => {
                        format!("@\"{sym}\"")
                    }
                    _ => "null".to_string(),
                };
                let word = if has_word {
                    match args.get(3) {
                        Some(operand) => {
                            let v = self.lower_operand(operand)?;
                            let t = self.operand_llvm_ty(operand);
                            Some(self.coerce_llvm_value(&v, &t, "i64"))
                        }
                        None => Some("0".to_string()),
                    }
                } else {
                    None
                };
                declare_rt(&mut self.runtime_refs, name);
                let tail = match &word {
                    Some(w) => format!(", i64 {w}"),
                    None => String::new(),
                };
                let tmp = self.fresh();
                writeln!(
                    self.out,
                    "  {tmp} = call {ret_ty} @{name}(ptr {map}, ptr {key}, ptr {desc_ptr}{tail})"
                )
                .unwrap();
                let coerced = self.coerce_llvm_value(&tmp, ret_ty, &dest_ty);
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            "gos_rc_alloc" | "gos_rc_alloc_tagged" => {
                // gos_rc_alloc(size_i64, meta_symbol) -> ptr to a
                // reference-counted heap object (strong count 1). The
                // second arg is a const-string naming the module-global
                // child-layout meta blob; an empty name means a leaf
                // (null meta). The blob globals are emitted alongside
                // the string pool (see emit.rs).
                let size_v = if args.is_empty() {
                    "0".to_string()
                } else {
                    let v = self.lower_operand(&args[0])?;
                    let t = self.operand_llvm_ty(&args[0]);
                    if t == "i64" {
                        v
                    } else {
                        let tmp = self.fresh();
                        writeln!(self.out, "  {tmp} = sext {t} {v} to i64").unwrap();
                        tmp
                    }
                };
                let meta_ptr = match args.get(1) {
                    Some(Operand::Const(ConstValue::Str(sym))) if !sym.is_empty() => {
                        format!("@\"{sym}\"")
                    }
                    _ => "null".to_string(),
                };
                let rt_name = if name == "gos_rc_alloc_tagged" {
                    "gos_rt_rc_alloc_tagged"
                } else {
                    "gos_rt_rc_alloc"
                };
                declare_rt(&mut self.runtime_refs, rt_name);
                let tmp = self.fresh();
                writeln!(
                    self.out,
                    "  {tmp} = call ptr @{rt_name}(i64 {size_v}, ptr {meta_ptr})"
                )
                .unwrap();
                let coerced = self.coerce_llvm_value(&tmp, "ptr", &dest_ty);
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            "gos_fn_addr" => {
                // gos_fn_addr("name") -> ptr to that function.
                let Some(Operand::Const(ConstValue::Str(fname))) = args.first() else {
                    return Err(BuildError::InternalLoweringBug("gos_fn_addr arg"));
                };
                // LLVM IR pointer-to-function constants are written
                // as the function symbol itself; declare-only is OK
                // because the cranelift companion (or another LLVM
                // body) provides the definition. When `fname` is a
                // runtime symbol (`gos_rt_*`), ensure the matching
                // `declare` lands in the module - otherwise opt
                // rejects `bitcast ptr @gos_rt_<name>` with "use of
                // undefined value".
                if fname.starts_with("gos_rt_") && gossamer_abi::lookup(fname).is_some() {
                    declare_rt(&mut self.runtime_refs, fname);
                }
                // Win64: a handler invoked by the rustc-compiled runtime
                // through `extern "C" fn(..) -> i128` must return the
                // 2-word `i128` in a vector register (xmm0), but `@fname`
                // (a gossamer `ret i128`) returns it in the GP-register
                // pair. Take the address of the synthesized `<16 x i8>`
                // C-ABI return thunk (`name$cabi`) instead so the runtime
                // reads the discriminant/payload from the register it expects.
                let sym = if crate::emit::target_is_windows()
                    && self.cabi_handlers.contains_key(fname.as_str())
                {
                    format!("{fname}$cabi")
                } else {
                    // Same mangling as the definition: a user function
                    // address (`PgDriver::dispatch`) is taken as
                    // `gosu.<name>`; `gos_rt_*` runtime symbols pass through.
                    mangle_fn_name(fname).into_owned()
                };
                let tmp = self.fresh();
                writeln!(self.out, "  {tmp} = bitcast ptr @\"{sym}\" to ptr").unwrap();
                let coerced = self.coerce_llvm_value(&tmp, "ptr", &dest_ty);
                self.store_value_to_place(destination, &dest_ty, &coerced);
            }
            _ => {
                return Err(BuildError::InternalLoweringBug(
                    "raw intrinsic is not inline-lowerable",
                ));
            }
        }
        // The Terminator::Call caller passes a target and expects
        // the branch to fall through to the next block; the
        // Rvalue-in-Assign caller passes `None` (it sits inside a
        // basic block before the terminator) and skips the branch.
        if target.is_some() {
            emit_terminator_branch(&mut self.out, target);
        }
        Ok(())
    }
}

pub(crate) fn resolve_external_binding_symbol(name: &str, argc: usize) -> Option<String> {
    let (module_path, item_name) = if let Some((module_path, item_name)) = name.rsplit_once("::") {
        let item = gossamer_resolve::lookup_external_item(name)?;
        if item.params.len() != argc {
            return None;
        }
        (module_path.to_string(), item_name.to_string())
    } else {
        let mut matches = Vec::new();
        for module in gossamer_resolve::all_external_modules() {
            for item in module.items {
                if item.name == name && item.params.len() == argc {
                    matches.push((module.path.clone(), item.name));
                }
            }
        }
        if matches.len() != 1 {
            return None;
        }
        matches.pop()?
    };
    Some(format!(
        "gos_binding_{}__{}",
        module_path.replace("::", "__"),
        item_name
    ))
}

pub(crate) fn external_binding_arity(name: &str) -> Option<usize> {
    if let Some(item) = gossamer_resolve::lookup_external_item(name) {
        return Some(item.params.len());
    }
    let mut matches = Vec::new();
    for module in gossamer_resolve::all_external_modules() {
        for item in module.items {
            if item.name == name {
                matches.push(item.params.len());
            }
        }
    }
    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

/// True for LLVM integer/bool scalar types - the safe element kinds for the
/// inline Vec get/set fast path (the loaded i64 maps cleanly to these).
fn is_primitive_int_llvm(ty: &str) -> bool {
    matches!(ty, "i64" | "i32" | "i16" | "i8" | "i1")
}

/// True for the scalar element kinds the inline Vec get/set fast path can
/// carry through an i64 word: the integer/bool types of
/// [`is_primitive_int_llvm`] plus `f64` (`double`), whose 8-byte word the
/// inline helpers bitcast to/from i64 (`store_i64_as` / `value_to_i64`).
/// `f32` (`float`) is a 4-byte stride the word-load path would over-read and
/// has no `store_i64_as` arm, so it stays on the runtime call.
fn is_inline_vec_scalar_llvm(ty: &str) -> bool {
    is_primitive_int_llvm(ty) || ty == "double"
}
