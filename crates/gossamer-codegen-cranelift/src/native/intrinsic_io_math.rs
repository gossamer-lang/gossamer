//! Cranelift intrinsic lowering - IO / math / time / OS family.
//!
//! Holds `lower_intrinsic_call_io_math`, the first partition in
//! the cranelift dispatch chain. Covers `gos_rt_print_*`,
//! `gos_rt_eprint*`, `gos_rt_fmt_prec`, `__concat`, the math
//! shims (`sqrt`/`sin`/`cos`/`exp`/`ln`/`abs`/`floor`/`ceil`/`pow`),
//! `gos_rt_time_*`, and the `gos_rt_os_*` / `gos_rt_env_*`
//! entries. See sibling files for the other partitions
//! (`intrinsic_collections`, `intrinsic_handles`,
//! `intrinsic_string`); the four files are walked in declaration
//! order from `intrinsic::lower_intrinsic_call`.

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
use gossamer_mir::{Body, ConstValue, Local, Operand, Place, Projection, UnOp};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};
use rayon::prelude::*;

use super::*;

pub(super) fn lower_intrinsic_call_io_math(
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
        // `__debug` is the `{:?}` channel: identical to `__concat` except a
        // float always renders with a fractional part or an exponent.
        "__concat" | "__debug" => {
            // Build the concatenated string into the runtime's
            // thread-local concat buffer, then return a fresh
            // String pointer. Lets `format!` produce a real value
            // that callers (errors::new, struct fields) can
            // consume past the surrounding `println`/`print`.
            if !destination.projection.is_empty() {
                bail!("native codegen: {name} destination cannot have projections");
            }
            let concat_f64 = if name == "__debug" {
                "gos_rt_concat_f64_debug"
            } else {
                "gos_rt_concat_f64"
            };
            let init = intrinsics.extern_fn_by_name(module, "gos_rt_concat_init")?;
            let init_ref = module.declare_func_in_func(init, builder.func);
            builder.ins().call(init_ref, &[]);
            for arg in args {
                let kind = operand_print_kind(body, tcx, arg);
                let value =
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?;
                let ty = value_type(value, builder);
                // Var(_) → StrPtr fallback, but value is a narrow int (e.g. `!bool` → I8).
                let kind = if matches!(kind, PrintKind::StrPtr) && ty.is_int() && ty != ptr_ty {
                    if ty == types::I8 {
                        PrintKind::Bool
                    } else {
                        PrintKind::Int
                    }
                } else {
                    kind
                };
                // A slice carries the `Vec` kind under the bare spelling.
                let (kind, bare) = match kind {
                    PrintKind::Seq(inner) => (*inner, true),
                    other => (other, false),
                };
                match kind {
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
                        let f = intrinsics.extern_fn(
                            module,
                            "gos_rt_concat_i64",
                            &[types::I64],
                            &[],
                        )?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[n]);
                    }
                    PrintKind::Uint => {
                        // Zero-extend so values >= 2^63 don't get
                        // sign-flipped on the way to the i64 helper.
                        let n = if ty.bits() < 64 {
                            builder.ins().uextend(types::I64, value)
                        } else {
                            value
                        };
                        let f = intrinsics.extern_fn(
                            module,
                            "gos_rt_concat_u64",
                            &[types::I64],
                            &[],
                        )?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[n]);
                    }
                    PrintKind::Float => {
                        let d = if ty == types::F32 {
                            builder.ins().fpromote(types::F64, value)
                        } else {
                            value
                        };
                        let f = intrinsics.extern_fn(module, concat_f64, &[types::F64], &[])?;
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
                        let f = intrinsics.extern_fn(
                            module,
                            "gos_rt_concat_bool",
                            &[types::I32],
                            &[],
                        )?;
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
                        let f = intrinsics.extern_fn(
                            module,
                            "gos_rt_concat_char",
                            &[types::I32],
                            &[],
                        )?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[c]);
                    }
                    // The spelling was taken off the kind above, so a
                    // sequence reaching here carries the kind it wraps.
                    PrintKind::Seq(_) => unreachable!("the sequence spelling is unwrapped above"),
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
                        let format_fn = intrinsics.extern_fn(
                            module,
                            helper,
                            &[ptr_ty, types::I32],
                            &[ptr_ty],
                        )?;
                        let format_ref = module.declare_func_in_func(format_fn, builder.func);
                        let spelling = builder.ins().iconst(types::I32, i64::from(bare));
                        let call = builder.ins().call(format_ref, &[value, spelling]);
                        let s = builder.inst_results(call)[0];
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
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
                        let format_fn = intrinsics.extern_fn(
                            module,
                            helper,
                            &[ptr_ty, types::I64],
                            &[ptr_ty],
                        )?;
                        let format_ref = module.declare_func_in_func(format_fn, builder.func);
                        let len_v = builder.ins().iconst(types::I64, len);
                        let call = builder.ins().call(format_ref, &[value, len_v]);
                        let s = builder.inst_results(call)[0];
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
                    }
                    PrintKind::ArrArrI64(..)
                    | PrintKind::ArrArrF64(..)
                    | PrintKind::ArrArrBool(..) => {
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
                    }
                    PrintKind::DynValue => {
                        let render_fn = intrinsics.extern_fn(
                            module,
                            "gos_rt_dyn_format",
                            &[ptr_ty],
                            &[ptr_ty],
                        )?;
                        let render_ref = module.declare_func_in_func(render_fn, builder.func);
                        let call = builder.ins().call(render_ref, &[value]);
                        let s = builder.inst_results(call)[0];
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
                    }
                    PrintKind::JsonValue => {
                        let render_fn = intrinsics.extern_fn(
                            module,
                            "gos_rt_json_display",
                            &[ptr_ty],
                            &[ptr_ty],
                        )?;
                        let render_ref = module.declare_func_in_func(render_fn, builder.func);
                        let call = builder.ins().call(render_ref, &[value]);
                        let s = builder.inst_results(call)[0];
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
                    }
                    PrintKind::ErrorMessage => {
                        // `{}` on an error renders the colon-joined cause
                        // chain, the same text the print path and the LLVM
                        // tier render; `.message()` is the top link alone.
                        let error_msg_fn = intrinsics.extern_fn(
                            module,
                            "gos_rt_error_display",
                            &[ptr_ty],
                            &[ptr_ty],
                        )?;
                        let err_ref = module.declare_func_in_func(error_msg_fn, builder.func);
                        let call = builder.ins().call(err_ref, &[value]);
                        let s = builder.inst_results(call)[0];
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
                    }
                    PrintKind::Tuple => {
                        let s = emit_tuple_format_value(
                            module, builder, body, tcx, arg, value, intrinsics,
                        )?;
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
                    }
                    PrintKind::Map => {
                        let s = emit_map_format_value(module, builder, value, intrinsics)?;
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
                    }
                    PrintKind::MapTagged(key_tag, val_tag) => {
                        let s = super::print_emit::emit_map_format_tagged_value(
                            module, builder, value, key_tag, val_tag, intrinsics,
                        )?;
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
                    }
                    PrintKind::HandleFormat(symbol) => {
                        let s =
                            emit_handle_format_value(module, builder, value, symbol, intrinsics)?;
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
                    }
                    PrintKind::SetFormat(symbol, ordered) => {
                        let s = emit_set_format_value(
                            module, builder, value, symbol, ordered, intrinsics,
                        )?;
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
                    }
                    PrintKind::Option(payload_kind) => {
                        let s = emit_debug_option_value(
                            module,
                            builder,
                            value,
                            payload_kind,
                            intrinsics,
                        )?;
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
                    }
                    PrintKind::Result(ok_kind, err_kind) => {
                        let s = emit_debug_result_value(
                            module, builder, value, ok_kind, err_kind, intrinsics,
                        )?;
                        let f = intrinsics.extern_fn_by_name(module, "gos_rt_concat_str")?;
                        let fref = module.declare_func_in_func(f, builder.func);
                        builder.ins().call(fref, &[s]);
                    }
                    PrintKind::Unsupported(label) => {
                        // A value this path cannot render is left to the
                        // bytecode VM, which renders every shape: emitting a
                        // placeholder here would answer `<value>` where the
                        // other tiers answer the value itself.
                        bail!(
                            "native codegen: no Display lowering for concat operand kind {label}"
                        );
                    }
                }
            }
            let finish = intrinsics.extern_fn_by_name(module, "gos_rt_concat_finish")?;
            let finish_ref = module.declare_func_in_func(finish, builder.func);
            let call = builder.ins().call(finish_ref, &[]);
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
        // `__fmt_prec(value, prec)` - emitted by macro expansion for
        // `{:.N}` specs. A number routes through `gos_rt_f64_prec_to_str`
        // and text through `gos_rt_str_prec_to_str`, so either way the
        // result is a String the surrounding `__concat` consumes.
        "__fmt_prec" => {
            if args.len() != 2 {
                bail!("native codegen: __fmt_prec expects exactly two arguments");
            }
            // The helper is chosen from the argument's own type: a
            // `String` const or a local whose type is `String` is text.
            let is_text = match &args[0] {
                Operand::Const(gossamer_mir::ConstValue::Str(_)) => true,
                Operand::Copy(place) => matches!(
                    tcx.kind_of(body.local_ty(place.local)),
                    gossamer_types::TyKind::String
                ),
                _ => false,
            };
            let value_raw = lower_operand(
                module, builder, locals, body, tcx, &args[0], None, intrinsics,
            )?;
            let prec_raw = lower_operand(
                module, builder, locals, body, tcx, &args[1], None, intrinsics,
            )?;
            let prec_ty = value_type(prec_raw, builder);
            let prec = if prec_ty.bits() < 64 {
                builder.ins().sextend(types::I64, prec_raw)
            } else if prec_ty.bits() > 64 {
                builder.ins().ireduce(types::I64, prec_raw)
            } else {
                prec_raw
            };
            let (helper, value, arg_ty) = if is_text {
                ("gos_rt_str_prec_to_str", value_raw, ptr_ty)
            } else {
                let value_ty = value_type(value_raw, builder);
                let widened = if value_ty == types::F64 {
                    value_raw
                } else if value_ty == types::F32 {
                    builder.ins().fpromote(types::F64, value_raw)
                } else {
                    builder.ins().fcvt_from_sint(types::F64, value_raw)
                };
                ("gos_rt_f64_prec_to_str", widened, types::F64)
            };
            let f = intrinsics.extern_fn(module, helper, &[arg_ty, types::I64], &[ptr_ty])?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[value, prec]);
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
        // `io::stdout()` / `io::stderr()` / `io::stdin()` -
        // return an opaque pointer to a static `GosStream`.
        // Method dispatch on the returned value routes to the
        // `gos_rt_stream_*` helpers below.
        "io::stdout" | "io::stderr" | "io::stdin" => {
            let rt_name = match name {
                "io::stdout" => "gos_rt_io_stdout",
                "io::stderr" => "gos_rt_io_stderr",
                "io::stdin" => "gos_rt_io_stdin",
                _ => unreachable!(),
            };
            let rt_fn = intrinsics.extern_fn(module, rt_name, &[], &[ptr_ty])?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
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
        // Method-side routing for stream values. The MIR
        // method-dispatch table maps `stream.write_byte(b)`
        // etc. to these symbols (`receiver` is arg 0).
        "gos_rt_stream_write_byte" => {
            let rt_fn = intrinsics.extern_fn(
                module,
                "gos_rt_stream_write_byte",
                &[ptr_ty, types::I64],
                &[],
            )?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let stream = match args.first() {
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
            let b = match args.get(1) {
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
            let b64 = coerce_arg_to(builder, b, types::I64)?;
            let _ = builder.ins().call(fref, &[stream, b64]);
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
        "gos_rt_stream_write_byte_array" => {
            // Bulk byte write - `out.write_byte_array(arr, len)`.
            // `arr` is a `[i64; N]` whose flat-slot layout
            // means each byte sits in the low 8 bits of an
            // `i64`; the runtime walks it once and packs into
            // the stdout buffer.
            let rt_fn = intrinsics.extern_fn(
                module,
                "gos_rt_stream_write_byte_array",
                &[ptr_ty, ptr_ty, types::I64],
                &[],
            )?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let stream = match args.first() {
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
            let arr = match args.get(1) {
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
            let _ = builder.ins().call(fref, &[stream, arr, len64]);
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
        "gos_rt_stream_write_str" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_stream_write_str")?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let stream = match args.first() {
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
            let s = match args.get(1) {
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
            let _ = builder.ins().call(fref, &[stream, s]);
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
        "gos_rt_stream_flush" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_stream_flush")?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let stream = match args.first() {
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
            let _ = builder.ins().call(fref, &[stream]);
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
        "gos_rt_stream_read_line" => {
            let stream = match args.first() {
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
            let buf = match args.get(1) {
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
            // The `Option<String>` return is a two-word carrier, which the
            // Win64 ABI hands back in a vector register: marshal it through
            // the wire-shape call.
            let result = emit_win64_rt_call(
                module,
                builder,
                intrinsics,
                "gos_rt_stream_read_line",
                &[ptr_ty, ptr_ty],
                Some(types::I128),
                &[stream, buf],
            )?
            .unwrap_or_else(|| builder.ins().iconst(types::I128, 0));
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                result,
            );
            Ok(true)
        }
        "gos_rt_stream_read_to_string" => {
            let rt_fn = intrinsics.extern_fn(
                module,
                "gos_rt_stream_read_to_string",
                &[ptr_ty],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(rt_fn, builder.func);
            let stream = match args.first() {
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
            let call = builder.ins().call(fref, &[stream]);
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
        "println" | "print" => {
            // Per-arg dispatch: each operand is printed through
            // the runtime helper matching its MIR type
            // (`gos_rt_print_str` for strings, `_i64` for
            // integers, `_f64` for floats, `_bool` / `_char`).
            // This is the same machinery `__concat` uses; bare
            // `println(5i64)` and interpolated `println!("{n}")`
            // therefore share one code path.
            //
            // The whole sequence runs under the process-global
            // stdout lock so concurrent goroutines on other OS
            // threads can't interleave bytes mid-line. The lock
            // is reentrant - each per-arg helper takes it again
            // - so this outer acquire merely extends the held
            // duration to cover the entire multi-call sequence.
            let acquire_fn = intrinsics.extern_fn_by_name(module, "gos_rt_stdout_acquire")?;
            let release_fn = intrinsics.extern_fn_by_name(module, "gos_rt_stdout_release")?;
            let acquire_ref = module.declare_func_in_func(acquire_fn, builder.func);
            let release_ref = module.declare_func_in_func(release_fn, builder.func);
            let _ = builder.ins().call(acquire_ref, &[]);
            emit_per_arg_print(module, builder, locals, body, tcx, args, intrinsics, " ")?;
            if name == "println" {
                let println_fn = intrinsics.extern_fn_by_name(module, "gos_rt_println")?;
                let pl_ref = module.declare_func_in_func(println_fn, builder.func);
                let _ = builder.ins().call(pl_ref, &[]);
            }
            let _ = builder.ins().call(release_ref, &[]);
            if !destination.projection.is_empty() {
                bail!("native codegen: intrinsic destination cannot have projections");
            }
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
        "eprintln" | "eprint" => {
            // Build the formatted message via the same per-arg
            // concat machinery `panic` uses, then drain it through
            // the stderr writer (which flushes stdout first so
            // diagnostic order is preserved). Keeps eprint output
            // off stdout without parallel `_err` versions of every
            // per-type print helper.
            let s = emit_args_to_concat_string(
                module, builder, locals, body, tcx, args, intrinsics, " ",
            )?;
            let eprint_fn = intrinsics.extern_fn_by_name(module, "gos_rt_eprint_str")?;
            let eprint_ref = module.declare_func_in_func(eprint_fn, builder.func);
            builder.ins().call(eprint_ref, &[s]);
            if name == "eprintln" {
                let nl_fn = intrinsics.extern_fn_by_name(module, "gos_rt_eprintln")?;
                let nl_ref = module.declare_func_in_func(nl_fn, builder.func);
                let _ = builder.ins().call(nl_ref, &[]);
            }
            if !destination.projection.is_empty() {
                bail!("native codegen: intrinsic destination cannot have projections");
            }
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
        "gos_fn_addr" => {
            // Returns the address of a named function as an i64 so
            // closures and other first-class callable values can
            // stash a function pointer in their heap env. The
            // argument is a `Const(Str(name))` naming the target.
            let Some(Operand::Const(ConstValue::Str(name))) = args.first() else {
                bail!("native codegen: gos_fn_addr requires a const-string name argument");
            };
            // Names starting with `gos_rt_` are runtime extern
            // symbols (the Fn-trait coercion trampolines plus a
            // handful of other one-off helpers MIR may stash into
            // a heap env). Declare them through the module's
            // intrinsic-fn machinery so the linker resolves them
            // against `gossamer-runtime`.
            // Win64: a body the Rust runtime enters as
            // `extern "C" fn(..) -> i128` answers its two-word carrier in the
            // register pair, while the runtime reads a vector register. The
            // wrapper emitted for exactly those bodies re-emits the carrier
            // there, so its address is the one that crosses.
            let func_id = if let Some(id) = intrinsics.cabi_callbacks.get(name).copied() {
                id
            } else if let Some(id) = intrinsics.functions.get(name).copied() {
                id
            } else if let Some(id) = intrinsics.externs.get(name.as_str()).copied() {
                // Runtime extern symbol - `gos_rt_router_serve` and
                // the other stateful-type serve dispatchers are
                // declared via `extern_fn_by_name` at codegen init
                // (loop over `gossamer_abi::REGISTRY`) and live in
                // `intrinsics.externs`. Surface them here so
                // `gos_fn_addr` can hand back their address for
                // handler-fn-ptr indirection through
                // `gos_rt_http_serve` etc.
                id
            } else if name.starts_with("__fn_thunk_") {
                // Per-shape callable thunk. The name encodes the
                // typed FnTrait sig (`__fn_thunk_<inputs>_<ret>`);
                // synthesise a real function in this module that
                // takes (env, typed_args...) -> typed_ret and
                // forwards to the real fn at env+8 with the right
                // calling convention. Replaces the earlier
                // mono-i64 `gos_rt_fn_tramp_N` family which
                // silently mangled f64 / bool / aggregate args.
                define_shape_thunk(module, intrinsics, name)?
            } else {
                bail!("gos_fn_addr: unknown fn `{name}`")
            };
            let func_ref = module.declare_func_in_func(func_id, builder.func);
            let addr = builder.ins().func_addr(ptr_ty, func_ref);
            let as_i64 = if ptr_ty == types::I64 {
                addr
            } else {
                builder.ins().uextend(types::I64, addr)
            };
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_fn_addr destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                as_i64,
            );
            Ok(true)
        }
        "gos_alloc" => {
            // Heap allocator primitive: forwards to libc `malloc`.
            // Single argument is the size in bytes; the return value
            // is a raw pointer (i64 on 64-bit, zero-extended on 32-bit).
            let malloc = intrinsics.extern_fn(module, "malloc", &[ptr_ty], &[ptr_ty])?;
            let malloc_ref = module.declare_func_in_func(malloc, builder.func);
            let size_val = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let size_ptr = if ptr_ty == types::I64 {
                size_val
            } else {
                builder.ins().ireduce(ptr_ty, size_val)
            };
            let call_inst = builder.ins().call(malloc_ref, &[size_ptr]);
            let raw_ptr = builder.inst_results(call_inst)[0];
            let as_i64 = if ptr_ty == types::I64 {
                raw_ptr
            } else {
                builder.ins().uextend(types::I64, raw_ptr)
            };
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_alloc destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                as_i64,
            );
            Ok(true)
        }
        // The enum-keyed map helpers take `(map, key_node, desc_symbol,
        // [word])`. The third argument names the module-global structural
        // descriptor blob, so it lowers as that global's address rather than
        // as a string constant.
        "gos_rt_map_insert_ekey_opt"
        | "gos_rt_map_get_ekey_opt"
        | "gos_rt_map_contains_ekey"
        | "gos_rt_map_pop_ekey"
        | "gos_rt_map_get_or_ekey"
        | "gos_rt_map_or_insert_ekey"
        | "gos_rt_map_inc_ekey" => {
            let (static_name, ret_ty, has_word): (&'static str, _, _) = match name {
                "gos_rt_map_insert_ekey_opt" => ("gos_rt_map_insert_ekey_opt", types::I128, true),
                "gos_rt_map_get_ekey_opt" => ("gos_rt_map_get_ekey_opt", types::I128, false),
                "gos_rt_map_pop_ekey" => ("gos_rt_map_pop_ekey", types::I128, false),
                "gos_rt_map_contains_ekey" => ("gos_rt_map_contains_ekey", types::I8, false),
                "gos_rt_map_get_or_ekey" => ("gos_rt_map_get_or_ekey", types::I64, true),
                "gos_rt_map_or_insert_ekey" => ("gos_rt_map_or_insert_ekey", types::I64, true),
                _ => ("gos_rt_map_inc_ekey", types::I64, true),
            };
            let mut call_args = Vec::with_capacity(4);
            for index in 0..2 {
                let v = match args.get(index) {
                    Some(arg) => {
                        lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                    }
                    None => builder.ins().iconst(ptr_ty, 0),
                };
                call_args.push(coerce_arg_to(builder, v, ptr_ty).unwrap_or(v));
            }
            let desc_val = match args.get(2) {
                Some(Operand::Const(ConstValue::Str(sym))) if !sym.is_empty() => {
                    let Some(blob) = tcx.rc_meta(sym) else {
                        bail!("native codegen: {static_name} references unknown meta `{sym}`");
                    };
                    let data_id = intrinsics.intern_rc_meta(module, sym, blob)?;
                    let gv = module.declare_data_in_func(data_id, builder.func);
                    builder.ins().symbol_value(ptr_ty, gv)
                }
                _ => builder.ins().iconst(ptr_ty, 0),
            };
            call_args.push(desc_val);
            let mut sig_params = vec![ptr_ty, ptr_ty, ptr_ty];
            if has_word {
                let w = match args.get(3) {
                    Some(arg) => {
                        lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                    }
                    None => builder.ins().iconst(types::I64, 0),
                };
                call_args.push(coerce_arg_to(builder, w, types::I64).unwrap_or(w));
                sig_params.push(types::I64);
            }
            let result = emit_win64_rt_call(
                module,
                builder,
                intrinsics,
                static_name,
                &sig_params,
                Some(ret_ty),
                &call_args,
            )?
            .unwrap_or_else(|| builder.ins().iconst(ret_ty, 0));
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                result,
            );
            Ok(true)
        }
        "gos_rt_vec_set_slot_children" => {
            // Tags an `AGGR_OWNED` vec with its element slot-children layout so
            // the runtime retains each pushed slot's children and deep-frees
            // them when the vec dies: `gos_rt_vec_set_slot_children(v, meta)`
            // where `v` is a vec pointer and `meta` names the module-global
            // slot-children blob (empty / absent => null => no-op). Lowered (not
            // no-op'd) because the JIT compiles the body for real and the
            // push-retain it sets up is load-bearing for RC accounting.
            let v = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let v = coerce_arg_to(builder, v, ptr_ty).unwrap_or(v);
            let meta_val = match args.get(1) {
                Some(Operand::Const(ConstValue::Str(sym))) if !sym.is_empty() => {
                    let Some(blob) = tcx.rc_meta(sym) else {
                        bail!(
                            "native codegen: gos_rt_vec_set_slot_children references unknown meta `{sym}`"
                        );
                    };
                    let data_id = intrinsics.intern_rc_meta(module, sym, blob)?;
                    let gv = module.declare_data_in_func(data_id, builder.func);
                    builder.ins().symbol_value(ptr_ty, gv)
                }
                _ => builder.ins().iconst(ptr_ty, 0),
            };
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_vec_set_slot_children",
                &[ptr_ty, ptr_ty],
                &[],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            builder.ins().call(fref, &[v, meta_val]);
            Ok(true)
        }
        // A container a by-value aggregate field owns: the copy takes storage
        // of its own and the field's death frees it. Every one of these
        // helpers reads the FIELD's address, so the place is lowered to an
        // address rather than to its word - a local holds the carrier itself,
        // and a reference holds the address of one.
        "gos_rt_map_field_release"
        | "gos_rt_map_field_clone"
        | "gos_rt_set_field_release"
        | "gos_rt_set_field_clone"
        | "gos_rt_deque_field_release"
        | "gos_rt_deque_field_clone"
        | "gos_rt_bheap_field_release"
        | "gos_rt_bheap_field_clone" => {
            let Some(Operand::Copy(place)) = args.first() else {
                return Ok(true);
            };
            // A bare local of map type IS the map word, and this backend keeps
            // such a local in an SSA variable with no address of its own. The
            // swap is expressed on the value instead: clone (or free) what the
            // local holds and write the answer back into it, which is what the
            // slot-address form does through memory.
            if place.projection.is_empty()
                && matches!(
                    tcx.kind_of(body.local_ty(place.local)),
                    TyKind::HashMap { .. }
                )
            {
                let var = ensure_var(
                    builder,
                    locals,
                    body,
                    tcx,
                    module,
                    &intrinsics.body_cl_types,
                    place.local,
                );
                let held = builder.use_var(var);
                let held_ty = value_type(held, builder);
                let current = coerce_arg_to(builder, held, ptr_ty).unwrap_or(held);
                if name == "gos_rt_map_field_clone" {
                    let f =
                        intrinsics.extern_fn(module, "gos_rt_map_clone", &[ptr_ty], &[ptr_ty])?;
                    let fref = module.declare_func_in_func(f, builder.func);
                    let call = builder.ins().call(fref, &[current]);
                    let cloned = builder.inst_results(call)[0];
                    let cloned = coerce_arg_to(builder, cloned, held_ty).unwrap_or(cloned);
                    builder.def_var(var, cloned);
                } else {
                    let f = intrinsics.extern_fn(module, "gos_rt_map_free", &[ptr_ty], &[])?;
                    let fref = module.declare_func_in_func(f, builder.func);
                    builder.ins().call(fref, &[current]);
                    let null = builder.ins().iconst(held_ty, 0);
                    builder.def_var(var, null);
                }
                return Ok(true);
            }
            let addr = lower_place_address(module, builder, locals, body, tcx, place, intrinsics)?;
            let slot = if place.projection.is_empty()
                && matches!(
                    tcx.kind_of(body.locals[place.local.0 as usize].ty),
                    TyKind::Ref { .. }
                ) {
                builder.ins().load(ptr_ty, MemFlagsData::trusted(), addr, 0)
            } else {
                addr
            };
            // The interner keeps the symbol for the module's lifetime, so the
            // name is re-spelled as a literal rather than borrowed from the MIR.
            let sym: &'static str = match name {
                "gos_rt_map_field_clone" => "gos_rt_map_field_clone",
                "gos_rt_map_field_release" => "gos_rt_map_field_release",
                "gos_rt_set_field_clone" => "gos_rt_set_field_clone",
                "gos_rt_set_field_release" => "gos_rt_set_field_release",
                "gos_rt_deque_field_clone" => "gos_rt_deque_field_clone",
                "gos_rt_deque_field_release" => "gos_rt_deque_field_release",
                "gos_rt_bheap_field_clone" => "gos_rt_bheap_field_clone",
                _ => "gos_rt_bheap_field_release",
            };
            let f = intrinsics.extern_fn(module, sym, &[ptr_ty], &[])?;
            let fref = module.declare_func_in_func(f, builder.func);
            builder.ins().call(fref, &[slot]);
            Ok(true)
        }
        // The map's own handle, not a field address: the tag says the entries
        // own their values, so an insert takes a share and a removal or the
        // map's death gives it back.
        "gos_rt_map_set_blob_values" | "gos_rt_map_set_vec_values" => {
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
                None => return Ok(true),
            };
            let sym: &'static str = if name == "gos_rt_map_set_blob_values" {
                "gos_rt_map_set_blob_values"
            } else {
                "gos_rt_map_set_vec_values"
            };
            let f = intrinsics.extern_fn(module, sym, &[ptr_ty], &[])?;
            let fref = module.declare_func_in_func(f, builder.func);
            builder.ins().call(fref, &[m]);
            Ok(true)
        }
        "gos_rt_aggr_release_children"
        | "gos_rt_aggr_retain_children"
        | "gos_rt_aggr_zero_guarded"
        | "gos_rt_option_slot_retain"
        | "gos_rt_option_slot_release"
        | "gos_rt_vec_set_elem_meta" => {
            // Guarded copy-blob accounting is emitted by the LLVM
            // lowering, which compiles every body of both build profiles
            // (debug is LLVM at -O0); this backend only sees the rare
            // bodies that fall back when the LLVM lowerer rejects a
            // construct. Such a body heap-allocates its aggregates at
            // construction instead of copying them into provenance-set
            // blobs, so these walks would only ever no-op - escaped
            // aggregates inside a fallback body degrade to a bounded
            // leak (never a corruption: every release is set-gated).
            Ok(true)
        }
        "gos_rt_rc_release"
        | "gos_rt_rc_retain"
        | "gos_rt_rc_weak_release"
        | "gos_rt_rc_weak_retain" => {
            // Drop-pass accounting: one pointer argument, no result.
            let p = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let static_name: &'static str = match name {
                "gos_rt_rc_release" => "gos_rt_rc_release",
                "gos_rt_rc_retain" => "gos_rt_rc_retain",
                "gos_rt_rc_weak_release" => "gos_rt_rc_weak_release",
                _ => "gos_rt_rc_weak_retain",
            };
            let f = intrinsics.extern_fn(module, static_name, &[types::I64], &[])?;
            let fref = module.declare_func_in_func(f, builder.func);
            builder.ins().call(fref, &[p]);
            Ok(true)
        }
        "gos_rt_rc_downgrade" => {
            // Returns the (possibly tagged) weak referent pointer.
            let p = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_rc_downgrade",
                &[types::I64],
                &[types::I64],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[p]);
            let v = builder.inst_results(call)[0];
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_rt_rc_downgrade destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                v,
            );
            Ok(true)
        }
        // The cranelift tier writes a payload aggregate into the slot as the
        // address of the block carrying its words, so the slot's own contents
        // are already where those words live.
        "gos_enum_load" | "gos_enum_slot_ptr" => {
            // Load i64 at ((ptr & !7) + off) - enum payload read; mask is
            // a no-op for header-repr (aligned) pointers. A two-word
            // `Option` / `Result` / inline-enum field occupies both its words
            // in the node, so the read takes its own width - a word-sized
            // load would keep only the discriminant.
            let load_ty = intrinsics
                .body_cl_types
                .get(destination.local.0 as usize)
                .copied()
                .flatten()
                .filter(|&t| t == types::I128)
                .unwrap_or(types::I64);
            let p = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let off = match args.get(1) {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let base = builder.ins().band_imm_s(p, -8);
            // Tagged-null unit variants (base zero) yield 0 instead of
            // dereferencing address zero.
            let load_b = builder.create_block();
            let done_b = builder.create_block();
            builder.append_block_param(done_b, load_ty);
            // `iconst` carries at most 64 bits, so a wide zero is built from
            // two halves.
            let zero = if load_ty == types::I128 {
                let half = builder.ins().iconst(types::I64, 0);
                builder.ins().iconcat(half, half)
            } else {
                builder.ins().iconst(load_ty, 0)
            };
            builder
                .ins()
                .brif(base, load_b, &[], done_b, &[zero.into()]);
            builder.switch_to_block(load_b);
            builder.seal_block(load_b);
            let addr = builder.ins().iadd(base, off);
            let lv = builder.ins().load(
                load_ty,
                cranelift_codegen::ir::MemFlagsData::trusted(),
                addr,
                0,
            );
            builder.ins().jump(done_b, &[lv.into()]);
            builder.switch_to_block(done_b);
            builder.seal_block(done_b);
            let v = builder.block_params(done_b)[0];
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_enum_load destination cannot have projections");
            }
            // A multi-slot or counted-handle aggregate payload is stored as a
            // pointer to a heap-boxed copy. The binding takes the words by
            // value and a share of the box's own children, so it still names
            // them once the node that carried it is gone.
            let dest_ty = body.local_ty(destination.local);
            let boxed_slots = boxed_payload_slots(tcx, dest_ty);
            if let Some(slots) = boxed_slots {
                let ptr_ty = module.target_config().pointer_type();
                let slot =
                    builder.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
                        cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                        slots * 8,
                        8,
                    ));
                let dst = builder.ins().stack_addr(ptr_ty, slot, 0);
                let copy_b = builder.create_block();
                let after_b = builder.create_block();
                builder.ins().brif(v, copy_b, &[], after_b, &[]);
                builder.switch_to_block(copy_b);
                builder.seal_block(copy_b);
                for word in 0..slots {
                    let off = ir::immediates::Offset32::new(i32::try_from(word * 8).unwrap_or(0));
                    let w = builder.ins().load(
                        types::I64,
                        cranelift_codegen::ir::MemFlagsData::trusted(),
                        v,
                        off,
                    );
                    builder.ins().store(
                        cranelift_codegen::ir::MemFlagsData::trusted(),
                        w,
                        dst,
                        off,
                    );
                }
                let retain =
                    intrinsics.extern_fn(module, "gos_rt_rc_retain_children", &[ptr_ty], &[])?;
                let retain_ref = module.declare_func_in_func(retain, builder.func);
                builder.ins().call(retain_ref, &[v]);
                builder.ins().jump(after_b, &[]);
                builder.switch_to_block(after_b);
                builder.seal_block(after_b);
                define_var_to(
                    builder,
                    locals,
                    &intrinsics.body_cl_types,
                    destination.local,
                    dst,
                );
                return Ok(true);
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                v,
            );
            Ok(true)
        }
        "gos_enum_tag" => {
            // ptr | (disc << 1) - tagged-repr enum constructor finish.
            let p = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let d = match args.get(1) {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let sh = builder.ins().ishl_imm_u(d, 1);
            let v = builder.ins().bor(p, sh);
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_enum_tag destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                v,
            );
            Ok(true)
        }
        "gos_enum_disc_tag" => {
            // (ptr >> 1) & 3.
            let p = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let sh = builder.ins().ushr_imm_u(p, 1);
            let v = builder.ins().band_imm_s(sh, 3);
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_enum_disc_tag destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                v,
            );
            Ok(true)
        }
        "gos_enum_untag" => {
            // ptr & !7 - payload base of a tagged enum pointer.
            let p = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let v = builder.ins().band_imm_s(p, -8);
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_enum_untag destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                v,
            );
            Ok(true)
        }
        "gos_enum_disc" => {
            // Discriminant byte at payload-3 (inside the RC header).
            let p = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            // A payload that is not there - the `None` half of an `Option`,
            // an unset carrier - has no header to read, so the load is
            // steered at a zeroed slot and answers a discriminant no variant
            // carries.
            let is_null = builder.ins().icmp_imm_s(IntCC::Equal, p, 0);
            let pad = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(types::I64, zero, pad, 0);
            let pad_addr = builder.ins().stack_addr(types::I64, pad, 3);
            let addr = builder.ins().select(is_null, pad_addr, p);
            let b = builder.ins().uload8(
                types::I64,
                cranelift_codegen::ir::MemFlagsData::trusted(),
                addr,
                -3,
            );
            let missing = builder.ins().iconst(types::I64, -1);
            let b = builder.ins().select(is_null, missing, b);
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_enum_disc destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                b,
            );
            Ok(true)
        }
        "gos_enum_set_disc" => {
            // Store the low discriminant byte at payload-3.
            let p = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let d = match args.get(1) {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            builder
                .ins()
                .istore8(cranelift_codegen::ir::MemFlagsData::trusted(), d, p, -3);
            Ok(true)
        }
        "gos_rt_rc_drop_reuse" => {
            // Perceus reuse (drop half): `token = gos_rt_rc_drop_reuse(S)`
            // releases S (cascading to its children, exactly as
            // `gos_rt_rc_release`) and returns S's block base for in-place reuse
            // when S is the unique thread-local weak-free owner, else null
            // (having released normally). The token feeds `gos_rc_alloc_reuse`.
            // Null-safe; a region object releases and returns null.
            let s_val = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let s_ptr = if builder.func.dfg.value_type(s_val) == ptr_ty {
                s_val
            } else if ptr_ty == types::I64 {
                builder.ins().uextend(types::I64, s_val)
            } else {
                builder.ins().ireduce(ptr_ty, s_val)
            };
            let drop_reuse =
                intrinsics.extern_fn(module, "gos_rt_rc_drop_reuse", &[ptr_ty], &[ptr_ty])?;
            let dr_ref = module.declare_func_in_func(drop_reuse, builder.func);
            let call_inst = builder.ins().call(dr_ref, &[s_ptr]);
            let raw_ptr = builder.inst_results(call_inst)[0];
            let as_i64 = if ptr_ty == types::I64 {
                raw_ptr
            } else {
                builder.ins().uextend(types::I64, raw_ptr)
            };
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_rt_rc_drop_reuse destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                as_i64,
            );
            Ok(true)
        }
        "gos_rc_alloc_reuse" => {
            // Perceus reuse (alloc half): `gos_rc_alloc_reuse(token, size, meta)`
            // re-homes a block from `gos_rt_rc_drop_reuse` into a fresh strong-1
            // object, or allocates fresh when the token is null. Same meta-symbol
            // handling as `gos_rc_alloc`, with the recycled block leading.
            let token_val = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let token_ptr = if builder.func.dfg.value_type(token_val) == ptr_ty {
                token_val
            } else if ptr_ty == types::I64 {
                builder.ins().uextend(types::I64, token_val)
            } else {
                builder.ins().ireduce(ptr_ty, token_val)
            };
            let size_val = match args.get(1) {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let size_i64 = if builder.func.dfg.value_type(size_val) == types::I64 {
                size_val
            } else {
                builder.ins().sextend(types::I64, size_val)
            };
            let meta_val = match args.get(2) {
                Some(Operand::Const(ConstValue::Str(sym))) if !sym.is_empty() => {
                    let Some(blob) = tcx.rc_meta(sym) else {
                        bail!("native codegen: gos_rc_alloc_reuse references unknown meta `{sym}`");
                    };
                    let data_id = intrinsics.intern_rc_meta(module, sym, blob)?;
                    let gv = module.declare_data_in_func(data_id, builder.func);
                    builder.ins().symbol_value(ptr_ty, gv)
                }
                _ => builder.ins().iconst(ptr_ty, 0),
            };
            let rc_reuse = intrinsics.extern_fn(
                module,
                "gos_rt_rc_alloc_reuse",
                &[ptr_ty, types::I64, ptr_ty],
                &[ptr_ty],
            )?;
            let rc_reuse_ref = module.declare_func_in_func(rc_reuse, builder.func);
            let call_inst = builder
                .ins()
                .call(rc_reuse_ref, &[token_ptr, size_i64, meta_val]);
            let raw_ptr = builder.inst_results(call_inst)[0];
            let as_i64 = if ptr_ty == types::I64 {
                raw_ptr
            } else {
                builder.ins().uextend(types::I64, raw_ptr)
            };
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_rc_alloc_reuse destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                as_i64,
            );
            Ok(true)
        }
        "gos_rc_alloc" | "gos_rc_alloc_tagged" => {
            // Reference-counted allocator: `gos_rc_alloc(size, meta)`
            // -> ptr with strong count 1. `size` is the payload byte
            // count; `meta` names the module-global child-layout blob
            // (empty name => leaf => null meta).
            let size_val = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let size_i64 = if ptr_ty == types::I64 {
                size_val
            } else {
                builder.ins().sextend(types::I64, size_val)
            };
            let meta_val = match args.get(1) {
                Some(Operand::Const(ConstValue::Str(sym))) if !sym.is_empty() => {
                    let Some(blob) = tcx.rc_meta(sym) else {
                        bail!("native codegen: gos_rc_alloc references unknown meta `{sym}`");
                    };
                    let data_id = intrinsics.intern_rc_meta(module, sym, blob)?;
                    let gv = module.declare_data_in_func(data_id, builder.func);
                    builder.ins().symbol_value(ptr_ty, gv)
                }
                _ => builder.ins().iconst(ptr_ty, 0),
            };
            let rt_name = if name == "gos_rc_alloc_tagged" {
                "gos_rt_rc_alloc_tagged"
            } else {
                "gos_rt_rc_alloc"
            };
            let rc_alloc =
                intrinsics.extern_fn(module, rt_name, &[types::I64, ptr_ty], &[ptr_ty])?;
            let rc_alloc_ref = module.declare_func_in_func(rc_alloc, builder.func);
            let call_inst = builder.ins().call(rc_alloc_ref, &[size_i64, meta_val]);
            let raw_ptr = builder.inst_results(call_inst)[0];
            let as_i64 = if ptr_ty == types::I64 {
                raw_ptr
            } else {
                builder.ins().uextend(types::I64, raw_ptr)
            };
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_rc_alloc destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                as_i64,
            );
            Ok(true)
        }
        "gos_rt_enum_box_aggr" | "gos_rt_rc_weak_cell" => {
            // Copy a by-value aggregate's slot bytes into a fresh RC cell:
            // `gos_rt_enum_box_aggr(size, meta, src)`. `meta` names the
            // module-global child-layout blob (empty name => leaf => null
            // meta) and `src` is the aggregate's data address, which is what
            // an aggregate local's cranelift value already holds.
            if args.len() < 3 {
                bail!("native codegen: {name} requires (size, meta, src)");
            }
            let size_val = lower_operand(
                module, builder, locals, body, tcx, &args[0], None, intrinsics,
            )?;
            let size_i64 = if builder.func.dfg.value_type(size_val) == types::I64 {
                size_val
            } else {
                builder.ins().sextend(types::I64, size_val)
            };
            let meta_val = match &args[1] {
                Operand::Const(ConstValue::Str(sym)) if !sym.is_empty() => {
                    let Some(blob) = tcx.rc_meta(sym) else {
                        bail!("native codegen: {name} references unknown meta `{sym}`");
                    };
                    let data_id = intrinsics.intern_rc_meta(module, sym, blob)?;
                    let gv = module.declare_data_in_func(data_id, builder.func);
                    builder.ins().symbol_value(ptr_ty, gv)
                }
                _ => builder.ins().iconst(ptr_ty, 0),
            };
            let src_raw = lower_operand(
                module, builder, locals, body, tcx, &args[2], None, intrinsics,
            )?;
            let src = if builder.func.dfg.value_type(src_raw) == ptr_ty {
                src_raw
            } else if ptr_ty == types::I64 {
                builder.ins().uextend(types::I64, src_raw)
            } else {
                builder.ins().ireduce(ptr_ty, src_raw)
            };
            let static_name: &'static str = if name == "gos_rt_rc_weak_cell" {
                "gos_rt_rc_weak_cell"
            } else {
                "gos_rt_enum_box_aggr"
            };
            let f = intrinsics.extern_fn(
                module,
                static_name,
                &[types::I64, ptr_ty, ptr_ty],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let call = builder.ins().call(fref, &[size_i64, meta_val, src]);
            let raw = builder.inst_results(call)[0];
            let as_i64 = if ptr_ty == types::I64 {
                raw
            } else {
                builder.ins().uextend(types::I64, raw)
            };
            if !destination.projection.is_empty() {
                bail!("native codegen: {name} destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                as_i64,
            );
            Ok(true)
        }
        "gos_store" | "gos_store_i128" => {
            // Raw heap store: `gos_store(ptr, offset, value)` writes
            // `value` as an i64 at `ptr + offset`. Companion to
            // `gos_load` + `gos_alloc`. `gos_store_i128` writes both words
            // of a two-word carrier into a slot sized for it.
            if args.len() < 3 {
                bail!("native codegen: gos_store requires (ptr, offset, value)");
            }
            let ptr_raw = lower_operand(
                module, builder, locals, body, tcx, &args[0], None, intrinsics,
            )?;
            let offset_raw = lower_operand(
                module, builder, locals, body, tcx, &args[1], None, intrinsics,
            )?;
            let value = lower_operand(
                module, builder, locals, body, tcx, &args[2], None, intrinsics,
            )?;
            // Closures pass `__env` as the first param; if its
            // declared type is the closure's body-return type
            // (bool/i8/etc), the inferred cl-type can be narrower
            // than ptr-width. Promote both halves to i64 before
            // adding so the iadd doesn't trip the verifier.
            let ptr_val = coerce_arg_to(builder, ptr_raw, types::I64).unwrap_or(ptr_raw);
            let offset_val = coerce_arg_to(builder, offset_raw, types::I64).unwrap_or(offset_raw);
            // The wide store keeps both words of a two-word carrier; every
            // other slot in the language is the one word `gos_store` writes.
            let value = if name == "gos_store_i128" {
                coerce_arg_to(builder, value, types::I128).unwrap_or(value)
            } else {
                coerce_arg_to(builder, value, types::I64).unwrap_or(value)
            };
            let addr_i64 = builder.ins().iadd(ptr_val, offset_val);
            let addr = if ptr_ty == types::I64 {
                addr_i64
            } else {
                builder.ins().ireduce(ptr_ty, addr_i64)
            };
            builder.ins().store(
                MemFlagsData::trusted(),
                value,
                addr,
                ir::immediates::Offset32::new(0),
            );
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
        "gos_load" => {
            // Raw heap load: `gos_load(ptr, offset)` reads an i64 at
            // `ptr + offset`.
            if args.len() < 2 {
                bail!("native codegen: gos_load requires (ptr, offset)");
            }
            let ptr_raw = lower_operand(
                module, builder, locals, body, tcx, &args[0], None, intrinsics,
            )?;
            let offset_raw = lower_operand(
                module, builder, locals, body, tcx, &args[1], None, intrinsics,
            )?;
            // See `gos_store` above - coerce both operands to i64
            // so the env-param's narrower inferred cl-type can't
            // mismatch the offset constant.
            let ptr_val = coerce_arg_to(builder, ptr_raw, types::I64).unwrap_or(ptr_raw);
            let offset_val = coerce_arg_to(builder, offset_raw, types::I64).unwrap_or(offset_raw);
            let addr_i64 = builder.ins().iadd(ptr_val, offset_val);
            let addr = if ptr_ty == types::I64 {
                addr_i64
            } else {
                builder.ins().ireduce(ptr_ty, addr_i64)
            };
            let loaded = builder.ins().load(
                types::I64,
                MemFlagsData::trusted(),
                addr,
                ir::immediates::Offset32::new(0),
            );
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                loaded,
            );
            Ok(true)
        }
        "panic" => {
            // Route through `gos_rt_panic(msg)` after building a
            // single concatenated message from all arguments
            // (mirrors `render_args` in the interpreter - pieces
            // joined by a single space). Multi-arg
            // `panic("code=", 42)` previously dropped every arg
            // after the first.
            let panic_fn = intrinsics.extern_fn_by_name(module, "gos_rt_panic")?;
            let panic_ref = module.declare_func_in_func(panic_fn, builder.func);
            let msg = if args.is_empty() {
                builder.ins().iconst(ptr_ty, 0)
            } else {
                emit_args_to_concat_string(
                    module, builder, locals, body, tcx, args, intrinsics, " ",
                )?
            };
            let _ = builder.ins().call(panic_ref, &[msg]);
            // `gos_rt_panic` is noreturn but Cranelift needs the
            // block to end in a terminator; emit an unreachable
            // trap so downstream jumps are correctly dead.
            builder.ins().trap(ir::TrapCode::user(4).unwrap());
            Ok(true)
        }
        // ----- Gossamer C-ABI runtime helpers -----
        // String concatenation delegates to the runtime shim.
        "gos_rt_str_concat" => {
            let concat_fn = intrinsics.extern_fn_by_name(module, "gos_rt_str_concat")?;
            let fref = module.declare_func_in_func(concat_fn, builder.func);
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
            let ptr = builder.inst_results(call)[0];
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_rt_str_concat destination cannot have projections");
            }
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ptr,
            );
            Ok(true)
        }
        // Byte-at: `s[i]` on a `String` loads the `i`-th byte and
        // zero-extends to `i64` (matching the interpreter's
        // convention of returning byte codes as `i64`).
        "gos_rt_os_read_dir" => {
            let f = intrinsics.extern_fn_by_name(module, "gos_rt_os_read_dir")?;
            let fref = module.declare_func_in_func(f, builder.func);
            let p = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let call = builder.ins().call(fref, &[p]);
            let ret = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ret,
            );
            Ok(true)
        }
        "gos_rt_str_substring" => {
            let f = intrinsics.extern_fn(
                module,
                "gos_rt_str_substring",
                &[ptr_ty, types::I64, types::I64],
                &[ptr_ty],
            )?;
            let fref = module.declare_func_in_func(f, builder.func);
            let s = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let start = match args.get(1) {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let end = match args.get(2) {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            let call = builder.ins().call(fref, &[s, start, end]);
            let ret = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ret,
            );
            Ok(true)
        }
        "gos_rt_str_byte_at" => {
            let ptr = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let idx = match args.get(1) {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            if !destination.projection.is_empty() {
                bail!("native codegen: gos_rt_str_byte_at destination cannot have projections");
            }
            // Bound the read by the string's byte length: any index outside
            // `[0, len)` yields 0 without dereferencing past the content.
            // `gos_rt_str_byte_len` is O(1) for header-carrying strings and is
            // null-safe (returns 0 for a null pointer).
            let str_len_fn =
                intrinsics.extern_fn(module, "gos_rt_str_byte_len", &[ptr_ty], &[types::I64])?;
            let str_len_ref = module.declare_func_in_func(str_len_fn, builder.func);
            let len_call = builder.ins().call(str_len_ref, &[ptr]);
            let len = builder.inst_results(len_call)[0];
            let idx_i64 = match value_type(idx, builder) {
                t if t == types::I64 => idx,
                t if t == types::I32 => builder.ins().sextend(types::I64, idx),
                _ => idx,
            };
            let idx_ptr = match value_type(idx, builder) {
                t if t == ptr_ty => idx,
                t if t == types::I64 && ptr_ty == types::I32 => builder.ins().ireduce(ptr_ty, idx),
                t if t == types::I32 && ptr_ty == types::I64 => builder.ins().uextend(ptr_ty, idx),
                _ => idx,
            };
            let ge0 = builder
                .ins()
                .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, idx_i64, 0);
            let lt_len = builder.ins().icmp(IntCC::SignedLessThan, idx_i64, len);
            let in_bounds = builder.ins().band(ge0, lt_len);
            let read_blk = builder.create_block();
            let oob_blk = builder.create_block();
            let done_blk = builder.create_block();
            builder.append_block_param(done_blk, types::I64);
            builder.ins().brif(in_bounds, read_blk, &[], oob_blk, &[]);

            builder.switch_to_block(read_blk);
            let addr = builder.ins().iadd(ptr, idx_ptr);
            let byte = builder
                .ins()
                .load(types::I8, MemFlagsData::trusted(), addr, 0);
            let read_val = builder.ins().uextend(types::I64, byte);
            builder
                .ins()
                .jump(done_blk, &[ir::BlockArg::Value(read_val)]);

            builder.switch_to_block(oob_blk);
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().jump(done_blk, &[ir::BlockArg::Value(zero)]);

            builder.switch_to_block(done_blk);
            let value = builder.block_params(done_blk)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                value,
            );
            Ok(true)
        }
        // Runtime strings carry scalar and byte lengths in the header.
        // Calling `strlen` here made byte scans quadratic because hot loops
        // ask for length on every iteration.
        "gos_rt_str_len" | "gos_rt_str_byte_len" => {
            let symbol = match name {
                "gos_rt_str_byte_len" => "gos_rt_str_byte_len",
                _ => "gos_rt_str_len",
            };
            let str_len = intrinsics.extern_fn(module, symbol, &[ptr_ty], &[types::I64])?;
            let str_len_ref = module.declare_func_in_func(str_len, builder.func);
            let ptr = match args.first() {
                Some(arg) => {
                    lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?
                }
                None => builder.ins().iconst(ptr_ty, 0),
            };
            let call = builder.ins().call(str_len_ref, &[ptr]);
            let len = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                len,
            );
            Ok(true)
        }
        // `os::args()` returns the program's argv as a
        // Vec<String>. The native runtime isn't wired yet; for
        // the build-to-native envelope we need a shape the
        // downstream `.len()`/`[0]` calls can consume. Returning
        // a null pointer and having `gos_rt_vec_len(null)` be 0
        // lets programs default their args.
        "gos_rt_os_args" | "os::args" => {
            // Forward to the runtime's `gos_rt_os_args`, which
            // returns a `*mut GosVec` view over `argv + 1`.
            // `args.len()` reads `len` at offset 0 (the standard
            // GosVec layout) and indexing reads the i-th
            // `*const c_char` through the GosVec `ptr` field.
            let args_fn = intrinsics.extern_fn_by_name(module, "gos_rt_os_args")?;
            let fref = module.declare_func_in_func(args_fn, builder.func);
            let call = builder.ins().call(fref, &[]);
            let ret = builder.inst_results(call)[0];
            define_var_to(
                builder,
                locals,
                &intrinsics.body_cl_types,
                destination.local,
                ret,
            );
            Ok(true)
        }
        // `std::time::now()` - opaque monotonic clock value. Cast
        // a `libc::clock_gettime` result into an i64 ns-since-
        // epoch. For now, return 0 so programs that print the
        // current instant compile; the interpreter path already
        // returns a real value.
        "time::now" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_time_now")?;
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
        "time::now_ms" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_time_now_ms")?;
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
        // `std::math::*` - all (f64) -> f64 except where noted.
        "math::sqrt" | "math::sin" | "math::cos" | "math::ln" | "math::log" | "math::exp"
        | "math::abs" | "math::floor" | "math::ceil" => {
            let rt_name = match name {
                "math::sqrt" => "gos_rt_math_sqrt",
                "math::sin" => "gos_rt_math_sin",
                "math::cos" => "gos_rt_math_cos",
                "math::ln" | "math::log" => "gos_rt_math_log",
                "math::exp" => "gos_rt_math_exp",
                "math::abs" => "gos_rt_math_abs",
                "math::floor" => "gos_rt_math_floor",
                "math::ceil" => "gos_rt_math_ceil",
                _ => unreachable!(),
            };
            let rt_fn = intrinsics.extern_fn(module, rt_name, &[types::F64], &[types::F64])?;
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
        "math::pow" => {
            let rt_fn = intrinsics.extern_fn(
                module,
                "gos_rt_math_pow",
                &[types::F64, types::F64],
                &[types::F64],
            )?;
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
            let y = match args.get(1) {
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
            let y64 = coerce_arg_to(builder, y, types::F64)?;
            let call = builder.ins().call(fref, &[x64, y64]);
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
        "time::now_ns" | "time::now_nanos" => {
            let rt_fn = intrinsics.extern_fn_by_name(module, "gos_rt_now_ns")?;
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
        "time::monotonic_ms" | "time::monotonic_nanos" => {
            let rt_name = if name == "time::monotonic_ms" {
                "gos_rt_monotonic_ms"
            } else {
                "gos_rt_monotonic_nanos"
            };
            let rt_fn = intrinsics.extern_fn_by_name(module, rt_name)?;
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
        _ => Ok(false),
    }
}
