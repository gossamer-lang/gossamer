// ---------------------------------------------------------------------------
// Share transfer and clone moves driven by uniqueness facts.
// ---------------------------------------------------------------------------

/// How many shares [`transfer_last_use_shares`] moved and clones
/// [`move_unique_clones`] turned into handoffs, for `--uniqueness-report`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UniquenessReport {
    /// Retains dropped because the holder they paid for took the last share
    /// of a value no one reads again.
    pub shares_transferred: usize,
    /// Deep copies replaced by handing the copied value over.
    pub clones_moved: usize,
}

/// The plain strong retains, each paired with the types whose share it pays
/// for.
fn plain_retain_matches(name: &str, tcx: &TyCtxt, ty: Ty) -> bool {
    match name {
        "gos_rt_vec_retain" => matches!(
            tcx.kind_of(ty),
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
        ),
        "gos_rt_str_retain_typed" | "gos_rt_str_retain" => {
            matches!(tcx.kind_of(ty), TyKind::String)
        }
        "gos_rt_rc_retain" => tcx.is_rc_managed(ty),
        _ => false,
    }
}

/// `true` when `place` projects only through fields, so it names a slot a
/// constant can be written into.
fn fields_only(place: &Place) -> bool {
    place
        .projection
        .iter()
        .all(|step| matches!(step, Projection::Field(_)))
}

/// Where a transferred share's source is set to null.
enum NullAt {
    /// In place of the retain, at this statement index of the same block.
    Retain(usize),
    /// At the start of this block, which the consuming call reaches.
    Entry(BlockId),
}

struct ShareTransfer {
    block: usize,
    retain: usize,
    source: Place,
    null_at: NullAt,
}

/// The place whose share the retain at `stmts[i]` of `retained` pays for, and
/// whether the holder is made by the statement before it or by the consuming
/// call that ends the block.
///
/// A retain follows the statement that made the new holder, possibly after
/// the retains of that statement's other operands. It names either the
/// operand the holder was made from, or, for a whole copy `d = a`, a place
/// inside the destination whose counterpart in `a` is the source.
fn retained_source(block: &BasicBlock, i: usize, retained: &Place) -> Option<(Place, bool)> {
    let is_retain = |stmt: &Statement| {
        matches!(
            &stmt.kind,
            StatementKind::Assign { rvalue: Rvalue::CallIntrinsic { name, .. }, .. }
                if is_rc_retain_name(name)
        )
    };
    let maker_at = block.stmts[..i].iter().rposition(|stmt| !is_retain(stmt));
    let maker = maker_at.map(|m| &block.stmts[m]);
    // The retains that follow the maker account for the holder it made. When
    // they name both the value handed on and the holder it went to, two
    // spellings pay for one handoff and which of them is the holder's share
    // cannot be told apart, so none of them moves.
    if let (Some(m), Some(Statement {
        kind: StatementKind::Assign { place, rvalue },
        ..
    })) = (maker_at, maker)
    {
        let group_end = block.stmts[m + 1..]
            .iter()
            .position(|stmt| !is_retain(stmt))
            .map_or(block.stmts.len(), |n| m + 1 + n);
        let mut sources: Vec<Local> = Vec::new();
        match rvalue {
            Rvalue::Use(Operand::Copy(p)) | Rvalue::Cast { operand: Operand::Copy(p), .. } => {
                sources.push(p.local);
            }
            Rvalue::Aggregate { operands, .. } => {
                sources.extend(operands.iter().filter_map(|op| match op {
                    Operand::Copy(p) => Some(p.local),
                    _ => None,
                }));
            }
            _ => {}
        }
        let group = &block.stmts[m + 1..group_end];
        let names_holder = group.iter().any(|stmt| stmt_mentions_local(stmt, place.local));
        let names_source = group
            .iter()
            .any(|stmt| sources.iter().any(|src| stmt_mentions_local(stmt, *src)));
        if names_holder && names_source {
            return None;
        }
    }
    if let Some(Statement {
        kind: StatementKind::Assign { place, rvalue },
        ..
    }) = maker
        && place.local != retained.local
    {
        let reads_retained = match rvalue {
            Rvalue::Use(op) | Rvalue::Cast { operand: op, .. } => {
                matches!(op, Operand::Copy(p) if p == retained)
            }
            Rvalue::Aggregate { operands, .. } => operands
                .iter()
                .any(|op| matches!(op, Operand::Copy(p) if p == retained)),
            _ => false,
        };
        if reads_retained && !place_mentions_local(place, retained.local) {
            return Some((retained.clone(), true));
        }
    }
    if let Some(Statement {
        kind:
            StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(copied)),
            },
        ..
    }) = maker
        && place.projection.is_empty()
        && copied.projection.is_empty()
        && place.local == retained.local
        && copied.local != retained.local
    {
        let mut source = copied.clone();
        source.projection.clone_from(&retained.projection);
        return Some((source, true));
    }
    let rest_leaves_it_alone = block.stmts[i + 1..].iter().all(|stmt| {
        is_retain(stmt) && !stmt_mentions_local(stmt, retained.local)
    });
    // Only a call that takes the argument's share for itself makes the retain
    // before it the new holder's. A call that borrows its argument reads the
    // source while it runs, and the retain is then the source's own share.
    if rest_leaves_it_alone
        && let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(callee)),
            args,
            ..
        } = &block.terminator
        && crate::lower::is_consuming_call(callee)
        && args
            .iter()
            .skip(1)
            .any(|op| matches!(op, Operand::Copy(p) if p == retained))
        && !matches!(args.first(), Some(Operand::Copy(p)) if p.local == retained.local)
    {
        return Some((retained.clone(), false));
    }
    None
}

/// Moves the share a local's last use hands on instead of paying for a new
/// one.
///
/// A copy, store, aggregate, or consuming call that gives a value a new holder
/// is followed by a retain: the new holder's share, while the source keeps its
/// own until its release. When the source is read by nothing after that but
/// its own releases, the two shares are one too many. The retain is dropped
/// and the source is set to null, so its releases find nothing to give back
/// and the new holder carries the only share:
///
/// ```text
/// t = Aggregate(a, b)            t = Aggregate(a, b)
/// retain(b)                  ->  b = 0
/// ...                            ...
/// release(b)                     release(b)       // a no-op on null
/// ```
///
/// The source must own the share it gives up: it is released on every path
/// from the handoff, so the release the null turns into a no-op is the one
/// the dropped retain balanced. A view of storage something else owns, such
/// as an element read, has no share of its own to give. A borrowed source is
/// skipped, since a live reference reads it after its last direct use, as is
/// a parameter, whose share belongs to the caller, a region local, and a
/// value another goroutine may count.
pub(crate) fn transfer_last_use_shares(
    body: &mut Body,
    tcx: &TyCtxt,
    summaries: &crate::uniqueness::CallSummaries,
) -> usize {
        let transfers = find_share_transfers(body, tcx, summaries);
    let count = transfers.len();
    // Every in-place rewrite lands before any statement is inserted, so the
    // indices the transfers were found at still name the retains they mean.
    let mut entries: Vec<(BlockId, Statement)> = Vec::new();
    for transfer in transfers {
        let block = &mut body.blocks[transfer.block];
        let retain = &block.stmts[transfer.retain];
        let null = Statement {
            kind: StatementKind::Assign {
                place: transfer.source.clone(),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            },
            span: retain.span,
            inlined: retain.inlined.clone(),
        };
        match transfer.null_at {
            NullAt::Retain(i) => block.stmts[i] = null,
            NullAt::Entry(target) => {
                block.stmts[transfer.retain].kind = StatementKind::Nop;
                entries.push((target, null));
            }
        }
    }
    for (target, null) in entries {
        body.blocks[target.0 as usize].stmts.insert(0, null);
    }
    count
}

/// The releases that give back a share the retain `name` takes.
fn paired_releases(name: &str) -> &'static [&'static str] {
    match name {
        "gos_rt_vec_retain" => &["gos_rt_vec_free"],
        "gos_rt_str_retain_typed" | "gos_rt_str_retain" => {
            &["gos_rt_str_free_typed", "gos_rt_str_free"]
        }
        "gos_rt_rc_retain" => &["gos_rt_rc_release"],
        _ => &[],
    }
}

/// `true` when `a` and `b` name overlapping storage: one is the other or
/// lies inside it.
fn places_overlap(a: &Place, b: &Place) -> bool {
    a.local == b.local
        && a
            .projection
            .iter()
            .zip(&b.projection)
            .all(|(x, y)| x == y)
}

/// What a statement does to the share held at `place`.
enum ShareStep {
    /// Gives the share back.
    Released,
    /// Overwrites or otherwise ends the share without giving it back here.
    Ended,
    /// Leaves it alone.
    Untouched,
}

fn statement_share_step(stmt: &Statement, place: &Place, releases: &[&str]) -> ShareStep {
    let StatementKind::Assign { place: dest, rvalue } = &stmt.kind else {
        return ShareStep::Untouched;
    };
    if let Rvalue::CallIntrinsic { name, args } = rvalue
        && is_rc_release_name(name)
        && let Some(Operand::Copy(released)) = args.first()
        && places_overlap(released, place)
    {
        return if released == place && releases.contains(name) {
            ShareStep::Released
        } else {
            ShareStep::Ended
        };
    }
    if places_overlap(dest, place) {
        return ShareStep::Ended;
    }
    ShareStep::Untouched
}

/// Whether the share held at `place` is given back on every path from
/// statement `from` of block `block`, before anything overwrites it or the
/// function returns.
///
/// A binding that owns its share is released at the end of its life; a view
/// of storage something else owns (an element read, a borrowed field) is not.
/// Only an owned share can move to a new holder.
fn released_on_every_path(
    body: &Body,
    block: usize,
    from: usize,
    place: &Place,
    releases: &[&str],
) -> bool {
    // `Some(answer)` when the block settles the question itself, `None` when
    // it falls through to its successors.
    let scan = |bi: usize, start: usize| -> Option<bool> {
        let b = &body.blocks[bi];
        for stmt in b.stmts.iter().skip(start) {
            match statement_share_step(stmt, place, releases) {
                ShareStep::Released => return Some(true),
                ShareStep::Ended => return Some(false),
                ShareStep::Untouched => {}
            }
        }
        match &b.terminator {
            Terminator::Return => Some(false),
            Terminator::Unreachable | Terminator::Panic { .. } => Some(true),
            Terminator::Call { destination, .. } if places_overlap(destination, place) => {
                Some(false)
            }
            Terminator::Drop { place: dropped, .. } if places_overlap(dropped, place) => {
                Some(false)
            }
            _ => None,
        }
    };
    let n = body.blocks.len();
    let whole: Vec<Option<bool>> = (0..n).map(|bi| scan(bi, 0)).collect();
    let mut must = vec![true; n];
    loop {
        let mut changed = false;
        for bi in 0..n {
            let value = whole[bi].unwrap_or_else(|| {
                successor_indices(&body.blocks[bi].terminator)
                    .into_iter()
                    .all(|s| must.get(s).copied().unwrap_or(false))
            });
            if value != must[bi] {
                must[bi] = value;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    scan(block, from).unwrap_or_else(|| {
        successor_indices(&body.blocks[block].terminator)
            .into_iter()
            .all(|s| must.get(s).copied().unwrap_or(false))
    })
}

/// The retains [`transfer_last_use_shares`] can drop, with where each
/// source is set to null.
fn find_share_transfers(
    body: &Body,
    tcx: &TyCtxt,
    summaries: &crate::uniqueness::CallSummaries,
) -> Vec<ShareTransfer> {
    use crate::uniqueness::{Point, Uniqueness};

    let facts = crate::uniqueness::analyze(body, tcx, summaries);
    let share = crate::ownership::ShareFacts::compute(body);
    let preds = block_predecessor_counts(body);
    let mut out = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (i, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign {
                place: sink,
                rvalue: Rvalue::CallIntrinsic { name, args },
            } = &stmt.kind
            else {
                continue;
            };
            let [Operand::Copy(retained)] = args.as_slice() else {
                continue;
            };
            if !sink.projection.is_empty() || !fields_only(retained) {
                continue;
            }
            let Some((source, before)) = retained_source(block, i, retained) else {
                continue;
            };
            let root = source.local;
            let ri = root.0 as usize;
            if ri <= body.arity as usize
                || ri >= body.locals.len()
                || body.locals[ri].region
                || !fields_only(&source)
                || share.is_goroutine_shared(root)
            {
                continue;
            }
            let Some(ty) = crate::uniqueness::place_ty(tcx, body, &source) else {
                continue;
            };
            if !plain_retain_matches(name, tcx, ty) {
                continue;
            }
            let point = Point {
                block: block.id,
                stmt: i,
            };
            if facts.at(point, root) == Uniqueness::Borrowed {
                continue;
            }
            let releases = paired_releases(name);
            let null_at = if before {
                if facts.live_after(point, root)
                    || !released_on_every_path(body, bi, i + 1, &source, releases)
                {
                    continue;
                }
                NullAt::Retain(i)
            } else {
                let Terminator::Call {
                    target: Some(target),
                    ..
                } = &block.terminator
                else {
                    continue;
                };
                let ti = target.0 as usize;
                if ti == bi || preds.get(ti).copied() != Some(1) {
                    continue;
                }
                if facts.live_on_entry(*target, root)
                    || !released_on_every_path(body, ti, 0, &source, releases)
                {
                    continue;
                }
                NullAt::Entry(*target)
            };
            out.push(ShareTransfer {
                block: bi,
                retain: i,
                source,
                null_at,
            });
        }
    }
    out
}

fn block_predecessor_counts(body: &Body) -> Vec<usize> {
    let mut counts = vec![0; body.blocks.len()];
    for block in &body.blocks {
        for succ in successor_indices(&block.terminator) {
            if let Some(slot) = counts.get_mut(succ) {
                *slot += 1;
            }
        }
    }
    counts
}

/// The release that gives back the value a deep-copy helper answers, which is
/// also the one its argument takes, or `None` for any other helper.
fn value_clone_release(name: &str) -> Option<&'static str> {
    match name {
        "gos_rt_vec_clone" => Some("gos_rt_vec_free"),
        "gos_rt_map_clone" => Some("gos_rt_map_free"),
        "gos_rt_set_clone" => Some("gos_rt_set_free"),
        "gos_rt_deque_clone" | "gos_rt_queue_clone" | "gos_rt_stack_clone" => {
            Some("gos_rt_deque_free")
        }
        _ => None,
    }
}

/// Hands a value to the binding that copies it when nothing else can observe
/// the value and nothing reads it again.
///
/// `let b = a` gives `b` a value of its own, which the lowering spells as a
/// deep copy. When `a` is unique at the copy and dead after it, no program
/// can tell the copy from `a` itself:
///
/// ```text
/// b = gos_rt_vec_clone(a)      b = a
///                          ->  a = 0
/// ```
///
/// `b` owns the share `a` held, and every release of `a` that follows reads a
/// null handle. `a` must own that share, released on every path after the
/// copy.
pub(crate) fn move_unique_clones(
    body: &mut Body,
    tcx: &TyCtxt,
    summaries: &crate::uniqueness::CallSummaries,
) -> usize {
    use crate::uniqueness::{Point, Uniqueness};

    let moves: Vec<(usize, Local, Local, BlockId)> = {
        let facts = crate::uniqueness::analyze(body, tcx, summaries);
        let mut out = Vec::new();
        for (bi, block) in body.blocks.iter().enumerate() {
            let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                destination,
                target: Some(target),
            } = &block.terminator
            else {
                continue;
            };
            let Some(release) = value_clone_release(name) else {
                continue;
            };
            if !destination.projection.is_empty() {
                continue;
            }
            let [Operand::Copy(src)] = args.as_slice() else {
                continue;
            };
            if !src.projection.is_empty() {
                continue;
            }
            let (src, dst) = (src.local, destination.local);
            let (si, di) = (src.0 as usize, dst.0 as usize);
            if src == dst
                || si <= body.arity as usize
                || si >= body.locals.len()
                || di >= body.locals.len()
                || body.locals[si].region
                || body.locals[di].region
                || body.locals[si].ty != body.locals[di].ty
            {
                continue;
            }
            let point = Point {
                block: block.id,
                stmt: block.stmts.len(),
            };
            if facts.at(point, src) != Uniqueness::Unique
                || facts.live_on_entry(*target, src)
                || !released_on_every_path(
                    body,
                    target.0 as usize,
                    0,
                    &Place::local(src),
                    &[release],
                )
            {
                continue;
            }
            out.push((bi, src, dst, *target));
        }
        out
    };
    let count = moves.len();
    for (bi, src, dst, target) in moves {
        let block = &mut body.blocks[bi];
        let span = block.terminator_span.unwrap_or(block.span);
        let inlined = block.terminator_inlined.clone();
        block.terminator = Terminator::Goto { target };
        block.stmts.push(Statement {
            kind: StatementKind::Assign {
                place: Place::local(dst),
                rvalue: Rvalue::Use(Operand::Copy(Place::local(src))),
            },
            span,
            inlined: inlined.clone(),
        });
        block.stmts.push(Statement {
            kind: StatementKind::Assign {
                place: Place::local(src),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            },
            span,
            inlined,
        });
    }
    count
}

static UNIQUENESS_REPORT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Makes the compiler name, on stderr, each function whose reference-count
/// traffic uniqueness removed, with what it removed.
pub fn enable_uniqueness_report() {
    UNIQUENESS_REPORT.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Prints `report` for `function` when the report is enabled and it removed
/// anything.
pub(crate) fn record_uniqueness(function: &str, report: UniquenessReport) {
    if UNIQUENESS_REPORT.load(std::sync::atomic::Ordering::Relaxed)
        && report != UniquenessReport::default()
    {
        eprintln!(
            "uniqueness: {function}: {} share(s) transferred, {} clone(s) moved",
            report.shares_transferred, report.clones_moved
        );
    }
}
