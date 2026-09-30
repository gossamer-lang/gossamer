// ---------------------------------------------------------------------------
// Overflow checks proven by a dominating fact.
// ---------------------------------------------------------------------------
//
// `x + 1` cannot overflow where `x < y` holds for some `y` of the same type,
// since then `x + 1 <= y`; `x - 1` cannot where `x > y` holds. Such a fact is
// established by a branch on the comparison, or by a successful element
// access `xs[x]`, which leaves `0 <= x < len` and so proves both. Where the fact reaches the
// arithmetic with `x` unchanged, the checked operation becomes wrapping - the
// same answer, since it cannot overflow.

/// What a fact proves about a local.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OverflowFact {
    /// `x < y`: `x + 1` cannot overflow.
    BelowSomething,
    /// `x > y`: `x - 1` cannot overflow.
    AboveSomething,
}

/// Runtime accessors that return only when their index argument lies in
/// `[0, len)`, with the argument's position. `get_opt` and `get_ptr` answer
/// `None` or null for any other index, so they prove nothing.
fn checked_index_argument(name: &str) -> Option<usize> {
    matches!(
        name,
        "gos_rt_vec_get_i64" | "gos_rt_vec_set_i64" | "gos_rt_vec_get_i128" | "gos_rt_vec_set_i128"
    )
    .then_some(1)
}

/// Rewrites every checked `x + 1` / `x - 1` a dominating fact proves.
pub(crate) fn elide_overflow_checks_by_facts(body: &mut Body) {
    let n = body.blocks.len();
    if n == 0 {
        return;
    }
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, ss) in succs.iter().enumerate() {
        for &s in ss {
            if s < n {
                preds[s].push(b);
            }
        }
    }
    let facts = overflow_facts(body, &preds);
    if facts.is_empty() {
        return;
    }
    let mut rewrites: Vec<(usize, usize)> = Vec::new();
    for (b, block) in body.blocks.iter().enumerate() {
        for (i, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign {
                rvalue: Rvalue::BinaryOp { op, lhs, rhs },
                ..
            } = &stmt.kind
            else {
                continue;
            };
            let (x, wanted) = match (op, lhs, rhs) {
                (BinOp::Add, Operand::Copy(p), Operand::Const(ConstValue::Int(1)))
                | (BinOp::Add, Operand::Const(ConstValue::Int(1)), Operand::Copy(p)) => {
                    (p, OverflowFact::BelowSomething)
                }
                (BinOp::Sub, Operand::Copy(p), Operand::Const(ConstValue::Int(1))) => {
                    (p, OverflowFact::AboveSomething)
                }
                _ => continue,
            };
            if !x.projection.is_empty() {
                continue;
            }
            let proven = facts.iter().any(|&(f, l, fact)| {
                l == x.local
                    && fact == wanted
                    && fact_reaches_unchanged(body, (&succs, &preds), f, (b, i), x.local)
            });
            if proven {
                rewrites.push((b, i));
            }
        }
    }
    for (b, i) in rewrites {
        if let StatementKind::Assign {
            rvalue: Rvalue::BinaryOp { op, .. },
            ..
        } = &mut body.blocks[b].stmts[i].kind
        {
            *op = match *op {
                BinOp::Add => BinOp::WrappingAdd,
                BinOp::Sub => BinOp::WrappingSub,
                other => other,
            };
        }
    }
}

/// Facts that hold on entry to a block: (block, local, fact). A block
/// entered from one predecessor inherits the fact on that edge.
fn overflow_facts(body: &Body, preds: &[Vec<usize>]) -> Vec<(usize, Local, OverflowFact)> {
    let mut facts: Vec<(usize, Local, OverflowFact)> = Vec::new();
    for block in &body.blocks {
        match &block.terminator {
            Terminator::SwitchInt { .. } => comparison_facts(body, block, preds, &mut facts),
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                target: Some(target),
                ..
            } => {
                let Some(at) = checked_index_argument(name) else {
                    continue;
                };
                let t = target.0 as usize;
                if preds[t].len() != 1 {
                    continue;
                }
                if let Some(Operand::Copy(p)) = args.get(at)
                    && p.projection.is_empty()
                {
                    facts.push((t, p.local, OverflowFact::BelowSomething));
                    facts.push((t, p.local, OverflowFact::AboveSomething));
                }
            }
            _ => {}
        }
    }
    facts
}

/// The facts a branch on `lhs op rhs`, computed by the block's last
/// statement, establishes on each edge entered from this block alone.
fn comparison_facts(
    body: &Body,
    block: &BasicBlock,
    preds: &[Vec<usize>],
    facts: &mut Vec<(usize, Local, OverflowFact)>,
) {
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
    let local_of = |op: &Operand| match op {
        Operand::Copy(p) if p.projection.is_empty() => Some(p.local),
        _ => None,
    };
    let same_type = match (lhs, rhs) {
        (Operand::Copy(a), Operand::Copy(c)) => body.local_ty(a.local) == body.local_ty(c.local),
        _ => true,
    };
    if !same_type {
        return;
    }
    let true_target = default.0 as usize;
    let false_target = arms[0].1.0 as usize;
    // (target, `lhs` is below `rhs`)
    let edges: &[(usize, bool)] = match op {
        BinOp::Lt => &[(true_target, true)],
        BinOp::Gt => &[(true_target, false)],
        BinOp::Ge => &[(false_target, true)],
        BinOp::Le => &[(false_target, false)],
        _ => &[],
    };
    for &(target, lhs_below) in edges {
        if preds[target].len() != 1 {
            continue;
        }
        let (low, high) = if lhs_below { (lhs, rhs) } else { (rhs, lhs) };
        if let Some(l) = local_of(low) {
            facts.push((target, l, OverflowFact::BelowSomething));
        }
        if let Some(h) = local_of(high) {
            facts.push((target, h, OverflowFact::AboveSomething));
        }
    }
}

/// Whether the fact holding on entry to block `from` still holds for `x` at
/// statement `stmt` of block `to`: `from` dominates `to`, and no path from
/// `from`'s entry to that statement writes `x`.
fn fact_reaches_unchanged(
    body: &Body,
    (succs, preds): (&[Vec<usize>], &[Vec<usize>]),
    from: usize,
    (to, stmt): (usize, usize),
    x: Local,
) -> bool {
    let block_writes = |b: usize| {
        body.blocks[b].stmts.iter().any(|s| stmt_writes_bare(s, x))
            || term_writes_bare(&body.blocks[b].terminator, x)
    };
    let writes_before = |upto: usize| {
        body.blocks[to].stmts[..upto]
            .iter()
            .any(|s| stmt_writes_bare(s, x))
    };
    if from == to {
        return !writes_before(stmt);
    }
    let len = body.blocks.len();
    // `from` dominates `to`: `to` is unreachable from the entry once `from`
    // is removed.
    let mut seen = vec![false; len];
    let mut stack = vec![0usize];
    while let Some(b) = stack.pop() {
        if b == from || std::mem::replace(&mut seen[b], true) {
            continue;
        }
        if b == to {
            return false;
        }
        stack.extend(succs[b].iter().copied());
    }
    // Blocks that reach `to` without passing `from`. Everything strictly
    // between the two is in this set, and so is any cycle through `to` that
    // avoids `from` - which would carry `x`'s own update back around.
    let mut backward = vec![false; len];
    let mut stack: Vec<usize> = preds[to].clone();
    while let Some(b) = stack.pop() {
        if b == from || std::mem::replace(&mut backward[b], true) {
            continue;
        }
        stack.extend(preds[b].iter().copied());
    }
    if backward[to] || block_writes(from) {
        return false;
    }
    if (0..len).any(|b| backward[b] && b != to && block_writes(b)) {
        return false;
    }
    !writes_before(stmt)
}
