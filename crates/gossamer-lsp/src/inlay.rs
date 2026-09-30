//! Inlay-hint collection.
//!
//! Walks every fn body in the open document and emits one hint per
//! binding whose type the user did **not** spell out - `let` bindings
//! and closure params without an explicit `: T` annotation. Each hint
//! anchors at the end of the binding pattern's span so the editor
//! renders ` : <type>` ghost text right after the name.
//!
//! Resolved types come from [`gossamer_types::TypeTable`] keyed by
//! the pattern's `NodeId`. Unresolved inference variables (rendered
//! as `?N`) and the unit type are filtered out - they add noise
//! without telling the user anything actionable.
//!
//! The client sends `textDocument/inlayHint` with a range; we honour
//! it by skipping hints whose anchor falls outside the range.

#![forbid(unsafe_code)]

use gossamer_ast::{
    ArrayExpr, Block, ClosureParam, Expr, ExprKind, FnDecl, ImplItem, Item, ItemKind, MatchArm,
    ModBody, Pattern, PatternKind, SelectArm, Stmt, StmtKind, TraitItem,
};
use gossamer_types::{TyCtxt, TypeTable, render_public_ty, render_ty};

use crate::session::DocumentAnalysis;

/// One inlay hint scheduled for emission.
pub(crate) struct InlayHint {
    /// 0-based line of the anchor position (LSP coordinates).
    pub line: u32,
    /// 0-based UTF-16 character (we approximate as bytes - see
    /// `position_to_offset` for the existing convention).
    pub character: u32,
    /// Text shown to the user, including the leading `:` separator.
    pub label: String,
}

/// Collects every inlay hint inside `doc`. `range` is the byte
/// half-open `[start, end)` window the client asked about; pass
/// `None` for "the whole document".
pub(crate) fn collect_inlays(doc: &DocumentAnalysis, range: Option<(u32, u32)>) -> Vec<InlayHint> {
    let mut out = Vec::new();
    let mut walker = Walker {
        doc,
        types: &doc.types,
        tcx: &doc.tcx,
        range,
        out: &mut out,
    };
    for item in &doc.sf.items {
        walker.walk_item(item);
    }
    out
}

struct Walker<'a> {
    doc: &'a DocumentAnalysis,
    types: &'a TypeTable,
    tcx: &'a TyCtxt,
    range: Option<(u32, u32)>,
    out: &'a mut Vec<InlayHint>,
}

impl Walker<'_> {
    fn walk_item(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Fn(decl) => self.walk_fn(decl),
            ItemKind::Impl(decl) => {
                for impl_item in &decl.items {
                    if let ImplItem::Fn(inner) = impl_item {
                        self.walk_fn(inner);
                    }
                }
            }
            ItemKind::Trait(decl) => {
                for trait_item in &decl.items {
                    if let TraitItem::Fn(inner) = trait_item {
                        self.walk_fn(inner);
                    }
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(items) = &decl.body {
                    for nested in items {
                        self.walk_item(nested);
                    }
                }
            }
            ItemKind::Const(_)
            | ItemKind::Static(_)
            | ItemKind::Struct(_)
            | ItemKind::Enum(_)
            | ItemKind::TypeAlias(_)
            | ItemKind::AttrItem(_) => {}
        }
    }

    fn walk_fn(&mut self, decl: &FnDecl) {
        if let Some(body) = &decl.body {
            self.walk_expr(body);
        }
    }

    // One arm per ExprKind variant - splitting it would split the
    // single match in half without making any branch shorter.
    #[allow(
        clippy::too_many_lines,
        reason = "flat-shape dispatch / lowering - splitting hides the per-arm intent"
    )]
    fn walk_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Block(block) | ExprKind::Unsafe(block) => self.walk_block(block),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.walk_expr(condition);
                self.walk_expr(then_branch);
                if let Some(else_branch) = else_branch {
                    self.walk_expr(else_branch);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee);
                for arm in arms {
                    self.walk_match_arm(arm);
                }
            }
            ExprKind::Loop { body, .. } | ExprKind::While { body, .. } => self.walk_expr(body),
            ExprKind::For {
                pattern,
                iter,
                body,
                ..
            } => {
                self.walk_expr(iter);
                self.emit_pattern_hint(pattern);
                self.walk_expr(body);
            }
            ExprKind::Closure { params, body, .. } => self.walk_closure(params, body),
            ExprKind::Return(inner) => {
                if let Some(inner) = inner {
                    self.walk_expr(inner);
                }
            }
            ExprKind::Break { value, .. } => {
                if let Some(value) = value {
                    self.walk_expr(value);
                }
            }
            ExprKind::Call { callee, args } => {
                self.walk_expr(callee);
                for arg in args {
                    self.walk_expr(arg);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.walk_expr(receiver);
                for arg in args {
                    self.walk_expr(arg);
                }
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.walk_expr(lhs);
                self.walk_expr(rhs);
            }
            ExprKind::Unary { operand, .. } => self.walk_expr(operand),
            ExprKind::Cast { value, .. } | ExprKind::Try(value) => self.walk_expr(value),
            ExprKind::FieldAccess { receiver, .. } => self.walk_expr(receiver),
            ExprKind::Index { base, index } => {
                self.walk_expr(base);
                self.walk_expr(index);
            }
            ExprKind::Tuple(parts) | ExprKind::MapLiteral(parts) | ExprKind::SetLiteral(parts) => {
                for part in parts {
                    self.walk_expr(part);
                }
            }
            ExprKind::Struct { fields, base, .. } => {
                for f in fields {
                    if let Some(value) = &f.value {
                        self.walk_expr(value);
                    }
                }
                if let Some(base) = base {
                    self.walk_expr(base);
                }
            }
            ExprKind::Array(arr) | ExprKind::FixedArray(arr) => match arr {
                ArrayExpr::List(elems) => {
                    for elem in elems {
                        self.walk_expr(elem);
                    }
                }
                ArrayExpr::Repeat { value, count } => {
                    self.walk_expr(value);
                    self.walk_expr(count);
                }
            },
            ExprKind::Assign { place, value, .. } => {
                self.walk_expr(place);
                self.walk_expr(value);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(start) = start {
                    self.walk_expr(start);
                }
                if let Some(end) = end {
                    self.walk_expr(end);
                }
            }
            ExprKind::Select(arms) => {
                for arm in arms {
                    self.walk_select_arm(arm);
                }
            }
            ExprKind::Literal(_)
            | ExprKind::Path(_)
            | ExprKind::Continue { .. }
            | ExprKind::Error => {}
        }
    }

    fn walk_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.walk_stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.walk_expr(tail);
        }
    }

    fn walk_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { pattern, ty, init } => {
                if let Some(init) = init {
                    self.walk_expr(init);
                }
                if ty.is_none() {
                    self.emit_pattern_hint(pattern);
                }
            }
            StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => {
                self.walk_expr(expr);
            }
            StmtKind::Item(item) => self.walk_item(item),
        }
    }

    fn walk_match_arm(&mut self, arm: &MatchArm) {
        if let Some(guard) = &arm.guard {
            self.walk_expr(guard);
        }
        self.walk_expr(&arm.body);
    }

    fn walk_select_arm(&mut self, arm: &SelectArm) {
        // SelectOp variants carry sub-exprs (send / recv) but those
        // bindings always come with explicit channel-typed
        // expressions, so the inlay value-add is low. Walk the body
        // for any nested closures or lets.
        self.walk_expr(&arm.body);
    }

    fn walk_closure(&mut self, params: &[ClosureParam], body: &Expr) {
        for param in params {
            if param.ty.is_none() {
                self.emit_pattern_hint(&param.pattern);
            }
        }
        self.walk_expr(body);
    }

    fn emit_pattern_hint(&mut self, pattern: &Pattern) {
        if !matches!(
            pattern.kind,
            PatternKind::Ident { .. } | PatternKind::Tuple(_)
        ) {
            return;
        }
        let Some(ty) = self.types.get(pattern.id) else {
            return;
        };
        let rendered = render_ty(self.tcx, ty);
        if !worth_showing(&rendered) {
            return;
        }
        if !self.in_range(pattern.span.end) {
            return;
        }
        let (line, character) = self.doc.offset_to_position(pattern.span.end);
        self.out.push(InlayHint {
            line,
            character,
            label: format!(": {}", render_public_ty(self.tcx, ty)),
        });
    }

    fn in_range(&self, offset: u32) -> bool {
        match self.range {
            None => true,
            Some((start, end)) => offset >= start && offset < end,
        }
    }
}

/// A hint is worth showing only when the rendered type carries
/// information the user didn't already see at the binding site:
/// no unresolved inference variables, no `<error>`, and we suppress
/// the unit type (`let _ = side_effect()` doesn't need a `: ()`
/// annotation in everybody's editor).
fn worth_showing(rendered: &str) -> bool {
    if rendered.is_empty() {
        return false;
    }
    if rendered == "()" || rendered == "<error>" {
        return false;
    }
    if rendered.contains('?') {
        return false;
    }
    true
}
