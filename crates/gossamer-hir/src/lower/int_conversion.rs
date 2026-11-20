//! Checked conversions between numeric types: `as` and integer `try_into`.
//!
//! A plain `as` whose operand may not fit its target tests the operand first
//! and panics with `cast overflow` when it does not, so a value never changes
//! silently; `as%` keeps the low bits of an integer and saturates a float.
//! The test is written here, in HIR, so every tier runs the same one, and
//! only where the checker cannot show the operand fits.
//!
//! `v.try_into()` from one integer type to another answers `Ok(v as T)` when
//! `v` fits `T` and an error naming `T` when it does not. The range test is
//! written in the source type, against the target's bounds wherever they
//! fall inside the source's, so every tier runs the same comparison and
//! nothing widens past 64 bits.

use gossamer_ast::{Expr as AstExpr, Ident};
use gossamer_lex::Span;
use gossamer_types::{CastCheck, GenericArg, IntTy, Ty, TyKind};

use crate::tree::{
    HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirLiteral, HirPat, HirPatKind, HirStmt,
    HirStmtKind, HirUnaryOp,
};

use super::Lowerer;

/// The binding the converted value is read from.
const SOURCE: &str = "__try_into_source";

/// The bit width of an integer type; pointer-sized types are 64 bits on
/// every target the toolchain builds.
pub(crate) const fn int_bits(ty: IntTy) -> u32 {
    match ty {
        IntTy::I8 | IntTy::U8 => 8,
        IntTy::I16 | IntTy::U16 => 16,
        IntTy::I32 | IntTy::U32 => 32,
        IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => 64,
        IntTy::I128 | IntTy::U128 => 128,
    }
}

/// The least and greatest values of `ty`, as magnitudes with a sign for the
/// least.
const fn int_bounds(ty: IntTy) -> (bool, u128, u128) {
    let bits = int_bits(ty);
    if ty.is_signed() {
        (true, 1 << (bits - 1), (1 << (bits - 1)) - 1)
    } else {
        (false, 0, (1 << bits) - 1)
    }
}

/// The binding a checked cast reads its operand from.
const CAST_SOURCE: &str = "__cast_source";

impl Lowerer<'_> {
    /// Lowers `value as T`, with the range test the checker says it needs.
    pub(super) fn lower_cast(
        &mut self,
        cast: &AstExpr,
        value: &AstExpr,
        target_node: gossamer_ast::NodeId,
    ) -> HirExprKind {
        let target = self.ty_of(target_node);
        let lowered = self.lower_expr(value);
        let Some(check) = gossamer_types::cast_check(cast, self.table, self.tcx) else {
            return HirExprKind::Cast {
                value: Box::new(lowered),
                ty: target,
            };
        };
        let span = cast.span;
        let source_ty = lowered.ty;
        let Some(fits) = self.cast_fits(check, source_ty, span) else {
            return HirExprKind::Cast {
                value: Box::new(lowered),
                ty: target,
            };
        };
        let guard = self.cast_overflow_guard(fits, source_ty, target, span);
        let bind = self.conv_let(CAST_SOURCE, lowered, span);
        let converted = HirExpr {
            id: self.fresh(),
            span,
            ty: target,
            kind: HirExprKind::Cast {
                value: Box::new(self.conv_path(CAST_SOURCE, source_ty, span)),
                ty: target,
            },
        };
        HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts: vec![bind, guard],
            tail: Some(Box::new(converted)),
            ty: target,
            is_comptime: false,
        })
    }

    /// The test a value of `source_ty` read from the cast's source binding
    /// passes when `check` holds, or `None` when it tests nothing.
    fn cast_fits(&mut self, check: CastCheck, source_ty: Ty, span: Span) -> Option<HirExpr> {
        let read = |this: &mut Self| this.conv_path(CAST_SOURCE, source_ty, span);
        let mut tests = Vec::new();
        match check {
            CastCheck::Int { least, greatest } => {
                for (bound, op) in [(least, HirBinaryOp::Ge), (greatest, HirBinaryOp::Le)] {
                    if let Some(bound) = bound {
                        let bound =
                            self.conv_int_literal(bound < 0, bound.unsigned_abs(), source_ty, span);
                        let lhs = read(self);
                        tests.push(self.conv_compare(op, lhs, bound, span));
                    }
                }
            }
            CastCheck::Float {
                least,
                least_inclusive,
                past,
            } => {
                let bound = self.conv_float_literal(least, source_ty, span);
                let lhs = read(self);
                let op = if least_inclusive {
                    HirBinaryOp::Ge
                } else {
                    HirBinaryOp::Gt
                };
                tests.push(self.conv_compare(op, lhs, bound, span));
                let bound = self.conv_float_literal(past, source_ty, span);
                let lhs = read(self);
                tests.push(self.conv_compare(HirBinaryOp::Lt, lhs, bound, span));
            }
            CastCheck::Char { greatest } => {
                let bound = HirExpr {
                    id: self.fresh(),
                    span,
                    ty: source_ty,
                    kind: HirExprKind::Literal(HirLiteral::Char(
                        char::from_u32(greatest).unwrap_or('\0'),
                    )),
                };
                let lhs = read(self);
                tests.push(self.conv_compare(HirBinaryOp::Le, lhs, bound, span));
            }
        }
        self.conv_all(tests, span)
    }

    /// `if !fits { panic("cast overflow: {value} does not fit T") }`.
    fn cast_overflow_guard(
        &mut self,
        fits: HirExpr,
        source_ty: Ty,
        target: Ty,
        span: Span,
    ) -> HirStmt {
        let unit = self.tcx.unit();
        let bool_ty = self.tcx.bool_ty();
        let string_ty = self.tcx.string_ty();
        let never = self.tcx.never();
        let does_not_fit = HirExpr {
            id: self.fresh(),
            span,
            ty: bool_ty,
            kind: HirExprKind::Unary {
                op: HirUnaryOp::Not,
                operand: Box::new(fits),
            },
        };
        let lead = self.conv_text("cast overflow: ".to_string(), span);
        let shown = self.conv_path(CAST_SOURCE, source_ty, span);
        let rendered = self.builtin_call("__debug", vec![shown], string_ty, span);
        let target_name = gossamer_types::render_ty(self.tcx, target);
        let tail = self.conv_text(format!(" does not fit {target_name}"), span);
        let message = self.builtin_call("__concat", vec![lead, rendered, tail], string_ty, span);
        let panic = self.builtin_call("panic", vec![message], never, span);
        let raise = HirExpr {
            id: self.fresh(),
            span,
            ty: unit,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts: vec![HirStmt {
                    id: self.fresh(),
                    span,
                    kind: HirStmtKind::Expr {
                        expr: panic,
                        has_semi: true,
                    },
                }],
                tail: None,
                ty: unit,
                is_comptime: false,
            }),
        };
        HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr: HirExpr {
                    id: self.fresh(),
                    span,
                    ty: unit,
                    kind: HirExprKind::If {
                        condition: Box::new(does_not_fit),
                        then_branch: Box::new(raise),
                        else_branch: None,
                    },
                },
                has_semi: true,
            },
        }
    }

    /// The conjunction of `tests`, or `None` when there are none.
    fn conv_all(&mut self, mut tests: Vec<HirExpr>, span: Span) -> Option<HirExpr> {
        let first = tests.pop()?;
        Some(tests.into_iter().fold(first, |acc, test| {
            self.conv_compare(HirBinaryOp::And, test, acc, span)
        }))
    }

    fn conv_text(&mut self, text: String, span: Span) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty: self.tcx.string_ty(),
            kind: HirExprKind::Literal(HirLiteral::String(text)),
        }
    }

    /// `let name = value`.
    fn conv_let(&mut self, name: &str, value: HirExpr, span: Span) -> HirStmt {
        let ty = value.ty;
        HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern: HirPat {
                    id: self.fresh(),
                    span,
                    ty,
                    kind: HirPatKind::Binding {
                        name: Ident::new(name),
                        mutable: false,
                    },
                },
                ty,
                init: Some(value),
            },
        }
    }

    /// The float `value` as a literal of the float type `ty`.
    fn conv_float_literal(&mut self, value: f64, ty: Ty, span: Span) -> HirExpr {
        let literal = HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Literal(HirLiteral::Float(format!("{:?}", value.abs()))),
        };
        if value >= 0.0 {
            return literal;
        }
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Unary {
                op: HirUnaryOp::Neg,
                operand: Box::new(literal),
            },
        }
    }

    /// Lowers `receiver.try_into()` whose receiver and target are both
    /// integers to its range check, or `None` for any other conversion.
    pub(super) fn lower_int_try_into(
        &mut self,
        receiver: &AstExpr,
        call: &AstExpr,
    ) -> Option<HirExprKind> {
        let result_ty = self.ty_of(call.id);
        let TyKind::Adt { substs, .. } = self.tcx.kind(result_ty)?.clone() else {
            return None;
        };
        let [GenericArg::Type(ok_ty), GenericArg::Type(err_ty)] = substs.as_slice() else {
            return None;
        };
        let (ok_ty, err_ty) = (*ok_ty, *err_ty);
        let Some(TyKind::Int(target)) = self.tcx.kind(ok_ty).cloned() else {
            return None;
        };
        let source_value = self.lower_expr(receiver);
        let source_ty = source_value.ty;
        let Some(TyKind::Int(source)) = self.tcx.kind(source_ty).cloned() else {
            return None;
        };
        let span = call.span;
        let read = |this: &mut Self| this.conv_path(SOURCE, source_ty, span);

        let (source_signed, source_least, source_greatest) = int_bounds(source);
        let (target_signed, target_least, target_greatest) = int_bounds(target);
        let mut tests = Vec::new();
        if source_signed {
            if !target_signed || target_least < source_least {
                let bound = self.conv_int_literal(target_signed, target_least, source_ty, span);
                let lhs = read(self);
                tests.push(self.conv_compare(HirBinaryOp::Ge, lhs, bound, span));
            }
        }
        if target_greatest < source_greatest {
            let bound = self.conv_int_literal(false, target_greatest, source_ty, span);
            let lhs = read(self);
            tests.push(self.conv_compare(HirBinaryOp::Le, lhs, bound, span));
        }

        let cast = HirExpr {
            id: self.fresh(),
            span,
            ty: ok_ty,
            kind: HirExprKind::Cast {
                value: Box::new(read(self)),
                ty: ok_ty,
            },
        };
        let ok = self.conv_variant_call("Ok", result_ty, cast, span);
        let answer = if let Some(condition) = self.conv_all(tests, span) {
            let message =
                self.conv_text(format!("value out of range for {}", target.as_str()), span);
            let error = HirExpr {
                id: self.fresh(),
                span,
                ty: err_ty,
                kind: HirExprKind::Call {
                    callee: Box::new(self.conv_path_of(&["errors", "Error", "from"], span)),
                    args: vec![message],
                },
            };
            let err = self.conv_variant_call("Err", result_ty, error, span);
            HirExpr {
                id: self.fresh(),
                span,
                ty: result_ty,
                kind: HirExprKind::If {
                    condition: Box::new(condition),
                    then_branch: Box::new(ok),
                    else_branch: Some(Box::new(err)),
                },
            }
        } else {
            ok
        };
        let bind = self.conv_let(SOURCE, source_value, span);
        Some(HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts: vec![bind],
            tail: Some(Box::new(answer)),
            ty: result_ty,
            is_comptime: false,
        }))
    }

    fn conv_path(&mut self, name: &str, ty: Ty, span: Span) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        }
    }

    fn conv_path_of(&mut self, segments: &[&str], span: Span) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty: self.error_ty(),
            kind: HirExprKind::Path {
                segments: segments.iter().map(|s| Ident::new(*s)).collect(),
                def: None,
            },
        }
    }

    /// The integer `magnitude` (negated when `negative`) as a literal of
    /// `ty`.
    fn conv_int_literal(&mut self, negative: bool, magnitude: u128, ty: Ty, span: Span) -> HirExpr {
        let literal = HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Literal(HirLiteral::Int(magnitude.to_string())),
        };
        if !negative {
            return literal;
        }
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Unary {
                op: HirUnaryOp::Neg,
                operand: Box::new(literal),
            },
        }
    }

    fn conv_compare(&mut self, op: HirBinaryOp, lhs: HirExpr, rhs: HirExpr, span: Span) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty: self.tcx.bool_ty(),
            kind: HirExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        }
    }

    fn conv_variant_call(
        &mut self,
        variant: &str,
        ty: Ty,
        payload: HirExpr,
        span: Span,
    ) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Call {
                callee: Box::new(HirExpr {
                    id: self.fresh(),
                    span,
                    ty,
                    kind: HirExprKind::Path {
                        segments: vec![Ident::new(variant)],
                        def: None,
                    },
                }),
                args: vec![payload],
            },
        }
    }
}
