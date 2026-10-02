//! Checks that the windows a call passes into one sequence are disjoint.
//!
//! A call may pass several mutable windows of the same sequence, the shape
//! a merge or a split takes: `merge(&mut xs[..mid], &mut xs[mid..])`. Each
//! window writes through to the sequence, so two that share an element would
//! alias it. Their bounds are evaluated once, into bindings of their own, and
//! every pair is checked before the call; the check panics with the clamped
//! ranges on every tier.

use std::collections::BTreeMap;

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_types::{FnSig, IntTy, Ty, TyCtxt, TyKind};

use crate::ids::HirIdGenerator;
use crate::tree::{
    HirBlock, HirExpr, HirExprKind, HirItem, HirItemKind, HirLiteral, HirPat, HirPatKind,
    HirProgram, HirStmt, HirStmtKind, HirUnaryOp, for_each_child_expr_mut,
};

/// The runtime check every guarded call runs first.
const WINDOWS_DISJOINT: &str = "__gos_windows_disjoint";

/// Rewrites every call in `program` that passes two or more windows of one
/// sequence.
pub(crate) fn guard_disjoint_windows(
    program: &mut HirProgram,
    tcx: &mut TyCtxt,
    ids: &mut HirIdGenerator,
) {
    let mut pass = Guard { tcx, ids, temp: 0 };
    for item in &mut program.items {
        pass.visit_item(item);
    }
}

struct Guard<'a> {
    tcx: &'a mut TyCtxt,
    ids: &'a mut HirIdGenerator,
    temp: u32,
}

/// One window argument's bounds, as the check reads them.
struct Bounds {
    lo: HirExpr,
    hi: HirExpr,
    inclusive: bool,
}

impl Guard<'_> {
    fn visit_item(&mut self, item: &mut HirItem) {
        match &mut item.kind {
            HirItemKind::Fn(f) => {
                if let Some(body) = &mut f.body {
                    self.visit_block(&mut body.block);
                }
            }
            HirItemKind::Impl(imp) => {
                for m in &mut imp.methods {
                    if let Some(body) = &mut m.body {
                        self.visit_block(&mut body.block);
                    }
                }
            }
            HirItemKind::Trait(t) => {
                for m in &mut t.methods {
                    if let Some(body) = &mut m.body {
                        self.visit_block(&mut body.block);
                    }
                }
            }
            HirItemKind::Const(_) | HirItemKind::Static(_) | HirItemKind::Adt(_) => {}
        }
    }

    fn visit_block(&mut self, block: &mut HirBlock) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { init: Some(e), .. }
                | HirStmtKind::Expr { expr: e, .. }
                | HirStmtKind::Defer(e) => self.visit_expr(e),
                HirStmtKind::Item(item) => self.visit_item(item),
                HirStmtKind::Let { init: None, .. } => {}
            }
        }
        if let Some(tail) = &mut block.tail {
            self.visit_expr(tail);
        }
    }

    fn visit_expr(&mut self, expr: &mut HirExpr) {
        if let HirExprKind::Block(block) = &mut expr.kind {
            self.visit_block(block);
            return;
        }
        for_each_child_expr_mut(expr, &mut |child| self.visit_expr(child));
        self.guard_call(expr);
    }

    fn guard_call(&mut self, expr: &mut HirExpr) {
        let (HirExprKind::Call { args, .. } | HirExprKind::MethodCall { args, .. }) =
            &mut expr.kind
        else {
            return;
        };
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, arg) in args.iter().enumerate() {
            if let Some(key) = window_base(arg).and_then(place_key) {
                groups.entry(key).or_default().push(index);
            }
        }
        groups.retain(|_, members| members.len() > 1);
        if groups.is_empty() {
            return;
        }
        let span = expr.span;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let mut stmts = Vec::new();
        for members in groups.values() {
            let Some(base) = window_base(&args[members[0]]).cloned() else {
                continue;
            };
            let len_name = self.temp_name("len");
            let len = self.expr(
                i64_ty,
                span,
                HirExprKind::MethodCall {
                    receiver: Box::new(base),
                    name: Ident::new("len"),
                    args: Vec::new(),
                    owner: None,
                },
            );
            stmts.push(self.let_stmt(&len_name, i64_ty, len, span));
            let mut bounds = Vec::new();
            for &member in members {
                if let Some(b) = self.hoist_bounds(&mut args[member], &len_name, &mut stmts) {
                    bounds.push(b);
                }
            }
            for (a_index, a) in bounds.iter().enumerate() {
                for b in &bounds[a_index + 1..] {
                    let check = self.check_call(&len_name, a, b, span);
                    stmts.push(HirStmt {
                        id: self.ids.next(),
                        span,
                        kind: HirStmtKind::Expr {
                            expr: check,
                            has_semi: true,
                        },
                    });
                }
            }
        }
        let call = std::mem::replace(
            expr,
            HirExpr {
                id: self.ids.next(),
                span,
                ty: expr.ty,
                kind: HirExprKind::Placeholder,
            },
        );
        let ty = call.ty;
        let block = HirBlock {
            id: self.ids.next(),
            span,
            stmts,
            tail: Some(Box::new(call)),
            ty,
            is_comptime: false,
        };
        *expr = self.expr(ty, span, HirExprKind::Block(block));
    }

    /// Moves a window argument's written bounds into bindings, so the window
    /// and its check read one value each, and answers the bounds the check
    /// compares. An open start is `0` and an open end is the length.
    fn hoist_bounds(
        &mut self,
        arg: &mut HirExpr,
        len_name: &str,
        stmts: &mut Vec<HirStmt>,
    ) -> Option<Bounds> {
        let HirExprKind::Unary { operand, .. } = &mut arg.kind else {
            return None;
        };
        let HirExprKind::Index { index, .. } = &mut operand.kind else {
            return None;
        };
        let span = index.span;
        let HirExprKind::Range {
            start,
            end,
            inclusive,
        } = &mut index.kind
        else {
            return None;
        };
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let lo = match start {
            Some(bound) => self.hoist(bound, stmts),
            None => self.int_lit(0, i64_ty, span),
        };
        let hi = match end {
            Some(bound) => self.hoist(bound, stmts),
            None => self.path(len_name, i64_ty, span),
        };
        Some(Bounds {
            lo,
            hi,
            inclusive: *inclusive && end.is_some(),
        })
    }

    /// Binds `bound`'s value, puts a path to the binding in its place, and
    /// answers that value as an `i64`.
    fn hoist(&mut self, bound: &mut HirExpr, stmts: &mut Vec<HirStmt>) -> HirExpr {
        let name = self.temp_name("bound");
        let ty = bound.ty;
        let span = bound.span;
        let path = self.path(&name, ty, span);
        let value = std::mem::replace(bound, path);
        stmts.push(self.let_stmt(&name, ty, value, span));
        let read = self.path(&name, ty, span);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        if matches!(self.tcx.kind_of(ty), TyKind::Int(IntTy::I64)) {
            read
        } else {
            self.expr(
                i64_ty,
                span,
                HirExprKind::Cast {
                    value: Box::new(read),
                    ty: i64_ty,
                },
            )
        }
    }

    fn check_call(&mut self, len_name: &str, a: &Bounds, b: &Bounds, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let callee_ty = self.tcx.intern(TyKind::FnPtr(FnSig {
            inputs: vec![i64_ty; 7],
            output: unit,
        }));
        let callee = self.path(WINDOWS_DISJOINT, callee_ty, span);
        let len = self.path(len_name, i64_ty, span);
        let a_inclusive = self.int_lit(i64::from(a.inclusive), i64_ty, span);
        let b_inclusive = self.int_lit(i64::from(b.inclusive), i64_ty, span);
        let args = vec![
            len,
            self.reclone(&a.lo),
            self.reclone(&a.hi),
            a_inclusive,
            self.reclone(&b.lo),
            self.reclone(&b.hi),
            b_inclusive,
        ];
        self.expr(
            unit,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        )
    }

    fn expr(&mut self, ty: Ty, span: Span, kind: HirExprKind) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind,
        }
    }

    fn path(&mut self, name: &str, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        )
    }

    fn int_lit(&mut self, v: i64, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Literal(HirLiteral::Int(v.to_string())),
        )
    }

    fn let_stmt(&mut self, name: &str, ty: Ty, init: HirExpr, span: Span) -> HirStmt {
        HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Let {
                pattern: HirPat {
                    id: self.ids.next(),
                    span,
                    ty,
                    kind: HirPatKind::Binding {
                        name: Ident::new(name),
                        mutable: false,
                    },
                },
                ty,
                init: Some(init),
            },
        }
    }

    fn reclone(&mut self, e: &HirExpr) -> HirExpr {
        let mut c = e.clone();
        c.id = self.ids.next();
        c
    }

    fn temp_name(&mut self, tag: &str) -> String {
        let n = self.temp;
        self.temp += 1;
        format!("__window_{tag}_{n}")
    }
}

/// The sequence a `&mut base[lo..hi]` argument windows.
fn window_base(arg: &HirExpr) -> Option<&HirExpr> {
    let HirExprKind::Unary {
        op: HirUnaryOp::RefMut,
        operand,
    } = &arg.kind
    else {
        return None;
    };
    let HirExprKind::Index { base, index } = &operand.kind else {
        return None;
    };
    matches!(index.kind, HirExprKind::Range { .. }).then_some(base.as_ref())
}

/// A key naming the place `expr` denotes, when evaluating it again names the
/// same place: a binding reached through fields and indexes that are
/// bindings or literals. The checker admits two windows of one root in one
/// call only over places with one key.
fn place_key(expr: &HirExpr) -> Option<String> {
    match &expr.kind {
        HirExprKind::Path { segments, .. } if segments.len() == 1 => Some(segments[0].name.clone()),
        HirExprKind::Field { receiver, name } => {
            Some(format!("{}.{}", place_key(receiver)?, name.name))
        }
        HirExprKind::TupleIndex { receiver, index } => {
            Some(format!("{}.{index}", place_key(receiver)?))
        }
        HirExprKind::Index { base, index } => {
            let index = match &index.kind {
                HirExprKind::Path { segments, .. } if segments.len() == 1 => {
                    segments[0].name.clone()
                }
                HirExprKind::Literal(HirLiteral::Int(text)) => text.clone(),
                _ => return None,
            };
            Some(format!("{}[{index}]", place_key(base)?))
        }
        HirExprKind::Unary {
            op: HirUnaryOp::Deref,
            operand,
        } => Some(format!("*{}", place_key(operand)?)),
        _ => None,
    }
}
