//! Which parameters and fields a body only reads, and table value copies.

use super::*;

/// True when the body only READS a by-value aggregate parameter: nothing
/// writes its slots, its words are never copied out whole, and no value read
/// out of it reaches a call, container, reference, or global that keeps it
/// past the call.
///
/// Such a parameter needs no share of its own. The caller holds every field
/// for the whole call, so the frame's entry retain and its return release
/// cancel, and eliding the pair is what keeps a read-only accessor on a
/// `Map`-carrying struct proportional to the work it does: a `GosMap` has no
/// reference count, so its retain copies the entire table.
///
/// Handing the parameter whole to another Gossamer function is such a read:
/// the callee books its own share if it needs one, so a nested by-value `self`
/// costs the lookups it performs rather than a copy of the table they read.
///
/// `rc_fields` names the parameter's heap-managed field paths, and only a
/// place that can reach one of them is judged at all: a scalar field owns
/// nothing, so putting one in a tuple, a struct literal, or a container says
/// nothing about who owns the table beside it.
///
/// Conservative by construction - every use the walk does not recognise as a
/// plain read answers `false`, and the parameter keeps its own share.
pub(super) fn param_fields_only_read(body: &Body, p: Local, rc_fields: &AggFieldPaths) -> bool {
    use crate::ir::{Operand, Projection, Rvalue, StatementKind, Terminator};

    // True when a place rooted in `p` can reach one of its heap-managed
    // fields: the whole parameter can, a projection can when it lies on the
    // path to such a field or runs through one, and a projection this walk
    // cannot read as a field path is assumed to. A projection that reaches
    // only scalar slots carries nothing the frame could have to own, so the
    // uses below judge it as they judge an unrelated local.
    let reaches_rc_field = |projection: &[Projection]| {
        let mut path = Vec::with_capacity(projection.len());
        for step in projection {
            match step {
                Projection::Field(f) => path.push(*f),
                Projection::Discriminant => return false,
                _ => return true,
            }
        }
        rc_fields.iter().any(|(field, _)| {
            field.starts_with(path.as_slice()) || path.starts_with(field.as_slice())
        })
    };
    // A place rooted in `p` whose value shares one of its heap fields.
    let touches_rc = |pl: &crate::ir::Place| pl.local == p && reaches_rc_field(&pl.projection);

    // Locals holding a heap-managed value read out of `p`'s slots, through
    // bare copies. Whatever they reach, the parameter's field reaches.
    let mut carries: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut changed = true;
    while changed {
        changed = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                if !place.projection.is_empty() {
                    continue;
                }
                let Rvalue::Use(Operand::Copy(src)) = rvalue else {
                    continue;
                };
                let carried = if src.local == p {
                    !src.projection.is_empty() && reaches_rc_field(&src.projection)
                } else {
                    src.projection.is_empty() && carries.contains(&src.local.0)
                };
                if carried && carries.insert(place.local.0) {
                    changed = true;
                }
            }
        }
    }

    // Any read of one of the parameter's heap fields, or of a local carrying
    // one.
    let reads = |op: &Operand| match op {
        Operand::Copy(pl) => touches_rc(pl) || carries.contains(&pl.local.0),
        _ => false,
    };
    // The parameter's words copied out whole: the copy names the same heap
    // values under a second owner, and every sharing rule downstream keys on
    // one, so the frame must own what it hands over.
    let copies_whole =
        |op: &Operand| matches!(op, Operand::Copy(pl) if pl.local == p && pl.projection.is_empty());
    // A call that keeps what it is handed, or one whose callee this walk
    // cannot name.
    let keeps_args = |callee: &Operand| match callee {
        Operand::Const(ConstValue::Str(name)) => {
            is_consuming_call(name) || stores_aggregate_by_pointer(name)
        }
        Operand::FnRef { .. } => false,
        _ => true,
    };
    // A Gossamer callee, as opposed to a runtime symbol. Its own drop pass
    // books whatever share its by-value aggregate parameter needs, into
    // parameter storage that is its frame's rather than the argument's, so
    // handing the whole parameter to one transfers no ownership: this frame
    // outlives the nested call, and whoever owns the fields still owns them
    // across it. A runtime symbol's contract is per-symbol instead, so a
    // whole hand-over to one keeps the share.
    let user_callee = |callee: &Operand| match callee {
        Operand::FnRef { .. } => true,
        Operand::Const(ConstValue::Str(name)) => {
            !name.starts_with("gos_rt_") && name != "gos_load" && name != "gos_store"
        }
        _ => false,
    };

    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    if place.local == p {
                        return false;
                    }
                    // A store through a projection hands the value to whatever
                    // the destination is part of.
                    let stores_into_slot = !place.projection.is_empty();
                    match rvalue {
                        Rvalue::Use(op) => {
                            if copies_whole(op) || (stores_into_slot && reads(op)) {
                                return false;
                            }
                        }
                        Rvalue::UnaryOp { operand: op, .. }
                        | Rvalue::Cast { operand: op, .. }
                        | Rvalue::Repeat { value: op, .. } => {
                            if copies_whole(op) || (stores_into_slot && reads(op)) {
                                return false;
                            }
                        }
                        Rvalue::BinaryOp { lhs, rhs, .. } => {
                            if [lhs, rhs]
                                .iter()
                                .any(|op| copies_whole(op) || (stores_into_slot && reads(op)))
                            {
                                return false;
                            }
                        }
                        Rvalue::Len(_) | Rvalue::StaticLoad(_) => {}
                        Rvalue::Ref { place: pl, .. } => {
                            if pl.local == p || carries.contains(&pl.local.0) {
                                return false;
                            }
                        }
                        Rvalue::Aggregate { operands, .. } => {
                            if operands.iter().any(&reads) {
                                return false;
                            }
                        }
                        Rvalue::CallIntrinsic { name, args } => {
                            // The field helpers take the ADDRESS of the place
                            // they are handed and write the slot back, so one
                            // over the parameter is a store, not a read.
                            let keeps = is_consuming_call(name)
                                || stores_aggregate_by_pointer(name)
                                || name.starts_with("gos_store")
                                || name.contains("_field_clone")
                                || name.contains("_field_release");
                            if args.iter().any(|op| {
                                copies_whole(op) || ((keeps || stores_into_slot) && reads(op))
                            }) {
                                return false;
                            }
                        }
                    }
                }
                StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
                StatementKind::SetDiscriminant { place, .. } => {
                    if place.local == p {
                        return false;
                    }
                }
                StatementKind::StaticStore { value, .. } => {
                    if reads(value) {
                        return false;
                    }
                }
                StatementKind::IterSource { dst, source, .. } => {
                    if dst.local == p || reads(source) {
                        return false;
                    }
                }
                StatementKind::IterAdapter {
                    dst,
                    upstream,
                    closure_or_arg,
                    ..
                } => {
                    if dst.local == p
                        || upstream.local == p
                        || carries.contains(&upstream.local.0)
                        || closure_or_arg.as_ref().is_some_and(&reads)
                    {
                        return false;
                    }
                }
                StatementKind::IterNext {
                    dst_option,
                    iter_place,
                    ..
                } => {
                    if dst_option.local == p
                        || iter_place.local == p
                        || carries.contains(&iter_place.local.0)
                    {
                        return false;
                    }
                }
            }
        }
        match &block.terminator {
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                if destination.local == p {
                    return false;
                }
                let keeps = keeps_args(callee);
                let hands_over = !user_callee(callee);
                if args
                    .iter()
                    .any(|op| (hands_over && copies_whole(op)) || (keeps && reads(op)))
                {
                    return false;
                }
            }
            Terminator::Drop { place, .. } => {
                if place.local == p || carries.contains(&place.local.0) {
                    return false;
                }
            }
            Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::SwitchInt { .. }
            | Terminator::Assert { .. }
            | Terminator::Unreachable
            | Terminator::Panic { .. } => {}
        }
    }
    true
}

/// A call the second argument's heap ownership moves through: a container
/// push, whatever container it is. The element store owns the pushed value
/// from then on, so the frame must not free it independently.
pub(crate) fn is_element_push(name: &str) -> bool {
    name.starts_with("gos_rt_vec_push")
        || name.starts_with("gos_rt_deque_push")
        || (name.starts_with("gos_rt_bheap_") && name.contains("_push"))
}

/// Consuming calls that mint the container's own share of a stored `Vec` and
/// give it back at the container's teardown.
///
/// The exchange is balanced on both sides, so the frame keeps the release of
/// the sequence it built and reclaims it per site rather than only at the
/// return - which is what a container filled in a loop needs.
pub(super) fn stores_owned_vec_value(name: &str) -> bool {
    is_element_push(name)
        || name.starts_with("gos_rt_map_insert")
        || name.starts_with("gos_rt_map_or_insert")
        || name.starts_with("gos_rt_omap_insert")
        || name.starts_with("gos_rt_set_insert")
        || name.starts_with("gos_rt_ovec_insert")
        || name.starts_with("gos_rt_vec_insert")
}

/// Consuming calls whose container keeps the ARGUMENT'S OWN aggregate word,
/// so a struct argument's heap fields need a share for the stored entry.
///
/// A hash container stores the word it is handed. A sequence container copies
/// the element's slots into its own storage and retains their heap children
/// itself, so a share minted here would never be given back - the entry reads
/// correctly either way, and the extra count leaks. Membership is therefore
/// evidence-driven: a container belongs here only where dropping the share
/// leaves a stored entry reading freed memory.
pub(super) fn stores_aggregate_by_pointer(name: &str) -> bool {
    name.starts_with("gos_rt_map_insert")
        || name.starts_with("gos_rt_map_or_insert")
        || name.starts_with("gos_rt_omap_insert")
        || name.starts_with("gos_rt_set_insert")
        || name.starts_with("gos_rt_chan_send")
}

/// `true` when `local`, an argument of the consuming call `name`, is a value
/// the receiving container stores a copy of, so the frame keeps its own: a
/// `Map`, `Set`, or deque value of a map that owns such values entry by entry
/// (`gos_rt_map_set_map_values` and kin), or a `Set`, deque, or heap element
/// of a vector or deque, whose store copies each one into its slot.
pub(super) fn stores_table_value_copy(
    tcx: &gossamer_types::TyCtxt,
    body: &Body,
    name: &str,
    args: &[Operand],
    local: Local,
) -> bool {
    use gossamer_types::TyKind;
    let Some(Operand::Copy(receiver)) = args.first() else {
        return false;
    };
    if !receiver.projection.is_empty() {
        return false;
    }
    let mut map_ty = body.locals[receiver.local.0 as usize].ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(map_ty) {
        map_ty = *inner;
    }
    let element_store = name.starts_with("gos_rt_vec_push")
        || name.starts_with("gos_rt_deque_push")
        || name.starts_with("gos_rt_vec_set")
        || name.starts_with("gos_rt_vec_insert");
    if element_store {
        let is_value = matches!(args.last(), Some(Operand::Copy(p)) if p.local == local);
        let elem = match tcx.kind_of(map_ty) {
            TyKind::Vec(elem) | TyKind::Slice(elem) => Some(*elem),
            TyKind::Adt { substs, .. } if handle_container(tcx, map_ty).is_some() => {
                substs.types().first().copied()
            }
            _ => None,
        };
        return is_value && elem.is_some_and(|elem| handle_container(tcx, elem).is_some());
    }
    if !(name.starts_with("gos_rt_map_insert") || name.starts_with("gos_rt_map_or_insert")) {
        return false;
    }
    let TyKind::HashMap { value, .. } = tcx.kind_of(map_ty) else {
        return false;
    };
    let table_value = matches!(tcx.kind_of(*value), TyKind::HashMap { .. })
        || matches!(
            handle_container(tcx, *value),
            Some(HandleContainer::Set | HandleContainer::Deque)
        );
    table_value && matches!(args.last(), Some(Operand::Copy(p)) if p.local == local)
}

/// Gives a container stored into another container a value of its own.
///
/// `v.push(x)`, `xs[i] = x`, `m.insert(k, x)`, and their kin keep what they
/// are handed, but a container a binding names is still the binding's: a
/// write through the binding afterwards must not reach the stored element.
/// A `Vec` store takes a share and a `Map`, `Set`, or deque store the handle
/// itself, and neither survives an in-place write, so the stored value is a
/// copy. A copy of a binding the store is the last use of is handed over
/// instead by the uniqueness pass, so a value built and stored in one go pays
/// nothing. A map that owns its table values copies them itself.
pub(crate) fn copy_stored_containers(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;
    let stores_value = |name: &str| {
        name.starts_with("gos_rt_vec_push")
            || name.starts_with("gos_rt_deque_push")
            || matches!(
                name,
                "gos_rt_vec_set" | "gos_rt_vec_set_i64" | "gos_rt_vec_set_i64_unchecked"
            )
            || name.starts_with("gos_rt_vec_insert")
            || name.starts_with("gos_rt_map_insert")
            || name.starts_with("gos_rt_map_or_insert")
    };
    let n_blocks = body.blocks.len();
    for bi in 0..n_blocks {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } = &body.blocks[bi].terminator
        else {
            continue;
        };
        if !stores_value(name) || args.len() < 2 {
            continue;
        }
        let value_index = args.len() - 1;
        let (Some(Operand::Copy(receiver)), Some(Operand::Copy(value))) =
            (args.first(), args.get(value_index))
        else {
            continue;
        };
        if !receiver.projection.is_empty() || !value.projection.is_empty() {
            continue;
        }
        let value_local = value.local;
        let decl = &body.locals[value_local.0 as usize];
        // A binding or a parameter names a value something else can still
        // reach; a temporary the lowering made for this store does not.
        if decl.debug_name.is_none() || decl.region {
            continue;
        }
        let mut container = body.locals[receiver.local.0 as usize].ty;
        while let TyKind::Ref { inner, .. } = tcx.kind_of(container) {
            container = *inner;
        }
        let elem = match tcx.kind_of(container) {
            TyKind::Vec(elem) | TyKind::Slice(elem) => *elem,
            TyKind::HashMap { value, .. } => *value,
            TyKind::Adt { substs, .. } if handle_container(tcx, container).is_some() => {
                match substs.types().first() {
                    Some(elem) => *elem,
                    None => continue,
                }
            }
            _ => continue,
        };
        let symbol = match tcx.kind_of(elem) {
            TyKind::Vec(_) | TyKind::Slice(_) => "gos_rt_vec_clone",
            TyKind::HashMap { .. } => "gos_rt_map_clone",
            _ => match handle_container(tcx, elem) {
                Some(HandleContainer::Set) => "gos_rt_set_clone",
                Some(HandleContainer::Deque) => "gos_rt_deque_clone",
                Some(HandleContainer::Heap) => "gos_rt_vec_clone",
                None => continue,
            },
        };
        let args_now = match &body.blocks[bi].terminator {
            Terminator::Call { args, .. } => args.clone(),
            _ => continue,
        };
        if stores_table_value_copy(tcx, body, name, &args_now, value_local) {
            continue;
        }
        let copy_ty = if matches!(
            tcx.kind_of(decl.ty),
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::HashMap { .. }
        ) || handle_container(tcx, decl.ty).is_some()
        {
            decl.ty
        } else {
            elem
        };
        let copy = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(crate::ir::LocalDecl {
            ty: copy_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let store_id = BlockId(u32::try_from(body.blocks.len()).expect("block overflow"));
        let block = &mut body.blocks[bi];
        let span = block.terminator_span.unwrap_or(block.span);
        let mut store = std::mem::replace(
            &mut block.terminator,
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(symbol.to_string())),
                args: vec![Operand::Copy(Place::local(value_local))],
                destination: Place::local(copy),
                target: Some(store_id),
            },
        );
        if let Terminator::Call { args, .. } = &mut store {
            args[value_index] = Operand::Copy(Place::local(copy));
        }
        let terminator_span = block.terminator_span;
        let terminator_inlined = block.terminator_inlined.clone();
        body.blocks.push(crate::ir::BasicBlock {
            id: store_id,
            stmts: Vec::new(),
            terminator: store,
            span,
            terminator_span,
            terminator_inlined,
        });
    }
}

pub(crate) fn is_consuming_call(name: &str) -> bool {
    is_element_push(name)
        // `xs[i] = v` writes the value into the element store, which owns its
        // elements from then on, so the store mints the container's share the
        // way a push does.
        || name.starts_with("gos_rt_vec_set_i64")
        || name.starts_with("gos_rt_vec_set_i128")
        || name.starts_with("gos_rt_vec_insert")
        || name.starts_with("gos_rt_set_insert")
        || name.starts_with("gos_rt_map_insert")
        // `HashMap::or_insert` consumes its key and, on an absent key,
        // stores the supplied value. The retained value share becomes the
        // map's ownership; the returned value is separately marked as an
        // interior borrow by `returns_borrowed_pointer`.
        || name.starts_with("gos_rt_map_or_insert")
        || name.starts_with("gos_rt_omap_insert")
        || name.starts_with("gos_rt_ovec_insert")
        || name.starts_with("gos_rt_chan_send")
        // `option.ok_or(err)` packs the error word into the carrier it
        // answers without taking a share of its own, so the carrier's
        // payload release is the error cell's only give-back.
        || name == "gos_rt_result_ok_or"
}

/// Picks the retain/release runtime helper for a heap value by its type. Vecs
/// carry no RC header, so they route through the Vec allocator's reference
/// count (`gos_rt_vec_retain` / `gos_rt_vec_free`); `Weak<T>` routes through the
/// weak helpers; compiler-typed strings route through typed string helpers so
/// generated cleanup does not lock the public raw-string registry; everything
/// else uses the generic `gos_rt_rc_retain` / `gos_rt_rc_release`.
pub(super) fn rc_helper(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    is_retain: bool,
) -> &'static str {
    use gossamer_types::TyKind;
    match tcx.kind_of(ty) {
        TyKind::String => {
            if is_retain {
                "gos_rt_str_retain_typed"
            } else {
                "gos_rt_str_free_typed"
            }
        }
        // A whole-local `Array` gap only arises for vec-carried arrays
        // (monomorphised `[T; N]` parameters); inline fixed arrays never
        // enter the retain/release schedule.
        TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } => {
            if is_retain {
                "gos_rt_vec_retain"
            } else {
                "gos_rt_vec_free"
            }
        }
        _ if tcx.is_weak_ty(ty) => {
            if is_retain {
                "gos_rt_rc_weak_retain"
            } else {
                "gos_rt_rc_weak_release"
            }
        }
        TyKind::Iterator(item) => {
            let pair = lazy_iter_is_pair_state(tcx, *item);
            match (is_retain, pair) {
                (true, false) => "gos_rt_lazy_iter_retain_i64",
                (true, true) => "gos_rt_lazy_iter_retain_pair_i64",
                (false, false) => "gos_rt_lazy_iter_drop_i64",
                (false, true) => "gos_rt_lazy_iter_drop_pair_i64",
            }
        }
        _ => {
            if is_retain {
                "gos_rt_rc_retain"
            } else {
                "gos_rt_rc_release"
            }
        }
    }
}
