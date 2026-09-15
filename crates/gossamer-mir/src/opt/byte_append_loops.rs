/// Rewrites `for b in s.bytes() { v.push(b) }` into one
/// `gos_rt_vec_extend_str_bytes(v, s)` call.
///
/// The loop is the counted walk a byte iteration lowers to: a length read, a
/// zeroed counter, a `counter < length` header, a byte read, the push, and the
/// increment. Only that exact shape is taken - nothing else in its blocks, no
/// other way into its header, and none of its locals read anywhere else - so
/// the bytes appended, their order, and the vector the call leaves are the ones
/// the loop produces. The loop's blocks stay in place, unreachable from the
/// rewritten call, for the block sweep to remove.
pub(crate) fn fuse_byte_append_loops(body: &mut Body, tcx: &TyCtxt) {
    let rewrites: Vec<(usize, ByteWalk)> = (0..body.blocks.len())
        .filter_map(|pre| {
            let walk = match_byte_walk(body, tcx, pre)?;
            byte_walk_is_closed(body, pre, &walk).then_some((pre, walk))
        })
        .collect();
    for (pre, walk) in rewrites {
        body.blocks[pre].terminator = Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_extend_str_bytes".to_string())),
            args: vec![
                Operand::Copy(Place::local(walk.vec)),
                Operand::Copy(Place::local(walk.string)),
            ],
            destination: Place::local(walk.unit),
            target: Some(walk.exit),
        };
    }
}

/// The blocks and locals of one matched byte walk.
struct ByteWalk {
    blocks: Vec<usize>,
    init: usize,
    header: usize,
    latch: usize,
    exit: BlockId,
    idx: Local,
    len: Local,
    cond: Local,
    byte: Local,
    unit: Local,
    step: Option<Local>,
    string: Local,
    vec: Local,
}

/// The counter a latch increments by one, and the local holding the one when
/// the constant is not yet folded into the addition.
fn match_step(block: &BasicBlock, idx: Local) -> Option<(Local, Option<Local>)> {
    match loud_statements(block).as_slice() {
        [
            StatementKind::Assign {
                place,
                rvalue:
                    Rvalue::BinaryOp {
                        op: BinOp::Add,
                        lhs,
                        rhs: Operand::Const(ConstValue::Int(1)),
                    },
            },
        ] if place.projection.is_empty() && is_bare_copy_of(lhs, idx) => Some((place.local, None)),
        [
            StatementKind::Assign {
                place: one,
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            },
            StatementKind::Assign {
                place,
                rvalue: Rvalue::BinaryOp {
                    op: BinOp::Add,
                    lhs,
                    rhs,
                },
            },
        ] if one.projection.is_empty()
            && place.projection.is_empty()
            && is_bare_copy_of(lhs, idx)
            && is_bare_copy_of(rhs, one.local) =>
        {
            Some((place.local, Some(one.local)))
        }
        _ => None,
    }
}

fn loud_statements(block: &BasicBlock) -> Vec<&StatementKind> {
    block
        .stmts
        .iter()
        .filter(|stmt| !is_quiet_statement(stmt))
        .map(|stmt| &stmt.kind)
        .collect()
}

/// The bare local a block's only statement assigns, when that statement's
/// right-hand side satisfies `shape`.
fn single_assign(block: &BasicBlock, shape: impl Fn(&Rvalue) -> bool) -> Option<Local> {
    let loud = loud_statements(block);
    let [StatementKind::Assign { place, rvalue }] = loud.as_slice() else {
        return None;
    };
    (place.projection.is_empty() && shape(rvalue)).then_some(place.local)
}

fn is_bare_copy_of(op: &Operand, local: Local) -> bool {
    matches!(op, Operand::Copy(p) if p.local == local && p.projection.is_empty())
}

fn is_byte_vec_local(body: &Body, tcx: &TyCtxt, local: Local) -> bool {
    use gossamer_types::{IntTy, TyKind};
    let mut ty = body.local_ty(local);
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    matches!(tcx.kind_of(ty), TyKind::Vec(elem) if matches!(tcx.kind_of(*elem), TyKind::Int(IntTy::U8)))
}

/// A call terminator to the runtime function `name` whose destination is a
/// bare local: its arguments, destination, and continuation.
fn named_call<'a>(block: &'a BasicBlock, names: &[&str]) -> Option<(&'a [Operand], Local, usize)> {
    let Terminator::Call {
        callee: Operand::Const(ConstValue::Str(callee)),
        args,
        destination,
        target: Some(target),
    } = &block.terminator
    else {
        return None;
    };
    (names.contains(&callee.as_str()) && destination.projection.is_empty())
        .then_some((args.as_slice(), destination.local, target.0 as usize))
}

fn goto_target(block: &BasicBlock) -> Option<usize> {
    match &block.terminator {
        Terminator::Goto { target } => Some(target.0 as usize),
        _ => None,
    }
}

fn match_byte_walk(body: &Body, tcx: &TyCtxt, pre: usize) -> Option<ByteWalk> {
    let block = |id: usize| body.blocks.get(id);

    // `len = gos_rt_str_byte_len(s)`, then `idx = 0`.
    let (len_args, len, init) = named_call(block(pre)?, &["gos_rt_str_byte_len"])?;
    let [Operand::Copy(string_place)] = len_args else {
        return None;
    };
    let string = string_place.local;
    if !string_place.projection.is_empty()
        || !matches!(tcx.kind_of(body.local_ty(string)), gossamer_types::TyKind::String)
    {
        return None;
    }
    let idx = single_assign(block(init)?, |rv| {
        matches!(rv, Rvalue::Use(Operand::Const(ConstValue::Int(0))))
    })?;
    let header = goto_target(block(init)?)?;

    // `cond = idx < len`, leaving on zero.
    let cond = single_assign(block(header)?, |rv| {
        matches!(rv, Rvalue::BinaryOp { op: BinOp::Lt, lhs, rhs }
            if is_bare_copy_of(lhs, idx) && is_bare_copy_of(rhs, len))
    })?;
    let Terminator::SwitchInt { discriminant, arms, default } = &block(header)?.terminator else {
        return None;
    };
    let [(0, exit)] = arms.as_slice() else {
        return None;
    };
    if !is_bare_copy_of(discriminant, cond) {
        return None;
    }
    let byte_block = default.0 as usize;

    // `byte = gos_rt_str_byte_at(s, idx)`, then `push(v, byte)`.
    if !loud_statements(block(byte_block)?).is_empty() {
        return None;
    }
    let (byte_args, byte, push_block) = named_call(block(byte_block)?, &["gos_rt_str_byte_at"])?;
    let [string_arg, idx_arg] = byte_args else {
        return None;
    };
    if !is_bare_copy_of(string_arg, string) || !is_bare_copy_of(idx_arg, idx) {
        return None;
    }
    if !loud_statements(block(push_block)?).is_empty() {
        return None;
    }
    let (push_args, unit, after_push) =
        named_call(block(push_block)?, &["gos_rt_vec_push", "gos_rt_vec_push_i64"])?;
    let [Operand::Copy(vec_place), byte_arg] = push_args else {
        return None;
    };
    let vec = vec_place.local;
    if !vec_place.projection.is_empty()
        || !is_bare_copy_of(byte_arg, byte)
        || !is_byte_vec_local(body, tcx, vec)
    {
        return None;
    }

    // `idx = idx + 1` (or `one = 1; idx = idx + one` before constants fold),
    // back to the header, through at most one empty block.
    let mut blocks = vec![init, header, byte_block, push_block];
    let mut latch = after_push;
    if loud_statements(block(latch)?).is_empty()
        && let Some(next) = goto_target(block(latch)?)
    {
        blocks.push(latch);
        latch = next;
    }
    let (stepped, step) = match_step(block(latch)?, idx)?;
    if stepped != idx || goto_target(block(latch)?) != Some(header) {
        return None;
    }
    blocks.push(latch);
    Some(ByteWalk {
        blocks,
        init,
        header,
        latch,
        exit: *exit,
        idx,
        len,
        cond,
        byte,
        unit,
        step,
        string,
        vec,
    })
}

/// `true` when nothing outside the walk enters it past its start or reads the
/// locals it owns, so replacing it with one call changes nothing else.
fn byte_walk_is_closed(body: &Body, pre: usize, walk: &ByteWalk) -> bool {
    let mut owned = vec![walk.idx, walk.len, walk.cond, walk.byte, walk.unit];
    owned.extend(walk.step);
    let mut locals: Vec<u32> = owned.iter().chain(&[walk.string, walk.vec]).map(|l| l.0).collect();
    locals.sort_unstable();
    locals.dedup();
    let in_walk: std::collections::HashSet<usize> = walk.blocks.iter().copied().collect();
    if locals.len() != owned.len() + 2 || in_walk.len() != walk.blocks.len() || in_walk.contains(&pre)
    {
        return false;
    }

    let mut preds: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for succ in successor_indices(&block.terminator) {
            preds.entry(succ).or_default().push(bi);
        }
    }
    let preds_of = |id: usize| {
        let mut p = preds.get(&id).cloned().unwrap_or_default();
        p.sort_unstable();
        p
    };
    let mut header_preds = vec![walk.init, walk.latch];
    header_preds.sort_unstable();
    if preds_of(walk.init) != vec![pre] || preds_of(walk.header) != header_preds {
        return false;
    }
    if walk.blocks[2..].iter().any(|&id| preds_of(id).len() != 1) {
        return false;
    }

    body.blocks.iter().enumerate().all(|(bi, block)| {
        in_walk.contains(&bi)
            || owned.iter().all(|&local| {
                !block.stmts.iter().any(|stmt| stmt_mentions_local(stmt, local))
                    && ((bi == pre && local == walk.len)
                        || !term_mentions_local(&block.terminator, local))
            })
    })
}
