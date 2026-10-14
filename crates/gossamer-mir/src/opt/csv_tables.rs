/// Reads `csv::read(text).unwrap_or(#[])` into a CSV table when the program
/// only indexes the answer: its length, a row by index, a row's length, and a
/// field read by `to_f64`, `to_i64`, or `to_bool`, or by one call that reads
/// a string and keeps nothing.
///
/// A table keeps the document's text and each field's span, so it builds no
/// `String` per field and no vector per row; a field read as a number is
/// parsed where it lies. Every index panics as indexing the rows would, and
/// a document `csv::read` rejects makes the same empty answer. Any other use
/// of the answer, a row, or a field leaves the call as it is.
pub(crate) fn read_csv_as_tables(body: &mut Body, tcx: &TyCtxt) {
    let Some(int64) = tcx.interned(&TyKind::Int(gossamer_types::IntTy::I64)) else {
        return;
    };
    let reads: Vec<usize> = (0..body.blocks.len())
        .filter(|&bi| {
            matches!(
                &body.blocks[bi].terminator,
                Terminator::Call { callee: Operand::Const(ConstValue::Str(name)), .. }
                    if name == "gos_rt_csv_read"
            )
        })
        .collect();
    for read in reads {
        if let Some(plan) = plan_csv_table(body, read) {
            apply_csv_table(body, &plan, int64);
        }
    }
}

/// Where a local is mentioned: a statement of a block, or its terminator.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Site {
    Stmt(usize, usize),
    Term(usize),
}

/// Every place `local` is mentioned.
fn csv_sites(body: &Body, local: Local) -> Vec<Site> {
    let mut sites = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            if stmt_mentions_local(stmt, local) {
                sites.push(Site::Stmt(bi, si));
            }
        }
        if term_mentions_local(&block.terminator, local) {
            sites.push(Site::Term(bi));
        }
    }
    sites
}

/// The runtime call a block ends with: its name, arguments, destination, and
/// next block.
fn csv_call(block: &BasicBlock) -> Option<(&str, &[Operand], &Place, BlockId)> {
    match &block.terminator {
        Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            target: Some(target),
        } => Some((name.as_str(), args.as_slice(), destination, *target)),
        _ => None,
    }
}

fn bare(op: &Operand, local: Local) -> bool {
    matches!(op, Operand::Copy(p) if p.local == local && p.projection.is_empty())
}

/// Whether the statement at `site` stores a constant or a fresh result
/// carrier into `local`, which every binding starts from.
fn csv_resets(body: &Body, site: Site, local: Local) -> bool {
    let Site::Stmt(bi, si) = site else {
        return false;
    };
    match &body.blocks[bi].stmts[si].kind {
        StatementKind::Assign { place, rvalue } if place.local == local && place.projection.is_empty() => {
            match rvalue {
                Rvalue::Use(Operand::Const(_)) => true,
                Rvalue::CallIntrinsic { name, args } => {
                    *name == "gos_rt_result_new"
                        && args.iter().all(|a| matches!(a, Operand::Const(_)))
                }
                _ => false,
            }
        }
        StatementKind::StorageLive(_) | StatementKind::StorageDead(_) => true,
        _ => false,
    }
}

/// Whether the statement at `site` is `name(local, ..)` as an intrinsic.
fn csv_intrinsic(body: &Body, site: Site, local: Local, names: &[&str]) -> bool {
    let Site::Stmt(bi, si) = site else {
        return false;
    };
    matches!(
        &body.blocks[bi].stmts[si].kind,
        StatementKind::Assign { place, rvalue: Rvalue::CallIntrinsic { name, args } }
            if place.local != local
                && names.contains(name)
                && args.first().is_some_and(|a| bare(a, local))
                && !args[1..].iter().any(|a| operand_mentions_local(a, local))
    )
}

/// How a field read is rewritten.
enum FieldRead {
    /// The get's block jumps on; the reader at `reader` reads in place.
    Fused { reader: usize, name: &'static str },
    /// The get answers a string the table owns until its next read.
    Borrowed,
}

/// One rewrite of a plan.
enum CsvEdit {
    /// `gos_rt_csv_read` becomes `gos_rt_csv_table`.
    Read(usize),
    /// `unwrap_or_vec(res, d) -> v` becomes `unwrap_or_empty(res) -> v`.
    Unwrap(usize),
    /// `gos_rt_vec_clone(v) -> r` becomes a move.
    Move(usize, Local, Local),
    /// `gos_rt_vec_free(t)` becomes the table's free.
    Free(usize, usize),
    /// A length of the table.
    Len(usize, Local),
    /// `get(t, i) -> w` becomes the bounds-checked record index.
    Record(usize, Local),
    /// A length of row `w` of table `t`.
    RowLen(usize, Local, Local),
    /// `get(w, j) -> f` of table `t`.
    Field(usize, Local, Local, FieldRead),
}

struct CsvPlan {
    edits: Vec<CsvEdit>,
    /// Locals that now hold a table handle or a record index.
    retyped: Vec<Local>,
}

fn plan_csv_table(body: &Body, read: usize) -> Option<CsvPlan> {
    let (_, _, dest, _) = csv_call(&body.blocks[read])?;
    if !dest.projection.is_empty() {
        return None;
    }
    let res = dest.local;
    let mut edits = vec![CsvEdit::Read(read)];
    let mut answer = None;
    for site in csv_sites(body, res) {
        if site == Site::Term(read)
            || csv_resets(body, site, res)
            || csv_releases_moved_payload(body, site, res)
        {
            continue;
        }
        let Site::Term(bi) = site else {
            return None;
        };
        let (name, args, v, _) = csv_call(&body.blocks[bi])?;
        if name != "gos_rt_result_unwrap_or_vec"
            || answer.is_some()
            || !v.projection.is_empty()
            || !matches!(args, [r, Operand::Copy(d)] if bare(r, res) && d.projection.is_empty() && csv_is_empty_vec(body, d.local))
        {
            return None;
        }
        answer = Some(v.local);
        edits.push(CsvEdit::Unwrap(bi));
    }
    let v = answer?;
    let mut tables = vec![v];
    let mut retyped = vec![v];
    let mut at = 0;
    while at < tables.len() {
        let t = tables[at];
        at += 1;
        for site in csv_sites(body, t) {
            if csv_resets(body, site, t) {
                continue;
            }
            if csv_intrinsic(body, site, t, &["gos_rt_vec_free"]) {
                let Site::Stmt(bi, si) = site else {
                    return None;
                };
                edits.push(CsvEdit::Free(bi, si));
                continue;
            }
            let Site::Term(bi) = site else {
                return None;
            };
            let (name, args, dest, target) = csv_call(&body.blocks[bi])?;
            if !dest.projection.is_empty() {
                return None;
            }
            if dest.local == t {
                // The answer's own definition, or the move into a binding.
                if (t == v && name == "gos_rt_result_unwrap_or_vec")
                    || (t != v && name == "gos_rt_vec_clone")
                {
                    continue;
                }
                return None;
            }
            match (name, args) {
                ("gos_rt_vec_clone", [src]) if bare(src, t) && t == v => {
                    tables.push(dest.local);
                    retyped.push(dest.local);
                    edits.push(CsvEdit::Move(bi, t, dest.local));
                }
                ("gos_rt_len" | "gos_rt_vec_len", [r]) if bare(r, t) => {
                    edits.push(CsvEdit::Len(bi, t));
                }
                ("gos_rt_vec_get_i64" | "gos_rt_vec_get_i64_unchecked", [r, i])
                    if bare(r, t) && !operand_mentions_local(i, t) =>
                {
                    let row = dest.local;
                    plan_csv_row(body, t, row, &mut edits)?;
                    retyped.push(row);
                    edits.push(CsvEdit::Record(bi, t));
                    let _ = target;
                }
                _ => return None,
            }
        }
    }
    Some(CsvPlan { edits, retyped })
}

/// Whether the statement at `site` releases the payload of `res` as a
/// carrier whose `Ok` value the unwrap moved out: only an error is released.
fn csv_releases_moved_payload(body: &Body, site: Site, res: Local) -> bool {
    let Site::Stmt(bi, si) = site else {
        return false;
    };
    matches!(
        &body.blocks[bi].stmts[si].kind,
        StatementKind::Assign { rvalue: Rvalue::CallIntrinsic { name, args }, .. }
            if *name == "gos_rt_result_payload_release"
                && matches!(args.as_slice(), [r, Operand::Const(ConstValue::Int(0)), Operand::Const(_)] if bare(r, res))
    )
}

/// Whether `local` only ever holds a fresh empty vector.
fn csv_is_empty_vec(body: &Body, local: Local) -> bool {
    csv_sites(body, local).into_iter().all(|site| match site {
        Site::Stmt(..) => {
            csv_resets(body, site, local)
                || csv_intrinsic(body, site, local, &["gos_rt_vec_free", "gos_rt_vec_mark_vec_elems"])
        }
        Site::Term(bi) => match csv_call(&body.blocks[bi]) {
            Some((name, args, dest, _)) if dest.local == local => {
                matches!(name, "Vec::new" | "gos_rt_vec_new" | "gos_rt_vec_new_typed")
                    && !args.iter().any(|a| operand_mentions_local(a, local))
            }
            Some(("gos_rt_result_unwrap_or_vec", [_, d], _, _)) => bare(d, local),
            _ => false,
        },
    })
}

/// Plans the reads of `row`, a row of table `t` the program borrows.
fn plan_csv_row(body: &Body, t: Local, row: Local, edits: &mut Vec<CsvEdit>) -> Option<()> {
    for site in csv_sites(body, row) {
        if csv_resets(body, site, row) {
            continue;
        }
        let Site::Term(bi) = site else {
            return None;
        };
        let (name, args, dest, target) = csv_call(&body.blocks[bi])?;
        if !dest.projection.is_empty() {
            return None;
        }
        match (name, args) {
            ("gos_rt_vec_get_i64" | "gos_rt_vec_get_i64_unchecked", [r, _])
                if dest.local == row && matches!(r, Operand::Copy(p) if p.local == t) =>
            {
                // The row's own definition.
            }
            ("gos_rt_len" | "gos_rt_vec_len", [r]) if bare(r, row) && dest.local != row => {
                edits.push(CsvEdit::RowLen(bi, t, row));
            }
            ("gos_rt_vec_get_i64" | "gos_rt_vec_get_i64_unchecked", [r, j])
                if bare(r, row) && !operand_mentions_local(j, row) && dest.local != row =>
            {
                let mut keep = vec![t, row];
                if let Operand::Copy(p) = j {
                    keep.push(p.local);
                }
                let read = plan_csv_field(body, dest.local, bi, target, &keep)?;
                edits.push(CsvEdit::Field(bi, t, row, read));
            }
            _ => return None,
        }
    }
    Some(())
}

/// Plans the one read of `field`, which the get ending block `get` answers
/// and `target` follows. `keep` are the locals the read still needs.
fn plan_csv_field(
    body: &Body,
    field: Local,
    get: usize,
    target: BlockId,
    keep: &[Local],
) -> Option<FieldRead> {
    let reader = target.0 as usize;
    let mut reads = 0;
    for site in csv_sites(body, field) {
        match site {
            Site::Term(bi) if bi == get => {}
            Site::Term(bi) if bi == reader => reads += 1,
            _ if csv_resets(body, site, field) => {}
            _ => return None,
        }
    }
    // Nothing between the get and its reader changes what the read names.
    let between = &body.blocks[reader].stmts;
    if reads != 1
        || between.iter().any(|s| {
            matches!(&s.kind, StatementKind::Assign { place, .. } if keep.contains(&place.local))
        })
    {
        return None;
    }
    let (name, args, dest, _) = csv_call(&body.blocks[reader])?;
    if keep.contains(&dest.local) || dest.local == field {
        return None;
    }
    let fused = match name {
        "gos_rt_str_to_f64_opt" => Some("gos_rt_csv_table_field_to_f64_opt"),
        "gos_rt_str_to_i64_opt" => Some("gos_rt_csv_table_field_to_i64_opt"),
        "gos_rt_str_to_bool_opt" => Some("gos_rt_csv_table_field_to_bool_opt"),
        _ => None,
    };
    if let Some(fused) = fused {
        return matches!(args, [f] if bare(f, field)).then_some(FieldRead::Fused { reader, name: fused });
    }
    // A string reader that keeps nothing of its arguments.
    let reads_only = matches!(
        name,
        "gos_rt_str_len" | "gos_rt_str_byte_len" | "gos_rt_str_is_empty" | "gos_rt_str_eq"
    );
    reads_only.then_some(FieldRead::Borrowed)
}

fn apply_csv_table(body: &mut Body, plan: &CsvPlan, int64: gossamer_types::Ty) {
    let copy = |local: Local| Operand::Copy(Place::local(local));
    let set_call = |body: &mut Body, bi: usize, name: &str, args: Vec<Operand>| {
        if let Terminator::Call { callee, args: old, .. } = &mut body.blocks[bi].terminator {
            *callee = Operand::Const(ConstValue::Str(name.into()));
            *old = args;
        }
    };
    for edit in &plan.edits {
        match edit {
            CsvEdit::Read(bi) => {
                if let Terminator::Call { callee, .. } = &mut body.blocks[*bi].terminator {
                    *callee = Operand::Const(ConstValue::Str("gos_rt_csv_table".into()));
                }
            }
            CsvEdit::Unwrap(bi) => {
                if let Terminator::Call { callee, args, .. } = &mut body.blocks[*bi].terminator {
                    *callee =
                        Operand::Const(ConstValue::Str("gos_rt_csv_table_unwrap_or_empty".into()));
                    args.truncate(1);
                }
            }
            CsvEdit::Move(bi, from, to) => {
                let block = &mut body.blocks[*bi];
                let Terminator::Call { target: Some(target), .. } = block.terminator else {
                    continue;
                };
                let span = block.span;
                block.terminator = Terminator::Goto { target };
                for (place, value) in [(*to, copy(*from)), (*from, Operand::Const(ConstValue::Int(0)))]
                {
                    block.stmts.push(Statement {
                        kind: StatementKind::Assign {
                            place: Place::local(place),
                            rvalue: Rvalue::Use(value),
                        },
                        span,
                        inlined: None,
                    });
                }
            }
            CsvEdit::Free(bi, si) => {
                if let StatementKind::Assign { rvalue: Rvalue::CallIntrinsic { name, .. }, .. } =
                    &mut body.blocks[*bi].stmts[*si].kind
                {
                    *name = "gos_rt_csv_table_free";
                }
            }
            CsvEdit::Len(bi, t) => set_call(body, *bi, "gos_rt_csv_table_len", vec![copy(*t)]),
            CsvEdit::Record(bi, t) => {
                let Some((_, [_, i], _, _)) = csv_call(&body.blocks[*bi]) else {
                    continue;
                };
                let i = i.clone();
                set_call(body, *bi, "gos_rt_csv_table_record", vec![copy(*t), i]);
            }
            CsvEdit::RowLen(bi, t, row) => {
                set_call(body, *bi, "gos_rt_csv_table_record_len", vec![copy(*t), copy(*row)]);
            }
            CsvEdit::Field(bi, t, row, read) => {
                let Some((_, [_, j], _, target)) = csv_call(&body.blocks[*bi]) else {
                    continue;
                };
                let args = vec![copy(*t), copy(*row), j.clone()];
                match read {
                    FieldRead::Borrowed => set_call(body, *bi, "gos_rt_csv_table_field", args),
                    FieldRead::Fused { reader, name } => {
                        body.blocks[*bi].terminator = Terminator::Goto { target };
                        set_call(body, *reader, name, args);
                    }
                }
            }
        }
    }
    for &local in &plan.retyped {
        body.locals[local.0 as usize].ty = int64;
    }
}
