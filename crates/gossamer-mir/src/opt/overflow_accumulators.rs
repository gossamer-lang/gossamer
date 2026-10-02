// ---------------------------------------------------------------------------
// Loop-carried sums bounded by their loop's trip count.
// ---------------------------------------------------------------------------
//
// An interval can not describe a running sum: `acc = acc + e` grows by `e`
// every pass, so no interval holds across the back edge and the dataflow
// widens `acc` to its type's range. What bounds it is the loop itself. When
// the loop's test moves an induction variable a constant step towards a
// bounded limit on every pass, the loop runs at most `T` passes, and a local
// written once per pass to itself plus terms that never read it lies within
// its entry value plus `T` times the terms' interval.
//
// The facts are derived from a fixpoint computed without them, which is sound
// on its own, and are then met with the header state of a second fixpoint.
// Each fact is true of every execution, so the second fixpoint stays sound.

/// A bound one loop's header proves for a tracked local.
#[derive(Clone, Copy, Debug)]
struct HeaderFact {
    header: usize,
    slot: usize,
    bound: Interval,
}

/// The facts every counted loop in the body proves for its sums.
fn accumulation_facts(
    analysis: &RangeAnalysis<'_>,
    entry: &[Option<RangeState>],
) -> Vec<HeaderFact> {
    let body = analysis.body;
    let (loops, dominators) = crate::preempt::loops_and_dominators(body);
    let mut regions: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for (header, region) in &loops {
        regions.entry(*header).or_default().extend(region.iter().copied());
    }
    let mut facts = Vec::new();
    for (&header, region) in &regions {
        let Some(trips) = trip_bound(analysis, entry, header, region, &dominators) else {
            continue;
        };
        for slot in 0..analysis.ranges.len() {
            if let Some(bound) =
                accumulation_bound(analysis, entry, &regions, header, region, slot, trips)
            {
                facts.push(HeaderFact {
                    header,
                    slot,
                    bound,
                });
            }
        }
    }
    facts
}

/// The state on the edge from `block` to `target`, from `block`'s entry.
fn edge_state(
    analysis: &RangeAnalysis<'_>,
    entry: &[Option<RangeState>],
    block: usize,
    target: usize,
) -> Option<RangeState> {
    let mut state = entry[block].clone()?;
    let body_block = &analysis.body.blocks[block];
    for stmt in &body_block.stmts {
        analysis.step(&mut state, stmt);
    }
    analysis.call(&mut state, &body_block.terminator);
    analysis.refine(&mut state, body_block, target);
    Some(state)
}

/// The interval `slot` holds on every edge entering `header` from outside
/// its loop.
fn entry_interval(
    analysis: &RangeAnalysis<'_>,
    entry: &[Option<RangeState>],
    header: usize,
    region: &BTreeSet<usize>,
    slot: usize,
) -> Option<Interval> {
    let mut joined: Option<Interval> = None;
    for (b, block) in analysis.body.blocks.iter().enumerate() {
        if region.contains(&b) || !successor_indices(&block.terminator).contains(&header) {
            continue;
        }
        let Some(state) = edge_state(analysis, entry, b, header) else {
            continue;
        };
        let value = state[slot]?;
        joined = Some(joined.map_or(value, |j| j.join(value)));
    }
    joined
}

/// The most passes the loop headed by `header` makes: its test compares an
/// induction variable with a bound, every write of the variable in the loop
/// steps it the same way by a positive constant, one such write happens on
/// every pass, and a step never wraps.
fn trip_bound(
    analysis: &RangeAnalysis<'_>,
    entry: &[Option<RangeState>],
    header: usize,
    region: &BTreeSet<usize>,
    dominators: &[Option<usize>],
) -> Option<i128> {
    let body = analysis.body;
    let block = &body.blocks[header];
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
    let (false_target, true_target) = (arms[0].1.0 as usize, default.0 as usize);
    let continues_when_true = match (region.contains(&true_target), region.contains(&false_target))
    {
        (true, false) => true,
        (false, true) => false,
        _ => return None,
    };
    let (compare, before) = block.stmts.split_last()?;
    let StatementKind::Assign {
        place,
        rvalue: Rvalue::BinaryOp { op, lhs, rhs },
    } = &compare.kind
    else {
        return None;
    };
    if place.local != disc.local || !place.projection.is_empty() {
        return None;
    }
    // The comparison that holds on every pass, with the operands as written.
    let holds = match (op, continues_when_true) {
        (BinOp::Lt, true) | (BinOp::Ge, false) => BinOp::Lt,
        (BinOp::Le, true) | (BinOp::Gt, false) => BinOp::Le,
        (BinOp::Gt, true) | (BinOp::Le, false) => BinOp::Gt,
        (BinOp::Ge, true) | (BinOp::Lt, false) => BinOp::Ge,
        _ => return None,
    };
    let mut state = entry[header].clone()?;
    for stmt in before {
        analysis.step(&mut state, stmt);
    }
    let latches: Vec<usize> = region
        .iter()
        .copied()
        .filter(|&b| successor_indices(&body.blocks[b].terminator).contains(&header))
        .collect();
    let every_pass =
        |b: usize| latches.iter().all(|&l| crate::preempt::dominates(dominators, b, l));
    // Either operand may be the induction variable; the comparison is read
    // from its side.
    for (var, other, op) in [(lhs, rhs, holds), (rhs, lhs, mirror(holds))] {
        let Operand::Copy(var) = var else { continue };
        if !var.projection.is_empty() {
            continue;
        }
        let local = header_source(block, before.len(), var.local);
        let Some(&slot) = analysis.slot.get(&local) else {
            continue;
        };
        let Some(bound) = analysis.operand(&state, other) else {
            continue;
        };
        let Some((rising, step, checked)) = induction_step(body, region, local, &every_pass)
        else {
            continue;
        };
        let start = entry_interval(analysis, entry, header, region, slot)?;
        let range = analysis.ranges[slot];
        let trips = match (op, rising) {
            (BinOp::Lt, true) => (bound.hi - start.lo + step - 1).div_euclid(step),
            (BinOp::Le, true) => (bound.hi - start.lo).div_euclid(step) + 1,
            (BinOp::Gt, false) => (start.hi - bound.lo + step - 1).div_euclid(step),
            (BinOp::Ge, false) => (start.hi - bound.lo).div_euclid(step) + 1,
            _ => continue,
        };
        // A wrapping step past the type's edge would restart the count.
        let last = if rising {
            bound.hi + step
        } else {
            bound.lo - step
        };
        if !(checked || (range.lo..=range.hi).contains(&last)) {
            continue;
        }
        return Some(trips.max(0));
    }
    None
}

/// The local a header's comparison operand reads: the operand itself, or the
/// local an earlier statement of the header copied into it.
fn header_source(block: &BasicBlock, upto: usize, local: Local) -> Local {
    for stmt in block.stmts[..upto].iter().rev() {
        if let StatementKind::Assign { place, rvalue } = &stmt.kind
            && place.local == local
            && place.projection.is_empty()
        {
            return match rvalue {
                Rvalue::Use(Operand::Copy(src)) if src.projection.is_empty() => src.local,
                _ => local,
            };
        }
    }
    local
}

/// How every write of `local` inside `region` moves it: the direction, the
/// smallest step, and whether every step is checked. `None` unless each write
/// adds (or each subtracts) a positive constant and one happens on every pass.
fn induction_step(
    body: &Body,
    region: &BTreeSet<usize>,
    local: Local,
    every_pass: &impl Fn(usize) -> bool,
) -> Option<(bool, i128, bool)> {
    let step_of = |rvalue: &Rvalue| -> Option<(i128, bool)> {
        let Rvalue::BinaryOp {
            op,
            lhs: Operand::Copy(src),
            rhs: Operand::Const(ConstValue::Int(k)),
        } = rvalue
        else {
            return None;
        };
        if src.local != local || !src.projection.is_empty() || *k <= 0 {
            return None;
        }
        match op {
            BinOp::Add => Some((*k, true)),
            BinOp::WrappingAdd => Some((*k, false)),
            BinOp::Sub => Some((-*k, true)),
            BinOp::WrappingSub => Some((-*k, false)),
            _ => None,
        }
    };
    let mut temps: HashMap<Local, (i128, bool)> = HashMap::new();
    for &b in region {
        for stmt in &body.blocks[b].stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && place.projection.is_empty()
                && let Some(step) = step_of(rvalue)
            {
                temps.insert(place.local, step);
            }
        }
    }
    let mut rising: Option<bool> = None;
    let mut smallest = i128::MAX;
    let mut checked = true;
    let mut every = false;
    for &b in region {
        for stmt in &body.blocks[b].stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.local != local {
                continue;
            }
            if !place.projection.is_empty() {
                return None;
            }
            let (step, is_checked) = match rvalue {
                Rvalue::Use(Operand::Copy(t)) if t.projection.is_empty() => {
                    temps.get(&t.local).copied()?
                }
                other => step_of(other)?,
            };
            if *rising.get_or_insert(step > 0) != (step > 0) {
                return None;
            }
            smallest = smallest.min(step.abs());
            checked &= is_checked;
            every |= every_pass(b);
        }
        if let Terminator::Call { destination, .. } = &body.blocks[b].terminator
            && destination.local == local
        {
            return None;
        }
    }
    (every && smallest != i128::MAX).then(|| (rising.unwrap_or(true), smallest, checked))
}

/// The interval `slot` keeps at `header` when the loop writes it exactly once
/// per pass, to itself plus terms that do not read it, over at most `trips`
/// passes.
fn accumulation_bound(
    analysis: &RangeAnalysis<'_>,
    entry: &[Option<RangeState>],
    regions: &BTreeMap<usize, BTreeSet<usize>>,
    header: usize,
    region: &BTreeSet<usize>,
    slot: usize,
    trips: i128,
) -> Option<Interval> {
    let body = analysis.body;
    let acc = *analysis
        .slot
        .iter()
        .find(|&(_, &s)| s == slot)
        .map(|(local, _)| local)?;
    let mut write: Option<(usize, usize)> = None;
    for &b in region {
        for (i, stmt) in body.blocks[b].stmts.iter().enumerate() {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.local == acc
            {
                if write.is_some() || !place.projection.is_empty() {
                    return None;
                }
                write = Some((b, i));
            }
        }
        if let Terminator::Call { destination, .. } = &body.blocks[b].terminator
            && destination.local == acc
        {
            return None;
        }
    }
    let (block, at) = write?;
    // An inner loop would run the write more than once per pass.
    let in_inner_loop = regions
        .iter()
        .any(|(&h, r)| h != header && r.contains(&block) && !r.is_superset(region));
    if in_inner_loop {
        return None;
    }
    let delta = sum_delta(analysis, entry, region, block, at, acc)?;
    let start = entry_interval(analysis, entry, header, region, slot)?;
    let lo = start.lo.checked_add(trips.checked_mul(delta.lo.min(0))?)?;
    let hi = start.hi.checked_add(trips.checked_mul(delta.hi.max(0))?)?;
    Interval { lo, hi }.meet(analysis.ranges[slot])
}

/// The interval one pass adds to `acc` when the statement at `at` in `block`
/// writes `acc` from a chain of sums and differences that starts at `acc`
/// and adds terms that do not depend on it.
fn sum_delta(
    analysis: &RangeAnalysis<'_>,
    entry: &[Option<RangeState>],
    region: &BTreeSet<usize>,
    block: usize,
    at: usize,
    acc: Local,
) -> Option<Interval> {
    let body = analysis.body;
    let stmts = &body.blocks[block].stmts;
    let mut states = Vec::with_capacity(at + 1);
    let mut state = entry[block].clone()?;
    for stmt in &stmts[..=at] {
        states.push(state.clone());
        analysis.step(&mut state, stmt);
    }
    let reads_acc = depends_on(body, region, acc);
    let definition = |local: Local, before: usize| {
        stmts[..before].iter().enumerate().rev().find_map(|(i, s)| match &s.kind {
            StatementKind::Assign { place, rvalue }
                if place.local == local && place.projection.is_empty() =>
            {
                Some((i, rvalue))
            }
            _ => None,
        })
    };
    let StatementKind::Assign { rvalue, .. } = &stmts[at].kind else {
        return None;
    };
    let mut delta = Interval { lo: 0, hi: 0 };
    let (mut index, mut rvalue) = (at, rvalue);
    loop {
        let (chain, term, negate) = match rvalue {
            Rvalue::Use(Operand::Copy(src)) if src.projection.is_empty() => {
                if src.local == acc {
                    return Some(delta);
                }
                let (i, def) = definition(src.local, index)?;
                (index, rvalue) = (i, def);
                continue;
            }
            Rvalue::BinaryOp {
                op: BinOp::Add | BinOp::WrappingAdd,
                lhs,
                rhs,
            } => {
                if leads_to(lhs, acc, &reads_acc) {
                    (lhs, rhs, false)
                } else {
                    (rhs, lhs, false)
                }
            }
            Rvalue::BinaryOp {
                op: BinOp::Sub | BinOp::WrappingSub,
                lhs,
                rhs,
            } if leads_to(lhs, acc, &reads_acc) => (lhs, rhs, true),
            _ => return None,
        };
        if let Operand::Copy(t) = term
            && reads_acc.contains(&t.local)
        {
            return None;
        }
        let term = analysis.operand(&states[index], term)?;
        let term = if negate {
            Interval {
                lo: -term.hi,
                hi: -term.lo,
            }
        } else {
            term
        };
        delta = Interval {
            lo: delta.lo + term.lo,
            hi: delta.hi + term.hi,
        };
        let Operand::Copy(next) = chain else {
            return None;
        };
        if !next.projection.is_empty() {
            return None;
        }
        if next.local == acc {
            return Some(delta);
        }
        let (i, def) = definition(next.local, index)?;
        (index, rvalue) = (i, def);
    }
}

/// Whether `operand` is `acc` or a local computed from it.
fn leads_to(operand: &Operand, acc: Local, reads_acc: &HashSet<Local>) -> bool {
    matches!(operand, Operand::Copy(p) if p.projection.is_empty()
        && (p.local == acc || reads_acc.contains(&p.local)))
}

/// Every local an assignment inside `region` computes from `acc`, directly or
/// through other such locals.
fn depends_on(body: &Body, region: &BTreeSet<usize>, acc: Local) -> HashSet<Local> {
    let mut found: HashSet<Local> = HashSet::new();
    loop {
        let before = found.len();
        for &b in region {
            for stmt in &body.blocks[b].stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                let reads = |op: &Operand| {
                    matches!(op, Operand::Copy(p) if p.local == acc || found.contains(&p.local))
                };
                let derived = match rvalue {
                    Rvalue::Use(op) | Rvalue::Cast { operand: op, .. } => reads(op),
                    Rvalue::BinaryOp { lhs, rhs, .. } => reads(lhs) || reads(rhs),
                    Rvalue::CallIntrinsic { args, .. } => args.iter().any(reads),
                    _ => false,
                };
                if derived && place.local != acc {
                    found.insert(place.local);
                }
            }
        }
        if found.len() == before {
            return found;
        }
    }
}
