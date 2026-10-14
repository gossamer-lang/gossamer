/// The count helpers a string literal answers as a no-op: a literal is static
/// text, never freed, so taking or giving back a share of it changes nothing.
fn is_string_count_helper(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_str_retain_typed"
            | "gos_rt_str_free_typed"
            | "gos_rt_str_retain"
            | "gos_rt_str_free"
            | "gos_rt_rc_retain"
            | "gos_rt_rc_release"
    )
}

/// Drops the count adjustments on string literals, and on the fields of a
/// local that only ever holds an aggregate of literals.
///
/// A `const` or `static` table of strings is built where it is read, as an
/// array of literals, and every element is retained when it is built and
/// released before the next build. Each of those calls is a run-time no-op on
/// a literal; in a lookup loop they are most of the work. A local qualifies
/// when every whole value it is given is an aggregate whose operands are all
/// constants, nothing else writes it, and nothing reads it except through a
/// projection into a fresh binding - so no field of it can hold anything but a
/// literal.
pub(crate) fn elide_literal_string_counts(body: &mut Body) {
    let n_locals = body.locals.len();
    let mut literal_table = vec![true; n_locals];
    let mut given_aggregate = vec![false; n_locals];
    let disqualify = |local: Local, table: &mut Vec<bool>| {
        if let Some(slot) = table.get_mut(local.0 as usize) {
            *slot = false;
        }
    };
    let count_call_on = |rvalue: &Rvalue| -> Option<Local> {
        match rvalue {
            Rvalue::CallIntrinsic { name, args } if is_string_count_helper(name) => {
                match args.as_slice() {
                    [Operand::Copy(p)] if !p.projection.is_empty() => Some(p.local),
                    _ => None,
                }
            }
            _ => None,
        }
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                for local in literal_scan_stmt_locals(stmt) {
                    disqualify(local, &mut literal_table);
                }
                continue;
            };
            if count_call_on(rvalue).is_some() {
                continue;
            }
            let zero = matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0))));
            if place.projection.is_empty() {
                match rvalue {
                    Rvalue::Aggregate { operands, .. }
                        if operands.iter().all(|op| matches!(op, Operand::Const(_))) =>
                    {
                        if let Some(slot) = given_aggregate.get_mut(place.local.0 as usize) {
                            *slot = true;
                        }
                    }
                    _ if zero => {}
                    _ => disqualify(place.local, &mut literal_table),
                }
            } else if !zero {
                disqualify(place.local, &mut literal_table);
            }
            // A read of the table is an element read through a projection; a
            // whole read hands the table on, where this pass cannot follow it.
            for op in literal_scan_operands(rvalue) {
                if let Operand::Copy(p) = op
                    && p.projection.is_empty()
                {
                    disqualify(p.local, &mut literal_table);
                }
            }
            if let Rvalue::Ref { place: p, .. } | Rvalue::Len(p) = rvalue {
                disqualify(p.local, &mut literal_table);
            }
        }
        for local in literal_scan_terminator_locals(&block.terminator) {
            disqualify(local, &mut literal_table);
        }
        if let Terminator::Call { destination, .. } = &block.terminator {
            disqualify(destination.local, &mut literal_table);
        }
    }
    let table = |local: Local| {
        let i = local.0 as usize;
        i < n_locals && i > body.arity as usize && literal_table[i] && given_aggregate[i]
    };
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            let StatementKind::Assign { rvalue, .. } = &stmt.kind else {
                continue;
            };
            let on_literal = matches!(rvalue, Rvalue::CallIntrinsic { name, args }
                if is_string_count_helper(name)
                    && matches!(args.as_slice(), [Operand::Const(ConstValue::Str(_))]));
            let on_table = count_call_on(rvalue).is_some_and(table);
            if on_literal || on_table {
                stmt.kind = StatementKind::Nop;
            }
        }
    }
}

/// The operands an rvalue reads.
fn literal_scan_operands(rvalue: &Rvalue) -> Vec<&Operand> {
    match rvalue {
        Rvalue::Use(op)
        | Rvalue::UnaryOp { operand: op, .. }
        | Rvalue::Cast { operand: op, .. }
        | Rvalue::Repeat { value: op, .. } => vec![op],
        Rvalue::BinaryOp { lhs, rhs, .. } => vec![lhs, rhs],
        Rvalue::Aggregate { operands, .. } => operands.iter().collect(),
        Rvalue::CallIntrinsic { args, .. } => args.iter().collect(),
        Rvalue::Ref { .. } | Rvalue::Len(_) | Rvalue::StaticLoad(_) => Vec::new(),
    }
}

/// The locals a terminator reads whole or through a projection.
fn literal_scan_terminator_locals(terminator: &Terminator) -> Vec<Local> {
    let of = |op: &Operand| match op {
        Operand::Copy(p) => Some(p.local),
        _ => None,
    };
    match terminator {
        Terminator::Call { callee, args, .. } => {
            args.iter().chain(std::iter::once(callee)).filter_map(of).collect()
        }
        Terminator::SwitchInt { discriminant, .. } => of(discriminant).into_iter().collect(),
        Terminator::Assert { cond, .. } => of(cond).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// The locals a non-assignment statement names.
fn literal_scan_stmt_locals(stmt: &Statement) -> Vec<Local> {
    match &stmt.kind {
        StatementKind::SetDiscriminant { place, .. } => vec![place.local],
        StatementKind::StaticStore {
            value: Operand::Copy(p),
            ..
        } => vec![p.local],
        _ => Vec::new(),
    }
}
