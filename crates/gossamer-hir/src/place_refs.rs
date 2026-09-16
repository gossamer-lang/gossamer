//! Expands a reference to a field place at each of its uses.
//!
//! `let data = &mut g.data` names the field `g.data` for as long as the
//! binding is in scope, and the borrow rules keep `g` from being read or
//! written in that scope by any other route. Every use of `data` is therefore
//! the same place as `g.data` spelled at that use, which is how each backend
//! already lowers a field write, a push through a field, and a field read.
//!
//! A reference used in place (`*(&mut p)`, `(&mut p)[i]`, `(&mut p).f`,
//! `(&mut p).m()`) is folded back to the place it was taken of, so both the
//! expanded uses and the same shapes written in source reach the backends as
//! plain place expressions. The `next` receiver of a `for` loop keeps its
//! reference, which is what binds each element by reference.

use std::collections::{HashMap, HashSet};

use crate::lift::collect_pattern_names;
use crate::tree::{
    HirArrayExpr, HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind, HirPat, HirProgram,
    HirSelectOp, HirStmtKind, HirUnaryOp,
};

/// Rewrites every function in `program`.
pub(crate) fn inline_place_references(program: &mut HirProgram) {
    for item in &mut program.items {
        visit_item(item);
    }
}

fn visit_item(item: &mut HirItem) {
    match &mut item.kind {
        HirItemKind::Fn(f) => rewrite_fn(f),
        HirItemKind::Impl(imp) => imp.methods.iter_mut().for_each(rewrite_fn),
        HirItemKind::Trait(t) => t.methods.iter_mut().for_each(rewrite_fn),
        HirItemKind::Const(_) | HirItemKind::Static(_) | HirItemKind::Adt(_) => {}
    }
}

fn rewrite_fn(f: &mut HirFn) {
    let Some(body) = &mut f.body else {
        return;
    };
    let mut binds: HashMap<String, usize> = HashMap::new();
    for param in &f.params {
        count_pattern(&param.pattern, &mut binds);
    }
    count_block_bindings(&body.block, &mut binds);
    expand_in_block(&mut body.block, &binds);
    fold_block(&mut body.block);
}

fn count_pattern(pat: &HirPat, binds: &mut HashMap<String, usize>) {
    let mut names = HashSet::new();
    collect_pattern_names(pat, &mut names);
    for name in names {
        *binds.entry(name).or_default() += 1;
    }
}

fn count_block_bindings(block: &HirBlock, binds: &mut HashMap<String, usize>) {
    for stmt in &block.stmts {
        match &stmt.kind {
            HirStmtKind::Let { pattern, init, .. } => {
                count_pattern(pattern, binds);
                if let Some(init) = init {
                    count_expr_bindings(init, binds);
                }
            }
            HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                count_expr_bindings(expr, binds);
            }
            HirStmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &block.tail {
        count_expr_bindings(tail, binds);
    }
}

fn count_expr_bindings(expr: &HirExpr, binds: &mut HashMap<String, usize>) {
    match &expr.kind {
        HirExprKind::Match { arms, .. } => {
            for arm in arms {
                count_pattern(&arm.pattern, binds);
            }
        }
        HirExprKind::Closure { params, .. } => {
            for param in params {
                count_pattern(&param.pattern, binds);
            }
        }
        HirExprKind::Select { arms } => {
            for arm in arms {
                if let HirSelectOp::Recv { pattern, .. } = &arm.op {
                    count_pattern(pattern, binds);
                }
            }
        }
        HirExprKind::Block(block) => {
            count_block_bindings(block, binds);
            return;
        }
        _ => {}
    }
    crate::tree::for_each_child_expr(expr, &mut |child| count_expr_bindings(child, binds));
}

/// The place `expr` names when it is a chain of field reads over a bare name,
/// with at least one field. The chain holds no index or call, so evaluating it
/// again at each use reads the same place.
fn field_place_root(expr: &HirExpr) -> Option<&str> {
    let mut cursor = expr;
    let mut fields = 0;
    loop {
        match &cursor.kind {
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                fields += 1;
                cursor = receiver;
            }
            HirExprKind::Path { segments, .. } => {
                return match segments.as_slice() {
                    [root] if fields > 0 => Some(root.name.as_str()),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
}

fn expand_in_block(block: &mut HirBlock, binds: &HashMap<String, usize>) {
    let mut i = 0;
    while i < block.stmts.len() {
        let candidate = match &block.stmts[i].kind {
            HirStmtKind::Let {
                pattern,
                init:
                    Some(
                        init @ HirExpr {
                            kind:
                                HirExprKind::Unary {
                                    op: HirUnaryOp::RefMut | HirUnaryOp::RefShared,
                                    operand,
                                },
                            ..
                        },
                    ),
                ..
            } => match &pattern.kind {
                crate::tree::HirPatKind::Binding { name, .. }
                    if binds.get(&name.name) == Some(&1)
                        && field_place_root(operand)
                            .is_some_and(|root| binds.get(root).is_some_and(|n| *n == 1)) =>
                {
                    Some((name.name.clone(), init.clone()))
                }
                _ => None,
            },
            _ => None,
        };
        if let Some((name, reference)) = candidate
            && !block_mentions_in_closure(block, i + 1, &name)
        {
            for stmt in &mut block.stmts[i + 1..] {
                match &mut stmt.kind {
                    HirStmtKind::Let { init: Some(e), .. }
                    | HirStmtKind::Expr { expr: e, .. }
                    | HirStmtKind::Defer(e) => substitute(e, &name, &reference),
                    HirStmtKind::Let { init: None, .. } | HirStmtKind::Item(_) => {}
                }
            }
            if let Some(tail) = &mut block.tail {
                substitute(tail, &name, &reference);
            }
            block.stmts.remove(i);
            continue;
        }
        i += 1;
    }
    for stmt in &mut block.stmts {
        match &mut stmt.kind {
            HirStmtKind::Let { init: Some(e), .. }
            | HirStmtKind::Expr { expr: e, .. }
            | HirStmtKind::Defer(e) => expand_in_expr(e, binds),
            HirStmtKind::Let { init: None, .. } | HirStmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &mut block.tail {
        expand_in_expr(tail, binds);
    }
}

fn expand_in_expr(expr: &mut HirExpr, binds: &HashMap<String, usize>) {
    if let HirExprKind::Block(block) = &mut expr.kind {
        expand_in_block(block, binds);
        return;
    }
    for_each_child_expr_mut(expr, &mut |child| expand_in_expr(child, binds));
}

/// Whether `name` is read inside a closure anywhere from statement `from` on.
/// A closure captures the reference, which is not the same as capturing the
/// struct it was taken of.
fn block_mentions_in_closure(block: &HirBlock, from: usize, name: &str) -> bool {
    let mut found = false;
    let mut check = |e: &HirExpr| found |= closure_mentions(e, name, false);
    for stmt in &block.stmts[from..] {
        match &stmt.kind {
            HirStmtKind::Let { init: Some(e), .. }
            | HirStmtKind::Expr { expr: e, .. }
            | HirStmtKind::Defer(e) => check(e),
            HirStmtKind::Let { init: None, .. } | HirStmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &block.tail {
        check(tail);
    }
    found
}

fn closure_mentions(expr: &HirExpr, name: &str, inside: bool) -> bool {
    match &expr.kind {
        HirExprKind::Path { segments, .. } => {
            inside && matches!(segments.as_slice(), [only] if only.name == name)
        }
        HirExprKind::Closure { body, .. } => closure_mentions(body, name, true),
        _ => {
            let mut found = false;
            crate::tree::for_each_child_expr(expr, &mut |child| {
                found |= closure_mentions(child, name, inside);
            });
            found
        }
    }
}

fn substitute(expr: &mut HirExpr, name: &str, reference: &HirExpr) {
    if let HirExprKind::Path { segments, .. } = &expr.kind
        && matches!(segments.as_slice(), [only] if only.name == name)
    {
        *expr = reference.clone();
        return;
    }
    for_each_child_expr_mut(expr, &mut |child| substitute(child, name, reference));
}

fn fold_block(block: &mut HirBlock) {
    for stmt in &mut block.stmts {
        match &mut stmt.kind {
            HirStmtKind::Let { init: Some(e), .. }
            | HirStmtKind::Expr { expr: e, .. }
            | HirStmtKind::Defer(e) => fold_expr(e),
            HirStmtKind::Let { init: None, .. } | HirStmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &mut block.tail {
        fold_expr(tail);
    }
}

/// The place under a reference taken of it, when `expr` is one.
fn referenced_place(expr: &mut HirExpr) -> Option<HirExpr> {
    match &mut expr.kind {
        HirExprKind::Unary {
            op: HirUnaryOp::RefMut | HirUnaryOp::RefShared,
            operand,
        } if is_place(operand) => {
            let hole = HirExpr {
                id: operand.id,
                span: operand.span,
                ty: operand.ty,
                kind: HirExprKind::Placeholder,
            };
            Some(std::mem::replace(operand.as_mut(), hole))
        }
        _ => None,
    }
}

fn is_place(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Path { segments, .. } => segments.len() == 1,
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            is_place(receiver)
        }
        _ => false,
    }
}

fn fold_expr(expr: &mut HirExpr) {
    for_each_child_expr_mut(expr, &mut fold_expr);
    match &mut expr.kind {
        HirExprKind::Unary {
            op: HirUnaryOp::Deref,
            operand,
        } => {
            if let Some(place) = referenced_place(operand) {
                *expr = place;
            }
        }
        HirExprKind::Index { base: receiver, .. }
        | HirExprKind::Field { receiver, .. }
        | HirExprKind::TupleIndex { receiver, .. } => {
            if let Some(place) = referenced_place(receiver) {
                **receiver = place;
            }
        }
        // A `for` loop over `&mut xs` lowers to `(&mut xs).next()`, where the
        // reference is what makes the loop bind each element by reference, so
        // that receiver keeps it. Every other method auto-references its
        // receiver the same way from the place itself.
        HirExprKind::MethodCall { receiver, name, .. } if name.name != "next" => {
            if let Some(place) = referenced_place(receiver) {
                **receiver = place;
            }
        }
        _ => {}
    }
}

/// Applies `f` to every expression directly under `expr`, mutably, in source
/// order. The mutable twin of [`crate::tree::for_each_child_expr`].
// One arm per HIR expression variant, as in the shared read-only walk.
#[allow(clippy::too_many_lines)]
fn for_each_child_expr_mut(expr: &mut HirExpr, f: &mut impl FnMut(&mut HirExpr)) {
    match &mut expr.kind {
        HirExprKind::Literal(_)
        | HirExprKind::Path { .. }
        | HirExprKind::Continue { .. }
        | HirExprKind::Return(None)
        | HirExprKind::Break { value: None, .. }
        | HirExprKind::Placeholder => {}
        HirExprKind::Call { callee, args } => {
            f(callee);
            args.iter_mut().for_each(&mut *f);
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            f(receiver);
            args.iter_mut().for_each(&mut *f);
        }
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            f(receiver);
        }
        HirExprKind::Index { base, index } => {
            f(base);
            f(index);
        }
        HirExprKind::Unary { operand, .. } => f(operand),
        HirExprKind::Binary { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        HirExprKind::Assign { place, value } => {
            f(place);
            f(value);
        }
        HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            f(condition);
            f(then_branch);
            if let Some(e) = else_branch {
                f(e);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            f(scrutinee);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    f(g);
                }
                f(&mut arm.body);
            }
        }
        HirExprKind::Loop { body, .. } => f(body),
        HirExprKind::While {
            condition, body, ..
        } => {
            f(condition);
            f(body);
        }
        HirExprKind::Block(block) => {
            for stmt in &mut block.stmts {
                match &mut stmt.kind {
                    HirStmtKind::Let { init: Some(e), .. }
                    | HirStmtKind::Expr { expr: e, .. }
                    | HirStmtKind::Defer(e) => f(e),
                    HirStmtKind::Let { init: None, .. } | HirStmtKind::Item(_) => {}
                }
            }
            if let Some(tail) = &mut block.tail {
                f(tail);
            }
        }
        HirExprKind::Closure { body, .. } => f(body),
        HirExprKind::LiftedClosure { captures, .. } => captures.iter_mut().for_each(&mut *f),
        HirExprKind::Select { arms } => {
            for arm in arms {
                match &mut arm.op {
                    HirSelectOp::Recv { channel, .. } => f(channel),
                    HirSelectOp::Send { channel, value } => {
                        f(channel);
                        f(value);
                    }
                    HirSelectOp::Default => {}
                }
                f(&mut arm.body);
            }
        }
        HirExprKind::Return(Some(e)) | HirExprKind::Break { value: Some(e), .. } => f(e),
        HirExprKind::Tuple(items) | HirExprKind::Array(HirArrayExpr::List(items)) => {
            items.iter_mut().for_each(&mut *f);
        }
        HirExprKind::Array(HirArrayExpr::Repeat { value, count }) => {
            f(value);
            f(count);
        }
        HirExprKind::Cast { value, .. } => f(value),
        HirExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                f(s);
            }
            if let Some(e) = end {
                f(e);
            }
        }
    }
}
