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

/// Emits a call to runtime function `name` whose logical Cranelift param /
/// return slots are `params` / `ret`, marshalling every `i128` slot across
/// the Win64 `extern "C"` boundary the way the rustc-compiled runtime
/// expects it. On `x86_64-pc-windows-msvc` rustc passes an `extern "C"`
/// `i128` argument by pointer and returns one in a 16-byte vector register
/// (`I8X16`), whereas Cranelift's native `i128` ABI uses integer register
/// pairs - so a bare `i128` call instruction disagrees with the runtime and
/// reads/writes garbage (the `[disc, payload]` Result/Option carrier then
/// decodes to a wild pointer and faults). Spill `i128` args to a 16-byte
/// slot and pass the address; declare + read an `i128` return as `I8X16`
/// and bit-cast it back. On the SysV ABI an `i128` is passed and returned
/// by value, unchanged. The LLVM tier already performs the identical
/// adjustment (`fat_i128_call_arg` / `<16 x i8>` return). `arg_values` hold
/// the logical values in `params` order; the result is returned in its
/// logical `ret` type.
pub(super) fn emit_win64_rt_call(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    name: &'static str,
    params: &[ir::Type],
    ret: Option<ir::Type>,
    arg_values: &[ir::Value],
) -> Result<Option<ir::Value>> {
    // `target_config()` (not `isa()`) - the parallel IR phase uses an
    // `OfflineModule` that panics on `isa()`.
    let cfg = module.target_config();
    let ptr_ty = cfg.pointer_type();
    let fat = |t: ir::Type| is_win64_abi(cfg) && t == types::I128;

    let wire_params: Vec<ir::Type> = params.iter().map(|&t| win64_wire_param(cfg, t)).collect();
    let wire_returns: Vec<ir::Type> = match ret {
        Some(t) => vec![win64_wire_return(cfg, t)],
        None => Vec::new(),
    };
    let func_id = intrinsics.extern_fn(module, name, &wire_params, &wire_returns)?;
    let fref = module.declare_func_in_func(func_id, builder.func);

    let mut wire_args: Vec<ir::Value> = Vec::with_capacity(arg_values.len());
    for (&val, &logical_ty) in arg_values.iter().zip(params.iter()) {
        if fat(logical_ty) {
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                16,
                4,
            ));
            builder.ins().stack_store(ptr_ty, val, slot, 0);
            wire_args.push(builder.ins().stack_addr(ptr_ty, slot, 0));
        } else {
            wire_args.push(val);
        }
    }
    let call = builder.ins().call(fref, &wire_args);
    Ok(match ret {
        Some(t) => {
            let raw = builder.inst_results(call)[0];
            let v = if fat(t) {
                bitcast_same_width(builder, types::I128, raw)
            } else {
                raw
            };
            Some(v)
        }
        None => None,
    })
}

/// Copies a two-word `[disc, payload]` carrier into a fresh heap block and
/// returns the block's address, the form a Result/Option payload takes when
/// the payload is itself a carrier.
fn heap_copy_carrier(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    carrier: ir::Value,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    let alloc_fn = intrinsics.extern_fn(module, "gos_rt_aggr_alloc", &[types::I64], &[ptr_ty])?;
    let alloc_ref = module.declare_func_in_func(alloc_fn, builder.func);
    let size = builder.ins().iconst(types::I64, 16);
    let call = builder.ins().call(alloc_ref, &[size]);
    let block = builder.inst_results(call)[0];
    let (disc, payload) = builder.ins().isplit(carrier);
    builder.ins().store(
        MemFlagsData::new(),
        disc,
        block,
        ir::immediates::Offset32::new(0),
    );
    builder.ins().store(
        MemFlagsData::new(),
        payload,
        block,
        ir::immediates::Offset32::new(8),
    );
    Ok(block)
}

/// Copies an aggregate's `slots` words into a fresh heap block and answers
/// its address, so a value handed over in a carrier outlives the frame that
/// built it. The symmetric read is whatever the payload's own type does with
/// the address - a memcpy out, or a field read through it.
fn heap_copy_aggregate(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    src: ir::Value,
    slots: u32,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    let alloc_fn = intrinsics.extern_fn(module, "gos_rt_aggr_alloc", &[types::I64], &[ptr_ty])?;
    let alloc_ref = module.declare_func_in_func(alloc_fn, builder.func);
    let bytes = i64::from(slots.max(1)) * 8;
    let size = builder.ins().iconst(types::I64, bytes);
    let call = builder.ins().call(alloc_ref, &[size]);
    let block = builder.inst_results(call)[0];
    for index in 0..slots.max(1) {
        let offset = ir::immediates::Offset32::new(i32::try_from(index * 8).unwrap_or(0));
        let word = builder
            .ins()
            .load(types::I64, MemFlagsData::new(), src, offset);
        builder
            .ins()
            .store(MemFlagsData::new(), word, block, offset);
    }
    Ok(block)
}

/// Calls runtime symbol `name` using the signature the ABI registry records
/// for it, marshalling every `i128` slot the way [`emit_win64_rt_call`]
/// documents. Call sites that already know their slot types call that
/// function directly; this wrapper keeps the registry the single source of
/// truth for the rest, so a helper's declared and called shapes cannot drift.
pub(super) fn emit_rt_call_by_name(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    name: &'static str,
    arg_values: &[ir::Value],
) -> Result<Option<ir::Value>> {
    let entry = gossamer_abi::lookup(name)
        .ok_or_else(|| anyhow!("emit_rt_call_by_name: unknown runtime symbol {name}"))?;
    let params: Vec<ir::Type> = entry
        .sig
        .params
        .iter()
        .filter_map(|t| abi_type_to_cranelift(*t))
        .collect();
    let ret = abi_type_to_cranelift(entry.sig.ret);
    emit_win64_rt_call(module, builder, intrinsics, name, &params, ret, arg_values)
}

fn pack_i64_carrier(
    builder: &mut FunctionBuilder<'_>,
    disc: ir::Value,
    payload: ir::Value,
) -> ir::Value {
    let disc = coerce_arg_to(builder, disc, types::I64).unwrap_or(disc);
    let payload = coerce_arg_to(builder, payload, types::I64).unwrap_or(payload);
    let disc128 = builder.ins().uextend(types::I128, disc);
    let payload128 = builder.ins().uextend(types::I128, payload);
    let shifted = builder.ins().ishl_imm_s(payload128, 64);
    builder.ins().bor(disc128, shifted)
}

fn lower_inline_result_carrier_call(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    args: &[Operand],
    intrinsics: &mut IntrinsicContext,
    destination: &gossamer_mir::Place,
    name: &str,
) -> Result<bool> {
    let value = match name {
        // The owning spelling differs only in who booked the payload's
        // share; this backend's box takes the words as they stand either way.
        "gos_rt_result_new" | "gos_rt_result_new_owned" => {
            let disc = match args.first() {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let payload = match args.get(1) {
                // A multi-slot payload - a struct, a tuple, a fixed array -
                // lives in the constructing frame's stack slot, which is gone
                // the moment that frame returns. The carrier travels with the
                // value, so it must hold the address of a heap copy.
                Some(arg) if operand_aggregate_slots(body, tcx, arg).is_some() => {
                    let slots = operand_aggregate_slots(body, tcx, arg).unwrap_or(1);
                    let addr =
                        lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?;
                    // The box owns the payload's heap children under the
                    // structural meta when one is registered, as on LLVM.
                    let copy_meta = match arg {
                        Operand::Copy(place) if place.projection.is_empty() => {
                            let ty = body.local_ty(place.local);
                            let structural = format!("gos_rc_meta_boxaggr_{}", ty.as_u32());
                            tcx.rc_meta(&structural).map(|_| structural).or_else(|| {
                                tcx.aggr_copy_meta(ty)
                                    .filter(|sym| !sym.is_empty())
                                    .map(str::to_owned)
                            })
                        }
                        _ => None,
                    };
                    match copy_meta.and_then(|sym| tcx.rc_meta(&sym).map(|blob| (sym, blob))) {
                        // A payload type with a copy meta travels as a counted
                        // blob holding its own shares of the children, the
                        // shape the carrier's release gives back.
                        Some((sym, blob)) => {
                            let ptr_ty = module.target_config().pointer_type();
                            let data_id = intrinsics.intern_rc_meta(module, &sym, blob)?;
                            let gv = module.declare_data_in_func(data_id, builder.func);
                            let meta = builder.ins().symbol_value(ptr_ty, gv);
                            let allocator = if name == "gos_rt_result_new_owned" {
                                "gos_rt_rc_alloc_move"
                            } else {
                                "gos_rt_rc_alloc_copy"
                            };
                            let alloc_fn = intrinsics.extern_fn(
                                module,
                                allocator,
                                &[types::I64, ptr_ty, ptr_ty],
                                &[ptr_ty],
                            )?;
                            let alloc_ref = module.declare_func_in_func(alloc_fn, builder.func);
                            let bytes = builder
                                .ins()
                                .iconst(types::I64, i64::from(slots.max(1)) * 8);
                            let src = coerce_arg_to(builder, addr, ptr_ty)?;
                            let call = builder.ins().call(alloc_ref, &[bytes, meta, src]);
                            builder.inst_results(call)[0]
                        }
                        None => heap_copy_aggregate(module, builder, intrinsics, addr, slots)?,
                    }
                }
                Some(arg) => {
                    let raw =
                        lower_operand(module, builder, locals, body, tcx, arg, None, intrinsics)?;
                    if value_type(raw, builder) == types::I128 {
                        // A payload that is itself a two-word carrier
                        // (`Some(Some(v))`, `Ok(Err(e))`, an inline user
                        // enum) outlives the constructing frame, so the
                        // carrier holds the address of a heap copy. The
                        // `gos_rt_result_payload_i128` extractor reads the
                        // two words back from that address. A carrier whose
                        // box meta is registered is a counted blob holding its
                        // own share of the payload, as on the LLVM tier.
                        let carrier_meta = match arg {
                            Operand::Copy(place) if place.projection.is_empty() => {
                                let ty = body.local_ty(place.local);
                                let is_carrier = matches!(
                                    tcx.kind_of(ty),
                                    TyKind::Adt { def, .. }
                                        if def.local == u32::MAX || def.local == u32::MAX - 1
                                );
                                let structural = format!("gos_rc_meta_boxaggr_{}", ty.as_u32());
                                let carrier_box = format!("gos_rc_meta_carrierbox_{}", ty.as_u32());
                                is_carrier
                                    .then(|| {
                                        tcx.rc_meta(&structural)
                                            .map(|_| structural)
                                            .or_else(|| {
                                                tcx.rc_meta(&carrier_box).map(|_| carrier_box)
                                            })
                                            .or_else(|| {
                                                tcx.aggr_copy_meta(ty)
                                                    .filter(|sym| !sym.is_empty())
                                                    .map(str::to_owned)
                                            })
                                    })
                                    .flatten()
                            }
                            _ => None,
                        };
                        match carrier_meta.and_then(|sym| tcx.rc_meta(&sym).map(|blob| (sym, blob)))
                        {
                            Some((sym, blob)) => {
                                let ptr_ty = module.target_config().pointer_type();
                                let data_id = intrinsics.intern_rc_meta(module, &sym, blob)?;
                                let gv = module.declare_data_in_func(data_id, builder.func);
                                let meta = builder.ins().symbol_value(ptr_ty, gv);
                                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                                    StackSlotKind::ExplicitSlot,
                                    16,
                                    4,
                                ));
                                builder.ins().stack_store(ptr_ty, raw, slot, 0);
                                let src = builder.ins().stack_addr(ptr_ty, slot, 0);
                                let alloc_fn = intrinsics.extern_fn(
                                    module,
                                    "gos_rt_rc_alloc_copy",
                                    &[types::I64, ptr_ty, ptr_ty],
                                    &[ptr_ty],
                                )?;
                                let alloc_ref = module.declare_func_in_func(alloc_fn, builder.func);
                                let bytes = builder.ins().iconst(types::I64, 16);
                                let call = builder.ins().call(alloc_ref, &[bytes, meta, src]);
                                builder.inst_results(call)[0]
                            }
                            None => heap_copy_carrier(module, builder, intrinsics, raw)?,
                        }
                    } else {
                        coerce_arg_to(builder, raw, types::I64)?
                    }
                }
                None => builder.ins().iconst(types::I64, 0),
            };
            pack_i64_carrier(builder, disc, payload)
        }
        "gos_rt_result_new_f64" => {
            let disc = match args.first() {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(types::I64),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I64, 0),
            };
            let payload = match args.get(1) {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(types::F64),
                    intrinsics,
                )?,
                None => builder.ins().f64const(0.0),
            };
            let payload_bits = builder
                .ins()
                .bitcast(types::I64, MemFlagsData::new(), payload);
            pack_i64_carrier(builder, disc, payload_bits)
        }
        "gos_rt_result_disc" => {
            let carrier = match args.first() {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(types::I128),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I128, 0),
            };
            let carrier = coerce_arg_to(builder, carrier, types::I128)?;
            builder.ins().ireduce(types::I64, carrier)
        }
        "gos_rt_result_payload" | "gos_rt_weak_opt_payload" => {
            let carrier = match args.first() {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(types::I128),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I128, 0),
            };
            let carrier = coerce_arg_to(builder, carrier, types::I128)?;
            let (_disc, payload) = builder.ins().isplit(carrier);
            // A payload read out of a carrier's box takes the words into its
            // own slot: the destination holds shares of its own and gives them
            // back on its next overwrite, which may come after the box is gone,
            // and the map-field helpers must reach the copy rather than the box.
            if name == "gos_rt_result_payload"
                && destination.projection.is_empty()
                && intrinsics.stack_slotted.contains(&destination.local)
                && !super::lowering_stmt_term::local_flows_to_return(body, destination.local)
                && super::lowering_stmt_term::payload_takes_own_storage(
                    tcx,
                    body,
                    destination.local,
                )
            {
                let ptr_ty = module.target_config().pointer_type();
                let slots =
                    super::ty_layout::type_slot_count(tcx, body.local_ty(destination.local)).max(1);
                let dst_var = locals
                    .get(&destination.local)
                    .copied()
                    .ok_or_else(|| anyhow!("payload destination has no slot"))?;
                let dst_raw = builder.use_var(dst_var);
                let dst_ptr = coerce_arg_to(builder, dst_raw, ptr_ty)?;
                let src_ptr = coerce_arg_to(builder, payload, ptr_ty)?;
                for slot_idx in 0..slots {
                    let off = ir::immediates::Offset32::new((slot_idx as i32) * 8);
                    let word =
                        builder
                            .ins()
                            .load(types::I64, MemFlagsData::trusted(), src_ptr, off);
                    builder
                        .ins()
                        .store(MemFlagsData::trusted(), word, dst_ptr, off);
                }
                return Ok(true);
            }
            payload
        }
        "gos_rt_result_payload_f64" => {
            let carrier = match args.first() {
                Some(arg) => lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(types::I128),
                    intrinsics,
                )?,
                None => builder.ins().iconst(types::I128, 0),
            };
            let carrier = coerce_arg_to(builder, carrier, types::I128)?;
            let (_disc, payload) = builder.ins().isplit(carrier);
            builder
                .ins()
                .bitcast(types::F64, MemFlagsData::new(), payload)
        }
        _ => return Ok(false),
    };
    define_var_to(
        builder,
        locals,
        &intrinsics.body_cl_types,
        destination.local,
        value,
    );
    Ok(true)
}

/// The address a content-keyed entry point reads its key's slots through.
/// MIR already hands an aggregate key over as the address of its storage, so
/// that operand crosses as it is. A key whose type is itself a handle word - a
/// `Vec`, a slice, or a `String` - or a two-word `Option` / `Result` carrier is
/// a value, so it is spilled into a slot of its own and that slot's address
/// crosses instead.
pub(super) fn lower_key_by_address(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    op: &Operand,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    let key_ty = match op {
        Operand::Copy(place) => Some(resolve_place_ty(tcx, body, place)),
        _ => None,
    };
    let spill_bytes = key_ty.and_then(|ty| {
        if is_carrier_ty(tcx, ty) {
            Some(16)
        } else if matches!(
            tcx.kind_of(ty),
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::String
        ) {
            Some(8)
        } else {
            None
        }
    });
    let Some(bytes) = spill_bytes else {
        let address = lower_operand(
            module,
            builder,
            locals,
            body,
            tcx,
            op,
            Some(ptr_ty),
            intrinsics,
        )?;
        return coerce_arg_to(builder, address, ptr_ty);
    };
    let value = lower_operand(module, builder, locals, body, tcx, op, None, intrinsics)?;
    let slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, bytes, 3));
    builder.ins().stack_store(ptr_ty, value, slot, 0);
    Ok(builder.ins().stack_addr(ptr_ty, slot, 0))
}

/// The address an ordered container's push reads its element's slots through.
/// MIR hands an addressed aggregate over as the address of its storage, so
/// that operand crosses as it is; a two-word carrier or a one-word value is
/// spilled into a slot of its own and that slot's address crosses instead.
pub(super) fn lower_elem_by_address(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    op: &Operand,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    if let Operand::Copy(place) = op {
        let ty = resolve_place_ty(tcx, body, place);
        if !is_carrier_ty(tcx, ty) && tcx.elem_is_addressed_aggregate(ty) {
            let address = lower_operand(
                module,
                builder,
                locals,
                body,
                tcx,
                op,
                Some(ptr_ty),
                intrinsics,
            )?;
            return coerce_arg_to(builder, address, ptr_ty);
        }
    }
    let value = lower_operand(module, builder, locals, body, tcx, op, None, intrinsics)?;
    if value_type(value, builder) == types::I128 {
        let slot =
            builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 3));
        let slot_addr = builder.ins().stack_addr(ptr_ty, slot, 0);
        store_i128_words(builder, value, slot_addr, 0);
        return Ok(slot_addr);
    }
    let word = coerce_arg_to(builder, value, types::I64)?;
    let slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    let slot_addr = builder.ins().stack_addr(ptr_ty, slot, 0);
    builder
        .ins()
        .store(MemFlagsData::trusted(), word, slot_addr, 0);
    Ok(slot_addr)
}

pub(super) fn lower_generic_rt_call(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    args: &[Operand],
    intrinsics: &mut IntrinsicContext,
    destination: &gossamer_mir::Place,
    name: &'static str,
) -> Result<()> {
    if lower_inline_result_carrier_call(
        module,
        builder,
        locals,
        body,
        tcx,
        args,
        intrinsics,
        destination,
        name,
    )? {
        return Ok(());
    }
    let ptr_ty = module.target_config().pointer_type();
    // The ABI registry declares every `gos_rt_*` symbol's C signature and
    // is what the LLVM backend's `declare` statements are built from, so
    // deriving the Cranelift signature from it is what keeps the two
    // compiled tiers calling the same function. A second, hand-written
    // copy of these signatures has no way to be checked against the first,
    // and a wrong entry is a wrong-ABI call with no diagnostic.
    let entry = gossamer_abi::lookup(name)
        .ok_or_else(|| anyhow!("lower_generic_rt_call: unknown runtime symbol {name}"))?;
    let registry_params: Vec<ir::Type> = entry
        .sig
        .params
        .iter()
        .filter_map(|t| abi_type_to_cranelift(*t))
        .collect();
    let (params, ret): (&[ir::Type], Option<ir::Type>) = (
        registry_params.as_slice(),
        abi_type_to_cranelift(entry.sig.ret),
    );
    let mut arg_values = Vec::with_capacity(params.len());
    for (i, param_ty) in params.iter().enumerate() {
        let v = match args.get(i) {
            Some(a) if i == 1 && gossamer_abi::takes_elem_by_address(name) => {
                lower_elem_by_address(module, builder, locals, body, tcx, a, intrinsics)?
            }
            Some(a) if i == 1 && gossamer_abi::takes_key_by_address(name) => {
                lower_key_by_address(module, builder, locals, body, tcx, a, intrinsics)?
            }
            Some(a) => {
                let hint = if *param_ty == ptr_ty {
                    Some(ptr_ty)
                } else {
                    None
                };
                lower_operand(module, builder, locals, body, tcx, a, hint, intrinsics)?
            }
            None => {
                if param_ty.is_int() {
                    builder.ins().iconst(*param_ty, 0)
                } else {
                    builder.ins().iconst(ptr_ty, 0)
                }
            }
        };
        let coerced = coerce_arg_to(builder, v, *param_ty)?;
        arg_values.push(coerced);
    }
    let result = emit_win64_rt_call(module, builder, intrinsics, name, params, ret, &arg_values)?;
    if let Some(value) = result
        && gossamer_abi::returns_fresh_aggregate(name)
    {
        return store_runtime_call_result(
            module,
            builder,
            locals,
            body,
            tcx,
            destination,
            value,
            intrinsics,
            name,
        );
    }
    // An `unwrap` answers the address of the payload words a carrier's box
    // keeps. The destination holds shares of its own and gives them back on
    // its next overwrite, which may come after the box is gone, so it copies
    // the words into its own slot rather than naming the box.
    if let Some(src) = result
        && matches!(name, "gos_rt_result_unwrap" | "gos_rt_option_unwrap")
        && destination.projection.is_empty()
        && intrinsics.stack_slotted.contains(&destination.local)
        && !super::lowering_stmt_term::local_flows_to_return(body, destination.local)
        && super::lowering_stmt_term::payload_takes_own_storage(tcx, body, destination.local)
    {
        let slots = super::ty_layout::type_slot_count(tcx, body.local_ty(destination.local)).max(1);
        let dst_var = locals
            .get(&destination.local)
            .copied()
            .ok_or_else(|| anyhow!("unwrap destination has no slot"))?;
        let dst_raw = builder.use_var(dst_var);
        let dst_ptr = coerce_arg_to(builder, dst_raw, ptr_ty)?;
        let src_ptr = coerce_arg_to(builder, src, ptr_ty)?;
        for slot_idx in 0..slots {
            let off = ir::immediates::Offset32::new((slot_idx as i32) * 8);
            let word = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), src_ptr, off);
            builder
                .ins()
                .store(MemFlagsData::trusted(), word, dst_ptr, off);
        }
        return Ok(());
    }
    let stored = match result {
        Some(v) => v,
        None => builder.ins().iconst(types::I64, 0),
    };
    define_var_to(
        builder,
        locals,
        &intrinsics.body_cl_types,
        destination.local,
        stored,
    );
    Ok(())
}

pub(super) fn lower_external_binding_call(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    args: &[Operand],
    intrinsics: &mut IntrinsicContext,
    destination: &gossamer_mir::Place,
    name: &str,
) -> Result<()> {
    let ptr_ty = module.target_config().pointer_type();

    let dest_ty = body.local_ty(destination.local);
    let dest_cl_ty = mir_ty_to_cabi(tcx, dest_ty, ptr_ty);

    let mut params: Vec<ir::Type> = Vec::with_capacity(args.len());
    for arg in args {
        let ty = operand_cabi_ty(arg, body, tcx, ptr_ty);
        params.push(ty);
    }

    let returns: Vec<ir::Type> = match dest_cl_ty {
        Some(t) => vec![t],
        None => Vec::new(),
    };
    let static_name: &'static str = Box::leak(name.to_string().into_boxed_str());
    let extern_fn = intrinsics.extern_fn(module, static_name, &params, &returns)?;
    let fref = module.declare_func_in_func(extern_fn, builder.func);

    let mut arg_values: Vec<ir::Value> = Vec::with_capacity(args.len());
    for (arg, &param_ty) in args.iter().zip(params.iter()) {
        let v = lower_operand(
            module,
            builder,
            locals,
            body,
            tcx,
            arg,
            Some(param_ty),
            intrinsics,
        )?;
        let coerced = coerce_arg_to(builder, v, param_ty)?;
        arg_values.push(coerced);
    }

    let call = builder.ins().call(fref, &arg_values);
    if dest_cl_ty.is_some() {
        let v = builder.inst_results(call)[0];
        define_var_to(
            builder,
            locals,
            &intrinsics.body_cl_types,
            destination.local,
            v,
        );
    } else {
        let zero = builder.ins().iconst(types::I64, 0);
        define_var_to(
            builder,
            locals,
            &intrinsics.body_cl_types,
            destination.local,
            zero,
        );
    }
    Ok(())
}
