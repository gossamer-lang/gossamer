// ---------------------------------------------------------------------------
// RC retain/release last-use elision (item 3).
// ---------------------------------------------------------------------------

/// The release helper that balances a given retain helper. `None` for any
/// name that is not a plain strong retain - weak / field / aggregate
/// retains are never paired here (a wrong pairing would unbalance a
/// reference count and free reachable memory).
fn rc_paired_release(retain: &str) -> Option<&'static str> {
    match retain {
        "gos_rt_rc_retain" => Some("gos_rt_rc_release"),
        "gos_rt_vec_retain" => Some("gos_rt_vec_free"),
        _ => None,
    }
}

/// The single bare-local argument of an RC accounting call, or `None`
/// when the call takes anything other than exactly one projection-free
/// `Copy(local)` (a field/weak/aggregate op, or a constant).
fn rc_bare_local_arg(args: &[Operand]) -> Option<Local> {
    if let [Operand::Copy(p)] = args
        && p.projection.is_empty()
    {
        return Some(p.local);
    }
    None
}

/// `true` when an accounting call's one argument names `holder` itself or a
/// field beneath it.
///
/// A container-valued field carries its own share, so the ops that account
/// for a struct copy read `Copy(holder.[.k])` rather than the holder word.
fn rc_arg_names_holder(args: &[Operand], holder: Local) -> bool {
    matches!(args, [Operand::Copy(p)]
        if p.local == holder
            && p.projection.iter().all(|x| matches!(x, Projection::Field(_))))
}

/// `true` when `place` reads or writes through `local` (root or an
/// `Index` projection local).
fn place_mentions_local(place: &Place, local: Local) -> bool {
    place.local == local
        || place
            .projection
            .iter()
            .any(|p| matches!(p, Projection::Index(l) if *l == local))
}

fn operand_mentions_local(op: &Operand, local: Local) -> bool {
    matches!(op, Operand::Copy(p) if place_mentions_local(p, local))
}

fn rvalue_mentions_local(rv: &Rvalue, local: Local) -> bool {
    match rv {
        Rvalue::Use(op)
        | Rvalue::UnaryOp { operand: op, .. }
        | Rvalue::Cast { operand: op, .. } => operand_mentions_local(op, local),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            operand_mentions_local(lhs, local) || operand_mentions_local(rhs, local)
        }
        Rvalue::Aggregate { operands, .. } => {
            operands.iter().any(|o| operand_mentions_local(o, local))
        }
        Rvalue::Repeat { value, .. } => operand_mentions_local(value, local),
        Rvalue::Len(p) | Rvalue::Ref { place: p, .. } => place_mentions_local(p, local),
        Rvalue::CallIntrinsic { args, .. } => args.iter().any(|o| operand_mentions_local(o, local)),
        Rvalue::StaticLoad(_) => false,
    }
}

/// `true` when `stmt` reads or writes `local` in any position.
fn stmt_mentions_local(stmt: &Statement, local: Local) -> bool {
    match &stmt.kind {
        StatementKind::Assign { place, rvalue } => {
            place_mentions_local(place, local) || rvalue_mentions_local(rvalue, local)
        }
        StatementKind::SetDiscriminant { place, .. } => place_mentions_local(place, local),
        StatementKind::StaticStore { value, .. } => operand_mentions_local(value, local),
        StatementKind::IterSource { dst, source, .. } => {
            place_mentions_local(dst, local) || operand_mentions_local(source, local)
        }
        StatementKind::IterAdapter {
            dst,
            upstream,
            closure_or_arg,
            ..
        } => {
            place_mentions_local(dst, local)
                || place_mentions_local(upstream, local)
                || closure_or_arg
                    .as_ref()
                    .is_some_and(|arg| operand_mentions_local(arg, local))
        }
        StatementKind::IterNext {
            dst_option,
            iter_place,
            ..
        } => place_mentions_local(dst_option, local) || place_mentions_local(iter_place, local),
        StatementKind::StorageLive(l) | StatementKind::StorageDead(l) => *l == local,
        StatementKind::Nop => false,
    }
}

/// `true` when `stmt` assigns the bare local `local` (a full
/// reassignment, not a projected field/element write).
fn stmt_writes_bare(stmt: &Statement, local: Local) -> bool {
    matches!(&stmt.kind, StatementKind::Assign { place, .. }
        if place.projection.is_empty() && place.local == local)
}

fn term_mentions_local(t: &Terminator, local: Local) -> bool {
    let m = |op: &Operand| operand_mentions_local(op, local);
    match t {
        Terminator::SwitchInt { discriminant, .. } => m(discriminant),
        Terminator::Call {
            callee,
            args,
            destination,
            ..
        } => m(callee) || args.iter().any(m) || place_mentions_local(destination, local),
        Terminator::Assert { cond, .. } => m(cond),
        _ => false,
    }
}

fn term_writes_bare(t: &Terminator, local: Local) -> bool {
    matches!(t, Terminator::Call { destination, .. }
        if destination.projection.is_empty() && destination.local == local)
}

/// Block successor indices.
fn successor_indices(t: &Terminator) -> Vec<usize> {
    match t {
        Terminator::Goto { target } => vec![target.0 as usize],
        Terminator::SwitchInt { arms, default, .. } => {
            let mut v: Vec<usize> = arms.iter().map(|(_, b)| b.0 as usize).collect();
            v.push(default.0 as usize);
            v
        }
        Terminator::Call { target, .. } => target.iter().map(|t| t.0 as usize).collect(),
        Terminator::Assert { target, .. } | Terminator::Drop { target, .. } => {
            vec![target.0 as usize]
        }
        Terminator::Return | Terminator::Unreachable | Terminator::Panic { .. } => Vec::new(),
    }
}

/// Forward liveness probe: starting just after statement `after_stmt` in
/// `start_block`, returns `true` when `x` is read on some path before it
/// is overwritten. A bare reassignment (or a call destination) kills the
/// path; any other appearance of `x` - including an RC accounting call on
/// it - counts as a read and makes `x` live. Conservative: any uncertain
/// reach reports live so the caller keeps the retain/release pair.
fn local_live_after(
    body: &Body,
    succs: &[Vec<usize>],
    start_block: usize,
    after_stmt: usize,
    x: Local,
) -> bool {
    let n = body.blocks.len();
    let mut stack: Vec<(usize, usize)> = vec![(start_block, after_stmt + 1)];
    let mut visited = vec![false; n];
    while let Some((b, from)) = stack.pop() {
        let blk = &body.blocks[b];
        let mut killed = false;
        for sj in from..blk.stmts.len() {
            let st = &blk.stmts[sj];
            if stmt_writes_bare(st, x) {
                // `x = f(x)` reads the old value before overwriting.
                if let StatementKind::Assign { rvalue, .. } = &st.kind
                    && rvalue_mentions_local(rvalue, x)
                {
                    return true;
                }
                killed = true;
                break;
            }
            if stmt_mentions_local(st, x) {
                return true;
            }
        }
        if killed {
            continue;
        }
        let t = &blk.terminator;
        if term_writes_bare(t, x) {
            if let Terminator::Call { callee, args, .. } = t
                && (operand_mentions_local(callee, x)
                    || args.iter().any(|o| operand_mentions_local(o, x)))
            {
                return true;
            }
            continue;
        }
        if term_mentions_local(t, x) {
            return true;
        }
        for &s in &succs[b] {
            if !visited[s] {
                visited[s] = true;
                stack.push((s, 0));
            }
        }
    }
    false
}

/// RC retain/release last-use elision (item 3). Cancels a tightly
/// bracketed `retain(x)` / `release(x)` pair on a non-shared,
/// non-region, RC-managed local `x` whose reference is moved into a
/// surviving holder.
///
/// A pair is cancelled only when, conservatively, all hold:
/// - the retain is a plain strong retain (`gos_rt_rc_retain` /
///   `gos_rt_vec_retain`) on a bare local - never a field / weak /
///   aggregate accounting op;
/// - `x` is RC-managed, not a `region` local, and the goroutine-share
///   analysis ([`crate::ownership::ShareFacts`]) reports it not
///   goroutine-shared (a shared object carries the `SHARED_BIT` atomic
///   boundary, where another goroutine may concurrently adjust the count,
///   so the balanced pair is load-bearing for that protocol);
/// - the statement directly before the retain reads `x` (the forwarding
///   use whose new reference the retain accounts for) and does not
///   reassign it;
/// - the matching release (the type-paired opposite name) follows in the
///   same block with no other mention of `x` between the two - a tight
///   bracket on one object;
/// - `x` is dead on every path after the release.
///
/// Because the holder created by the forwarding use keeps its own
/// balanced release and `x` is dead, removing both members moves `x`'s
/// single share into that holder without changing the object's reference
/// count at any point outside the bracket. Both members are removed or
/// neither; a missed pair only keeps the original (correct) timing.
/// Drops RC accounting on a place the block just set to the null constant.
/// Every `gos_rt_*` release null-checks its argument, so such a call cannot do
/// anything; removing it is a pure win at the drop-elaboration entry a
/// constructor emits. Block-local and conservative: any write reaching the
/// place, or its root local, forgets the fact.
pub(crate) fn elide_null_rc_accounting(body: &mut Body) {
    for block in &mut body.blocks {
        let mut null_places: Vec<Place> = Vec::new();
        let mut drop_at: Vec<usize> = Vec::new();
        for (index, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if let Rvalue::CallIntrinsic { name, args } = rvalue
                && rc_release_only(name)
                && let [Operand::Copy(target)] = args.as_slice()
                && null_places.iter().any(|known| known == target)
            {
                drop_at.push(index);
                continue;
            }
            // The statement writes `place`; anything previously known about
            // it, or about a place rooted in the same local, no longer holds.
            null_places.retain(|known| known.local != place.local);
            if matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0)))) {
                null_places.push(place.clone());
            }
        }
        for index in drop_at.into_iter().rev() {
            block.stmts.remove(index);
        }
    }
}

/// Whether `name` is an RC release whose argument the runtime null-checks.
fn rc_release_only(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_rc_release"
            | "gos_rt_vec_free"
            | "gos_rt_str_free"
            | "gos_rt_str_free_typed"
            | "gos_rt_map_free"
            | "gos_rt_error_free"
    )
}

pub(crate) fn elide_redundant_rc_pairs(body: &mut Body, tcx: &TyCtxt) {
    let n_blocks = body.blocks.len();
    if n_blocks == 0 {
        return;
    }
    let n_locals = body.locals.len();
    let share = crate::ownership::ShareFacts::compute(body);
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();

    let is_rc_local = |x: Local| -> bool {
        let i = x.0 as usize;
        i < n_locals && tcx.is_rc_managed(body.locals[i].ty) && !body.locals[i].region
    };

    let mut cancels: Vec<(usize, usize, usize)> = Vec::new();
    for bi in 0..n_blocks {
        let stmts = &body.blocks[bi].stmts;
        for ir in 0..stmts.len() {
            let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } = &stmts[ir].kind
            else {
                continue;
            };
            let Some(rel_name) = rc_paired_release(name) else {
                continue;
            };
            let Some(x) = rc_bare_local_arg(args) else {
                continue;
            };
            if !is_rc_local(x) || share.is_goroutine_shared(x) {
                continue;
            }
            // The forwarding use that the retain accounts for must sit
            // immediately before it and read (not reassign) `x`.
            if ir == 0 {
                continue;
            }
            let prev = &stmts[ir - 1];
            if stmt_writes_bare(prev, x) || !stmt_mentions_local(prev, x) {
                continue;
            }
            // Find the paired release: the first matching release of `x`
            // in this block, with no other mention of `x` in between.
            let mut paired: Option<usize> = None;
            for (j, stmt) in stmts.iter().enumerate().skip(ir + 1) {
                if let StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name: dn, args: da },
                    ..
                } = &stmt.kind
                    && *dn == rel_name
                    && rc_bare_local_arg(da) == Some(x)
                {
                    paired = Some(j);
                    break;
                }
                if stmt_mentions_local(stmt, x) {
                    break;
                }
            }
            let Some(id) = paired else {
                continue;
            };
            if local_live_after(body, &succs, bi, id, x) {
                continue;
            }
            // Collector-vs-stack-live contract (F13): a cycle-capable value
            // (a user struct/enum that can hold a back-reference) is a
            // potential trial-deletion candidate. Its retain accounts for the
            // stack reference; cancelling the retain/release pair leaves that
            // reference uncounted, so a trial deletion triggered by an
            // allocation safepoint inside the window would treat the value as
            // cycle-internal and reclaim a still-stack-live member. Keep the
            // pair when an allocation lies between the retain and its release.
            // Strings, vecs, and maps cannot form a collectable cycle, so
            // their pairs still cancel.
            let cycle_capable = matches!(
                body.locals.get(x.0 as usize).map(|d| tcx.kind_of(d.ty)),
                Some(gossamer_types::TyKind::Adt { .. })
            );
            if cycle_capable && block_allocates_between(&body.blocks[bi], ir, id) {
                continue;
            }
            cancels.push((bi, ir, id));
        }
    }
    if !cancels.is_empty() && std::env::var_os("GOS_RC_ELIDE_STATS").is_some() {
        eprintln!(
            "[rc-elide] {}: cancelled {} pair(s)",
            body.name,
            cancels.len()
        );
    }
    for (bi, ir, id) in cancels {
        body.blocks[bi].stmts[ir].kind = StatementKind::Nop;
        body.blocks[bi].stmts[id].kind = StatementKind::Nop;
    }
}

/// True when block `block` performs an RC allocation between statement
/// indices `from` and `to` (exclusive). A `gos_rc_alloc` can trip the
/// cycle collector's allocation-pressure trigger, so it is a collection
/// safepoint within the retain/release window.
fn block_allocates_between(block: &BasicBlock, from: usize, to: usize) -> bool {
    block
        .stmts
        .iter()
        .take(to)
        .skip(from + 1)
        .any(|s| match &s.kind {
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, .. },
                ..
            } => *name == "gos_rc_alloc" || *name == "gos_rc_alloc_tagged",
            _ => false,
        })
}

// ---------------------------------------------------------------------------
// Borrowed-holder RC elision.
// ---------------------------------------------------------------------------

/// The runtime helpers that only read the heap value at argument `index`,
/// leaving both what it reaches and its reference count alone.
fn helper_reads_only(name: &str, index: usize) -> bool {
    match index {
        0 => matches!(
            name,
            "gos_rt_vec_len"
                | "gos_rt_len"
                | "gos_rt_vec_get_i64"
                | "gos_rt_vec_get_i64_unchecked"
                | "gos_rt_vec_get_f64"
                | "gos_rt_vec_get_i128"
                | "gos_rt_vec_get_ptr"
                | "gos_rt_vec_is_empty"
                | "gos_rt_str_len"
                | "gos_rt_str_byte_len"
                | "gos_rt_str_char_at"
                | "gos_rt_str_byte_at"
                | "gos_rt_str_is_empty"
                | "gos_rt_hash_crc32_checksum"
                | "gos_rt_hash_crc32_checksum_string"
                // A two-word carrier reaches these by value, so reading a
                // word out of one cannot write anything the caller holds.
                | "gos_rt_result_disc"
                | "gos_rt_result_payload"
                | "gos_rt_option_unwrap"
                | "gos_rt_result_unwrap"
        ),
        1 => matches!(
            name,
            "gos_rt_hash_crc32_update"
                | "gos_rt_hash_crc32_update_window"
                | "gos_rt_str_push_utf8"
                | "gos_rt_str_concat_drop_a"
                | "gos_rt_str_concat"
        ),
        _ => false,
    }
}

/// The runtime helpers that neither keep nor release a share of the value at
/// argument `index`, so a call through one leaves that value's accounting to
/// whoever owns it.
///
/// Writing through a value and keeping a share of it are separate questions.
/// An element store replaces what a slot holds and releases the outgoing
/// element; the receiver's own count is not what it touches. A carrier reader
/// answers a word out of two the caller already holds.
fn helper_keeps_no_share(name: &str, index: usize) -> bool {
    if helper_reads_only(name, index) {
        return true;
    }
    index == 0
        && matches!(
            name,
            "gos_rt_vec_set_i64" | "gos_rt_vec_set_i64_unchecked" | "gos_rt_vec_push"
        )
}

/// How a holder's share is spelled: the helper names that mint and give it
/// back, and, for a guarded aggregate, the copy-blob descriptor its walk
/// carries as a second argument.
#[derive(Clone)]
struct HolderRc {
    retains: &'static [&'static str],
    releases: &'static [&'static str],
    meta: Option<String>,
}

/// `true` when `ty` is an `Option`/`Result` carrying a guarded aggregate, so
/// a copy of one of its slots is accounted through the option-slot pair.
fn is_guarded_option(tcx: &TyCtxt, ty: Ty) -> bool {
    match tcx.kind_of(ty) {
        TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => substs
            .types()
            .iter()
            .take(2)
            .any(|p| tcx.aggr_copy_meta(*p).is_some()),
        _ => false,
    }
}

/// The accounting a local of `ty` carries as a borrowed holder, or `None` for
/// a type whose copy takes no share at all.
fn holder_rc_names(tcx: &TyCtxt, ty: Ty) -> Option<HolderRc> {
    let plain = |retains: &'static [&'static str], releases: &'static [&'static str]| {
        Some(HolderRc {
            retains,
            releases,
            meta: None,
        })
    };
    match tcx.kind_of(ty) {
        TyKind::String => plain(
            &["gos_rt_str_retain_typed", "gos_rt_str_retain"],
            &["gos_rt_str_free_typed", "gos_rt_str_free"],
        ),
        TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } => {
            plain(&["gos_rt_vec_retain"], &["gos_rt_vec_free"])
        }
        TyKind::Adt { .. } => {
            // A guarded aggregate's copy walks a copy-blob descriptor, which
            // names which of its words are counted. A struct whose fields the
            // lowering books one at a time carries the same share spelled per
            // field, and a type may be reached either way, so a holder of one
            // answers for both spellings.
            let meta = tcx.aggr_copy_meta(ty).map(str::to_string);
            if is_guarded_option(tcx, ty) {
                return Some(HolderRc {
                    retains: &["gos_rt_option_slot_retain"],
                    releases: &["gos_rt_option_slot_release"],
                    meta,
                });
            }
            // Only the field kinds whose share is a count on shared storage
            // qualify - a value container's share is a copy of the storage,
            // which the copy's own death is what returns.
            let fields = crate::lower::aggregate_rc_field_paths(tcx, ty);
            if fields.iter().any(|(_, kind)| {
                !matches!(
                    kind,
                    crate::lower::FieldRcKind::Vec | crate::lower::FieldRcKind::Rc
                )
            }) {
                return None;
            }
            if meta.is_none() && fields.is_empty() {
                return None;
            }
            Some(HolderRc {
                retains: &["gos_rt_vec_retain", "gos_rt_rc_retain"],
                releases: &["gos_rt_vec_free", "gos_rt_rc_release"],
                meta,
            })
        }
        _ => None,
    }
}

fn is_rc_retain_name(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_rc_retain"
            | "gos_rt_vec_retain"
            | "gos_rt_str_retain_typed"
            | "gos_rt_str_retain"
            | "gos_rt_map_retain"
            | "gos_rt_rc_weak_retain"
            | "gos_rt_aggr_retain_children"
            | "gos_rt_option_slot_retain"
    )
}

fn is_rc_release_name(name: &str) -> bool {
    rc_release_only(name)
        || matches!(
            name,
            "gos_rt_rc_weak_release"
                | "gos_rt_map_field_release"
                | "gos_rt_aggr_release_children"
                | "gos_rt_option_slot_release"
        )
}

/// The bare local and copy-blob descriptor a guarded-walk accounting call
/// names, or `None` for any other argument shape.
fn rc_walk_local_arg(args: &[Operand]) -> Option<(Local, &str)> {
    if let [Operand::Copy(p), Operand::Const(ConstValue::Str(meta))] = args
        && p.projection.is_empty()
    {
        return Some((p.local, meta.as_str()));
    }
    None
}

/// How a local is written, for resolving which value it aliases.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AliasDef {
    /// Never written other than by a constant.
    None,
    /// One write, copying `Place` (bare or projected).
    Copy(Local),
    /// One write, the element address `gos_rt_vec_get_ptr` answers for the Vec in `Local`.
    ElementPtr(Local),
    /// One write, the payload word held in the carrier `Local`. The carrier's
    /// slot is what owns the value; the extraction mints nothing.
    Payload(Local),
    /// One write of some other shape, or several writes.
    Opaque,
}

/// `true` when the helper answers a view of the payload its carrier argument
/// holds, rather than a value of its own.
fn extracts_carrier_payload(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_result_payload" | "gos_rt_option_unwrap" | "gos_rt_result_unwrap"
    )
}

fn alias_defs(body: &Body) -> Vec<AliasDef> {
    let mut defs = vec![AliasDef::None; body.locals.len()];
    let mut note = |local: Local, def: AliasDef| {
        let slot = &mut defs[local.0 as usize];
        *slot = if *slot == AliasDef::None { def } else { AliasDef::Opaque };
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && place.projection.is_empty()
            {
                match rvalue {
                    Rvalue::Use(Operand::Const(_)) => {}
                    Rvalue::Use(Operand::Copy(src)) if src.local != place.local => {
                        note(place.local, AliasDef::Copy(src.local));
                    }
                    Rvalue::CallIntrinsic { name, args }
                        if extracts_carrier_payload(name)
                            && rc_bare_local_arg(args).is_some_and(|c| c != place.local) =>
                    {
                        let carrier = rc_bare_local_arg(args).unwrap_or(place.local);
                        note(place.local, AliasDef::Payload(carrier));
                    }
                    _ => note(place.local, AliasDef::Opaque),
                }
            }
        }
        if let Terminator::Call {
            callee,
            args,
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
        {
            let def = match (callee, args.first()) {
                (Operand::Const(ConstValue::Str(name)), Some(Operand::Copy(vec)))
                    if name == "gos_rt_vec_get_ptr" =>
                {
                    AliasDef::ElementPtr(vec.local)
                }
                (Operand::Const(ConstValue::Str(name)), Some(Operand::Copy(carrier)))
                    if extracts_carrier_payload(name)
                        && carrier.projection.is_empty()
                        && carrier.local != destination.local =>
                {
                    AliasDef::Payload(carrier.local)
                }
                _ => AliasDef::Opaque,
            };
            note(destination.local, def);
        }
    }
    defs
}

/// Every value a local may take on: the locals a write copies or extracts a
/// payload out of, and whether any write is of a shape this analysis cannot
/// follow. A constant write contributes neither.
struct DefEdges {
    sources: Vec<Local>,
    opaque: bool,
}

fn def_edges(body: &Body) -> Vec<DefEdges> {
    let mut out: Vec<DefEdges> = (0..body.locals.len())
        .map(|_| DefEdges {
            sources: Vec::new(),
            opaque: false,
        })
        .collect();
    let mut note = |local: Local, source: Option<Local>| {
        let slot = &mut out[local.0 as usize];
        match source {
            Some(src) if src != local => {
                if !slot.sources.contains(&src) {
                    slot.sources.push(src);
                }
            }
            _ => slot.opaque = true,
        }
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && place.projection.is_empty()
            {
                match rvalue {
                    Rvalue::Use(Operand::Const(_)) => {}
                    Rvalue::Use(Operand::Copy(src)) => note(place.local, Some(src.local)),
                    Rvalue::CallIntrinsic { name, args } if extracts_carrier_payload(name) => {
                        note(place.local, rc_bare_local_arg(args));
                    }
                    _ => note(place.local, None),
                }
            }
        }
        if let Terminator::Call {
            callee,
            args,
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
        {
            let source = match (callee, args.first()) {
                (Operand::Const(ConstValue::Str(name)), Some(Operand::Copy(src)))
                    if (name == "gos_rt_vec_get_ptr" || extracts_carrier_payload(name))
                        && src.projection.is_empty() =>
                {
                    Some(src.local)
                }
                _ => None,
            };
            note(destination.local, source);
        }
    }
    out
}

/// The local whose value `local` is an alias of: the origin of its chain of
/// copies and element addresses, or `local` itself.
fn alias_root(defs: &[AliasDef], local: Local) -> Local {
    let mut current = local;
    let mut steps = 0;
    loop {
        let next = match defs[current.0 as usize] {
            AliasDef::Copy(src) | AliasDef::ElementPtr(src) | AliasDef::Payload(src) => src,
            AliasDef::None | AliasDef::Opaque => return current,
        };
        steps += 1;
        if next == current || steps > defs.len() {
            return current;
        }
        current = next;
    }
}

fn operand_in_chain(op: &Operand, chain: &[bool]) -> bool {
    matches!(op, Operand::Copy(p) if chain[p.local.0 as usize])
}

/// What a call's callee is, for deciding whether an argument it reads could
/// be written through.
enum CalleeKind<'a> {
    User,
    Runtime(&'a str),
    Unknown,
}

fn callee_kind(callee: &Operand) -> CalleeKind<'_> {
    match callee {
        Operand::Const(ConstValue::Str(name)) => {
            if name.starts_with("gos_rt_") || name.starts_with("gos_rc") {
                CalleeKind::Runtime(name)
            } else {
                CalleeKind::User
            }
        }
        Operand::FnRef { .. } => CalleeKind::User,
        _ => CalleeKind::Unknown,
    }
}

/// `true` when a call passing `place` at argument `index` to `callee` could
/// write the value the place reaches. A user function is handed by-value
/// arguments as borrows or as clones and can write only through a reference,
/// so a bare reference-typed local is the one by-value shape that reaches it
/// writable. A runtime helper is a borrow only where it is known to read.
fn call_arg_may_write(
    body: &Body,
    tcx: &TyCtxt,
    callee: &CalleeKind<'_>,
    index: usize,
    place: &Place,
) -> bool {
    match callee {
        CalleeKind::User => {
            place.projection.is_empty()
                && matches!(
                    tcx.kind_of(body.locals[place.local.0 as usize].ty),
                    TyKind::Ref { .. }
                )
        }
        CalleeKind::Runtime(name) => !helper_reads_only(name, index),
        CalleeKind::Unknown => true,
    }
}

/// `true` when `stmt` could change or release the structure reached through
/// the chain rooted at `root`. Retains never do; a release of the root or of
/// an alias other than the holder itself may.
fn stmt_disturbs_chain(
    stmt: &Statement,
    chain: &[bool],
    root: Local,
    is_member: &[bool],
) -> bool {
    let in_chain = |l: Local| chain[l.0 as usize];
    match &stmt.kind {
        StatementKind::Assign { place, rvalue } => {
            if in_chain(place.local) && (place.local == root || !place.projection.is_empty()) {
                return true;
            }
            match rvalue {
                Rvalue::Ref { place: p, .. } => in_chain(p.local),
                Rvalue::CallIntrinsic { name, args } => {
                    if is_rc_retain_name(name) {
                        false
                    } else if is_rc_release_name(name) {
                        args.iter().any(|a| match a {
                            Operand::Copy(p) => in_chain(p.local) && !is_member[p.local.0 as usize],
                            _ => false,
                        })
                    } else {
                        args.iter().enumerate().any(|(i, a)| {
                            operand_in_chain(a, chain) && !helper_reads_only(name, i)
                        })
                    }
                }
                _ => false,
            }
        }
        StatementKind::SetDiscriminant { place, .. } => in_chain(place.local),
        StatementKind::StaticStore { value, .. } => operand_in_chain(value, chain),
        StatementKind::IterSource { dst, source, .. } => {
            in_chain(dst.local) || operand_in_chain(source, chain)
        }
        StatementKind::IterAdapter {
            dst,
            upstream,
            closure_or_arg,
            ..
        } => {
            in_chain(dst.local)
                || in_chain(upstream.local)
                || closure_or_arg
                    .as_ref()
                    .is_some_and(|a| operand_in_chain(a, chain))
        }
        StatementKind::IterNext {
            dst_option,
            iter_place,
            ..
        } => in_chain(dst_option.local) || in_chain(iter_place.local),
        StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => false,
    }
}

fn term_disturbs_chain(body: &Body, tcx: &TyCtxt, t: &Terminator, chain: &[bool], root: Local) -> bool {
    let in_chain = |l: Local| chain[l.0 as usize];
    match t {
        Terminator::Call {
            callee,
            args,
            destination,
            ..
        } => {
            if in_chain(destination.local)
                && (destination.local == root || !destination.projection.is_empty())
            {
                return true;
            }
            let kind = callee_kind(callee);
            if matches!(kind, CalleeKind::Unknown) {
                return true;
            }
            args.iter().enumerate().any(|(i, a)| match a {
                Operand::Copy(p) if in_chain(p.local) => call_arg_may_write(body, tcx, &kind, i, p),
                _ => false,
            })
        }
        Terminator::Drop { place, .. } => in_chain(place.local),
        _ => false,
    }
}

/// `true` when `stmt` is one of `holder`'s own accounting calls.
///
/// A guarded aggregate's calls are keyed by the descriptor they carry: one
/// naming a different copy blob accounts for something else. `zero_guarded`
/// joins them because it clears the very words those releases read.
fn stmt_is_rc_op_on(stmt: &Statement, holder: Local, rc: &HolderRc) -> bool {
    let StatementKind::Assign {
        rvalue: Rvalue::CallIntrinsic { name, args },
        ..
    } = &stmt.kind
    else {
        return false;
    };
    if let Some(meta) = &rc.meta
        && matches!(
            *name,
            "gos_rt_aggr_retain_children"
                | "gos_rt_aggr_release_children"
                | "gos_rt_aggr_zero_guarded"
        )
    {
        return rc_walk_local_arg(args) == Some((holder, meta.as_str()));
    }
    (is_rc_retain_name(name) || is_rc_release_name(name)) && rc_arg_names_holder(args, holder)
}

/// `true` when `stmt` reads `holder` in a shape a borrow answers: a
/// projected read, a call argument through a borrowing helper or a user
/// function, or storage bookkeeping. A bare copy into another place, an
/// aggregate capture, or a reference to it needs the holder's own share.
fn stmt_use_is_borrow(stmt: &Statement, holder: Local, is_member: &[bool]) -> bool {
    let bare = |op: &Operand| matches!(op, Operand::Copy(p) if p.local == holder && p.projection.is_empty());
    match &stmt.kind {
        StatementKind::Assign { place, rvalue } => {
            // A bare copy into another member is accounted by that member's
            // own bracket, which this pass removes along with this one.
            if is_member[place.local.0 as usize]
                && place.projection.is_empty()
                && matches!(rvalue, Rvalue::Use(op) if bare(op))
            {
                return true;
            }
            if place.local == holder {
                // A constant written into the holder - the word itself, or a
                // guarded field's zero - is bookkeeping on the copy, never a
                // read of the value it borrows. Ordering is the window walk's
                // question, which counts such a write as a disturbance.
                return matches!(rvalue, Rvalue::Use(Operand::Const(_)));
            }
            match rvalue {
                Rvalue::Use(op) | Rvalue::UnaryOp { operand: op, .. } | Rvalue::Cast { operand: op, .. } => {
                    !bare(op)
                }
                Rvalue::BinaryOp { lhs, rhs, .. } => !bare(lhs) && !bare(rhs),
                Rvalue::Len(_) => true,
                Rvalue::CallIntrinsic { name, args } => args
                    .iter()
                    .enumerate()
                    .all(|(i, a)| !bare(a) || helper_keeps_no_share(name, i)),
                Rvalue::Aggregate { .. } | Rvalue::Repeat { .. } | Rvalue::Ref { .. } => false,
                Rvalue::StaticLoad(_) => true,
            }
        }
        StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => true,
        StatementKind::SetDiscriminant { .. }
        | StatementKind::StaticStore { .. }
        | StatementKind::IterSource { .. }
        | StatementKind::IterAdapter { .. }
        | StatementKind::IterNext { .. } => false,
    }
}

fn term_use_is_borrow(t: &Terminator, holder: Local, _is_member: &[bool]) -> bool {
    let bare = |op: &Operand| matches!(op, Operand::Copy(p) if p.local == holder && p.projection.is_empty());
    match t {
        Terminator::Call {
            callee,
            args,
            destination,
            ..
        } => {
            if destination.local == holder || bare(callee) {
                return false;
            }
            let kind = callee_kind(callee);
            args.iter().enumerate().all(|(i, a)| {
                !bare(a)
                    || match kind {
                        CalleeKind::User => true,
                        CalleeKind::Runtime(name) => helper_keeps_no_share(name, i),
                        CalleeKind::Unknown => false,
                    }
            })
        }
        Terminator::SwitchInt { discriminant, .. } => !bare(discriminant),
        Terminator::Assert { cond, .. } => !bare(cond),
        Terminator::Drop { place, .. } => place.local != holder,
        Terminator::Goto { .. }
        | Terminator::Return
        | Terminator::Unreachable
        | Terminator::Panic { .. } => true,
    }
}

/// A set of locals that only ever name values one local outside the set owns,
/// together with that root and the accounting each member carries.
///
/// A lone holder is the one-member case. Where several locals copy from one
/// another - a walk that rebinds its cursor from a child and back to the root -
/// no single one of them has a definition the others do not feed, so the
/// redundancy argument is made for the set at once.
struct Class {
    members: Vec<Local>,
    is_member: Vec<bool>,
    rcs: Vec<Option<HolderRc>>,
    root: Local,
    chain: Vec<bool>,
}

impl Class {
    fn rc_of(&self, member: Local) -> &HolderRc {
        self.rcs[member.0 as usize]
            .as_ref()
            .expect("a class member carries accounting")
    }

    /// `true` when `stmt` is one of the class's own accounting calls.
    fn owns_rc_op(&self, stmt: &Statement) -> bool {
        self.members
            .iter()
            .any(|&m| stmt_is_rc_op_on(stmt, m, self.rc_of(m)))
    }
}

/// Walks forward from the holder's definition and answers `true` when every
/// read of the holder happens before anything could disturb the structure
/// it borrows from. A bare write of the holder ends the window on that path.
fn holder_window_is_clean(
    body: &Body,
    tcx: &TyCtxt,
    succs: &[Vec<usize>],
    def: HolderDef,
    holder: Local,
    class: &Class,
) -> bool {
    let root = class.root;
    let chain = &class.chain[..];
    let n = body.blocks.len();
    let mut visited = vec![[false; 2]; n];
    // A terminator definition is the last thing in its block, so the window
    // opens in each successor rather than after a statement.
    let mut stack: Vec<(usize, usize, bool)> = match def {
        HolderDef::Stmt(block, index) => vec![(block, index + 1, false)],
        HolderDef::Term(block) => succs[block].iter().map(|&s| (s, 0, false)).collect(),
    };
    if let HolderDef::Term(block) = def {
        for &s in &succs[block] {
            visited[s][0] = true;
        }
    }
    while let Some((b, from, mut dirty)) = stack.pop() {
        let blk = &body.blocks[b];
        let mut ended = false;
        for stmt in &blk.stmts[from..] {
            if class.owns_rc_op(stmt) {
                continue;
            }
            if stmt_writes_bare(stmt, holder) {
                ended = true;
                break;
            }
            if stmt_mentions_local(stmt, holder) && dirty {
                return false;
            }
            if stmt_disturbs_chain(stmt, chain, root, &class.is_member) {
                dirty = true;
            }
        }
        if ended {
            continue;
        }
        let t = &blk.terminator;
        if term_mentions_local(t, holder) && dirty {
            return false;
        }
        if term_writes_bare(t, holder) {
            continue;
        }
        if term_disturbs_chain(body, tcx, t, chain, root) {
            dirty = true;
        }
        for &s in &succs[b] {
            if !visited[s][usize::from(dirty)] {
                visited[s][usize::from(dirty)] = true;
                stack.push((s, 0, dirty));
            }
        }
    }
    true
}

/// Where a holder's one non-constant definition sits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HolderDef {
    /// Statement `.1` of block `.0`.
    Stmt(usize, usize),
    /// The terminator of block `.0`, whose destination is the holder.
    Term(usize),
}

/// Where a holder is defined and how many accounting calls it carries.
struct HolderUses {
    defs: Vec<HolderDef>,
    retains: usize,
    releases: usize,
}

/// What one statement is, for a holder: an accounting call it carries, a
/// definition of it, or neither.
enum HolderStmt {
    Retain,
    Release,
    /// An accounting call of the holder's family that mints nothing.
    Bookkeeping,
    Define,
    Other,
    /// A shape that disqualifies the holder outright.
    Reject,
}

/// Classifies one statement against `holder`.
fn classify_holder_stmt(
    stmt: &Statement,
    holder: Local,
    rc: &HolderRc,
    feeds_holder: &impl Fn(Local) -> bool,
) -> HolderStmt {
    let StatementKind::Assign { place, rvalue } = &stmt.kind else {
        return HolderStmt::Other;
    };
    let names_holder = place.local == holder && place.projection.is_empty();
    match rvalue {
        Rvalue::CallIntrinsic { name, args } => {
            if matches!(
                *name,
                "gos_rt_aggr_retain_children"
                    | "gos_rt_aggr_release_children"
                    | "gos_rt_aggr_zero_guarded"
            ) {
                let Some((local, carried)) = rc_walk_local_arg(args) else {
                    return HolderStmt::Other;
                };
                if local != holder {
                    return HolderStmt::Other;
                }
                // A walk over this local under another descriptor is not this
                // holder's accounting, and pairing across two blobs would
                // unbalance both.
                if rc.meta.as_deref() != Some(carried) {
                    return HolderStmt::Reject;
                }
                return match *name {
                    "gos_rt_aggr_retain_children" => HolderStmt::Retain,
                    "gos_rt_aggr_release_children" => HolderStmt::Release,
                    _ => HolderStmt::Bookkeeping,
                };
            }
            // A payload read out of a carrier the class borrows from is a
            // definition: the words stay the carrier's.
            if names_holder && extracts_carrier_payload(name) {
                return if rc_bare_local_arg(args).is_some_and(feeds_holder) {
                    HolderStmt::Define
                } else {
                    HolderStmt::Reject
                };
            }
            if rc_arg_names_holder(args, holder) {
                if rc.retains.contains(name) {
                    return HolderStmt::Retain;
                }
                if rc.releases.contains(name) {
                    return HolderStmt::Release;
                }
            }
            HolderStmt::Other
        }
        Rvalue::Use(Operand::Copy(src)) if names_holder => {
            if feeds_holder(src.local) {
                HolderStmt::Define
            } else {
                HolderStmt::Reject
            }
        }
        _ => HolderStmt::Other,
    }
}

/// Classifies every mention of `holder`: its definitions out of the class or
/// its root, its retains and releases, and reads that are borrows. `None` when
/// any mention is of another shape.
fn classify_holder_uses(
    body: &Body,
    holder: Local,
    rc: &HolderRc,
    is_member: &[bool],
    chain: &[bool],
) -> Option<HolderUses> {
    let mut defs = Vec::new();
    let mut retains = 0usize;
    let mut releases = 0usize;
    // A definition may name another member of the class, the local the class
    // borrows from, or any view of that local's value; nothing else.
    let feeds_holder = |src: Local| is_member[src.0 as usize] || chain[src.0 as usize];
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            match classify_holder_stmt(stmt, holder, rc, &feeds_holder) {
                HolderStmt::Reject => return None,
                HolderStmt::Retain => retains += 1,
                HolderStmt::Release => releases += 1,
                HolderStmt::Bookkeeping => {}
                HolderStmt::Define => defs.push(HolderDef::Stmt(bi, si)),
                HolderStmt::Other => {
                    if stmt_mentions_local(stmt, holder)
                        && !stmt_use_is_borrow(stmt, holder, is_member)
                    {
                        return None;
                    }
                }
            }
        }
        let t = &block.terminator;
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            ..
        } = t
            && destination.local == holder
            && destination.projection.is_empty()
            && extracts_carrier_payload(name)
        {
            if !rc_bare_local_arg(args).is_some_and(&feeds_holder) {
                return None;
            }
            defs.push(HolderDef::Term(bi));
            continue;
        }
        if term_mentions_local(t, holder) && !term_use_is_borrow(t, holder, is_member) {
            return None;
        }
    }
    if defs.is_empty() {
        return None;
    }
    Some(HolderUses {
        defs,
        retains,
        releases,
    })
}

/// The guarded-walk call a statement makes on a bare local, as
/// `(name, local, meta)`.
fn guarded_walk_call(stmt: &Statement) -> Option<(&str, Local, &str)> {
    let StatementKind::Assign {
        rvalue: Rvalue::CallIntrinsic { name, args },
        ..
    } = &stmt.kind
    else {
        return None;
    };
    if !matches!(
        *name,
        "gos_rt_aggr_retain_children" | "gos_rt_aggr_release_children" | "gos_rt_aggr_zero_guarded"
    ) {
        return None;
    }
    let (local, meta) = rc_walk_local_arg(args)?;
    Some((name, local, meta))
}

/// The index of the next statement that is not a `Nop`.
fn next_live_stmt(stmts: &[Statement], from: usize) -> Option<usize> {
    (from..stmts.len()).find(|&i| !matches!(stmts[i].kind, StatementKind::Nop))
}

/// Whether the source of a move still holds children anything reads after
/// `at`.
///
/// A release of the source is what the move gives up; anything that reads its
/// slots for their children afterwards would then read words the destination
/// now owns. A guarded zero settles them: every later walk over the source is
/// a no-op on zeroed words, and a redefinition ends the question.
fn source_is_settled_after(
    body: &Body,
    succs: &[Vec<usize>],
    at: (usize, usize),
    source: Local,
    meta: &str,
) -> bool {
    let n = body.blocks.len();
    let mut visited = vec![[false; 2]; n];
    let mut stack = vec![(at.0, at.1 + 1, false)];
    while let Some((b, from, mut zeroed)) = stack.pop() {
        let blk = &body.blocks[b];
        let mut ended = false;
        for stmt in &blk.stmts[from..] {
            match guarded_walk_call(stmt) {
                Some(("gos_rt_aggr_zero_guarded", local, carried))
                    if local == source && carried == meta =>
                {
                    zeroed = true;
                    continue;
                }
                Some(("gos_rt_aggr_release_children", local, carried))
                    if local == source && carried == meta =>
                {
                    // Only a walk the zero has already emptied may follow.
                    if !zeroed {
                        return false;
                    }
                    continue;
                }
                _ => {}
            }
            if stmt_writes_bare(stmt, source) {
                ended = true;
                break;
            }
            if stmt_mentions_local(stmt, source) {
                return false;
            }
        }
        if ended {
            continue;
        }
        let t = &blk.terminator;
        if term_writes_bare(t, source) {
            continue;
        }
        if term_mentions_local(t, source) {
            return false;
        }
        for &s in &succs[b] {
            if !visited[s][usize::from(zeroed)] {
                visited[s][usize::from(zeroed)] = true;
                stack.push((s, 0, zeroed));
            }
        }
    }
    true
}

/// The operand locals of an `Aggregate` rvalue, or `None` when any operand is
/// something other than a bare local or a constant.
fn aggregate_operand_locals(rvalue: &Rvalue) -> Option<Vec<Local>> {
    let Rvalue::Aggregate { operands, .. } = rvalue else {
        return None;
    };
    let mut out = Vec::new();
    for operand in operands {
        match operand {
            Operand::Const(_) => {}
            Operand::Copy(place) if place.projection.is_empty() => out.push(place.local),
            _ => return None,
        }
    }
    Some(out)
}

/// Where the share an operand handed to an aggregate is given back, or `None`
/// when the operand is read again, is written again, or reaches a return with
/// its share still in hand.
///
/// On every path out of the aggregate the operand's own accounting has to end
/// at exactly one release, with nothing reading its children on the way and
/// nothing touching it afterwards. That is what makes the aggregate's retain
/// and those releases one share moving between two holders.
fn operand_share_moves_out(
    body: &Body,
    succs: &[Vec<usize>],
    at: (usize, usize),
    operand: Local,
    rc: &HolderRc,
) -> Option<Vec<(usize, usize)>> {
    let is_release = |stmt: &Statement| -> bool {
        let StatementKind::Assign {
            rvalue: Rvalue::CallIntrinsic { name, args },
            ..
        } = &stmt.kind
        else {
            return false;
        };
        if !rc.releases.contains(name) {
            return false;
        }
        match &rc.meta {
            Some(meta) => rc_walk_local_arg(args) == Some((operand, meta.as_str())),
            None => rc_arg_names_holder(args, operand),
        }
    };
    let mut sites = Vec::new();
    let mut visited = vec![[false; 2]; body.blocks.len()];
    let mut stack = vec![(at.0, at.1 + 1, false)];
    while let Some((b, from, mut found)) = stack.pop() {
        let blk = &body.blocks[b];
        let mut ended = false;
        for (si, stmt) in blk.stmts.iter().enumerate().skip(from) {
            if !found && is_release(stmt) {
                if !sites.contains(&(b, si)) {
                    sites.push((b, si));
                }
                found = true;
                continue;
            }
            if stmt_writes_bare(stmt, operand) {
                if !found {
                    return None;
                }
                ended = true;
                break;
            }
            if stmt_mentions_local(stmt, operand) {
                return None;
            }
        }
        if ended {
            continue;
        }
        let t = &blk.terminator;
        if term_writes_bare(t, operand) {
            if !found {
                return None;
            }
            continue;
        }
        if term_mentions_local(t, operand) {
            return None;
        }
        if succs[b].is_empty() {
            if !found {
                return None;
            }
            continue;
        }
        for &s in &succs[b] {
            if !visited[s][usize::from(found)] {
                visited[s][usize::from(found)] = true;
                stack.push((s, 0, found));
            }
        }
    }
    (!sites.is_empty()).then_some(sites)
}

/// The counting sibling of a materialising runtime call: same answer when all
/// the caller wanted was how many, without building any of them.
fn counting_sibling(name: &str) -> Option<&'static str> {
    match name {
        "gos_rt_regex_find_all" => Some("gos_rt_regex_count"),
        _ => None,
    }
}

/// Rewrites a materialising call whose one reader is `len()` into the call
/// that answers the count.
///
/// Building a sequence to ask how long it is allocates one element per match
/// and frees them again. Where the sequence local is read exactly once, by
/// `gos_rt_vec_len`, and otherwise only freed, the counting sibling answers
/// the same number without any of that.
pub(crate) fn reduce_materialised_counts(body: &mut Body) {
    let n_locals = body.locals.len();
    if n_locals == 0 {
        return;
    }
    // Per local: the one materialising definition, the one length read, the
    // frees, and whether anything else mentions it.
    let mut definition: Vec<Option<(usize, usize)>> = vec![None; n_locals];
    let mut length_read: Vec<Option<(usize, usize)>> = vec![None; n_locals];
    let mut frees: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n_locals];
    let mut disqualified = vec![false; n_locals];
    let mut mentions = vec![0usize; n_locals];
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                for (local, flag) in disqualified.iter_mut().enumerate() {
                    if stmt_mentions_local(stmt, Local(local as u32)) {
                        *flag = true;
                    }
                }
                continue;
            };
            if let Rvalue::CallIntrinsic { name, args } = rvalue {
                if counting_sibling(name).is_some() && place.projection.is_empty() {
                    if definition[place.local.0 as usize]
                        .replace((bi, si))
                        .is_some()
                    {
                        disqualified[place.local.0 as usize] = true;
                    }
                } else if *name == "gos_rt_vec_len"
                    && let Some(read) = rc_bare_local_arg(args)
                {
                    mentions[read.0 as usize] += 1;
                    if length_read[read.0 as usize].replace((bi, si)).is_some() {
                        disqualified[read.0 as usize] = true;
                    }
                    continue;
                } else if *name == "gos_rt_vec_free"
                    && let Some(freed) = rc_bare_local_arg(args)
                {
                    frees[freed.0 as usize].push((bi, si));
                    continue;
                }
            }
            for (local, count) in mentions.iter_mut().enumerate() {
                if local != place.local.0 as usize
                    && stmt_mentions_local(stmt, Local(local as u32))
                {
                    *count += 1;
                }
            }
        }
        for (local, flag) in disqualified.iter_mut().enumerate() {
            if term_mentions_local(&block.terminator, Local(local as u32)) {
                *flag = true;
            }
        }
    }
    for (local, blocked) in disqualified.iter().enumerate() {
        if *blocked || mentions[local] != 1 {
            continue;
        }
        let (Some((dbi, dsi)), Some((lbi, lsi))) = (definition[local], length_read[local]) else {
            continue;
        };
        let StatementKind::Assign {
            rvalue: Rvalue::CallIntrinsic { name, args },
            ..
        } = &body.blocks[dbi].stmts[dsi].kind
        else {
            continue;
        };
        let Some(counting) = counting_sibling(name) else {
            continue;
        };
        let args = args.clone();
        // The count lands where the length read did, so every reader of the
        // length sees the same value from the same place.
        let StatementKind::Assign { place, .. } = body.blocks[lbi].stmts[lsi].kind.clone() else {
            continue;
        };
        body.blocks[lbi].stmts[lsi].kind = StatementKind::Assign {
            place,
            rvalue: Rvalue::CallIntrinsic {
                name: counting,
                args,
            },
        };
        body.blocks[dbi].stmts[dsi].kind = StatementKind::Nop;
        for (fbi, fsi) in &frees[local] {
            body.blocks[*fbi].stmts[*fsi].kind = StatementKind::Nop;
        }
    }
}

/// Hands an `Option` / `Result` carrier the share its payload was holding.
///
/// Boxing a guarded aggregate into a carrier copies its words and gives the
/// box a share of every child those words name; the walk that follows gives
/// the source's back. Where the source is settled straight after, the two are
/// one share moving into the box, which the owning constructor takes without
/// minting.
fn move_payload_shares_into_carriers(body: &mut Body, tcx: &TyCtxt) {
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();
    let share = crate::ownership::ShareFacts::compute(body);
    let mut moves: Vec<(usize, usize, usize)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } = &stmt.kind
            else {
                continue;
            };
            if *name != "gos_rt_result_new" || args.len() != 2 {
                continue;
            }
            let Some(Operand::Copy(payload)) = args.get(1) else {
                continue;
            };
            if !payload.projection.is_empty() {
                continue;
            }
            let source = payload.local;
            if share.is_goroutine_shared(source) || body.locals[source.0 as usize].region {
                continue;
            }
            let ty = body.locals[source.0 as usize].ty;
            let Some(rc) = holder_rc_names(tcx, ty) else {
                continue;
            };
            let Some(meta) = rc.meta.clone() else {
                continue;
            };
            let Some(ri) = next_live_stmt(&block.stmts, si + 1) else {
                continue;
            };
            let Some(("gos_rt_aggr_release_children", released, carried)) =
                guarded_walk_call(&block.stmts[ri])
            else {
                continue;
            };
            if released != source || carried != meta {
                continue;
            }
            if !source_is_settled_after(body, &succs, (bi, ri), source, &meta) {
                continue;
            }
            moves.push((bi, si, ri));
        }
    }
    for (bi, si, ri) in moves {
        if let StatementKind::Assign {
            rvalue: Rvalue::CallIntrinsic { name, .. },
            ..
        } = &mut body.blocks[bi].stmts[si].kind
        {
            *name = "gos_rt_result_new_owned";
        }
        body.blocks[bi].stmts[ri].kind = StatementKind::Nop;
    }
}

/// Cancels the accounting a whole-aggregate move leaves behind.
///
/// `dst = src` copies the words, so both slots name the same children while
/// the source is still live. The pair the drop schedule emits around such a
/// copy - a walk that gives the destination a share and a walk that gives the
/// source's back - moves one share between two holders that hold it at once,
/// which is what the destination inherits when neither runs. The children keep
/// the count the source already had, so nothing outside the pair sees a
/// different number.
pub(crate) fn elide_moved_aggregate_shares(body: &mut Body, tcx: &TyCtxt) {
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();
    let mut cancelled: Vec<(usize, usize)> = Vec::new();
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
            let (dst, source) = (place.local, src.local);
            if dst == source {
                continue;
            }
            let Some(ri) = next_live_stmt(&block.stmts, si + 1) else {
                continue;
            };
            let Some(("gos_rt_aggr_retain_children", retained, meta)) =
                guarded_walk_call(&block.stmts[ri])
            else {
                continue;
            };
            if retained != dst {
                continue;
            }
            let Some(ji) = next_live_stmt(&block.stmts, ri + 1) else {
                continue;
            };
            let Some(("gos_rt_aggr_release_children", released, released_meta)) =
                guarded_walk_call(&block.stmts[ji])
            else {
                continue;
            };
            if released != source || released_meta != meta {
                continue;
            }
            if !source_is_settled_after(body, &succs, (bi, ji), source, meta) {
                continue;
            }
            cancelled.push((bi, ri));
            cancelled.push((bi, ji));
        }
    }
    for (bi, si) in cancelled {
        body.blocks[bi].stmts[si].kind = StatementKind::Nop;
    }
    elide_shares_moved_into_aggregates(body, tcx);
    move_payload_shares_into_carriers(body, tcx);
}

/// Cancels the share an aggregate constructor mints for children its operands
/// were already holding.
///
/// `dst = Aggregate{ .. }` writes each operand's words into the new value, and
/// the walk that follows gives the new value a share of every guarded child
/// those words name. Where each such operand then gives its own share back
/// with nothing reading it in between, the two are one share moving from the
/// operand's slot into the aggregate's, which is what the aggregate holds when
/// neither is booked.
fn elide_shares_moved_into_aggregates(body: &mut Body, tcx: &TyCtxt) {
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();
    let share = crate::ownership::ShareFacts::compute(body);
    let mut cancelled: Vec<(usize, usize)> = Vec::new();
    for bi in 0..body.blocks.len() {
        for si in 0..body.blocks[bi].stmts.len() {
            let StatementKind::Assign { place, rvalue } = &body.blocks[bi].stmts[si].kind else {
                continue;
            };
            if !place.projection.is_empty() {
                continue;
            }
            let destination = place.local;
            let Some(operands) = aggregate_operand_locals(rvalue) else {
                continue;
            };
            let Some(ri) = next_live_stmt(&body.blocks[bi].stmts, si + 1) else {
                continue;
            };
            let Some(("gos_rt_aggr_retain_children", retained, _)) =
                guarded_walk_call(&body.blocks[bi].stmts[ri])
            else {
                continue;
            };
            if retained != destination {
                continue;
            }
            // Only the operands whose own words the walk reaches carry a share
            // for it to mint; a scalar or a container field is booked
            // elsewhere and is left alone.
            let mut guarded: Vec<(Local, HolderRc)> = Vec::new();
            let mut usable = true;
            for &operand in &operands {
                let ty = body.locals[operand.0 as usize].ty;
                let Some(rc) = holder_rc_names(tcx, ty) else {
                    continue;
                };
                if rc.meta.is_none() && !is_guarded_option(tcx, ty) {
                    continue;
                }
                if guarded.iter().any(|(seen, _)| *seen == operand)
                    || share.is_goroutine_shared(operand)
                    || body.locals[operand.0 as usize].region
                {
                    usable = false;
                    break;
                }
                guarded.push((operand, rc));
            }
            if !usable || guarded.is_empty() {
                continue;
            }
            let mut sites = vec![(bi, ri)];
            for (operand, rc) in &guarded {
                if let Some(found) = operand_share_moves_out(body, &succs, (bi, ri), *operand, rc)
                {
                    sites.extend(found);
                } else {
                    usable = false;
                    break;
                }
            }
            if usable {
                cancelled.extend(sites);
            }
        }
    }
    for (bi, si) in cancelled {
        body.blocks[bi].stmts[si].kind = StatementKind::Nop;
    }
}

/// `true` when `stmt` gives `local` words of its own: a write to it, bare or
/// projected, or a reference through which one could be written.
fn stmt_refills(stmt: &Statement, local: Local) -> bool {
    match &stmt.kind {
        StatementKind::Assign { place, rvalue } => {
            if place.local == local {
                return true;
            }
            matches!(rvalue, Rvalue::Ref { place: p, .. } if p.local == local)
        }
        StatementKind::SetDiscriminant { place, .. } => place.local == local,
        StatementKind::IterSource { dst, .. } => dst.local == local,
        StatementKind::IterAdapter { dst, .. } => dst.local == local,
        StatementKind::IterNext { dst_option, .. } => dst_option.local == local,
        _ => false,
    }
}

fn term_refills(t: &Terminator, local: Local) -> bool {
    matches!(t, Terminator::Call { destination, .. } if destination.local == local)
}

/// Removes the guarded walks that read words already known to be zero, and
/// the zeros whose words nothing reads.
///
/// The lowering brackets each guarded local with a zero at entry so the
/// release standing before its first assignment walks nothing, and with a zero
/// after a move so every later release walks nothing either. Where the
/// analysis can see that a release stands on zeroed words the call does
/// nothing, and where every reader of a zero has gone the zero itself writes
/// words no one reads.
pub(crate) fn elide_settled_guarded_walks(body: &mut Body) {
    let n_blocks = body.blocks.len();
    let n_locals = body.locals.len();
    if n_blocks == 0 || n_locals == 0 {
        return;
    }
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();

    // Must-analysis: a local is zeroed at a point when every path there ends
    // in a zero with nothing writing the local since. Every block but the
    // entry starts optimistic and loses bits until the fixpoint; the entry
    // starts from stack words, which are not zero.
    let mut entry_state: Vec<Vec<bool>> = vec![vec![true; n_locals]; n_blocks];
    entry_state[0] = vec![false; n_locals];
    let mut changed = true;
    while changed {
        changed = false;
        for b in 0..n_blocks {
            let mut out = entry_state[b].clone();
            zeroed_state_through(&body.blocks[b], &mut out);
            for &s in &succs[b] {
                if s == 0 {
                    continue;
                }
                let mut next = entry_state[s].clone();
                let mut lost = false;
                for (slot, held) in next.iter_mut().zip(out.iter()) {
                    if *slot && !*held {
                        *slot = false;
                        lost = true;
                    }
                }
                if lost {
                    entry_state[s] = next;
                    changed = true;
                }
            }
        }
    }

    // A release standing on zeroed words does nothing.
    let mut dead: Vec<(usize, usize)> = Vec::new();
    for (b, block) in body.blocks.iter().enumerate() {
        let mut state = entry_state[b].clone();
        for (si, stmt) in block.stmts.iter().enumerate() {
            if let Some(("gos_rt_aggr_release_children", local, _)) = guarded_walk_call(stmt)
                && state[local.0 as usize]
            {
                dead.push((b, si));
            }
            zeroed_state_after(stmt, &mut state);
        }
    }
    for &(b, si) in &dead {
        body.blocks[b].stmts[si].kind = StatementKind::Nop;
    }

    // A zero whose words nothing reads before the local is written again.
    let mut dead_zeros: Vec<(usize, usize)> = Vec::new();
    for b in 0..body.blocks.len() {
        for si in 0..body.blocks[b].stmts.len() {
            let Some(("gos_rt_aggr_zero_guarded", local, _)) =
                guarded_walk_call(&body.blocks[b].stmts[si])
            else {
                continue;
            };
            if zeroed_words_are_unread(body, &succs, (b, si), local) {
                dead_zeros.push((b, si));
            }
        }
    }
    for (b, si) in dead_zeros {
        body.blocks[b].stmts[si].kind = StatementKind::Nop;
    }
}

/// Advances the zeroed-word state across one statement.
fn zeroed_state_after(stmt: &Statement, state: &mut [bool]) {
    match guarded_walk_call(stmt) {
        Some(("gos_rt_aggr_zero_guarded", local, _)) => {
            state[local.0 as usize] = true;
            return;
        }
        // A walk over the words leaves them as it found them.
        Some(_) => return,
        None => {}
    }
    for (i, zeroed) in state.iter_mut().enumerate() {
        if *zeroed && stmt_refills(stmt, Local(i as u32)) {
            *zeroed = false;
        }
    }
}

/// Advances the zeroed-word state across a whole block.
fn zeroed_state_through(block: &BasicBlock, state: &mut [bool]) {
    for stmt in &block.stmts {
        zeroed_state_after(stmt, state);
    }
    for (i, zeroed) in state.iter_mut().enumerate() {
        if *zeroed && term_refills(&block.terminator, Local(i as u32)) {
            *zeroed = false;
        }
    }
}

/// `true` when nothing reads `local` between the zero at `at` and the next
/// write of it, on every path.
fn zeroed_words_are_unread(
    body: &Body,
    succs: &[Vec<usize>],
    at: (usize, usize),
    local: Local,
) -> bool {
    let mut visited = vec![false; body.blocks.len()];
    let mut stack = vec![(at.0, at.1 + 1)];
    while let Some((b, from)) = stack.pop() {
        let blk = &body.blocks[b];
        let mut ended = false;
        for stmt in &blk.stmts[from..] {
            if stmt_writes_bare(stmt, local) {
                ended = true;
                break;
            }
            if stmt_mentions_local(stmt, local) {
                return false;
            }
        }
        if ended {
            continue;
        }
        let t = &blk.terminator;
        if term_writes_bare(t, local) {
            continue;
        }
        if term_mentions_local(t, local) {
            return false;
        }
        for &s in &succs[b] {
            if !visited[s] {
                visited[s] = true;
                stack.push((s, 0));
            }
        }
    }
    true
}

/// Borrowed-holder RC elision.
///
/// A read through a nested place (`st.tables[i].slots[j]`, `st.buffer[p]`,
/// `keys[c]`), a `&self` call on a field, and a walk that rebinds a cursor
/// down a tree and back are all lowered by copying a heap-valued intermediate
/// into a local, and the drop schedule gives that local a share of its own:
/// a retain where the copy lands and a release on every exit. Such a local
/// only ever borrows - the value it names stays owned by the place it was
/// copied from - so its share is redundant for as long as that place is
/// neither written nor released.
///
/// The locals are handled as an alias class: the set connected by definition
/// edges, which is a single holder in the simple case and the whole cursor
/// set where several locals feed one another. A class is elided when all of
/// the following hold:
/// - every member carries accounting this pass recognises (a `String`, a
///   `Vec`, a guarded `Option` slot, or a struct whose share is a count on
///   each of its `Vec` and RC fields), and none is a parameter, a region
///   local, or goroutine-shared;
/// - exactly one definition edge leaves the class, to a root that is neither
///   a member nor goroutine-shared, and every write to a member takes its
///   value from another member or from that root;
/// - every other read of a member is a borrow: a projected read, an argument
///   to a user function or to a runtime helper known to leave its share
///   alone, a bare copy into another member, or storage bookkeeping - never
///   a bare copy out of the class, a capture, or a reference;
/// - from every definition of every member, on every path, nothing writes
///   into, references, or releases the structure the class aliases, and no
///   call receives a writable path to it.
///
/// Without the class's shares the root keeps every value alive across the
/// window, which is what the retains were for, so no count outside the window
/// changes. A guarded member's `zero_guarded` goes with them: it exists so an
/// early release reads a dead payload word, and every release that could read
/// it is one of the calls being removed.
pub(crate) fn elide_borrowed_holder_rc(body: &mut Body, tcx: &TyCtxt) {
    let n_blocks = body.blocks.len();
    let n_locals = body.locals.len();
    if n_blocks == 0 || n_locals == 0 {
        return;
    }
    let share = crate::ownership::ShareFacts::compute(body);
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();
    let defs = alias_defs(body);
    let roots: Vec<Local> = (0..n_locals)
        .map(|i| alias_root(&defs, Local(i as u32)))
        .collect();
    let edges = def_edges(body);
    let rcs: Vec<Option<HolderRc>> = body
        .locals
        .iter()
        .map(|decl| holder_rc_names(tcx, decl.ty))
        .collect();
    // A local can carry a class's accounting when it holds a counted value of
    // its own, is not a parameter or a region local, is not adjusted
    // concurrently, and every write to it names a value this analysis can
    // follow to its origin.
    let eligible: Vec<bool> = (0..n_locals)
        .map(|i| {
            let local = Local(i as u32);
            i > body.arity as usize
                && rcs[i].is_some()
                && !body.locals[i].region
                && !share.is_goroutine_shared(local)
                && !edges[i].opaque
                && !edges[i].sources.is_empty()
        })
        .collect();

    let mut seen = vec![false; n_locals];
    let mut elided = 0usize;
    for i in (body.arity as usize + 1)..n_locals {
        if seen[i] || !eligible[i] {
            continue;
        }
        let Some(class) = build_class(&edges, &eligible, &rcs, &roots, Local(i as u32)) else {
            seen[i] = true;
            continue;
        };
        for &m in &class.members {
            seen[m.0 as usize] = true;
        }
        if try_elide_class(body, tcx, &succs, &share, &eligible, &class) {
            elided += class.members.len();
            continue;
        }
        // A member whose own accounting is redundant does not stop being so
        // because another member of its class reads its value in a shape this
        // pass cannot follow. Each is retried alone.
        if class.members.len() > 1 {
            for &member in &class.members {
                let Some(single) = singleton_class(&rcs, &roots, member) else {
                    continue;
                };
                if try_elide_class(body, tcx, &succs, &share, &eligible, &single) {
                    elided += 1;
                }
            }
        }
    }
    if elided > 0 && std::env::var_os("GOS_RC_ELIDE_STATS").is_some() {
        eprintln!("[rc-elide] {}: elided {elided} borrowed holder(s)", body.name);
    }
}

/// Qualifies a class and, when it holds, removes every member's accounting.
/// Answers whether it did.
fn try_elide_class(
    body: &mut Body,
    tcx: &TyCtxt,
    succs: &[Vec<usize>],
    share: &crate::ownership::ShareFacts,
    eligible: &[bool],
    class: &Class,
) -> bool {
    if share.is_goroutine_shared(class.root) || eligible[class.root.0 as usize] {
        return false;
    }
    let mut retains = 0usize;
    let mut releases = 0usize;
    let mut sites: Vec<(Local, HolderDef)> = Vec::new();
    for &member in &class.members {
        let Some(uses) = classify_holder_uses(
            body,
            member,
            class.rc_of(member),
            &class.is_member,
            &class.chain,
        ) else {
            return false;
        };
        // A lone holder is bracketed by one retain at its one definition. A
        // class's brackets are spread over its members, and only the total is
        // what the root's own share has to cover.
        if class.members.len() == 1 && uses.retains != 1 {
            return false;
        }
        retains += uses.retains;
        releases += uses.releases;
        sites.extend(uses.defs.iter().map(|&d| (member, d)));
    }
    if retains == 0 || releases == 0 {
        return false;
    }
    if !sites
        .iter()
        .all(|&(member, def)| holder_window_is_clean(body, tcx, succs, def, member, class))
    {
        return false;
    }
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            if class.owns_rc_op(stmt) {
                stmt.kind = StatementKind::Nop;
            }
        }
    }
    true
}

/// The one-member class a local forms with the origin of its chain of copies.
fn singleton_class(rcs: &[Option<HolderRc>], roots: &[Local], member: Local) -> Option<Class> {
    let root = roots[member.0 as usize];
    if root == member {
        return None;
    }
    let n = rcs.len();
    let mut is_member = vec![false; n];
    is_member[member.0 as usize] = true;
    let chain: Vec<bool> = (0..n)
        .map(|i| i == member.0 as usize || roots[i] == root || Local(i as u32) == root)
        .collect();
    Some(Class {
        members: vec![member],
        is_member,
        rcs: rcs.to_vec(),
        root,
        chain,
    })
}

/// Grows the alias class containing `seed`/// Grows the alias class containing `seed`: every local reachable by following
/// definition edges out of a member, stopping at the one local outside the set
/// that feeds it.
///
/// `None` when the walk leaves the set by more than one edge, which means the
/// members do not all name one owner's value.
fn build_class(
    edges: &[DefEdges],
    eligible: &[bool],
    rcs: &[Option<HolderRc>],
    roots: &[Local],
    seed: Local,
) -> Option<Class> {
    let n = edges.len();
    let mut is_member = vec![false; n];
    let mut members = vec![seed];
    is_member[seed.0 as usize] = true;
    let mut stack = vec![seed];
    let mut root: Option<Local> = None;
    while let Some(member) = stack.pop() {
        for &source in &edges[member.0 as usize].sources {
            if is_member[source.0 as usize] {
                continue;
            }
            if eligible[source.0 as usize] {
                is_member[source.0 as usize] = true;
                members.push(source);
                stack.push(source);
                continue;
            }
            match root {
                None => root = Some(source),
                Some(existing) if existing == source => {}
                Some(_) => return None,
            }
        }
    }
    let root = root?;
    if is_member[root.0 as usize] {
        return None;
    }
    // Everything the root's chain of copies reaches is the structure the
    // class borrows from, and so is every member.
    let chain: Vec<bool> = (0..n)
        .map(|i| is_member[i] || roots[i] == root || Local(i as u32) == root)
        .collect();
    Some(Class {
        members,
        is_member,
        rcs: rcs.to_vec(),
        root,
        chain,
    })
}
