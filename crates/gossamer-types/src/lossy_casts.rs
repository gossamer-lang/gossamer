//! `as` conversions that can change the value they convert.
//!
//! `as` keeps the low bits of an integer that does not fit its target and
//! saturates a float, without a word. A cast is reported when the values its
//! operand can hold do not all fit the target: a narrowing integer cast, a
//! signed and unsigned swap, a float to an integer, a `char` to an integer
//! too narrow for it, or an integer wider than a byte to a `char`. A cast the
//! operand's own form keeps in range is not: a literal, a mask, a remainder,
//! a shift, or a `clamp` with literal bounds, and for a float one that is
//! both rounded and clamped.

#![forbid(unsafe_code)]

use gossamer_ast::visitor::{Visitor, walk_expr};
use gossamer_ast::{BinaryOp, Expr, ExprKind, Literal, SourceFile, UnaryOp};
use gossamer_lex::Span;

use crate::context::TyCtxt;
use crate::table::TypeTable;
use crate::ty::{FloatTy, IntTy, Ty, TyKind};

/// The lint these findings belong to.
pub const LOSSY_CAST_LINT: &str = "lossy_cast";

/// One `as` conversion that can change its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LossyCast {
    /// The cast.
    pub span: Span,
    /// The headline.
    pub title: String,
    /// The explicit spellings to use instead.
    pub help: String,
}

/// Every `as` conversion in `sf` that can change the value it converts.
#[must_use]
pub fn find_lossy_casts(sf: &SourceFile, table: &TypeTable, tcx: &TyCtxt) -> Vec<LossyCast> {
    let mut finder = Finder {
        table,
        tcx,
        out: Vec::new(),
    };
    finder.visit_source_file(sf);
    finder.out
}

/// An inclusive range of integer values.
type Range = (i128, i128);

const CHAR_RANGE: Range = (0, 0x10_FFFF);
/// What `as char` keeps of an integer: its low byte.
const CHAR_TARGET_RANGE: Range = (0, 0xFF);

fn int_range(ty: IntTy) -> Range {
    let bits: u32 = match ty {
        IntTy::I8 | IntTy::U8 => 8,
        IntTy::I16 | IntTy::U16 => 16,
        IntTy::I32 | IntTy::U32 => 32,
        IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => 64,
        IntTy::I128 | IntTy::U128 => 127,
    };
    if ty.is_signed() {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    }
}

/// The value of an integer literal's text: digits in any radix, with
/// underscores and a type suffix allowed.
fn int_literal_value(text: &str) -> Option<i128> {
    let digits: String = text.chars().filter(|c| *c != '_').collect();
    let (radix, body) = if let Some(rest) = digits.strip_prefix("0x") {
        (16, rest)
    } else if let Some(rest) = digits.strip_prefix("0o") {
        (8, rest)
    } else if let Some(rest) = digits.strip_prefix("0b") {
        (2, rest)
    } else {
        (10, digits.as_str())
    };
    let end = body
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(body.len());
    i128::from_str_radix(&body[..end], radix).ok()
}

fn float_literal_value(text: &str) -> Option<f64> {
    let digits: String = text.chars().filter(|c| *c != '_').collect();
    let body = digits
        .strip_suffix("f64")
        .or_else(|| digits.strip_suffix("f32"))
        .unwrap_or(&digits);
    body.parse().ok()
}

/// The value of an integer literal, or of a negated one.
pub(crate) fn literal_int(expr: &Expr) -> Option<i128> {
    match &expr.kind {
        ExprKind::Literal(Literal::Int(text)) => int_literal_value(text),
        ExprKind::Unary {
            op: UnaryOp::Neg,
            operand,
        } => literal_int(operand).map(|v| -v),
        _ => None,
    }
}

/// The value of a float literal (or an integer one), or of a negated one.
fn literal_float(expr: &Expr) -> Option<f64> {
    match &expr.kind {
        ExprKind::Literal(Literal::Float(text)) => float_literal_value(text),
        #[allow(
            clippy::cast_precision_loss,
            reason = "a bound written as an integer literal is compared as the float it is"
        )]
        ExprKind::Literal(Literal::Int(text)) => int_literal_value(text).map(|v| v as f64),
        ExprKind::Unary {
            op: UnaryOp::Neg,
            operand,
        } => literal_float(operand).map(|v| -v),
        _ => None,
    }
}

fn render_bound(value: i128) -> String {
    if value < 0 {
        format!("-{}", value.unsigned_abs())
    } else {
        value.to_string()
    }
}

struct Finder<'a> {
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    out: Vec<LossyCast>,
}

impl Finder<'_> {
    fn kind(&self, ty: Ty) -> Option<&TyKind> {
        let mut ty = ty;
        loop {
            match self.tcx.kind(ty)? {
                TyKind::Ref { inner, .. } => ty = *inner,
                kind => return Some(kind),
            }
        }
    }

    fn int_of(&self, expr: &Expr) -> Option<IntTy> {
        match self.kind(self.table.get(expr.id)?)? {
            TyKind::Int(int) => Some(*int),
            _ => None,
        }
    }

    /// The values an integer expression of type `ty` can take, narrowed by
    /// what its own form shows.
    fn value_range(&self, expr: &Expr, ty: IntTy) -> Range {
        let whole = int_range(ty);
        if let Some(value) = literal_int(expr) {
            return (value, value);
        }
        let narrowed = match &expr.kind {
            ExprKind::Binary { op, lhs, rhs } => match op {
                BinaryOp::BitAnd => literal_int(rhs)
                    .or_else(|| literal_int(lhs))
                    .filter(|mask| *mask >= 0)
                    .map(|mask| (0, mask)),
                BinaryOp::Rem => literal_int(rhs).filter(|n| *n > 0).map(|n| {
                    if whole.0 < 0 {
                        (-(n - 1), n - 1)
                    } else {
                        (0, n - 1)
                    }
                }),
                BinaryOp::Shr if whole.0 >= 0 => literal_int(rhs)
                    .and_then(|k| u32::try_from(k).ok())
                    .filter(|k| *k < 127)
                    .map(|k| (0, whole.1 >> k)),
                _ => None,
            },
            ExprKind::MethodCall { name, args, .. } if name.name == "clamp" => {
                match (
                    args.first().and_then(literal_int),
                    args.get(1).and_then(literal_int),
                ) {
                    (Some(lo), Some(hi)) if lo <= hi => Some((lo, hi)),
                    _ => None,
                }
            }
            ExprKind::Cast { value, .. } => self.int_of(value).map(int_range),
            _ => None,
        };
        narrowed.map_or(whole, |(lo, hi)| (lo.max(whole.0), hi.min(whole.1)))
    }

    /// Whether a float operand is rounded and clamped into `target` by its
    /// own method chain: a `clamp` with literal bounds inside the target's
    /// range, and one of `round`, `floor`, `ceil`, or `trunc`.
    fn float_spelled_in_range(&self, expr: &Expr, target: Range) -> bool {
        let mut rounded = false;
        let mut clamped = false;
        let mut cur = expr;
        loop {
            if let Some(value) = literal_float(cur) {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "the target's bounds are compared as floats"
                )]
                let fits =
                    value.fract() == 0.0 && value >= target.0 as f64 && value <= target.1 as f64;
                return fits || (rounded && clamped);
            }
            let ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } = &cur.kind
            else {
                return rounded && clamped;
            };
            match name.name.as_str() {
                "round" | "floor" | "ceil" | "trunc" => rounded = true,
                "clamp" => {
                    #[allow(
                        clippy::cast_precision_loss,
                        reason = "the target's bounds are compared as floats"
                    )]
                    if let (Some(lo), Some(hi)) = (
                        args.first().and_then(literal_float),
                        args.get(1).and_then(literal_float),
                    ) && lo >= target.0 as f64
                        && hi <= target.1 as f64
                    {
                        clamped = true;
                    }
                }
                _ => {}
            }
            cur = receiver;
        }
    }

    /// The run-time test `cast` needs, with the warning that names it, or
    /// `None` when the operand provably fits or the cast cannot change it.
    fn needed_check(&self, cast: &Expr, value: &Expr) -> Option<(CastCheck, String, String)> {
        let from = self.kind(self.table.get(value.id)?)?.clone();
        let to = self.kind(self.table.get(cast.id)?)?.clone();
        match (&from, &to) {
            (TyKind::Int(source), TyKind::Int(target)) => {
                let check = int_check(self.value_range(value, *source), int_range(*target))?;
                let (source, target) = (source.as_str(), target.as_str());
                let range = int_range_text(int_range(to_int(&to)?));
                Some((
                    check,
                    format!("this `as` panics when the `{source}` value does not fit `{target}`"),
                    format!(
                        "say what a value that does not fit becomes: `v.try_into()` answers `Err`, `v as% {target}` keeps its low bits, `v.clamp({range}) as {target}` saturates"
                    ),
                ))
            }
            (TyKind::Float(source), TyKind::Int(target)) => {
                let target_range = int_range(*target);
                if self.float_spelled_in_range(value, target_range) {
                    return None;
                }
                let check = float_check(*source, target_range);
                let (source, target) = (source.as_str(), target.as_str());
                let range = int_range_text(target_range).replace(", ", ".0, ") + ".0";
                Some((
                    check,
                    format!(
                        "this `as` panics when the `{source}` value is NaN or past the range of `{target}`"
                    ),
                    format!(
                        "say what such a value becomes: `x as% {target}` saturates and reads NaN as 0, `x.round().clamp({range}) as {target}` rounds and saturates"
                    ),
                ))
            }
            (TyKind::Char, TyKind::Int(target)) => {
                let check = int_check(CHAR_RANGE, int_range(*target))?;
                let target = target.as_str();
                Some((
                    CastCheck::Char {
                        greatest: match check {
                            CastCheck::Int { greatest, .. } => {
                                u32::try_from(greatest.unwrap_or(0)).unwrap_or(0)
                            }
                            _ => 0,
                        },
                    },
                    format!("this `as` panics when the character does not fit `{target}`"),
                    format!(
                        "`c as u32` holds every character; `c as% {target}` keeps its low bits"
                    ),
                ))
            }
            (TyKind::Int(source), TyKind::Char) => {
                let check = int_check(self.value_range(value, *source), CHAR_TARGET_RANGE)?;
                Some((
                    check,
                    format!(
                        "this `as` panics when the `{}` value is past `\\u{{FF}}`",
                        source.as_str()
                    ),
                    "`char::from_u32(v)` answers `None` for a value that names no character; `v as% char` keeps its low byte".to_string(),
                ))
            }
            _ => None,
        }
    }

    fn check_cast(&mut self, cast: &Expr, value: &Expr) {
        if let Some((_, title, help)) = self.needed_check(cast, value) {
            self.out.push(LossyCast {
                span: cast.span,
                title,
                help,
            });
        }
    }
}

fn to_int(kind: &TyKind) -> Option<IntTy> {
    match kind {
        TyKind::Int(int) => Some(*int),
        _ => None,
    }
}

fn int_range_text(range: Range) -> String {
    format!("{}, {}", render_bound(range.0), render_bound(range.1))
}

/// The bounds a value in `values` must be tested against to fit `target`,
/// or `None` when every such value fits.
fn int_check(values: Range, target: Range) -> Option<CastCheck> {
    let least = (values.0 < target.0).then_some(target.0);
    let greatest = (values.1 > target.1).then_some(target.1);
    (least.is_some() || greatest.is_some()).then_some(CastCheck::Int { least, greatest })
}

/// The test a float of type `source` passes when it truncates to a value
/// `target` holds, which `as` and `try_into` share.
#[must_use]
pub fn float_to_int_check(source: FloatTy, target: IntTy) -> CastCheck {
    float_check(source, int_range(target))
}

/// The test a float of type `source` passes when it truncates to a value in
/// `target`: above the integer below the least, or at the least where that
/// integer is no `source` value, and below the integer past the greatest.
/// NaN fails both comparisons.
fn float_check(source: FloatTy, target: Range) -> CastCheck {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "each bound is tested for whether the float type holds it exactly"
    )]
    let exact = |value: i128| match source {
        FloatTy::F32 => (value as f32) as i128 == value,
        FloatTy::F64 => (value as f64) as i128 == value,
    };
    let below = target.0 - 1;
    #[allow(
        clippy::cast_precision_loss,
        reason = "the bounds are written as float literals of the source type"
    )]
    let (least, least_inclusive) = if exact(below) {
        (below as f64, false)
    } else {
        (target.0 as f64, true)
    };
    #[allow(
        clippy::cast_precision_loss,
        reason = "the integer past the greatest is a power of two, exact in every float type"
    )]
    let past = (target.1 + 1) as f64;
    CastCheck::Float {
        least,
        least_inclusive,
        past,
    }
}

/// The test an `as` runs before it converts, so a value the target cannot
/// hold panics rather than changing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CastCheck {
    /// An integer (or a `char` read as one) compared in its own type with
    /// the target's least and greatest values, each where the operand can
    /// pass it.
    Int {
        /// The least value the target holds, when the operand can be below it.
        least: Option<i128>,
        /// The greatest value the target holds, when the operand can be above it.
        greatest: Option<i128>,
    },
    /// A float compared in its own type: above (or at) `least` and below
    /// `past`.
    Float {
        /// The lower bound.
        least: f64,
        /// Whether the lower bound itself passes.
        least_inclusive: bool,
        /// The exclusive upper bound.
        past: f64,
    },
    /// A `char` whose scalar value must be at most `greatest`.
    Char {
        /// The greatest scalar value the target holds.
        greatest: u32,
    },
}

/// The test the plain `as` conversion `cast` runs before converting, or
/// `None` when the operand provably fits. An `as%` conversion runs none.
#[must_use]
pub fn cast_check(cast: &Expr, table: &TypeTable, tcx: &TyCtxt) -> Option<CastCheck> {
    let ExprKind::Cast {
        value,
        wrapping: false,
        ..
    } = &cast.kind
    else {
        return None;
    };
    let finder = Finder {
        table,
        tcx,
        out: Vec::new(),
    };
    finder.needed_check(cast, value).map(|(check, _, _)| check)
}

/// The values an integer expression can take, narrowed by what its own
/// form shows (a literal, a mask, a remainder, a shift, a literal `clamp`),
/// or `None` when it is not an integer.
pub(crate) fn int_value_range(
    expr: &Expr,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Option<(i128, i128)> {
    let finder = Finder {
        table,
        tcx,
        out: Vec::new(),
    };
    let ty = finder.int_of(expr)?;
    Some(finder.value_range(expr, ty))
}

impl Visitor for Finder<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if let ExprKind::Cast {
            value,
            wrapping: false,
            ..
        } = &expr.kind
        {
            self.check_cast(expr, value);
        }
        walk_expr(self, expr);
    }
}
