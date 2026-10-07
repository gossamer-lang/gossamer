//! Channels confined to one body, and set definitions.

use super::*;

/// Reclaims a channel that never leaves the function that made it.
///
/// A channel is shared: the sender end, the receiver end, and any goroutine
/// that captured one all reach the same handle, and only a party that took a
/// reference of its own may give one back. So the drop is emitted only where
/// every local derived from the pair is confined to this body - read by the
/// channel's own runtime helpers and nothing else. A channel that is returned,
/// stored, captured, or handed to any other callee is left alone.
///
/// The drop is what runs the channel's teardown, which gives back the share
/// each send minted for a value nobody received.
pub(crate) fn drop_confined_channels(body: &mut Body) {
    const CREATORS: &[&str] = &[
        "channel",
        "channel::new",
        "channel::unbounded",
        "sync::channel",
        "sync::channel_unbounded",
        "std::sync::channel",
        "std::sync::channel_unbounded",
        "sync::Channel::new",
        "Channel::new",
    ];

    let n = body.locals.len();
    let creations: Vec<Local> = body
        .blocks
        .iter()
        .filter_map(|block| match &block.terminator {
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                destination,
                ..
            } if destination.projection.is_empty()
                && (destination.local.0 as usize) < n
                && CREATORS.contains(&name.as_str()) =>
            {
                Some(destination.local)
            }
            _ => None,
        })
        .collect();
    if creations.is_empty() {
        return;
    }

    // Each channel is judged on its own: one that escapes leaves the others
    // alone, and a body that opens several reclaims each of them.
    let mut handles: Vec<Local> = Vec::new();
    for pair in creations {
        if let Some(handle) = confined_channel_handle(body, pair, n) {
            handles.push(handle);
        }
    }
    handles.sort_unstable_by_key(|l| l.0);
    handles.dedup();
    for handle in handles {
        emit_channel_drops(body, handle);
    }
}

/// The local holding a channel's handle, when every local derived from the pair
/// `pair` names is confined to this body - read by the channel's own runtime
/// helpers and nothing else. A channel that is returned, stored, captured, or
/// handed to any other callee answers `None`.
pub(super) fn confined_channel_handle(body: &Body, pair: Local, n: usize) -> Option<Local> {
    let mut derived = vec![false; n];
    derived[pair.0 as usize] = true;
    let mut changed = true;
    while changed {
        changed = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < n
                    && !derived[place.local.0 as usize]
                    && let Rvalue::Use(Operand::Copy(src)) = rvalue
                    && (src.local.0 as usize) < n
                    && derived[src.local.0 as usize]
                {
                    derived[place.local.0 as usize] = true;
                    changed = true;
                }
            }
        }
    }

    let reads = |op: &Operand| -> bool {
        matches!(op, Operand::Copy(p) if (p.local.0 as usize) < n && derived[p.local.0 as usize])
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            let confined_target = place.projection.is_empty()
                && (place.local.0 as usize) < n
                && derived[place.local.0 as usize];
            match rvalue {
                // Splitting the pair, or aliasing an end, stays inside.
                Rvalue::Use(op) if reads(op) && !confined_target => return None,
                Rvalue::CallIntrinsic { name, args }
                    if args.iter().any(&reads) && !name.starts_with("gos_rt_chan_") =>
                {
                    return None;
                }
                Rvalue::Aggregate { operands, .. }
                    if operands.iter().any(&reads) && !confined_target =>
                {
                    return None;
                }
                Rvalue::BinaryOp { lhs, rhs, .. } if reads(lhs) || reads(rhs) => return None,
                _ => {}
            }
        }
        match &block.terminator {
            Terminator::Call { callee, args, .. } if args.iter().any(&reads) => {
                let confined_callee = matches!(
                    callee,
                    Operand::Const(ConstValue::Str(name)) if name.starts_with("gos_rt_chan_")
                );
                if !confined_callee {
                    return None;
                }
            }
            Terminator::SwitchInt { discriminant, .. } if reads(discriminant) => return None,
            _ => {}
        }
    }
    // The return slot is derived exactly when an end is returned.
    if derived[Local::RETURN.0 as usize] {
        return None;
    }

    // Drop through the local a send or a recv named: that one holds the handle
    // itself rather than the pair it was projected out of.
    body.blocks
        .iter()
        .find_map(|block| match &block.terminator {
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                ..
            } if name.starts_with("gos_rt_chan_") => match args.first() {
                Some(Operand::Copy(p))
                    if p.projection.is_empty()
                        && (p.local.0 as usize) < n
                        && derived[p.local.0 as usize] =>
                {
                    Some(p.local)
                }
                _ => None,
            },
            _ => None,
        })
}

/// One drop per channel: wherever the handle is reassigned, and again at every
/// exit. A loop that opens a channel per iteration reclaims each one as the
/// next takes its place, and the handle starts null so the first iteration's
/// drop is the no-op a null handle answers.
pub(super) fn emit_channel_drops(body: &mut Body, handle: Local) {
    let unit_ty = body.locals[0].ty;
    let drop_stmt = |body: &mut Body, span: Span| -> Statement {
        let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(crate::ir::LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(sink),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_chan_drop",
                    args: vec![Operand::Copy(Place::local(handle))],
                },
            },
            span,
            inlined: None,
        }
    };

    let mut reassignments: Vec<(usize, usize)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
                && place.local == handle
            {
                reassignments.push((bi, si));
            }
        }
    }
    reassignments.sort_unstable();
    for (bi, si) in reassignments.into_iter().rev() {
        let span = body.blocks[bi].span;
        let stmt = drop_stmt(body, span);
        body.blocks[bi].stmts.insert(si, stmt);
    }

    let returns: Vec<usize> = body
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| matches!(b.terminator, Terminator::Return))
        .map(|(i, _)| i)
        .collect();
    for bi in returns {
        let span = body.blocks[bi].span;
        let stmt = drop_stmt(body, span);
        body.blocks[bi].stmts.push(stmt);
    }

    let span = body.blocks[0].span;
    body.blocks[0].stmts.insert(
        0,
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(handle),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            },
            span,
            inlined: None,
        },
    );
}

/// Tells a channel what its element word owns, at each send.
///
/// The send mints the channel's share of the element's heap storage (see
/// `stores_aggregate_by_pointer`); a receiver gives it back, and a value nobody
/// receives is given back by the channel's teardown - which needs to know the
/// shape of the word it is holding, and learns it here, where the element's
/// static type is in hand.
/// The runtime call that marks a value of type `ty` shared as it is sent to
/// another goroutine, with the layout meta symbol an in-place aggregate walk
/// reads, or `None` when the value counts nothing.
pub(super) fn send_mark_shared_call(
    tcx: &mut gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<(&'static str, Option<String>)> {
    use gossamer_types::TyKind;
    let mut cur = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(cur) {
        cur = *inner;
    }
    match tcx.kind_of(cur).clone() {
        TyKind::Vec(_) | TyKind::Slice(_) => return Some(("gos_rt_vec_mark_shared", None)),
        TyKind::HashMap { .. } => return Some(("gos_rt_map_mark_shared", None)),
        TyKind::Adt { def, .. } if is_set_def(tcx, def) => {
            return Some(("gos_rt_set_mark_shared", None));
        }
        _ => {}
    }
    if tcx.is_rc_managed(cur) {
        return Some(("gos_rt_rc_mark_shared", None));
    }
    let inline = match tcx.kind_of(cur) {
        TyKind::Tuple(_) | TyKind::Array { .. } => true,
        TyKind::Adt { def, .. } => {
            def.local < u32::MAX - 16 && tcx.struct_field_tys(*def).is_some()
        }
        _ => false,
    };
    if !inline {
        return None;
    }
    let mut entries = Vec::new();
    send_layout_entries(tcx, cur, 0, 0, &mut entries);
    if entries.is_empty() {
        return None;
    }
    let symbol = format!("gos_rc_meta_sendmark_{}", cur.as_u32());
    if tcx.rc_meta(&symbol).is_none() {
        let mut blob = vec![gossamer_abi::rc::RC_KIND_STRUCT, 1, 0, entries.len() as i64];
        blob.extend_from_slice(&entries);
        tcx.register_rc_meta(symbol.clone(), blob);
    }
    Some(("gos_rt_aggr_mark_shared_children", Some(symbol)))
}

/// Whether `def` names the `Set` / `BTreeSet` handle.
pub(super) fn is_set_def(tcx: &gossamer_types::TyCtxt, def: gossamer_resolve::DefId) -> bool {
    tcx.def_name(def)
        .is_some_and(|name| matches!(name, "Set" | "BTreeSet"))
}

/// Child-word entries naming every counted word of the by-value aggregate `ty`
/// laid out from `base_word`, for the in-place sharing walk.
pub(super) fn send_layout_entries(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    base_word: i64,
    depth: u32,
    out: &mut Vec<i64>,
) {
    use gossamer_abi::rc::{
        RC_CHILD_KIND_SHIFT, RC_CHILD_MAP, RC_CHILD_RC, RC_CHILD_SET, RC_CHILD_VEC,
    };
    use gossamer_types::TyKind;
    if depth > 16 {
        return;
    }
    let field_tys: Vec<gossamer_types::Ty> = match tcx.kind_of(ty) {
        TyKind::Tuple(elems) => elems.clone(),
        TyKind::Array { elem, len } => vec![*elem; len.to_usize()],
        TyKind::Adt { def, substs } if def.local < u32::MAX - 16 => {
            match tcx.adt_field_tys(*def, substs) {
                Some(fields) => fields.to_vec(),
                None => return,
            }
        }
        _ => return,
    };
    let mut word = base_word;
    for fty in field_tys {
        let fwords = i64::from(tcx.slot_bytes(fty).max(8) / 8);
        let entry = |kind: i64, at: i64| (kind << RC_CHILD_KIND_SHIFT) | at;
        match tcx.kind_of(fty).clone() {
            TyKind::Vec(_) | TyKind::Slice(_) => out.push(entry(RC_CHILD_VEC, word)),
            TyKind::HashMap { .. } => out.push(entry(RC_CHILD_MAP, word)),
            TyKind::Adt { def, .. } if is_set_def(tcx, def) => {
                out.push(entry(RC_CHILD_SET, word));
            }
            TyKind::Adt { .. } if handle_container(tcx, fty) == Some(HandleContainer::Deque) => {
                out.push(entry(gossamer_abi::rc::RC_CHILD_DEQUE, word));
            }
            TyKind::Adt { .. } if handle_container(tcx, fty) == Some(HandleContainer::Heap) => {
                out.push(entry(gossamer_abi::rc::RC_CHILD_HEAP, word));
            }
            TyKind::Iterator(_) => {
                if let Some(kind) = lazy_iter_child_kind(tcx, fty) {
                    out.push(entry(kind, word));
                }
            }
            // An `Option` / `Result` holds its payload in the word after the
            // discriminant; a `None` payload word is zero, which the walk
            // skips, and a scalar payload is never a counted kind.
            TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => {
                let payload_kind = substs.types().iter().find_map(|t| match tcx.kind_of(*t) {
                    _ if tcx.is_counted_node(*t) => Some(RC_CHILD_RC),
                    TyKind::String => Some(RC_CHILD_RC),
                    TyKind::Vec(_) | TyKind::Slice(_) => Some(RC_CHILD_VEC),
                    _ => None,
                });
                let uniform = substs.types().iter().all(|t| {
                    tcx.is_counted_node(*t)
                        || matches!(
                            tcx.kind_of(*t),
                            TyKind::String | TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Unit
                        )
                });
                if let Some(kind) = payload_kind
                    && uniform
                    && substs
                        .types()
                        .iter()
                        .filter(|t| !matches!(tcx.kind_of(**t), TyKind::Unit))
                        .all(|t| {
                            let same = match tcx.kind_of(*t) {
                                _ if tcx.is_counted_node(*t) => RC_CHILD_RC,
                                TyKind::String => RC_CHILD_RC,
                                _ => RC_CHILD_VEC,
                            };
                            same == kind
                        })
                {
                    out.push(entry(kind, word + 1));
                }
            }
            _ if tcx.is_rc_managed(fty) => out.push(entry(RC_CHILD_RC, word)),
            TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Adt { .. } => {
                send_layout_entries(tcx, fty, word, depth + 1, out);
            }
            _ => {}
        }
        word += fwords;
    }
}

pub(crate) fn record_channel_elem_kind(body: &mut Body, tcx: &mut gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;

    let kind_of = |ty: gossamer_types::Ty| -> Option<i64> {
        let mut cur = ty;
        loop {
            match tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::String => return Some(1),
                TyKind::Vec(_) | TyKind::Slice(_) => return Some(2),
                // A struct or tuple travels as one counted node whose own
                // teardown reaches its fields.
                TyKind::Adt { def, .. } if def.local < u32::MAX - 16 => {
                    return tcx.struct_field_tys(*def).map(|_| 3);
                }
                TyKind::Tuple(_) => return Some(3),
                _ => return None,
            }
        }
    };

    // One character per 8-byte slot of an aggregate element, saying what that
    // slot owns. The channel carries a heap copy of the aggregate, so this is
    // what its teardown walks to give a value nobody received back.
    fn slot_desc(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty, out: &mut String) -> bool {
        use gossamer_types::TyKind;
        match tcx.kind_of(ty) {
            TyKind::Ref { inner, .. } => slot_desc(tcx, *inner, out),
            TyKind::String => {
                out.push('S');
                true
            }
            TyKind::Vec(_) | TyKind::Slice(_) => {
                out.push('V');
                true
            }
            TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_) => {
                out.push('s');
                true
            }
            TyKind::Tuple(items) => {
                let items = items.clone();
                !items.is_empty() && items.iter().all(|item| slot_desc(tcx, *item, out))
            }
            TyKind::Array { elem, len } => {
                let elem = *elem;
                let gossamer_types::ArrayLen::Concrete(len) = *len else {
                    return false;
                };
                len > 0 && (0..len).all(|_| slot_desc(tcx, elem, out))
            }
            TyKind::Adt { def, substs } if def.local < u32::MAX - 16 => {
                let fields = tcx
                    .adt_field_tys(*def, substs)
                    .map(<[gossamer_types::Ty]>::to_vec);
                match fields {
                    Some(fields) if !fields.is_empty() => {
                        fields.iter().all(|f| slot_desc(tcx, *f, out))
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }
    let descriptor = |ty: gossamer_types::Ty| -> Option<String> {
        let mut out = String::new();
        // A descriptor earns its place only where a slot owns storage; an
        // all-scalar aggregate has nothing to give back.
        (slot_desc(tcx, ty, &mut out) && out.bytes().any(|b| b != b's')).then_some(out)
    };

    let mut sites: Vec<(usize, Local, Local, i64, Option<String>)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } = &block.terminator
        else {
            continue;
        };
        if !matches!(name.as_str(), "gos_rt_chan_send" | "gos_rt_chan_try_send") {
            continue;
        }
        let (Some(Operand::Copy(chan)), Some(Operand::Copy(val))) = (args.first(), args.get(1))
        else {
            continue;
        };
        if !chan.projection.is_empty() || (val.local.0 as usize) >= body.locals.len() {
            continue;
        }
        if !val.projection.is_empty() {
            continue;
        }
        let val_ty = body.locals[val.local.0 as usize].ty;
        let kind = kind_of(val_ty);
        // A value boxed under its structural meta is a counted copy that owns
        // its children, so releasing the box is the whole give-back.
        let boxed_with_children = tcx
            .rc_meta(&format!("gos_rc_meta_boxaggr_{}", val_ty.as_u32()))
            .is_some();
        let desc = if kind == Some(3) && !boxed_with_children {
            descriptor(val_ty)
        } else {
            None
        };
        sites.push((bi, chan.local, val.local, kind.unwrap_or(0), desc));
    }
    if sites.is_empty() {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    for (bi, chan, val, kind, desc) in sites {
        // The value reaches the receiving goroutine, so everything it counts
        // switches to atomic reference counting before it is enqueued.
        let val_ty = body.locals[val.0 as usize].ty;
        if let Some((name, meta)) = send_mark_shared_call(tcx, val_ty) {
            let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(crate::ir::LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            if let Some(block) = body.blocks.get_mut(bi) {
                let span = block.span;
                let mut args = vec![Operand::Copy(Place::local(val))];
                if let Some(meta) = meta {
                    args.push(Operand::Const(ConstValue::Str(meta)));
                }
                block.stmts.push(Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(sink),
                        rvalue: Rvalue::CallIntrinsic { name, args },
                    },
                    span,
                    inlined: None,
                });
            }
        }
        if kind == 0 {
            continue;
        }
        let record = |name: &'static str, arg: Operand, body: &mut Body| {
            let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(crate::ir::LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            let Some(block) = body.blocks.get_mut(bi) else {
                return;
            };
            let span = block.span;
            block.stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(sink),
                    rvalue: Rvalue::CallIntrinsic {
                        name,
                        args: vec![Operand::Copy(Place::local(chan)), arg],
                    },
                },
                span,
                inlined: None,
            });
        };
        record(
            "gos_rt_chan_set_elem_kind",
            Operand::Const(ConstValue::Int(i128::from(kind))),
            body,
        );
        if let Some(desc) = desc {
            record(
                "gos_rt_chan_set_elem_desc",
                Operand::Const(ConstValue::Str(desc)),
                body,
            );
        }
    }
}

/// Gives back the `String` / `Vec` payload of every carrier local the frame
/// owns, on every path.
///
/// A carrier minted by a call, copied from another owned carrier, or built by
/// `gos_rt_result_new` holds the payload's one share. That share leaves the
/// local through a consuming mention - an extraction (`if let`, `unwrap`,
/// `?`), a call that takes the carrier, a copy into another carrier or into the
/// return slot - and stays through a borrowing one: an arm query (`is_some`,
/// `is_ok`) or a rendering. So the local releases its payload before each
/// redefinition and at every return, and is emptied to `None` right after a
/// consuming mention; the release on a `None` arm is a no-op, which is what
/// makes the placement independent of the path taken.
///
/// A runtime call that consumes a carrier the local outlives is handed a share
/// of its own first. A local whose uses the walk cannot classify, or whose
/// other consuming mention is not its last use, is left alone, and so is every
/// carrier it exchanges values with.
pub(crate) fn own_carrier_payloads(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;

    // Entry points that read a carrier's discriminant and nothing else.
    fn queries_arm(name: &str) -> bool {
        matches!(
            name,
            "gos_rt_result_is_ok"
                | "gos_rt_result_is_err"
                | "gos_rt_result_disc"
                | "gos_rt_option_is_some"
                | "gos_rt_option_is_none"
        )
    }
    // Entry points that render their arguments and keep none of them.
    fn renders_args(name: &str) -> bool {
        matches!(
            name,
            "__concat" | "__debug" | "println" | "print" | "eprintln" | "eprint"
        )
    }

    // The `gos_rt_result_payload_release` kinds of a carrier's two arms,
    // `(ok, err)`: `1` for a `String`, `2` for a `Vec` / slice, `4` for a
    // counted node (an `errors::Error` cell, a payload-enum node, or a
    // callable's environment), `5` / `6` / `7` for a `Map` / `Set` / deque
    // (see [`table_payload_kind`]), `0` for an arm whose payload the helper
    // does not own. `None` when neither arm is one.
    let payload_kind = |ty: gossamer_types::Ty| -> Option<(i64, i64)> {
        let arm = |payload: Option<&gossamer_types::Ty>| match payload {
            Some(t) if tcx.is_counted_node(*t) => 4,
            Some(t) => match tcx.kind_of(*t) {
                TyKind::String => 1,
                TyKind::Vec(_) | TyKind::Slice(_) => 2,
                TyKind::DynError => 4,
                _ => table_payload_kind(tcx, *t).unwrap_or(0),
            },
            None => 0,
        };
        match tcx.kind_of(ty) {
            TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => {
                let tys = substs.types();
                // An arm holding a counted aggregate blob makes the carrier an
                // option holder, whose walk owns the blob and releases it
                // after every field copied out of it has taken its share.
                if tys.iter().take(2).any(|p| tcx.aggr_copy_meta(*p).is_some()) {
                    return None;
                }
                let kinds = (arm(tys.first()), arm(tys.get(1)));
                (kinds != (0, 0)).then_some(kinds)
            }
            _ => None,
        }
    };

    let n_locals = body.locals.len();
    let arity = body.arity as usize;
    // A runtime call answering a counted aggregate blob makes its destination
    // an option holder, which the holder walk owns the way it owns a carrier
    // whose arm type names the blob.
    let mut counted_answer = vec![false; n_locals];
    for block in &body.blocks {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < n_locals
            && answers_counted_blob(name, destination.local, body, tcx)
        {
            counted_answer[destination.local.0 as usize] = true;
        }
    }
    // A region carrier's counted payload lives in the arena that frees it
    // wholesale, but a table is always a heap allocation of its own, so the
    // carrier still owns a table arm.
    let kinds: Vec<Option<(i64, i64)>> = (0..n_locals)
        .map(|i| {
            if i == 0 || counted_answer[i] {
                None
            } else if body.locals[i].region {
                payload_kind(body.locals[i].ty).and_then(|(ok, err)| {
                    let table_only = |kind: i64| if is_table_kind(kind) { kind } else { 0 };
                    let kinds = (table_only(ok), table_only(err));
                    (kinds != (0, 0)).then_some(kinds)
                })
            } else {
                payload_kind(body.locals[i].ty)
            }
        })
        .collect();
    if kinds.iter().all(Option::is_none) {
        return;
    }
    let is_carrier = |local: usize| local < n_locals && kinds[local].is_some();
    // A table is not counted, so a carrier holding one either owns it outright
    // or not at all: it is owned only when a call answered it fresh, and it
    // leaves the frame only through a mention that moves the whole payload.
    // Every site that would share the payload instead withdraws the carrier.
    let holds_table = |local: usize| {
        local < n_locals
            && kinds[local].is_some_and(|(ok, err)| is_table_kind(ok) || is_table_kind(err))
    };
    // A by-value carrier parameter is the caller's value, lent for the call.
    // A mention that hands it on takes a share of its own for what it hands
    // over, and a parameter the body reassigns takes a share at entry and is
    // then owned the way any other carrier local is.
    let is_param = |local: usize| (1..=arity).contains(&local);
    let mut reassigned = vec![false; n_locals];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
                && (place.local.0 as usize) < n_locals
            {
                reassigned[place.local.0 as usize] = true;
            }
        }
        if let Terminator::Call { destination, .. } = &block.terminator
            && (destination.local.0 as usize) < n_locals
        {
            reassigned[destination.local.0 as usize] = true;
        }
    }
    let lent_param = |local: usize| is_param(local) && is_carrier(local) && !reassigned[local];

    /// Where a consuming mention sits, which decides where the emptying goes.
    #[derive(Clone, Copy)]
    enum Site {
        /// Statement `si` of block `bi`: emptied right after it.
        Stmt(usize, usize),
        /// The call ending block `bi`: emptied at the head of its target.
        Call(usize),
    }

    // Temporaries built only to be read by an ordering comparator: a key
    // wrapped in a one-element tuple for `gos_rt_desc_cmp`. The comparator
    // reads the words and keeps none of them, so a carrier placed in one is
    // lent rather than handed over.
    let mut comparator_temp = vec![true; n_locals];
    {
        let mut built = vec![false; n_locals];
        for block in &body.blocks {
            for stmt in &block.stmts {
                match &stmt.kind {
                    StatementKind::Assign {
                        place,
                        rvalue: Rvalue::Aggregate { .. },
                    } if place.projection.is_empty() && (place.local.0 as usize) < n_locals => {
                        built[place.local.0 as usize] = true;
                        for_each_rvalue_place(
                            match &stmt.kind {
                                StatementKind::Assign { rvalue, .. } => rvalue,
                                _ => unreachable!("matched an assignment above"),
                            },
                            &mut |p| {
                                if (p.local.0 as usize) < n_locals {
                                    comparator_temp[p.local.0 as usize] = false;
                                }
                            },
                        );
                    }
                    other => for_each_stmt_place(other, &mut |p| {
                        if (p.local.0 as usize) < n_locals {
                            comparator_temp[p.local.0 as usize] = false;
                        }
                    }),
                }
            }
            let reads_only = matches!(
                &block.terminator,
                Terminator::Call { callee: Operand::Const(ConstValue::Str(name)), .. }
                    if name == "gos_rt_desc_cmp"
            );
            match &block.terminator {
                Terminator::Call {
                    callee,
                    args,
                    destination,
                    ..
                } => {
                    if (destination.local.0 as usize) < n_locals {
                        comparator_temp[destination.local.0 as usize] = false;
                    }
                    for op in std::iter::once(callee).chain(args) {
                        if let Operand::Copy(p) = op
                            && (p.local.0 as usize) < n_locals
                            && !(reads_only && p.projection.is_empty())
                        {
                            comparator_temp[p.local.0 as usize] = false;
                        }
                    }
                }
                other => {
                    for local in 0..n_locals {
                        if crate::opt::term_mentions_local(other, Local(local as u32)) {
                            comparator_temp[local] = false;
                        }
                    }
                }
            }
        }
        for (local, built) in built.into_iter().enumerate() {
            comparator_temp[local] &= built;
        }
    }

    // Whether the by-value aggregate a local holds gives back the carrier
    // payloads its slots own - the field walk's reach.
    let owns_carrier_fields = |local: usize| {
        let ty = body.locals[local].ty;
        let by_value = match tcx.kind_of(ty) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            TyKind::Adt { def, .. } => {
                def.local < u32::MAX - 16 && !tcx.is_inline_enum_ty(ty) && !tcx.is_rc_managed(ty)
            }
            _ => false,
        };
        by_value
            && aggregate_rc_field_paths(tcx, ty)
                .iter()
                .any(|(_, kind)| matches!(kind, FieldRcKind::Carrier { .. }))
    };
    // Whether a box of a carrier of type `ty` owns the carrier's payload: only
    // the structural meta names the payload word as a child.
    let box_owns_payload = |ty: gossamer_types::Ty| {
        tcx.rc_meta(&format!("gos_rc_meta_boxaggr_{}", ty.as_u32()))
            .is_some()
    };
    // Whether a `gos_rt_result_new` payload operand is a carrier the backend
    // copies into a counted box, which takes a share of its payload.
    let boxes_with_share = |op: &Operand| {
        let Operand::Copy(p) = op else {
            return false;
        };
        if !p.projection.is_empty() || p.local.0 as usize >= n_locals {
            return false;
        }
        kinds[p.local.0 as usize].is_some() && box_owns_payload(body.locals[p.local.0 as usize].ty)
    };
    // Carriers read out of a box, which keeps its own share: each takes one
    // of its own right after the read.
    let mut views: Vec<(usize, Site)> = Vec::new();
    let aliases = bare_copies(body);
    // `(carrier, block, stmt)`: an aggregate built at that statement took a
    // share of the carrier's payload.
    let mut aggregate_shares: Vec<(usize, usize, usize)> = Vec::new();
    let mut withdrawn = vec![false; n_locals];
    let mut copies: Vec<(usize, usize)> = Vec::new();
    let mut consumes: Vec<(usize, Site)> = Vec::new();
    // Mentions that hand a lent parameter on, each taking a share first.
    let mut escapes: Vec<(usize, Site)> = Vec::new();
    // Redefinitions: before statement `si` of block `bi`, or before the call
    // ending block `bi` (`None`).
    let mut redefinitions: Vec<(usize, usize, Option<usize>)> = Vec::new();
    let mut predecessors = vec![0u32; body.blocks.len()];

    let mentions_of = |op: &Operand, out: &mut Vec<usize>| {
        if let Operand::Copy(p) = op {
            out.push(p.local.0 as usize);
        }
    };

    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            // The give-back of an arm a call left unheld belongs to that call's
            // mention, which the terminator walk classifies.
            if let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { args, .. },
                ..
            } = &stmt.kind
                && let Some(Operand::Copy(p)) = args.first()
                && gives_back_payload(stmt, p.local)
            {
                continue;
            }
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                for_each_stmt_place(&stmt.kind, &mut |p| {
                    if (p.local.0 as usize) < n_locals {
                        withdrawn[p.local.0 as usize] = true;
                    }
                });
                continue;
            };
            let dest = place.local.0 as usize;
            if is_carrier(dest) {
                if place.projection.is_empty() {
                    let owned_def = match rvalue {
                        // A carrier copied out of an aggregate's field leaves
                        // the field holding its share, so the binding takes
                        // one of its own and gives it back like any owner.
                        Rvalue::Use(Operand::Copy(src))
                            if !src.projection.is_empty()
                                && (src.local.0 as usize) < n_locals
                                && src
                                    .projection
                                    .iter()
                                    .all(|p| matches!(p, crate::ir::Projection::Field(_))) =>
                        {
                            views.push((dest, Site::Stmt(bi, si)));
                            true
                        }
                        Rvalue::Use(Operand::Copy(src)) => {
                            src.projection.is_empty()
                                && is_carrier(src.local.0 as usize)
                                && kinds[src.local.0 as usize] == kinds[dest]
                        }
                        Rvalue::CallIntrinsic { name, .. } => {
                            *name == "gos_rt_result_new" || reads_boxed_carrier(name)
                        }
                        _ => false,
                    };
                    if let Rvalue::CallIntrinsic { name, .. } = rvalue
                        && reads_boxed_carrier(name)
                    {
                        if box_owns_payload(body.locals[dest].ty) {
                            views.push((dest, Site::Stmt(bi, si)));
                        } else {
                            withdrawn[dest] = true;
                        }
                    }
                    let bare_copy = matches!(rvalue,
                        Rvalue::Use(Operand::Copy(src)) if src.projection.is_empty());
                    if owned_def && (bare_copy || !holds_table(dest)) {
                        redefinitions.push((dest, bi, Some(si)));
                    } else {
                        withdrawn[dest] = true;
                    }
                } else {
                    withdrawn[dest] = true;
                }
            }
            match rvalue {
                Rvalue::Use(Operand::Copy(src))
                    if src.projection.is_empty() && is_carrier(src.local.0 as usize) =>
                {
                    let s = src.local.0 as usize;
                    if lent_param(s) {
                        escapes.push((s, Site::Stmt(bi, si)));
                    } else {
                        if place.projection.is_empty()
                            && is_carrier(dest)
                            && kinds[s] == kinds[dest]
                        {
                            copies.push((dest, s));
                        }
                        consumes.push((s, Site::Stmt(bi, si)));
                    }
                }
                Rvalue::CallIntrinsic { name, args } => {
                    let mut locals = Vec::new();
                    for (idx, arg) in args.iter().enumerate() {
                        if idx == 0 && queries_arm(name) {
                            continue;
                        }
                        // A carrier payload is copied into a box holding its
                        // own share, so the operand keeps the one it has.
                        if idx == 1 && *name == "gos_rt_result_new" && boxes_with_share(arg) {
                            continue;
                        }
                        // The release pass takes no share out of the extraction
                        // of an aliased carrier or of a parameter, so the carrier
                        // keeps its own.
                        // A table read out of its carrier is always handed
                        // over whole: the binding owns it (see
                        // `insert_drops_at_returns`), so the carrier must not.
                        let moves_table = *name == "gos_rt_result_payload"
                            && place.projection.is_empty()
                            && table_payload_kind(tcx, body.locals[dest].ty).is_some();
                        if idx == 0
                            && *name == "gos_rt_result_payload"
                            && !moves_table
                            && matches!(arg, Operand::Copy(p)
                                if p.projection.is_empty()
                                    && (p.local.0 as usize) < n_locals
                                    && (aliases.sourced[p.local.0 as usize]
                                        || aliases.target[p.local.0 as usize]
                                        || is_param(p.local.0 as usize)))
                        {
                            continue;
                        }
                        // Any other runtime entry could keep or free the table
                        // a carrier holds, so the carrier is left alone.
                        if let Operand::Copy(p) = arg
                            && p.projection.is_empty()
                            && holds_table(p.local.0 as usize)
                            && !(idx == 0
                                && (*name == "gos_rt_result_payload" || queries_arm(name)))
                        {
                            withdrawn[p.local.0 as usize] = true;
                            continue;
                        }
                        mentions_of(arg, &mut locals);
                    }
                    for l in locals.into_iter().filter(|l| is_carrier(*l)) {
                        if lent_param(l) {
                            escapes.push((l, Site::Stmt(bi, si)));
                        } else {
                            consumes.push((l, Site::Stmt(bi, si)));
                        }
                    }
                    for arg in args {
                        if let Operand::Copy(p) = arg
                            && !p.projection.is_empty()
                            && (p.local.0 as usize) < n_locals
                        {
                            withdrawn[p.local.0 as usize] = true;
                        }
                    }
                }
                Rvalue::Aggregate { .. }
                    if place.projection.is_empty() && comparator_temp[dest.min(n_locals - 1)] => {}
                // A tuple, array, or by-value struct takes a share of each
                // carrier slot it is built from, and gives it back at its own
                // death, so the operand is lent and keeps its share.
                Rvalue::Aggregate { operands, .. }
                    if place.projection.is_empty()
                        && dest < n_locals
                        && owns_carrier_fields(dest) =>
                {
                    for op in operands {
                        match op {
                            // An aggregate's carrier field owns the `Ok` /
                            // `Some` arm only.
                            Operand::Copy(p)
                                if p.projection.is_empty()
                                    && kinds
                                        .get(p.local.0 as usize)
                                        .copied()
                                        .flatten()
                                        .is_some_and(|(ok, err)| ok != 0 && err == 0) =>
                            {
                                aggregate_shares.push((p.local.0 as usize, bi, si));
                            }
                            Operand::Copy(p) if (p.local.0 as usize) < n_locals => {
                                withdrawn[p.local.0 as usize] = true;
                            }
                            _ => {}
                        }
                    }
                }
                _ => for_each_rvalue_place(rvalue, &mut |p| {
                    if (p.local.0 as usize) < n_locals {
                        withdrawn[p.local.0 as usize] = true;
                    }
                }),
            }
        }
        match &block.terminator {
            Terminator::Call {
                callee,
                args,
                destination,
                target,
            } => {
                let name = match callee {
                    Operand::Const(ConstValue::Str(name)) => name.as_str(),
                    _ => "",
                };
                if let Operand::Copy(p) = callee
                    && (p.local.0 as usize) < n_locals
                {
                    withdrawn[p.local.0 as usize] = true;
                }
                let dest = destination.local.0 as usize;
                let gossamer_callee = match callee {
                    Operand::FnRef { .. } | Operand::Copy(_) => true,
                    Operand::Const(ConstValue::Str(name)) => {
                        !name.starts_with("gos_rt_") && name != "gos_load" && name != "gos_store"
                    }
                    Operand::Const(_) => false,
                };
                let mut consumed_here = Vec::new();
                for (idx, arg) in args.iter().enumerate() {
                    let Operand::Copy(p) = arg else {
                        continue;
                    };
                    let l = p.local.0 as usize;
                    if l >= n_locals {
                        continue;
                    }
                    if !p.projection.is_empty() {
                        withdrawn[l] = true;
                        continue;
                    }
                    // A vec that holds carriers owns their payloads, so its push
                    // takes a share of its own and the argument keeps its one.
                    // A map keeps a two-word value in a box of its own, copied
                    // at the call, and a `get_or` default is read in place, so
                    // the argument keeps its share either way.
                    let map_copies_value = name.starts_with("gos_rt_map_get_or")
                        || ((name.starts_with("gos_rt_map_insert")
                            || name.starts_with("gos_rt_map_or_insert"))
                            && {
                                let ty = body.locals[l].ty;
                                tcx.rc_meta(&format!("gos_rc_meta_boxaggr_{}", ty.as_u32()))
                                    .is_some()
                                    || tcx.aggr_copy_meta(ty).is_some()
                            });
                    // A table carrier is lent to a callee that only reads it,
                    // handed over by an unwrap that answers the payload whole,
                    // and withdrawn from any other runtime entry, which could
                    // keep or free the table.
                    // A push onto a heap vec clones the element's table in,
                    // so the carrier keeps the one it holds; a region vec
                    // owns no children and takes nothing of its own.
                    let push_copies_table = name.starts_with("gos_rt_vec_push")
                        && idx == 1
                        && matches!(args.first(), Some(Operand::Copy(v))
                            if v.projection.is_empty()
                                && (v.local.0 as usize) < n_locals
                                && !body.locals[v.local.0 as usize].region);
                    if holds_table(l)
                        && !gossamer_callee
                        && !renders_args(name)
                        && !(idx == 0 && queries_arm(name))
                        && !reads_table_carrier(name)
                        && !push_copies_table
                    {
                        if (moves_table_carrier(name) || passes_table_through(name)) && idx == 0 {
                            consumed_here.push(l);
                        } else {
                            withdrawn[l] = true;
                        }
                        continue;
                    }
                    // A boxed-carrier reader answers words the box still owns
                    // and takes its own share of them, so the fallback it may
                    // answer instead is lent the same way. A Gossamer callee
                    // takes its by-value parameters as borrows it cannot
                    // outlive, so a carrier handed to one stays this frame's.
                    if !is_carrier(l)
                        || reads_table_carrier(name)
                        || renders_args(name)
                        || (idx == 0 && queries_arm(name))
                        || name.starts_with("gos_rt_vec_push")
                        || map_copies_value
                        || reads_boxed_carrier(name)
                        || gossamer_callee
                    {
                        continue;
                    }
                    consumed_here.push(l);
                }
                for l in consumed_here {
                    match target {
                        _ if lent_param(l) => escapes.push((l, Site::Call(bi))),
                        Some(_) => consumes.push((l, Site::Call(bi))),
                        None => withdrawn[l] = true,
                    }
                }
                if is_carrier(dest)
                    && destination.projection.is_empty()
                    && target.is_some()
                    && reads_boxed_carrier(name)
                {
                    if box_owns_payload(body.locals[dest].ty) {
                        views.push((dest, Site::Call(bi)));
                    } else {
                        withdrawn[dest] = true;
                    }
                }
                if is_carrier(dest) {
                    // A carrier read out of a container slot borrows the
                    // payload the container still owns, and a table carrier
                    // is owned only when the call answered its table fresh.
                    if destination.projection.is_empty()
                        && target.is_some()
                        && !answers_borrowed_element(name)
                        && !returns_borrowed_pointer(name)
                        && (!holds_table(dest)
                            || answers_owned_table_carrier(callee)
                            || passes_table_through(name))
                    {
                        // The answer holds the argument's table, so the two
                        // share whatever the walk decides for either.
                        if holds_table(dest)
                            && passes_table_through(name)
                            && let Some(Operand::Copy(src)) = args.first()
                            && src.projection.is_empty()
                            && is_carrier(src.local.0 as usize)
                        {
                            copies.push((dest, src.local.0 as usize));
                        }
                        redefinitions.push((dest, bi, None));
                    } else {
                        withdrawn[dest] = true;
                    }
                }
            }
            Terminator::SwitchInt { discriminant, .. } => {
                if let Operand::Copy(p) = discriminant
                    && (p.local.0 as usize) < n_locals
                {
                    withdrawn[p.local.0 as usize] = true;
                }
            }
            Terminator::Assert { cond, msg, .. } => {
                for op in std::iter::once(cond).chain(msg.operands()) {
                    if let Operand::Copy(p) = op
                        && (p.local.0 as usize) < n_locals
                    {
                        withdrawn[p.local.0 as usize] = true;
                    }
                }
            }
            Terminator::Drop { place, .. } => {
                if (place.local.0 as usize) < n_locals {
                    withdrawn[place.local.0 as usize] = true;
                }
            }
            Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::Resume
            | Terminator::Unreachable
            | Terminator::Panic { .. } => {}
        }
        for succ in successors_of(&block.terminator) {
            if let Some(count) = predecessors.get_mut(succ) {
                *count += 1;
            }
        }
    }

    // A lent parameter is the caller's to release, so this frame never owns it.
    for (local, slot) in withdrawn.iter_mut().enumerate() {
        if lent_param(local) {
            *slot = true;
        }
    }
    // A redefinition that also reads the local hands the old value to the
    // call or rvalue defining the new one, so the release before it would free
    // what that reader takes.
    for &(local, bi, si) in &redefinitions {
        let block = &body.blocks[bi];
        let reads_itself = match si {
            Some(si) => match &block.stmts[si].kind {
                StatementKind::Assign { rvalue, .. } => {
                    crate::opt::rvalue_mentions_local(rvalue, Local(local as u32))
                }
                _ => true,
            },
            None => match &block.terminator {
                Terminator::Call { callee, args, .. } => std::iter::once(callee)
                    .chain(args)
                    .any(|op| matches!(op, Operand::Copy(p) if p.local.0 as usize == local)),
                _ => true,
            },
        };
        if reads_itself {
            withdrawn[local] = true;
        }
    }
    // A consuming mention empties the local, which is sound only when nothing
    // reads it afterwards, and a call's emptying needs a target only that
    // call reaches.
    let mut kept_consumes = Vec::with_capacity(consumes.len());
    // `(carrier, block, arm)`: the call ending the block takes a share of the
    // carrier before it runs - of the one arm named, or of both.
    let mut handed_shares: Vec<(usize, usize, Option<usize>)> = Vec::new();
    for &(local, site) in &consumes {
        let last_use = match site {
            Site::Stmt(bi, si) => copy_is_last_use(body, (bi, si), Local(local as u32)),
            Site::Call(bi) => {
                let single_entry = match &body.blocks[bi].terminator {
                    Terminator::Call {
                        target: Some(t), ..
                    } => predecessors.get(t.0 as usize).copied() == Some(1),
                    _ => false,
                };
                let read_later = !call_is_last_read(body, bi, Local(local as u32));
                // A runtime call the local outlives is handed a share of its
                // own, so the local keeps the one it holds for its later
                // readers. A call that leaves one receiver arm unheld is
                // handed only the arm its answer keeps, since nothing gives
                // the other arm back while the local still holds it.
                if read_later
                    && let Terminator::Call { callee, args, .. } = &body.blocks[bi].terminator
                {
                    let name = match callee {
                        Operand::Const(ConstValue::Str(name)) => name.as_str(),
                        _ => "",
                    };
                    let answered_arm = discarded_receiver_arm(name).and_then(|(arm, recv_at)| {
                        matches!(args.get(recv_at), Some(Operand::Copy(p))
                            if p.projection.is_empty() && p.local.0 as usize == local)
                        .then_some(1 - arm)
                    });
                    handed_shares.push((local, bi, answered_arm));
                    continue;
                }
                single_entry && !read_later
            }
        };
        if last_use {
            kept_consumes.push((local, site));
            continue;
        }
        // A copy into another carrier binding that the source outlives leaves
        // both holding the value, so the copy takes a share of its own and
        // neither is emptied.
        let shared_copy = match site {
            Site::Stmt(bi, si) => match &body.blocks[bi].stmts[si].kind {
                StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                } if place.projection.is_empty()
                    && src.projection.is_empty()
                    && src.local.0 as usize == local
                    && place.local != Local::RETURN
                    && copies.contains(&(place.local.0 as usize, local)) =>
                {
                    Some(place.local.0 as usize)
                }
                _ => None,
            },
            Site::Call(_) => None,
        };
        match shared_copy {
            Some(dest) if holds_table(local) => {
                withdrawn[local] = true;
                withdrawn[dest] = true;
            }
            Some(dest) => {
                views.push((dest, site));
                copies.retain(|&pair| pair != (dest, local));
            }
            None => {
                withdrawn[local] = true;
                kept_consumes.push((local, site));
            }
        }
    }
    consumes = kept_consumes;
    // A table cannot be shared, so a table carrier on any site that takes a
    // share of its payload is left to whatever holds the table now.
    for &(local, _) in views.iter().chain(&escapes) {
        if holds_table(local) {
            withdrawn[local] = true;
        }
    }
    for &(local, _, _) in &aggregate_shares {
        if holds_table(local) {
            withdrawn[local] = true;
        }
    }
    for &(local, _, _) in &handed_shares {
        if holds_table(local) {
            withdrawn[local] = true;
        }
    }
    for local in (0..n_locals).filter(|&l| is_param(l) && holds_table(l)) {
        withdrawn[local] = true;
    }
    views.retain(|&(local, _)| !holds_table(local));
    escapes.retain(|&(local, _)| !holds_table(local));
    aggregate_shares.retain(|&(local, _, _)| !holds_table(local));
    handed_shares.retain(|&(local, _, _)| !holds_table(local));
    // Two carriers that exchange a value share its fate.
    loop {
        let mut changed = false;
        for &(dest, src) in &copies {
            if withdrawn[dest] != withdrawn[src] {
                withdrawn[dest] = true;
                withdrawn[src] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let owner = |local: usize| is_carrier(local) && !withdrawn[local];
    if (0..n_locals).all(|l| !owner(l))
        && aggregate_shares.is_empty()
        && views.is_empty()
        && escapes.is_empty()
        && handed_shares.is_empty()
    {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let new_local = |body: &mut Body| {
        let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(crate::ir::LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        sink
    };
    let release_stmt = |sink: Local, local: usize, span| Statement {
        kind: StatementKind::Assign {
            place: Place::local(sink),
            rvalue: Rvalue::CallIntrinsic {
                name: "gos_rt_result_payload_release",
                args: vec![
                    Operand::Copy(Place::local(Local(local as u32))),
                    Operand::Const(ConstValue::Int(i128::from(kinds[local].map_or(0, |k| k.0)))),
                    Operand::Const(ConstValue::Int(i128::from(kinds[local].map_or(0, |k| k.1)))),
                ],
            },
        },
        span,
        inlined: None,
    };
    let retain_stmt = |sink: Local, local: usize, span| Statement {
        kind: StatementKind::Assign {
            place: Place::local(sink),
            rvalue: Rvalue::CallIntrinsic {
                name: "gos_rt_result_payload_retain",
                args: vec![
                    Operand::Copy(Place::local(Local(local as u32))),
                    Operand::Const(ConstValue::Int(i128::from(kinds[local].map_or(0, |k| k.0)))),
                    Operand::Const(ConstValue::Int(i128::from(kinds[local].map_or(0, |k| k.1)))),
                ],
            },
        },
        span,
        inlined: None,
    };
    let empty_stmt = |local: usize, span| Statement {
        kind: StatementKind::Assign {
            place: Place::local(Local(local as u32)),
            rvalue: Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Const(ConstValue::Int(1)),
                    Operand::Const(ConstValue::Int(0)),
                ],
            },
        },
        span,
        inlined: None,
    };

    // `(block, position, rank, statement)`, applied from the highest position
    // down so each insertion leaves the positions below it valid. Statements
    // sharing a position run in rank order: an entry emptying or entry share
    // first (`ENTRY`), then the emptying of a value a mention just handed over
    // (`EMPTY`), then releases (`RELEASE`), which so read either a share the
    // local still holds or an empty arm, and shares taken last (`RETAIN`).
    const ENTRY: u8 = 0;
    const EMPTY: u8 = 1;
    const RELEASE: u8 = 2;
    const RETAIN: u8 = 3;
    let mut inserts: Vec<(usize, usize, u8, Statement)> = Vec::new();
    // The aggregate's field walk gives back both arms' payloads, so the share
    // it takes covers both arms too.
    for &(local, bi, si) in &aggregate_shares {
        let sink = new_local(body);
        let span = body.blocks[bi].span;
        inserts.push((bi, si + 1, RETAIN, retain_stmt(sink, local, span)));
    }
    for &(local, bi, si) in &redefinitions {
        if !owner(local) {
            continue;
        }
        let sink = new_local(body);
        let span = body.blocks[bi].span;
        let position = si.unwrap_or(body.blocks[bi].stmts.len());
        inserts.push((bi, position, RELEASE, release_stmt(sink, local, span)));
    }
    for &(local, site) in &consumes {
        if !owner(local) {
            continue;
        }
        match site {
            Site::Stmt(bi, si) => {
                let span = body.blocks[bi].span;
                inserts.push((bi, si + 1, EMPTY, empty_stmt(local, span)));
            }
            Site::Call(bi) => {
                if let Terminator::Call {
                    target: Some(t), ..
                } = &body.blocks[bi].terminator
                {
                    let target = t.0 as usize;
                    let span = body.blocks[target].span;
                    // The give-back of the arm the call left unheld reads the
                    // carrier first, so the emptying follows it.
                    let position = body.blocks[target]
                        .stmts
                        .iter()
                        .take_while(|stmt| gives_back_payload(stmt, Local(local as u32)))
                        .count();
                    inserts.push((target, position, EMPTY, empty_stmt(local, span)));
                }
            }
        }
    }
    // A view takes its share whether or not the walk owns it: an owner gives
    // the share back, and a local the walk leaves alone keeps the box's
    // payload alive rather than reading it after the box is gone.
    for &(local, site) in &views {
        let (bi, position) = match site {
            Site::Stmt(bi, si) => (bi, si + 1),
            Site::Call(bi) => match &body.blocks[bi].terminator {
                Terminator::Call {
                    target: Some(t), ..
                } => (t.0 as usize, 0),
                _ => continue,
            },
        };
        let sink = new_local(body);
        let span = body.blocks[bi].span;
        inserts.push((bi, position, RETAIN, retain_stmt(sink, local, span)));
    }
    // A lent parameter takes the share it hands on right before the mention
    // that hands it on, which leaves the parameter itself as it was.
    for &(local, site) in &escapes {
        let (bi, position) = match site {
            Site::Stmt(bi, si) => (bi, si),
            Site::Call(bi) => (bi, body.blocks[bi].stmts.len()),
        };
        let sink = new_local(body);
        let span = body.blocks[bi].span;
        inserts.push((bi, position, RETAIN, retain_stmt(sink, local, span)));
    }
    for &(local, bi, arm) in &handed_shares {
        if !owner(local) {
            continue;
        }
        let sink = new_local(body);
        let span = body.blocks[bi].span;
        let mut stmt = retain_stmt(sink, local, span);
        if let (
            Some(arm),
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { args, .. },
                ..
            },
        ) = (arm, &mut stmt.kind)
        {
            args[2 - arm] = Operand::Const(ConstValue::Int(0));
        }
        let position = body.blocks[bi].stmts.len();
        inserts.push((bi, position, RETAIN, stmt));
    }
    let returns: Vec<usize> = body
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| matches!(block.terminator, Terminator::Return))
        .map(|(bi, _)| bi)
        .collect();
    for local in (0..n_locals).filter(|l| owner(*l)) {
        for &bi in &returns {
            let sink = new_local(body);
            let span = body.blocks[bi].span;
            let position = body.blocks[bi].stmts.len();
            inserts.push((bi, position, RELEASE, release_stmt(sink, local, span)));
        }
        let span = body.blocks[0].span;
        // A reassigned parameter arrives holding the caller's payload, so it
        // takes a share of that at entry where any other local starts empty.
        if is_param(local) {
            let sink = new_local(body);
            inserts.push((0, 0, ENTRY, retain_stmt(sink, local, span)));
        } else {
            inserts.push((0, 0, ENTRY, empty_stmt(local, span)));
        }
    }
    inserts.sort_by_key(|a| (a.0, a.1, a.2));
    for (bi, position, _, stmt) in inserts.into_iter().rev() {
        let stmts = &mut body.blocks[bi].stmts;
        let position = position.min(stmts.len());
        stmts.insert(position, stmt);
    }
}
