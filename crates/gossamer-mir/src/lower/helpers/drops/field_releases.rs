//! Releases for reference-counted fields, and lazy iterator state.

use super::*;

/// Builds a `gos_rt_rc_retain` / `gos_rt_rc_release` call on one RC field of a
/// by-value aggregate local (`local.field_idx`).
pub(super) fn field_rc_call_stmt(
    name: &'static str,
    dest: Local,
    local: Local,
    field_path: &[u32],
    span: gossamer_lex::Span,
) -> Statement {
    Statement {
        kind: StatementKind::Assign {
            place: Place::local(dest),
            rvalue: Rvalue::CallIntrinsic {
                name,
                args: vec![Operand::Copy(Place {
                    local,
                    projection: field_path
                        .iter()
                        .map(|idx| crate::ir::Projection::Field(*idx))
                        .collect(),
                })],
            },
        },
        span,
        inlined: None,
    }
}

/// Appends a carrier field's arm kinds to the payload call `stmt` makes on it;
/// every other field kind's call takes the field alone.
pub(crate) fn push_carrier_kinds(stmt: &mut Statement, kind: FieldRcKind) {
    if let FieldRcKind::Carrier { ok, err } = kind
        && let StatementKind::Assign {
            rvalue: Rvalue::CallIntrinsic { args, .. },
            ..
        } = &mut stmt.kind
    {
        args.push(Operand::Const(ConstValue::Int(i128::from(ok))));
        args.push(Operand::Const(ConstValue::Int(i128::from(err))));
    }
}

pub(super) fn rc_call_stmt(
    name: &'static str,
    dest: Local,
    local: Local,
    span: gossamer_lex::Span,
) -> Statement {
    Statement {
        kind: StatementKind::Assign {
            place: Place::local(dest),
            rvalue: Rvalue::CallIntrinsic {
                name,
                args: vec![Operand::Copy(Place::local(local))],
            },
        },
        span,
        inlined: None,
    }
}

/// Deterministic reclamation for escaped value-aggregate heap copies.
///
/// The LLVM backend heap-copies a multi-slot struct that flows into a
/// `Some(..)`/`Ok(..)`/`Err(..)` payload (`gos_rt_rc_alloc_copy`, an RC
/// blob in the copy-blob provenance set). This pass gives every holder
/// of such a payload pointer exactly one share:
///
/// - an option-typed local (`{disc, payload}` by value) is a holder: it
///   retains after every initialisation except the `gos_rt_result_new`
///   mint itself and call destinations (the callee's return-copy mints
///   the caller's share), and releases before reassignment and at
///   return;
/// - a guarded slot of a stack aggregate is a holder: the aggregate
///   retains its children after every whole-local initialisation
///   (construction operands keep their own shares) and releases them
///   before reassignment, before a call-destination overwrite, and at
///   return;
/// - an option field store (`s.next = o`, directly or through a
///   reference) releases the slot's previous payload and retains the
///   new one in place;
/// - entry blocks zero the guarded slots and option locals so the first
///   release never reads stack garbage.
///
/// Every retain/release the runtime performs is gated on the copy-blob
/// provenance set, so pointers produced by anything other than
/// `gos_rt_rc_alloc_copy` (map gets, borrows, the Cranelift tier's
/// construction-allocated aggregates) are never touched: a missed entry
/// can only leak, never corrupt.
pub(crate) fn insert_aggr_copy_drops(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;
    let n_locals = body.locals.len();
    if n_locals == 0 {
        return;
    }
    let arity = body.arity as usize;

    // A guarded meta symbol with at least one (gate, disc, payload) entry.
    let walk_meta = |ty: gossamer_types::Ty| -> Option<String> {
        let sym = tcx.aggr_copy_meta(ty)?;
        let blob = tcx.rc_meta(sym)?;
        if blob.len() >= 2 && blob[1] > 0 {
            Some(sym.to_string())
        } else {
            None
        }
    };
    let guarded_locals: Vec<(Local, String)> = ((arity + 1)..n_locals)
        .filter(|&i| !body.locals[i].region)
        .filter_map(|i| {
            walk_meta(body.locals[i].ty).map(|sym| (Local(u32::try_from(i).unwrap_or(0)), sym))
        })
        .collect();
    // The return slot participates in retains only: a return-copy mints
    // the caller's share (released by the caller), but the slot itself
    // is never released here.
    let retain_meta_of = |l: Local| -> Option<String> {
        let i = l.0 as usize;
        if i >= n_locals || body.locals[i].region || (1..=arity).contains(&i) {
            return None;
        }
        walk_meta(body.locals[i].ty)
    };

    // By-value Option/Result locals whose payload type registered a
    // copy-blob meta on either side.
    let is_guarded_option = |ty: gossamer_types::Ty| -> bool {
        match tcx.kind_of(ty) {
            TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => {
                substs
                    .types()
                    .iter()
                    .take(2)
                    .any(|p| tcx.aggr_copy_meta(*p).is_some())
            }
            _ => false,
        }
    };
    // A runtime call answering its aggregate payload as a counted blob hands
    // the frame the only share of that blob, so the destination holds it the
    // way a carrier the frame built does.
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
    let option_holder = |l: Local| -> bool {
        let i = l.0 as usize;
        i > arity
            && i < n_locals
            && !body.locals[i].region
            && (is_guarded_option(body.locals[i].ty) || counted_answer[i])
    };
    // `result_new` destinations whose payload type carries a copy-blob
    // meta are guarded option holders even when the typer left the
    // destination's type unresolved (`Ok(S { .. })` through a `Var`
    // temp): without the classification, the temp's sweep release is
    // never emitted and the payload blob leaves the function one count
    // high - pinned in the collector buffer, one leak per call.
    let mut mint_holders = vec![false; n_locals];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && place.projection.is_empty()
                && (place.local.0 as usize) < n_locals
                && let Rvalue::CallIntrinsic { name, args } = rvalue
                && (*name == "gos_rt_result_new" || *name == "gos_rt_result_new_f64")
                && let Some(Operand::Copy(pp)) = args.get(1)
                && pp.projection.is_empty()
                && (pp.local.0 as usize) < n_locals
                && tcx
                    .aggr_copy_meta(body.locals[pp.local.0 as usize].ty)
                    .is_some()
            {
                mint_holders[place.local.0 as usize] = true;
            }
        }
    }
    let option_holders: Vec<Local> = ((arity + 1)..n_locals)
        .filter(|&i| {
            !body.locals[i].region
                && (is_guarded_option(body.locals[i].ty) || mint_holders[i] || counted_answer[i])
        })
        .map(|i| Local(u32::try_from(i).unwrap_or(0)))
        .collect();

    // A field store whose base resolves (through references) to a type
    // with a guarded meta, assigning an option-typed value: the slot's
    // old payload is released and the new one retained in place.
    let peel_ref = |mut ty: gossamer_types::Ty| -> gossamer_types::Ty {
        while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
            ty = *inner;
        }
        ty
    };
    // The type a chain of field projections reaches, seeing through a
    // reference at each step. `None` when the path leaves the layout this
    // walk understands.
    let projected_field_ty = |base: gossamer_types::Ty,
                              projection: &[crate::ir::Projection]|
     -> Option<gossamer_types::Ty> {
        let mut ty = peel_ref(base);
        for step in projection {
            let next = match step {
                crate::ir::Projection::Field(idx) => match tcx.kind_of(ty) {
                    TyKind::Adt { def, substs } => tcx
                        .adt_field_tys(*def, substs)
                        .and_then(|tys| tys.get(*idx as usize).copied()),
                    TyKind::Tuple(elems) => elems.get(*idx as usize).copied(),
                    TyKind::Array { elem, len } if (*idx as usize) < len.to_usize() => Some(*elem),
                    _ => None,
                },
                crate::ir::Projection::Index(_) => match tcx.kind_of(ty) {
                    TyKind::Array { elem, .. } | TyKind::Vec(elem) | TyKind::Slice(elem) => {
                        Some(*elem)
                    }
                    _ => None,
                },
                crate::ir::Projection::Deref
                | crate::ir::Projection::Downcast(_)
                | crate::ir::Projection::Discriminant => None,
            }?;
            ty = peel_ref(next);
        }
        Some(ty)
    };
    // The by-value `{disc, payload}` carrier itself, whatever it holds. The
    // slot helpers read the payload word beside the discriminant, so the
    // address handed to one has to name a two-word carrier and nothing else:
    // pointed at a single-word field they would read the field beside it.
    let is_option_slot_ty = |ty: gossamer_types::Ty| -> bool {
        matches!(
            tcx.kind_of(peel_ref(ty)),
            TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1
        )
    };
    // A store through `&mut Option<T>` reaches the caller's carrier: the
    // slot it names takes a share of the payload exactly as an aggregate's
    // own carrier field does.
    let is_carrier_deref_store = |place: &Place, rvalue: &Rvalue| -> bool {
        if place.projection.as_slice() != [crate::ir::Projection::Deref] {
            return false;
        }
        let i = place.local.0 as usize;
        if i >= n_locals {
            return false;
        }
        let TyKind::Ref { inner, .. } = tcx.kind_of(body.locals[i].ty) else {
            return false;
        };
        is_option_slot_ty(*inner) && !matches!(rvalue, Rvalue::Use(Operand::Const(_)))
    };
    let is_option_field_store = |place: &Place, rvalue: &Rvalue| -> bool {
        if is_carrier_deref_store(place, rvalue) {
            return true;
        }
        if place.projection.is_empty()
            || !place.projection.iter().all(|p| {
                matches!(
                    p,
                    crate::ir::Projection::Field(_) | crate::ir::Projection::Index(_)
                )
            })
        {
            return false;
        }
        let i = place.local.0 as usize;
        if i >= n_locals {
            return false;
        }
        // The destination has to be the carrier itself. A store into any
        // other field or element of the same aggregate is an ordinary write.
        let Some(slot_ty) = projected_field_ty(body.locals[i].ty, &place.projection) else {
            return false;
        };
        if !is_option_slot_ty(slot_ty) {
            return false;
        }
        // An element of a sequence of carriers takes its own share the way
        // an aggregate's carrier field does: the sequence owns what it
        // holds, so the temporary the store copied from is free to release
        // its own on the next overwrite and at the return sweep.
        if walk_meta(peel_ref(body.locals[i].ty)).is_none() && !is_guarded_option(slot_ty) {
            return false;
        }
        match rvalue {
            Rvalue::Use(Operand::Copy(src)) if src.projection.is_empty() => {
                option_holder(src.local) || is_guarded_option(body.locals[src.local.0 as usize].ty)
            }
            Rvalue::Use(_) | Rvalue::CallIntrinsic { .. } => {
                // Any other store into a carrier slot replaces whatever it
                // held, so the old payload is released and the new one
                // retained; a payload outside the copy-blob set no-ops.
                true
            }
            _ => false,
        }
    };

    let mut gaps: Vec<Vec<Vec<Statement>>> = body
        .blocks
        .iter()
        .map(|b| vec![Vec::new(); b.stmts.len() + 1])
        .collect();
    let mut next_unit = body.locals.len();
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut extra_locals = 0usize;
    let call_stmt = |name: &'static str,
                     args: Vec<Operand>,
                     span: gossamer_lex::Span,
                     next_unit: &mut usize,
                     extra: &mut usize|
     -> Statement {
        let dest = Local(u32::try_from(*next_unit).expect("local overflow"));
        *next_unit += 1;
        *extra += 1;
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(dest),
                rvalue: Rvalue::CallIntrinsic { name, args },
            },
            span,
            inlined: None,
        }
    };
    let walk_args = |l: Local, sym: &str| -> Vec<Operand> {
        vec![
            Operand::Copy(Place::local(l)),
            Operand::Const(ConstValue::Str(sym.to_string())),
        ]
    };

    for (bi, block) in body.blocks.iter().enumerate() {
        let len = block.stmts.len();
        let span = block.span;
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.projection.is_empty() {
                // Whole-local (re)initialisation of a guarded aggregate:
                // release the previous children, retain the new ones.
                if let Some((_, sym)) = guarded_locals.iter().find(|(l, _)| *l == place.local) {
                    gaps[bi][si].push(call_stmt(
                        "gos_rt_aggr_release_children",
                        walk_args(place.local, sym),
                        span,
                        &mut next_unit,
                        &mut extra_locals,
                    ));
                }
                if let Some(sym) = retain_meta_of(place.local)
                    && !matches!(rvalue, Rvalue::Use(Operand::Const(_)))
                {
                    gaps[bi][si + 1].push(call_stmt(
                        "gos_rt_aggr_retain_children",
                        walk_args(place.local, &sym),
                        span,
                        &mut next_unit,
                        &mut extra_locals,
                    ));
                }
                // Whole-local (re)initialisation of an option holder.
                if option_holder(place.local) || place.local == Local::RETURN {
                    let holder_ty_ok = if place.local == Local::RETURN {
                        is_guarded_option(body.locals[0].ty)
                    } else {
                        true
                    };
                    if holder_ty_ok {
                        if option_holder(place.local) {
                            gaps[bi][si].push(call_stmt(
                                "gos_rt_option_slot_release",
                                vec![Operand::Copy(Place::local(place.local))],
                                span,
                                &mut next_unit,
                                &mut extra_locals,
                            ));
                        }
                        let is_mint = matches!(
                            rvalue,
                            Rvalue::CallIntrinsic { name, .. }
                                if *name == "gos_rt_result_new"
                                    || *name == "gos_rt_result_new_f64"
                        );
                        let is_const = matches!(rvalue, Rvalue::Use(Operand::Const(_)));
                        if !is_mint && !is_const {
                            gaps[bi][si + 1].push(call_stmt(
                                "gos_rt_option_slot_retain",
                                vec![Operand::Copy(Place::local(place.local))],
                                span,
                                &mut next_unit,
                                &mut extra_locals,
                            ));
                        }
                    }
                }
            } else if is_option_field_store(place, rvalue) {
                // Overwriting an owning option slot in place: release the
                // old payload, store, retain the new one. The helpers read
                // the payload word beside the discriminant, so they take the
                // slot's address: a reference already is that address, while
                // a field names it through the aggregate.
                let slot = if place.projection.as_slice() == [crate::ir::Projection::Deref] {
                    Place::local(place.local)
                } else {
                    place.clone()
                };
                gaps[bi][si].push(call_stmt(
                    "gos_rt_option_slot_release",
                    vec![Operand::Copy(slot.clone())],
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
                gaps[bi][si + 1].push(call_stmt(
                    "gos_rt_option_slot_retain",
                    vec![Operand::Copy(slot)],
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
        }
        // A container store takes its own share of the carrier's payload:
        // the element lives as long as the container, while the local the
        // value came from is still released on its next overwrite and by
        // the return sweep. `xs[i] = v` also drops what the element held.
        // A by-value parameter is lent, so the caller keeps its share and the
        // container takes one of its own exactly as from a local.
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } = &block.terminator
            && is_consuming_call(name)
        {
            for arg in args.iter().skip(1) {
                let Operand::Copy(p) = arg else { continue };
                let lent_param = (1..=arity).contains(&(p.local.0 as usize))
                    && (p.local.0 as usize) < n_locals
                    && is_guarded_option(body.locals[p.local.0 as usize].ty);
                if !p.projection.is_empty() || !(option_holder(p.local) || lent_param) {
                    continue;
                }
                if name.starts_with("gos_rt_vec_set")
                    && let Some(Operand::Copy(recv)) = args.first()
                    && let Some(Operand::Copy(index)) = args.get(1)
                    && recv.projection.is_empty()
                    && index.projection.is_empty()
                    && p.local != index.local
                {
                    let element = Place {
                        local: recv.local,
                        projection: vec![crate::ir::Projection::Index(index.local)].into(),
                    };
                    gaps[bi][len].push(call_stmt(
                        "gos_rt_option_slot_release",
                        vec![Operand::Copy(element)],
                        span,
                        &mut next_unit,
                        &mut extra_locals,
                    ));
                }
                gaps[bi][len].push(call_stmt(
                    "gos_rt_option_slot_retain",
                    vec![Operand::Copy(Place::local(p.local))],
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
        }
        // A call destination is minted by the callee: release the old
        // value, never retain the new one.
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
        {
            if let Some((_, sym)) = guarded_locals.iter().find(|(l, _)| *l == destination.local) {
                gaps[bi][len].push(call_stmt(
                    "gos_rt_aggr_release_children",
                    walk_args(destination.local, sym),
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
            if option_holder(destination.local) {
                gaps[bi][len].push(call_stmt(
                    "gos_rt_option_slot_release",
                    vec![Operand::Copy(Place::local(destination.local))],
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
        }
        // `unwrap` copies out the words of a payload its carrier keeps, and the
        // carrier gives those words' children back when it is released, so a
        // destination that releases children of its own takes its shares once
        // the call has answered. A carrier this frame releases is a holder; a
        // carrier parameter is released by the caller.
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            target: Some(target),
            ..
        } = &block.terminator
            && matches!(
                name.as_str(),
                "gos_rt_result_unwrap" | "gos_rt_option_unwrap"
            )
            && destination.projection.is_empty()
            && let Some(Operand::Copy(carrier)) = args.first()
            && carrier.projection.is_empty()
            && (carrier.local.0 as usize) < n_locals
            && (option_holder(carrier.local)
                || ((1..=arity).contains(&(carrier.local.0 as usize))
                    && is_guarded_option(body.locals[carrier.local.0 as usize].ty)))
            && let Some(sym) = retain_meta_of(destination.local)
            && let Some(head) = gaps.get_mut(target.0 as usize).and_then(|g| g.first_mut())
        {
            head.push(call_stmt(
                "gos_rt_aggr_retain_children",
                walk_args(destination.local, &sym),
                span,
                &mut next_unit,
                &mut extra_locals,
            ));
        }
        // A carrier read out of a box is a view of words the box keeps, so
        // the destination takes its own share once the call has answered.
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            target: Some(target),
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && option_holder(destination.local)
            && reads_boxed_carrier(name)
            && let Some(head) = gaps.get_mut(target.0 as usize).and_then(|g| g.first_mut())
        {
            head.push(call_stmt(
                "gos_rt_option_slot_retain",
                vec![Operand::Copy(Place::local(destination.local))],
                span,
                &mut next_unit,
                &mut extra_locals,
            ));
        }
        // A combinator that answers its receiver's `Ok` / `Some` payload as it
        // is leaves that one blob named by two holders, each of which gives a
        // share back at its death, so the second holder takes a share of its
        // own. When the answer can instead be a payload a closure built, the
        // share is taken on the receiver before the call, where it names the
        // receiver's payload only.
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            target: Some(target),
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && option_holder(destination.local)
            && let Some((receivers, after)) = passthrough_receivers(name)
            && receivers.iter().all(|&at| {
                matches!(args.get(at), Some(Operand::Copy(carrier))
                    if carrier.projection.is_empty()
                        && (carrier.local.0 as usize) < n_locals
                        && (option_holder(carrier.local)
                            || ((1..=arity).contains(&(carrier.local.0 as usize))
                                && is_guarded_option(body.locals[carrier.local.0 as usize].ty))))
            })
        {
            if after {
                if let Some(head) = gaps.get_mut(target.0 as usize).and_then(|g| g.first_mut()) {
                    head.push(call_stmt(
                        "gos_rt_option_slot_retain_ok",
                        vec![Operand::Copy(Place::local(destination.local))],
                        span,
                        &mut next_unit,
                        &mut extra_locals,
                    ));
                }
            } else if let Some(Operand::Copy(carrier)) =
                receivers.first().and_then(|&at| args.get(at))
            {
                gaps[bi][len].push(call_stmt(
                    "gos_rt_option_slot_retain_ok",
                    vec![Operand::Copy(Place::local(carrier.local))],
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
        }
        if matches!(block.terminator, Terminator::Return) {
            for (l, sym) in &guarded_locals {
                gaps[bi][len].push(call_stmt(
                    "gos_rt_aggr_release_children",
                    walk_args(*l, sym),
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
            for l in &option_holders {
                gaps[bi][len].push(call_stmt(
                    "gos_rt_option_slot_release",
                    vec![Operand::Copy(Place::local(*l))],
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
        }
    }

    if extra_locals == 0 && guarded_locals.is_empty() && option_holders.is_empty() {
        return;
    }

    // Entry-block zeroing: guarded slots via the runtime walk, option
    // holders via a plain zero store (both {disc, payload} words).
    let mut entry_inits: Vec<Statement> = Vec::new();
    if let Some(first) = body.blocks.first() {
        let span = first.span;
        for (l, sym) in &guarded_locals {
            entry_inits.push(call_stmt(
                "gos_rt_aggr_zero_guarded",
                walk_args(*l, sym),
                span,
                &mut next_unit,
                &mut extra_locals,
            ));
        }
        for l in &option_holders {
            entry_inits.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(*l),
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                },
                span,
                inlined: None,
            });
        }
    }

    for _ in 0..extra_locals {
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }

    let n_blocks = body.blocks.len();
    for bi in 0..n_blocks {
        let orig: Vec<Statement> = std::mem::take(&mut body.blocks[bi].stmts);
        let block_gaps = std::mem::take(&mut gaps[bi]);
        let mut new_stmts: Vec<Statement> = Vec::with_capacity(orig.len() + 4);
        if bi == 0 {
            new_stmts.append(&mut entry_inits);
        }
        let mut orig_iter = orig.into_iter();
        for g in 0..block_gaps.len() {
            new_stmts.extend(block_gaps[g].iter().cloned());
            if let Some(stmt) = orig_iter.next() {
                new_stmts.push(stmt);
            }
        }
        body.blocks[bi].stmts = new_stmts;
    }
}
