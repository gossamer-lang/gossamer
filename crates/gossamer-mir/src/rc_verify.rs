//! Release discipline for reference-counted handles in MIR.
//!
//! A release call hands back the share one place holds - a whole local or a
//! field path inside one. From that point until something is assigned to the
//! place again, reading it reaches memory this body no longer owns, and
//! releasing it again gives up a share it does not have.
//!
//! Each program point carries two facts per place: released on every path
//! reaching it (a fault whichever way the body runs), and released on some
//! path (a fault on the paths that released it). A retain gives a place a
//! share beyond its own, which the next release of that place gives back
//! first. Which entry points release and retain is the ABI registry's
//! `Ownership`, not a list kept here.
//!
//! A leak - a share never released on some path - is the remaining failure;
//! the leak ledger and the runtime's reference trace measure that one.

use std::collections::{BTreeMap, BTreeSet};

use crate::ir::{
    BlockId, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, Statement, StatementKind,
    Terminator,
};

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
    /// `local` is read after some path here released it.
    ConditionalUseAfterRelease {
        /// Function the fault is in.
        body: String,
        /// Block holding the read.
        block: BlockId,
        /// The local released on some path.
        local: Local,
    },
    /// `local` is released again after some path here released it.
    ConditionalDoubleRelease {
        /// Function the fault is in.
        body: String,
        /// Block holding the second release.
        block: BlockId,
        /// The local released on some path.
        local: Local,
        /// The runtime entry point of the second release.
        release: String,
    },
}

/// A released place: a local and the field path inside it (empty for the
/// whole local).
type Key = (Local, Vec<u32>);

/// The most shares beyond its own the walk records for one place. Fewer
/// recorded shares only make a later release look like the place's own, so the
/// bound can report a fault the body does not have but never hides one, and it
/// keeps the walk finite around a loop that retains on every turn.
const MAX_EXTRA_SHARES: u32 = 4;

/// The released places at one program point.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct State {
    /// Released on every path reaching the point.
    must: BTreeSet<Key>,
    /// Released on some path reaching the point.
    may: BTreeSet<Key>,
    /// Shares a place holds beyond its own on every path reaching the point,
    /// taken by a retain and given back by the next release.
    extra: BTreeMap<Key, u32>,
}

impl State {
    fn join(&mut self, other: &Self) {
        self.must = self.must.intersection(&other.must).cloned().collect();
        self.may.extend(other.may.iter().cloned());
        self.extra = self
            .extra
            .iter()
            .filter_map(|(key, count)| {
                let theirs = other.extra.get(key).copied().unwrap_or(0);
                let both = (*count).min(theirs);
                (both > 0).then(|| (key.clone(), both))
            })
            .collect();
    }

    /// Forgets every released place `key` contains, and every share beyond
    /// its own: a write renews it.
    fn renew(&mut self, key: &Key) {
        let covered = |k: &Key| k.0 == key.0 && k.1.starts_with(&key.1);
        self.must.retain(|k| !covered(k));
        self.may.retain(|k| !covered(k));
        self.extra.retain(|k, _| !covered(k));
    }

    fn retain(&mut self, key: Key) {
        let count = self.extra.entry(key).or_insert(0);
        *count = (*count + 1).min(MAX_EXTRA_SHARES);
    }

    /// Gives back a share `key` holds beyond its own, answering whether it
    /// had one.
    fn give_back_extra(&mut self, key: &Key) -> bool {
        let Some(count) = self.extra.get_mut(key) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            self.extra.remove(key);
        }
        true
    }

    fn forget_local(&mut self, local: Local) {
        self.renew(&(local, Vec::new()));
    }
}

/// Checks `body`'s release discipline, answering every fault it finds.
pub fn verify_rc(body: &Body) -> Result<(), Vec<RcViolation>> {
    let n = body.blocks.len();
    if n == 0 {
        return Ok(());
    }
    let mut entry_state: Vec<Option<State>> = vec![None; n];
    entry_state[0] = Some(State::default());
    let predecessors = predecessors(body);
    let mut worklist: Vec<usize> = vec![0];
    let mut exit_state: Vec<Option<State>> = vec![None; n];
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
                .fold(None::<State>, |acc, state| match acc {
                    None => Some(state.clone()),
                    Some(mut acc) => {
                        acc.join(state);
                        Some(acc)
                    }
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
/// and answers the released places at the block's end.
fn transfer(
    body: &Body,
    index: usize,
    mut state: State,
    violations: &mut Vec<RcViolation>,
) -> State {
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
            let released = released_place(callee_name(callee), args);
            check_args(body, block.id, args, released.is_some(), &state, violations);
            if let Some((key, name)) = released {
                release(body, block.id, key, name, &mut state, violations);
            }
            if let Some(key) = retained_place(callee_name(callee), args) {
                state.retain(key);
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
        | Terminator::Resume
        | Terminator::Panic { .. } => {}
    }
    state
}

fn step_statement(
    body: &Body,
    block: BlockId,
    stmt: &Statement,
    state: &mut State,
    violations: &mut Vec<RcViolation>,
) {
    match &stmt.kind {
        StatementKind::Assign { place, rvalue } => {
            if let Rvalue::CallIntrinsic { name, args } = rvalue {
                let released = released_place(Some(name), args);
                check_args(body, block, args, released.is_some(), state, violations);
                if let Some((key, name)) = released {
                    release(body, block, key, name, state, violations);
                }
                if let Some(key) = retained_place(Some(name), args) {
                    state.retain(key);
                }
            } else {
                check_rvalue(body, block, rvalue, state, violations);
            }
            define(place, body, block, state, violations);
        }
        StatementKind::StorageDead(local) => state.forget_local(*local),
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

/// Whether the runtime entry point `name` gives up its first argument's share.
fn releases(name: &str) -> bool {
    gossamer_abi::registry::lookup(name).is_some_and(gossamer_abi::RuntimeEntry::releases_first_arg)
}

/// The leading field path of `place`, up to its first projection that is not
/// a field.
fn field_prefix(place: &Place) -> Vec<u32> {
    place
        .projection
        .iter()
        .map_while(|p| match p {
            Projection::Field(index) => Some(*index),
            _ => None,
        })
        .collect()
}

/// The place a call to `name` releases, when it is a release of a whole local
/// or of a field path inside one.
fn released_place<'a>(name: Option<&'a str>, args: &[Operand]) -> Option<(Key, &'a str)> {
    let name = name?;
    if !releases(name) {
        return None;
    }
    match args.first() {
        Some(Operand::Copy(place))
            if place
                .projection
                .iter()
                .all(|p| matches!(p, Projection::Field(_))) =>
        {
            Some(((place.local, field_prefix(place)), name))
        }
        _ => None,
    }
}

/// The place a call to `name` gives a share beyond its own, when it is a
/// retain of a whole local or of a field path inside one.
fn retained_place(name: Option<&str>, args: &[Operand]) -> Option<Key> {
    let name = name?;
    if !gossamer_abi::registry::lookup(name)
        .is_some_and(gossamer_abi::RuntimeEntry::retains_first_arg)
    {
        return None;
    }
    match args.first() {
        Some(Operand::Copy(place))
            if place
                .projection
                .iter()
                .all(|p| matches!(p, Projection::Field(_))) =>
        {
            Some((place.local, field_prefix(place)))
        }
        _ => None,
    }
}

/// Whether `released` covers the place `key` reaches: the released place is
/// the key's place or one containing it.
fn covers(released: &Key, key: &Key) -> bool {
    released.0 == key.0 && key.1.starts_with(&released.1)
}

fn release(
    body: &Body,
    block: BlockId,
    key: Key,
    name: &str,
    state: &mut State,
    violations: &mut Vec<RcViolation>,
) {
    if state.give_back_extra(&key) {
        return;
    }
    if state.must.iter().any(|r| covers(r, &key)) {
        violations.push(RcViolation::DoubleRelease {
            body: body.name.clone(),
            block,
            local: key.0,
            release: name.to_string(),
        });
    } else if state.may.iter().any(|r| covers(r, &key)) {
        violations.push(RcViolation::ConditionalDoubleRelease {
            body: body.name.clone(),
            block,
            local: key.0,
            release: name.to_string(),
        });
    }
    state.must.insert(key.clone());
    state.may.insert(key);
}

/// A write to `place`: it renews the place it names, and a write into part of
/// a local reads the parts of the local around it.
fn define(
    place: &Place,
    body: &Body,
    block: BlockId,
    state: &mut State,
    violations: &mut Vec<RcViolation>,
) {
    let fields = field_prefix(place);
    if fields.len() == place.projection.len() {
        for depth in 0..fields.len() {
            check_key(
                body,
                block,
                &(place.local, fields[..depth].to_vec()),
                state,
                violations,
            );
        }
        state.renew(&(place.local, fields));
    } else {
        check_place(body, block, place, state, violations);
    }
}

fn check_args(
    body: &Body,
    block: BlockId,
    args: &[Operand],
    first_is_released: bool,
    state: &State,
    violations: &mut Vec<RcViolation>,
) {
    for (position, operand) in args.iter().enumerate() {
        // The released handle itself is judged by `release`.
        if position == 0 && first_is_released {
            continue;
        }
        check_operand(body, block, operand, state, violations);
    }
}

fn check_rvalue(
    body: &Body,
    block: BlockId,
    rvalue: &Rvalue,
    state: &State,
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
    state: &State,
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
    state: &State,
    violations: &mut Vec<RcViolation>,
) {
    check_key(
        body,
        block,
        &(place.local, field_prefix(place)),
        state,
        violations,
    );
    for projection in &place.projection {
        if let Projection::Index(local) = projection {
            check_key(body, block, &(*local, Vec::new()), state, violations);
        }
    }
}

/// Reports a read of `key` that a released place covers.
fn check_key(
    body: &Body,
    block: BlockId,
    key: &Key,
    state: &State,
    violations: &mut Vec<RcViolation>,
) {
    if state.must.iter().any(|r| covers(r, key)) {
        violations.push(RcViolation::UseAfterRelease {
            body: body.name.clone(),
            block,
            local: key.0,
        });
    } else if state.may.iter().any(|r| covers(r, key)) {
        violations.push(RcViolation::ConditionalUseAfterRelease {
            body: body.name.clone(),
            block,
            local: key.0,
        });
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
        Terminator::Return
        | Terminator::Unreachable
        | Terminator::Resume
        | Terminator::Panic { .. } => Vec::new(),
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
    if std::env::var_os("GOS_VERIFY_RC_REPORT").is_some() {
        for fault in &faults {
            eprintln!("rc-verify: {fault:?}");
        }
        return;
    }
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

    fn retain(local: u32, into: u32) -> Statement {
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(into)),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_vec_retain",
                    args: vec![Operand::Copy(Place::local(Local(local)))],
                },
            },
            span: gossamer_lex::Span::default(),
            inlined: None,
        }
    }

    #[test]
    fn a_release_after_a_retain_gives_back_the_retained_share() {
        let b = body(vec![(
            vec![retain(1, 2), free(1, 3), read(1, 4), free(1, 6)],
            Terminator::Return,
        )]);
        assert!(verify_rc(&b).is_ok());
    }

    #[test]
    fn a_retain_covers_one_release_only() {
        let b = body(vec![(
            vec![retain(1, 2), free(1, 3), free(1, 4), free(1, 6)],
            Terminator::Return,
        )]);
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
    fn a_retain_on_some_paths_covers_no_release_after_them() {
        let faults =
            verify_rc(&branchy(vec![retain(1, 2)], vec![free(1, 3), free(1, 4)])).unwrap_err();
        assert!(matches!(
            faults[0],
            RcViolation::DoubleRelease {
                local: Local(1),
                ..
            }
        ));
    }

    fn branchy(then_block: Vec<Statement>, join: Vec<Statement>) -> Body {
        body(vec![
            (
                Vec::new(),
                Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::local(Local(5))),
                    arms: vec![(0, BlockId(1))],
                    default: BlockId(2),
                },
            ),
            (then_block, Terminator::Goto { target: BlockId(3) }),
            (Vec::new(), Terminator::Goto { target: BlockId(3) }),
            (join, Terminator::Return),
        ])
    }

    fn field(local: u32, index: u32) -> Place {
        Place {
            local: Local(local),
            projection: vec![crate::ir::Projection::Field(index)],
        }
    }

    fn free_place(place: Place, into: u32) -> Statement {
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(into)),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_vec_free",
                    args: vec![Operand::Copy(place)],
                },
            },
            span: gossamer_lex::Span::default(),
            inlined: None,
        }
    }

    fn read_place(place: Place, into: u32) -> Statement {
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(into)),
                rvalue: Rvalue::Use(Operand::Copy(place)),
            },
            span: gossamer_lex::Span::default(),
            inlined: None,
        }
    }

    #[test]
    fn a_read_after_a_release_on_one_branch_is_reported() {
        let faults = verify_rc(&branchy(vec![free(1, 2)], vec![read(1, 3)])).unwrap_err();
        assert!(matches!(
            faults[0],
            RcViolation::ConditionalUseAfterRelease {
                local: Local(1),
                ..
            }
        ));
    }

    #[test]
    fn a_release_after_a_release_on_one_branch_is_reported() {
        let faults = verify_rc(&branchy(vec![free(1, 2)], vec![free(1, 3)])).unwrap_err();
        assert!(matches!(
            faults[0],
            RcViolation::ConditionalDoubleRelease {
                local: Local(1),
                ..
            }
        ));
    }

    #[test]
    fn releases_on_both_branches_balance_a_later_reassignment() {
        let reassign = Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(1)),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            },
            span: gossamer_lex::Span::default(),
            inlined: None,
        };
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
            (vec![free(1, 4)], Terminator::Goto { target: BlockId(3) }),
            (vec![reassign, read(1, 3)], Terminator::Return),
        ]);
        assert!(verify_rc(&b).is_ok());
    }

    #[test]
    fn a_read_of_a_released_field_is_reported_and_its_sibling_is_not() {
        let b = body(vec![(
            vec![
                free_place(field(1, 1), 2),
                read_place(field(1, 0), 3),
                read_place(field(1, 1), 4),
            ],
            Terminator::Return,
        )]);
        let faults = verify_rc(&b).unwrap_err();
        assert_eq!(faults.len(), 1, "{faults:?}");
        assert!(matches!(
            faults[0],
            RcViolation::UseAfterRelease {
                local: Local(1),
                ..
            }
        ));
    }

    #[test]
    fn a_field_write_renews_the_released_field() {
        let refill = Statement {
            kind: StatementKind::Assign {
                place: field(1, 1),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            },
            span: gossamer_lex::Span::default(),
            inlined: None,
        };
        let b = body(vec![(
            vec![
                free_place(field(1, 1), 2),
                refill,
                free_place(field(1, 1), 3),
            ],
            Terminator::Return,
        )]);
        assert!(verify_rc(&b).is_ok());
    }

    #[test]
    fn a_whole_release_covers_every_field() {
        let b = body(vec![(
            vec![free(1, 2), read_place(field(1, 0), 3)],
            Terminator::Return,
        )]);
        assert!(matches!(
            verify_rc(&b).unwrap_err()[0],
            RcViolation::UseAfterRelease {
                local: Local(1),
                ..
            }
        ));
    }

    #[test]
    fn a_release_carried_around_a_loop_is_reported() {
        let b = body(vec![
            (Vec::new(), Terminator::Goto { target: BlockId(1) }),
            (
                vec![free(1, 2)],
                Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::local(Local(5))),
                    arms: vec![(0, BlockId(2))],
                    default: BlockId(1),
                },
            ),
            (Vec::new(), Terminator::Return),
        ]);
        assert!(matches!(
            verify_rc(&b).unwrap_err()[0],
            RcViolation::ConditionalDoubleRelease {
                local: Local(1),
                ..
            }
        ));
    }

    #[test]
    fn every_release_helper_the_drop_pass_emits_is_a_registry_release() {
        for name in [
            "gos_rt_rc_release",
            "gos_rt_rc_weak_release",
            "gos_rt_vec_free",
            "gos_rt_str_free_typed",
            "gos_rt_map_free",
            "gos_rt_set_free",
            "gos_rt_deque_free",
            "gos_rt_lazy_iter_drop_i64",
            "gos_rt_lazy_iter_drop_pair_i64",
        ] {
            assert!(
                super::releases(name),
                "{name} is not marked Ownership::ReleasesFirstArg"
            );
        }
        assert!(!super::releases("gos_rt_vec_retain"));
    }
}
