// ---------------------------------------------------------------------------
// Overflow-check versioning.
// ---------------------------------------------------------------------------
//
// `+` and `-` on `i64` check for overflow. In a loop that counts toward a
// bound, the counter and the accumulators it drives cannot overflow when the
// values they start from, the bound, and their steps are small, and a
// check in every iteration keeps the loop from vectorizing. So the loop is
// versioned: one guard before it proves the values small, and a clone that
// runs under the guard does that arithmetic wrapping - the same answer, since
// nothing overflows - while the original, checked loop runs otherwise.
//
// The proof, with `L = OVERFLOW_VALUE_LIMIT`, `S = OVERFLOW_STEP_LIMIT`, and
// `k` the number of stepping statements in the loop:
//
// - Every in-loop write of a stepped local adds (or, for a descending one,
//   subtracts) a loop-invariant step in `[0, S]`, so within one iteration it
//   moves by at most `k * S`, or resets it to a constant within `L` of zero.
// - The counter passes the header test at the start of each iteration, so it
//   is within `L` of zero there and within `L + k * S` anywhere in the
//   iteration.
// - When a step of at least one moves the counter toward its bound on every
//   path through the loop, the loop runs at most `2 * L + 1` iterations, so
//   an accumulator starting within `L` of zero stays within
//   `L + k * S * (2 * L + 1)`, below `2^58` for the limits here.

/// Largest magnitude of a bound, a counter's start, or an accumulator's
/// start that the guard admits.
const OVERFLOW_VALUE_LIMIT: i64 = 1 << 31;

/// Largest step the guard admits.
const OVERFLOW_STEP_LIMIT: i64 = 1 << 20;

/// Most stepping statements one versioned loop may hold, which keeps the
/// bound in the proof below `2^63`.
const OVERFLOW_MAX_STEPS: usize = 16;

/// Largest count any length shim answers: no object in memory has more
/// elements than the largest address space of a supported target (57 bits).
const LENGTH_LIMIT: i128 = 1 << 57;

/// Whether the runtime call `name` answers a length, which lies in
/// `[0, LENGTH_LIMIT]`.
fn is_length_call(name: &str) -> bool {
    name.starts_with("gos_rt_") && name.ends_with("_len")
}

/// Largest total constant step per iteration, and largest constant start of
/// an accumulator, that the static proof admits. The counter starts within
/// `LENGTH_LIMIT` of zero, so the loop runs at most `2 * LENGTH_LIMIT + 1`
/// iterations and an accumulator stays within `2^61 + 16 * (2^58 + 1)`,
/// below `2^63`.
const STATIC_STEP_LIMIT: i128 = 16;
const STATIC_START_LIMIT: i128 = 1 << 61;

/// A write `local = k` of a constant inside the loop, which starts the local
/// over rather than stepping it.
type Reset = (Local, i128);

/// One checked `v = v + s` / `v = v - s` in the loop.
#[derive(Clone)]
struct StepSite {
    block: usize,
    stmt: usize,
    local: Local,
    step: Operand,
}

/// Versions every loop in `body` whose induction arithmetic the guard above
/// proves in range.
pub(crate) fn overflow_check_versioning(body: &mut Body, tcx: &TyCtxt) {
    let headers: Vec<usize> = (1..body.blocks.len())
        .filter(|&h| counted_header_direction(&body.blocks[h]).is_some())
        .collect();
    for h in headers {
        try_version_overflow(body, tcx, h);
    }
}

/// The counter, bound, exit, and body entry of a counted loop header, and
/// whether it counts up (`<`, `<=`) or down (`>`, `>=`).
fn counted_header_direction(block: &BasicBlock) -> Option<(Local, Local, usize, usize, bool)> {
    let Terminator::SwitchInt {
        discriminant: Operand::Copy(disc),
        arms,
        default,
    } = &block.terminator
    else {
        return None;
    };
    if !disc.projection.is_empty() || arms.len() != 1 || arms[0].0 != 0 {
        return None;
    }
    let StatementKind::Assign {
        place,
        rvalue:
            Rvalue::BinaryOp {
                op,
                lhs: Operand::Copy(lhs),
                rhs: Operand::Copy(rhs),
            },
    } = &block.stmts.last()?.kind
    else {
        return None;
    };
    if place.local != disc.local || !lhs.projection.is_empty() || !rhs.projection.is_empty() {
        return None;
    }
    let ascending = match op {
        BinOp::Lt | BinOp::Le => true,
        BinOp::Gt | BinOp::Ge => false,
        _ => return None,
    };
    Some((
        lhs.local,
        rhs.local,
        arms[0].1.0 as usize,
        default.0 as usize,
        ascending,
    ))
}

fn try_version_overflow(body: &mut Body, tcx: &TyCtxt, h: usize) {
    let Some((counter, bound, exit, body_entry, ascending)) =
        counted_header_direction(&body.blocks[h])
    else {
        return;
    };
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();
    let Some((region, latch)) = counted_loop_region(body, &succs, h, body_entry, exit) else {
        return;
    };
    let loop_blocks: Vec<usize> = std::iter::once(h).chain(region.iter().copied()).collect();
    // Innermost loops only, so each block runs at most once per iteration:
    // no edge inside the loop closes a cycle that avoids the header.
    let inner_cycle = region.iter().any(|&b| {
        succs[b]
            .iter()
            .any(|&s| s != h && region.contains(&s) && reaches(&succs, s, b, h))
    });
    if inner_cycle {
        return;
    }
    let is_i64 = |l: Local| matches!(tcx.kind_of(body.local_ty(l)), TyKind::Int(gossamer_types::IntTy::I64));
    if !is_i64(counter) || !is_i64(bound) || !local_is_loop_invariant(body, h, &region, counter, bound) {
        return;
    }
    let Some((sites, resets)) = step_sites(body, &loop_blocks, ascending, counter) else {
        return;
    };
    let counter_sites: Vec<&StepSite> = sites.iter().filter(|s| s.local == counter).collect();
    if counter_sites.is_empty() || sites.len() > OVERFLOW_MAX_STEPS {
        return;
    }
    // Accumulators are proven only when the counter moves toward the bound
    // on every path, which bounds the iterations.
    let dominating = counter_sites
        .iter()
        .find(|s| dominates_latch(&succs, s.block, body_entry, latch, h))
        .map(|s| s.step.clone());
    let proven: Vec<StepSite> = sites
        .iter()
        .filter(|s| s.local == counter || dominating.is_some())
        .filter(|s| is_i64(s.local))
        .cloned()
        .collect();
    let invariant = |op: &Operand| match op {
        Operand::Const(ConstValue::Int(_)) => true,
        Operand::Copy(p) => {
            p.projection.is_empty()
                && is_i64(p.local)
                && local_is_loop_invariant(body, h, &region, counter, p.local)
        }
        _ => false,
    };
    if proven.iter().any(|s| !invariant(&s.step)) {
        return;
    }
    // A reset starts a proven local over from a constant, which the proof
    // bounds as it bounds the value the local enters the loop with.
    let reset_too_large = resets.iter().any(|&(local, k)| {
        proven.iter().any(|s| s.local == local) && k.abs() > i128::from(OVERFLOW_VALUE_LIMIT)
    });
    if reset_too_large {
        return;
    }
    // Values the guard bounds by magnitude, steps it bounds to `[0, S]`, and
    // the step that must be at least one.
    let mut values: Vec<Operand> = vec![Operand::Copy(Place::local(bound))];
    let mut starts: Vec<Local> = proven.iter().map(|s| s.local).collect();
    starts.sort_by_key(|l| l.0);
    starts.dedup();
    values.extend(starts.into_iter().map(|l| Operand::Copy(Place::local(l))));
    if statically_in_range(
        body,
        &loop_blocks,
        (counter, bound),
        (&proven, &resets),
        dominating.as_ref(),
    ) {
        let wrap = wrapping_sites(body, &loop_blocks, &proven, is_i64);
        make_wrapping(body, &wrap);
        return;
    }
    if !calls_only_accessors(body, &loop_blocks) {
        return;
    }
    let wrap = wrapping_sites(body, &loop_blocks, &proven, is_i64);
    // A clone that turns no checked operation wrapping would be the original
    // loop again, behind a guard that buys nothing.
    if !wrap.iter().any(|&site| is_checked_step(body, site)) {
        return;
    }
    let steps: Vec<Operand> = proven.iter().map(|s| s.step.clone()).collect();
    emit_overflow_version(body, h, &loop_blocks, &wrap, values, steps, dominating);
}

/// Whether the proof holds for every input without a guard: the bound is a
/// length, every stepped local starts from a small constant, and every step is
/// a small constant. The counter then runs at most `2 * LENGTH_LIMIT + 1`
/// iterations, and nothing it drives can reach `2^63`.
fn statically_in_range(
    body: &Body,
    loop_blocks: &[usize],
    (counter, bound): (Local, Local),
    (sites, resets): (&[StepSite], &[Reset]),
    dominating: Option<&Operand>,
) -> bool {
    let bound_is_length = matches!(
        unique_def_outside(body, loop_blocks, bound),
        Some(Definition::Call(name)) if is_length_call(name)
    ) || matches!(
        unique_def_outside(body, loop_blocks, bound),
        Some(Definition::Const(k, _)) if k.abs() <= LENGTH_LIMIT
    );
    if !bound_is_length {
        return false;
    }
    // Accumulators need the iterations bounded: a constant step of at least
    // one moving the counter on every path.
    let bounded = matches!(dominating, Some(Operand::Const(ConstValue::Int(k))) if *k >= 1);
    if !bounded && sites.iter().any(|s| s.local != sites[0].local) {
        return false;
    }
    let mut per_local: HashMap<Local, i128> = HashMap::new();
    for site in sites {
        let Operand::Const(ConstValue::Int(k)) = site.step else {
            return false;
        };
        *per_local.entry(site.local).or_insert(0) += k.abs();
    }
    per_local.iter().all(|(&local, &total)| {
        let start_limit = if local == counter || !bounded {
            LENGTH_LIMIT
        } else {
            STATIC_START_LIMIT
        };
        total <= STATIC_STEP_LIMIT
            && const_at_entry(body, loop_blocks, local)
                .is_some_and(|k| k.abs() <= start_limit)
            && resets
                .iter()
                .filter(|(l, _)| *l == local)
                .all(|(_, k)| k.abs() <= start_limit)
    })
}

/// The constant `local` holds whenever the loop is entered: its one
/// definition outside the loop is a constant, the block assigning it
/// dominates the loop, and no path from the loop back to its header avoids
/// that block - so an enclosing loop cannot carry an earlier value in.
fn const_at_entry(body: &Body, loop_blocks: &[usize], local: Local) -> Option<i128> {
    let Some(Definition::Const(k, def_block)) = unique_def_outside(body, loop_blocks, local) else {
        return None;
    };
    let header = *loop_blocks.first()?;
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();
    let reaches_header_avoiding_def = |start: usize| {
        let mut seen = vec![false; succs.len()];
        let mut stack = vec![start];
        while let Some(b) = stack.pop() {
            if b == header {
                return true;
            }
            if b == def_block || b >= succs.len() || std::mem::replace(&mut seen[b], true) {
                continue;
            }
            stack.extend(succs[b].iter().copied());
        }
        false
    };
    if def_block != 0 && reaches_header_avoiding_def(0) {
        return None;
    }
    let exits: Vec<usize> = loop_blocks
        .iter()
        .flat_map(|&b| succs[b].iter().copied())
        .filter(|s| !loop_blocks.contains(s))
        .collect();
    if exits.into_iter().any(reaches_header_avoiding_def) {
        return None;
    }
    Some(k)
}

/// The one definition a local has outside the loop.
enum Definition<'a> {
    /// A constant, assigned in the given block.
    Const(i128, usize),
    Call(&'a str),
}

fn unique_def_outside<'a>(body: &'a Body, loop_blocks: &[usize], local: Local) -> Option<Definition<'a>> {
    let mut found: Option<Definition<'a>> = None;
    for (b, block) in body.blocks.iter().enumerate() {
        if loop_blocks.contains(&b) {
            continue;
        }
        for stmt in &block.stmts {
            if !stmt_writes_bare(stmt, local) {
                continue;
            }
            let StatementKind::Assign {
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(k))),
                ..
            } = &stmt.kind
            else {
                return None;
            };
            if found.replace(Definition::Const(*k, b)).is_some() {
                return None;
            }
        }
        if term_writes_bare(&block.terminator, local) {
            let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                ..
            } = &block.terminator
            else {
                return None;
            };
            if found.replace(Definition::Call(name.as_str())).is_some() {
                return None;
            }
        }
    }
    // A parameter holds a value from the caller, which no definition here
    // bounds.
    if (1..=body.arity as usize).contains(&(local.0 as usize)) {
        return None;
    }
    found
}

/// Whether every call in the loop is an element accessor that compiles
/// inline, leaving only a panic out of line. The guarded clone pays for
/// itself by letting the loop be transformed as a whole (vectorized,
/// unrolled, its index checks hoisted); a loop that makes any other call
/// keeps that call's slow path and gains nothing from the clone but the
/// guard it runs on every entry.
fn calls_only_accessors(body: &Body, loop_blocks: &[usize]) -> bool {
    loop_blocks.iter().all(|&b| match &body.blocks[b].terminator {
        Terminator::Call { callee, .. } => matches!(
            callee,
            Operand::Const(ConstValue::Str(name))
                if ["gos_rt_vec_get", "gos_rt_vec_set", "gos_rt_vec_len", "gos_rt_len"]
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
        ),
        _ => true,
    })
}

/// The statements a proven loop runs wrapping: every step, and every `v + k`,
/// `v - k`, or `k - v` of a stepped local `v` and a constant within
/// `OVERFLOW_VALUE_LIMIT` of zero. Anywhere in the loop a stepped local lies
/// within `2^62 + 2^61 + 16` of zero (the larger of the two proofs' bounds),
/// so such an expression stays below `2^63`.
fn wrapping_sites(
    body: &Body,
    loop_blocks: &[usize],
    proven: &[StepSite],
    is_i64: impl Fn(Local) -> bool,
) -> Vec<(usize, usize)> {
    let stepped = |op: &Operand| {
        matches!(op, Operand::Copy(p) if p.projection.is_empty()
            && proven.iter().any(|s| s.local == p.local))
    };
    let small = |op: &Operand| {
        matches!(op, Operand::Const(ConstValue::Int(k))
            if k.abs() <= i128::from(OVERFLOW_VALUE_LIMIT))
    };
    let mut wrap: Vec<(usize, usize)> = proven.iter().map(|s| (s.block, s.stmt)).collect();
    for &b in loop_blocks {
        for (i, stmt) in body.blocks[b].stmts.iter().enumerate() {
            let StatementKind::Assign {
                place,
                rvalue: Rvalue::BinaryOp { op, lhs, rhs },
            } = &stmt.kind
            else {
                continue;
            };
            let derived = matches!(op, BinOp::Add | BinOp::Sub)
                && ((stepped(lhs) && small(rhs)) || (small(lhs) && stepped(rhs)));
            if derived && is_i64(place.local) && !wrap.contains(&(b, i)) {
                wrap.push((b, i));
            }
        }
    }
    wrap
}

/// Rewrites each listed statement to its wrapping form in place.
/// Whether the statement at `(block, stmt)` is a checked add or subtract.
fn is_checked_step(body: &Body, (block, stmt): (usize, usize)) -> bool {
    matches!(
        &body.blocks[block].stmts[stmt].kind,
        StatementKind::Assign {
            rvalue: Rvalue::BinaryOp {
                op: BinOp::Add | BinOp::Sub,
                ..
            },
            ..
        }
    )
}

fn make_wrapping(body: &mut Body, wrap: &[(usize, usize)]) {
    for &(block, stmt) in wrap {
        if let StatementKind::Assign {
            rvalue: Rvalue::BinaryOp { op, .. },
            ..
        } = &mut body.blocks[block].stmts[stmt].kind
        {
            *op = match *op {
                BinOp::Add => BinOp::WrappingAdd,
                BinOp::Sub => BinOp::WrappingSub,
                other => other,
            };
        }
    }
}

/// Every checked add or subtract of a local by an operand in the loop, when
/// each local so written is written only that way. `None` when the counter,
/// or any written local, has another kind of in-loop write.
fn step_sites(
    body: &Body,
    loop_blocks: &[usize],
    ascending: bool,
    counter: Local,
) -> Option<(Vec<StepSite>, Vec<Reset>)> {
    // Temps holding `v + s`: temp -> (local, step, block, stmt, is_add).
    let mut temps: HashMap<Local, (Local, Operand, usize, usize, bool)> = HashMap::new();
    for &b in loop_blocks {
        for (i, stmt) in body.blocks[b].stmts.iter().enumerate() {
            if let StatementKind::Assign { place, rvalue: Rvalue::BinaryOp { op, lhs, rhs } } =
                &stmt.kind
                && place.projection.is_empty()
                && let Operand::Copy(v) = lhs
                && v.projection.is_empty()
                && matches!(
                    op,
                    BinOp::Add | BinOp::Sub | BinOp::WrappingAdd | BinOp::WrappingSub
                )
            {
                // A step another pass already proved counts here too.
                let add = matches!(op, BinOp::Add | BinOp::WrappingAdd);
                temps.insert(place.local, (v.local, rhs.clone(), b, i, add));
            }
        }
    }
    let mut sites: Vec<StepSite> = Vec::new();
    let mut resets: Vec<Reset> = Vec::new();
    let mut disqualified: Vec<Local> = Vec::new();
    let mut written: Vec<Local> = Vec::new();
    for &b in loop_blocks {
        let block = &body.blocks[b];
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if !place.projection.is_empty() {
                continue;
            }
            let v = place.local;
            let step = match rvalue {
                Rvalue::Use(Operand::Copy(t)) if t.projection.is_empty() => temps
                    .get(&t.local)
                    .filter(|(src, ..)| *src == v)
                    .cloned(),
                Rvalue::BinaryOp { .. } => temps.get(&v).filter(|(src, ..)| *src == v).cloned(),
                _ => None,
            };
            written.push(v);
            match step {
                Some((_, s, sb, si, is_add)) => {
                    // A counter moves only toward its bound; any other local
                    // may move either way, so long as it is monotone.
                    let direction_ok = if v == counter { is_add == ascending } else { true };
                    if direction_ok {
                        sites.push(StepSite {
                            block: sb,
                            stmt: si,
                            local: v,
                            step: s,
                        });
                    } else {
                        disqualified.push(v);
                    }
                }
                None => match rvalue {
                    // A reset to a constant starts the local over; the proof
                    // bounds the constant as it bounds the starting value.
                    Rvalue::Use(Operand::Const(ConstValue::Int(k))) if v != counter => {
                        resets.push((v, *k));
                    }
                    _ => {
                        if !temps.contains_key(&v) {
                            disqualified.push(v);
                        }
                    }
                },
            }
        }
        if let Terminator::Call { destination, .. } = &block.terminator {
            disqualified.push(destination.local);
        }
    }
    if disqualified.contains(&counter) {
        return None;
    }
    // A local whose written-back steps mix adds and subtracts is not
    // monotone.
    let is_add = |site: &StepSite| {
        temps
            .values()
            .any(|(v, _, b, i, add)| *v == site.local && *b == site.block && *i == site.stmt && *add)
    };
    let kept: Vec<StepSite> = sites
        .iter()
        .filter(|site| !disqualified.contains(&site.local))
        .filter(|site| {
            sites
                .iter()
                .filter(|other| other.local == site.local)
                .all(|other| is_add(other) == is_add(site))
        })
        .cloned()
        .collect();
    Some((kept, resets))
}

/// Whether `target` is reachable from `from` without passing `header`.
fn reaches(succs: &[Vec<usize>], from: usize, target: usize, header: usize) -> bool {
    let mut seen = vec![false; succs.len()];
    let mut stack = vec![from];
    while let Some(b) = stack.pop() {
        if b == target {
            return true;
        }
        if b == header || std::mem::replace(&mut seen[b], true) {
            continue;
        }
        stack.extend(succs[b].iter().copied());
    }
    false
}

/// Whether every path from `entry` to `latch` inside the loop passes `block`.
fn dominates_latch(succs: &[Vec<usize>], block: usize, entry: usize, latch: usize, header: usize) -> bool {
    if block == entry || block == latch {
        return true;
    }
    let mut seen = vec![false; succs.len()];
    let mut stack = vec![entry];
    while let Some(b) = stack.pop() {
        if b == latch {
            return false;
        }
        if b == block || b == header || std::mem::replace(&mut seen[b], true) {
            continue;
        }
        stack.extend(succs[b].iter().copied());
    }
    true
}

/// Appends a copy of the loop's blocks, numbered from the current end of the
/// body, with the listed statements made wrapping.
fn append_wrapping_clone(body: &mut Body, loop_blocks: &[usize], wrap: &[(usize, usize)]) {
    let n0 = body.blocks.len();
    let map: HashMap<usize, usize> = loop_blocks
        .iter()
        .enumerate()
        .map(|(i, &ob)| (ob, n0 + i))
        .collect();
    let mut clones: Vec<BasicBlock> = Vec::with_capacity(loop_blocks.len());
    for &ob in loop_blocks {
        let mut blk = body.blocks[ob].clone();
        blk.id = BlockId(map[&ob] as u32);
        remap_terminator_blocks(&mut blk.terminator, &map);
        for &(_, stmt) in wrap.iter().filter(|(b, _)| *b == ob) {
            if let StatementKind::Assign {
                rvalue: Rvalue::BinaryOp { op, .. },
                ..
            } = &mut blk.stmts[stmt].kind
            {
                *op = match *op {
                    BinOp::Add => BinOp::WrappingAdd,
                    BinOp::Sub => BinOp::WrappingSub,
                    other => other,
                };
            }
        }
        clones.push(blk);
    }
    body.blocks.extend(clones);
}

/// One guard block: `lo <= op <= hi` passes to `next`, anything else
/// branches to the checked loop.
fn overflow_guard_block(
    body: &mut Body,
    (id, span, bool_ty): (BlockId, gossamer_lex::Span, Ty),
    (op, lo, hi): (Operand, i64, i64),
    (next, checked): (BlockId, BlockId),
) -> BasicBlock {
    let above = fresh_local(body, bool_ty);
    let below = fresh_local(body, bool_ty);
    let both = fresh_local(body, bool_ty);
    let stmt = |kind| Statement {
        kind,
        span,
        inlined: None,
    };
    let compare = |op, lhs, rhs| Rvalue::BinaryOp { op, lhs, rhs };
    BasicBlock {
        id,
        stmts: vec![
            stmt(StatementKind::Assign {
                place: Place::local(above),
                rvalue: compare(BinOp::Ge, op.clone(), Operand::Const(ConstValue::Int(i128::from(lo)))),
            }),
            stmt(StatementKind::Assign {
                place: Place::local(below),
                rvalue: compare(BinOp::Le, op, Operand::Const(ConstValue::Int(i128::from(hi)))),
            }),
            stmt(StatementKind::Assign {
                place: Place::local(both),
                rvalue: compare(
                    BinOp::BitAnd,
                    Operand::Copy(Place::local(above)),
                    Operand::Copy(Place::local(below)),
                ),
            }),
        ],
        terminator: Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(both)),
            arms: vec![(0, checked)],
            default: next,
        },
        span,
        terminator_span: None,
        terminator_inlined: None,
    }
}

/// Emits the guard chain, the wrapping clone, and the redirect of the
/// loop's entry edges into the guard.
fn emit_overflow_version(
    body: &mut Body,
    h: usize,
    loop_blocks: &[usize],
    wrap: &[(usize, usize)],
    values: Vec<Operand>,
    steps: Vec<Operand>,
    dominating: Option<Operand>,
) {
    let Terminator::SwitchInt {
        discriminant: Operand::Copy(disc),
        ..
    } = &body.blocks[h].terminator
    else {
        return;
    };
    let bool_ty = body.local_ty(disc.local);
    if wrap.is_empty() {
        return;
    }
    let span = body.blocks[h].span;
    let n0 = body.blocks.len();
    append_wrapping_clone(body, loop_blocks, wrap);

    // Each check is one block ending in a branch to the checked loop on
    // failure; the last passes into the wrapping clone.
    let mut checks: Vec<(Operand, i64, i64)> = values
        .into_iter()
        .map(|v| (v, -OVERFLOW_VALUE_LIMIT, OVERFLOW_VALUE_LIMIT))
        .collect();
    checks.extend(steps.into_iter().map(|s| (s, 0, OVERFLOW_STEP_LIMIT)));
    if let Some(step) = dominating {
        checks.push((step, 1, OVERFLOW_STEP_LIMIT));
    }
    checks.retain(|(op, lo, hi)| match op {
        Operand::Const(ConstValue::Int(k)) => !(i128::from(*lo) <= *k && *k <= i128::from(*hi)),
        _ => true,
    });
    if checks
        .iter()
        .any(|(op, _, _)| matches!(op, Operand::Const(_)))
    {
        // A constant outside its range fails the guard every time.
        body.blocks.truncate(n0);
        return;
    }
    let guard_start = body.blocks.len();
    let fast = BlockId(n0 as u32);
    let checked = BlockId(h as u32);
    let count = checks.len();
    for (i, (op, lo, hi)) in checks.into_iter().enumerate() {
        let next = if i + 1 == count {
            fast
        } else {
            BlockId((guard_start + i + 1) as u32)
        };
        let id = BlockId((guard_start + i) as u32);
        let block = overflow_guard_block(body, (id, span, bool_ty), (op, lo, hi), (next, checked));
        body.blocks.push(block);
    }
    let entry = if count == 0 { fast } else { BlockId(guard_start as u32) };
    let loop_set: std::collections::HashSet<usize> = loop_blocks.iter().copied().collect();
    for bi in 0..n0 {
        if loop_set.contains(&bi) {
            continue;
        }
        redirect_terminator_target(&mut body.blocks[bi].terminator, h, entry);
    }
}
