//! Cranelift intrinsic lowering - opaque-handle family (close,
//! JSON, BTreeMap, arena/array iterators). Third partition in
//! the dispatch chain. Holds `lower_intrinsic_call_handles`.

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

pub(super) fn lower_intrinsic_call_handles(
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
        "gos_rt_map_inc_at_str_i64" => {
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_map_inc_at_str_i64",
                &[ptr_ty, ptr_ty, types::I64, types::I64, types::I64],
                &[types::I64],
            )?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let seq = match args.get(1) {
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
            let start_v = match args.get(2) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let len_v = match args.get(3) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let by_v = match args.get(4) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 1),
            };
            let start64 = coerce_arg_to(builder, start_v, types::I64)?;
            let len64 = coerce_arg_to(builder, len_v, types::I64)?;
            let by64 = coerce_arg_to(builder, by_v, types::I64)?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[m, seq, start64, len64, by64]);
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
        // Drop helpers emitted by the MIR's drop-insertion pass.
        // Each frees a heap-owned runtime container so the
        // process doesn't leak its contents across calls.
        "gos_rt_map_free"
        | "gos_rt_vec_free"
        | "gos_rt_vec_retain"
        | "gos_rt_vec_mark_rc_elems"
        | "gos_rt_vec_mark_vec_elems"
        | "gos_rt_vec_compact_elems"
        | "gos_rt_set_free"
        | "gos_rt_arr_iter_free" => {
            let static_name: &'static str = match name {
                "gos_rt_map_free" => "gos_rt_map_free",
                "gos_rt_vec_free" => "gos_rt_vec_free",
                "gos_rt_vec_retain" => "gos_rt_vec_retain",
                "gos_rt_vec_mark_rc_elems" => "gos_rt_vec_mark_rc_elems",
                "gos_rt_vec_mark_vec_elems" => "gos_rt_vec_mark_vec_elems",
                "gos_rt_vec_compact_elems" => "gos_rt_vec_compact_elems",
                "gos_rt_set_free" => "gos_rt_set_free",
                "gos_rt_arr_iter_free" => "gos_rt_arr_iter_free",
                _ => unreachable!(),
            };
            let f = intrinsics.extern_fn(module, static_name, &[ptr_ty], &[])?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let fref = module.declare_func_in_func(f, builder.func);
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
        // HashMap iteration helpers - each returns a *mut GosVec
        // snapshot of the requested column so the for-loop lowerer
        // can iterate it through the regular gos_rt_vec_* helpers.
        // The aggregate-key snapshot shares the same shape, rebuilding each
        // struct or tuple key from the map's own slot descriptor.
        "gos_rt_map_keys_i64"
        | "gos_rt_map_values_i64"
        | "gos_rt_map_keys_str"
        | "gos_rt_map_values_str"
        | "gos_rt_map_keys_skey" => {
            let static_name: &'static str = match name {
                "gos_rt_map_keys_i64" => "gos_rt_map_keys_i64",
                "gos_rt_map_values_i64" => "gos_rt_map_values_i64",
                "gos_rt_map_keys_str" => "gos_rt_map_keys_str",
                "gos_rt_map_values_str" => "gos_rt_map_values_str",
                "gos_rt_map_keys_skey" => "gos_rt_map_keys_skey",
                _ => unreachable!(),
            };
            let rt_fn = intrinsics.extern_fn(module, static_name, &[ptr_ty], &[ptr_ty])?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[m]);
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
        "gos_rt_map_inc_i64" => {
            let inc_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_inc_i64",
                &[ptr_ty, types::I64, types::I64],
                &[types::I64],
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
            let by_val = match args.get(2) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 1),
            };
            let k64 = coerce_arg_to(builder, k_val, types::I64)?;
            let by64 = coerce_arg_to(builder, by_val, types::I64)?;
            let fref = module.declare_func_in_func(inc_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k64, by64]);
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
        "gos_rt_map_inc_str_i64" | "gos_rt_map_inc_typed_str_i64" => {
            let symbol = if name == "gos_rt_map_inc_typed_str_i64" {
                "gos_rt_map_inc_typed_str_i64"
            } else {
                "gos_rt_map_inc_str_i64"
            };
            let inc_fn = intrinsics.extern_fn(
                module,
                symbol,
                &[ptr_ty, ptr_ty, types::I64],
                &[types::I64],
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
            let k = match args.get(1) {
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
            let by_val = match args.get(2) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 1),
            };
            let k_ptr = coerce_arg_to(builder, k, ptr_ty)?;
            let by64 = coerce_arg_to(builder, by_val, types::I64)?;
            let fref = module.declare_func_in_func(inc_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k_ptr, by64]);
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
        "gos_rt_map_get_or_i64" => {
            let get_or_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_get_or_i64",
                &[ptr_ty, types::I64, types::I64],
                &[types::I64],
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
            let d_val = match args.get(2) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let k64 = coerce_arg_to(builder, k_val, types::I64)?;
            let d64 = coerce_arg_to(builder, d_val, types::I64)?;
            let fref = module.declare_func_in_func(get_or_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k64, d64]);
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
        // String-keyed `get_or` for `HashMap<String, i64>`. The key
        // travels as a `*const c_char`, the default and the result
        // are both i64.
        "gos_rt_map_get_or_str_i64" | "gos_rt_map_get_or_typed_str_i64" => {
            let symbol = if name == "gos_rt_map_get_or_typed_str_i64" {
                "gos_rt_map_get_or_typed_str_i64"
            } else {
                "gos_rt_map_get_or_str_i64"
            };
            let get_or_fn = intrinsics.extern_fn(
                module,
                symbol,
                &[ptr_ty, ptr_ty, types::I64],
                &[types::I64],
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
            let d_val = match args.get(2) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let d64 = coerce_arg_to(builder, d_val, types::I64)?;
            let fref = module.declare_func_in_func(get_or_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k_val, d64]);
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
        // String-keyed, string-valued `get_or`. Default and result
        // travel as `*const c_char`.
        "gos_rt_map_get_or_str_str" => {
            let get_or_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_get_or_str_str",
                &[ptr_ty, ptr_ty, ptr_ty],
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
            let d_val = match args.get(2) {
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
            let fref = module.declare_func_in_func(get_or_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k_val, d_val]);
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
        // i64-keyed, string-valued `get_or` for `HashMap<i64, String>`.
        "gos_rt_map_get_or_i64_str" => {
            let get_or_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_get_or_i64_str",
                &[ptr_ty, types::I64, ptr_ty],
                &[ptr_ty],
            )?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let d_val = match args.get(2) {
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
            let k64 = coerce_arg_to(builder, k_val, types::I64)?;
            let fref = module.declare_func_in_func(get_or_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k64, d_val]);
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
        // `m.insert(k: i64, v: String)` for `HashMap<i64, String>`.
        "gos_rt_map_insert_i64_str" => {
            let ins_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_insert_i64_str",
                &[ptr_ty, types::I64, ptr_ty],
                &[],
            )?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
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
            let k64 = coerce_arg_to(builder, k_val, types::I64)?;
            let fref = module.declare_func_in_func(ins_fn, builder.func);
            let _ = builder.ins().call(fref, &[m, k64, v_val]);
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
        // `m.get(k: i64) -> String` for `HashMap<i64, String>`.
        "gos_rt_map_get_i64_str" => {
            let get_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_get_i64_str",
                &[ptr_ty, types::I64],
                &[ptr_ty],
            )?;
            let m = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let k_val = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let k64 = coerce_arg_to(builder, k_val, types::I64)?;
            let fref = module.declare_func_in_func(get_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k64]);
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
        "gos_rt_map_remove" => {
            let rm_fn = intrinsics.extern_fn(
                module,
                "gos_rt_map_remove",
                &[ptr_ty, ptr_ty],
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
            let k_addr = builder.ins().stack_addr(ptr_ty, k_slot, 0);
            builder.ins().store(MemFlagsData::trusted(), k64, k_addr, 0);
            let fref = module.declare_func_in_func(rm_fn, builder.func);
            let call = builder.ins().call(fref, &[m, k_addr]);
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
        // JSON runtime - every helper accepts an opaque
        // `*mut GosJson` pointer so the codegen treats them as
        // pointer-sized values. The MIR rewriter routes
        // `value.field` on a `json::Value` receiver into a
        // `gos_rt_json_get(value, "field")` call before this
        // backend sees it.
        "gos_rt_json_parse" => {
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let v = emit_rt_call_by_name(module, builder, intrinsics, "gos_rt_json_parse", &[arg])?
                .ok_or_else(|| anyhow!("gos_rt_json_parse returns a value"))?;
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                v,
            );
            Ok(true)
        }
        "gos_rt_json_value_string" | "gos_rt_json_value_array" | "gos_rt_json_value_object" => {
            let helper: &'static str = match name {
                "gos_rt_json_value_string" => "gos_rt_json_value_string",
                "gos_rt_json_value_array" => "gos_rt_json_value_array",
                _ => "gos_rt_json_value_object",
            };
            let rt_fn = intrinsics.extern_fn(module, helper, &[ptr_ty], &[ptr_ty])?;
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[arg]);
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
        "gos_rt_json_value_object_n" => {
            let rt_fn = intrinsics.extern_fn(
                module,
                "gos_rt_json_value_object_n",
                &[types::I64, ptr_ty],
                &[ptr_ty],
            )?;
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
            let pairs = match args.get(1) {
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
            let n64 = coerce_arg_to(builder, n, types::I64)?;
            let pairs_ptr = coerce_arg_to(builder, pairs, ptr_ty)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[n64, pairs_ptr]);
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
        "gos_rt_json_value_int" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_value_int")?;
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
            let n = coerce_arg_to(builder, n, types::I64)?;
            let call = builder.ins().call(fref, &[n]);
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
        "gos_rt_json_value_float" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_value_float")?;
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
            let call = builder.ins().call(fref, &[x]);
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
        "gos_rt_json_value_bool" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_value_bool")?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let b = match args.first() {
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
            let b = coerce_arg_to(builder, b, types::I32)?;
            let call = builder.ins().call(fref, &[b]);
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
        "gos_rt_json_value_null" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_value_null")?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[]);
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
        "gos_rt_json_render" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_render")?;
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[arg]);
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
        "gos_rt_json_as_str" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_as_str")?;
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[arg]);
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
        "gos_rt_json_get" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_get")?;
            let recv = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let key = match args.get(1) {
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
            let key_ptr = coerce_arg_to(builder, key, ptr_ty)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[recv, key_ptr]);
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
        "gos_rt_json_get_opt" => {
            let rt_fn = intrinsics.extern_fn(
                module,
                "gos_rt_json_get_opt",
                &[ptr_ty, ptr_ty],
                &[ptr_ty],
            )?;
            let recv = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let key = match args.get(1) {
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
            let key_ptr = coerce_arg_to(builder, key, ptr_ty)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[recv, key_ptr]);
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
        "gos_rt_json_keys_opt" => {
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let v =
                emit_rt_call_by_name(module, builder, intrinsics, "gos_rt_json_keys_opt", &[arg])?
                    .ok_or_else(|| anyhow!("gos_rt_json_keys_opt returns a value"))?;
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                v,
            );
            Ok(true)
        }
        "gos_rt_json_as_array_opt" => {
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let v = emit_rt_call_by_name(
                module,
                builder,
                intrinsics,
                "gos_rt_json_as_array_opt",
                &[arg],
            )?
            .ok_or_else(|| anyhow!("gos_rt_json_as_array_opt returns a value"))?;
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                v,
            );
            Ok(true)
        }
        "gos_rt_json_at" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_at")?;
            let recv = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let idx = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let idx64 = coerce_arg_to(builder, idx, types::I64)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[recv, idx64]);
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
        "gos_rt_json_len" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_len")?;
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[arg]);
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
        "gos_rt_json_as_i64" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_as_i64")?;
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[arg]);
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
        "gos_rt_json_as_f64" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_as_f64")?;
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[arg]);
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
        "gos_rt_json_is_null" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_is_null")?;
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[arg]);
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
        "gos_rt_json_as_bool" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_as_bool")?;
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let call = builder.ins().call(fref, &[arg]);
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
        "gos_rt_json_identity" => {
            let arg = lower_first_ptr_arg(module, builder, locals, body, tcx, args, intrinsics)?;
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                arg,
            );
            Ok(true)
        }
        // Channels delegate to the gossamer-runtime staticlib.
        // Element size is hard-coded to i64-equivalent (8 bytes) -
        // every scalar and every GC pointer fits in that word.
        // `cap = 0` is an unbuffered rendezvous channel. Explicit
        // unbounded queue channels use `cap = -1`.
        //
        // The frontend types `channel()` as a tuple
        // `(Sender<T>, Receiver<T>)` - two slots - so the user's
        // `let tx, rx = channel()` / `pair.0` / `pair.1`
        // pattern projects with a 0/8-byte offset. We allocate
        // a 16-byte stack slot here and store the channel
        // pointer at *both* offsets so subsequent
        // `pair.0` / `pair.1` projections hand the same
        // channel handle to send and receive sites. Without
        // this, `pair.1` reads garbage from the second tuple
        // slot and `recv` no-ops on a null channel pointer.
        "channel"
        | "channel::new"
        | "channel::unbounded"
        | "sync::channel"
        | "sync::channel_unbounded"
        | "std::sync::channel_unbounded"
        | "sync::Channel::new"
        | "gos_rt_chan_new"
        | "Channel::new" => {
            let new_fn = intrinsics.extern_fn(
                module,
                "gos_rt_chan_new",
                &[types::I32, types::I64],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(new_fn, builder.func);
            let elem = builder.ins().iconst(types::I32, 8);
            let cap = if matches!(
                name,
                "channel::unbounded" | "sync::channel_unbounded" | "std::sync::channel_unbounded"
            ) {
                builder.ins().iconst(types::I64, -1)
            } else if let Some(arg) = args.first() {
                let raw = lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?;
                coerce_arg_to(builder, raw, types::I64)?
            } else {
                builder.ins().iconst(types::I64, 0)
            };
            let call = builder.ins().call(fref, &[elem, cap]);
            let chan_ptr = builder.inst_results(call)[0];
            // 16-byte tuple slot; write chan_ptr to offsets 0
            // and 8 so both `Sender` and `Receiver` projections
            // observe the same handle.
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                16,
                3, // 8-byte alignment
            ));
            let base = builder.ins().stack_addr(ptr_ty, slot, 0);
            builder.ins().store(
                MemFlagsData::trusted(),
                chan_ptr,
                base,
                ir::immediates::Offset32::new(0),
            );
            builder.ins().store(
                MemFlagsData::trusted(),
                chan_ptr,
                base,
                ir::immediates::Offset32::new(8),
            );
            // Mark the destination as a 2-slot aggregate so
            // projections lower as memory loads from `base + N*8`
            // rather than reading a Variable directly.
            intrinsics.local_slots.insert(destination.local, 2);
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                base,
            );
            Ok(true)
        }
        "gos_rt_chan_send" | "send" => {
            // Stack-spill the value word so the runtime's
            // `gos_rt_chan_send(chan, *const u8)` can memcpy it in.
            let chan = match args.first() {
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
                None => bail!("chan_send: missing channel arg"),
            };
            let mut value = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            // A multi-slot aggregate value lives in the sender frame's
            // backing slot; storing its address into the channel hands the
            // receiver - on its own goroutine stack - a pointer that
            // dangles once the sender frame is reused. Heap-copy it so the
            // channel carries a stable pointer the receiver owns.
            if let Some(slots) = args
                .get(1)
                .and_then(|a| operand_aggregate_slots(body, tcx, a))
            {
                value = clone_aggregate_value(module, builder, intrinsics, value, slots)?;
            }
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
            let send_fn = intrinsics.extern_fn_by_name(module, "gos_rt_chan_send")?;
            let fref = module.declare_func_in_func(send_fn, builder.func);
            let _ = builder.ins().call(fref, &[chan, slot_addr]);
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
        "gos_rt_chan_try_send" | "try_send" => {
            let chan = match args.first() {
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
                None => bail!("chan_try_send: missing channel arg"),
            };
            let mut value = match args.get(1) {
                Some(a) => lower_operand(module, builder, locals, body, tcx, a, None, intrinsics)?,
                None => builder.ins().iconst(types::I64, 0),
            };
            if let Some(slots) = args
                .get(1)
                .and_then(|a| operand_aggregate_slots(body, tcx, a))
            {
                value = clone_aggregate_value(module, builder, intrinsics, value, slots)?;
            }
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
            let send_fn = intrinsics.extern_fn(
                module,
                "gos_rt_chan_try_send",
                &[ptr_ty, ptr_ty],
                &[types::I32],
            )?;
            let fref = module.declare_func_in_func(send_fn, builder.func);
            let call = builder.ins().call(fref, &[chan, slot_addr]);
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
        "gos_rt_chan_try_recv_option" | "gos_rt_chan_try_recv" | "try_recv" => {
            let chan = match args.first() {
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
                None => bail!("chan_try_recv: missing channel arg"),
            };
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_chan_try_recv_option",
                &[ptr_ty],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[chan]);
            let opt_ptr = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                opt_ptr,
            );
            Ok(true)
        }
        "gos_rt_chan_close" | "close" => {
            let chan = match args.first() {
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
                None => bail!("chan_close: missing channel arg"),
            };
            let close_fn = intrinsics.extern_fn_by_name(module, "gos_rt_chan_close")?;
            let fref = module.declare_func_in_func(close_fn, builder.func);
            let _ = builder.ins().call(fref, &[chan]);
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
        "gos_rt_chan_recv_option" | "gos_rt_chan_recv" | "recv" => {
            let chan = match args.first() {
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
                None => bail!("chan_recv: missing channel arg"),
            };
            let opt_ptr = emit_rt_call_by_name(
                module,
                builder,
                intrinsics,
                "gos_rt_chan_recv_option",
                &[chan],
            )?
            .ok_or_else(|| anyhow!("gos_rt_chan_recv_option returns a carrier"))?;
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                opt_ptr,
            );
            Ok(true)
        }
        // ---- Mutex<T> primitive ----
        "Mutex::new" | "sync::Mutex::new" | "mutex::new" | "gos_rt_mutex_new" => {
            let new_fn = intrinsics.extern_fn_by_name(module, "gos_rt_mutex_new")?;
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
        "gos_rt_mutex_lock" => {
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
                None => bail!("mutex_lock: missing receiver"),
            };
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_mutex_lock")?;
            let fref = module.declare_func_in_func(f, builder.func);
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
        "gos_rt_mutex_unlock" => {
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
                None => bail!("mutex_unlock: missing receiver"),
            };
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_mutex_unlock")?;
            let fref = module.declare_func_in_func(f, builder.func);
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
        // ---- WaitGroup primitive ----
        "WaitGroup::new" | "sync::WaitGroup::new" | "wg::new" | "gos_rt_wg_new" => {
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_wg_new")?;
            let fref = module.declare_func_in_func(f, builder.func);
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
        "gos_rt_wg_add" => {
            let wg = match args.first() {
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
                None => bail!("wg_add: missing receiver"),
            };
            let n = match args.get(1) {
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
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_wg_add",
                &[ptr_ty, types::I64],
                &[types::I64],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[wg, n64]);
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
        "gos_rt_wg_done" => {
            let wg = match args.first() {
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
                None => bail!("wg_done: missing receiver"),
            };
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_wg_done")?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[wg]);
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
        "gos_rt_wg_wait" => {
            let wg = match args.first() {
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
                None => bail!("wg_wait: missing receiver"),
            };
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_wg_wait")?;
            let fref = module.declare_func_in_func(f, builder.func);
            let _ = builder.ins().call(fref, &[wg]);
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
        // ---- Heap [i64] primitive ----
        "I64Vec::new" | "heap_i64::new" | "gos_rt_heap_i64_new" => {
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
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_heap_i64_new")?;
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
        "gos_rt_heap_i64_get" => {
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
                None => bail!("heap_i64_get: missing receiver"),
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
                "gos_rt_heap_i64_get",
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
        _ => Ok(false),
    }
}
