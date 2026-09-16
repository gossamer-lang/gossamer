// Simple MIR optimisations.
// Commits to three lightweight passes: constant folding,
// copy propagation, and dead-store elimination. Each pass is
// idempotent so callers can run them in any order.

use std::collections::{HashMap, HashSet, VecDeque};

use gossamer_lex::Span;
use gossamer_types::{TyCtxt, TyKind};

use gossamer_types::Ty;

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Projection,
    Rvalue, Statement, StatementKind, Terminator,
};

/// Returns `false` when `GOSSAMER_INLINE=0` (or `false`) is set. Lets
/// the differential test assert that inlining never changes observable
/// program behaviour: run a program with the inliner on and off and
/// compare output. Read live (not memoised) so a test can flip it.
fn inlining_enabled() -> bool {
    !matches!(
        std::env::var("GOSSAMER_INLINE").ok().as_deref(),
        Some("0" | "false")
    )
}

/// Loop versioning duplicates a counted loop behind runtime guards. It can
/// help when a single preheader check removes a small number of runtime calls,
/// while a large candidate set can bloat tight inner loops enough to block
/// LLVM's best codegen on DP kernels. Leave it enabled by default, with an
/// escape hatch for differential benchmark work.
fn bounds_versioning_enabled() -> bool {
    !matches!(
        std::env::var("GOSSAMER_BOUNDS_VERSIONING").ok().as_deref(),
        Some("0" | "false" | "no")
    )
}

/// Maps each body's resolver `DefId` (its `local` index) to the body
/// name, so an `Operand::FnRef` call site can be resolved to the
/// callee body the same way the JIT compile-set BFS resolves it.
fn def_to_name_map(bodies: &[Body]) -> HashMap<u32, String> {
    bodies
        .iter()
        .filter_map(|b| b.def.map(|d| (d.local, b.name.clone())))
        .collect()
}

/// Resolves a call terminator's `callee` to the name of the user body
/// it targets, if any. Handles both by-name (`Const(Str)`) calls and
/// monomorphic `FnRef` calls (resolved through `def_to_name`). Generic
/// `FnRef` calls (non-empty `substs`) target a mangled specialisation
/// and are left for the call-site lowering to resolve.
fn callee_body_name(callee: &Operand, def_to_name: &HashMap<u32, String>) -> Option<String> {
    match callee {
        Operand::Const(ConstValue::Str(name)) => Some(name.clone()),
        Operand::FnRef { def, substs } if substs.is_empty() => def_to_name.get(&def.local).cloned(),
        _ => None,
    }
}

/// Runs the full optimisation pipeline on `body`. Copy propagation
/// runs before constant folding so that temporaries introduced by the
/// lowerer (`tmp = Const(1); out = BinaryOp(Copy(tmp), ...)`) collapse
/// into the two-constant form folding recognises. A second copy-prop
/// pass after folding propagates the newly-created constants.
pub fn optimise(body: &mut Body, tcx: &TyCtxt) {
    optimise_with_bounds_limit(body, tcx, Some(versioning_candidate_limit()), false);
}

/// JIT preparation optimises for Cranelift promotion admission and hot-loop
/// dispatch, where the unchecked clone is often required to make a body
/// lowerable. Keep the general versioning pass aggressive for this path.
pub fn optimise_for_jit(body: &mut Body, tcx: &TyCtxt) {
    optimise_with_bounds_limit(body, tcx, None, true);
}

fn optimise_with_bounds_limit(
    body: &mut Body,
    tcx: &TyCtxt,
    versioning_candidate_limit: Option<usize>,
    checked_overflow: bool,
) {
    crate::verify::debug_verify_body(body);
    copy_propagate(body, tcx);
    elide_vec_clone_in_three_way_swaps(body);
    crate::verify::debug_verify_body(body);
    const_fold_typed(body, tcx, checked_overflow);
    crate::verify::debug_verify_body(body);
    copy_propagate(body, tcx);
    crate::verify::debug_verify_body(body);
    unbox_local_carriers(body, tcx);
    crate::verify::debug_verify_body(body);
    scalar_replace_short_lived_aggregates(body);
    crate::verify::debug_verify_body(body);
    const_branch_elim(body);
    crate::verify::debug_verify_body(body);
    dead_block_sweep(body);
    crate::verify::debug_verify_body(body);
    dead_store_elim(body, tcx);
    crate::verify::debug_verify_body(body);
    hoist_loop_invariant_field_reads(body, tcx);
    crate::verify::debug_verify_body(body);
    tabulate_nested_row_reads(body, tcx);
    crate::verify::debug_verify_body(body);
    let bounds_before = bounds_access_counts(body);
    bounds_check_elim(body, tcx);
    let after_counted = bounds_access_counts(body);
    crate::verify::debug_verify_body(body);
    local_branch_bounds_check_elim(body, tcx);
    let after_local = bounds_access_counts(body);
    crate::verify::debug_verify_body(body);
    if bounds_versioning_enabled() {
        bounds_check_versioning_with_limit(body, tcx, versioning_candidate_limit);
    }
    if std::env::var_os("GOS_BOUNDS_REMARKS").is_some() {
        let after_versioning = bounds_access_counts(body);
        eprintln!(
            "gos-bounds: function={} checked_before={} counted_eliminated={} local_eliminated={} versioned_fast_paths={} checked_fallbacks={} unchecked_paths={}",
            body.name,
            bounds_before.0,
            bounds_before.0.saturating_sub(after_counted.0),
            after_counted.0.saturating_sub(after_local.0),
            after_versioning.1.saturating_sub(after_local.1),
            after_versioning.0,
            after_versioning.1,
        );
    }
    crate::verify::debug_verify_body(body);
}

/// Returns checked access sites and unchecked fast-path sites in a body.
fn bounds_access_counts(body: &Body) -> (usize, usize) {
    body.blocks.iter().fold((0, 0), |mut counts, block| {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            ..
        } = &block.terminator
        else {
            return counts;
        };
        match name.as_str() {
            "gos_rt_vec_get_i64" | "gos_rt_vec_set_i64" => counts.0 += 1,
            "gos_rt_vec_get_i64_unchecked" | "gos_rt_vec_set_i64_unchecked" => counts.1 += 1,
            _ => {}
        }
        counts
    })
}

/// Fast canonicalisation for unoptimised native builds.
///
/// Monomorphisation and ownership lowering happen outside this function and
/// remain identical across profiles. Debug builds skip whole-program inlining,
/// but retain inexpensive local canonicalisation and local bounds facts. Those
/// facts are required for predictable debug-tier runtime on counted loops.
pub fn optimise_debug(body: &mut Body, tcx: &TyCtxt) {
    crate::verify::debug_verify_body(body);
    copy_propagate(body, tcx);
    crate::verify::debug_verify_body(body);
    const_fold_typed(body, tcx, true);
    crate::verify::debug_verify_body(body);
    copy_propagate(body, tcx);
    crate::verify::debug_verify_body(body);
    const_branch_elim(body);
    crate::verify::debug_verify_body(body);
    dead_block_sweep(body);
    crate::verify::debug_verify_body(body);
    dead_store_elim(body, tcx);
    crate::verify::debug_verify_body(body);
    bounds_check_elim(body, tcx);
    crate::verify::debug_verify_body(body);
    local_branch_bounds_check_elim(body, tcx);
    crate::verify::debug_verify_body(body);
    if bounds_versioning_enabled() {
        bounds_check_versioning(body, tcx);
    }
    crate::verify::debug_verify_body(body);
}

/// Eliminates a short-lived aggregate when every use is a direct scalar field
/// read in the same block.  This is deliberately narrower than a general SSA
/// conversion: it accepts only a single aggregate construction, no escaping
/// use, no borrow, no whole-value copy, and operands whose source locals are
/// not subsequently written.  Those rules preserve the construction-time
/// snapshot semantics while making `Pair(a, b).right` use `b` directly on the
/// VM as well as in the native tiers.
pub(crate) fn scalar_replace_short_lived_aggregates(body: &mut Body) {
    for block_index in 0..body.blocks.len() {
        let candidates: Vec<(usize, Local, Vec<Operand>)> = {
            let block = &body.blocks[block_index];
            let mut candidates = Vec::new();
            for (idx, stmt) in block.stmts.iter().enumerate() {
                let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Aggregate { operands, .. },
                } = &stmt.kind
                else {
                    continue;
                };
                if !place.is_simple()
                    || !aggregate_is_confined_to_block(body, block_index, place.local)
                    || !aggregate_operands_stable_after(&block.stmts, idx, operands)
                    || !aggregate_uses_are_direct_fields(
                        &block.stmts,
                        idx,
                        &block.terminator,
                        place.local,
                        operands.len(),
                    )
                {
                    continue;
                }
                candidates.push((idx, place.local, operands.clone()));
            }
            candidates
        };

        let block = &mut body.blocks[block_index];
        for (idx, local, operands) in candidates {
            for stmt in block.stmts.iter_mut().skip(idx + 1) {
                replace_aggregate_field_reads(&mut stmt.kind, local, &operands);
            }
            block.stmts[idx].kind = StatementKind::Nop;
        }
    }
}

/// Scalar replacement does not cross basic-block boundaries. A successor can
/// observe the aggregate through a loop-carried or ordinary local, while this
/// small pass only rewrites direct field reads in the construction block.
fn aggregate_is_confined_to_block(body: &Body, source_block: usize, local: Local) -> bool {
    body.blocks
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != source_block)
        .all(|(_, block)| !block_mentions_local(block, local))
}

fn block_mentions_local(block: &BasicBlock, local: Local) -> bool {
    block
        .stmts
        .iter()
        .any(|stmt| statement_mentions_local(stmt, local))
        || terminator_mentions_local(&block.terminator, local)
}

fn statement_mentions_local(stmt: &Statement, local: Local) -> bool {
    match &stmt.kind {
        StatementKind::Assign { place, rvalue } => {
            scalar_place_mentions_local(place, local) || scalar_rvalue_mentions_local(rvalue, local)
        }
        StatementKind::StorageLive(marked) | StatementKind::StorageDead(marked) => *marked == local,
        StatementKind::SetDiscriminant { place, .. } => scalar_place_mentions_local(place, local),
        StatementKind::StaticStore { value, .. } => scalar_replacement_operand_mentions_local(value, local),
        StatementKind::IterSource { dst, source, .. } => {
            scalar_place_mentions_local(dst, local)
                || scalar_replacement_operand_mentions_local(source, local)
        }
        StatementKind::IterAdapter {
            dst,
            upstream,
            closure_or_arg,
            ..
        } => {
            scalar_place_mentions_local(dst, local)
                || scalar_place_mentions_local(upstream, local)
                || closure_or_arg
                    .as_ref()
                    .is_some_and(|operand| scalar_replacement_operand_mentions_local(operand, local))
        }
        StatementKind::IterNext {
            dst_option,
            iter_place,
            ..
        } => {
            scalar_place_mentions_local(dst_option, local)
                || scalar_place_mentions_local(iter_place, local)
        }
        StatementKind::Nop => false,
    }
}

fn scalar_rvalue_mentions_local(rvalue: &Rvalue, local: Local) -> bool {
    match rvalue {
        Rvalue::Use(operand) | Rvalue::UnaryOp { operand, .. } | Rvalue::Cast { operand, .. } => {
            scalar_replacement_operand_mentions_local(operand, local)
        }
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            scalar_replacement_operand_mentions_local(lhs, local)
                || scalar_replacement_operand_mentions_local(rhs, local)
        }
        Rvalue::Aggregate { operands, .. } | Rvalue::CallIntrinsic { args: operands, .. } => operands
            .iter()
            .any(|operand| scalar_replacement_operand_mentions_local(operand, local)),
        Rvalue::Len(place) | Rvalue::Ref { place, .. } => scalar_place_mentions_local(place, local),
        Rvalue::Repeat { value, .. } => scalar_replacement_operand_mentions_local(value, local),
        Rvalue::StaticLoad(_) => false,
    }
}

fn scalar_place_mentions_local(place: &Place, local: Local) -> bool {
    place.local == local
        || place
            .projection
            .iter()
            .any(|projection| matches!(projection, Projection::Index(index) if *index == local))
}

fn aggregate_operands_stable_after(stmts: &[Statement], idx: usize, operands: &[Operand]) -> bool {
    operands.iter().all(|operand| match operand {
        Operand::Copy(place) => !stmts.iter().skip(idx + 1).any(|stmt| {
            matches!(&stmt.kind, StatementKind::Assign { place: destination, .. } if destination.local == place.local)
        }),
        Operand::Const(_) | Operand::FnRef { .. } => true,
    })
}

fn aggregate_uses_are_direct_fields(
    stmts: &[Statement],
    idx: usize,
    terminator: &Terminator,
    local: Local,
    operand_count: usize,
) -> bool {
    for stmt in stmts.iter().skip(idx + 1) {
        match &stmt.kind {
            StatementKind::Assign { place, rvalue } => {
                if place.local == local
                    || !rvalue_uses_only_direct_fields(rvalue, local, operand_count)
                {
                    return false;
                }
            }
            // A static store can outlive this frame, and neither the VM nor
            // native backends may retain a pointer into the erased aggregate.
            StatementKind::StaticStore { value, .. }
                if scalar_replacement_operand_mentions_local(value, local) =>
            {
                return false;
            }
            StatementKind::SetDiscriminant { place, .. } if place.local == local => return false,
            StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::StaticStore { .. }
            | StatementKind::Nop
            | StatementKind::SetDiscriminant { .. }
            | StatementKind::IterSource { .. }
            | StatementKind::IterAdapter { .. }
            | StatementKind::IterNext { .. } => {}
        }
    }
    !terminator_mentions_local(terminator, local)
}

fn rvalue_uses_only_direct_fields(rvalue: &Rvalue, local: Local, operand_count: usize) -> bool {
    let op_ok = |operand: &Operand| operand_is_direct_field_or_other(operand, local, operand_count);
    match rvalue {
        Rvalue::Use(operand) | Rvalue::UnaryOp { operand, .. } | Rvalue::Cast { operand, .. } => {
            op_ok(operand)
        }
        Rvalue::BinaryOp { lhs, rhs, .. } => op_ok(lhs) && op_ok(rhs),
        Rvalue::Aggregate { operands, .. } => operands.iter().all(op_ok),
        Rvalue::Repeat { value, .. } => op_ok(value),
        Rvalue::CallIntrinsic { args, .. } => args.iter().all(op_ok),
        Rvalue::Len(place) | Rvalue::Ref { place, .. } => place.local != local,
        Rvalue::StaticLoad(_) => true,
    }
}

fn operand_is_direct_field_or_other(operand: &Operand, local: Local, operand_count: usize) -> bool {
    match operand {
        Operand::Copy(place) if place.local == local => matches!(
            place.projection.as_slice(),
            [Projection::Field(field)] if (*field as usize) < operand_count
        ),
        _ => true,
    }
}

fn scalar_replacement_operand_mentions_local(operand: &Operand, local: Local) -> bool {
    matches!(operand, Operand::Copy(place) if place.local == local)
}

fn terminator_mentions_local(terminator: &Terminator, local: Local) -> bool {
    match terminator {
        Terminator::Goto { .. } | Terminator::Return | Terminator::Unreachable | Terminator::Panic { .. } => false,
        Terminator::SwitchInt { discriminant, .. } => {
            scalar_replacement_operand_mentions_local(discriminant, local)
        }
        Terminator::Assert { cond, msg, .. } => {
            scalar_replacement_operand_mentions_local(cond, local)
                || msg.operands().any(|op| scalar_replacement_operand_mentions_local(op, local))
        }
        Terminator::Call {
            callee,
            args,
            destination,
            ..
        } => {
            scalar_replacement_operand_mentions_local(callee, local)
                || args
                    .iter()
                    .any(|arg| scalar_replacement_operand_mentions_local(arg, local))
                || destination.local == local
        }
        Terminator::Drop { place, .. } => place.local == local,
    }
}

fn replace_aggregate_field_reads(kind: &mut StatementKind, local: Local, operands: &[Operand]) {
    let replace = |operand: &mut Operand| {
        if let Operand::Copy(place) = operand
            && place.local == local
            && let [Projection::Field(field)] = place.projection.as_slice()
            && let Some(source) = operands.get(*field as usize)
        {
            *operand = source.clone();
        }
    };
    match kind {
        StatementKind::Assign { rvalue, .. } => match rvalue {
            Rvalue::Use(operand) | Rvalue::UnaryOp { operand, .. } | Rvalue::Cast { operand, .. } => {
                replace(operand);
            }
            Rvalue::BinaryOp { lhs, rhs, .. } => {
                replace(lhs);
                replace(rhs);
            }
            Rvalue::Aggregate { operands, .. } | Rvalue::CallIntrinsic { args: operands, .. } => {
                for operand in operands {
                    replace(operand);
                }
            }
            Rvalue::Repeat { value, .. } => replace(value),
            Rvalue::Len(_) | Rvalue::Ref { .. } | Rvalue::StaticLoad(_) => {}
        },
        StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::StaticStore { .. } | StatementKind::Nop | StatementKind::SetDiscriminant { .. } | StatementKind::IterSource { .. } | StatementKind::IterAdapter { .. } | StatementKind::IterNext { .. } => {}
    }
}

/// Rewrites `Vec::new(elem_bytes)` into `gos_rt_vec_with_capacity(elem_bytes,
/// bound)` when the fresh vector is immediately populated by a counted loop
/// whose condition is `i < bound` and whose body pushes into that same vector.
///
/// This is intentionally allocation-only: it does not change the loop, element
/// values, container type, or visible semantics. A negative / zero bound is
/// harmless because the runtime capacity helper treats it like an empty reserve.
pub(crate) fn reserve_vecs_for_counted_push_loops(body: &mut Body) {
    let mut rewrites: Vec<(usize, Vec<Operand>)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee,
            args,
            destination,
            target,
        } = &block.terminator
        else {
            continue;
        };
        if !destination.projection.is_empty() || args.len() != 1 {
            continue;
        }
        if !matches!(callee, Operand::Const(ConstValue::Str(name)) if name == "Vec::new" || name == "gos_rt_vec_new")
        {
            continue;
        }
        let Some(start) = target else { continue };
        let Some(bound) =
            counted_push_loop_bound(body, block.id, *start, destination.local)
        else {
            continue;
        };
        if !reserve_bound_available_at_entry(body, &bound, block.id) {
            continue;
        }
        rewrites.push((bi, vec![args[0].clone(), bound]));
    }
    for (bi, args) in rewrites {
        if let Terminator::Call {
            callee,
            args: call_args,
            ..
        } = &mut body.blocks[bi].terminator
        {
            *callee = Operand::Const(ConstValue::Str("gos_rt_vec_with_capacity".to_string()));
            *call_args = args;
        }
    }
}

/// Rewrites a fresh word-layout `HashMap::new()` into
/// `gos_rt_map_new_with_capacity` when a following counted loop performs
/// exactly one insert into that map on every iteration. The native capacity
/// constructor carries a proven capacity; native backends select the typed
/// storage layout from the destination map type. Unsupported aggregate layouts
/// retain lazy storage while preserving the same constructor semantics.
/// Duplicate keys are harmless: the loop bound is an upper bound, never an
/// assertion about the final map length.
pub(crate) fn reserve_hashmaps_for_counted_insert_loops(body: &mut Body, tcx: &TyCtxt) {
    let mut rewrites: Vec<(usize, Vec<Operand>)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee,
            args,
            destination,
            target,
        } = &block.terminator
        else {
            continue;
        };
        // Surface `HashMap::new()` has no arguments. The native backends
        // derive its fixed word-sized key/value layout from the destination,
        // and their `*_with_capacity` lowering likewise expects just the
        // capacity operand. Keep accepting the historical runtime-shaped
        // form too, but discard its layout operands rather than accidentally
        // treating the key width as the capacity.
        if !destination.projection.is_empty()
            || !(args.is_empty()
                || (matches!(callee, Operand::Const(ConstValue::Str(name)) if name == "gos_rt_map_new")
                    && args.len() == 2))
        {
            continue;
        }
        if !matches!(callee, Operand::Const(ConstValue::Str(name)) if matches!(name.as_str(), "Map::new" | "collections::Map::new" | "HashMap::new" | "collections::HashMap::new" | "gos_rt_map_new"))
        {
            continue;
        }
        if !is_hashmap_local(body, tcx, destination.local) {
            continue;
        }
        let Some(start) = target else { continue };
        let Some(bound) =
            counted_insert_loop_bound(body, block.id, *start, destination.local)
        else {
            continue;
        };
        if !reserve_bound_available_at_entry(body, &bound, block.id) {
            continue;
        }
        rewrites.push((bi, vec![bound]));
    }
    for (bi, args) in rewrites {
        if let Terminator::Call {
            callee,
            args: call_args,
            ..
        } = &mut body.blocks[bi].terminator
        {
            *callee = Operand::Const(ConstValue::Str("gos_rt_map_new_with_capacity".to_string()));
            *call_args = args;
        }
    }
}
