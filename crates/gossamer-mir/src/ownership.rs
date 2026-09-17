//! Goroutine-share analysis backing safe RC retain/release elision.
//!
//! Eliding a balanced `retain(x)` / `release(x)` pair is reference-count
//! preserving: the object reaches every program point at the same count it
//! would have reached with the pair in place. That makes elision sound for
//! any value whose count is only ever adjusted by the current goroutine. It
//! is unsound once the value has crossed a goroutine boundary: another
//! goroutine may concurrently adjust the same count under the biased-RC
//! `SHARED_BIT` atomic protocol, so a locally balanced pair becomes
//! load-bearing for that protocol. This module computes, per body, the
//! locals whose object may be goroutine-shared.

use crate::ir::{Body, ConstValue, Local, Operand, Rvalue, StatementKind, Terminator};

/// Runtime helper emitted at every point an RC value escapes to another
/// goroutine (`go f(args)`, `spawn` closure captures, channel `send`).
const MARK_SHARED: &str = "gos_rt_rc_mark_shared";

/// The in-place counterpart of [`MARK_SHARED`] for a by-value aggregate.
const MARK_SHARED_AGGREGATE: &str = "gos_rt_aggr_mark_shared_children";

/// Per-body facts about which locals may reference a goroutine-shared
/// object.
pub(crate) struct ShareFacts {
    shared: Vec<bool>,
}

impl ShareFacts {
    /// `true` when `local` may reference an object reachable from another
    /// goroutine. An out-of-range local conservatively reports shared.
    pub(crate) fn is_goroutine_shared(&self, local: Local) -> bool {
        self.shared.get(local.0 as usize).copied().unwrap_or(true)
    }

    /// Computes the share set for `body`: the alias-closure of every local
    /// that crosses a goroutine boundary (`mark_shared`) or is written into
    /// a `static mut` global (reachable from any goroutine). Aliasing is
    /// over-approximated through copies, references, repeats, casts, and
    /// aggregate membership; over-approximation only widens the shared set,
    /// which forbids more elisions, never frees one wrongly.
    pub(crate) fn compute(body: &Body) -> Self {
        let n = body.locals.len();
        let mut adj: Vec<Vec<Local>> = vec![Vec::new(); n];
        let mut seeds: Vec<Local> = Vec::new();

        for block in &body.blocks {
            for stmt in &block.stmts {
                match &stmt.kind {
                    StatementKind::Assign { place, rvalue } => {
                        let dest = place.local;
                        collect_rvalue_aliases(rvalue, &mut |src| {
                            connect(&mut adj, n, dest, src);
                        });
                        if let Rvalue::CallIntrinsic { name, args } = rvalue
                            && (*name == MARK_SHARED || *name == MARK_SHARED_AGGREGATE)
                        {
                            push_operand_locals(args, &mut seeds);
                        }
                    }
                    StatementKind::StaticStore {
                        value: Operand::Copy(p),
                        ..
                    } => {
                        seeds.push(p.local);
                    }
                    _ => {}
                }
            }
            if let Terminator::Call { callee, args, .. } = &block.terminator
                && is_mark_shared(callee)
            {
                push_operand_locals(args, &mut seeds);
            }
        }

        let mut shared = vec![false; n];
        let mut stack = seeds;
        while let Some(l) = stack.pop() {
            let i = l.0 as usize;
            if i >= n || shared[i] {
                continue;
            }
            shared[i] = true;
            for &nb in &adj[i] {
                if !shared[nb.0 as usize] {
                    stack.push(nb);
                }
            }
        }
        Self { shared }
    }
}

/// Adds an undirected alias edge between `a` and `b`.
fn connect(adj: &mut [Vec<Local>], n: usize, a: Local, b: Local) {
    let (ai, bi) = (a.0 as usize, b.0 as usize);
    if a != b && ai < n && bi < n {
        adj[ai].push(b);
        adj[bi].push(a);
    }
}

fn push_operand_locals(args: &[Operand], out: &mut Vec<Local>) {
    for a in args {
        if let Operand::Copy(p) = a {
            out.push(p.local);
        }
    }
}

fn is_mark_shared(callee: &Operand) -> bool {
    matches!(callee, Operand::Const(ConstValue::Str(s)) if s == MARK_SHARED)
}

/// Invokes `f` with every local whose object the destination of `rvalue`
/// may alias or nest - the source of a copy, reference, repeat, cast, or
/// aggregate element. A retain / release / copy intrinsic does not create a
/// new alias of its argument's object (the same object, the same local), so
/// `CallIntrinsic` contributes no edges here; `mark_shared` is seeded by the
/// caller instead.
fn collect_rvalue_aliases(rvalue: &Rvalue, f: &mut impl FnMut(Local)) {
    let mut op = |o: &Operand| {
        if let Operand::Copy(p) = o {
            f(p.local);
        }
    };
    match rvalue {
        Rvalue::Use(o) | Rvalue::Repeat { value: o, .. } | Rvalue::Cast { operand: o, .. } => op(o),
        Rvalue::Ref { place, .. } => f(place.local),
        Rvalue::Aggregate { operands, .. } => {
            for o in operands {
                op(o);
            }
        }
        Rvalue::BinaryOp { .. }
        | Rvalue::UnaryOp { .. }
        | Rvalue::Len(_)
        | Rvalue::CallIntrinsic { .. }
        | Rvalue::StaticLoad(_) => {}
    }
}
