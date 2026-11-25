//! `==` / `!=` between a computed float and a literal its type cannot hold.
//!
//! `let a = x + y; a == 0.3` is false for the `x` and `y` that sum to 0.3 in
//! decimal, since `0.3` has no binary form: the sum rounds to one neighbour
//! of it and the literal to another. A literal the type holds exactly
//! (`0.5`, `0.0`, `1e3`) compares as written, and a binding every write of
//! which is a literal holds exactly what was written, so neither is refused.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use gossamer_ast::visitor::{Visitor, walk_expr, walk_stmt};
use gossamer_ast::{
    BinaryOp, Expr, ExprKind, Literal, NodeId, PatternKind, SourceFile, Stmt, StmtKind, UnaryOp,
};
use gossamer_resolve::{Resolution, Resolutions};

use crate::{FloatTy, TyCtxt, TyKind, TypeDiagnostic, TypeError, TypeTable};

/// The lint id for an inexact literal compared with a float that is not
/// computed in sight (a parameter, a field).
pub const FLOAT_EQ_LINT: &str = "float_eq_zero";

/// A float `==` / `!=` against a literal the float type cannot hold exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InexactFloatEquality {
    /// The comparison.
    pub span: gossamer_lex::Span,
    /// The refusal this comparison is, when the other operand computes.
    pub refusal: Option<TypeDiagnostic>,
}

/// Every float equality in `sf` that compares a computed value with a literal
/// its type cannot hold exactly, refused.
#[must_use]
pub fn check_float_equalities(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<TypeDiagnostic> {
    find_inexact_float_equalities(sf, resolutions, table, tcx)
        .into_iter()
        .filter_map(|found| found.refusal)
        .collect()
}

/// Every float equality in `sf` against a literal its type cannot hold
/// exactly, each carrying its refusal when the other operand computes.
#[must_use]
pub fn find_inexact_float_equalities(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<InexactFloatEquality> {
    let mut sources = Sources {
        resolutions,
        writes: HashMap::new(),
    };
    sources.visit_source_file(sf);
    let mut find = Find {
        resolutions,
        table,
        tcx,
        writes: &sources.writes,
        out: Vec::new(),
    };
    find.visit_source_file(sf);
    find.out
        .into_iter()
        .filter(|found| sf.is_program_text(found.span))
        .collect()
}

/// What one write of a local binding gives it: whether the value computes
/// on its own, and the locals it copies.
#[derive(Default)]
struct Summary {
    computes: bool,
    aliases: Vec<NodeId>,
}

/// Summarises `expr` as a value: arithmetic and calls compute, a binding
/// copies whatever its own writes give it, and a literal is held as written.
fn summarize(expr: &Expr, resolutions: &Resolutions, out: &mut Summary) {
    match &expr.kind {
        ExprKind::Binary { op, .. } => {
            out.computes |= matches!(
                op,
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem
            );
        }
        ExprKind::Call { .. } | ExprKind::MethodCall { .. } => out.computes = true,
        ExprKind::Unary {
            op: UnaryOp::Neg,
            operand,
        } => summarize(operand, resolutions, out),
        ExprKind::Block(block) => {
            if let Some(tail) = block.tail.as_deref() {
                summarize(tail, resolutions, out);
            }
        }
        ExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            summarize(then_branch, resolutions, out);
            if let Some(branch) = else_branch.as_deref() {
                summarize(branch, resolutions, out);
            }
        }
        ExprKind::Path(path) if path.segments.len() == 1 => {
            if let Some(Resolution::Local(id)) = resolutions.get(expr.id) {
                out.aliases.push(id);
            }
        }
        _ => {}
    }
}

struct Sources<'a> {
    resolutions: &'a Resolutions,
    writes: HashMap<NodeId, Vec<Summary>>,
}

impl Visitor for Sources<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let StmtKind::Let { pattern, init, .. } = &stmt.kind
            && let PatternKind::Ident {
                subpattern: None, ..
            } = &pattern.kind
        {
            let mut summary = Summary::default();
            if let Some(init) = init.as_deref() {
                summarize(init, self.resolutions, &mut summary);
            }
            self.writes.entry(pattern.id).or_default().push(summary);
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let ExprKind::Assign { place, value, op } = &expr.kind
            && let ExprKind::Path(path) = &place.kind
            && path.segments.len() == 1
            && let Some(Resolution::Local(id)) = self.resolutions.get(place.id)
        {
            let mut summary = Summary::default();
            if matches!(op, gossamer_ast::AssignOp::Assign) {
                summarize(value, self.resolutions, &mut summary);
            } else {
                summary.computes = true;
            }
            self.writes.entry(id).or_default().push(summary);
        }
        walk_expr(self, expr);
    }
}

struct Find<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    writes: &'a HashMap<NodeId, Vec<Summary>>,
    out: Vec<InexactFloatEquality>,
}

impl Find<'_> {
    fn float_ty(&self, expr: &Expr) -> Option<FloatTy> {
        let ty = self.table.get(expr.id)?;
        match self.tcx.kind(ty)? {
            TyKind::Float(float) => Some(*float),
            _ => None,
        }
    }

    /// Whether `expr` computes its value: arithmetic, a call, or a binding
    /// some write of which computes.
    fn computes(&self, expr: &Expr) -> bool {
        let mut summary = Summary::default();
        summarize(expr, self.resolutions, &mut summary);
        let mut seen = HashSet::new();
        let mut pending = summary.aliases;
        if summary.computes {
            return true;
        }
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            for write in self.writes.get(&id).into_iter().flatten() {
                if write.computes {
                    return true;
                }
                pending.extend(write.aliases.iter().copied());
            }
        }
        false
    }
}

impl Visitor for Find<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if let ExprKind::Binary { op, lhs, rhs } = &expr.kind
            && matches!(op, BinaryOp::Eq | BinaryOp::Ne)
        {
            for (literal_side, other) in [(lhs, rhs), (rhs, lhs)] {
                let Some(text) = float_literal_text(literal_side) else {
                    continue;
                };
                let Some(ty) = self.float_ty(other).or_else(|| self.float_ty(literal_side)) else {
                    continue;
                };
                if float_literal_is_exact(&text, ty) || float_literal_text(other).is_some() {
                    continue;
                }
                let refusal = self.computes(other).then(|| TypeDiagnostic {
                    error: TypeError::InexactFloatEquality {
                        op: if *op == BinaryOp::Eq { "==" } else { "!=" },
                        literal: text,
                        ty: match ty {
                            FloatTy::F32 => "f32",
                            FloatTy::F64 => "f64",
                        },
                    },
                    span: expr.span,
                });
                self.out.push(InexactFloatEquality {
                    span: expr.span,
                    refusal,
                });
                break;
            }
        }
        walk_expr(self, expr);
    }
}

/// The literal `expr` spells, sign included, when it is a float literal.
fn float_literal_text(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Literal(Literal::Float(text)) => Some(text.clone()),
        ExprKind::Unary {
            op: UnaryOp::Neg,
            operand,
        } => float_literal_text(operand).map(|text| format!("-{text}")),
        _ => None,
    }
}

/// Whether the decimal float literal `text` names a value `ty` holds exactly.
#[must_use]
pub fn float_literal_is_exact(text: &str, ty: FloatTy) -> bool {
    let text = text.trim_start_matches('-').replace('_', "");
    let text = text
        .strip_suffix("f64")
        .or_else(|| text.strip_suffix("f32"))
        .unwrap_or(&text);
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(at) => (&text[..at], &text[at + 1..]),
        None => (text, "0"),
    };
    let Ok(exponent) = exponent.parse::<i64>() else {
        return false;
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return true;
    }
    let Ok(mut m) = digits.parse::<u128>() else {
        return false;
    };
    // value = m * 10^scale = m * 5^scale * 2^scale
    let scale = exponent - i64::try_from(fraction.len()).unwrap_or(i64::MAX);
    let mut twos = scale;
    if scale >= 0 {
        for _ in 0..scale {
            let Some(next) = m.checked_mul(5) else {
                return false;
            };
            m = next;
        }
    } else {
        for _ in 0..scale.unsigned_abs() {
            if m % 5 != 0 {
                return false;
            }
            m /= 5;
        }
    }
    while m % 2 == 0 {
        m /= 2;
        twos += 1;
    }
    let (mantissa_bits, min_exp, max_exp) = match ty {
        FloatTy::F32 => (24, -149, 127),
        FloatTy::F64 => (53, -1074, 1023),
    };
    let bits = 128 - i64::from(m.leading_zeros());
    bits <= mantissa_bits && twos >= min_exp && twos + bits - 1 <= max_exp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_exactness_matches_binary_fractions() {
        for exact in [
            "0.5", "0.0", "1.0", "0.25", "1e3", "2.5e-1", "1_024.0", "-0.75", "1e10",
        ] {
            assert!(float_literal_is_exact(exact, FloatTy::F64), "{exact}");
        }
        for inexact in ["0.1", "0.3", "1e-9", "0.2", "3.14159", "1e300"] {
            assert!(!float_literal_is_exact(inexact, FloatTy::F64), "{inexact}");
        }
        assert!(float_literal_is_exact("16777216.0", FloatTy::F32));
        assert!(!float_literal_is_exact("16777217.0", FloatTy::F32));
        assert!(float_literal_is_exact("16777217.0", FloatTy::F64));
    }
}
