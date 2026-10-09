//! Bare copies, last reads, and overwritten constructor values.

use super::*;

/// The locals bare whole-local copies (`dest = Copy(src)`) connect.
pub(super) struct BareCopies {
    /// Read whole by a copy.
    pub(super) sourced: Vec<bool>,
    /// Read whole by a copy into a binding. A copy into the return slot hands
    /// the share to the caller only on the path that returns, so a payload
    /// read out of a carrier is still this frame's on every other.
    pub(super) sourced_to_binding: Vec<bool>,
    /// Written whole by a copy. With `sourced` this flags an enum value that
    /// is aliased (copied to or from another binding): its by-value payload
    /// pointer is shared, so no extraction owns or releases it, since matching
    /// both aliases would free the one payload twice.
    pub(super) target: Vec<bool>,
}

pub(super) fn bare_copies(body: &Body) -> BareCopies {
    let n_locals = body.locals.len();
    let mut facts = BareCopies {
        sourced: vec![false; n_locals],
        sourced_to_binding: vec![false; n_locals],
        target: vec![false; n_locals],
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(p)),
            } = &stmt.kind
                && p.projection.is_empty()
                && (p.local.0 as usize) < n_locals
            {
                facts.sourced[p.local.0 as usize] = true;
                if place.local != Local::RETURN {
                    facts.sourced_to_binding[p.local.0 as usize] = true;
                }
                if place.projection.is_empty() && (place.local.0 as usize) < n_locals {
                    facts.target[place.local.0 as usize] = true;
                }
            }
        }
    }
    facts
}

/// Readers that answer the carrier a two-word payload was boxed as: the words
/// stay the box's, so the answer is a view of what the box owns.
/// The carrier arguments a combinator may answer unchanged, and whether its
/// answered `Ok` / `Some` payload is always one of theirs (`true`) or may be
/// one its closure built (`false`).
pub(super) fn passthrough_receivers(name: &str) -> Option<(&'static [usize], bool)> {
    Some(match name {
        "gos_rt_result_map_err"
        | "gos_rt_result_map_err_bare"
        | "gos_rt_result_to_opt_ok"
        | "gos_rt_result_ok_or"
        | "gos_rt_result_ok_or_else"
        | "gos_rt_option_filter" => (&[0], true),
        "gos_rt_option_or" => (&[0, 1], true),
        "gos_rt_result_or_else" | "gos_rt_option_or_else" => (&[0], false),
        _ => return None,
    })
}

pub(super) fn reads_boxed_carrier(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_result_payload_i128"
            | "gos_rt_option_unwrap_carrier"
            | "gos_rt_result_unwrap_carrier"
            | "gos_rt_result_unwrap_or_carrier"
    )
}

/// Whether `stmt` releases the payload `local` holds.
pub(super) fn gives_back_payload(stmt: &Statement, local: Local) -> bool {
    matches!(&stmt.kind, StatementKind::Assign {
        rvalue: Rvalue::CallIntrinsic { name, args },
        ..
    } if matches!(*name, "gos_rt_result_payload_release" | "gos_rt_result_ok_payload_release")
        && matches!(args.first(), Some(Operand::Copy(p))
            if p.projection.is_empty() && p.local == local))
}

/// Whether nothing reads `local` after the call ending block `bi`, on any path
/// up to wherever `local` is next defined, counting a give-back of its payload
/// as no read: releasing what the carrier holds hands nothing to a reader.
pub(super) fn call_is_last_read(body: &Body, bi: usize, local: Local) -> bool {
    let mut work = successors_of(&body.blocks[bi].terminator);
    let mut seen = vec![false; body.blocks.len()];
    while let Some(b) = work.pop() {
        if b >= body.blocks.len() || std::mem::replace(&mut seen[b], true) {
            continue;
        }
        let block = &body.blocks[b];
        let rest: Vec<Statement> = block
            .stmts
            .iter()
            .filter(|stmt| !gives_back_payload(stmt, local))
            .cloned()
            .collect();
        match scan_for_local(&rest, Some(&block.terminator), local) {
            LocalScan::Mentioned => return false,
            LocalScan::Redefined => {}
            LocalScan::Clear => work.extend(successors_of(&block.terminator)),
        }
    }
    true
}

/// Whether the call ending block `bi` holds the last mention of `local` on
/// every path out of it, up to wherever `local` is next defined.
/// Whether nothing reads `local` after statement `si` of block `bi`, on any
/// path, before it is written again.
pub(super) fn stmt_is_last_use(body: &Body, bi: usize, si: usize, local: Local) -> bool {
    let block = &body.blocks[bi];
    match scan_for_local(&block.stmts[si + 1..], Some(&block.terminator), local) {
        LocalScan::Mentioned => false,
        LocalScan::Redefined => true,
        LocalScan::Clear => terminator_is_last_use(body, bi, local),
    }
}

pub(super) fn terminator_is_last_use(body: &Body, bi: usize, local: Local) -> bool {
    let mut work = successors_of(&body.blocks[bi].terminator);
    let mut seen = vec![false; body.blocks.len()];
    while let Some(b) = work.pop() {
        if b >= body.blocks.len() || std::mem::replace(&mut seen[b], true) {
            continue;
        }
        let block = &body.blocks[b];
        match scan_for_local(&block.stmts, Some(&block.terminator), local) {
            LocalScan::Mentioned => return false,
            LocalScan::Redefined => {}
            LocalScan::Clear => work.extend(successors_of(&block.terminator)),
        }
    }
    true
}

/// Every place an rvalue reads.
pub(super) fn for_each_rvalue_place(rvalue: &Rvalue, f: &mut impl FnMut(&Place)) {
    let mut operand = |op: &Operand| {
        if let Operand::Copy(p) = op {
            f(p);
        }
    };
    match rvalue {
        Rvalue::Use(op)
        | Rvalue::UnaryOp { operand: op, .. }
        | Rvalue::Cast { operand: op, .. }
        | Rvalue::Repeat { value: op, .. } => operand(op),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            operand(lhs);
            operand(rhs);
        }
        Rvalue::Aggregate { operands, .. } | Rvalue::CallIntrinsic { args: operands, .. } => {
            for op in operands {
                operand(op);
            }
        }
        Rvalue::Len(place) | Rvalue::Ref { place, .. } => f(place),
        Rvalue::StaticLoad(_) => {}
    }
}

/// Every place a non-assignment statement reads or writes.
pub(super) fn for_each_stmt_place(kind: &StatementKind, f: &mut impl FnMut(&Place)) {
    let mut operand = |op: &Operand| {
        if let Operand::Copy(p) = op {
            f(p);
        }
    };
    match kind {
        StatementKind::Assign { place, rvalue } => {
            f(place);
            for_each_rvalue_place(rvalue, f);
        }
        StatementKind::StaticStore { value, .. } => operand(value),
        StatementKind::IterSource { source, .. } => operand(source),
        StatementKind::IterAdapter {
            closure_or_arg: Some(op),
            ..
        } => operand(op),
        _ => {}
    }
}

/// The receiver arm a `Result` entry point leaves nobody holding, with the
/// receiver's argument position: `0` for `Ok`, `1` for `Err`.
///
/// `map` and `and_then` hand the closure the `Ok` payload, `map_err` and
/// `or_else` the `Err` one. A call that answers the `Ok` payload or a fallback,
/// or turns the carrier into an `Option` of one arm, leaves the other arm's
/// payload unheld. The arm it does not name is the one its answer holds.
pub(super) fn discarded_receiver_arm(name: &str) -> Option<(usize, usize)> {
    Some(match name {
        "gos_rt_result_map" | "gos_rt_result_map_bare" | "gos_rt_result_and_then" => (0, 0),
        "gos_rt_result_map_i64" => (0, 1),
        "gos_rt_result_default" | "gos_rt_result_default_f64" => (1, 1),
        "gos_rt_result_to_opt_err" | "gos_rt_result_err" => (0, 0),
        "gos_rt_result_map_err"
        | "gos_rt_result_map_err_bare"
        | "gos_rt_result_or_else"
        | "gos_rt_result_unwrap_or"
        | "gos_rt_result_unwrap_or_str"
        | "gos_rt_result_unwrap_or_node"
        | "gos_rt_result_unwrap_or_vec"
        | "gos_rt_result_unwrap_or_map"
        | "gos_rt_result_unwrap_or_carrier"
        | "gos_rt_result_default_with"
        | "gos_rt_result_ok"
        | "gos_rt_result_to_opt_ok" => (1, 0),
        _ => return None,
    })
}

/// Emits the give-back for `carrier.map(f)`.
///
/// `map` hands the payload to the closure, which answers a value of its own (a
/// parameter it returns unchanged mints the caller's share), so the receiver's
/// payload has no holder left. A carrier never releases a `String` or `Vec`
/// payload of its own accord, so the release is emitted at the call. The
/// helper answers the arm: an `Err` / `None` payload word belongs to the value
/// the mapped carrier still carries.
pub(crate) fn release_mapped_payloads(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;

    // `arm` is 0 for the `Ok` / `Some` payload and 1 for the `Err` payload.
    let payload_kind = |ty: gossamer_types::Ty, arm: usize| -> Option<i64> {
        let mut cur = ty;
        loop {
            match tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Adt { def, substs }
                    if def.local == u32::MAX || def.local == u32::MAX - 1 =>
                {
                    return substs
                        .types()
                        .get(arm)
                        .and_then(|payload| counted_payload_kind(tcx, *payload))
                        .map(i64::from);
                }
                _ => return None,
            }
        }
    };

    let mut counted_answer_dest = vec![false; body.locals.len()];
    for block in &body.blocks {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < body.locals.len()
            && answers_counted_blob(name, destination.local, body, tcx)
        {
            counted_answer_dest[destination.local.0 as usize] = true;
        }
    }
    let mut sites: Vec<(usize, Local, i64, usize)> = Vec::new();
    let mut passthrough_shares: Vec<(usize, Local, i64)> = Vec::new();
    for (block_index, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            target: Some(target),
            ..
        } = &block.terminator
        else {
            continue;
        };
        // `map` and `and_then` answer an `Err` receiver as it is, so the
        // answer holds the receiver's error. A holder receiver keeps its own
        // share for its walk to give back, so the answer takes another.
        let passthrough_recv = match name.as_str() {
            "gos_rt_result_map" | "gos_rt_result_map_bare" | "gos_rt_result_and_then" => {
                args.first()
            }
            "gos_rt_result_map_i64" => args.get(1),
            _ => None,
        };
        if let Some(Operand::Copy(recv)) = passthrough_recv
            && recv.projection.is_empty()
            && (recv.local.0 as usize) < body.locals.len()
        {
            let recv_ty = body.locals[recv.local.0 as usize].ty;
            if (holds_counted_blob_arm(tcx, recv_ty) || counted_answer_dest[recv.local.0 as usize])
                && let Some(kind) = holder_err_kind(tcx, recv_ty)
            {
                passthrough_shares.push((block_index, recv.local, kind));
            }
        }
        let Some((arm, recv_at)) = discarded_receiver_arm(name) else {
            continue;
        };
        let Some(Operand::Copy(recv)) = args.get(recv_at) else {
            continue;
        };
        if !recv.projection.is_empty() || (recv.local.0 as usize) >= body.locals.len() {
            continue;
        }
        // An option holder's walk gives back its payloads itself.
        let recv_ty = body.locals[recv.local.0 as usize].ty;
        if holds_counted_blob_arm(tcx, recv_ty) || counted_answer_dest[recv.local.0 as usize] {
            continue;
        }
        // A receiver read again after the call still holds its payloads for
        // that reader, which gives them back on its own.
        if let Some(kind) = payload_kind(recv_ty, arm)
            && terminator_is_last_use(body, block_index, recv.local)
        {
            sites.push((target.0 as usize, recv.local, kind, arm));
        }
    }
    if sites.is_empty() && passthrough_shares.is_empty() {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    for (block_index, recv, kind) in passthrough_shares {
        let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(crate::ir::LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let block = &mut body.blocks[block_index];
        let span = block.span;
        block.stmts.push(Statement {
            kind: StatementKind::Assign {
                place: Place::local(sink),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_result_payload_retain",
                    args: vec![
                        Operand::Copy(Place::local(recv)),
                        Operand::Const(ConstValue::Int(0)),
                        Operand::Const(ConstValue::Int(i128::from(kind))),
                    ],
                },
            },
            span,
            inlined: None,
        });
    }
    for (target, recv, kind, arm) in sites {
        let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(crate::ir::LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let Some(block) = body.blocks.get_mut(target) else {
            continue;
        };
        let span = block.span;
        block.stmts.insert(
            0,
            Statement {
                kind: StatementKind::Assign {
                    place: Place::local(sink),
                    // Each helper acts on its own arm only: the other arm's
                    // payload is the one the mapped carrier still holds.
                    rvalue: if arm == 0 {
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_ok_payload_release",
                            args: vec![
                                Operand::Copy(Place::local(recv)),
                                Operand::Const(ConstValue::Int(i128::from(kind))),
                            ],
                        }
                    } else {
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_payload_release",
                            args: vec![
                                Operand::Copy(Place::local(recv)),
                                Operand::Const(ConstValue::Int(0)),
                                Operand::Const(ConstValue::Int(i128::from(kind))),
                            ],
                        }
                    },
                },
                span,
                inlined: None,
            },
        );
    }
}

/// Reclaims a constructed container that is overwritten before it is ever read.
///
/// `let mut vals: Vec<T> = #[]` followed by `vals = f(..)?` builds an empty
/// container and then replaces it. The replacement disqualifies the local from
/// the per-site reuse machinery, so the construction it replaced was reaching
/// no free at all - one buffer per iteration of whatever loop it sits in.
///
/// The free is emitted only where every path back from the overwrite reaches a
/// construction of that same local with no mention of it in between. A value no
/// one read cannot have been aliased or stored, so nothing else can be holding
/// it, and the construction on every incoming path is what makes the free
/// well-defined rather than a free of an uninitialised slot.
/// The reclamation helper for a container constructor, or `None` when the name
/// is not one. Shared with [`free_overwritten_ctor_values`] so the two agree on
/// what the frame owns.
pub(crate) fn container_ctor_free(name: &str) -> Option<&'static str> {
    match name {
        "gos_rt_map_new" | "gos_rt_map_new_with_capacity" | "Map::new" | "HashMap::new" => {
            Some("gos_rt_map_free")
        }
        "gos_rt_vec_new" | "gos_rt_vec_with_capacity" | "Vec::new" => Some("gos_rt_vec_free"),
        "gos_rt_set_new" | "gos_rt_btree_set_new" | "Set::new" => Some("gos_rt_set_free"),
        _ => None,
    }
}

/// Releases a constructed container whose only use is being built into an
/// aggregate that mints its own share of it.
///
/// `rows.push(Row { vals: v })` gives the `Row` a share of `v` and the
/// container another, so `v` holds three: its own, the aggregate's, and the
/// container's. The aggregate and the container each give theirs back; nothing
/// gives back the frame's, and a loop building one row per iteration keeps
/// every `v` it ever made.
///
/// Emitted only where the local is read exactly once - by that aggregate - so
/// the value is dead the moment the aggregate has taken its share.
pub(crate) fn free_overwritten_ctor_values(
    body: &mut Body,
    tcx: &gossamer_types::TyCtxt,
    ctor_free: &dyn Fn(&str) -> Option<&'static str>,
) {
    let n = body.locals.len();
    // A rebound owner gives back every value it replaces itself.
    let rebound = rebound_vec_owners(body, tcx);
    // Constructions, by local.
    let mut ctor_at: std::collections::HashMap<u32, &'static str> =
        std::collections::HashMap::new();
    let mut ctor_blocks: std::collections::HashSet<(usize, u32)> = std::collections::HashSet::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < n
            && !rebound[destination.local.0 as usize]
            && !matches!(
                tcx.kind_of(body.locals[destination.local.0 as usize].ty),
                gossamer_types::TyKind::Adt { def, .. } if def.local < u32::MAX - 64
            )
            && let Some(free) = ctor_free(name.as_str())
        {
            ctor_at.insert(destination.local.0, free);
            ctor_blocks.insert((bi, destination.local.0));
        }
    }
    if ctor_at.is_empty() {
        return;
    }

    let mentions =
        |op: &Operand, local: u32| -> bool { matches!(op, Operand::Copy(p) if p.local.0 == local) };
    let stmt_mentions = |stmt: &Statement, local: u32| -> bool {
        match &stmt.kind {
            StatementKind::Assign { place, rvalue } => {
                let read = match rvalue {
                    Rvalue::Use(op)
                    | Rvalue::UnaryOp { operand: op, .. }
                    | Rvalue::Cast { operand: op, .. }
                    | Rvalue::Repeat { value: op, .. } => mentions(op, local),
                    Rvalue::BinaryOp { lhs, rhs, .. } => {
                        mentions(lhs, local) || mentions(rhs, local)
                    }
                    Rvalue::Aggregate { operands, .. } => {
                        operands.iter().any(|op| mentions(op, local))
                    }
                    Rvalue::CallIntrinsic { args, .. } => args.iter().any(|op| mentions(op, local)),
                    Rvalue::Ref { place, .. } | Rvalue::Len(place) => place.local.0 == local,
                    Rvalue::StaticLoad(_) => false,
                };
                // A projected write reads the local to reach the field.
                read || (!place.projection.is_empty() && place.local.0 == local)
            }
            _ => false,
        }
    };
    let term_mentions = |t: &Terminator, local: u32| -> bool {
        match t {
            Terminator::Call { callee, args, .. } => {
                mentions(callee, local) || args.iter().any(|op| mentions(op, local))
            }
            Terminator::SwitchInt { discriminant, .. } => mentions(discriminant, local),
            Terminator::Assert { cond, msg, .. } => {
                mentions(cond, local) || msg.operands().any(|op| mentions(op, local))
            }
            Terminator::Drop { place, .. } => place.local.0 == local,
            _ => false,
        }
    };

    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); body.blocks.len()];
    for (bi, block) in body.blocks.iter().enumerate() {
        for succ in successors_of(&block.terminator) {
            if succ < preds.len() {
                preds[succ].push(bi);
            }
        }
    }

    // Overwrite sites: a whole-local assignment that does not read the local.
    let mut sites: Vec<(usize, usize, Local, &'static str)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, .. } = &stmt.kind else {
                continue;
            };
            if !place.projection.is_empty() {
                continue;
            }
            let local = place.local.0;
            let Some(&free) = ctor_at.get(&local) else {
                continue;
            };
            if stmt_mentions(stmt, local) {
                continue;
            }
            // Every path back from here must reach a construction of `local`
            // with no mention of it in between.
            let mut ok = true;
            let mut seen_blocks: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            // (block, index one past the last statement to examine)
            let mut work: Vec<(usize, usize)> = vec![(bi, si)];
            while let Some((wb, upto)) = work.pop() {
                let mut reached_ctor = false;
                for stmt in body.blocks[wb].stmts[..upto].iter().rev() {
                    if stmt_mentions(stmt, local) {
                        ok = false;
                        break;
                    }
                    if let StatementKind::Assign { place, .. } = &stmt.kind
                        && place.projection.is_empty()
                        && place.local.0 == local
                    {
                        // An earlier whole-local write that is not the
                        // construction: its value is what this site would free,
                        // and it is not known to be owned.
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    break;
                }
                if upto == body.blocks[wb].stmts.len()
                    && term_mentions(&body.blocks[wb].terminator, local)
                {
                    // The terminator reads it - unless it is the construction
                    // that defines it.
                    if ctor_blocks.contains(&(wb, local)) {
                        reached_ctor = true;
                    } else {
                        ok = false;
                        break;
                    }
                } else if ctor_blocks.contains(&(wb, local)) && upto == body.blocks[wb].stmts.len()
                {
                    reached_ctor = true;
                }
                if reached_ctor {
                    continue;
                }
                if wb == 0 || preds[wb].is_empty() {
                    // Entry reached with no construction on this path.
                    ok = false;
                    break;
                }
                for &pred in &preds[wb] {
                    if seen_blocks.insert(pred) {
                        work.push((pred, body.blocks[pred].stmts.len()));
                    }
                }
            }
            if ok {
                sites.push((bi, si, place.local, free));
            }
        }
    }
    if sites.is_empty() {
        return;
    }

    let unit_ty = body.locals[0].ty;
    sites.sort_by_key(|&(bi, si, _, _)| (bi, si));
    for (bi, si, local, free) in sites.into_iter().rev() {
        let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(crate::ir::LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let span = body.blocks[bi].stmts[si].span;
        body.blocks[bi].stmts.insert(
            si,
            Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest),
                    rvalue: Rvalue::CallIntrinsic {
                        name: free,
                        args: vec![Operand::Copy(Place::local(local))],
                    },
                },
                span,
                inlined: None,
            },
        );
    }
}

/// What a run of statements does with one local, read in order.
pub(super) enum LocalScan {
    /// The local is read (or written through) before any redefinition.
    Mentioned,
    /// The local is redefined whole before any read.
    Redefined,
    /// The local is neither read nor redefined.
    Clear,
}

pub(super) fn scan_for_local(
    stmts: &[Statement],
    terminator: Option<&Terminator>,
    local: Local,
) -> LocalScan {
    for stmt in stmts {
        if let StatementKind::Assign { place, rvalue } = &stmt.kind
            && place.projection.is_empty()
            && place.local == local
        {
            return if crate::opt::rvalue_mentions_local(rvalue, local) {
                LocalScan::Mentioned
            } else {
                LocalScan::Redefined
            };
        }
        if crate::opt::stmt_mentions_local(stmt, local) {
            return LocalScan::Mentioned;
        }
    }
    match terminator {
        None => LocalScan::Clear,
        Some(Terminator::Call {
            callee,
            args,
            destination,
            ..
        }) if destination.projection.is_empty() && destination.local == local => {
            let reads = std::iter::once(callee).chain(args).any(
                |op| matches!(op, Operand::Copy(p) if crate::opt::place_mentions_local(p, local)),
            );
            if reads {
                LocalScan::Mentioned
            } else {
                LocalScan::Redefined
            }
        }
        Some(Terminator::Drop { place, .. }) if crate::opt::place_mentions_local(place, local) => {
            LocalScan::Mentioned
        }
        Some(t) if crate::opt::term_mentions_local(t, local) => LocalScan::Mentioned,
        Some(_) => LocalScan::Clear,
    }
}

/// Whether the copy at statement `si` of block `bi` is the last mention of
/// `local` on every path, up to wherever `local` is next defined.
///
/// A move hands the allocation to the copy's destination, which is sound only
/// when nothing reads `local` afterwards and the copy cannot run again for the
/// same allocation: a path that returns to the copy without redefining `local`
/// would hand one allocation over on every turn of the loop.
pub(super) fn copy_is_last_use(body: &Body, (bi, si): (usize, usize), local: Local) -> bool {
    let block = &body.blocks[bi];
    let mut work = match scan_for_local(&block.stmts[si + 1..], Some(&block.terminator), local) {
        LocalScan::Mentioned => return false,
        LocalScan::Redefined => return true,
        LocalScan::Clear => successors_of(&block.terminator),
    };
    let mut seen = vec![false; body.blocks.len()];
    while let Some(b) = work.pop() {
        if b >= body.blocks.len() || std::mem::replace(&mut seen[b], true) {
            continue;
        }
        if b == bi {
            // Back at the copy's own block: only the statements ahead of the
            // copy stand between this path and the copy running again.
            match scan_for_local(&body.blocks[bi].stmts[..si], None, local) {
                LocalScan::Redefined => continue,
                LocalScan::Mentioned | LocalScan::Clear => return false,
            }
        }
        let next = &body.blocks[b];
        match scan_for_local(&next.stmts, Some(&next.terminator), local) {
            LocalScan::Mentioned => return false,
            LocalScan::Redefined => {}
            LocalScan::Clear => work.extend(successors_of(&next.terminator)),
        }
    }
    true
}

/// The statement that defines `local` as a bare copy of another local.
pub(super) fn copy_definition_site(body: &Body, local: usize) -> Option<(usize, usize)> {
    body.blocks.iter().enumerate().find_map(|(bi, block)| {
        block
            .stmts
            .iter()
            .position(|stmt| {
                matches!(
                    &stmt.kind,
                    StatementKind::Assign { place, rvalue: Rvalue::Use(Operand::Copy(_)) }
                        if place.projection.is_empty() && place.local.0 as usize == local
                )
            })
            .map(|si| (bi, si))
    })
}

/// Whether every copy along a move chain is the last use of the local it
/// reads: the copy at `site` of `source`, and each pass-through hop's copy of
/// the local below it, down to `origin`.
pub(super) fn move_chain_is_last_use(
    body: &Body,
    site: (usize, usize),
    source: usize,
    origin: usize,
    hops: &[usize],
) -> bool {
    if !copy_is_last_use(body, site, Local(source as u32)) {
        return false;
    }
    let chain: Vec<usize> = hops
        .iter()
        .copied()
        .chain(std::iter::once(origin))
        .collect();
    chain.windows(2).all(|pair| {
        copy_definition_site(body, pair[0])
            .is_some_and(|def| copy_is_last_use(body, def, Local(pair[1] as u32)))
    })
}

/// Blocks control can reach from `t`.
pub(super) fn successors_of(t: &Terminator) -> Vec<usize> {
    match t {
        Terminator::Goto { target } => vec![target.0 as usize],
        Terminator::SwitchInt { arms, default, .. } => {
            let mut v: Vec<usize> = arms.iter().map(|(_, t)| t.0 as usize).collect();
            v.push(default.0 as usize);
            v
        }
        Terminator::Call { target, .. } => target.iter().map(|t| t.0 as usize).collect(),
        Terminator::Assert { target, .. } => vec![target.0 as usize],
        Terminator::Drop { target, .. } => vec![target.0 as usize],
        _ => Vec::new(),
    }
}

/// Clears the region flag on every call result.
///
/// A local created while a region is open is region storage only where the
/// allocation came from the region's bump. A call result does not: a user
/// function allocates under its own frame's rules, and the string helpers
/// promote a copy of region-backed bytes to the heap so a recycled slab cannot
/// land on its own source. Neither is reclaimed by the slab sweep at pop, so
/// the frame has to release it.
///
/// Clearing the flag only lets the ownership rules apply; it never makes a
/// borrowed result owned. Where a result really is region storage, every free
/// path (`gos_rt_rc_release`, `gos_rt_vec_free`, `str_free_impl`) answers an
/// address-range test and returns without touching the memory, so a release
/// the region already reclaimed is a no-op.
pub(crate) fn clear_region_on_call_results(body: &mut Body) {
    let mut results: Vec<Local> = Vec::new();
    for block in &body.blocks {
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
        {
            results.push(destination.local);
        }
    }
    for local in results {
        if let Some(decl) = body.locals.get_mut(local.0 as usize) {
            decl.region = false;
        }
    }
}

/// Reclaim helpers whose value is a unique, non-reference-counted allocation,
/// so a bare copy that consumes it for the last time can carry the reclaim to
/// its new holder.
///
/// A value outside this set is left on the conservative at-return reclaim: two
/// owners of one un-counted allocation free it twice.
pub(super) fn transferable_by_move(free: &str) -> bool {
    matches!(
        free,
        "gos_rt_vec_free"
            | "gos_rt_map_free"
            | "gos_rt_set_free"
            | "gos_rt_deque_free"
            | "gos_rt_lazy_iter_drop_i64"
            | "gos_rt_lazy_iter_drop_pair_i64"
            | "gos_rt_http_response_free"
    )
}
