//! A generic declaration's HIR with every type it names instantiated.
//!
//! The MIR builder decides how a value is compared, rendered, hashed, stored
//! and called from the value's type. A template lowered against `Param` makes
//! each of those decisions for an opaque slot, so an instantiation is lowered
//! from its own copy of the declaration whose types are already concrete, and
//! every decision is the one the concrete program would get.

use gossamer_ast::Ident;
use gossamer_hir::{
    HirArrayExpr, HirBlock, HirExpr, HirExprKind, HirFn, HirMatchArm, HirParam, HirPat, HirPatKind,
    HirSelectOp, HirStmtKind,
};
use gossamer_types::Ty;

/// What changes between a generic declaration and one instantiation of it.
pub(crate) trait Instantiation {
    /// The instantiated form of `ty`.
    fn ty(&mut self, ty: Ty) -> Ty;

    /// The `impl` block a method call reaches once its receiver's type, typed
    /// `template` in the declaration, is `concrete`. The checker records the
    /// block for a receiver it knew the type of, and a receiver typed by a
    /// parameter only has one in an instantiation.
    fn method_owner(&mut self, template: Ty, concrete: Ty, method: &str) -> Option<Ident>;
}

/// Rewrites every type `decl` names through `map`.
pub(crate) fn map_fn_types(decl: &mut HirFn, map: &mut impl Instantiation) {
    map_params(&mut decl.params, map);
    if let Some(ret) = &mut decl.ret {
        *ret = map.ty(*ret);
    }
    if let Some(body) = &mut decl.body {
        map_block(&mut body.block, map);
    }
}

fn map_params(params: &mut [HirParam], map: &mut impl Instantiation) {
    for param in params {
        param.ty = map.ty(param.ty);
        map_pat(&mut param.pattern, map);
    }
}

fn map_block(block: &mut HirBlock, map: &mut impl Instantiation) {
    block.ty = map.ty(block.ty);
    for stmt in &mut block.stmts {
        match &mut stmt.kind {
            HirStmtKind::Let { pattern, ty, init } => {
                *ty = map.ty(*ty);
                map_pat(pattern, map);
                if let Some(init) = init {
                    map_expr(init, map);
                }
            }
            HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => map_expr(expr, map),
            // A nested item is its own declaration, typed against its own
            // parameters rather than the enclosing ones.
            HirStmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &mut block.tail {
        map_expr(tail, map);
    }
}

fn map_pat(pat: &mut HirPat, map: &mut impl Instantiation) {
    pat.ty = map.ty(pat.ty);
    match &mut pat.kind {
        HirPatKind::Wildcard
        | HirPatKind::Binding { .. }
        | HirPatKind::Literal(_)
        | HirPatKind::Rest
        | HirPatKind::Range { .. } => {}
        HirPatKind::Tuple(items)
        | HirPatKind::Or(items)
        | HirPatKind::Variant { fields: items, .. } => {
            for item in items {
                map_pat(item, map);
            }
        }
        HirPatKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for item in prefix.iter_mut().chain(suffix.iter_mut()) {
                map_pat(item, map);
            }
            if let Some(rest) = rest {
                map_pat(rest, map);
            }
        }
        HirPatKind::Struct { fields, .. } => {
            for field in fields {
                if let Some(p) = &mut field.pattern {
                    map_pat(p, map);
                }
            }
        }
        HirPatKind::Ref { inner, .. } | HirPatKind::At { sub: inner, .. } => map_pat(inner, map),
    }
}

fn map_arm(arm: &mut HirMatchArm, map: &mut impl Instantiation) {
    map_pat(&mut arm.pattern, map);
    if let Some(guard) = &mut arm.guard {
        map_expr(guard, map);
    }
    map_expr(&mut arm.body, map);
}

fn map_expr(expr: &mut HirExpr, map: &mut impl Instantiation) {
    expr.ty = map.ty(expr.ty);
    match &mut expr.kind {
        HirExprKind::Literal(_)
        | HirExprKind::Path { .. }
        | HirExprKind::Continue { .. }
        | HirExprKind::Return(None)
        | HirExprKind::Break { value: None, .. }
        | HirExprKind::Placeholder => {}
        HirExprKind::Call { callee, args } => {
            map_expr(callee, map);
            for arg in args.iter_mut() {
                map_expr(arg, map);
            }
        }
        HirExprKind::MethodCall {
            receiver,
            name,
            args,
            owner,
        } => {
            let template = receiver.ty;
            map_expr(receiver, map);
            if owner.is_none() {
                *owner = map.method_owner(template, receiver.ty, &name.name);
            }
            for arg in args.iter_mut() {
                map_expr(arg, map);
            }
        }
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            map_expr(receiver, map);
        }
        HirExprKind::Index { base, index } => {
            map_expr(base, map);
            map_expr(index, map);
        }
        HirExprKind::Unary { operand, .. } => map_expr(operand, map),
        HirExprKind::Binary { lhs, rhs, .. } => {
            map_expr(lhs, map);
            map_expr(rhs, map);
        }
        HirExprKind::Assign { place, value } => {
            map_expr(place, map);
            map_expr(value, map);
        }
        HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            map_expr(condition, map);
            map_expr(then_branch, map);
            if let Some(e) = else_branch {
                map_expr(e, map);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            map_expr(scrutinee, map);
            for arm in arms.iter_mut() {
                map_arm(arm, map);
            }
        }
        HirExprKind::Loop { body, .. } => map_expr(body, map),
        HirExprKind::While {
            condition, body, ..
        } => {
            map_expr(condition, map);
            map_expr(body, map);
        }
        HirExprKind::Block(block) => map_block(block, map),
        HirExprKind::Closure { params, ret, body } => {
            map_params(params, map);
            if let Some(ret) = ret {
                *ret = map.ty(*ret);
            }
            map_expr(body, map);
        }
        HirExprKind::LiftedClosure { captures, .. } => {
            for capture in captures.iter_mut() {
                map_expr(capture, map);
            }
        }
        HirExprKind::Select { arms } => {
            for arm in arms {
                match &mut arm.op {
                    HirSelectOp::Recv { pattern, channel } => {
                        map_pat(pattern, map);
                        map_expr(channel, map);
                    }
                    HirSelectOp::Send { channel, value } => {
                        map_expr(channel, map);
                        map_expr(value, map);
                    }
                    HirSelectOp::Default => {}
                }
                map_expr(&mut arm.body, map);
            }
        }
        HirExprKind::Return(Some(e)) | HirExprKind::Break { value: Some(e), .. } => {
            map_expr(e, map);
        }
        HirExprKind::Tuple(items) | HirExprKind::Array(HirArrayExpr::List(items)) => {
            for item in items.iter_mut() {
                map_expr(item, map);
            }
        }
        HirExprKind::Array(HirArrayExpr::Repeat { value, count }) => {
            map_expr(value, map);
            map_expr(count, map);
        }
        HirExprKind::Cast { value, ty } => {
            *ty = map.ty(*ty);
            map_expr(value, map);
        }
        HirExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                map_expr(s, map);
            }
            if let Some(e) = end {
                map_expr(e, map);
            }
        }
    }
}
