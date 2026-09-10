/// Capacity planning only applies to a statically known `HashMap` local.
fn is_hashmap_local(body: &Body, tcx: &TyCtxt, local: Local) -> bool {
    let Some(decl) = body.locals.get(local.0 as usize) else {
        return false;
    };
    matches!(tcx.kind_of(decl.ty), TyKind::HashMap { .. })
}

fn reserve_bound_available_at_entry(body: &Body, bound: &Operand, allocation: BlockId) -> bool {
    match bound {
        Operand::Const(_) => true,
        Operand::Copy(place) if place.projection.is_empty() => {
            if place.local.0 == 0 {
                return false;
            }
            // Parameters exist at function entry. A local must instead have
            // one whole-local definition that dominates the constructor; a
            // second write anywhere in the body means a loop back-edge or a
            // branch can change the reserve bound, so remain conservative.
            place.local.0 <= body.arity
                || single_local_definition(body, place.local)
                    .is_some_and(|definition| block_dominates(body, definition, allocation))
        }
        _ => false,
    }
}

/// Returns the sole block that defines `local`, counting both statement and
/// call-result writes. Projection writes count too: a reserve bound must be
/// immutable for the whole loop, not merely free of direct scalar writes.
fn single_local_definition(body: &Body, local: Local) -> Option<BlockId> {
    let mut definition = None;
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.local == local
            {
                if definition.replace(block.id).is_some() {
                    return None;
                }
            }
        }
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.local == local
        {
            if definition.replace(block.id).is_some() {
                return None;
            }
        }
    }
    definition
}

/// True when every entry-to-`target` path passes through `gate`.
fn block_dominates(body: &Body, gate: BlockId, target: BlockId) -> bool {
    let Some(entry) = body.blocks.first().map(|block| block.id) else {
        return false;
    };
    if gate == target || gate == entry {
        return true;
    }
    let mut seen = HashSet::from([entry]);
    let mut work = VecDeque::from([entry]);
    while let Some(block_id) = work.pop_front() {
        if block_id == gate {
            continue;
        }
        if block_id == target {
            return false;
        }
        let Some(block) = body.blocks.get(block_id.0 as usize) else {
            return false;
        };
        for successor in terminator_successors(&block.terminator) {
            if successor != gate && seen.insert(successor) {
                work.push_back(successor);
            }
        }
    }
    true
}

fn counted_push_loop_bound(
    body: &Body,
    allocation: BlockId,
    start: BlockId,
    vec_local: Local,
) -> Option<Operand> {
    let mut seen = HashSet::new();
    let mut work = VecDeque::from([(start, 0usize)]);
    while let Some((bid, depth)) = work.pop_front() {
        if depth > 64 || !seen.insert(bid) {
            continue;
        }
        let block = body.blocks.get(bid.0 as usize)?;
        if let Some((body_entry, bound)) = counted_loop_body_and_bound(block)
            && allocation_precedes_loop(body, allocation, bid)
            && loop_body_has_exactly_one_vec_push(body, body_entry, bid, vec_local)
        {
            return Some(bound);
        }
        for succ in terminator_successors(&block.terminator) {
            work.push_back((succ, depth + 1));
        }
    }
    None
}

fn counted_insert_loop_bound(
    body: &Body,
    allocation: BlockId,
    start: BlockId,
    map_local: Local,
) -> Option<Operand> {
    let mut seen = HashSet::new();
    let mut work = VecDeque::from([(start, 0usize)]);
    while let Some((bid, depth)) = work.pop_front() {
        if depth > 64 || !seen.insert(bid) {
            continue;
        }
        let block = body.blocks.get(bid.0 as usize)?;
        if let Some((body_entry, bound)) = counted_loop_body_and_bound(block)
            && allocation_precedes_loop(body, allocation, bid)
            && loop_body_has_exactly_one_map_insert(body, body_entry, bid, map_local)
        {
            return Some(bound);
        }
        for succ in terminator_successors(&block.terminator) {
            work.push_back((succ, depth + 1));
        }
    }
    None
}

/// Whether the container is built once, ahead of the loop that fills it.
/// The reserve reasons about the whole loop, so it is only a capacity when
/// the allocation runs before the loop head - an allocation the loop body
/// itself performs runs once per iteration, and reserving the trip count
/// there sizes every one of those containers for the entire loop.
fn allocation_precedes_loop(body: &Body, allocation: BlockId, loop_head: BlockId) -> bool {
    block_dominates(body, allocation, loop_head)
}

fn counted_loop_body_and_bound(block: &BasicBlock) -> Option<(BlockId, Operand)> {
    let Terminator::SwitchInt {
        discriminant,
        arms,
        default,
    } = &block.terminator
    else {
        return None;
    };
    let cond_local = whole_copy_local(discriminant)?;
    let body_entry = *default;
    if !arms
        .iter()
        .any(|(value, target)| *value == 0 && *target != body_entry)
    {
        return None;
    }
    for stmt in &block.stmts {
        let StatementKind::Assign { place, rvalue } = &stmt.kind else {
            continue;
        };
        if place.local != cond_local || !place.projection.is_empty() {
            continue;
        }
        let Rvalue::BinaryOp {
            op: BinOp::Lt,
            lhs,
            rhs,
        } = rvalue
        else {
            continue;
        };
        if whole_copy_local(lhs).is_some() {
            return Some((body_entry, rhs.clone()));
        }
    }
    None
}

/// Proves that every loop-body path reaches the loop head after exactly one
/// push into `vec_local`. A reserve is only exact under that condition: seeing
/// one push somewhere is insufficient when another branch skips it or pushes
/// twice. Loops inside the candidate body and paths which leave the body are
/// deliberately rejected rather than guessed about.
fn loop_body_has_exactly_one_vec_push(
    body: &Body,
    entry: BlockId,
    loop_head: BlockId,
    vec_local: Local,
) -> bool {
    let mut seen = HashSet::new();
    let mut work = VecDeque::from([(entry, 0u8)]);
    let mut reached_head = false;
    while let Some((bid, pushes)) = work.pop_front() {
        if bid == loop_head {
            if pushes != 1 {
                return false;
            }
            reached_head = true;
            continue;
        }
        if !seen.insert((bid, pushes)) {
            // A cycle in the candidate body makes the number of pushes
            // unbounded or path-dependent, so it is not an exact reserve.
            return false;
        }
        let Some(block) = body.blocks.get(bid.0 as usize) else {
            return false;
        };
        let pushes = pushes.saturating_add(u8::from(terminator_pushes_vec(
            &block.terminator,
            vec_local,
        )));
        if pushes > 1 {
            return false;
        }
        let successors = terminator_successors(&block.terminator);
        if successors.is_empty() {
            return false;
        }
        for successor in successors {
            work.push_back((successor, pushes));
        }
    }
    reached_head
}

fn terminator_pushes_vec(term: &Terminator, vec_local: Local) -> bool {
    let Terminator::Call { callee, args, .. } = term else {
        return false;
    };
    if !matches!(callee, Operand::Const(ConstValue::Str(name)) if name == "gos_rt_vec_push" || name == "gos_rt_vec_push_i64")
    {
        return false;
    }
    args.first().and_then(whole_copy_local) == Some(vec_local)
}

fn loop_body_has_exactly_one_map_insert(
    body: &Body,
    entry: BlockId,
    loop_head: BlockId,
    map_local: Local,
) -> bool {
    let mut seen = HashSet::new();
    let mut work = VecDeque::from([(entry, 0u8)]);
    let mut reached_head = false;
    while let Some((bid, inserts)) = work.pop_front() {
        if bid == loop_head {
            if inserts != 1 {
                return false;
            }
            reached_head = true;
            continue;
        }
        if !seen.insert((bid, inserts)) {
            return false;
        }
        let Some(block) = body.blocks.get(bid.0 as usize) else {
            return false;
        };
        let inserts = inserts.saturating_add(u8::from(terminator_inserts_map(
            &block.terminator,
            map_local,
        )));
        if inserts > 1 {
            return false;
        }
        let successors = terminator_successors(&block.terminator);
        if successors.is_empty() {
            return false;
        }
        for successor in successors {
            work.push_back((successor, inserts));
        }
    }
    reached_head
}

fn terminator_inserts_map(term: &Terminator, map_local: Local) -> bool {
    let Terminator::Call { callee, args, .. } = term else {
        return false;
    };
    if !matches!(callee, Operand::Const(ConstValue::Str(name)) if matches!(name.as_str(),
        "gos_rt_map_insert"
            | "gos_rt_map_insert_i64_i64"
            | "gos_rt_map_insert_i64_i64_opt"
            | "gos_rt_map_insert_str_i64"
            | "gos_rt_map_insert_str_i64_opt"
            | "gos_rt_map_insert_typed_str_i64"
            | "gos_rt_map_insert_typed_str_i64_opt"
            | "gos_rt_map_insert_i64_str"
            | "gos_rt_map_insert_i64_str_opt"
            | "gos_rt_map_insert_str_str"
            | "gos_rt_map_insert_str_str_opt"
            | "gos_rt_map_insert_skey"
            | "gos_rt_map_insert_skey_opt"))
    {
        return false;
    }
    args.first().and_then(whole_copy_local) == Some(map_local)
}

fn terminator_successors(term: &Terminator) -> Vec<BlockId> {
    match term {
        Terminator::Goto { target } => vec![*target],
        Terminator::SwitchInt { arms, default, .. } => {
            let mut out = Vec::with_capacity(arms.len() + 1);
            out.push(*default);
            out.extend(arms.iter().map(|(_, target)| *target));
            out
        }
        Terminator::Assert { target, .. } => vec![*target],
        Terminator::Call {
            target: Some(target),
            ..
        } => vec![*target],
        Terminator::Return
        | Terminator::Unreachable
        | Terminator::Panic { .. }
        | Terminator::Drop { .. }
        | Terminator::Call { target: None, .. } => Vec::new(),
    }
}

/// Fuses `strings::slice(input,start,end)?` immediately consumed by
/// `strconv::parse_i64` / `parse_f64` into a range parse helper. The helper
/// performs the same range and UTF-8 validation as `strings::slice`, then
/// parses the borrowed bytes without allocating a temporary runtime `String`.
///
/// This runs after drop insertion, when `?` has been lowered to result-disc /
/// payload blocks. It only fires when the unwrapped string local is otherwise
/// used for parse/release/zeroing, so user-visible `String` values still
/// materialise normally.
pub(crate) fn fuse_slice_parse_ranges(body: &mut Body) {
    let slice_by_result = slice_calls_by_result(body);
    if slice_by_result.is_empty() {
        return;
    }

    let mut payload_defs: HashMap<Local, Local> = HashMap::new();
    let mut copy_defs: HashMap<Local, Option<Local>> = HashMap::new();
    let mut bindings: Vec<(Local, usize, Local)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if !place.projection.is_empty() {
                continue;
            }
            match rvalue {
                Rvalue::CallIntrinsic { name, args }
                    if *name == "gos_rt_result_payload" && args.len() == 1 =>
                {
                    if let Some(src) = whole_copy_local(&args[0]) {
                        payload_defs.insert(place.local, src);
                    }
                }
                Rvalue::Use(op) => {
                    if let Some(src) = whole_copy_local(op) {
                        let prior = copy_defs.insert(place.local, Some(src));
                        if prior.is_some() {
                            copy_defs.insert(place.local, None);
                        }
                        bindings.push((place.local, bi, src));
                    }
                }
                _ => {}
            }
        }
    }

    let mut candidates: HashMap<Local, (usize, Vec<Operand>, usize)> = HashMap::new();
    for (str_local, bind_block, src) in bindings {
        let Some(result_local) = trace_payload_result(src, &copy_defs, &payload_defs) else {
            continue;
        };
        let Some((slice_block, range_args)) = slice_by_result.get(&result_local) else {
            continue;
        };
        if slice_parse_local_is_private(body, str_local) {
            candidates.insert(str_local, (*slice_block, range_args.clone(), bind_block));
        }
    }
    if candidates.is_empty() {
        return;
    }

    let mut changed: HashMap<Local, bool> = HashMap::new();
    for block in &mut body.blocks {
        let Terminator::Call { callee, args, .. } = &mut block.terminator else {
            continue;
        };
        let Some(str_local) = args.first().and_then(whole_copy_local) else {
            continue;
        };
        let Some((_, range_args, _)) = candidates.get(&str_local) else {
            continue;
        };
        let Operand::Const(ConstValue::Str(name)) = callee else {
            continue;
        };
        let helper = match name.as_str() {
            "gos_rt_strconv_parse_i64" => "gos_rt_strconv_parse_i64_range",
            "gos_rt_strconv_parse_f64" => "gos_rt_strconv_parse_f64_range",
            _ => continue,
        };
        *name = helper.to_string();
        args.clone_from(range_args);
        changed.insert(str_local, true);
    }

    for (str_local, (slice_block, _, bind_block)) in candidates {
        if !changed.get(&str_local).copied().unwrap_or(false) {
            continue;
        }
        let target = body.blocks[bind_block].id;
        body.blocks[slice_block].terminator = Terminator::Goto { target };
        for stmt in &mut body.blocks[bind_block].stmts {
            if assigns_whole_local(stmt, str_local) {
                stmt.kind = StatementKind::Nop;
            }
        }
    }
}

fn slice_calls_by_result(body: &Body) -> HashMap<Local, (usize, Vec<Operand>)> {
    let mut slice_by_result = HashMap::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee,
            args,
            destination,
            target,
        } = &block.terminator
        else {
            continue;
        };
        if *target == Some(block.id) || args.len() != 3 {
            continue;
        }
        if !matches!(callee, Operand::Const(ConstValue::Str(name)) if name == "gos_rt_str_slice") {
            continue;
        }
        if destination.projection.is_empty() {
            slice_by_result.insert(destination.local, (bi, args.clone()));
        }
    }
    slice_by_result
}

fn whole_copy_local(op: &Operand) -> Option<Local> {
    match op {
        Operand::Copy(p) if p.projection.is_empty() => Some(p.local),
        _ => None,
    }
}

fn trace_payload_result(
    mut local: Local,
    copy_defs: &HashMap<Local, Option<Local>>,
    payload_defs: &HashMap<Local, Local>,
) -> Option<Local> {
    for _ in 0..16 {
        if let Some(result) = payload_defs.get(&local) {
            return Some(*result);
        }
        match copy_defs.get(&local).copied().flatten() {
            Some(next) if next != local => local = next,
            _ => return None,
        }
    }
    None
}

fn assigns_whole_local(stmt: &Statement, local: Local) -> bool {
    matches!(
        &stmt.kind,
        StatementKind::Assign { place, .. } if place.local == local && place.projection.is_empty()
    )
}

fn slice_parse_local_is_private(body: &Body, local: Local) -> bool {
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.local == local && place.projection.is_empty() {
                match rvalue {
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))) => {}
                    Rvalue::Use(Operand::Copy(_)) => {}
                    _ => return false,
                }
            } else if rvalue_mentions_local(rvalue, local) {
                if !matches!(
                    rvalue,
                    Rvalue::CallIntrinsic { name, args }
                        if *name == "gos_rt_rc_release"
                            && args.len() == 1
                            && whole_copy_local(&args[0]) == Some(local)
                ) {
                    return false;
                }
            }
        }
        if terminator_mentions_local_forbidden(&block.terminator, local) {
            return false;
        }
    }
    true
}

fn terminator_mentions_local_forbidden(term: &Terminator, local: Local) -> bool {
    match term {
        Terminator::Call { callee, args, .. } => {
            let is_parse = matches!(
                callee,
                Operand::Const(ConstValue::Str(name))
                    if name == "gos_rt_strconv_parse_i64" || name == "gos_rt_strconv_parse_f64"
            );
            for (idx, arg) in args.iter().enumerate() {
                if operand_mentions_local(arg, local) && !(is_parse && idx == 0) {
                    return true;
                }
            }
            operand_mentions_local(callee, local)
        }
        Terminator::SwitchInt { discriminant, .. } => operand_mentions_local(discriminant, local),
        Terminator::Assert { cond, .. } => operand_mentions_local(cond, local),
        Terminator::Drop { place, .. } => place_mentions_local(place, local),
        Terminator::Goto { .. }
        | Terminator::Return
        | Terminator::Unreachable
        | Terminator::Panic { .. } => false,
    }
}
/// Turns the deep clone introduced for a source-level three-way Vec swap
/// back into an ownership-preserving pointer move:
///
/// ```text
/// tmp = clone(left); left = right; right = tmp
/// ```
///
/// A normal `let tmp = left` must retain value semantics because either
/// binding may subsequently mutate independently. In this exact permutation,
/// however, `left` is overwritten before `tmp` is observed, so cloning the
/// full buffer is unnecessary. Radix-sort's per-pass ping-pong swap otherwise
/// copied the entire input on every pass and retained those copies until
/// function exit.
pub(crate) fn elide_vec_clone_in_three_way_swaps(body: &mut Body) {
    let mut rewrites = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            target: Some(target),
        } = &block.terminator
        else {
            continue;
        };
        if name != "gos_rt_vec_clone"
            || args.len() != 1
            || !destination.projection.is_empty()
        {
            continue;
        }
        let Operand::Copy(source) = &args[0] else {
            continue;
        };
        if !source.projection.is_empty() {
            continue;
        }
        let Some(next) = body.blocks.get(target.0 as usize) else {
            continue;
        };
        let mut copies = next.stmts.iter().filter_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(source)),
            } if place.projection.is_empty() && source.projection.is_empty() => {
                Some((place, source))
            }
            _ => None,
        });
        let (Some((first_dst, first_src)), Some((second_dst, second_src))) =
            (copies.next(), copies.next())
        else {
            continue;
        };
        if first_dst.local == source.local
            && second_dst.local == first_src.local
            && second_src.local == destination.local
            && source.local != first_src.local
        {
            rewrites.push((bi, source.clone(), destination.clone(), *target, block.span));
        }
    }
    for (bi, source, destination, target, span) in rewrites {
        body.blocks[bi].stmts.push(Statement {
            kind: StatementKind::Assign {
                place: destination,
                rvalue: Rvalue::Use(Operand::Copy(source)),
            },
            span,
        });
        body.blocks[bi].terminator = Terminator::Goto { target };
    }
}

fn fresh_vec_temporary_sources(body: &Body) -> HashSet<Local> {
    let mut out = HashSet::new();
    for block in &body.blocks {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
        else {
            continue;
        };
        if !matches!(
            name.as_str(),
            "gos_rt_vec_from_arr"
                | "gos_rt_vec_from_packed_arr"
                | "gos_rt_vec_repeat_primitive"
                | "gos_rt_nested_arr_to_vec"
        ) || !destination.projection.is_empty()
        {
            continue;
        }
        let local = destination.local;
        if body
            .locals
            .get(local.0 as usize)
            .is_some_and(|decl| decl.debug_name.is_none())
        {
            out.insert(local);
        }
    }
    let mut result_vec_sources = HashSet::new();
    for block in &body.blocks {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
        else {
            continue;
        };
        if !matches!(
            name.as_str(),
            "gos_rt_vec_slice_result"
                | "gos_rt_intarr_slice_result"
                | "gos_rt_floatarr_slice_result"
                | "gos_rt_bytearr_slice_result"
                | "gos_rt_packed_bytearr_slice_result"
        ) || !destination.projection.is_empty()
        {
            continue;
        }
        result_vec_sources.insert(destination.local);
    }
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if !place.projection.is_empty() {
                continue;
            }
            if let Rvalue::CallIntrinsic { name, args } = rvalue
                && *name == "gos_rt_result_payload"
                && matches!(
                    args.as_slice(),
                    [Operand::Copy(source)]
                        if source.projection.is_empty()
                            && result_vec_sources.contains(&source.local)
                )
            {
                out.insert(place.local);
            }
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(source)),
                } = &stmt.kind
                else {
                    continue;
                };
                if place.projection.is_empty()
                    && source.projection.is_empty()
                    && out.contains(&source.local)
                    && out.insert(place.local)
                {
                    changed = true;
                }
            }
        }
    }
    out
}

fn is_vec_accounting_call(name: &str) -> bool {
    matches!(name, "gos_rt_vec_free" | "gos_rt_vec_retain")
}

fn bump_clone_elision_reads(op: &Operand, reads: &mut [u32], clone_source: Option<Local>) {
    let Operand::Copy(place) = op else {
        return;
    };
    if !place.projection.is_empty() {
        return;
    }
    if Some(place.local) == clone_source {
        return;
    }
    if let Some(read) = reads.get_mut(place.local.0 as usize) {
        *read = read.saturating_add(1);
    }
}

struct VecCloneRewrite {
    block: usize,
    source: Local,
    destination: Place,
    target: BlockId,
    span: Span,
}

fn count_vec_clone_elision_reads(body: &Body) -> Vec<u32> {
    let mut reads = vec![0u32; body.locals.len()];
    for block in &body.blocks {
        count_vec_clone_elision_stmt_reads(block, &mut reads);
        count_vec_clone_elision_terminator_reads(block, &mut reads);
    }
    reads
}

fn count_vec_clone_elision_stmt_reads(block: &BasicBlock, reads: &mut [u32]) {
    for stmt in &block.stmts {
        let StatementKind::Assign { rvalue, .. } = &stmt.kind else {
            continue;
        };
        match rvalue {
            Rvalue::Use(op)
            | Rvalue::UnaryOp { operand: op, .. }
            | Rvalue::Cast { operand: op, .. }
            | Rvalue::Repeat { value: op, .. } => {
                bump_clone_elision_reads(op, reads, None);
            }
            Rvalue::BinaryOp { lhs, rhs, .. } => {
                bump_clone_elision_reads(lhs, reads, None);
                bump_clone_elision_reads(rhs, reads, None);
            }
            Rvalue::Aggregate { operands, .. } => {
                for op in operands {
                    bump_clone_elision_reads(op, reads, None);
                }
            }
            Rvalue::CallIntrinsic { name, args } => {
                if is_vec_accounting_call(name) {
                    continue;
                }
                for op in args {
                    bump_clone_elision_reads(op, reads, None);
                }
            }
            Rvalue::Len(place) | Rvalue::Ref { place, .. } => {
                if place.projection.is_empty()
                    && let Some(read) = reads.get_mut(place.local.0 as usize)
                {
                    *read = read.saturating_add(1);
                }
            }
            Rvalue::StaticLoad(_) => {}
        }
    }
}

fn count_vec_clone_elision_terminator_reads(block: &BasicBlock, reads: &mut [u32]) {
    let Terminator::Call {
        callee,
        args,
        destination,
        ..
    } = &block.terminator
    else {
        return;
    };
    let clone_source = match (callee, args.as_slice()) {
        (Operand::Const(ConstValue::Str(name)), [Operand::Copy(place)])
            if name == "gos_rt_vec_clone" && place.projection.is_empty() =>
        {
            Some(place.local)
        }
        _ => None,
    };
    bump_clone_elision_reads(callee, reads, clone_source);
    for arg in args {
        bump_clone_elision_reads(arg, reads, clone_source);
    }
    if !destination.projection.is_empty()
        && let Some(read) = reads.get_mut(destination.local.0 as usize)
    {
        *read = read.saturating_add(1);
    }
}

/// Blocks that lie on a cycle, i.e. that can reach themselves.
///
/// A clone written once in the source runs once per iteration inside a loop,
/// so the static read count that licenses eliding it does not apply there:
/// the second iteration would hand out the same buffer again.
///
/// Computed as the strongly-connected components of the CFG (Kosaraju, two
/// iterative passes) so the cost stays linear in blocks plus edges. A block is
/// on a cycle when its component holds more than one block, or when it branches
/// to itself.
fn blocks_on_a_cycle(body: &Body) -> Vec<bool> {
    let n = body.blocks.len();
    let index: HashMap<u32, usize> = body
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id.0, i))
        .collect();
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut pred: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut self_edge = vec![false; n];
    for (i, block) in body.blocks.iter().enumerate() {
        for target in terminator_targets(&block.terminator) {
            let Some(&j) = index.get(&target.0) else {
                continue;
            };
            if i == j {
                self_edge[i] = true;
            }
            succ[i].push(j);
            pred[j].push(i);
        }
    }

    let mut visited = vec![false; n];
    let mut order = Vec::with_capacity(n);
    for root in 0..n {
        if visited[root] {
            continue;
        }
        // (node, next successor to visit); post-order push on exhaustion.
        let mut stack = vec![(root, 0usize)];
        visited[root] = true;
        while let Some((node, cursor)) = stack.pop() {
            match succ[node].get(cursor) {
                Some(&next) => {
                    stack.push((node, cursor + 1));
                    if !visited[next] {
                        visited[next] = true;
                        stack.push((next, 0));
                    }
                }
                None => order.push(node),
            }
        }
    }

    let mut component = vec![usize::MAX; n];
    let mut sizes: Vec<usize> = Vec::new();
    for &root in order.iter().rev() {
        if component[root] != usize::MAX {
            continue;
        }
        let id = sizes.len();
        let mut size = 0;
        let mut stack = vec![root];
        component[root] = id;
        while let Some(node) = stack.pop() {
            size += 1;
            for &back in &pred[node] {
                if component[back] == usize::MAX {
                    component[back] = id;
                    stack.push(back);
                }
            }
        }
        sizes.push(size);
    }

    (0..n)
        .map(|i| self_edge[i] || sizes.get(component[i]).copied().unwrap_or(1) > 1)
        .collect()
}

/// Every block a terminator may transfer control to.
fn terminator_targets(t: &Terminator) -> Vec<crate::ir::BlockId> {
    match t {
        Terminator::Goto { target }
        | Terminator::Assert { target, .. }
        | Terminator::Drop { target, .. } => vec![*target],
        Terminator::SwitchInt { arms, default, .. } => {
            let mut out: Vec<crate::ir::BlockId> = arms.iter().map(|(_, b)| *b).collect();
            out.push(*default);
            out
        }
        Terminator::Call { target, .. } => target.iter().copied().collect(),
        Terminator::Return | Terminator::Unreachable | Terminator::Panic { .. } => Vec::new(),
    }
}

fn collect_fresh_vec_clone_rewrites(
    body: &Body,
    fresh: &HashSet<Local>,
    reads: &[u32],
) -> Vec<VecCloneRewrite> {
    let cyclic = blocks_on_a_cycle(body);
    let mut rewrites = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        if cyclic.get(bi).copied().unwrap_or(false) {
            continue;
        }
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            target: Some(target),
        } = &block.terminator
        else {
            continue;
        };
        if name != "gos_rt_vec_clone" || !destination.projection.is_empty() {
            continue;
        }
        let [Operand::Copy(source)] = args.as_slice() else {
            continue;
        };
        if !source.projection.is_empty() || !fresh.contains(&source.local) {
            continue;
        }
        if reads.get(source.local.0 as usize).copied().unwrap_or(0) != 0 {
            continue;
        }
        rewrites.push(VecCloneRewrite {
            block: bi,
            source: source.local,
            destination: destination.clone(),
            target: *target,
            span: block.span,
        });
    }
    rewrites
}

fn apply_fresh_vec_clone_rewrites(
    body: &mut Body,
    unit_ty: Ty,
    rewrites: Vec<VecCloneRewrite>,
) {
    for rewrite in rewrites {
        let retain_dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        body.blocks[rewrite.block].stmts.push(Statement {
            kind: StatementKind::Assign {
                place: Place::local(retain_dest),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_vec_retain",
                    args: vec![Operand::Copy(Place::local(rewrite.source))],
                },
            },
            span: rewrite.span,
        });
        body.blocks[rewrite.block].stmts.push(Statement {
            kind: StatementKind::Assign {
                place: rewrite.destination,
                rvalue: Rvalue::Use(Operand::Copy(Place::local(rewrite.source))),
            },
            span: rewrite.span,
        });
        body.blocks[rewrite.block].terminator = Terminator::Goto {
            target: rewrite.target,
        };
    }
}

/// Rewrites a deep clone of an unnameable fresh Vec temporary into a retained
/// pointer alias:
///
/// ```text
/// tmp = gos_rt_vec_from_arr(...)
/// cloned = gos_rt_vec_clone(tmp)
/// ```
///
/// becomes:
///
/// ```text
/// gos_rt_vec_retain(tmp)
/// cloned = tmp
/// ```
///
/// This preserves ownership accounting because the clone destination still owns
/// a share and existing drop sites release both `tmp` and `cloned`. It is only
/// applied when `tmp` is compiler-generated and the clone is its only
/// non-accounting read, so user-observable `v.clone()` remains a deep copy.
pub(crate) fn elide_vec_clone_of_fresh_temporary(body: &mut Body, tcx: &TyCtxt) {
    let fresh = fresh_vec_temporary_sources(body);
    if fresh.is_empty() {
        return;
    }

    let reads = count_vec_clone_elision_reads(body);
    let unit_ty = tcx
        .unit_interned()
        .unwrap_or_else(|| body.locals.first().expect("body has return local").ty);
    let rewrites = collect_fresh_vec_clone_rewrites(body, &fresh, &reads);
    apply_fresh_vec_clone_rewrites(body, unit_ty, rewrites);
}

/// Blocks reachable from `start`, following every successor edge.
fn blocks_reachable_from(body: &Body, start: usize) -> Vec<bool> {
    let mut seen = vec![false; body.blocks.len()];
    let mut queue = vec![start];
    while let Some(bi) = queue.pop() {
        for succ in block_successors(&body.blocks[bi].terminator) {
            let idx = succ.as_u32() as usize;
            if idx < seen.len() && !seen[idx] {
                seen[idx] = true;
                queue.push(idx);
            }
        }
    }
    seen
}

/// Whether any statement or terminator in `block` reads `local` other than
/// through the reference-count helpers that bracket an ownership transfer.
fn block_reads_vec_local(block: &BasicBlock, local: Local, from_stmt: usize) -> bool {
    let mentions = |op: &Operand| matches!(op, Operand::Copy(p) if p.local == local);
    for stmt in block.stmts.iter().skip(from_stmt) {
        let StatementKind::Assign { place, rvalue } = &stmt.kind else {
            continue;
        };
        if place.local == local && !place.projection.is_empty() {
            return true;
        }
        match rvalue {
            Rvalue::CallIntrinsic { name, args } => {
                if matches!(
                    name,
                    &"gos_rt_vec_retain" | &"gos_rt_vec_free" | &"gos_rt_vec_mark_shared"
                ) {
                    continue;
                }
                if args.iter().any(mentions) {
                    return true;
                }
            }
            Rvalue::Use(op) | Rvalue::UnaryOp { operand: op, .. } | Rvalue::Cast { operand: op, .. } => {
                if mentions(op) {
                    return true;
                }
            }
            Rvalue::BinaryOp { lhs, rhs, .. } => {
                if mentions(lhs) || mentions(rhs) {
                    return true;
                }
            }
            Rvalue::Aggregate { operands, .. } => {
                if operands.iter().any(mentions) {
                    return true;
                }
            }
            Rvalue::Repeat { value, .. } => {
                if mentions(value) {
                    return true;
                }
            }
            Rvalue::Ref { place: p, .. } | Rvalue::Len(p) => {
                if p.local == local {
                    return true;
                }
            }
            Rvalue::StaticLoad(_) => {}
        }
    }
    match &block.terminator {
        Terminator::Call { args, .. } => args.iter().any(mentions),
        Terminator::SwitchInt { discriminant, .. } => mentions(discriminant),
        _ => false,
    }
}

/// Drops the deep copy a struct binding takes of a vector field when the
/// vector the field was built from is never named again.
///
/// A binding gives its aggregate storage of its own so a later push through a
/// field cannot reach the value the field was built from. When that value has
/// no reader left, the two cannot be told apart, and the copy - which walks
/// every nested vector - is work whose result nothing can observe.
pub(crate) fn elide_vec_clone_of_dead_aggregate_source(body: &mut Body, user_fns: &HashSet<String>) {
    let n_blocks = body.blocks.len();
    if n_blocks == 0 {
        return;
    }
    // (clone block, target block) pairs to rewrite.
    let mut rewrites: Vec<(usize, usize, Local)> = Vec::new();

    for bi in 0..n_blocks {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            target: Some(target),
        } = &body.blocks[bi].terminator
        else {
            continue;
        };
        if name != "gos_rt_vec_clone" || !destination.projection.is_empty() {
            continue;
        }
        let [Operand::Copy(field)] = args.as_slice() else {
            continue;
        };
        let [Projection::Field(index)] = field.projection.as_slice() else {
            continue;
        };
        let (index, base, cloned, ti) = (*index, field.local, destination.local, target.as_u32() as usize);
        if ti >= n_blocks {
            continue;
        }
        // The target block re-publishes the copy: release the old field, store
        // the copy, take a share of it.
        let stmts = &body.blocks[ti].stmts;
        if stmts.len() < 3
            || !matches!(&stmts[0].kind, StatementKind::Assign { rvalue: Rvalue::CallIntrinsic { name, args }, .. }
                if *name == "gos_rt_vec_free"
                    && matches!(args.as_slice(), [Operand::Copy(p)] if *p == *field))
            || !matches!(&stmts[1].kind, StatementKind::Assign { place, rvalue: Rvalue::Use(Operand::Copy(src)) }
                if *place == *field && src.projection.is_empty() && src.local == cloned)
            || !matches!(&stmts[2].kind, StatementKind::Assign { rvalue: Rvalue::CallIntrinsic { name, args }, .. }
                if *name == "gos_rt_vec_retain"
                    && matches!(args.as_slice(), [Operand::Copy(p)] if *p == *field))
        {
            continue;
        }

        match aggregate_field_origin(body, base, index, user_fns) {
            // A value a function answered is the caller's own; the callee's
            // own binding rules gave it storage nothing else names.
            Some(AggregateOrigin::CallResult) => {}
            Some(AggregateOrigin::Field {
                block: origin_block,
                index: origin_stmt,
                source,
            }) => {
                // A vector nothing names after the aggregate cannot be told
                // apart from the field that now holds it.
                let reachable = blocks_reachable_from(body, origin_block);
                let mut live =
                    block_reads_vec_local(&body.blocks[origin_block], source, origin_stmt + 1);
                for (idx, block) in body.blocks.iter().enumerate() {
                    if idx != origin_block
                        && reachable[idx]
                        && block_reads_vec_local(block, source, 0)
                    {
                        live = true;
                        break;
                    }
                }
                if live {
                    continue;
                }
            }
            None => continue,
        }
        rewrites.push((bi, ti, cloned));
    }

    for (bi, ti, cloned) in rewrites {
        let span = body.blocks[bi].span;
        let Terminator::Call { target, .. } = body.blocks[bi].terminator.clone() else {
            continue;
        };
        let Some(target) = target else { continue };
        // The clone destination keeps a defined value so its own sweep release
        // reads a null handle rather than an uninitialised slot.
        body.blocks[bi].stmts.push(Statement {
            kind: StatementKind::Assign {
                place: Place::local(cloned),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            },
            span,
        });
        body.blocks[bi].terminator = Terminator::Goto { target };
        body.blocks[ti].stmts.drain(0..3);
    }
}

/// Where the aggregate a binding local holds came from.
enum AggregateOrigin {
    /// A function answered it, so its storage is this frame's own.
    CallResult,
    /// It was built here from a vector named by `source`.
    Field {
        block: usize,
        index: usize,
        source: Local,
    },
}

/// The origin of the aggregate whose field `index` a binding local names.
fn aggregate_field_origin(
    body: &Body,
    base: Local,
    index: u32,
    user_fns: &HashSet<String>,
) -> Option<AggregateOrigin> {
    let mut binding_init: Option<Local> = None;
    let mut writes = 0u32;
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.local != base || !place.projection.is_empty() {
                // A store into a field replaces what the field names; it
                // cannot make the dead source observable again.
                continue;
            }
            // The pre-init sentinel every aggregate slot starts at is not a
            // value the binding ever holds.
            if matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0)))) {
                continue;
            }
            writes += 1;
            if let Rvalue::Use(Operand::Copy(src)) = rvalue
                && src.projection.is_empty()
            {
                binding_init = Some(src.local);
            }
        }
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.local == base
            && destination.projection.is_empty()
        {
            return None;
        }
    }
    if writes != 1 {
        return None;
    }
    let temp = binding_init?;
    // The binding may copy a temporary a user function answered directly.
    let mut answered_by_call = false;
    for block in &body.blocks {
        if let Terminator::Call {
            callee,
            destination,
            ..
        } = &block.terminator
            && destination.local == temp
            && destination.projection.is_empty()
        {
            let user = match callee {
                Operand::FnRef { .. } => true,
                Operand::Const(ConstValue::Str(name)) => user_fns.contains(name),
                _ => false,
            };
            if !user {
                return None;
            }
            answered_by_call = true;
        }
    }
    if answered_by_call {
        return Some(AggregateOrigin::CallResult);
    }
    let mut found: Option<AggregateOrigin> = None;
    let mut temp_writes = 0u32;
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.local != temp || !place.projection.is_empty() {
                continue;
            }
            if matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0)))) {
                continue;
            }
            temp_writes += 1;
            if let Rvalue::Aggregate { operands, .. } = rvalue
                && let Some(Operand::Copy(src)) = operands.get(index as usize)
                && src.projection.is_empty()
            {
                found = Some(AggregateOrigin::Field {
                    block: bi,
                    index: si,
                    source: src.local,
                });
            }
        }
    }
    if temp_writes == 1 { found } else { None }
}
