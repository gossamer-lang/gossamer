//! Uniqueness inference over reference-counted handles.
//!
//! A local is [`Uniqueness::Unique`] at a program point when the object it
//! holds, and everything that object reaches, is observable through that
//! local alone: no other live local, field, container, global, closure
//! environment, or goroutine can read it, and no reference to the local is
//! live. A unique value can be handed to a new holder instead of copied, and
//! the share it carries can move with it instead of being retained and
//! released around the handoff.
//!
//! Retains and releases are bookkeeping for holders made elsewhere, so the
//! analysis reads neither as creating or ending an observer. What makes a
//! second observer is the copy, store, aggregate, capture, or call that hands
//! the handle on, and that is what the transfer functions model. A local
//! whose only remaining mentions are its own releases is dead: a copy out of
//! it is a move, the destination inherits its state, and the local itself
//! becomes shared, since its handle now also lives elsewhere.
//!
//! The analysis may only over-approximate sharing. Reporting a unique value
//! as shared costs a copy or a count adjustment; reporting a shared value as
//! unique lets a pass free or mutate memory another holder still reads.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use gossamer_resolve::DefId;
use gossamer_types::{Ty, TyCtxt, TyKind};

use crate::ir::{
    BasicBlock, BlockId, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, Statement,
    StatementKind, Terminator,
};

/// How many observers a local's object has at a program point.
///
/// Ordered by how much a pass may assume: a join takes the larger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Uniqueness {
    /// The local is the only observer of its object.
    Unique,
    /// The local is the only holder, but a reference to it is live.
    Borrowed,
    /// Another holder may observe the object.
    Shared,
}

/// A program point: before statement `stmt` of `block`, or before the
/// block's terminator when `stmt` is the statement count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    /// The block holding the point.
    pub block: BlockId,
    /// The statement index within the block.
    pub stmt: usize,
}

/// What a call does with its arguments and result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSummary {
    /// Per parameter: whether the callee may keep a handle to the argument
    /// past the call.
    pub retains: Vec<bool>,
    /// Whether the value the callee answers is observable only through the
    /// caller's destination.
    pub returns_unique: bool,
}

impl CallSummary {
    /// The starting point of a fixed-point iteration: a callee that keeps
    /// nothing and answers a fresh value.
    fn optimistic(arity: usize) -> Self {
        Self {
            retains: vec![false; arity],
            returns_unique: true,
        }
    }

    /// Widens `self` by `other`: a parameter either may retain stays
    /// retained, and a result either may share stays shared.
    fn widen(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (mine, theirs) in self.retains.iter_mut().zip(&other.retains) {
            if *theirs && !*mine {
                *mine = true;
                changed = true;
            }
        }
        if self.returns_unique && !other.returns_unique {
            self.returns_unique = false;
            changed = true;
        }
        changed
    }
}

/// Call summaries for every function in a program, keyed by the name a call
/// site spells.
#[derive(Clone, Debug, Default)]
pub struct CallSummaries {
    by_name: HashMap<String, CallSummary>,
    name_of_def: HashMap<DefId, String>,
}

impl CallSummaries {
    /// Summaries for `bodies`, computed callees first. A recursive group
    /// starts from the optimistic summary and widens until no member's
    /// summary changes.
    #[must_use]
    pub fn compute(bodies: &[Body], tcx: &TyCtxt) -> Self {
        let mut summaries = Self {
            by_name: HashMap::new(),
            name_of_def: bodies
                .iter()
                .filter_map(|body| body.def.map(|def| (def, body.name.clone())))
                .collect(),
        };
        let index: HashMap<&str, usize> = bodies
            .iter()
            .enumerate()
            .map(|(i, body)| (body.name.as_str(), i))
            .collect();
        let edges: Vec<Vec<usize>> = bodies
            .iter()
            .map(|body| {
                let mut out: Vec<usize> = callee_names(body, &summaries.name_of_def)
                    .into_iter()
                    .filter_map(|name| index.get(name.as_str()).copied())
                    .collect();
                out.sort_unstable();
                out.dedup();
                out
            })
            .collect();
        for group in strongly_connected_components(&edges) {
            for &member in &group {
                let body = &bodies[member];
                summaries.by_name.insert(
                    body.name.clone(),
                    CallSummary::optimistic(body.arity as usize),
                );
            }
            loop {
                let mut changed = false;
                for &member in &group {
                    let body = &bodies[member];
                    let computed = summarize(body, tcx, &summaries);
                    if let Some(current) = summaries.by_name.get_mut(&body.name) {
                        changed |= current.widen(&computed);
                    }
                }
                if !changed {
                    break;
                }
            }
        }
        summaries
    }

    /// The summary for a call through `callee`, or `None` when the callee is
    /// not a function of the program (a runtime helper or a dynamic call).
    #[must_use]
    pub fn get(&self, callee: &Operand) -> Option<&CallSummary> {
        match callee {
            Operand::Const(ConstValue::Str(name)) => self.by_name.get(name),
            Operand::FnRef { def, .. } => self
                .name_of_def
                .get(def)
                .and_then(|name| self.by_name.get(name)),
            _ => None,
        }
    }
}

/// The names of the program functions `body` calls.
fn callee_names(body: &Body, name_of_def: &HashMap<DefId, String>) -> Vec<String> {
    body.blocks
        .iter()
        .filter_map(|block| match &block.terminator {
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                ..
            } => Some(name.clone()),
            Terminator::Call {
                callee: Operand::FnRef { def, .. },
                ..
            } => name_of_def.get(def).cloned(),
            _ => None,
        })
        .collect()
}

/// Strongly connected components of the graph `edges`, each component
/// listed after every component it reaches.
fn strongly_connected_components(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNVISITED: usize = usize::MAX;
    let n = edges.len();
    let mut index = vec![UNVISITED; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut next = 0;
    for root in 0..n {
        if index[root] != UNVISITED {
            continue;
        }
        // Each frame is a node and the position of the next edge to follow.
        let mut frames: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(frame) = frames.last_mut() {
            let node = frame.0;
            if let Some(&succ) = edges[node].get(frame.1) {
                frame.1 += 1;
                if index[succ] == UNVISITED {
                    index[succ] = next;
                    low[succ] = next;
                    next += 1;
                    stack.push(succ);
                    on_stack[succ] = true;
                    frames.push((succ, 0));
                } else if on_stack[succ] {
                    low[node] = low[node].min(index[succ]);
                }
                continue;
            }
            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                let mut group = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    group.push(member);
                    if member == node {
                        break;
                    }
                }
                out.push(group);
            }
        }
    }
    out
}

/// The summary of `body` given the summaries of what it calls.
fn summarize(body: &Body, tcx: &TyCtxt, summaries: &CallSummaries) -> CallSummary {
    let facts = analyze(body, tcx, summaries);
    let returns_unique = body.locals.first().is_none_or(|decl| !tracks(tcx, decl.ty))
        || body.blocks.iter().all(|block| {
            !matches!(block.terminator, Terminator::Return)
                || facts.at(
                    Point {
                        block: block.id,
                        stmt: block.stmts.len(),
                    },
                    Local::RETURN,
                ) == Uniqueness::Unique
        });
    let retains = (1..=body.arity)
        .map(|param| param_escapes(body, tcx, summaries, Local(param)))
        .collect();
    CallSummary {
        retains,
        returns_unique,
    }
}

/// Whether `body` may keep a handle to parameter `param` past the call: any
/// mention of it other than a read that answers a scalar or a call that keeps
/// no handle.
fn param_escapes(body: &Body, tcx: &TyCtxt, summaries: &CallSummaries, param: Local) -> bool {
    let ty = body.local_ty(param);
    if !tracks(tcx, ty) {
        return false;
    }
    let reads_scalar = |place: &Place| {
        place.local == param && place_ty(tcx, body, place).is_some_and(|t| !tracks(tcx, t))
    };
    let names = |op: &Operand| matches!(op, Operand::Copy(p) if place_mentions(p, param));
    for block in &body.blocks {
        for stmt in &block.stmts {
            let escapes = match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    place_mentions(place, param)
                        || match rvalue {
                            Rvalue::Use(Operand::Copy(p)) | Rvalue::Cast { operand: Operand::Copy(p), .. } => {
                                place_mentions(p, param) && !reads_scalar(p)
                            }
                            Rvalue::Use(_) | Rvalue::Cast { .. } | Rvalue::StaticLoad(_) => false,
                            Rvalue::Len(_) => false,
                            Rvalue::BinaryOp { lhs, rhs, .. } => [lhs, rhs].into_iter().any(|op| {
                                matches!(op, Operand::Copy(p) if place_mentions(p, param) && !reads_scalar(p))
                            }),
                            Rvalue::UnaryOp { operand, .. } => {
                                matches!(operand, Operand::Copy(p) if place_mentions(p, param) && !reads_scalar(p))
                            }
                            Rvalue::Aggregate { operands, .. } => operands.iter().any(|op| {
                                matches!(op, Operand::Copy(p) if place_mentions(p, param) && !reads_scalar(p))
                            }),
                            Rvalue::Repeat { value, .. } => names(value),
                            Rvalue::Ref { place, .. } => place_mentions(place, param),
                            Rvalue::CallIntrinsic { name, args } => {
                                args.iter().enumerate().any(|(i, op)| {
                                    names(op) && !runtime_arg_kept_no_handle(name, i)
                                })
                            }
                        }
                }
                StatementKind::StaticStore { value, .. } => names(value),
                StatementKind::SetDiscriminant { place, .. } => place_mentions(place, param),
                StatementKind::IterSource { dst, source, .. } => {
                    place_mentions(dst, param) || names(source)
                }
                StatementKind::IterAdapter {
                    dst,
                    upstream,
                    closure_or_arg,
                    ..
                } => {
                    place_mentions(dst, param)
                        || place_mentions(upstream, param)
                        || closure_or_arg.as_ref().is_some_and(names)
                }
                _ => false,
            };
            if escapes {
                return true;
            }
        }
        let escapes = match &block.terminator {
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                place_mentions(destination, param)
                    || names(callee)
                    || args.iter().enumerate().any(|(i, op)| {
                        let Operand::Copy(p) = op else {
                            return false;
                        };
                        if !place_mentions(p, param) || reads_scalar(p) {
                            return false;
                        }
                        match (summaries.get(callee), callee) {
                            (Some(summary), _) => summary.retains.get(i).copied().unwrap_or(true),
                            (None, Operand::Const(ConstValue::Str(name))) => {
                                !runtime_arg_kept_no_handle(name, i)
                            }
                            (None, _) => true,
                        }
                    })
            }
            Terminator::Drop { place, .. } => place_mentions(place, param),
            _ => false,
        };
        if escapes {
            return true;
        }
    }
    false
}

/// Per-body uniqueness facts, answered at any program point.
pub struct UniquenessFacts<'b> {
    body: &'b Body,
    tcx: &'b TyCtxt,
    summaries: &'b CallSummaries,
    entry: Vec<State>,
    liveness: Liveness,
    goroutine_shared: Vec<bool>,
}

impl UniquenessFacts<'_> {
    /// The uniqueness of `local` just before `point`.
    #[must_use]
    pub fn at(&self, point: Point, local: Local) -> Uniqueness {
        let li = local.0 as usize;
        let Some(decl) = self.body.locals.get(li) else {
            return Uniqueness::Shared;
        };
        if decl.region
            || self.goroutine_shared.get(li).copied().unwrap_or(true)
            || !tracks(self.tcx, decl.ty)
        {
            return Uniqueness::Shared;
        }
        let state = self.state_before(point);
        if state.base[li] == Base::Shared {
            return Uniqueness::Shared;
        }
        let live = self.liveness.live_before(self.body, point);
        let borrowed = state
            .borrowers
            .get(&local.0)
            .is_some_and(|refs| refs.iter().any(|r| live.contains(Local(*r))));
        if borrowed {
            Uniqueness::Borrowed
        } else {
            Uniqueness::Unique
        }
    }

    /// Whether `local` is read after `point` by anything other than its own
    /// releases. A release of a local no one reads again is where its share
    /// ends, not an observer of its object.
    #[must_use]
    pub fn live_after(&self, point: Point, local: Local) -> bool {
        self.liveness.live_after(self.body, point).contains(local)
    }

    /// Whether `local` is read on entry to `block`.
    #[must_use]
    pub fn live_on_entry(&self, block: BlockId, local: Local) -> bool {
        self.liveness.live_in[block.0 as usize].contains(local)
    }

    /// Whether any reference to `local` may be taken in this body.
    #[must_use]
    pub fn has_borrowers(&self, local: Local) -> bool {
        self.entry
            .iter()
            .any(|state| state.borrowers.get(&local.0).is_some_and(|r| !r.is_empty()))
            || self.body.blocks.iter().any(|block| {
                block.stmts.iter().any(|stmt| {
                    matches!(
                        &stmt.kind,
                        StatementKind::Assign { rvalue: Rvalue::Ref { place, .. }, .. }
                            if place.local == local
                    )
                })
            })
    }

    fn state_before(&self, point: Point) -> State {
        let bi = point.block.0 as usize;
        let block = &self.body.blocks[bi];
        let mut state = self.entry[bi].clone();
        let live_after = self.liveness.live_after_each(self.body, bi);
        let transfer = Transfer {
            body: self.body,
            tcx: self.tcx,
            summaries: self.summaries,
        };
        for (si, stmt) in block.stmts.iter().enumerate().take(point.stmt) {
            transfer.statement(&mut state, stmt, &live_after[si]);
        }
        state
    }
}

/// Computes [`UniquenessFacts`] for `body`.
#[must_use]
pub fn analyze<'b>(
    body: &'b Body,
    tcx: &'b TyCtxt,
    summaries: &'b CallSummaries,
) -> UniquenessFacts<'b> {
    let n_locals = body.locals.len();
    let n_blocks = body.blocks.len();
    let liveness = Liveness::compute(body);
    let share = crate::ownership::ShareFacts::compute(body);
    let goroutine_shared = (0..n_locals)
        .map(|i| share.is_goroutine_shared(Local(u32::try_from(i).unwrap_or(u32::MAX))))
        .collect();
    let mut start = State::new(n_locals);
    for param in 1..=body.arity as usize {
        if param < n_locals {
            start.base[param] = Base::Shared;
        }
    }
    let mut entry: Vec<Option<State>> = vec![None; n_blocks];
    if n_blocks > 0 {
        entry[0] = Some(start);
    }
    let transfer = Transfer {
        body,
        tcx,
        summaries,
    };
    let mut work: Vec<usize> = vec![0];
    let mut queued = vec![false; n_blocks];
    if n_blocks > 0 {
        queued[0] = true;
    }
    while let Some(bi) = work.pop() {
        queued[bi] = false;
        let Some(mut state) = entry[bi].clone() else {
            continue;
        };
        let block = &body.blocks[bi];
        let live_after = liveness.live_after_each(body, bi);
        for (si, stmt) in block.stmts.iter().enumerate() {
            transfer.statement(&mut state, stmt, &live_after[si]);
        }
        let out = transfer.terminator(state, block, &live_after[block.stmts.len()]);
        for (succ, succ_state) in out {
            let si = succ.0 as usize;
            if si >= n_blocks {
                continue;
            }
            let changed = match &mut entry[si] {
                Some(existing) => existing.join(&succ_state),
                slot @ None => {
                    *slot = Some(succ_state);
                    true
                }
            };
            if changed && !queued[si] {
                queued[si] = true;
                work.push(si);
            }
        }
    }
    UniquenessFacts {
        body,
        tcx,
        summaries,
        entry: entry
            .into_iter()
            .map(|state| state.unwrap_or_else(|| State::new(n_locals)))
            .collect(),
        liveness,
        goroutine_shared,
    }
}

/// A local's own share of the lattice, before references are considered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Base {
    /// Not yet given a value on any path reaching the point.
    Uninit,
    Unique,
    Shared,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    base: Vec<Base>,
    /// For each local a reference has been taken to, the reference locals.
    borrowers: BTreeMap<u32, BTreeSet<u32>>,
}

impl State {
    fn new(n_locals: usize) -> Self {
        Self {
            base: vec![Base::Uninit; n_locals],
            borrowers: BTreeMap::new(),
        }
    }

    fn join(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (mine, theirs) in self.base.iter_mut().zip(&other.base) {
            if *theirs > *mine {
                *mine = *theirs;
                changed = true;
            }
        }
        for (root, refs) in &other.borrowers {
            let mine = self.borrowers.entry(*root).or_default();
            for r in refs {
                changed |= mine.insert(*r);
            }
        }
        changed
    }

    fn base_of(&self, local: Local) -> Base {
        self.base
            .get(local.0 as usize)
            .copied()
            .unwrap_or(Base::Shared)
    }

    fn set(&mut self, local: Local, base: Base) {
        if let Some(slot) = self.base.get_mut(local.0 as usize) {
            *slot = base;
        }
    }

    fn widen(&mut self, local: Local, base: Base) {
        if let Some(slot) = self.base.get_mut(local.0 as usize)
            && base > *slot
        {
            *slot = base;
        }
    }

    /// The locals `reference` is a reference to.
    fn roots_of(&self, reference: Local) -> Vec<Local> {
        self.borrowers
            .iter()
            .filter(|(_, refs)| refs.contains(&reference.0))
            .map(|(root, _)| Local(*root))
            .collect()
    }

    /// Marks `local` shared, and everything it is a reference to.
    fn share(&mut self, local: Local) {
        self.set(local, Base::Shared);
        for root in self.roots_of(local) {
            self.set(root, Base::Shared);
        }
    }

    fn forget_reference(&mut self, reference: Local) {
        for refs in self.borrowers.values_mut() {
            refs.remove(&reference.0);
        }
    }

    fn add_borrower(&mut self, root: Local, reference: Local) {
        if root != reference {
            self.borrowers
                .entry(root.0)
                .or_default()
                .insert(reference.0);
        }
    }
}

struct Transfer<'b> {
    body: &'b Body,
    tcx: &'b TyCtxt,
    summaries: &'b CallSummaries,
}

impl Transfer<'_> {
    fn is_reference(&self, local: Local) -> bool {
        self.body
            .locals
            .get(local.0 as usize)
            .is_some_and(|decl| matches!(self.tcx.kind_of(decl.ty), TyKind::Ref { .. }))
    }

    /// Whether the value `place` reads can carry a handle.
    fn carries_handle(&self, place: &Place) -> bool {
        place_ty(self.tcx, self.body, place).is_none_or(|ty| tracks(self.tcx, ty))
    }

    /// The state an operand hands to whatever it is written into, applying
    /// what handing it on does to its source: a live source now shares its
    /// object, and a dead one moves it.
    fn hand_on(&self, state: &mut State, op: &Operand, live_after: &LocalSet) -> Base {
        match op {
            Operand::Const(ConstValue::Str(_)) => Base::Shared,
            Operand::Const(_) | Operand::FnRef { .. } => Base::Unique,
            Operand::Copy(place) => {
                if !self.carries_handle(place) {
                    return Base::Unique;
                }
                let root = place.local;
                let through_reference =
                    place.projection.first() == Some(&Projection::Deref) || self.is_reference(root);
                if through_reference {
                    state.share(root);
                    return Base::Shared;
                }
                if !place.projection.is_empty() {
                    state.share(root);
                    return Base::Shared;
                }
                let borrowed = state
                    .borrowers
                    .get(&root.0)
                    .is_some_and(|refs| refs.iter().any(|r| live_after.contains(Local(*r))));
                if live_after.contains(root) || borrowed {
                    state.share(root);
                    Base::Shared
                } else {
                    let moved = state.base_of(root);
                    state.set(root, Base::Shared);
                    if moved == Base::Uninit {
                        Base::Unique
                    } else {
                        moved
                    }
                }
            }
        }
    }

    /// Writes a value of state `incoming` into `place`.
    fn write(&self, state: &mut State, place: &Place, incoming: Base) {
        let root = place.local;
        if place.projection.is_empty() {
            state.set(root, incoming);
            return;
        }
        if place.projection.first() == Some(&Projection::Deref) || self.is_reference(root) {
            for target in state.roots_of(root) {
                state.widen(target, incoming);
            }
            state.widen(root, incoming);
            return;
        }
        state.widen(root, incoming);
    }

    fn statement(&self, state: &mut State, stmt: &Statement, live_after: &LocalSet) {
        match &stmt.kind {
            StatementKind::Assign { place, rvalue } => {
                if place.projection.is_empty() && self.is_reference(place.local) {
                    state.forget_reference(place.local);
                }
                let incoming = self.rvalue(state, place, rvalue, live_after);
                self.write(state, place, incoming);
            }
            StatementKind::StaticStore {
                value: Operand::Copy(p),
                ..
            } => state.share(p.local),
            StatementKind::IterSource { dst, source, .. } => {
                if let Operand::Copy(p) = source {
                    state.share(p.local);
                }
                state.share(dst.local);
            }
            StatementKind::IterAdapter {
                dst,
                upstream,
                closure_or_arg,
                ..
            } => {
                state.share(upstream.local);
                if let Some(Operand::Copy(p)) = closure_or_arg {
                    state.share(p.local);
                }
                state.share(dst.local);
            }
            _ => {}
        }
    }

    fn rvalue(
        &self,
        state: &mut State,
        dest: &Place,
        rvalue: &Rvalue,
        live_after: &LocalSet,
    ) -> Base {
        match rvalue {
            Rvalue::Use(op) | Rvalue::Cast { operand: op, .. } => {
                if let Operand::Copy(src) = op
                    && src.projection.is_empty()
                    && self.is_reference(src.local)
                    && dest.projection.is_empty()
                {
                    for root in state.roots_of(src.local) {
                        state.add_borrower(root, dest.local);
                    }
                }
                self.hand_on(state, op, live_after)
            }
            Rvalue::Aggregate { operands, .. } => operands
                .iter()
                .map(|op| self.hand_on(state, op, live_after))
                .max()
                .unwrap_or(Base::Unique),
            Rvalue::Repeat { value, count } => {
                let handed = self.hand_on(state, value, live_after);
                let copies_a_handle = matches!(value, Operand::Copy(p) if self.carries_handle(p));
                if *count > 1 && copies_a_handle {
                    Base::Shared
                } else {
                    handed
                }
            }
            Rvalue::Ref { place, .. } => {
                if dest.projection.is_empty() {
                    if place.projection.first() == Some(&Projection::Deref)
                        || self.is_reference(place.local)
                    {
                        for root in state.roots_of(place.local) {
                            state.add_borrower(root, dest.local);
                        }
                    }
                    state.add_borrower(place.local, dest.local);
                } else {
                    state.share(place.local);
                }
                Base::Unique
            }
            Rvalue::BinaryOp { .. } | Rvalue::UnaryOp { .. } | Rvalue::Len(_) => Base::Unique,
            Rvalue::StaticLoad(_) => Base::Shared,
            Rvalue::CallIntrinsic { name, args } => {
                if is_share_neutral_intrinsic(name) {
                    return Base::Unique;
                }
                if is_goroutine_publish(name) {
                    for op in args {
                        if let Operand::Copy(p) = op {
                            state.share(p.local);
                        }
                    }
                    return Base::Unique;
                }
                self.call_effects(
                    state,
                    args,
                    |i| runtime_arg_kept_no_handle(name, i),
                    live_after,
                );
                let scalar =
                    place_ty(self.tcx, self.body, dest).is_some_and(|ty| !tracks(self.tcx, ty));
                if runtime_answers_fresh(name) || scalar {
                    Base::Unique
                } else {
                    Base::Shared
                }
            }
        }
    }

    /// Applies a call's effect on its arguments. An argument the callee
    /// keeps is handed on; one it only reads leaves its holder's state alone
    /// but absorbs whatever the call stores into it.
    fn call_effects(
        &self,
        state: &mut State,
        args: &[Operand],
        kept_no_handle: impl Fn(usize) -> bool,
        live_after: &LocalSet,
    ) {
        let mut stored = Base::Unique;
        let mut receivers: Vec<Local> = Vec::new();
        for (i, op) in args.iter().enumerate() {
            let Operand::Copy(place) = op else {
                continue;
            };
            if !self.carries_handle(place) {
                continue;
            }
            if kept_no_handle(i) {
                receivers.push(place.local);
                continue;
            }
            stored = stored.max(self.hand_on(state, op, live_after));
        }
        for receiver in receivers {
            if self.is_reference(receiver) {
                for root in state.roots_of(receiver) {
                    state.widen(root, stored);
                }
            }
            state.widen(receiver, stored);
        }
    }

    /// The successor states `block`'s terminator produces from `state`.
    fn terminator(
        &self,
        mut state: State,
        block: &BasicBlock,
        live_after: &LocalSet,
    ) -> Vec<(BlockId, State)> {
        match &block.terminator {
            Terminator::Call {
                callee,
                args,
                destination,
                target,
            } => {
                let summary = self.summaries.get(callee);
                let answer = match (summary, callee) {
                    (Some(summary), _) => {
                        self.call_effects(
                            &mut state,
                            args,
                            |i| !summary.retains.get(i).copied().unwrap_or(true),
                            live_after,
                        );
                        if summary.returns_unique {
                            Base::Unique
                        } else {
                            Base::Shared
                        }
                    }
                    (None, Operand::Const(ConstValue::Str(name))) => {
                        self.call_effects(
                            &mut state,
                            args,
                            |i| runtime_arg_kept_no_handle(name, i),
                            live_after,
                        );
                        if runtime_answers_fresh(name) {
                            Base::Unique
                        } else {
                            Base::Shared
                        }
                    }
                    (None, _) => {
                        if let Operand::Copy(p) = callee {
                            state.share(p.local);
                        }
                        self.call_effects(&mut state, args, |_| false, live_after);
                        Base::Shared
                    }
                };
                let answer = if place_ty(self.tcx, self.body, destination)
                    .is_some_and(|ty| !tracks(self.tcx, ty))
                {
                    Base::Unique
                } else {
                    answer
                };
                if destination.projection.is_empty() && self.is_reference(destination.local) {
                    state.forget_reference(destination.local);
                    for op in args {
                        if let Operand::Copy(p) = op
                            && self.is_reference(p.local)
                        {
                            for root in state.roots_of(p.local) {
                                state.add_borrower(root, destination.local);
                                state.set(root, Base::Shared);
                            }
                        }
                    }
                }
                self.write(&mut state, destination, answer);
                target.map(|t| vec![(t, state)]).unwrap_or_default()
            }
            other => successors(other)
                .into_iter()
                .map(|b| (b, state.clone()))
                .collect(),
        }
    }
}

fn successors(terminator: &Terminator) -> Vec<BlockId> {
    match terminator {
        Terminator::Goto { target }
        | Terminator::Assert { target, .. }
        | Terminator::Drop { target, .. } => vec![*target],
        Terminator::SwitchInt { arms, default, .. } => {
            let mut out: Vec<BlockId> = arms.iter().map(|(_, b)| *b).collect();
            out.push(*default);
            out
        }
        Terminator::Call { target, .. } => target.iter().copied().collect(),
        _ => Vec::new(),
    }
}

/// Whether a local of `ty` can hold a handle this analysis tracks.
fn tracks(tcx: &TyCtxt, ty: Ty) -> bool {
    !matches!(
        tcx.kind_of(ty),
        TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::Unit
            | TyKind::Never
            | TyKind::Duration
            | TyKind::Instant
            | TyKind::Simd { .. }
            | TyKind::FnDef { .. }
            | TyKind::FnPtr(_)
    )
}

/// The type of the value `place` names, or `None` when a step is not
/// statically resolvable here.
pub(crate) fn place_ty(tcx: &TyCtxt, body: &Body, place: &Place) -> Option<Ty> {
    let mut ty = body.locals.get(place.local.0 as usize)?.ty;
    for step in &place.projection {
        let mut base = ty;
        if !matches!(step, Projection::Deref) {
            while let TyKind::Ref { inner, .. } = tcx.kind_of(base) {
                base = *inner;
            }
        }
        ty = match (step, tcx.kind_of(base)) {
            (Projection::Deref, TyKind::Ref { inner, .. }) => *inner,
            (Projection::Field(i), TyKind::Adt { def, substs }) => {
                *tcx.adt_field_tys(*def, substs)?.get(*i as usize)?
            }
            (Projection::Field(i), TyKind::Tuple(elems)) => *elems.get(*i as usize)?,
            (
                Projection::Index(_),
                TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. },
            ) => *elem,
            _ => return None,
        };
    }
    Some(ty)
}

fn place_mentions(place: &Place, local: Local) -> bool {
    place.local == local
        || place
            .projection
            .iter()
            .any(|step| matches!(step, Projection::Index(i) if *i == local))
}

/// The RC bookkeeping intrinsics: they account for a holder made by some
/// other statement, or end one, and make or end no observer themselves.
pub(crate) fn is_share_neutral_intrinsic(name: &str) -> bool {
    is_release_intrinsic(name)
        || matches!(
            name,
            "gos_rt_rc_retain"
                | "gos_rt_vec_retain"
                | "gos_rt_str_retain_typed"
                | "gos_rt_str_retain"
                | "gos_rt_map_retain"
                | "gos_rt_aggr_retain_children"
                | "gos_rt_option_slot_retain"
                | "gos_rt_result_payload_retain"
        )
}

/// The intrinsics that give back a share of their argument and read nothing
/// else. Each null-checks what it is handed.
pub(crate) fn is_release_intrinsic(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_rc_release"
            | "gos_rt_vec_free"
            | "gos_rt_str_free"
            | "gos_rt_str_free_typed"
            | "gos_rt_map_free"
            | "gos_rt_set_free"
            | "gos_rt_deque_free"
            | "gos_rt_map_field_release"
            | "gos_rt_aggr_release_children"
            | "gos_rt_option_slot_release"
            | "gos_rt_result_payload_release"
            | "gos_rt_result_ok_payload_release"
    )
}

/// The intrinsics that publish their argument to other goroutines.
fn is_goroutine_publish(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_rc_mark_shared"
            | "gos_rt_aggr_mark_shared_children"
            | "gos_rt_vec_mark_shared"
            | "gos_rt_map_mark_shared"
            | "gos_rt_set_mark_shared"
    )
}

/// The runtime helpers whose answer is a value no argument or global holds.
fn runtime_answers_fresh(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_vec_with_capacity"
            | "Vec::new"
            | "Vec::with_capacity"
            | "gos_rt_vec_new"
            | "gos_rt_vec_new_typed"
            | "gos_rt_vec_with_capacity_typed"
            | "gos_rt_vec_from_arr"
            | "gos_rt_vec_from_packed_arr"
            | "gos_rt_vec_repeat_primitive"
            | "gos_rt_vec_clone"
            | "gos_rt_map_clone"
            | "gos_rt_set_clone"
            | "gos_rt_deque_clone"
            | "gos_rt_queue_clone"
            | "gos_rt_stack_clone"
            | "gos_rt_par_run"
    ) || gossamer_abi::lookup(name).is_some_and(|entry| {
        // A sequence combinator builds the collection it answers, so nothing
        // else holds it.
        entry.mints_string
            || (entry.combinator.is_some() && entry.sig.ret == gossamer_abi::AbiType::Ptr)
    })
}

/// Whether the runtime helper `name` neither keeps nor hands out a handle to
/// its argument at `index`: it reads or changes what the argument reaches in
/// place and answers nothing that aliases it.
pub(crate) fn runtime_arg_kept_no_handle(name: &str, index: usize) -> bool {
    let receiver_only = matches!(
        name,
        "gos_rt_vec_len"
            | "gos_rt_len"
            | "gos_rt_len_is_zero"
            | "gos_rt_vec_capacity"
            | "gos_rt_vec_is_empty"
            | "gos_rt_vec_push"
            | "gos_rt_vec_push_i64"
            | "gos_rt_vec_pop"
            | "gos_rt_vec_reserve"
            | "gos_rt_vec_get_i64"
            | "gos_rt_vec_get_i64_unchecked"
            | "gos_rt_vec_get_f64"
            | "gos_rt_vec_set_i64"
            | "gos_rt_vec_set_i64_unchecked"
            | "gos_rt_vec_swap_safe"
            | "gos_rt_vec_swap_unchecked"
            | "gos_rt_vec_clone"
            | "gos_rt_map_clone"
            | "gos_rt_set_clone"
            | "gos_rt_deque_clone"
            | "gos_rt_queue_clone"
            | "gos_rt_stack_clone"
            | "gos_rt_str_len"
            | "gos_rt_str_byte_len"
            | "gos_rt_str_char_at"
            | "gos_rt_str_byte_at"
            | "gos_rt_str_is_empty"
            | "gos_rt_map_get_or_i64_i64"
            | "gos_rt_map_get_or_str_i64"
            | "gos_rt_map_get_or_typed_str_i64"
            | "gos_rt_map_inc_at_str_i64"
            | "gos_rt_map_inc_i64"
            | "gos_rt_map_inc_str_i64"
            | "gos_rt_map_inc_typed_str_i64"
            | "gos_rt_map_or_insert_i64_i64"
            | "gos_rt_map_or_insert_str_i64"
            | "gos_rt_map_or_insert_typed_str_i64"
    );
    (index == 0 && receiver_only)
        || (index == 1
            && matches!(
                name,
                "gos_rt_map_get_or_str_i64"
                    | "gos_rt_map_get_or_typed_str_i64"
                    | "gos_rt_map_inc_str_i64"
                    | "gos_rt_map_inc_typed_str_i64"
                    | "gos_rt_map_inc_at_str_i64"
                    | "gos_rt_map_or_insert_str_i64"
                    | "gos_rt_map_or_insert_typed_str_i64"
            ))
}

/// A set of locals.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalSet(Vec<u64>);

impl LocalSet {
    fn new(n: usize) -> Self {
        Self(vec![0; n.div_ceil(64)])
    }

    fn contains(&self, local: Local) -> bool {
        let i = local.0 as usize;
        self.0.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }

    fn insert(&mut self, local: Local) {
        let i = local.0 as usize;
        if let Some(w) = self.0.get_mut(i / 64) {
            *w |= 1 << (i % 64);
        }
    }

    fn remove(&mut self, local: Local) {
        let i = local.0 as usize;
        if let Some(w) = self.0.get_mut(i / 64) {
            *w &= !(1 << (i % 64));
        }
    }

    fn union(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (mine, theirs) in self.0.iter_mut().zip(&other.0) {
            let next = *mine | theirs;
            changed |= next != *mine;
            *mine = next;
        }
        changed
    }
}

/// Which locals are read later, counting a local's own releases as no read.
struct Liveness {
    n_locals: usize,
    live_in: Vec<LocalSet>,
    live_out: Vec<LocalSet>,
}

impl Liveness {
    fn compute(body: &Body) -> Self {
        let n_locals = body.locals.len();
        let n_blocks = body.blocks.len();
        let mut live_in = vec![LocalSet::new(n_locals); n_blocks];
        let mut live_out = vec![LocalSet::new(n_locals); n_blocks];
        let preds = predecessors(body);
        let mut work: Vec<usize> = (0..n_blocks).collect();
        let mut queued = vec![true; n_blocks];
        while let Some(bi) = work.pop() {
            queued[bi] = false;
            let mut out = LocalSet::new(n_locals);
            for succ in successors(&body.blocks[bi].terminator) {
                if let Some(set) = live_in.get(succ.0 as usize) {
                    out.union(set);
                }
            }
            let mut live = out.clone();
            let block = &body.blocks[bi];
            terminator_liveness(&block.terminator, &mut live);
            for stmt in block.stmts.iter().rev() {
                statement_liveness(stmt, &mut live);
            }
            live_out[bi] = out;
            if live != live_in[bi] {
                live_in[bi] = live;
                for &p in &preds[bi] {
                    if !queued[p] {
                        queued[p] = true;
                        work.push(p);
                    }
                }
            }
        }
        Self {
            n_locals,
            live_in,
            live_out,
        }
    }

    /// For each statement index of block `bi` (and the terminator, at the
    /// statement count), the locals live just after it.
    fn live_after_each(&self, body: &Body, bi: usize) -> Vec<LocalSet> {
        let block = &body.blocks[bi];
        let mut out = vec![LocalSet::new(self.n_locals); block.stmts.len() + 1];
        let mut live = self.live_out[bi].clone();
        out[block.stmts.len()] = live.clone();
        terminator_liveness(&block.terminator, &mut live);
        for (si, stmt) in block.stmts.iter().enumerate().rev() {
            out[si] = live.clone();
            statement_liveness(stmt, &mut live);
        }
        out
    }

    fn live_after(&self, body: &Body, point: Point) -> LocalSet {
        self.live_after_each(body, point.block.0 as usize)
            .swap_remove(point.stmt)
    }

    fn live_before(&self, body: &Body, point: Point) -> LocalSet {
        let bi = point.block.0 as usize;
        let block = &body.blocks[bi];
        if point.stmt >= block.stmts.len() {
            let mut live = self.live_out[bi].clone();
            terminator_liveness(&block.terminator, &mut live);
            return live;
        }
        let mut live = self.live_after(body, point);
        statement_liveness(&block.stmts[point.stmt], &mut live);
        live
    }
}

fn predecessors(body: &Body) -> Vec<Vec<usize>> {
    let mut preds = vec![Vec::new(); body.blocks.len()];
    for (bi, block) in body.blocks.iter().enumerate() {
        for succ in successors(&block.terminator) {
            if let Some(list) = preds.get_mut(succ.0 as usize) {
                list.push(bi);
            }
        }
    }
    preds
}

fn gen_place(place: &Place, live: &mut LocalSet) {
    live.insert(place.local);
    for step in &place.projection {
        if let Projection::Index(i) = step {
            live.insert(*i);
        }
    }
}

fn gen_operand(op: &Operand, live: &mut LocalSet) {
    if let Operand::Copy(place) = op {
        gen_place(place, live);
    }
}

/// The local a release names, when every argument is rooted at it.
fn released_local(rvalue: &Rvalue) -> Option<Local> {
    let Rvalue::CallIntrinsic { name, args } = rvalue else {
        return None;
    };
    if !is_release_intrinsic(name) {
        return None;
    }
    let mut root = None;
    for op in args {
        match op {
            Operand::Copy(p) => {
                if p.projection
                    .iter()
                    .any(|s| !matches!(s, Projection::Field(_)))
                {
                    return None;
                }
                match root {
                    None => root = Some(p.local),
                    Some(r) if r == p.local => {}
                    Some(_) => return None,
                }
            }
            Operand::Const(_) => {}
            Operand::FnRef { .. } => return None,
        }
    }
    root
}

fn statement_liveness(stmt: &Statement, live: &mut LocalSet) {
    match &stmt.kind {
        StatementKind::Assign { place, rvalue } => {
            if place.projection.is_empty() {
                live.remove(place.local);
            } else {
                gen_place(place, live);
            }
            if released_local(rvalue).is_some() {
                return;
            }
            rvalue_liveness(rvalue, live);
        }
        StatementKind::StaticStore { value, .. } => gen_operand(value, live),
        StatementKind::SetDiscriminant { place, .. } => gen_place(place, live),
        StatementKind::IterSource { dst, source, .. } => {
            gen_place(dst, live);
            gen_operand(source, live);
        }
        StatementKind::IterAdapter {
            dst,
            upstream,
            closure_or_arg,
            ..
        } => {
            gen_place(dst, live);
            gen_place(upstream, live);
            if let Some(op) = closure_or_arg {
                gen_operand(op, live);
            }
        }
        _ => {}
    }
}

fn rvalue_liveness(rvalue: &Rvalue, live: &mut LocalSet) {
    match rvalue {
        Rvalue::Use(op)
        | Rvalue::Cast { operand: op, .. }
        | Rvalue::UnaryOp { operand: op, .. } => {
            gen_operand(op, live);
        }
        Rvalue::Repeat { value, .. } => gen_operand(value, live),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            gen_operand(lhs, live);
            gen_operand(rhs, live);
        }
        Rvalue::Aggregate { operands: ops, .. } | Rvalue::CallIntrinsic { args: ops, .. } => {
            for op in ops {
                gen_operand(op, live);
            }
        }
        Rvalue::Len(place) | Rvalue::Ref { place, .. } => gen_place(place, live),
        Rvalue::StaticLoad(_) => {}
    }
}

fn terminator_liveness(terminator: &Terminator, live: &mut LocalSet) {
    match terminator {
        Terminator::Call {
            callee,
            args,
            destination,
            ..
        } => {
            if destination.projection.is_empty() {
                live.remove(destination.local);
            } else {
                gen_place(destination, live);
            }
            gen_operand(callee, live);
            for op in args {
                gen_operand(op, live);
            }
        }
        Terminator::SwitchInt { discriminant, .. } => gen_operand(discriminant, live),
        Terminator::Assert { cond, .. } => gen_operand(cond, live),
        Terminator::Drop { place, .. } => gen_place(place, live),
        Terminator::Return => live.insert(Local::RETURN),
        _ => {}
    }
}
