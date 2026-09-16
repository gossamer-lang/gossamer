//! The lane loops a `Simd` operation lowers to, emitted as 128-bit vector
//! instructions.
//!
//! Every lane-wise `Simd` operation reaches MIR as a counted loop over its
//! lanes: a header comparing a counter against the lane count, and a body
//! that reads each operand's lane, applies scalar operations, stores the
//! result lane, and steps the counter. Cranelift has no loop vectoriser, so
//! a loop of exactly that shape is lowered here as two-lane vector operations
//! over consecutive lane pairs instead. Each operation is lane-independent
//! and the vector instruction computes the same IEEE or wrapping result per
//! lane, so the bits match the scalar loop on every tier.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use cranelift_codegen::ir::{self, InstBuilder, MemFlagsData, immediates::Ieee64, types};
use cranelift_frontend::{FunctionBuilder, Variable};
use cranelift_module::Module;
use gossamer_mir::{
    BinOp, BlockId, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, StatementKind,
    Terminator,
};
use gossamer_types::{ArrayLen, FloatTy, IntTy, TyCtxt, TyKind};

use super::{IntrinsicContext, define_var_to, lower_place_address};

/// The lane element a recognised loop computes over.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LaneElem {
    F64,
    I64,
}

impl LaneElem {
    fn of(tcx: &TyCtxt, ty: gossamer_types::Ty) -> Option<Self> {
        match tcx.kind_of(ty) {
            TyKind::Float(FloatTy::F64) => Some(Self::F64),
            TyKind::Int(IntTy::I64) => Some(Self::I64),
            _ => None,
        }
    }

    /// The two-lane vector type over this element.
    const fn pair(self) -> ir::Type {
        match self {
            Self::F64 => types::F64X2,
            Self::I64 => types::I64X2,
        }
    }

    /// Whether the scalar loop's `op` has a lane-wise vector instruction with
    /// the same result.
    const fn vectorises(self, op: BinOp) -> bool {
        match self {
            Self::F64 => matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div),
            Self::I64 => matches!(
                op,
                BinOp::WrappingAdd
                    | BinOp::WrappingSub
                    | BinOp::WrappingMul
                    | BinOp::BitAnd
                    | BinOp::BitOr
                    | BinOp::BitXor
            ),
        }
    }
}

/// A lane operand: a value the body computed, or a constant.
#[derive(Clone, Copy)]
enum LaneValue {
    Temp(Local),
    /// The constant's bits, as the lane element spells them.
    Const(u64),
}

/// One statement of the loop body, in order.
enum LaneStep {
    /// `dst = array[counter]`
    Load { dst: Local, array: Local },
    /// `dst = lhs <op> rhs`
    Binary {
        dst: Local,
        op: BinOp,
        lhs: LaneValue,
        rhs: LaneValue,
    },
}

/// A lane loop recognised in a body.
pub(super) struct LaneLoop {
    elem: LaneElem,
    lanes: u32,
    counter: Local,
    exit: BlockId,
    body: BlockId,
    steps: Vec<LaneStep>,
    out: Local,
    stored: Local,
}

/// Every lane loop of a body, by header block.
#[derive(Default)]
pub(super) struct LaneLoops {
    by_header: HashMap<u32, LaneLoop>,
    bodies: HashSet<u32>,
}

impl LaneLoops {
    /// The loop whose header is `block`.
    pub(super) fn headed_by(&self, block: BlockId) -> Option<&LaneLoop> {
        self.by_header.get(&block.as_u32())
    }

    /// Whether `block` is the body of a loop the vector form replaces, and so
    /// is never entered.
    pub(super) fn replaces(&self, block: BlockId) -> bool {
        self.bodies.contains(&block.as_u32())
    }

    /// Drops the loop headed by `header`, which then lowers as written.
    pub(super) fn keep_scalar(&mut self, header: BlockId) {
        if let Some(lane_loop) = self.by_header.remove(&header.as_u32()) {
            self.bodies.remove(&lane_loop.body.as_u32());
        }
    }

    /// Every recognised loop's header and body.
    pub(super) fn blocks(&self) -> Vec<(BlockId, BlockId)> {
        self.by_header
            .iter()
            .map(|(header, lane_loop)| (BlockId(*header), lane_loop.body))
            .collect()
    }
}

/// Finds the lane loops of `body`.
pub(super) fn find_lane_loops(body: &Body, tcx: &TyCtxt) -> LaneLoops {
    let mut predecessors: HashMap<u32, Vec<u32>> = HashMap::new();
    for block in &body.blocks {
        for succ in successors(&block.terminator) {
            predecessors
                .entry(succ.as_u32())
                .or_default()
                .push(block.id.as_u32());
        }
    }
    let mut loops = LaneLoops::default();
    for block in &body.blocks {
        if let Some(lane_loop) = recognise(body, tcx, block.id, &predecessors) {
            loops.bodies.insert(lane_loop.body.as_u32());
            loops.by_header.insert(block.id.as_u32(), lane_loop);
        }
    }
    loops
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

fn bare(operand: &Operand) -> Option<Local> {
    match operand {
        Operand::Copy(place) if place.projection.is_empty() => Some(place.local),
        _ => None,
    }
}

/// The local a place indexes by `counter`, when that is all the place is.
fn lane_of(place: &Place, counter: Local) -> Option<Local> {
    match place.projection.as_slice() {
        [Projection::Index(index)] if *index == counter => Some(place.local),
        _ => None,
    }
}

fn recognise(
    body: &Body,
    tcx: &TyCtxt,
    header_id: BlockId,
    predecessors: &HashMap<u32, Vec<u32>>,
) -> Option<LaneLoop> {
    let header = body.blocks.get(header_id.as_u32() as usize)?;
    let [test] = header.stmts.as_slice() else {
        return None;
    };
    let StatementKind::Assign {
        place: cond_place,
        rvalue:
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs,
                rhs: Operand::Const(ConstValue::Int(count)),
            },
    } = &test.kind
    else {
        return None;
    };
    let counter = bare(lhs)?;
    let lanes = u32::try_from(*count).ok()?;
    if !matches!(lanes, 2 | 4 | 8 | 16) || !cond_place.projection.is_empty() {
        return None;
    }
    let Terminator::SwitchInt {
        discriminant,
        arms,
        default: body_id,
    } = &header.terminator
    else {
        return None;
    };
    let [(0, exit)] = arms.as_slice() else {
        return None;
    };
    if bare(discriminant)? != cond_place.local || *body_id == header_id || *exit == header_id {
        return None;
    }
    let loop_body = body.blocks.get(body_id.as_u32() as usize)?;
    if !matches!(loop_body.terminator, Terminator::Goto { target } if target == header_id) {
        return None;
    }
    if predecessors.get(&body_id.as_u32())? != &vec![header_id.as_u32()] {
        return None;
    }
    // The loop is entered once, from a block that starts the counter at zero.
    let header_preds = predecessors.get(&header_id.as_u32())?;
    let [first, second] = header_preds.as_slice() else {
        return None;
    };
    let entry = if *first == body_id.as_u32() {
        *second
    } else if *second == body_id.as_u32() {
        *first
    } else {
        return None;
    };
    let entry_block = body.blocks.get(entry as usize)?;
    if !matches!(entry_block.terminator, Terminator::Goto { target } if target == header_id) {
        return None;
    }
    let starts_at_zero = entry_block
        .stmts
        .iter()
        .rev()
        .find_map(|stmt| match &stmt.kind {
            StatementKind::Assign { place, rvalue } if place.local == counter => Some(
                place.projection.is_empty()
                    && matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0)))),
            ),
            _ => None,
        });
    if starts_at_zero != Some(true) {
        return None;
    }

    // The body ends by storing the result lane and stepping the counter.
    let [lane_stmts @ .., store, step, advance] = loop_body.stmts.as_slice() else {
        return None;
    };
    let StatementKind::Assign {
        place: step_place,
        rvalue:
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: step_lhs,
                rhs: Operand::Const(ConstValue::Int(1)),
            },
    } = &step.kind
    else {
        return None;
    };
    if !step_place.projection.is_empty() || bare(step_lhs)? != counter {
        return None;
    }
    let StatementKind::Assign {
        place: advance_place,
        rvalue: Rvalue::Use(advanced),
    } = &advance.kind
    else {
        return None;
    };
    if advance_place.local != counter
        || !advance_place.projection.is_empty()
        || bare(advanced)? != step_place.local
    {
        return None;
    }
    let StatementKind::Assign {
        place: store_place,
        rvalue: Rvalue::Use(stored_operand),
    } = &store.kind
    else {
        return None;
    };
    let out = lane_of(store_place, counter)?;
    let stored = bare(stored_operand)?;
    let (elem, len) = match tcx.kind_of(body.local_ty(out)) {
        TyKind::Array { elem, len } => (LaneElem::of(tcx, *elem)?, *len),
        _ => return None,
    };
    let lane_array = |local: Local| {
        matches!(
            tcx.kind_of(body.local_ty(local)),
            TyKind::Array { elem: e, len: l }
                if LaneElem::of(tcx, *e) == Some(elem) && *l == len
        )
    };
    if len != ArrayLen::Concrete(usize::try_from(lanes).ok()?) {
        return None;
    }

    let mut defined: HashSet<Local> = HashSet::new();
    let lane_value = |operand: &Operand, defined: &HashSet<Local>| match operand {
        Operand::Copy(place) if place.projection.is_empty() && defined.contains(&place.local) => {
            Some(LaneValue::Temp(place.local))
        }
        Operand::Const(ConstValue::Float(bits)) if elem == LaneElem::F64 => {
            Some(LaneValue::Const(*bits))
        }
        Operand::Const(ConstValue::Int(value)) if elem == LaneElem::I64 => Some(LaneValue::Const(
            i64::try_from(*value).ok()?.cast_unsigned(),
        )),
        _ => None,
    };
    let mut steps = Vec::with_capacity(lane_stmts.len());
    for stmt in lane_stmts {
        let StatementKind::Assign { place, rvalue } = &stmt.kind else {
            return None;
        };
        let dst = place.local;
        if !place.projection.is_empty()
            || dst == counter
            || defined.contains(&dst)
            || LaneElem::of(tcx, body.local_ty(dst)) != Some(elem)
        {
            return None;
        }
        let step = match rvalue {
            Rvalue::Use(Operand::Copy(source)) => {
                let array = lane_of(source, counter)?;
                if !lane_array(array) {
                    return None;
                }
                LaneStep::Load { dst, array }
            }
            Rvalue::BinaryOp { op, lhs, rhs } if elem.vectorises(*op) => LaneStep::Binary {
                dst,
                op: *op,
                lhs: lane_value(lhs, &defined)?,
                rhs: lane_value(rhs, &defined)?,
            },
            _ => return None,
        };
        steps.push(step);
        defined.insert(dst);
    }
    if !defined.contains(&stored) || !lane_array(out) {
        return None;
    }

    // The vector form computes no scalar temporaries, so none may be read
    // outside the loop.
    let mut private = defined;
    private.insert(cond_place.local);
    private.insert(step_place.local);
    let read_elsewhere = body
        .blocks
        .iter()
        .filter(|block| block.id != header_id && block.id != *body_id)
        .any(|block| {
            block
                .stmts
                .iter()
                .any(|stmt| statement_mentions(&stmt.kind, &private))
                || terminator_mentions(&block.terminator, &private)
        });
    if read_elsewhere {
        return None;
    }
    Some(LaneLoop {
        elem,
        lanes,
        counter,
        exit: *exit,
        body: *body_id,
        steps,
        out,
        stored,
    })
}

fn place_mentions(place: &Place, locals: &HashSet<Local>) -> bool {
    locals.contains(&place.local)
        || place.projection.iter().any(
            |projection| matches!(projection, Projection::Index(index) if locals.contains(index)),
        )
}

fn operand_mentions(operand: &Operand, locals: &HashSet<Local>) -> bool {
    match operand {
        Operand::Copy(place) => place_mentions(place, locals),
        Operand::Const(_) | Operand::FnRef { .. } => false,
    }
}

fn rvalue_mentions(rvalue: &Rvalue, locals: &HashSet<Local>) -> bool {
    match rvalue {
        Rvalue::Use(operand)
        | Rvalue::UnaryOp { operand, .. }
        | Rvalue::Cast { operand, .. }
        | Rvalue::Repeat { value: operand, .. } => operand_mentions(operand, locals),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            operand_mentions(lhs, locals) || operand_mentions(rhs, locals)
        }
        Rvalue::Aggregate { operands, .. } | Rvalue::CallIntrinsic { args: operands, .. } => {
            operands
                .iter()
                .any(|operand| operand_mentions(operand, locals))
        }
        Rvalue::Len(place) | Rvalue::Ref { place, .. } => place_mentions(place, locals),
        Rvalue::StaticLoad(_) => false,
    }
}

fn statement_mentions(kind: &StatementKind, locals: &HashSet<Local>) -> bool {
    match kind {
        StatementKind::Assign { place, rvalue } => {
            place_mentions(place, locals) || rvalue_mentions(rvalue, locals)
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            locals.contains(local)
        }
        StatementKind::SetDiscriminant { place, .. } => place_mentions(place, locals),
        StatementKind::StaticStore { value, .. } => operand_mentions(value, locals),
        StatementKind::IterSource { dst, source, .. } => {
            place_mentions(dst, locals) || operand_mentions(source, locals)
        }
        StatementKind::IterAdapter {
            dst,
            upstream,
            closure_or_arg,
            ..
        } => {
            place_mentions(dst, locals)
                || place_mentions(upstream, locals)
                || closure_or_arg
                    .as_ref()
                    .is_some_and(|operand| operand_mentions(operand, locals))
        }
        StatementKind::IterNext {
            dst_option,
            iter_place,
            ..
        } => place_mentions(dst_option, locals) || place_mentions(iter_place, locals),
        StatementKind::Nop => false,
    }
}

fn terminator_mentions(terminator: &Terminator, locals: &HashSet<Local>) -> bool {
    match terminator {
        Terminator::SwitchInt { discriminant, .. } => operand_mentions(discriminant, locals),
        Terminator::Call {
            callee,
            args,
            destination,
            ..
        } => {
            operand_mentions(callee, locals)
                || args.iter().any(|operand| operand_mentions(operand, locals))
                || place_mentions(destination, locals)
        }
        Terminator::Assert { cond, msg, .. } => {
            operand_mentions(cond, locals) || msg.operands().any(|op| operand_mentions(op, locals))
        }
        Terminator::Drop { place, .. } => place_mentions(place, locals),
        Terminator::Goto { .. }
        | Terminator::Return
        | Terminator::Unreachable
        | Terminator::Panic { .. } => false,
    }
}

/// Emits `lane_loop` in place of its header: each lane pair loaded, combined,
/// and stored as one vector, the counter left at the lane count, and a jump
/// to the loop's exit.
pub(super) fn emit_lane_loop(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    intrinsics: &mut IntrinsicContext,
    lane_loop: &LaneLoop,
    exit: ir::Block,
) -> Result<()> {
    let pair = lane_loop.elem.pair();
    let flags = MemFlagsData::trusted();
    let mut addresses: HashMap<Local, ir::Value> = HashMap::new();
    let arrays = lane_loop
        .steps
        .iter()
        .filter_map(|step| match step {
            LaneStep::Load { array, .. } => Some(*array),
            LaneStep::Binary { .. } => None,
        })
        .chain(std::iter::once(lane_loop.out));
    for array in arrays {
        if let std::collections::hash_map::Entry::Vacant(slot) = addresses.entry(array) {
            let address = lower_place_address(
                module,
                builder,
                locals,
                body,
                tcx,
                &Place::local(array),
                intrinsics,
            )?;
            slot.insert(address);
        }
    }
    for pair_index in 0..lane_loop.lanes / 2 {
        let offset = i32::try_from(pair_index * 16)?;
        let mut values: HashMap<Local, ir::Value> = HashMap::new();
        let operand = |builder: &mut FunctionBuilder<'_>,
                       values: &HashMap<Local, ir::Value>,
                       value: LaneValue| match value {
            LaneValue::Temp(local) => values[&local],
            LaneValue::Const(bits) => {
                let scalar = match lane_loop.elem {
                    LaneElem::F64 => builder.ins().f64const(Ieee64::with_bits(bits)),
                    LaneElem::I64 => builder.ins().iconst(types::I64, bits.cast_signed()),
                };
                builder.ins().splat(pair, scalar)
            }
        };
        for step in &lane_loop.steps {
            match step {
                LaneStep::Load { dst, array } => {
                    let loaded = builder.ins().load(pair, flags, addresses[array], offset);
                    values.insert(*dst, loaded);
                }
                LaneStep::Binary { dst, op, lhs, rhs } => {
                    let a = operand(builder, &values, *lhs);
                    let b = operand(builder, &values, *rhs);
                    let combined = match (lane_loop.elem, op) {
                        (LaneElem::F64, BinOp::Add) => builder.ins().fadd(a, b),
                        (LaneElem::F64, BinOp::Sub) => builder.ins().fsub(a, b),
                        (LaneElem::F64, BinOp::Mul) => builder.ins().fmul(a, b),
                        (LaneElem::F64, _) => builder.ins().fdiv(a, b),
                        (LaneElem::I64, BinOp::WrappingAdd) => builder.ins().iadd(a, b),
                        (LaneElem::I64, BinOp::WrappingSub) => builder.ins().isub(a, b),
                        (LaneElem::I64, BinOp::WrappingMul) => builder.ins().imul(a, b),
                        (LaneElem::I64, BinOp::BitAnd) => builder.ins().band(a, b),
                        (LaneElem::I64, BinOp::BitOr) => builder.ins().bor(a, b),
                        (LaneElem::I64, _) => builder.ins().bxor(a, b),
                    };
                    values.insert(*dst, combined);
                }
            }
        }
        builder.ins().store(
            flags,
            values[&lane_loop.stored],
            addresses[&lane_loop.out],
            offset,
        );
    }
    let done = builder.ins().iconst(types::I64, i64::from(lane_loop.lanes));
    define_var_to(
        builder,
        locals,
        &intrinsics.body_cl_types,
        lane_loop.counter,
        done,
    );
    builder.ins().jump(exit, &[]);
    Ok(())
}

impl LaneLoop {
    /// The block the loop leaves to.
    pub(super) const fn exit(&self) -> BlockId {
        self.exit
    }
}
