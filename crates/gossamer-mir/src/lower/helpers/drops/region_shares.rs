//! Shares a node in region storage takes of the children stored into it.

use super::*;

/// The `gos_rt_retain_held` kind of a retain helper's child, or `None` for a
/// call that is no share-taking retain.
fn held_kind(name: &str) -> Option<i64> {
    match name {
        "gos_rt_str_retain_typed" | "gos_rt_str_retain" => Some(1),
        "gos_rt_vec_retain" => Some(2),
        "gos_rt_rc_retain" => Some(0),
        _ => None,
    }
}

/// Ties the share a store into a region node takes of its child to the node.
///
/// A node built while a region is open lives in the region's storage, which
/// is freed at pop without a walk of its children, so a share it took of a
/// child from outside the region would never be given back. The retain that
/// takes the store's share becomes `gos_rt_retain_held`, which gives that
/// share back when the region pops - and is the plain retain for a holder
/// outside region storage, since a region is a run-time property.
pub(crate) fn hold_region_node_shares(body: &mut Body) {
    let n_locals = body.locals.len();
    let in_region = |local: Local| {
        body.locals
            .get(local.0 as usize)
            .is_some_and(|decl| decl.region)
    };
    let mut rewrites: Vec<(usize, usize, Local, Local, i64)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let mut taken: Vec<usize> = Vec::new();
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } = &stmt.kind
            else {
                continue;
            };
            let ("gos_store", [Operand::Copy(holder), _, Operand::Copy(child)]) =
                (*name, args.as_slice())
            else {
                continue;
            };
            if !holder.projection.is_empty()
                || !child.projection.is_empty()
                || !in_region(holder.local)
                || (child.local.0 as usize) >= n_locals
            {
                continue;
            }
            // The store's share is the retain of the same child nearest to it
            // in the block, with no other write of the child between them.
            let retain_of = |sj: usize| {
                let StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, args },
                    ..
                } = &block.stmts[sj].kind
                else {
                    return None;
                };
                match args.as_slice() {
                    [Operand::Copy(p)] if p.projection.is_empty() && p.local == child.local => {
                        held_kind(name)
                    }
                    _ => None,
                }
            };
            let writes_child = |sj: usize| {
                matches!(&block.stmts[sj].kind,
                    StatementKind::Assign { place, .. } if place.local == child.local)
            };
            let after = (si + 1..block.stmts.len())
                .take_while(|&sj| !writes_child(sj))
                .find(|&sj| retain_of(sj).is_some() && !taken.contains(&sj));
            let before = (0..si)
                .rev()
                .take_while(|&sj| !writes_child(sj))
                .find(|&sj| retain_of(sj).is_some() && !taken.contains(&sj));
            let nearest = match (before, after) {
                (Some(b), Some(a)) => Some(if si - b <= a - si { b } else { a }),
                (b, a) => b.or(a),
            };
            if let Some(sj) = nearest
                && let Some(kind) = retain_of(sj)
            {
                taken.push(sj);
                rewrites.push((bi, sj, holder.local, child.local, kind));
            }
        }
    }
    for (bi, sj, holder, child, kind) in rewrites {
        if let StatementKind::Assign { rvalue, .. } = &mut body.blocks[bi].stmts[sj].kind {
            *rvalue = Rvalue::CallIntrinsic {
                name: "gos_rt_retain_held",
                args: vec![
                    Operand::Copy(Place::local(holder)),
                    Operand::Copy(Place::local(child)),
                    Operand::Const(ConstValue::Int(i128::from(kind))),
                ],
            };
        }
    }
}
