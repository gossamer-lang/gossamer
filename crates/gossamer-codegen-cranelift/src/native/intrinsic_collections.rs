//! Cranelift intrinsic lowering - collections / result / flag /
//! HTTP family. Second partition in the dispatch chain. Holds
//! `lower_intrinsic_call_collections` plus the `gos_rt_vec_*`,
//! `gos_rt_map_*`, `gos_rt_result_*`, `gos_rt_flag_*`, and
//! `gos_rt_http_*` dispatch arms.

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

pub(super) fn lower_intrinsic_call_collections(
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
        "sync::yield_now" | "runtime::yield_now" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_go_yield")?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let _ = builder.ins().call(fref, &[]);
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
        "time::sleep" => {
            // `time::sleep(ms: i64)` matches the VM and the Go
            // reference - argument is milliseconds. Routes
            // through the runtime's `gos_rt_sleep_ms` shim that
            // multiplies by 1_000_000 internally; before the
            // shim landed the compiled tier called
            // `gos_rt_sleep_ns(ms)` directly and slept for
            // nanoseconds, busy-spinning every poll loop.
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_sleep_ms")?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let ms = match args.first() {
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
            let ms = coerce_arg_to(builder, ms, types::I64)?;
            let _ = builder.ins().call(fref, &[ms]);
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
        // `std::strconv::parse_i64(s)` / `parse_f64(s)` - route
        // to the runtime. Ignore the `ok` out-parameter the
        // runtime exposes; callers that care about success take
        // the interpreter path. A real `Result<T, ParseError>`
        // path needs enum-with-payload support.
        // Numeric-to-String formatters (used by `42.to_string()`
        // and `3.14.to_string()`).
        "gos_rt_i64_to_str" | "gos_rt_i64_chars" => {
            let symbol = if name == "gos_rt_i64_chars" {
                "gos_rt_i64_chars"
            } else {
                "gos_rt_i64_to_str"
            };
            let rt_fn = intrinsics.extern_fn_by_name(module, symbol)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let n = match args.first() {
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
            let n64 = coerce_arg_to(builder, n, types::I64)?;
            let call = builder.ins().call(fref, &[n64]);
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
        "gos_rt_f64_to_str" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_f64_to_str")?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let x = match args.first() {
                Some(a) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    a,
                    Some(types::F64),
                    intrinsics,
                )?,
                None => builder.ins().f64const(0.0),
            };
            let x64 = coerce_arg_to(builder, x, types::F64)?;
            let call = builder.ins().call(fref, &[x64]);
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
        "strconv::parse_i64" | "gos_rt_parse_i64" => {
            let rt_fn = intrinsics.extern_fn(
                module,
                "gos_rt_parse_i64",
                &[ptr_ty, ptr_ty],
                &[types::I64],
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
            let null = builder.ins().iconst(ptr_ty, 0);
            let call = builder.ins().call(fref, &[s, null]);
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
        "gos_rt_parse_i64_result" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_parse_i64_result")?;
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
            let r = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                r,
            );
            Ok(true)
        }
        "gos_rt_result_map_err" | "gos_rt_result_map" => {
            let helper_name: &'static str = if name == "gos_rt_result_map_err" {
                "gos_rt_result_map_err"
            } else {
                "gos_rt_result_map"
            };
            let rt_fn = intrinsics.extern_fn(module, helper_name, &[ptr_ty, ptr_ty], &[ptr_ty])?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let recv = match args.first() {
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
            let clos = match args.get(1) {
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
            let call = builder.ins().call(fref, &[recv, clos]);
            let r = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                r,
            );
            Ok(true)
        }
        "gos_rt_flag_cell_load_str"
        | "gos_rt_flag_cell_load_i64"
        | "gos_rt_flag_cell_load_bool"
        | "gos_rt_flag_cell_load_f64"
        | "gos_rt_flag_cell_load_vec" => {
            let helper_name: &'static str = match name {
                "gos_rt_flag_cell_load_str" => "gos_rt_flag_cell_load_str",
                "gos_rt_flag_cell_load_i64" => "gos_rt_flag_cell_load_i64",
                "gos_rt_flag_cell_load_f64" => "gos_rt_flag_cell_load_f64",
                "gos_rt_flag_cell_load_vec" => "gos_rt_flag_cell_load_vec",
                _ => "gos_rt_flag_cell_load_bool",
            };
            let ret_ty = match helper_name {
                "gos_rt_flag_cell_load_i64" | "gos_rt_flag_cell_load_bool" => types::I64,
                "gos_rt_flag_cell_load_f64" => types::F64,
                _ => ptr_ty,
            };
            let rt_fn = intrinsics.extern_fn(module, helper_name, &[ptr_ty], &[ret_ty])?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let raw_cell = match args.first() {
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
            let cell = coerce_arg_to(builder, raw_cell, ptr_ty)?;
            let call = builder.ins().call(fref, &[cell]);
            let mut r = builder.inst_results(call)[0];
            // Bool destination is declared as i8 in cranelift (MIR
            // bool_ty maps to I8). The helper returns i64 so the
            // result needs an ireduce to fit the destination Variable.
            if helper_name == "gos_rt_flag_cell_load_bool" {
                r = builder.ins().ireduce(types::I8, r);
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                r,
            );
            Ok(true)
        }
        "strconv::parse_f64" => {
            let rt_fn = intrinsics.extern_fn(
                module,
                "gos_rt_parse_f64",
                &[ptr_ty, ptr_ty],
                &[types::F64],
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
            let null = builder.ins().iconst(ptr_ty, 0);
            let call = builder.ins().call(fref, &[s, null]);
            let x = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                x,
            );
            Ok(true)
        }
        // `std::http::serve(addr, handler)` - start a blocking
        // TCP listener on `addr` and dispatch requests through the
        // handler fn-ptr. Returns the packed `Result<(), Error>`:
        // `Err` on bind failure, `Ok(())` if the accept loop exits.
        "http::serve" | "gos_rt_http_serve" => {
            let addr = match args.first() {
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
            let env = match args.get(1) {
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
            let env_ptr = coerce_arg_to(builder, env, ptr_ty)?;
            let fn_ptr = match args.get(2) {
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
            let fn_ptr64 = coerce_arg_to(builder, fn_ptr, types::I64)?;
            let result = emit_win64_rt_call(
                module,
                builder,
                intrinsics,
                "gos_rt_http_serve",
                &[ptr_ty, ptr_ty, types::I64],
                Some(types::I128),
                &[addr, env_ptr, fn_ptr64],
            )?
            .expect("gos_rt_http_serve returns a Result carrier");
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                result,
            );
            Ok(true)
        }
        // Same shape as http::serve but routes to the h2 server.
        "http2::bind_and_run_h2c" | "gos_rt_http2_bind_and_run_h2c" => {
            let addr = match args.first() {
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
            let env = match args.get(1) {
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
            let env_ptr = coerce_arg_to(builder, env, ptr_ty)?;
            let fn_ptr = match args.get(2) {
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
            let fn_ptr64 = coerce_arg_to(builder, fn_ptr, types::I64)?;
            let result = emit_win64_rt_call(
                module,
                builder,
                intrinsics,
                "gos_rt_http2_bind_and_run_h2c",
                &[ptr_ty, ptr_ty, types::I64],
                Some(types::I128),
                &[addr, env_ptr, fn_ptr64],
            )?
            .expect("gos_rt_http2_bind_and_run_h2c returns a Result carrier");
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                result,
            );
            Ok(true)
        }
        // `os::exit(code)` / `process::exit(code)` - both spellings
        // route through `gos_rt_exit` (which calls
        // `std::process::exit` - identical behavior to libc's
        // `exit`, but keeps every syscall that touches process
        // state inside the runtime crate).
        "os::exit" | "process::exit" => {
            let exit = intrinsics.extern_fn_by_name(module, "gos_rt_exit")?;
            let exit_ref = module.declare_func_in_func(exit, builder.func);
            let code = match args.first() {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I32, 0),
            };
            let code32 = match value_type(code, builder) {
                t if t == types::I32 => code,
                t if t.is_int() && t.bits() > 32 => builder.ins().ireduce(types::I32, code),
                _ => code,
            };
            let _ = builder.ins().call(exit_ref, &[code32]);
            let zero = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                zero,
            );
            Ok(true)
        }
        // `process::id()` -> u32. Calls the runtime helper that
        // wraps `std::process::id`. Width-widen to i64 for the
        // destination since the local slots are 8 bytes.
        "process::id" => {
            let id_fn = intrinsics.extern_fn_by_name(module, "gos_rt_process_id")?;
            let id_ref = module.declare_func_in_func(id_fn, builder.func);
            let call = builder.ins().call(id_ref, &[]);
            let result = builder.inst_results(call)[0];
            let widened = builder.ins().uextend(types::I64, result);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                widened,
            );
            Ok(true)
        }
        // `process::abort()` -> !. Routes through gos_rt_process_abort.
        "process::abort" => {
            let abort_fn = intrinsics.extern_fn_by_name(module, "gos_rt_process_abort")?;
            let abort_ref = module.declare_func_in_func(abort_fn, builder.func);
            let _ = builder.ins().call(abort_ref, &[]);
            let zero = builder.ins().iconst(types::I64, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                zero,
            );
            Ok(true)
        }
        // `Vec::new(elem_bytes)` / `Vec::with_capacity(elem_bytes,
        // cap)`. The MIR builder passes the actual element width
        // as the leading argument (sized from the binding's
        // `Vec<T>` element type via `elem_bytes_of`). Reading that
        // arg through - rather than hard-coding 8 - lets multi-
        // slot elements like `(String, i64)` reach the runtime
        // with the right stride.
        "Vec::new" | "gos_rt_vec_new" => {
            let kind = vec_elem_kind_from_dest(body, tcx, destination.local);
            let eb_raw = match args.first() {
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
                None => {
                    let bytes = vec_elem_bytes_from_dest(body, tcx, destination.local).unwrap_or(8);
                    builder.ins().iconst(types::I64, bytes)
                }
            };
            let eb_i64 = coerce_arg_to(builder, eb_raw, types::I64)?;
            let eb = builder.ins().ireduce(types::I32, eb_i64);
            let ptr = if kind == gossamer_types::vec_elem_kind::PRIMITIVE {
                let new_fn = intrinsics.extern_fn_by_name(module, "gos_rt_vec_new")?;
                let fref = module.declare_func_in_func(new_fn, builder.func);
                let call = builder.ins().call(fref, &[eb]);
                builder.inst_results(call)[0]
            } else {
                // Typed-allocation path: the runtime's deep-free
                // walks element pointers at vec_free time so a
                // `Vec<String>` / `Vec<Vec<T>>` / `Vec<HashMap<...>>`
                // does not leak its element payloads.
                let new_fn = intrinsics.extern_fn_by_name(module, "gos_rt_vec_new_typed")?;
                let fref = module.declare_func_in_func(new_fn, builder.func);
                let kind_val = builder.ins().iconst(types::I8, i64::from(kind));
                let call = builder.ins().call(fref, &[eb, kind_val]);
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
        "Vec::with_capacity" | "gos_rt_vec_with_capacity" => {
            let kind = vec_elem_kind_from_dest(body, tcx, destination.local);
            let eb_raw = match args.first() {
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
                None => {
                    let bytes = vec_elem_bytes_from_dest(body, tcx, destination.local).unwrap_or(8);
                    builder.ins().iconst(types::I64, bytes)
                }
            };
            let eb_i64 = coerce_arg_to(builder, eb_raw, types::I64)?;
            let eb = builder.ins().ireduce(types::I32, eb_i64);
            let cap = match args.get(1) {
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
            let cap64 = coerce_arg_to(builder, cap, types::I64)?;
            let ptr = if kind == gossamer_types::vec_elem_kind::PRIMITIVE {
                let new_fn = intrinsics.extern_fn(
                    module,
                    "gos_rt_vec_with_capacity",
                    &[types::I32, types::I64],
                    &[ptr_ty],
                )?;
                let fref = module.declare_func_in_func(new_fn, builder.func);
                let call = builder.ins().call(fref, &[eb, cap64]);
                builder.inst_results(call)[0]
            } else {
                let new_fn = intrinsics.extern_fn(
                    module,
                    "gos_rt_vec_with_capacity_typed",
                    &[types::I32, types::I64, types::I8],
                    &[ptr_ty],
                )?;
                let fref = module.declare_func_in_func(new_fn, builder.func);
                let kind_val = builder.ins().iconst(types::I8, i64::from(kind));
                let call = builder.ins().call(fref, &[eb, cap64, kind_val]);
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
        "gos_rt_vec_from_arr" | "gos_rt_vec_borrow_arr" => {
            // Wraps a fixed-size array `[T; N]` in a heap GosVec.
            // Args: (elem_bytes: i64 -> coerced to u32, data: ptr,
            // len: i64). The MIR side emits this at the binding-
            // call boundary when a Vec<T> param meets a [T; N]
            // arg. `borrow_arr` is the non-owning view variant for a
            // `&[T]` parameter (same construction, identical ABI).
            let sym: &'static str = if name == "gos_rt_vec_borrow_arr" {
                "gos_rt_vec_borrow_arr"
            } else {
                "gos_rt_vec_from_arr"
            };
            let new_fn =
                intrinsics.extern_fn(module, sym, &[types::I32, ptr_ty, types::I64], &[ptr_ty])?;
            let fref = module.declare_func_in_func(new_fn, builder.func);
            let elem_bytes = match args.first() {
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
                None => builder.ins().iconst(types::I64, 8),
            };
            let eb_i32 = coerce_arg_to(builder, elem_bytes, types::I32)?;
            let data_ptr = match args.get(1) {
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
            let data_coerced = coerce_arg_to(builder, data_ptr, ptr_ty)?;
            let len = match args.get(2) {
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
            let call = builder.ins().call(fref, &[eb_i32, data_coerced, len64]);
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
        "gos_rt_nested_arr_to_vec" => {
            // Converts `[Array{T,inner_len}; outer_len]` → `Vec<Vec<T>>`.
            // Args: (inner_elem_bytes: i64, inner_len: i64, raw: ptr, outer_len: i64)
            let new_fn = intrinsics.extern_fn(
                module,
                "gos_rt_nested_arr_to_vec",
                &[types::I64, types::I64, ptr_ty, types::I64],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(new_fn, builder.func);
            let inner_eb = match args.first() {
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
                None => builder.ins().iconst(types::I64, 8),
            };
            let inner_len_v = match args.get(1) {
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
            let raw_ptr = match args.get(2) {
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
            let outer_len_v = match args.get(3) {
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
            let raw_coerced = coerce_arg_to(builder, raw_ptr, ptr_ty)?;
            let call = builder
                .ins()
                .call(fref, &[inner_eb, inner_len_v, raw_coerced, outer_len_v]);
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
        // HashMap runtime. Key/value widths are hard-coded to 8
        // bytes (one word each) - matches the codegen's flat-
        // slot representation. Real per-type sizing needs MIR
        // plumbing that L3 didn't cover.
        "Map::new"
        | "collections::Map::new"
        | "std::collections::Map::new"
        | "HashMap::new"
        | "collections::HashMap::new"
        | "std::collections::HashMap::new"
        | "gos_rt_map_new" => {
            let typed_kinds = body
                .locals
                .get(destination.local.0 as usize)
                .and_then(|decl| match tcx.kind_of(decl.ty) {
                    TyKind::HashMap { key, value, .. } => {
                        let kind = |ty| match tcx.kind_of(ty) {
                            TyKind::Int(_) => Some(0),
                            TyKind::String => Some(1),
                            TyKind::Vec(elem) | TyKind::Slice(elem)
                                if matches!(
                                    tcx.kind_of(*elem),
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
            let call = if let Some((key_kind, val_kind)) = typed_kinds {
                let new_fn = intrinsics.extern_fn(
                    module,
                    "gos_rt_map_new_with_capacity_typed",
                    &[types::I32, types::I32, types::I64],
                    &[ptr_ty],
                )?;
                let fref = module.declare_func_in_func(new_fn, builder.func);
                let k = builder.ins().iconst(types::I32, key_kind);
                let v = builder.ins().iconst(types::I32, val_kind);
                let cap = builder.ins().iconst(types::I64, 0);
                builder.ins().call(fref, &[k, v, cap])
            } else {
                let new_fn = intrinsics.extern_fn(
                    module,
                    "gos_rt_map_new",
                    &[types::I32, types::I32],
                    &[ptr_ty],
                )?;
                let fref = module.declare_func_in_func(new_fn, builder.func);
                let k = builder.ins().iconst(types::I32, 8);
                let v = builder.ins().iconst(types::I32, 8);
                builder.ins().call(fref, &[k, v])
            };
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
        // `Map::from([...])` and the `{k: v}` literal that desugars to it. The
        // array of key/value tuples is materialised in a stack slot, so the
        // map is built by walking that slot and inserting each pair - the same
        // shape the LLVM tier emits, keeping a map literal off the bytecode
        // fallback in an otherwise promotable body.
        "Map::from"
        | "collections::Map::from"
        | "std::collections::Map::from"
        | "HashMap::from"
        | "collections::HashMap::from"
        | "std::collections::HashMap::from"
        | "BTreeMap::from"
        | "collections::BTreeMap::from"
        | "std::collections::BTreeMap::from"
            if args.len() == 1 =>
        {
            let arg = &args[0];
            let Operand::Copy(arg_place) = arg else {
                bail!("Map::from expects a materialised array place");
            };
            let arr_ty = {
                let mut t = resolve_place_ty(tcx, body, arg_place);
                while let TyKind::Ref { inner, .. } = tcx.kind_of(t) {
                    t = *inner;
                }
                t
            };
            let TyKind::Array { elem, len } = tcx.kind_of(arr_ty).clone() else {
                bail!("Map::from expects an array argument");
            };
            let TyKind::Tuple(fields) = tcx.kind_of(elem).clone() else {
                bail!("Map::from expects tuple array elements");
            };
            let [key_ty, val_ty] = fields.as_slice() else {
                bail!("Map::from expects key/value tuple elements");
            };
            // 0 = i64-shaped storage, 1 = String storage. Anything else has no
            // typed insert helper, so the body stays on bytecode.
            let storage_kind = |ty: Ty| match tcx.kind_of(ty) {
                TyKind::Int(_) | TyKind::Bool | TyKind::Char => Some(0i64),
                TyKind::String => Some(1i64),
                _ => None,
            };
            let (Some(key_kind), Some(val_kind)) = (storage_kind(*key_ty), storage_kind(*val_ty))
            else {
                bail!("Map::from supports i64-shaped and String keys and values");
            };
            let count = i64::try_from(len.to_usize()).unwrap_or(i64::MAX);
            let new_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_new_with_capacity_typed",
                &[types::I32, types::I32, types::I64],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(new_fn, builder.func);
            let k = builder.ins().iconst(types::I32, key_kind);
            let v = builder.ins().iconst(types::I32, val_kind);
            let cap = builder.ins().iconst(types::I64, count);
            let call = builder.ins().call(fref, &[k, v, cap]);
            let map_ptr = builder.inst_results(call)[0];

            let base =
                lower_place_address(module, builder, locals, body, tcx, arg_place, intrinsics)?;
            let stride = i64::from(type_slot_count(tcx, elem).max(1));
            let value_offset = i64::from(type_slot_count(tcx, *key_ty).max(1));
            let helper = match (key_kind, val_kind) {
                (0, 0) => "gos_rt_map_insert_i64_i64",
                (0, 1) => "gos_rt_map_insert_i64_str",
                (1, 0) => "gos_rt_map_insert_str_i64",
                _ => "gos_rt_map_insert_str_str",
            };
            let key_cl = if key_kind == 0 { types::I64 } else { ptr_ty };
            let val_cl = if val_kind == 0 { types::I64 } else { ptr_ty };
            let ins_fn = intrinsics.extern_fn(module, helper, &[ptr_ty, key_cl, val_cl], &[])?;
            let ins_ref = module.declare_func_in_func(ins_fn, builder.func);
            let retain_fn =
                intrinsics.extern_fn(module, "gos_rt_str_retain_typed", &[ptr_ty], &[])?;
            let retain_ref = module.declare_func_in_func(retain_fn, builder.func);
            for index in 0..len.to_usize() {
                let elem_slot = i64::try_from(index)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(stride);
                let key_off = i32::try_from(elem_slot.saturating_mul(8)).unwrap_or(i32::MAX);
                let val_off =
                    i32::try_from(elem_slot.saturating_add(value_offset).saturating_mul(8))
                        .unwrap_or(i32::MAX);
                let key_val = builder
                    .ins()
                    .load(key_cl, MemFlagsData::trusted(), base, key_off);
                let val_val = builder
                    .ins()
                    .load(val_cl, MemFlagsData::trusted(), base, val_off);
                // Each insert consumes a share of the strings it is handed, and
                // the array keeps the shares it holds, so the insert is handed
                // shares of its own.
                for (kind, value) in [(key_kind, key_val), (val_kind, val_val)] {
                    if kind == 1 {
                        let _ = builder.ins().call(retain_ref, &[value]);
                    }
                }
                let _ = builder.ins().call(ins_ref, &[map_ptr, key_val, val_val]);
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                map_ptr,
            );
            Ok(true)
        }
        "Map::with_capacity"
        | "collections::Map::with_capacity"
        | "std::collections::Map::with_capacity"
        | "HashMap::with_capacity"
        | "collections::HashMap::with_capacity"
        | "std::collections::HashMap::with_capacity"
        | "gos_rt_map_new_with_capacity" => {
            let typed_kinds = body
                .locals
                .get(destination.local.0 as usize)
                .and_then(|decl| match tcx.kind_of(decl.ty) {
                    TyKind::HashMap { key, value, .. } => {
                        let kind = |ty| match tcx.kind_of(ty) {
                            TyKind::Int(_) => Some(0),
                            TyKind::String => Some(1),
                            TyKind::Vec(elem) | TyKind::Slice(elem)
                                if matches!(
                                    tcx.kind_of(*elem),
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
            let Some((key_kind, val_kind)) = typed_kinds else {
                let new_fn = intrinsics.extern_fn(
                    module,
                    "gos_rt_map_new",
                    &[types::I32, types::I32],
                    &[ptr_ty],
                )?;
                let fref = module.declare_func_in_func(new_fn, builder.func);
                let width = builder.ins().iconst(types::I32, 8);
                let call = builder.ins().call(fref, &[width, width]);
                define_var_to(
                    builder,
                    locals,
                    &intrinsics.body_cl_types,
                    destination.local,
                    builder.inst_results(call)[0],
                );
                return Ok(true);
            };
            let new_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_new_with_capacity_typed",
                &[types::I32, types::I32, types::I64],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(new_fn, builder.func);
            let k = builder.ins().iconst(types::I32, key_kind);
            let v = builder.ins().iconst(types::I32, val_kind);
            let cap = match args.first() {
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
            let cap64 = coerce_arg_to(builder, cap, types::I64)?;
            let call = builder.ins().call(fref, &[k, v, cap64]);
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
        "Set::new" | "collections::Set::new" | "HashSet::new" | "collections::HashSet::new" => {
            let new_fn = intrinsics.extern_fn_by_name(module, "gos_rt_set_new")?;
            let fref = module.declare_func_in_func(new_fn, builder.func);
            let call = builder.ins().call(fref, &[]);
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
        "BTreeMap::new" | "collections::BTreeMap::new" => {
            // A `BTreeMap` is the ordinary map runtime with key-sorted
            // snapshots, so its constructor allocates the same `GosMap`.
            let new_fn = intrinsics.extern_fn_by_name(module, "gos_rt_map_new")?;
            let fref = module.declare_func_in_func(new_fn, builder.func);
            let call = builder.ins().call(fref, &[]);
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
        "gos_rt_map_len" => {
            let len_fn = intrinsics.extern_fn_by_name(module, "gos_rt_map_len")?;
            let fref = module.declare_func_in_func(len_fn, builder.func);
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
            let call = builder.ins().call(fref, &[m]);
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
        "gos_rt_map_insert" => {
            let ins_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_insert",
                &[ptr_ty, ptr_ty, ptr_ty],
                &[],
            )?;
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
            let k_val = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let v_val = match args.get(2) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let k64 = coerce_arg_to(builder, k_val, types::I64)?;
            let v64 = coerce_arg_to(builder, v_val, types::I64)?;
            let k_slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            let v_slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            let k_addr = builder.ins().stack_addr(ptr_ty, k_slot, 0);
            let v_addr = builder.ins().stack_addr(ptr_ty, v_slot, 0);
            builder.ins().store(MemFlagsData::trusted(), k64, k_addr, 0);
            builder.ins().store(MemFlagsData::trusted(), v64, v_addr, 0);
            let fref = module.declare_func_in_func(ins_fn, builder.func);
            let _ = builder.ins().call(fref, &[m, k_addr, v_addr]);
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
        "gos_rt_map_get" => {
            let get_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_get",
                &[ptr_ty, ptr_ty, ptr_ty],
                &[types::I32],
            )?;
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
            let k_val = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let k64 = coerce_arg_to(builder, k_val, types::I64)?;
            let k_slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            let out_slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            let k_addr = builder.ins().stack_addr(ptr_ty, k_slot, 0);
            let out_addr = builder.ins().stack_addr(ptr_ty, out_slot, 0);
            builder.ins().store(MemFlagsData::trusted(), k64, k_addr, 0);
            let fref = module.declare_func_in_func(get_fn, builder.func);
            let _ = builder.ins().call(fref, &[m, k_addr, out_addr]);
            let loaded = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), out_addr, 0);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                loaded,
            );
            Ok(true)
        }
        // Scalar-ABI insert: `m.insert(k, v)` for HashMap<K, V>
        // whose key + value widths are 8 bytes. Avoids the
        // stack-pointer dance the byte-erased
        // `gos_rt_map_insert` requires.
        "gos_rt_map_insert_i64_i64" => {
            let ins_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_insert_i64_i64",
                &[ptr_ty, types::I64, types::I64],
                &[],
            )?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let v_val = match args.get(2) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            // A map entry holds a counted copy of a by-value aggregate.
            let minted = match args.get(2) {
                Some(a) => super::lowering_calls::map_value_copy(
                    module, builder, intrinsics, body, tcx, a, v_val,
                )?,
                None => None,
            };
            let k = coerce_arg_to(builder, k_val, types::I64)?;
            let v = coerce_arg_to(builder, minted.unwrap_or(v_val), types::I64)?;
            let fref = module.declare_func_in_func(ins_fn, builder.func);
            let _ = builder.ins().call(fref, &[m, k, v]);
            if let Some(blob) = minted {
                super::lowering_calls::release_map_value_copy(module, builder, intrinsics, blob)?;
            }
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
        // Scalar-ABI lookup. Returns 0 when the key is absent
        // (matches the Option-flat happy-path encoding the rest
        // of the compiled tier already uses).
        "gos_rt_map_get_i64" => {
            let get_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_get_i64",
                &[ptr_ty, types::I64],
                &[types::I64],
            )?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let k = coerce_arg_to(builder, k_val, types::I64)?;
            let fref = module.declare_func_in_func(get_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k]);
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
        "gos_rt_map_remove_i64" => {
            let rm_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_remove_i64",
                &[ptr_ty, types::I64],
                &[types::I8],
            )?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let k = coerce_arg_to(builder, k_val, types::I64)?;
            let fref = module.declare_func_in_func(rm_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k]);
            let ok = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ok,
            );
            Ok(true)
        }
        "gos_rt_map_contains_key_i64" => {
            let ck_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_contains_key_i64",
                &[ptr_ty, types::I64],
                &[types::I8],
            )?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let k = coerce_arg_to(builder, k_val, types::I64)?;
            let fref = module.declare_func_in_func(ck_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k]);
            let ok = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ok,
            );
            Ok(true)
        }
        "gos_rt_map_insert_str_i64" | "gos_rt_map_insert_typed_str_i64" => {
            let symbol = if name == "gos_rt_map_insert_typed_str_i64" {
                "gos_rt_map_insert_typed_str_i64"
            } else {
                "gos_rt_map_insert_str_i64"
            };
            let ins_fn =
                intrinsics.extern_fn(module, symbol, &[ptr_ty, ptr_ty, types::I64], &[])?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
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
            let v_val = match args.get(2) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let v = coerce_arg_to(builder, v_val, types::I64)?;
            let fref = module.declare_func_in_func(ins_fn, builder.func);
            let _ = builder.ins().call(fref, &[m, k_val, v]);
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
        "gos_rt_map_get_str_i64" | "gos_rt_map_get_typed_str_i64" => {
            let symbol = if name == "gos_rt_map_get_typed_str_i64" {
                "gos_rt_map_get_typed_str_i64"
            } else {
                "gos_rt_map_get_str_i64"
            };
            let get_fn = intrinsics.extern_fn(module, symbol, &[ptr_ty, ptr_ty], &[types::I64])?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
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
            let fref = module.declare_func_in_func(get_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k_val]);
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
        "gos_rt_map_insert_str_str" => {
            let ins_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_insert_str_str",
                &[ptr_ty, ptr_ty, ptr_ty],
                &[],
            )?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
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
            let v_val = match args.get(2) {
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
            let fref = module.declare_func_in_func(ins_fn, builder.func);
            let _ = builder.ins().call(fref, &[m, k_val, v_val]);
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
        "gos_rt_map_get_str_str" => {
            let get_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_get_str_str",
                &[ptr_ty, ptr_ty],
                &[ptr_ty],
            )?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
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
            let fref = module.declare_func_in_func(get_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k_val]);
            let s = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                s,
            );
            Ok(true)
        }
        "gos_rt_map_contains_key_str" | "gos_rt_map_contains_key_typed_str" => {
            let symbol = if name == "gos_rt_map_contains_key_typed_str" {
                "gos_rt_map_contains_key_typed_str"
            } else {
                "gos_rt_map_contains_key_str"
            };
            let ck_fn = intrinsics.extern_fn(module, symbol, &[ptr_ty, ptr_ty], &[types::I8])?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
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
            let fref = module.declare_func_in_func(ck_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k_val]);
            let ok = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ok,
            );
            Ok(true)
        }
        "gos_rt_map_remove_str" | "gos_rt_map_remove_typed_str" => {
            let symbol = if name == "gos_rt_map_remove_typed_str" {
                "gos_rt_map_remove_typed_str"
            } else {
                "gos_rt_map_remove_str"
            };
            let rm_fn = intrinsics.extern_fn(module, symbol, &[ptr_ty, ptr_ty], &[types::I8])?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
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
            let fref = module.declare_func_in_func(rm_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k_val]);
            let ok = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ok,
            );
            Ok(true)
        }
        "gos_rt_map_clear" => {
            let cl_fn = intrinsics.extern_fn_by_name(module, "gos_rt_map_clear")?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let fref = module.declare_func_in_func(cl_fn, builder.func);
            let _ = builder.ins().call(fref, &[m]);
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
        // `m.inc_at(seq, start, len, by)` - zero-copy slice hash
        // for `HashMap<String, i64>`, matching Rust's
        // `*m.entry(&seq[i..i+k]).or_insert(0) += by`.
        _ => Ok(false),
    }
}
