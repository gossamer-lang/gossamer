//! Landing pads that run pending `defer`s when a fault unwinds a frame.
//!
//! The deferred expressions pending at a point depend only on where the
//! point sits in the source, so a body that registers any keeps a region
//! local, set at every point the pending set changes. Each region's pending
//! expressions are lowered there, while the locals they name are in scope,
//! into blocks reached only from the body's landing pad, which dispatches on
//! the region local. A probe at entry gives the pad an edge every pass sees;
//! the native backends drop that edge and reach the pad from each call.
//!
//! A fault raised by a deferred expression while the frame unwinds reaches a
//! second pad, which records it as a note and continues with the next
//! expression; the region local names where.

use std::collections::HashSet;

use gossamer_hir::HirExpr;
use gossamer_lex::Span;

use super::Builder;
use crate::ir::{
    AssertMessage, BlockId, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, Statement,
    StatementKind, Terminator,
};

/// Debug name of the local a probe reads to give the landing pads an edge.
pub const UNWIND_PROBE_NAME: &str = "__gos_unwind_probe";
/// Debug name of the local naming the pending deferred expressions.
pub const UNWIND_REGION_NAME: &str = "__gos_unwind_region";
/// The probe's arm to the pad that starts unwinding a frame.
pub const CLEANUP_PAD_ARM: i128 = 1;
/// The probe's arm to the pad that records a deferred expression's fault.
pub const NOTE_PAD_ARM: i128 = 2;

/// A body's landing pads and the regions they dispatch to.
#[derive(Debug)]
pub(crate) struct UnwindState {
    region: Local,
    cleanup_pad: BlockId,
    note_pad: BlockId,
    resume: BlockId,
    cleanup_arms: Vec<(i128, BlockId)>,
    note_arms: Vec<(i128, BlockId)>,
    next_id: i128,
}

/// Whether `block` registers a `defer`, directly or in a nested block of
/// the same body.
fn registers_defer(block: &gossamer_hir::HirBlock) -> bool {
    let mut found = block
        .stmts
        .iter()
        .any(|stmt| matches!(stmt.kind, gossamer_hir::HirStmtKind::Defer(_)));
    if !found {
        gossamer_hir::for_each_child_expr_in_block(block, &mut |expr| {
            found |= expr_registers_defer(expr);
        });
    }
    found
}

fn expr_registers_defer(expr: &HirExpr) -> bool {
    if let gossamer_hir::HirExprKind::Block(block) = &expr.kind {
        return registers_defer(block);
    }
    let mut found = false;
    gossamer_hir::for_each_child_expr(expr, &mut |child| {
        found |= expr_registers_defer(child);
    });
    found
}

impl Builder<'_> {
    /// Gives the body being lowered its landing pads when `block`, its
    /// body, registers a deferred expression. Called with the entry block
    /// current; lowering continues in a fresh block.
    pub(crate) fn begin_unwind(&mut self, block: &gossamer_hir::HirBlock, span: Span) {
        if !registers_defer(block) {
            return;
        }
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let region = self.push_local(
            i64_ty,
            Some(gossamer_ast::Ident::new(UNWIND_REGION_NAME)),
            true,
        );
        let probe = self.push_local(
            i64_ty,
            Some(gossamer_ast::Ident::new(UNWIND_PROBE_NAME)),
            false,
        );
        self.emit_assign(
            Place::local(region),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        self.emit_assign(
            Place::local(probe),
            Rvalue::CallIntrinsic {
                name: "gos_unwind_probe",
                args: Vec::new(),
            },
            span,
        );
        let cleanup_pad = self.new_block(span);
        let note_pad = self.new_block(span);
        let resume = self.new_block(span);
        let body = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(probe)),
            arms: vec![(CLEANUP_PAD_ARM, cleanup_pad), (NOTE_PAD_ARM, note_pad)],
            default: body,
        });
        self.blocks[resume.0 as usize].terminator = Terminator::Resume;
        self.unwind = Some(UnwindState {
            region,
            cleanup_pad,
            note_pad,
            resume,
            cleanup_arms: Vec::new(),
            note_arms: Vec::new(),
            next_id: 1,
        });
        self.set_current(body);
    }

    /// Writes the landing pads' dispatch once every region is known.
    pub(crate) fn finish_unwind(&mut self) {
        let Some(state) = self.unwind.take() else {
            return;
        };
        self.blocks[state.cleanup_pad.0 as usize].terminator = Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(state.region)),
            arms: state.cleanup_arms,
            default: state.resume,
        };
        self.blocks[state.note_pad.0 as usize].terminator = Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(state.region)),
            arms: state.note_arms,
            default: state.resume,
        };
    }

    /// The deferred expressions a fault here runs, in the order it runs them:
    /// innermost block first, each block's last registered first.
    fn pending_defers(&self) -> Vec<HirExpr> {
        let mut pending = Vec::new();
        for (frame_idx, frame) in self.defer_stack.iter().enumerate().rev() {
            for (expr_idx, expr) in frame.iter().enumerate().rev() {
                if !self.running_defers.contains(&(frame_idx, expr_idx)) {
                    pending.push(expr.clone());
                }
            }
        }
        pending
    }

    /// Starts the region whose pending set is the one about to take effect,
    /// lowering its landing-pad code. Called after the change is applied.
    fn enter_unwind_region(&mut self, span: Span) {
        if self.in_unwind_pad > 0 || self.current.is_none() {
            return;
        }
        let Some(region) = self.unwind.as_ref().map(|state| state.region) else {
            return;
        };
        let pending = self.pending_defers();
        let id = if pending.is_empty() {
            0
        } else {
            let id = self.unwind.as_mut().map_or(0, |state| {
                let id = state.next_id;
                state.next_id += 1;
                id
            });
            self.lower_unwind_pad(id, &pending, span);
            id
        };
        self.emit_assign(
            Place::local(region),
            Rvalue::Use(Operand::Const(ConstValue::Int(id))),
            span,
        );
    }

    /// Lowers region `id`'s landing-pad code: each pending expression in
    /// turn, a fault inside one continuing with the next, then `Resume`.
    fn lower_unwind_pad(&mut self, id: i128, pending: &[HirExpr], span: Span) {
        let Some(region) = self.unwind.as_ref().map(|state| state.region) else {
            return;
        };
        let resume_here = self.current;
        let pad = self.new_block(span);
        self.set_current(pad);
        self.in_unwind_pad += 1;
        for expr in pending {
            let note = self.unwind.as_mut().map_or(0, |state| {
                let note = state.next_id;
                state.next_id += 1;
                note
            });
            if self.current.is_some() {
                self.emit_assign(
                    Place::local(region),
                    Rvalue::Use(Operand::Const(ConstValue::Int(note))),
                    span,
                );
                let _ = self.lower_expr(expr);
            }
            let next = self.new_block(span);
            if self.current.is_some() {
                self.terminate(Terminator::Goto { target: next });
            }
            if let Some(state) = self.unwind.as_mut() {
                state.note_arms.push((note, next));
            }
            self.set_current(next);
        }
        self.terminate(Terminator::Resume);
        self.in_unwind_pad -= 1;
        if let Some(state) = self.unwind.as_mut() {
            state.cleanup_arms.push((id, pad));
        }
        self.current = resume_here;
    }

    /// Registers `expr` as deferred in the innermost block.
    pub(crate) fn push_defer(&mut self, expr: HirExpr, span: Span) {
        if let Some(frame) = self.defer_stack.last_mut() {
            frame.push(expr);
        }
        self.enter_unwind_region(span);
    }

    /// Marks the `expr_idx`th defer of frame `frame_idx` as running on the
    /// exit edge being lowered, so a fault inside it does not run it again.
    pub(crate) fn mark_defer_running(&mut self, frame_idx: usize, expr_idx: usize, span: Span) {
        if self.in_unwind_pad > 0 {
            return;
        }
        self.running_defers.push((frame_idx, expr_idx));
        self.enter_unwind_region(span);
    }

    /// Ends an exit edge. The region it leaves set, without the defers it
    /// ran, is the one pending where the edge lands: a `break` or `continue`
    /// runs every frame inside its loop, and the code at its target sits
    /// outside them. Code lowered after the edge is reached by other paths,
    /// which keep their own region, so the defers are pending again there.
    pub(crate) fn finish_defer_edge(&mut self) {
        if self.in_unwind_pad > 0 {
            return;
        }
        self.running_defers.clear();
    }

    /// Pops the innermost block's defer frame, ending the region its defers
    /// were pending in.
    pub(crate) fn pop_defer_frame(&mut self, span: Span) -> Vec<HirExpr> {
        let frame = self.defer_stack.pop().unwrap_or_default();
        if !frame.is_empty() {
            self.enter_unwind_region(span);
        }
        frame
    }
}

/// Where a lowered body's landing pads are, for a backend reaching them from
/// each call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnwindPads {
    /// The local the entry probe switches on.
    pub probe: Local,
    /// The block a fault in the body's own code lands in.
    pub cleanup_pad: BlockId,
    /// The block a fault in a pad's code lands in.
    pub note_pad: BlockId,
    /// The blocks a pad's code runs in, the pads included.
    pub pad_code: HashSet<BlockId>,
}

impl UnwindPads {
    /// The pads of `body`, when it has them.
    #[must_use]
    pub fn of(body: &Body) -> Option<Self> {
        let probe = body.locals.iter().position(|decl| {
            decl.debug_name
                .as_ref()
                .is_some_and(|name| name.name == UNWIND_PROBE_NAME)
        })?;
        let probe = Local(u32::try_from(probe).ok()?);
        let (cleanup_pad, note_pad) =
            body.blocks
                .iter()
                .find_map(|block| match &block.terminator {
                    Terminator::SwitchInt {
                        discriminant: Operand::Copy(place),
                        arms,
                        ..
                    } if place.local == probe && place.projection.is_empty() => {
                        let arm = |value: i128| {
                            arms.iter()
                                .find(|(arm, _)| *arm == value)
                                .map(|(_, block)| *block)
                        };
                        Some((arm(CLEANUP_PAD_ARM)?, arm(NOTE_PAD_ARM)?))
                    }
                    _ => None,
                })?;
        let mut pad_code = HashSet::new();
        let mut pending = vec![cleanup_pad, note_pad];
        while let Some(block) = pending.pop() {
            if !pad_code.insert(block) {
                continue;
            }
            if let Some(found) = body.blocks.get(block.as_u32() as usize) {
                pending.extend(successors(&found.terminator));
            }
        }
        Some(Self {
            probe,
            cleanup_pad,
            note_pad,
            pad_code,
        })
    }

    /// Every local a pad's code names, which the pad reads as the body's
    /// own code last left it.
    #[must_use]
    pub fn pad_locals(&self, body: &Body) -> HashSet<Local> {
        let mut locals = HashSet::new();
        for block in body.blocks.iter().filter(|b| self.pad_code.contains(&b.id)) {
            for statement in &block.stmts {
                collect_statement_locals(statement, &mut locals);
            }
            terminator_locals(&block.terminator, &mut locals);
        }
        locals
    }
}

/// The blocks `terminator` may continue in.
fn successors(terminator: &Terminator) -> Vec<BlockId> {
    match terminator {
        Terminator::Goto { target } | Terminator::Drop { target, .. } => vec![*target],
        Terminator::SwitchInt { arms, default, .. } => arms
            .iter()
            .map(|(_, block)| *block)
            .chain(std::iter::once(*default))
            .collect(),
        Terminator::Call { target, .. } => target.iter().copied().collect(),
        Terminator::Assert { target, .. } => vec![*target],
        Terminator::Return
        | Terminator::Resume
        | Terminator::Unreachable
        | Terminator::Panic { .. } => Vec::new(),
    }
}

fn place_locals(place: &Place, out: &mut HashSet<Local>) {
    out.insert(place.local);
    for projection in &place.projection {
        match projection {
            Projection::Index(index) => {
                out.insert(*index);
            }
            Projection::Deref
            | Projection::Field(_)
            | Projection::Downcast(_)
            | Projection::Discriminant => {}
        }
    }
}

fn operand_locals(operand: &Operand, out: &mut HashSet<Local>) {
    match operand {
        Operand::Copy(place) => place_locals(place, out),
        Operand::Const(_) | Operand::FnRef { .. } => {}
    }
}

fn rvalue_locals(rvalue: &Rvalue, out: &mut HashSet<Local>) {
    match rvalue {
        Rvalue::Use(operand)
        | Rvalue::UnaryOp { operand, .. }
        | Rvalue::Cast { operand, .. }
        | Rvalue::Repeat { value: operand, .. } => operand_locals(operand, out),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            operand_locals(lhs, out);
            operand_locals(rhs, out);
        }
        Rvalue::Aggregate { operands, .. } | Rvalue::CallIntrinsic { args: operands, .. } => {
            for operand in operands {
                operand_locals(operand, out);
            }
        }
        Rvalue::Len(place) | Rvalue::Ref { place, .. } => place_locals(place, out),
        Rvalue::StaticLoad(_) => {}
    }
}

/// Every local `statement` names.
#[must_use]
pub fn statement_locals(statement: &Statement) -> HashSet<Local> {
    let mut out = HashSet::new();
    collect_statement_locals(statement, &mut out);
    out
}

fn collect_statement_locals(statement: &Statement, out: &mut HashSet<Local>) {
    match &statement.kind {
        StatementKind::Assign { place, rvalue } => {
            place_locals(place, out);
            rvalue_locals(rvalue, out);
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            out.insert(*local);
        }
        StatementKind::SetDiscriminant { place, .. } => place_locals(place, out),
        StatementKind::StaticStore { value, .. } => operand_locals(value, out),
        StatementKind::IterSource { dst, source, .. } => {
            place_locals(dst, out);
            operand_locals(source, out);
        }
        StatementKind::IterAdapter {
            dst,
            upstream,
            closure_or_arg,
            ..
        } => {
            place_locals(dst, out);
            place_locals(upstream, out);
            if let Some(operand) = closure_or_arg {
                operand_locals(operand, out);
            }
        }
        StatementKind::IterNext {
            dst_option,
            iter_place,
            ..
        } => {
            place_locals(dst_option, out);
            place_locals(iter_place, out);
        }
        StatementKind::Nop => {}
    }
}

fn terminator_locals(terminator: &Terminator, out: &mut HashSet<Local>) {
    match terminator {
        Terminator::SwitchInt { discriminant, .. } => operand_locals(discriminant, out),
        Terminator::Call {
            callee,
            args,
            destination,
            ..
        } => {
            operand_locals(callee, out);
            for operand in args {
                operand_locals(operand, out);
            }
            place_locals(destination, out);
        }
        Terminator::Assert { cond, msg, .. } => {
            operand_locals(cond, out);
            if let AssertMessage::BoundsCheck { index, seq } = msg {
                operand_locals(index, out);
                operand_locals(seq, out);
            }
        }
        Terminator::Drop { place, .. } => place_locals(place, out),
        Terminator::Goto { .. }
        | Terminator::Return
        | Terminator::Resume
        | Terminator::Unreachable
        | Terminator::Panic { .. } => {}
    }
}
