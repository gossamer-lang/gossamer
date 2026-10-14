//! Payload reads that borrow from a node the caller keeps alive.

use super::*;

/// Whether a `gos_enum_load` carries the view marker
/// [`borrow_lent_payload_reads`] adds.
#[must_use]
pub fn is_payload_view(args: &[Operand]) -> bool {
    args.len() == 3
}

/// Marks a boxed aggregate payload read out of a by-value parameter as a
/// view of the box rather than a copy that owns a share of each field.
///
/// The caller holds a parameter for the whole call, so a node it names - and
/// the box a variant keeps its aggregate payload in - outlives every use the
/// frame makes of a binding read out of it. When nothing writes the
/// parameter and the binding is only read, field by field or handed whole to
/// another Gossamer function (which borrows it the same way), the binding
/// needs no share of its own: copying the words is enough, and the retain of
/// every field at the read and the release at the frame's exit both go. A
/// recursive walk of a tree, such as an encoder, otherwise pays both on every
/// node.
pub(crate) fn borrow_lent_payload_reads(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let n_locals = body.locals.len();
    let arity = body.arity as usize;
    // Parameters the body never writes, whole or through a projection, and
    // never borrows.
    let mut stable = vec![false; n_locals];
    for (i, slot) in stable.iter_mut().enumerate().take(arity + 1).skip(1) {
        *slot = !matches!(
            tcx.kind_of(body.locals[i].ty),
            gossamer_types::TyKind::Ref { .. }
        );
    }
    let mut defs = vec![0u32; n_locals];
    let mut misused = vec![false; n_locals];
    let mark = |flags: &mut Vec<bool>, local: Local| {
        if let Some(slot) = flags.get_mut(local.0 as usize) {
            *slot = true;
        }
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    if let Some(slot) = stable.get_mut(place.local.0 as usize) {
                        *slot = false;
                    }
                    if place.projection.is_empty()
                        && let Some(count) = defs.get_mut(place.local.0 as usize)
                    {
                        *count += 1;
                    }
                    if !place.projection.is_empty() {
                        mark(&mut misused, place.local);
                    }
                    match rvalue {
                        // The candidate read itself.
                        Rvalue::CallIntrinsic { name, .. } if *name == "gos_enum_load" => {}
                        Rvalue::Ref { place: p, .. } => {
                            mark(&mut misused, p.local);
                            if let Some(slot) = stable.get_mut(p.local.0 as usize) {
                                *slot = false;
                            }
                        }
                        Rvalue::Len(_) | Rvalue::StaticLoad(_) => {}
                        other => {
                            // A read through a projection takes a field; a
                            // whole read hands the binding on.
                            for op in rvalue_reads(other) {
                                if let Operand::Copy(p) = op
                                    && p.projection.is_empty()
                                {
                                    mark(&mut misused, p.local);
                                }
                            }
                        }
                    }
                }
                StatementKind::SetDiscriminant { place, .. } => {
                    mark(&mut misused, place.local);
                    if let Some(slot) = stable.get_mut(place.local.0 as usize) {
                        *slot = false;
                    }
                }
                StatementKind::StaticStore {
                    value: Operand::Copy(p),
                    ..
                } => mark(&mut misused, p.local),
                _ => {}
            }
        }
        match &block.terminator {
            // A Gossamer callee borrows its by-value arguments.
            Terminator::Call {
                callee: Operand::FnRef { .. },
                destination,
                ..
            } => {
                mark(&mut misused, destination.local);
                if let Some(slot) = stable.get_mut(destination.local.0 as usize) {
                    *slot = false;
                }
            }
            Terminator::Call {
                args, destination, ..
            } => {
                for op in args {
                    if let Operand::Copy(p) = op
                        && p.projection.is_empty()
                    {
                        mark(&mut misused, p.local);
                    }
                }
                mark(&mut misused, destination.local);
                if let Some(slot) = stable.get_mut(destination.local.0 as usize) {
                    *slot = false;
                }
            }
            Terminator::SwitchInt {
                discriminant: Operand::Copy(p),
                ..
            }
            | Terminator::Assert {
                cond: Operand::Copy(p),
                ..
            } if p.projection.is_empty() => mark(&mut misused, p.local),
            _ => {}
        }
    }
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            let StatementKind::Assign { place, rvalue } = &mut stmt.kind else {
                continue;
            };
            let Rvalue::CallIntrinsic { name, args } = rvalue else {
                continue;
            };
            let binding = place.local.0 as usize;
            if *name != "gos_enum_load"
                || args.len() != 2
                || !place.projection.is_empty()
                || binding <= arity
                || binding >= n_locals
                || defs[binding] != 1
                || misused[binding]
                || body.locals[binding].region
                || !tcx.is_boxed_payload_binding(body.locals[binding].ty)
            {
                continue;
            }
            let from_stable_param = matches!(args.first(), Some(Operand::Copy(p))
                if p.projection.is_empty() && stable.get(p.local.0 as usize).copied().unwrap_or(false));
            if from_stable_param {
                args.push(Operand::Const(ConstValue::Int(1)));
            }
        }
    }
}

/// The operands an rvalue reads.
fn rvalue_reads(rvalue: &Rvalue) -> Vec<&Operand> {
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
