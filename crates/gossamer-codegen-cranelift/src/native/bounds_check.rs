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

pub(super) fn emit_array_bounds_check(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    current_ty: Ty,
    idx_val: ir::Value,
    tcx: &TyCtxt,
) -> Result<()> {
    if std::env::var_os("GOSSAMER_DISABLE_BOUNDS_CHECK").is_some() {
        return Ok(());
    }
    let mut peeled = current_ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(peeled).clone() {
        peeled = inner;
    }
    let TyKind::Array { len, .. } = tcx.kind_of(peeled).clone() else {
        return Ok(());
    };
    let len_i64 = i64::try_from(len.to_usize()).unwrap_or(i64::MAX);
    // Widen the index to i64 for both the compare and the
    // helper-call payload. Cranelift requires both icmp operands to
    // share a type.
    let idx64 = match value_type(idx_val, builder) {
        t if t == types::I64 => idx_val,
        t if t.is_int() && t.bits() < 64 => builder.ins().sextend(types::I64, idx_val),
        _ => idx_val,
    };
    let len_val = builder.ins().iconst(types::I64, len_i64);
    // Unsigned >=: i64 compared as u64 also catches negative idx
    // (which wrap to >= 2^63 - strictly greater than any sane len).
    let oob = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, idx64, len_val);
    let ok = builder.create_block();
    let fail = builder.create_block();
    builder.ins().brif(oob, fail, &[], ok, &[]);
    builder.switch_to_block(fail);
    // Call `gos_rt_panic_oob("array index", idx, len)` then fall
    // into an `unreachable` trap so the verifier sees a terminator
    // even though the helper is `-> !`. Blocks are sealed
    // collectively at function-end via `seal_all_blocks`.
    let panic_fn = intrinsics.extern_fn_by_name(module, "gos_rt_panic_oob")?;
    let panic_ref = module.declare_func_in_func(panic_fn, builder.func);
    let what_data = intrinsics.intern_string(module, "array index")?;
    let what_ptr = intrinsics.static_string_body_ptr(module, builder, what_data);
    let _ = builder.ins().call(panic_ref, &[what_ptr, idx64, len_val]);
    builder.ins().trap(ir::TrapCode::user(5).unwrap());
    builder.switch_to_block(ok);
    Ok(())
}

/// Address of element `idx` of the `Vec` / slice whose header is `handle`.
///
/// A null handle is an empty Vec. Both out-of-range shapes raise the same
/// `vec index` panic the other tiers raise, so the address this answers is
/// always inside the element buffer.
pub(super) fn emit_vec_index_address(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    handle: ir::Value,
    idx: ir::Value,
    ptr_ty: ir::Type,
) -> Result<ir::Value> {
    let panic_fn = intrinsics.extern_fn_by_name(module, "gos_rt_panic_oob")?;
    let what_data = intrinsics.intern_string(module, "vec index")?;

    let nonnull = builder.create_block();
    let null_fail = builder.create_block();
    let is_null = builder.ins().icmp_imm_s(IntCC::Equal, handle, 0);
    builder.ins().brif(is_null, null_fail, &[], nonnull, &[]);

    builder.switch_to_block(null_fail);
    builder.set_cold_block(null_fail);
    let panic_ref = module.declare_func_in_func(panic_fn, builder.func);
    let what_ptr = intrinsics.static_string_body_ptr(module, builder, what_data);
    let zero = builder.ins().iconst(types::I64, 0);
    let _ = builder.ins().call(panic_ref, &[what_ptr, idx, zero]);
    builder.ins().trap(ir::TrapCode::user(5).unwrap());

    builder.switch_to_block(nonnull);
    let len = builder
        .ins()
        .load(types::I64, MemFlagsData::trusted(), handle, 0);
    // Unsigned >=: a negative index wraps past every length.
    let oob = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, idx, len);
    let in_range = builder.create_block();
    let oob_fail = builder.create_block();
    builder.ins().brif(oob, oob_fail, &[], in_range, &[]);

    builder.switch_to_block(oob_fail);
    builder.set_cold_block(oob_fail);
    let panic_ref = module.declare_func_in_func(panic_fn, builder.func);
    let what_ptr = intrinsics.static_string_body_ptr(module, builder, what_data);
    let _ = builder.ins().call(panic_ref, &[what_ptr, idx, len]);
    builder.ins().trap(ir::TrapCode::user(5).unwrap());

    builder.switch_to_block(in_range);
    let stride32 = builder
        .ins()
        .load(types::I32, MemFlagsData::trusted(), handle, 16);
    let stride = builder.ins().uextend(types::I64, stride32);
    let off64 = builder.ins().imul(idx, stride);
    let off = if ptr_ty == types::I64 {
        off64
    } else {
        builder.ins().ireduce(ptr_ty, off64)
    };
    let data = builder
        .ins()
        .load(ptr_ty, MemFlagsData::trusted(), handle, 24);
    Ok(builder.ins().iadd(data, off))
}
