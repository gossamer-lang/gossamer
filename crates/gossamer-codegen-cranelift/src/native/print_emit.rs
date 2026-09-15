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

use super::*;

/// Reclaims a rendering an aggregate formatter allocated for one print or
/// join step.
///
/// The formatters answer a fresh `String` this frame is the only holder of,
/// and both consumers copy out of it - `gos_rt_print_str` writes the bytes and
/// `gos_rt_concat_str` appends them - so the rendering is dead once the call
/// it fed returns.
pub(super) fn free_rendered_string(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    rendered: ir::Value,
) -> Result<()> {
    let f = intrinsics.extern_fn_by_name(module, "gos_rt_str_free_typed")?;
    let fref = module.declare_func_in_func(f, builder.func);
    builder.ins().call(fref, &[rendered]);
    Ok(())
}

pub(super) fn emit_per_arg_print(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    args: &[Operand],
    intrinsics: &mut IntrinsicContext,
    separator: &str,
) -> Result<()> {
    let ptr_ty = module.target_config().pointer_type();
    let print_str = intrinsics.extern_fn_by_name(module, "gos_rt_print_str")?;
    let print_i64 = intrinsics.extern_fn_by_name(module, "gos_rt_print_i64")?;
    let print_f64 = intrinsics.extern_fn_by_name(module, "gos_rt_print_f64")?;
    let print_bool = intrinsics.extern_fn_by_name(module, "gos_rt_print_bool")?;
    let print_char = intrinsics.extern_fn_by_name(module, "gos_rt_print_char")?;
    let sep_data = if separator.is_empty() {
        None
    } else {
        Some(intrinsics.intern_string(module, separator)?)
    };
    for (idx, arg) in args.iter().enumerate() {
        if idx > 0 {
            if let Some(data) = sep_data {
                let ptr = intrinsics.static_string_body_ptr(module, builder, data);
                let fref = module.declare_func_in_func(print_str, builder.func);
                builder.ins().call(fref, &[ptr]);
            }
        }
        let kind = operand_print_kind(body, tcx, arg);
        if let PrintKind::Unsupported(label) = kind {
            // 0.8.0: no `<value>` placeholder fallback. A type the
            // print path doesn't know how to render is a compile
            // error, not a runtime "<value>" string - the user
            // wants real Display lowering, not a stub.
            bail!(
                "native codegen: refusing to emit '<value>' placeholder for print of unsupported \
                operand kind {label}; add a Display dispatch for this type or convert it explicitly"
            );
        }
        let value = lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?;
        let ty = value_type(value, builder);
        // When Var(_) resolves to StrPtr but the value is a non-pointer int (e.g. `!bool`
        // returning I8), use the correct formatter rather than passing a narrow int as a ptr.
        let kind = if matches!(kind, PrintKind::StrPtr) && ty.is_int() && ty != ptr_ty {
            if ty == types::I8 {
                PrintKind::Bool
            } else {
                PrintKind::Int
            }
        } else {
            kind
        };
        // A slice carries the `Vec` kind under the bare-bracket spelling.
        let (kind, bare) = match kind {
            PrintKind::Seq(inner) => (*inner, true),
            other => (other, false),
        };
        match kind {
            // The spelling was taken off the kind above, so a sequence
            // reaching here carries the kind it wraps.
            PrintKind::Seq(_) => unreachable!("the sequence spelling is unwrapped above"),
            PrintKind::StrPtr => {
                let fref = module.declare_func_in_func(print_str, builder.func);
                builder.ins().call(fref, &[value]);
            }
            PrintKind::Int => {
                let n = if ty.bits() < 64 {
                    builder.ins().sextend(types::I64, value)
                } else {
                    value
                };
                let fref = module.declare_func_in_func(print_i64, builder.func);
                builder.ins().call(fref, &[n]);
            }
            PrintKind::Uint => {
                // Zero-extend to 64 bits so we don't sign-extend
                // a sub-i64 unsigned value into a giant negative
                // number. Then route to `gos_rt_print_u64`.
                let n = if ty.bits() < 64 {
                    builder.ins().uextend(types::I64, value)
                } else {
                    value
                };
                let print_u64 = intrinsics.extern_fn_by_name(module, "gos_rt_print_u64")?;
                let fref = module.declare_func_in_func(print_u64, builder.func);
                builder.ins().call(fref, &[n]);
            }
            PrintKind::Float => {
                let d = if ty == types::F32 {
                    builder.ins().fpromote(types::F64, value)
                } else {
                    value
                };
                let fref = module.declare_func_in_func(print_f64, builder.func);
                builder.ins().call(fref, &[d]);
            }
            PrintKind::Bool => {
                let b = if ty.bits() < 32 {
                    builder.ins().uextend(types::I32, value)
                } else if ty.bits() > 32 {
                    builder.ins().ireduce(types::I32, value)
                } else {
                    value
                };
                let fref = module.declare_func_in_func(print_bool, builder.func);
                builder.ins().call(fref, &[b]);
            }
            PrintKind::Char => {
                let c = if ty.bits() > 32 {
                    builder.ins().ireduce(types::I32, value)
                } else if ty.bits() < 32 {
                    builder.ins().uextend(types::I32, value)
                } else {
                    value
                };
                let fref = module.declare_func_in_func(print_char, builder.func);
                builder.ins().call(fref, &[c]);
            }
            PrintKind::VecI64 => emit_vec_print(
                module,
                builder,
                "gos_rt_vec_format_i64",
                value,
                print_str,
                intrinsics,
                Some(bare),
            )?,
            PrintKind::VecUint => emit_vec_print(
                module,
                builder,
                "gos_rt_vec_format_u64",
                value,
                print_str,
                intrinsics,
                Some(bare),
            )?,
            PrintKind::VecF64 => emit_vec_print(
                module,
                builder,
                "gos_rt_vec_format_f64",
                value,
                print_str,
                intrinsics,
                Some(bare),
            )?,
            PrintKind::VecBool => emit_vec_print(
                module,
                builder,
                "gos_rt_vec_format_bool",
                value,
                print_str,
                intrinsics,
                Some(bare),
            )?,
            PrintKind::VecString => emit_vec_print(
                module,
                builder,
                "gos_rt_vec_format_string",
                value,
                print_str,
                intrinsics,
                Some(bare),
            )?,
            PrintKind::VecVecI64 => emit_vec_print(
                module,
                builder,
                "gos_rt_vec_format_vec_i64",
                value,
                print_str,
                intrinsics,
                Some(bare),
            )?,
            PrintKind::VecVecString => emit_vec_print(
                module,
                builder,
                "gos_rt_vec_format_vec_string",
                value,
                print_str,
                intrinsics,
                Some(bare),
            )?,
            PrintKind::ArrI64(len) => emit_arr_print(
                module,
                builder,
                "gos_rt_arr_format_i64",
                value,
                len,
                print_str,
                intrinsics,
            )?,
            PrintKind::ArrF64(len) => emit_arr_print(
                module,
                builder,
                "gos_rt_arr_format_f64",
                value,
                len,
                print_str,
                intrinsics,
            )?,
            PrintKind::ArrBool(len) => emit_arr_print(
                module,
                builder,
                "gos_rt_arr_format_bool",
                value,
                len,
                print_str,
                intrinsics,
            )?,
            PrintKind::ArrString(len) => emit_arr_print(
                module,
                builder,
                "gos_rt_arr_format_string",
                value,
                len,
                print_str,
                intrinsics,
            )?,
            PrintKind::ArrArrI64(n, m) => emit_arr_arr_print(
                module,
                builder,
                "gos_rt_arr_format_arr_i64",
                value,
                n,
                m,
                print_str,
                intrinsics,
            )?,
            PrintKind::ArrArrF64(n, m) => emit_arr_arr_print(
                module,
                builder,
                "gos_rt_arr_format_arr_f64",
                value,
                n,
                m,
                print_str,
                intrinsics,
            )?,
            PrintKind::ArrArrBool(n, m) => emit_arr_arr_print(
                module,
                builder,
                "gos_rt_arr_format_arr_bool",
                value,
                n,
                m,
                print_str,
                intrinsics,
            )?,
            PrintKind::JsonValue => emit_vec_print(
                module,
                builder,
                "gos_rt_json_display",
                value,
                print_str,
                intrinsics,
                None,
            )?,
            PrintKind::DynValue => emit_vec_print(
                module,
                builder,
                "gos_rt_dyn_format",
                value,
                print_str,
                intrinsics,
                None,
            )?,
            PrintKind::ErrorMessage => {
                // Display renders the colon-joined cause chain;
                // `.message()` keeps `gos_rt_error_message`.
                let error_msg_fn = intrinsics.extern_fn_by_name(module, "gos_rt_error_display")?;
                let fref = module.declare_func_in_func(error_msg_fn, builder.func);
                let call = builder.ins().call(fref, &[value]);
                let msg = builder.inst_results(call)[0];
                let fref2 = module.declare_func_in_func(print_str, builder.func);
                builder.ins().call(fref2, &[msg]);
                free_rendered_string(module, builder, intrinsics, msg)?;
            }
            PrintKind::Tuple => {
                let s =
                    emit_tuple_format_value(module, builder, body, tcx, arg, value, intrinsics)?;
                let fref = module.declare_func_in_func(print_str, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::VecTuple(tags, arity) => {
                let s = emit_vec_tuple_format_value(
                    module, builder, value, &tags, arity, bare, intrinsics,
                )?;
                let fref = module.declare_func_in_func(print_str, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::Map => {
                let s = emit_map_format_value(module, builder, value, intrinsics)?;
                let fref = module.declare_func_in_func(print_str, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::MapTagged(key_tag, val_tag) => {
                let s = emit_map_format_tagged_value(
                    module, builder, value, key_tag, val_tag, intrinsics,
                )?;
                let fref = module.declare_func_in_func(print_str, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::HandleFormat(symbol) => {
                let s = emit_handle_format_value(module, builder, value, symbol, intrinsics)?;
                let fref = module.declare_func_in_func(print_str, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::SetFormat(symbol, ordered) => {
                let s = emit_set_format_value(module, builder, value, symbol, ordered, intrinsics)?;
                let fref = module.declare_func_in_func(print_str, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::Option(payload_kind) => {
                let s = emit_debug_option_value(module, builder, value, payload_kind, intrinsics)?;
                let fref = module.declare_func_in_func(print_str, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::Result(ok_kind, err_kind) => {
                let s =
                    emit_debug_result_value(module, builder, value, ok_kind, err_kind, intrinsics)?;
                let fref = module.declare_func_in_func(print_str, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::Unsupported(_) => unreachable!("checked above"),
        }
    }
    Ok(())
}

/// Emits `gos_rt_debug_option(opt_i128, kind)` and returns the rendered string
/// pointer. `value` is the by-value `i128` Option enum.
pub(super) fn emit_debug_option_value(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    value: ir::Value,
    payload_kind: u8,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    let kind_v = builder.ins().iconst(types::I64, i64::from(payload_kind));
    let result = emit_win64_rt_call(
        module,
        builder,
        intrinsics,
        "gos_rt_debug_option",
        &[types::I128, types::I64],
        Some(ptr_ty),
        &[value, kind_v],
    )?;
    Ok(result.expect("gos_rt_debug_option returns a pointer"))
}

/// Emits `gos_rt_debug_result(res_i128, ok_kind, err_kind)` and returns the
/// rendered string pointer. `value` is the by-value `i128` Result enum.
pub(super) fn emit_debug_result_value(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    value: ir::Value,
    ok_kind: u8,
    err_kind: u8,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    let ok_v = builder.ins().iconst(types::I64, i64::from(ok_kind));
    let err_v = builder.ins().iconst(types::I64, i64::from(err_kind));
    let result = emit_win64_rt_call(
        module,
        builder,
        intrinsics,
        "gos_rt_debug_result",
        &[types::I128, types::I64, types::I64],
        Some(ptr_ty),
        &[value, ok_v, err_v],
    )?;
    Ok(result.expect("gos_rt_debug_result returns a pointer"))
}

pub(super) fn emit_arr_print(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    helper_name: &'static str,
    value: ir::Value,
    len: i64,
    print_str: cranelift_module::FuncId,
    intrinsics: &mut IntrinsicContext,
) -> Result<()> {
    let ptr_ty = module.target_config().pointer_type();
    let f = intrinsics.extern_fn(module, helper_name, &[ptr_ty, types::I64], &[ptr_ty])?;
    let fref = module.declare_func_in_func(f, builder.func);
    let len_v = builder.ins().iconst(types::I64, len);
    let call = builder.ins().call(fref, &[value, len_v]);
    let result = builder.inst_results(call)[0];
    let print_ref = module.declare_func_in_func(print_str, builder.func);
    builder.ins().call(print_ref, &[result]);
    free_rendered_string(module, builder, intrinsics, result)
}

/// Prints a flat nested fixed array through its `(ptr, outer, inner)`
/// runtime formatter.
pub(super) fn emit_arr_arr_print(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    helper_name: &'static str,
    value: ir::Value,
    outer: i64,
    inner: i64,
    print_str: cranelift_module::FuncId,
    intrinsics: &mut IntrinsicContext,
) -> Result<()> {
    let ptr_ty = module.target_config().pointer_type();
    let f = intrinsics.extern_fn(
        module,
        helper_name,
        &[ptr_ty, types::I64, types::I64],
        &[ptr_ty],
    )?;
    let fref = module.declare_func_in_func(f, builder.func);
    let outer_v = builder.ins().iconst(types::I64, outer);
    let inner_v = builder.ins().iconst(types::I64, inner);
    let call = builder.ins().call(fref, &[value, outer_v, inner_v]);
    let result = builder.inst_results(call)[0];
    let print_ref = module.declare_func_in_func(print_str, builder.func);
    builder.ins().call(print_ref, &[result]);
    free_rendered_string(module, builder, intrinsics, result)
}

/// Renders `value` through `helper_name` and prints it.
///
/// `bare` is `Some` for a helper that takes the sequence spelling - a `Vec`
/// writes `#[..]` and a slice `[..]` - and `None` for one rendering a shape
/// with only one way to write it.
pub(super) fn emit_vec_print(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    helper_name: &'static str,
    value: ir::Value,
    print_str: cranelift_module::FuncId,
    intrinsics: &mut IntrinsicContext,
    bare: Option<bool>,
) -> Result<()> {
    let ptr_ty = module.target_config().pointer_type();
    let params: &[ir::Type] = match bare {
        Some(_) => &[ptr_ty, types::I32],
        None => &[ptr_ty],
    };
    let f = intrinsics.extern_fn(module, helper_name, params, &[ptr_ty])?;
    let fref = module.declare_func_in_func(f, builder.func);
    let call = match bare {
        Some(bare) => {
            let spelling = builder.ins().iconst(types::I32, i64::from(bare));
            builder.ins().call(fref, &[value, spelling])
        }
        None => builder.ins().call(fref, &[value]),
    };
    let s = builder.inst_results(call)[0];
    let pref = module.declare_func_in_func(print_str, builder.func);
    builder.ins().call(pref, &[s]);
    free_rendered_string(module, builder, intrinsics, s)
}

/// Emits `gos_rt_tuple_format(buf, n, tags)` and returns the rendered
/// string pointer. `value` is the address of the tuple's flat
/// `[N x i64]` slot buffer; the tag array is materialised as a
/// read-only data object holding one byte per element.
pub(super) fn emit_tuple_format_value(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    body: &Body,
    tcx: &TyCtxt,
    arg: &Operand,
    value: ir::Value,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    let Some((count, tags)) = tuple_tags(tcx, body, arg) else {
        bail!("native codegen: tuple element type is not formattable on the compiled tier");
    };
    let n = count as i64;
    let tags_data = intrinsics.intern_tuple_tags(module, &tags)?;
    let tags_global = module.declare_data_in_func(tags_data, builder.func);
    let tags_ptr = builder.ins().symbol_value(ptr_ty, tags_global);
    let n_v = builder.ins().iconst(types::I64, n);
    let f = intrinsics.extern_fn_by_name(module, "gos_rt_tuple_format")?;
    let fref = module.declare_func_in_func(f, builder.func);
    let call = builder.ins().call(fref, &[value, n_v, tags_ptr]);
    Ok(builder.inst_results(call)[0])
}

/// Emits `gos_rt_vec_format_tuple(vec, arity, tags, bare)` and returns the
/// rendered string pointer. `tags` is the per-field tag stream every element
/// renders through, interned before the parallel lowering phase.
pub(super) fn emit_vec_tuple_format_value(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    value: ir::Value,
    tags: &[u8],
    arity: i64,
    bare: bool,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    let tags_data = intrinsics.intern_tuple_tags(module, tags)?;
    let tags_global = module.declare_data_in_func(tags_data, builder.func);
    let tags_ptr = builder.ins().symbol_value(ptr_ty, tags_global);
    let format_fn = intrinsics.extern_fn(
        module,
        "gos_rt_vec_format_tuple",
        &[ptr_ty, types::I64, ptr_ty, types::I32],
        &[ptr_ty],
    )?;
    let format_ref = module.declare_func_in_func(format_fn, builder.func);
    let arity_v = builder.ins().iconst(types::I64, arity);
    let spelling = builder.ins().iconst(types::I32, i64::from(bare));
    let call = builder
        .ins()
        .call(format_ref, &[value, arity_v, tags_ptr, spelling]);
    Ok(builder.inst_results(call)[0])
}

/// Emits `gos_rt_map_format(map)` and returns the rendered string
/// pointer. `value` is the `GosMap` pointer.
/// Emits `gos_rt_map_format_tagged(map, key_tag, val_tag, null, 0)` for a map
/// whose key or value carries a declared width, returning the rendered string
/// pointer.
pub(super) fn emit_map_format_tagged_value(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    value: ir::Value,
    key_tag: u8,
    val_tag: u8,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    let f = intrinsics.extern_fn_by_name(module, "gos_rt_map_format_tagged")?;
    let fref = module.declare_func_in_func(f, builder.func);
    let key_tag = builder.ins().iconst(types::I64, i64::from(key_tag));
    let val_tag = builder.ins().iconst(types::I64, i64::from(val_tag));
    let null = builder.ins().iconst(ptr_ty, 0);
    let aux_n = builder.ins().iconst(types::I64, 0);
    let call = builder
        .ins()
        .call(fref, &[value, key_tag, val_tag, null, aux_n]);
    Ok(builder.inst_results(call)[0])
}

pub(super) fn emit_map_format_value(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    value: ir::Value,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let f = intrinsics.extern_fn_by_name(module, "gos_rt_map_format")?;
    let fref = module.declare_func_in_func(f, builder.func);
    let call = builder.ins().call(fref, &[value]);
    Ok(builder.inst_results(call)[0])
}

/// Emits `<sym>(handle)` for a container handle - `Deque` / `Queue` /
/// `Stack` / `MaxHeap` / `MinHeap` - and returns the rendered string
/// pointer.
pub(super) fn emit_handle_format_value(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    value: ir::Value,
    symbol: &'static str,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let f = intrinsics.extern_fn_by_name(module, symbol)?;
    let fref = module.declare_func_in_func(f, builder.func);
    let call = builder.ins().call(fref, &[value]);
    Ok(builder.inst_results(call)[0])
}

/// Emits `<sym>(set, ordered)` for a `HashSet` / `BTreeSet` handle and
/// returns the rendered string pointer.
pub(super) fn emit_set_format_value(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    value: ir::Value,
    symbol: &'static str,
    ordered: i32,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let f = intrinsics.extern_fn_by_name(module, symbol)?;
    let fref = module.declare_func_in_func(f, builder.func);
    let ordered_v = builder.ins().iconst(types::I32, i64::from(ordered));
    let call = builder.ins().call(fref, &[value, ordered_v]);
    Ok(builder.inst_results(call)[0])
}

pub(super) fn emit_args_to_concat_string(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    args: &[Operand],
    intrinsics: &mut IntrinsicContext,
    separator: &str,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    let empty_data = intrinsics.intern_string(module, "")?;
    if args.is_empty() {
        return Ok(intrinsics.static_string_body_ptr(module, builder, empty_data));
    }

    // Use the runtime's thread-local concat buffer (the same path
    // `__concat` takes for `format!`) instead of chaining N-1
    // `gos_rt_str_concat` calls. Each pairwise concat allocates a
    // throwaway String and then drops the previous accumulator; the
    // batched buffer appends bytes into one growing buffer and
    // hands back a single owned String at the end.
    let init = intrinsics.extern_fn_by_name(module, "gos_rt_concat_init")?;
    let init_ref = module.declare_func_in_func(init, builder.func);
    builder.ins().call(init_ref, &[]);

    let sep_data = if separator.is_empty() {
        None
    } else {
        Some(intrinsics.intern_string(module, separator)?)
    };

    for (idx, arg) in args.iter().enumerate() {
        if idx > 0 {
            if let Some(data) = sep_data {
                let sep_ptr = intrinsics.static_string_body_ptr(module, builder, data);
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[sep_ptr]);
            }
        }
        let kind = operand_print_kind(body, tcx, arg);
        if let PrintKind::Unsupported(label) = kind {
            bail!(
                "native codegen: cannot stringify a value of {label} type - \
                 the compiled tier has no Display dispatch yet"
            );
        }
        let value = lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?;
        let ty = value_type(value, builder);
        // Same guard as in emit_per_arg_print: Var(_) → StrPtr but value is a narrow int.
        let kind = if matches!(kind, PrintKind::StrPtr) && ty.is_int() && ty != ptr_ty {
            if ty == types::I8 {
                PrintKind::Bool
            } else {
                PrintKind::Int
            }
        } else {
            kind
        };
        // A slice carries the `Vec` kind under the bare-bracket spelling.
        let (kind, bare) = match kind {
            PrintKind::Seq(inner) => (*inner, true),
            other => (other, false),
        };
        match kind {
            // The spelling was taken off the kind above, so a sequence
            // reaching here carries the kind it wraps.
            PrintKind::Seq(_) => unreachable!("the sequence spelling is unwrapped above"),
            PrintKind::StrPtr => {
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[value]);
            }
            PrintKind::Int => {
                let n = if ty.bits() < 64 {
                    builder.ins().sextend(types::I64, value)
                } else {
                    value
                };
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_i64")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[n]);
            }
            PrintKind::Uint => {
                let n = if ty.bits() < 64 {
                    builder.ins().uextend(types::I64, value)
                } else {
                    value
                };
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_u64")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[n]);
            }
            PrintKind::Float => {
                let d = if ty == types::F32 {
                    builder.ins().fpromote(types::F64, value)
                } else {
                    value
                };
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_f64")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[d]);
            }
            PrintKind::Bool => {
                let b = if ty.bits() < 32 {
                    builder.ins().uextend(types::I32, value)
                } else if ty.bits() > 32 {
                    builder.ins().ireduce(types::I32, value)
                } else {
                    value
                };
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_bool")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[b]);
            }
            PrintKind::Char => {
                let c = if ty.bits() > 32 {
                    builder.ins().ireduce(types::I32, value)
                } else if ty.bits() < 32 {
                    builder.ins().uextend(types::I32, value)
                } else {
                    value
                };
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_char")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[c]);
            }
            PrintKind::VecI64
            | PrintKind::VecUint
            | PrintKind::VecF64
            | PrintKind::VecBool
            | PrintKind::VecString
            | PrintKind::VecVecI64
            | PrintKind::VecVecString => {
                let helper = match kind {
                    PrintKind::VecI64 => "gos_rt_vec_format_i64",
                    PrintKind::VecUint => "gos_rt_vec_format_u64",
                    PrintKind::VecF64 => "gos_rt_vec_format_f64",
                    PrintKind::VecBool => "gos_rt_vec_format_bool",
                    PrintKind::VecString => "gos_rt_vec_format_string",
                    PrintKind::VecVecI64 => "gos_rt_vec_format_vec_i64",
                    PrintKind::VecVecString => "gos_rt_vec_format_vec_string",
                    _ => unreachable!(),
                };
                let format_fn =
                    intrinsics.extern_fn(module, helper, &[ptr_ty, types::I32], &[ptr_ty])?;
                let format_ref = module.declare_func_in_func(format_fn, builder.func);
                let spelling = builder.ins().iconst(types::I32, i64::from(bare));
                let call = builder.ins().call(format_ref, &[value, spelling]);
                let s = builder.inst_results(call)[0];
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::ArrI64(_)
            | PrintKind::ArrF64(_)
            | PrintKind::ArrBool(_)
            | PrintKind::ArrString(_) => {
                let (helper, len) = match kind {
                    PrintKind::ArrI64(n) => ("gos_rt_arr_format_i64", n),
                    PrintKind::ArrF64(n) => ("gos_rt_arr_format_f64", n),
                    PrintKind::ArrBool(n) => ("gos_rt_arr_format_bool", n),
                    PrintKind::ArrString(n) => ("gos_rt_arr_format_string", n),
                    _ => unreachable!(),
                };
                let format_fn =
                    intrinsics.extern_fn(module, helper, &[ptr_ty, types::I64], &[ptr_ty])?;
                let format_ref = module.declare_func_in_func(format_fn, builder.func);
                let len_v = builder.ins().iconst(types::I64, len);
                let call = builder.ins().call(format_ref, &[value, len_v]);
                let s = builder.inst_results(call)[0];
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::ArrArrI64(..) | PrintKind::ArrArrF64(..) | PrintKind::ArrArrBool(..) => {
                let (helper, n, m) = match kind {
                    PrintKind::ArrArrI64(n, m) => ("gos_rt_arr_format_arr_i64", n, m),
                    PrintKind::ArrArrF64(n, m) => ("gos_rt_arr_format_arr_f64", n, m),
                    PrintKind::ArrArrBool(n, m) => ("gos_rt_arr_format_arr_bool", n, m),
                    _ => unreachable!(),
                };
                let format_fn = intrinsics.extern_fn(
                    module,
                    helper,
                    &[ptr_ty, types::I64, types::I64],
                    &[ptr_ty],
                )?;
                let format_ref = module.declare_func_in_func(format_fn, builder.func);
                let n_v = builder.ins().iconst(types::I64, n);
                let m_v = builder.ins().iconst(types::I64, m);
                let call = builder.ins().call(format_ref, &[value, n_v, m_v]);
                let s = builder.inst_results(call)[0];
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::DynValue => {
                let render_fn = intrinsics.extern_fn_by_name(module, "gos_rt_dyn_format")?;
                let render_ref = module.declare_func_in_func(render_fn, builder.func);
                let call = builder.ins().call(render_ref, &[value]);
                let s = builder.inst_results(call)[0];
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::JsonValue => {
                let render_fn = intrinsics.extern_fn_by_name(module, "gos_rt_json_display")?;
                let render_ref = module.declare_func_in_func(render_fn, builder.func);
                let call = builder.ins().call(render_ref, &[value]);
                let s = builder.inst_results(call)[0];
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::ErrorMessage => {
                // Display renders the colon-joined cause chain;
                // `.message()` keeps `gos_rt_error_message`.
                let error_msg_fn = intrinsics.extern_fn_by_name(module, "gos_rt_error_display")?;
                let err_ref = module.declare_func_in_func(error_msg_fn, builder.func);
                let call = builder.ins().call(err_ref, &[value]);
                let s = builder.inst_results(call)[0];
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::Tuple => {
                let s =
                    emit_tuple_format_value(module, builder, body, tcx, arg, value, intrinsics)?;
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::VecTuple(tags, arity) => {
                let s = emit_vec_tuple_format_value(
                    module, builder, value, &tags, arity, bare, intrinsics,
                )?;
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::Map => {
                let s = emit_map_format_value(module, builder, value, intrinsics)?;
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::MapTagged(key_tag, val_tag) => {
                let s = emit_map_format_tagged_value(
                    module, builder, value, key_tag, val_tag, intrinsics,
                )?;
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::HandleFormat(symbol) => {
                let s = emit_handle_format_value(module, builder, value, symbol, intrinsics)?;
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::SetFormat(symbol, ordered) => {
                let s = emit_set_format_value(module, builder, value, symbol, ordered, intrinsics)?;
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::Option(payload_kind) => {
                let s = emit_debug_option_value(module, builder, value, payload_kind, intrinsics)?;
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::Result(ok_kind, err_kind) => {
                let s =
                    emit_debug_result_value(module, builder, value, ok_kind, err_kind, intrinsics)?;
                let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                let fref = module.declare_func_in_func(f, builder.func);
                builder.ins().call(fref, &[s]);
                free_rendered_string(module, builder, intrinsics, s)?;
            }
            PrintKind::Unsupported(_) => unreachable!("filtered above"),
        }
    }

    let finish = intrinsics.extern_fn_by_name(module, "gos_rt_concat_finish")?;
    let finish_ref = module.declare_func_in_func(finish, builder.func);
    let call = builder.ins().call(finish_ref, &[]);
    Ok(builder.inst_results(call)[0])
}
