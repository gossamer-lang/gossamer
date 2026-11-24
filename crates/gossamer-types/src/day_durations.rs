//! A whole number of days, as a `Duration`, added to or taken from a
//! `time::Time`.
//!
//! A `Duration` is elapsed time: `t + Duration::from_secs(86_400)` is 24
//! hours later, which across a daylight-saving change is 23:00 or 01:00 on
//! the wall clock rather than the same time the next day. `t.add_date(0, 0,
//! 1)` moves the calendar date and keeps the wall-clock time.

#![forbid(unsafe_code)]

use gossamer_ast::visitor::{Visitor, walk_expr};
use gossamer_ast::{AssignOp, BinaryOp, Expr, ExprKind, Literal, SourceFile};
use gossamer_lex::Span;

use crate::context::TyCtxt;
use crate::table::TypeTable;
use crate::ty::TyKind;

/// The lint these findings belong to.
pub const DAY_DURATION_LINT: &str = "day_duration";

/// The `Duration` constructors and how many of their unit make a day.
const UNITS_PER_DAY: &[(&str, i128)] = &[
    ("from_secs", 86_400),
    ("from_millis", 86_400_000),
    ("from_micros", 86_400_000_000),
    ("from_nanos", 86_400_000_000_000),
];

/// One `Time` moved by a whole number of days of elapsed time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayDuration {
    /// The `+`, `-`, `+=`, or `-=` expression.
    pub span: Span,
    /// The days the `Duration` spans; negative when they are taken away.
    pub days: i128,
}

/// Every `+` / `-` in `sf` of a `time::Time` and a `Duration` built from a
/// constant whole number of days.
#[must_use]
pub fn find_day_durations(sf: &SourceFile, table: &TypeTable, tcx: &TyCtxt) -> Vec<DayDuration> {
    let mut finder = Finder {
        table,
        tcx,
        out: Vec::new(),
    };
    finder.visit_source_file(sf);
    finder.out
}

struct Finder<'a> {
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    out: Vec<DayDuration>,
}

impl Finder<'_> {
    fn is_time(&self, expr: &Expr) -> bool {
        self.table.get(expr.id).is_some_and(|ty| {
            matches!(self.tcx.kind(ty), Some(TyKind::Adt { def, .. })
                if matches!(self.tcx.def_name(*def), Some("__gos_time_Time" | "time::Time")))
        })
    }

    /// The whole days a `Duration::from_*(constant)` spans.
    fn days(&self, expr: &Expr) -> Option<i128> {
        if !self
            .table
            .get(expr.id)
            .is_some_and(|ty| matches!(self.tcx.kind(ty), Some(TyKind::Duration)))
        {
            return None;
        }
        let ExprKind::Call { callee, args } = &expr.kind else {
            return None;
        };
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let [.., owner, ctor] = path.segments.as_slice() else {
            return None;
        };
        if owner.name.name != "Duration" {
            return None;
        }
        let per_day = UNITS_PER_DAY
            .iter()
            .find(|(name, _)| ctor.name.name == *name)
            .map(|(_, per_day)| *per_day)?;
        let [amount] = args.as_slice() else {
            return None;
        };
        let amount = constant(amount)?;
        (amount != 0 && amount % per_day == 0).then(|| amount / per_day)
    }

    fn note(&mut self, span: Span, time: &Expr, duration: &Expr, sign: i128) {
        if self.is_time(time)
            && let Some(days) = self.days(duration)
        {
            self.out.push(DayDuration {
                span,
                days: days * sign,
            });
        }
    }
}

/// The value of an integer literal, or a product of them (`24 * 60 * 60`).
fn constant(expr: &Expr) -> Option<i128> {
    match &expr.kind {
        ExprKind::Literal(Literal::Int(text)) => crate::lossy_casts::int_literal_value(text),
        ExprKind::Binary {
            op: BinaryOp::Mul,
            lhs,
            rhs,
        } => constant(lhs)?.checked_mul(constant(rhs)?),
        _ => None,
    }
}

impl Visitor for Finder<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Binary {
                op: op @ (BinaryOp::Add | BinaryOp::Sub),
                lhs,
                rhs,
            } => {
                let sign = if matches!(op, BinaryOp::Sub) { -1 } else { 1 };
                self.note(expr.span, lhs, rhs, sign);
            }
            ExprKind::Assign {
                op: op @ (AssignOp::AddAssign | AssignOp::SubAssign),
                place,
                value,
            } => {
                let sign = if matches!(op, AssignOp::SubAssign) {
                    -1
                } else {
                    1
                };
                self.note(expr.span, place, value, sign);
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}
