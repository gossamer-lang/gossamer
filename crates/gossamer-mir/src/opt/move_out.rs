/// Moves elements out of a vector a counted walk reads once each, instead of
/// copying them, when the walk is all that remains of the vector.
///
/// `for row in rows { c.push(row) }` reads each element by index and stores a
/// copy of it, because the store must own what it holds while `rows` still
/// owns its own. When `rows` is a frame-owned vector that only that walk reads
/// (every read of it is the walk's element read at its counter, a length, or
/// its final free) and the element read is used by nothing but the copy, the
/// element itself can change hands: the runtime takes it out of its slot,
/// leaving the slot empty so the vector's free passes it over. The runtime
/// makes the copy after all when the vector's storage is shared, or the slot
/// no longer holds the element, so the move is only ever an optimisation.
///
/// Each slot is read once because the counter only counts up from zero, and
/// every reset to zero follows, along a single path, a fresh value of the
/// vector.
pub(crate) fn move_elements_out_of_walked_vecs(body: &mut Body) {
    let arity = body.arity as usize;
    let preds = predecessor_lists(body);
    let mut rewrites: Vec<(usize, Local, Local)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } = &block.terminator
        else {
            continue;
        };
        if name != "gos_rt_vec_clone" {
            continue;
        }
        let [Operand::Copy(value)] = args.as_slice() else {
            continue;
        };
        if !value.projection.is_empty() {
            continue;
        }
        let Some((src, idx)) = walked_element(body, value.local, bi) else {
            continue;
        };
        if (src.0 as usize) <= arity
            || !only_walked(body, src, idx)
            || !counts_up(body, idx)
            || !resets_follow_fresh_value(body, &preds, src, idx)
        {
            continue;
        }
        rewrites.push((bi, src, idx));
    }
    for (bi, src, idx) in rewrites {
        if let Terminator::Call { callee, args, .. } = &mut body.blocks[bi].terminator {
            let value = args[0].clone();
            *callee = Operand::Const(ConstValue::Str("gos_rt_vec_take_vec_elem".to_string()));
            *args = vec![
                Operand::Copy(Place::local(src)),
                Operand::Copy(Place::local(idx)),
                value,
            ];
        }
    }
}

/// The vector and counter `value` was read from, when its one value is an
/// element read `gos_rt_vec_get_i64(src, idx)` and its one use the copy that
/// ends block `copy_block`.
fn walked_element(body: &Body, value: Local, copy_block: usize) -> Option<(Local, Local)> {
    let mut read: Option<(Local, Local)> = None;
    for (bi, block) in body.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            if !stmt_mentions_local(stmt, value) {
                continue;
            }
            // Clearing the binding is no value of its own.
            let clears = matches!(&stmt.kind, StatementKind::Assign { place, rvalue }
                if place.local == value
                    && place.projection.is_empty()
                    && matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0)))));
            if !clears {
                return None;
            }
        }
        if !term_mentions_local(&block.terminator, value) {
            continue;
        }
        if bi == copy_block {
            continue;
        }
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            ..
        } = &block.terminator
        else {
            return None;
        };
        let element_read = matches!(
            name.as_str(),
            "gos_rt_vec_get_i64" | "gos_rt_vec_get_i64_unchecked"
        );
        let defines = destination.local == value && destination.projection.is_empty();
        match (element_read, defines, args.as_slice()) {
            (true, true, [Operand::Copy(src), Operand::Copy(idx)])
                if src.projection.is_empty()
                    && idx.projection.is_empty()
                    && src.local != value
                    && idx.local != value
                    && read.is_none() =>
            {
                read = Some((src.local, idx.local));
            }
            _ => return None,
        }
    }
    read
}

/// Whether every mention of `src` is a whole new value of it, its length, its
/// free, or the one element read at `idx`.
fn only_walked(body: &Body, src: Local, idx: Local) -> bool {
    let mut element_reads = 0usize;
    for block in &body.blocks {
        for stmt in &block.stmts {
            if !stmt_mentions_local(stmt, src) {
                continue;
            }
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                return false;
            };
            let defines = place.local == src
                && place.projection.is_empty()
                && (matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0))))
                    || private_payload(body, rvalue));
            let frees = matches!(rvalue, Rvalue::CallIntrinsic { name: "gos_rt_vec_free", args }
                if matches!(args.as_slice(), [Operand::Copy(p)] if p.local == src && p.projection.is_empty()));
            let reads_src = rvalue_mentions_local(rvalue, src);
            let allowed = (defines && !reads_src) || (frees && place.local != src);
            if !allowed {
                return false;
            }
        }
        if !term_mentions_local(&block.terminator, src) {
            continue;
        }
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            ..
        } = &block.terminator
        else {
            return false;
        };
        let names_src = |op: &Operand| matches!(op, Operand::Copy(p) if p.local == src);
        if destination.local == src {
            if !destination.projection.is_empty() || args.iter().any(names_src) {
                return false;
            }
            continue;
        }
        match (name.as_str(), args.as_slice()) {
            ("gos_rt_vec_len" | "gos_rt_len" | "gos_rt_vec_free", [Operand::Copy(p)])
                if p.local == src && p.projection.is_empty() => {}
            (
                "gos_rt_vec_get_i64" | "gos_rt_vec_get_i64_unchecked",
                [Operand::Copy(p), Operand::Copy(i)],
            ) if p.local == src
                && p.projection.is_empty()
                && i.local == idx
                && i.projection.is_empty() =>
            {
                element_reads += 1;
            }
            _ => return false,
        }
    }
    element_reads == 1
}

/// Whether `rvalue` takes the payload out of a `Result` local that nothing
/// else reads: a fresh call's answer, checked and emptied where it stands.
/// The payload then has no holder but the local it is moved into.
fn private_payload(body: &Body, rvalue: &Rvalue) -> bool {
    let Rvalue::CallIntrinsic {
        name: "gos_rt_result_payload",
        args,
    } = rvalue
    else {
        return false;
    };
    let [Operand::Copy(res)] = args.as_slice() else {
        return false;
    };
    if !res.projection.is_empty() || (res.local.0 as usize) <= body.arity as usize {
        return false;
    }
    let res = res.local;
    let names_res = |op: &Operand| matches!(op, Operand::Copy(p) if p.local == res);
    body.blocks.iter().all(|block| {
        let stmts_ok = block.stmts.iter().all(|stmt| {
            if !stmt_mentions_local(stmt, res) {
                return true;
            }
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                return false;
            };
            match rvalue {
                Rvalue::CallIntrinsic {
                    name: "gos_rt_result_new",
                    args,
                } => place.local == res && place.projection.is_empty() && !args.iter().any(names_res),
                Rvalue::Use(Operand::Const(ConstValue::Int(0))) => place.local == res,
                Rvalue::CallIntrinsic {
                    name:
                        "gos_rt_result_disc" | "gos_rt_result_payload" | "gos_rt_result_payload_release",
                    args,
                } => place.local != res && matches!(args.as_slice(), [Operand::Copy(p)] if p.local == res && p.projection.is_empty()),
                _ => false,
            }
        });
        let term_ok = !term_mentions_local(&block.terminator, res)
            || matches!(&block.terminator, Terminator::Call { args, destination, .. }
                if destination.local == res
                    && destination.projection.is_empty()
                    && !args.iter().any(names_res));
        stmts_ok && term_ok
    })
}

/// Whether `idx` is only ever set to zero or advanced by one.
fn counts_up(body: &Body, idx: Local) -> bool {
    body.blocks.iter().all(|block| {
        block.stmts.iter().all(|stmt| match &stmt.kind {
            StatementKind::Assign { place, rvalue } if place.local == idx => {
                place.projection.is_empty()
                    && match rvalue {
                        Rvalue::Use(Operand::Const(ConstValue::Int(0))) => true,
                        Rvalue::BinaryOp {
                            op: BinOp::Add | BinOp::WrappingAdd,
                            lhs: Operand::Copy(l),
                            rhs: Operand::Const(ConstValue::Int(1)),
                        } => l.local == idx && l.projection.is_empty(),
                        _ => false,
                    }
            }
            _ => true,
        }) && !matches!(&block.terminator, Terminator::Call { destination, .. }
            if destination.local == idx)
    })
}

/// Whether every reset of `idx` to zero is reached, along a path with no
/// other way in, from a new value of `src`.
fn resets_follow_fresh_value(body: &Body, preds: &[Vec<usize>], src: Local, idx: Local) -> bool {
    let gives_src = |stmt: &Statement| {
        matches!(&stmt.kind, StatementKind::Assign { place, rvalue }
            if place.local == src
                && place.projection.is_empty()
                && private_payload(body, rvalue))
    };
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let resets = matches!(&stmt.kind, StatementKind::Assign { place, rvalue }
                if place.local == idx
                    && matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0)))));
            if !resets {
                continue;
            }
            if block.stmts[..si].iter().any(gives_src) {
                continue;
            }
            let mut at = bi;
            let mut found = false;
            for _ in 0..body.blocks.len() {
                let [pred] = preds[at].as_slice() else {
                    break;
                };
                let pred_block = &body.blocks[*pred];
                let ends_with_src = matches!(&pred_block.terminator, Terminator::Call { destination, .. }
                    if destination.local == src && destination.projection.is_empty());
                if ends_with_src || pred_block.stmts.iter().any(gives_src) {
                    found = true;
                    break;
                }
                at = *pred;
            }
            if !found {
                return false;
            }
        }
    }
    true
}
