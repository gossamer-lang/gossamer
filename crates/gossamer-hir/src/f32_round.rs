//! Gives every `f32` value the precision its type declares.
//!
//! An `f32` is carried in a double-width slot on every tier, so an operation
//! on two of them computes at `f64` precision. Rounding the result to `f32`
//! at each producing expression gives exactly the value an `f32` operation
//! answers: `+`, `-`, `*`, `/`, and `sqrt` of two `f32` values rounded once
//! from the exact `f64` result equal the correctly rounded `f32` result, since
//! `f64` carries more than twice `f32`'s significand bits.
//!
//! The rounding is an `as f32` cast, which every tier already lowers as a
//! round through single precision whatever the operand's type. A literal is
//! rewritten to the value it names at `f32` precision instead, so it stays a
//! constant. Values read from a binding, a field, or an element were rounded
//! where they were produced and need nothing further.

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_types::{FloatTy, FnSig, Ty, TyCtxt, TyKind};

use crate::ids::HirIdGenerator;
use crate::tree::{
    HirBinaryOp, HirExpr, HirExprKind, HirItemKind, HirLiteral, HirParam, HirPat, HirPatKind,
    HirProgram, HirStmtKind, for_each_child_expr_mut,
};

/// Rounds every `f32`-producing expression in `program` to `f32` precision.
pub(crate) fn round_f32_values(
    program: &mut HirProgram,
    tcx: &mut TyCtxt,
    ids: &mut HirIdGenerator,
) {
    let string_ty = tcx.string_ty();
    let f32_ty = tcx.float_ty(FloatTy::F32);
    let step_ty = tcx.intern(TyKind::FnPtr(FnSig {
        inputs: vec![f32_ty, f32_ty],
        output: f32_ty,
    }));
    let vec_f32_ty = tcx.intern(TyKind::Vec(f32_ty));
    let iter_f32_ty = tcx.intern(TyKind::Iterator(f32_ty));
    let mut pass = Rounder {
        tcx,
        ids,
        string_ty,
        f32_ty,
        step_ty,
        vec_f32_ty,
        iter_f32_ty,
    };
    for item in &mut program.items {
        match &mut item.kind {
            HirItemKind::Fn(f) => pass.function(f),
            HirItemKind::Impl(imp) => imp.methods.iter_mut().for_each(|f| pass.function(f)),
            HirItemKind::Trait(t) => t.methods.iter_mut().for_each(|f| pass.function(f)),
            HirItemKind::Const(c) => pass.expr(&mut c.value),
            HirItemKind::Static(s) => pass.expr(&mut s.value),
            HirItemKind::Adt(_) => {}
        }
    }
}

struct Rounder<'a> {
    tcx: &'a TyCtxt,
    ids: &'a mut HirIdGenerator,
    string_ty: Ty,
    f32_ty: Ty,
    /// `Fn(f32, f32) -> f32`, the type of a rounding reduction step.
    step_ty: Ty,
    vec_f32_ty: Ty,
    iter_f32_ty: Ty,
}

impl Rounder<'_> {
    fn function(&mut self, f: &mut crate::tree::HirFn) {
        let Some(body) = &mut f.body else {
            return;
        };
        for stmt in &mut body.block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { init: Some(e), .. }
                | HirStmtKind::Expr { expr: e, .. }
                | HirStmtKind::Defer(e) => self.expr(e),
                HirStmtKind::Let { init: None, .. } | HirStmtKind::Item(_) => {}
            }
        }
        if let Some(tail) = &mut body.block.tail {
            self.expr(tail);
        }
    }

    fn is_f32(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(ty), Some(TyKind::Float(FloatTy::F32)))
    }

    fn expr(&mut self, expr: &mut HirExpr) {
        for_each_child_expr_mut(expr, &mut |child| self.expr(child));
        self.render_f32_operands(expr);
        self.fold_f32_reduction(expr);
        if !self.is_f32(expr.ty) {
            return;
        }
        let rounds = match &mut expr.kind {
            HirExprKind::Literal(HirLiteral::Float(text) | HirLiteral::Int(text)) => {
                match f32_literal_text(text) {
                    Some(rounded) => {
                        expr.kind = HirExprKind::Literal(HirLiteral::Float(rounded));
                        false
                    }
                    None => true,
                }
            }
            HirExprKind::Binary { op, .. } => matches!(
                op,
                HirBinaryOp::Add
                    | HirBinaryOp::Sub
                    | HirBinaryOp::Mul
                    | HirBinaryOp::Div
                    | HirBinaryOp::Rem
            ),
            HirExprKind::Call { .. } | HirExprKind::MethodCall { .. } => true,
            _ => false,
        };
        if rounds {
            let ty = expr.ty;
            let inner = std::mem::replace(
                expr,
                HirExpr {
                    id: self.ids.next(),
                    span: expr.span,
                    ty,
                    kind: HirExprKind::Placeholder,
                },
            );
            expr.kind = HirExprKind::Cast {
                value: Box::new(inner),
                ty,
            };
        }
    }
}

impl Rounder<'_> {
    /// Renders each `f32` a formatting call or `to_string` shows through the
    /// single-precision renderer. The value's slot is double width, so only
    /// its static type says which digits read back as it.
    fn render_f32_operands(&mut self, expr: &mut HirExpr) {
        match &mut expr.kind {
            HirExprKind::Call { callee, args } => {
                let HirExprKind::Path {
                    segments,
                    def: None,
                } = &callee.kind
                else {
                    return;
                };
                let renderer = match segments.as_slice() {
                    [only] if only.name == "__debug" => "__gos_f32_debug",
                    [only]
                        if matches!(
                            only.name.as_str(),
                            "__concat"
                                | "println"
                                | "print"
                                | "eprintln"
                                | "eprint"
                                | "format"
                                | "panic"
                        ) =>
                    {
                        "__gos_f32_display"
                    }
                    _ => return,
                };
                for arg in args.iter_mut() {
                    if self.is_f32(arg.ty) {
                        self.wrap_in_renderer(arg, renderer);
                    }
                }
            }
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } if name.name == "to_string" && args.is_empty() && self.is_f32(receiver.ty) => {
                let mut value = std::mem::replace(
                    receiver.as_mut(),
                    HirExpr {
                        id: self.ids.next(),
                        span: expr.span,
                        ty: expr.ty,
                        kind: HirExprKind::Placeholder,
                    },
                );
                self.wrap_in_renderer(&mut value, "__gos_f32_display");
                *expr = value;
            }
            _ => {}
        }
    }

    /// Rewrites a `sum` / `product` (or its `_by` form) answering an `f32`
    /// into the `fold` whose step rounds, so the total rounds after every
    /// element as a sequence of `f32` additions does. A reduction the fusion
    /// pass turned into a loop already has that shape; this reaches the ones
    /// it left, whose runtime walk would otherwise accumulate at double width.
    fn fold_f32_reduction(&mut self, expr: &mut HirExpr) {
        if !self.is_f32(expr.ty) {
            return;
        }
        let span = expr.span;
        match &mut expr.kind {
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                owner: None,
            } => {
                let Some((op, by)) = reduction(name.name.as_str(), args.len()) else {
                    return;
                };
                let mut source = std::mem::replace(receiver.as_mut(), self.placeholder(span));
                if by {
                    let callback = args.remove(0);
                    source = self.mapped(source, callback, span);
                }
                let init = self.f32_literal(op, span);
                let step = self.step(op, span);
                *name = Ident::new("fold");
                **receiver = source;
                *args = vec![init, step];
            }
            HirExprKind::Call { callee, args } => {
                let HirExprKind::Path {
                    segments,
                    def: None,
                } = &mut callee.kind
                else {
                    return;
                };
                let [module, name] = segments.as_mut_slice() else {
                    return;
                };
                if module.name != "iter" {
                    return;
                }
                let Some((op, by)) = reduction(name.name.as_str(), args.len().saturating_sub(1))
                else {
                    return;
                };
                *name = Ident::new("fold");
                let mut source = args.remove(0);
                if by {
                    let callback = args.remove(0);
                    source = self.mapped(source, callback, span);
                }
                let init = self.f32_literal(op, span);
                let step = self.step(op, span);
                *args = vec![source, init, step];
            }
            _ => {}
        }
    }

    /// `|acc, x| (acc <op> x) as f32`.
    fn step(&mut self, op: HirBinaryOp, span: Span) -> HirExpr {
        let (acc, elem) = ("__f32_acc", "__f32_elem");
        let lhs = self.binding_path(acc, span);
        let rhs = self.binding_path(elem, span);
        let combined = HirExpr {
            id: self.ids.next(),
            span,
            ty: self.f32_ty,
            kind: HirExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        };
        let body = HirExpr {
            id: self.ids.next(),
            span,
            ty: self.f32_ty,
            kind: HirExprKind::Cast {
                value: Box::new(combined),
                ty: self.f32_ty,
            },
        };
        let params = [acc, elem]
            .into_iter()
            .map(|name| HirParam {
                pattern: HirPat {
                    id: self.ids.next(),
                    span,
                    ty: self.f32_ty,
                    kind: HirPatKind::Binding {
                        name: Ident::new(name),
                        mutable: false,
                    },
                },
                ty: self.f32_ty,
                is_comptime: false,
            })
            .collect();
        HirExpr {
            id: self.ids.next(),
            span,
            ty: self.step_ty,
            kind: HirExprKind::Closure {
                params,
                ret: Some(self.f32_ty),
                body: Box::new(body),
            },
        }
    }

    fn binding_path(&mut self, name: &str, span: Span) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty: self.f32_ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        }
    }

    /// The identity of `op`: `0.0` for a sum, `1.0` for a product.
    fn f32_literal(&mut self, op: HirBinaryOp, span: Span) -> HirExpr {
        let text = if matches!(op, HirBinaryOp::Mul) {
            "1.0"
        } else {
            "0.0"
        };
        HirExpr {
            id: self.ids.next(),
            span,
            ty: self.f32_ty,
            kind: HirExprKind::Literal(HirLiteral::Float(text.to_string())),
        }
    }

    /// `source.map(callback)`, typed as the map it is: eager over a sequence,
    /// lazy over anything else.
    fn mapped(&mut self, source: HirExpr, callback: HirExpr, span: Span) -> HirExpr {
        let mut peeled = source.ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
            peeled = *inner;
        }
        let ty = match self.tcx.kind(peeled) {
            Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }) => self.vec_f32_ty,
            _ => self.iter_f32_ty,
        };
        HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::MethodCall {
                receiver: Box::new(source),
                name: Ident::new("map"),
                args: vec![callback],
                owner: None,
            },
        }
    }

    fn placeholder(&mut self, span: Span) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty: self.f32_ty,
            kind: HirExprKind::Placeholder,
        }
    }

    fn wrap_in_renderer(&mut self, value: &mut HirExpr, renderer: &str) {
        let span = value.span;
        let string_ty = self.string_ty;
        let callee = HirExpr {
            id: self.ids.next(),
            span,
            ty: string_ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(renderer)],
                def: None,
            },
        };
        let inner = std::mem::replace(
            value,
            HirExpr {
                id: self.ids.next(),
                span,
                ty: string_ty,
                kind: HirExprKind::Placeholder,
            },
        );
        value.kind = HirExprKind::Call {
            callee: Box::new(callee),
            args: vec![inner],
        };
    }
}

/// The operator a reduction named `name` taking `args` arguments combines
/// with, and whether it maps each element through a callback first.
fn reduction(name: &str, args: usize) -> Option<(HirBinaryOp, bool)> {
    match (name, args) {
        ("sum", 0) => Some((HirBinaryOp::Add, false)),
        ("product", 0) => Some((HirBinaryOp::Mul, false)),
        ("sum_by", 1) => Some((HirBinaryOp::Add, true)),
        ("product_by", 1) => Some((HirBinaryOp::Mul, true)),
        _ => None,
    }
}

/// The literal text naming `text`'s value at `f32` precision, or `None` when
/// that value is not finite and the literal has to round at run time.
fn f32_literal_text(text: &str) -> Option<String> {
    let digits: String = text.chars().filter(|c| *c != '_').collect();
    let digits = digits
        .strip_suffix("f32")
        .or_else(|| digits.strip_suffix("f64"))
        .unwrap_or(&digits);
    let value: f64 = digits.parse().ok()?;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "rounding to single precision is the conversion this computes"
    )]
    let rounded = f64::from(value as f32);
    rounded.is_finite().then(|| format!("{rounded:?}"))
}

#[cfg(test)]
mod tests {
    use super::f32_literal_text;

    #[test]
    fn f32_literal_text_names_the_single_precision_value() {
        assert_eq!(
            f32_literal_text("0.1").as_deref(),
            Some("0.10000000149011612")
        );
        assert_eq!(
            f32_literal_text("16777217.0").as_deref(),
            Some("16777216.0")
        );
        assert_eq!(f32_literal_text("1_000f32").as_deref(), Some("1000.0"));
        assert_eq!(f32_literal_text("1e39"), None);
    }
}
