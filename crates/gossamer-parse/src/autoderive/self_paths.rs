// Inside an `impl` block `Self` names the implementing type. A value path, a
// struct literal, or a pattern headed by it is rewritten to that type's own
// name, so every pass below reaches `Self::origin()`, `Self { .. }`, and
// `Self::Variant(x)` exactly as it reaches the same item written by name.

/// Rewrites the `Self` head of expression paths, struct literals, and patterns
/// inside every `impl` block to the implementing type's name.
pub(crate) fn rewrite_self_paths(sf: &mut SourceFile) {
    rewrite_self_in_items(&mut sf.items);
}

fn rewrite_self_in_items(items: &mut [Item]) {
    use gossamer_ast::VisitorMut as _;
    for item in items {
        match &mut item.kind {
            ItemKind::Impl(decl) => {
                let Some(head) = gossamer_ast::assoc::type_head_name(&decl.self_ty) else {
                    continue;
                };
                if head == "Self" {
                    continue;
                }
                let mut rewriter = SelfHeadRewriter {
                    head: head.to_string(),
                };
                for impl_item in &mut decl.items {
                    match impl_item {
                        ImplItem::Fn(decl) => {
                            if let Some(body) = &mut decl.body {
                                rewriter.visit_expr(body);
                            }
                        }
                        ImplItem::Const { value, .. } => rewriter.visit_expr(value),
                        ImplItem::Type { .. } => {}
                    }
                }
            }
            ItemKind::Mod(mod_decl) => {
                if let ModBody::Inline(inner) = &mut mod_decl.body {
                    rewrite_self_in_items(inner);
                }
            }
            _ => {}
        }
    }
}

/// Renames a leading `Self` segment to the implementing type's name.
struct SelfHeadRewriter {
    head: String,
}

impl SelfHeadRewriter {
    fn rename_expr_path(&self, path: &mut gossamer_ast::PathExpr) {
        if let Some(first) = path.segments.first_mut()
            && first.name.name == "Self"
        {
            first.name.name.clone_from(&self.head);
        }
    }

    fn rename_type_path(&self, path: &mut gossamer_ast::TypePath) {
        if let Some(first) = path.segments.first_mut()
            && first.name.name == "Self"
        {
            first.name.name.clone_from(&self.head);
        }
    }
}

impl gossamer_ast::VisitorMut for SelfHeadRewriter {
    fn visit_expr(&mut self, expr: &mut gossamer_ast::Expr) {
        gossamer_ast::visitor::walk_expr_mut(self, expr);
        match &mut expr.kind {
            gossamer_ast::ExprKind::Path(path) | gossamer_ast::ExprKind::Struct { path, .. } => {
                self.rename_expr_path(path);
            }
            _ => {}
        }
    }

    fn visit_pattern(&mut self, pattern: &mut gossamer_ast::Pattern) {
        gossamer_ast::visitor::walk_pattern_mut(self, pattern);
        match &mut pattern.kind {
            gossamer_ast::PatternKind::Path(path)
            | gossamer_ast::PatternKind::Struct { path, .. }
            | gossamer_ast::PatternKind::TupleStruct { path, .. } => self.rename_type_path(path),
            _ => {}
        }
    }
}
