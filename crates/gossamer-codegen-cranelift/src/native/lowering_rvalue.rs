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

fn repeated_vec_child_words(tcx: &TyCtxt, ty: Ty) -> Vec<u32> {
    fn walk(tcx: &TyCtxt, ty: Ty, base: u32, out: &mut Vec<u32>, depth: u8) {
        if depth > 16 {
            return;
        }
        match tcx.kind_of(ty) {
            TyKind::Vec(_) => out.push(base),
            TyKind::Tuple(fields) => {
                let mut word = base;
                for field in fields {
                    walk(tcx, *field, word, out, depth + 1);
                    word = word.saturating_add(type_slot_count(tcx, *field).max(1));
                }
            }
            TyKind::Adt { def, substs } if !tcx.is_inline_enum_ty(ty) => {
                if let Some(fields) = tcx.adt_field_tys(*def, substs) {
                    let mut word = base;
                    for field in fields {
                        walk(tcx, *field, word, out, depth + 1);
                        word = word.saturating_add(type_slot_count(tcx, *field).max(1));
                    }
                }
            }
            TyKind::Array { elem, len } => {
                let stride = type_slot_count(tcx, *elem).max(1);
                if let Ok(count) = u32::try_from(len.to_usize()) {
                    for index in 0..count {
                        walk(
                            tcx,
                            *elem,
                            base.saturating_add(index.saturating_mul(stride)),
                            out,
                            depth + 1,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    walk(tcx, ty, 0, &mut out, 0);
    out
}

pub(super) fn lower_rvalue(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    rvalue: &Rvalue,
    dst_hint: Option<ir::Type>,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    lower_rvalue_into(
        module, builder, locals, body, tcx, rvalue, dst_hint, None, None, intrinsics,
    )
}

/// Lowers `rvalue`, optionally writing an `Aggregate` / `Repeat` result into
/// `dest_base` (a caller-owned backing slot) rather than a fresh
/// `gos_rt_aggr_alloc` heap block. Used for non-escaping aggregate locals so a
/// hot loop reuses one stack slot per iteration instead of leaking a heap
/// block each time (the bytecode VM and the LLVM tier already keep these on the
/// stack). Every non-aggregate rvalue ignores `dest_base`.
/// Stores the low and high 64-bit halves of an `i128` carrier value
/// (`Option<T>` / `Result<T, E>` - the `[disc, payload]` pair) into
/// `base + off` / `base + off + 8`, the same little-endian two-word
/// layout a `load.i128` of the slot reproduces.
pub(super) fn store_i128_words(
    builder: &mut FunctionBuilder<'_>,
    val: ir::Value,
    base: ir::Value,
    off: i32,
) {
    let (lo, hi) = builder.ins().isplit(val);
    builder.ins().store(
        MemFlagsData::trusted(),
        lo,
        base,
        ir::immediates::Offset32::new(off),
    );
    builder.ins().store(
        MemFlagsData::trusted(),
        hi,
        base,
        ir::immediates::Offset32::new(off + 8),
    );
}

pub(super) fn lower_rvalue_into(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    rvalue: &Rvalue,
    dst_hint: Option<ir::Type>,
    dest_ty: Option<Ty>,
    dest_base: Option<ir::Value>,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    Ok(match rvalue {
        Rvalue::Use(operand) => lower_operand(
            module, builder, locals, body, tcx, operand, dst_hint, intrinsics,
        )?,
        Rvalue::BinaryOp { op, lhs, rhs } => {
            // For arithmetic, both operands share the result's cl
            // type, so forward `dst_hint` down. For comparisons the
            // result is I8 (bool) but operands aren't - fall through
            // to MIR-local inference by leaving `hint` as None. An
            // operand-side cross-hint (lhs's lowered type seeds
            // rhs's hint) handles comparisons of projected places
            // whose MIR local type is an opaque ADT.
            let arith_hint = match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => None,
                _ => dst_hint,
            };
            let a = lower_operand(
                module, builder, locals, body, tcx, lhs, arith_hint, intrinsics,
            )?;
            let b_hint = arith_hint.or_else(|| Some(value_type(a, builder)));
            let b = lower_operand(module, builder, locals, body, tcx, rhs, b_hint, intrinsics)?;
            // String comparisons must go through `gos_rt_str_compare`
            // rather than comparing pointer addresses (which would
            // produce random ordering based on heap layout).
            let is_str_cmp = matches!(
                op,
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
            ) && operand_is_string(tcx, body, lhs);
            if is_str_cmp {
                let ptr_ty = module.target_config().pointer_type();
                let cmp_fn = intrinsics.extern_fn(
                    module,
                    "gos_rt_str_compare",
                    &[ptr_ty, ptr_ty],
                    &[types::I32],
                )?;
                let cmp_ref = module.declare_func_in_func(cmp_fn, builder.func);
                let call = builder.ins().call(cmp_ref, &[a, b]);
                let cmp_result = builder.inst_results(call)[0];
                let zero = builder.ins().iconst(types::I32, 0);
                match op {
                    BinOp::Eq => builder.ins().icmp(IntCC::Equal, cmp_result, zero),
                    BinOp::Ne => builder.ins().icmp(IntCC::NotEqual, cmp_result, zero),
                    BinOp::Lt => builder.ins().icmp(IntCC::SignedLessThan, cmp_result, zero),
                    BinOp::Le => builder
                        .ins()
                        .icmp(IntCC::SignedLessThanOrEqual, cmp_result, zero),
                    BinOp::Gt => builder
                        .ins()
                        .icmp(IntCC::SignedGreaterThan, cmp_result, zero),
                    BinOp::Ge => {
                        builder
                            .ins()
                            .icmp(IntCC::SignedGreaterThanOrEqual, cmp_result, zero)
                    }
                    _ => unreachable!(),
                }
            // Float `%` on f64 → `libc::fmod`. Cranelift has no
            // direct opcode. Intercept before the generic binop
            // dispatch so the rest stays module-free.
            } else if matches!(op, BinOp::Rem) && value_type(a, builder).is_float() {
                let fmod_fn = intrinsics.extern_fn(
                    module,
                    "fmod",
                    &[types::F64, types::F64],
                    &[types::F64],
                )?;
                let fref = module.declare_func_in_func(fmod_fn, builder.func);
                let a64 = if value_type(a, builder) == types::F32 {
                    builder.ins().fpromote(types::F64, a)
                } else {
                    a
                };
                let b64 = if value_type(b, builder) == types::F32 {
                    builder.ins().fpromote(types::F64, b)
                } else {
                    b
                };
                let call = builder.ins().call(fref, &[a64, b64]);
                builder.inst_results(call)[0]
            } else {
                let unsigned = cmp_shift_is_unsigned(tcx, body, lhs, rhs);
                let arithmetic_ty = arithmetic_int_ty(tcx, body, lhs, rhs).or_else(|| {
                    dest_ty.and_then(|ty| match tcx.kind_of(ty) {
                        TyKind::Int(int_ty) => Some(*int_ty),
                        _ => None,
                    })
                });
                lower_binop(
                    module,
                    builder,
                    intrinsics,
                    *op,
                    a,
                    b,
                    unsigned,
                    arithmetic_ty,
                )?
            }
        }
        Rvalue::UnaryOp { op, operand } => {
            let v = lower_operand(
                module, builder, locals, body, tcx, operand, dst_hint, intrinsics,
            )?;
            match op {
                UnOp::Neg => {
                    if value_type(v, builder).is_float() {
                        builder.ins().fneg(v)
                    } else {
                        builder.ins().ineg(v)
                    }
                }
                UnOp::Not => {
                    // For booleans (cranelift `I8`) `!` is *logical*
                    // negation - flip bit 0 only. `bnot` flips
                    // every bit (1 → 0xfe), which the downstream
                    // `if !flag` non-zero test then misreads as
                    // true and the user code's `if !first` arm
                    // always fires. For wider integer types the
                    // user wrote bitwise NOT (Rust convention),
                    // so keep `bnot` there.
                    let vt = value_type(v, builder);
                    if vt == types::I8 {
                        let one = builder.ins().iconst(types::I8, 1);
                        builder.ins().bxor(v, one)
                    } else {
                        builder.ins().bnot(v)
                    }
                }
            }
        }
        Rvalue::Cast { operand, target } => {
            // Determine source / destination cranelift type and
            // emit the right conversion. Without this i64 ↔ f64
            // bit-transmutes (the operand SSA value just gets
            // reused), which silently miscompiles benchmarks
            // that do `i as f64`.
            let src_v = lower_operand(
                module, builder, locals, body, tcx, operand, dst_hint, intrinsics,
            )?;
            let src_ty = builder.func.dfg.value_type(src_v);
            let dst_ty = cl_type_of(tcx, *target, module);
            match (src_ty, dst_ty) {
                // Same cranelift type. Under the i64 runtime model a
                // narrow declared target still masks: the cast is the
                // language's single truncation point (VM parity:
                // `300 as u8` == 44, `200 as i8` == -56). Reduce to
                // the declared width and extend back by the target's
                // signedness.
                (a, b) if a == b => {
                    let narrow = match tcx.kind_of(*target) {
                        TyKind::Int(IntTy::I8 | IntTy::U8) => Some(types::I8),
                        TyKind::Int(IntTy::I16 | IntTy::U16) => Some(types::I16),
                        TyKind::Int(IntTy::I32 | IntTy::U32) => Some(types::I32),
                        _ => None,
                    };
                    match narrow {
                        Some(n) if a == types::I64 => {
                            let reduced = builder.ins().ireduce(n, src_v);
                            let unsigned = matches!(
                                tcx.kind_of(*target),
                                TyKind::Int(IntTy::U8 | IntTy::U16 | IntTy::U32)
                            );
                            if unsigned {
                                builder.ins().uextend(types::I64, reduced)
                            } else {
                                builder.ins().sextend(types::I64, reduced)
                            }
                        }
                        _ => src_v,
                    }
                }
                // Integer → float (f32 / f64). Use signed
                // conversion since Gossamer's primary integer is
                // signed `i64`. Unsigned casts go through a same-
                // width int rebox before this point.
                (s, d) if s.is_int() && d.is_float() => builder.ins().fcvt_from_sint(d, src_v),
                // Float → integer saturates at the TARGET's range, so
                // `300.7 as u8` is `255` and `-1.5 as u8` is `0`; NaN reads
                // as zero. The intrinsic saturates at the machine width, and
                // the declared width clamps what it answers.
                (s, d) if s.is_float() && d.is_int() => {
                    let converted = builder.ins().fcvt_to_sint_sat(d, src_v);
                    let (width, signed) = match tcx.kind_of(*target) {
                        TyKind::Int(IntTy::I8) => (8, true),
                        TyKind::Int(IntTy::U8) => (8, false),
                        TyKind::Int(IntTy::I16) => (16, true),
                        TyKind::Int(IntTy::U16) => (16, false),
                        TyKind::Int(IntTy::I32) => (32, true),
                        TyKind::Int(IntTy::U32) => (32, false),
                        _ => (64, true),
                    };
                    if width >= 64 || d != types::I64 {
                        converted
                    } else {
                        let (low, high) = gossamer_abi::int_range::bounds(width, signed);
                        let low_v = builder.ins().iconst(types::I64, low);
                        let high_v = builder.ins().iconst(types::I64, high);
                        let lifted = builder.ins().smax(converted, low_v);
                        builder.ins().smin(lifted, high_v)
                    }
                }
                // `u8 as char`: mask to the declared u8 width before
                // narrowing into the char's i32 code-point slot -
                // matches the VM's `cast_scalar` and the LLVM tier.
                (s, d)
                    if s.is_int() && d.is_int() && matches!(tcx.kind_of(*target), TyKind::Char) =>
                {
                    let masked = builder.ins().band_imm_s(src_v, 0xFF);
                    if d.bits() < s.bits() {
                        builder.ins().ireduce(d, masked)
                    } else if d.bits() > s.bits() {
                        builder.ins().uextend(d, masked)
                    } else {
                        masked
                    }
                }
                // Integer width adjustments.
                (s, d) if s.is_int() && d.is_int() => {
                    let converted = if d.bits() > s.bits() {
                        // Use zero-extension for unsigned source types
                        // (u8/u16/u32) so that e.g. `255u8 as i32`
                        // yields 255, not -1. `bool` / `char` sources
                        // (i1-style i8 / code-point i32) are likewise
                        // non-negative and zero-extend.
                        let src_unsigned = if let Operand::Copy(place) = operand {
                            matches!(
                                tcx.kind_of(body.local_ty(place.local)),
                                TyKind::Int(IntTy::U8 | IntTy::U16 | IntTy::U32)
                                    | TyKind::Bool
                                    | TyKind::Char
                            )
                        } else {
                            matches!(
                                operand,
                                Operand::Const(ConstValue::Bool(_) | ConstValue::Char(_))
                            )
                        };
                        if src_unsigned {
                            builder.ins().uextend(d, src_v)
                        } else {
                            builder.ins().sextend(d, src_v)
                        }
                    } else if d.bits() < s.bits() {
                        builder.ins().ireduce(d, src_v)
                    } else {
                        src_v
                    };
                    // A narrow declared target still masks when the
                    // source SSA type was not already i64 (`char as
                    // u8`, `bool as i8`): reduce to the declared
                    // width and extend back by its signedness. The
                    // i64 → i64 narrow case is handled by the
                    // same-type arm above and never reaches here.
                    let narrow = match tcx.kind_of(*target) {
                        TyKind::Int(IntTy::I8 | IntTy::U8) => Some(types::I8),
                        TyKind::Int(IntTy::I16 | IntTy::U16) => Some(types::I16),
                        TyKind::Int(IntTy::I32 | IntTy::U32) => Some(types::I32),
                        _ => None,
                    };
                    match narrow {
                        Some(n) if n.bits() < d.bits() => {
                            let reduced = builder.ins().ireduce(n, converted);
                            let unsigned = matches!(
                                tcx.kind_of(*target),
                                TyKind::Int(IntTy::U8 | IntTy::U16 | IntTy::U32)
                            );
                            if unsigned {
                                builder.ins().uextend(d, reduced)
                            } else {
                                builder.ins().sextend(d, reduced)
                            }
                        }
                        _ => converted,
                    }
                }
                // Float width adjustments (f32 ↔ f64).
                (s, d) if s.is_float() && d.is_float() => {
                    if d.bits() > s.bits() {
                        builder.ins().fpromote(d, src_v)
                    } else if d.bits() < s.bits() {
                        builder.ins().fdemote(d, src_v)
                    } else {
                        src_v
                    }
                }
                _ => src_v,
            }
        }
        Rvalue::Aggregate { kind, operands } => {
            // Aggregates live in a stack slot N*8 bytes wide. Each
            // scalar field occupies an 8-byte slot. Arrays of
            // structs stride by (#struct-fields) slots so that
            // `a[i].f` projects correctly.
            //
            // Structs (`AggregateKind::Adt`) with struct variant
            // shapes use the flat-slot layout where every nested
            // struct/tuple field expands inline - the running byte
            // offset is summed from `type_slot_count` of each prior
            // operand's source, so `outer.tag` lands past the
            // embedded `inner` instead of overlapping it.
            let elem_slots: u32 = match kind {
                // A nested-aggregate element (e.g. `[Boxed; N]` where
                // `Boxed { p: Point }` is 2 slots) may lack `local_slots`
                // metadata; `operand_elem_slots` falls back to the element
                // type's slot count so the array strides by its full width,
                // matching the reader's `stride_slots_from_ty`. Without it the
                // stride collapses to one slot and every element past the first
                // reads past the buffer.
                gossamer_mir::AggregateKind::Array => operands.first().map_or(1, |op| {
                    operand_elem_slots(&intrinsics.local_slots, tcx, body, op).unwrap_or(1)
                }),
                _ => 1,
            };
            // Pre-compute per-operand slot widths for ADT/Tuple
            // aggregates. These drive both the total allocation
            // size and the running destination offset for each
            // field's store/memcpy below.
            let operand_slot_widths: Vec<u32> = match kind {
                gossamer_mir::AggregateKind::Adt { def, .. } => {
                    let from_tcx = tcx.struct_field_tys(*def).map(|tys| {
                        tys.iter()
                            .map(|t| type_slot_count(tcx, *t))
                            .collect::<Vec<_>>()
                    });
                    if let Some(widths) = from_tcx {
                        // Pad with 1-slot defaults if MIR provides
                        // more operands than the registered field
                        // list (defensive).
                        let mut widths = widths;
                        while widths.len() < operands.len() {
                            widths.push(1);
                        }
                        widths
                    } else {
                        operands
                            .iter()
                            .map(|op| match op {
                                Operand::Copy(place) if place.projection.is_empty() => intrinsics
                                    .local_slots
                                    .get(&place.local)
                                    .copied()
                                    .or_else(|| {
                                        let ty = body.local_ty(place.local);
                                        let n = type_slot_count(tcx, ty);
                                        if n > 1 { Some(n) } else { None }
                                    })
                                    .unwrap_or(1),
                                Operand::Copy(place) => {
                                    let leaf = resolve_place_ty(tcx, body, place);
                                    type_slot_count(tcx, leaf).max(1)
                                }
                                _ => 1,
                            })
                            .collect()
                    }
                }
                gossamer_mir::AggregateKind::Tuple => operands
                    .iter()
                    .map(|op| match op {
                        Operand::Copy(place) if place.projection.is_empty() => intrinsics
                            .local_slots
                            .get(&place.local)
                            .copied()
                            .or_else(|| {
                                let ty = body.local_ty(place.local);
                                let n = type_slot_count(tcx, ty);
                                if n > 1 { Some(n) } else { None }
                            })
                            .unwrap_or(1),
                        Operand::Copy(place) => {
                            let leaf = resolve_place_ty(tcx, body, place);
                            type_slot_count(tcx, leaf).max(1)
                        }
                        _ => 1,
                    })
                    .collect(),
                gossamer_mir::AggregateKind::Array => Vec::new(),
            };
            let total_slots: u32 = match kind {
                gossamer_mir::AggregateKind::Array => (operands.len() as u32) * elem_slots,
                _ => operand_slot_widths.iter().copied().sum::<u32>().max(1),
            };
            let size = total_slots * 8;
            let ptr_ty = module.target_config().pointer_type();
            // A caller-owned backing slot (a non-escaping aggregate local's
            // stack slot) receives the fields directly - no heap block, so a
            // hot loop reuses one slot per iteration instead of leaking. When
            // the aggregate may escape the frame (returned, stored in a
            // longer-lived container) the caller passes `None` and we
            // heap-allocate (zeroed) via `gos_rt_aggr_alloc`, whose backing
            // block outlives the frame.
            let base = if let Some(db) = dest_base {
                db
            } else {
                let alloc_fn =
                    intrinsics.extern_fn(module, "gos_rt_aggr_alloc", &[types::I64], &[ptr_ty])?;
                let alloc_ref = module.declare_func_in_func(alloc_fn, builder.func);
                let size_val = builder.ins().iconst(types::I64, i64::from(size.max(8)));
                let alloc_call = builder.ins().call(alloc_ref, &[size_val]);
                builder.inst_results(alloc_call)[0]
            };
            // Running destination offset (in bytes) for ADT/Tuple
            // aggregates so each prior nested struct's full slot
            // span shifts subsequent fields past its layout.
            let mut running_dst_off: u32 = 0;
            for (i, operand) in operands.iter().enumerate() {
                // A `Copy(local)` operand whose source local is a
                // pointer-to-aggregate (has `local_slots` metadata)
                // must be memcpy'd into the destination: the source
                // Variable's value is the source's base address, not
                // its contents. The simple `store` path is only
                // correct for scalar operands (ints/floats/booleans
                // - values that live directly in the local's SSA
                // Variable).
                // A multi-slot source local (with or without recorded metadata)
                // is memcpy'd by its slot count, not stored as a single
                // pointer-shaped word. `operand_elem_slots` covers both the
                // bare-local and projected-field cases with the same type
                // fallback used for the array stride above.
                let operand_aggregate_slots: Option<u32> =
                    operand_elem_slots(&intrinsics.local_slots, tcx, body, operand);
                let dst_off = match kind {
                    gossamer_mir::AggregateKind::Array => (i as u32) * elem_slots * 8,
                    _ => running_dst_off,
                };
                if !matches!(kind, gossamer_mir::AggregateKind::Array) {
                    let width = operand_slot_widths.get(i).copied().unwrap_or(1).max(1);
                    running_dst_off = running_dst_off.saturating_add(width * 8);
                }
                if let Some(copy_slots) = operand_aggregate_slots {
                    let src = lower_operand(
                        module, builder, locals, body, tcx, operand, None, intrinsics,
                    )?;
                    if value_type(src, builder) == types::I128 {
                        // A two-slot inline carrier (`Option<T>` /
                        // `Result<T, E>`) lowers to an i128 SSA *value*,
                        // not an address - store its halves directly
                        // rather than word-copying through it as a
                        // pointer.
                        store_i128_words(builder, src, base, dst_off as i32);
                    } else {
                        for slot_idx in 0..copy_slots {
                            let off = (slot_idx as i32) * 8;
                            let word = builder.ins().load(
                                types::I64,
                                MemFlagsData::trusted(),
                                src,
                                ir::immediates::Offset32::new(off),
                            );
                            builder.ins().store(
                                MemFlagsData::trusted(),
                                word,
                                base,
                                ir::immediates::Offset32::new((dst_off as i32) + off),
                            );
                        }
                    }
                } else {
                    let value = lower_operand(
                        module, builder, locals, body, tcx, operand, None, intrinsics,
                    )?;
                    let word = widen_to_slot_word(builder, tcx, body, operand, value);
                    builder.ins().store(
                        MemFlagsData::trusted(),
                        word,
                        base,
                        ir::immediates::Offset32::new(dst_off as i32),
                    );
                }
            }
            let _ = kind;
            base
        }
        Rvalue::Len(place) => {
            // With the flat-8-byte layout we can't recover the
            // aggregate length from the pointer alone. Emit a
            // placeholder zero - callers that actually need `len`
            // will use it with arrays of known size via MIR opt.
            let _ = place;
            builder.ins().iconst(types::I64, 0)
        }
        Rvalue::Repeat { value, count } => {
            // `Some(n)` marks an address-represented aggregate element (its
            // operand value is a pointer whose `n` slots are copied per
            // repetition, `n == 1` included); `None` is a scalar element
            // stored by value.
            let operand_agg_slots: Option<u32> =
                operand_elem_slots(&intrinsics.local_slots, tcx, body, value);
            let elem_slots: u32 = operand_agg_slots.unwrap_or(1);
            let vec_child_words = match value {
                Operand::Copy(place) => repeated_vec_child_words(tcx, body.local_ty(place.local)),
                Operand::Const(_) | Operand::FnRef { .. } => Vec::new(),
            };
            let total_slots = u32::try_from(*count)
                .map_err(|_| anyhow!("native codegen: repeat count too large"))?
                .saturating_mul(elem_slots);
            let size = total_slots.saturating_mul(8);
            let ptr_ty = module.target_config().pointer_type();
            // As in the `Aggregate` path: fill a caller-owned non-escaping
            // slot directly, else heap-allocate (zeroed) via
            // `gos_rt_aggr_alloc`. The assign path treats any slot it hands
            // in as filled on return (it does not rebind the local to the
            // returned pointer), so a provided `dest_base` must always be
            // the buffer written here - including for scalar elements,
            // where the slot (unlike the heap block) is not pre-zeroed and
            // the zero-store elision below must not fire.
            let base = if let Some(db) = dest_base {
                db
            } else {
                let alloc_fn =
                    intrinsics.extern_fn(module, "gos_rt_aggr_alloc", &[types::I64], &[ptr_ty])?;
                let alloc_ref = module.declare_func_in_func(alloc_fn, builder.func);
                let size_val = builder.ins().iconst(types::I64, i64::from(size.max(8)));
                let alloc_call = builder.ins().call(alloc_ref, &[size_val]);
                builder.inst_results(alloc_call)[0]
            };
            // Threshold for switching from unrolled stores to a counted
            // loop. Unrolling beyond this generates O(count) Cranelift
            // instructions - for `[f64; 6000]` that inflates the JIT IR
            // to tens of thousands of ops, pushing peak RSS ~30 MB for a
            // single compilation. A loop keeps the IR size O(1).
            const UNROLL_LIMIT: u64 = 16;
            if operand_agg_slots.is_some() {
                if let Operand::Copy(place) = value {
                    let src = lower_place_read(
                        module,
                        builder,
                        locals,
                        body,
                        tcx,
                        &Place::local(place.local),
                        Some(ptr_ty),
                        intrinsics,
                    )?;
                    if value_type(src, builder) == types::I128 {
                        // A two-slot inline carrier element is an i128
                        // value, not an address - store its halves per
                        // repetition instead of loading through it.
                        if *count <= UNROLL_LIMIT {
                            for i in 0..*count {
                                let dst_offset = i32::try_from(i * 16).map_err(|_| {
                                    anyhow!("native codegen: repeat offset too large")
                                })?;
                                store_i128_words(builder, src, base, dst_offset);
                            }
                        } else {
                            let (lo, hi) = builder.ins().isplit(src);
                            let loop_hdr = builder.create_block();
                            let loop_body = builder.create_block();
                            let exit_blk = builder.create_block();
                            builder.append_block_param(loop_hdr, types::I64);
                            let zero = builder.ins().iconst(types::I64, 0);
                            builder.ins().jump(loop_hdr, &[ir::BlockArg::Value(zero)]);
                            builder.switch_to_block(loop_hdr);
                            let ctr = builder.block_params(loop_hdr)[0];
                            let cnt_v = builder.ins().iconst(types::I64, *count as i64);
                            let ok = builder.ins().icmp(IntCC::SignedLessThan, ctr, cnt_v);
                            builder.ins().brif(ok, loop_body, &[], exit_blk, &[]);
                            builder.switch_to_block(loop_body);
                            let byte_off = builder.ins().imul_imm_s(ctr, 16_i64);
                            let dst = builder.ins().iadd(base, byte_off);
                            builder.ins().store(
                                MemFlagsData::trusted(),
                                lo,
                                dst,
                                ir::immediates::Offset32::new(0),
                            );
                            builder.ins().store(
                                MemFlagsData::trusted(),
                                hi,
                                dst,
                                ir::immediates::Offset32::new(8),
                            );
                            let next = builder.ins().iadd_imm_s(ctr, 1);
                            builder.ins().jump(loop_hdr, &[ir::BlockArg::Value(next)]);
                            builder.switch_to_block(exit_blk);
                        }
                    } else if *count <= UNROLL_LIMIT {
                        for i in 0..*count {
                            let dst_offset = (i as u32) * elem_slots * 8;
                            for slot_idx in 0..elem_slots {
                                let off = (slot_idx as i32) * 8;
                                let word = builder.ins().load(
                                    types::I64,
                                    MemFlagsData::trusted(),
                                    src,
                                    ir::immediates::Offset32::new(off),
                                );
                                builder.ins().store(
                                    MemFlagsData::trusted(),
                                    word,
                                    base,
                                    ir::immediates::Offset32::new((dst_offset as i32) + off),
                                );
                            }
                            for child_word in &vec_child_words {
                                let child = builder.ins().load(
                                    ptr_ty,
                                    MemFlagsData::trusted(),
                                    src,
                                    ir::immediates::Offset32::new(*child_word as i32 * 8),
                                );
                                let clone_fn = intrinsics.extern_fn(
                                    module,
                                    "gos_rt_vec_clone",
                                    &[ptr_ty],
                                    &[ptr_ty],
                                )?;
                                let clone_ref = module.declare_func_in_func(clone_fn, builder.func);
                                let call = builder.ins().call(clone_ref, &[child]);
                                let cloned = builder.inst_results(call)[0];
                                builder.ins().store(
                                    MemFlagsData::trusted(),
                                    cloned,
                                    base,
                                    ir::immediates::Offset32::new(
                                        dst_offset as i32 + *child_word as i32 * 8,
                                    ),
                                );
                            }
                        }
                    } else {
                        // Counted loop: counter in loop_header block param.
                        let loop_hdr = builder.create_block();
                        let loop_body = builder.create_block();
                        let exit_blk = builder.create_block();
                        builder.append_block_param(loop_hdr, types::I64);
                        let zero = builder.ins().iconst(types::I64, 0);
                        builder.ins().jump(loop_hdr, &[ir::BlockArg::Value(zero)]);
                        builder.switch_to_block(loop_hdr);
                        let ctr = builder.block_params(loop_hdr)[0];
                        let cnt_v = builder.ins().iconst(types::I64, *count as i64);
                        let ok = builder.ins().icmp(IntCC::SignedLessThan, ctr, cnt_v);
                        builder.ins().brif(ok, loop_body, &[], exit_blk, &[]);
                        builder.switch_to_block(loop_body);
                        let stride = i64::from(elem_slots) * 8;
                        let dst_base = builder.ins().imul_imm_s(ctr, stride);
                        for slot_idx in 0..elem_slots {
                            let src_off = ir::immediates::Offset32::new(slot_idx as i32 * 8);
                            let word = builder.ins().load(
                                types::I64,
                                MemFlagsData::trusted(),
                                src,
                                src_off,
                            );
                            let slot_off =
                                builder.ins().iadd_imm_s(dst_base, i64::from(slot_idx) * 8);
                            let dst = builder.ins().iadd(base, slot_off);
                            builder.ins().store(
                                MemFlagsData::trusted(),
                                word,
                                dst,
                                ir::immediates::Offset32::new(0),
                            );
                        }
                        for child_word in &vec_child_words {
                            let child = builder.ins().load(
                                ptr_ty,
                                MemFlagsData::trusted(),
                                src,
                                ir::immediates::Offset32::new(*child_word as i32 * 8),
                            );
                            let clone_fn = intrinsics.extern_fn(
                                module,
                                "gos_rt_vec_clone",
                                &[ptr_ty],
                                &[ptr_ty],
                            )?;
                            let clone_ref = module.declare_func_in_func(clone_fn, builder.func);
                            let call = builder.ins().call(clone_ref, &[child]);
                            let cloned = builder.inst_results(call)[0];
                            let child_off = builder
                                .ins()
                                .iadd_imm_s(dst_base, i64::from(*child_word) * 8);
                            let child_dst = builder.ins().iadd(base, child_off);
                            builder.ins().store(
                                MemFlagsData::trusted(),
                                cloned,
                                child_dst,
                                ir::immediates::Offset32::new(0),
                            );
                        }
                        let next = builder.ins().iadd_imm_s(ctr, 1);
                        builder.ins().jump(loop_hdr, &[ir::BlockArg::Value(next)]);
                        builder.switch_to_block(exit_blk);
                    }
                }
            } else {
                // Scalar repeat (`[v; N]` where v is one slot wide).
                // The heap block arrives calloc-zeroed, so zero constants
                // need no stores there; a caller-provided stack slot holds
                // stale words and must be written even for zeros.
                let is_zero = dest_base.is_none()
                    && matches!(
                        value,
                        Operand::Const(
                            ConstValue::Int(0)
                                | ConstValue::Float(0)
                                | ConstValue::Bool(false)
                                | ConstValue::Unit
                        )
                    );
                if !is_zero {
                    let element =
                        lower_operand(module, builder, locals, body, tcx, value, None, intrinsics)?;
                    if *count <= UNROLL_LIMIT {
                        for i in 0..*count {
                            let offset = ir::immediates::Offset32::new(
                                i32::try_from(i.saturating_mul(8)).map_err(|_| {
                                    anyhow!("native codegen: repeat offset too large")
                                })?,
                            );
                            builder
                                .ins()
                                .store(MemFlagsData::trusted(), element, base, offset);
                        }
                    } else {
                        let loop_hdr = builder.create_block();
                        let loop_body = builder.create_block();
                        let exit_blk = builder.create_block();
                        builder.append_block_param(loop_hdr, types::I64);
                        let zero = builder.ins().iconst(types::I64, 0);
                        builder.ins().jump(loop_hdr, &[ir::BlockArg::Value(zero)]);
                        builder.switch_to_block(loop_hdr);
                        let ctr = builder.block_params(loop_hdr)[0];
                        let cnt_v = builder.ins().iconst(types::I64, *count as i64);
                        let ok = builder.ins().icmp(IntCC::SignedLessThan, ctr, cnt_v);
                        builder.ins().brif(ok, loop_body, &[], exit_blk, &[]);
                        builder.switch_to_block(loop_body);
                        let byte_off = builder.ins().imul_imm_s(ctr, 8_i64);
                        let dst = builder.ins().iadd(base, byte_off);
                        builder.ins().store(
                            MemFlagsData::trusted(),
                            element,
                            dst,
                            ir::immediates::Offset32::new(0),
                        );
                        let next = builder.ins().iadd_imm_s(ctr, 1);
                        builder.ins().jump(loop_hdr, &[ir::BlockArg::Value(next)]);
                        builder.switch_to_block(exit_blk);
                    }
                }
            }
            base
        }
        // `&place` / `&mut place` → the address of `place`. For a
        // bare local, that's the Variable's SSA value (which is
        // already a pointer when the local holds an aggregate);
        // for a projected place, it's the computed projection
        // address.
        Rvalue::Ref { place, mutable } => {
            if place.projection.is_empty() {
                let var = ensure_var(
                    builder,
                    locals,
                    body,
                    tcx,
                    module,
                    &intrinsics.body_cl_types,
                    place.local,
                );
                let val = builder.use_var(var);
                // Scalar locals (i64 / f64 / bool / char from primitive
                // HIR types) and `String` locals (a flat `*mut c_char`
                // pointer-value) live in SSA Variables that have no
                // machine address - `use_var` returns the *value*. When
                // the caller asks for `&x`, we need an actual pointer.
                // Materialise a fresh stack slot, store the current
                // value (8 bytes for either a scalar or a pointer), and
                // return its address. This matches the LLVM tier (whose
                // locals are alloca-backed, so the slot address is
                // always available).
                //
                // A `&mut x` writeback through this pointer lands in the
                // throwaway stack slot, not in the original SSA
                // Variable. The post-call `place = *ref` reload emitted
                // by the MIR `lower_call` pass pulls the callee's new
                // value back into the Variable, completing the
                // round-trip. Aggregate locals (Vec / struct / map) are
                // already pointer-to-header, so `&x` is the value
                // itself - no slot needed.
                let ty = body.local_ty(place.local);
                // A local still typed as a type parameter belongs to a
                // template serving scalar instantiations - an aggregate one
                // is routed to its own specialised copy - so its slot holds a
                // value and needs the same materialised address a scalar does.
                let is_addressable_value = matches!(
                    tcx.kind_of(ty),
                    gossamer_types::TyKind::Int(_)
                        | gossamer_types::TyKind::Float(_)
                        | gossamer_types::TyKind::Bool
                        | gossamer_types::TyKind::Char
                        | gossamer_types::TyKind::Param { .. }
                ) || (*mutable
                    && (matches!(tcx.kind_of(ty), gossamer_types::TyKind::String)
                        // A `&mut` payload enum is the receiver a callee
                        // rebinds whole (`*self = Variant(..)`), so it names a
                        // slot and the post-call reload carries the new node
                        // back into the Variable.
                        || tcx.is_payload_enum(ty)));
                // An `Option` / `Result` / inline user enum is the packed
                // two-word carrier held in a register, so it has no machine
                // address either. Its slot is the pair of words, and the
                // reference is that slot's address.
                let is_two_word_carrier = crate::native::compile::is_inline_two_word_ty(tcx, ty);
                if is_addressable_value || is_two_word_carrier {
                    let ptr_ty = module.target_config().pointer_type();
                    let bytes = if is_two_word_carrier { 16 } else { 8 };
                    let slot =
                        builder.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
                            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                            bytes,
                            8,
                        ));
                    builder.ins().stack_store(ptr_ty, val, slot, 0);
                    builder.ins().stack_addr(ptr_ty, slot, 0)
                } else {
                    val
                }
            } else {
                let leaf_ty = resolve_place_ty(tcx, body, place);
                let addr =
                    lower_place_address(module, builder, locals, body, tcx, place, intrinsics)?;
                if matches!(
                    tcx.kind_of(leaf_ty),
                    gossamer_types::TyKind::String
                        | gossamer_types::TyKind::Slice(_)
                        | gossamer_types::TyKind::Vec(_)
                        | gossamer_types::TyKind::HashMap { .. }
                ) {
                    builder.ins().load(
                        module.target_config().pointer_type(),
                        MemFlagsData::new(),
                        addr,
                        0,
                    )
                } else {
                    addr
                }
            }
        }
        // `CallIntrinsic` as an Rvalue is dispatched at the
        // `Assign` statement layer; reaching it here means the
        // statement path already returned. Unreachable in
        // practice.
        Rvalue::CallIntrinsic { .. } => {
            unreachable!("CallIntrinsic must be routed through the statement path")
        }
        // `static mut` scalar read: load from the static's backing writable
        // data object. Non-scalar statics (String/aggregate) keep the VM
        // fallback since their init is a heap value, not an inline word.
        Rvalue::StaticLoad(sref) => {
            if !is_scalar_static_ty(tcx, sref.ty) {
                bail!(
                    "native codegen: static mut load of non-scalar type unsupported; running on VM"
                )
            }
            let cl_ty = cl_type_of(tcx, sref.ty, module);
            let data_id = intrinsics.intern_static(module, sref, cl_ty)?;
            let ptr_ty = module.target_config().pointer_type();
            let gv = module.declare_data_in_func(data_id, builder.func);
            let addr = builder.ins().symbol_value(ptr_ty, gv);
            builder.ins().load(cl_ty, MemFlagsData::trusted(), addr, 0)
        }
    })
}

pub(super) fn lower_operand(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    operand: &Operand,
    hint: Option<ir::Type>,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    Ok(match operand {
        Operand::Copy(place) => {
            // For projected reads through a known aggregate root,
            // prefer the root's recorded element type over any
            // hint from the caller - the hint is an approximation,
            // the element table is ground truth.
            let effective_hint = if place.projection.is_empty() {
                hint
            } else {
                intrinsics.elem_cl_ty.get(&place.local).copied().or(hint)
            };
            lower_place_read(
                module,
                builder,
                locals,
                body,
                tcx,
                place,
                effective_hint,
                intrinsics,
            )?
        }
        Operand::Const(value) => lower_const(module, builder, value, hint, intrinsics)?,
        Operand::FnRef { def, .. } => {
            // `let f = some_fn; f(x)` passes the function by
            // reference. Emit a `func_addr` whose value is a
            // pointer-typed SSA value; the indirect-call path
            // picks it up through the local's variable.
            let ptr_ty = module.target_config().pointer_type();
            match intrinsics.functions_by_def.get(&def.local).copied() {
                Some(func_id) => {
                    let fr = module.declare_func_in_func(func_id, builder.func);
                    builder.ins().func_addr(ptr_ty, fr)
                }
                None => builder.ins().iconst(ptr_ty, 0),
            }
        }
    })
}

pub(super) fn lower_const(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    value: &ConstValue,
    hint: Option<ir::Type>,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    Ok(match value {
        ConstValue::Int(n) => {
            let ty = hint.filter(|t| t.is_int()).unwrap_or(types::I64);
            builder.ins().iconst(ty, i64_truncate(*n))
        }
        ConstValue::Bool(b) => {
            let ty = hint.filter(|t| t.is_int()).unwrap_or(types::I8);
            builder.ins().iconst(ty, i64::from(*b))
        }
        ConstValue::Char(c) => {
            let ty = hint.filter(|t| t.is_int()).unwrap_or(types::I32);
            builder.ins().iconst(ty, i64::from(u32::from(*c)))
        }
        ConstValue::Unit => builder.ins().iconst(types::I64, 0),
        ConstValue::Str(text) => {
            // String constants live in `.rodata` as null-terminated
            // bytes; the value we return is the address of those
            // bytes, sized as the target's pointer type.
            let data_id = intrinsics.intern_string(module, text)?;
            intrinsics.static_string_body_ptr(module, builder, data_id)
        }
        ConstValue::Float(bits) => {
            let ty = hint.filter(|t| t.is_float()).unwrap_or(types::F64);
            let val = f64::from_bits(*bits);
            if ty == types::F32 {
                builder.ins().f32const(val as f32)
            } else {
                builder.ins().f64const(val)
            }
        }
    })
}

/// Whether `< <= > >=` and `>>` on these operands must use unsigned
/// machine ops. Mirrors the LLVM backend and the VM: only `u64` /
/// `usize` (values that can exceed `i64::MAX`) need unsigned compare
/// plus logical shift; every signed type and every `<=32`-bit
/// unsigned type masks below `2^63` and compares identically as
/// signed. A constant operand carries no declared signedness, so the
/// place operand decides; two constants stay signed.
fn cmp_shift_is_unsigned(tcx: &TyCtxt, body: &Body, lhs: &Operand, rhs: &Operand) -> bool {
    let pick = |op: &Operand| -> Option<IntTy> {
        let Operand::Copy(place) = op else {
            return None;
        };
        let mut ty = resolve_place_ty(tcx, body, place);
        while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
            ty = *inner;
        }
        match tcx.kind_of(ty) {
            TyKind::Int(i) => Some(*i),
            _ => None,
        }
    };
    matches!(
        pick(lhs).or_else(|| pick(rhs)),
        Some(IntTy::U64 | IntTy::Usize | IntTy::U128)
    )
}

fn arithmetic_int_ty(tcx: &TyCtxt, body: &Body, lhs: &Operand, rhs: &Operand) -> Option<IntTy> {
    let pick = |op: &Operand| -> Option<IntTy> {
        let Operand::Copy(place) = op else {
            return None;
        };
        let mut ty = resolve_place_ty(tcx, body, place);
        while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
            ty = *inner;
        }
        match tcx.kind_of(ty) {
            TyKind::Int(int_ty) => Some(*int_ty),
            _ => None,
        }
    };
    pick(lhs).or_else(|| pick(rhs))
}

pub(super) fn lower_binop(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    op: BinOp,
    a: ir::Value,
    b: ir::Value,
    unsigned: bool,
    arithmetic_ty: Option<IntTy>,
) -> Result<ir::Value> {
    let mut a_ty = value_type(a, builder);
    let mut b_ty = value_type(b, builder);
    let mut a = a;
    let mut b = b;
    if a_ty != b_ty {
        // Reinterpret where possible: a common mismatch pattern is
        // a projected read whose MIR element type was left as an
        // unresolved inference variable, defaulting to `i64`,
        // paired with a concrete `f64` operand. Aggregates store
        // every scalar in an 8-byte slot, so the 8 bytes loaded
        // as an i64 are the same bits that were stored as an f64,
        // and a `bitcast` is a zero-cost reinterpret.
        if a_ty == types::I64 && b_ty == types::F64 {
            a = builder
                .ins()
                .bitcast(types::F64, ir::MemFlagsData::new(), a);
            a_ty = types::F64;
        } else if a_ty == types::F64 && b_ty == types::I64 {
            b = builder
                .ins()
                .bitcast(types::F64, ir::MemFlagsData::new(), b);
            b_ty = types::F64;
        } else if a_ty.is_int() && b_ty.is_int() {
            // Integer width mismatch: extend the narrower side up
            // to the wider one. Common cause is a closure capture
            // whose env-stored value was loaded with a wider type
            // than its source bool/i8 width - `if pred(x)` or a
            // comparison whose other operand is a full i64.
            if a_ty.bits() < b_ty.bits() {
                a = builder.ins().sextend(b_ty, a);
                a_ty = b_ty;
            } else {
                b = builder.ins().sextend(a_ty, b);
                b_ty = a_ty;
            }
        } else {
            bail!("native codegen: binop operand type mismatch (op={op:?}, {a_ty:?} vs {b_ty:?})");
        }
        let _ = b_ty;
    }
    if a_ty.is_float() {
        return Ok(match op {
            BinOp::Add => builder.ins().fadd(a, b),
            BinOp::Sub => builder.ins().fsub(a, b),
            BinOp::Mul => builder.ins().fmul(a, b),
            BinOp::Div => builder.ins().fdiv(a, b),
            // Float `%` is intercepted in lower_rvalue and routed
            // through libc::fmod before this match runs; reaching
            // here on a float means the caller bypassed that path
            // - a compiler bug.
            BinOp::Rem => unreachable!("float Rem handled in lower_rvalue"),
            BinOp::Eq => fcmp_bool(builder, ir::condcodes::FloatCC::Equal, a, b),
            BinOp::Ne => fcmp_bool(builder, ir::condcodes::FloatCC::NotEqual, a, b),
            BinOp::Lt => fcmp_bool(builder, ir::condcodes::FloatCC::LessThan, a, b),
            BinOp::Le => fcmp_bool(builder, ir::condcodes::FloatCC::LessThanOrEqual, a, b),
            BinOp::Gt => fcmp_bool(builder, ir::condcodes::FloatCC::GreaterThan, a, b),
            BinOp::Ge => fcmp_bool(builder, ir::condcodes::FloatCC::GreaterThanOrEqual, a, b),
            // Bitwise on float is a typecheck error; reaching
            // here is a compiler bug.
            BinOp::WrappingAdd
            | BinOp::WrappingMul
            | BinOp::BitAnd
            | BinOp::BitOr
            | BinOp::BitXor
            | BinOp::Shl
            | BinOp::Shr => {
                unreachable!("bitwise op on float - should be a type error")
            }
        });
    }
    if matches!(op, BinOp::WrappingAdd | BinOp::WrappingMul) {
        return Ok(match op {
            BinOp::WrappingAdd => builder.ins().iadd(a, b),
            BinOp::WrappingMul => builder.ins().imul(a, b),
            _ => unreachable!(),
        });
    }
    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) {
        let arithmetic_ty = arithmetic_ty.unwrap_or(IntTy::I64);
        let overflow_unsigned = matches!(
            arithmetic_ty,
            IntTy::U8 | IntTy::U16 | IntTy::U32 | IntTy::U64 | IntTy::U128 | IntTy::Usize
        );
        let checked_ty = match arithmetic_ty {
            IntTy::I8 | IntTy::U8 => types::I8,
            IntTy::I16 | IntTy::U16 => types::I16,
            IntTy::I32 | IntTy::U32 => types::I32,
            IntTy::I64 | IntTy::U64 | IntTy::I128 | IntTy::U128 | IntTy::Isize | IntTy::Usize => {
                types::I64
            }
        };
        let checked_a = if a_ty == checked_ty {
            a
        } else {
            builder.ins().ireduce(checked_ty, a)
        };
        let checked_b = if b_ty == checked_ty {
            b
        } else {
            builder.ins().ireduce(checked_ty, b)
        };
        let (value, overflow) = match (op, overflow_unsigned) {
            (BinOp::Add, true) => builder.ins().uadd_overflow(checked_a, checked_b),
            (BinOp::Add, false) => builder.ins().sadd_overflow(checked_a, checked_b),
            (BinOp::Sub, true) => builder.ins().usub_overflow(checked_a, checked_b),
            (BinOp::Sub, false) => builder.ins().ssub_overflow(checked_a, checked_b),
            (BinOp::Mul, true) => builder.ins().umul_overflow(checked_a, checked_b),
            (BinOp::Mul, false) => builder.ins().smul_overflow(checked_a, checked_b),
            _ => unreachable!(),
        };
        let pass = builder.create_block();
        let fail = builder.create_block();
        builder.ins().brif(overflow, fail, &[], pass, &[]);
        builder.switch_to_block(fail);
        // Name the operation, the way the bytecode tier and Rust both do: a
        // bare "arithmetic overflow" leaves the reader to find which operator
        // in the expression produced it.
        let overflow_text = match op {
            BinOp::Add => "attempt to add with overflow\n",
            BinOp::Sub => "attempt to subtract with overflow\n",
            _ => "attempt to multiply with overflow\n",
        };
        emit_runtime_panic(module, builder, intrinsics, overflow_text)?;
        builder.switch_to_block(pass);
        let value = if checked_ty == a_ty {
            value
        } else if overflow_unsigned {
            builder.ins().uextend(a_ty, value)
        } else {
            builder.ins().sextend(a_ty, value)
        };
        return Ok(value);
    }
    Ok(match op {
        BinOp::Add | BinOp::WrappingAdd | BinOp::Sub | BinOp::Mul | BinOp::WrappingMul => {
            unreachable!()
        }
        BinOp::Div => {
            if unsigned {
                builder.ins().udiv(a, b)
            } else {
                builder.ins().sdiv(a, b)
            }
        }
        BinOp::Rem => {
            if unsigned {
                builder.ins().urem(a, b)
            } else {
                builder.ins().srem(a, b)
            }
        }
        BinOp::BitAnd => builder.ins().band(a, b),
        BinOp::BitOr => builder.ins().bor(a, b),
        BinOp::BitXor => builder.ins().bxor(a, b),
        BinOp::Shl => builder.ins().ishl(a, b),
        // `u64` / `usize` operands shift logically (`ushr`); every
        // signed type and the narrower unsigned types keep the
        // arithmetic shift, matching the LLVM backend and the VM
        // (`wrapping_shr` over the declared signedness).
        BinOp::Shr => {
            if unsigned {
                builder.ins().ushr(a, b)
            } else {
                builder.ins().sshr(a, b)
            }
        }
        BinOp::Eq => compare_bool(builder, ir::condcodes::IntCC::Equal, a, b),
        BinOp::Ne => compare_bool(builder, ir::condcodes::IntCC::NotEqual, a, b),
        BinOp::Lt => compare_bool(builder, int_cmp_cc(BinOp::Lt, unsigned), a, b),
        BinOp::Le => compare_bool(builder, int_cmp_cc(BinOp::Le, unsigned), a, b),
        BinOp::Gt => compare_bool(builder, int_cmp_cc(BinOp::Gt, unsigned), a, b),
        BinOp::Ge => compare_bool(builder, int_cmp_cc(BinOp::Ge, unsigned), a, b),
    })
}

/// Cranelift integer condition for an ordered comparison, selecting
/// the unsigned variant for `u64` / `usize` operands. Equality is
/// sign-independent and handled by the caller.
fn int_cmp_cc(op: BinOp, unsigned: bool) -> ir::condcodes::IntCC {
    use ir::condcodes::IntCC;
    match (op, unsigned) {
        (BinOp::Lt, false) => IntCC::SignedLessThan,
        (BinOp::Lt, true) => IntCC::UnsignedLessThan,
        (BinOp::Le, false) => IntCC::SignedLessThanOrEqual,
        (BinOp::Le, true) => IntCC::UnsignedLessThanOrEqual,
        (BinOp::Gt, false) => IntCC::SignedGreaterThan,
        (BinOp::Gt, true) => IntCC::UnsignedGreaterThan,
        (BinOp::Ge, false) => IntCC::SignedGreaterThanOrEqual,
        (BinOp::Ge, true) => IntCC::UnsignedGreaterThanOrEqual,
        _ => unreachable!("int_cmp_cc only handles ordered comparisons"),
    }
}

/// Widens a scalar narrower than a slot to the whole word the slot holds, so
/// the bytes beside it carry the value rather than whatever the frame left
/// there. A signed integer keeps its sign; a `char`, a `bool`, and an
/// unsigned integer are magnitudes.
fn widen_to_slot_word(
    builder: &mut FunctionBuilder<'_>,
    tcx: &TyCtxt,
    body: &Body,
    operand: &Operand,
    value: ir::Value,
) -> ir::Value {
    let ty = builder.func.dfg.value_type(value);
    if !ty.is_int() || ty.bits() >= 64 {
        return value;
    }
    let signed = match operand {
        Operand::Copy(place) => matches!(
            tcx.kind_of(resolve_place_ty(tcx, body, place)),
            TyKind::Int(int_ty) if !int_ty_is_unsigned(*int_ty)
        ),
        Operand::Const(ConstValue::Int(_)) => true,
        _ => false,
    };
    if signed {
        builder.ins().sextend(types::I64, value)
    } else {
        builder.ins().uextend(types::I64, value)
    }
}
