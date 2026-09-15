/// Payload bindings a backend may read in place.
///
/// A returned local has one definition, `local = gos_rt_result_payload(..)`,
/// and is otherwise read only as a whole argument of a call to a named
/// function. From the definition to each such call, control passes through
/// `Goto` edges and blocks holding only `Nop` and storage markers, so the
/// payload block holds exactly the words the binding would copy for as long
/// as the callee can read them. Whether a callee takes the aggregate by shared
/// reference is the backend's question, since only it sees the signatures.
#[must_use]
pub fn carrier_payload_views(body: &Body) -> Vec<Local> {
    let mut defs: Vec<(Local, usize, usize)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            if let StatementKind::Assign {
                place,
                rvalue:
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_payload",
                        args,
                    },
            } = &stmt.kind
                && place.projection.is_empty()
                && place.local.0 > body.arity
                && args.len() == 1
                && !operand_mentions_local(&args[0], place.local)
            {
                defs.push((place.local, bi, si));
            }
        }
    }
    defs.into_iter()
        .filter(|&(local, bi, si)| payload_view_is_read_in_place(body, local, bi, si))
        .map(|(local, _, _)| local)
        .collect()
}

fn is_quiet_statement(stmt: &Statement) -> bool {
    matches!(
        stmt.kind,
        StatementKind::Nop | StatementKind::StorageLive(_) | StatementKind::StorageDead(_)
    )
}

/// `true` when every mention of `local` other than its definition is a whole
/// argument of a named call that a quiet path from the definition reaches.
fn payload_view_is_read_in_place(
    body: &Body,
    local: Local,
    def_block: usize,
    def_stmt: usize,
) -> bool {
    let mut calls = 0usize;
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let is_def = bi == def_block && si == def_stmt;
            if !is_def && !is_quiet_statement(stmt) && stmt_mentions_local(stmt, local) {
                return false;
            }
        }
        if !term_mentions_local(&block.terminator, local) {
            continue;
        }
        let Terminator::Call {
            callee,
            args,
            destination,
            ..
        } = &block.terminator
        else {
            return false;
        };
        let named = match callee {
            Operand::FnRef { .. } => true,
            Operand::Const(ConstValue::Str(name)) => {
                !name.starts_with("gos_") && !name.starts_with("__")
            }
            _ => false,
        };
        let whole_arguments = args.iter().all(|arg| {
            !operand_mentions_local(arg, local)
                || matches!(arg, Operand::Copy(p) if p.local == local && p.projection.is_empty())
        });
        if !named
            || !whole_arguments
            || place_mentions_local(destination, local)
            || !quiet_path_reaches(body, def_block, def_stmt, bi)
        {
            return false;
        }
        calls += 1;
    }
    calls > 0
}

/// `true` when control leaving statement `from_stmt` of `from_block` reaches
/// the terminator of `to_block` through `Goto` edges and quiet statements.
fn quiet_path_reaches(body: &Body, from_block: usize, from_stmt: usize, to_block: usize) -> bool {
    let mut block = from_block;
    let mut rest = &body.blocks[block].stmts[from_stmt + 1..];
    for _ in 0..=body.blocks.len() {
        if !rest.iter().all(is_quiet_statement) {
            return false;
        }
        if block == to_block {
            return true;
        }
        let Terminator::Goto { target } = &body.blocks[block].terminator else {
            return false;
        };
        block = target.0 as usize;
        rest = &body.blocks[block].stmts;
    }
    false
}
