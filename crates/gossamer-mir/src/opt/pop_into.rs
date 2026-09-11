/// The container pops whose multi-slot payload is a fresh heap copy of the
/// element, paired with the shim that moves the element into caller-owned
/// storage instead. Each `_into` form takes the storage as its trailing
/// argument and answers the `Option` discriminant the carrier would carry.
const POP_INTO_SHIMS: &[(&str, &str)] = &[
    ("gos_rt_vec_pop_opt", "gos_rt_vec_pop_into"),
    ("gos_rt_deque_pop_front", "gos_rt_deque_pop_front_into"),
    ("gos_rt_deque_pop_back", "gos_rt_deque_pop_back_into"),
    ("gos_rt_bheap_max_pop_desc", "gos_rt_bheap_max_pop_desc_into"),
    ("gos_rt_bheap_min_pop_desc", "gos_rt_bheap_min_pop_desc_into"),
];

/// Every local a place mentions: its root and each index local.
fn place_locals(place: &Place, out: &mut Vec<Local>) {
    out.push(place.local);
    for projection in &place.projection {
        if let Projection::Index(local) = projection {
            out.push(*local);
        }
    }
}

fn operand_locals(operand: &Operand, out: &mut Vec<Local>) {
    if let Operand::Copy(place) = operand {
        place_locals(place, out);
    }
}

/// Every local a statement reads or writes, with no attempt to tell the two
/// apart: the fusion below wants a local nothing else mentions at all.
fn statement_locals(stmt: &StatementKind, out: &mut Vec<Local>) {
    match stmt {
        StatementKind::Assign { place, rvalue } => {
            place_locals(place, out);
            match rvalue {
                Rvalue::Use(op) | Rvalue::UnaryOp { operand: op, .. } => operand_locals(op, out),
                Rvalue::Cast { operand, .. } | Rvalue::Repeat { value: operand, .. } => {
                    operand_locals(operand, out);
                }
                Rvalue::BinaryOp { lhs, rhs, .. } => {
                    operand_locals(lhs, out);
                    operand_locals(rhs, out);
                }
                Rvalue::Aggregate { operands, .. } | Rvalue::CallIntrinsic { args: operands, .. } => {
                    for op in operands {
                        operand_locals(op, out);
                    }
                }
                Rvalue::Len(place) | Rvalue::Ref { place, .. } => place_locals(place, out),
                Rvalue::StaticLoad(_) => {}
            }
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => out.push(*local),
        StatementKind::SetDiscriminant { place, .. } => place_locals(place, out),
        StatementKind::StaticStore { value, .. } => operand_locals(value, out),
        StatementKind::IterSource { dst, source, .. } => {
            place_locals(dst, out);
            operand_locals(source, out);
        }
        StatementKind::IterAdapter {
            dst,
            upstream,
            closure_or_arg,
            ..
        } => {
            place_locals(dst, out);
            place_locals(upstream, out);
            if let Some(op) = closure_or_arg {
                operand_locals(op, out);
            }
        }
        StatementKind::IterNext {
            dst_option,
            iter_place,
            ..
        } => {
            place_locals(dst_option, out);
            place_locals(iter_place, out);
        }
        StatementKind::Nop => {}
    }
}

fn terminator_locals(term: &Terminator, out: &mut Vec<Local>) {
    match term {
        Terminator::SwitchInt { discriminant, .. } => operand_locals(discriminant, out),
        Terminator::Call {
            callee,
            args,
            destination,
            ..
        } => {
            operand_locals(callee, out);
            for arg in args {
                operand_locals(arg, out);
            }
            place_locals(destination, out);
        }
        Terminator::Assert { cond, .. } => operand_locals(cond, out),
        Terminator::Drop { place, .. } => place_locals(place, out),
        Terminator::Goto { .. }
        | Terminator::Return
        | Terminator::Unreachable
        | Terminator::Panic { .. } => {}
    }
}

/// How many statements and terminators mention each local.
fn local_mention_counts(body: &Body) -> Vec<u32> {
    let mut counts = vec![0u32; body.locals.len()];
    let mut scratch = Vec::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            scratch.clear();
            statement_locals(&stmt.kind, &mut scratch);
            scratch.sort_unstable();
            scratch.dedup();
            for local in &scratch {
                counts[local.0 as usize] += 1;
            }
        }
        scratch.clear();
        terminator_locals(&block.terminator, &mut scratch);
        scratch.sort_unstable();
        scratch.dedup();
        for local in &scratch {
            counts[local.0 as usize] += 1;
        }
    }
    counts
}

/// The carrier read a statement performs, if it is one of the two the fusion
/// understands: `(carrier, destination, is_payload_extract)`.
fn carrier_read(stmt: &StatementKind) -> Option<(Local, Local, bool)> {
    let StatementKind::Assign {
        place,
        rvalue: Rvalue::CallIntrinsic { name, args },
    } = stmt
    else {
        return None;
    };
    if !place.projection.is_empty() || args.len() != 1 {
        return None;
    }
    let carrier = whole_copy_local(&args[0])?;
    match *name {
        "gos_rt_result_disc" => Some((carrier, place.local, false)),
        "gos_result_payload_owned" => Some((carrier, place.local, true)),
        _ => None,
    }
}

/// One pop rewritten to fill the local its payload is bound to.
struct PopFusion {
    pop_block: usize,
    into: &'static str,
    payload: Local,
    disc_local: Local,
}

/// Moves a popped all-scalar aggregate straight into the local that binds it.
///
/// A container pop answers `Option<T>` as a two-word carrier whose payload
/// word, for a multi-slot `T`, is a fresh heap copy of the element that the
/// owned extract then copies out and frees. Where the carrier is read only by
/// its discriminant and by one owned extract into a local written nowhere
/// else, and `T` owns no reference-counted child, the pop writes the element
/// into that local itself: the `_into` shim takes the local's storage,
/// answers the discriminant the carrier would have carried, and the extract
/// has nothing left to do.
pub(crate) fn pop_scalar_aggregates_in_place(body: &mut Body, tcx: &TyCtxt) {
    if body.locals.is_empty() || body.blocks.is_empty() {
        return;
    }
    let pops = pop_carriers(body);
    if pops.is_empty() {
        return;
    }
    let fusions = pop_fusions(body, tcx, &pops);
    if fusions.is_empty() {
        return;
    }
    for fusion in fusions.values() {
        let Terminator::Call {
            callee,
            args,
            destination,
            ..
        } = &mut body.blocks[fusion.pop_block].terminator
        else {
            continue;
        };
        *callee = Operand::Const(ConstValue::Str(fusion.into.to_string()));
        args.push(Operand::Copy(Place::local(fusion.payload)));
        *destination = Place::local(fusion.disc_local);
    }
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            let Some((carrier, dest, is_payload)) = carrier_read(&stmt.kind) else {
                continue;
            };
            let Some(fusion) = fusions.get(&carrier) else {
                continue;
            };
            stmt.kind = if is_payload {
                StatementKind::Nop
            } else {
                disc_copy(dest, fusion.disc_local)
            };
        }
    }
}

/// carrier local -> (block of the pop that writes it, its `_into` shim), for
/// every carrier exactly one pop writes.
fn pop_carriers(body: &Body) -> HashMap<Local, (usize, &'static str)> {
    let n_locals = body.locals.len();
    let mut pops: HashMap<Local, (usize, &'static str)> = HashMap::new();
    let mut doubled: Vec<Local> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            target: Some(_),
            ..
        } = &block.terminator
        else {
            continue;
        };
        let Some((_, into)) = POP_INTO_SHIMS.iter().find(|(from, _)| *from == name.as_str())
        else {
            continue;
        };
        if !destination.projection.is_empty() || destination.local.0 as usize >= n_locals {
            continue;
        }
        if pops.insert(destination.local, (bi, into)).is_some() {
            doubled.push(destination.local);
        }
    }
    for local in doubled {
        pops.remove(&local);
    }
    pops
}

/// The pops whose carrier is consumed by discriminant reads and one owned
/// extract alone, with the discriminant local each will answer into.
fn pop_fusions(
    body: &mut Body,
    tcx: &TyCtxt,
    pops: &HashMap<Local, (usize, &'static str)>,
) -> HashMap<Local, PopFusion> {
    let mentions = local_mention_counts(body);
    // carrier -> (disc reads, payload extract destinations)
    let mut reads: HashMap<Local, (u32, Vec<Local>)> = HashMap::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            let Some((carrier, dest, is_payload)) = carrier_read(&stmt.kind) else {
                continue;
            };
            if !pops.contains_key(&carrier) {
                continue;
            }
            let entry = reads.entry(carrier).or_insert((0, Vec::new()));
            if is_payload {
                entry.1.push(dest);
            } else {
                entry.0 += 1;
            }
        }
    }
    let mut fusions: HashMap<Local, PopFusion> = HashMap::new();
    for (carrier, (pop_block, into)) in pops {
        let Some((disc_reads, payloads)) = reads.get(carrier) else {
            continue;
        };
        let [payload] = payloads.as_slice() else {
            continue;
        };
        // The pop's own mention, every discriminant read, and the one extract
        // account for all of the carrier; anything else reads it another way.
        if mentions[carrier.0 as usize] != 1 + disc_reads + 1 {
            continue;
        }
        // The payload local is written by the extract alone, so the pop can
        // write it instead; it is also the storage the shim fills, so it must
        // be a whole aggregate local rather than a parameter or the return.
        if payload.0 == 0 || payload.0 <= body.arity || payload == carrier {
            continue;
        }
        if !tcx.is_scalar_inline_aggregate(body.local_ty(*payload)) {
            continue;
        }
        if local_write_count(body, *payload) != 1 {
            continue;
        }
        let disc_ty = disc_ty_of(body, *carrier);
        let disc_local = fresh_local(body, disc_ty);
        fusions.insert(
            *carrier,
            PopFusion {
                pop_block: *pop_block,
                into,
                payload: *payload,
                disc_local,
            },
        );
    }
    fusions
}

/// The type the discriminant read answers: that of the local it lands in.
fn disc_ty_of(body: &Body, carrier: Local) -> Ty {
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let Some((c, dest, false)) = carrier_read(&stmt.kind)
                && c == carrier
            {
                return body.local_ty(dest);
            }
        }
    }
    body.local_ty(carrier)
}

fn disc_copy(dest: Local, disc_local: Local) -> StatementKind {
    StatementKind::Assign {
        place: Place::local(dest),
        rvalue: Rvalue::Use(Operand::Copy(Place::local(disc_local))),
    }
}

/// How many statements and call terminators write the whole local.
fn local_write_count(body: &Body, local: Local) -> u32 {
    let mut count = 0;
    for block in &body.blocks {
        for stmt in &block.stmts {
            let written = match &stmt.kind {
                StatementKind::Assign { place, .. }
                | StatementKind::SetDiscriminant { place, .. }
                | StatementKind::IterSource { dst: place, .. }
                | StatementKind::IterAdapter { dst: place, .. }
                | StatementKind::IterNext {
                    dst_option: place, ..
                } => place.local == local,
                StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::StaticStore { .. }
                | StatementKind::Nop => false,
            };
            if written {
                count += 1;
            }
        }
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.local == local
        {
            count += 1;
        }
    }
    count
}
