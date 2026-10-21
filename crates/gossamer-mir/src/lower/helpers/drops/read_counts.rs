//! Local read counts and initialisation at returns.

use super::*;

/// Rewrites `gos_rt_str_concat` calls to the consuming variant when the MIR
/// emits the copy-back pattern: `tmp = str_concat(out, frag); out = Copy(tmp)`.
///
/// The Gossamer MIR builder lowers `out += frag` as two instructions across
/// two basic blocks:
///
/// ```text
/// bb_n:  Call { gos_rt_str_concat, [Copy(out), Copy(frag)] → tmp, target: bb_succ }
/// bb_succ: Assign { out ← Use(Copy(tmp)) }; …
/// ```
///
/// After the copy-back, the OLD value of `out` is unreachable. Without the consuming
/// variant, that allocation leaks on every loop iteration, producing O(n²) total
/// allocations for an accumulation loop over n elements.
///
/// `gos_rt_str_concat_drop_a(out, frag)` reads both args, allocates the result,
/// then frees `out` - safe because the free happens after the read. It no-ops
/// silently on null and rodata/literal `out` values.
/// Counts how many times each local is *read* across the whole body (as an
/// operand, a `Ref`/`Len`/`Drop` place base, a projected store base, or an
/// `Index` projection). Assignment / call-`destination` positions are writes
/// and are not counted. Used by [`fuse_substring_map_inc`] to prove the scratch
/// key String flows only into the fused probe.
pub(super) fn collect_local_read_counts(body: &Body) -> HashMap<u32, usize> {
    fn read_index_locals(place: &Place, counts: &mut HashMap<u32, usize>) {
        for proj in &place.projection {
            if let crate::ir::Projection::Index(idx) = proj {
                *counts.entry(idx.0).or_insert(0) += 1;
            }
        }
    }
    fn read_place(place: &Place, counts: &mut HashMap<u32, usize>) {
        *counts.entry(place.local.0).or_insert(0) += 1;
        read_index_locals(place, counts);
    }
    // A store destination reads its base only when addressed through a
    // projection (`*p = v`, `a[i] = v`); a bare `x = v` is a pure write.
    fn read_store_dest(place: &Place, counts: &mut HashMap<u32, usize>) {
        if !place.projection.is_empty() {
            *counts.entry(place.local.0).or_insert(0) += 1;
        }
        read_index_locals(place, counts);
    }
    fn read_operand(op: &Operand, counts: &mut HashMap<u32, usize>) {
        if let Operand::Copy(place) = op {
            read_place(place, counts);
        }
    }
    fn read_rvalue(rv: &Rvalue, counts: &mut HashMap<u32, usize>) {
        match rv {
            Rvalue::Use(op)
            | Rvalue::UnaryOp { operand: op, .. }
            | Rvalue::Cast { operand: op, .. } => {
                read_operand(op, counts);
            }
            Rvalue::BinaryOp { lhs, rhs, .. } => {
                read_operand(lhs, counts);
                read_operand(rhs, counts);
            }
            Rvalue::Aggregate { operands, .. } => {
                for op in operands {
                    read_operand(op, counts);
                }
            }
            Rvalue::Repeat { value, .. } => read_operand(value, counts),
            Rvalue::CallIntrinsic { args, .. } => {
                for op in args {
                    read_operand(op, counts);
                }
            }
            Rvalue::Len(place) | Rvalue::Ref { place, .. } => read_place(place, counts),
            Rvalue::StaticLoad(_) => {}
        }
    }
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    read_store_dest(place, &mut counts);
                    read_rvalue(rvalue, &mut counts);
                }
                StatementKind::SetDiscriminant { place, .. } => read_store_dest(place, &mut counts),
                StatementKind::StaticStore { value, .. } => read_operand(value, &mut counts),
                StatementKind::IterSource { dst, source, .. } => {
                    read_store_dest(dst, &mut counts);
                    read_operand(source, &mut counts);
                }
                StatementKind::IterAdapter {
                    dst,
                    upstream,
                    closure_or_arg,
                    ..
                } => {
                    read_store_dest(dst, &mut counts);
                    read_place(upstream, &mut counts);
                    if let Some(arg) = closure_or_arg {
                        read_operand(arg, &mut counts);
                    }
                }
                StatementKind::IterNext {
                    dst_option,
                    iter_place,
                    ..
                } => {
                    read_store_dest(dst_option, &mut counts);
                    read_place(iter_place, &mut counts);
                }
                StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
            }
        }
        match &block.terminator {
            Terminator::SwitchInt { discriminant, .. } => read_operand(discriminant, &mut counts),
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                read_operand(callee, &mut counts);
                for op in args {
                    read_operand(op, &mut counts);
                }
                read_store_dest(destination, &mut counts);
            }
            Terminator::Assert { cond, msg, .. } => {
                read_operand(cond, &mut counts);
                for op in msg.operands() {
                    read_operand(op, &mut counts);
                }
            }
            Terminator::Drop { place, .. } => read_place(place, &mut counts),
            Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::Unreachable
            | Terminator::Resume
            | Terminator::Panic { .. } => {}
        }
    }
    counts
}

/// Fuses a map probe keyed by a substring - `m.inc(seq.substring(i, j), by)`,
/// `m.get_or(..)`, `m.get(..)`, `m.contains_key(..)`, or `m.or_insert(..)`
/// on a map whose values own nothing - with an optional `.trim()` of the
/// substring between, into one call that reads the key in place out of `seq`.
/// The scratch Strings the substring and the trim would allocate on every
/// probe are removed; the map makes an owned key only for a key it has not
/// seen. Runs before the RC passes so no retain/release is emitted for the
/// Strings that no longer exist.
///
/// The rewrite only fires when each scratch String flows *only* into the next
/// step (read exactly once), so a key observed elsewhere keeps the allocating
/// path.
pub(crate) fn fuse_substring_map_inc(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let n = body.blocks.len();
    let reads = collect_local_read_counts(body);
    let reads_of = |local: Local| reads.get(&local.0).copied().unwrap_or(0);

    struct Plan {
        substr_idx: usize,
        probe_idx: usize,
        fused: &'static str,
        m: Operand,
        seq: Operand,
        start: Operand,
        end: Operand,
        trim: bool,
        last: Option<Operand>,
        dest: Place,
        target: Option<BlockId>,
        scratch: Vec<Local>,
        remove_copy_local: Option<Local>,
    }

    // The single-successor call in block `idx` producing `local`, by name.
    let producer = |local: Local, target_idx: usize, wanted: &str| {
        (0..n).find_map(|idx| {
            let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                destination,
                target: Some(target),
            } = &body.blocks[idx].terminator
            else {
                return None;
            };
            (name == wanted
                && destination.local == local
                && destination.projection.is_empty()
                && target.0 as usize == target_idx)
                .then(|| (idx, args.clone()))
        })
    };

    let mut plans: Vec<Plan> = Vec::new();
    for probe_idx in 0..n {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            target,
        } = &body.blocks[probe_idx].terminator
        else {
            continue;
        };
        let (fused, takes_last) = match name.as_str() {
            "gos_rt_map_inc_str_i64" | "gos_rt_map_inc_typed_str_i64" => {
                ("gos_rt_map_inc_substring_str_i64", true)
            }
            "gos_rt_map_get_or_str_str" => ("gos_rt_map_get_or_substring_str_str", true),
            "gos_rt_map_get_or_str_i64" | "gos_rt_map_get_or_typed_str_i64" => {
                ("gos_rt_map_get_or_substring_str_i64", true)
            }
            "gos_rt_map_get_str_opt" | "gos_rt_map_get_typed_str_opt" => {
                ("gos_rt_map_get_opt_substring_str", false)
            }
            "gos_rt_map_contains_key_str" | "gos_rt_map_contains_key_typed_str" => {
                ("gos_rt_map_contains_substring_str", false)
            }
            "gos_rt_map_or_insert_str_i64" | "gos_rt_map_or_insert_typed_str_i64"
                if args
                    .first()
                    .is_some_and(|m| map_values_own_nothing(body, tcx, m)) =>
            {
                ("gos_rt_map_or_insert_substring_str_i64", true)
            }
            _ => continue,
        };
        let (m, key_place, last) = match args.as_slice() {
            [m, Operand::Copy(key_place)] if !takes_last => (m, key_place, None),
            [m, Operand::Copy(key_place), last] if takes_last => (m, key_place, Some(last)),
            _ => continue,
        };
        if !key_place.projection.is_empty() {
            continue;
        }
        let key_local = key_place.local;

        // The key is the producing call's result, or a copy of it made in
        // this block (`key = Copy(src)`, a `let` binding).
        let mut src_local = key_local;
        let mut remove_copy_local: Option<Local> = None;
        for stmt in &body.blocks[probe_idx].stmts {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
                && place.local == key_local
                && place.projection.is_empty()
                && src.projection.is_empty()
            {
                src_local = src.local;
                remove_copy_local = Some(key_local);
            }
        }
        if reads_of(key_local) != 1 || (src_local != key_local && reads_of(src_local) != 1) {
            continue;
        }

        // `src = substring(seq, i, j)` straight into this block, or
        // `sub = substring(..)` into a block that only computes
        // `src = trim(sub)` into this one.
        let (substr_idx, substr_args, trim, scratch) =
            if let Some((idx, sa)) = producer(src_local, probe_idx, "gos_rt_str_substring") {
                (idx, sa, false, vec![src_local])
            } else if let Some((trim_idx, ta)) = producer(src_local, probe_idx, "gos_rt_str_trim")
                && body.blocks[trim_idx].stmts.iter().all(|stmt| {
                    matches!(
                        stmt.kind,
                        StatementKind::Nop
                            | StatementKind::StorageLive(_)
                            | StatementKind::StorageDead(_)
                    )
                })
                && let [Operand::Copy(sub_place)] = ta.as_slice()
                && sub_place.projection.is_empty()
                && reads_of(sub_place.local) == 1
                && let Some((idx, sa)) = producer(sub_place.local, trim_idx, "gos_rt_str_substring")
            {
                (idx, sa, true, vec![sub_place.local, src_local])
            } else {
                continue;
            };
        let [seq, start, end] = substr_args.as_slice() else {
            continue;
        };
        plans.push(Plan {
            substr_idx,
            probe_idx,
            fused,
            m: m.clone(),
            seq: seq.clone(),
            start: start.clone(),
            end: end.clone(),
            trim,
            last: last.cloned(),
            dest: destination.clone(),
            target: *target,
            scratch,
            remove_copy_local,
        });
    }

    for plan in plans {
        let substr_span = body.blocks[plan.substr_idx].span;
        let probe_block_id = body.blocks[plan.probe_idx].id;
        {
            let substr_block = &mut body.blocks[plan.substr_idx];
            // Null each scratch String slot so it is a defined null: any
            // release the RC pass may still schedule for its declared `String`
            // type is then a no-op rather than a read of an unassigned slot.
            for &local in &plan.scratch {
                substr_block.stmts.push(Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(local),
                        rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    },
                    span: substr_span,
                    inlined: None,
                });
            }
            substr_block.terminator = Terminator::Goto {
                target: probe_block_id,
            };
        }
        let probe_block = &mut body.blocks[plan.probe_idx];
        if let Some(copy_local) = plan.remove_copy_local {
            let src = *plan.scratch.last().unwrap_or(&copy_local);
            probe_block.stmts.retain(|stmt| {
                !matches!(
                    &stmt.kind,
                    StatementKind::Assign {
                        place,
                        rvalue: Rvalue::Use(Operand::Copy(from)),
                    } if place.local == copy_local
                        && place.projection.is_empty()
                        && from.local == src
                        && from.projection.is_empty()
                )
            });
        }
        let mut args = vec![
            plan.m,
            plan.seq,
            plan.start,
            plan.end,
            Operand::Const(ConstValue::Int(i128::from(plan.trim))),
        ];
        args.extend(plan.last);
        probe_block.terminator = Terminator::Call {
            callee: Operand::Const(ConstValue::Str(plan.fused.to_string())),
            args,
            destination: plan.dest,
            target: plan.target,
        };
    }
}

/// Whether `map` is a `Map<String, V>` whose `V` owns nothing: an integer,
/// float, `bool`, or `char`, which an `or_insert` stores as the word itself.
fn map_values_own_nothing(body: &Body, tcx: &gossamer_types::TyCtxt, map: &Operand) -> bool {
    use gossamer_types::TyKind;
    let Operand::Copy(place) = map else {
        return false;
    };
    if !place.projection.is_empty() {
        return false;
    }
    let Some(decl) = body.locals.get(place.local.0 as usize) else {
        return false;
    };
    let mut ty = decl.ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    matches!(
        tcx.kind_of(ty),
        TyKind::HashMap { value, .. }
            if matches!(
                tcx.kind_of(*value),
                TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
            )
    )
}

pub(crate) fn rewrite_str_concat_consuming(body: &mut Body) {
    let n_blocks = body.blocks.len();
    // Collect rename targets: (block_idx) where the Call should be renamed.
    let mut targets: Vec<usize> = Vec::new();
    for block_idx in 0..n_blocks {
        let Terminator::Call {
            callee,
            args,
            destination,
            target,
        } = &body.blocks[block_idx].terminator
        else {
            continue;
        };
        // Must be a str_concat call.
        let Operand::Const(ConstValue::Str(name)) = callee else {
            continue;
        };
        if name != "gos_rt_str_concat" {
            continue;
        }
        // Destination must be a bare local (no projection).
        if !destination.projection.is_empty() {
            continue;
        }
        let tmp_local = destination.local;
        // First arg must be a bare Copy of some local `src`.
        let Some(Operand::Copy(src_place)) = args.first() else {
            continue;
        };
        if !src_place.projection.is_empty() {
            continue;
        }
        let src_local = src_place.local;
        // If first-arg == destination (no copy-back needed), rename directly.
        if src_local == tmp_local {
            targets.push(block_idx);
            continue;
        }
        // Otherwise: check that the successor block's FIRST statement copies
        // `tmp` back into `src` - the copy-back pattern.
        let Some(succ_id) = target else { continue };
        let succ_idx = succ_id.0 as usize;
        if succ_idx >= n_blocks {
            continue;
        }
        let first_stmt = body.blocks[succ_idx].stmts.first();
        let is_copy_back = matches!(
            first_stmt,
            Some(Statement {
                kind: StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src_of_copy)),
                },
                ..
            }) if place.local == src_local
                && place.projection.is_empty()
                && src_of_copy.local == tmp_local
                && src_of_copy.projection.is_empty()
        );
        if is_copy_back {
            targets.push(block_idx);
        }
    }
    // Apply the renames.
    for block_idx in targets {
        if let Terminator::Call { callee, .. } = &mut body.blocks[block_idx].terminator {
            *callee = Operand::Const(ConstValue::Str("gos_rt_str_concat_drop_a".to_string()));
        }
    }
}

pub(crate) fn compute_init_at_block_entries(
    body: &Body,
    targets: &[(Local, &'static str)],
) -> Vec<Vec<bool>> {
    let n_blocks = body.blocks.len();
    let n_targets = targets.len();
    if n_blocks == 0 || n_targets == 0 {
        return vec![vec![false; n_targets]; n_blocks];
    }

    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        for s in block_successors(&block.terminator) {
            let si = s.0 as usize;
            if si < n_blocks {
                preds[si].push(i);
            }
        }
    }
    let target_locals: Vec<u32> = targets.iter().map(|(l, _)| l.0).collect();
    // A parameter holds the caller's value from entry.
    let param_target: Vec<bool> = target_locals
        .iter()
        .map(|l| (1..=body.arity).contains(l))
        .collect();

    let mut stmt_defs = vec![vec![false; n_targets]; n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
            {
                for (t, l) in target_locals.iter().enumerate() {
                    if place.local.0 == *l {
                        stmt_defs[i][t] = true;
                    }
                }
            }
        }
    }
    let mut term_defs = vec![vec![false; n_targets]; n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
        {
            for (t, l) in target_locals.iter().enumerate() {
                if destination.local.0 == *l {
                    term_defs[i][t] = true;
                }
            }
        }
    }

    // Must-init ("definitely initialised") is a forward intersection
    // analysis, so its correct solution is the GREATEST fixpoint: seed
    // every block TOP (`true`) and iterate downward. The entry block (no
    // predecessors) pins to `false`, and any loop back-edge that starts
    // `true` lets a value defined before the loop stay must-init across
    // the join instead of collapsing to `false` on the first pass (which
    // a least-fixpoint `false` seed would do, wrongly reporting a
    // pre-loop definition as not-yet-initialised inside the loop).
    let mut init_in = vec![vec![true; n_targets]; n_blocks];
    let mut init_out = vec![vec![true; n_targets]; n_blocks];
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..n_blocks {
            for t in 0..n_targets {
                let new_in = if preds[i].is_empty() {
                    i == 0 && param_target[t]
                } else {
                    preds[i].iter().all(|&p| init_out[p][t] || term_defs[p][t])
                };
                let new_out = new_in || stmt_defs[i][t];
                if new_in != init_in[i][t] || new_out != init_out[i][t] {
                    init_in[i][t] = new_in;
                    init_out[i][t] = new_out;
                    changed = true;
                }
            }
        }
    }
    init_in
}

pub(crate) fn compute_init_at_returns(
    body: &Body,
    targets: &[(Local, &'static str)],
) -> Vec<Vec<bool>> {
    let n_blocks = body.blocks.len();
    let n_targets = targets.len();
    let mut out = vec![vec![false; n_targets]; n_blocks];
    if n_blocks == 0 || n_targets == 0 {
        return out;
    }

    // Predecessor map for join nodes.
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        for s in block_successors(&block.terminator) {
            let si = s.0 as usize;
            if si < n_blocks {
                preds[si].push(i);
            }
        }
    }

    let target_locals: Vec<u32> = targets.iter().map(|(l, _)| l.0).collect();
    // A parameter holds the caller's value from entry.
    let param_target: Vec<bool> = target_locals
        .iter()
        .map(|l| (1..=body.arity).contains(l))
        .collect();

    // init_in[B][t] - must-init at entry of B.
    // init_out[B][t] - must-init after all of B's stmts (used at
    // the Return point for Return-terminated blocks).
    // Must-init is a forward intersection analysis; its correct
    // solution is the GREATEST fixpoint, so seed every block TOP
    // (`true`) and iterate downward. The entry block pins to `false`
    // (no predecessors), while a loop back-edge seeded `true` keeps a
    // value defined before the loop must-init across the join - a
    // `false` seed would read the join as not-init forever and skip an
    // otherwise-required at-return free.
    let mut init_in = vec![vec![true; n_targets]; n_blocks];
    let mut init_out = vec![vec![true; n_targets]; n_blocks];

    // Pre-compute stmt-position defs per (block, target).
    let mut stmt_defs = vec![vec![false; n_targets]; n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
            {
                for (t, l) in target_locals.iter().enumerate() {
                    if place.local.0 == *l {
                        stmt_defs[i][t] = true;
                    }
                }
            }
        }
    }
    // Terminator-position defs (Call destinations).
    let mut term_defs = vec![vec![false; n_targets]; n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
        {
            for (t, l) in target_locals.iter().enumerate() {
                if destination.local.0 == *l {
                    term_defs[i][t] = true;
                }
            }
        }
    }

    // Successors of a Call see the destination as already
    // initialised. Encode that by folding `term_defs[B]` into
    // `init_out[B]` *and* into the value propagated to successors.
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..n_blocks {
            for t in 0..n_targets {
                // Join: must-init at entry = AND across predecessors.
                let new_in = if preds[i].is_empty() {
                    i == 0 && param_target[t]
                } else {
                    preds[i].iter().all(|&p| init_out[p][t] || term_defs[p][t])
                };
                // Transfer: pick up stmt defs that fire before any
                // terminator-position read. The Return point reads
                // *after* stmts but the terminator itself is the
                // return - so `init_out` for a Return block sees
                // stmt defs from this block.
                let new_out = new_in || stmt_defs[i][t];
                if new_in != init_in[i][t] || new_out != init_out[i][t] {
                    init_in[i][t] = new_in;
                    init_out[i][t] = new_out;
                    changed = true;
                }
            }
        }
    }

    // For each block, `out[B][t]` is the must-init bit at the
    // *point of return*. Return blocks read `init_out[B]` (defs in
    // this block's stmts count); non-Return blocks see the value
    // they would have at the terminator boundary, which callers
    // ignore - the drop pass only consults Return blocks.
    for i in 0..n_blocks {
        out[i].clone_from(&init_out[i]);
    }
    out
}

pub(crate) fn block_successors(t: &Terminator) -> Vec<BlockId> {
    match t {
        Terminator::Goto { target } => vec![*target],
        Terminator::SwitchInt { arms, default, .. } => {
            let mut out: Vec<BlockId> = arms.iter().map(|(_, b)| *b).collect();
            out.push(*default);
            out
        }
        Terminator::Call { target, .. } => target.iter().copied().collect(),
        Terminator::Assert { target, .. } | Terminator::Drop { target, .. } => vec![*target],
        Terminator::Return
        | Terminator::Unreachable
        | Terminator::Resume
        | Terminator::Panic { .. } => Vec::new(),
    }
}

/// Hoists loop-carried release-before-reassign pairs to the value's
/// last mention in the previous iteration.
///
/// `insert_rc_releases` anchors the release of a reassigned local's
/// OLD value to the reassignment itself. In the ubiquitous loop shape
///
/// ```text
/// loop { tree = build(d); use(&tree) }
/// ```
///
/// the reassignment sits AFTER the next value has been built, so the
/// old and new structures coexist - for binary-trees-style workloads
/// that doubles transient RSS. This pass walks back from each
/// `release(x); x = Copy(tmp)` pair through the unique-predecessor
/// chain to x's last mention, and inserts `release(x); x = null`
/// right after it. The original release stays as a null-safe
/// backstop (releasing null is a no-op), so a missed hoist can only
/// keep the old timing - never double-free.
pub(crate) fn hoist_loop_carried_releases(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let n_locals = body.locals.len();
    let n_blocks = body.blocks.len();
    if n_blocks == 0 {
        return;
    }
    let is_rc = |l: Local| -> bool {
        let i = l.0 as usize;
        i < n_locals && tcx.is_rc_managed(body.locals[i].ty) && !body.locals[i].region
    };
    // Predecessor map (multi-pred blocks stop the backward walk).
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n_blocks];
    for (bi, block) in body.blocks.iter().enumerate() {
        let mut add = |t: &BlockId| preds[t.0 as usize].push(bi);
        match &block.terminator {
            Terminator::Goto { target } => add(target),
            Terminator::SwitchInt { arms, default, .. } => {
                for (_, t) in arms {
                    add(t);
                }
                add(default);
            }
            Terminator::Call {
                target: Some(t), ..
            } => add(t),
            Terminator::Assert { target, .. } | Terminator::Drop { target, .. } => add(target),
            _ => {}
        }
    }
    // Successor map for the forward-liveness safety check below.
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| match &b.terminator {
            Terminator::Goto { target } => vec![target.0 as usize],
            Terminator::SwitchInt { arms, default, .. } => {
                let mut v: Vec<usize> = arms.iter().map(|(_, t)| t.0 as usize).collect();
                v.push(default.0 as usize);
                v
            }
            Terminator::Call { target, .. } => target.iter().map(|t| t.0 as usize).collect(),
            Terminator::Assert { target, .. } => vec![target.0 as usize],
            Terminator::Drop { target, .. } => vec![target.0 as usize],
            _ => Vec::new(),
        })
        .collect();

    // The release-side accounting names whose args are not value READS.
    let accounting_release = |name: &str| -> bool {
        matches!(
            name,
            "gos_rt_rc_release"
                | "gos_rt_rc_weak_release"
                | "gos_rt_aggr_release_children"
                | "gos_rt_aggr_zero_guarded"
                | "gos_rt_option_slot_release"
        )
    };
    // True when the statement READS local x (writes excepted; the
    // backstop release of x itself excepted).
    let stmt_mentions = |stmt: &Statement, x: Local| -> bool {
        let StatementKind::Assign { place, rvalue } = &stmt.kind else {
            return false;
        };
        if !place.projection.is_empty() && place.local == x {
            return true;
        }
        let in_op = |op: &Operand| matches!(op, Operand::Copy(p) if p.local == x);
        match rvalue {
            Rvalue::Use(op) => in_op(op),
            Rvalue::BinaryOp { lhs, rhs, .. } => in_op(lhs) || in_op(rhs),
            Rvalue::UnaryOp { operand, .. } | Rvalue::Cast { operand, .. } => in_op(operand),
            Rvalue::Aggregate { operands, .. } => operands.iter().any(in_op),
            Rvalue::Repeat { value, .. } => in_op(value),
            Rvalue::Ref { place: rp, .. } => rp.local == x,
            Rvalue::Len(p) => p.local == x,
            Rvalue::CallIntrinsic { name, args } => {
                if accounting_release(name) {
                    false
                } else {
                    args.iter().any(in_op)
                }
            }
            // Reads a scalar global by symbol; mentions no local.
            Rvalue::StaticLoad(_) => false,
        }
    };
    let stmt_writes = |stmt: &Statement, x: Local| -> bool {
        matches!(&stmt.kind, StatementKind::Assign { place, .. }
            if place.projection.is_empty() && place.local == x)
    };
    let term_mentions = |t: &Terminator, x: Local| -> bool {
        let in_op = |op: &Operand| matches!(op, Operand::Copy(p) if p.local == x);
        match t {
            Terminator::SwitchInt { discriminant, .. } => in_op(discriminant),
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                in_op(callee)
                    || args.iter().any(in_op)
                    || (!destination.projection.is_empty() && destination.local == x)
            }
            Terminator::Assert { cond, msg, .. } => in_op(cond) || msg.operands().any(in_op),
            _ => false,
        }
    };
    let term_writes = |t: &Terminator, x: Local| -> bool {
        matches!(t, Terminator::Call { destination, .. }
            if destination.projection.is_empty() && destination.local == x)
    };

    // Collect the hoists: (target block, insert-after stmt index or
    // None for "after terminator-mention is unsupported"), the local.
    struct Hoist {
        at_block: usize,
        after_stmt: usize,
        local: Local,
    }
    // A borrowed local is pinned: the reference names its slot and outlives
    // the statement that took it, so the local's last direct mention is not
    // where its value stops being read. The same rule
    // [`insert_early_releases`] applies for the same reason.
    let mut borrowed: Vec<bool> = vec![false; n_locals];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign {
                rvalue: Rvalue::Ref { place, .. },
                ..
            } = &stmt.kind
                && (place.local.0 as usize) < n_locals
            {
                borrowed[place.local.0 as usize] = true;
            }
        }
    }
    let mut hoists: Vec<Hoist> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for si in 0..block.stmts.len().saturating_sub(1) {
            // Pattern: release(x) immediately followed by x = Copy(_).
            let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } = &block.stmts[si].kind
            else {
                continue;
            };
            if *name != "gos_rt_rc_release" {
                continue;
            }
            let Some(Operand::Copy(xp)) = args.first() else {
                continue;
            };
            if !xp.projection.is_empty() {
                continue;
            }
            let x = xp.local;
            if !is_rc(x) || borrowed[x.0 as usize] {
                continue;
            }
            let reassign = matches!(&block.stmts[si + 1].kind,
                StatementKind::Assign { place, rvalue }
                    if place.projection.is_empty()
                        && place.local == x
                        && matches!(rvalue, Rvalue::Use(Operand::Copy(_))));
            if !reassign {
                continue;
            }
            // Walk backward to x's last mention, through unique-pred
            // edges, without crossing a write to x or another release
            // of x (an existing earlier release means this one is
            // already a backstop).
            let mut cur = bi;
            let mut start = si; // exclusive upper bound within cur
            let mut found: Option<(usize, usize)> = None;
            let mut steps = 0;
            'walk: loop {
                let blk = &body.blocks[cur];
                for sj in (0..start).rev() {
                    let st = &blk.stmts[sj];
                    if let StatementKind::Assign {
                        rvalue: Rvalue::CallIntrinsic { name, args },
                        ..
                    } = &st.kind
                        && *name == "gos_rt_rc_release"
                        && matches!(args.first(), Some(Operand::Copy(p)) if p.local == x)
                    {
                        // Already released earlier on this path.
                        break 'walk;
                    }
                    if stmt_writes(st, x) {
                        break 'walk;
                    }
                    if stmt_mentions(st, x) {
                        found = Some((cur, sj));
                        break 'walk;
                    }
                }
                steps += 1;
                if steps > 64 {
                    break;
                }
                // At a join (e.g. a loop head: entry edge + back edge),
                // follow the back edge - the highest-numbered
                // predecessor, i.e. the loop body's bottom. This is
                // sound because the original release stays in place as
                // a null-safe backstop: paths that bypass the hoisted
                // release (the loop-entry edge) release the old value
                // exactly where they always did, and every block on
                // the walked segment has been verified mention-free in
                // full, so no path through it can read the nulled
                // local.
                let Some(&p) = preds[cur].iter().max() else {
                    break;
                };
                if p == cur {
                    break;
                }
                let pterm = &body.blocks[p].terminator;
                if term_writes(pterm, x) {
                    break;
                }
                if term_mentions(pterm, x) {
                    // Terminator-position mention (e.g. a call arg):
                    // inserting after a terminator means a successor
                    // head, and `cur`'s head IS that point - but only
                    // when the mention is the unique pred's terminator
                    // and x is not its destination. Insert at the head
                    // of `cur`.
                    found = Some((cur, usize::MAX));
                    break;
                }
                cur = p;
                start = body.blocks[p].stmts.len();
            }
            let Some((mb, ms)) = found else {
                continue;
            };
            // Hoisting to the immediate predecessor position of the
            // original release is a no-op; skip. (`usize::MAX` is the
            // head-of-block sentinel for terminator mentions - always
            // a real hoist, and `+ 1` on it would overflow.)
            if mb == bi && ms != usize::MAX && ms + 1 >= si {
                continue;
            }
            // Forward-liveness guard. The hoisted release NULLS `x`, so it
            // is only sound when `x` is dead from the insertion point until
            // its next write on EVERY path - not just the single back-edge
            // path the walk above verified. With a branch inside the loop
            // body (e.g. a group-match `for` loop that reads the key in one
            // arm and pushes it in another), `x` is read again past the
            // chosen mention; nulling it there frees a still-live value.
            // Walk forward from the insertion point; skip the hoist if any
            // path reads `x` before rewriting it.
            let start_stmt = if ms == usize::MAX { 0 } else { ms + 1 };
            let mut live = false;
            {
                let mut stack: Vec<(usize, usize)> = vec![(mb, start_stmt)];
                let mut visited_from0 = vec![false; n_blocks];
                'fwd: while let Some((b, from)) = stack.pop() {
                    let blk = &body.blocks[b];
                    let mut killed = false;
                    for sj in from..blk.stmts.len() {
                        let st = &blk.stmts[sj];
                        if stmt_mentions(st, x) {
                            live = true;
                            break 'fwd;
                        }
                        if stmt_writes(st, x) {
                            killed = true;
                            break;
                        }
                    }
                    if killed {
                        continue;
                    }
                    if term_mentions(&blk.terminator, x) {
                        live = true;
                        break 'fwd;
                    }
                    // A terminator call whose destination is `x` reissues it
                    // on return: the old value is dead past this block.
                    if term_writes(&blk.terminator, x) {
                        continue;
                    }
                    for &s in &succs[b] {
                        if !visited_from0[s] {
                            visited_from0[s] = true;
                            stack.push((s, 0));
                        }
                    }
                }
            }
            if live {
                continue;
            }
            hoists.push(Hoist {
                at_block: mb,
                after_stmt: ms,
                local: x,
            });
        }
    }
    if hoists.is_empty() {
        return;
    }

    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut next_local = body.locals.len();
    // Descending insertion order keeps earlier indices valid.
    // A head insertion uses the sentinel MAX, but its actual position is zero.
    // Insert it last so it cannot shift another release's original index.
    hoists.sort_by_key(|h| {
        let at = if h.after_stmt == usize::MAX {
            0
        } else {
            h.after_stmt + 1
        };
        std::cmp::Reverse((h.at_block, at))
    });
    for h in hoists {
        let span = body.blocks[h.at_block].span;
        let rel_dest = Local(u32::try_from(next_local).expect("local overflow"));
        next_local += 1;
        let release = Statement {
            kind: StatementKind::Assign {
                place: Place::local(rel_dest),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_rc_release",
                    args: vec![Operand::Copy(Place::local(h.local))],
                },
            },
            span,
            inlined: None,
        };
        let null_out = Statement {
            kind: StatementKind::Assign {
                place: Place::local(h.local),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            },
            span,
            inlined: None,
        };
        let at = if h.after_stmt == usize::MAX {
            0
        } else {
            h.after_stmt + 1
        };
        body.blocks[h.at_block].stmts.insert(at, null_out);
        body.blocks[h.at_block].stmts.insert(at, release);
    }
    for _ in body.locals.len()..next_local {
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }
}

/// Names of bodies that borrow every `json::Value` parameter they take.
///
/// A `gos_rt_json_*` entry reads the tree its handle views and mints a fresh
/// handle for anything it answers, so a parameter that reaches nothing else
/// cannot leave the call inside the result. `gos_rt_json_identity` is the one
/// entry that answers its own argument, so it is not one of those reads.
///
/// The callers of `option::and_then` / `result::map` and their siblings pass
/// the payload to one of these bodies, which is what lets the carrier holding
/// it be reclaimed after the call.
pub(crate) fn collect_json_borrowing_fns(
    bodies: &[Body],
    tcx: &gossamer_types::TyCtxt,
) -> std::collections::HashSet<String> {
    use gossamer_types::TyKind;
    let mut out = std::collections::HashSet::new();
    for body in bodies {
        let arity = body.arity as usize;
        let json_params: Vec<usize> = (1..=arity)
            .filter(|&i| {
                body.locals
                    .get(i)
                    .is_some_and(|l| matches!(tcx.kind_of(l.ty), TyKind::JsonValue))
            })
            .collect();
        if json_params.is_empty() {
            continue;
        }
        let escaped = std::cell::Cell::new(false);
        let mut escapes = |p: &Place| {
            if json_params.contains(&(p.local.0 as usize)) {
                escaped.set(true);
            }
        };
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    for_each_stmt_place(&stmt.kind, &mut escapes);
                    continue;
                };
                if json_params.contains(&(place.local.0 as usize)) {
                    escaped.set(true);
                }
                match rvalue {
                    Rvalue::CallIntrinsic { name, args } if json_entry_borrows(name) => {
                        let _ = args;
                    }
                    _ => for_each_rvalue_place(rvalue, &mut escapes),
                }
            }
            match &block.terminator {
                Terminator::Call { callee, args, .. } => {
                    let borrows = matches!(
                        callee,
                        Operand::Const(ConstValue::Str(n)) if json_entry_borrows(n)
                    );
                    if !borrows {
                        for a in args {
                            if let Operand::Copy(p) = a {
                                escapes(p);
                            }
                        }
                    }
                }
                Terminator::SwitchInt {
                    discriminant: Operand::Copy(p),
                    ..
                } => escapes(p),
                _ => {}
            }
        }
        if !escaped.get() {
            out.insert(body.name.clone());
        }
    }
    out
}
