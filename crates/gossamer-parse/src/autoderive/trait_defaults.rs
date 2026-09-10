// Materializes trait default method bodies into the impls that inherit them.

/// Gives every `impl Trait for Type` its own copy of the default-bodied
/// trait methods it does not override.
///
/// A default body is written once against `Self` and belongs to each
/// implementor; copying it here is what makes it an ordinary method for
/// every pass below, so a call reaches the same dispatch a written method
/// reaches on every tier.
pub(crate) fn materialize_trait_defaults(sf: &mut SourceFile) {
    let defaults = collect_trait_defaults(&sf.items);
    if defaults.is_empty() {
        return;
    }
    let mut next = sf.next_node_id;
    fill_items(&mut sf.items, &defaults, &mut next);
    sf.next_node_id = next;
}

/// Assigns every node in a copied body an identifier of its own.
///
/// Per-node tables - the type of an expression, the impl block a method call
/// names - are keyed by identifier, so two impls sharing one body's ids would
/// share one entry, and the last one checked would answer for both.
struct RenumberNodes<'a> {
    next: &'a mut u32,
}

impl RenumberNodes<'_> {
    fn id(&mut self) -> NodeId {
        let id = NodeId::from_raw(*self.next);
        *self.next = self.next.saturating_add(1);
        id
    }
}

impl gossamer_ast::visitor::VisitorMut for RenumberNodes<'_> {
    fn visit_expr(&mut self, expr: &mut gossamer_ast::Expr) {
        expr.id = self.id();
        gossamer_ast::visitor::walk_expr_mut(self, expr);
    }

    fn visit_stmt(&mut self, stmt: &mut gossamer_ast::Stmt) {
        stmt.id = self.id();
        gossamer_ast::visitor::walk_stmt_mut(self, stmt);
    }

    fn visit_pattern(&mut self, pattern: &mut gossamer_ast::Pattern) {
        pattern.id = self.id();
        gossamer_ast::visitor::walk_pattern_mut(self, pattern);
    }

    fn visit_type(&mut self, ty: &mut gossamer_ast::Type) {
        ty.id = self.id();
        gossamer_ast::visitor::walk_type_mut(self, ty);
    }

    fn visit_item(&mut self, item: &mut Item) {
        item.id = self.id();
        gossamer_ast::visitor::walk_item_mut(self, item);
    }
}

fn renumber_impl_item(item: &mut ImplItem, next: &mut u32) {
    use gossamer_ast::visitor::VisitorMut as _;
    let mut renumber = RenumberNodes { next };
    match item {
        ImplItem::Fn(decl) => {
            for param in &mut decl.params {
                if let gossamer_ast::FnParam::Typed { pattern, ty, .. } = param {
                    renumber.visit_pattern(pattern);
                    renumber.visit_type(ty);
                }
            }
            if let Some(ret) = &mut decl.ret {
                renumber.visit_type(ret);
            }
            if let Some(body) = &mut decl.body {
                renumber.visit_expr(body);
            }
        }
        ImplItem::Type { .. } | ImplItem::Const { .. } => {}
    }
}

/// Default-bodied methods of every trait in the program, keyed by trait name.
///
/// A name two modules both declare is ambiguous from an impl header alone,
/// so it carries no defaults rather than the wrong module's.
fn collect_trait_defaults(items: &[Item]) -> HashMap<String, Vec<FnDecl>> {
    let mut found: HashMap<String, Option<Vec<FnDecl>>> = HashMap::new();
    collect_into(items, &mut found);
    found
        .into_iter()
        .filter_map(|(name, methods)| {
            let methods = methods?;
            (!methods.is_empty()).then_some((name, methods))
        })
        .collect()
}

fn collect_into(items: &[Item], found: &mut HashMap<String, Option<Vec<FnDecl>>>) {
    for item in items {
        match &item.kind {
            ItemKind::Trait(decl) => {
                let methods: Vec<FnDecl> = decl
                    .items
                    .iter()
                    .filter_map(|trait_item| match trait_item {
                        TraitItem::Fn(f) if f.body.is_some() => Some(f.clone()),
                        _ => None,
                    })
                    .collect();
                found
                    .entry(decl.name.name.clone())
                    .and_modify(|slot| *slot = None)
                    .or_insert(Some(methods));
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    collect_into(inner, found);
                }
            }
            _ => {}
        }
    }
}

fn fill_items(items: &mut [Item], defaults: &HashMap<String, Vec<FnDecl>>, next: &mut u32) {
    for item in items {
        match &mut item.kind {
            ItemKind::Impl(decl) => {
                let Some(bound) = &decl.trait_ref else { continue };
                let Some(segment) = bound.path.segments.last() else {
                    continue;
                };
                let Some(methods) = defaults.get(segment.name.name.as_str()) else {
                    continue;
                };
                let defined: Vec<String> = decl
                    .items
                    .iter()
                    .filter_map(|impl_item| match impl_item {
                        ImplItem::Fn(f) => Some(f.name.name.clone()),
                        _ => None,
                    })
                    .collect();
                for method in methods {
                    if !defined.contains(&method.name.name) {
                        let mut copy = ImplItem::Fn(method.clone());
                        renumber_impl_item(&mut copy, next);
                        decl.items.push(copy);
                    }
                }
            }
            ItemKind::Mod(mod_decl) => {
                if let ModBody::Inline(inner) = &mut mod_decl.body {
                    fill_items(inner, defaults, next);
                }
            }
            _ => {}
        }
    }
}
