//! A mutex guard used anywhere but its own `unlock()`.
//!
//! `let guard = m.lock()` holds the lock to the end of the block that binds
//! `guard`, and that block releases it at every exit. A guard passed on,
//! returned, stored, or copied would outlive the release its block makes,
//! so the only thing a program does with one is `guard.unlock()`.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use gossamer_ast::visitor::{Visitor, walk_expr, walk_stmt};
use gossamer_ast::{Expr, ExprKind, NodeId, PatternKind, SourceFile, Stmt, StmtKind};
use gossamer_resolve::{Resolution, Resolutions};

use crate::{TyCtxt, TyKind, TypeDiagnostic, TypeError, TypeTable};

/// Every use of a guard binding other than the receiver of its `unlock()`.
#[must_use]
pub fn check_guard_uses(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<TypeDiagnostic> {
    let mut walk = Walk {
        resolutions,
        table,
        tcx,
        guards: HashSet::new(),
        unlocking: HashSet::new(),
        out: Vec::new(),
    };
    walk.visit_source_file(sf);
    walk.out
        .into_iter()
        .filter(|diag| sf.is_program_text(diag.span))
        .collect()
}

struct Walk<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    guards: HashSet<NodeId>,
    unlocking: HashSet<NodeId>,
    out: Vec<TypeDiagnostic>,
}

impl Walk<'_> {
    fn is_guard(&self, expr: &Expr) -> bool {
        self.table.get(expr.id).is_some_and(|ty| {
            matches!(
                self.tcx.kind(ty),
                Some(TyKind::Adt { def, .. })
                    if self.tcx.def_name(*def) == Some(crate::checker::GUARD_TYPE)
            )
        })
    }

    fn guard_local(&self, expr: &Expr) -> Option<NodeId> {
        match self.resolutions.get(expr.id) {
            Some(Resolution::Local(id)) if self.guards.contains(&id) => Some(id),
            _ => None,
        }
    }
}

impl Visitor for Walk<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let StmtKind::Let {
            pattern,
            init: Some(init),
            ..
        } = &stmt.kind
            && matches!(pattern.kind, PatternKind::Ident { .. })
            && matches!(&init.kind, ExprKind::MethodCall { name, .. } if name.name == "lock")
            && self.is_guard(init)
        {
            self.guards.insert(pattern.id);
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } if name.name == "unlock" && args.is_empty() => {
                self.unlocking.insert(receiver.id);
            }
            ExprKind::Path(path)
                if path.segments.len() == 1
                    && self.guard_local(expr).is_some()
                    && !self.unlocking.contains(&expr.id) =>
            {
                self.out.push(TypeDiagnostic {
                    error: TypeError::GuardEscape {
                        name: path.segments[0].name.name.clone(),
                    },
                    span: expr.span,
                });
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}
