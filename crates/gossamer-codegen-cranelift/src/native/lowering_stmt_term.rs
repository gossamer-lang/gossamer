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
    AssertMessage, BinOp, Body, ConstValue, IteratorAdapterKind, IteratorSourceKind, Local,
    Operand, Place, Projection, Rvalue, StatementKind, Terminator, UnOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};
use rayon::prelude::*;

use super::*;

fn emit_lazy_iter_runtime_call(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    name: &'static str,
    args: &[ir::Value],
) -> Result<Option<ir::Value>> {
    emit_rt_call_by_name(module, builder, intrinsics, name, args)
}

fn store_typed_iter_value(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    place: &Place,
    value: ir::Value,
    intrinsics: &mut IntrinsicContext,
) -> Result<()> {
    if place.projection.is_empty() {
        define_var_to(
            builder,
            locals,
            &intrinsics.body_cl_types,
            place.local,
            value,
        );
        return Ok(());
    }
    let leaf_ty = cl_type_of(tcx, resolve_place_ty(tcx, body, place), module);
    lower_place_store(
        module, builder, locals, body, tcx, place, value, leaf_ty, intrinsics,
    )
}

fn emit_nonescaping_iter_next(
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    local: Local,
    allowed: ir::Value,
) -> Option<(ir::Value, ir::Value)> {
    match intrinsics.nonescaping_iter_state.get(&local).copied()? {
        NonescapingIteratorState::Range { current, end } => {
            let current_value = builder.use_var(current);
            let end_value = builder.use_var(end);
            let before_end = builder
                .ins()
                .icmp(IntCC::SignedLessThan, current_value, end_value);
            let has_value = builder.ins().band(allowed, before_end);
            let incremented = builder.ins().iadd_imm_s(current_value, 1);
            let next_value = builder.ins().select(has_value, incremented, current_value);
            builder.def_var(current, next_value);
            Some((has_value, current_value))
        }
        NonescapingIteratorState::Take {
            upstream,
            remaining,
        } => {
            let remaining_value = builder.use_var(remaining);
            let zero = builder.ins().iconst(types::I64, 0);
            let has_budget = builder
                .ins()
                .icmp(IntCC::SignedGreaterThan, remaining_value, zero);
            let upstream_allowed = builder.ins().band(allowed, has_budget);
            let (has_value, value) =
                emit_nonescaping_iter_next(builder, intrinsics, upstream, upstream_allowed)?;
            let decremented = builder.ins().iadd_imm_s(remaining_value, -1);
            let next_remaining = builder
                .ins()
                .select(has_value, decremented, remaining_value);
            builder.def_var(remaining, next_remaining);
            Some((has_value, value))
        }
    }
}

fn lower_typed_iterator_statement(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    statement: &gossamer_mir::Statement,
    intrinsics: &mut IntrinsicContext,
) -> Result<bool> {
    let ptr_ty = module.target_config().pointer_type();
    match &statement.kind {
        StatementKind::IterSource {
            dst,
            source_kind,
            source,
            ..
        } => {
            if intrinsics.nonescaping_iter_locals.contains(&dst.local)
                && matches!(source_kind, IteratorSourceKind::Range)
            {
                let current = builder.declare_var(types::I64);
                let end_var = builder.declare_var(types::I64);
                let zero = builder.ins().iconst(types::I64, 0);
                let end = lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    source,
                    Some(types::I64),
                    intrinsics,
                )?;
                builder.def_var(current, zero);
                builder.def_var(end_var, end);
                intrinsics.nonescaping_iter_state.insert(
                    dst.local,
                    NonescapingIteratorState::Range {
                        current,
                        end: end_var,
                    },
                );
                return Ok(true);
            }
            let (name, args) = match source_kind {
                IteratorSourceKind::Range => {
                    let start = builder.ins().iconst(types::I64, 0);
                    let end = lower_operand(
                        module,
                        builder,
                        locals,
                        body,
                        tcx,
                        source,
                        Some(types::I64),
                        intrinsics,
                    )?;
                    ("gos_rt_lazy_iter_range_i64", vec![start, end])
                }
                IteratorSourceKind::Slice | IteratorSourceKind::VecInto => {
                    let source = lower_operand(
                        module,
                        builder,
                        locals,
                        body,
                        tcx,
                        source,
                        Some(ptr_ty),
                        intrinsics,
                    )?;
                    let source = coerce_arg_to(builder, source, ptr_ty)?;
                    ("gos_rt_lazy_iter_from_vec_i64", vec![source])
                }
            };
            let value = emit_lazy_iter_runtime_call(module, builder, intrinsics, name, &args)?
                .unwrap_or_else(|| builder.ins().iconst(ptr_ty, 0));
            store_typed_iter_value(module, builder, locals, body, tcx, dst, value, intrinsics)?;
            Ok(true)
        }
        StatementKind::IterAdapter {
            dst,
            adapter_kind,
            upstream,
            closure_or_arg,
            ..
        } => {
            if intrinsics.nonescaping_iter_locals.contains(&dst.local)
                && matches!(adapter_kind, IteratorAdapterKind::Take)
            {
                let remaining = builder.declare_var(types::I64);
                let count = match closure_or_arg {
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
                let zero = builder.ins().iconst(types::I64, 0);
                let positive = builder.ins().icmp(IntCC::SignedGreaterThan, count, zero);
                let count = builder.ins().select(positive, count, zero);
                builder.def_var(remaining, count);
                intrinsics.nonescaping_iter_state.insert(
                    dst.local,
                    NonescapingIteratorState::Take {
                        upstream: upstream.local,
                        remaining,
                    },
                );
                return Ok(true);
            }
            let upstream = lower_place_read(
                module,
                builder,
                locals,
                body,
                tcx,
                upstream,
                Some(ptr_ty),
                intrinsics,
            )?;
            let upstream = coerce_arg_to(builder, upstream, ptr_ty)?;
            let (name, args) = match adapter_kind {
                IteratorAdapterKind::Take | IteratorAdapterKind::Skip => {
                    let n = match closure_or_arg {
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
                    let name = if matches!(adapter_kind, IteratorAdapterKind::Take) {
                        "gos_rt_lazy_iter_take_i64"
                    } else {
                        "gos_rt_lazy_iter_skip_i64"
                    };
                    (name, vec![n, upstream])
                }
                IteratorAdapterKind::Map | IteratorAdapterKind::Filter => {
                    let env = match closure_or_arg {
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
                    let env = coerce_arg_to(builder, env, ptr_ty)?;
                    let name = if matches!(adapter_kind, IteratorAdapterKind::Map) {
                        "gos_rt_lazy_iter_map_i64"
                    } else {
                        "gos_rt_lazy_iter_filter_i64"
                    };
                    (name, vec![env, upstream])
                }
                IteratorAdapterKind::Enumerate => {
                    ("gos_rt_lazy_iter_enumerate_i64", vec![upstream])
                }
                IteratorAdapterKind::Chain | IteratorAdapterKind::Zip => {
                    let rhs = match closure_or_arg {
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
                    let rhs = coerce_arg_to(builder, rhs, ptr_ty)?;
                    let name = if matches!(adapter_kind, IteratorAdapterKind::Chain) {
                        "gos_rt_lazy_iter_chain_i64"
                    } else {
                        "gos_rt_lazy_iter_zip_i64"
                    };
                    (name, vec![upstream, rhs])
                }
            };
            let value = emit_lazy_iter_runtime_call(module, builder, intrinsics, name, &args)?
                .unwrap_or_else(|| builder.ins().iconst(ptr_ty, 0));
            store_typed_iter_value(module, builder, locals, body, tcx, dst, value, intrinsics)?;
            Ok(true)
        }
        StatementKind::IterNext {
            dst_option,
            iter_place,
            ..
        } => {
            if intrinsics
                .nonescaping_iter_locals
                .contains(&iter_place.local)
            {
                let allowed = builder.ins().iconst(types::I8, 1);
                let (has_value, value) =
                    emit_nonescaping_iter_next(builder, intrinsics, iter_place.local, allowed)
                        .ok_or_else(|| anyhow!("missing nonescaping iterator state"))?;
                let zero = builder.ins().iconst(types::I64, 0);
                let one = builder.ins().iconst(types::I64, 1);
                let disc = builder.ins().select(has_value, zero, one);
                let payload = builder.ins().select(has_value, value, zero);
                let packed = emit_lazy_iter_runtime_call(
                    module,
                    builder,
                    intrinsics,
                    "gos_rt_result_new",
                    &[disc, payload],
                )?
                .unwrap_or_else(|| builder.ins().iconst(types::I128, 0));
                store_typed_iter_value(
                    module, builder, locals, body, tcx, dst_option, packed, intrinsics,
                )?;
                return Ok(true);
            }
            let iter = lower_place_read(
                module,
                builder,
                locals,
                body,
                tcx,
                iter_place,
                Some(ptr_ty),
                intrinsics,
            )?;
            let iter = coerce_arg_to(builder, iter, ptr_ty)?;
            let value = emit_lazy_iter_runtime_call(
                module,
                builder,
                intrinsics,
                "gos_rt_lazy_iter_next_i64",
                &[iter],
            )?
            .unwrap_or_else(|| builder.ins().iconst(types::I128, 0));
            store_typed_iter_value(
                module, builder, locals, body, tcx, dst_option, value, intrinsics,
            )?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// True when the return local (`Local::RETURN`) provably holds a freshly
/// constructed standalone heap aggregate this frame exclusively owns, so its
/// block can be freed once its words are copied into the sret slot instead of
/// leaking on every call.
///
/// A local is *fresh* when every bare assignment to it is an `Aggregate` /
/// `Repeat` rvalue (its own `gos_rt_aggr_alloc` block) or a move
/// (`Use(Copy(bare))`) of another fresh local, and it is never written through a
/// projection, never `Ref`d / `Len`d / `Drop`ped, and never a call destination -
/// any of which could make it an interior pointer, an aliased value, or a
/// passthrough of an argument the caller still owns. The return local is
/// free-safe when it is fresh and its move-closure reaches at least one real
/// `Aggregate` (so it is genuinely a block, not a passthrough of a param). A
/// tuple stored elsewhere is copied by value into the destination's inline
/// slots, never pointer-aliased, so freeing the return block cannot dangle
/// another owner.
fn return_local_is_fresh_aggregate(body: &Body) -> bool {
    use std::collections::{HashMap, HashSet};

    // Per bare local: whether every assignment so far is a fresh rvalue
    // (`Aggregate`/`Repeat`, or a move of a bare local recorded in `move_srcs`).
    let mut fresh_ok: HashMap<u32, bool> = HashMap::new();
    // Whether a local has at least one direct `Aggregate`/`Repeat` assignment.
    let mut has_agg: HashSet<u32> = HashSet::new();
    // Bare-local move sources: `L <- Use(Copy(src))` with both bare.
    let mut move_srcs: HashMap<u32, Vec<u32>> = HashMap::new();
    // Locals that can never be a clean freeable block: projected write, `Ref`,
    // `Len`, `Drop`, or a call destination.
    let mut tainted: HashSet<u32> = HashSet::new();

    let mut mark_assign = |l: u32, fresh: bool| {
        let e = fresh_ok.entry(l).or_insert(true);
        *e = *e && fresh;
    };

    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if !place.projection.is_empty() {
                // A projected write with a *constant* value is the drop-safety
                // zero-init the MIR emits into a tuple/struct scratch before its
                // real aggregate lands (`_L.0 = 0`). It cannot store an interior
                // or aliased pointer, and the scratch is abandoned once `_L` is
                // rebound to the fresh block, so it does not taint. Any other
                // projected write may build an interior field, so it does.
                if !matches!(rvalue, Rvalue::Use(Operand::Const(_))) {
                    tainted.insert(place.local.0);
                }
                continue;
            }
            let l = place.local.0;
            match rvalue {
                Rvalue::Aggregate { .. } | Rvalue::Repeat { .. } => {
                    has_agg.insert(l);
                    mark_assign(l, true);
                }
                Rvalue::Use(Operand::Copy(p)) if p.projection.is_empty() => {
                    move_srcs.entry(l).or_default().push(p.local.0);
                    mark_assign(l, true);
                }
                Rvalue::Ref { place, .. } | Rvalue::Len(place) => {
                    tainted.insert(place.local.0);
                    mark_assign(l, false);
                }
                // Any other source (a projected read, a constant, an arithmetic
                // result, an intrinsic) is not a standalone fresh block.
                _ => mark_assign(l, false),
            }
        }
        match &block.terminator {
            Terminator::Call { destination, .. } => {
                tainted.insert(destination.local.0);
            }
            Terminator::Drop { place, .. } => {
                tainted.insert(place.local.0);
            }
            _ => {}
        }
    }

    // Walk the return local's move-closure: every local reached must be
    // untainted and fresh, and the closure must reach a real `Aggregate`.
    let mut visited: HashSet<u32> = HashSet::new();
    let mut stack = vec![Local::RETURN.0];
    let mut saw_agg = false;
    while let Some(l) = stack.pop() {
        if !visited.insert(l) {
            continue;
        }
        if tainted.contains(&l) || !fresh_ok.get(&l).copied().unwrap_or(false) {
            return false;
        }
        if has_agg.contains(&l) {
            saw_agg = true;
        }
        // A reached local with neither an `Aggregate` nor a move source is a
        // param or externally-defined value - not a block this frame owns.
        if !has_agg.contains(&l) && !move_srcs.contains_key(&l) {
            return false;
        }
        for &src in move_srcs.get(&l).into_iter().flatten() {
            stack.push(src);
        }
    }
    saw_agg
}

/// When `expected` (the callee's Cranelift signature param types) carries one
/// more param than the call supplies and that trailing param is a pointer, the
/// callee uses the structural-return (sret) ABI for a by-value aggregate
/// return. Allocate a stack slot sized to the callee's return aggregate
/// (`ret_slots` 8-byte words) and append its address as the hidden result-slot
/// arg; the callee fills it and returns the same pointer. The slot lives in
/// this frame and is copied into the owning destination before cleanup.
///
/// `ret_slots` is resolved from the callee's own return type ([`callee_sret_slots`]).
/// A fixed guess would overflow whenever the aggregate exceeds two words (a
/// struct with three fields, a `[i64; 8]`), so when the sret ABI fires but the
/// size is unresolved this refuses to emit rather than corrupt the stack -
/// aborting JIT compilation of the body (the bytecode VM then runs it) instead
/// of silently miscompiling.
fn maybe_push_sret_slot(
    builder: &mut FunctionBuilder<'_>,
    ptr_ty: ir::Type,
    expected: &[ir::Type],
    n_args: usize,
    ret_slots: Option<u32>,
    arg_values: &mut Vec<ir::Value>,
) -> Result<()> {
    let fire = expected.len() == n_args + 1 && expected.last() == Some(&ptr_ty);
    if !fire {
        return Ok(());
    }
    let slots = ret_slots.ok_or_else(|| {
        anyhow!(
            "native codegen: sret callee return size unresolved - refusing to emit a \
             fixed-size result slot that could overflow the caller's stack"
        )
    })?;
    let bytes = slots.max(1).saturating_mul(8);
    let slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, bytes, 3));
    arg_values.push(builder.ins().stack_addr(ptr_ty, slot, 0));
    Ok(())
}

/// Whether the callee takes parameter `idx` by reference. Such a parameter
/// names the caller's own storage, so an aggregate argument reaches it as an
/// address rather than through the by-value copy every other aggregate
/// argument gets.
fn callee_param_is_by_ref(callee: &Operand, intrinsics: &IntrinsicContext, idx: usize) -> bool {
    let params = match callee {
        Operand::Const(ConstValue::Str(name)) => intrinsics.ref_params_by_name.get(name),
        Operand::FnRef { def, substs } => {
            let mangled = (!substs.is_empty()).then(|| gossamer_mir::mangled_name(*def, substs));
            mangled
                .and_then(|name| intrinsics.ref_params_by_name.get(&name))
                .or_else(|| intrinsics.ref_params_by_def.get(&def.local))
        }
        _ => None,
    };
    params.is_some_and(|by_ref| by_ref.get(idx).copied().unwrap_or(false))
}

/// Resolves the callee's sret return-slot count for a `Call` terminator,
/// mirroring [`resolve_callee`]'s key lookup so the size matches the exact body
/// the call resolves to. `name_hint` is the literal name for a `Const(Str)`
/// callee (the registry-name path); an `FnRef` callee resolves by its mangled
/// monomorphised name, then its def-local id, then the `fn#{def}` fallback.
/// Returns `None` when the callee is not a body that returns an aggregate via
/// the sret ABI.
fn callee_sret_slots(
    callee: &Operand,
    name_hint: Option<&str>,
    intrinsics: &IntrinsicContext,
) -> Option<u32> {
    if let Some(name) = name_hint
        && let Some(&n) = intrinsics.sret_slots_by_name.get(name)
    {
        return Some(n);
    }
    match callee {
        Operand::FnRef { def, substs } => {
            if !substs.is_empty() {
                let mangled = gossamer_mir::mangled_name(*def, substs);
                if let Some(&n) = intrinsics.sret_slots_by_name.get(&mangled) {
                    return Some(n);
                }
            }
            if let Some(&n) = intrinsics.sret_slots_by_def.get(&def.local) {
                return Some(n);
            }
            intrinsics
                .sret_slots_by_name
                .get(&format!("fn#{}", def.local))
                .copied()
        }
        _ => None,
    }
}

/// True when `rvalue` is a whole-value `Copy` of a place whose leaf type is a
/// multi-slot aggregate (tuple / array / struct) held in flat slot storage -
/// the shape that must be memcpy'd word-for-word into the destination's own
/// slot rather than aliased by rebinding the variable. Inline two-word enums
/// (`Result` / `Option` / inline user enums) are register-packed `i128`
/// values, not slot storage, so they are excluded.
fn is_aggregate_copy_src(body: &Body, tcx: &TyCtxt, rvalue: &Rvalue) -> bool {
    let Rvalue::Use(Operand::Copy(src)) = rvalue else {
        return false;
    };
    // Peel `&T` wrappers: the inliner rewrites a `*param` deref-copy as a
    // bare `Copy(ref_local)` into a value-typed destination, and the ref
    // local's VALUE is the source address the memcpy needs. Without the
    // peel the copy takes the rebind path, aliasing the destination to the
    // referent - a later projected write then mutates the source. A peeled
    // source must lead to a MULTI-SLOT aggregate: a one-word
    // address-represented value (an RC enum handle) copies its handle word
    // by rebinding, and "memcpying" it would dereference the handle.
    let mut leaf = resolve_place_ty(tcx, body, src);
    let mut peeled = false;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(leaf) {
        leaf = *inner;
        peeled = true;
    }
    let slots_ok = if peeled {
        type_slot_count(tcx, leaf) > 1
    } else {
        type_slot_count(tcx, leaf) > 1 || single_slot_addr_aggregate(tcx, leaf)
    };
    !is_inline_two_word_ty(tcx, leaf)
        && slots_ok
        && matches!(
            tcx.kind_of(leaf),
            TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Adt { .. }
        )
}

/// True when `local`'s value can reach the return slot (`Local::RETURN`)
/// through a chain of whole-value `Copy` assignments. Such a local's backing
/// pointer is handed to the caller by the return lowering, so it must stay a
/// heap allocation that outlives the frame rather than a reused stack slot.
/// Every other downstream use of an aggregate copies its words (a
/// whole-aggregate copy memcpies, a call argument is defensively cloned, a
/// nested-aggregate operand is memcpied into its parent), so excluding only the
/// return-flow set keeps stack construction sound.
/// Whether a field of `local` is written through its ADDRESS rather than by an
/// assignment to a projected place.
///
/// The map-field helpers take the address of the field they own and swap the
/// table stored there, so a copy destination one of them names has storage of
/// its own to keep: aliasing it to the source would let the source's own field
/// teardown reach into the value the caller was handed.
pub(super) fn local_fields_written_through_address(body: &Body, local: Local) -> bool {
    body.blocks.iter().any(|block| {
        block.stmts.iter().any(|stmt| {
            let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } = &stmt.kind
            else {
                return false;
            };
            if !matches!(*name, "gos_rt_map_field_clone" | "gos_rt_map_field_release") {
                return false;
            }
            matches!(
                args.first(),
                Some(Operand::Copy(p)) if p.local == local && !p.projection.is_empty()
            )
        })
    })
}

fn local_flows_to_return(body: &Body, local: Local) -> bool {
    let n = body.locals.len();
    let mut in_return = vec![false; n];
    if (Local::RETURN.0 as usize) < n {
        in_return[Local::RETURN.0 as usize] = true;
    }
    // Least-fixpoint over `dst = Copy(src)` edges: seed the return slot, then
    // pull every source that flows into an already-marked destination.
    let mut changed = true;
    while changed {
        changed = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                } = &stmt.kind
                    && place.projection.is_empty()
                    && src.projection.is_empty()
                {
                    let d = place.local.0 as usize;
                    let s = src.local.0 as usize;
                    if d < n && s < n && in_return[d] && !in_return[s] {
                        in_return[s] = true;
                        changed = true;
                    }
                }
            }
        }
    }
    (local.0 as usize) < n && in_return[local.0 as usize]
}

pub(super) fn lower_statement(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    statement: &gossamer_mir::Statement,
    intrinsics: &mut IntrinsicContext,
) -> Result<()> {
    if lower_typed_iterator_statement(module, builder, locals, body, tcx, statement, intrinsics)? {
        return Ok(());
    }
    match &statement.kind {
        StatementKind::Assign { place, rvalue } => {
            // Route rvalue-position intrinsic calls (gos_alloc,
            // gos_store, gos_load, …) through the same handler the
            // terminator path uses. Keeps the heap primitives usable
            // as inline expressions inside a single basic block.
            if let Rvalue::CallIntrinsic { name, args } = rvalue {
                if lower_intrinsic_call(
                    module, builder, locals, body, tcx, args, name, place, intrinsics,
                )? {
                    return Ok(());
                }
                // Unrecognised intrinsic name in rvalue position.
                // The MIR lowerer has been audited to never emit
                // names without a matching cranelift dispatch, so
                // hitting this path is a real gap in the runtime
                // table - surface it loudly rather than silently
                // miscompiling.
                let fn_name = &body.name;
                bail!(
                    "native codegen: unrecognised intrinsic `{name}` in fn `{fn_name}`\n  \
                     add a dispatch arm in `lower_intrinsic_call` (and a runtime \
                     symbol in `gossamer-runtime/src/c_abi.rs`) for `{name}`"
                );
            }
            // Destination hint: when the place has no projections, it's
            // the root local's type. When it does, we still use the
            // root's classification as the hint, but the projected
            // store below picks the correct width from the leaf type.
            let dst_hint = cl_type_of(tcx, body.local_ty(place.local), module);
            // When the rvalue is an aggregate, remember the first
            // operand's cranelift type as the uniform element type.
            // Projected reads/writes later look this up as a hint
            // when the MIR element type is an unresolved inference
            // variable.
            let aggregate_elem_ty: Option<ir::Type> = match rvalue {
                Rvalue::Aggregate { operands, .. } => operands
                    .first()
                    .and_then(|op| operand_cl_type(body, tcx, op, module)),
                Rvalue::Repeat { value, .. } => operand_cl_type(body, tcx, value, module),
                _ => None,
            };
            // Same for slot counts: remember per-element and total
            // slot widths so downstream projected addresses stride
            // correctly through aggregates of aggregates.
            let (aggregate_elem_slots, aggregate_total_slots): (Option<u32>, Option<u32>) =
                match rvalue {
                    Rvalue::Aggregate { kind, operands } => {
                        let elem = match kind {
                            gossamer_mir::AggregateKind::Array => operands
                                .first()
                                .and_then(|op| {
                                    operand_elem_slots(&intrinsics.local_slots, tcx, body, op)
                                })
                                .unwrap_or(1),
                            _ => 1,
                        };
                        let total = match kind {
                            gossamer_mir::AggregateKind::Array => (operands.len() as u32) * elem,
                            // A tuple / struct's slot span is the SUM of its
                            // operands' slot widths, not the operand count: a
                            // field that is itself a multi-slot aggregate
                            // (`Wrapper { value: Point }`, `Boxed { p: Point }`)
                            // occupies its full width. Recording the operand
                            // count here understated the metadata to one slot,
                            // collapsing the stride of any array whose element
                            // is such a nested aggregate. This matches the
                            // `total_slots` the rvalue lowering allocates.
                            _ => operands
                                .iter()
                                .map(|op| {
                                    operand_elem_slots(&intrinsics.local_slots, tcx, body, op)
                                        .unwrap_or(1)
                                })
                                .sum::<u32>()
                                .max(1),
                        };
                        (Some(elem), Some(total))
                    }
                    Rvalue::Repeat { value, count } => {
                        let elem = operand_elem_slots(&intrinsics.local_slots, tcx, body, value)
                            .unwrap_or(1);
                        let total = u32::try_from(*count).unwrap_or(1).saturating_mul(elem);
                        (Some(elem), Some(total))
                    }
                    _ => (None, None),
                };
            // For `Use(Copy(src))`/`Use(Move(src))` where the source
            // is a plain local, inherit the source's aggregate
            // metadata. Let-bindings desugar to this pattern
            // (`let ps = <array-literal-temp>`), and without this
            // propagation the binding loses the element stride that
            // the temp had picked up from the aggregate rvalue.
            let copy_src_meta: Option<Local> = match rvalue {
                Rvalue::Use(Operand::Copy(p)) if p.projection.is_empty() => Some(p.local),
                _ => None,
            };
            // For `Use(Copy(src.field…))` where the leaf type is a
            // multi-slot aggregate (struct/tuple embedded inline),
            // the rvalue lowering returns the field's address. Mark
            // the destination local as holding an aggregate of the
            // leaf's slot width so subsequent projections off of it
            // stride correctly instead of treating the address as
            // a scalar pointer to be dereferenced once.
            let projected_aggregate_slots: Option<u32> = match rvalue {
                Rvalue::Use(Operand::Copy(p)) if !p.projection.is_empty() => {
                    let leaf = resolve_place_ty(tcx, body, p);
                    let count = type_slot_count(tcx, leaf);
                    if count > 1 { Some(count) } else { None }
                }
                _ => None,
            };
            // Construct a non-escaping aggregate directly into its own stack
            // slot instead of a fresh `gos_rt_aggr_alloc` heap block. The
            // block is otherwise never reclaimed on the non-region path, so a
            // hot loop leaks one aggregate per iteration (the LLVM tier keeps
            // these on the stack; see `lower/mod.rs`). Only slot-backed locals
            // that do not flow to the return qualify - the return lowering
            // hands the pointer to the caller, which a stack slot cannot
            // outlive.
            let agg_into_slot: Option<ir::Value> = if place.projection.is_empty()
                && matches!(rvalue, Rvalue::Aggregate { .. } | Rvalue::Repeat { .. })
                && intrinsics.stack_slotted.contains(&place.local)
                && !local_flows_to_return(body, place.local)
            {
                let dst_var = ensure_var(
                    builder,
                    locals,
                    body,
                    tcx,
                    module,
                    &intrinsics.body_cl_types,
                    place.local,
                );
                let raw = builder.use_var(dst_var);
                let ptr_ty = module.target_config().pointer_type();
                Some(coerce_arg_to(builder, raw, ptr_ty).unwrap_or(raw))
            } else {
                None
            };
            let value = lower_rvalue_into(
                module,
                builder,
                locals,
                body,
                tcx,
                rvalue,
                Some(dst_hint),
                Some(resolve_place_ty(tcx, body, place)),
                agg_into_slot,
                intrinsics,
            )?;
            if place.projection.is_empty() {
                // Whole-aggregate copy into a slot-backed local: memcpy the
                // source words into the destination's own stack slot instead
                // of rebinding its variable to the source pointer. Rebinding
                // aliases the two locals, so a later mutation of the copy
                // writes through to the source (a silent wrong answer) and the
                // drop pass's per-field retain/release pairs operate on shared
                // storage, leaking one reference per iteration in a hot loop.
                // Mirrors the LLVM backend's `llvm.memcpy` of the aggregate
                // alloca (see `lower/stmt.rs`). Region-allocated payloads copy
                // their (possibly headerless) pointer words safely: the drop
                // pass's retain/release calls no-op on region objects, so no
                // per-node free is introduced and the arena still bulk-frees
                // at pop.
                // A copy whose destination flows to the return slot keeps the
                // source's storage: the return lowering hands the caller that
                // pointer (and, for a fresh aggregate, frees it with
                // `gos_rt_aggr_free`), which a reused stack slot can neither
                // outlive nor be freed as. Such copies stay on the existing
                // path; only frame-local copies memcpy into their own slot.
                let agg_copy_src = matches!(rvalue, Rvalue::Use(Operand::Copy(_)))
                    && is_aggregate_copy_src(body, tcx, rvalue);
                // A body that returns through the sret ABI copies the result
                // words into the caller's buffer at Return, so flowing to the
                // return does not disqualify the stack slot - PROVIDED the
                // Return will not also free the returned pointer as a fresh
                // heap block (`return_local_is_fresh_aggregate`): freeing a
                // stack slot address aborts. A Copy-built return chain is
                // never fresh, so the `np = *pos; mutate; np` shape stays
                // slot-backed while constructor-built returns keep their
                // heap path.
                let whole_agg_copy = agg_copy_src
                    && intrinsics.stack_slotted.contains(&place.local)
                    && ((intrinsics.sret_ptr.is_some() && !return_local_is_fresh_aggregate(body))
                        || !local_flows_to_return(body, place.local));
                // A copy destination that is MUTATED through a projection and
                // flows to the return slot can take neither path above: a
                // stack slot cannot outlive the frame, and a rebind aliases
                // the source so the mutation writes through to it (`let mut
                // np = *pos; np.f = ..; np`). Copy into a fresh heap
                // aggregate instead - the same storage a returned aggregate
                // construction uses, freed by the caller's aggregate drop.
                let heap_agg_copy = agg_copy_src
                    && !whole_agg_copy
                    && local_flows_to_return(body, place.local)
                    && (body.blocks.iter().any(|b| {
                        b.stmts.iter().any(|s| {
                            matches!(&s.kind, StatementKind::Assign { place: p, .. }
                                if p.local == place.local && !p.projection.is_empty())
                        })
                    }) || local_fields_written_through_address(body, place.local));
                if whole_agg_copy {
                    let slots = type_slot_count(tcx, body.local_ty(place.local)).max(1);
                    let dst_var = ensure_var(
                        builder,
                        locals,
                        body,
                        tcx,
                        module,
                        &intrinsics.body_cl_types,
                        place.local,
                    );
                    let dst_addr = builder.use_var(dst_var);
                    let ptr_ty = module.target_config().pointer_type();
                    let dst_ptr = coerce_arg_to(builder, dst_addr, ptr_ty).unwrap_or(dst_addr);
                    if value_type(value, builder) == types::I128 {
                        // A two-slot inline carrier (`Option` / `Result`)
                        // is an i128 value, not a source address - store
                        // its halves into the slot directly.
                        store_i128_words(builder, value, dst_ptr, 0);
                    } else {
                        let src_ptr = coerce_arg_to(builder, value, ptr_ty).unwrap_or(value);
                        for slot_idx in 0..slots {
                            let off = ir::immediates::Offset32::new((slot_idx as i32) * 8);
                            let word = builder.ins().load(
                                types::I64,
                                MemFlagsData::trusted(),
                                src_ptr,
                                off,
                            );
                            builder
                                .ins()
                                .store(MemFlagsData::trusted(), word, dst_ptr, off);
                        }
                        // The words are in the destination's own slot now, so
                        // the copy the container allocated for this read has
                        // no reader left.
                        if matches!(rvalue, Rvalue::CallIntrinsic { name, .. } if *name == "gos_result_payload_owned")
                        {
                            let free_fn = intrinsics.extern_fn(
                                module,
                                "gos_rt_aggr_free",
                                &[ptr_ty, types::I64],
                                &[],
                            )?;
                            let free_ref = module.declare_func_in_func(free_fn, builder.func);
                            let size_val = builder
                                .ins()
                                .iconst(types::I64, i64::from((slots * 8).max(8)));
                            builder.ins().call(free_ref, &[src_ptr, size_val]);
                        }
                    }
                } else if heap_agg_copy {
                    let slots = type_slot_count(tcx, body.local_ty(place.local)).max(1);
                    let ptr_ty = module.target_config().pointer_type();
                    let alloc_fn = intrinsics.extern_fn(
                        module,
                        "gos_rt_aggr_alloc",
                        &[types::I64],
                        &[ptr_ty],
                    )?;
                    let alloc_ref = module.declare_func_in_func(alloc_fn, builder.func);
                    let size_val = builder
                        .ins()
                        .iconst(types::I64, i64::from((slots * 8).max(8)));
                    let alloc_call = builder.ins().call(alloc_ref, &[size_val]);
                    let dst_ptr = builder.inst_results(alloc_call)[0];
                    if value_type(value, builder) == types::I128 {
                        store_i128_words(builder, value, dst_ptr, 0);
                    } else {
                        let src_ptr = coerce_arg_to(builder, value, ptr_ty).unwrap_or(value);
                        for slot_idx in 0..slots {
                            let off = ir::immediates::Offset32::new((slot_idx as i32) * 8);
                            let word = builder.ins().load(
                                types::I64,
                                MemFlagsData::trusted(),
                                src_ptr,
                                off,
                            );
                            builder
                                .ins()
                                .store(MemFlagsData::trusted(), word, dst_ptr, off);
                        }
                    }
                    define_var_to(
                        builder,
                        locals,
                        &intrinsics.body_cl_types,
                        place.local,
                        dst_ptr,
                    );
                } else if agg_into_slot.is_some() {
                    // The aggregate was filled into `place.local`'s own slot;
                    // its variable already holds that slot address, so there
                    // is nothing to rebind.
                } else {
                    define_var_to(
                        builder,
                        locals,
                        &intrinsics.body_cl_types,
                        place.local,
                        value,
                    );
                }
                if let Some(elem) = aggregate_elem_ty {
                    intrinsics.elem_cl_ty.insert(place.local, elem);
                }
                if let Some(slots) = aggregate_elem_slots {
                    intrinsics.elem_slots.insert(place.local, slots);
                }
                if let Some(total) = aggregate_total_slots {
                    intrinsics.local_slots.insert(place.local, total);
                }
                if let Some(slots) = projected_aggregate_slots {
                    intrinsics.local_slots.insert(place.local, slots);
                }
                if let Some(src) = copy_src_meta {
                    if let Some(et) = intrinsics.elem_cl_ty.get(&src).copied() {
                        intrinsics.elem_cl_ty.entry(place.local).or_insert(et);
                    }
                    if let Some(es) = intrinsics.elem_slots.get(&src).copied() {
                        intrinsics.elem_slots.entry(place.local).or_insert(es);
                    }
                    if let Some(ls) = intrinsics.local_slots.get(&src).copied() {
                        intrinsics.local_slots.entry(place.local).or_insert(ls);
                    }
                }
            } else {
                let elem_hint = intrinsics.elem_cl_ty.get(&place.local).copied();
                let leaf_ty = resolve_place_cl_type(
                    tcx,
                    body,
                    place,
                    module,
                    elem_hint.or(Some(value_type(value, builder))),
                );
                lower_place_store(
                    module, builder, locals, body, tcx, place, value, leaf_ty, intrinsics,
                )?;
            }
        }
        StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {}
        StatementKind::IterSource { .. }
        | StatementKind::IterAdapter { .. }
        | StatementKind::IterNext { .. } => unreachable!("typed iterator statements handled above"),
        // SetDiscriminant: store variant index at offset 0 of the
        // enum's backing place. Matches the Downcast convention
        // (tag at slot 0, payload at +8).
        StatementKind::SetDiscriminant { place, variant } => {
            let addr = if place.projection.is_empty() {
                let var = ensure_var(
                    builder,
                    locals,
                    body,
                    tcx,
                    module,
                    &intrinsics.body_cl_types,
                    place.local,
                );
                builder.use_var(var)
            } else {
                lower_place_address(module, builder, locals, body, tcx, place, intrinsics)?
            };
            let tag = builder.ins().iconst(types::I64, i64::from(*variant));
            builder.ins().store(
                MemFlagsData::trusted(),
                tag,
                addr,
                ir::immediates::Offset32::new(0),
            );
        }
        // `static mut` scalar write: store into the static's backing
        // writable data object. Non-scalar statics keep the VM fallback
        // since their init is a heap value, not an inline word.
        StatementKind::StaticStore { target, value } => {
            if !is_scalar_static_ty(tcx, target.ty) {
                bail!(
                    "native codegen: static mut store of non-scalar type unsupported; running on VM"
                )
            }
            let cl_ty = cl_type_of(tcx, target.ty, module);
            let data_id = intrinsics.intern_static(module, target, cl_ty)?;
            let val = lower_operand(
                module,
                builder,
                locals,
                body,
                tcx,
                value,
                Some(cl_ty),
                intrinsics,
            )?;
            let ptr_ty = module.target_config().pointer_type();
            let gv = module.declare_data_in_func(data_id, builder.func);
            let addr = builder.ins().symbol_value(ptr_ty, gv);
            builder.ins().store(MemFlagsData::trusted(), val, addr, 0);
        }
    }
    Ok(())
}

/// True when `op` is a `Vec`/`Slice` (or `&` to one) whose element provably
/// occupies an 8-byte stride in every construction path: the word-width ints,
/// `f64`, and nested `Vec`/`Slice` handle slots. Mirrors the LLVM backend's
/// `vec_operand_has_word_elem`. Narrower elements (`bool`, `u8` byte buffers,
/// `i32`, `String`, aggregates) keep the runtime call so the header-driven
/// stride / load width stays correct.
fn vec_operand_has_word_elem(body: &Body, tcx: &TyCtxt, op: &Operand) -> bool {
    let Operand::Copy(pl) = op else {
        return false;
    };
    // Method helpers commonly index a Vec field (`self.memory[i]`). Resolve
    // the projected leaf type instead of rejecting it as non-local, so the
    // JIT and native paths retain the same direct word load/store fast path
    // for Intcode-style state machines.
    let mut ty = resolve_place_ty(tcx, body, pl);
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    let elem = match tcx.kind_of(ty) {
        TyKind::Vec(e) | TyKind::Slice(e) => *e,
        _ => return false,
    };
    matches!(
        tcx.kind_of(elem),
        TyKind::Int(IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize)
            | TyKind::Float(FloatTy::F64)
            | TyKind::Vec(_)
            | TyKind::Slice(_)
    )
}

/// True when the operand is a `Vec`/`[T]` whose element word is a handle the
/// vector owns, so a store has to give back the share the slot held.
fn vec_operand_elem_owns_word(body: &Body, tcx: &TyCtxt, op: &Operand) -> bool {
    let Operand::Copy(pl) = op else {
        return false;
    };
    let mut ty = resolve_place_ty(tcx, body, pl);
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    let elem = match tcx.kind_of(ty) {
        TyKind::Vec(e) | TyKind::Slice(e) => *e,
        _ => return false,
    };
    matches!(
        tcx.kind_of(elem),
        TyKind::String
            | TyKind::Vec(_)
            | TyKind::Slice(_)
            | TyKind::HashMap { .. }
            | TyKind::JsonValue
    ) || tcx.is_rc_managed(elem)
}

/// Inline the word-stride `Vec`/`Slice` element get/set runtime calls
/// (`gos_rt_vec_get_i64` / `gos_rt_vec_set_i64` and their `_unchecked`
/// variants) as direct loads/stores off the `GosVec` header, mirroring the
/// LLVM backend's inline fast path so a JIT-compiled hot loop keeps the
/// loop-invariant data-pointer and length in registers instead of an opaque
/// per-element call. Returns `Ok(true)` when handled inline; `Ok(false)`
/// leaves the call to the generic dispatch unchanged.
///
/// Only the provably-8-byte-stride elements ([`vec_operand_has_word_elem`])
/// take this path. Semantics match the runtime exactly: a checked access
/// outside `[0, len)`, or through a null receiver, panics with the text every
/// tier uses for an indexed read. GosVec layout: `len@0`, `ptr@24`.
fn try_lower_vec_index_inline(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    blocks: &HashMap<u32, ir::Block>,
    name: &str,
    args: &[Operand],
    destination: &Place,
    target: Option<&gossamer_mir::BlockId>,
    intrinsics: &mut IntrinsicContext,
) -> Result<bool> {
    let (is_get, checked, want_args) = match name {
        "gos_rt_vec_get_i64" => (true, true, 2),
        "gos_rt_vec_get_i64_unchecked" => (true, false, 2),
        "gos_rt_vec_set_i64" => (false, true, 3),
        "gos_rt_vec_set_i64_unchecked" => (false, false, 3),
        _ => return Ok(false),
    };
    if args.len() != want_args || !vec_operand_has_word_elem(body, tcx, &args[0]) {
        return Ok(false);
    }
    // A store that replaces a handle the vector owns has to give the outgoing
    // one back, which is the runtime helper's job.
    if !is_get && vec_operand_elem_owns_word(body, tcx, &args[0]) {
        return Ok(false);
    }
    let ptr_ty = module.target_config().pointer_type();
    let vec_raw = lower_operand(
        module,
        builder,
        locals,
        body,
        tcx,
        &args[0],
        Some(ptr_ty),
        intrinsics,
    )?;
    let vec_ptr = coerce_arg_to(builder, vec_raw, ptr_ty).unwrap_or(vec_raw);
    let idx_raw = lower_operand(
        module,
        builder,
        locals,
        body,
        tcx,
        &args[1],
        Some(types::I64),
        intrinsics,
    )?;
    let idx = match value_type(idx_raw, builder) {
        t if t == types::I64 => idx_raw,
        t if t.is_int() && t.bits() < 64 => builder.ins().sextend(types::I64, idx_raw),
        _ => idx_raw,
    };
    let idx_ptr = if ptr_ty == types::I64 {
        idx
    } else {
        builder.ins().ireduce(ptr_ty, idx)
    };
    if is_get {
        if checked {
            let check_b = builder.create_block();
            let load_b = builder.create_block();
            let dflt_b = builder.create_block();
            let cont_b = builder.create_block();
            builder.append_block_param(cont_b, types::I64);
            builder.append_block_param(dflt_b, types::I64);
            let null = builder.ins().iconst(ptr_ty, 0);
            let zero_len = builder.ins().iconst(types::I64, 0);
            let isnull = builder.ins().icmp(IntCC::Equal, vec_ptr, null);
            builder.ins().brif(
                isnull,
                dflt_b,
                &[ir::BlockArg::Value(zero_len)],
                check_b,
                &[],
            );
            builder.switch_to_block(check_b);
            let len = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), vec_ptr, 0);
            let zero_i = builder.ins().iconst(types::I64, 0);
            let lo = builder.ins().icmp(IntCC::SignedLessThan, idx, zero_i);
            let hi = builder
                .ins()
                .icmp(IntCC::SignedGreaterThanOrEqual, idx, len);
            let bad = builder.ins().bor(lo, hi);
            builder
                .ins()
                .brif(bad, dflt_b, &[ir::BlockArg::Value(len)], load_b, &[]);
            builder.switch_to_block(load_b);
            let v = load_vec_word(builder, ptr_ty, vec_ptr, idx_ptr);
            builder.ins().jump(cont_b, &[ir::BlockArg::Value(v)]);
            builder.switch_to_block(dflt_b);
            let oob_len = builder.block_params(dflt_b)[0];
            emit_vec_index_panic(module, builder, intrinsics, idx, oob_len)?;
            builder.switch_to_block(cont_b);
            let result = builder.block_params(cont_b)[0];
            store_call_result(
                module,
                builder,
                locals,
                body,
                tcx,
                destination,
                result,
                intrinsics,
            )?;
        } else {
            let v = load_vec_word(builder, ptr_ty, vec_ptr, idx_ptr);
            store_call_result(
                module,
                builder,
                locals,
                body,
                tcx,
                destination,
                v,
                intrinsics,
            )?;
        }
    } else {
        let val = lower_operand(
            module, builder, locals, body, tcx, &args[2], None, intrinsics,
        )?;
        if checked {
            let check_b = builder.create_block();
            let store_b = builder.create_block();
            let oob_b = builder.create_block();
            let cont_b = builder.create_block();
            builder.append_block_param(oob_b, types::I64);
            let null = builder.ins().iconst(ptr_ty, 0);
            let zero_len = builder.ins().iconst(types::I64, 0);
            let isnull = builder.ins().icmp(IntCC::Equal, vec_ptr, null);
            builder.ins().brif(
                isnull,
                oob_b,
                &[ir::BlockArg::Value(zero_len)],
                check_b,
                &[],
            );
            builder.switch_to_block(check_b);
            let len = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), vec_ptr, 0);
            let zero_i = builder.ins().iconst(types::I64, 0);
            let lo = builder.ins().icmp(IntCC::SignedLessThan, idx, zero_i);
            let hi = builder
                .ins()
                .icmp(IntCC::SignedGreaterThanOrEqual, idx, len);
            let bad = builder.ins().bor(lo, hi);
            builder
                .ins()
                .brif(bad, oob_b, &[ir::BlockArg::Value(len)], store_b, &[]);
            builder.switch_to_block(oob_b);
            let set_oob_len = builder.block_params(oob_b)[0];
            emit_vec_index_panic(module, builder, intrinsics, idx, set_oob_len)?;
            builder.switch_to_block(store_b);
            store_vec_word(builder, ptr_ty, vec_ptr, idx_ptr, val);
            builder.ins().jump(cont_b, &[]);
            builder.switch_to_block(cont_b);
        } else {
            store_vec_word(builder, ptr_ty, vec_ptr, idx_ptr, val);
        }
    }
    match target {
        Some(block_id) => {
            let block = blocks[&block_id.as_u32()];
            builder.ins().jump(block, &[]);
        }
        None => {
            builder.ins().trap(ir::TrapCode::user(1).unwrap());
        }
    }
    Ok(true)
}

/// Emits the out-of-range exit of an inline vec access: the same
/// `gos_rt_panic_oob("vec index", idx, len)` call the LLVM tier emits and the
/// bytecode VM performs, so an index outside `[0, len)` fails with one text on
/// every tier. The trap after the call gives the block a terminator, since the
/// helper's Rust signature returns `!`.
fn emit_vec_index_panic(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    idx: ir::Value,
    len: ir::Value,
) -> Result<()> {
    let panic_fn = intrinsics.extern_fn_by_name(module, "gos_rt_panic_oob")?;
    let panic_ref = module.declare_func_in_func(panic_fn, builder.func);
    let what_data = intrinsics.intern_string(module, "vec index")?;
    let what_ptr = intrinsics.static_string_body_ptr(module, builder, what_data);
    let _ = builder.ins().call(panic_ref, &[what_ptr, idx, len]);
    builder.ins().trap(ir::TrapCode::user(5).unwrap());
    Ok(())
}

/// Element address for a word-stride `GosVec`: load the data pointer from
/// header offset 24, then `data_ptr + idx*8`, and load the 8-byte word.
fn load_vec_word(
    builder: &mut FunctionBuilder<'_>,
    ptr_ty: ir::Type,
    vec_ptr: ir::Value,
    idx_ptr: ir::Value,
) -> ir::Value {
    let dptr = builder
        .ins()
        .load(ptr_ty, MemFlagsData::trusted(), vec_ptr, 24);
    let eight = builder.ins().iconst(ptr_ty, 8);
    let off = builder.ins().imul(idx_ptr, eight);
    let ea = builder.ins().iadd(dptr, off);
    builder
        .ins()
        .load(types::I64, MemFlagsData::trusted(), ea, 0)
}

/// Store the 8-byte word `val` into a word-stride `GosVec` element address
/// (`data_ptr + idx*8`). `val` keeps its native type (`i64` or `f64`); both
/// write the same 8 bytes the runtime helper would.
fn store_vec_word(
    builder: &mut FunctionBuilder<'_>,
    ptr_ty: ir::Type,
    vec_ptr: ir::Value,
    idx_ptr: ir::Value,
    val: ir::Value,
) {
    let dptr = builder
        .ins()
        .load(ptr_ty, MemFlagsData::trusted(), vec_ptr, 24);
    let eight = builder.ins().iconst(ptr_ty, 8);
    let off = builder.ins().imul(idx_ptr, eight);
    let ea = builder.ins().iadd(dptr, off);
    builder.ins().store(MemFlagsData::trusted(), val, ea, 0);
}

pub(super) fn lower_terminator(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    blocks: &mut HashMap<u32, ir::Block>,
    callees_by_def: &HashMap<u32, ir::FuncRef>,
    callees_by_name: &HashMap<String, ir::FuncRef>,
    terminator: &Terminator,
    intrinsics: &mut IntrinsicContext,
    cleanup_plan: &gossamer_mir::CleanupPlan,
    src_block: u32,
) -> Result<()> {
    match terminator {
        Terminator::Goto { target } => {
            let _ = src_block;
            let block = blocks[&target.as_u32()];
            builder.ins().jump(block, &[]);
        }
        Terminator::Return => {
            // No call-stack pop: the compiled tier no longer maintains
            // a per-call shadow stack (see the matching note in
            // `lowering_body::lower_body`). Backtraces come from real
            // stack unwinding on panic / SIGQUIT.
            // Emit cleanup calls using the same summary-aware plan the body
            // lowering used for block-entry/block-exit cleanup. Recomputing a
            // summary-less plan here can disagree with block cleanup and
            // produce either leaks or duplicate frees.
            if !cleanup_plan.is_empty() {
                let ptr_ty = module.target_config().pointer_type();
                for entry in cleanup_plan.at_return() {
                    let Some(&var) = locals.get(&entry.local) else {
                        continue;
                    };
                    let raw = builder.use_var(var);
                    let ptr = coerce_arg_to(builder, raw, ptr_ty).unwrap_or(raw);
                    let free_fn = intrinsics.extern_fn(module, entry.free_fn, &[ptr_ty], &[])?;
                    let free_ref = module.declare_func_in_func(free_fn, builder.func);
                    builder.ins().call(free_ref, &[ptr]);
                }
            }
            let retval = match locals.get(&Local(0)).copied() {
                Some(var) => builder.use_var(var),
                None => builder.ins().iconst(types::I64, 0),
            };
            // Aggregate returns: the local-0 variable holds a pointer
            // into a stack slot owned by the current frame. Returning
            // it directly hands the caller a dangling pointer the
            // moment the frame pops. Heap-allocate via
            // `gos_rt_gc_alloc`, copy the inline data over, and return
            // the heap pointer instead. Mirrors the LLVM tier so both
            // backends agree on the by-value-aggregate ABI.
            let ret_ty_mir = body.local_ty(Local(0));
            // A by-value two-word inline enum (`Result` / `Option` / inline
            // user enum) is returned as a packed `i128` value, NOT a pointer
            // to a heap block: the function signature declares the slot `I128`
            // (see `build_signature_from_types`) and the local already holds
            // the `gos_rt_result_new`-shaped `[disc, payload]` value, so it
            // flows straight through the scalar-coerce path below.
            let ret_is_inline_two_word = is_inline_two_word_ty(tcx, ret_ty_mir);
            let ret_is_aggregate = !ret_is_inline_two_word
                && matches!(
                    tcx.kind_of(ret_ty_mir),
                    TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Adt { .. }
                );
            let ret_slots = type_slot_count(tcx, ret_ty_mir).max(1);
            // 1-slot values are themselves the value (an i64
            // / pointer / scalar). Copying through them by
            // dereferencing the local would treat the value as
            // a *pointer to data* and load 8 bytes from the
            // pointee - corrupting any function returning a
            // user-defined enum (heap pointer to a `[disc, ...]`
            // aggregate). Only the multi-slot aggregate cases
            // (real Tuple, Array, struct Adt) need the heap
            // copy that escapes the stack frame.
            let ret_is_aggregate = ret_is_aggregate && ret_slots > 1;
            if let Some(sret) = intrinsics.sret_ptr {
                // Structural-return ABI: copy the two result words into the
                // caller-owned slot (`sret`) instead of allocating a fresh heap
                // block per call. The source block this frame built is then
                // freed - so a hot tuple-returning body never leaks - but only
                // when the return local is a provably standalone allocation; an
                // interior pointer into another aggregate must not be freed.
                let ptr_ty = module.target_config().pointer_type();
                let slots = type_slot_count(tcx, ret_ty_mir).max(1);
                for slot_idx in 0..slots {
                    let off = (slot_idx as i32) * 8;
                    let word = builder.ins().load(
                        types::I64,
                        MemFlagsData::trusted(),
                        retval,
                        ir::immediates::Offset32::new(off),
                    );
                    builder.ins().store(
                        MemFlagsData::trusted(),
                        word,
                        sret,
                        ir::immediates::Offset32::new(off),
                    );
                }
                if return_local_is_fresh_aggregate(body) {
                    let bytes = u64::from(slots) * 8;
                    let free_fn = intrinsics.extern_fn(
                        module,
                        "gos_rt_aggr_free",
                        &[ptr_ty, types::I64],
                        &[],
                    )?;
                    let free_ref = module.declare_func_in_func(free_fn, builder.func);
                    let bytes_v = builder.ins().iconst(types::I64, bytes as i64);
                    builder.ins().call(free_ref, &[retval, bytes_v]);
                }
                builder.ins().return_(&[sret]);
            } else if ret_is_aggregate {
                // Arrays are always heap-allocated (calloc'd by Rvalue::Repeat /
                // Rvalue::Aggregate). The local already holds a dedicated heap
                // pointer, so returning it directly is safe - no second copy
                // needed. A tuple / Adt built only from `Aggregate` / `Repeat`
                // rvalues is equally standalone, so it can also be returned
                // directly; copying it would abandon (leak) the original block
                // on every call. A tuple / Adt that may carry an interior
                // pointer into a containing aggregate's buffer (a field
                // projection or call passthrough) still needs the gc_alloc +
                // memcpy escape path.
                if matches!(tcx.kind_of(ret_ty_mir), TyKind::Array { .. })
                    || return_local_is_fresh_aggregate(body)
                {
                    builder.ins().return_(&[retval]);
                } else {
                    let bytes = u64::from(ret_slots) * 8;
                    let alloc_id = intrinsics.extern_fn_by_name(module, "gos_rt_gc_alloc")?;
                    let alloc_ref = module.declare_func_in_func(alloc_id, builder.func);
                    let bytes_v = builder.ins().iconst(types::I64, bytes as i64);
                    let call = builder.ins().call(alloc_ref, &[bytes_v]);
                    let heap = builder.inst_results(call)[0];
                    let slots = type_slot_count(tcx, ret_ty_mir).max(1);
                    for slot_idx in 0..slots {
                        let off = (slot_idx as i32) * 8;
                        let word = builder.ins().load(
                            types::I64,
                            MemFlagsData::trusted(),
                            retval,
                            ir::immediates::Offset32::new(off),
                        );
                        builder.ins().store(
                            MemFlagsData::trusted(),
                            word,
                            heap,
                            ir::immediates::Offset32::new(off),
                        );
                    }
                    builder.ins().return_(&[heap]);
                }
            } else {
                // Coerce the return value to the function's declared
                // return type. The MIR may have stored a narrow
                // type (i8 for bool) into the RETURN local even when
                // the function signature is `i64`; cranelift's
                // verifier rejects width mismatches between the
                // returned value and the function signature.
                let want = builder
                    .func
                    .signature
                    .returns
                    .first()
                    .map_or(types::I64, |p| p.value_type);
                let coerced = coerce_arg_to(builder, retval, want)
                    .unwrap_or_else(|_| builder.ins().iconst(want, 0));
                builder.ins().return_(&[coerced]);
            }
        }
        Terminator::Call {
            callee,
            args,
            destination,
            target,
        } => {
            // Runtime-intrinsic shortcut: calls to the prelude
            // `println` / `panic` don't reach user code - they land
            // in a C-ABI runtime function. MIR lowering carries the
            // callee name as a `Const(Str(...))` when the resolver
            // hasn't assigned a `DefId` (prelude values fall into
            // this bucket). `noreturn` intrinsics (panic) are
            // responsible for terminating the block themselves; the
            // fall-through `jump target` is skipped.
            // Before the general intrinsic lowering, which answers for every
            // symbol the ABI registry names and would take these two with it:
            // a word-stride Vec/Slice element get/set inlines to a load/store
            // off the `GosVec` header, mirroring the LLVM backend so a
            // JIT-compiled hot index loop keeps the data pointer and the
            // length in registers instead of calling out per element.
            if let Operand::Const(ConstValue::Str(name)) = callee {
                if try_lower_vec_index_inline(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    &*blocks,
                    name,
                    args,
                    destination,
                    target.as_ref(),
                    intrinsics,
                )? {
                    return Ok(());
                }
            }
            if let Some(name) = callee_prelude_name(callee) {
                let outcome = lower_intrinsic_outcome(
                    &name,
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    args,
                    destination,
                    intrinsics,
                )?;
                if outcome.handled {
                    if !outcome.noreturn {
                        match target {
                            Some(block_id) => {
                                let block = blocks[&block_id.as_u32()];
                                builder.ins().jump(block, &[]);
                            }
                            None => {
                                builder.ins().trap(ir::TrapCode::user(1).unwrap());
                            }
                        }
                    }
                    return Ok(());
                }
            }
            // Indirect call through a closure value. The callee
            // operand is the local that holds the closure's heap
            // env pointer. The env's first word is the real function
            // pointer; subsequent words carry the captures the lifted
            // function reads via `gos_load(env, 8*i)`. The callee's
            // signature is `(env_ptr, args…) -> i64`.
            if let Operand::Copy(place) = callee {
                let ptr_ty = module.target_config().pointer_type();
                // Two shapes hide behind the "Copy(local) callee":
                //   1. Closure env: local holds a pointer to a
                //      heap record whose first word is the lifted
                //      function's address. Indirect-call through
                //      `load(env+0)` with `env` as the implicit
                //      first arg.
                //   2. Plain function pointer: local is a
                //      `FnDef`-typed value obtained from an
                //      `Operand::FnRef` - its value IS the function
                //      address directly. No `env` prelude, no
                //      leading load. `f(x)` becomes a straight
                //      `call_indirect(addr, x)`.
                let callee_ty = body.local_ty(place.local);
                // `FnTrait` is the closure-trait callable shape. Its
                // value is an env_ptr (heap blob `[fn_addr, captures…]`),
                // so it routes through the same env+code dispatch the
                // MIR `Closure` shape uses. Bare `fn` and `fn item`
                // values stay on the raw single-pointer call path.
                // Direct call only when the local was assigned from
                // an `Operand::FnRef` (a `FnDef`-typed value): the
                // value IS the function address. `FnPtr` and
                // `FnTrait` locals are now uniformly env_ptr-shaped
                // - the MIR's coercion at let / return / assign
                // boundaries wraps every bare fn item into an
                // `[fn_addr, captures…]` heap blob first. Loading
                // through env[0] and forwarding `(env, args…)` is
                // the universal dispatch.
                let is_plain_fn = matches!(tcx.kind_of(callee_ty), TyKind::FnDef { .. });
                // Pull the FnTrait / FnPtr signature so the
                // indirect-call dispatch uses real argument and
                // return types, not flat i64. This is the fix
                // that lets capturing closures returning bool /
                // f64 / aggregates flow through Fn(...) params
                // without calling-convention drift.
                let fn_sig = match tcx.kind_of(callee_ty).clone() {
                    TyKind::FnTrait(s) | TyKind::FnPtr(s) => Some(s),
                    _ => None,
                };
                let env_value =
                    lower_place_read(module, builder, locals, body, tcx, place, None, intrinsics)?;
                let env_ptr = if ptr_ty == types::I64 {
                    env_value
                } else {
                    builder.ins().ireduce(ptr_ty, env_value)
                };
                let fn_ptr = if is_plain_fn {
                    env_ptr
                } else {
                    builder.ins().load(
                        ptr_ty,
                        MemFlagsData::trusted(),
                        env_ptr,
                        ir::immediates::Offset32::new(0),
                    )
                };
                let mut sig = module.make_signature();
                if !is_plain_fn {
                    sig.params.push(AbiParam::new(types::I64));
                }
                // Per-arg cranelift types: prefer the FnTrait /
                // FnPtr sig's input types over flat-i64 so f64 /
                // bool / aggregate args use the right register.
                let mut typed_param_tys: Vec<ir::Type> = Vec::with_capacity(args.len());
                for (i, _) in args.iter().enumerate() {
                    let want = match &fn_sig {
                        Some(sig_ref) if i < sig_ref.inputs.len() => {
                            callable_slot_ty(tcx, sig_ref.inputs[i], module)
                        }
                        _ => types::I64,
                    };
                    typed_param_tys.push(want);
                    sig.params.push(AbiParam::new(want));
                }
                let typed_ret_ty = match &fn_sig {
                    Some(sig_ref) if !matches!(tcx.kind_of(sig_ref.output), TyKind::Unit) => {
                        Some(callable_slot_ty(tcx, sig_ref.output, module))
                    }
                    _ => Some(types::I64),
                };
                // The word at `env+0` is the entry the Rust runtime also
                // calls, so it carries the platform's wire shape: a two-word
                // carrier comes back in a vector register under the Win64
                // ABI. A bare `fn` item value is called directly and keeps
                // the shape Cranelift compiled it with.
                let wire_ret_ty = typed_ret_ty.map(|t| {
                    if is_plain_fn {
                        t
                    } else {
                        win64_wire_return(module.target_config(), t)
                    }
                });
                if let Some(t) = wire_ret_ty {
                    sig.returns.push(AbiParam::new(t));
                }
                // A callee returning a by-value aggregate was compiled with the
                // sret ABI (a hidden trailing pointer param it writes the result
                // through and returns). The indirect call must supply that
                // pointer or the callee reads an unset param as its result slot.
                // The size comes from the callable's return type (its `Fn`
                // signature output) or, for a bare `fn` item value, the target
                // body's recorded sret slot count.
                let indirect_sret_slots = match &fn_sig {
                    Some(s) => {
                        let out = s.output;
                        (!is_inline_two_word_ty(tcx, out)
                            && matches!(
                                tcx.kind_of(out),
                                TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Adt { .. }
                            )
                            && (type_slot_count(tcx, out) > 1
                                || single_slot_addr_aggregate(tcx, out)))
                        .then(|| type_slot_count(tcx, out).max(1))
                    }
                    None => match tcx.kind_of(callee_ty).clone() {
                        TyKind::FnDef { def, substs } => (!substs.is_empty())
                            .then(|| gossamer_mir::mangled_name(def, &substs))
                            .and_then(|m| intrinsics.sret_slots_by_name.get(&m).copied())
                            .or_else(|| intrinsics.sret_slots_by_def.get(&def.local).copied()),
                        _ => None,
                    },
                };
                if indirect_sret_slots.is_some() {
                    sig.params.push(AbiParam::new(ptr_ty));
                }
                let sig_ref = builder.import_signature(sig);
                let mut arg_values = Vec::with_capacity(args.len() + 1);
                if !is_plain_fn {
                    arg_values.push(env_value);
                }
                for (i, op) in args.iter().enumerate() {
                    let v =
                        lower_operand(module, builder, locals, body, tcx, op, None, intrinsics)?;
                    let want = typed_param_tys.get(i).copied().unwrap_or(types::I64);
                    let coerced = coerce_arg_to(builder, v, want).unwrap_or(v);
                    arg_values.push(coerced);
                }
                if let Some(slots) = indirect_sret_slots {
                    let bytes = slots.max(1).saturating_mul(8);
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        bytes,
                        3,
                    ));
                    arg_values.push(builder.ins().stack_addr(ptr_ty, slot, 0));
                }
                let call = builder.ins().call_indirect(sig_ref, fn_ptr, &arg_values);
                let results = builder.inst_results(call).to_vec();
                let results: Vec<ir::Value> = match (typed_ret_ty, wire_ret_ty) {
                    (Some(logical), Some(wire)) if logical != wire => results
                        .into_iter()
                        .map(|v| bitcast_same_width(builder, logical, v))
                        .collect(),
                    _ => results,
                };
                if let Some(&ret) = results.first() {
                    store_call_result(
                        module,
                        builder,
                        locals,
                        body,
                        tcx,
                        destination,
                        ret,
                        intrinsics,
                    )?;
                }
                match target {
                    Some(block_id) => {
                        let block = blocks[&block_id.as_u32()];
                        builder.ins().jump(block, &[]);
                    }
                    None => {
                        builder.ins().trap(ir::TrapCode::user(1).unwrap());
                    }
                }
                return Ok(());
            }
            // First try resolving a `Const(Str("name"))` callee
            // against the module's function table - closures lifted
            // by `lift_closures` appear here as `Const(Str)` when the
            // MIR lowerer records them via `local_fn_name`. Only fall
            // through to the runtime diagnostic stub when the name
            // is genuinely unknown.
            if let Operand::Const(ConstValue::Str(name)) = callee {
                if let Some(func_ref) = callees_by_name.get(name).copied() {
                    let expected = builder
                        .func
                        .dfg
                        .signatures
                        .get(builder.func.dfg.ext_funcs[func_ref].signature)
                        .map(|s| s.params.iter().map(|p| p.value_type).collect::<Vec<_>>())
                        .unwrap_or_default();
                    let mut arg_values: Vec<ir::Value> = Vec::with_capacity(args.len());
                    let ptr_ty_local = module.target_config().pointer_type();
                    for (idx, op) in args.iter().enumerate() {
                        let mut v = lower_operand(
                            module, builder, locals, body, tcx, op, None, intrinsics,
                        )?;
                        // Special case: char→ptr promotion for
                        // string-API helpers where the user
                        // passed a `char` literal where a
                        // `String` was expected (`s.split(',')`,
                        // `s.contains('-')`, …). The existing
                        // coerce extends i32 to i64 which is
                        // wrong - the runtime would dereference
                        // the char value as a pointer. Route
                        // through `gos_rt_char_to_str` instead.
                        if let Some(want) = expected.get(idx).copied() {
                            let have = value_type(v, builder);
                            if want == ptr_ty_local
                                && have == types::I32
                                && operand_is_char(body, tcx, op)
                            {
                                let cts = intrinsics.extern_fn(
                                    module,
                                    "gos_rt_char_to_str",
                                    &[types::I32],
                                    &[ptr_ty_local],
                                )?;
                                let cts_ref = module.declare_func_in_func(cts, builder.func);
                                let call = builder.ins().call(cts_ref, &[v]);
                                v = builder.inst_results(call)[0];
                            } else {
                                v = coerce_arg_to(builder, v, want)?;
                            }
                        }
                        arg_values.push(v);
                    }
                    let ret_slots = callee_sret_slots(callee, Some(name), intrinsics);
                    maybe_push_sret_slot(
                        builder,
                        ptr_ty_local,
                        &expected,
                        args.len(),
                        ret_slots,
                        &mut arg_values,
                    )?;
                    let call = builder.ins().call(func_ref, &arg_values);
                    let results = builder.inst_results(call).to_vec();
                    if let Some(&ret) = results.first() {
                        store_call_result(
                            module,
                            builder,
                            locals,
                            body,
                            tcx,
                            destination,
                            ret,
                            intrinsics,
                        )?;
                    }
                    match target {
                        Some(block_id) => {
                            let block = blocks[&block_id.as_u32()];
                            builder.ins().jump(block, &[]);
                        }
                        None => {
                            builder.ins().trap(ir::TrapCode::user(1).unwrap());
                        }
                    }
                    return Ok(());
                }
                // Registry-known `gos_rt_*` symbol that wasn't pre-bound
                // into `callees_by_name`. The registry records the helper's
                // logical signature; `emit_rt_call_by_name` declares and calls
                // it in the platform's wire shape, so a two-word carrier
                // crosses the way the runtime reads it on every target.
                // Without this branch every newly-added runtime helper that
                // MIR references by name silently zeros at codegen time.
                if name.starts_with("gos_rt_") {
                    if let Some(entry) = gossamer_abi::lookup(name) {
                        let logical: Vec<ir::Type> = entry
                            .sig
                            .params
                            .iter()
                            .filter_map(|t| abi_type_to_cranelift(*t))
                            .collect();
                        let mut arg_values: Vec<ir::Value> = Vec::with_capacity(args.len());
                        for (idx, op) in args.iter().enumerate() {
                            let mut v = lower_operand(
                                module, builder, locals, body, tcx, op, None, intrinsics,
                            )?;
                            if let Some(want) = logical.get(idx).copied() {
                                v = coerce_arg_to(builder, v, want)?;
                            }
                            arg_values.push(v);
                        }
                        let result = emit_rt_call_by_name(
                            module,
                            builder,
                            intrinsics,
                            entry.name,
                            &arg_values,
                        )?;
                        if let Some(ret) = result {
                            store_call_result(
                                module,
                                builder,
                                locals,
                                body,
                                tcx,
                                destination,
                                ret,
                                intrinsics,
                            )?;
                        }
                        match target {
                            Some(block_id) => {
                                let block = blocks[&block_id.as_u32()];
                                builder.ins().jump(block, &[]);
                            }
                            None => {
                                builder.ins().trap(ir::TrapCode::user(1).unwrap());
                            }
                        }
                        return Ok(());
                    }
                }
                // Stdlib-shaped callees (`std::...`, `fmt::...`,
                // `os::...`, `sync::...`, …) plus enum-variant
                // constructors (`Ok`, `Err`, `Some`, `None`, user
                // enums that start with an uppercase letter) and
                // anything else the codegen has not wired default
                // to a zero-return stub so the program still
                // builds. Semantics match the call returning a
                // default value of its declared type. This is a
                // deliberate L1 compromise; L2 replaces stubs with
                // real runtime symbols.
                let is_variant = name.chars().next().is_some_and(char::is_uppercase);
                if name.contains("::") || is_variant {
                    // Option / Result variant constructors with a
                    // payload (`Ok(v)`, `Some(v)`, `Err(e)`) lower to
                    // identity: the wrapped value passes through
                    // unchanged so `r.unwrap()` (also identity)
                    // recovers it.
                    if matches!(name.as_str(), "Ok" | "Some" | "Err") && !args.is_empty() {
                        let result_value = lower_operand(
                            module, builder, locals, body, tcx, &args[0], None, intrinsics,
                        )?;
                        store_call_result(
                            module,
                            builder,
                            locals,
                            body,
                            tcx,
                            destination,
                            result_value,
                            intrinsics,
                        )?;
                        match target {
                            Some(block_id) => {
                                let block = blocks[&block_id.as_u32()];
                                builder.ins().jump(block, &[]);
                            }
                            None => {
                                builder.ins().trap(ir::TrapCode::user(1).unwrap());
                            }
                        }
                        return Ok(());
                    }
                    // Any other qualified or capitalised callee that
                    // reaches here matched neither the intrinsic
                    // dispatch, the runtime-symbol table, nor a user
                    // body - the cranelift backend has no lowering for
                    // it. Emitting a typed-zero default would silently
                    // corrupt the result; refuse so JIT compilation of
                    // this program aborts and the bytecode VM (which
                    // resolves the call correctly) runs it instead.
                    bail!(
                        "native codegen: refusing to emit zero-stub for unresolved \
                         qualified/variant call '{name}' - the bytecode VM resolves it correctly"
                    );
                }
                // 0.8.0: no soft-zero fallback. An unknown call
                // name is a hard error - silent zero stubs hide
                // typos and miscompiled stdlib paths. The legacy
                // `GOSSAMER_STRICT_LOWER=1` env var was the opt-in;
                // it is now the only behaviour.
                bail!(
                    "native codegen: refusing to emit zero-stub for unknown call '{name}' - \
                    typos and missing dispatch entries are a compile error, not a runtime crash"
                );
            }
            // 0.8.0: same policy as the unknown-name path above -
            // an unresolved FnRef callee is a hard error rather
            // than a silent zero. The historical zero-stub bypass
            // was the opt-in under `GOSSAMER_STRICT_LOWER=1`; it
            // is now the only behaviour.
            let func_ref =
                resolve_callee(callee, callees_by_def, callees_by_name).map_err(|_| {
                    anyhow!(
                        "native codegen: refusing to emit zero-stub for unresolved FnRef callee \
                    (likely a variant constructor missing from the codegen dispatch table)"
                    )
                })?;
            let expected = builder
                .func
                .dfg
                .signatures
                .get(builder.func.dfg.ext_funcs[func_ref].signature)
                .map(|s| s.params.iter().map(|p| p.value_type).collect::<Vec<_>>())
                .unwrap_or_default();
            let mut arg_values: Vec<ir::Value> = Vec::with_capacity(args.len());
            for (idx, op) in args.iter().enumerate() {
                let mut v =
                    lower_operand(module, builder, locals, body, tcx, op, None, intrinsics)?;
                // Defensive copy of by-value aggregate args: the
                // local holds a pointer into a heap slot the caller
                // owns. Forwarding it directly aliases the caller's
                // struct, so the callee's `mut p; p.x = …` mutates
                // the caller's value too. Copy into a per-call-site
                // stack slot: the callee's frame nests inside the
                // call, every callee-side escape (return via sret,
                // container push, rebinding) copies the words on, and
                // a heap block here is never reclaimed - a hot search
                // loop allocated gigabytes per second through it. A
                // very large aggregate keeps the heap path rather
                // than risking frame overflow.
                if let Some(slots) = operand_aggregate_slots(body, tcx, op)
                    .filter(|_| !callee_param_is_by_ref(callee, intrinsics, idx))
                {
                    if slots <= 4096 {
                        let ptr_ty = module.target_config().pointer_type();
                        let slot = builder.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            slots * 8,
                            3,
                        ));
                        let dst = builder.ins().stack_addr(ptr_ty, slot, 0);
                        let src_ptr = coerce_arg_to(builder, v, ptr_ty).unwrap_or(v);
                        for slot_idx in 0..slots {
                            let off = ir::immediates::Offset32::new((slot_idx as i32) * 8);
                            let word = builder.ins().load(
                                types::I64,
                                MemFlagsData::trusted(),
                                src_ptr,
                                off,
                            );
                            builder.ins().store(MemFlagsData::trusted(), word, dst, off);
                        }
                        v = dst;
                    } else {
                        v = clone_aggregate_value(module, builder, intrinsics, v, slots)?;
                    }
                }
                if let Some(want) = expected.get(idx).copied() {
                    v = coerce_arg_to(builder, v, want)?;
                }
                arg_values.push(v);
            }
            let ptr_ty = module.target_config().pointer_type();
            let ret_slots = callee_sret_slots(callee, None, intrinsics);
            maybe_push_sret_slot(
                builder,
                ptr_ty,
                &expected,
                args.len(),
                ret_slots,
                &mut arg_values,
            )?;
            let call = builder.ins().call(func_ref, &arg_values);
            let results = builder.inst_results(call).to_vec();
            if let Some(&ret) = results.first() {
                store_call_result(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    destination,
                    ret,
                    intrinsics,
                )?;
            }
            match target {
                Some(block_id) => {
                    let block = blocks[&block_id.as_u32()];
                    builder.ins().jump(block, &[]);
                }
                None => {
                    builder.ins().trap(ir::TrapCode::user(1).unwrap());
                }
            }
        }
        Terminator::SwitchInt {
            discriminant,
            arms,
            default,
        } => {
            let _ = src_block;
            let value = lower_operand(
                module,
                builder,
                locals,
                body,
                tcx,
                discriminant,
                None,
                intrinsics,
            )?;
            let value_ty = value_type(value, builder);
            let default_block = blocks[&default.as_u32()];
            // Chain a compare-and-branch per arm, falling through
            // to the next compare on a miss. Cranelift's optimiser
            // collapses the chain into a jump table for dense arms.
            for (arm_value, arm_target) in arms {
                let arm_block = blocks[&arm_target.as_u32()];
                let next = builder.create_block();
                // Match the discriminant's cranelift type; bool
                // discriminants come back as i8, smaller ints as
                // their natural width.
                let cmp_value = builder.ins().iconst(value_ty, i64_truncate(*arm_value));
                let matched = builder
                    .ins()
                    .icmp(ir::condcodes::IntCC::Equal, value, cmp_value);
                builder.ins().brif(matched, arm_block, &[], next, &[]);
                builder.switch_to_block(next);
            }
            builder.ins().jump(default_block, &[]);
        }
        Terminator::Assert {
            cond,
            expected,
            target,
            msg,
        } => {
            let value = lower_operand(module, builder, locals, body, tcx, cond, None, intrinsics)?;
            let value_ty = value_type(value, builder);
            // `expected` is a bool; coerce the constant to whatever
            // width the cond produces.
            let expected_value = builder.ins().iconst(value_ty, i64::from(*expected));
            let pass = builder.create_block();
            let fail = builder.create_block();
            let matched = builder
                .ins()
                .icmp(ir::condcodes::IntCC::Equal, value, expected_value);
            builder.ins().brif(matched, pass, &[], fail, &[]);
            builder.switch_to_block(fail);
            // A failed assertion renders `error[GX0005]` and exits /
            // unwinds through the runtime, matching the VM and AOT
            // tiers, instead of a bare illegal-instruction trap.
            emit_runtime_panic(module, builder, intrinsics, assert_message_text(msg))?;
            builder.switch_to_block(pass);
            let block = blocks[&target.as_u32()];
            builder.ins().jump(block, &[]);
        }
        Terminator::Panic { message } => {
            emit_runtime_panic(module, builder, intrinsics, message)?;
        }
        Terminator::Drop { target, .. } => {
            // No destructors to run today; treat the drop as a
            // direct jump and revisit once real RAII semantics
            // land in MIR.
            let block = blocks[&target.as_u32()];
            builder.ins().jump(block, &[]);
        }
        Terminator::Unreachable => {
            // A well-formed program never reaches this terminator. If a
            // miscompiled path does, render a clean diagnostic and exit /
            // unwind through the runtime rather than executing an
            // illegal-instruction trap.
            emit_runtime_panic(module, builder, intrinsics, UNREACHABLE_PANIC_MSG)?;
        }
    }
    Ok(())
}

/// Diagnostic text rendered by `Terminator::Unreachable` if a
/// miscompiled path ever reaches it.
pub(super) const UNREACHABLE_PANIC_MSG: &str = "internal error: reached unreachable code\n";

/// Diagnostic text for each assertion kind, mirroring the LLVM
/// backend's strings so panic output is identical across tiers.
pub(super) fn assert_message_text(msg: &AssertMessage) -> &'static str {
    match msg {
        AssertMessage::BoundsCheck => "index out of bounds\n",
        AssertMessage::Overflow => "arithmetic overflow\n",
        AssertMessage::DivideByZero => "divide by zero\n",
    }
}

/// Every fixed panic message the terminator lowering can emit. The
/// JIT pre-interns these as data before its parallel codegen phase so
/// `emit_runtime_panic` never declares a fresh string mid-parallel.
/// Keep in sync with [`assert_message_text`] and [`UNREACHABLE_PANIC_MSG`].
pub(super) const STATIC_PANIC_MESSAGES: &[&str] = &[
    "index out of bounds\n",
    "arithmetic overflow\n",
    "attempt to add with overflow\n",
    "attempt to subtract with overflow\n",
    "attempt to multiply with overflow\n",
    "divide by zero\n",
    UNREACHABLE_PANIC_MSG,
];

/// Emits a call to the runtime panic shim with a static message,
/// followed by a trap that serves as the block's unreachable
/// terminator (the shim is `-> !`: it renders `error[GX0005]` and
/// exits on the main goroutine or unwinds inside a spawned one).
/// Mirrors the LLVM backend's `gos_rt_panic` lowering so the
/// in-process JIT tier produces the same clean diagnostic the VM and
/// AOT tiers do instead of a raw machine trap.
pub(super) fn emit_runtime_panic(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    intrinsics: &mut IntrinsicContext,
    message: &str,
) -> Result<()> {
    let panic_fn = intrinsics.extern_fn_by_name(module, "gos_rt_panic")?;
    let panic_ref = module.declare_func_in_func(panic_fn, builder.func);
    let msg_data = intrinsics.intern_string(module, message)?;
    let msg_ptr = intrinsics.static_string_body_ptr(module, builder, msg_data);
    let _ = builder.ins().call(panic_ref, &[msg_ptr]);
    builder.ins().trap(ir::TrapCode::user(4).unwrap());
    Ok(())
}
