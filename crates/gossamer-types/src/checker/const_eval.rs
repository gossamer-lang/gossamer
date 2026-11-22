//! Integer constant expressions evaluated while checking: the values of
//! `const` and `static` initializers and of constant indexes.

use gossamer_ast::{BinaryOp, Expr, ExprKind, Literal, NodeId, UnaryOp};
use gossamer_lex::Span;
use gossamer_resolve::Resolution;

use super::literals::{int_bounds, parse_int_magnitude};
use super::{IntTy, TypeChecker};
use crate::{TyKind, TypeError};

/// Deepest chain of `const` items one evaluation follows.
const MAX_CONST_DEPTH: u32 = 64;

/// An operation of a constant expression with no value in its type.
pub(super) struct ConstFault {
    /// What went wrong, as the report states it.
    pub detail: String,
    /// The operation that faulted.
    pub span: Span,
}

/// The smallest and largest value of `int`.
fn int_range(int: IntTy) -> (i128, i128) {
    match int_bounds(int) {
        (_, _, Some(unsigned_max)) => (0, i128::try_from(unsigned_max).unwrap_or(i128::MAX)),
        (signed_max, signed_min_abs, None) => (
            i128::try_from(signed_min_abs).map_or(i128::MIN, |m| -m),
            i128::try_from(signed_max).unwrap_or(i128::MAX),
        ),
    }
}

fn bits_of(int: IntTy) -> u32 {
    match int {
        IntTy::I8 | IntTy::U8 => 8,
        IntTy::I16 | IntTy::U16 => 16,
        IntTy::I32 | IntTy::U32 => 32,
        IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => 64,
        IntTy::I128 | IntTy::U128 => 128,
    }
}

/// `value` reduced to `int`'s width, as the wrapping operators do.
fn wrap_to(value: i128, int: IntTy) -> i128 {
    let bits = bits_of(int);
    if bits >= 128 {
        return value;
    }
    let mask = (1i128 << bits) - 1;
    let low = value & mask;
    let (min, _) = int_range(int);
    if min < 0 && low >= 1i128 << (bits - 1) {
        low - (1i128 << bits)
    } else {
        low
    }
}

fn int_name(int: IntTy) -> &'static str {
    match int {
        IntTy::I8 => "i8",
        IntTy::I16 => "i16",
        IntTy::I32 => "i32",
        IntTy::I64 => "i64",
        IntTy::I128 => "i128",
        IntTy::Isize => "isize",
        IntTy::U8 => "u8",
        IntTy::U16 => "u16",
        IntTy::U32 => "u32",
        IntTy::U64 => "u64",
        IntTy::U128 => "u128",
        IntTy::Usize => "usize",
    }
}

/// Whether `expr` is built only from what a body's constant check reads:
/// integer literals, names, unary `-`, casts, and arithmetic. Shifts are
/// left out: a body's shift amount is GT0116's to check.
fn is_const_shaped(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Literal(Literal::Int(_)) | ExprKind::Path(_) => true,
        ExprKind::Unary {
            op: UnaryOp::Neg,
            operand,
        } => is_const_shaped(operand),
        ExprKind::Cast { value, .. } => is_const_shaped(value),
        ExprKind::Binary { op, lhs, rhs } => {
            !matches!(
                op,
                BinaryOp::Shl | BinaryOp::Shr | BinaryOp::WrappingShl | BinaryOp::WrappingShr
            ) && is_const_shaped(lhs)
                && is_const_shaped(rhs)
        }
        _ => false,
    }
}

/// `a op b` evaluated in `int`, `Ok(None)` for an operator that answers no
/// integer, and a fault where the run-time operation would panic.
fn fold_binary(
    op: BinaryOp,
    a: i128,
    b: i128,
    int: IntTy,
    span: Span,
) -> Result<Option<i128>, ConstFault> {
    let (min, max) = int_range(int);
    let fits = |v: i128| v >= min && v <= max;
    let fault = |detail: String| ConstFault { detail, span };
    let overflow = |sign: &str| fault(format!("{a} {sign} {b} overflows `{}`", int_name(int)));
    let bits = i128::from(bits_of(int));
    let value = match op {
        BinaryOp::Add => a
            .checked_add(b)
            .filter(|v| fits(*v))
            .ok_or_else(|| overflow("+"))?,
        BinaryOp::Sub => a
            .checked_sub(b)
            .filter(|v| fits(*v))
            .ok_or_else(|| overflow("-"))?,
        BinaryOp::Mul => a
            .checked_mul(b)
            .filter(|v| fits(*v))
            .ok_or_else(|| overflow("*"))?,
        BinaryOp::Div | BinaryOp::Rem if b == 0 => {
            return Err(fault(format!(
                "{a} {} 0 divides by zero",
                if matches!(op, BinaryOp::Div) {
                    "/"
                } else {
                    "%"
                }
            )));
        }
        // A signed `MIN / -1` wraps at the declared width.
        BinaryOp::Div => wrap_to(a / b, int),
        BinaryOp::Rem => a.checked_rem(b).ok_or_else(|| overflow("%"))?,
        BinaryOp::WrappingAdd => wrap_to(a.wrapping_add(b), int),
        BinaryOp::WrappingSub => wrap_to(a.wrapping_sub(b), int),
        BinaryOp::WrappingMul => wrap_to(a.wrapping_mul(b), int),
        BinaryOp::Shl | BinaryOp::Shr if !(0..bits).contains(&b) => {
            return Err(fault(format!(
                "a shift by {b} is outside 0..{bits} for `{}`",
                int_name(int)
            )));
        }
        BinaryOp::Shl => wrap_to(a << b, int),
        BinaryOp::Shr => a >> b,
        BinaryOp::WrappingShl => wrap_to(a << b.rem_euclid(bits), int),
        BinaryOp::WrappingShr => a >> b.rem_euclid(bits),
        BinaryOp::BitAnd => a & b,
        BinaryOp::BitOr => a | b,
        BinaryOp::BitXor => a ^ b,
        _ => return Ok(None),
    };
    Ok(Some(value))
}

impl TypeChecker<'_> {
    /// The value of `expr` evaluated in `int`, when it is built only from
    /// integer literals, integer `const` items, unary `-`, and arithmetic.
    /// `Ok(None)` for any other expression.
    pub(super) fn const_int_eval(
        &mut self,
        expr: &Expr,
        int: IntTy,
    ) -> Result<Option<i128>, ConstFault> {
        self.const_int_eval_at(expr, int, 0)
    }

    fn const_int_eval_at(
        &mut self,
        expr: &Expr,
        int: IntTy,
        depth: u32,
    ) -> Result<Option<i128>, ConstFault> {
        if depth > MAX_CONST_DEPTH {
            return Ok(None);
        }
        let (min, max) = int_range(int);
        let fits = |v: i128| v >= min && v <= max;
        match &expr.kind {
            ExprKind::Literal(Literal::Int(text)) => {
                let Some(magnitude) = parse_int_magnitude(text) else {
                    return Ok(None);
                };
                let Ok(value) = i128::try_from(magnitude) else {
                    return Ok(None);
                };
                Ok(fits(value).then_some(value))
            }
            ExprKind::Path(_)
                if let Some(Resolution::Local(binding)) = self.resolutions.get(expr.id) =>
            {
                Ok(self
                    .local_int_consts
                    .get(&binding)
                    .copied()
                    .filter(|v| fits(*v)))
            }
            ExprKind::Path(_) => {
                let Some(Resolution::Def {
                    def,
                    kind: gossamer_resolve::DefKind::Const,
                }) = self.resolutions.get(expr.id)
                else {
                    return Ok(None);
                };
                let Some((value, own)) = self.const_int_exprs.get(&def).cloned() else {
                    return Ok(None);
                };
                // A fault inside another constant is reported at that
                // constant's own initializer.
                Ok(self
                    .const_int_eval_at(&value, own, depth + 1)
                    .ok()
                    .flatten()
                    .filter(|v| fits(*v)))
            }
            ExprKind::Cast {
                value, wrapping, ..
            } => {
                // The operand is evaluated in its own type, as it runs: a
                // `+%` there wraps at that width before the cast reads it.
                let source = self
                    .table
                    .get(value.id)
                    .map(|ty| self.infer.resolve(self.tcx, ty))
                    .and_then(|ty| match self.tcx.kind(ty) {
                        Some(TyKind::Int(source)) => Some(*source),
                        _ => None,
                    })
                    .unwrap_or(IntTy::I128);
                let Some(v) = self.const_int_eval_at(value, source, depth)? else {
                    return Ok(None);
                };
                if *wrapping {
                    Ok(Some(wrap_to(v, int)))
                } else if fits(v) {
                    Ok(Some(v))
                } else {
                    Err(ConstFault {
                        detail: format!("{v} as {} does not fit", int_name(int)),
                        span: expr.span,
                    })
                }
            }
            ExprKind::Unary {
                op: UnaryOp::Neg,
                operand,
            } => {
                let Some(v) = self.const_int_eval_at(operand, int, depth)? else {
                    return Ok(None);
                };
                // Unary `-` on a signed integer wraps at the declared width,
                // so `-(-128i8)` is `-128`.
                Ok(Some(wrap_to(-v, int)))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let Some(a) = self.const_int_eval_at(lhs, int, depth)? else {
                    return Ok(None);
                };
                let Some(b) = self.const_int_eval_at(rhs, int, depth)? else {
                    return Ok(None);
                };
                fold_binary(*op, a, b, int, expr.span)
            }
            _ => Ok(None),
        }
    }

    /// GT0134 for a `const` or `static` integer initializer that overflows
    /// its declared type or divides by zero.
    pub(super) fn check_const_initializer(&mut self, value: &Expr, int: IntTy) {
        if let Err(fault) = self.const_int_eval(value, int) {
            self.report_const_fault(fault);
        }
    }

    fn report_const_fault(&mut self, fault: ConstFault) {
        if self
            .reported_const_faults
            .insert((fault.span.start, fault.span.end))
        {
            self.emit(
                TypeError::ConstArithmetic {
                    detail: fault.detail,
                },
                fault.span,
            );
        }
    }

    /// Notes an arithmetic, negation, or cast expression in a body whose
    /// operands may all be constant, to evaluate once its type settles.
    pub(super) fn note_const_candidate(&mut self, expr: &Expr) {
        let candidate = match &expr.kind {
            ExprKind::Binary { op, rhs, .. } => {
                matches!(
                    op,
                    BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem
                ) && (is_const_shaped(expr)
                    || (matches!(op, BinaryOp::Div | BinaryOp::Rem) && is_const_shaped(rhs)))
            }
            ExprKind::Unary {
                op: UnaryOp::Neg, ..
            }
            | ExprKind::Cast { .. } => is_const_shaped(expr),
            _ => false,
        };
        if candidate {
            self.deferred_const_exprs.push(expr.clone());
        }
    }

    /// GT0134 for a body expression whose constant operands fault in the
    /// type inference settled on: an overflow, a division by a constant
    /// zero, or a constant that does not fit the type it is cast to.
    pub(super) fn check_deferred_const_exprs(&mut self) {
        for expr in std::mem::take(&mut self.deferred_const_exprs) {
            let Some(ty) = self.table.get(expr.id) else {
                continue;
            };
            let ty = self.infer.resolve(self.tcx, ty);
            let Some(TyKind::Int(int)) = self.tcx.kind(ty).cloned() else {
                continue;
            };
            if let ExprKind::Binary {
                op: op @ (BinaryOp::Div | BinaryOp::Rem),
                rhs,
                ..
            } = &expr.kind
                && let Ok(Some(0)) = self.const_int_eval(rhs, int)
            {
                let what = if matches!(op, BinaryOp::Div) {
                    "a division"
                } else {
                    "a remainder"
                };
                self.report_const_fault(ConstFault {
                    detail: format!("{what} by the constant 0 always panics"),
                    span: expr.span,
                });
                continue;
            }
            if let Err(fault) = self.const_int_eval(&expr, int) {
                self.report_const_fault(fault);
            }
        }
    }

    /// Remembers the value of an immutable binding a constant initializes,
    /// so constant expressions naming it evaluate.
    pub(super) fn note_local_constant(&mut self, binding: NodeId, init: &Expr) {
        if let Ok(Some(value)) = self.const_int_eval(init, IntTy::I128) {
            self.local_int_consts.insert(binding, value);
        }
    }

    /// GT0133 for a constant index past either end of a fixed array of
    /// `len` elements.
    pub(super) fn check_const_index(&mut self, index: &Expr, len: usize) {
        let Ok(Some(value)) = self.const_int_eval(index, IntTy::I64) else {
            return;
        };
        let past_end = usize::try_from(value).map_or(true, |i| i >= len);
        if past_end {
            self.emit(
                TypeError::ConstIndexOutOfBounds {
                    index: value.to_string(),
                    len,
                },
                index.span,
            );
        }
    }

    /// The key a literal spells, rendered as source, when it is a constant
    /// a duplicate can be told from: an integer, a string, a `char`, a
    /// `bool`, or a tuple of those.
    fn literal_key(&mut self, expr: &Expr) -> Option<String> {
        match &expr.kind {
            ExprKind::Literal(Literal::String(text) | Literal::RawString { value: text, .. }) => {
                Some(format!("{text:?}"))
            }
            ExprKind::Literal(Literal::Char(c)) => Some(format!("{c:?}")),
            ExprKind::Literal(Literal::Byte(b)) => Some(format!("b'{}'", b.escape_ascii())),
            ExprKind::Literal(Literal::Bool(b)) => Some(b.to_string()),
            ExprKind::Tuple(parts) => {
                let keys = parts
                    .iter()
                    .map(|part| self.literal_key(part))
                    .collect::<Option<Vec<_>>>()?;
                Some(format!("({})", keys.join(", ")))
            }
            _ => self
                .const_int_eval(expr, IntTy::I128)
                .ok()
                .flatten()
                .map(|v| v.to_string()),
        }
    }

    /// GT0137 for a key a map or set literal spells twice: the later entry
    /// would silently replace or merge into the earlier one.
    pub(super) fn check_duplicate_literal_keys<'e>(
        &mut self,
        keys: impl IntoIterator<Item = &'e Expr>,
        collection: &'static str,
    ) {
        let mut seen = std::collections::HashSet::new();
        for key in keys {
            if let Some(rendered) = self.literal_key(key)
                && !seen.insert(rendered.clone())
            {
                self.emit(
                    TypeError::DuplicateLiteralKey {
                        key: rendered,
                        collection,
                    },
                    key.span,
                );
            }
        }
    }
}
