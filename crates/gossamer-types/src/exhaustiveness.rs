//! Match exhaustiveness and reachability.
//!
//! Implements Maranget's usefulness algorithm over a pattern matrix,
//! specialising each column by the constructors its type admits. For every
//! `match` expression in a source file it reports missing patterns that
//! would make the match exhaustive, each rendered as a concrete witness
//! value, and flags arms dominated by earlier arms as unreachable.
//!
//! A column's type decides how far the search goes. Booleans, tuples,
//! fixed-length arrays, user enums, `Option`, and `Result` enumerate their
//! constructors and are decomposed recursively. An integer or `char` column
//! is the interval of its type's values, which literal and range patterns
//! split into segments, so a set of ranges that tiles the type covers it
//! without a catch-all. Floats and strings have no finite constructor list,
//! so covering them takes a catch-all arm. Every other type contributes no
//! witness of its own: the search cannot enumerate it, and reporting a gap
//! it cannot see would be a guess. Slice patterns whose fixed elements sit
//! around a rest constrain a span of lengths rather than one constructor,
//! so they stay opaque to the usefulness lattice.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;

use gossamer_ast::{
    Expr, ExprKind, Item, ItemKind, Literal, MatchArm, Pattern, PatternKind, SourceFile, StmtKind,
};
use gossamer_lex::Span;
use gossamer_resolve::{DefId, Resolutions};
use thiserror::Error;

use crate::context::TyCtxt;
use crate::table::TypeTable;
use crate::ty::{Ty, TyKind};

/// Walks every `match` in `source` and reports exhaustiveness and
/// reachability diagnostics.
#[must_use]
pub fn check_exhaustiveness(
    source: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<ExhaustivenessDiagnostic> {
    let enums = collect_enums(source, resolutions);
    let mut checker = Checker {
        tcx,
        table,
        enums: &enums,
        diagnostics: Vec::new(),
    };
    checker.walk_items(&source.items);
    checker.diagnostics
}

struct Checker<'a> {
    tcx: &'a TyCtxt,
    table: &'a TypeTable,
    enums: &'a HashMap<DefId, Vec<String>>,
    diagnostics: Vec<ExhaustivenessDiagnostic>,
}

impl Checker<'_> {
    fn walk_items(&mut self, items: &[Item]) {
        for item in items {
            match &item.kind {
                ItemKind::Fn(decl) => {
                    if let Some(body) = &decl.body {
                        self.walk_expr(body);
                    }
                }
                ItemKind::Impl(decl) => {
                    for impl_item in &decl.items {
                        if let gossamer_ast::ImplItem::Fn(fn_decl) = impl_item {
                            if let Some(body) = &fn_decl.body {
                                self.walk_expr(body);
                            }
                        }
                    }
                }
                ItemKind::Trait(decl) => {
                    for trait_item in &decl.items {
                        if let gossamer_ast::TraitItem::Fn(fn_decl) = trait_item {
                            if let Some(body) = &fn_decl.body {
                                self.walk_expr(body);
                            }
                        }
                    }
                }
                ItemKind::Const(decl) => self.walk_expr(&decl.value),
                ItemKind::Static(decl) => self.walk_expr(&decl.value),
                ItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        self.walk_items(inner);
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Match { scrutinee, arms } => self.walk_match(scrutinee, arms, expr.span),
            ExprKind::Block(block) | ExprKind::Unsafe(block) => self.walk_block(block),
            ExprKind::Call { callee, args } => {
                self.walk_expr(callee);
                self.walk_exprs(args);
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.walk_expr(receiver);
                self.walk_exprs(args);
            }
            ExprKind::Binary { lhs, rhs, .. }
            | ExprKind::Assign {
                place: lhs,
                value: rhs,
                ..
            }
            | ExprKind::Index {
                base: lhs,
                index: rhs,
            } => {
                self.walk_expr(lhs);
                self.walk_expr(rhs);
            }
            ExprKind::Unary { operand, .. }
            | ExprKind::FieldAccess {
                receiver: operand, ..
            }
            | ExprKind::Try(operand) => self.walk_expr(operand),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.walk_if(condition, then_branch, else_branch.as_deref()),
            ExprKind::Loop { body, .. } | ExprKind::Closure { body, .. } => self.walk_expr(body),
            ExprKind::While {
                condition, body, ..
            } => {
                self.walk_expr(condition);
                self.walk_expr(body);
            }
            ExprKind::For { iter, body, .. } => {
                self.walk_expr(iter);
                self.walk_expr(body);
            }
            ExprKind::Return(value) | ExprKind::Break { value, .. } => {
                self.walk_optional(value.as_deref());
            }
            ExprKind::Tuple(elems) | ExprKind::MapLiteral(elems) | ExprKind::SetLiteral(elems) => {
                self.walk_exprs(elems);
            }
            ExprKind::Struct { fields, base, .. } => self.walk_struct(fields, base.as_deref()),
            ExprKind::Array(arr) | ExprKind::FixedArray(arr) => self.walk_array(arr),
            ExprKind::Range { start, end, .. } => {
                self.walk_optional(start.as_deref());
                self.walk_optional(end.as_deref());
            }
            ExprKind::Cast { value, .. } => self.walk_expr(value),
            _ => {}
        }
    }

    fn walk_match(&mut self, scrutinee: &Expr, arms: &[MatchArm], span: Span) {
        self.walk_expr(scrutinee);
        for arm in arms {
            if let Some(guard) = &arm.guard {
                self.walk_expr(guard);
            }
            self.walk_expr(&arm.body);
        }
        self.check_match(scrutinee, arms, span);
    }

    fn walk_block(&mut self, block: &gossamer_ast::Block) {
        for stmt in &block.stmts {
            self.walk_stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.walk_expr(tail);
        }
    }

    fn walk_if(&mut self, condition: &Expr, then_branch: &Expr, else_branch: Option<&Expr>) {
        self.walk_expr(condition);
        self.walk_expr(then_branch);
        self.walk_optional(else_branch);
    }

    fn walk_struct(&mut self, fields: &[gossamer_ast::StructExprField], base: Option<&Expr>) {
        for field in fields {
            if let Some(value) = &field.value {
                self.walk_expr(value);
            }
        }
        self.walk_optional(base);
    }

    fn walk_exprs(&mut self, exprs: &[Expr]) {
        for expr in exprs {
            self.walk_expr(expr);
        }
    }

    fn walk_optional(&mut self, expr: Option<&Expr>) {
        if let Some(expr) = expr {
            self.walk_expr(expr);
        }
    }

    fn walk_array(&mut self, arr: &gossamer_ast::ArrayExpr) {
        match arr {
            gossamer_ast::ArrayExpr::List(elems) => {
                for elem in elems {
                    self.walk_expr(elem);
                }
            }
            gossamer_ast::ArrayExpr::Repeat { value, count } => {
                self.walk_expr(value);
                self.walk_expr(count);
            }
        }
    }

    fn walk_stmt(&mut self, stmt: &gossamer_ast::Stmt) {
        match &stmt.kind {
            StmtKind::Let { init, .. } => {
                if let Some(init) = init {
                    self.walk_expr(init);
                }
            }
            StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => {
                self.walk_expr(expr);
            }
            StmtKind::Item(item) => self.walk_items(std::slice::from_ref(item)),
        }
    }

    fn check_match(&mut self, scrutinee: &Expr, arms: &[MatchArm], span: Span) {
        let scrutinee_ty = self.table.get(scrutinee.id);
        let rows: Vec<Row> = arms
            .iter()
            .map(|arm| Row {
                pat: lower_pattern(&arm.pattern),
                has_guard: arm.guard.is_some(),
                span: arm.pattern.span,
            })
            .collect();
        self.report_redundancy(scrutinee_ty, &rows);
        self.report_non_exhaustive(scrutinee_ty, &rows, span);
    }

    /// Reports each arm whose every value an earlier unguarded arm already
    /// matches. Over an integer or `char` scrutinee the earlier arms' ranges
    /// cover it together, and an arm outside the type's range matches no
    /// value at all.
    fn report_redundancy(&mut self, scrutinee_ty: Option<Ty>, rows: &[Row]) {
        let intervals = match self.domain_of(scrutinee_ty) {
            Domain::Ints { intervals, .. } => Some(intervals),
            _ => None,
        };
        for (i, row) in rows.iter().enumerate() {
            let earlier: Vec<&Pat> = rows[..i]
                .iter()
                .filter(|earlier| !earlier.has_guard)
                .map(|earlier| &earlier.pat)
                .collect();
            if covered(&earlier, &row.pat, intervals.as_deref()) {
                self.diagnostics.push(ExhaustivenessDiagnostic::new(
                    ExhaustivenessError::UnreachableArm,
                    row.span,
                ));
            }
        }
    }

    fn report_non_exhaustive(&mut self, scrutinee_ty: Option<Ty>, rows: &[Row], span: Span) {
        let relevant: Vec<&Pat> = rows
            .iter()
            .filter(|row| !row.has_guard)
            .map(|row| &row.pat)
            .collect();
        if relevant.iter().any(|pat| is_catch_all(pat)) {
            return;
        }
        let missing = self.compute_missing(scrutinee_ty, &relevant);
        if missing.is_empty() {
            return;
        }
        self.diagnostics.push(ExhaustivenessDiagnostic::new(
            ExhaustivenessError::NonExhaustive { missing },
            span,
        ));
    }

    /// Missing witnesses for a single-column matrix over the scrutinee.
    fn compute_missing(&self, scrutinee_ty: Option<Ty>, patterns: &[&Pat]) -> Vec<String> {
        let rows: Vec<Vec<Pat>> = patterns.iter().map(|pat| vec![(*pat).clone()]).collect();
        self.missing_rows(rows, &[scrutinee_ty], 0)
            .into_iter()
            .filter_map(|witness| witness.into_iter().next())
            .take(MAX_WITNESSES)
            .collect()
    }

    /// Maranget usefulness over a pattern matrix: returns one rendered
    /// witness per uncovered value shape, or an empty list when the rows
    /// cover every value the column types can take.
    ///
    /// A column whose type carries no usable information contributes no
    /// witness of its own, so an unknown scrutinee is never reported as
    /// non-exhaustive on the strength of its own column.
    fn missing_rows(
        &self,
        rows: Vec<Vec<Pat>>,
        tys: &[Option<Ty>],
        depth: usize,
    ) -> Vec<Vec<String>> {
        let Some((head_ty, rest_tys)) = tys.split_first() else {
            return if rows.is_empty() {
                vec![Vec::new()]
            } else {
                Vec::new()
            };
        };
        if depth >= MAX_DEPTH {
            return Vec::new();
        }
        let rows = expand_or_heads(rows);
        let mut out: Vec<Vec<String>> = Vec::new();
        let domain = self.domain_of(*head_ty);
        if let Domain::Ints { intervals, kind } = &domain {
            return self.missing_int_rows(&rows, intervals, *kind, rest_tys, depth);
        }
        let signature = match &domain {
            Domain::Finite(ctors) => Some(ctors.clone()),
            Domain::Ints { .. } | Domain::Infinite | Domain::Unknown => None,
        };
        // A signature constructor counts as tested once some row head other
        // than a wildcard can match it, whatever shape that head takes: a
        // name-only variant pattern and a rest-carrying slice pattern test
        // their constructor just as a fully written one does.
        let tested = |ctor: &Ctor| {
            rows.iter().any(|row| {
                row.first().is_some_and(|head| {
                    !matches!(head, Pat::Wild) && head_fields(head, ctor).is_some()
                })
            })
        };
        let used = head_ctors(&rows);
        // Every constructor the rows already test is descended into, so a
        // gap nested under a present constructor is still reported.
        let explore: Vec<Ctor> = match &signature {
            Some(ctors) => ctors.iter().filter(|c| tested(c)).cloned().collect(),
            None => used.clone(),
        };
        for ctor in &explore {
            let field_tys = self.field_tys(*head_ty, ctor);
            let specialized = specialize(&rows, ctor);
            let mut sub_tys = field_tys;
            sub_tys.extend_from_slice(rest_tys);
            for witness in self.missing_rows(specialized, &sub_tys, depth + 1) {
                out.push(apply_ctor(ctor, witness));
            }
        }
        let uncovered: Vec<Ctor> = match &signature {
            Some(ctors) => ctors.iter().filter(|c| !tested(c)).cloned().collect(),
            None => Vec::new(),
        };
        let head_is_open = match domain {
            Domain::Finite(_) => !uncovered.is_empty(),
            Domain::Ints { .. } | Domain::Infinite => true,
            // An unknown head only leaves a gap when no row constrains it.
            Domain::Unknown => used.is_empty(),
        };
        if head_is_open {
            let default_rows: Vec<Vec<Pat>> = rows
                .iter()
                .filter(|row| matches!(row.first(), Some(Pat::Wild)))
                .map(|row| row[1..].to_vec())
                .collect();
            for witness in self.missing_rows(default_rows, rest_tys, depth + 1) {
                if uncovered.is_empty() {
                    let mut full = vec!["_".to_string()];
                    full.extend(witness);
                    out.push(full);
                } else {
                    for ctor in &uncovered {
                        let mut full = vec![render_ctor(ctor)];
                        full.extend(witness.iter().cloned());
                        out.push(full);
                    }
                }
            }
        }
        out.truncate(MAX_WITNESSES);
        out
    }

    /// [`Self::missing_rows`] over an integer or `char` head column: the
    /// type's intervals split at every bound the head patterns name, so each
    /// segment lies wholly inside or wholly outside each pattern, and each
    /// segment is a constructor. A run of adjacent segments no row covers is
    /// reported as one range.
    fn missing_int_rows(
        &self,
        rows: &[Vec<Pat>],
        intervals: &[Interval],
        kind: IntKind,
        rest_tys: &[Option<Ty>],
        depth: usize,
    ) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = Vec::new();
        let mut gap: Option<Interval> = None;
        let flush = |gap: &mut Option<Interval>, out: &mut Vec<Vec<String>>| {
            if let Some((lo, hi)) = gap.take() {
                let mut witness = vec![render_ctor(&Ctor::IntRange { lo, hi, kind })];
                witness.extend(rest_tys.iter().map(|_| "_".to_string()));
                out.push(witness);
            }
        };
        for (lo, hi) in int_segments(rows, intervals) {
            let ctor = Ctor::IntRange { lo, hi, kind };
            let specialized = specialize(rows, &ctor);
            if specialized.is_empty() {
                gap = match gap {
                    Some((start, end)) if end.checked_add(1) == Some(lo) => Some((start, hi)),
                    other => {
                        let mut pending = other;
                        flush(&mut pending, &mut out);
                        Some((lo, hi))
                    }
                };
                continue;
            }
            flush(&mut gap, &mut out);
            for witness in self.missing_rows(specialized, rest_tys, depth + 1) {
                out.push(apply_ctor(&ctor, witness));
            }
        }
        flush(&mut gap, &mut out);
        out.truncate(MAX_WITNESSES);
        out
    }

    /// Value domain of a column's type.
    fn domain_of(&self, ty: Option<Ty>) -> Domain {
        let Some(kind) = ty.and_then(|ty| self.tcx.kind(ty)) else {
            return Domain::Unknown;
        };
        match kind {
            TyKind::Bool => Domain::Finite(vec![Ctor::Bool(true), Ctor::Bool(false)]),
            TyKind::Unit => Domain::Finite(vec![Ctor::Tuple(0)]),
            TyKind::Tuple(items) => Domain::Finite(vec![Ctor::Tuple(items.len())]),
            TyKind::Array {
                len: crate::ArrayLen::Concrete(n),
                ..
            } => Domain::Finite(vec![Ctor::List(*n)]),
            TyKind::Adt { def, .. } => self.adt_domain(*def),
            // A domain no finite list of patterns can exhaust: only a
            // catch-all arm covers it.
            TyKind::Int(int_ty) => {
                let (lo, hi) = int_type_bounds(*int_ty);
                Domain::Ints {
                    intervals: vec![(lo, hi)],
                    kind: IntKind::Int(*int_ty),
                }
            }
            // A `char` is a Unicode scalar value, which excludes the
            // surrogate code points.
            TyKind::Char => Domain::Ints {
                intervals: vec![(0, 0xD7FF), (0xE000, 0x10_FFFF)],
                kind: IntKind::Char,
            },
            TyKind::Float(_) | TyKind::String => Domain::Infinite,
            _ => Domain::Unknown,
        }
    }

    /// Constructor signature of a named ADT: a user enum's declared
    /// variants, or the built-in `Option` / `Result` sentinels. Structs
    /// carry no discriminant to enumerate.
    fn adt_domain(&self, def: DefId) -> Domain {
        if let Some(variants) = self.enums.get(&def) {
            let arities = self.tcx.enum_variant_tys(def);
            return Domain::Finite(
                variants
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        let arity = arities.and_then(|all| all.get(i)).map_or(0, Vec::len);
                        Ctor::Variant {
                            name: name.clone(),
                            arity,
                        }
                    })
                    .collect(),
            );
        }
        match self.tcx.def_name(def) {
            Some("Option") => Domain::Finite(vec![
                Ctor::Variant {
                    name: "Some".to_string(),
                    arity: 1,
                },
                Ctor::Variant {
                    name: "None".to_string(),
                    arity: 0,
                },
            ]),
            Some("Result") => Domain::Finite(vec![
                Ctor::Variant {
                    name: "Ok".to_string(),
                    arity: 1,
                },
                Ctor::Variant {
                    name: "Err".to_string(),
                    arity: 1,
                },
            ]),
            _ => Domain::Unknown,
        }
    }

    /// Column types a constructor's fields occupy when the value it
    /// destructures has type `ty`. `None` entries stand for a field whose
    /// type is not recoverable here.
    fn field_tys(&self, ty: Option<Ty>, ctor: &Ctor) -> Vec<Option<Ty>> {
        let unknown = vec![None; ctor.arity()];
        let Some(kind) = ty.and_then(|ty| self.tcx.kind(ty)) else {
            return unknown;
        };
        match (kind, ctor) {
            (TyKind::Tuple(items), Ctor::Tuple(n)) if items.len() == *n => {
                items.iter().map(|item| Some(*item)).collect()
            }
            (TyKind::Array { elem, .. }, Ctor::List(n)) => vec![Some(*elem); *n],
            (TyKind::Adt { def, substs }, Ctor::Variant { name, arity }) => {
                let args = substs.types();
                match (self.tcx.def_name(*def), name.as_str()) {
                    (Some("Option"), "Some") | (Some("Result"), "Ok") => {
                        vec![args.first().copied()]
                    }
                    (Some("Result"), "Err") => vec![args.get(1).copied()],
                    _ => {
                        // A generic enum's stored field types mention
                        // un-substituted parameters, so they name no usable
                        // column type here.
                        if !args.is_empty() {
                            return unknown;
                        }
                        let Some(variants) = self.enums.get(def) else {
                            return unknown;
                        };
                        let Some(position) = variants.iter().position(|v| v == name) else {
                            return unknown;
                        };
                        match self
                            .tcx
                            .enum_variant_tys(*def)
                            .and_then(|all| all.get(position))
                        {
                            Some(tys) if tys.len() == *arity => {
                                tys.iter().map(|ty| Some(*ty)).collect()
                            }
                            _ => unknown,
                        }
                    }
                }
            }
            _ => unknown,
        }
    }
}

/// Largest number of witnesses reported for one match.
const MAX_WITNESSES: usize = 3;

/// Deepest constructor nesting the usefulness search descends into. A
/// deeper gap is left unreported rather than driving the search into an
/// exponential expansion.
const MAX_DEPTH: usize = 8;

/// An inclusive interval of an integer or `char` column's values.
type Interval = (i128, i128);

/// How an integer-valued column's values are spelled in a witness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntKind {
    /// An integer of this type.
    Int(crate::ty::IntTy),
    /// A `char`, by its scalar value.
    Char,
    /// An integer of a type the column does not name.
    Plain,
}

/// Value domain of a scrutinee column.
enum Domain {
    /// Every constructor the type admits.
    Finite(Vec<Ctor>),
    /// The intervals of an integer or `char` type's values.
    Ints {
        /// Disjoint, ascending intervals of the values.
        intervals: Vec<Interval>,
        /// How a value is spelled.
        kind: IntKind,
    },
    /// A domain no finite pattern list exhausts, such as the integers.
    Infinite,
    /// No usable type information.
    Unknown,
}

/// One constructor a column's values can take.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ctor {
    /// A boolean value.
    Bool(bool),
    /// A named enum variant with its payload arity.
    Variant {
        /// Variant name as written.
        name: String,
        /// Number of payload fields.
        arity: usize,
    },
    /// A tuple of the given width.
    Tuple(usize),
    /// A fixed-length sequence.
    List(usize),
    /// A literal value in an unbounded domain, keyed by its spelling.
    Literal(String),
    /// A segment of an integer or `char` column's values.
    IntRange {
        /// Least value in the segment.
        lo: i128,
        /// Greatest value in the segment.
        hi: i128,
        /// How the values are spelled.
        kind: IntKind,
    },
}

impl Ctor {
    fn arity(&self) -> usize {
        match self {
            Self::Bool(_) | Self::Literal(_) | Self::IntRange { .. } => 0,
            Self::Variant { arity, .. } => *arity,
            Self::Tuple(n) | Self::List(n) => *n,
        }
    }
}

/// Expands every row whose head is an or-pattern into one row per
/// alternative, so head-constructor analysis sees a flat matrix.
fn expand_or_heads(rows: Vec<Vec<Pat>>) -> Vec<Vec<Pat>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match row.split_first() {
            Some((Pat::Or(alts), tail)) => {
                for alt in alts.clone() {
                    let mut expanded = vec![alt];
                    expanded.extend_from_slice(tail);
                    out.push(expanded);
                }
            }
            _ => out.push(row),
        }
    }
    // A nested or-pattern head becomes a fresh or-pattern head after one
    // expansion, so keep flattening until none remain.
    if out
        .iter()
        .any(|row| matches!(row.first(), Some(Pat::Or(_))))
    {
        return expand_or_heads(out);
    }
    out
}

/// Distinct constructors appearing in the matrix's head column, in first
/// occurrence order. Wildcards and opaque patterns name no constructor.
fn head_ctors(rows: &[Vec<Pat>]) -> Vec<Ctor> {
    let mut out: Vec<Ctor> = Vec::new();
    for row in rows {
        let Some(ctor) = row.first().and_then(pat_ctor) else {
            continue;
        };
        if !out.contains(&ctor) {
            out.push(ctor);
        }
    }
    out
}

/// The constructor a head pattern tests for, or `None` for a wildcard or
/// an opaque pattern that constrains no single constructor.
fn pat_ctor(pat: &Pat) -> Option<Ctor> {
    match pat {
        Pat::Bool(value) => Some(Ctor::Bool(*value)),
        Pat::Variant { name, fields } => Some(Ctor::Variant {
            name: name.clone(),
            arity: fields.len(),
        }),
        Pat::Tuple(fields) => Some(Ctor::Tuple(fields.len())),
        Pat::List(fields) => Some(Ctor::List(fields.len())),
        Pat::Literal(text) => Some(Ctor::Literal(text.clone())),
        Pat::Range(lo, hi) => Some(Ctor::IntRange {
            lo: *lo,
            hi: *hi,
            kind: IntKind::Plain,
        }),
        // A rest-carrying slice spans a range of lengths, so it pins no
        // single constructor of its own.
        Pat::Wild | Pat::Or(_) | Pat::Opaque | Pat::SliceRest { .. } => None,
    }
}

/// Field patterns a head pattern contributes when the value it matches is
/// built with `ctor`, or `None` when the head cannot match that
/// constructor at all.
fn head_fields(head: &Pat, ctor: &Ctor) -> Option<Vec<Pat>> {
    let arity = ctor.arity();
    match head {
        Pat::Wild => Some(vec![Pat::Wild; arity]),
        // A name-only variant pattern (`Shape::Box(..)`, a struct-variant
        // pattern, or a bare path) tests the name and leaves the payload
        // open.
        Pat::Variant { name, fields } => match ctor {
            Ctor::Variant { name: want, .. } if name == want => Some(if fields.len() == arity {
                fields.clone()
            } else {
                vec![Pat::Wild; arity]
            }),
            _ => None,
        },
        Pat::Bool(value) => match ctor {
            Ctor::Bool(want) if value == want => Some(Vec::new()),
            _ => None,
        },
        Pat::Literal(text) => match ctor {
            Ctor::Literal(want) if text == want => Some(Vec::new()),
            _ => None,
        },
        // A segment lies wholly inside or wholly outside each pattern.
        Pat::Range(plo, phi) => match ctor {
            Ctor::IntRange { lo, hi, .. } if plo <= lo && hi <= phi => Some(Vec::new()),
            _ => None,
        },
        Pat::Tuple(fields) => match ctor {
            Ctor::Tuple(n) if fields.len() == *n => Some(fields.clone()),
            _ => None,
        },
        Pat::List(fields) => match ctor {
            Ctor::List(n) if fields.len() == *n => Some(fields.clone()),
            _ => None,
        },
        // `[a, ..rest]` / `(a, .., d)` matches every width the fixed
        // elements fit into; the rest spans the middle.
        Pat::SliceRest { prefix, suffix } => match ctor {
            Ctor::List(n) | Ctor::Tuple(n) if prefix.len() + suffix.len() <= *n => {
                let mut fields = prefix.clone();
                fields.resize(n - suffix.len(), Pat::Wild);
                fields.extend(suffix.iter().cloned());
                Some(fields)
            }
            _ => None,
        },
        Pat::Or(_) | Pat::Opaque => None,
    }
}

/// Matrix rows that can match a value built with `ctor`, with the head
/// column replaced by the constructor's fields.
fn specialize(rows: &[Vec<Pat>], ctor: &Ctor) -> Vec<Vec<Pat>> {
    let mut out = Vec::new();
    for row in rows {
        let Some((head, tail)) = row.split_first() else {
            continue;
        };
        if let Some(fields) = head_fields(head, ctor) {
            let mut expanded = fields;
            expanded.extend_from_slice(tail);
            out.push(expanded);
        }
    }
    out
}

/// Folds a constructor's field witnesses back into one rendered witness,
/// leaving the remaining columns untouched.
fn apply_ctor(ctor: &Ctor, mut witness: Vec<String>) -> Vec<String> {
    let arity = ctor.arity();
    let rest = witness.split_off(arity.min(witness.len()));
    let mut out = vec![render_ctor_with(ctor, &witness)];
    out.extend(rest);
    out
}

/// Renders a constructor with wildcard fields.
fn render_ctor(ctor: &Ctor) -> String {
    let fields = vec!["_".to_string(); ctor.arity()];
    render_ctor_with(ctor, &fields)
}

/// Renders a constructor applied to the given field witnesses.
fn render_ctor_with(ctor: &Ctor, fields: &[String]) -> String {
    match ctor {
        Ctor::Bool(value) => value.to_string(),
        Ctor::Literal(text) => text.clone(),
        Ctor::IntRange { lo, hi, kind } => {
            if lo == hi {
                render_int_value(*lo, *kind)
            } else {
                format!(
                    "{}..={}",
                    render_int_value(*lo, *kind),
                    render_int_value(*hi, *kind)
                )
            }
        }
        Ctor::Variant { name, arity } => {
            if *arity == 0 {
                name.clone()
            } else {
                format!("{name}({})", fields.join(", "))
            }
        }
        Ctor::Tuple(_) => format!("({})", fields.join(", ")),
        Ctor::List(_) => format!("[{}]", fields.join(", ")),
    }
}

fn is_catch_all(pat: &Pat) -> bool {
    match pat {
        Pat::Wild => true,
        Pat::Or(alts) => alts.iter().any(is_catch_all),
        _ => false,
    }
}

/// True when `earlier` matches every input `later` matches.
///
/// element-aware for `Tuple` and `Variant`.
/// Previously `Pat::Tuple(_)` subsumed every other `Pat::Tuple(_)`
/// regardless of element shape, so `match (b, x) { (true, false)
/// => .., (true, true) => .. }` reported the second arm as
/// unreachable. Now subsumption descends through fields: a tuple
/// `(P1, P2)` subsumes `(Q1, Q2)` iff `P1 ⊃ Q1` and `P2 ⊃ Q2`.
/// Variant subsumption is the same shape plus the name-match
/// gate.
fn subsumes(earlier: &Pat, later: &Pat) -> bool {
    match earlier {
        Pat::Wild => true,
        Pat::Or(alts) => alts.iter().any(|a| subsumes(a, later)),
        Pat::Bool(b) => match later {
            Pat::Bool(other) => other == b,
            Pat::Or(alts) => alts.iter().all(|a| subsumes(earlier, a)),
            _ => false,
        },
        Pat::Variant {
            name: en,
            fields: ef,
        } => match later {
            Pat::Variant {
                name: ln,
                fields: lf,
            } => {
                if en != ln {
                    return false;
                }
                // No fields recorded on the earlier pattern means
                // a name-only test (`SomeVariant` / `Path` form);
                // treat as wildcard over fields so it subsumes
                // every later occurrence of the same variant.
                if ef.is_empty() {
                    return true;
                }
                if lf.len() != ef.len() {
                    return false;
                }
                ef.iter().zip(lf.iter()).all(|(e, l)| subsumes(e, l))
            }
            Pat::Or(alts) => alts.iter().all(|a| subsumes(earlier, a)),
            _ => false,
        },
        Pat::Literal(text) => match later {
            Pat::Literal(other) => other == text,
            Pat::Or(alts) => alts.iter().all(|a| subsumes(earlier, a)),
            _ => false,
        },
        Pat::Range(lo, hi) => match later {
            Pat::Range(other_lo, other_hi) => lo <= other_lo && other_hi <= hi,
            Pat::Or(alts) => alts.iter().all(|a| subsumes(earlier, a)),
            _ => false,
        },
        Pat::Tuple(ef) => match later {
            Pat::Tuple(lf) => {
                if lf.len() != ef.len() {
                    return false;
                }
                ef.iter().zip(lf.iter()).all(|(e, l)| subsumes(e, l))
            }
            Pat::Or(alts) => alts.iter().all(|a| subsumes(earlier, a)),
            _ => false,
        },
        Pat::List(ef) => match later {
            Pat::List(lf) => {
                if lf.len() != ef.len() {
                    return false;
                }
                ef.iter().zip(lf.iter()).all(|(e, l)| subsumes(e, l))
            }
            Pat::Or(alts) => alts.iter().all(|a| subsumes(earlier, a)),
            _ => false,
        },
        // A rest-carrying slice imposes a length range rather than a single
        // shape, so it is opaque to the subsumption lattice.
        Pat::SliceRest { .. } | Pat::Opaque => false,
    }
}

fn lower_pattern(pattern: &Pattern) -> Pat {
    match &pattern.kind {
        PatternKind::Wildcard | PatternKind::Rest => Pat::Wild,
        PatternKind::Ident { subpattern, .. } => {
            // `x @ subpat` matches whatever `subpat` matches; the
            // bare `x` form is a wildcard. Without recursing, a
            // user could write `x @ 1 => …` followed by `1 => …`
            // and the second arm would silently be flagged as
            // reachable-only when it's actually shadowed (or vice
            // versa for unreachability).
            match subpattern {
                Some(inner) => lower_pattern(inner),
                None => Pat::Wild,
            }
        }
        PatternKind::Literal(lit) => lower_literal(lit),
        PatternKind::Path(path) => {
            let name = path
                .segments
                .last()
                .map(|seg| seg.name.name.clone())
                .unwrap_or_default();
            Pat::Variant {
                name,
                fields: Vec::new(),
            }
        }
        PatternKind::TupleStruct { path, elems } => {
            let name = path
                .segments
                .last()
                .map(|seg| seg.name.name.clone())
                .unwrap_or_default();
            Pat::Variant {
                name,
                fields: elems.iter().map(lower_pattern).collect(),
            }
        }
        PatternKind::Struct { path, .. } => {
            let name = path
                .segments
                .last()
                .map(|seg| seg.name.name.clone())
                .unwrap_or_default();
            Pat::Variant {
                name,
                fields: Vec::new(),
            }
        }
        // `(a, .., d)` names a prefix and a suffix around a rest that spans
        // however many elements the tuple's width leaves between them.
        PatternKind::Tuple(parts) => match parts
            .iter()
            .position(|part| matches!(part.kind, PatternKind::Rest))
        {
            Some(at) => Pat::SliceRest {
                prefix: parts[..at].iter().map(lower_pattern).collect(),
                suffix: parts[at + 1..].iter().map(lower_pattern).collect(),
            },
            None => Pat::Tuple(parts.iter().map(lower_pattern).collect()),
        },
        // A `[..]` / `[..rest]` slice (no fixed elements) matches any
        // slice, so it acts as a catch-all. A fixed-width `[a, b]` slice
        // constrains the length, which is a constructor over a scrutinee
        // whose length is part of its type. A slice mixing fixed elements
        // with a rest spans a range of lengths and stays opaque to the
        // usefulness lattice (it never subsumes another pattern).
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => match rest {
            Some(_) if prefix.is_empty() && suffix.is_empty() => Pat::Wild,
            Some(_) => Pat::SliceRest {
                prefix: prefix.iter().map(lower_pattern).collect(),
                suffix: suffix.iter().map(lower_pattern).collect(),
            },
            None => Pat::List(prefix.iter().map(lower_pattern).collect()),
        },
        PatternKind::Or(alts) => Pat::Or(alts.iter().map(lower_pattern).collect()),
        PatternKind::Range { lo, hi, kind } => {
            let bound = |lit: &Option<Literal>, open: i128| match lit {
                None => Some(open),
                Some(lit) => literal_value(lit),
            };
            let open_end = hi.is_none();
            let (Some(lo), Some(hi)) = (bound(lo, i128::MIN), bound(hi, i128::MAX)) else {
                return Pat::Opaque;
            };
            // `lo..` reaches the type's maximum; `..hi` and `lo..hi` stop
            // before `hi`.
            let hi = match kind {
                gossamer_ast::RangeKind::Exclusive if !open_end => hi - 1,
                _ => hi,
            };
            Pat::Range(lo, hi)
        }
        PatternKind::Ref { inner, .. } => lower_pattern(inner),
        PatternKind::Error => Pat::Opaque,
    }
}

fn lower_literal(lit: &Literal) -> Pat {
    if let Some(value) = literal_value(lit) {
        return Pat::Range(value, value);
    }
    match lit {
        Literal::Bool(value) => Pat::Bool(*value),
        Literal::Int(text) | Literal::Float(text) => Pat::Literal(text.clone()),
        Literal::String(text) => Pat::Literal(format!("\"{text}\"")),
        Literal::Char(c) => Pat::Literal(format!("'{c}'")),
        // The unit type has exactly one value, so `()` is the whole of its
        // domain.
        Literal::Unit => Pat::Tuple(Vec::new()),
        _ => Pat::Opaque,
    }
}

fn collect_enums(source: &SourceFile, resolutions: &Resolutions) -> HashMap<DefId, Vec<String>> {
    let mut map = HashMap::new();
    collect_enums_in(&source.items, resolutions, &mut map);
    map
}

fn collect_enums_in(
    items: &[gossamer_ast::Item],
    resolutions: &Resolutions,
    map: &mut HashMap<DefId, Vec<String>>,
) {
    for item in items {
        match &item.kind {
            ItemKind::Enum(decl) => {
                let Some(def) = resolutions.definition_of(item.id) else {
                    continue;
                };
                let variants = decl
                    .variants
                    .iter()
                    .map(|variant| variant.name.name.clone())
                    .collect();
                map.insert(def, variants);
            }
            ItemKind::Mod(decl) => {
                if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                    collect_enums_in(inner, resolutions, map);
                }
            }
            _ => {}
        }
    }
}

/// Internal lowered pattern form the checker works over.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pat {
    Wild,
    Bool(bool),
    Variant {
        name: String,
        fields: Vec<Pat>,
    },
    Tuple(Vec<Pat>),
    List(Vec<Pat>),
    SliceRest {
        prefix: Vec<Pat>,
        suffix: Vec<Pat>,
    },
    Literal(String),
    /// The inclusive interval of integer or `char` values a literal or range
    /// pattern matches; an open bound reaches past every type's range.
    Range(i128, i128),
    Or(Vec<Pat>),
    Opaque,
}

#[derive(Debug)]
struct Row {
    pat: Pat,
    has_guard: bool,
    span: Span,
}

/// One exhaustiveness diagnostic with its primary source span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExhaustivenessDiagnostic {
    /// Specific error variant.
    pub error: ExhaustivenessError,
    /// Where in the source the problem was detected.
    pub span: Span,
}

impl ExhaustivenessDiagnostic {
    /// Constructs a diagnostic from its error and span.
    #[must_use]
    pub const fn new(error: ExhaustivenessError, span: Span) -> Self {
        Self { error, span }
    }

    /// Renders this diagnostic as a structured
    /// [`gossamer_diagnostics::Diagnostic`].
    #[must_use]
    pub fn to_diagnostic(&self) -> gossamer_diagnostics::Diagnostic {
        use gossamer_diagnostics::{Code, Diagnostic, Location};
        let location = Location::new(self.span.file, self.span);
        let title = format!("{}", self.error);
        let out = match self.error {
            ExhaustivenessError::UnreachableArm => {
                Diagnostic::warning(Code(self.error.code()), title.clone())
            }
            ExhaustivenessError::NonExhaustive { .. } => {
                Diagnostic::error(Code(self.error.code()), title.clone())
            }
        };
        out.with_primary(location, title)
    }
}

impl fmt::Display for ExhaustivenessDiagnostic {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}", self.error)
    }
}

/// Every failure mode the exhaustiveness checker can report.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExhaustivenessError {
    /// The match does not cover every possible scrutinee value.
    #[error("non-exhaustive patterns: {} not covered", format_missing(missing))]
    NonExhaustive {
        /// Missing patterns that witness the incompleteness.
        missing: Vec<String>,
    },
    /// An arm is dominated by a preceding arm and can never be matched.
    #[error("unreachable pattern: earlier arm already matches this value")]
    UnreachableArm,
}

impl ExhaustivenessError {
    /// Returns a short stable tag useful for snapshot tests.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::NonExhaustive { .. } => "non-exhaustive",
            Self::UnreachableArm => "unreachable-arm",
        }
    }

    /// Stable error code used by the diagnostics framework.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NonExhaustive { .. } => "GM0001",
            Self::UnreachableArm => "GM0002",
        }
    }
}

/// The integer value an integer, `char`, or byte literal names, or `None`
/// for any other literal.
fn literal_value(lit: &Literal) -> Option<i128> {
    match lit {
        Literal::Int(text) => parse_int_literal(text),
        Literal::Char(c) => Some(i128::from(u32::from(*c))),
        Literal::Byte(b) => Some(i128::from(*b)),
        _ => None,
    }
}

/// The value of an integer literal's spelling: sign, radix prefix,
/// underscores, and type suffix included.
fn parse_int_literal(text: &str) -> Option<i128> {
    let (negative, body) = match text.trim().strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.trim()),
    };
    let body = [
        "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
    ]
    .iter()
    .find_map(|suffix| body.strip_suffix(suffix))
    .unwrap_or(body);
    let digits: String = body.chars().filter(|c| *c != '_').collect();
    let (radix, digits) = match digits.get(..2) {
        Some("0x" | "0X") => (16, &digits[2..]),
        Some("0o" | "0O") => (8, &digits[2..]),
        Some("0b" | "0B") => (2, &digits[2..]),
        _ => (10, digits.as_str()),
    };
    let magnitude = i128::from_str_radix(digits, radix).ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// The least and greatest value of an integer type; `isize` and `usize` are
/// 64-bit.
fn int_type_bounds(ty: crate::ty::IntTy) -> Interval {
    use crate::ty::IntTy;
    let bits: u32 = match ty {
        IntTy::I8 | IntTy::U8 => 8,
        IntTy::I16 | IntTy::U16 => 16,
        IntTy::I32 | IntTy::U32 => 32,
        IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => 64,
        IntTy::I128 | IntTy::U128 => 127,
    };
    if ty.is_signed() {
        (-(1_i128 << (bits - 1)), (1_i128 << (bits - 1)) - 1)
    } else {
        (0, (1_i128 << bits) - 1)
    }
}

/// `value` as a witness spells it: a `char` literal, a type's `MIN` or
/// `MAX` by name, or the number.
fn render_int_value(value: i128, kind: IntKind) -> String {
    match kind {
        IntKind::Char => match u32::try_from(value).ok().and_then(char::from_u32) {
            Some(c) if !c.is_control() && !c.is_whitespace() => format!("'{c}'"),
            _ => format!("'\\u{{{value:x}}}'"),
        },
        IntKind::Int(ty) => {
            let (lo, hi) = int_type_bounds(ty);
            if value == lo && lo != 0 {
                format!("{}::MIN", ty.as_str())
            } else if value == hi {
                format!("{}::MAX", ty.as_str())
            } else {
                value.to_string()
            }
        }
        IntKind::Plain => value.to_string(),
    }
}

/// The segments of `intervals` cut at every bound a head range pattern of
/// `rows` names, so each segment lies wholly inside or wholly outside each
/// head pattern.
fn int_segments(rows: &[Vec<Pat>], intervals: &[Interval]) -> Vec<Interval> {
    let mut cuts: Vec<i128> = Vec::new();
    for row in rows {
        if let Some(Pat::Range(lo, hi)) = row.first() {
            cuts.push(*lo);
            if let Some(next) = hi.checked_add(1) {
                cuts.push(next);
            }
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut segments = Vec::new();
    for &(start, end) in intervals {
        let mut lo = start;
        for &cut in cuts.iter().filter(|cut| **cut > start && **cut <= end) {
            segments.push((lo, cut - 1));
            lo = cut;
        }
        segments.push((lo, end));
    }
    segments
}

/// Whether the `earlier` patterns together match every value `later` does.
/// A range is covered when the earlier ranges and wildcards span it within
/// `domain`, the scrutinee type's intervals when it has them; a range wholly
/// outside the type matches nothing and so is covered by anything.
fn covered(earlier: &[&Pat], later: &Pat, domain: Option<&[Interval]>) -> bool {
    match later {
        Pat::Or(alts) => alts.iter().all(|alt| covered(earlier, alt, domain)),
        // A catch-all after ranges that tile the type matches nothing new.
        Pat::Wild if domain.is_some() => {
            covered(earlier, &Pat::Range(i128::MIN, i128::MAX), domain)
        }
        Pat::Range(lo, hi) => {
            let wanted: Vec<Interval> = match domain {
                Some(domain) => domain
                    .iter()
                    .filter_map(|&(start, end)| {
                        let (lo, hi) = ((*lo).max(start), (*hi).min(end));
                        (lo <= hi).then_some((lo, hi))
                    })
                    .collect(),
                None => vec![(*lo, *hi)],
            };
            let mut spans: Vec<Interval> = Vec::new();
            for pat in earlier {
                if !collect_ranges(pat, &mut spans) {
                    return true;
                }
            }
            wanted
                .iter()
                .all(|&(lo, hi)| interval_covered((lo, hi), &mut spans))
        }
        _ => earlier.iter().any(|pat| subsumes(pat, later)),
    }
}

/// Adds the ranges `pat` matches to `spans`; answers `false` when `pat`
/// matches every value, which covers any range on its own.
fn collect_ranges(pat: &Pat, spans: &mut Vec<Interval>) -> bool {
    match pat {
        Pat::Wild => false,
        Pat::Range(lo, hi) => {
            spans.push((*lo, *hi));
            true
        }
        Pat::Or(alts) => alts.iter().all(|alt| collect_ranges(alt, spans)),
        _ => true,
    }
}

/// Whether the union of `spans` contains every value of `wanted`.
fn interval_covered(wanted: Interval, spans: &mut [Interval]) -> bool {
    spans.sort_unstable();
    let mut next = wanted.0;
    for &(lo, hi) in spans.iter() {
        if lo > next {
            break;
        }
        if hi >= next {
            match hi.checked_add(1) {
                Some(after) => next = after,
                None => return true,
            }
        }
        if next > wanted.1 {
            return true;
        }
    }
    next > wanted.1
}

fn format_missing(missing: &[String]) -> String {
    missing
        .iter()
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod exhaustiveness_tests {
    use super::*;

    /// Tuple subsumption descends field by field: disjoint elements make
    /// two same-width tuples disjoint.
    #[test]
    fn tuple_subsumption_recurses_through_fields() {
        let earlier = Pat::Tuple(vec![Pat::Bool(true), Pat::Bool(false)]);
        let later = Pat::Tuple(vec![Pat::Bool(true), Pat::Bool(true)]);
        assert!(
            !subsumes(&earlier, &later),
            "(true, false) must NOT subsume (true, true) - they're disjoint"
        );
    }

    /// Wildcard fields still propagate: `(true, _)` does subsume
    /// `(true, true)` because the wildcard covers every concrete
    /// shape.
    #[test]
    fn tuple_subsumption_propagates_wildcard_fields() {
        let earlier = Pat::Tuple(vec![Pat::Bool(true), Pat::Wild]);
        let later = Pat::Tuple(vec![Pat::Bool(true), Pat::Bool(true)]);
        assert!(subsumes(&earlier, &later));
    }

    /// Specialising by a constructor keeps the rows that can match it and
    /// replaces the head column with that constructor's fields.
    #[test]
    fn specialize_keeps_matching_rows_and_expands_fields() {
        let rows = vec![
            vec![Pat::Variant {
                name: "A".to_string(),
                fields: vec![Pat::Bool(true)],
            }],
            vec![Pat::Variant {
                name: "B".to_string(),
                fields: Vec::new(),
            }],
            vec![Pat::Wild],
        ];
        let ctor = Ctor::Variant {
            name: "A".to_string(),
            arity: 1,
        };
        let specialized = specialize(&rows, &ctor);
        assert_eq!(
            specialized,
            vec![vec![Pat::Bool(true)], vec![Pat::Wild]],
            "only `A(..)` and the wildcard row match `A`"
        );
    }

    /// A name-only variant pattern tests the name alone, so specialising
    /// it against a payload-carrying constructor yields wildcard fields.
    #[test]
    fn specialize_treats_name_only_variant_as_wildcard_fields() {
        let rows = vec![vec![Pat::Variant {
            name: "A".to_string(),
            fields: Vec::new(),
        }]];
        let ctor = Ctor::Variant {
            name: "A".to_string(),
            arity: 2,
        };
        assert_eq!(specialize(&rows, &ctor), vec![vec![Pat::Wild, Pat::Wild]]);
    }

    /// An or-pattern head becomes one row per alternative, including when
    /// the alternatives nest.
    #[test]
    fn or_heads_expand_to_one_row_each() {
        let rows = vec![vec![
            Pat::Or(vec![
                Pat::Bool(true),
                Pat::Or(vec![Pat::Bool(false), Pat::Wild]),
            ]),
            Pat::Wild,
        ]];
        let expanded = expand_or_heads(rows);
        assert_eq!(expanded.len(), 3);
        assert!(expanded.iter().all(|row| row.len() == 2));
    }

    /// A witness for a constructor's fields folds back into the
    /// constructor's own spelling, leaving later columns untouched.
    #[test]
    fn apply_ctor_folds_field_witnesses_into_the_constructor() {
        let ctor = Ctor::Variant {
            name: "A".to_string(),
            arity: 1,
        };
        let witness = apply_ctor(&ctor, vec!["false".to_string(), "true".to_string()]);
        assert_eq!(witness, vec!["A(false)".to_string(), "true".to_string()]);
    }

    /// Variant subsumption is name-and-field aware: a shared name with a
    /// disjoint payload does not subsume.
    #[test]
    fn variant_subsumption_name_and_field_aware() {
        let earlier = Pat::Variant {
            name: "A".to_string(),
            fields: vec![Pat::Bool(true)],
        };
        let later = Pat::Variant {
            name: "A".to_string(),
            fields: vec![Pat::Bool(false)],
        };
        assert!(
            !subsumes(&earlier, &later),
            "A(true) must NOT subsume A(false)"
        );
    }

    /// A name-only variant pattern leaves the payload open, so it subsumes
    /// every occurrence of the same variant.
    #[test]
    fn name_only_variant_treated_as_wildcard_over_fields() {
        let earlier = Pat::Variant {
            name: "A".to_string(),
            fields: Vec::new(),
        };
        let later = Pat::Variant {
            name: "A".to_string(),
            fields: vec![Pat::Bool(true)],
        };
        assert!(
            subsumes(&earlier, &later),
            "name-only `A` should subsume `A(true)`"
        );
    }
}
