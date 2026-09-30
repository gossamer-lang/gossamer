//! Table and response carriers, and their payload kinds.

use super::*;

/// Runtime calls that only READ the `http::Response` they are given: the box
/// they are handed does not outlive the call and is not what they answer.
///
/// Reclaiming a response is safe exactly where the frame can see its whole
/// life, and a call outside this set can hand the box on - `with_header`
/// answers the very same pointer - so a response that reaches one is left to
/// whoever ends up holding it.
pub(super) fn reads_response_only(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_http_response_status"
            | "gos_rt_http_response_body"
            | "gos_rt_http_response_raw_bytes"
            | "gos_rt_http_response_headers"
            | "gos_rt_http_response_get_header"
            | "gos_rt_http_response_content_type"
            | "gos_rt_http_response_location"
            | "gos_rt_http_response_free"
    )
}

/// Which arms of a carrier type hold a `HashMap` word, as
/// `gos_rt_carrier_own_map` reads them: `(ok, err)`. `None` for anything that
/// is not an `Option` / `Result` over a map.
pub(super) fn carrier_map_arms(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<(bool, bool)> {
    use gossamer_types::TyKind;
    let TyKind::Adt { def, substs } = tcx.kind_of(ty) else {
        return None;
    };
    if def.local != u32::MAX && def.local != u32::MAX - 1 {
        return None;
    }
    let tys = substs.types();
    let is_map = |arm: Option<&gossamer_types::Ty>| {
        arm.is_some_and(|t| matches!(tcx.kind_of(*t), TyKind::HashMap { .. }))
    };
    let arms = (is_map(tys.first()), is_map(tys.get(1)));
    (arms.0 || arms.1).then_some(arms)
}

/// Whether a map answered by a call of `name` is the caller's own table.
pub(super) fn answers_owned_map(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_map_new"
            | "gos_rt_map_new_with_capacity"
            | "gos_rt_map_new_with_capacity_typed"
            | "gos_rt_map_clone"
            | "gos_rt_map_window"
            | "Map::new"
            | "collections::Map::new"
            | "HashMap::new"
            | "collections::HashMap::new"
            | "BTreeMap::new"
            | "collections::BTreeMap::new"
    ) || name.starts_with("gos_rt_map_range_")
        || takes_element_out(name)
        || name.starts_with("gos_rt_chan_recv")
        || name.starts_with("gos_rt_chan_try_recv")
}

/// Whether `name` answers, in a carrier, an element it took out of its
/// container, which no longer holds it: a pop from a `Vec`, a deque (and so a
/// `Queue` or `Stack`), or an ordered map, or a `Vec` remove.
pub(super) fn takes_element_out(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_vec_pop_opt"
            | "gos_rt_vec_remove_safe"
            | "gos_rt_deque_pop_front"
            | "gos_rt_deque_pop_back"
    ) || name.starts_with("gos_rt_map_pop")
}

/// The `gos_rt_result_payload_release` kind of a table payload - `5` a `Map`,
/// `6` a `Set`, `7` a `Deque` / `Queue` / `Stack` - or `None` for any other
/// type. Tables are not counted: a carrier holding one owns it outright.
pub(crate) fn table_payload_kind(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<i64> {
    match tcx.kind_of(ty) {
        gossamer_types::TyKind::HashMap { .. } => Some(5),
        _ => match handle_container(tcx, ty) {
            Some(HandleContainer::Set) => Some(6),
            Some(HandleContainer::Deque) => Some(7),
            Some(HandleContainer::Heap) | None => None,
        },
    }
}

/// Whether a carrier payload kind names a table (see [`table_payload_kind`]).
pub(super) const fn is_table_kind(kind: i64) -> bool {
    matches!(kind, 5..=7)
}

/// The free a table payload's binding owes, or `None` for a non-table type.
pub(super) fn table_free(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<&'static str> {
    match table_payload_kind(tcx, ty)? {
        5 => Some("gos_rt_map_free"),
        6 => Some("gos_rt_set_free"),
        _ => Some("gos_rt_deque_free"),
    }
}

/// Whether a call's answer is a carrier whose table the caller owns: a
/// Gossamer function normalises the carrier it answers (see
/// [`own_returned_map_payloads`]), and a receive, pop, or remove hands over a
/// table its source no longer holds.
pub(super) fn answers_owned_table_carrier(callee: &Operand) -> bool {
    match callee {
        Operand::FnRef { .. } => true,
        Operand::Const(ConstValue::Str(name)) => {
            takes_element_out(name)
                || name.starts_with("gos_rt_chan_recv")
                || name.starts_with("gos_rt_chan_try_recv")
        }
        _ => false,
    }
}

/// Runtime entries that answer their carrier argument's table arm unchanged in
/// a carrier of their own: `opt.ok_or(e)` and `opt.ok_or_else(f)`.
pub(super) fn passes_table_through(name: &str) -> bool {
    matches!(name, "gos_rt_result_ok_or" | "gos_rt_result_ok_or_else")
}

/// Runtime entries that read a table carrier and keep nothing of it:
/// `unwrap_or` on an `Option<Map>` answers a map of the caller's own.
pub(super) fn reads_table_carrier(name: &str) -> bool {
    name == "gos_rt_result_unwrap_or_map"
}

/// Runtime entries that answer a table carrier's payload whole, which the
/// binding they define then owns.
pub(super) fn moves_table_carrier(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_option_unwrap"
            | "gos_rt_result_unwrap"
            | "gos_rt_option_expect"
            | "gos_rt_result_expect"
    )
}

/// A carrier a function answers owns the map it holds, so the frame that
/// receives it is the one that frees it. A payload this body did not build -
/// a map read out of another, or one lent to it as a parameter - is replaced
/// by a table of its own before the return, and a payload it did build is
/// handed over as it stands.
pub(crate) fn own_returned_map_payloads(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    if body.locals.is_empty() {
        return;
    }
    let Some((ok_is_map, err_is_map)) = carrier_map_arms(tcx, body.locals[0].ty) else {
        return;
    };
    let n_locals = body.locals.len();
    let arity = body.arity as usize;
    // The locals a copy chain lets the return slot stand for, so a carrier
    // built in one local and returned through another is read at its source.
    let mut sources: Vec<u32> = vec![Local::RETURN.0];
    let mut seen: std::collections::HashSet<u32> = sources.iter().copied().collect();
    let mut index = 0;
    while index < sources.len() {
        let current = sources[index];
        index += 1;
        for stmt in body.blocks.iter().flat_map(|b| &b.stmts) {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
                && place.projection.is_empty()
                && src.projection.is_empty()
                && place.local.0 == current
                && seen.insert(src.local.0)
            {
                sources.push(src.local.0);
            }
        }
    }
    // A map local the body itself built, reached through the same copy edges.
    let owned_map = |local: Local| -> bool {
        let mut reach: Vec<u32> = vec![local.0];
        let mut visited: std::collections::HashSet<u32> = reach.iter().copied().collect();
        let mut at = 0;
        while at < reach.len() {
            let current = reach[at];
            at += 1;
            if (current as usize) <= arity {
                return false;
            }
            let mut built = false;
            for block in &body.blocks {
                for stmt in &block.stmts {
                    if let StatementKind::Assign { place, rvalue } = &stmt.kind
                        && place.projection.is_empty()
                        && place.local.0 == current
                    {
                        match rvalue {
                            Rvalue::Use(Operand::Copy(src)) if src.projection.is_empty() => {
                                if visited.insert(src.local.0) {
                                    reach.push(src.local.0);
                                }
                                built = true;
                            }
                            _ => return false,
                        }
                    }
                }
                if let Terminator::Call {
                    callee,
                    destination,
                    ..
                } = &block.terminator
                    && destination.projection.is_empty()
                    && destination.local.0 == current
                {
                    match callee {
                        Operand::FnRef { .. } => built = true,
                        Operand::Const(ConstValue::Str(name)) if answers_owned_map(name) => {
                            built = true;
                        }
                        _ => return false,
                    }
                }
            }
            if !built {
                return false;
            }
        }
        true
    };
    let mut normalise = false;
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if !place.projection.is_empty() || !seen.contains(&place.local.0) {
                continue;
            }
            match rvalue {
                // The carrier the body builds around a map it owns hands that
                // map over; one built around any other map is a view of it.
                Rvalue::Aggregate { operands, .. }
                | Rvalue::CallIntrinsic {
                    name: "gos_rt_result_new",
                    args: operands,
                } => {
                    for operand in operands {
                        if let Operand::Copy(p) = operand
                            && p.projection.is_empty()
                            && (p.local.0 as usize) < n_locals
                            && matches!(
                                tcx.kind_of(body.locals[p.local.0 as usize].ty),
                                gossamer_types::TyKind::HashMap { .. }
                            )
                            && !owned_map(p.local)
                        {
                            normalise = true;
                        }
                    }
                }
                Rvalue::Use(Operand::Copy(_)) => {}
                _ => normalise = true,
            }
        }
        if let Terminator::Call {
            callee,
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && seen.contains(&destination.local.0)
            && !matches!(callee, Operand::FnRef { .. })
            && !matches!(
                callee,
                Operand::Const(ConstValue::Str(name)) if answers_owned_map(name)
            )
        {
            normalise = true;
        }
    }
    if !normalise {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let _ = unit_ty;
    for block_idx in 0..body.blocks.len() {
        if !matches!(body.blocks[block_idx].terminator, Terminator::Return) {
            continue;
        }
        let span = body.blocks[block_idx].span;
        body.blocks[block_idx].stmts.push(Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local::RETURN),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_carrier_own_map",
                    args: vec![
                        Operand::Copy(Place::local(Local::RETURN)),
                        Operand::Const(ConstValue::Int(i128::from(ok_is_map))),
                        Operand::Const(ConstValue::Int(i128::from(err_is_map))),
                    ],
                },
            },
            span,
            inlined: None,
        });
    }
}

pub(crate) fn insert_drops_at_returns(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;

    if body.locals.is_empty() {
        return;
    }
    // Balanced share for a Vec element pushed into a vec: the container's
    // element teardown (`gos_rt_vec_free`'s VEC element kind) releases one
    // share per slot, so the push must mint the container's own share
    // here while the frame keeps its per-site/at-return free - correct on
    // every path, including a conditional push that never runs.
    {
        let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
        let mut retains: Vec<(usize, Local)> = Vec::new();
        for (bi, block) in body.blocks.iter().enumerate() {
            if let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                ..
            } = &block.terminator
                && is_element_push(name)
                && let Some(Operand::Copy(p)) = args.get(1)
                && p.projection.is_empty()
                && (p.local.0 as usize) < body.locals.len()
                && matches!(
                    tcx.kind_of(body.locals[p.local.0 as usize].ty),
                    TyKind::Vec(_) | TyKind::Slice(_)
                )
                && !body.locals[p.local.0 as usize].region
            {
                retains.push((bi, p.local));
            }
        }
        for (bi, l) in retains {
            let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            let span = body.blocks[bi].span;
            body.blocks[bi].stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest),
                    rvalue: Rvalue::CallIntrinsic {
                        name: "gos_rt_vec_retain",
                        args: vec![Operand::Copy(Place::local(l))],
                    },
                },
                span,
                inlined: None,
            });
        }
    }
    // Per-local: the constructor symbol that allocated it (if
    // any). `None` means the local was either never assigned, was
    // assigned by something other than a recognised constructor,
    // or has been disqualified by a subsequent re-assignment.
    let mut owner_ctor: Vec<Option<&'static str>> = vec![None; body.locals.len()];
    let mut moved_into_return: Vec<bool> = vec![false; body.locals.len()];
    // An iterator a caller passes by value is this frame's from then on: the
    // caller gives up its owner record at the call, so what this frame
    // neither returns nor hands on is released here.
    for (i, slot) in owner_ctor
        .iter_mut()
        .enumerate()
        .skip(1)
        .take(body.arity as usize)
    {
        if let TyKind::Iterator(item) = tcx.kind_of(body.locals[i].ty) {
            *slot = Some(if lazy_iter_is_pair_state(tcx, *item) {
                "gos_rt_lazy_iter_drop_pair_i64"
            } else {
                "gos_rt_lazy_iter_drop_i64"
            });
        }
    }

    // Drop-before-overwrite sites for aggregate-typed locals. Each
    // entry `(block_idx, stmt_idx, local, size_bytes)` means
    // "insert `gos_rt_aggr_free(local, size)` before block
    // `block_idx`'s statement at index `stmt_idx`". The null check
    // inside `gos_rt_aggr_free` makes this a no-op on the first
    // assignment (the local holds 0/null pre-init) and reclaims
    // the previous allocation on every subsequent assignment
    // - closing the loop-body aggregate-leak case.
    let mut drop_before_sites: Vec<(usize, usize, Local, i64)> = Vec::new();

    // A value of `Iterator<(i64, i64)>` is the two-word pair state and every
    // other iterator the one-word state, whichever call built it.
    let iterator_free = |ty: Ty, _callee: &Operand| -> Option<&'static str> {
        let TyKind::Iterator(item) = tcx.kind_of(ty) else {
            return None;
        };
        if lazy_iter_is_pair_state(tcx, *item) {
            Some("gos_rt_lazy_iter_drop_pair_i64")
        } else {
            Some("gos_rt_lazy_iter_drop_i64")
        }
    };

    let ctor_to_free = |name: &str| -> Option<&'static str> {
        match name {
            // Runtime-symbol form (used by some peephole sites).
            "gos_rt_map_new" | "gos_rt_map_new_with_capacity" => Some("gos_rt_map_free"),
            "gos_rt_vec_new"
            | "gos_rt_vec_with_capacity"
            | "gos_rt_vec_repeat_primitive"
            | "gos_rt_bheap_max_new_i64"
            | "gos_rt_bheap_max_from_vec_i64"
            | "gos_rt_bheap_min_new_i64"
            | "gos_rt_bheap_min_from_vec_i64"
            | "gos_rt_bheap_new_typed"
            | "gos_rt_bheap_max_from_vec_desc"
            | "gos_rt_bheap_min_from_vec_desc" => Some("gos_rt_vec_free"),
            // Always returns a freshly allocated vec the frame owns,
            // whatever the destination's inferred type (a cloned borrowed
            // row lands in a Slice-typed local the type-based inference
            // below does not cover).
            "gos_rt_vec_clone" => Some("gos_rt_vec_free"),
            // A binding taken from a container copies its storage, so the
            // copy is the frame's to reclaim exactly as a constructed one is.
            // A queue and a stack share the deque header, so they share its
            // reclamation too.
            "gos_rt_set_clone" => Some("gos_rt_set_free"),
            "gos_rt_map_clone" => Some("gos_rt_map_free"),
            "gos_rt_deque_clone" | "gos_rt_queue_clone" | "gos_rt_stack_clone" => {
                Some("gos_rt_deque_free")
            }
            "gos_rt_set_new"
            | "gos_rt_btree_set_new"
            | "gos_rt_set_union"
            | "gos_rt_set_intersection"
            | "gos_rt_set_intersection_skey"
            | "gos_rt_set_difference"
            | "gos_rt_set_symmetric_difference" => Some("gos_rt_set_free"),
            // A `http::Response` is a box the runtime owns; the server's
            // reclaim after writing a handler's answer is the same call, so a
            // response that never leaves the frame that built it is reclaimed
            // exactly once here instead of outliving the process.
            "gos_rt_http_response_text_new"
            | "gos_rt_http_response_json_new"
            | "gos_rt_http_response_stream_new" => Some("gos_rt_http_response_free"),
            // Iterator over a Vec - the destination local is typed as
            // the source Vec so the `.next()` dispatch can recover the
            // element type. Without this entry the type-based
            // `inferred_free` path would schedule `gos_rt_vec_free` on
            // a `*mut GosArrIter`, mis-interpreting its bytes as a
            // `GosVec` header and corrupting the heap on free.
            "gos_rt_arr_iter" => Some("gos_rt_arr_iter_free"),
            // Path-form constructors emitted by the call lowerer.
            // The cranelift backend's `lower_intrinsic_call` table
            // routes these straight to the runtime helper, so the
            // drop pass needs to recognise both forms.
            "Map::new"
            | "collections::Map::new"
            | "HashMap::new"
            | "collections::HashMap::new"
            | "Map::with_capacity"
            | "collections::Map::with_capacity"
            | "HashMap::with_capacity"
            | "collections::HashMap::with_capacity"
            | "BTreeMap::new"
            | "collections::BTreeMap::new" => Some("gos_rt_map_free"),
            "Vec::new" | "Vec::with_capacity" => Some("gos_rt_vec_free"),
            "Set::new"
            | "collections::Set::new"
            | "HashSet::new"
            | "collections::HashSet::new"
            | "BTreeSet::new"
            | "collections::BTreeSet::new" => Some("gos_rt_set_free"),
            "gos_rt_deque_new"
            | "gos_rt_deque_new_typed"
            | "gos_rt_deque_from_vec"
            | "Deque::new"
            | "collections::Deque::new"
            | "VecDeque::new"
            | "collections::VecDeque::new"
            | "gos_rt_deque_from_vec_i64"
            | "gos_rt_queue_new"
            | "Queue::new"
            | "collections::Queue::new"
            | "VecQueue::new"
            | "collections::VecQueue::new"
            | "gos_rt_queue_from_vec_i64"
            | "gos_rt_stack_new"
            | "Stack::new"
            | "collections::Stack::new"
            | "VecStack::new"
            | "collections::VecStack::new"
            | "gos_rt_stack_from_vec_i64" => Some("gos_rt_deque_free"),
            _ => None,
        }
    };

    let arity = body.arity as usize;
    let last_block = body.blocks.len();

    // A carrier this frame's own answers a map the frame owns: a channel
    // receive hands over a table of its own, and so does a Gossamer call,
    // whose return normalises the carrier it answers (see
    // `own_returned_map_payloads`). A copy of such a carrier carries that
    // with it.
    let n_all = body.locals.len();
    let mut owned_carrier = vec![false; n_all];
    for block in &body.blocks {
        if let Terminator::Call {
            callee,
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < n_all
            && answers_owned_table_carrier(callee)
        {
            owned_carrier[destination.local.0 as usize] = true;
        }
    }
    loop {
        let mut changed = false;
        for stmt in body.blocks.iter().flat_map(|b| &b.stmts) {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
                && place.projection.is_empty()
                && src.projection.is_empty()
                && (place.local.0 as usize) < n_all
                && (src.local.0 as usize) < n_all
                && owned_carrier[src.local.0 as usize]
                && !owned_carrier[place.local.0 as usize]
            {
                owned_carrier[place.local.0 as usize] = true;
                changed = true;
            }
        }
        for block in &body.blocks {
            if let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                destination,
                ..
            } = &block.terminator
                && passes_table_through(name)
                && destination.projection.is_empty()
                && (destination.local.0 as usize) < n_all
                && !owned_carrier[destination.local.0 as usize]
                && matches!(args.first(), Some(Operand::Copy(src))
                    if src.projection.is_empty()
                        && (src.local.0 as usize) < n_all
                        && owned_carrier[src.local.0 as usize])
            {
                owned_carrier[destination.local.0 as usize] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // A table read out of a carrier this frame owns is this frame's to free.
    let takes_owned_table = |rvalue: &Rvalue, dest: usize| -> Option<&'static str> {
        let free = table_free(tcx, body.locals[dest].ty)?;
        matches!(
            rvalue,
            Rvalue::CallIntrinsic { name: "gos_rt_result_payload", args }
                if matches!(
                    args.first(),
                    Some(Operand::Copy(c))
                        if c.projection.is_empty()
                            && (c.local.0 as usize) < n_all
                            && owned_carrier[c.local.0 as usize]
                )
        )
        .then_some(free)
    };

    // Pass 1: discover constructor-allocated locals. Track every
    // assignment that *might* invalidate ownership (re-assignment,
    // projection writes) so we can disqualify aliasing patterns.
    //
    // Also disqualifies any local passed as a Copy arg to a Call
    // whose callee may capture its arguments (any user FnRef, or a
    // named runtime helper outside the non-capturing whitelist).
    // Without this disqualification, the drop pass would free a
    // container whose pointer is now retained inside the callee
    // (e.g. `flag::parse(os::args())` slurps the args vec; freeing
    // the args vec after the call orphans the parsed `rest`
    // strings).
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind {
                let idx = place.local.0 as usize;
                if !place.projection.is_empty() {
                    // Writing through a projection on this local
                    // doesn't move ownership, so it stays valid.
                    continue;
                }
                if idx == 0 || idx <= arity || idx >= owner_ctor.len() {
                    continue;
                }
                // note: `Rvalue::Aggregate` /
                // `Rvalue::Repeat` are NOT tracked here. The LLVM
                // backend (used by `gos build`) lowers aggregates
                // to stack slots that die with the function frame
                // - no leak. The Cranelift backend (used by the
                // in-process JIT for `gos`) routes them through
                // `gos_rt_aggr_alloc`, which lives in the
                // process-wide registry; long-running JIT bodies
                // can call `gos_rt_gc_reset` at safepoints to
                // reclaim. Emitting `gos_rt_aggr_free` here would
                // double-free the stack slot under LLVM, which is
                // the default backend.
                // Re-assignment of an owning local - disqualify.
                if owner_ctor[idx].is_some() && !matches!(rvalue, Rvalue::CallIntrinsic { .. }) {
                    owner_ctor[idx] = None;
                }
                if owner_ctor[idx].is_none()
                    && let Some(free) = takes_owned_table(rvalue, idx)
                {
                    owner_ctor[idx] = Some(free);
                }
            }
        }
        if let Terminator::Call {
            callee,
            destination,
            args,
            ..
        } = &block.terminator
        {
            let idx = destination.local.0 as usize;
            if idx == 0 || idx <= arity || idx >= owner_ctor.len() {
                continue;
            }
            if !destination.projection.is_empty() {
                continue;
            }
            // Any local of a heap-container type that's the
            // destination of a Call also owns the result - the
            // callee returned a freshly-allocated container that
            // this frame must drop unless it's then moved into
            // the return slot. Match by static type, since the
            // callee name ("count_kmers", arbitrary user fn)
            // doesn't telegraph ownership.
            //
            // A handful of runtime callees return *borrowed*
            // pointers - `gos_rt_os_args` hands back the global
            // `ARGS_VEC` sentinel that lives for the whole
            // process; passing it to `gos_rt_vec_free` aborts in
            // `__libc_free` on the next-pointer probe. Skip the
            // inferred_free assignment for those.
            let dest_ty = body.locals[idx].ty;
            // A map unwrapped out of a carrier is the payload that carrier
            // holds: the frame owns it only when it owns the carrier, and a
            // map answered by `get` on a map of maps is the outer map's.
            let unwraps_lent_map = matches!(tcx.kind_of(dest_ty), TyKind::HashMap { .. })
                && matches!(
                    callee,
                    Operand::Const(ConstValue::Str(s))
                        if matches!(
                            s.as_str(),
                            "gos_rt_option_unwrap" | "gos_rt_result_unwrap" | "gos_rt_option_expect"
                                | "gos_rt_result_expect"
                        )
                )
                && !matches!(
                    args.first(),
                    Some(Operand::Copy(c))
                        if c.projection.is_empty()
                            && (c.local.0 as usize) < n_all
                            && owned_carrier[c.local.0 as usize]
                );
            // A `Set` or deque unwrapped out of a carrier this frame owns is
            // handed over whole, as a map is.
            let unwrapped_table = match callee {
                Operand::Const(ConstValue::Str(s)) if moves_table_carrier(s) => {
                    match args.first() {
                        Some(Operand::Copy(c))
                            if c.projection.is_empty()
                                && (c.local.0 as usize) < n_all
                                && owned_carrier[c.local.0 as usize] =>
                        {
                            table_free(tcx, dest_ty)
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            let borrowed_callee = unwraps_lent_map
                || matches!(
                    callee,
                    Operand::Const(ConstValue::Str(s))
                        if returns_borrowed_pointer(s.as_str())
                );
            let inferred_free: Option<&'static str> = if borrowed_callee {
                None
            } else {
                // A Gossamer function answering `[T]` - a slice parameter it
                // returns, or the runtime-length carrier of a const generic
                // `[T; N]` - hands the caller a share of its own.
                let gossamer_callee = match callee {
                    Operand::FnRef { .. } => true,
                    Operand::Const(ConstValue::Str(s)) => {
                        !s.starts_with("gos_rt_") && !s.starts_with("__")
                    }
                    _ => false,
                };
                match tcx.kind_of(dest_ty) {
                    TyKind::HashMap { .. } => Some("gos_rt_map_free"),
                    TyKind::Vec(_) => Some("gos_rt_vec_free"),
                    TyKind::Slice(_) if gossamer_callee => Some("gos_rt_vec_free"),
                    _ if unwrapped_table.is_some() => unwrapped_table,
                    _ => iterator_free(dest_ty, callee),
                }
            };
            // A constructor the program declares shares its spelling with a
            // standard one (`Stack::new`) but builds a value of its own type.
            let program_value = matches!(
                tcx.kind_of(dest_ty),
                TyKind::Adt { def, .. } if def.local < u32::MAX - 64
            );
            if let Operand::Const(ConstValue::Str(name)) = callee
                && !program_value
            {
                if let Some(free) = ctor_to_free(name.as_str()) {
                    if owner_ctor[idx].is_none() {
                        owner_ctor[idx] = Some(free);
                        continue;
                    }
                }
            }
            if let Some(free) = inferred_free {
                if owner_ctor[idx].is_none() {
                    owner_ctor[idx] = Some(free);
                    continue;
                }
            }
            // when a Call returns an aggregate
            // (Adt / Tuple / Array) into a local, queue a
            // drop-before-overwrite of the prior value at the end
            // of this block (just before the Call terminator
            // runs). On the first execution the local holds 0/null
            // and `gos_rt_aggr_free` no-ops via its null check; on
            // every subsequent execution (loop reuse, repeated
            // call) the prior allocation is reclaimed instead of
            // leaked. The end-of-scope drop continues to handle
            // the final allocation at function return.
            let dest_is_aggregate = matches!(
                tcx.kind_of(dest_ty),
                TyKind::Adt { .. } | TyKind::Tuple(_) | TyKind::Array { .. }
            );
            // note: Call destinations of aggregate
            // type are not tracked here. See the matching comment in
            // the stmt-loop above - LLVM uses stack slots, Cranelift
            // JIT uses tracked heap allocs reclaimable via
            // `gos_rt_gc_reset` at safepoints.
            let _ = dest_is_aggregate;
            // Any other Call destination invalidates ownership
            // (the local now holds something else).
            owner_ctor[idx] = None;
        }
    }

    // A response the frame hands to anything but a reader may be the value
    // that call answers, so the frame can no longer see where the box ends up
    // and leaves the reclaim to whoever does.
    for block in &body.blocks {
        let Terminator::Call { callee, args, .. } = &block.terminator else {
            continue;
        };
        let reader = matches!(
            callee,
            Operand::Const(ConstValue::Str(name)) if reads_response_only(name.as_str())
        );
        if reader {
            continue;
        }
        for arg in args {
            let Operand::Copy(place) = arg else {
                continue;
            };
            let idx = place.local.0 as usize;
            if place.projection.is_empty()
                && idx < owner_ctor.len()
                && owner_ctor[idx] == Some("gos_rt_http_response_free")
            {
                owner_ctor[idx] = None;
            }
        }
    }

    // Heap slots that own what is written into them: an `gos_rc_alloc`
    // result carries a child descriptor, so its release reclaims the
    // children and a store into it needs no aliasing suppression.
    let owning_rc_slots: std::collections::HashSet<Local> = {
        let mut slots = std::collections::HashSet::new();
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::CallIntrinsic { name, .. },
                } = &stmt.kind
                    && matches!(*name, "gos_rc_alloc" | "gos_rc_alloc_tagged")
                    && place.projection.is_empty()
                {
                    slots.insert(place.local);
                }
            }
        }
        slots
    };

    // Aliasing summary: a local that is the source of a bare `Copy`, or
    // the value element (arg1..) of a consuming container/channel/closure
    // call, may outlive this frame, so the per-iteration reuse free must
    // not reclaim it. Computed once here and shared by the move-transfer
    // below and the reuse filter further down.
    let mut aliased = {
        let mut aliased = vec![false; body.locals.len()];
        // A write into an aggregate's own field is balanced the way a store
        // into an owning heap slot is: the by-value-aggregate pass mints the
        // field's share at the write and gives it back at the field's death,
        // so the source local stays reclaimable by its own frame. Without
        // this, a container written into a field reaches only the at-return
        // drop, so a frame that fills such a field in a loop keeps every
        // buffer but the last.
        let writes_owned_field = |place: &Place| -> bool {
            let Some(&crate::ir::Projection::Field(idx)) = place.projection.first() else {
                return false;
            };
            place.projection.len() == 1
                && body.locals.get(place.local.0 as usize).is_some_and(|decl| {
                    aggregate_rc_field_paths(tcx, decl.ty)
                        .iter()
                        .any(|(path, kind)| {
                            (matches!(kind, FieldRcKind::Vec) || kind.is_value_container())
                                && path.as_slice() == [idx]
                        })
                })
        };
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place: dest,
                    rvalue: Rvalue::Use(Operand::Copy(p)),
                } = &stmt.kind
                    && p.projection.is_empty()
                    && (p.local.0 as usize) < aliased.len()
                    && !writes_owned_field(dest)
                {
                    aliased[p.local.0 as usize] = true;
                }
                // `gos_store(slot, offset, value)` writes `value` into a heap
                // slot the frame cannot see through, so the per-iteration
                // reuse free must not reclaim what it points at - unless the
                // slot belongs to an object that owns its children. An RC
                // allocation carries a descriptor whose release walks those
                // slots, so its store is balanced and the source local goes
                // back to being reclaimed by its own frame.
                if let StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, args },
                    ..
                } = &stmt.kind
                    && *name == "gos_store"
                    && let Some(Operand::Copy(p)) = args.get(2)
                    && p.projection.is_empty()
                    && (p.local.0 as usize) < aliased.len()
                    && !args
                        .first()
                        .and_then(|slot| match slot {
                            Operand::Copy(place) if place.projection.is_empty() => {
                                Some(place.local)
                            }
                            _ => None,
                        })
                        .is_some_and(|slot| owning_rc_slots.contains(&slot))
                {
                    aliased[p.local.0 as usize] = true;
                }
            }
            if let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                ..
            } = &block.terminator
                && is_consuming_call(name)
            {
                for arg in args.iter().skip(1) {
                    if let Operand::Copy(p) = arg
                        && p.projection.is_empty()
                        && (p.local.0 as usize) < aliased.len()
                    {
                        // A `Vec` stored into a container is BALANCED (a
                        // retain minted by the container hands it its own
                        // share, freed by the container's element or value
                        // teardown), so the frame's per-site reuse of the
                        // stored local stays sound and load-bearing.
                        if stores_owned_vec_value(name)
                            && (matches!(
                                tcx.kind_of(body.locals[p.local.0 as usize].ty),
                                TyKind::Vec(_) | TyKind::Slice(_)
                            ) || handle_container(tcx, body.locals[p.local.0 as usize].ty)
                                == Some(HandleContainer::Heap))
                        {
                            continue;
                        }
                        // A map that owns `Map` / `Set` values stores a copy of
                        // the one it is handed, so the frame keeps its own.
                        if stores_table_value_copy(tcx, body, name, args, p.local) {
                            continue;
                        }
                        aliased[p.local.0 as usize] = true;
                    }
                }
            }
        }
        aliased
    };

    // Move-transfer: a bare `dst = Copy(src)` that consumes a
    // constructor-owned container (`Vec` / `HashMap`) for the last time
    // hands its allocation to `dst`. Pass 1's reassignment rule
    // disqualified `dst` (it is written by a plain copy, not a
    // constructor) and marked `src` aliased, dropping both onto the
    // conservative return-only free - so `let mut v = ...; while ... { v =
    // make() }` leaks every prior buffer. Transferring `src`'s free to
    // `dst` (and clearing `src`) lets the null-safe per-site reuse
    // machinery below free `dst`'s previous value before each overwrite
    // and its final value at return; `src` is never freed (its allocation
    // now lives in `dst`).
    //
    // The transfer fires only when `src` is a live `Vec`/`Map` owner
    // consumed exactly once (this copy, so it is dead afterwards -
    // counting every operand appearance keeps that conservative) and
    // `dst` is not itself aliased into a surviving holder (which would let
    // the per-iteration free dangle the alias). `dst` then lands in
    // `reuse`, and each transferred copy is recorded as a stmt-position
    // drop-before-overwrite site.
    fn bump_place_read(reads: &mut [u32], p: &Place) {
        let i = p.local.0 as usize;
        if i < reads.len() {
            reads[i] = reads[i].saturating_add(1);
        }
    }
    fn bump_op_read(reads: &mut [u32], op: &Operand) {
        if let Operand::Copy(p) = op {
            bump_place_read(reads, p);
        }
    }
    // A freshly-owned container handed back by a call: a `Vec<T>` /
    // `[T]` (`Slice`) / `HashMap` Call-destination whose callee is not a
    // borrowed-pointer returner. These are the same heap allocation at
    // runtime (`rc_helper` routes `Vec`/`Slice` to `gos_rt_vec_free`), so
    // when one is CONSUMED EXACTLY ONCE by a bare copy the move-transfer
    // may hand its ownership to the copy target. Unlike `inferred_free`
    // this is NOT folded into `owner_ctor` globally: a `Slice` result read
    // more than once stays a non-owner (the pre-existing conservative
    // leak), because the move-based drop pass cannot safely give an
    // aliased, non-refcounted container two owners (double-free).
    let fresh_container_free: Vec<Option<&'static str>> = {
        let mut fresh = vec![None; body.locals.len()];
        for block in &body.blocks {
            if let Terminator::Call {
                callee,
                destination,
                ..
            } = &block.terminator
                && destination.projection.is_empty()
                && (destination.local.0 as usize) < fresh.len()
            {
                let borrowed = matches!(
                    callee,
                    Operand::Const(ConstValue::Str(s)) if returns_borrowed_pointer(s.as_str())
                );
                if !borrowed {
                    // A constructor the reclaim table names answers its own
                    // free whatever the destination's type says: an opaque
                    // runtime handle carries no container type to read it from.
                    let named = match callee {
                        Operand::Const(ConstValue::Str(s)) => ctor_to_free(s.as_str()),
                        _ => None,
                    };
                    fresh[destination.local.0 as usize] = named.or_else(|| {
                        match tcx.kind_of(body.locals[destination.local.0 as usize].ty) {
                            TyKind::HashMap { .. } => Some("gos_rt_map_free"),
                            TyKind::Vec(_) | TyKind::Slice(_) => Some("gos_rt_vec_free"),
                            _ => {
                                iterator_free(body.locals[destination.local.0 as usize].ty, callee)
                            }
                        }
                    });
                }
            }
        }
        fresh
    };

    let mut move_copy_sites: Vec<(usize, usize, Local)> = Vec::new();
    // `(block, dst, src, origin)` for each move: the origin is emptied right
    // after the copy, so it keeps a reclaim for every path that skips the move.
    let mut moved_sources: Vec<(usize, Local, Local, Local)> = Vec::new();
    {
        let mut consume_reads = vec![0u32; body.locals.len()];
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { rvalue, .. } = &stmt.kind {
                    match rvalue {
                        Rvalue::Use(op)
                        | Rvalue::UnaryOp { operand: op, .. }
                        | Rvalue::Cast { operand: op, .. }
                        | Rvalue::Repeat { value: op, .. } => {
                            bump_op_read(&mut consume_reads, op);
                        }
                        Rvalue::BinaryOp { lhs, rhs, .. } => {
                            bump_op_read(&mut consume_reads, lhs);
                            bump_op_read(&mut consume_reads, rhs);
                        }
                        Rvalue::Aggregate { operands, .. } => {
                            for op in operands {
                                bump_op_read(&mut consume_reads, op);
                            }
                        }
                        // Tagging lazy state records how its elements are
                        // owned on the state itself; it takes no share of the
                        // handle and keeps no pointer to it, so it does not
                        // stand between the state and the local a later move
                        // hands it to.
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_lazy_iter_set_elem_meta",
                            ..
                        } => {}
                        Rvalue::CallIntrinsic { args, .. } => {
                            for op in args {
                                bump_op_read(&mut consume_reads, op);
                            }
                        }
                        Rvalue::Len(p) | Rvalue::Ref { place: p, .. } => {
                            bump_place_read(&mut consume_reads, p);
                        }
                        Rvalue::StaticLoad(_) => {}
                    }
                }
            }
            match &block.terminator {
                Terminator::SwitchInt { discriminant, .. } => {
                    bump_op_read(&mut consume_reads, discriminant);
                }
                Terminator::Call { callee, args, .. } => {
                    bump_op_read(&mut consume_reads, callee);
                    // An in-place append writes through the container it is
                    // handed; it takes no share of it and keeps no pointer to
                    // it, so it does not stand between the container and the
                    // local a later move hands it to. Counting it would leave
                    // `let v = #[a, b]` inside a loop non-transferable, and
                    // the outer binding it is moved into would leak every
                    // prior buffer.
                    let in_place_container = matches!(
                        callee,
                        Operand::Const(ConstValue::Str(name))
                            if appends_through_container(name.as_str())
                                || borrows_vec_receiver(name.as_str())
                    );
                    for (idx, op) in args.iter().enumerate() {
                        if in_place_container && idx == 0 {
                            continue;
                        }
                        bump_op_read(&mut consume_reads, op);
                    }
                }
                Terminator::Assert { cond, msg, .. } => {
                    bump_op_read(&mut consume_reads, cond);
                    for op in msg.operands() {
                        bump_op_read(&mut consume_reads, op);
                    }
                }
                Terminator::Drop { place, .. } => bump_place_read(&mut consume_reads, place),
                _ => {}
            }
        }
        // The sole whole-local definition of each local, when it is a bare
        // copy of another local. `let t = make(); x = t` names one allocation
        // through `t`, and the transfer has to see past that hop: otherwise
        // the first copy is refused because `t` is copied onward and the
        // second because `t` never became an owner, so neither end frees and
        // every prior buffer is lost.
        let sole_copy_source: Vec<Option<usize>> = {
            let mut source: Vec<Option<usize>> = vec![None; body.locals.len()];
            let mut definitions = vec![0u32; body.locals.len()];
            for block in &body.blocks {
                for stmt in &block.stmts {
                    if let StatementKind::Assign { place, rvalue } = &stmt.kind
                        && place.projection.is_empty()
                        && (place.local.0 as usize) < source.len()
                        // A null store holds no allocation: it is the placeholder
                        // that keeps a release on an untaken path a no-op.
                        && !matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0))))
                    {
                        let i = place.local.0 as usize;
                        definitions[i] = definitions[i].saturating_add(1);
                        if let Rvalue::Use(Operand::Copy(from)) = rvalue
                            && from.projection.is_empty()
                        {
                            source[i] = Some(from.local.0 as usize);
                        }
                    }
                }
                if let Terminator::Call { destination, .. } = &block.terminator
                    && destination.projection.is_empty()
                    && (destination.local.0 as usize) < source.len()
                {
                    let i = destination.local.0 as usize;
                    definitions[i] = definitions[i].saturating_add(1);
                }
            }
            for i in 0..source.len() {
                if definitions[i] != 1 {
                    source[i] = None;
                }
            }
            source
        };
        // Walks back through pass-through hops to the local that owns the
        // allocation, answering it with the hops crossed. A hop qualifies
        // only when its one definition is the copy that brought the
        // allocation in and its one read is the copy that hands it on, so it
        // holds that allocation and nothing else, and clearing its ownership
        // record can strand nothing.
        fn resolve_origin(
            mut current: usize,
            owner_ctor: &[Option<&'static str>],
            fresh_container_free: &[Option<&'static str>],
            sole_copy_source: &[Option<usize>],
            consume_reads: &[u32],
            arity: usize,
        ) -> Option<(usize, Vec<usize>)> {
            let mut hops = Vec::new();
            for _ in 0..8 {
                if owner_ctor[current]
                    .or(fresh_container_free[current])
                    .is_some()
                {
                    return Some((current, hops));
                }
                let previous = sole_copy_source[current]?;
                if previous <= arity || previous >= owner_ctor.len() || consume_reads[current] != 1
                {
                    return None;
                }
                hops.push(current);
                current = previous;
            }
            None
        }
        // A move-transfer target must ALWAYS hold a value it owns, so its
        // drop-before-overwrite never frees a pointer another local owns.
        // `dst` qualifies only when every whole-local assignment to it
        // establishes ownership: a fresh container call-result, or a bare
        // copy of a fresh container consumed exactly once (itself
        // move-transferable). A plain alias-copy (`cur = h` where `h` is
        // read elsewhere too) disqualifies `dst` - freeing `cur`'s aliased
        // initial value would double-free `h`'s owner.
        let owning_copy = |src: &Place, site: (usize, usize)| -> bool {
            if !src.projection.is_empty() {
                return false;
            }
            let s = src.local.0 as usize;
            if s >= owner_ctor.len() {
                return false;
            }
            let Some((origin, hops)) = resolve_origin(
                s,
                &owner_ctor,
                &fresh_container_free,
                &sole_copy_source,
                &consume_reads,
                arity,
            ) else {
                return false;
            };
            owner_ctor[origin]
                .or(fresh_container_free[origin])
                .is_some_and(transferable_by_move)
                && consume_reads[origin] == 1
                && move_chain_is_last_use(body, site, s, origin, &hops)
        };
        // A rebound owner takes a share of what is copied into it, so a copy
        // into it never moves its source's.
        let mut dst_all_owning: Vec<bool> = rebound_vec_owners(body, tcx)
            .into_iter()
            .map(|r| !r)
            .collect();
        for (bi, block) in body.blocks.iter().enumerate() {
            for (si, stmt) in block.stmts.iter().enumerate() {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < dst_all_owning.len()
                {
                    let owning = matches!(
                        rvalue,
                        Rvalue::Use(Operand::Copy(src)) if owning_copy(src, (bi, si))
                    );
                    if !owning {
                        dst_all_owning[place.local.0 as usize] = false;
                    }
                }
            }
            if let Terminator::Call { destination, .. } = &block.terminator
                && destination.projection.is_empty()
                && (destination.local.0 as usize) < dst_all_owning.len()
                && fresh_container_free[destination.local.0 as usize].is_none()
            {
                dst_all_owning[destination.local.0 as usize] = false;
            }
        }

        for (bi, block) in body.blocks.iter().enumerate() {
            for (si, stmt) in block.stmts.iter().enumerate() {
                let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                } = &stmt.kind
                else {
                    continue;
                };
                if !place.projection.is_empty() || !src.projection.is_empty() {
                    continue;
                }
                let d = place.local.0 as usize;
                let s = src.local.0 as usize;
                if d == s
                    || d <= arity
                    || s <= arity
                    || d >= owner_ctor.len()
                    || s >= owner_ctor.len()
                {
                    continue;
                }
                if !dst_all_owning[d] {
                    continue;
                }
                // `src` is a live owner either recorded in `owner_ctor`
                // (a constructor / `Vec`-returning call) or a fresh
                // `Vec`/`Slice`/`Map` call-result (`fresh_container_free`,
                // which unlike `owner_ctor` also covers `Slice`), reached
                // through any number of pass-through copies.
                let Some((origin, hops)) = resolve_origin(
                    s,
                    &owner_ctor,
                    &fresh_container_free,
                    &sole_copy_source,
                    &consume_reads,
                    arity,
                ) else {
                    continue;
                };
                let Some(free) = owner_ctor[origin].or(fresh_container_free[origin]) else {
                    continue;
                };
                if !transferable_by_move(free) {
                    continue;
                }
                // `src` must be consumed exactly once (this copy) and `dst`
                // must not be aliased into another holder. `dst` may only
                // already own the same free (a prior constructor of the
                // same kind, disqualified by pass 1's reassignment rule).
                if consume_reads[origin] != 1 || aliased[d] {
                    continue;
                }
                if !move_chain_is_last_use(body, (bi, si), s, origin, &hops) {
                    continue;
                }
                if let Some(existing) = owner_ctor[d]
                    && existing != free
                {
                    continue;
                }
                owner_ctor[d] = Some(free);
                // The origin keeps its own reclaim. The copy is its only read,
                // so it is emptied right after it: the reclaim is a no-op on
                // the path that moved the value, and still frees the value on a
                // path that never reached the copy.
                aliased[origin] = false;
                for hop in hops {
                    owner_ctor[hop] = None;
                }
                move_copy_sites.push((bi, si, place.local));
                moved_sources.push((bi, place.local, src.local, Local(origin as u32)));
            }
        }
    }

    // Iterator values are unique lazy-runtime handles. Passing one by value to
    // another function transfers ownership to that callee, and the runtime
    // lazy helpers either embed it in a returned adapter or consume and drop
    // it. Clear this frame's owner record so return cleanup cannot free it
    // after the callee already consumed it.
    // An advance reads the state in place and leaves it with this frame.
    for block in &body.blocks {
        let Terminator::Call { callee, args, .. } = &block.terminator else {
            continue;
        };
        if let Operand::Const(ConstValue::Str(name)) = callee
            && matches!(
                name.as_str(),
                "gos_rt_lazy_iter_next_i64"
                    | "gos_rt_lazy_iter_next_f64"
                    | "gos_rt_lazy_iter_next_pair_i64"
            )
        {
            continue;
        }
        for arg in args {
            let Operand::Copy(place) = arg else {
                continue;
            };
            let idx = place.local.0 as usize;
            if place.projection.is_empty()
                && idx < owner_ctor.len()
                && matches!(tcx.kind_of(body.locals[idx].ty), TyKind::Iterator(_))
            {
                owner_ctor[idx] = None;
            }
        }
    }

    // A container handle stored into an aggregate's field belongs to that
    // aggregate from then on - it outlives the frame whenever the aggregate
    // does. Handles the field walk does not track (a `Map`, a `Set`, an
    // ordered container: no RC header, no field-death free) would otherwise
    // be freed at return while the field still names them. A `Vec` or an
    // RC field is tracked, retained at its store, and stays out of this.
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
            else {
                continue;
            };
            if place.projection.is_empty()
                || !place
                    .projection
                    .iter()
                    .all(|p| matches!(p, crate::ir::Projection::Field(_)))
                || !src.projection.is_empty()
            {
                continue;
            }
            let idx = src.local.0 as usize;
            if idx >= owner_ctor.len() {
                continue;
            }
            let ty = body.locals[idx].ty;
            let tracked = matches!(tcx.kind_of(ty), TyKind::Vec(_) | TyKind::Slice(_))
                || tcx.is_rc_managed(ty);
            if !tracked {
                owner_ctor[idx] = None;
            }
        }
    }

    // Pass 2: detect locals that *transitively* flow into the
    // return slot. The constructor result may be copied through a
    // chain of intermediate locals before landing in `Local::RETURN`
    // (e.g. `Local(0) = Local(4); Local(4) = Local(5);
    // Local(5) = HashMap::new()`). Any local in that chain
    // shares the same heap pointer and must not be dropped, since
    // `Local::RETURN` will be moved out to the caller.
    //
    // Build a "Copy edge" graph (`from` → `to` whenever
    // `Assign(to, Use(Copy(from)))` appears with bare projections),
    // then walk it backwards from `Local::RETURN` to its closure.
    let call_dest = call_destinations(body);
    let mut copy_edges_to: Vec<Vec<Local>> = vec![Vec::new(); body.locals.len()];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind {
                if !place.projection.is_empty() {
                    continue;
                }
                let to_idx = place.local.0 as usize;
                if to_idx >= copy_edges_to.len() {
                    continue;
                }
                match rvalue {
                    Rvalue::Use(Operand::Copy(p)) if p.projection.is_empty() => {
                        copy_edges_to[to_idx].push(p.local);
                    }
                    // An aggregate moves each `Copy` operand into
                    // the constructed value's storage. If the
                    // aggregate later flows to RETURN, every
                    // moved-in source local must skip its drop -
                    // its allocation is now owned by the caller via
                    // the returned aggregate. Without this edge,
                    // a `let v = Vec::new(); push(v, ...); Foo {
                    // ids: v }` body emits a `gos_rt_vec_free(v)`
                    // before Return, freeing storage that the
                    // returned struct's `ids` field still aliases -
                    // the caller's `f.ids.len()` then reads garbage.
                    // A slot that takes a container of its own leaves the
                    // source holding the one it built, so that source is not
                    // part of the returned value.
                    Rvalue::Aggregate { operands, .. } => {
                        for (idx, op) in operands.iter().enumerate() {
                            if let Operand::Copy(p) = op
                                && p.projection.is_empty()
                                && !aggregate_slot_takes_own_container(
                                    tcx, body, place, idx, p, &call_dest,
                                )
                            {
                                copy_edges_to[to_idx].push(p.local);
                            }
                        }
                    }
                    // `Ok(v)` / `Some(v)` puts the payload's word in the
                    // carrier, so a carrier that reaches the return slot takes
                    // the payload with it and the frame's own reclaim would
                    // free storage the caller is about to read.
                    Rvalue::CallIntrinsic { name, args } if *name == "gos_rt_result_new" => {
                        if let Some(Operand::Copy(p)) = args.get(1)
                            && p.projection.is_empty()
                        {
                            copy_edges_to[to_idx].push(p.local);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    let mut stack = vec![Local::RETURN];
    moved_into_return[Local::RETURN.0 as usize] = true;
    while let Some(cur) = stack.pop() {
        let cur_idx = cur.0 as usize;
        if cur_idx >= copy_edges_to.len() {
            continue;
        }
        for src in copy_edges_to[cur_idx].clone() {
            let src_idx = src.0 as usize;
            if src_idx >= moved_into_return.len() {
                continue;
            }
            if !moved_into_return[src_idx] {
                moved_into_return[src_idx] = true;
                stack.push(src);
            }
        }
    }
    // Enum-box locals (`gos_rc_alloc` / `gos_rc_alloc_tagged` results).
    // A Vec stored into one is BALANCED at the constructor - the store
    // retains the box's share and the box's kind-tagged meta entry frees
    // it on teardown - so the frame's own free stays load-bearing and the
    // `gos_store` moved-into-return rule below must not suppress it.
    let enum_box_locals: Vec<bool> = {
        let mut boxes = vec![false; body.locals.len()];
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::CallIntrinsic { name, .. },
                } = &stmt.kind
                    && matches!(*name, "gos_rc_alloc" | "gos_rc_alloc_tagged")
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < boxes.len()
                {
                    boxes[place.local.0 as usize] = true;
                }
            }
        }
        boxes
    };
    let is_container_local = |op: &Operand| -> bool {
        matches!(op, Operand::Copy(p) if p.projection.is_empty()
        && (p.local.0 as usize) < body.locals.len()
        && matches!(
            tcx.kind_of(body.locals[p.local.0 as usize].ty),
            gossamer_types::TyKind::Vec(_) | gossamer_types::TyKind::Slice(_)
        ))
    };

    // Calls whose destination flows into `Local::RETURN` move every
    // pointer-shaped Copy argument into the return value too. Tuple
    // construction in particular lowers as a synthesised
    // `__tuple(...)` Call - the Vec/aggregate operands are moved
    // into the constructed value, so they must skip their drop.
    // Iterate to a fixed point because a moved-in Call destination
    // can propagate the same closure backwards through more Copy
    // edges (the dest of an inner construct may feed an outer one).
    let mut changed = true;
    while changed {
        changed = false;
        // Helper: propagate "moved into return" through one Call's
        // arg list when its destination already flows there.
        // Used for both Terminator::Call and Rvalue::CallIntrinsic
        // (the result-ctor / aggregate-helper paths route through
        // the Rvalue form), so the same chain - Vec → struct
        // operand → gos_rt_result_new → Local::RETURN - is walked
        // back to the Vec and skips its drop.
        let propagate_call_args = |args: &[Operand], moved: &mut Vec<bool>, changed: &mut bool| {
            for arg in args {
                if let Operand::Copy(p) = arg
                    && p.projection.is_empty()
                {
                    let idx = p.local.0 as usize;
                    if idx < moved.len() && !moved[idx] {
                        moved[idx] = true;
                        *changed = true;
                        let mut stack = vec![Local(u32::try_from(idx).unwrap_or(0))];
                        while let Some(cur) = stack.pop() {
                            let cur_idx = cur.0 as usize;
                            if cur_idx >= copy_edges_to.len() {
                                continue;
                            }
                            for src in copy_edges_to[cur_idx].clone() {
                                let src_idx = src.0 as usize;
                                if src_idx < moved.len() && !moved[src_idx] {
                                    moved[src_idx] = true;
                                    *changed = true;
                                    stack.push(src);
                                }
                            }
                        }
                    }
                }
            }
        };
        for block in &body.blocks {
            // Rvalue-position calls (the `Ok(...)` /
            // result-ctor path uses `Rvalue::CallIntrinsic
            // { name: "gos_rt_result_new", args: [disc, payload] }`).
            // Without this arm, a `Vec` inside a struct that's
            // wrapped in `Result::Ok(R { xs: v })` was not
            // recognised as moved-into-return and the drop pass
            // freed it before the caller unwrapped, producing a
            // dangling Vec in the returned `Result`.
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                if let Rvalue::CallIntrinsic { name, args } = rvalue {
                    // `gos_store(obj, off, val)`: storing `val` into heap
                    // object `obj`. When `obj` escapes into the return
                    // value (a recursive-enum payload, e.g.
                    // `J::Arr(v)` stored as `gos_store(arr, 8, v)` then
                    // `return arr`), `val` escapes with it. Freeing `val`
                    // here would dangle the returned object's child
                    // pointer - exactly the `Vec`-in-enum crash.
                    if *name == "gos_store"
                        && let Some(Operand::Copy(obj_p)) = args.first()
                        && obj_p.projection.is_empty()
                    {
                        let obj_idx = obj_p.local.0 as usize;
                        if obj_idx < moved_into_return.len() && moved_into_return[obj_idx] {
                            if let Some(val) = args.get(2) {
                                // A Vec payload stored into an enum box is
                                // balanced (constructor retain + box-owned
                                // free through the kind-tagged meta), so
                                // the frame's own free stays; only
                                // non-container children escape with the
                                // returned box.
                                let balanced =
                                    enum_box_locals.get(obj_idx).copied().unwrap_or(false)
                                        && is_container_local(val);
                                if !balanced {
                                    propagate_call_args(
                                        std::slice::from_ref(val),
                                        &mut moved_into_return,
                                        &mut changed,
                                    );
                                }
                            }
                        }
                        continue;
                    }
                }
                if place.projection.is_empty()
                    && let Rvalue::CallIntrinsic { args, .. } = rvalue
                {
                    let dest_idx = place.local.0 as usize;
                    if dest_idx >= moved_into_return.len() || !moved_into_return[dest_idx] {
                        continue;
                    }
                    propagate_call_args(args, &mut moved_into_return, &mut changed);
                }
            }
            // `gos_rt_vec_push(container, elem)`: the element's heap
            // ownership moves into the container, which deep-frees its direct
            // elements on drop or carries them to the caller when returned -
            // either way an independent drop of the element here would
            // double-free / dangle. Mark the direct element unconditionally.
            // Done inside the fixpoint (not a separate pass) so a pushed enum's
            // own escaped children - `inner` in `outer.push(J::Arr(inner))`,
            // reached via the `gos_store` rule above - propagate through
            // arbitrarily deep nesting.
            //
            // The element's TRANSITIVE children only escape when the container
            // itself does. When the pushed element is a tuple aggregate
            // `(k, J::Map(inner))`, the nested enum box and the `inner` Vec it
            // owns reach the caller only if the container is returned; then
            // walking the copy-edge graph back from the element suppresses
            // their drops so they survive the escape. When the container is
            // freed locally its deep-free reclaims the direct tuple element but
            // does not recurse into the nested Vec's own elements, so those keep
            // their independent drops - suppressing them unconditionally would
            // leak. Gate the copy-edge walk on the container being
            // moved-into-return.
            if let Terminator::Call { callee, args, .. } = &block.terminator
                && let Operand::Const(ConstValue::Str(name)) = callee
                && is_element_push(name)
                && let Some(elem_op @ Operand::Copy(p)) = args.get(1)
                && p.projection.is_empty()
                && !is_container_local(elem_op)
                // A store that copies the element leaves the frame its own.
                && !stores_table_value_copy(tcx, body, name, args, p.local)
            {
                let idx = p.local.0 as usize;
                if idx < moved_into_return.len() && !moved_into_return[idx] {
                    moved_into_return[idx] = true;
                    changed = true;
                }
                if let Some(Operand::Copy(container)) = args.first()
                    && container.projection.is_empty()
                    && (container.local.0 as usize) < moved_into_return.len()
                    && moved_into_return[container.local.0 as usize]
                {
                    // Walk the copy-edge graph back from the element's children
                    // (a tuple aggregate's `Copy(enum_box)` operand), marking
                    // each transitively. Starting from `p.local` rather than
                    // calling `propagate_call_args` avoids its short-circuit on
                    // the already-marked element, which would stop before the
                    // enum box. Marking the enum box lets the fixpoint's
                    // `gos_enum_tag` / `gos_store` rules carry moved-ness on to
                    // the nested `inner` Vec.
                    let mut stack = vec![p.local];
                    while let Some(cur) = stack.pop() {
                        let cur_idx = cur.0 as usize;
                        if cur_idx >= copy_edges_to.len() {
                            continue;
                        }
                        for src in copy_edges_to[cur_idx].clone() {
                            let src_idx = src.0 as usize;
                            if src_idx < moved_into_return.len() && !moved_into_return[src_idx] {
                                moved_into_return[src_idx] = true;
                                changed = true;
                                stack.push(src);
                            }
                        }
                    }
                }
            }
            if let Terminator::Call {
                callee,
                destination,
                args,
                ..
            } = &block.terminator
            {
                if !destination.projection.is_empty() {
                    continue;
                }
                let dest_idx = destination.local.0 as usize;
                if dest_idx >= moved_into_return.len() || !moved_into_return[dest_idx] {
                    continue;
                }
                // Only aggregate-constructor callees actually move
                // their args into the destination value. Generic
                // Calls (println, str_concat, map_get_or, every
                // user fn) consume their args without retaining
                // them, so propagating "moved" through their args
                // would mark unrelated heap-owning locals as
                // moved-into-return and silently skip their drops.
                if !is_aggregate_ctor_callee(callee) {
                    continue;
                }
                propagate_call_args(args, &mut moved_into_return, &mut changed);
            }
        }
    }

    // A table the frame owns, wrapped for the last time into an `Ok` / `Some`
    // the frame answers, leaves with the answer on that path only. It keeps its
    // own reclaim and is emptied right after the wrap, so the reclaim frees it
    // on a path that answers something else and is a no-op on the one that
    // hands it over.
    let mut wrapped_tables: Vec<(usize, Local, Local)> = Vec::new();
    {
        let mut edge_count = vec![0usize; body.locals.len()];
        // Mentions as an operand of an intrinsic or an aggregate, any of which
        // could carry the table somewhere this walk does not follow.
        let mut held_in = vec![0usize; body.locals.len()];
        for stmt in body.blocks.iter().flat_map(|b| &b.stmts) {
            if let StatementKind::Assign {
                rvalue:
                    Rvalue::CallIntrinsic { args: operands, .. } | Rvalue::Aggregate { operands, .. },
                ..
            } = &stmt.kind
            {
                for op in operands {
                    if let Operand::Copy(p) = op
                        && let Some(count) = held_in.get_mut(p.local.0 as usize)
                    {
                        *count += 1;
                    }
                }
            }
        }
        for edges in &copy_edges_to {
            for src in edges {
                if let Some(count) = edge_count.get_mut(src.0 as usize) {
                    *count += 1;
                }
            }
        }
        for (bi, block) in body.blocks.iter().enumerate() {
            for (si, stmt) in block.stmts.iter().enumerate() {
                let StatementKind::Assign {
                    place,
                    rvalue:
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_new",
                            args,
                        },
                } = &stmt.kind
                else {
                    continue;
                };
                let Some(Operand::Copy(p)) = args.get(1) else {
                    continue;
                };
                let payload = p.local.0 as usize;
                if !place.projection.is_empty()
                    || !p.projection.is_empty()
                    || payload <= arity
                    || payload >= owner_ctor.len()
                    || !moved_into_return[place.local.0 as usize]
                    || edge_count[payload] != 1
                    || held_in[payload] != 1
                    || aliased[payload]
                    || !owner_ctor[payload].is_some_and(|free| {
                        matches!(
                            free,
                            "gos_rt_map_free"
                                | "gos_rt_set_free"
                                | "gos_rt_deque_free"
                                | "gos_rt_vec_free"
                        )
                    })
                    || !copy_is_last_use(body, (bi, si), p.local)
                {
                    continue;
                }
                moved_into_return[payload] = false;
                // A sequence is counted, and the wrap mints the answer's share
                // of it, so the frame's reclaim stays on every path. A table
                // has no count, so the one the answer takes is emptied here.
                if owner_ctor[payload] != Some("gos_rt_vec_free") {
                    wrapped_tables.push((bi, place.local, p.local));
                }
            }
        }
    }
    // (`gos_rt_vec_push` element-ownership transfer is handled inside
    // the fixpoint above so it composes with the `gos_store` rule for
    // arbitrarily deep enum/container nesting.)

    // A `HashMap` consumed as an operand of a struct/tuple `Rvalue::Aggregate`
    // is MOVED into that aggregate's field WHEN nothing mints a share for the
    // slot: ownership transfers and the aggregate's field-death release is the
    // only one, so freeing it here too would double-free. A slot the retain
    // pass gives a table of its own is the other side of that condition - the
    // frame still owns what it built and releases it here.
    //
    // A `Vec` / `[T]` operand is the other case and is deliberately absent:
    // the construction mints the field's share, so the frame keeps the release
    // of the sequence it built. Suppressing that release left the
    // construction's own share with nothing to return it, and a frame building
    // such a value in a loop held every buffer it ever built.
    let moved_into_aggregate = {
        let mut moved = vec![false; body.locals.len()];
        // A slot that takes a table of its own (the retain pass's aggregate
        // arm) leaves the frame the release of the one it built - the same
        // schedule a `Set`, a `Deque`, and a heap operand already run.
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Aggregate { operands, .. },
                } = &stmt.kind
                {
                    for (idx, op) in operands.iter().enumerate() {
                        if let Operand::Copy(p) = op
                            && p.projection.is_empty()
                            && (p.local.0 as usize) < moved.len()
                            && matches!(
                                tcx.kind_of(body.locals[p.local.0 as usize].ty),
                                TyKind::HashMap { .. }
                            )
                            && !aggregate_slot_takes_own_container(
                                tcx, body, place, idx, p, &call_dest,
                            )
                        {
                            moved[p.local.0 as usize] = true;
                        }
                    }
                }
            }
        }
        moved
    };

    let mints_own_share = aggregates_minting_their_own_share(body, tcx);

    // Pass 3: collect drop targets in stable local-index order.
    // The constructor-name → free-name table already restricts
    // candidates to runtime container shapes; we trust the MIR's
    // type assignment and skip a redundant TyKind check here.
    let _ = TyKind::Bool; // silence unused-import lint outside the closure
    // A store into a module global hands the container to a cell that outlives
    // every frame, so the frame that built it keeps no claim on it.
    let stored_in_static = {
        let mut stored = vec![false; body.locals.len()];
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::StaticStore {
                    value: Operand::Copy(p),
                    ..
                } = &stmt.kind
                    && p.projection.is_empty()
                    && (p.local.0 as usize) < stored.len()
                {
                    stored[p.local.0 as usize] = true;
                }
            }
        }
        stored
    };
    // A value container sent through a channel is the receiver's from then
    // on: its handle carries no count, so the send hands the table itself over
    // and the sender keeps no claim on it.
    let mut sent_away = vec![false; owner_ctor.len()];
    for block in &body.blocks {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } = &block.terminator
            && matches!(name.as_str(), "gos_rt_chan_send" | "gos_rt_chan_try_send")
            && let Some(Operand::Copy(value)) = args.get(1)
            && value.projection.is_empty()
            && (value.local.0 as usize) < sent_away.len()
        {
            sent_away[value.local.0 as usize] = true;
        }
    }
    let drop_targets_all: Vec<(Local, &'static str)> = (0..owner_ctor.len())
        .filter_map(|i| {
            let free = owner_ctor[i]?;
            if stored_in_static[i] {
                return None;
            }
            if sent_away[i]
                && matches!(
                    free,
                    "gos_rt_map_free" | "gos_rt_set_free" | "gos_rt_deque_free"
                )
            {
                return None;
            }
            if (moved_into_return[i] || moved_into_aggregate[i]) && !mints_own_share[i] {
                return None;
            }
            Some((Local(i as u32), free))
        })
        .collect();

    // Non-aliased container ctor locals get full per-site management below
    // (zero-init + drop-before-overwrite + at-return, all null-safe) so a
    // container rebuilt each loop iteration frees every prior allocation
    // instead of leaking all but the last. Aliased locals (the source of a
    // bare `Copy`) are left to the conservative return-only path - freeing one
    // before its reassignment could dangle the alias. Locals captured by a
    // call were already disqualified from `owner_ctor` in pass 1. `aliased`
    // was computed once after pass 1 and shared with the move-transfer.
    let reuse: Vec<(Local, &'static str)> = drop_targets_all
        .iter()
        .filter(|(l, free)| {
            // Every counted container reclaims per site, not only the two
            // that were named here: a `Set`, a `Deque`, a `Queue`, and a
            // `Stack` rebuilt each iteration reached the return-only path and
            // so freed all but the last. A `http::Response` box is the same
            // per-site shape: one built per turn of a loop is reclaimed on
            // each turn rather than growing the process by one response, and
            // so is the lazy state a loop over an adapter chain builds.
            // A parameter arrives holding the caller's value, so it is never
            // zero-initialised for per-site reclaim.
            !aliased[l.0 as usize]
                && l.0 as usize > body.arity as usize
                && matches!(
                    *free,
                    "gos_rt_vec_free"
                        | "gos_rt_map_free"
                        | "gos_rt_set_free"
                        | "gos_rt_deque_free"
                        | "gos_rt_http_response_free"
                        | "gos_rt_lazy_iter_drop_i64"
                        | "gos_rt_lazy_iter_drop_pair_i64"
                )
        })
        .copied()
        .collect();
    let reuse_set: std::collections::BTreeSet<u32> = reuse.iter().map(|(l, _)| l.0).collect();
    let drop_targets: Vec<(Local, &'static str)> = drop_targets_all
        .into_iter()
        .filter(|(l, _)| !reuse_set.contains(&l.0))
        .collect();

    if drop_targets.is_empty() && reuse.is_empty() {
        return;
    }

    // Per-target must-init dataflow. For each drop target `L`,
    // compute `init_at_return[L][R]` - `true` when every path from
    // entry to Return block `R` passes through at least one
    // definition of `L`. A definition is a Call terminator whose
    // destination is `L` or a stmt-position assignment to `L`.
    //
    // The earlier (type-only) pass scheduled a free at every
    // Return for every recognised owner local, including shapes
    // like `let m: HashMap<...>; if cond { m = HashMap::new() };
    // return m;` where the `else` branch reaches Return without
    // ever initialising `m`. Calling `gos_rt_map_free` on the
    // uninit slot aborts in the allocator metadata probe.
    //
    // Approach: minimal forward dataflow with intersection at
    // joins (the "must-init" lattice). Drops are emitted only at
    // Return blocks where the target is must-init at the point of
    // return; cases where the proof is undecidable (irreducible
    // CFG, complex loops) conservatively skip the drop - a leak
    // is preferable to a free of uninit memory.
    let init_at_return = compute_init_at_returns(body, &drop_targets);

    for block_idx in 0..last_block {
        if !matches!(body.blocks[block_idx].terminator, Terminator::Return) {
            continue;
        }
        let span = body.blocks[block_idx].span;
        let init_row = &init_at_return[block_idx];
        for (target_idx, (local, free_name)) in drop_targets.iter().enumerate() {
            if !init_row[target_idx] {
                continue;
            }
            let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
            body.locals.push(LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            // Emit the free as a CallIntrinsic stmt - the cranelift
            // lowerer's statement path handles it without any block
            // rewiring. `gos_rt_aggr_free` needs a second `size`
            // arg the codegen derives from the local's type; all
            // other helpers (Vec/Map/Set/...) are single-arg.
            // `gos_rt_aggr_free` takes 2 args (ptr + size); the
            // other heap-container free helpers take only the
            // receiver pointer.
            let args = if *free_name == "gos_rt_aggr_free" {
                let size = aggr_size_bytes(tcx, body.locals[local.0 as usize].ty);
                vec![
                    Operand::Copy(Place::local(*local)),
                    Operand::Const(ConstValue::Int(i128::from(size))),
                ]
            } else {
                vec![Operand::Copy(Place::local(*local))]
            };
            body.blocks[block_idx].stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest),
                    rvalue: Rvalue::CallIntrinsic {
                        name: free_name,
                        args,
                    },
                },
                span,
                inlined: None,
            });
        }
    }

    // drop-before-overwrite for aggregate
    // reassignments. Skip sites where the local is not provably
    // initialised on every path leading to this statement -
    // freeing an uninitialised aggregate local reads garbage from
    // the Cranelift Variable slot and aborts in `__libc_free`.
    //
    // For each candidate site, compute "is local must-init at
    // block entry?" via the same dataflow used by
    // `compute_init_at_returns`. Then walk the block statements
    // up to `stmt_idx`, updating must-init on each Assign to
    // this local. Drop is emitted only if must-init is true at
    // the point of the candidate stmt.
    let candidate_locals: Vec<Local> = drop_before_sites
        .iter()
        .map(|(_, _, l, _)| *l)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let init_at_each_return = if candidate_locals.is_empty() {
        Vec::new()
    } else {
        let targets: Vec<(Local, &'static str)> = candidate_locals
            .iter()
            .map(|l| (*l, "gos_rt_aggr_free"))
            .collect();
        compute_init_at_block_entries(body, &targets)
    };
    let local_to_target_idx: std::collections::BTreeMap<Local, usize> = candidate_locals
        .iter()
        .enumerate()
        .map(|(i, l)| (*l, i))
        .collect();
    let must_init_at = |block_idx: usize, stmt_idx: usize, local: Local| -> bool {
        let Some(target_idx) = local_to_target_idx.get(&local) else {
            return false;
        };
        if block_idx >= init_at_each_return.len() {
            return false;
        }
        let mut init = init_at_each_return[block_idx][*target_idx];
        // Walk stmts up to stmt_idx and update must-init based on
        // Assign destinations.
        for (i, stmt) in body.blocks[block_idx].stmts.iter().enumerate() {
            if i >= stmt_idx {
                break;
            }
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
                && place.local == local
            {
                init = true;
            }
        }
        init
    };
    drop_before_sites.sort_by_key(|a| (a.0, a.1));
    let drop_before_sites: Vec<_> = drop_before_sites
        .into_iter()
        .filter(|(b, s, l, _)| must_init_at(*b, *s, *l))
        .collect();
    for (block_idx, stmt_idx, local, size) in drop_before_sites.into_iter().rev() {
        if block_idx >= body.blocks.len() {
            continue;
        }
        let span = body.blocks[block_idx]
            .stmts
            .get(stmt_idx)
            .map_or(body.blocks[block_idx].span, |s| s.span);
        let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let drop_stmt = Statement {
            kind: StatementKind::Assign {
                place: Place::local(dest),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_aggr_free",
                    args: vec![
                        Operand::Copy(Place::local(local)),
                        Operand::Const(ConstValue::Int(i128::from(size))),
                    ],
                },
            },
            span,
            inlined: None,
        };
        body.blocks[block_idx].stmts.insert(stmt_idx, drop_stmt);
    }

    // Drop-before-overwrite at each move-transfer copy `dst = Copy(src)`:
    // free `dst`'s previous value before it is rebound, so a container
    // moved into an outer binding every loop iteration reclaims each prior
    // buffer. Null-safe on the first pass via the reuse zero-init below.
    // Restricted to `dst` locals that reached `reuse` (a non-aliased
    // Vec/Map owner not moved into the return slot); a `dst` moved into the
    // return is freed by the caller instead. Inserted in reverse
    // (block, stmt) order so earlier statement indices stay valid, and
    // before the reuse zero-init prepends at block 0.
    if !move_copy_sites.is_empty() {
        let mut sites: Vec<(usize, usize, Local, &'static str)> = move_copy_sites
            .iter()
            .filter_map(|&(bi, si, dst)| {
                reuse
                    .iter()
                    .find(|(l, _)| *l == dst)
                    .map(|(_, free)| (bi, si, dst, *free))
            })
            .collect();
        sites.sort_by_key(|&(bi, si, _, _)| (bi, si));
        let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
        for (block_idx, stmt_idx, local, free_name) in sites.into_iter().rev() {
            if block_idx >= body.blocks.len() || stmt_idx > body.blocks[block_idx].stmts.len() {
                continue;
            }
            let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            let span = body.blocks[block_idx]
                .stmts
                .get(stmt_idx)
                .map_or(body.blocks[block_idx].span, |s| s.span);
            body.blocks[block_idx].stmts.insert(
                stmt_idx,
                Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(dest),
                        rvalue: Rvalue::CallIntrinsic {
                            name: free_name,
                            args: vec![Operand::Copy(Place::local(local))],
                        },
                    },
                    span,
                    inlined: None,
                },
            );
        }
    }

    // Empty each table right after the wrap that hands it to the answer, found
    // by its content for the same reason as the moved origins below.
    for &(block_idx, carrier, table) in &wrapped_tables {
        let Some(block) = body.blocks.get_mut(block_idx) else {
            continue;
        };
        let Some(wrap_idx) = block.stmts.iter().position(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign {
                    place,
                    rvalue: Rvalue::CallIntrinsic { name: "gos_rt_result_new", args },
                } if place.projection.is_empty()
                    && place.local == carrier
                    && matches!(args.get(1), Some(Operand::Copy(p))
                        if p.projection.is_empty() && p.local == table)
            )
        }) else {
            continue;
        };
        let span = block.stmts[wrap_idx].span;
        block.stmts.insert(
            wrap_idx + 1,
            Statement {
                kind: StatementKind::Assign {
                    place: Place::local(table),
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                },
                span,
                inlined: None,
            },
        );
    }

    // Empty each moved origin right after the copy that moved it. The copy is
    // found by its content, since the insertions above shift statement indices.
    for &(block_idx, dst, src, origin) in &moved_sources {
        let Some(block) = body.blocks.get_mut(block_idx) else {
            continue;
        };
        let Some(copy_idx) = block.stmts.iter().position(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign { place, rvalue: Rvalue::Use(Operand::Copy(from)) }
                    if place.projection.is_empty()
                        && place.local == dst
                        && from.projection.is_empty()
                        && from.local == src
            )
        }) else {
            continue;
        };
        let span = block.stmts[copy_idx].span;
        block.stmts.insert(
            copy_idx + 1,
            Statement {
                kind: StatementKind::Assign {
                    place: Place::local(origin),
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                },
                span,
                inlined: None,
            },
        );
    }

    // Dedicated lifetime for non-aliased Vec/Map ctor locals: zero-init at
    // entry (null), free the previous value before each ctor-Call that
    // reassigns the local (loop reuse), and free the final value at every
    // Return. Every free is null-safe (`gos_rt_vec_free` / `gos_rt_map_free`
    // no-op on null), so this needs no path-sensitive must-init proof and never
    // double-frees: the drop-before frees prior allocations, the at-Return
    // frees the last one, and a never-constructed local stays null.
    if !reuse.is_empty() {
        let span0 = body.blocks[0].span;
        for (local, _) in reuse.iter().rev() {
            body.blocks[0].stmts.insert(
                0,
                Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(*local),
                        rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    },
                    span: span0,
                    inlined: None,
                },
            );
        }
        let free_of: std::collections::BTreeMap<u32, &'static str> =
            reuse.iter().map(|(l, f)| (l.0, *f)).collect();
        // (block_idx, free_name, local) - each appended to the block's stmts,
        // i.e. just before its terminator.
        let mut sites: Vec<(usize, &'static str, Local)> = Vec::new();
        for (block_idx, block) in body.blocks.iter().enumerate() {
            match &block.terminator {
                Terminator::Call {
                    destination, args, ..
                } if destination.projection.is_empty() => {
                    // A call that READS its own destination (`xs = f(xs)`)
                    // still needs the old value live when it runs, so the
                    // drop-before-overwrite is skipped there; the prior
                    // binding is reclaimed by the at-return free instead.
                    let self_read = args.iter().any(|a| {
                        matches!(a, Operand::Copy(p)
                            if p.projection.is_empty() && p.local == destination.local)
                    });
                    if !self_read && let Some(&free_name) = free_of.get(&destination.local.0) {
                        sites.push((block_idx, free_name, destination.local));
                    }
                }
                Terminator::Return => {
                    for (local, free_name) in &reuse {
                        sites.push((block_idx, *free_name, *local));
                    }
                }
                _ => {}
            }
        }
        // A map taken out of an owned carrier is defined by a statement, so
        // its previous value is freed just before that statement runs.
        let mut stmt_sites: Vec<(usize, usize, &'static str, Local)> = Vec::new();
        for (block_idx, block) in body.blocks.iter().enumerate() {
            for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
                if let StatementKind::Assign {
                    place,
                    rvalue:
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_payload",
                            ..
                        },
                } = &stmt.kind
                    && place.projection.is_empty()
                    && let Some(&free_name) = free_of.get(&place.local.0)
                {
                    stmt_sites.push((block_idx, stmt_idx, free_name, place.local));
                }
            }
        }
        let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
        for (block_idx, stmt_idx, free_name, local) in stmt_sites.into_iter().rev() {
            let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            let span = body.blocks[block_idx].span;
            body.blocks[block_idx].stmts.insert(
                stmt_idx,
                Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(dest),
                        rvalue: Rvalue::CallIntrinsic {
                            name: free_name,
                            args: vec![Operand::Copy(Place::local(local))],
                        },
                    },
                    span,
                    inlined: None,
                },
            );
        }
        for (block_idx, free_name, local) in sites {
            let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            let span = body.blocks[block_idx].span;
            body.blocks[block_idx].stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest),
                    rvalue: Rvalue::CallIntrinsic {
                        name: free_name,
                        args: vec![Operand::Copy(Place::local(local))],
                    },
                },
                span,
                inlined: None,
            });
        }
    }
}
