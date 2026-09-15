//! An `Option` / `Result` / inline enum reached through a `&mut`.
//!
//! The two words are held by value, so a mutable reference to one is an
//! address. These passes make the IR say which of the two an operand names:
//! a read goes through the reference, and a copy-back out of one lands in
//! its own local before the destination's old payload is released.

use gossamer_types::{TyCtxt, TyKind};

use crate::ir::{
    Body, LocalDecl, Operand, Place, Projection, Rvalue, Statement, StatementKind, Terminator,
};

/// Whether `ty` is an `Option` / `Result` / inline user enum: two words held
/// by value, so a reference to one is an address rather than the value.
pub(crate) fn is_two_word_carrier(tcx: &TyCtxt, ty: gossamer_types::Ty) -> bool {
    matches!(
        tcx.kind_of(ty),
        TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1
    ) || tcx.is_inline_enum_ty(ty)
}

/// Names the builtin and runtime consumers that take a carrier by value. A
/// user function takes the reference itself, so its arguments are left as
/// they are written.
fn consumes_carrier_by_value(callee: &Operand) -> bool {
    match callee {
        Operand::Const(crate::ir::ConstValue::Str(name)) => {
            name.starts_with("gos_rt_")
                || matches!(
                    name.as_str(),
                    "__debug"
                        | "__concat"
                        | "__fmt_prec"
                        | "println"
                        | "print"
                        | "eprintln"
                        | "eprint"
                        | "panic"
                )
        }
        _ => false,
    }
}

/// Rewrites every operand that reads a two-word carrier through a reference
/// so it names the carrier rather than the address.
///
/// The interpreter reads through a reference wherever it finds one; the
/// compiled tiers take the operand as written, so a `&mut Option<T>` read as
/// a value has to say so in the IR.
pub(crate) fn deref_carrier_reads(body: &mut Body, tcx: &TyCtxt) {
    let carrier_ref: Vec<bool> = body
        .locals
        .iter()
        .map(|decl| match tcx.kind_of(decl.ty) {
            // A shared borrow of a carrier is transparent - it carries the
            // two words themselves - so only a `&mut` names an address.
            TyKind::Ref {
                inner,
                mutability: gossamer_types::Mutbl::Mut,
            } => is_two_word_carrier(tcx, *inner),
            _ => false,
        })
        .collect();
    if !carrier_ref.iter().any(|is| *is) {
        return;
    }
    let mut deref = |operand: &mut Operand| {
        if let Operand::Copy(place) = operand
            && place.projection.is_empty()
            && carrier_ref
                .get(place.local.0 as usize)
                .copied()
                .unwrap_or(false)
        {
            place.projection.push(Projection::Deref);
        }
    };
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            let StatementKind::Assign { rvalue, .. } = &mut stmt.kind else {
                continue;
            };
            match rvalue {
                Rvalue::CallIntrinsic { args, .. } => args.iter_mut().for_each(&mut deref),
                Rvalue::Use(operand) => deref(operand),
                Rvalue::BinaryOp { lhs, rhs, .. } => {
                    deref(lhs);
                    deref(rhs);
                }
                Rvalue::UnaryOp { operand, .. } => deref(operand),
                Rvalue::Cast { operand, .. } => deref(operand),
                _ => {}
            }
        }
        match &mut block.terminator {
            Terminator::Call { callee, args, .. } if consumes_carrier_by_value(callee) => {
                args.iter_mut().for_each(&mut deref);
            }
            Terminator::SwitchInt { discriminant, .. } => deref(discriminant),
            _ => {}
        }
    }
}

/// Reads a carrier the caller copies back out of a reference into a fresh
/// temporary first.
///
/// A `&mut` argument's copy-back reads through a reference that can address
/// the very local it writes: the release of the destination's old payload
/// would then be a release of the value being copied in. Landing the read in
/// its own local first keeps the two apart, so each side's share is accounted
/// exactly once.
pub(crate) fn split_carrier_writebacks(body: &mut Body, tcx: &TyCtxt) {
    let is_carrier: Vec<bool> = body
        .locals
        .iter()
        .map(|decl| is_two_word_carrier(tcx, decl.ty))
        .collect();
    if !is_carrier.iter().any(|is| *is) {
        return;
    }
    let mut fresh: Vec<LocalDecl> = Vec::new();
    for block in &mut body.blocks {
        let mut out: Vec<Statement> = Vec::with_capacity(block.stmts.len());
        for stmt in block.stmts.drain(..) {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                out.push(stmt);
                continue;
            };
            let carrier_dest = place.projection.is_empty()
                && is_carrier
                    .get(place.local.0 as usize)
                    .copied()
                    .unwrap_or(false);
            let through_ref = matches!(
                rvalue,
                Rvalue::Use(Operand::Copy(src))
                    if src.projection.as_slice() == [Projection::Deref]
            );
            if !(carrier_dest && through_ref) {
                out.push(stmt);
                continue;
            }
            let ty = body.locals[place.local.0 as usize].ty;
            let temp = crate::ir::Local(
                u32::try_from(body.locals.len() + fresh.len()).unwrap_or(u32::MAX),
            );
            fresh.push(LocalDecl {
                ty,
                debug_name: None,
                mutable: false,
                region: body.locals[place.local.0 as usize].region,
            });
            let dest = place.clone();
            let span = stmt.span;
            out.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(temp),
                    rvalue: rvalue.clone(),
                },
                span,
                inlined: None,
            });
            out.push(Statement {
                kind: StatementKind::Assign {
                    place: dest,
                    rvalue: Rvalue::Use(Operand::Copy(Place::local(temp))),
                },
                span,
                inlined: None,
            });
        }
        block.stmts = out;
    }
    body.locals.extend(fresh);
}
