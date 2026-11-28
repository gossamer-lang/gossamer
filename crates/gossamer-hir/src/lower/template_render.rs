//! Rendering a `std::text::template` / `std::html::template` template.
//!
//! `t.render(data)` and `template::render(source, data)` take any value with
//! a JSON form. The value is encoded where the call is written, where its type
//! is concrete: the call becomes `__gos_text_template_render_parsed(t,
//! json::encode(data))` (the source form `..._render_encoded(source, ..)`),
//! and the encode is written by the walk the serde pass generates for that
//! type.

use gossamer_ast::{Expr as AstExpr, ExprKind as AstExprKind};

use crate::tree::{HirExpr, HirExprKind};

use super::Lowerer;

impl Lowerer<'_> {
    /// The lowered call for a template render, or `None` for any other
    /// expression.
    pub(super) fn lower_template_render(&mut self, expr: &AstExpr) -> Option<HirExpr> {
        let (target, leading, data) = match &expr.kind {
            AstExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } if name.name == "render" && args.len() == 1 => (
                self.template_render_target(receiver)?,
                receiver.as_ref(),
                &args[0],
            ),
            AstExprKind::Call { callee, args } if args.len() == 2 => {
                let AstExprKind::Path(path) = &callee.kind else {
                    return None;
                };
                let [segment] = path.segments.as_slice() else {
                    return None;
                };
                let target = match segment.name.name.as_str() {
                    "__gos_text_template_render" => "__gos_text_template_render_encoded",
                    "__gos_html_template_render" => "__gos_html_template_render_encoded",
                    _ => return None,
                };
                (target, &args[0], &args[1])
            }
            _ => return None,
        };
        // `json::encode` answers the `Result<String, errors::Error>` a render
        // answers, so the encode takes the call's own type.
        let ty = self.ty_of(expr.id);
        let first = self.lower_expr(leading);
        let value = self.lower_expr(data);
        let encode = HirExpr {
            id: self.fresh(),
            span: data.span,
            ty,
            kind: HirExprKind::Call {
                callee: Box::new(self.conv_path_of(&["json", "encode"], data.span)),
                args: vec![value],
            },
        };
        Some(HirExpr {
            id: self.fresh(),
            span: expr.span,
            ty,
            kind: HirExprKind::Call {
                callee: Box::new(self.conv_path_of(&[target], expr.span)),
                args: vec![first, encode],
            },
        })
    }

    /// The render function for a receiver of either template type.
    fn template_render_target(&mut self, receiver: &AstExpr) -> Option<&'static str> {
        let mut ty = self.ty_of(receiver.id);
        while let Some(gossamer_types::TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
            ty = *inner;
        }
        let Some(gossamer_types::TyKind::Adt { def, .. }) = self.tcx.kind(ty) else {
            return None;
        };
        match self.tcx.def_name(*def) {
            Some("__gos_text_template_Template") => Some("__gos_text_template_render_parsed"),
            Some("__gos_html_template_Template") => Some("__gos_html_template_render_parsed"),
            _ => None,
        }
    }
}
