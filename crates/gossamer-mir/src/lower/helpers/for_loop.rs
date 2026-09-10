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

pub(crate) fn is_aggregate_ctor_callee(callee: &Operand) -> bool {
    if let Operand::Const(ConstValue::Str(name)) = callee {
        return matches!(
            name.as_str(),
            // Result / Option payload constructors. Wrapping a
            // heap-owning local into `Result::Ok(R { xs: v })`
            // moves the Vec into the returned aggregate.
            "gos_rt_result_new"
                | "gos_rt_option_new"
                | "gos_rt_option_some"
                // Synthetic tuple / struct lowerings.
                | "__tuple"
                | "__struct"
        );
    }
    false
}

/// Runtime helpers that append through the container handed to them as
/// their first argument. Each writes into that container's own storage,
/// returns unit, takes no share of the container, and keeps no pointer to
/// it, so reading a container here does not make some other holder its
/// owner. The re-binding `push_back` / `push_front` family is excluded: it
/// answers the container pointer, and its result is what the caller keeps.
pub(crate) fn appends_through_container(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_vec_push" | "gos_rt_vec_push_i64" | "gos_rt_vec_push_i128"
    )
}

pub(crate) fn returns_borrowed_pointer(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_os_args"
            // Container element accessors return an interior borrow into the
            // container's own storage, not an owned value. The container
            // still owns each element and deep-frees it on drop, so releasing
            // the borrow here double-frees - `x[0]` on a `Vec<Vec<T>>` (the
            // inner Vec) or a `Vec<String>` (the element string) frees memory
            // the outer container reclaims again at scope end.
            | "gos_rt_vec_get_i64"
            // Same interior-borrow contract as the checked reader; the
            // counted-loop element read emits this when the index is
            // provably in range.
            | "gos_rt_vec_get_i64_unchecked"
            | "gos_rt_vec_get_ptr"
            | "gos_rt_vec_first"
            | "gos_rt_vec_last"
            // `m.or_insert(k, default)` / `m.get_or(k, default)` on a
            // Vec-valued map hand back a borrow of the map's stored
            // value (like Rust's `&mut V`), not an owned vec. The map
            // still owns and frees each value. Freeing the returned
            // borrow here would double-free: for a *fresh* key the
            // returned word aliases the inserted value temp (which the
            // ctor-cleanup already frees), and for a present key it
            // aliases the value another binding owns.
            | "gos_rt_map_or_insert_str_i64"
            | "gos_rt_map_or_insert_typed_str_i64"
            | "gos_rt_map_or_insert_i64_i64"
            | "gos_rt_map_get_or_str_i64"
            | "gos_rt_map_get_or_typed_str_i64"
            | "gos_rt_map_get_or_i64"
            // A raw word read through a pointer (closure-env capture
            // unpacks, handle field loads). The pointee's owner keeps the
            // only reference this local sees; a lifted closure freeing an
            // env-loaded container would tear down storage the enclosing
            // frame still reads on the next call.
            | "gos_load"
    )
}

pub(crate) fn aggr_size_bytes(tcx: &gossamer_types::TyCtxt, ty: Ty) -> i64 {
    use gossamer_types::TyKind;
    let bytes = match tcx.kind_of(ty) {
        TyKind::Tuple(elems) => {
            let total: i64 = elems
                .iter()
                .map(|t| aggr_size_bytes(tcx, *t).max(8) / 8)
                .sum();
            total.max(1) * 8
        }
        TyKind::Array { elem, len } => {
            let elem_bytes = aggr_size_bytes(tcx, *elem).max(8);
            i64::try_from(len.to_usize())
                .unwrap_or(1)
                .saturating_mul(elem_bytes)
        }
        TyKind::Adt { def, .. } => {
            // `Result<T,E>` / `Option<T>` are the 2-word by-value `i128`
            // (16-byte) representation - two slots as an aggregate element.
            if def.local == u32::MAX || def.local == u32::MAX - 1 {
                16
            } else if let Some(field_tys) = tcx.struct_field_tys(*def) {
                let total: i64 = field_tys
                    .iter()
                    .map(|t| aggr_size_bytes(tcx, *t).max(8) / 8)
                    .sum();
                total.max(1) * 8
            } else {
                // Other sentinel Adts (DirInfo, …): single heap-pointer slot.
                8
            }
        }
        _ => 8,
    };
    bytes.max(8)
}

pub(crate) struct ForLoopShape<'h> {
    pub(crate) iter_expr: &'h HirExpr,
    pub(crate) loop_pat: &'h HirPat,
    pub(crate) body: &'h HirExpr,
}

pub(crate) fn detect_for_loop(body: &HirExpr) -> Option<ForLoopShape<'_>> {
    let HirExprKind::Block(block) = &body.kind else {
        return None;
    };
    if !block.stmts.is_empty() {
        return None;
    }
    let tail = block.tail.as_deref()?;
    let HirExprKind::Match { scrutinee, arms } = &tail.kind else {
        return None;
    };
    if arms.len() != 2 {
        return None;
    }
    let HirExprKind::MethodCall {
        receiver,
        name,
        args,
        owner: None,
    } = &scrutinee.kind
    else {
        return None;
    };
    if name.name != "next" || !args.is_empty() {
        return None;
    }
    let some_arm = &arms[0];
    let none_arm = &arms[1];
    let HirPatKind::Variant {
        name: some_name,
        fields: some_fields,
    } = &some_arm.pattern.kind
    else {
        return None;
    };
    if some_name.name != "Some" || some_fields.len() != 1 {
        return None;
    }
    let HirPatKind::Variant {
        name: none_name,
        fields: none_fields,
    } = &none_arm.pattern.kind
    else {
        return None;
    };
    if none_name.name != "None" || !none_fields.is_empty() {
        return None;
    }
    Some(ForLoopShape {
        iter_expr: receiver,
        loop_pat: &some_fields[0],
        body: &some_arm.body,
    })
}

pub(crate) fn enumerate_inner_expr(expr: &HirExpr) -> Option<&HirExpr> {
    let HirExprKind::MethodCall { receiver, name, .. } = &expr.kind else {
        return None;
    };
    if name.name != "enumerate" {
        return None;
    }
    let inner: &HirExpr = receiver;
    if let HirExprKind::MethodCall {
        receiver: inner_recv,
        name: inner_name,
        ..
    } = &inner.kind
        && inner_name.name == "iter"
    {
        return Some(inner_recv);
    }
    Some(inner)
}

pub(crate) fn literal_u64(expr: &HirExpr) -> Option<u64> {
    let HirExprKind::Literal(HirLiteral::Int(text)) = &expr.kind else {
        return None;
    };
    let parsed = parse_int(text)?;
    u64::try_from(parsed).ok()
}

pub(crate) fn literal_to_const(lit: &HirLiteral) -> ConstValue {
    match lit {
        HirLiteral::Unit => ConstValue::Unit,
        HirLiteral::Bool(b) => ConstValue::Bool(*b),
        HirLiteral::Int(text) => ConstValue::Int(parse_int(text).unwrap_or(0)),
        HirLiteral::Float(text) => ConstValue::Float(parse_float(text).to_bits()),
        HirLiteral::Char(c) => ConstValue::Char(*c),
        HirLiteral::String(text) => ConstValue::Str(text.clone()),
        HirLiteral::Byte(b) => ConstValue::Int(i128::from(*b)),
        HirLiteral::ByteString(bytes) => {
            ConstValue::Str(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

pub(crate) fn parse_int(text: &str) -> Option<i128> {
    let cleaned = strip_int_suffix(text).replace('_', "");
    if let Some(rest) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        return i128::from_str_radix(rest, 16).ok();
    }
    if let Some(rest) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        return i128::from_str_radix(rest, 2).ok();
    }
    if let Some(rest) = cleaned
        .strip_prefix("0o")
        .or_else(|| cleaned.strip_prefix("0O"))
    {
        return i128::from_str_radix(rest, 8).ok();
    }
    cleaned.parse::<i128>().ok()
}

pub(crate) fn parse_float(text: &str) -> f64 {
    strip_float_suffix(text)
        .replace('_', "")
        .parse::<f64>()
        .unwrap_or(0.0)
}

/// Drops a float literal's width suffix, leaving the digits and any
/// `_` separators the source wrote.
pub(crate) fn strip_float_suffix(text: &str) -> &str {
    for suffix in &["f32", "f64"] {
        if let Some(stripped) = text.strip_suffix(suffix) {
            return stripped;
        }
    }
    text
}

pub(crate) fn strip_int_suffix(text: &str) -> String {
    const SUFFIXES: &[&str] = &[
        "i128", "u128", "isize", "usize", "i64", "u64", "i32", "u32", "i16", "u16", "i8", "u8",
    ];
    for suffix in SUFFIXES {
        if let Some(stripped) = text.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    text.to_string()
}

pub(crate) fn lower_binop(op: HirBinaryOp) -> BinOp {
    match op {
        HirBinaryOp::Add => BinOp::Add,
        HirBinaryOp::Sub => BinOp::Sub,
        HirBinaryOp::Mul => BinOp::Mul,
        HirBinaryOp::Div => BinOp::Div,
        HirBinaryOp::Rem => BinOp::Rem,
        // Logical `&&` / `||` lower to bitwise on the i1/i8
        // bool representation. The truth tables match: for
        // operands `a, b ∈ {0, 1}`, `a & b == a && b` and
        // `a | b == a || b`. (Short-circuit evaluation - not
        // calling the rhs when the lhs settles the result -
        // is a separate concern handled at HIR-to-MIR control
        // flow if/when we expose `&&`/`||` over expressions
        // with side effects.)
        HirBinaryOp::And | HirBinaryOp::BitAnd => BinOp::BitAnd,
        HirBinaryOp::Or | HirBinaryOp::BitOr => BinOp::BitOr,
        HirBinaryOp::BitXor => BinOp::BitXor,
        HirBinaryOp::Shl => BinOp::Shl,
        HirBinaryOp::Shr => BinOp::Shr,
        HirBinaryOp::Eq => BinOp::Eq,
        HirBinaryOp::Ne => BinOp::Ne,
        HirBinaryOp::Lt => BinOp::Lt,
        HirBinaryOp::Le => BinOp::Le,
        HirBinaryOp::Gt => BinOp::Gt,
        HirBinaryOp::Ge => BinOp::Ge,
    }
}

pub(crate) fn exprs_match(a: &HirExpr, b: &HirExpr) -> bool {
    match (&a.kind, &b.kind) {
        (HirExprKind::Path { segments: sa, .. }, HirExprKind::Path { segments: sb, .. }) => {
            sa.len() == sb.len() && sa.iter().zip(sb).all(|(x, y)| x.name == y.name)
        }
        (HirExprKind::Literal(la), HirExprKind::Literal(lb)) => match (la, lb) {
            (HirLiteral::Int(x), HirLiteral::Int(y)) => x == y,
            (HirLiteral::Bool(x), HirLiteral::Bool(y)) => x == y,
            (HirLiteral::Char(x), HirLiteral::Char(y)) => x == y,
            (HirLiteral::String(x), HirLiteral::String(y)) => x == y,
            _ => false,
        },
        (
            HirExprKind::Field {
                receiver: ra,
                name: na,
            },
            HirExprKind::Field {
                receiver: rb,
                name: nb,
            },
        ) => na.name == nb.name && exprs_match(ra, rb),
        (
            HirExprKind::TupleIndex {
                receiver: ra,
                index: ia,
            },
            HirExprKind::TupleIndex {
                receiver: rb,
                index: ib,
            },
        ) => ia == ib && exprs_match(ra, rb),
        _ => false,
    }
}
