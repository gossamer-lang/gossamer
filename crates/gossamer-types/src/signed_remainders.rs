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
//! A length or count a builtin answers is at least zero, and so is a callback
//! parameter fed the elements of such a range. A `let mut` counter that
//! starts at zero or above and is only ever assigned, or grown by, values of
//! zero or more stays there too: overflow panics rather than wrapping. A
//! guard such as `i > 0` on an immutable binding shows it non-negative in
//! the right operand of its `&&` and in the body of its `if` or `while`.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use gossamer_ast::visitor::{Visitor, walk_expr, walk_pattern, walk_stmt};
use gossamer_ast::{
    AssignOp, BinaryOp, Expr, ExprKind, FieldSelector, Mutability, NodeId, Pattern, PatternKind,
    SourceFile, Stmt, StmtKind, UnaryOp,
};
use gossamer_lex::Span;
use gossamer_resolve::{Resolution, Resolutions};

use crate::context::TyCtxt;
use crate::lossy_casts::{int_value_range, literal_int};
use crate::table::TypeTable;
use crate::ty::{Ty, TyKind};

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
    /// The comparison respelled to hold for every sign, when both operands
    /// have a spelling of their own.
    pub fix: Option<String>,
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
    // Each survey proves the counters whose writes all hold given the ones
    // proved before it, so the set only grows and stops once a survey adds
    // nothing.
    let mutable = mutable_bindings(sf);
    let mut counters = HashSet::new();
    loop {
        let mut survey = Finder::new(resolutions, table, tcx, Pass::Survey, &mutable);
        survey.non_negative.extend(counters.iter().copied());
        survey.visit_source_file(sf);
        let proved = survey.counters();
        if proved.len() == counters.len() {
            break;
        }
        counters = proved;
    }
    let mut finder = Finder::new(resolutions, table, tcx, Pass::Report, &mutable);
    finder.non_negative.extend(counters);
    finder.visit_source_file(sf);
    finder.out
}

/// Every binding a pattern declares `mut`.
fn mutable_bindings(sf: &SourceFile) -> HashSet<NodeId> {
    struct Mutable(HashSet<NodeId>);
    impl Visitor for Mutable {
        fn visit_pattern(&mut self, pattern: &Pattern) {
            if let PatternKind::Ident {
                mutability: Mutability::Mutable,
                ..
            } = &pattern.kind
            {
                self.0.insert(pattern.id);
            }
            walk_pattern(self, pattern);
        }
    }
    let mut mutable = Mutable(HashSet::new());
    mutable.visit_source_file(sf);
    mutable.0
}

/// The walk runs as surveys that learn which `let mut` counters stay at zero
/// or above, then the report, which knows them from the start.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Survey,
    Report,
}

/// One write to a counter candidate, and whether it keeps the candidate at
/// zero or more given the counters already proved.
struct CounterWrite {
    target: NodeId,
    holds: bool,
}

/// Builtin methods whose integer answer is never negative on any receiver.
const NON_NEGATIVE_INT_METHODS: &[&str] = &[
    "count_ones",
    "count_zeros",
    "leading_zeros",
    "trailing_zeros",
];

/// Builtin methods that answer a length or count.
const LENGTH_METHODS: &[&str] = &["len", "byte_len", "count", "capacity"];

/// Adapters whose elements are some of their receiver's own elements.
const ELEMENT_PRESERVING: &[&str] = &[
    "iter",
    "into_iter",
    "rev",
    "step_by",
    "skip",
    "take",
    "filter",
    "take_while",
    "skip_while",
    "inspect",
];

/// Methods whose callback's one parameter is an element of the receiver.
const ELEMENT_CALLBACKS: &[&str] = &[
    "filter",
    "map",
    "any",
    "all",
    "find",
    "position",
    "for_each",
    "take_while",
    "skip_while",
    "filter_map",
    "flat_map",
    "inspect",
    "max_by_key",
    "min_by_key",
];

struct Finder<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    pass: Pass,
    /// Bindings declared `mut`, which a guard says nothing about past itself.
    mutable: &'a HashSet<NodeId>,
    /// Bindings whose every value is at least zero.
    non_negative: HashSet<NodeId>,
    /// `let mut` bindings that may count up from zero.
    candidates: HashSet<NodeId>,
    /// Every write to a candidate, its initializer included.
    writes: Vec<CounterWrite>,
    /// Candidates something may write through a `&mut`.
    escaped: HashSet<NodeId>,
    out: Vec<SignedRemainder>,
}

impl<'a> Finder<'a> {
    fn new(
        resolutions: &'a Resolutions,
        table: &'a TypeTable,
        tcx: &'a TyCtxt,
        pass: Pass,
        mutable: &'a HashSet<NodeId>,
    ) -> Self {
        Self {
            resolutions,
            table,
            tcx,
            pass,
            mutable,
            non_negative: HashSet::new(),
            candidates: HashSet::new(),
            writes: Vec::new(),
            escaped: HashSet::new(),
            out: Vec::new(),
        }
    }

    /// The candidates nothing writes through a `&mut` whose every write
    /// keeps them at zero or more.
    fn counters(&self) -> HashSet<NodeId> {
        let failed: HashSet<NodeId> = self
            .writes
            .iter()
            .filter(|write| !write.holds)
            .map(|write| write.target)
            .collect();
        self.candidates
            .iter()
            .copied()
            .filter(|id| !self.escaped.contains(id) && !failed.contains(id))
            .collect()
    }

    /// The immutable bindings `condition` shows to be at least zero where it
    /// holds: `x > c` for `c >= -1`, `x >= c` for `c >= 0`, or a bound that
    /// is itself non-negative, joined by `&&`.
    fn guarded(&self, condition: &Expr, out: &mut Vec<NodeId>) {
        let ExprKind::Binary { op, lhs, rhs } = &condition.kind else {
            return;
        };
        let (value, bound, inclusive) = match op {
            BinaryOp::And => {
                self.guarded(lhs, out);
                self.guarded(rhs, out);
                return;
            }
            BinaryOp::Gt => (lhs, rhs, false),
            BinaryOp::Ge => (lhs, rhs, true),
            BinaryOp::Lt => (rhs, lhs, false),
            BinaryOp::Le => (rhs, lhs, true),
            _ => return,
        };
        let least = if inclusive { 0 } else { -1 };
        let holds = literal_int(bound).is_some_and(|c| c >= least) || self.is_non_negative(bound);
        if holds
            && let Some(binding) = self.local(value)
            && !self.mutable.contains(&binding)
        {
            out.push(binding);
        }
    }

    /// Adds what `condition` guards to the non-negative bindings, answering
    /// the ones it added so they can be forgotten where it stops holding.
    fn assume(&mut self, condition: &Expr) -> Vec<NodeId> {
        let mut guarded = Vec::new();
        self.guarded(condition, &mut guarded);
        guarded.retain(|binding| self.non_negative.insert(*binding));
        guarded
    }

    fn forget(&mut self, assumed: Vec<NodeId>) {
        for binding in assumed {
            self.non_negative.remove(&binding);
        }
    }

    /// Walks a construct whose condition guards part of it, answering
    /// whether `expr` was one.
    fn visit_guarded(&mut self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Binary {
                op: BinaryOp::And,
                lhs,
                rhs,
            } => {
                self.visit_expr(lhs);
                let assumed = self.assume(lhs);
                self.visit_expr(rhs);
                self.forget(assumed);
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr(condition);
                let assumed = self.assume(condition);
                self.visit_expr(then_branch);
                self.forget(assumed);
                if let Some(else_branch) = else_branch {
                    self.visit_expr(else_branch);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                self.visit_expr(condition);
                let assumed = self.assume(condition);
                self.visit_expr(body);
                self.forget(assumed);
            }
            _ => return false,
        }
        true
    }

    /// The local an expression names, when it is a plain local path.
    fn local(&self, expr: &Expr) -> Option<NodeId> {
        match (&expr.kind, self.resolutions.get(expr.id)) {
            (ExprKind::Path(_), Some(Resolution::Local(binding))) => Some(binding),
            _ => None,
        }
    }

    fn type_kind(&self, expr: &Expr) -> Option<&TyKind> {
        let mut ty: Ty = self.table.get(expr.id)?;
        loop {
            match self.tcx.kind(ty)? {
                TyKind::Ref { inner, .. } => ty = *inner,
                kind => return Some(kind),
            }
        }
    }

    /// Whether `receiver.name()` is a builtin length or count, never negative.
    fn is_builtin_length(&self, receiver: &Expr, name: &str) -> bool {
        if NON_NEGATIVE_INT_METHODS.contains(&name) {
            return matches!(self.type_kind(receiver), Some(TyKind::Int(_)));
        }
        LENGTH_METHODS.contains(&name)
            && matches!(
                self.type_kind(receiver),
                Some(
                    TyKind::String
                        | TyKind::Vec(_)
                        | TyKind::Array { .. }
                        | TyKind::Slice(_)
                        | TyKind::HashMap { .. }
                        | TyKind::Iterator(_)
                        | TyKind::Range(_)
                )
            )
    }

    /// Whether every element `expr` yields is at least zero: a range that
    /// starts at zero or above, through adapters that pass elements on.
    fn elements_non_negative(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Range {
                start: Some(start), ..
            } => self.is_non_negative(start),
            ExprKind::MethodCall { receiver, name, .. }
                if ELEMENT_PRESERVING.contains(&name.name.as_str()) =>
            {
                self.elements_non_negative(receiver)
            }
            _ => false,
        }
    }

    fn is_non_negative(&self, expr: &Expr) -> bool {
        if let Some(binding) = self.local(expr)
            && self.non_negative.contains(&binding)
        {
            return true;
        }
        if let ExprKind::MethodCall { receiver, name, .. } = &expr.kind
            && self.is_builtin_length(receiver, &name.name)
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
                "rev" | "step_by" | "skip" | "take" | "filter" | "take_while" | "skip_while"
                | "inspect" | "iter" | "into_iter" => self.mark_loop(pattern, receiver),
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
        let spelled_value = spelling(dividend);
        let spelled_k = divisor_value
            .map(|k| k.to_string())
            .or_else(|| spelling(divisor));
        let value = spelled_value.clone().unwrap_or_else(|| "x".to_string());
        let k = spelled_k.clone().unwrap_or_else(|| "k".to_string());
        let (verb, never) = if op == BinaryOp::Eq {
            ("==", "false")
        } else {
            ("!=", "true")
        };
        let title = format!(
            "`{value} % {k} {verb} {expected}` is {never} for every negative `{value}`: `%` keeps the sign of the value it divides, so the remainder of a negative `{value}` is never positive"
        );
        let oddness = divisor_value == Some(2) && expected == 1;
        let odd = if op == BinaryOp::Eq { "!=" } else { "==" };
        let help = if oddness {
            format!(
                "`{value} % 2 {odd} 0` tests oddness for every sign; `{value}.rem_euclid(2) {verb} 1` answers the remainder in `0..2`"
            )
        } else {
            format!(
                "`{value}.rem_euclid({k}) {verb} {expected}` compares the remainder in `0..{k}`, the same for every sign"
            )
        };
        let fix = spelled_value.zip(spelled_k).map(|(value, k)| {
            if oddness {
                format!("{value} % 2 {odd} 0")
            } else {
                format!("{value}.rem_euclid({k}) {verb} {expected}")
            }
        });
        self.out.push(SignedRemainder {
            span: compare.span,
            title,
            help,
            fix,
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
        {
            if self.is_non_negative(init) {
                self.mark(pattern);
            }
            if self.pass == Pass::Survey
                && let PatternKind::Ident {
                    mutability: Mutability::Mutable,
                    subpattern: None,
                    ..
                } = &pattern.kind
            {
                self.candidates.insert(pattern.id);
                self.writes.push(CounterWrite {
                    target: pattern.id,
                    holds: self.is_non_negative(init),
                });
            }
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if self.visit_guarded(expr) {
            return;
        }
        match &expr.kind {
            ExprKind::For { pattern, iter, .. } => self.mark_loop(pattern, iter),
            ExprKind::Binary {
                op: op @ (BinaryOp::Eq | BinaryOp::Ne),
                lhs,
                rhs,
            } if self.pass == Pass::Report => self.check(expr, *op, lhs, rhs),
            ExprKind::Assign { op, place, value } if self.pass == Pass::Survey => {
                if let Some(target) = self.local(place)
                    && self.candidates.contains(&target)
                {
                    let keeps_sign = matches!(
                        op,
                        AssignOp::Assign
                            | AssignOp::AddAssign
                            | AssignOp::MulAssign
                            | AssignOp::DivAssign
                            | AssignOp::RemAssign
                            | AssignOp::BitAndAssign
                            | AssignOp::BitOrAssign
                    );
                    self.writes.push(CounterWrite {
                        target,
                        holds: keeps_sign && self.is_non_negative(value),
                    });
                }
            }
            ExprKind::Unary {
                op: UnaryOp::RefMut,
                operand,
            } if self.pass == Pass::Survey => {
                if let Some(target) = self.local(operand) {
                    self.escaped.insert(target);
                }
            }
            ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } if ELEMENT_CALLBACKS.contains(&name.name.as_str())
                && self.elements_non_negative(receiver) =>
            {
                for arg in args {
                    if let ExprKind::Closure { params, .. } = &arg.kind
                        && let [param] = params.as_slice()
                    {
                        self.mark(&param.pattern);
                    }
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}
