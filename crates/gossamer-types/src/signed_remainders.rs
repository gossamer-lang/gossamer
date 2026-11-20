//! A remainder compared with a positive number while its dividend may be
//! negative.
//!
//! `%` answers a remainder with the sign of the value it divides, as in C,
//! Rust, and Go, so `n % 2 == 1` is false for every negative odd `n`: the
//! remainder there is `-1`. A comparison of `x % k` with a positive literal
//! is reported when `x` is a signed integer nothing shows to be at least
//! zero. A dividend is shown non-negative by its own form (a literal, a
//! mask, a literal `clamp`, an integer's `abs` or `rem_euclid`), or by a
//! binding that counts up from zero: a `for` over a range that starts at
//! zero or above, the index `enumerate` answers, or a `let` of such a value.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use gossamer_ast::visitor::{Visitor, walk_expr, walk_stmt};
use gossamer_ast::{
    BinaryOp, Expr, ExprKind, FieldSelector, Mutability, NodeId, Pattern, PatternKind, SourceFile,
    Stmt, StmtKind,
};
use gossamer_lex::Span;
use gossamer_resolve::{Resolution, Resolutions};

use crate::context::TyCtxt;
use crate::lossy_casts::{int_value_range, literal_int};
use crate::table::TypeTable;

/// The lint these findings belong to.
pub const SIGNED_REMAINDER_LINT: &str = "signed_remainder";

/// One comparison of a signed remainder with a positive number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRemainder {
    /// The comparison.
    pub span: Span,
    /// The headline.
    pub title: String,
    /// The spellings that hold for every sign.
    pub help: String,
}

/// Every comparison in `sf` of a remainder with a positive number whose
/// dividend may be negative.
#[must_use]
pub fn find_signed_remainders(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<SignedRemainder> {
    let mut finder = Finder {
        resolutions,
        table,
        tcx,
        non_negative: HashSet::new(),
        out: Vec::new(),
    };
    finder.visit_source_file(sf);
    finder.out
}

struct Finder<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    /// Bindings whose every value is at least zero.
    non_negative: HashSet<NodeId>,
    out: Vec<SignedRemainder>,
}

impl Finder<'_> {
    fn is_non_negative(&self, expr: &Expr) -> bool {
        if let ExprKind::Path(_) = &expr.kind
            && let Some(Resolution::Local(binding)) = self.resolutions.get(expr.id)
            && self.non_negative.contains(&binding)
        {
            return true;
        }
        // An integer's `abs` and `rem_euclid` answer zero or more: `abs` of
        // the one value with no positive counterpart panics instead.
        if let ExprKind::MethodCall { receiver, name, .. } = &expr.kind
            && matches!(name.name.as_str(), "abs" | "rem_euclid")
            && int_value_range(receiver, self.table, self.tcx).is_some()
        {
            return true;
        }
        int_value_range(expr, self.table, self.tcx).is_some_and(|(least, _)| least >= 0)
    }

    fn mark(&mut self, pattern: &Pattern) {
        if let PatternKind::Ident {
            mutability: Mutability::Immutable,
            subpattern: None,
            ..
        } = &pattern.kind
        {
            self.non_negative.insert(pattern.id);
        }
    }

    /// Records the bindings a `for` loop gives values of zero or more.
    fn mark_loop(&mut self, pattern: &Pattern, iter: &Expr) {
        match &iter.kind {
            ExprKind::Range {
                start: Some(start), ..
            } if self.is_non_negative(start) => self.mark(pattern),
            ExprKind::MethodCall { name, receiver, .. } => match name.name.as_str() {
                "enumerate" => {
                    if let PatternKind::Tuple(parts) = &pattern.kind
                        && let Some(index) = parts.first()
                    {
                        self.mark(index);
                    }
                }
                "rev" | "step_by" | "skip" | "take" => self.mark_loop(pattern, receiver),
                _ => {}
            },
            _ => {}
        }
    }

    fn check(&mut self, compare: &Expr, op: BinaryOp, lhs: &Expr, rhs: &Expr) {
        let (remainder, expected) = match (&lhs.kind, &rhs.kind) {
            (
                ExprKind::Binary {
                    op: BinaryOp::Rem, ..
                },
                _,
            ) => (lhs, rhs),
            (
                _,
                ExprKind::Binary {
                    op: BinaryOp::Rem, ..
                },
            ) => (rhs, lhs),
            _ => return,
        };
        let ExprKind::Binary {
            lhs: dividend,
            rhs: divisor,
            ..
        } = &remainder.kind
        else {
            return;
        };
        let Some(expected) = literal_int(expected).filter(|r| *r > 0) else {
            return;
        };
        let divisor_value = literal_int(divisor);
        if divisor_value.is_some_and(|k| k <= expected) {
            return;
        }
        let Some((least, _)) = int_value_range(dividend, self.table, self.tcx) else {
            return;
        };
        if least >= 0 || self.is_non_negative(dividend) {
            return;
        }
        let value = spelling(dividend).unwrap_or_else(|| "x".to_string());
        let k = divisor_value
            .map(|k| k.to_string())
            .or_else(|| spelling(divisor))
            .unwrap_or_else(|| "k".to_string());
        let (verb, never) = if op == BinaryOp::Eq {
            ("==", "false")
        } else {
            ("!=", "true")
        };
        let title = format!(
            "`{value} % {k} {verb} {expected}` is {never} for every negative `{value}`: `%` keeps the sign of the value it divides, so the remainder of a negative `{value}` is never positive"
        );
        let help = if divisor_value == Some(2) && expected == 1 {
            let odd = if op == BinaryOp::Eq { "!=" } else { "==" };
            format!(
                "`{value} % 2 {odd} 0` tests oddness for every sign; `{value}.rem_euclid(2) {verb} 1` answers the remainder in `0..2`"
            )
        } else {
            format!(
                "`{value}.rem_euclid({k}) {verb} {expected}` compares the remainder in `0..{k}`, the same for every sign"
            )
        };
        self.out.push(SignedRemainder {
            span: compare.span,
            title,
            help,
        });
    }
}

/// How a dividend that is a plain name or field path is written.
fn spelling(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Path(path) if path.segments.len() == 1 => path
            .segments
            .first()
            .map(|segment| segment.name.name.clone()),
        ExprKind::FieldAccess { receiver, field } => {
            let base = spelling(receiver)?;
            Some(match field {
                FieldSelector::Named(name) => format!("{base}.{}", name.name),
                FieldSelector::Index(index) => format!("{base}.{index}"),
            })
        }
        _ => None,
    }
}

impl Visitor for Finder<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let StmtKind::Let {
            pattern,
            init: Some(init),
            ..
        } = &stmt.kind
            && self.is_non_negative(init)
        {
            self.mark(pattern);
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::For { pattern, iter, .. } => self.mark_loop(pattern, iter),
            ExprKind::Binary {
                op: op @ (BinaryOp::Eq | BinaryOp::Ne),
                lhs,
                rhs,
            } => self.check(expr, *op, lhs, rhs),
            _ => {}
        }
        walk_expr(self, expr);
    }
}
