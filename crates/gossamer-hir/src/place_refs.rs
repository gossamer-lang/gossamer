//! Expands a reference to a field or element place at each of its uses.
//!
//! `let data = &mut g.data` names the field `g.data` for as long as the
//! binding is in scope, and the borrow rules keep `g` from being read or
//! written in that scope by any other route. Every use of `data` is therefore
//! the same place as `g.data` spelled at that use, which is how each backend
//! already lowers a field write, a push through a field, and a field read.
//! An element place (`&mut grid[i][j]`, `&mut items[k].count`) evaluates each
//! index once, into a binding of its own where the reference was taken, so
//! every use names the element the reference was taken of. A range index is a
//! window, which keeps its reference.
//!
//! A reference used in place (`*(&mut p)`, `(&mut p)[i]`, `(&mut p).f`,
//! `(&mut p).m()`) is folded back to the place it was taken of, so both the
//! expanded uses and the same shapes written in source reach the backends as
//! plain place expressions. The `next` receiver of a `for` loop keeps its
//! reference, which is what binds each element by reference.

use std::collections::{HashMap, HashSet};

use gossamer_ast::Ident;

use crate::ids::HirIdGenerator;
use crate::lift::collect_pattern_names;
use crate::tree::{
    HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind, HirPat, HirPatKind, HirProgram,
    HirSelectOp, HirStmt, HirStmtKind, HirUnaryOp, for_each_child_expr_mut,
};

/// Rewrites every function in `program`.
pub(crate) fn inline_place_references(program: &mut HirProgram, ids: &mut HirIdGenerator) {
    for item in &mut program.items {
        visit_item(item, ids);
    }
}

fn visit_item(item: &mut HirItem, ids: &mut HirIdGenerator) {
    match &mut item.kind {
        HirItemKind::Fn(f) => rewrite_fn(f, ids),
        HirItemKind::Impl(imp) => imp.methods.iter_mut().for_each(|f| rewrite_fn(f, ids)),
        HirItemKind::Trait(t) => t.methods.iter_mut().for_each(|f| rewrite_fn(f, ids)),
        HirItemKind::Const(_) | HirItemKind::Static(_) | HirItemKind::Adt(_) => {}
    }
}

fn rewrite_fn(f: &mut HirFn, ids: &mut HirIdGenerator) {
    let Some(body) = &mut f.body else {
        return;
    };
    let mut binds: HashMap<String, usize> = HashMap::new();
    for param in &f.params {
        count_pattern(&param.pattern, &mut binds);
    }
    count_block_bindings(&body.block, &mut binds);
    expand_in_block(&mut body.block, &binds, ids);
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

/// The root a place names when `expr` is a chain of field reads and element
/// indexes over a bare name, with at least one step. A range index is a
/// window, not an element, and ends the chain's eligibility.
fn element_place_root(expr: &HirExpr) -> Option<&str> {
    let mut cursor = expr;
    let mut steps = 0;
    loop {
        match &cursor.kind {
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                steps += 1;
                cursor = receiver;
            }
            HirExprKind::Index { base, index } => {
                if matches!(index.kind, HirExprKind::Range { .. }) {
                    return None;
                }
                steps += 1;
                cursor = base;
            }
            HirExprKind::Path { segments, .. } => {
                return match segments.as_slice() {
                    [root] if steps > 0 => Some(root.name.as_str()),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
}

/// Moves each index of the element place under `reference` into a `let` of
/// its own, in evaluation order, so the place names the same element at every
/// use. Returns the bindings to run where the reference was taken.
fn hoist_place_indices(
    reference: &mut HirExpr,
    name: &str,
    ids: &mut HirIdGenerator,
) -> Vec<HirStmt> {
    fn walk(place: &mut HirExpr, name: &str, ids: &mut HirIdGenerator, out: &mut Vec<HirStmt>) {
        match &mut place.kind {
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                walk(receiver, name, ids, out);
            }
            HirExprKind::Index { base, index } => {
                walk(base, name, ids, out);
                if matches!(index.kind, HirExprKind::Literal(_)) {
                    return;
                }
                let binding = format!("__{name}_index_{}", out.len());
                let path = HirExpr {
                    id: ids.next(),
                    span: index.span,
                    ty: index.ty,
                    kind: HirExprKind::Path {
                        segments: vec![Ident::new(binding.clone())],
                        def: None,
                    },
                };
                let value = std::mem::replace(index.as_mut(), path);
                out.push(HirStmt {
                    id: ids.next(),
                    span: value.span,
                    kind: HirStmtKind::Let {
                        pattern: HirPat {
                            id: ids.next(),
                            span: value.span,
                            ty: value.ty,
                            kind: HirPatKind::Binding {
                                name: Ident::new(binding),
                                mutable: false,
                            },
                        },
                        ty: value.ty,
                        init: Some(value),
                    },
                });
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let HirExprKind::Unary { operand, .. } = &mut reference.kind {
        walk(operand, name, ids, &mut out);
    }
    out
}

fn expand_in_block(block: &mut HirBlock, binds: &HashMap<String, usize>, ids: &mut HirIdGenerator) {
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
                        && element_place_root(operand)
                            .is_some_and(|root| binds.get(root).is_some_and(|n| *n == 1)) =>
                {
                    Some((name.name.clone(), init.clone()))
                }
                _ => None,
            },
            _ => None,
        };
        if let Some((name, mut reference)) = candidate
            && !block_mentions_in_closure(block, i + 1, &name)
        {
            let hoisted = hoist_place_indices(&mut reference, &name, ids);
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
            let hoisted_len = hoisted.len();
            block.stmts.splice(i..=i, hoisted);
            i += hoisted_len;
            continue;
        }
        i += 1;
    }
    for stmt in &mut block.stmts {
        match &mut stmt.kind {
            HirStmtKind::Let { init: Some(e), .. }
            | HirStmtKind::Expr { expr: e, .. }
            | HirStmtKind::Defer(e) => expand_in_expr(e, binds, ids),
            HirStmtKind::Let { init: None, .. } | HirStmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &mut block.tail {
        expand_in_expr(tail, binds, ids);
    }
}

fn expand_in_expr(expr: &mut HirExpr, binds: &HashMap<String, usize>, ids: &mut HirIdGenerator) {
    if let HirExprKind::Block(block) = &mut expr.kind {
        expand_in_block(block, binds, ids);
        return;
    }
    for_each_child_expr_mut(expr, &mut |child| expand_in_expr(child, binds, ids));
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

/// Whether `expr` is a place whose reference can be dropped where it is used
/// in place. A range index stays referenced: `&mut xs[a..b]` is a window,
/// while the bare index is a copy.
fn is_place(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Path { segments, .. } => segments.len() == 1,
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            is_place(receiver)
        }
        HirExprKind::Index { base, index } => {
            !matches!(index.kind, HirExprKind::Range { .. }) && is_place(base)
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
