//! A spawned task's handle that `main` binds and never names again.
//!
//! `let task = spawn(..)` in `main`, outside a `cohort { }`, with no later
//! read of `task`: nothing waits for the task, so `main` goes on, and can
//! exit, before the task's work is done. A cohort joins every child at its
//! exit, so a handle bound inside one is waited for already.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use gossamer_ast::visitor::{Visitor, walk_block, walk_expr, walk_item};
use gossamer_ast::{Block, Expr, ExprKind, Item, ItemKind, NodeId, PatternKind, SourceFile, Stmt};
use gossamer_ast::{StmtKind, visitor::walk_stmt};
use gossamer_lex::Span;
use gossamer_resolve::{Resolution, Resolutions};

/// The lint id the default registry denies.
pub const UNJOINED_TASK_LINT: &str = "unjoined_task";

/// A `spawn` handle bound in `main` that nothing joins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnjoinedTask {
    /// The handle's binding name.
    pub name: String,
    /// The binding.
    pub span: Span,
}

/// Every handle `main` binds from a prelude `spawn` outside a cohort and
/// never reads afterwards.
#[must_use]
pub fn find_unjoined_tasks(sf: &SourceFile, resolutions: &Resolutions) -> Vec<UnjoinedTask> {
    let mut walk = Walk {
        resolutions,
        in_main: false,
        cohort_depth: 0,
        candidates: Vec::new(),
        read: HashSet::new(),
    };
    walk.visit_source_file(sf);
    let Walk {
        candidates, read, ..
    } = walk;
    candidates
        .into_iter()
        .filter(|(id, task)| !read.contains(id) && sf.is_program_text(task.span))
        .map(|(_, task)| task)
        .collect()
}

struct Walk<'a> {
    resolutions: &'a Resolutions,
    in_main: bool,
    cohort_depth: usize,
    candidates: Vec<(NodeId, UnjoinedTask)>,
    read: HashSet<NodeId>,
}

impl Walk<'_> {
    /// Whether `expr` calls the prelude `spawn`, not a module's own.
    fn is_prelude_spawn(&self, expr: &Expr) -> bool {
        let ExprKind::Call { callee, .. } = &expr.kind else {
            return false;
        };
        let ExprKind::Path(path) = &callee.kind else {
            return false;
        };
        matches!(path.segments.as_slice(), [seg] if seg.name.name == "spawn")
            && !matches!(
                self.resolutions.get(callee.id),
                Some(Resolution::Local(_) | Resolution::Def { .. })
            )
    }
}

impl Visitor for Walk<'_> {
    fn visit_item(&mut self, item: &Item) {
        let was_main = self.in_main;
        self.in_main = matches!(&item.kind, ItemKind::Fn(decl) if decl.name.name == "main");
        walk_item(self, item);
        self.in_main = was_main;
    }

    fn visit_block(&mut self, block: &Block) {
        let cohort = block.is_cohort();
        self.cohort_depth += usize::from(cohort);
        walk_block(self, block);
        self.cohort_depth -= usize::from(cohort);
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let StmtKind::Let {
            pattern,
            init: Some(init),
            ..
        } = &stmt.kind
            && self.in_main
            && self.cohort_depth == 0
            && let PatternKind::Ident {
                name,
                subpattern: None,
                ..
            } = &pattern.kind
            && !name.name.starts_with('_')
            && self.is_prelude_spawn(init)
        {
            self.candidates.push((
                pattern.id,
                UnjoinedTask {
                    name: name.name.clone(),
                    span: pattern.span,
                },
            ));
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let ExprKind::Path(_) = &expr.kind
            && let Some(Resolution::Local(id)) = self.resolutions.get(expr.id)
        {
            self.read.insert(id);
        }
        walk_expr(self, expr);
    }
}
