//! A `let` in an inner block that hides an outer `let mut` binding nothing
//! ever assigns.
//!
//! `let mut status = "pending"` followed, inside an `if`, by
//! `let status = "done"` declares a second binding: the outer `status`
//! keeps its value, and its `mut` is never used. The `mut` says a write to
//! the outer binding was meant, so the inner `let` is refused. Shadowing an
//! immutable binding, and shadowing a `mut` binding the code assigns, borrows
//! mutably, or calls a method on anywhere, stay legal.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use gossamer_ast::visitor::{Visitor, walk_block, walk_expr, walk_stmt};
use gossamer_ast::{
    Block, Expr, ExprKind, Mutability, NodeId, Pattern, PatternKind, SourceFile, Stmt, StmtKind,
    UnaryOp,
};
use gossamer_lex::Span;
use gossamer_resolve::{Resolution, Resolutions};

/// An inner `let` that hides an outer `let mut` binding nothing assigns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedMutDiagnostic {
    /// The name both bindings share.
    pub name: String,
    /// The inner binding.
    pub span: Span,
    /// The outer `let mut` binding.
    pub outer: Span,
}

impl ShadowedMutDiagnostic {
    /// The code this refusal carries.
    pub const CODE: &'static str = "GT0139";

    /// Renders this refusal as a structured diagnostic.
    #[must_use]
    pub fn to_diagnostic(&self) -> gossamer_diagnostics::Diagnostic {
        use gossamer_diagnostics::{Code, Diagnostic, Location};
        let name = &self.name;
        Diagnostic::error(
            Code(Self::CODE),
            format!("this `let {name}` hides `let mut {name}`, which nothing ever assigns"),
        )
        .with_primary(
            Location::new(self.span.file, self.span),
            format!("declares a second `{name}`; the outer one keeps its value"),
        )
        .with_secondary(
            Location::new(self.outer.file, self.outer),
            "declared `mut` here",
        )
        .with_help(format!(
            "to change the outer `{name}`, assign it: `{name} = ..`; otherwise drop its `mut` \
             and give this binding a name of its own"
        ))
    }
}

/// Every `let` in `sf` that, in an inner block, hides an outer `let mut`
/// binding the code never assigns, borrows mutably, or calls a method on.
#[must_use]
pub fn check_shadowed_mut(
    sf: &SourceFile,
    resolutions: &Resolutions,
) -> Vec<ShadowedMutDiagnostic> {
    let mut walk = Walk {
        resolutions,
        scopes: Vec::new(),
        written: HashSet::new(),
        candidates: Vec::new(),
    };
    walk.visit_source_file(sf);
    let Walk {
        written,
        candidates,
        ..
    } = walk;
    candidates
        .into_iter()
        .filter(|candidate| !written.contains(&candidate.outer_id))
        .filter(|candidate| sf.is_program_text(candidate.span))
        .map(|candidate| ShadowedMutDiagnostic {
            name: candidate.name,
            span: candidate.span,
            outer: candidate.outer_span,
        })
        .collect()
}

/// A plain `let` binding in scope.
struct Binding {
    name: String,
    id: NodeId,
    span: Span,
    mutable: bool,
}

struct Candidate {
    name: String,
    span: Span,
    outer_id: NodeId,
    outer_span: Span,
}

struct Walk<'a> {
    resolutions: &'a Resolutions,
    /// The `let` bindings of each enclosing block, innermost last.
    scopes: Vec<Vec<Binding>>,
    /// Bindings some expression writes through.
    written: HashSet<NodeId>,
    candidates: Vec<Candidate>,
}

impl Walk<'_> {
    /// The binding a place expression writes through, when its root is a
    /// local.
    fn place_root(&self, place: &Expr) -> Option<NodeId> {
        match &place.kind {
            ExprKind::Path(path) if path.segments.len() == 1 => {
                match self.resolutions.get(place.id) {
                    Some(Resolution::Local(id)) => Some(id),
                    _ => None,
                }
            }
            ExprKind::FieldAccess { receiver, .. } => self.place_root(receiver),
            ExprKind::Index { base, .. } => self.place_root(base),
            ExprKind::Unary {
                op: UnaryOp::Deref,
                operand,
            } => self.place_root(operand),
            _ => None,
        }
    }

    fn note_written(&mut self, place: &Expr) {
        if let Some(id) = self.place_root(place) {
            self.written.insert(id);
        }
    }

    fn declare(&mut self, pattern: &Pattern) {
        let PatternKind::Ident {
            mutability,
            name,
            subpattern: None,
        } = &pattern.kind
        else {
            return;
        };
        let name = name.name.clone();
        if name.starts_with('_') {
            return;
        }
        let Some((current, enclosing)) = self.scopes.split_last() else {
            return;
        };
        let hidden_here = current.iter().any(|binding| binding.name == name);
        if !hidden_here
            && let Some(outer) = enclosing
                .iter()
                .rev()
                .find_map(|scope| scope.iter().rev().find(|binding| binding.name == name))
            && outer.mutable
        {
            self.candidates.push(Candidate {
                name: name.clone(),
                span: pattern.span,
                outer_id: outer.id,
                outer_span: outer.span,
            });
        }
        if let Some(current) = self.scopes.last_mut() {
            current.push(Binding {
                name,
                id: pattern.id,
                span: pattern.span,
                mutable: matches!(mutability, Mutability::Mutable),
            });
        }
    }
}

impl Visitor for Walk<'_> {
    fn visit_block(&mut self, block: &Block) {
        self.scopes.push(Vec::new());
        walk_block(self, block);
        self.scopes.pop();
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let StmtKind::Let { pattern, init, .. } = &stmt.kind {
            if let Some(init) = init {
                self.visit_expr(init);
            }
            self.visit_pattern(pattern);
            self.declare(pattern);
            return;
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Assign { place, .. } => self.note_written(place),
            ExprKind::MethodCall { receiver, .. } => self.note_written(receiver),
            ExprKind::Unary {
                op: UnaryOp::RefMut,
                operand,
            } => self.note_written(operand),
            _ => {}
        }
        walk_expr(self, expr);
    }
}
