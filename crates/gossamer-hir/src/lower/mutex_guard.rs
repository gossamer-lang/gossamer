//! Lowering a `let` bound to a mutex's `lock()`: the guard holds the lock to
//! the end of the block that binds it.
//!
//! `let guard = m.lock()` becomes `let guard = __gos_sync_mutex_guard(m)`
//! followed by `defer guard.unlock()`, so every exit of the block - its end,
//! `return`, `break`, `?`, and a panic unwinding through it - releases the
//! hold the guard took. A release of a hold already given up does nothing,
//! so an earlier `guard.unlock()` or `m.unlock()` is safe.

use gossamer_ast::{ExprKind as AstExprKind, Ident, PatternKind, Stmt as AstStmt, StmtKind};

use crate::tree::{HirExpr, HirExprKind, HirPat, HirPatKind, HirStmt, HirStmtKind};

use super::Lowerer;

/// The wrapper type a guard `let` binds.
pub(crate) const GUARD_TYPE: &str = "__gos_sync_MutexGuard";

impl Lowerer<'_> {
    /// The two statements a guard `let` lowers to, or `None` for any other
    /// statement.
    pub(super) fn lower_guard_let(&mut self, stmt: &AstStmt) -> Option<[HirStmt; 2]> {
        let StmtKind::Let {
            pattern,
            init: Some(init),
            ..
        } = &stmt.kind
        else {
            return None;
        };
        let AstExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &init.kind
        else {
            return None;
        };
        if name.name != "lock" || !args.is_empty() {
            return None;
        }
        let guard_ty = self.ty_of(init.id);
        let is_guard = matches!(
            self.tcx.kind(guard_ty),
            Some(gossamer_types::TyKind::Adt { def, .. })
                if self.tcx.def_name(*def) == Some(GUARD_TYPE)
        );
        if !is_guard {
            return None;
        }
        let span = stmt.span;
        let mutex = self.lower_expr(receiver);
        let call = HirExpr {
            id: self.fresh(),
            span: init.span,
            ty: guard_ty,
            kind: HirExprKind::Call {
                callee: Box::new(self.conv_path_of(&["__gos_sync_mutex_guard"], init.span)),
                args: vec![mutex],
            },
        };
        let name = match &pattern.kind {
            PatternKind::Ident { name, .. } => name.name.clone(),
            _ => format!("__mutex_guard_{}", pattern.span.start),
        };
        let binding = HirPat {
            id: self.fresh(),
            span: pattern.span,
            ty: guard_ty,
            kind: HirPatKind::Binding {
                name: Ident::new(&name),
                mutable: false,
            },
        };
        let bind = HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern: binding,
                ty: guard_ty,
                init: Some(call),
            },
        };
        let read = HirExpr {
            id: self.fresh(),
            span,
            ty: guard_ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(&name)],
                def: None,
            },
        };
        let release = HirExpr {
            id: self.fresh(),
            span,
            ty: self.tcx.unit(),
            kind: HirExprKind::MethodCall {
                receiver: Box::new(read),
                name: Ident::new("unlock"),
                args: Vec::new(),
                owner: Some(Ident::new(GUARD_TYPE)),
            },
        };
        let defer = HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Defer(release),
        };
        Some([bind, defer])
    }
}
