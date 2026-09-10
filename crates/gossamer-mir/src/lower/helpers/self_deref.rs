//! Reads of a reference receiver load through it.
//!
//! `&self` names the same value `self` does, so a method body spells its
//! receiver both ways: `*self` is an explicit load and a bare `self` is the
//! same read written shorter. The lowering only produced the load for the
//! explicit spelling, leaving the bare one holding whatever the receiver
//! parameter carries - which is the referent's address, because the call site
//! takes a reference. Where the address and the value have the same shape,
//! as they do for every heap-backed type, the two readings coincide and
//! nothing shows; for a scalar they differ, and the body read the address as
//! though it were the number.
//!
//! Every read of the receiver therefore loads, and the two spellings produce
//! the same MIR. The one read that keeps the reference is the receiver
//! argument of another method that declares one, which is asking for the
//! reference rather than for the value behind it.

use std::collections::{HashMap, HashSet};

use gossamer_types::{IntTy, Ty, TyCtxt, TyKind};

use crate::ir::{
    Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue, Statement, StatementKind,
    Terminator,
};

/// Rewrites every body whose receiver is a reference to a scalar so each read
/// of that receiver loads the value it names.
pub(crate) fn load_reference_receiver_reads(bodies: &mut [Body], tcx: &mut TyCtxt) {
    let takes_reference_receiver = reference_receiver_methods(bodies, tcx);
    for index in 0..bodies.len() {
        let Some(pointee) = scalar_reference_receiver(&bodies[index], tcx) else {
            continue;
        };
        rewrite_body(&mut bodies[index], pointee, &takes_reference_receiver, tcx);
    }
}

/// Names of the method bodies whose own receiver is an address, so a call to
/// one wants the reference rather than the value behind it.
fn reference_receiver_methods(bodies: &[Body], tcx: &TyCtxt) -> HashSet<String> {
    bodies
        .iter()
        .filter(|body| is_method(body))
        .filter(|body| {
            body.locals
                .get(1)
                .is_some_and(|recv| receiver_is_address(recv.ty, tcx))
        })
        .map(|body| body.name.clone())
        .collect()
}

/// Whether a receiver of type `ty` carries an address rather than a value.
///
/// A payload enum is a single word either way, so a shared receiver carries
/// the node itself and only `&mut self` - the receiver a body rebinds whole -
/// names the caller's slot.
fn receiver_is_address(ty: Ty, tcx: &TyCtxt) -> bool {
    let TyKind::Ref { mutability, inner } = tcx.kind_of(ty) else {
        return false;
    };
    !tcx.is_payload_enum(*inner) || matches!(mutability, gossamer_types::Mutbl::Mut)
}

/// Whether `body` is a method, which is what makes local 1 its receiver.
fn is_method(body: &Body) -> bool {
    body.arity >= 1 && body.name.contains("::")
}

/// The value a body's receiver refers to, when a read of that receiver has to
/// load through it.
///
/// A scalar is one such: for every heap-backed type the reference and the
/// value are the same machine word, so a read of either answers the same. A
/// `&mut self` payload enum is the other, because that receiver names the
/// caller's slot so the body can rebind the whole node through it.
fn scalar_reference_receiver(body: &Body, tcx: &TyCtxt) -> Option<Ty> {
    if !is_method(body) {
        return None;
    }
    let TyKind::Ref { mutability, inner } = tcx.kind_of(body.locals.get(1)?.ty) else {
        return None;
    };
    let is_mut = matches!(mutability, gossamer_types::Mutbl::Mut);
    let inner = *inner;
    (matches!(
        tcx.kind_of(inner),
        TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
    ) || (is_mut && tcx.is_payload_enum(inner)))
    .then_some(inner)
}

/// Whether `operand` reads the whole receiver, rather than a projection of it.
fn reads_receiver(operand: &Operand) -> bool {
    matches!(operand, Operand::Copy(place) if place.local == Local(1) && place.projection.is_empty())
}

fn rewrite_body(
    body: &mut Body,
    pointee: Ty,
    takes_reference_receiver: &HashSet<String>,
    tcx: &mut TyCtxt,
) {
    let zero_ty = tcx.int_ty(IntTy::I64);
    // One load per reading statement rather than one for the whole body: a
    // `&mut self` method may write through the receiver between two reads, and
    // a cached value would answer with what was there before the write.
    for block_index in 0..body.blocks.len() {
        let mut rewritten: Vec<Statement> = Vec::new();
        let statements = std::mem::take(&mut body.blocks[block_index].stmts);
        for mut statement in statements {
            let mut loaded: Option<Local> = None;
            visit_statement_operands(&mut statement.kind, &mut |operand| {
                if !reads_receiver(operand) {
                    return None;
                }
                Some(*loaded.get_or_insert_with(|| {
                    let local =
                        emit_receiver_load(&mut rewritten, body, pointee, zero_ty, statement.span);
                    local
                }))
            });
            rewritten.push(statement);
        }
        let span = body.span;
        let mut loaded: Option<Local> = None;
        let mut terminator = std::mem::replace(
            &mut body.blocks[block_index].terminator,
            Terminator::Unreachable,
        );
        visit_terminator_operands(&mut terminator, takes_reference_receiver, &mut |operand| {
            if !reads_receiver(operand) {
                return None;
            }
            Some(*loaded.get_or_insert_with(|| {
                emit_receiver_load(&mut rewritten, body, pointee, zero_ty, span)
            }))
        });
        body.blocks[block_index].terminator = terminator;
        body.blocks[block_index].stmts = rewritten;
    }
}

/// Appends `tmp = gos_load(self, 0)` and answers the local holding the value.
///
/// The same intrinsic the explicit `*self` spelling lowers to, so both
/// spellings reach the backends as one shape.
fn emit_receiver_load(
    statements: &mut Vec<Statement>,
    body: &mut Body,
    pointee: Ty,
    zero_ty: Ty,
    span: gossamer_lex::Span,
) -> Local {
    let zero = push_local(body, zero_ty);
    statements.push(Statement {
        kind: StatementKind::Assign {
            place: Place::local(zero),
            rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
        },
        span,
    });
    let value = push_local(body, pointee);
    statements.push(Statement {
        kind: StatementKind::Assign {
            place: Place::local(value),
            rvalue: Rvalue::CallIntrinsic {
                name: "gos_load",
                args: vec![
                    Operand::Copy(Place::local(Local(1))),
                    Operand::Copy(Place::local(zero)),
                ],
            },
        },
        span,
    });
    value
}

fn push_local(body: &mut Body, ty: Ty) -> Local {
    let local = Local(u32::try_from(body.locals.len()).expect("local index fits in u32"));
    body.locals.push(LocalDecl {
        ty,
        debug_name: None,
        mutable: false,
        region: false,
    });
    local
}

/// Applies `f` to every operand a statement reads, replacing the operand with
/// a read of the local `f` answers.
fn visit_statement_operands(
    kind: &mut StatementKind,
    f: &mut impl FnMut(&Operand) -> Option<Local>,
) {
    match kind {
        StatementKind::Assign { rvalue, .. } => visit_rvalue_operands(rvalue, f),
        StatementKind::StaticStore { value, .. } => replace_operand(value, f),
        StatementKind::IterSource { source, .. } => replace_operand(source, f),
        _ => {}
    }
}

fn visit_rvalue_operands(rvalue: &mut Rvalue, f: &mut impl FnMut(&Operand) -> Option<Local>) {
    match rvalue {
        Rvalue::Use(operand) | Rvalue::UnaryOp { operand, .. } => replace_operand(operand, f),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            replace_operand(lhs, f);
            replace_operand(rhs, f);
        }
        Rvalue::Cast { operand, .. } => replace_operand(operand, f),
        Rvalue::Aggregate { operands, .. } => {
            for operand in operands {
                replace_operand(operand, f);
            }
        }
        Rvalue::Repeat { value, .. } => replace_operand(value, f),
        // The receiver's own load reads the reference by construction, and a
        // taken reference is asking for the reference itself.
        Rvalue::CallIntrinsic { name, args } => {
            if *name == "gos_load" {
                return;
            }
            for operand in args {
                replace_operand(operand, f);
            }
        }
        Rvalue::Ref { .. } | Rvalue::Len(_) | Rvalue::StaticLoad(_) => {}
    }
}

fn visit_terminator_operands(
    terminator: &mut Terminator,
    takes_reference_receiver: &HashSet<String>,
    f: &mut impl FnMut(&Operand) -> Option<Local>,
) {
    match terminator {
        Terminator::SwitchInt { discriminant, .. } => replace_operand(discriminant, f),
        Terminator::Assert { cond, .. } => replace_operand(cond, f),
        Terminator::Call { callee, args, .. } => {
            // A callee that declares a reference receiver is asking for the
            // reference this body holds, not for the value behind it.
            let keeps_reference = matches!(
                callee,
                Operand::Const(ConstValue::Str(name)) if takes_reference_receiver.contains(name)
            );
            for (index, operand) in args.iter_mut().enumerate() {
                if index == 0 && keeps_reference {
                    continue;
                }
                replace_operand(operand, f);
            }
        }
        _ => {}
    }
}

fn replace_operand(operand: &mut Operand, f: &mut impl FnMut(&Operand) -> Option<Local>) {
    if let Some(local) = f(operand) {
        *operand = Operand::Copy(Place::local(local));
    }
}

/// Method bodies keyed by name, for callers that need the receiver contract of
/// a body they do not hold.
pub(crate) fn receiver_is_reference(bodies: &[Body], tcx: &TyCtxt) -> HashMap<String, bool> {
    bodies
        .iter()
        .filter(|body| is_method(body))
        .filter_map(|body| {
            body.locals
                .get(1)
                .map(|recv| (body.name.clone(), receiver_is_address(recv.ty, tcx)))
        })
        .collect()
}
