//! Cleanup points: where a released `runtime::Guard`, or a value of a type
//! with `impl Drop`, runs its cleanup.
//!
//! Gossamer releases a value at its last use, and the tiers reach that point
//! at different instructions, so a released cleanup is queued and runs at the
//! next cleanup point this pass places, on every tier:
//!
//! - A binding whose type holds a cleanup (`TyCtxt::is_scope_bound`) is scope
//!   bound: it lives to the end of its block, as in Rust, and a deferred
//!   `__gos_release_binding(x); __gos_run_cleanups()` gives back its share and
//!   runs the cleanup at every exit of the block, in deferred order.
//! - A value of such a type a statement discards (`let _ = e`, or `e;`) is
//!   bound to a temporary released at the end of its statement.
//! - A statement in which such a value takes part is followed by
//!   `__gos_run_cleanups()`, so a cleanup its releases queued runs when it
//!   completes.
//! - A parameter of such a type is read by a deferred `__gos_keep`, so it
//!   lives to the end of the call; the caller's statement then runs its
//!   cleanup if the call released the last reference.

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_types::{Ty, TyCtxt, TyKind};

use crate::ids::HirIdGenerator;
use crate::tree::{
    HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind, HirPat, HirPatKind, HirProgram,
    HirStmt, HirStmtKind, for_each_child_expr, for_each_child_expr_mut,
};

/// Prefix of the temporary a discarded cleanup-holding value is bound to.
const DISCARD_PREFIX: &str = "__gos_discard_";

/// Places the releases and cleanup points of every value whose type holds a
/// cleanup. Programs without such a type are left as they are.
pub(crate) fn insert_cleanup_points(
    program: &mut HirProgram,
    tcx: &mut TyCtxt,
    ids: &mut HirIdGenerator,
) {
    if !tcx.has_cleanup_types() {
        return;
    }
    let mut pass = CleanupPass {
        tcx,
        ids,
        holds: HashMap::new(),
        next: 0,
    };
    for item in &mut program.items {
        pass.visit_item(item);
    }
}

struct CleanupPass<'a> {
    tcx: &'a mut TyCtxt,
    ids: &'a mut HirIdGenerator,
    /// `is_scope_bound` answers, by type.
    holds: HashMap<Ty, bool>,
    /// Counter naming discard temporaries.
    next: u32,
}

impl CleanupPass<'_> {
    fn holds(&mut self, ty: Ty) -> bool {
        if let Some(&known) = self.holds.get(&ty) {
            return known;
        }
        let answer = self.tcx.is_scope_bound(ty);
        self.holds.insert(ty, answer);
        answer
    }

    fn visit_item(&mut self, item: &mut HirItem) {
        match &mut item.kind {
            HirItemKind::Fn(decl) => self.visit_fn(decl),
            HirItemKind::Impl(imp) => imp.methods.iter_mut().for_each(|m| self.visit_fn(m)),
            HirItemKind::Trait(tr) => tr.methods.iter_mut().for_each(|m| self.visit_fn(m)),
            HirItemKind::Const(_) | HirItemKind::Static(_) | HirItemKind::Adt(_) => {}
        }
    }

    fn visit_fn(&mut self, decl: &mut HirFn) {
        let Some(body) = decl.body.as_mut() else {
            return;
        };
        self.visit_block(&mut body.block);
        let mut kept = Vec::new();
        for param in &decl.params {
            self.scope_bound_bindings(&param.pattern, &mut kept);
        }
        if kept.is_empty() {
            return;
        }
        let span = body.block.span;
        let mut stmts = Vec::new();
        for (name, ty) in kept {
            let read = self.path(&name, ty, span);
            let keep = self.call("__gos_keep", vec![read], span);
            stmts.push(self.expr_stmt(keep));
        }
        let defer = self.defer(stmts, span);
        body.block.stmts.insert(0, defer);
    }

    fn visit_expr(&mut self, expr: &mut HirExpr) {
        if let HirExprKind::Block(block) = &mut expr.kind {
            self.visit_block(block);
            return;
        }
        for_each_child_expr_mut(expr, &mut |child| self.visit_expr(child));
        if let HirExprKind::Match { arms, .. } = &mut expr.kind {
            for arm in arms {
                self.release_arm_bindings(arm);
            }
        }
        self.release_temporary_scrutinee(expr);
    }

    /// Each arm of a `match` on a temporary `Option` or `Result` whose type
    /// holds a cleanup gives the temporary back as it starts, once its
    /// bindings hold their own shares of the payload: the cleanup then runs
    /// when the last binding goes, on every tier.
    fn release_temporary_scrutinee(&mut self, expr: &mut HirExpr) {
        let HirExprKind::Match { scrutinee, arms } = &mut expr.kind else {
            return;
        };
        let temporary = !matches!(
            scrutinee.kind,
            HirExprKind::Path { .. }
                | HirExprKind::Field { .. }
                | HirExprKind::TupleIndex { .. }
                | HirExprKind::Index { .. }
                | HirExprKind::Unary { .. }
        );
        if !temporary || !self.tcx.is_two_word_value(scrutinee.ty) || !self.holds(scrutinee.ty) {
            return;
        }
        // An arm that answers its binding (`Ok(v) => v`, the `?` desugar's)
        // moves the payload out whole, leaving the temporary nothing to give.
        let moves_payload = arms
            .iter()
            .any(|arm| answered_name(&arm.body).is_some_and(|name| binds_name(&arm.pattern, name)));
        if moves_payload {
            return;
        }
        let id = scrutinee.id.0;
        for arm in arms.iter_mut() {
            let span = arm.body.span;
            let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            let literal = HirExpr {
                id: self.ids.next(),
                span,
                ty: i64_ty,
                kind: HirExprKind::Literal(crate::tree::HirLiteral::Int(id.to_string())),
            };
            let release = self.call("__gos_release_scrutinee", vec![literal], span);
            let release = self.expr_stmt(release);
            let ty = arm.body.ty;
            if let HirExprKind::Block(block) = &mut arm.body.kind {
                block.stmts.insert(0, release);
                continue;
            }
            let hole = HirExpr {
                id: self.ids.next(),
                span,
                ty,
                kind: HirExprKind::Placeholder,
            };
            let body = std::mem::replace(&mut arm.body, hole);
            arm.body = HirExpr {
                id: self.ids.next(),
                span,
                ty,
                kind: HirExprKind::Block(HirBlock {
                    id: self.ids.next(),
                    span,
                    stmts: vec![release],
                    tail: Some(Box::new(body)),
                    ty,
                    is_comptime: false,
                }),
            };
        }
    }

    /// Gives back the share each binding of `arm`'s pattern whose type holds
    /// a cleanup took, when the arm's body ends.
    fn release_arm_bindings(&mut self, arm: &mut crate::tree::HirMatchArm) {
        let mut bound = Vec::new();
        self.scope_bound_bindings(&arm.pattern, &mut bound);
        // A binding the arm answers leaves it as the `match`'s value.
        let answered = answered_name(&arm.body);
        bound.retain(|(name, _)| Some(name.as_str()) != answered);
        if bound.is_empty() {
            return;
        }
        let span = arm.body.span;
        let mut stmts = Vec::new();
        for (name, ty) in bound.into_iter().rev() {
            let read = self.path(&name, ty, span);
            let release = self.call("__gos_release_binding", vec![read], span);
            stmts.push(self.expr_stmt(release));
        }
        let drain = self.drain(span);
        stmts.push(drain);
        let defer = self.defer(stmts, span);
        let ty = arm.body.ty;
        let placeholder = HirExpr {
            id: arm.body.id,
            span,
            ty,
            kind: HirExprKind::Placeholder,
        };
        let body = std::mem::replace(&mut arm.body, placeholder);
        arm.body = HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.ids.next(),
                span,
                stmts: vec![defer],
                tail: Some(Box::new(body)),
                ty,
                is_comptime: false,
            }),
        };
    }

    fn visit_block(&mut self, block: &mut HirBlock) {
        let stmts = std::mem::take(&mut block.stmts);
        let mut out = Vec::with_capacity(stmts.len());
        for mut stmt in stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { init, .. } => {
                    if let Some(init) = init {
                        self.visit_expr(init);
                    }
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => self.visit_expr(expr),
                HirStmtKind::Item(item) => self.visit_item(item),
            }
            self.place_stmt(stmt, &mut out);
        }
        block.stmts = out;
        if let Some(tail) = block.tail.as_mut() {
            self.visit_expr(tail);
        }
        self.keep_matched_carriers(block);
        // A binding the block answers leaves it as the block's value, so its
        // share goes with that value rather than ending at the block.
        if let Some(HirExpr {
            kind: HirExprKind::Path { segments, .. },
            ..
        }) = block.tail.as_deref()
            && let [name] = segments.as_slice()
        {
            keep_answered_binding(&mut block.stmts, &name.name);
        }
        // A tail that is itself control flow keeps its place: its blocks have
        // cleanup points of their own, and a backend reads a loop's shape
        // from the block that holds it (`for` is a `loop` whose body ends in
        // its `match`).
        let unit_tail = block.tail.as_ref().is_some_and(|tail| {
            matches!(self.tcx.kind(tail.ty), Some(TyKind::Unit))
                && !matches!(
                    tail.kind,
                    HirExprKind::Match { .. }
                        | HirExprKind::If { .. }
                        | HirExprKind::Loop { .. }
                        | HirExprKind::While { .. }
                        | HirExprKind::Block(_)
                )
        });
        if unit_tail && let Some(tail) = block.tail.take() {
            if self.expr_involves(&tail) {
                let span = tail.span;
                block.stmts.push(HirStmt {
                    id: self.ids.next(),
                    span,
                    kind: HirStmtKind::Expr {
                        expr: *tail,
                        has_semi: true,
                    },
                });
                let drain = self.drain(span);
                block.stmts.push(drain);
            } else {
                block.tail = Some(tail);
            }
        }
    }

    /// Pushes `stmt` onto `out` with the releases and cleanup point it needs.
    fn place_stmt(&mut self, mut stmt: HirStmt, out: &mut Vec<HirStmt>) {
        let span = stmt.span;
        match &mut stmt.kind {
            HirStmtKind::Let {
                pattern,
                init: Some(init),
                ..
            } if matches!(pattern.kind, HirPatKind::Wildcard) && self.holds(init.ty) => {
                let name = self.discard_name();
                pattern.kind = HirPatKind::Binding {
                    name: Ident::new(&name),
                    mutable: false,
                };
                let ty = init.ty;
                out.push(stmt);
                self.release_now(&name, ty, span, out);
            }
            HirStmtKind::Let { pattern, .. } => {
                let mut bound = Vec::new();
                self.scope_bound_bindings(pattern, &mut bound);
                let involves = self.stmt_involves(&stmt);
                out.push(stmt);
                if involves {
                    let drain = self.drain(span);
                    out.push(drain);
                }
                if !bound.is_empty() {
                    let mut stmts = Vec::new();
                    for (name, ty) in bound.into_iter().rev() {
                        let read = self.path(&name, ty, span);
                        let release = self.call("__gos_release_binding", vec![read], span);
                        stmts.push(self.expr_stmt(release));
                    }
                    let drain = self.drain(span);
                    stmts.push(drain);
                    let defer = self.defer(stmts, span);
                    out.push(defer);
                }
            }
            HirStmtKind::Expr { expr, .. }
                if !matches!(self.tcx.kind(expr.ty), Some(TyKind::Unit | TyKind::Never))
                    && self.holds(expr.ty) =>
            {
                let name = self.discard_name();
                let ty = expr.ty;
                let HirStmtKind::Expr { expr, .. } = stmt.kind else {
                    unreachable!("matched as an expression statement")
                };
                let pattern = HirPat {
                    id: self.ids.next(),
                    span,
                    ty,
                    kind: HirPatKind::Binding {
                        name: Ident::new(&name),
                        mutable: false,
                    },
                };
                out.push(HirStmt {
                    id: stmt.id,
                    span,
                    kind: HirStmtKind::Let {
                        pattern,
                        ty,
                        init: Some(expr),
                    },
                });
                self.release_now(&name, ty, span, out);
            }
            HirStmtKind::Expr { expr, .. } => {
                let diverges = matches!(self.tcx.kind(expr.ty), Some(TyKind::Never));
                let involves = !diverges && self.stmt_involves(&stmt);
                out.push(stmt);
                if involves {
                    let drain = self.drain(span);
                    out.push(drain);
                }
            }
            HirStmtKind::Defer(_) | HirStmtKind::Item(_) => out.push(stmt),
        }
    }

    fn discard_name(&mut self) -> String {
        let name = format!("{DISCARD_PREFIX}{}", self.next);
        self.next += 1;
        name
    }

    /// Releases the binding `name` and runs the cleanups it queued, right
    /// after the statement that bound it.
    fn release_now(&mut self, name: &str, ty: Ty, span: Span, out: &mut Vec<HirStmt>) {
        let read = self.path(name, ty, span);
        let release = self.call("__gos_release_binding", vec![read], span);
        let release = self.expr_stmt(release);
        out.push(release);
        let drain = self.drain(span);
        out.push(drain);
    }

    /// The bindings of `pat` whose type holds a cleanup, in source order.
    fn scope_bound_bindings(&mut self, pat: &HirPat, out: &mut Vec<(String, Ty)>) {
        match &pat.kind {
            HirPatKind::Binding { name, .. }
                if self.holds(pat.ty) && !out.iter().any(|(seen, _)| *seen == name.name) =>
            {
                out.push((name.name.clone(), pat.ty));
            }
            HirPatKind::Tuple(subs) | HirPatKind::Variant { fields: subs, .. } => {
                for sub in subs {
                    self.scope_bound_bindings(sub, out);
                }
            }
            HirPatKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for sub in prefix {
                    self.scope_bound_bindings(sub, out);
                }
                if let Some(rest) = rest {
                    self.scope_bound_bindings(rest, out);
                }
                for sub in suffix {
                    self.scope_bound_bindings(sub, out);
                }
            }
            HirPatKind::Struct { fields, .. } => {
                for field in fields {
                    if let Some(sub) = &field.pattern {
                        self.scope_bound_bindings(sub, out);
                    }
                }
            }
            HirPatKind::Ref { inner, .. } => self.scope_bound_bindings(inner, out),
            HirPatKind::Or(alternatives) => {
                if let Some(first) = alternatives.first() {
                    self.scope_bound_bindings(first, out);
                }
            }
            _ => {}
        }
    }

    /// Whether a value whose type holds a cleanup takes part in `stmt`.
    fn stmt_involves(&mut self, stmt: &HirStmt) -> bool {
        match &stmt.kind {
            HirStmtKind::Let { pattern, init, .. } => {
                self.holds(pattern.ty) || init.as_ref().is_some_and(|init| self.expr_involves(init))
            }
            HirStmtKind::Expr { expr, .. } => self.expr_involves(expr),
            HirStmtKind::Defer(_) | HirStmtKind::Item(_) => false,
        }
    }

    fn expr_involves(&mut self, expr: &HirExpr) -> bool {
        let mut tys = Vec::new();
        collect_tys(expr, &mut tys);
        tys.into_iter().any(|ty| self.holds(ty))
    }

    fn path(&mut self, name: &str, ty: Ty, span: Span) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        }
    }

    fn call(&mut self, callee: &str, args: Vec<HirExpr>, span: Span) -> HirExpr {
        let error = self.tcx.error_ty();
        let unit = self.tcx.unit();
        let callee = self.path(callee, error, span);
        HirExpr {
            id: self.ids.next(),
            span,
            ty: unit,
            kind: HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        }
    }

    fn expr_stmt(&mut self, expr: HirExpr) -> HirStmt {
        HirStmt {
            id: self.ids.next(),
            span: expr.span,
            kind: HirStmtKind::Expr {
                expr,
                has_semi: true,
            },
        }
    }

    /// `__gos_run_cleanups();`
    fn drain(&mut self, span: Span) -> HirStmt {
        let call = self.call("__gos_run_cleanups", Vec::new(), span);
        self.expr_stmt(call)
    }

    fn defer(&mut self, stmts: Vec<HirStmt>, span: Span) -> HirStmt {
        let unit = self.tcx.unit();
        let body = HirExpr {
            id: self.ids.next(),
            span,
            ty: unit,
            kind: HirExprKind::Block(HirBlock {
                id: self.ids.next(),
                span,
                stmts,
                tail: None,
                ty: unit,
                is_comptime: false,
            }),
        };
        HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Defer(body),
        }
    }
}

impl CleanupPass<'_> {
    /// Drops the release of an `Option` / `Result` binding a later `match`
    /// takes apart: the compiled tiers move its payload out where it is
    /// matched, so the binding holds nothing to give back, and the bindings
    /// the arms make hold the shares.
    fn keep_matched_carriers(&mut self, block: &mut HirBlock) {
        let mut released: Vec<(usize, String)> = Vec::new();
        for (index, stmt) in block.stmts.iter().enumerate() {
            if let HirStmtKind::Let { pattern, .. } = &stmt.kind
                && let HirPatKind::Binding { name, .. } = &pattern.kind
                && self.tcx.is_two_word_value(pattern.ty)
                && self.holds(pattern.ty)
            {
                released.push((index, name.name.clone()));
            }
        }
        for (index, name) in released {
            let matched = block.stmts[index + 1..]
                .iter()
                .any(|stmt| stmt_matches_on(stmt, &name))
                || block
                    .tail
                    .as_deref()
                    .is_some_and(|tail| expr_matches_on(tail, &name));
            if matched {
                keep_answered_binding(&mut block.stmts, &name);
            }
        }
    }
}

fn stmt_matches_on(stmt: &HirStmt, name: &str) -> bool {
    match &stmt.kind {
        HirStmtKind::Let {
            init: Some(init), ..
        } => expr_matches_on(init, name),
        HirStmtKind::Expr { expr, .. } => expr_matches_on(expr, name),
        _ => false,
    }
}

/// Whether `expr` holds a `match` on the binding `name`.
fn expr_matches_on(expr: &HirExpr, name: &str) -> bool {
    if let HirExprKind::Match { scrutinee, .. } = &expr.kind
        && matches!(&scrutinee.kind, HirExprKind::Path { segments, .. }
            if matches!(segments.as_slice(), [only] if only.name == name))
    {
        return true;
    }
    let mut found = false;
    for_each_child_expr(expr, &mut |child| {
        found = found || expr_matches_on(child, name);
    });
    found
}

/// The binding `expr` answers as a whole, through blocks' tails.
fn answered_name(expr: &HirExpr) -> Option<&str> {
    match &expr.kind {
        HirExprKind::Path { segments, .. } => match segments.as_slice() {
            [only] => Some(only.name.as_str()),
            _ => None,
        },
        HirExprKind::Block(block) => block.tail.as_deref().and_then(answered_name),
        _ => None,
    }
}

/// Drops the release of `name` from the last deferred release in `stmts`
/// that gives it back.
fn keep_answered_binding(stmts: &mut [HirStmt], name: &str) {
    for stmt in stmts.iter_mut().rev() {
        if let HirStmtKind::Let { pattern, .. } = &stmt.kind
            && binds_name(pattern, name)
        {
            return;
        }
        let HirStmtKind::Defer(HirExpr {
            kind: HirExprKind::Block(body),
            ..
        }) = &mut stmt.kind
        else {
            continue;
        };
        let before = body.stmts.len();
        body.stmts.retain(|inner| !releases(inner, name));
        if body.stmts.len() != before {
            return;
        }
    }
}

/// Whether `stmt` is `__gos_release_binding(name)`.
fn releases(stmt: &HirStmt, name: &str) -> bool {
    let HirStmtKind::Expr { expr, .. } = &stmt.kind else {
        return false;
    };
    let HirExprKind::Call { callee, args } = &expr.kind else {
        return false;
    };
    matches!(&callee.kind, HirExprKind::Path { segments, .. }
        if matches!(segments.as_slice(), [only] if only.name == "__gos_release_binding"))
        && matches!(args.as_slice(), [HirExpr { kind: HirExprKind::Path { segments, .. }, .. }]
            if matches!(segments.as_slice(), [only] if only.name == name))
}

/// Whether `pat` binds `name`.
fn binds_name(pat: &HirPat, name: &str) -> bool {
    let mut names = std::collections::HashSet::new();
    crate::lift::collect_pattern_names(pat, &mut names);
    names.contains(name)
}

/// The types of `expr` and every expression inside it, closure bodies aside:
/// a closure's body runs where it is called, not where it is written.
fn collect_tys(expr: &HirExpr, out: &mut Vec<Ty>) {
    out.push(expr.ty);
    if matches!(expr.kind, HirExprKind::Closure { .. }) {
        return;
    }
    for_each_child_expr(expr, &mut |child| collect_tys(child, out));
}
