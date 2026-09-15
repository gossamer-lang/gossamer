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

pub(super) fn store_call_result(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    destination: &Place,
    value: ir::Value,
    intrinsics: &mut IntrinsicContext,
) -> Result<()> {
    if destination.projection.is_empty() {
        let ret_ty = value_type(value, builder);
        intrinsics
            .local_declared_ty
            .insert(destination.local, ret_ty);
        define_var_to(
            builder,
            locals,
            &intrinsics.body_cl_types,
            destination.local,
            value,
        );
        return Ok(());
    }
    let elem_hint = intrinsics.elem_cl_ty.get(&destination.local).copied();
    let leaf_ty = resolve_place_cl_type(
        tcx,
        body,
        destination,
        module,
        elem_hint.or(Some(value_type(value, builder))),
    );
    lower_place_store(
        module,
        builder,
        locals,
        body,
        tcx,
        destination,
        value,
        leaf_ty,
        intrinsics,
    )
}

/// Stores a runtime call's by-value aggregate result that arrived as a block
/// the callee allocated: its words move into a slot of this frame and the
/// block is freed. Any other result is stored as it is.
#[allow(
    clippy::too_many_arguments,
    reason = "the same place-store context `store_call_result` takes"
)]
pub(super) fn store_runtime_call_result(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    destination: &Place,
    value: ir::Value,
    intrinsics: &mut IntrinsicContext,
    callee: &str,
) -> Result<()> {
    let slots = type_slot_count(tcx, body.local_ty(destination.local));
    if !gossamer_abi::returns_fresh_aggregate(callee)
        || !destination.projection.is_empty()
        || slots <= 1
    {
        return store_call_result(
            module,
            builder,
            locals,
            body,
            tcx,
            destination,
            value,
            intrinsics,
        );
    }
    let ptr_ty = module.target_config().pointer_type();
    let bytes = slots.saturating_mul(8);
    let slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, bytes, 3));
    let slot_addr = builder.ins().stack_addr(ptr_ty, slot, 0);
    let block = coerce_arg_to(builder, value, ptr_ty).unwrap_or(value);
    for index in 0..slots {
        let offset = ir::immediates::Offset32::new(i32::try_from(index * 8).unwrap_or(0));
        let word = builder
            .ins()
            .load(types::I64, MemFlagsData::trusted(), block, offset);
        builder
            .ins()
            .store(MemFlagsData::trusted(), word, slot_addr, offset);
    }
    let free_fn = intrinsics.extern_fn(module, "gos_rt_aggr_free", &[ptr_ty, types::I64], &[])?;
    let free_ref = module.declare_func_in_func(free_fn, builder.func);
    let size = builder.ins().iconst(types::I64, i64::from(bytes));
    builder.ins().call(free_ref, &[block, size]);
    store_call_result(
        module,
        builder,
        locals,
        body,
        tcx,
        destination,
        slot_addr,
        intrinsics,
    )
}

pub(super) fn emit_cleanup_drop(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    intrinsics: &mut IntrinsicContext,
    entry: &gossamer_mir::CleanupEntry,
) -> Result<()> {
    let ptr_ty = module.target_config().pointer_type();
    let Some(&var) = locals.get(&entry.local) else {
        return Ok(());
    };
    let raw = builder.use_var(var);
    let ptr = coerce_arg_to(builder, raw, ptr_ty).unwrap_or(raw);
    let free_fn = intrinsics.extern_fn(module, entry.free_fn, &[ptr_ty], &[])?;
    let free_ref = module.declare_func_in_func(free_fn, builder.func);
    builder.ins().call(free_ref, &[ptr]);
    Ok(())
}

pub(super) fn clone_aggregate_value(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    src: ir::Value,
    slots: u32,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    let bytes = u64::from(slots) * 8;
    let alloc_fn = intrinsics.extern_fn_by_name(module, "gos_rt_gc_alloc")?;
    let alloc_ref = module.declare_func_in_func(alloc_fn, builder.func);
    let bytes_v = builder.ins().iconst(types::I64, bytes as i64);
    let call = builder.ins().call(alloc_ref, &[bytes_v]);
    let dst = builder.inst_results(call)[0];
    let src_ptr = match value_type(src, builder) {
        t if t == ptr_ty => src,
        t if t == types::I64 && ptr_ty == types::I32 => builder.ins().ireduce(ptr_ty, src),
        t if t == types::I32 && ptr_ty == types::I64 => builder.ins().uextend(ptr_ty, src),
        _ => src,
    };
    for slot_idx in 0..slots {
        let off = (slot_idx as i32) * 8;
        let word = builder.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            src_ptr,
            ir::immediates::Offset32::new(off),
        );
        builder.ins().store(
            MemFlagsData::trusted(),
            word,
            dst,
            ir::immediates::Offset32::new(off),
        );
    }
    Ok(dst)
}
