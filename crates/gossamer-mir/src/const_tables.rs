//! Fixed arrays of constants a body only reads, which a backend can keep in
//! read-only data instead of building them in the frame.

use std::collections::HashMap;

use gossamer_types::{TyCtxt, TyKind};

use crate::ir::{
    AssertMessage, Body, ConstValue, Local, Operand, Place, Rvalue, StatementKind, Terminator,
};

/// A table shorter than this is cheaper to build in place than to address.
const MIN_TABLE_WORDS: usize = 16;

/// The locals of `body` that hold a constant table, with the words each one's
/// slots hold.
///
/// Such a local is a fixed array of scalars, built once from an array literal
/// whose elements are all constants, and otherwise only read: an element or a
/// field read through it, its length, or a whole copy into another local. No
/// write reaches it and no call, reference, or runtime helper is handed it, so
/// storage that is never written can stand for it.
#[must_use]
pub fn constant_table_locals(body: &Body, tcx: &TyCtxt) -> HashMap<Local, Vec<u64>> {
    let n = body.locals.len();
    let mut words: Vec<Option<Vec<u64>>> = vec![None; n];
    let mut defs = vec![0usize; n];
    let mut disqualified = vec![false; n];
    // The return slot and the parameters hold values other frames see.
    for flag in disqualified.iter_mut().take(body.arity as usize + 1) {
        *flag = true;
    }
    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    let i = place.local.0 as usize;
                    if place.projection.is_empty()
                        && i < n
                        && let Rvalue::Aggregate { operands, .. } = rvalue
                        && let Some(table) = table_words(tcx, body.locals[i].ty, operands)
                    {
                        defs[i] += 1;
                        words[i] = Some(table);
                    } else {
                        write(place, &mut disqualified);
                    }
                    rvalue_uses(rvalue, &mut disqualified);
                }
                StatementKind::SetDiscriminant { place, .. } => write(place, &mut disqualified),
                StatementKind::StaticStore { value, .. } => {
                    operand_use(value, &mut disqualified);
                }
                StatementKind::IterSource { dst, source, .. } => {
                    write(dst, &mut disqualified);
                    operand_use(source, &mut disqualified);
                }
                StatementKind::IterAdapter {
                    dst,
                    upstream,
                    closure_or_arg,
                    ..
                } => {
                    write(dst, &mut disqualified);
                    write(upstream, &mut disqualified);
                    if let Some(op) = closure_or_arg {
                        operand_use(op, &mut disqualified);
                    }
                }
                StatementKind::IterNext {
                    dst_option,
                    iter_place,
                    ..
                } => {
                    write(dst_option, &mut disqualified);
                    write(iter_place, &mut disqualified);
                }
                StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
            }
        }
        match &block.terminator {
            Terminator::SwitchInt { discriminant, .. } => {
                operand_use(discriminant, &mut disqualified);
            }
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                write(destination, &mut disqualified);
                operand_use(callee, &mut disqualified);
                for arg in args {
                    operand_use(arg, &mut disqualified);
                }
            }
            Terminator::Assert { cond, msg, .. } => {
                operand_use(cond, &mut disqualified);
                // The failing path of a bounds check only reads the length of
                // the sequence it names.
                if let AssertMessage::BoundsCheck { index, .. } = msg {
                    operand_use(index, &mut disqualified);
                }
            }
            Terminator::Drop { place, .. } => write(place, &mut disqualified),
            Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::Resume
            | Terminator::Unreachable
            | Terminator::Panic { .. } => {}
        }
    }
    words
        .into_iter()
        .enumerate()
        .filter_map(|(i, table)| {
            let table = table?;
            let local = Local(u32::try_from(i).ok()?);
            (defs[i] == 1 && !disqualified[i]).then_some((local, table))
        })
        .collect()
}

/// The slot words of a fixed array of scalars built from `operands`, when
/// every operand is a constant and the table is long enough to keep.
fn table_words(tcx: &TyCtxt, ty: gossamer_types::Ty, operands: &[Operand]) -> Option<Vec<u64>> {
    let TyKind::Array { elem, .. } = tcx.kind_of(ty) else {
        return None;
    };
    if !matches!(
        tcx.kind_of(*elem),
        TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
    ) || operands.len() < MIN_TABLE_WORDS
    {
        return None;
    }
    operands
        .iter()
        .map(|op| match op {
            // A slot holds the word a scalar reads as: integers two's
            // complement, floats by their bits, `bool` and `char` widened.
            Operand::Const(ConstValue::Int(v)) => Some(*v as u64),
            Operand::Const(ConstValue::Float(bits)) => Some(*bits),
            Operand::Const(ConstValue::Bool(b)) => Some(u64::from(*b)),
            Operand::Const(ConstValue::Char(c)) => Some(u64::from(u32::from(*c))),
            _ => None,
        })
        .collect()
}

/// Marks a local an operand hands on whole: a value handed to another site may
/// be written there.
fn operand_use(op: &Operand, disqualified: &mut [bool]) {
    if let Operand::Copy(place) = op
        && place.projection.is_empty()
        && let Some(flag) = disqualified.get_mut(place.local.0 as usize)
    {
        *flag = true;
    }
}

/// Marks the local a statement or terminator writes.
fn write(place: &Place, disqualified: &mut [bool]) {
    if let Some(flag) = disqualified.get_mut(place.local.0 as usize) {
        *flag = true;
    }
}

fn rvalue_uses(rvalue: &Rvalue, disqualified: &mut [bool]) {
    match rvalue {
        // A whole copy reads the table into storage of the copy's own.
        Rvalue::Use(Operand::Copy(_)) | Rvalue::Len(_) | Rvalue::StaticLoad(_) => {}
        Rvalue::Use(op)
        | Rvalue::UnaryOp { operand: op, .. }
        | Rvalue::Cast { operand: op, .. } => {
            operand_use(op, disqualified);
        }
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            operand_use(lhs, disqualified);
            operand_use(rhs, disqualified);
        }
        Rvalue::Aggregate { operands, .. } => {
            for op in operands {
                operand_use(op, disqualified);
            }
        }
        Rvalue::Repeat { value, .. } => operand_use(value, disqualified),
        Rvalue::Ref { place, .. } => {
            if let Some(flag) = disqualified.get_mut(place.local.0 as usize) {
                *flag = true;
            }
        }
        Rvalue::CallIntrinsic { args, .. } => {
            for op in args {
                operand_use(op, disqualified);
            }
        }
    }
}
