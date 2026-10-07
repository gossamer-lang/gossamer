//! Where compiled code polls for cooperative preemption.
//!
//! Scheduling is cooperative, so a compiled loop that reaches no safepoint
//! holds its worker until it ends. Both native backends poll at the header of
//! the outermost loop of each loop nest, once per pass: the loops inside it
//! run between two of its polls. A nest that is counted all the way down and
//! calls none of the program's functions is not polled - the numeric kernel
//! whose trip counts its caller chose. A call into the program's own code is
//! not itself a safepoint, while a call into the runtime is short, or is one.
//!
//! A poll is a test of the runtime's `gos_rt_preempt_requested` byte and a
//! branch to a cold call when it is set, so it holds no value of its own
//! across the loop.

use std::collections::{BTreeSet, HashSet};

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Operand, Rvalue, StatementKind, Terminator,
};

/// The loop headers of `body` that poll for preemption on entry.
/// `calls_program` answers whether a call's callee is one of the program's own
/// functions, which only the backend compiling the whole module knows.
#[must_use]
pub fn preemption_polls(body: &Body, calls_program: impl Fn(&Operand) -> bool) -> HashSet<BlockId> {
    let n = body.blocks.len();
    // `GOS_NO_PREEMPT_POLLS` leaves compiled loops unpolled, for differential
    // measurement of what the polls cost.
    if n == 0 || std::env::var_os("GOS_NO_PREEMPT_POLLS").is_some() {
        return HashSet::new();
    }
    let (loops, dominators) = loops_and_dominators(body);
    let dominates = |a: usize, b: usize| dominates(&dominators, a, b);
    let mut polls = HashSet::new();
    for (header, region) in &loops {
        let header = *header;
        let enclosed = loops
            .iter()
            .any(|(h, r)| *h != header && r.contains(&header));
        if enclosed {
            continue;
        }
        let calls = region.iter().any(|&b| {
            matches!(&body.blocks[b].terminator, Terminator::Call { callee, .. } if calls_program(callee))
        });
        let counted_nest = loops
            .iter()
            .filter(|(h, _)| region.contains(h))
            .all(|(h, r)| {
                is_counted_header(&body.blocks[*h])
                    || indexes_monotonically(body, *h, r, &dominates)
            });
        if calls || !counted_nest {
            polls.insert(BlockId(u32::try_from(header).unwrap_or(u32::MAX)));
        }
    }
    polls
}

fn successors(terminator: &Terminator) -> Vec<usize> {
    match terminator {
        Terminator::Goto { target }
        | Terminator::Assert { target, .. }
        | Terminator::Drop { target, .. } => vec![target.0 as usize],
        Terminator::SwitchInt { arms, default, .. } => arms
            .iter()
            .map(|(_, target)| target.0 as usize)
            .chain(std::iter::once(default.0 as usize))
            .collect(),
        Terminator::Call { target, .. } => target.iter().map(|b| b.0 as usize).collect(),
        Terminator::Return
        | Terminator::Unreachable
        | Terminator::Resume
        | Terminator::Panic { .. } => Vec::new(),
    }
}

/// Each reachable block's immediate dominator (the entry names itself), by
/// the iterative algorithm of Cooper, Harvey, and Kennedy over reverse
/// postorder. An unreachable block has none.
fn immediate_dominators(successors: &[Vec<usize>]) -> Vec<Option<usize>> {
    let n = successors.len();
    let mut order = Vec::with_capacity(n);
    let mut visited = vec![false; n];
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    visited[0] = true;
    while let Some((block, next)) = stack.pop() {
        if let Some(&s) = successors[block].get(next) {
            stack.push((block, next + 1));
            if !visited[s] {
                visited[s] = true;
                stack.push((s, 0));
            }
        } else {
            order.push(block);
        }
    }
    order.reverse();
    let mut rank = vec![usize::MAX; n];
    for (position, &block) in order.iter().enumerate() {
        rank[block] = position;
    }
    let mut predecessors = vec![Vec::new(); n];
    for (source, succs) in successors.iter().enumerate() {
        if rank[source] == usize::MAX {
            continue;
        }
        for &s in succs {
            predecessors[s].push(source);
        }
    }
    let mut idom: Vec<Option<usize>> = vec![None; n];
    idom[0] = Some(0);
    let mut changed = true;
    while changed {
        changed = false;
        for &block in order.iter().skip(1) {
            let mut new_idom: Option<usize> = None;
            for &p in &predecessors[block] {
                if idom[p].is_none() {
                    continue;
                }
                new_idom = Some(match new_idom {
                    None => p,
                    Some(current) => intersect(&idom, &rank, p, current),
                });
            }
            if new_idom.is_some() && idom[block] != new_idom {
                idom[block] = new_idom;
                changed = true;
            }
        }
    }
    idom
}

fn intersect(idom: &[Option<usize>], rank: &[usize], mut a: usize, mut b: usize) -> usize {
    while a != b {
        while rank[a] > rank[b] {
            a = idom[a].unwrap_or(0);
        }
        while rank[b] > rank[a] {
            b = idom[b].unwrap_or(0);
        }
    }
    a
}

/// The blocks of the loop a back edge `source -> header` closes: the header
/// and every block that reaches `source` without passing through it.
fn natural_loop(predecessors: &[Vec<usize>], source: usize, header: usize) -> BTreeSet<usize> {
    let mut region = BTreeSet::from([header, source]);
    let mut pending = if source == header {
        Vec::new()
    } else {
        vec![source]
    };
    while let Some(block) = pending.pop() {
        for &p in &predecessors[block] {
            if region.insert(p) {
                pending.push(p);
            }
        }
    }
    region
}

/// Whether every pass through the loop headed by `header` makes a checked
/// element access `xs[i]` with an index `i` the loop only ever moves one
/// way by a constant: the access fails once `i` leaves `xs`, so the loop
/// runs at most `len(xs)` passes, bounded by its entry state as a counted
/// loop is.
fn indexes_monotonically(
    body: &Body,
    header: usize,
    region: &BTreeSet<usize>,
    dominates: &impl Fn(usize, usize) -> bool,
) -> bool {
    // Every pass reaches each block that dominates all of the loop's latches.
    let latches: Vec<usize> = region
        .iter()
        .copied()
        .filter(|&b| successors(&body.blocks[b].terminator).contains(&header))
        .collect();
    let every_pass = |b: usize| latches.iter().all(|&l| dominates(b, l));
    region.iter().any(|&b| {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } = &body.blocks[b].terminator
        else {
            return false;
        };
        let checked = (name.starts_with("gos_rt_vec_get") || name.starts_with("gos_rt_vec_set"))
            && !name.ends_with("_unchecked");
        let Some(Operand::Copy(index)) = args.get(1) else {
            return false;
        };
        checked
            && index.projection.is_empty()
            && every_pass(b)
            && moves_one_way(body, region, index.local, &every_pass)
    })
}

/// Whether every write to `local` inside `region` adds a positive constant,
/// or every one subtracts one, and some write happens on every pass.
fn moves_one_way(
    body: &Body,
    region: &BTreeSet<usize>,
    local: crate::ir::Local,
    every_pass: &impl Fn(usize) -> bool,
) -> bool {
    // A step is `local = local op k`, or a temp holding it copied back.
    let step_of = |rvalue: &Rvalue| -> Option<i128> {
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
            BinOp::Add | BinOp::WrappingAdd => Some(*k),
            BinOp::Sub | BinOp::WrappingSub => Some(-*k),
            _ => None,
        }
    };
    let mut temps: std::collections::HashMap<crate::ir::Local, i128> =
        std::collections::HashMap::new();
    for &b in region {
        for stmt in &body.blocks[b].stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            // A reference lets a write reach the index unseen.
            if matches!(rvalue, Rvalue::Ref { place, .. } if place.local == local) {
                return false;
            }
            if place.projection.is_empty()
                && let Some(step) = step_of(rvalue)
            {
                temps.insert(place.local, step);
            }
        }
    }
    let mut direction: Option<bool> = None;
    let mut stepped_every_pass = false;
    for &b in region {
        for stmt in &body.blocks[b].stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.local != local {
                continue;
            }
            if !place.projection.is_empty() {
                return false;
            }
            let step = match rvalue {
                Rvalue::Use(Operand::Copy(t)) if t.projection.is_empty() => {
                    temps.get(&t.local).copied()
                }
                other => step_of(other),
            };
            let Some(step) = step else {
                return false;
            };
            if *direction.get_or_insert(step > 0) != (step > 0) {
                return false;
            }
            stepped_every_pass |= every_pass(b);
        }
        if let Terminator::Call { destination, .. } = &body.blocks[b].terminator
            && destination.local == local
        {
            return false;
        }
    }
    stepped_every_pass
}

/// Whether `block` is a counted loop's header: its last statement compares
/// two locals with `<`, `<=`, `>`, or `>=`, and it branches on that comparison
/// with the false arm leaving the loop - the `for i in lo..hi` and
/// `for i in (lo..hi).rev()` shapes the lowerer writes.
/// A natural loop: its header and every block in it.
pub(crate) type NaturalLoop = (usize, BTreeSet<usize>);

/// The natural loops of `body`, one per back edge.
#[must_use]
pub(crate) fn natural_loops(body: &Body) -> Vec<NaturalLoop> {
    loops_and_dominators(body).0
}

/// The natural loops of `body` and each reachable block's immediate
/// dominator.
pub(crate) fn loops_and_dominators(body: &Body) -> (Vec<NaturalLoop>, Vec<Option<usize>>) {
    let n = body.blocks.len();
    if n == 0 {
        return (Vec::new(), Vec::new());
    }
    let successors: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|block| {
            successors(&block.terminator)
                .into_iter()
                .filter(|&s| s < n)
                .collect()
        })
        .collect();
    let dominators = immediate_dominators(&successors);
    let back_edges: Vec<(usize, usize)> = successors
        .iter()
        .enumerate()
        .filter(|(source, _)| dominators[*source].is_some())
        .flat_map(|(source, succs)| succs.iter().map(move |&header| (source, header)))
        .filter(|&(source, header)| dominates(&dominators, header, source))
        .collect();
    let mut predecessors = vec![Vec::new(); n];
    for (source, succs) in successors.iter().enumerate() {
        for &s in succs {
            predecessors[s].push(source);
        }
    }
    let loops = back_edges
        .iter()
        .map(|&(source, header)| (header, natural_loop(&predecessors, source, header)))
        .collect();
    (loops, dominators)
}

/// Whether block `a` dominates block `b`.
pub(crate) fn dominates(dominators: &[Option<usize>], a: usize, mut b: usize) -> bool {
    loop {
        if a == b {
            return true;
        }
        match dominators[b] {
            Some(parent) if parent != b => b = parent,
            _ => return false,
        }
    }
}

fn is_counted_header(block: &BasicBlock) -> bool {
    let Terminator::SwitchInt {
        discriminant: Operand::Copy(disc),
        arms,
        ..
    } = &block.terminator
    else {
        return false;
    };
    if !disc.projection.is_empty() || arms.len() != 1 || arms[0].0 != 0 {
        return false;
    }
    // The bound may be a constant, as a `Simd` lane loop's lane count is.
    let bound = |operand: &Operand| match operand {
        Operand::Copy(place) => place.projection.is_empty(),
        Operand::Const(ConstValue::Int(_)) => true,
        _ => false,
    };
    matches!(
        block.stmts.last().map(|stmt| &stmt.kind),
        Some(StatementKind::Assign {
            place,
            rvalue: Rvalue::BinaryOp {
                op: BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge,
                lhs: Operand::Copy(lhs),
                rhs,
            },
        }) if place.local == disc.local
            && place.projection.is_empty()
            && lhs.projection.is_empty()
            && bound(rhs)
    )
}

#[cfg(test)]
mod tests {
    use super::immediate_dominators;

    #[test]
    fn a_loop_header_dominates_its_latch() {
        // 0 -> 1 -> 2 -> 1, 1 -> 3
        let succs = vec![vec![1], vec![2, 3], vec![1], vec![]];
        let idom = immediate_dominators(&succs);
        assert_eq!(idom[2], Some(1));
        assert_eq!(idom[3], Some(1));
        assert_eq!(idom[1], Some(0));
    }

    #[test]
    fn an_unreachable_block_has_no_dominator() {
        let succs = vec![vec![], vec![0]];
        assert_eq!(immediate_dominators(&succs)[1], None);
    }
}
