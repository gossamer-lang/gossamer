// ---------------------------------------------------------------------------
// Releases moved from the return block to where a local's region ends.
// ---------------------------------------------------------------------------

/// The release calls a frame books at its return for a local it gave a
/// counted value, each nulled the same way at entry.
fn is_exit_release_name(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_vec_free"
            | "gos_rt_rc_release"
            | "gos_rt_str_free_typed"
            | "gos_rt_str_free"
            | "gos_rt_option_slot_release"
            | "gos_rt_aggr_release_children"
    )
}

/// The local an exit release gives back, when its first argument is a place
/// rooted at a local through fields only.
fn exit_release_local(stmt: &Statement) -> Option<Local> {
    let StatementKind::Assign {
        place,
        rvalue: Rvalue::CallIntrinsic { name, args },
    } = &stmt.kind
    else {
        return None;
    };
    if !place.projection.is_empty() || !is_exit_release_name(name) {
        return None;
    }
    match args.first() {
        Some(Operand::Copy(p)) if fields_only(p) => Some(p.local),
        _ => None,
    }
}

/// Whether `stmt` sets `local`, or a field of it, to the null value the entry
/// block gives it before anything else runs.
fn nulls_local(stmt: &Statement, local: Local) -> bool {
    match &stmt.kind {
        StatementKind::Assign { place, rvalue } if place.local == local && fields_only(place) => {
            matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0))))
                || matches!(rvalue, Rvalue::CallIntrinsic { name: "gos_rt_result_new", args }
                    if matches!(args.as_slice(),
                        [Operand::Const(ConstValue::Int(1)), Operand::Const(ConstValue::Int(0))]))
        }
        StatementKind::Assign {
            rvalue: Rvalue::CallIntrinsic { name: "gos_rt_aggr_zero_guarded", args },
            ..
        } => matches!(args.first(), Some(Operand::Copy(p)) if p.local == local && p.projection.is_empty()),
        _ => false,
    }
}

/// Immediate dominators of the blocks reachable from the entry, by the
/// iterative algorithm over reverse postorder; `None` for an unreachable
/// block.
fn immediate_dominators(succs: &[Vec<usize>]) -> Vec<Option<usize>> {
    let n = succs.len();
    let mut order = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    if n > 0 {
        seen[0] = true;
        stack.push((0, 0));
    }
    while let Some((block, next)) = stack.pop() {
        if let Some(&succ) = succs[block].get(next) {
            stack.push((block, next + 1));
            if succ < n && !seen[succ] {
                seen[succ] = true;
                stack.push((succ, 0));
            }
        } else {
            order.push(block);
        }
    }
    order.reverse();
    let mut rpo_index = vec![usize::MAX; n];
    for (i, &b) in order.iter().enumerate() {
        rpo_index[b] = i;
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, ss) in succs.iter().enumerate() {
        for &s in ss {
            if s < n {
                preds[s].push(b);
            }
        }
    }
    let mut idom: Vec<Option<usize>> = vec![None; n];
    if n == 0 {
        return idom;
    }
    idom[0] = Some(0);
    let intersect = |idom: &[Option<usize>], mut a: usize, mut b: usize| {
        while a != b {
            while rpo_index[a] > rpo_index[b] {
                a = idom[a].unwrap_or(0);
            }
            while rpo_index[b] > rpo_index[a] {
                b = idom[b].unwrap_or(0);
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &b in order.iter().skip(1) {
            let mut new_idom: Option<usize> = None;
            for &p in &preds[b] {
                if idom[p].is_none() {
                    continue;
                }
                new_idom = Some(match new_idom {
                    None => p,
                    Some(current) => intersect(&idom, p, current),
                });
            }
            if new_idom.is_some() && idom[b] != new_idom {
                idom[b] = new_idom;
                changed = true;
            }
        }
    }
    idom
}

/// Whether `a` dominates `b` under `idom`.
fn dominates(idom: &[Option<usize>], a: usize, b: usize) -> bool {
    let mut at = b;
    loop {
        if at == a {
            return true;
        }
        match idom[at] {
            Some(up) if up != at => at = up,
            _ => return false,
        }
    }
}

/// The nearest block dominating both `a` and `b`.
fn common_dominator(idom: &[Option<usize>], a: usize, b: usize) -> usize {
    let mut at = a;
    while !dominates(idom, at, b) {
        match idom[at] {
            Some(up) if up != at => at = up,
            _ => return 0,
        }
    }
    at
}

/// Gives back a local where control leaves the blocks that use it, rather
/// than at the frame's return.
///
/// The drop schedule releases a local that holds a counted value at the
/// return, null-safe, after the entry block nulled it. A local bound in one
/// arm of a wide `match` is then released on every call, whichever arm ran:
/// a call per local, most of them on a null. When every use of a local lies
/// in the blocks one block dominates, the local is null on any path that
/// never entered them, so its release belongs on the edges that leave them.
/// Each such edge gives the value back and nulls the local again, the way
/// the entry did, so a loop that re-enters the blocks finds it empty and the
/// return-block release is dropped.
pub(crate) fn sink_exit_releases(body: &mut Body, tcx: &TyCtxt) {
    let returns: Vec<usize> = body
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| matches!(b.terminator, Terminator::Return))
        .map(|(i, _)| i)
        .collect();
    let [ret] = returns.as_slice() else {
        return;
    };
    let ret = *ret;
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();
    let idom = immediate_dominators(&succs);
    if idom[ret].is_none() {
        return;
    }
    // `(from, to)` edge -> statements to run on it.
    let mut edge_stmts: Vec<((usize, usize), Vec<Statement>)> = Vec::new();
    let mut removed: Vec<usize> = Vec::new();
    for (local, sites) in return_releases(body, ret) {
        if local.0 as usize <= body.arity as usize {
            continue;
        }
        let nullers: Vec<Statement> = body.blocks[0]
            .stmts
            .iter()
            .filter(|stmt| nulls_local(stmt, local))
            .cloned()
            .collect();
        if nullers.is_empty() || !entry_nulls_cover(body, ret, &sites, &nullers) {
            continue;
        }
        let Some(head) = region_head(body, tcx, &idom, local, ret, &sites) else {
            continue;
        };
        if head == 0 || dominates(&idom, head, ret) {
            continue;
        }
        let mut moved: Vec<Statement> = sites
            .iter()
            .map(|&si| body.blocks[ret].stmts[si].clone())
            .collect();
        moved.extend(nullers);
        for (from, targets) in succs.iter().enumerate() {
            if idom[from].is_none() || !dominates(&idom, head, from) {
                continue;
            }
            for &to in targets {
                if dominates(&idom, head, to) {
                    continue;
                }
                match edge_stmts.iter_mut().find(|(edge, _)| *edge == (from, to)) {
                    Some((_, stmts)) => stmts.extend(moved.iter().cloned()),
                    None => edge_stmts.push(((from, to), moved.clone())),
                }
            }
        }
        removed.extend(sites);
    }
    for si in removed {
        body.blocks[ret].stmts[si].kind = StatementKind::Nop;
    }
    place_edge_statements(body, edge_stmts);
}

/// The locals the return block releases, each with the positions of its
/// release statements there.
fn return_releases(body: &Body, ret: usize) -> Vec<(Local, Vec<usize>)> {
    let mut by_local: Vec<(Local, Vec<usize>)> = Vec::new();
    for (si, stmt) in body.blocks[ret].stmts.iter().enumerate() {
        if let Some(local) = exit_release_local(stmt) {
            match by_local.iter_mut().find(|(l, _)| *l == local) {
                Some((_, sites)) => sites.push(si),
                None => by_local.push((local, vec![si])),
            }
        }
    }
    by_local
}

/// Whether the entry block's `nullers` empty every part of the value the
/// release statements at `sites` give back, so a path that never set the
/// local releases nothing.
fn entry_nulls_cover(body: &Body, ret: usize, sites: &[usize], nullers: &[Statement]) -> bool {
    sites.iter().all(|&si| {
        let StatementKind::Assign {
            rvalue: Rvalue::CallIntrinsic { name, args },
            ..
        } = &body.blocks[ret].stmts[si].kind
        else {
            return false;
        };
        let Some(Operand::Copy(released)) = args.first() else {
            return false;
        };
        let walk = *name == "gos_rt_aggr_release_children";
        nullers.iter().any(|n| match &n.kind {
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name: "gos_rt_aggr_zero_guarded", .. },
                ..
            } => walk,
            StatementKind::Assign { place, .. } => {
                !walk && (place.projection.is_empty() || place.projection == released.projection)
            }
            _ => false,
        })
    })
}

/// The block that dominates every other mention of `local` and of the
/// locals its value reaches, or `None` when a reference to one is taken or
/// a mention sits in a block the entry cannot reach.
fn region_head(
    body: &Body,
    tcx: &TyCtxt,
    idom: &[Option<usize>],
    local: Local,
    ret: usize,
    sites: &[usize],
) -> Option<usize> {
    // The value may also be read through a local it was copied into, or one
    // built from it, so those locals' uses must lie in the region too.
    let holders = value_holders(body, tcx, local, ret, sites);
    let mut head: Option<usize> = None;
    let mut referenced = false;
    for (bi, block) in body.blocks.iter().enumerate() {
        let in_block = holders.iter().any(|&holder| {
            block.stmts.iter().enumerate().any(|(si, stmt)| {
                if holder == local && bi == ret && sites.contains(&si) {
                    return false;
                }
                if bi == 0 && nulls_local(stmt, holder) {
                    return false;
                }
                if let StatementKind::Assign {
                    rvalue: Rvalue::Ref { place, .. },
                    ..
                } = &stmt.kind
                    && place.local == holder
                {
                    referenced = true;
                }
                stmt_mentions_local(stmt, holder)
            }) || term_mentions_local(&block.terminator, holder)
                || matches!(&block.terminator, Terminator::Drop { place, .. } if place.local == holder)
        });
        if !in_block {
            continue;
        }
        idom[bi]?;
        head = Some(match head {
            None => bi,
            Some(h) => common_dominator(idom, h, bi),
        });
    }
    if referenced { None } else { head }
}

/// Runs each edge's statements where control crosses it.
fn place_edge_statements(body: &mut Body, edge_stmts: Vec<((usize, usize), Vec<Statement>)>) {
    for ((from, to), stmts) in edge_stmts {
        // An edge that is its block's only way out runs right after the
        // block's statements, so they join that block and the loop shapes
        // the bounds and overflow provers match - a latch ending in its
        // counter step - stay intact.
        if matches!(body.blocks[from].terminator, Terminator::Goto { .. }) {
            body.blocks[from].stmts.extend(stmts);
            continue;
        }
        let id = BlockId(u32::try_from(body.blocks.len()).expect("block overflow"));
        let span = body.blocks[from].span;
        body.blocks.push(BasicBlock {
            id,
            stmts,
            terminator: Terminator::Goto {
                target: BlockId(u32::try_from(to).expect("block overflow")),
            },
            span,
            terminator_span: None,
            terminator_inlined: None,
        });
        retarget_successor(&mut body.blocks[from].terminator, to, id);
    }
}

/// `local` and every local a value read out of it can reach: the
/// destination of any assignment or call whose operands mention a holder,
/// unless that destination holds a scalar, which carries no pointer.
fn value_holders(
    body: &Body,
    tcx: &TyCtxt,
    local: Local,
    ret: usize,
    sites: &[usize],
) -> Vec<Local> {
    let carries = |l: Local| {
        !matches!(
            body.locals.get(l.0 as usize).map(|d| tcx.kind_of(d.ty)),
            Some(
                TyKind::Int(_)
                    | TyKind::Float(_)
                    | TyKind::Bool
                    | TyKind::Char
                    | TyKind::Unit
                    | TyKind::Never
            )
        )
    };
    let mut holders = vec![local];
    let mut changed = true;
    while changed {
        changed = false;
        for (bi, block) in body.blocks.iter().enumerate() {
            for (si, stmt) in block.stmts.iter().enumerate() {
                if bi == ret && sites.contains(&si) {
                    continue;
                }
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                if holders.contains(&place.local) || !carries(place.local) {
                    continue;
                }
                if holders.iter().any(|&h| rvalue_mentions_local(rvalue, h)) {
                    holders.push(place.local);
                    changed = true;
                }
            }
            if let Terminator::Call {
                callee,
                args,
                destination,
                ..
            } = &block.terminator
                && !holders.contains(&destination.local)
                && carries(destination.local)
                && holders.iter().any(|&h| {
                    args.iter()
                        .chain(std::iter::once(callee))
                        .any(|op| matches!(op, Operand::Copy(p) if place_mentions_local(p, h)))
                })
            {
                holders.push(destination.local);
                changed = true;
            }
        }
    }
    holders
}

/// Points every edge of `t` that leads to block `to` at `new` instead.
fn retarget_successor(t: &mut Terminator, to: usize, new: BlockId) {
    let swap = |target: &mut BlockId| {
        if target.0 as usize == to {
            *target = new;
        }
    };
    match t {
        Terminator::Goto { target }
        | Terminator::Assert { target, .. }
        | Terminator::Drop { target, .. } => swap(target),
        Terminator::SwitchInt { arms, default, .. } => {
            for (_, target) in arms.iter_mut() {
                swap(target);
            }
            swap(default);
        }
        Terminator::Call { target, .. } => {
            if let Some(target) = target.as_mut() {
                swap(target);
            }
        }
        Terminator::Return | Terminator::Unreachable | Terminator::Resume | Terminator::Panic { .. } => {}
    }
}
