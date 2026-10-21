// ---------------------------------------------------------------------------
// Values a region made give their share back before the region's pop.
// ---------------------------------------------------------------------------

fn calls_runtime(t: &Terminator, name: &str) -> bool {
    matches!(t, Terminator::Call { callee: Operand::Const(ConstValue::Str(callee)), .. } if callee == name)
}

/// A release `stmt` books for the value at its first argument: the release's
/// name and arguments, when the argument is a place rooted at a local through
/// fields only. `gos_rt_vec_free_unless_returned` is left out, since what it
/// gives back depends on the answer the frame returns.
fn place_release(stmt: &Statement) -> Option<(&'static str, &[Operand], &Place)> {
    let StatementKind::Assign {
        place,
        rvalue: Rvalue::CallIntrinsic { name, args },
    } = &stmt.kind
    else {
        return None;
    };
    if !place.projection.is_empty()
        || !crate::uniqueness::is_release_intrinsic(name)
        || *name == "gos_rt_vec_free_unless_returned"
    {
        return None;
    }
    match args.first() {
        Some(Operand::Copy(p)) if fields_only(p) => Some((*name, args.as_slice(), p)),
        _ => None,
    }
}

/// The statement that empties what the release `name` with `args` gives
/// back, the way the entry block empties it. `sink` takes the answer of a
/// runtime call that does the emptying.
fn null_after_release(
    name: &str,
    args: &[Operand],
    released: &Place,
    sink: Local,
    span: Span,
) -> Statement {
    let kind = if name == "gos_rt_aggr_release_children" {
        StatementKind::Assign {
            place: Place::local(sink),
            rvalue: Rvalue::CallIntrinsic {
                name: "gos_rt_aggr_zero_guarded",
                args: args.to_vec(),
            },
        }
    } else {
        StatementKind::Assign {
            place: released.clone(),
            rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
        }
    };
    Statement {
        kind,
        span,
        inlined: None,
    }
}

/// The blocks that run inside the region the `arena_pop` ending block `pop`
/// closes, each with how many regions nested inside that one are open while
/// its statements run. `None` when the walk back to the matching push does
/// not settle on one nesting per block.
fn region_blocks(body: &Body, preds: &[Vec<usize>], pop: usize) -> Option<Vec<(usize, u32)>> {
    let mut depth: Vec<Option<u32>> = vec![None; body.blocks.len()];
    depth[pop] = Some(0);
    let mut work = vec![pop];
    while let Some(block) = work.pop() {
        let here = depth[block]?;
        for &pred in &preds[block] {
            let term = &body.blocks[pred].terminator;
            let before = if calls_runtime(term, "gos_rt_arena_push") {
                if here == 0 {
                    // The push this pop matches: `pred` runs before the region.
                    continue;
                }
                here - 1
            } else if calls_runtime(term, "gos_rt_arena_pop") {
                here + 1
            } else {
                here
            };
            match depth[pred] {
                Some(seen) if seen == before => {}
                Some(_) => return None,
                None => {
                    // A path that reaches the entry never passed the push.
                    if pred == 0 && !calls_runtime(term, "gos_rt_arena_push") {
                        return None;
                    }
                    depth[pred] = Some(before);
                    work.push(pred);
                }
            }
        }
    }
    Some(
        depth
            .iter()
            .enumerate()
            .filter_map(|(bi, d)| d.map(|d| (bi, d)))
            .collect(),
    )
}

/// Whether `stmt` gives `local` a value of its own: a whole-local write
/// other than the null every counted local starts from.
fn defines_local(stmt: &Statement, local: Local) -> bool {
    matches!(&stmt.kind, StatementKind::Assign { place, rvalue }
        if place.local == local
            && place.projection.is_empty()
            && !matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0)))))
}

/// The locals the region's own blocks give a value, and no block outside
/// it does.
fn region_born_locals(body: &Body, inside: &[(usize, u32)]) -> Vec<Local> {
    let mut nesting: Vec<Option<u32>> = vec![None; body.blocks.len()];
    for &(bi, depth) in inside {
        nesting[bi] = Some(depth);
    }
    let mut born_here = vec![false; body.locals.len()];
    let mut born_outside = vec![false; body.locals.len()];
    for (bi, block) in body.blocks.iter().enumerate() {
        let mut mark = |local: Local| {
            let li = local.0 as usize;
            if li >= born_here.len() {
                return;
            }
            match nesting[bi] {
                Some(0) => born_here[li] = true,
                Some(_) => {}
                None => born_outside[li] = true,
            }
        };
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && defines_local(stmt, place.local)
            {
                mark(place.local);
            }
        }
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
        {
            mark(destination.local);
        }
    }
    (body.arity as usize + 1..body.locals.len())
        .filter(|&li| born_here[li] && !born_outside[li])
        .map(|li| Local(u32::try_from(li).expect("local overflow")))
        .collect()
}

/// A release booked after a region's pop, with the place it gives back.
struct LateRelease {
    name: &'static str,
    args: Vec<Operand>,
    place: Place,
}

/// The releases of `local` that run after the pop ending block `pop`, or
/// `None` when anything but a release, a null, or a fresh value reaches
/// `local` there first.
fn releases_after_pop(body: &Body, pop: usize, local: Local) -> Option<Vec<LateRelease>> {
    let mut found: Vec<LateRelease> = Vec::new();
    let mut seen = vec![false; body.blocks.len()];
    let mut work: Vec<usize> = successor_indices(&body.blocks[pop].terminator);
    'blocks: while let Some(bi) = work.pop() {
        if bi >= seen.len() || seen[bi] {
            continue;
        }
        seen[bi] = true;
        let block = &body.blocks[bi];
        for stmt in &block.stmts {
            if let Some((name, args, place)) = place_release(stmt)
                && place.local == local
                && !args[1..].iter().any(|op| operand_mentions_local(op, local))
            {
                if !found.iter().any(|r| r.place == *place && r.name == name) {
                    found.push(LateRelease {
                        name,
                        args: args.to_vec(),
                        place: place.clone(),
                    });
                }
                continue;
            }
            if defines_local(stmt, local) {
                continue 'blocks;
            }
            if nulls_local(stmt, local) {
                continue;
            }
            if stmt_mentions_local(stmt, local) {
                return None;
            }
        }
        match &block.terminator {
            Terminator::Call { destination, .. }
                if destination.local == local && destination.projection.is_empty() => {}
            Terminator::Drop { place, .. } if place.local == local => return None,
            term if term_mentions_local(term, local) => return None,
            term => work.extend(successor_indices(term)),
        }
    }
    Some(found)
}

/// Gives back, before each `arena_pop`, the values its region made that the
/// frame would otherwise release after the pop.
///
/// A value built while a region is open is region storage, or a heap value
/// that holds region storage. Most of it is bump memory the address test
/// recognises forever, but a string the region promoted is a heap body the
/// region frees at the pop. A release after the pop reads that freed body's
/// header, directly or through the value that holds it, and once the
/// allocator has handed the address to another string it gives back that
/// string's share instead. Nothing the region made is read after the pop, so
/// the frame's release moves in front of it, where every kind of storage
/// answers it correctly, and the place is emptied so the releases that follow
/// the pop give back nothing.
/// Narrows each slot retain and release to the arms its holder's type keeps
/// as copy blobs (see `option_slot_helpers`).
///
/// Runs after every pass that places or pairs slot calls, which all match the
/// helpers by their both-arm names.
pub(crate) fn narrow_slot_calls_to_blob_arms(body: &mut Body, tcx: &TyCtxt) {
    for bi in 0..body.blocks.len() {
        for si in 0..body.blocks[bi].stmts.len() {
            let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } = &body.blocks[bi].stmts[si].kind
            else {
                continue;
            };
            let retain = match *name {
                "gos_rt_option_slot_retain" => true,
                "gos_rt_option_slot_release" => false,
                _ => continue,
            };
            let Some(Operand::Copy(slot)) = args.first() else {
                continue;
            };
            let Some(ty) = crate::uniqueness::place_ty(tcx, body, slot) else {
                continue;
            };
            let (retain_name, release_name) = crate::lower::option_slot_helpers(tcx, ty);
            if let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, .. },
                ..
            } = &mut body.blocks[bi].stmts[si].kind
            {
                *name = if retain { retain_name } else { release_name };
            }
        }
    }
}

pub(crate) fn release_region_values_before_pop(body: &mut Body, tcx: &TyCtxt) {
    let pops: Vec<usize> = body
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| calls_runtime(&b.terminator, "gos_rt_arena_pop"))
        .map(|(i, _)| i)
        .collect();
    if pops.is_empty() {
        return;
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); body.blocks.len()];
    for (bi, block) in body.blocks.iter().enumerate() {
        for succ in successor_indices(&block.terminator) {
            if let Some(slot) = preds.get_mut(succ) {
                slot.push(bi);
            }
        }
    }
    let mut settles: Vec<(usize, Vec<LateRelease>)> = Vec::new();
    for &pop in &pops {
        let Some(inside) = region_blocks(body, &preds, pop) else {
            continue;
        };
        let mut releases: Vec<LateRelease> = Vec::new();
        for local in region_born_locals(body, &inside) {
            if let Some(found) = releases_after_pop(body, pop, local) {
                releases.extend(found);
            }
        }
        if !releases.is_empty() {
            settles.push((pop, releases));
        }
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut settled: Vec<Place> = Vec::new();
    for (pop, releases) in settles {
        let span = body.blocks[pop].span;
        for release in releases {
            let mut unit_local = || {
                let local = Local(u32::try_from(body.locals.len()).expect("local overflow"));
                body.locals.push(LocalDecl {
                    ty: unit_ty,
                    debug_name: None,
                    mutable: false,
                    region: false,
                });
                local
            };
            let sink = unit_local();
            let null_sink = unit_local();
            let null =
                null_after_release(release.name, &release.args, &release.place, null_sink, span);
            let stmts = &mut body.blocks[pop].stmts;
            stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(sink),
                    rvalue: Rvalue::CallIntrinsic {
                        name: release.name,
                        args: release.args,
                    },
                },
                span,
                inlined: None,
            });
            stmts.push(null);
            if !settled.contains(&release.place) {
                settled.push(release.place);
            }
        }
    }
    for place in settled {
        drop_releases_of_null(body, &place);
    }
}

/// How `stmt` leaves `place`: emptied, possibly holding a value, or as it was.
fn null_step(stmt: &Statement, place: &Place) -> Option<bool> {
    let StatementKind::Assign { place: dest, rvalue } = &stmt.kind else {
        return None;
    };
    if dest.local != place.local {
        return None;
    }
    if dest == place {
        return Some(!matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0)))));
    }
    // A write to the place's container, or to a part of the place, leaves a
    // value there; one to a sibling field leaves it alone.
    let covers = |outer: &Place, inner: &Place| {
        outer.projection.len() <= inner.projection.len()
            && outer.projection.iter().zip(&inner.projection).all(|(a, b)| a == b)
    };
    (covers(dest, place) || covers(place, dest)).then_some(true)
}

/// Removes every release of `place` that runs where `place` is certainly
/// empty: the releases a region's pop has already given back for it, and
/// any other release that only ever sees the null the entry block wrote.
fn drop_releases_of_null(body: &mut Body, place: &Place) {
    let n = body.blocks.len();
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (bi, block) in body.blocks.iter().enumerate() {
        for succ in successor_indices(&block.terminator) {
            if let Some(slot) = preds.get_mut(succ) {
                slot.push(bi);
            }
        }
    }
    let block_out = |block: &BasicBlock, mut live: bool| {
        for stmt in &block.stmts {
            if let Some(step) = null_step(stmt, place) {
                live = step;
            }
        }
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.local == place.local
        {
            live = true;
        }
        live
    };
    // Whether `place` may hold a value on entry to each block. The function's
    // own entry is unknown until its first write.
    let mut live_in = vec![false; n];
    if n > 0 {
        live_in[0] = true;
    }
    let mut changed = true;
    while changed {
        changed = false;
        for bi in 0..n {
            let incoming = bi == 0
                || preds[bi]
                    .iter()
                    .any(|&p| block_out(&body.blocks[p], live_in[p]));
            if incoming && !live_in[bi] {
                live_in[bi] = true;
                changed = true;
            }
        }
    }
    for (block, &entry) in body.blocks.iter_mut().zip(&live_in) {
        let mut live = entry;
        for stmt in &mut block.stmts {
            let releases_place = matches!(&stmt.kind, StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args }, ..
            } if crate::uniqueness::is_release_intrinsic(name)
                && matches!(args.first(), Some(Operand::Copy(p)) if p == place));
            if releases_place && !live {
                stmt.kind = StatementKind::Nop;
                continue;
            }
            if let Some(step) = null_step(stmt, place) {
                live = step;
            }
        }
    }
}
