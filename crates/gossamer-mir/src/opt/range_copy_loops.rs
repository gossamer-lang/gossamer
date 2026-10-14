/// Rewrites a counted loop that copies elements from one vector to another,
/// `for i in a..b { dst[i + c] = src[i + d] }`, into one
/// `gos_rt_vec_copy_range_unchecked`, and one that stores a value the loop
/// does not change, `for i in a..b { dst[i + c] = v }`, into one
/// `gos_rt_vec_fill_range_unchecked`.
///
/// Only the fast loop the bounds-check versioner proved in range is taken,
/// in exactly the shape it builds: a `counter < bound` (or `<=`) header, an
/// unchecked read, an unchecked write, and the increment. Nothing else may be
/// in its blocks, its header has one way in besides the latch, and neither
/// its own locals nor its counter are read after it exits. The
/// elements are scalars owning nothing, so a write moves only bytes, and the
/// runtime copies forward wherever a copy within one vector overlaps the way
/// the loop's writes would reach its reads, so the vector ends as the loop
/// leaves it. The loop's blocks stay in place, unreachable, for the block
/// sweep to remove.
pub(crate) fn fuse_range_copy_loops(body: &mut Body, tcx: &TyCtxt) {
    let rewrites: Vec<(usize, RangeLoop)> = (0..body.blocks.len())
        .filter_map(|header| {
            let found = match_range_loop(body, tcx, header)?;
            range_loop_is_closed(body, header, &found).then_some((header, found))
        })
        .collect();
    for (header, found) in rewrites {
        rewrite_range_loop(body, tcx, header, &found);
    }
}

/// What one matched loop moves.
enum RangeWork {
    /// `dst[i + dst_off] = src[i + src_off]`.
    Copy { src: Operand, src_off: Option<Operand> },
    /// `dst[i + dst_off] = value`.
    Fill { value: Operand },
}

/// The blocks and locals of one matched loop.
struct RangeLoop {
    blocks: Vec<usize>,
    exit: BlockId,
    counter: Local,
    bound: Operand,
    inclusive: bool,
    dst: Operand,
    dst_off: Option<Operand>,
    work: RangeWork,
    /// Locals the loop defines for itself, dead once it exits.
    own: Vec<Local>,
}

/// The index operand an unchecked access in a loop body uses, as an offset
/// from the counter (`None` for the counter itself), with the locals the
/// body computes it through. A local the body copies the counter into stands
/// for the counter.
fn counter_offset(
    stmts: &[&StatementKind],
    index: &Operand,
    counter: Local,
) -> Option<(Option<Operand>, Vec<Local>)> {
    let def_of = |local: Local| {
        let defs: Vec<&Rvalue> = stmts
            .iter()
            .filter_map(|kind| match kind {
                StatementKind::Assign { place, rvalue }
                    if place.local == local && place.projection.is_empty() =>
                {
                    Some(rvalue)
                }
                _ => None,
            })
            .collect();
        match defs.as_slice() {
            [rvalue] => Some(*rvalue),
            _ => None,
        }
    };
    // The counter, or a local the body copies it into.
    let counter_like = |op: &Operand| -> Option<Vec<Local>> {
        let Operand::Copy(p) = op else {
            return None;
        };
        if !p.projection.is_empty() {
            return None;
        }
        if p.local == counter {
            return Some(Vec::new());
        }
        match def_of(p.local)? {
            Rvalue::Use(op) if is_bare_copy_of(op, counter) => Some(vec![p.local]),
            _ => None,
        }
    };
    if let Some(locals) = counter_like(index) {
        return Some((None, locals));
    }
    let Operand::Copy(place) = index else {
        return None;
    };
    if !place.projection.is_empty() {
        return None;
    }
    let Rvalue::BinaryOp {
        op: BinOp::Add | BinOp::WrappingAdd,
        lhs,
        rhs,
    } = def_of(place.local)?
    else {
        return None;
    };
    let (mut locals, offset) = match (counter_like(lhs), counter_like(rhs)) {
        (Some(locals), None) => (locals, rhs),
        (None, Some(locals)) => (locals, lhs),
        _ => return None,
    };
    locals.push(place.local);
    Some((Some(offset.clone()), locals))
}

fn match_range_loop(body: &Body, tcx: &TyCtxt, header: usize) -> Option<RangeLoop> {
    let head = &body.blocks[header];
    let Terminator::SwitchInt {
        discriminant: Operand::Copy(cond_place),
        arms,
        default,
    } = &head.terminator
    else {
        return None;
    };
    let [(0, exit)] = arms.as_slice() else {
        return None;
    };
    let head_stmts = loud_statements(head);
    let [
        StatementKind::Assign {
            place: cond,
            rvalue:
                Rvalue::BinaryOp {
                    op: cmp @ (BinOp::Lt | BinOp::Le),
                    lhs: Operand::Copy(counter_place),
                    rhs: bound,
                },
        },
    ] = head_stmts.as_slice()
    else {
        return None;
    };
    if cond.local != cond_place.local
        || !cond.projection.is_empty()
        || !counter_place.projection.is_empty()
    {
        return None;
    }
    let counter = counter_place.local;
    let inclusive = matches!(cmp, BinOp::Le);

    // The body: an optional read, then the write.
    let first = default.0 as usize;
    let LoopBody {
        dst,
        dst_off,
        work,
        next: write_target,
        own: body_own,
        blocks: body_blocks,
    } = match_loop_body(body, first, counter)?;
    let mut own: Vec<Local> = vec![cond.local];
    own.extend(body_own);
    let mut blocks = vec![header, first];
    blocks.extend(body_blocks);

    // An optional empty block, then the latch back to the header.
    let mut latch = write_target.0 as usize;
    if loud_statements(&body.blocks[latch]).is_empty()
        && let Terminator::Goto { target } = body.blocks[latch].terminator
        && target.0 as usize != header
    {
        blocks.push(latch);
        latch = target.0 as usize;
    }
    let latch_block = &body.blocks[latch];
    let Terminator::Goto { target } = latch_block.terminator else {
        return None;
    };
    if target.0 as usize != header {
        return None;
    }
    if !steps_counter_by_one(latch_block, counter) {
        return None;
    }
    blocks.push(latch);

    // Scalars owning nothing move as bytes alone.
    let scalar_vec = |op: &Operand| match op {
        Operand::Copy(p) if p.projection.is_empty() => is_scalar_elem_vec(body, tcx, p.local),
        _ => false,
    };
    if !scalar_vec(&dst) {
        return None;
    }
    if let RangeWork::Copy { src, .. } = &work
        && !scalar_vec(src)
    {
        return None;
    }
    Some(RangeLoop {
        blocks,
        exit: *exit,
        counter,
        bound: bound.clone(),
        inclusive,
        dst,
        dst_off,
        work,
        own,
    })
}

/// The read and write blocks of a matched loop.
struct LoopBody {
    dst: Operand,
    dst_off: Option<Operand>,
    work: RangeWork,
    /// The block the write continues to.
    next: BlockId,
    own: Vec<Local>,
    /// Blocks past the first that the body spans.
    blocks: Vec<usize>,
}

/// Matches the loop body starting at block `first`: an unchecked read of a
/// counter-indexed element written at a counter-indexed slot, or a write of
/// one value at a counter-indexed slot.
fn match_loop_body(body: &Body, first: usize, counter: Local) -> Option<LoopBody> {
    let first_block = &body.blocks[first];
    let first_stmts = loud_statements(first_block);
    let Terminator::Call {
        callee: Operand::Const(ConstValue::Str(first_name)),
        args: first_args,
        destination: first_dest,
        target: Some(after_first),
    } = &first_block.terminator
    else {
        return None;
    };
    let mut own: Vec<Local> = Vec::new();
    let mut blocks: Vec<usize> = Vec::new();
    let (dst, dst_off, work, next) = match first_name.as_str() {
        "gos_rt_vec_get_i64_unchecked" => {
            let [src, src_index] = first_args.as_slice() else {
                return None;
            };
            let (src_off, src_locals) = counter_offset(&first_stmts, src_index, counter)?;
            if !first_dest.projection.is_empty() {
                return None;
            }
            let read = first_dest.local;
            let write = after_first.0 as usize;
            let write_block = &body.blocks[write];
            let write_stmts = loud_statements(write_block);
            let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(write_name)),
                args: write_args,
                destination: write_dest,
                target: Some(after_write),
            } = &write_block.terminator
            else {
                return None;
            };
            if write_name != "gos_rt_vec_set_i64_unchecked" {
                return None;
            }
            let [dst, dst_index, value] = write_args.as_slice() else {
                return None;
            };
            if !is_bare_copy_of(value, read) {
                return None;
            }
            let mut stmts = first_stmts.clone();
            stmts.extend(write_stmts.iter().copied());
            let (dst_off, dst_locals) = counter_offset(&stmts, dst_index, counter)?;
            // Every statement in the two blocks computes one of the indices.
            let index_locals: Vec<Local> = src_locals.iter().chain(&dst_locals).copied().collect();
            if !stmts.iter().all(|kind| {
                matches!(kind, StatementKind::Assign { place, .. } if index_locals.contains(&place.local))
            }) {
                return None;
            }
            own.extend(index_locals);
            own.push(read);
            own.push(write_dest.local);
            blocks.push(write);
            (
                dst.clone(),
                dst_off,
                RangeWork::Copy {
                    src: src.clone(),
                    src_off,
                },
                *after_write,
            )
        }
        "gos_rt_vec_set_i64_unchecked" => {
            let [dst, dst_index, value] = first_args.as_slice() else {
                return None;
            };
            let (dst_off, dst_locals) = counter_offset(&first_stmts, dst_index, counter)?;
            if !first_stmts.iter().all(|kind| {
                matches!(kind, StatementKind::Assign { place, .. } if dst_locals.contains(&place.local))
            }) {
                return None;
            }
            own.extend(dst_locals);
            own.push(first_dest.local);
            (
                dst.clone(),
                dst_off,
                RangeWork::Fill {
                    value: value.clone(),
                },
                *after_first,
            )
        }
        _ => return None,
    };
    Some(LoopBody {
        dst,
        dst_off,
        work,
        next,
        own,
        blocks,
    })
}

/// Whether `block` does nothing but add one to `counter`, checked or not.
fn steps_counter_by_one(block: &BasicBlock, counter: Local) -> bool {
    matches!(
        loud_statements(block).as_slice(),
        [StatementKind::Assign {
            place,
            rvalue: Rvalue::BinaryOp {
                op: BinOp::Add | BinOp::WrappingAdd,
                lhs,
                rhs: Operand::Const(ConstValue::Int(1)),
            },
        }] if place.local == counter && place.projection.is_empty() && is_bare_copy_of(lhs, counter)
    )
}

fn is_scalar_elem_vec(body: &Body, tcx: &TyCtxt, local: Local) -> bool {
    use gossamer_types::TyKind;
    let mut ty = body.local_ty(local);
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    matches!(
        tcx.kind_of(ty),
        TyKind::Vec(elem) | TyKind::Slice(elem)
            if matches!(
                tcx.kind_of(*elem),
                TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
            )
    )
}

/// Whether the loop can be replaced whole: its header is entered once from
/// outside, nothing it reads changes inside it, and neither its own locals
/// nor its counter are read after it exits.
fn range_loop_is_closed(body: &Body, header: usize, found: &RangeLoop) -> bool {
    let header_id = body.blocks[header].id;
    let entries = body
        .blocks
        .iter()
        .enumerate()
        .filter(|(bi, block)| {
            !found.blocks.contains(bi) && block_successors(&block.terminator).contains(&header_id)
        })
        .count();
    if entries != 1 {
        return false;
    }
    // Locals the loop reads but must not write.
    let mut invariant: Vec<Local> = Vec::new();
    let mut note = |op: &Operand| {
        if let Operand::Copy(p) = op {
            invariant.push(p.local);
        }
    };
    note(&found.bound);
    note(&found.dst);
    if let Some(off) = &found.dst_off {
        note(off);
    }
    match &found.work {
        RangeWork::Copy { src, src_off, .. } => {
            note(src);
            if let Some(off) = src_off {
                note(off);
            }
        }
        RangeWork::Fill { value } => note(value),
    }
    for &bi in &found.blocks {
        let block = &body.blocks[bi];
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && invariant.contains(&place.local)
            {
                return false;
            }
        }
        if let Terminator::Call { destination, .. } = &block.terminator
            && invariant.contains(&destination.local)
        {
            return false;
        }
    }
    // The loop's own locals, and its counter, are dead once it exits.
    let exit = found.exit.0 as usize;
    !found
        .own
        .iter()
        .chain(std::iter::once(&found.counter))
        .any(|&local| live_from(body, exit, local, &found.blocks))
}

/// Whether some path from block `start`, avoiding `skip`, reads `local`
/// before writing it.
fn live_from(body: &Body, start: usize, local: Local, skip: &[usize]) -> bool {
    let mut seen = vec![false; body.blocks.len()];
    let mut work = vec![start];
    while let Some(bi) = work.pop() {
        if seen[bi] || skip.contains(&bi) {
            continue;
        }
        seen[bi] = true;
        let block = &body.blocks[bi];
        let mut written = false;
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    if rvalue_mentions_local(rvalue, local)
                        || (place.local == local && !place.projection.is_empty())
                    {
                        return true;
                    }
                    if place.local == local {
                        written = true;
                        break;
                    }
                }
                StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {}
                _ => {
                    if statement_mentions_local(stmt, local) {
                        return true;
                    }
                }
            }
        }
        if written {
            continue;
        }
        match &block.terminator {
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                if scalar_replacement_operand_mentions_local(callee, local)
                    || args
                        .iter()
                        .any(|arg| scalar_replacement_operand_mentions_local(arg, local))
                    || (destination.local == local && !destination.projection.is_empty())
                {
                    return true;
                }
                if destination.local == local {
                    continue;
                }
            }
            other => {
                if terminator_mentions_local(other, local) {
                    return true;
                }
            }
        }
        work.extend(
            block_successors(&block.terminator)
                .into_iter()
                .map(|id| id.0 as usize),
        );
    }
    false
}

fn rewrite_range_loop(body: &mut Body, tcx: &TyCtxt, header: usize, found: &RangeLoop) {
    // Counts and positions are words of the counter's own type.
    let i64_ty = body.local_ty(found.counter);
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let fresh = |body: &mut Body, ty| {
        let local = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(LocalDecl {
            ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        local
    };
    let span = body.blocks[header].span;
    let assign = |place: Local, rvalue: Rvalue| Statement {
        kind: StatementKind::Assign {
            place: Place::local(place),
            rvalue,
        },
        span,
        inlined: None,
    };
    let counter = Operand::Copy(Place::local(found.counter));
    let mut stmts = Vec::new();
    // The elements the loop visits: `bound - i`, one more for `<=`.
    let span_len = fresh(body, i64_ty);
    stmts.push(assign(
        span_len,
        Rvalue::BinaryOp {
            op: BinOp::WrappingSub,
            lhs: found.bound.clone(),
            rhs: counter.clone(),
        },
    ));
    let count = if found.inclusive {
        let count = fresh(body, i64_ty);
        stmts.push(assign(
            count,
            Rvalue::BinaryOp {
                op: BinOp::WrappingAdd,
                lhs: Operand::Copy(Place::local(span_len)),
                rhs: Operand::Const(ConstValue::Int(1)),
            },
        ));
        count
    } else {
        span_len
    };
    let start = |body: &mut Body, stmts: &mut Vec<Statement>, off: &Option<Operand>| match off {
        None => counter.clone(),
        Some(off) => {
            let at = fresh(body, i64_ty);
            stmts.push(assign(
                at,
                Rvalue::BinaryOp {
                    op: BinOp::WrappingAdd,
                    lhs: counter.clone(),
                    rhs: off.clone(),
                },
            ));
            Operand::Copy(Place::local(at))
        }
    };
    let dst_start = start(body, &mut stmts, &found.dst_off);
    let (callee, args) = match &found.work {
        RangeWork::Copy { src, src_off, .. } => {
            let src_start = start(body, &mut stmts, src_off);
            (
                "gos_rt_vec_copy_range_unchecked",
                vec![
                    found.dst.clone(),
                    dst_start,
                    src.clone(),
                    src_start,
                    Operand::Copy(Place::local(count)),
                ],
            )
        }
        RangeWork::Fill { value } => (
            "gos_rt_vec_fill_range_unchecked",
            vec![
                found.dst.clone(),
                dst_start,
                Operand::Copy(Place::local(count)),
                value.clone(),
            ],
        ),
    };
    let unit = fresh(body, unit_ty);
    let block = &mut body.blocks[header];
    block.stmts = stmts;
    block.terminator = Terminator::Call {
        callee: Operand::Const(ConstValue::Str(callee.to_string())),
        args,
        destination: Place::local(unit),
        target: Some(found.exit),
    };
}
