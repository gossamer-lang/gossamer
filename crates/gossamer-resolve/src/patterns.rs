//! Settles each single-name pattern into the form its name resolved to, and
//! spells each reference to an imported variant from the unit root.
//!
//! The parser reads a lone name in a pattern by its case, but a name means
//! what it resolves to: one naming a unit variant, a struct, or a const
//! matches that value, and any other binds a fresh local. The resolver
//! decides which while it binds, and this pass rewrites the patterns it
//! decided against the parser's reading, so everything after resolution
//! sees the pattern the name means. Every tier reads a bare variant by its
//! name, which another enum may also declare, so an imported variant is
//! respelled as the `crate::` path of the one the import named.

#![forbid(unsafe_code)]

use gossamer_ast::visitor::{VisitorMut, walk_expr_mut, walk_item_mut, walk_pattern_mut};
use gossamer_ast::{
    Expr, ExprKind, Ident, Item, Mutability, NodeId, PathSegment, Pattern, PatternKind, SourceFile,
    TypePath, TypePathSegment,
};

use crate::resolutions::{Resolution, Resolutions};

/// Rewrites each single-name pattern of `sf` into the binding or path
/// pattern [`Resolutions`] records for it.
pub fn settle_single_name_patterns(sf: &mut SourceFile, resolutions: &Resolutions) {
    let mut settle = Settle { resolutions };
    settle.visit_source_file(sf);
}

struct Settle<'a> {
    resolutions: &'a Resolutions,
}

impl Settle<'_> {
    /// The `crate::` path of the definition the reference at `node` names,
    /// when resolution marked it for respelling.
    fn root_path(&self, node: NodeId) -> Option<Vec<String>> {
        if !self.resolutions.is_spelled_from_root(node) {
            return None;
        }
        let Some(Resolution::Def { def, .. }) = self.resolutions.get(node) else {
            return None;
        };
        let home = self.resolutions.path_of(def)?;
        Some(
            std::iter::once("crate".to_string())
                .chain(home.iter().cloned())
                .collect(),
        )
    }
}

impl VisitorMut for Settle<'_> {
    // The resolver leaves an inactive item unresolved.
    fn visit_item(&mut self, item: &mut Item) {
        if gossamer_ast::cfg::item_is_active(&item.attrs) {
            walk_item_mut(self, item);
        }
    }

    fn visit_expr(&mut self, expr: &mut Expr) {
        if let ExprKind::Path(path) | ExprKind::Struct { path, .. } = &mut expr.kind
            && let Some(home) = self.root_path(expr.id)
        {
            let last = path.segments.pop();
            path.segments = home.iter().map(PathSegment::new).collect();
            if let (Some(written), Some(item)) = (last, path.segments.last_mut()) {
                item.generics = written.generics;
            }
        }
        walk_expr_mut(self, expr);
    }

    fn visit_pattern(&mut self, pattern: &mut Pattern) {
        if let PatternKind::Path(path)
        | PatternKind::TupleStruct { path, .. }
        | PatternKind::Struct { path, .. } = &mut pattern.kind
            && let Some(home) = self.root_path(pattern.id)
        {
            let last = path.segments.pop();
            path.segments = home.iter().map(TypePathSegment::new).collect();
            if let (Some(written), Some(item)) = (last, path.segments.last_mut()) {
                item.generics = written.generics;
            }
        }
        let recorded = self.resolutions.get(pattern.id);
        match &pattern.kind {
            PatternKind::Ident {
                name,
                mutability: Mutability::Immutable,
                subpattern: None,
            } if recorded.is_some_and(|r| !matches!(r, Resolution::Local(_))) => {
                pattern.kind = PatternKind::Path(TypePath::single(name.name.clone()));
            }
            PatternKind::Path(path)
                if path.segments.len() == 1 && recorded == Some(Resolution::Local(pattern.id)) =>
            {
                pattern.kind = PatternKind::Ident {
                    mutability: Mutability::Immutable,
                    name: Ident::new(path.segments[0].name.name.clone()),
                    subpattern: None,
                };
            }
            _ => {}
        }
        walk_pattern_mut(self, pattern);
    }
}
