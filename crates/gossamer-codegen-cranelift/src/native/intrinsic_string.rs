//! Cranelift intrinsic lowering - String / Vec primitive helpers
//! (length, slice, byte access, concat, etc). Fourth and final
//! partition in the dispatch chain. Holds
//! `lower_intrinsic_call_string`.

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
//! Real Cranelift-backed native codegen.
//! Lowers a slice of MIR [`Body`]s into a `cranelift-object` module
//! and serialises the result as ELF (or the host's equivalent object
//! format). Supported today:
//! - `fn main() -> i64` with integer arithmetic (`+`, `-`, `*`, `/`,
//!   `%`, `&`, `|`, `^`, `<<`, `>>`, unary `-`, `!`),
//! - integer constants,
//! - direct calls between lowered functions,
//! - `return` of an `i64`.
//!
//! A C-ABI shim `main(argc, argv) -> i32` is emitted automatically:
//! it calls the Gossamer `main` and truncates the `i64` result into
//! the process exit code, so the object file links through a
//! standard `cc` invocation.
//! Aggregates (tuples/arrays/structs), strings, closures, and
//! anything that needs a GC heap are not yet lowered - those
//! constructs fall back to [`crate::emit::emit_module`] for
//! inspection.

// Allow patterns the Cranelift lowering deliberately uses:
//   - `similar_names` fires on `print_str`/`print_i64`/etc.
//     intrinsic-name shadowing within the same arm. The
//     parallel naming makes the dispatch table readable.
//   - `many_single_char_names` fires on hot inner-loop locals
//     (`a`, `b`, `n`, `m`, `k`) where longer names would
//     overflow the 100-col limit.
//   - `items_after_statements` flags inline `extern "C"` decls
//     localised to the one helper that uses them. Hoisting them
//     to module scope spreads the FFI surface; localised wins.
//   - `too_many_lines` / `cognitive_complexity` fire on the
//     intrinsic-dispatch arm and the `lower_intrinsic_call`
//     match. Splitting either hides the one-arm-per-symbol
//     structure that makes the table grep-able.
//   - `unnecessary_wraps` flags helpers whose `Result` exists
//     so call sites can still `?` them once a future lowering
//     can fail.
//   - `if_chain_can_be_rewritten_with_match` would flatten
//     short `if let Some(x) = .. else if let Some(y) = ..`
//     chains into match-on-tuple-of-options that's strictly
//     uglier here.
//   - `doc_markdown` flags identifiers like `i64`, `f64`,
//     etc. in plain-prose docs. Backticking every numeric
//     type name in every comment is noise.
//   - `manual_debug_impl` flags `JitModule`'s `Debug` impl
//     (which deliberately omits the JIT module pointer to keep
//     debug output stable across runs).
#![forbid(unsafe_code)]
#![allow(clippy::comparison_chain)]

use std::collections::HashMap;

use std::collections::HashSet;

use anyhow::{Result, anyhow, bail};
use cranelift_codegen::ir::{
    AbiParam, ExtFuncData, Function, GlobalValueData, InstBuilder, MemFlagsData, Signature,
    StackSlotData, StackSlotKind, UserExternalName, UserFuncName, condcodes::IntCC,
    immediates::Imm64, types,
};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, ir};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module, ModuleDeclarations};
use cranelift_object::{ObjectBuilder, ObjectModule};
use gossamer_mir::{
    BinOp, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, StatementKind, Terminator,
    UnOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};
use rayon::prelude::*;

use super::*;

pub(super) fn lower_intrinsic_call_string(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    args: &[Operand],
    name: &str,
    destination: &gossamer_mir::Place,
    intrinsics: &mut IntrinsicContext,
) -> Result<bool> {
    #![allow(clippy::too_many_lines, clippy::too_many_arguments)]
    let ptr_ty = module.target_config().pointer_type();
    let _ = ptr_ty; // suppress unused if all arms inline
    match name {
        "gos_rt_str_free_typed" | "gos_rt_str_retain_typed" => {
            let symbol = match name {
                "gos_rt_str_free_typed" => "gos_rt_str_free_typed",
                _ => "gos_rt_str_retain_typed",
            };
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let f = intrinsics.extern_fn_by_name(module, symbol)?;
            let fref = module.declare_func_in_func(f, builder.func);
            let _ = builder.ins().call(fref, &[arg]);
            let unit = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                unit,
            );
            Ok(true)
        }
        "gos_rt_heap_i64_set" => {
            let v = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("heap_i64_set: missing receiver"),
            };
            let idx = match args.get(1) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let val = match args.get(2) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let idx64 = coerce_arg_to(builder, idx, types::I64)?;
            let val64 = coerce_arg_to(builder, val, types::I64)?;
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_heap_i64_set",
                &[ptr_ty, types::I64, types::I64],
                &[],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let _ = builder.ins().call(fref, &[v, idx64, val64]);
            let unit = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                unit,
            );
            Ok(true)
        }
        "gos_rt_heap_i64_len" => {
            let v = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("heap_i64_len: missing receiver"),
            };
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_heap_i64_len")?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[v]);
            let val = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                val,
            );
            Ok(true)
        }
        "gos_rt_heap_i64_write_lines_to_stdout" => {
            let v = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("heap_i64_write_lines: missing receiver"),
            };
            let s = match args.get(1) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let n = match args.get(2) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let w = match args.get(3) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 60),
            };
            let s64 = coerce_arg_to(builder, s, types::I64)?;
            let n64 = coerce_arg_to(builder, n, types::I64)?;
            let w64 = coerce_arg_to(builder, w, types::I64)?;
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_heap_i64_write_lines_to_stdout",
                &[ptr_ty, types::I64, types::I64, types::I64],
                &[],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let _ = builder.ins().call(fref, &[v, s64, n64, w64]);
            let unit = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                unit,
            );
            Ok(true)
        }
        "gos_rt_heap_i64_write_bytes_to_stdout" => {
            let v = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("heap_i64_write: missing receiver"),
            };
            let s = match args.get(1) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let n = match args.get(2) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let s64 = coerce_arg_to(builder, s, types::I64)?;
            let n64 = coerce_arg_to(builder, n, types::I64)?;
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_heap_i64_write_bytes_to_stdout",
                &[ptr_ty, types::I64, types::I64],
                &[],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let _ = builder.ins().call(fref, &[v, s64, n64]);
            let unit = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                unit,
            );
            Ok(true)
        }
        // ---- Heap [u8] primitive (`U8Vec`) - 1 byte per element ----
        "U8Vec::new" | "heap_u8::new" | "gos_rt_heap_u8_new" => {
            let len = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let len64 = coerce_arg_to(builder, len, types::I64)?;
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_heap_u8_new")?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[len64]);
            let ptr = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ptr,
            );
            Ok(true)
        }
        "gos_rt_heap_u8_get" => {
            let v = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("heap_u8_get: missing receiver"),
            };
            let idx = match args.get(1) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let idx64 = coerce_arg_to(builder, idx, types::I64)?;
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_heap_u8_get",
                &[ptr_ty, types::I64],
                &[types::I64],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[v, idx64]);
            let val = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                val,
            );
            Ok(true)
        }
        "gos_rt_heap_u8_set" => {
            let v = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("heap_u8_set: missing receiver"),
            };
            let idx = match args.get(1) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let val = match args.get(2) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let idx64 = coerce_arg_to(builder, idx, types::I64)?;
            let val64 = coerce_arg_to(builder, val, types::I64)?;
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_heap_u8_set",
                &[ptr_ty, types::I64, types::I64],
                &[],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let _ = builder.ins().call(fref, &[v, idx64, val64]);
            let unit = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                unit,
            );
            Ok(true)
        }
        "gos_rt_heap_u8_len" => {
            let v = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("heap_u8_len: missing receiver"),
            };
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_heap_u8_len")?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[v]);
            let val = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                val,
            );
            Ok(true)
        }
        // `buf.to_string(len)` - freezes the first `len` bytes of
        // a `U8Vec` build buffer into an immutable `String`.
        "gos_rt_heap_u8_to_string" => {
            let v = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("heap_u8_to_string: missing receiver"),
            };
            let len_v = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let len64 = coerce_arg_to(builder, len_v, types::I64)?;
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_heap_u8_to_string",
                &[ptr_ty, types::I64],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[v, len64]);
            let val = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                val,
            );
            Ok(true)
        }
        "gos_rt_heap_u8_write_lines_to_stdout" => {
            let v = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("heap_u8_write_lines: missing receiver"),
            };
            let s = match args.get(1) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let n = match args.get(2) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let w = match args.get(3) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 60),
            };
            let s64 = coerce_arg_to(builder, s, types::I64)?;
            let n64 = coerce_arg_to(builder, n, types::I64)?;
            let w64 = coerce_arg_to(builder, w, types::I64)?;
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_heap_u8_write_lines_to_stdout",
                &[ptr_ty, types::I64, types::I64, types::I64],
                &[],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let _ = builder.ins().call(fref, &[v, s64, n64, w64]);
            let unit = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                unit,
            );
            Ok(true)
        }
        "gos_rt_heap_u8_write_bytes_to_stdout" => {
            let v = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("heap_u8_write: missing receiver"),
            };
            let s = match args.get(1) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let n = match args.get(2) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let s64 = coerce_arg_to(builder, s, types::I64)?;
            let n64 = coerce_arg_to(builder, n, types::I64)?;
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_heap_u8_write_bytes_to_stdout",
                &[ptr_ty, types::I64, types::I64],
                &[],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let _ = builder.ins().call(fref, &[v, s64, n64]);
            let unit = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                unit,
            );
            Ok(true)
        }
        // ---- Atomic<i64> primitive ----
        "Atomic::new"
        | "sync::Atomic::new"
        | "atomic::new"
        | "AtomicI64::new"
        | "sync::AtomicI64::new"
        | "AtomicU64::new"
        | "sync::AtomicU64::new"
        | "gos_rt_atomic_i64_new" => {
            let initial = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let i64 = coerce_arg_to(builder, initial, types::I64)?;
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_atomic_i64_new")?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[i64]);
            let ptr = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ptr,
            );
            Ok(true)
        }
        // ---- Atomic<bool> primitive ----
        // Shares the i64 storage but keeps distinct symbols so the
        // load result pins to `bool` (renders `true` / `false`).
        "AtomicBool::new" | "sync::AtomicBool::new" | "gos_rt_atomic_bool_new" => {
            let initial = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I8),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I8, 0),
            };
            let i8v = coerce_arg_to(builder, initial, types::I8)?;
            let f =
                intrinsics.extern_fn(module, "gos_rt_atomic_bool_new", &[types::I8], &[ptr_ty])?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[i8v]);
            let ptr = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ptr,
            );
            Ok(true)
        }
        "gos_rt_atomic_bool_load" => {
            let a = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("atomic_bool_load: missing receiver"),
            };
            let f =
                intrinsics.extern_fn(module, "gos_rt_atomic_bool_load", &[ptr_ty], &[types::I8])?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[a]);
            let val = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                val,
            );
            Ok(true)
        }
        "gos_rt_atomic_bool_store" => {
            let a = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("atomic_bool_store: missing receiver"),
            };
            let v = match args.get(1) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I8),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I8, 0),
            };
            let v8 = coerce_arg_to(builder, v, types::I8)?;
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_atomic_bool_store",
                &[ptr_ty, types::I8],
                &[],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let _ = builder.ins().call(fref, &[a, v8]);
            let unit = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                unit,
            );
            Ok(true)
        }
        "gos_rt_atomic_i64_load" => {
            let a = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("atomic_load: missing receiver"),
            };
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_atomic_i64_load")?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[a]);
            let val = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                val,
            );
            Ok(true)
        }
        "gos_rt_atomic_i64_store" => {
            let a = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("atomic_store: missing receiver"),
            };
            let v = match args.get(1) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let v64 = coerce_arg_to(builder, v, types::I64)?;
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_atomic_i64_store",
                &[ptr_ty, types::I64],
                &[],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let _ = builder.ins().call(fref, &[a, v64]);
            let unit = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                unit,
            );
            Ok(true)
        }
        // LCG jump-ahead helper. Used by multi-threaded programs
        // to seed each worker at the
        // right point in the random stream so the per-worker
        // streams interleave back into the same sequence the
        // single-thread reference produces.
        "gos_rt_lcg_jump" | "lcg::jump" | "lcg_jump" => {
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_lcg_jump",
                &[types::I64, types::I64, types::I64, types::I64, types::I64],
                &[types::I64],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let args_v: Vec<_> = (0..5)
                .map(|i| match args.get(i) {
                    Some(a) => lower_operand(
                        module,
                        builder,
                        locals,
                        body,
                        tcx,
                        a,
                        Some(types::I64),
                        intrinsics,
                    ),
                    None => Ok(builder.ins().iconst(types::I64, 0)),
                })
                .collect::<Result<Vec<_>>>()?;
            let coerced: Vec<_> = args_v
                .into_iter()
                .map(|v| coerce_arg_to(builder, v, types::I64))
                .collect::<Result<Vec<_>>>()?;
            let call = builder.ins().call(fref, &coerced);
            let val = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                val,
            );
            Ok(true)
        }
        "gos_rt_atomic_i64_fetch_add" | "gos_rt_atomic_i64_fetch_sub" => {
            let a = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => bail!("atomic_fetch_add: missing receiver"),
            };
            let d = match args.get(1) {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let d64 = coerce_arg_to(builder, d, types::I64)?;
            let symbol: &'static str = if name == "gos_rt_atomic_i64_fetch_sub" {
                "gos_rt_atomic_i64_fetch_sub"
            } else {
                "gos_rt_atomic_i64_fetch_add"
            };
            let f = intrinsics.extern_fn(module, symbol, &[ptr_ty, types::I64], &[types::I64])?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[a, d64]);
            let val = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                val,
            );
            Ok(true)
        }
        // `Vec<T>::len()` - the runtime exposes `len` as the first
        // i64 of the `#[repr(C)] GosVec { len, cap, elem_bytes, ptr }`
        // header (see runtime/src/c_abi.rs:1791). Inline the read as
        // a null check + offset-0 load so the for-loop bound check
        // doesn't pay the C-ABI call cost on every iteration. The
        // null guard preserves the helper's `null -> 0` semantics
        // (relied on by the `os::args` placeholder shape and any
        // uninitialised-Vec carrier in the codegen).
        "gos_rt_vec_len" => {
            let m = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let zero = builder.ins().iconst(types::I64, 0);
            let null_ptr = builder.ins().iconst(ptr_ty, 0);
            let is_null = builder.ins().icmp(ir::condcodes::IntCC::Equal, m, null_ptr);
            let loaded = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), m, 0);
            let n = builder.ins().select(is_null, zero, loaded);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                n,
            );
            Ok(true)
        }
        // Array length: forward to the runtime shim, which reads
        // the first i64 slot of the passed pointer (GosArgs and
        // other len-prefixed buffers share that layout).
        "gos_rt_str_is_empty" => {
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_str_is_empty")?;
            let fref = module.declare_func_in_func(f, builder.func);
            let p = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let call = builder.ins().call(fref, &[p]);
            let v = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                v,
            );
            Ok(true)
        }
        "gos_rt_len_is_zero" => {
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_len_is_zero")?;
            let fref = module.declare_func_in_func(f, builder.func);
            let p = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let call = builder.ins().call(fref, &[p]);
            let v = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                v,
            );
            Ok(true)
        }
        "gos_rt_arr_len" | "gos_rt_len" => {
            let len_fn = intrinsics.extern_fn_by_name(module, "gos_rt_arr_len")?;
            let len_ref = module.declare_func_in_func(len_fn, builder.func);
            let p = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let call = builder.ins().call(len_ref, &[p]);
            let n = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                n,
            );
            Ok(true)
        }
        // Unary string helpers that return a fresh String
        // (allocated by the runtime). Signatures are `(ptr) -> ptr`.
        "gos_rt_str_trim"
        | "gos_rt_str_to_lower"
        | "gos_rt_str_to_upper"
        | "gos_rt_str_as_bytes"
        | "gos_rt_vec_clone" => {
            let rt_fn = intrinsics.extern_fn(
                module,
                match name {
                    "gos_rt_str_trim" => "gos_rt_str_trim",
                    "gos_rt_str_to_lower" => "gos_rt_str_to_lower",
                    "gos_rt_str_to_upper" => "gos_rt_str_to_upper",
                    "gos_rt_str_as_bytes" => "gos_rt_str_as_bytes",
                    "gos_rt_vec_clone" => "gos_rt_vec_clone",
                    _ => unreachable!(),
                },
                &[ptr_ty],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let s = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let call = builder.ins().call(fref, &[s]);
            let ptr = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ptr,
            );
            Ok(true)
        }
        // Predicate string helpers: `(ptr, ptr) -> i32`.
        "gos_rt_str_contains" | "gos_rt_str_starts_with" | "gos_rt_str_ends_with" => {
            let rt_fn = intrinsics.extern_fn(
                module,
                match name {
                    "gos_rt_str_contains" => "gos_rt_str_contains",
                    "gos_rt_str_starts_with" => "gos_rt_str_starts_with",
                    "gos_rt_str_ends_with" => "gos_rt_str_ends_with",
                    _ => unreachable!(),
                },
                &[ptr_ty, ptr_ty],
                &[types::I32],
            )?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let a = match args.first() {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let b = match args.get(1) {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let call = builder.ins().call(fref, &[a, b]);
            let result = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                result,
            );
            Ok(true)
        }
        "gos_rt_str_find" | "gos_rt_str_find_opt" => {
            // `find_opt` returns `*mut GosResult` (Option<i64>);
            // the bare `find` returns raw i64. Both share two
            // `*const c_char` argument shapes - pick the result
            // type by the symbol name.
            let (sym, ret_ty): (&'static str, _) = if name == "gos_rt_str_find_opt" {
                ("gos_rt_str_find_opt", ptr_ty)
            } else {
                ("gos_rt_str_find", types::I64)
            };
            let rt_fn = intrinsics.extern_fn(module, sym, &[ptr_ty, ptr_ty], &[ret_ty])?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let a = match args.first() {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let b = match args.get(1) {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let call = builder.ins().call(fref, &[a, b]);
            let n = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                n,
            );
            Ok(true)
        }
        // `s.split(sep)`, `s.lines()`, `s.repeat(n)`. Each
        // returns a fresh GC-managed pointer (Vec or String).
        "gos_rt_str_eq" => {
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_str_eq")?;
            let fref = module.declare_func_in_func(f, builder.func);
            let a = match args.first() {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let b = match args.get(1) {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let call = builder.ins().call(fref, &[a, b]);
            let v = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                v,
            );
            Ok(true)
        }
        "gos_rt_str_split" | "gos_rt_str_lines" => {
            let arity_two = name == "gos_rt_str_split";
            let params: &[ir::Type] = if arity_two {
                &[ptr_ty, ptr_ty]
            } else {
                &[ptr_ty]
            };
            // `extern_fn` keys on a `&'static str`; leak the
            // matched name once. Bounded leak - at most two
            // entries (split + lines) across the program.
            let static_name: &'static str = match name {
                "gos_rt_str_split" => "gos_rt_str_split",
                "gos_rt_str_lines" => "gos_rt_str_lines",
                _ => unreachable!(),
            };
            let rt_fn = intrinsics.extern_fn(module, static_name, params, &[ptr_ty])?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let s = match args.first() {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let result = if arity_two {
                let sep = match args.get(1) {
                    Some(arg) => {
                        let raw = lower_operand(
                            module,
                            builder,
                            locals,
                            body,
                            tcx,
                            arg,
                            Some(ptr_ty),
                            intrinsics,
                        )?;
                        if operand_is_char(body, tcx, arg) {
                            // Char separator: convert to a one-
                            // char c-string before passing to
                            // the runtime helper.
                            let cts = intrinsics.extern_fn(
                                module,
                                "gos_rt_char_to_str",
                                &[types::I32],
                                &[ptr_ty],
                            )?;
                            let cts_ref = module.declare_func_in_func(cts, builder.func);
                            let call = builder.ins().call(cts_ref, &[raw]);
                            builder.inst_results(call)[0]
                        } else {
                            coerce_arg_to(builder, raw, ptr_ty)?
                        }
                    }
                    None => builder.ins().iconst(ptr_ty, 0),
                };
                builder.ins().call(fref, &[s, sep])
            } else {
                builder.ins().call(fref, &[s])
            };
            let ptr = builder.inst_results(result)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ptr,
            );
            Ok(true)
        }
        "gos_rt_str_repeat" => {
            let rt_fn = intrinsics.extern_fn(
                module,
                "gos_rt_str_repeat",
                &[ptr_ty, types::I64],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let s = match args.first() {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let n_val = match args.get(1) {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let n = coerce_arg_to(builder, n_val, types::I64)?;
            let call = builder.ins().call(fref, &[s, n]);
            let ptr = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ptr,
            );
            Ok(true)
        }
        "gos_rt_str_replace" => {
            let rt_fn = intrinsics.extern_fn(
                module,
                "gos_rt_str_replace",
                &[ptr_ty, ptr_ty, ptr_ty],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let a = match args.first() {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let b = match args.get(1) {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let c = match args.get(2) {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let call = builder.ins().call(fref, &[a, b, c]);
            let ptr = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ptr,
            );
            Ok(true)
        }
        // `v.push(x)` on a Vec<T>: spill x to a stack slot and
        // call the runtime's typed push. The runtime reads
        // `vec.elem_bytes` bytes from the pointer we pass, so for
        // multi-slot aggregates (tuples / structs / inline arrays)
        // we must pass the address of the actual storage -
        // spilling the operand's pointer-value into an 8-byte
        // slot leaks only the first word and rereads adjacent
        // stack bytes for the rest. Scalars still go through the
        // 8-byte slot path so misaligned int / float types reach
        // the runtime as a clean little-endian 8-byte payload.
        "gos_rt_vec_push" | "gos_rt_deque_push_back_wide" | "gos_rt_deque_push_front_wide" => {
            let callee: &'static str = match name {
                "gos_rt_deque_push_back_wide" => "gos_rt_deque_push_back_wide",
                "gos_rt_deque_push_front_wide" => "gos_rt_deque_push_front_wide",
                _ => "gos_rt_vec_push",
            };
            let push_fn = intrinsics.extern_fn_by_name(module, callee)?;
            let vec_p = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let elem_arg = args.get(1);
            let agg_slots = elem_arg.and_then(|a| operand_aggregate_slots(body, tcx, a));
            let elem_addr = if let (Some(slots), Some(a)) = (agg_slots, elem_arg) {
                // Multi-slot aggregate operand. Take the address of
                // its backing storage and pass it through - the
                // runtime memcpys `slots * 8` bytes into the vec.
                let _ = slots;
                let Operand::Copy(place) = a else {
                    // operand_aggregate_slots only returns Some for
                    // Copy(place) - unreachable otherwise.
                    unreachable!("aggregate-slot operand must be Copy(place)")
                };
                lower_place_address(module, builder, locals, body, tcx, place, intrinsics)?
            } else {
                let value = match elem_arg {
                    Some(a) => {
                        lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?
                    }
                    None => builder.ins().iconst(types::I64, 0),
                };
                if value_type(value, builder) == types::I128 {
                    // A two-word carrier element (`Option<T>` /
                    // `Result<T, E>`): spill the packed value into a
                    // 16-byte slot - the runtime memcpys
                    // `vec.elem_bytes` (16) from the address.
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        16,
                        3,
                    ));
                    let slot_addr = builder.ins().stack_addr(ptr_ty, slot, 0);
                    store_i128_words(builder, value, slot_addr, 0);
                    slot_addr
                } else {
                    let v64 = coerce_arg_to(builder, value, types::I64)?;
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        3,
                    ));
                    let slot_addr = builder.ins().stack_addr(ptr_ty, slot, 0);
                    builder
                        .ins()
                        .store(MemFlagsData::trusted(), v64, slot_addr, 0);
                    slot_addr
                }
            };
            let fref = module.declare_func_in_func(push_fn, builder.func);
            let _ = builder.ins().call(fref, &[vec_p, elem_addr]);
            let unit = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                unit,
            );
            Ok(true)
        }
        // Typed-i64 push used by the dynamic-count `[value; n]`
        // lowering. The wrapper handles the stack-slot dance
        // inside the runtime so the codegen doesn't have to.
        "gos_rt_vec_push_i64" => {
            let push_fn = intrinsics.extern_fn_by_name(module, "gos_rt_vec_push_i64")?;
            let vec_p = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let value = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let v64 = coerce_arg_to(builder, value, types::I64)?;
            let fref = module.declare_func_in_func(push_fn, builder.func);
            let _ = builder.ins().call(fref, &[vec_p, v64]);
            let unit = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                unit,
            );
            Ok(true)
        }
        // `arr[lo..hi]` - copies a subrange into a new GosVec.
        "gos_rt_vec_slice" => {
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_vec_slice",
                &[ptr_ty, types::I64, types::I64],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let v = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let lo_v = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let hi_v = match args.get(2) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let lo = coerce_arg_to(builder, lo_v, types::I64)?;
            let hi = coerce_arg_to(builder, hi_v, types::I64)?;
            let call = builder.ins().call(fref, &[v, lo, hi]);
            let p = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                p,
            );
            Ok(true)
        }
        // `vec_get_ptr(v, i)` - returns a `*mut u8` pointer to
        // the i-th element's slot. Used by the for-vec loop
        // lowering to read each element via a follow-up
        // `gos_load(ptr, 0)` so the same code handles scalar
        // and pointer-shaped element types.
        // The unchecked form addresses an index MIR already proved in bounds,
        // which the checked address reaches along its in-bounds path.
        "gos_rt_vec_get_ptr" | "gos_rt_vec_get_ptr_unchecked" => {
            let vec_p = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(ptr_ty),
                    intrinsics,
                )?,
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let i_val = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let i = coerce_arg_to(builder, i_val, types::I64)?;
            let can_inline_header_stride = args.first().is_some_and(|arg| match arg {
                Operand::Copy(place) => match tcx.kind_of(resolve_place_ty(tcx, body, place)) {
                    TyKind::Vec(elem) | TyKind::Slice(elem) => {
                        !matches!(tcx.kind_of(*elem), TyKind::Vec(_))
                    }
                    _ => false,
                },
                _ => false,
            });
            let ptr = if can_inline_header_stride && destination.projection.is_empty() {
                let valid_blk = builder.create_block();
                let null_blk = builder.create_block();
                let done_blk = builder.create_block();
                builder.append_block_param(done_blk, ptr_ty);

                let is_null = builder.ins().icmp_imm_s(IntCC::Equal, vec_p, 0);
                builder.ins().brif(is_null, null_blk, &[], valid_blk, &[]);

                builder.switch_to_block(valid_blk);
                let len = builder
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), vec_p, 0);
                let ge0 = builder
                    .ins()
                    .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, i, 0);
                let lt_len = builder.ins().icmp(IntCC::SignedLessThan, i, len);
                let in_bounds = builder.ins().band(ge0, lt_len);
                let load_blk = builder.create_block();
                let oob_blk = builder.create_block();
                builder.ins().brif(in_bounds, load_blk, &[], oob_blk, &[]);

                builder.switch_to_block(load_blk);
                let elem_bytes32 =
                    builder
                        .ins()
                        .load(types::I32, MemFlagsData::trusted(), vec_p, 16);
                let elem_bytes64 = builder.ins().uextend(types::I64, elem_bytes32);
                let off64 = builder.ins().imul(i, elem_bytes64);
                let off = coerce_arg_to(builder, off64, ptr_ty)?;
                let data = builder
                    .ins()
                    .load(ptr_ty, MemFlagsData::trusted(), vec_p, 24);
                let elem_ptr = builder.ins().iadd(data, off);
                builder
                    .ins()
                    .jump(done_blk, &[ir::BlockArg::Value(elem_ptr)]);

                builder.switch_to_block(oob_blk);
                let null = builder.ins().iconst(ptr_ty, 0);
                builder.ins().jump(done_blk, &[ir::BlockArg::Value(null)]);

                builder.switch_to_block(null_blk);
                let null = builder.ins().iconst(ptr_ty, 0);
                builder.ins().jump(done_blk, &[ir::BlockArg::Value(null)]);

                builder.switch_to_block(done_blk);
                builder.block_params(done_blk)[0]
            } else {
                let get_fn = intrinsics.extern_fn(
                    module,
                    "gos_rt_vec_get_ptr",
                    &[ptr_ty, types::I64],
                    &[ptr_ty],
                )?;
                let fref = module.declare_func_in_func(get_fn, builder.func);
                let call = builder.ins().call(fref, &[vec_p, i]);
                builder.inst_results(call)[0]
            };
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ptr,
            );
            Ok(true)
        }
        // `v.pop()` - `gos_rt_vec_pop_opt` returns the popped
        // element as a packed Option and routes through the
        // generic forwarding below (same shape as `v.first()`).
        // Generic forwarding for the new stdlib helpers added in
        // round 3 (errors / regex / fs / path / flag / bufio /
        // http / gzip / slog / testing). Each follows the same
        // shape: the MIR side picked a single runtime symbol
        // and supplies the args; we declare the extern with the
        // right signature based on the symbol name and call it.
        s if generic_rt_static_name(s).is_some() => {
            let static_name = generic_rt_static_name(s).expect("checked above");
            lower_generic_rt_call(
                module,
                builder,
                locals,
                body,
                tcx,
                args,
                intrinsics,
                destination,
                static_name,
            )?;
            Ok(true)
        }
        s if s.starts_with("gos_binding_") => {
            lower_external_binding_call(
                module,
                builder,
                locals,
                body,
                tcx,
                args,
                intrinsics,
                destination,
                s,
            )?;
            Ok(true)
        }
        #[allow(
            unreachable_patterns,
            reason = "the catch-all keeps the arm list open as intrinsics are added"
        )]
        _ => Ok(false),
        #[allow(
            unreachable_patterns,
            reason = "the catch-all keeps the arm list open as intrinsics are added"
        )]
        _ => Ok(false),
    }
}
