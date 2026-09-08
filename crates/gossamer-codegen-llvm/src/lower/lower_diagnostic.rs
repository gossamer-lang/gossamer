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
    /// Emits the runtime call + `unreachable` for a MIR
    /// `Terminator::Panic`. The message is interned as a
    /// private rodata global; `gos_rt_panic` is `noreturn`.
    pub(crate) fn lower_panic(&mut self, message: &str) {
        declare_rt(&mut self.runtime_refs, "gos_rt_panic");
        let (msg_name, _) = self.strings.borrow_mut().intern(message);
        self.emit_panic_site_line();
        writeln!(self.out, "  call void @gos_rt_panic(ptr {msg_name})").unwrap();
        writeln!(self.out, "  unreachable").unwrap();
    }

    /// Lowers `Terminator::Assert`: branches to the success
    /// target on the expected condition; on the other branch
    /// emits a category-specific panic message. Mirrors the
    /// Cranelift backend's `BoundsCheck` / `Overflow` /
    /// `DivideByZero` strings so panic output stays consistent
    /// across backends.
    pub(crate) fn lower_assert(
        &mut self,
        cond: &Operand,
        expected: bool,
        target: gossamer_mir::BlockId,
        msg: &gossamer_mir::AssertMessage,
    ) -> Result<(), BuildError> {
        let v = self.lower_operand(cond)?;
        let cond_ty = render_ty(self.tcx, self.operand_ty(cond));
        let cond_bit = if cond_ty == "i1" {
            v
        } else {
            let t = self.fresh();
            writeln!(self.out, "  {t} = icmp ne {cond_ty} {v}, 0").unwrap();
            t
        };
        let ok_label = format!("bb{}", target.as_u32());
        let fail_label = format!("assert_fail_{}", self.next_ssa);
        self.next_ssa += 1;
        let br_true = if expected { &ok_label } else { &fail_label };
        let br_false = if expected { &fail_label } else { &ok_label };
        writeln!(
            self.out,
            "  br i1 {cond_bit}, label %{br_true}, label %{br_false}"
        )
        .unwrap();
        let msg_text = match msg {
            gossamer_mir::AssertMessage::BoundsCheck => "index out of bounds\n",
            gossamer_mir::AssertMessage::Overflow => "arithmetic overflow\n",
            gossamer_mir::AssertMessage::DivideByZero => "divide by zero\n",
        };
        declare_rt(&mut self.runtime_refs, "gos_rt_panic");
        // Intern the message through the module-scoped string pool so a
        // function with several asserts of the same kind (e.g. repeated
        // `%` in gcd) - and asserts across different functions in the
        // module - all reference one shared global instead of each
        // emitting a colliding `@.assert_msg_*` definition.
        let (msg_name, _) = self.strings.borrow_mut().intern(msg_text);
        let cold_start = self.out.len();
        writeln!(self.out, "{fail_label}:").unwrap();
        self.emit_panic_site_line();
        writeln!(self.out, "  call void @gos_rt_panic(ptr {msg_name})").unwrap();
        writeln!(self.out, "  unreachable").unwrap();
        self.mark_cold(cold_start);
        Ok(())
    }

    /// Lowers `println` / `print` / `eprintln` / `eprint` by
    /// dispatching each argument through the runtime helper
    /// matching its MIR type (`gos_rt_print_str` for strings,
    /// `_i64` for integers, `_f64` for floats, `_bool`, `_char`).
    /// Mirrors the per-arg shape of `lower_concat_call` so that
    /// bare `println(5i64)` and interpolated `println!("{n}")`
    /// share one code path. `*ln` variants append a trailing
    /// `gos_rt_println()` for the newline; stdout is flushed by
    /// explicit flushes, stderr ordering, and process exit.
    pub(crate) fn lower_print_call(
        &mut self,
        name: &str,
        args: &[Operand],
        destination: &Place,
        target: Option<&gossamer_mir::BlockId>,
    ) -> Result<(), BuildError> {
        for sym in [
            "gos_rt_eprint_str",
            "gos_rt_eprintln",
            "gos_rt_stdout_acquire",
            "gos_rt_println",
            "gos_rt_stdout_release",
        ] {
            declare_rt(&mut self.runtime_refs, sym);
        }
        if matches!(name, "eprint" | "eprintln") {
            // Build the message via the same per-arg concat
            // machinery `panic` uses, then route it through the
            // stderr writer (which flushes stdout first so
            // diagnostic order is preserved). Keeps eprint output
            // off stdout without parallel `_err` versions of
            // every per-type helper.
            let s = self.emit_args_to_concat_string(args, " ")?;
            writeln!(self.out, "  call void @gos_rt_eprint_str(ptr {s})").unwrap();
            if name == "eprintln" {
                writeln!(self.out, "  call void @gos_rt_eprintln()").unwrap();
            }
        } else {
            // Hold the stdout lock for the whole sequence - every
            // per-arg print + the trailing newline is one atomic
            // unit so concurrent goroutines on other OS threads
            // can't interleave their output mid-line. The lock is
            // reentrant, so the inner runtime helpers (which also
            // acquire) coexist with this outer acquire on the same
            // thread. If an internal backend invariant fails later,
            // the LLVM module is dropped before execution, so a
            // dangling acquire in emitted text is harmless.
            writeln!(self.out, "  call void @gos_rt_stdout_acquire()").unwrap();
            // Spec: each arg is space-separated. Mirrors the
            // interpreter's `render_args` (which inserts a `' '`
            // between each pair).
            self.emit_per_arg_print(args, " ")?;
            if name == "println" {
                writeln!(self.out, "  call void @gos_rt_println()").unwrap();
            }
            writeln!(self.out, "  call void @gos_rt_stdout_release()").unwrap();
        }
        let dest_ty_mir = self.place_leaf_ty(destination);
        if !is_unit(self.tcx, dest_ty_mir) {
            let dest_llvm = render_ty(self.tcx, dest_ty_mir);
            // `println`'s return value is `()` per the prelude;
            // give the destination slot a zero value of its
            // declared type so any unexpected reader sees a sane
            // bit pattern.
            self.store_zero_to_place(destination, &dest_llvm);
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
}
