/// Rewrites `for word in s.split_whitespace() { m.inc(word) }` into one
/// `gos_rt_map_inc_fields_str_i64(m, s, by)` call, which counts each piece in
/// place instead of building every word as a `String` first.
///
/// The loop is the counted walk a `for` over the split's `[String]` lowers
/// to: the split, a length read, a zeroed counter, a `counter < length`
/// header, an element read, the `inc` with a constant step, and the
/// increment. Only that exact shape is taken - nothing else in its blocks, no
/// other way into its header, and none of its locals read anywhere else - so
/// the counts the map ends with are the ones the loop produces. The loop's
/// blocks stay in place, unreachable from the rewritten call, for the block
/// sweep to remove.
pub(crate) fn fuse_field_count_loops(body: &mut Body, tcx: &mut TyCtxt) {
    let rewrites: Vec<(usize, FieldWalk)> = (0..body.blocks.len())
        .filter_map(|pre| {
            let walk = match_field_walk(body, tcx, pre)?;
            field_walk_is_closed(body, pre, &walk).then_some((pre, walk))
        })
        .collect();
    if rewrites.is_empty() {
        return;
    }
    let unit_ty = tcx.unit();
    for (pre, walk) in rewrites {
        let unit = Local(body.locals.len() as u32);
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        body.blocks[pre].terminator = Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_map_inc_fields_str_i64".to_string())),
            args: vec![
                Operand::Copy(Place::local(walk.map)),
                walk.string,
                Operand::Const(ConstValue::Int(walk.by)),
            ],
            destination: Place::local(unit),
            target: Some(walk.exit),
        };
    }
}

/// The blocks and locals of one matched field walk.
struct FieldWalk {
    blocks: Vec<usize>,
    init: usize,
    header: usize,
    latch: usize,
    exit: BlockId,
    /// Locals the walk alone defines and reads.
    owned: Vec<Local>,
    string: Operand,
    map: Local,
    by: i128,
}

fn match_field_walk(body: &Body, tcx: &TyCtxt, pre: usize) -> Option<FieldWalk> {
    use gossamer_types::{IntTy, TyKind};
    let block = |id: usize| body.blocks.get(id);

    // `words = split_whitespace(s)`, `len = vec_len(words)`, then `idx = 0`.
    let (split_args, words, len_block) = named_call(
        block(pre)?,
        &["gos_rt_str_split_whitespace", "gos_rt_str_fields"],
    )?;
    let [string] = split_args else {
        return None;
    };
    if !loud_statements(block(len_block)?).is_empty() {
        return None;
    }
    let (len_args, len, init) = named_call(block(len_block)?, &["gos_rt_vec_len"])?;
    if !matches!(len_args, [words_arg] if is_bare_copy_of(words_arg, words)) {
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
    let read_block = default.0 as usize;

    // `word = vec_get(words, idx)`, then `step = <const>; inc(m, word, step)`.
    if !loud_statements(block(read_block)?).is_empty() {
        return None;
    }
    let (get_args, word, inc_block) = named_call(block(read_block)?, &["gos_rt_vec_get_i64"])?;
    if !matches!(get_args, [w, i] if is_bare_copy_of(w, words) && is_bare_copy_of(i, idx)) {
        return None;
    }
    let (inc_args, counted, after_inc) = named_call(
        block(inc_block)?,
        &["gos_rt_map_inc_str_i64", "gos_rt_map_inc_typed_str_i64"],
    )?;
    let [Operand::Copy(map_place), word_arg, step_arg] = inc_args else {
        return None;
    };
    let map = map_place.local;
    if !map_place.projection.is_empty() || !is_bare_copy_of(word_arg, word) {
        return None;
    }
    let (by, step) = match (step_arg, loud_statements(block(inc_block)?).as_slice()) {
        (Operand::Const(ConstValue::Int(by)), []) => (*by, None),
        (
            Operand::Copy(step_place),
            [StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(by))),
            }],
        ) if step_place.projection.is_empty()
            && place.projection.is_empty()
            && place.local == step_place.local =>
        {
            (*by, Some(place.local))
        }
        _ => return None,
    };
    if !matches!(tcx.kind_of(body.local_ty(counted)), TyKind::Int(IntTy::I64)) {
        return None;
    }

    // `idx = idx + 1`, back to the header, through at most one empty block.
    let mut blocks = vec![len_block, init, header, read_block, inc_block];
    let mut latch = after_inc;
    if loud_statements(block(latch)?).is_empty()
        && let Some(next) = goto_target(block(latch)?)
    {
        blocks.push(latch);
        latch = next;
    }
    let (stepped, one) = match_step(block(latch)?, idx)?;
    if stepped != idx || goto_target(block(latch)?) != Some(header) {
        return None;
    }
    blocks.push(latch);
    let mut owned = vec![words, len, idx, cond, word, counted];
    owned.extend(step);
    owned.extend(one);
    Some(FieldWalk {
        blocks,
        init,
        header,
        latch,
        exit: *exit,
        owned,
        string: string.clone(),
        map,
        by,
    })
}

fn field_walk_is_closed(body: &Body, pre: usize, walk: &FieldWalk) -> bool {
    let mut locals: Vec<u32> = walk.owned.iter().map(|l| l.0).collect();
    locals.push(walk.map.0);
    if let Operand::Copy(place) = &walk.string {
        locals.push(place.local.0);
    }
    locals.sort_unstable();
    locals.dedup();
    let string_is_local = matches!(&walk.string, Operand::Copy(p) if p.projection.is_empty());
    let distinct = walk.owned.len() + 1 + usize::from(string_is_local);
    let in_walk: std::collections::HashSet<usize> = walk.blocks.iter().copied().collect();
    if !string_is_local
        || locals.len() != distinct
        || in_walk.len() != walk.blocks.len()
        || in_walk.contains(&pre)
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
    if preds_of(walk.blocks[0]) != vec![pre] || preds_of(walk.header) != header_preds {
        return false;
    }
    if walk
        .blocks
        .iter()
        .filter(|&&id| id != walk.header)
        .any(|&id| preds_of(id).len() != 1)
    {
        return false;
    }

    // The walk's own locals appear nowhere else; the split's result is
    // assigned only by the split itself. The map and the string may be used
    // freely outside the walk, but inside it only by the split and the `inc`.
    let words = walk.owned[0];
    body.blocks.iter().enumerate().all(|(bi, block)| {
        if in_walk.contains(&bi) {
            return block.stmts.iter().all(|stmt| {
                !stmt_mentions_local(stmt, walk.map)
                    && !matches!(&walk.string, Operand::Copy(p) if stmt_mentions_local(stmt, p.local))
            });
        }
        walk.owned.iter().all(|&local| {
            !block.stmts.iter().any(|stmt| {
                stmt_mentions_local(stmt, local)
                    && !(local == words && is_null_init_of(stmt, words))
            }) && (bi == pre && local == words || !term_mentions_local(&block.terminator, local))
        })
    })
}

/// Whether `stmt` is `local = 0`, the null a `[String]` slot starts as.
fn is_null_init_of(stmt: &Statement, local: Local) -> bool {
    matches!(&stmt.kind, StatementKind::Assign {
        place,
        rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
    } if place.local == local && place.projection.is_empty())
}
