//! JSON runtime entries and borrowed vector receivers.

use super::*;

/// `true` when `name` is a runtime entry that takes a `json::Value` handle.
/// `xml::encode` reads the tree `xml::parse` answers, which is a `json::Value`.
pub(super) fn json_runtime_entry(name: &str) -> bool {
    name.starts_with("gos_rt_json_") || name == "gos_rt_xml_encode"
}

/// `true` when `name` is a json runtime entry that reads its handle argument
/// and answers something that never aliases it.
pub(super) fn json_entry_borrows(name: &str) -> bool {
    json_runtime_entry(name)
        && !matches!(
            name,
            "gos_rt_json_identity" | "gos_rt_json_free" | "gos_rt_json_free_slots"
        )
}

/// `true` when a value of `ty` can carry a `json::Value` handle.
///
/// A callee that answers one may be handing back the very handle it was
/// given, which is the one shape where a by-value argument is not a borrow.
pub(super) fn ty_reaches_json_value(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> bool {
    fn walk(
        tcx: &gossamer_types::TyCtxt,
        ty: gossamer_types::Ty,
        seen: &mut Vec<gossamer_types::Ty>,
    ) -> bool {
        use gossamer_types::TyKind;
        if seen.contains(&ty) {
            return false;
        }
        seen.push(ty);
        match tcx.kind_of(ty) {
            TyKind::JsonValue => true,
            TyKind::Ref { inner, .. } => walk(tcx, *inner, seen),
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => {
                walk(tcx, *elem, seen)
            }
            TyKind::Tuple(elems) => elems.clone().iter().any(|e| walk(tcx, *e, seen)),
            TyKind::HashMap { key, value, .. } => {
                let (key, value) = (*key, *value);
                walk(tcx, key, seen) || walk(tcx, value, seen)
            }
            TyKind::Adt { def, substs } => {
                let def = *def;
                if substs.types().iter().any(|t| walk(tcx, *t, seen)) {
                    return true;
                }
                match tcx.adt_field_tys(def, substs) {
                    Some(fields) => fields.to_vec().iter().any(|f| walk(tcx, *f, seen)),
                    None => false,
                }
            }
            _ => false,
        }
    }
    walk(tcx, ty, &mut Vec::new())
}

/// Frees provably single-owner `json::Value` handle locals.
///
/// `gos_rt_json_parse` / `gos_rt_json_get` mint one heap handle per
/// call (a `Box<GosJson>` holding an `Arc` share of the parsed tree);
/// nothing reclaimed them, so every parse in a loop leaked the whole
/// document. A local qualifies when every whole-local write is a call
/// destination or a null/zero init, and its value never escapes: it
/// may only be read as an argument to `gos_rt_json_*` runtime entries
/// (which borrow). Qualifying locals get `gos_rt_json_free` before
/// each re-initialising call and at every return. Aliased, stored,
/// returned, or user-call-passed handles keep today's (leaking)
/// behaviour - a leak is recoverable, a dangling handle is not.
pub(crate) fn insert_json_frees(
    body: &mut Body,
    tcx: &gossamer_types::TyCtxt,
    json_borrowing_fns: &std::collections::HashSet<String>,
) {
    use gossamer_types::TyKind;
    let n_locals = body.locals.len();
    let arity = body.arity as usize;
    let mut candidate = vec![false; n_locals];
    // A carrier local whose `Some` / `Ok` payload is a handle owns that
    // handle: `json::get(v, k)` mints one per call and the arm is the only
    // thing naming it, so the give-back is the carrier's rather than a
    // separate local's.
    let mut is_carrier = vec![false; n_locals];
    let mut any = false;
    for i in (arity + 1)..n_locals {
        if body.locals[i].region {
            continue;
        }
        match tcx.kind_of(body.locals[i].ty) {
            TyKind::JsonValue => {
                candidate[i] = true;
                any = true;
            }
            TyKind::Adt { def, substs }
                if (def.local == u32::MAX || def.local == u32::MAX - 1)
                    && substs
                        .types()
                        .first()
                        .is_some_and(|p| matches!(tcx.kind_of(*p), TyKind::JsonValue)) =>
            {
                candidate[i] = true;
                is_carrier[i] = true;
                any = true;
            }
            _ => {}
        }
    }
    if !any {
        return;
    }
    let is_json_rt = json_runtime_entry;
    // Entries that read a carrier's arm and hand nothing of its payload out.
    let carrier_query = |name: &str| {
        matches!(
            name,
            "gos_rt_result_is_ok" | "gos_rt_result_is_err" | "gos_rt_result_disc"
        )
    };
    // Combinators that hand the payload to a closure, as (carrier, env)
    // argument positions. `filter` is deliberately absent: it answers the very
    // payload it was given.
    let combinator_slots = |name: &str| -> Option<(usize, usize)> {
        match name {
            "gos_rt_option_and_then" | "gos_rt_result_and_then" | "gos_rt_result_map" => {
                Some((0, 1))
            }
            "gos_rt_option_map_i64" => Some((1, 0)),
            _ => None,
        }
    };
    // The closure each env local carries, from the `gos_fn_addr` the lowering
    // stores at offset 8. An env written with more than one closure answers
    // `None`, so a reused env is judged as unknown rather than as the last
    // name written into it.
    let closure_of_env = {
        let mut fn_addr: std::collections::HashMap<u32, &str> = std::collections::HashMap::new();
        let mut const_int: std::collections::HashMap<u32, i128> = std::collections::HashMap::new();
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                match rvalue {
                    Rvalue::CallIntrinsic { name, args } if *name == "gos_fn_addr" => {
                        if let Some(Operand::Const(ConstValue::Str(n))) = args.first() {
                            fn_addr.insert(place.local.0, n.as_str());
                        }
                    }
                    Rvalue::Use(Operand::Const(ConstValue::Int(n))) => {
                        const_int.insert(place.local.0, *n);
                    }
                    _ => {}
                }
            }
        }
        // The callable slot the closure lowering writes: offset 8 of the env
        // block, spelled either as a literal or as a local holding it.
        let is_callable_slot = |op: &Operand| match op {
            Operand::Const(ConstValue::Int(n)) => *n == 8,
            Operand::Copy(p) => const_int.get(&p.local.0) == Some(&8),
            _ => false,
        };
        let mut env: std::collections::HashMap<u32, Option<&str>> =
            std::collections::HashMap::new();
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { rvalue, .. } = &stmt.kind else {
                    continue;
                };
                let Rvalue::CallIntrinsic { name, args } = rvalue else {
                    continue;
                };
                if *name != "gos_store" {
                    continue;
                }
                let [Operand::Copy(target), offset, Operand::Copy(value)] = args.as_slice() else {
                    continue;
                };
                if !is_callable_slot(offset) {
                    continue;
                }
                let stored = fn_addr.get(&value.local.0).copied();
                env.entry(target.local.0)
                    .and_modify(|slot| {
                        if *slot != stored {
                            *slot = None;
                        }
                    })
                    .or_insert(stored);
            }
        }
        env
    };
    let combinator_borrows = |name: &str, args: &[Operand], local: u32| -> bool {
        let Some((carrier_idx, env_idx)) = combinator_slots(name) else {
            return false;
        };
        if !matches!(args.get(carrier_idx), Some(Operand::Copy(p)) if p.local.0 == local) {
            return false;
        }
        let Some(Operand::Copy(env)) = args.get(env_idx) else {
            return false;
        };
        closure_of_env
            .get(&env.local.0)
            .copied()
            .flatten()
            .is_some_and(|n| json_borrowing_fns.contains(n))
    };
    // A handle read out of a container is the container's, not the frame's:
    // the container hands back the slot's word and reclaims it at its own
    // death, so freeing it here would give the same handle back twice.
    let borrows_from_container = |name: &str| {
        name.starts_with("gos_rt_vec_get")
            || name.starts_with("gos_rt_iter")
            || name.starts_with("gos_rt_deque_get")
            || name.starts_with("gos_rt_map_get")
    };
    // A handle read through a borrowed carrier is borrowed too: unwrapping or
    // extracting the payload of a `map.get(k)` answers the container's own
    // handle, so the frame holds no share of it to give back. A payload read
    // out of a carrier that is read again - on a later iteration, or by its
    // own release - is a borrow of that carrier's handle as well; ownership
    // moves when the carrier is dead after the extraction, or when the
    // lowering consumes it by resetting it to an empty `Err` right after.
    let extracts_payload = |name: &str| {
        matches!(
            name,
            "gos_rt_result_payload"
                | "gos_rt_result_unwrap"
                | "gos_rt_option_unwrap"
                | "gos_rt_result_expect"
                | "gos_rt_result_unwrap_carrier"
                | "gos_rt_option_unwrap_carrier"
        ) || name.starts_with("gos_rt_result_unwrap_or")
    };
    let resets = |stmt: Option<&Statement>, carrier: u32| {
        matches!(
            stmt.map(|s| &s.kind),
            Some(StatementKind::Assign {
                place,
                rvalue: Rvalue::CallIntrinsic { name, args },
            }) if place.local.0 == carrier
                && place.projection.is_empty()
                && *name == "gos_rt_result_new"
                && args.iter().all(|a| matches!(a, Operand::Const(_)))
        )
    };
    let carrier_arg = |args: &[Operand]| match args.first() {
        Some(Operand::Copy(p)) if p.projection.is_empty() => Some(p.local.0),
        _ => None,
    };
    let borrowed = {
        let mut borrowed = vec![false; n_locals];
        for (bi, block) in body.blocks.iter().enumerate() {
            for (si, stmt) in block.stmts.iter().enumerate() {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::CallIntrinsic { name, args },
                } = &stmt.kind
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < n_locals
                    && extracts_payload(name)
                    && let Some(carrier) = carrier_arg(args)
                    && !resets(block.stmts.get(si + 1), carrier)
                    && !stmt_is_last_use(body, bi, si, Local(carrier))
                {
                    borrowed[place.local.0 as usize] = true;
                }
            }
            if let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                destination,
                target,
                ..
            } = &block.terminator
                && destination.projection.is_empty()
                && (destination.local.0 as usize) < n_locals
                && extracts_payload(name)
                && let Some(carrier) = carrier_arg(args)
            {
                let consumed = target.is_some_and(|t| {
                    body.blocks
                        .get(t.0 as usize)
                        .is_some_and(|next| resets(next.stmts.first(), carrier))
                });
                if !consumed && !terminator_is_last_use(body, bi, Local(carrier)) {
                    borrowed[destination.local.0 as usize] = true;
                }
            }
        }
        let reads_carrier =
            |name: &str| name.starts_with("gos_rt_option_") || name.starts_with("gos_rt_result_");
        let mut changed = true;
        while changed {
            changed = false;
            let mut mark = |i: usize, borrowed: &mut Vec<bool>| {
                if i < n_locals && !borrowed[i] {
                    borrowed[i] = true;
                    changed = true;
                }
            };
            let from_borrowed = |args: &[Operand], borrowed: &[bool]| {
                args.iter().any(|a| {
                    matches!(a, Operand::Copy(p)
                        if (p.local.0 as usize) < n_locals && borrowed[p.local.0 as usize])
                })
            };
            for block in &body.blocks {
                for stmt in &block.stmts {
                    let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                        continue;
                    };
                    if !place.projection.is_empty() {
                        continue;
                    }
                    let dest = place.local.0 as usize;
                    let is_borrowed = match rvalue {
                        Rvalue::CallIntrinsic { name, args } => {
                            borrows_from_container(name)
                                || (reads_carrier(name) && from_borrowed(args, &borrowed))
                        }
                        Rvalue::Use(Operand::Copy(src)) => {
                            (src.local.0 as usize) < n_locals && borrowed[src.local.0 as usize]
                        }
                        _ => false,
                    };
                    if is_borrowed {
                        mark(dest, &mut borrowed);
                    }
                }
                if let Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(name)),
                    args,
                    destination,
                    ..
                } = &block.terminator
                    && destination.projection.is_empty()
                    && (borrows_from_container(name)
                        || (reads_carrier(name) && from_borrowed(args, &borrowed)))
                {
                    mark(destination.local.0 as usize, &mut borrowed);
                }
            }
        }
        borrowed
    };
    for (i, is_borrowed) in borrowed.iter().enumerate() {
        if *is_borrowed {
            candidate[i] = false;
        }
    }
    // Whole-local handle moves (`v = Copy(tmp)` with both sides
    // JSON-typed): ownership transfers when the move is the source's
    // ONLY value read and its only such move - the destination owns
    // the handle, the source is never freed. Pre-scan to identify
    // them so the escape check below can treat the move as allowed.
    // Locals that own a handle, directly or through a carrier's arm. A move
    // hands ownership on within one class; the two are never interchangeable,
    // since one names the handle and the other names the arm holding it.
    let jv: Vec<bool> = (0..n_locals)
        .map(|i| matches!(tcx.kind_of(body.locals[i].ty), TyKind::JsonValue) || is_carrier[i])
        .collect();
    let mut value_reads = vec![0usize; n_locals];
    let mut move_edges: Vec<(usize, usize, usize, usize)> = Vec::new(); // (src, dest, bi, si)
    for (bi, block) in body.blocks.iter().enumerate() {
        let mut count_op = |op: &Operand| {
            if let Operand::Copy(p) = op
                && p.projection.is_empty()
                && (p.local.0 as usize) < n_locals
            {
                value_reads[p.local.0 as usize] += 1;
            }
        };
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            match rvalue {
                Rvalue::Use(Operand::Copy(src)) => {
                    if place.projection.is_empty()
                        && src.projection.is_empty()
                        && (place.local.0 as usize) < n_locals
                        && (src.local.0 as usize) < n_locals
                        && jv[place.local.0 as usize]
                        && jv[src.local.0 as usize]
                        && is_carrier[place.local.0 as usize] == is_carrier[src.local.0 as usize]
                    {
                        move_edges.push((src.local.0 as usize, place.local.0 as usize, bi, si));
                    } else {
                        count_op(&Operand::Copy(src.clone()));
                    }
                }
                Rvalue::CallIntrinsic { name, args } if is_json_rt(name) => {
                    // Borrowing json-runtime args are not value reads.
                    let _ = args;
                }
                Rvalue::CallIntrinsic { args, .. } => {
                    for a in args {
                        count_op(a);
                    }
                }
                Rvalue::BinaryOp { lhs, rhs, .. } => {
                    count_op(lhs);
                    count_op(rhs);
                }
                Rvalue::UnaryOp { operand, .. } | Rvalue::Cast { operand, .. } => count_op(operand),
                Rvalue::Aggregate { operands, .. } => {
                    for a in operands {
                        count_op(a);
                    }
                }
                Rvalue::Repeat { value, .. } => count_op(value),
                Rvalue::Ref { .. } | Rvalue::Len(_) => {}
                Rvalue::Use(_) => {}
                Rvalue::StaticLoad(_) => {}
            }
        }
        if let Terminator::Call { callee, args, .. } = &block.terminator {
            let allowed = matches!(callee, Operand::Const(ConstValue::Str(n)) if is_json_rt(n));
            if !allowed {
                for a in args {
                    count_op(a);
                }
            }
        }
    }
    // A source moves cleanly when it has exactly one outgoing move and
    // no other value reads.
    let mut moved_from = vec![false; n_locals];
    let mut move_inits: Vec<(usize, usize, usize)> = Vec::new(); // (dest, bi, si)
    {
        let mut out_moves = vec![0usize; n_locals];
        for &(src, _, _, _) in &move_edges {
            out_moves[src] += 1;
        }
        for &(src, dest, bi, si) in &move_edges {
            if out_moves[src] == 1 && value_reads[src] == 0 {
                moved_from[src] = true;
                move_inits.push((dest, bi, si));
            }
        }
    }
    // A read withdraws the local unless the site allows its class: a handle
    // and a carrier reach different entry points, so each has its own verdict.
    fn check_op(
        op: &Operand,
        allowed: bool,
        allowed_carrier: bool,
        carrier: &[bool],
        c: &mut [bool],
    ) {
        if let Operand::Copy(p) = op
            && (p.local.0 as usize) < c.len()
            && c[p.local.0 as usize]
        {
            let ok = if carrier[p.local.0 as usize] {
                allowed_carrier
            } else {
                allowed
            };
            if !ok {
                c[p.local.0 as usize] = false;
            }
        }
    }
    // Init sites per local: (block, stmt-or-terminator marker).
    let mut init_sites: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n_locals];
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            // Reads: any appearance as a Copy operand outside a
            // json-runtime call argument escapes the handle.
            match rvalue {
                Rvalue::CallIntrinsic { name, args } => {
                    let allowed = is_json_rt(name);
                    let queries = carrier_query(name);
                    for a in args {
                        let carrier_ok = queries
                            || matches!(a, Operand::Copy(p) if combinator_borrows(name, args, p.local.0));
                        check_op(a, allowed, carrier_ok, &is_carrier, &mut candidate);
                    }
                }
                Rvalue::Use(op) => {
                    let clean_move = matches!(
                        op,
                        Operand::Copy(p)
                            if p.projection.is_empty()
                                && (p.local.0 as usize) < n_locals
                                && moved_from[p.local.0 as usize]
                                && place.projection.is_empty()
                                && (place.local.0 as usize) < n_locals
                                && jv[place.local.0 as usize]
                    );
                    check_op(op, clean_move, clean_move, &is_carrier, &mut candidate);
                }
                Rvalue::BinaryOp { lhs, rhs, .. } => {
                    check_op(lhs, false, false, &is_carrier, &mut candidate);
                    check_op(rhs, false, false, &is_carrier, &mut candidate);
                }
                Rvalue::UnaryOp { operand, .. } | Rvalue::Cast { operand, .. } => {
                    check_op(operand, false, false, &is_carrier, &mut candidate);
                }
                Rvalue::Aggregate { operands, .. } => {
                    for a in operands {
                        check_op(a, false, false, &is_carrier, &mut candidate);
                    }
                }
                Rvalue::Repeat { value, .. } => {
                    check_op(value, false, false, &is_carrier, &mut candidate);
                }
                Rvalue::Ref { place: rp, .. } => {
                    if candidate.get(rp.local.0 as usize).copied().unwrap_or(false) {
                        candidate[rp.local.0 as usize] = false;
                    }
                }
                Rvalue::Len(_) => {}
                Rvalue::StaticLoad(_) => {}
            }
            // Writes to the candidate itself.
            if place.projection.is_empty() && (place.local.0 as usize) < n_locals {
                let i = place.local.0 as usize;
                if candidate[i] {
                    match rvalue {
                        Rvalue::CallIntrinsic { name, .. } if borrows_from_container(name) => {
                            candidate[i] = false;
                        }
                        Rvalue::CallIntrinsic { .. } => init_sites[i].push((bi, si)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(_))) => {}
                        // A move carries the source's ownership, so it hands
                        // over a free only when the source had one to give.
                        Rvalue::Use(Operand::Copy(src))
                            if src.projection.is_empty()
                                && (src.local.0 as usize) < n_locals
                                && moved_from[src.local.0 as usize] =>
                        {
                            if candidate[src.local.0 as usize] {
                                init_sites[i].push((bi, si));
                            } else {
                                candidate[i] = false;
                            }
                        }
                        _ => candidate[i] = false,
                    }
                }
            } else if !place.projection.is_empty()
                && (place.local.0 as usize) < n_locals
                && candidate[place.local.0 as usize]
            {
                candidate[place.local.0 as usize] = false;
            }
        }
        match &block.terminator {
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                let callee_name = match callee {
                    Operand::Const(ConstValue::Str(n)) => Some(n.as_str()),
                    _ => None,
                };
                // A by-value argument to a named user function is a borrow:
                // the callee cannot outlive the call, and a container it
                // stores the handle in takes a handle of its own. The shape
                // that would alias is a callee whose answer can carry the
                // handle back out, so a destination type reaching a
                // `json::Value` leaves the frame's handle disowned.
                let user_call_borrows = matches!(callee, Operand::FnRef { .. })
                    && (destination.local.0 as usize) < n_locals
                    && !ty_reaches_json_value(tcx, body.locals[destination.local.0 as usize].ty);
                let allowed = callee_name.is_some_and(is_json_rt) || user_call_borrows;
                let queries = callee_name.is_some_and(carrier_query);
                for a in args {
                    if let Operand::Copy(p) = a
                        && (p.local.0 as usize) < n_locals
                        && candidate[p.local.0 as usize]
                    {
                        let ok = if is_carrier[p.local.0 as usize] {
                            queries
                                || callee_name
                                    .is_some_and(|n| combinator_borrows(n, args, p.local.0))
                        } else {
                            allowed
                        };
                        if !ok {
                            candidate[p.local.0 as usize] = false;
                        }
                    }
                }
                if destination.projection.is_empty()
                    && (destination.local.0 as usize) < n_locals
                    && candidate[destination.local.0 as usize]
                {
                    if callee_name.is_some_and(borrows_from_container) {
                        candidate[destination.local.0 as usize] = false;
                    } else {
                        init_sites[destination.local.0 as usize].push((bi, usize::MAX));
                    }
                }
            }
            Terminator::SwitchInt { discriminant, .. } => {
                if let Operand::Copy(p) = discriminant
                    && (p.local.0 as usize) < n_locals
                    && candidate[p.local.0 as usize]
                {
                    candidate[p.local.0 as usize] = false;
                }
            }
            _ => {}
        }
    }
    let qualified: Vec<usize> = (0..n_locals)
        .filter(|&i| candidate[i] && !moved_from[i])
        .collect();
    if qualified.is_empty() {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut next_local = body.locals.len();
    let free_stmt = |l: usize, span: gossamer_lex::Span, next: &mut usize| -> Statement {
        let dest = Local(u32::try_from(*next).expect("local overflow"));
        *next += 1;
        let operand = Operand::Copy(Place::local(Local(u32::try_from(l).unwrap_or(0))));
        // A carrier gives back the handle its `Some` / `Ok` arm names; the
        // other arm's payload word belongs to the error value. Kind 3 is the
        // `json::Value` payload.
        let rvalue = if is_carrier[l] {
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_ok_payload_release",
                args: vec![operand, Operand::Const(ConstValue::Int(3))],
            }
        } else {
            Rvalue::CallIntrinsic {
                name: "gos_rt_json_free",
                args: vec![operand],
            }
        };
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(dest),
                rvalue,
            },
            span,
            inlined: None,
        }
    };
    // Per-block gap lists: stmt-index -> stmts to insert before it,
    // plus an end-of-block list for Return frees.
    let nb = body.blocks.len();
    let mut pre_gaps: Vec<Vec<(usize, Statement)>> = vec![Vec::new(); nb];
    let mut end_gaps: Vec<Vec<Statement>> = vec![Vec::new(); nb];
    for &l in &qualified {
        // Free the previous value before each re-initialising call
        // (first execution frees the zero init, which is null-safe).
        for &(bi, si) in &init_sites[l] {
            let span = body.blocks[bi].span;
            if si == usize::MAX {
                end_gaps[bi].push(free_stmt(l, span, &mut next_local));
            } else {
                pre_gaps[bi].push((si, free_stmt(l, span, &mut next_local)));
            }
        }
    }
    for (bi, block) in body.blocks.iter().enumerate() {
        if matches!(block.terminator, Terminator::Return) {
            let span = block.span;
            for &l in &qualified {
                end_gaps[bi].push(free_stmt(l, span, &mut next_local));
            }
        }
    }
    for bi in (0..nb).rev() {
        pre_gaps[bi].sort_by_key(|(si, _)| std::cmp::Reverse(*si));
        let drained: Vec<(usize, Statement)> = std::mem::take(&mut pre_gaps[bi]);
        for (si, stmt) in drained {
            body.blocks[bi].stmts.insert(si, stmt);
        }
        for stmt in std::mem::take(&mut end_gaps[bi]) {
            body.blocks[bi].stmts.push(stmt);
        }
    }
    // The pre-init frees below read the local's previous value; only
    // some locals get the MIR zero-init, so make it explicit for every
    // qualified local (free of null is a no-op).
    if !body.blocks.is_empty() {
        let span = body.blocks[0].span;
        for (k, &l) in qualified.iter().enumerate() {
            body.blocks[0].stmts.insert(
                k,
                Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(Local(u32::try_from(l).unwrap_or(0))),
                        rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    },
                    span,
                    inlined: None,
                },
            );
        }
    }
    for _ in body.locals.len()..next_local {
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }
}

/// A vector helper that reads or writes through the vector it is handed
/// without taking a share of it or keeping a pointer to it.
///
/// Such a call stands beside the value rather than between it and the local a
/// later move hands it to, so it does not make the vector's ownership
/// ambiguous.
pub(super) fn borrows_vec_receiver(name: &str) -> bool {
    if !name.starts_with("gos_rt_vec_") {
        return matches!(name, "gos_rt_len");
    }
    !matches!(
        name,
        "gos_rt_vec_free"
            | "gos_rt_vec_retain"
            | "gos_rt_vec_mark_shared"
            | "gos_rt_vec_assign"
            | "gos_rt_vec_clone"
            | "gos_rt_vec_set_slot_children"
            | "gos_rt_vec_set_elem_meta"
    )
}

/// Gives back the share a frame minted for a holder when the binding that
/// minted it is rebound.
///
/// A store into a heap object takes a share of the value it writes, and the
/// frame keeps its own. Rebinding the frame's name to the object that now
/// holds the value leaves that share with no name, so it is returned here.
pub(crate) fn release_rebound_rc_locals(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let n_locals = body.locals.len();
    let arity = body.arity as usize;
    if n_locals == 0 {
        return;
    }
    let is_rc = |l: Local| -> bool {
        let i = l.0 as usize;
        i > arity && i < n_locals && tcx.is_rc_managed(body.locals[i].ty) && !body.locals[i].region
    };
    let bare_arg = |args: &[Operand]| -> Option<Local> {
        match args.first() {
            Some(Operand::Copy(p)) if p.projection.is_empty() => Some(p.local),
            _ => None,
        }
    };
    let mut sites: Vec<(usize, usize, Local)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        // Locals this block minted a holder's share for, still unbalanced.
        let mut minted: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if let Rvalue::CallIntrinsic { name, args } = rvalue {
                match *name {
                    "gos_rt_rc_retain" => {
                        if let Some(l) = bare_arg(args)
                            && is_rc(l)
                        {
                            minted.insert(l.0);
                        }
                        continue;
                    }
                    "gos_rt_rc_release" => {
                        if let Some(l) = bare_arg(args) {
                            minted.remove(&l.0);
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            if !place.projection.is_empty() || !minted.contains(&place.local.0) {
                continue;
            }
            // A rebinding that names another value, not one derived from the
            // local itself.
            let rebinds = match rvalue {
                Rvalue::Use(Operand::Copy(src)) => {
                    src.projection.is_empty() && src.local != place.local
                }
                _ => false,
            };
            if rebinds {
                sites.push((bi, si, place.local));
            }
            minted.remove(&place.local.0);
        }
    }
    if sites.is_empty() {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    for (bi, si, local) in sites.into_iter().rev() {
        let span = body.blocks[bi].stmts[si].span;
        let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        body.blocks[bi]
            .stmts
            .insert(si, rc_call_stmt("gos_rt_rc_release", dest, local, span));
    }
}
