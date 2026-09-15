#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::if_not_else)]
#![allow(clippy::single_match_else)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::redundant_else)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::single_match)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::let_and_return)]
#![allow(clippy::needless_collect)]

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_hir::{
    HirAdtKind, HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind,
    HirLiteral, HirMatchArm, HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind, HirUnaryOp,
};
use gossamer_lex::Span;
use gossamer_types::{Ty, TyCtxt};

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};

use super::*;

pub(crate) fn shape_char(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> char {
    use gossamer_types::{FloatTy as Ft, IntTy as It, TyKind};
    match tcx.kind_of(ty) {
        TyKind::Bool => 'b',
        TyKind::Char => 'c',
        // A narrow integer's thunk widens its result to a whole word, which
        // takes the sign for a signed width and zeroes for an unsigned one.
        TyKind::Int(int) => match int {
            It::I8 => 'y',
            It::U8 => 'Y',
            It::I16 => 'k',
            It::U16 => 'K',
            It::I32 => 'j',
            It::U32 => 'J',
            It::I64 | It::U64 | It::Isize | It::Usize | It::I128 | It::U128 => 'i',
        },
        TyKind::Float(f) => match f {
            Ft::F32 => 'g',
            Ft::F64 => 'f',
        },
        TyKind::Unit | TyKind::Never => 'u',
        // Result/Option ride the 2-word packed i128 representation,
        // so a callable returning one needs an i128-ret thunk - an
        // i64-ret thunk would truncate the payload word.
        TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1 => 'r',
        // A reference to a two-word carrier is the one shape whose caller and
        // callee disagree: a combinator hands its callback the ADDRESS of the
        // element's storage, while the callback's own parameter is the carrier
        // by value. The thunk bridges the two by loading, and this is the
        // shape that says so.
        TyKind::Ref { inner, .. }
            if crate::lower::carrier_ref::is_two_word_carrier(tcx, *inner) =>
        {
            'q'
        }
        // Pointer-shaped on 64-bit; refs / strings / aggregates
        // / opaque handles all share the same i64 register slot.
        _ => 'i',
    }
}

#[must_use]
pub fn mangle_callable_shape(tcx: &gossamer_types::TyCtxt, sig: &gossamer_types::FnSig) -> String {
    let mut name = String::with_capacity("__fn_thunk_".len() + sig.inputs.len() + 2);
    name.push_str("__fn_thunk_");
    for input in &sig.inputs {
        name.push(shape_char(tcx, *input));
    }
    name.push('_');
    name.push(shape_char(tcx, sig.output));
    name
}
