/// Where a statement sits: its block and its index in that block, or the
/// block's terminator when the index is `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MirSite {
    block: usize,
    stmt: Option<usize>,
}

/// A temporary `from` that is defined on each of several paths that meet at
/// one move of it whole into `into`, and otherwise only zeroed or released
/// while it holds nothing.
struct Forward {
    from: Local,
    into: Local,
    /// The definitions of the whole temporary, one per path to the move.
    defs: Vec<MirSite>,
    /// The statements between a definition and the move.
    window: Vec<MirSite>,
    /// The move itself, followed by the zeroing of `from`'s counted places.
    moved: MirSite,
    zeroed: usize,
    /// Releases of `into`'s previous value inside the window, which run
    /// before the definition instead. Only a single definition hoists.
    hoisted: Vec<MirSite>,
}

/// Builds a moved temporary in the local it is moved to.
///
/// `t = <def>; ...; l = t; t.p = 0 ...` becomes `l = <def>; ...`: the value
/// is built in place, and the copy, the zeroing that marks `t` moved, and
/// every release of `t` (each a release of nothing, as `t` holds a value only
/// between its definition and the move) disappear. The arms of a `match` or
/// `if` that each define `t` before joining at the move are built in place
/// alike. Chains of moves collapse
/// one link per round. A parameter is never the destination, as an aggregate
/// parameter is the caller's storage, and the return slot is one only when no
/// parameter can reach storage of its type, as the caller's storage behind a
/// parameter may be the very place the result lands.
pub(crate) fn forward_moved_temporaries(body: &mut Body, tcx: &TyCtxt) {
    if has_landing_pads(body) {
        return;
    }
    let return_slot_free = return_slot_unaliased(body, tcx);
    // A scalar or a two-word carrier lives in registers, where a copy costs
    // nothing; only a wider aggregate is worth building in place.
    let wide: Vec<bool> = body
        .locals
        .iter()
        .map(|decl| {
            matches!(
                tcx.kind_of(decl.ty),
                TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Adt { .. }
            ) && !crate::lower::carrier_ref::is_two_word_carrier(tcx, decl.ty)
        })
        .collect();
    // Each round removes one whole-local copy and adds none, so this ends.
    while let Some(forward) = find_forward(body, return_slot_free, &wide) {
        apply_forward(body, &forward);
    }
    if return_slot_free && let Some(forward) = find_return_forward(body, &wide) {
        apply_return_forward(body, &forward);
    }
}

/// A local `from` moved whole into the return slot on the one exit that
/// answers it, however often it is defined first: `_0` is written nowhere
/// else but in blocks that return, and read nowhere at all, so `from` can be
/// the return slot throughout. On another exit the write of `_0` replaces
/// `from`'s last value, whose releases follow the write in that block and
/// move ahead of it.
struct ReturnForward {
    from: Local,
    /// The move, followed by `zeroed` statements marking `from` moved.
    moved: MirSite,
    zeroed: usize,
    /// Statements after the move in its block releasing what the move
    /// emptied, which release nothing.
    empty_releases: Vec<usize>,
    /// Each other write of `_0`, with the releases of `from` after it in its
    /// block.
    exits: Vec<(MirSite, Vec<usize>)>,
}

fn find_return_forward(body: &Body, wide: &[bool]) -> Option<ReturnForward> {
    let (moved, from, writes) = return_writes(body, wide)?;
    let returns = |block: usize| matches!(body.blocks[block].terminator, Terminator::Return);
    if !returns(moved.block) {
        return None;
    }
    let move_block = &body.blocks[moved.block];
    let moved_index = moved.stmt?;
    let zeroed_places = zeroed_after(move_block, moved_index, from);
    let zeroed = zeroed_places.len();
    let mut empty_releases = Vec::new();
    for (si, stmt) in move_block.stmts.iter().enumerate().skip(moved_index + 1 + zeroed) {
        if !crate::opt::stmt_mentions_local(stmt, from) {
            continue;
        }
        match releases_part_of(&stmt.kind, from) {
            Some(target) if zeroed_places.contains(&target) => empty_releases.push(si),
            _ => return None,
        }
    }
    let mut exits = Vec::new();
    for write in writes {
        let block = &body.blocks[write.block];
        let wi = write.stmt?;
        if write.block == moved.block || !returns(write.block) {
            return None;
        }
        if let StatementKind::Assign { rvalue, .. } = &block.stmts[wi].kind
            && rvalue_mentions(rvalue, from)
        {
            return None;
        }
        let mut releases = Vec::new();
        for (si, stmt) in block.stmts.iter().enumerate().skip(wi + 1) {
            if !crate::opt::stmt_mentions_local(stmt, from) {
                continue;
            }
            releases_part_of(&stmt.kind, from)?;
            releases.push(si);
        }
        exits.push((write, releases));
    }
    // A drop of `from` would drop the answer.
    if body
        .blocks
        .iter()
        .any(|block| matches!(&block.terminator, Terminator::Drop { place, .. } if place.local == from))
    {
        return None;
    }
    Some(ReturnForward {
        from,
        moved,
        zeroed,
        empty_releases,
        exits,
    })
}

/// Whether `rvalue` reads `local`.
fn rvalue_mentions(rvalue: &Rvalue, local: Local) -> bool {
    let mut mentioned = Vec::new();
    statement_locals(
        &StatementKind::Assign {
            place: Place::local(Local(u32::MAX)),
            rvalue: rvalue.clone(),
        },
        &mut mentioned,
    );
    mentioned.contains(&local)
}

/// The one write of `_0` that moves a wide, unborrowed local of the answer's
/// type into it, that local, and every other write of `_0`. `None` when `_0`
/// is reached any other way or more than one write moves a local in.
fn return_writes(body: &Body, wide: &[bool]) -> Option<(MirSite, Local, Vec<MirSite>)> {
    let sites = local_sites(body);
    let borrowed = borrowed_locals(body);
    let mut moved: Option<(MirSite, Local)> = None;
    let mut writes: Vec<MirSite> = Vec::new();
    for &site in sites.first()? {
        let si = site.stmt?;
        let StatementKind::Assign { place, rvalue } = &body.blocks[site.block].stmts[si].kind
        else {
            return None;
        };
        if place.local != Local::RETURN || !place.projection.is_empty() {
            return None;
        }
        if rvalue_mentions(rvalue, Local::RETURN) {
            return None;
        }
        if let Rvalue::Use(Operand::Copy(src)) = rvalue
            && src.projection.is_empty()
            && src.local.0 > body.arity
            && wide[src.local.0 as usize]
            && !borrowed[src.local.0 as usize]
            && body.locals[src.local.0 as usize].ty == body.locals[0].ty
        {
            if moved.is_some() {
                return None;
            }
            moved = Some((site, src.local));
        } else {
            writes.push(site);
        }
    }
    let (moved, from) = moved?;
    Some((moved, from, writes))
}

fn apply_return_forward(body: &mut Body, forward: &ReturnForward) {
    let from = forward.from;
    let moved_index = forward.moved.stmt.unwrap_or_default();
    let move_block = &mut body.blocks[forward.moved.block];
    for si in moved_index..=moved_index + forward.zeroed {
        move_block.stmts[si].kind = StatementKind::Nop;
    }
    for &si in &forward.empty_releases {
        move_block.stmts[si].kind = StatementKind::Nop;
    }
    // The releases of `from`'s last value run before the write replacing it.
    for (write, releases) in &forward.exits {
        let Some(wi) = write.stmt else { continue };
        let stmts = &mut body.blocks[write.block].stmts;
        let hoisted: Vec<Statement> = releases.iter().map(|&si| stmts[si].clone()).collect();
        for &si in releases.iter().rev() {
            stmts.remove(si);
        }
        stmts.splice(wi..wi, hoisted);
    }
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            rename_in_statement(&mut stmt.kind, from, Local::RETURN);
        }
        rename_in_terminator(&mut block.terminator, from, Local::RETURN);
        if let Terminator::Call { destination, .. } = &mut block.terminator
            && destination.local == from
        {
            destination.local = Local::RETURN;
        }
    }
}

/// Whether a fault can resume in this body's landing pads, which run between
/// any two statements and would see a value half moved.
fn has_landing_pads(body: &Body) -> bool {
    body.blocks.iter().any(|block| {
        matches!(block.terminator, Terminator::Resume)
            || block.stmts.iter().any(|stmt| {
                matches!(&stmt.kind, StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, .. },
                    ..
                } if *name == "gos_unwind_probe")
            })
    })
}

/// Whether no parameter's type can reach storage of the return type, so the
/// return slot cannot be storage a parameter still reads.
fn return_slot_unaliased(body: &Body, tcx: &TyCtxt) -> bool {
    let Some(ret) = body.locals.first().map(|decl| decl.ty) else {
        return false;
    };
    (1..=body.arity as usize)
        .filter_map(|index| body.locals.get(index))
        .all(|decl| !may_reach(tcx, decl.ty, ret, 0))
}

/// Whether a value of type `ty` holds, or points to storage holding, a value
/// of type `target`. Answers yes for any type it cannot see through.
fn may_reach(tcx: &TyCtxt, ty: Ty, target: Ty, depth: u32) -> bool {
    if ty == target || depth > 12 {
        return true;
    }
    let next = depth + 1;
    match tcx.kind_of(ty) {
        TyKind::Bool
        | TyKind::Char
        | TyKind::String
        | TyKind::Int(_)
        | TyKind::Float(_)
        | TyKind::Unit
        | TyKind::Never
        | TyKind::Duration
        | TyKind::Instant
        | TyKind::JsonValue
        | TyKind::DynValue
        | TyKind::DynError
        | TyKind::FnDef { .. }
        | TyKind::FnPtr(_) => false,
        TyKind::Tuple(elems) => elems.iter().any(|e| may_reach(tcx, *e, target, next)),
        TyKind::Array { elem, .. }
        | TyKind::Simd { elem, .. }
        | TyKind::Slice(elem)
        | TyKind::Vec(elem)
        | TyKind::Iterator(elem)
        | TyKind::Range(elem)
        | TyKind::Sender(elem)
        | TyKind::Receiver(elem)
        | TyKind::JoinHandle(elem) => may_reach(tcx, *elem, target, next),
        TyKind::HashMap { key, value, .. } => {
            may_reach(tcx, *key, target, next) || may_reach(tcx, *value, target, next)
        }
        TyKind::Ref { inner, .. } => may_reach(tcx, *inner, target, next),
        TyKind::Nominal { repr, .. } => may_reach(tcx, *repr, target, next),
        TyKind::Adt { def, substs } => {
            if substs
                .types()
                .iter()
                .any(|t| may_reach(tcx, *t, target, next))
            {
                return true;
            }
            if let Some(fields) = tcx.struct_field_tys(*def) {
                return fields.iter().any(|f| may_reach(tcx, *f, target, next));
            }
            match tcx.enum_variant_tys(*def) {
                Some(variants) => variants
                    .iter()
                    .flatten()
                    .any(|f| may_reach(tcx, *f, target, next)),
                None => true,
            }
        }
        _ => true,
    }
}

/// Every site that mentions each local.
fn local_sites(body: &Body) -> Vec<Vec<MirSite>> {
    let mut sites: Vec<Vec<MirSite>> = vec![Vec::new(); body.locals.len()];
    let mut mentioned = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            mentioned.clear();
            statement_locals(&stmt.kind, &mut mentioned);
            mentioned.sort_unstable_by_key(|l| l.0);
            mentioned.dedup();
            for local in &mentioned {
                if let Some(list) = sites.get_mut(local.0 as usize) {
                    list.push(MirSite {
                        block: bi,
                        stmt: Some(si),
                    });
                }
            }
        }
        mentioned.clear();
        terminator_locals(&block.terminator, &mut mentioned);
        mentioned.sort_unstable_by_key(|l| l.0);
        mentioned.dedup();
        for local in &mentioned {
            if let Some(list) = sites.get_mut(local.0 as usize) {
                list.push(MirSite {
                    block: bi,
                    stmt: None,
                });
            }
        }
    }
    sites
}

fn find_forward(body: &Body, return_slot_free: bool, wide: &[bool]) -> Option<Forward> {
    let sites = local_sites(body);
    let borrowed = borrowed_locals(body);
    let preds = predecessor_lists(body);
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(source)),
            } = &stmt.kind
            else {
                continue;
            };
            if !place.projection.is_empty() || !source.projection.is_empty() {
                continue;
            }
            let (into, from) = (place.local, source.local);
            if into == from || from.0 == 0 || from.0 <= body.arity {
                continue;
            }
            if (1..=body.arity).contains(&into.0) || (into.0 == 0 && !return_slot_free) {
                continue;
            }
            if !wide[from.0 as usize] || borrowed[into.0 as usize] || borrowed[from.0 as usize] {
                continue;
            }
            if body.locals[into.0 as usize].ty != body.locals[from.0 as usize].ty {
                continue;
            }
            let moved = MirSite {
                block: bi,
                stmt: Some(si),
            };
            if let Some(forward) =
                plan_forward(body, &sites[from.0 as usize], &preds, from, into, moved)
            {
                return Some(forward);
            }
        }
    }
    None
}

/// Locals whose address is taken anywhere in the body.
fn borrowed_locals(body: &Body) -> Vec<bool> {
    let mut borrowed = vec![false; body.locals.len()];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign {
                rvalue: Rvalue::Ref { place, .. },
                ..
            } = &stmt.kind
                && let Some(flag) = borrowed.get_mut(place.local.0 as usize)
            {
                *flag = true;
            }
        }
    }
    borrowed
}

fn predecessor_lists(body: &Body) -> Vec<Vec<usize>> {
    let mut preds = vec![Vec::new(); body.blocks.len()];
    for (bi, block) in body.blocks.iter().enumerate() {
        for succ in successor_indices(&block.terminator) {
            if let Some(list) = preds.get_mut(succ) {
                list.push(bi);
            }
        }
    }
    preds
}

fn statement_at(body: &Body, site: MirSite) -> Option<&StatementKind> {
    site.stmt
        .and_then(|si| body.blocks.get(site.block)?.stmts.get(si))
        .map(|stmt| &stmt.kind)
}

/// Whether `stmt` writes 0 to a projection of `local`, the mark of a place
/// that holds nothing.
fn zeroes_part_of(stmt: &StatementKind, local: Local) -> Option<&Place> {
    match stmt {
        StatementKind::Assign {
            place,
            rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
        } if place.local == local
            && !place.projection.is_empty()
            && place
                .projection
                .iter()
                .all(|step| matches!(step, Projection::Field(_))) =>
        {
            Some(place)
        }
        _ => None,
    }
}

/// The place an RC release of a projection of `local` releases.
fn releases_part_of(stmt: &StatementKind, local: Local) -> Option<&Place> {
    match stmt {
        StatementKind::Assign {
            place: result,
            rvalue: Rvalue::CallIntrinsic { name, args },
        } if rc_release_only(name) && result.local != local => match args.as_slice() {
            [Operand::Copy(target)] if target.local == local => Some(target),
            _ => None,
        },
        _ => None,
    }
}

/// The places of `from` the statements right after the move at `index` in
/// `block` mark empty.
fn zeroed_after(block: &BasicBlock, index: usize, from: Local) -> Vec<&Place> {
    block.stmts[index + 1..]
        .iter()
        .map_while(|stmt| zeroes_part_of(&stmt.kind, from))
        .collect()
}

/// The definitions of the whole temporary `from`: one, or one per block of
/// several that each reach the move, apart from the move's own block.
fn whole_definitions(
    body: &Body,
    from_sites: &[MirSite],
    from: Local,
    moved: MirSite,
) -> Option<Vec<MirSite>> {
    let defs: Vec<MirSite> = from_sites
        .iter()
        .copied()
        .filter(|&site| match site.stmt {
            Some(_) => matches!(statement_at(body, site),
                Some(StatementKind::Assign { place, .. })
                    if place.local == from && place.projection.is_empty()),
            None => matches!(&body.blocks[site.block].terminator,
                Terminator::Call { destination, .. }
                    if destination.local == from && destination.projection.is_empty()),
        })
        .collect();
    if defs.is_empty() {
        return None;
    }
    if defs.len() > 1
        && defs.iter().enumerate().any(|(i, def)| {
            def.block == moved.block || defs[..i].iter().any(|other| other.block == def.block)
        })
    {
        return None;
    }
    Some(defs)
}

fn plan_forward(
    body: &Body,
    from_sites: &[MirSite],
    preds: &[Vec<usize>],
    from: Local,
    into: Local,
    moved: MirSite,
) -> Option<Forward> {
    let moved_index = moved.stmt?;
    let zeroed_places = zeroed_after(&body.blocks[moved.block], moved_index, from);
    let zeroed = zeroed_places.len();
    let defs = whole_definitions(body, from_sites, from, moved)?;
    let (window, every_path) = region_window(body, preds, &defs, moved)?;
    // A definition may not read the destination, which it now writes.
    let def_reads_into = defs.iter().any(|def| match def.stmt {
        Some(si) => stmt_mentions_local(&body.blocks[def.block].stmts[si], into),
        None => term_mentions_local(&body.blocks[def.block].terminator, into),
    });
    if def_reads_into {
        return None;
    }
    let mut hoisted = Vec::new();
    for &site in &window {
        let Some(si) = site.stmt else {
            // A terminator inside the window may read the temporary, which
            // it then reads from the destination, but may not name the
            // destination, whose new value it would see early.
            let term = &body.blocks[site.block].terminator;
            let allowed = matches!(
                term,
                Terminator::Goto { .. }
                    | Terminator::SwitchInt { .. }
                    | Terminator::Assert { .. }
                    | Terminator::Panic { .. }
                    | Terminator::Unreachable
                    | Terminator::Call { target: Some(_), .. }
            );
            if !allowed || term_mentions_local(term, into) {
                return None;
            }
            if let Terminator::Call { destination, .. } = term
                && destination.local == from
            {
                return None;
            }
            continue;
        };
        let stmt = &body.blocks[site.block].stmts[si].kind;
        if !matches!(
            stmt,
            StatementKind::Assign { .. }
                | StatementKind::Nop
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
        ) {
            return None;
        }
        let mut mentioned = Vec::new();
        statement_locals(stmt, &mut mentioned);
        if mentioned.contains(&into) {
            // A release of the destination's old value moves ahead of the
            // definition, so it must run on every path that reaches it.
            if defs.len() > 1
                || releases_part_of(stmt, into).is_none()
                || !every_path.contains(&site)
            {
                return None;
            }
            hoisted.push(site);
        }
    }
    // Outside the window the temporary holds nothing: anything there may
    // only zero it or release a place the move marks empty.
    let zero_sites: Vec<usize> = (moved_index + 1..=moved_index + zeroed).collect();
    for &site in from_sites {
        if defs.contains(&site) || site == moved || window.contains(&site) {
            continue;
        }
        if site.block == moved.block && site.stmt.is_some_and(|si| zero_sites.contains(&si)) {
            continue;
        }
        let stmt = statement_at(body, site)?;
        if zeroes_part_of(stmt, from).is_some()
            || matches!(stmt, StatementKind::StorageLive(_) | StatementKind::StorageDead(_))
        {
            continue;
        }
        match releases_part_of(stmt, from) {
            Some(target) if zeroed_places.contains(&target) => {}
            _ => return None,
        }
    }
    Some(Forward {
        from,
        into,
        defs,
        window,
        moved,
        zeroed,
        hoisted,
    })
}

/// The sites between a definition and `moved`: the statements and
/// terminators of every block a run from a definition passes before the
/// move, when every such run reaches the move, no run from one definition
/// meets another, and nothing else enters that stretch. Also the statements
/// of the window that run on every path, which for a single definition are
/// the ones in its block after it and in the move's block before it.
fn region_window(
    body: &Body,
    preds: &[Vec<usize>],
    defs: &[MirSite],
    moved: MirSite,
) -> Option<(Vec<MirSite>, Vec<MirSite>)> {
    let moved_index = moved.stmt?;
    let stmt_sites = |block: usize, range: std::ops::Range<usize>| {
        range.map(move |si| MirSite {
            block,
            stmt: Some(si),
        })
    };
    if let [def] = defs
        && def.block == moved.block
    {
        let from = def.stmt? + 1;
        if from > moved_index {
            return None;
        }
        let window: Vec<MirSite> = stmt_sites(def.block, from..moved_index).collect();
        return Some((window.clone(), window));
    }
    let mut window: Vec<MirSite> = Vec::new();
    let mut every_path: Vec<MirSite> = Vec::new();
    let mut is_def_block = vec![false; body.blocks.len()];
    for def in defs {
        is_def_block[def.block] = true;
        if let Some(si) = def.stmt {
            let after: Vec<MirSite> =
                stmt_sites(def.block, si + 1..body.blocks[def.block].stmts.len()).collect();
            window.extend(after.iter().copied());
            if defs.len() == 1 {
                every_path.extend(after);
            }
            window.push(MirSite {
                block: def.block,
                stmt: None,
            });
        }
    }
    // The blocks a run passes between a definition and the move, found
    // walking forward from the definitions and stopping at the move's block.
    let mut in_region = vec![false; body.blocks.len()];
    let mut region: Vec<usize> = Vec::new();
    let mut stack: Vec<usize> = defs
        .iter()
        .flat_map(|def| successor_indices(&body.blocks[def.block].terminator))
        .collect();
    while let Some(b) = stack.pop() {
        if b == moved.block || in_region[b] {
            continue;
        }
        if is_def_block[b] {
            return None;
        }
        in_region[b] = true;
        region.push(b);
        stack.extend(successor_indices(&body.blocks[b].terminator));
    }
    let inside = |p: &usize| is_def_block[*p] || in_region[*p];
    if !preds[moved.block].iter().all(inside) || preds[moved.block].is_empty() {
        return None;
    }
    for &b in &region {
        if !preds[b].iter().all(inside) {
            return None;
        }
        // A run that leaves the function here never reaches the move.
        if matches!(
            body.blocks[b].terminator,
            Terminator::Return | Terminator::Resume | Terminator::Drop { .. }
        ) {
            return None;
        }
        window.extend(stmt_sites(b, 0..body.blocks[b].stmts.len()));
        window.push(MirSite {
            block: b,
            stmt: None,
        });
    }
    let before: Vec<MirSite> = stmt_sites(moved.block, 0..moved_index).collect();
    window.extend(before.iter().copied());
    if defs.len() == 1 {
        every_path.extend(before);
    }
    Some((window, every_path))
}

fn apply_forward(body: &mut Body, forward: &Forward) {
    let Forward {
        from,
        into,
        moved,
        zeroed,
        ..
    } = *forward;
    let from_sites = local_sites(body).swap_remove(from.0 as usize);
    // The releases of `into`'s previous value run before it is rebuilt.
    let hoisted: Vec<Statement> = forward
        .hoisted
        .iter()
        .filter_map(|site| {
            let stmt = &mut body.blocks[site.block].stmts[site.stmt?];
            let taken = stmt.clone();
            stmt.kind = StatementKind::Nop;
            Some(taken)
        })
        .collect();
    for &site in &forward.window {
        match site.stmt {
            Some(si) => {
                rename_in_statement(&mut body.blocks[site.block].stmts[si].kind, from, into);
            }
            None => rename_in_terminator(&mut body.blocks[site.block].terminator, from, into),
        }
    }
    for def in &forward.defs {
        match def.stmt {
            Some(si) => {
                rename_in_statement(&mut body.blocks[def.block].stmts[si].kind, from, into);
            }
            None => {
                if let Terminator::Call { destination, .. } = &mut body.blocks[def.block].terminator
                {
                    destination.local = into;
                }
            }
        }
    }
    // Everything else that names the temporary zeroes or releases nothing.
    let moved_index = moved.stmt.unwrap_or_default();
    for site in from_sites {
        if forward.defs.contains(&site) || forward.window.contains(&site) {
            continue;
        }
        if let Some(si) = site.stmt {
            body.blocks[site.block].stmts[si].kind = StatementKind::Nop;
        }
    }
    for si in moved_index..=moved_index + zeroed {
        body.blocks[moved.block].stmts[si].kind = StatementKind::Nop;
    }
    // Hoisting is planned only for a single definition.
    if let [def] = forward.defs.as_slice() {
        let block = &mut body.blocks[def.block];
        let at = def.stmt.unwrap_or(block.stmts.len());
        block.stmts.splice(at..at, hoisted);
    }
}

/// Replaces every read of `from` in `term` with `to`.
fn rename_in_terminator(term: &mut Terminator, from: Local, to: Local) {
    let operand = |op: &mut Operand| {
        if let Operand::Copy(p) = op {
            if p.local == from {
                p.local = to;
            }
            for step in &mut p.projection {
                if let Projection::Index(l) = step
                    && *l == from
                {
                    *l = to;
                }
            }
        }
    };
    match term {
        Terminator::SwitchInt { discriminant, .. } => operand(discriminant),
        Terminator::Call { callee, args, .. } => {
            operand(callee);
            args.iter_mut().for_each(operand);
        }
        Terminator::Assert { cond, msg, .. } => {
            operand(cond);
            msg.operands_mut().for_each(operand);
        }
        _ => {}
    }
}

/// Replaces every mention of `from` in `stmt` with `to`.
fn rename_in_statement(stmt: &mut StatementKind, from: Local, to: Local) {
    let place = |p: &mut Place| {
        if p.local == from {
            p.local = to;
        }
        for step in &mut p.projection {
            if let Projection::Index(l) = step
                && *l == from
            {
                *l = to;
            }
        }
    };
    let operand = |op: &mut Operand| {
        if let Operand::Copy(p) = op {
            place(p);
        }
    };
    match stmt {
        StatementKind::Assign { place: dst, rvalue } => {
            place(dst);
            match rvalue {
                Rvalue::Use(op)
                | Rvalue::UnaryOp { operand: op, .. }
                | Rvalue::Cast { operand: op, .. }
                | Rvalue::Repeat { value: op, .. } => operand(op),
                Rvalue::BinaryOp { lhs, rhs, .. } => {
                    operand(lhs);
                    operand(rhs);
                }
                Rvalue::Aggregate { operands, .. } | Rvalue::CallIntrinsic { args: operands, .. } => {
                    operands.iter_mut().for_each(operand);
                }
                Rvalue::Len(p) | Rvalue::Ref { place: p, .. } => place(p),
                Rvalue::StaticLoad(_) => {}
            }
        }
        StatementKind::SetDiscriminant { place: p, .. } => place(p),
        StatementKind::StaticStore { value, .. } => operand(value),
        StatementKind::IterSource { dst, source, .. } => {
            place(dst);
            operand(source);
        }
        StatementKind::IterAdapter {
            dst,
            upstream,
            closure_or_arg,
            ..
        } => {
            place(dst);
            place(upstream);
            if let Some(op) = closure_or_arg {
                operand(op);
            }
        }
        StatementKind::IterNext {
            dst_option,
            iter_place,
            ..
        } => {
            place(dst_option);
            place(iter_place);
        }
        StatementKind::StorageLive(l) | StatementKind::StorageDead(l) => {
            if *l == from {
                *l = to;
            }
        }
        StatementKind::Nop => {}
    }
}
