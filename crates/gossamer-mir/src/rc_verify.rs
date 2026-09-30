//! Release discipline for reference-counted handles in MIR.
//!
//! A release call hands back the share one local holds. From that point
//! until something is assigned to the local again, reading it reaches memory
//! this body no longer owns, and releasing it again gives up a share it does
//! not have. Both are checked here on every path: a local counts as released
//! at a program point when every path reaching the point released it, so a
//! report names a fault the body commits whichever way it runs.
//!
//! A leak - a share never released on some path - is the remaining failure;
//! the leak ledger measures that one at run time.

use std::collections::BTreeSet;

use crate::ir::{
    BlockId, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, Statement, StatementKind,
    Terminator,
};

/// Runtime entry points that give up the share their first argument holds.
const RELEASES: &[&str] = &[
    "gos_rt_aggr_free",
    "gos_rt_arr_iter_free",
    "gos_rt_binding_callback_release",
    "gos_rt_binding_map_free",
    "gos_rt_deque_free",
    "gos_rt_dyn_free",
    "gos_rt_heap_i64_free",
    "gos_rt_heap_u8_free",
    "gos_rt_http_response_free",
    "gos_rt_json_free",
    "gos_rt_lazy_iter_drop_i64",
    "gos_rt_lazy_iter_drop_pair_i64",
    "gos_rt_map_free",
    "gos_rt_rc_release",
    "gos_rt_rc_weak_release",
    "gos_rt_select_free",
    "gos_rt_set_free",
    "gos_rt_str_free",
    "gos_rt_str_free_typed",
    "gos_rt_vec_free",
];

/// One fault the verifier found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RcViolation {
    /// `local` is read after every path here released it.
    UseAfterRelease {
        /// Function the fault is in.
        body: String,
        /// Block holding the read.
        block: BlockId,
        /// The released local.
        local: Local,
    },
    /// `local` is released again after every path here released it.
    DoubleRelease {
        /// Function the fault is in.
        body: String,
        /// Block holding the second release.
        block: BlockId,
        /// The released local.
        local: Local,
        /// The runtime entry point of the second release.
        release: String,
    },
}

/// Checks `body`'s release discipline, answering every fault it finds.
pub fn verify_rc(body: &Body) -> Result<(), Vec<RcViolation>> {
    let n = body.blocks.len();
    let mut entry_state: Vec<Option<BTreeSet<Local>>> = vec![None; n];
    if n == 0 {
        return Ok(());
    }
    entry_state[0] = Some(BTreeSet::new());
    let predecessors = predecessors(body);
    let mut worklist: Vec<usize> = vec![0];
    let mut exit_state: Vec<Option<BTreeSet<Local>>> = vec![None; n];
    while let Some(index) = worklist.pop() {
        let Some(state) = entry_state[index].clone() else {
            continue;
        };
        let out = transfer(body, index, state, &mut Vec::new());
        if exit_state[index].as_ref() == Some(&out) {
            continue;
        }
        exit_state[index] = Some(out);
        for successor in successors(&body.blocks[index].terminator) {
            let s = successor.0 as usize;
            if s >= n {
                continue;
            }
            let joined = predecessors[s]
                .iter()
                .filter_map(|p| exit_state[*p].as_ref())
                .fold(None::<BTreeSet<Local>>, |acc, set| match acc {
                    None => Some(set.clone()),
                    Some(acc) => Some(acc.intersection(set).copied().collect()),
                });
            if joined.is_some() && entry_state[s] != joined {
                entry_state[s] = joined;
                worklist.push(s);
            }
        }
    }
    let mut violations = Vec::new();
    for (index, state) in entry_state.into_iter().enumerate() {
        if let Some(state) = state {
            transfer(body, index, state, &mut violations);
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Runs block `index` from `state`, recording each fault in `violations`,
/// and answers the released set at the block's end.
fn transfer(
    body: &Body,
    index: usize,
    mut state: BTreeSet<Local>,
    violations: &mut Vec<RcViolation>,
) -> BTreeSet<Local> {
    let block = &body.blocks[index];
    for stmt in &block.stmts {
        step_statement(body, block.id, stmt, &mut state, violations);
    }
    match &block.terminator {
        Terminator::Call {
            callee,
            args,
            destination,
            ..
        } => {
            let released = released_local(callee_name(callee), args);
            check_args(body, block.id, args, released, &state, violations);
            if let Some((local, name)) = released {
                release(body, block.id, local, name, &mut state, violations);
            }
            define(destination, body, block.id, &mut state, violations);
        }
        Terminator::SwitchInt { discriminant, .. } => {
            check_operand(body, block.id, discriminant, &state, violations);
        }
        Terminator::Assert { cond, msg, .. } => {
            check_operand(body, block.id, cond, &state, violations);
            for operand in msg.operands() {
                check_operand(body, block.id, operand, &state, violations);
            }
        }
        Terminator::Drop { place, .. } => check_place(body, block.id, place, &state, violations),
        Terminator::Goto { .. }
        | Terminator::Return
        | Terminator::Unreachable
        | Terminator::Panic { .. } => {}
    }
    state
}

fn step_statement(
    body: &Body,
    block: BlockId,
    stmt: &Statement,
    state: &mut BTreeSet<Local>,
    violations: &mut Vec<RcViolation>,
) {
    match &stmt.kind {
        StatementKind::Assign { place, rvalue } => {
            if let Rvalue::CallIntrinsic { name, args } = rvalue {
                let released = released_local(Some(name), args);
                check_args(body, block, args, released, state, violations);
                if let Some((local, name)) = released {
                    release(body, block, local, name, state, violations);
                }
            } else {
                check_rvalue(body, block, rvalue, state, violations);
            }
            define(place, body, block, state, violations);
        }
        StatementKind::StorageDead(local) => {
            state.remove(local);
        }
        StatementKind::SetDiscriminant { place, .. } => {
            check_place(body, block, place, state, violations);
        }
        StatementKind::StaticStore { value, .. } => {
            check_operand(body, block, value, state, violations);
        }
        StatementKind::IterSource { dst, source, .. } => {
            check_operand(body, block, source, state, violations);
            define(dst, body, block, state, violations);
        }
        StatementKind::IterAdapter {
            dst,
            upstream,
            closure_or_arg,
            ..
        } => {
            check_place(body, block, upstream, state, violations);
            if let Some(operand) = closure_or_arg {
                check_operand(body, block, operand, state, violations);
            }
            define(dst, body, block, state, violations);
        }
        StatementKind::IterNext {
            dst_option,
            iter_place,
            ..
        } => {
            check_place(body, block, iter_place, state, violations);
            define(dst_option, body, block, state, violations);
        }
        StatementKind::StorageLive(_) | StatementKind::Nop => {}
    }
}

fn callee_name(callee: &Operand) -> Option<&str> {
    match callee {
        Operand::Const(ConstValue::Str(name)) => Some(name.as_str()),
        _ => None,
    }
}

/// The local a call to `name` releases, when it is a release of a whole
/// local.
fn released_local<'a>(name: Option<&'a str>, args: &[Operand]) -> Option<(Local, &'a str)> {
    let name = name?;
    if !RELEASES.contains(&name) {
        return None;
    }
    match args.first() {
        Some(Operand::Copy(Place { local, projection })) if projection.is_empty() => {
            Some((*local, name))
        }
        _ => None,
    }
}

fn release(
    body: &Body,
    block: BlockId,
    local: Local,
    name: &str,
    state: &mut BTreeSet<Local>,
    violations: &mut Vec<RcViolation>,
) {
    if !state.insert(local) {
        violations.push(RcViolation::DoubleRelease {
            body: body.name.clone(),
            block,
            local,
            release: name.to_string(),
        });
    }
}

/// A write to `place`: a whole-local write gives the local a new value, and
/// a write into part of one reads the local it projects from.
fn define(
    place: &Place,
    body: &Body,
    block: BlockId,
    state: &mut BTreeSet<Local>,
    violations: &mut Vec<RcViolation>,
) {
    if place.projection.is_empty() {
        state.remove(&place.local);
    } else {
        check_place(body, block, place, state, violations);
    }
}

fn check_args(
    body: &Body,
    block: BlockId,
    args: &[Operand],
    released: Option<(Local, &str)>,
    state: &BTreeSet<Local>,
    violations: &mut Vec<RcViolation>,
) {
    for (position, operand) in args.iter().enumerate() {
        // The released handle itself is judged by `release`.
        if position == 0 && released.is_some() {
            continue;
        }
        check_operand(body, block, operand, state, violations);
    }
}

fn check_rvalue(
    body: &Body,
    block: BlockId,
    rvalue: &Rvalue,
    state: &BTreeSet<Local>,
    violations: &mut Vec<RcViolation>,
) {
    match rvalue {
        Rvalue::Use(operand)
        | Rvalue::UnaryOp { operand, .. }
        | Rvalue::Cast { operand, .. }
        | Rvalue::Repeat { value: operand, .. } => {
            check_operand(body, block, operand, state, violations);
        }
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            check_operand(body, block, lhs, state, violations);
            check_operand(body, block, rhs, state, violations);
        }
        Rvalue::Aggregate { operands, .. } | Rvalue::CallIntrinsic { args: operands, .. } => {
            for operand in operands {
                check_operand(body, block, operand, state, violations);
            }
        }
        Rvalue::Len(place) | Rvalue::Ref { place, .. } => {
            check_place(body, block, place, state, violations);
        }
        Rvalue::StaticLoad(_) => {}
    }
}

fn check_operand(
    body: &Body,
    block: BlockId,
    operand: &Operand,
    state: &BTreeSet<Local>,
    violations: &mut Vec<RcViolation>,
) {
    if let Operand::Copy(place) = operand {
        check_place(body, block, place, state, violations);
    }
}

fn check_place(
    body: &Body,
    block: BlockId,
    place: &Place,
    state: &BTreeSet<Local>,
    violations: &mut Vec<RcViolation>,
) {
    let indices = place.projection.iter().filter_map(|p| match p {
        Projection::Index(local) => Some(*local),
        _ => None,
    });
    for local in std::iter::once(place.local).chain(indices) {
        if state.contains(&local) {
            violations.push(RcViolation::UseAfterRelease {
                body: body.name.clone(),
                block,
                local,
            });
        }
    }
}

fn successors(terminator: &Terminator) -> Vec<BlockId> {
    match terminator {
        Terminator::Goto { target }
        | Terminator::Assert { target, .. }
        | Terminator::Drop { target, .. } => vec![*target],
        Terminator::SwitchInt { arms, default, .. } => arms
            .iter()
            .map(|(_, target)| *target)
            .chain(std::iter::once(*default))
            .collect(),
        Terminator::Call { target, .. } => target.iter().copied().collect(),
        Terminator::Return | Terminator::Unreachable | Terminator::Panic { .. } => Vec::new(),
    }
}

fn predecessors(body: &Body) -> Vec<Vec<usize>> {
    let mut out = vec![Vec::new(); body.blocks.len()];
    for (index, block) in body.blocks.iter().enumerate() {
        for successor in successors(&block.terminator) {
            if let Some(list) = out.get_mut(successor.0 as usize) {
                list.push(index);
            }
        }
    }
    out
}

/// Whether the verifier runs on every lowered body: always in a build with
/// debug assertions, and in any build when `GOS_VERIFY_RC` is set, which is
/// how the tier-parity suite runs it over every fixture.
#[must_use]
pub fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| cfg!(debug_assertions) || std::env::var_os("GOS_VERIFY_RC").is_some())
}

/// Panics with every fault in `bodies` when the verifier is enabled.
///
/// # Panics
///
/// Panics when a body releases a local twice or reads one after releasing
/// it on every path.
pub fn check_program(bodies: &[Body]) {
    if enabled() {
        reject_faults(bodies);
    }
}

/// Panics with every fault in `bodies`.
fn reject_faults(bodies: &[Body]) {
    let faults: Vec<RcViolation> = bodies
        .iter()
        .filter_map(|body| verify_rc(body).err())
        .flatten()
        .collect();
    assert!(
        faults.is_empty(),
        "reference-count verifier rejected the program:\n{}",
        faults
            .iter()
            .map(|fault| format!("  {fault:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::{RcViolation, verify_rc};
    use crate::ir::{
        BasicBlock, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue, Statement,
        StatementKind, Terminator,
    };

    fn free(local: u32, into: u32) -> Statement {
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(into)),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_vec_free",
                    args: vec![Operand::Copy(Place::local(Local(local)))],
                },
            },
            span: gossamer_lex::Span::default(),
            inlined: None,
        }
    }

    fn read(local: u32, into: u32) -> Statement {
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(into)),
                rvalue: Rvalue::Use(Operand::Copy(Place::local(Local(local)))),
            },
            span: gossamer_lex::Span::default(),
            inlined: None,
        }
    }

    fn body(blocks: Vec<(Vec<Statement>, Terminator)>) -> Body {
        let mut tcx = gossamer_types::TyCtxt::new();
        let int = tcx.int_ty(gossamer_types::IntTy::I64);
        Body {
            name: "probe".to_string(),
            def: None,
            arity: 0,
            locals: (0..8)
                .map(|_| LocalDecl {
                    ty: int,
                    debug_name: None,
                    mutable: true,
                    region: false,
                })
                .collect(),
            blocks: blocks
                .into_iter()
                .enumerate()
                .map(|(i, (stmts, terminator))| BasicBlock {
                    id: BlockId(u32::try_from(i).unwrap()),
                    stmts,
                    terminator,
                    span: gossamer_lex::Span::default(),
                    terminator_span: None,
                    terminator_inlined: None,
                })
                .collect(),
            span: gossamer_lex::Span::default(),
        }
    }

    #[test]
    fn a_read_after_a_release_is_reported() {
        let b = body(vec![(vec![free(1, 2), read(1, 3)], Terminator::Return)]);
        let faults = verify_rc(&b).unwrap_err();
        assert!(matches!(
            faults[0],
            RcViolation::UseAfterRelease {
                local: Local(1),
                ..
            }
        ));
    }

    #[test]
    fn a_second_release_is_reported() {
        let b = body(vec![(vec![free(1, 2), free(1, 3)], Terminator::Return)]);
        let faults = verify_rc(&b).unwrap_err();
        assert!(matches!(
            faults[0],
            RcViolation::DoubleRelease {
                local: Local(1),
                ..
            }
        ));
    }

    #[test]
    fn a_reassignment_ends_the_release() {
        let reassign = Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(1)),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            },
            span: gossamer_lex::Span::default(),
            inlined: None,
        };
        let b = body(vec![(
            vec![free(1, 2), reassign, read(1, 3), free(1, 4)],
            Terminator::Return,
        )]);
        assert!(verify_rc(&b).is_ok());
    }

    #[test]
    #[should_panic(expected = "reference-count verifier rejected the program")]
    fn a_program_with_a_fault_is_rejected() {
        super::reject_faults(&[body(vec![(
            vec![free(1, 2), free(1, 3)],
            Terminator::Return,
        )])]);
    }

    #[test]
    fn a_release_on_one_branch_only_is_not_a_fault_at_the_join() {
        let b = body(vec![
            (
                Vec::new(),
                Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::local(Local(5))),
                    arms: vec![(0, BlockId(1))],
                    default: BlockId(2),
                },
            ),
            (vec![free(1, 2)], Terminator::Goto { target: BlockId(3) }),
            (Vec::new(), Terminator::Goto { target: BlockId(3) }),
            (vec![read(1, 3)], Terminator::Return),
        ]);
        assert!(verify_rc(&b).is_ok());
    }
}
