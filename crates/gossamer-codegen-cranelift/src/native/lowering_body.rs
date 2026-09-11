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

pub(super) fn collect_body_str_consts(body: &Body) -> Vec<String> {
    pub(super) fn op_str(op: &Operand) -> Option<String> {
        if let Operand::Const(ConstValue::Str(s)) = op {
            Some(s.clone())
        } else {
            None
        }
    }
    pub(super) fn rvalue_strs(rv: &Rvalue) -> Vec<String> {
        match rv {
            Rvalue::Use(op)
            | Rvalue::UnaryOp { operand: op, .. }
            | Rvalue::Cast { operand: op, .. }
            | Rvalue::Repeat { value: op, .. } => op_str(op).into_iter().collect(),
            Rvalue::BinaryOp { lhs, rhs, .. } => {
                op_str(lhs).into_iter().chain(op_str(rhs)).collect()
            }
            Rvalue::Aggregate { operands, .. } => operands.iter().filter_map(op_str).collect(),
            Rvalue::CallIntrinsic { args, .. } => args.iter().filter_map(op_str).collect(),
            Rvalue::Len(_) | Rvalue::Ref { .. } | Rvalue::StaticLoad(_) => vec![],
        }
    }
    let mut out: Vec<String> = Vec::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { rvalue, .. } = &stmt.kind {
                out.extend(rvalue_strs(rvalue));
            }
        }
        match &block.terminator {
            Terminator::Call { callee, args, .. } => {
                out.extend(op_str(callee));
                out.extend(args.iter().filter_map(op_str));
            }
            Terminator::SwitchInt { discriminant, .. } => {
                out.extend(op_str(discriminant));
            }
            Terminator::Assert { cond, .. } => {
                out.extend(op_str(cond));
            }
            Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::Unreachable
            | Terminator::Panic { .. }
            | Terminator::Drop { .. } => {}
        }
    }
    out
}

/// Every `static mut` referenced by a load or store in `body`, so the caller
/// can pre-intern the backing writable data object in the real module before
/// the parallel IR phase (the `OfflineModule` panics on `declare_data`).
pub(super) fn collect_body_static_refs(body: &Body) -> Vec<&gossamer_mir::StaticRef> {
    let mut out: Vec<&gossamer_mir::StaticRef> = Vec::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign {
                    rvalue: Rvalue::StaticLoad(sref),
                    ..
                } => out.push(sref),
                StatementKind::StaticStore { target, .. } => out.push(target),
                _ => {}
            }
        }
    }
    out
}

/// Upper bound on an aggregate that may be constructed or copied into a frame
/// stack slot rather than a heap block. This is deliberately byte-based rather
/// than slot-count based: Cranelift currently lays aggregate leaves out as
/// 8-byte words, so common scratch buffers like `[u8; 1024]` need an 8 KiB slot
/// and must not fall back to unreclaimed heap blocks inside hot loops.
const MAX_STACK_AGGR_BYTES: u32 = 64 * 1024;

fn stack_aggregate_slots(tcx: &TyCtxt, ty: Ty) -> Option<u32> {
    let slots = type_slot_count(tcx, ty).max(1);
    let bytes = slots.saturating_mul(8);
    (bytes <= MAX_STACK_AGGR_BYTES).then_some(slots)
}

/// Slot width of a by-value aggregate parameter that arrives as the ADDRESS of
/// the caller's storage, or `None` for every other parameter shape.
///
/// A parameter of this shape names a value the callee owns: the drop pass
/// schedules its ownership on the parameter's own words - a `Map` field is
/// swapped for a clone at entry through `gos_rt_map_field_clone` and freed
/// through `gos_rt_map_field_release` at every exit, both taking the field's
/// address. The caller's aggregate has to be storage the callee never reaches,
/// so the entry copies the words into a frame slot of its own and binds the
/// local there. Mirrors the LLVM backend's entry `llvm.memcpy` of a
/// by-pointer parameter into the body's own alloca (see `lower/setup.rs`).
///
/// The shape test matches [`body_returns_sret_aggregate`]: an inline two-word
/// carrier crosses as a packed `i128` value and a one-word handle aggregate as
/// its pointer, neither of which is an address of caller-owned words.
///
/// An aggregate wider than the frame budget still takes a slot when the body
/// writes its words - the caller's value is what a missing copy corrupts, and
/// the slot is the size the caller already reserved for the same aggregate.
/// A body that only reads keeps the budget's ceiling, so a large read-only
/// buffer parameter stays a pointer.
fn by_address_param_slots(tcx: &TyCtxt, body: &Body, local: Local) -> Option<u32> {
    let ty = body.local_ty(local);
    if is_inline_two_word_ty(tcx, ty)
        || !matches!(
            tcx.kind_of(ty),
            TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Adt { .. }
        )
    {
        return None;
    }
    if type_slot_count(tcx, ty) <= 1 && !single_slot_addr_aggregate(tcx, ty) {
        return None;
    }
    stack_aggregate_slots(tcx, ty)
        .or_else(|| param_storage_is_written(body, local).then(|| type_slot_count(tcx, ty).max(1)))
}

/// True when the body stores into parameter `local`'s own words: a projected
/// assignment (`p.f = v`) or a map-field helper handed the address of one of
/// its fields. Either reaches the caller's aggregate while the parameter names
/// the caller's storage.
fn param_storage_is_written(body: &Body, local: Local) -> bool {
    local_fields_written_through_address(body, local)
        || body.blocks.iter().any(|block| {
            block.stmts.iter().any(|stmt| {
                matches!(&stmt.kind, StatementKind::Assign { place, .. }
                    if place.local == local && !place.projection.is_empty())
            })
        })
}

pub(super) fn lower_body(
    module: &mut dyn Module,
    func: &mut Function,
    fb_ctx: &mut FunctionBuilderContext,
    body: &Body,
    tcx: &TyCtxt,
    function_ids_by_def: &HashMap<u32, FuncId>,
    function_ids_by_name: &HashMap<String, FuncId>,
    intrinsics: &mut IntrinsicContext,
    capture_summary: &gossamer_mir::CaptureSummary,
) -> Result<()> {
    let mut builder = FunctionBuilder::new(func, fb_ctx);

    intrinsics.nonescaping_iter_locals.clear();
    intrinsics.nonescaping_iter_state.clear();
    let mut iter_defs = HashMap::new();
    for block in &body.blocks {
        for statement in &block.stmts {
            match &statement.kind {
                StatementKind::IterSource {
                    dst,
                    source_kind: gossamer_mir::IteratorSourceKind::Range,
                    ..
                } if dst.projection.is_empty() => {
                    iter_defs.insert(dst.local, None);
                }
                StatementKind::IterAdapter {
                    dst,
                    adapter_kind: gossamer_mir::IteratorAdapterKind::Take,
                    upstream,
                    ..
                } if dst.projection.is_empty() && upstream.projection.is_empty() => {
                    iter_defs.insert(dst.local, Some(upstream.local));
                }
                _ => {}
            }
        }
    }
    for block in &body.blocks {
        for statement in &block.stmts {
            let StatementKind::IterNext { iter_place, .. } = &statement.kind else {
                continue;
            };
            if !iter_place.projection.is_empty() {
                continue;
            }
            let mut chain = Vec::new();
            let mut cursor = iter_place.local;
            loop {
                chain.push(cursor);
                match iter_defs.get(&cursor) {
                    Some(Some(upstream)) => cursor = *upstream,
                    Some(None) => {
                        intrinsics.nonescaping_iter_locals.extend(chain);
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    let mut locals: HashMap<Local, Variable> = HashMap::new();
    let mut blocks: HashMap<u32, ir::Block> = HashMap::new();

    for block in &body.blocks {
        let cl_block = builder.create_block();
        blocks.insert(block.id.as_u32(), cl_block);
    }

    // Entry block gets the parameters as its block params.
    if let Some(first_block) = body.blocks.first() {
        let entry = blocks[&first_block.id.as_u32()];
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let param_ptr_ty = module.target_config().pointer_type();
        for (index, param_local_u32) in (1..=body.arity).enumerate() {
            let local = Local(param_local_u32);
            let param_value = builder.block_params(entry)[index];
            if let Some(slots) = by_address_param_slots(tcx, body, local) {
                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    slots * 8,
                    3,
                ));
                let addr = builder.ins().stack_addr(param_ptr_ty, slot, 0);
                for word_idx in 0..slots {
                    let off = ir::immediates::Offset32::new((word_idx as i32) * 8);
                    let word =
                        builder
                            .ins()
                            .load(types::I64, MemFlagsData::trusted(), param_value, off);
                    builder
                        .ins()
                        .store(MemFlagsData::trusted(), word, addr, off);
                }
                define_var_to_with(&mut builder, &mut locals, local, addr, Some(param_ptr_ty));
                intrinsics.stack_slotted.insert(local);
                continue;
            }
            define_var_to(
                &mut builder,
                &mut locals,
                &intrinsics.body_cl_types,
                local,
                param_value,
            );
        }
        // The sret pointer (an aggregate-returning body's hidden trailing
        // param) follows the user params; the `Return` lowering writes the
        // result words through it.
        intrinsics.sret_ptr = if body_returns_sret_aggregate(body, tcx) {
            Some(builder.block_params(entry)[body.arity as usize])
        } else {
            None
        };
        // Pre-allocate backing storage for multi-slot aggregate locals that
        // are written through a field projection before any whole-value
        // assignment gives them an address. The MIR zero-inits a tuple /
        // struct scratch (`L.field = 0`, for drop safety) before its
        // `Aggregate` lands; without a stack slot that field store writes
        // through a null base and faults. A later whole-value assignment
        // rebinds the variable to its own (heap) storage, leaving this slot
        // dead. Parameters (already bound to a pointer) and the return local
        // are skipped.
        let ptr_ty = module.target_config().pointer_type();
        let mut field_store_targets: std::collections::HashSet<u32> =
            std::collections::HashSet::new();
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind {
                    if !place.projection.is_empty() {
                        field_store_targets.insert(place.local.0);
                    } else if matches!(
                        rvalue,
                        Rvalue::Use(Operand::Copy(_))
                            | Rvalue::Aggregate { .. }
                            | Rvalue::Repeat { .. }
                    ) && stack_aggregate_slots(tcx, body.local_ty(place.local)).is_some()
                    {
                        // A whole-aggregate copy destination (`let q = p`) or
                        // an aggregate constructor (`let p = Foo { .. }`,
                        // `[v; n]`) needs its own slot so `lower_statement` can
                        // memcpy / construct into it rather than aliasing the
                        // source pointer or leaking a fresh heap block each
                        // iteration. Aliasing would let a mutation of the copy
                        // write through to the source and unbalance the drop
                        // pass's per-field retain/release pairs; a per-iteration
                        // heap block is never reclaimed on the non-region path.
                        // Bound the slot size so an oversized `[v; N]` array
                        // still avoids blowing the machine stack.
                        field_store_targets.insert(place.local.0);
                    }
                }
            }
        }
        // An aggregate local handed to a call crosses as the address of its
        // words, so a call that fills it - a pop that moves an element into
        // the local that binds it - needs the local to own storage before
        // any statement has given it one.
        for block in &body.blocks {
            let Terminator::Call { args, .. } = &block.terminator else {
                continue;
            };
            for arg in args {
                if let Operand::Copy(place) = arg
                    && place.projection.is_empty()
                {
                    field_store_targets.insert(place.local.0);
                }
            }
        }
        let mut targets: Vec<u32> = field_store_targets.into_iter().collect();
        targets.sort_unstable();
        for local_u32 in targets {
            if local_u32 <= body.arity {
                continue;
            }
            let local = Local(local_u32);
            if locals.contains_key(&local) {
                continue;
            }
            let ty = body.local_ty(local);
            if !matches!(
                tcx.kind_of(ty),
                TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Adt { .. }
            ) {
                continue;
            }
            // A by-value two-word inline enum (`Result` / `Option` / inline
            // user enum) is an `i128` value held in a register, NOT a
            // field-store aggregate that needs a backing slot. Stack-slotting
            // it would bind the variable to a pointer where the body expects
            // the packed value, zeroing every read through it.
            if is_inline_two_word_ty(tcx, ty) {
                continue;
            }
            let slots = type_slot_count(tcx, ty);
            // A one-word aggregate whose sole leaf is heap-managed is
            // address-represented (see `single_slot_addr_aggregate`): the drop
            // pass zero-inits and frees its field through `local.Field(0)`
            // before any whole-value assignment, so it needs the same backing
            // slot a multi-slot aggregate gets. Plain one-slot structs
            // (`P { x: i64 }`) keep the heap-block path.
            if slots <= 1 && !single_slot_addr_aggregate(tcx, ty) {
                continue;
            }
            let Some(stack_slots) = stack_aggregate_slots(tcx, ty) else {
                continue;
            };
            let bytes = stack_slots * 8;
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                bytes,
                3,
            ));
            let addr = builder.ins().stack_addr(ptr_ty, slot, 0);
            define_var_to_with(&mut builder, &mut locals, local, addr, Some(ptr_ty));
            intrinsics.stack_slotted.insert(local);
        }
    }

    // Declare a Cranelift-side reference for every callable function.
    let mut callees_by_def: HashMap<u32, ir::FuncRef> = HashMap::new();
    let mut callees_by_name: HashMap<String, ir::FuncRef> = HashMap::new();
    for (def_local, id) in function_ids_by_def {
        let func_ref = module.declare_func_in_func(*id, builder.func);
        callees_by_def.insert(*def_local, func_ref);
    }
    for (name, id) in function_ids_by_name {
        let func_ref = module.declare_func_in_func(*id, builder.func);
        callees_by_name.insert(name.clone(), func_ref);
    }

    let cleanup_plan = gossamer_mir::plan_cleanup_with_summary(body, capture_summary);
    let entry_block_id = body.blocks.first().map(|b| b.id.as_u32());
    let mut entry_block_filled = false;
    for block in &body.blocks {
        let cl_block = blocks[&block.id.as_u32()];
        // The entry block is already current from the parameter-
        // binding section above. Cranelift's debug-assert trips if we
        // call `switch_to_block` on an unfilled current block, so skip
        // the redundant switch on that one iteration.
        if Some(block.id.as_u32()) != entry_block_id || entry_block_filled {
            builder.switch_to_block(cl_block);
        }

        // No per-call call-stack instrumentation: panic traces and
        // SIGQUIT dumps for the compiled tier come from unwinding
        // the real machine stack on demand. A push/pop pair on
        // every function entry blocks leaf-function inlining and
        // serialises on a global lock - unacceptable in hot loops.
        entry_block_filled = true;

        if !cleanup_plan.is_empty() {
            for entry in cleanup_plan.at_block_entry(block.id) {
                emit_cleanup_drop(module, &mut builder, &mut locals, intrinsics, entry)?;
            }
        }

        for statement in &block.stmts {
            lower_statement(
                module,
                &mut builder,
                &mut locals,
                body,
                tcx,
                statement,
                intrinsics,
            )?;
        }

        if !cleanup_plan.is_empty() {
            for entry in cleanup_plan.at_block_exit(block.id) {
                emit_cleanup_drop(module, &mut builder, &mut locals, intrinsics, entry)?;
            }
        }

        lower_terminator(
            module,
            &mut builder,
            &mut locals,
            body,
            tcx,
            &mut blocks,
            &callees_by_def,
            &callees_by_name,
            &block.terminator,
            intrinsics,
            &cleanup_plan,
            block.id.as_u32(),
        )?;
    }

    builder.seal_all_blocks();
    builder.finalize(module.target_config());
    Ok(())
}

pub(super) fn ensure_var(
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    module: &dyn Module,
    body_cl_types: &[Option<ir::Type>],
    local: Local,
) -> Variable {
    if let Some(var) = locals.get(&local).copied() {
        return var;
    }
    // Read-before-write fallback: prefer the inferred effective type
    // (from body scanning) and only fall back to the MIR-declared
    // type if the inference turned up nothing.
    let inferred = body_cl_types.get(local.0 as usize).copied().flatten();
    let cl = inferred.unwrap_or_else(|| cl_type_of(tcx, body.local_ty(local), module));
    let var = builder.declare_var(cl);
    locals.insert(local, var);
    var
}

pub(super) fn infer_body_cl_types(
    body: &Body,
    tcx: &TyCtxt,
    module: &dyn Module,
) -> Vec<Option<ir::Type>> {
    let n = body.locals.len();
    let mut table: HashMap<Local, ir::Type> = HashMap::with_capacity(n);
    // Seed: MIR types that directly map to a concrete cranelift type.
    for (idx, decl) in body.locals.iter().enumerate() {
        // A by-value two-word inline enum (`Result` / `Option` / inline user
        // enum) is the packed `i128` the runtime's result helpers produce and
        // consume; pin every such local to `I128` so construction, the
        // `gos_rt_result_*` calls, and the function's `I128` return signature
        // all agree (a stray `I64` would truncate the payload half).
        if is_inline_two_word_ty(tcx, decl.ty) {
            table.insert(Local(idx as u32), types::I128);
            continue;
        }
        if let Some(cl) = cl_type_of_if_concrete(tcx, decl.ty, module) {
            table.insert(Local(idx as u32), cl);
        }
    }
    let rvalue_ty = |rvalue: &Rvalue, table: &HashMap<Local, ir::Type>| -> Option<ir::Type> {
        let op_ty = |op: &Operand| -> Option<ir::Type> {
            match op {
                Operand::Const(ConstValue::Int(_)) => Some(types::I64),
                Operand::Const(ConstValue::Float(_)) => Some(types::F64),
                Operand::Const(ConstValue::Bool(_)) => Some(types::I8),
                Operand::Const(ConstValue::Char(_)) => Some(types::I32),
                Operand::Const(ConstValue::Str(_)) => Some(module.target_config().pointer_type()),
                Operand::Const(ConstValue::Unit) => None,
                Operand::Copy(place) => {
                    if place.projection.is_empty() {
                        table.get(&place.local).copied()
                    } else {
                        cl_type_of_if_concrete(tcx, resolve_place_ty(tcx, body, place), module)
                    }
                }
                Operand::FnRef { .. } => None,
            }
        };
        match rvalue {
            Rvalue::Use(op) | Rvalue::UnaryOp { operand: op, .. } => op_ty(op),
            Rvalue::BinaryOp { op, lhs, rhs } => match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    Some(types::I8)
                }
                _ => op_ty(lhs).or_else(|| op_ty(rhs)),
            },
            Rvalue::Cast { operand, target } => {
                cl_type_of_if_concrete(tcx, *target, module).or_else(|| op_ty(operand))
            }
            Rvalue::Aggregate { .. } | Rvalue::Repeat { .. } => {
                Some(module.target_config().pointer_type())
            }
            _ => None,
        }
    };
    let mut changed = true;
    while changed {
        changed = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind {
                    if !place.projection.is_empty() {
                        continue;
                    }
                    if let Some(cl) = rvalue_ty(rvalue, &table) {
                        match table.get(&place.local).copied() {
                            None => {
                                table.insert(place.local, cl);
                                changed = true;
                            }
                            // Only upgrade i64 placeholders - locals
                            // whose MIR type or earlier inference
                            // grounded them to a specific non-i64
                            // cranelift type are trusted.
                            Some(current) if current == types::I64 && cl == types::F64 => {
                                table.insert(place.local, cl);
                                changed = true;
                            }
                            _ => {}
                        }
                    }
                    // Reverse propagation: when the destination of
                    // an assignment has a concrete type and the
                    // operation's semantics guarantee the operands
                    // share that type (Use / UnaryOp / same-type
                    // BinaryOp arithmetic), propagate the type
                    // back to any still-unresolved operand. Catches
                    // parameters that were never assigned (so the
                    // forward sweep never saw them) but are used as
                    // the source of a known-typed copy or arith
                    // expression.
                    if let Some(dst_ty) = table.get(&place.local).copied() {
                        let propagate = match rvalue {
                            Rvalue::Use(_) | Rvalue::UnaryOp { .. } => true,
                            Rvalue::BinaryOp { op, .. } => !matches!(
                                op,
                                BinOp::Eq
                                    | BinOp::Ne
                                    | BinOp::Lt
                                    | BinOp::Le
                                    | BinOp::Gt
                                    | BinOp::Ge
                            ),
                            _ => false,
                        };
                        if propagate {
                            for op in operand_locals(rvalue) {
                                let existing = table.get(&op).copied();
                                let upgrade = existing.is_none()
                                    || (existing == Some(types::I64) && dst_ty == types::F64);
                                if upgrade && existing != Some(dst_ty) {
                                    table.insert(op, dst_ty);
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // Convert to Vec indexed by local.0 for O(1) lookup.
    let mut vec = vec![None; n];
    for (local, ty) in table {
        if (local.0 as usize) < n {
            vec[local.0 as usize] = Some(ty);
        }
    }
    vec
}

pub(super) fn define_var_to(
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body_cl_types: &[Option<ir::Type>],
    local: Local,
    value: ir::Value,
) {
    let preferred = body_cl_types.get(local.0 as usize).copied().flatten();
    define_var_to_with(builder, locals, local, value, preferred);
}

pub(super) fn define_var_to_with(
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    local: Local,
    value: ir::Value,
    preferred_ty: Option<ir::Type>,
) {
    let value_ty = value_type(value, builder);
    let new_decl_ty = preferred_ty.unwrap_or(value_ty);
    let (var, decl_ty) = if let Some(v) = locals.get(&local).copied() {
        // Variable was declared earlier - its type is locked
        // for the rest of the function. Read the type back
        // from the builder rather than trusting the caller's
        // hint; mismatches here are the leading cause of
        // verifier panics.
        let actual = builder.try_use_var(v).map(|val| value_type(val, builder));
        (v, actual.unwrap_or(new_decl_ty))
    } else {
        let v = builder.declare_var(new_decl_ty);
        locals.insert(local, v);
        (v, new_decl_ty)
    };
    // Coerce the value to the declared variable width when they
    // disagree (e.g. we declared the local as F64 from inference,
    // but this particular value was loaded as I64 because the MIR
    // path still considered the source as an inference variable).
    let coerced = if decl_ty == value_ty {
        value
    } else if decl_ty == types::F64 && value_ty == types::I64 {
        builder
            .ins()
            .bitcast(types::F64, ir::MemFlagsData::new(), value)
    } else if decl_ty == types::I64 && value_ty == types::F64 {
        builder
            .ins()
            .bitcast(types::I64, ir::MemFlagsData::new(), value)
    } else if decl_ty == types::F32 && value_ty == types::I64 {
        let truncated = builder.ins().ireduce(types::I32, value);
        builder
            .ins()
            .bitcast(types::F32, ir::MemFlagsData::new(), truncated)
    } else if decl_ty == types::F32 && value_ty == types::F64 {
        builder.ins().fdemote(types::F32, value)
    } else if decl_ty == types::F64 && value_ty == types::F32 {
        builder.ins().fpromote(types::F64, value)
    } else if decl_ty.is_int() && value_ty.is_int() {
        if decl_ty.bits() > value_ty.bits() {
            builder.ins().sextend(decl_ty, value)
        } else {
            builder.ins().ireduce(decl_ty, value)
        }
    } else if decl_ty.is_int() && value_ty.is_float() {
        // Float→int through a bitcast at the same width then
        // resize as needed. Used when the MIR has assigned a
        // float-shaped value to an int-shaped local (rare -
        // typically a fallback path miscalculated the kind).
        let int_form = if value_ty == types::F64 {
            builder
                .ins()
                .bitcast(types::I64, ir::MemFlagsData::new(), value)
        } else {
            builder
                .ins()
                .bitcast(types::I32, ir::MemFlagsData::new(), value)
        };
        let int_ty = value_type(int_form, builder);
        if decl_ty.bits() > int_ty.bits() {
            builder.ins().sextend(decl_ty, int_form)
        } else if decl_ty.bits() < int_ty.bits() {
            builder.ins().ireduce(decl_ty, int_form)
        } else {
            int_form
        }
    } else if decl_ty.is_float() && value_ty.is_int() {
        // Int→float: resize to match width, then bitcast.
        let resized = if value_ty.bits() > decl_ty.bits() {
            builder.ins().ireduce(
                if decl_ty == types::F64 {
                    types::I64
                } else {
                    types::I32
                },
                value,
            )
        } else if value_ty.bits() < decl_ty.bits() {
            builder.ins().sextend(
                if decl_ty == types::F64 {
                    types::I64
                } else {
                    types::I32
                },
                value,
            )
        } else {
            value
        };
        builder
            .ins()
            .bitcast(decl_ty, ir::MemFlagsData::new(), resized)
    } else {
        // Last-ditch: bitcast through equal-width types when we
        // can; otherwise drop the value and substitute a typed
        // zero so the def_var doesn't trap the verifier.
        if decl_ty.bits() == value_ty.bits() {
            bitcast_same_width(builder, decl_ty, value)
        } else if decl_ty.is_int() {
            builder.ins().iconst(decl_ty, 0)
        } else if decl_ty == types::F64 {
            builder.ins().f64const(0.0)
        } else if decl_ty == types::F32 {
            builder.ins().f32const(0.0)
        } else {
            value
        }
    };
    builder.def_var(var, coerced);
}
