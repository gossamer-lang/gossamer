//! A `sync::Shared` written with a value read from it under a separate lock.
//!
//! `counter.set(counter.get() + 1)` takes the lock for the `get`, releases
//! it, and takes it again for the `set`, so a write another goroutine makes
//! in between is overwritten and lost. `counter.update(|n| n + 1)` holds the
//! lock across the read and the write. A `set` whose argument reads the same
//! handle, through `get` or `with`, is reported; the handle is the same when
//! both name one binding, or one field path below it.

#![forbid(unsafe_code)]

use gossamer_ast::visitor::{Visitor, walk_expr};
use gossamer_ast::{Expr, ExprKind, FieldSelector, SourceFile};
use gossamer_lex::Span;
use gossamer_resolve::Resolutions;

use crate::context::TyCtxt;
use crate::table::TypeTable;
use crate::ty::TyKind;

/// The lint these findings belong to.
pub const SPLIT_SHARED_UPDATE_LINT: &str = "split_shared_update";

/// One `set` of a `sync::Shared` fed by a read of the same handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitSharedUpdate {
    /// The `set` call.
    pub span: Span,
    /// The handle as written, `counter` or `self.hits`.
    pub handle: String,
    /// The read that feeds the write: `get` or `with`.
    pub read: String,
}

/// Every `set` in `sf` whose argument reads the same `sync::Shared` it writes.
#[must_use]
pub fn find_split_shared_updates(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<SplitSharedUpdate> {
    let mut finder = Finder {
        resolutions,
        table,
        tcx,
        out: Vec::new(),
    };
    finder.visit_source_file(sf);
    finder.out
}

struct Finder<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    out: Vec<SplitSharedUpdate>,
}

impl Finder<'_> {
    fn is_shared(&self, expr: &Expr) -> bool {
        self.table.get(expr.id).is_some_and(|ty| {
            matches!(self.tcx.kind(ty), Some(TyKind::Adt { def, .. })
                if self.tcx.def_name(*def) == Some("sync::Shared"))
        })
    }

    /// The identity of the handle `expr` names: its root binding's
    /// resolution and the field path below it, or `None` for an expression
    /// that names no place (a call, a literal).
    fn handle_key(&self, expr: &Expr) -> Option<String> {
        match &expr.kind {
            ExprKind::Path(_) => self
                .resolutions
                .get(expr.id)
                .map(|resolution| format!("{resolution:?}")),
            ExprKind::FieldAccess { receiver, field } => {
                let root = self.handle_key(receiver)?;
                Some(match field {
                    FieldSelector::Named(name) => format!("{root}.{}", name.name),
                    FieldSelector::Index(index) => format!("{root}.{index}"),
                })
            }
            _ => None,
        }
    }

    /// The first read of the handle keyed `key` inside `expr`.
    fn read_of(&self, expr: &Expr, key: &str) -> Option<String> {
        let mut search = ReadSearch {
            finder: self,
            key,
            found: None,
        };
        search.visit_expr(expr);
        search.found
    }
}

impl Visitor for Finder<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if let ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &expr.kind
            && name.name == "set"
            && self.is_shared(receiver)
            && let Some(key) = self.handle_key(receiver)
            && let Some(read) = args.iter().find_map(|arg| self.read_of(arg, &key))
        {
            self.out.push(SplitSharedUpdate {
                span: expr.span,
                handle: handle_text(receiver),
                read,
            });
        }
        walk_expr(self, expr);
    }
}

struct ReadSearch<'f, 'a> {
    finder: &'f Finder<'a>,
    key: &'f str,
    found: Option<String>,
}

impl Visitor for ReadSearch<'_, '_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if self.found.is_some() {
            return;
        }
        if let ExprKind::MethodCall { receiver, name, .. } = &expr.kind
            && matches!(name.name.as_str(), "get" | "with")
            && self.finder.is_shared(receiver)
            && self.finder.handle_key(receiver).as_deref() == Some(self.key)
        {
            self.found = Some(name.name.clone());
            return;
        }
        walk_expr(self, expr);
    }
}

/// The handle as the source spells it: a path, or a field path below one.
fn handle_text(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Path(path) => path
            .segments
            .iter()
            .map(|segment| segment.name.name.as_str())
            .collect::<Vec<_>>()
            .join("::"),
        ExprKind::FieldAccess { receiver, field } => match field {
            FieldSelector::Named(name) => format!("{}.{}", handle_text(receiver), name.name),
            FieldSelector::Index(index) => format!("{}.{index}", handle_text(receiver)),
        },
        _ => "the handle".to_string(),
    }
}
