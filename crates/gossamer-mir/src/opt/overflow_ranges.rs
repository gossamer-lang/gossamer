// ---------------------------------------------------------------------------
// Overflow checks proven by value ranges.
// ---------------------------------------------------------------------------
//
// A forward dataflow over the integer locals that feed checked arithmetic
// computes, at every statement, an interval each such local lies in: a
// constant, the range of its type, the image of an operation over its
// operands' intervals, narrowed along a branch on a comparison with a
// constant or another bounded local. Where the exact result of a checked
// `+`, `-`, or `*` over its operands' intervals fits the destination type,
// the operation cannot overflow and becomes wrapping - the same answer.
//
// Widening is per bound and per loop: after `RANGE_ROUNDS` visits of a loop
// header, a local the loop writes whose bound still moves takes its type's
// limit on that side, and the other bound stays. A local the loop only reads
// settles when the enclosing loop's header does, so an outer counter keeps
// the range its own header proves inside an inner loop. Any block still
// changing after `FALLBACK_ROUNDS` widens every local, which bounds the
// iteration whatever the control flow. A local that settles in a few rounds,
// like a bit position reset at eight, keeps its exact bounds. A runtime call
// answering a length lies in `[0, LENGTH_LIMIT]`, and the index of an element
// access that returned lies in `[0, LENGTH_LIMIT)`.
//
// An integer `Vec` the body builds empty and touches only through element
// reads, writes, pushes, lengths, frees, and copies to other such locals holds
// only values the body stored, so a read from it lies in the join of the
// stored values' intervals. That join is computed from the type's range
// downward: every round recomputes it from the stores with reads taken from
// the previous round, and each round's join still holds every value a store
// can write, because each stored value is computed from values read inside
// the previous join.

/// Visits of a loop header after which a moving bound of a local the loop
/// writes widens to its type's limit.
const RANGE_ROUNDS: u32 = 24;

/// Visits of one block after which every moving bound widens there.
const FALLBACK_ROUNDS: u32 = 4 * RANGE_ROUNDS;

/// Rounds that narrow the element intervals of the `Vec`s the pass reads.
const ELEMENT_ROUNDS: usize = 3;

/// A closed interval of integer values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Interval {
    lo: i128,
    hi: i128,
}

impl Interval {
    fn join(self, other: Interval) -> Interval {
        Interval {
            lo: self.lo.min(other.lo),
            hi: self.hi.max(other.hi),
        }
    }

    fn meet(self, other: Interval) -> Option<Interval> {
        let lo = self.lo.max(other.lo);
        let hi = self.hi.min(other.hi);
        (lo <= hi).then_some(Interval { lo, hi })
    }

    fn within(self, outer: Interval) -> bool {
        outer.lo <= self.lo && self.hi <= outer.hi
    }
}

/// The values an integer type holds; `None` for a type the pass does not
/// bound (128-bit integers).
fn int_range(tcx: &TyCtxt, ty: Ty) -> Option<Interval> {
    use gossamer_types::IntTy;
    let TyKind::Int(int) = tcx.kind_of(ty) else {
        return None;
    };
    let (lo, hi): (i128, i128) = match int {
        IntTy::I8 => (i8::MIN.into(), i8::MAX.into()),
        IntTy::I16 => (i16::MIN.into(), i16::MAX.into()),
        IntTy::I32 => (i32::MIN.into(), i32::MAX.into()),
        IntTy::I64 | IntTy::Isize => (i64::MIN.into(), i64::MAX.into()),
        IntTy::U8 => (0, u8::MAX.into()),
        IntTy::U16 => (0, u16::MAX.into()),
        IntTy::U32 => (0, u32::MAX.into()),
        IntTy::U64 | IntTy::Usize => (0, u64::MAX.into()),
        IntTy::I128 | IntTy::U128 => return None,
    };
    Some(Interval { lo, hi })
}

/// Per-block entry states: `None` while no path has reached the block.
type RangeState = Vec<Option<Interval>>;

struct RangeAnalysis<'a> {
    body: &'a Body,
    /// Tracked local -> its slot in a state.
    slot: HashMap<Local, usize>,
    /// Each tracked local's type range.
    ranges: Vec<Interval>,
    /// The interval every element of a whole-seen `Vec` local lies in.
    elements: HashMap<Local, Interval>,
}

impl RangeAnalysis<'_> {
    fn operand(&self, state: &RangeState, op: &Operand) -> Option<Interval> {
        match op {
            Operand::Const(ConstValue::Int(k)) => Some(Interval { lo: *k, hi: *k }),
            Operand::Copy(p) if p.projection.is_empty() => {
                self.slot.get(&p.local).and_then(|&s| state[s])
            }
            _ => None,
        }
    }

    /// The interval an rvalue answers, before clamping to the destination.
    fn rvalue(&self, state: &RangeState, rvalue: &Rvalue) -> Option<Interval> {
        match rvalue {
            Rvalue::Use(op) | Rvalue::Cast { operand: op, .. } => self.operand(state, op),
            Rvalue::BinaryOp { op, lhs, rhs } => {
                let a = self.operand(state, lhs)?;
                let b = self.operand(state, rhs)?;
                binary_interval(*op, a, b)
            }
            _ => None,
        }
    }

    /// Applies one statement to `state`.
    fn step(&self, state: &mut RangeState, stmt: &Statement) {
        let StatementKind::Assign { place, rvalue } = &stmt.kind else {
            return;
        };
        if !place.projection.is_empty() {
            return;
        }
        // A checked `x * x` that completes leaves `x` within the square root
        // of its type's largest value.
        if let Rvalue::BinaryOp {
            op: BinOp::Mul,
            lhs: Operand::Copy(a),
            rhs: Operand::Copy(b),
        } = rvalue
            && a == b
            && a.projection.is_empty()
            && a.local != place.local
            && let Some(&x) = self.slot.get(&a.local)
        {
            let root = i128::try_from(self.ranges[x].hi.max(0).cast_unsigned().isqrt())
                .unwrap_or(i128::MAX);
            let bound = Interval {
                lo: -root,
                hi: root,
            };
            state[x] = state[x].map(|cur| cur.meet(bound).unwrap_or(cur));
        }
        if let Some(&s) = self.slot.get(&place.local) {
            let range = self.ranges[s];
            // A value outside the destination's range wraps or truncates
            // into it, so only a result inside it is kept exactly.
            state[s] = Some(
                self.rvalue(state, rvalue)
                    .filter(|i| i.within(range))
                    .unwrap_or(range),
            );
        }
    }

    /// Applies a block's terminator to `state`: a call's destination takes
    /// the interval the callee answers, and an element access that returned
    /// leaves its index in bounds.
    fn call(&self, state: &mut RangeState, terminator: &Terminator) {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            ..
        } = terminator
        else {
            if let Terminator::Call { destination, .. } = terminator
                && destination.projection.is_empty()
                && let Some(&s) = self.slot.get(&destination.local)
            {
                state[s] = Some(self.ranges[s]);
            }
            return;
        };
        if is_element_access(name)
            && let Some(Operand::Copy(index)) = args.get(1)
            && index.projection.is_empty()
            && let Some(&s) = self.slot.get(&index.local)
        {
            let in_bounds = Interval {
                lo: 0,
                hi: LENGTH_LIMIT - 1,
            };
            state[s] = state[s].map(|cur| cur.meet(in_bounds).unwrap_or(cur));
        }
        if !destination.projection.is_empty() {
            return;
        }
        let Some(&s) = self.slot.get(&destination.local) else {
            return;
        };
        let answered = if is_length_call(name) {
            Some(Interval {
                lo: 0,
                hi: LENGTH_LIMIT,
            })
        } else if is_element_read(name)
            && let Some(Operand::Copy(vec)) = args.first()
            && vec.projection.is_empty()
        {
            self.elements.get(&vec.local).copied()
        } else {
            None
        };
        let range = self.ranges[s];
        state[s] = Some(answered.and_then(|i| i.meet(range)).unwrap_or(range));
    }

    /// Narrows `state` along the edge from `block` to `target`, where
    /// `block` ends in a branch on a comparison computed by its last
    /// statement.
    fn refine(&self, state: &mut RangeState, block: &BasicBlock, target: usize) {
        let Terminator::SwitchInt {
            discriminant: Operand::Copy(disc),
            arms,
            default,
        } = &block.terminator
        else {
            return;
        };
        if !disc.projection.is_empty() || arms.len() != 1 || arms[0].0 != 0 {
            return;
        }
        let false_target = arms[0].1.0 as usize;
        let true_target = default.0 as usize;
        if false_target == true_target {
            return;
        }
        let Some(StatementKind::Assign {
            place,
            rvalue: Rvalue::BinaryOp { op, lhs, rhs },
        }) = block.stmts.last().map(|s| &s.kind)
        else {
            return;
        };
        if place.local != disc.local || !place.projection.is_empty() {
            return;
        }
        let holds = target == true_target;
        let op = match (op, holds) {
            (BinOp::Eq, true) | (BinOp::Ne, false) => BinOp::Eq,
            (BinOp::Eq, false) | (BinOp::Ne, true) => BinOp::Ne,
            (BinOp::Lt, true) | (BinOp::Ge, false) => BinOp::Lt,
            (BinOp::Lt, false) | (BinOp::Ge, true) => BinOp::Ge,
            (BinOp::Le, true) | (BinOp::Gt, false) => BinOp::Le,
            (BinOp::Le, false) | (BinOp::Gt, true) => BinOp::Gt,
            _ => return,
        };
        let (Some(a), Some(b)) = (self.operand(state, lhs), self.operand(state, rhs)) else {
            return;
        };
        let narrowed_lhs = narrow_compared(op, a, b);
        let narrowed_rhs = narrow_compared(mirror(op), b, a);
        // The compared operands and every local this block copied them
        // from without an intervening write hold the same value.
        for (operand, narrowed) in [(lhs, narrowed_lhs), (rhs, narrowed_rhs)] {
            let Operand::Copy(p) = operand else { continue };
            if !p.projection.is_empty() {
                continue;
            }
            for local in copies_of(block, p.local) {
                if let Some(&s) = self.slot.get(&local) {
                    state[s] = match (state[s], narrowed) {
                        (Some(cur), Some(n)) => cur.meet(n).or(Some(n)),
                        (cur, _) => cur,
                    };
                }
            }
        }
    }
}

/// `op` with its operands swapped.
fn mirror(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Gt => BinOp::Lt,
        BinOp::Le => BinOp::Ge,
        BinOp::Ge => BinOp::Le,
        other => other,
    }
}

/// The interval `a` narrows to where `a op b` holds.
fn narrow_compared(op: BinOp, a: Interval, b: Interval) -> Option<Interval> {
    match op {
        BinOp::Eq => a.meet(b),
        BinOp::Ne if b.lo == b.hi => {
            if a.lo == b.lo {
                Some(Interval { lo: a.lo + 1, hi: a.hi })
            } else if a.hi == b.lo {
                Some(Interval { lo: a.lo, hi: a.hi - 1 })
            } else {
                Some(a)
            }
            .filter(|i| i.lo <= i.hi)
        }
        BinOp::Lt => a.meet(Interval { lo: i128::MIN, hi: b.hi - 1 }),
        BinOp::Le => a.meet(Interval { lo: i128::MIN, hi: b.hi }),
        BinOp::Gt => a.meet(Interval { lo: b.lo + 1, hi: i128::MAX }),
        BinOp::Ge => a.meet(Interval { lo: b.lo, hi: i128::MAX }),
        _ => Some(a),
    }
}

/// `local` and the locals `block` assigned from it by a plain copy, where
/// neither is written again before the block ends.
fn copies_of(block: &BasicBlock, local: Local) -> Vec<Local> {
    let mut same = vec![local];
    for (i, stmt) in block.stmts.iter().enumerate() {
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
        let (dst, src) = (place.local, src.local);
        let related = if dst == local {
            Some(src)
        } else if src == local {
            Some(dst)
        } else {
            None
        };
        if let Some(other) = related {
            let rewritten = block.stmts[i + 1..]
                .iter()
                .any(|s| stmt_writes_bare(s, other) || stmt_writes_bare(s, local));
            if !rewritten && !same.contains(&other) {
                same.push(other);
            }
        }
    }
    same
}

/// The exact interval of `a op b` over every pair of values, where the pass
/// models `op`.
fn binary_interval(op: BinOp, a: Interval, b: Interval) -> Option<Interval> {
    let corners = |f: fn(i128, i128) -> Option<i128>| -> Option<Interval> {
        let values = [f(a.lo, b.lo)?, f(a.lo, b.hi)?, f(a.hi, b.lo)?, f(a.hi, b.hi)?];
        Some(Interval {
            lo: *values.iter().min()?,
            hi: *values.iter().max()?,
        })
    };
    match op {
        BinOp::Add | BinOp::WrappingAdd => corners(i128::checked_add),
        BinOp::Sub | BinOp::WrappingSub => corners(i128::checked_sub),
        BinOp::Mul | BinOp::WrappingMul => corners(i128::checked_mul),
        // Truncating division is monotonic in each operand while the divisor
        // keeps one sign, so the corners bound it.
        BinOp::Div if b.lo > 0 || b.hi < 0 => corners(i128::checked_div),
        BinOp::BitAnd if a.lo >= 0 || b.lo >= 0 => {
            // A non-negative operand bounds the result from both sides.
            let hi = match (a.lo >= 0, b.lo >= 0) {
                (true, true) => a.hi.min(b.hi),
                (true, false) => a.hi,
                _ => b.hi,
            };
            Some(Interval { lo: 0, hi })
        }
        // A truncating remainder by a positive divisor keeps the dividend's
        // sign and a magnitude below the divisor's.
        BinOp::Rem if b.lo > 0 => Some(Interval {
            lo: if a.lo >= 0 { 0 } else { a.lo.max(1 - b.hi) },
            hi: if a.hi <= 0 { 0 } else { a.hi.min(b.hi - 1) },
        }),
        BinOp::Shr if a.lo >= 0 && b.lo >= 0 && b.hi < 64 => Some(Interval {
            lo: a.lo >> b.hi,
            hi: a.hi >> b.lo,
        }),
        _ => None,
    }
}

/// Rewrites every checked `+`, `-`, and `*` whose operands' ranges prove
/// the result in its type's range.
pub(crate) fn elide_overflow_checks_by_ranges(body: &mut Body, tcx: &TyCtxt) {
    let n = body.blocks.len();
    if n == 0 {
        return;
    }
    let Some(mut analysis) = range_analysis(body, tcx) else {
        return;
    };
    let mut entry = analysis_fixpoint(&analysis);
    let vecs = whole_seen_vecs(body, tcx);
    if !vecs.is_empty() {
        for _ in 0..ELEMENT_ROUNDS {
            analysis.elements = stored_element_ranges(&analysis, &entry, &vecs);
            entry = analysis_fixpoint(&analysis);
        }
    }
    let mut rewrites: Vec<(usize, usize, BinOp)> = Vec::new();
    for (b, block) in body.blocks.iter().enumerate() {
        let Some(mut state) = entry[b].clone() else {
            continue;
        };
        for (i, stmt) in block.stmts.iter().enumerate() {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::BinaryOp { op, lhs, rhs },
            } = &stmt.kind
                && place.projection.is_empty()
                && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul)
                && let Some(range) = int_range(tcx, body.local_ty(place.local))
                && let (Some(left), Some(right)) =
                    (analysis.operand(&state, lhs), analysis.operand(&state, rhs))
                && binary_interval(*op, left, right).is_some_and(|r| r.within(range))
            {
                let wrapping = match op {
                    BinOp::Add => BinOp::WrappingAdd,
                    BinOp::Sub => BinOp::WrappingSub,
                    _ => BinOp::WrappingMul,
                };
                rewrites.push((b, i, wrapping));
            }
            analysis.step(&mut state, stmt);
        }
    }
    for (b, i, wrapping) in rewrites {
        if let StatementKind::Assign {
            rvalue: Rvalue::BinaryOp { op, .. },
            ..
        } = &mut body.blocks[b].stmts[i].kind
        {
            *op = wrapping;
        }
    }
}

/// The locals the analysis tracks: integer locals feeding a checked
/// operation, through the assignments and comparisons that compute them,
/// minus any whose address is taken (a write through a reference is not
/// an assignment the pass sees). `None` when nothing is checked.
fn range_analysis<'a>(body: &'a Body, tcx: &TyCtxt) -> Option<RangeAnalysis<'a>> {
    let mut address_taken: HashSet<Local> = HashSet::new();
    let mut wanted: Vec<Local> = Vec::new();
    let copied = |op: &Operand| match op {
        Operand::Copy(p) if p.projection.is_empty() => Some(p.local),
        _ => None,
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { rvalue, .. } = &stmt.kind {
                match rvalue {
                    Rvalue::Ref { place, .. } => {
                        address_taken.insert(place.local);
                    }
                    Rvalue::BinaryOp {
                        op: BinOp::Add | BinOp::Sub | BinOp::Mul,
                        lhs,
                        rhs,
                    } => wanted.extend(copied(lhs).into_iter().chain(copied(rhs))),
                    _ => {}
                }
            }
        }
    }
    if wanted.is_empty() {
        return None;
    }
    // A stored element's interval bounds later reads of its `Vec`.
    for block in &body.blocks {
        if let Some((_, Operand::Copy(value))) = element_store(&block.terminator)
            && value.projection.is_empty()
        {
            wanted.push(value.local);
        }
    }
    // Close over the sources of every tracked local and the operands of
    // comparisons against it.
    let mut tracked: HashSet<Local> = HashSet::new();
    while let Some(local) = wanted.pop() {
        if !tracked.insert(local) {
            continue;
        }
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                if !place.projection.is_empty() {
                    continue;
                }
                let operands: Vec<&Operand> = match rvalue {
                    Rvalue::Use(op) | Rvalue::Cast { operand: op, .. } => vec![op],
                    Rvalue::BinaryOp { lhs, rhs, .. } => vec![lhs, rhs],
                    _ => Vec::new(),
                };
                let reads_local = operands.iter().any(|op| copied(op) == Some(local));
                if place.local == local || reads_local {
                    wanted.extend(operands.into_iter().filter_map(copied));
                    if reads_local
                        && matches!(
                            rvalue,
                            Rvalue::Use(_)
                                | Rvalue::BinaryOp {
                                    op: BinOp::Eq
                                        | BinOp::Ne
                                        | BinOp::Lt
                                        | BinOp::Le
                                        | BinOp::Gt
                                        | BinOp::Ge,
                                    ..
                                }
                        )
                    {
                        wanted.push(place.local);
                    }
                }
            }
        }
    }
    let mut slot = HashMap::new();
    let mut ranges = Vec::new();
    let mut locals: Vec<Local> = tracked.into_iter().collect();
    locals.sort_by_key(|l| l.0);
    for local in locals {
        if address_taken.contains(&local) {
            continue;
        }
        if let Some(range) = int_range(tcx, body.local_ty(local)) {
            slot.insert(local, ranges.len());
            ranges.push(range);
        }
    }
    (!slot.is_empty()).then_some(RangeAnalysis {
        body,
        slot,
        ranges,
        elements: HashMap::new(),
    })
}

/// For each block, the tracked slots a loop headed there writes: by an
/// assignment or as a call's destination anywhere in the loop.
fn loop_writes(analysis: &RangeAnalysis<'_>) -> Vec<HashSet<usize>> {
    let body = analysis.body;
    let mut writes = vec![HashSet::new(); body.blocks.len()];
    for (header, region) in crate::preempt::natural_loops(body) {
        for &b in &region {
            let block = &body.blocks[b];
            let assigned = block.stmts.iter().filter_map(|stmt| match &stmt.kind {
                StatementKind::Assign { place, .. } if place.projection.is_empty() => {
                    Some(place.local)
                }
                _ => None,
            });
            let called = match &block.terminator {
                Terminator::Call { destination, .. } if destination.projection.is_empty() => {
                    Some(destination.local)
                }
                _ => None,
            };
            for local in assigned.chain(called) {
                if let Some(&s) = analysis.slot.get(&local) {
                    writes[header].insert(s);
                }
            }
        }
    }
    writes
}

/// Each block's entry state at the fixpoint.
fn analysis_fixpoint(analysis: &RangeAnalysis<'_>) -> Vec<Option<RangeState>> {
    let body = analysis.body;
    let n = body.blocks.len();
    let width = analysis.ranges.len();
    // Parameters and every local on entry may hold anything their type
    // holds; a local's first assignment replaces that.
    let top: RangeState = analysis.ranges.iter().map(|r| Some(*r)).collect();
    let widened_at = loop_writes(analysis);
    let mut entry: Vec<Option<RangeState>> = vec![None; n];
    let mut visits = vec![0u32; n];
    entry[0] = Some(top);
    let mut work: VecDeque<usize> = VecDeque::from([0]);
    let mut queued = vec![false; n];
    queued[0] = true;
    while let Some(b) = work.pop_front() {
        queued[b] = false;
        let Some(mut state) = entry[b].clone() else {
            continue;
        };
        let block = &body.blocks[b];
        for stmt in &block.stmts {
            analysis.step(&mut state, stmt);
        }
        analysis.call(&mut state, &block.terminator);
        for target in successor_indices(&block.terminator) {
            if target >= n {
                continue;
            }
            let mut out = state.clone();
            analysis.refine(&mut out, block, target);
            let merged: RangeState = match &entry[target] {
                None => out,
                Some(prev) => (0..width)
                    .map(|s| match (prev[s], out[s]) {
                        (Some(p), Some(o)) => Some(p.join(o)),
                        (p, o) => p.or(o),
                    })
                    .collect(),
            };
            if entry[target].as_ref() == Some(&merged) {
                continue;
            }
            visits[target] += 1;
            let merged = if visits[target] > RANGE_ROUNDS {
                let prev = entry[target].as_ref();
                let every = visits[target] > FALLBACK_ROUNDS;
                merged
                    .into_iter()
                    .enumerate()
                    .map(|(s, m)| {
                        let widens = every || widened_at[target].contains(&s);
                        match (prev.map(|p| p[s]), m) {
                            (Some(Some(p)), Some(m)) if widens && p != m => {
                                let limit = analysis.ranges[s];
                                Some(Interval {
                                    lo: if m.lo < p.lo { limit.lo } else { m.lo },
                                    hi: if m.hi > p.hi { limit.hi } else { m.hi },
                                })
                            }
                            (Some(p), m) if widens && p != m => Some(analysis.ranges[s]),
                            _ => m,
                        }
                    })
                    .collect()
            } else {
                merged
            };
            entry[target] = Some(merged);
            if !queued[target] {
                queued[target] = true;
                work.push_back(target);
            }
        }
    }
    entry
}

/// Whether `name` is a runtime element read or write whose index argument
/// (position 1) lies in `[0, len)` once it returns: the checked forms panic
/// otherwise, and the `_unchecked` forms run only where the index is proven.
fn is_element_access(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_vec_get_i64"
            | "gos_rt_vec_get_i64_unchecked"
            | "gos_rt_vec_set_i64"
            | "gos_rt_vec_set_i64_unchecked"
    )
}

/// Whether `name` reads one element of the `Vec` in its first argument.
fn is_element_read(name: &str) -> bool {
    matches!(name, "gos_rt_vec_get_i64" | "gos_rt_vec_get_i64_unchecked")
}

/// The `Vec` operand and the stored value of an element write or push.
fn element_store(terminator: &Terminator) -> Option<(&Operand, &Operand)> {
    let Terminator::Call {
        callee: Operand::Const(ConstValue::Str(name)),
        args,
        ..
    } = terminator
    else {
        return None;
    };
    match (name.as_str(), args.as_slice()) {
        ("gos_rt_vec_set_i64" | "gos_rt_vec_set_i64_unchecked", [vec, _, value])
        | ("gos_rt_vec_push", [vec, value]) => Some((vec, value)),
        _ => None,
    }
}

/// Integer `Vec` locals whose every element the body stores itself, grouped
/// by the copies between them; each group maps its members to one id. A
/// local qualifies when it is not a parameter, every value it holds comes
/// from an empty constructor or another member, and every other use reads,
/// writes, pushes, measures, frees, or drops it - nothing else can reach its
/// elements.
fn whole_seen_vecs(body: &Body, tcx: &TyCtxt) -> HashMap<Local, usize> {
    let candidates: Vec<Local> = (0..body.locals.len())
        .map(|i| Local(i as u32))
        .filter(|&l| {
            matches!(tcx.kind_of(body.local_ty(l)), TyKind::Vec(elem)
                if int_range(tcx, *elem).is_some())
        })
        .collect();
    if candidates.is_empty() {
        return HashMap::new();
    }
    let index: HashMap<Local, usize> =
        candidates.iter().enumerate().map(|(i, &l)| (l, i)).collect();
    let mut groups = VecGroups {
        parent: (0..candidates.len()).collect(),
    };
    let mut poisoned: Vec<bool> = candidates
        .iter()
        .map(|l| l.0 >= 1 && l.0 <= body.arity)
        .collect();
    for block in &body.blocks {
        for stmt in &block.stmts {
            let allowed = match vec_statement_use(stmt, &index) {
                VecStatementUse::Join(a, b) => {
                    groups.join(index[&a], index[&b]);
                    continue;
                }
                VecStatementUse::Allows(allowed) => allowed,
            };
            for &l in &candidates {
                if !allowed.contains(&l) && stmt_mentions_local(stmt, l) {
                    poisoned[index[&l]] = true;
                }
            }
        }
        let allowed = vec_terminator_use(&block.terminator);
        for &l in &candidates {
            if !allowed.contains(&l) && term_mentions_local(&block.terminator, l) {
                poisoned[index[&l]] = true;
            }
        }
    }
    for i in 0..candidates.len() {
        if poisoned[i] {
            let root = groups.find(i);
            poisoned[root] = true;
        }
    }
    candidates
        .iter()
        .enumerate()
        .filter_map(|(i, &l)| {
            let root = groups.find(i);
            (!poisoned[root]).then_some((l, root))
        })
        .collect()
}

/// Union-find over candidate indices joined by copies.
struct VecGroups {
    parent: Vec<usize>,
}

impl VecGroups {
    fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    fn join(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        self.parent[ra] = rb;
    }
}

/// How a statement touches the candidate `Vec` locals.
enum VecStatementUse {
    /// `a = b` between two candidates: one group from here on.
    Join(Local, Local),
    /// The candidates the statement may mention without reaching elements.
    Allows(Vec<Local>),
}

/// The local a bare `Copy` operand names.
fn bare_local(op: &Operand) -> Option<Local> {
    match op {
        Operand::Copy(p) if p.projection.is_empty() => Some(p.local),
        _ => None,
    }
}

fn vec_statement_use(stmt: &Statement, index: &HashMap<Local, usize>) -> VecStatementUse {
    match &stmt.kind {
        StatementKind::Assign {
            place,
            rvalue: Rvalue::Use(op),
        } if place.projection.is_empty() && index.contains_key(&place.local) => {
            match bare_local(op) {
                Some(source) if index.contains_key(&source) => {
                    VecStatementUse::Join(place.local, source)
                }
                // A null handle holds no elements.
                None if matches!(op, Operand::Const(ConstValue::Int(0))) => {
                    VecStatementUse::Allows(vec![place.local])
                }
                _ => VecStatementUse::Allows(Vec::new()),
            }
        }
        StatementKind::Assign {
            place,
            rvalue: Rvalue::CallIntrinsic { name, args },
        } if *name == "gos_rt_vec_free" && !index.contains_key(&place.local) => {
            VecStatementUse::Allows(args.iter().filter_map(bare_local).collect())
        }
        StatementKind::StorageLive(l) | StatementKind::StorageDead(l) => {
            VecStatementUse::Allows(vec![*l])
        }
        _ => VecStatementUse::Allows(Vec::new()),
    }
}

/// The candidates a block's terminator may mention without reaching their
/// elements other than through a read, write, push, or length: the receiver
/// of those calls, or the destination of an empty constructor.
fn vec_terminator_use(terminator: &Terminator) -> Vec<Local> {
    let Terminator::Call {
        callee: Operand::Const(ConstValue::Str(name)),
        args,
        destination,
        ..
    } = terminator
    else {
        return Vec::new();
    };
    let receiver = args.first().and_then(bare_local);
    let accessor = matches!(name.as_str(), "gos_rt_vec_len" | "gos_rt_vec_push")
        || is_element_access(name);
    let receiver_only = receiver.is_some_and(|v| {
        !args[1..].iter().any(|a| operand_mentions_local(a, v))
            && !place_mentions_local(destination, v)
    });
    if accessor && receiver_only {
        return receiver.into_iter().collect();
    }
    let empty_constructor = matches!(name.as_str(), "gos_rt_vec_with_capacity" | "gos_rt_vec_new")
        && destination.projection.is_empty();
    if empty_constructor {
        vec![destination.local]
    } else {
        Vec::new()
    }
}

/// The interval each whole-seen `Vec` group's elements lie in, joined over
/// every store into the group at the states in `entry`, keyed by member.
fn stored_element_ranges(
    analysis: &RangeAnalysis<'_>,
    entry: &[Option<RangeState>],
    vecs: &HashMap<Local, usize>,
) -> HashMap<Local, Interval> {
    let body = analysis.body;
    let mut joined: HashMap<usize, Interval> = HashMap::new();
    for (b, block) in body.blocks.iter().enumerate() {
        let Some((Operand::Copy(vec), value)) = element_store(&block.terminator) else {
            continue;
        };
        let Some(&group) = vecs.get(&vec.local) else {
            continue;
        };
        let Some(mut state) = entry[b].clone() else {
            continue;
        };
        for stmt in &block.stmts {
            analysis.step(&mut state, stmt);
        }
        let stored = analysis.operand(&state, value).unwrap_or(Interval {
            lo: i64::MIN.into(),
            hi: u64::MAX.into(),
        });
        joined
            .entry(group)
            .and_modify(|i| *i = i.join(stored))
            .or_insert(stored);
    }
    vecs.iter()
        .filter_map(|(&local, group)| joined.get(group).map(|&i| (local, i)))
        .collect()
}
