//! Imports the toolchain adds for the code it writes, kept out of the
//! program's own reach.
//!
//! Generated serde stubs, comparators, and standard-library wrappers name
//! `json::`, `errors::`, `runtime::`, and other modules, so the toolchain
//! imports those modules for them. The program's own text still imports
//! what it names: a path there whose first segment only such an import
//! binds is refused as if the import were absent, whatever code the
//! toolchain happened to add beside it.

use std::collections::{HashMap, HashSet};

use gossamer_ast::visitor::{Visitor, walk_expr, walk_item, walk_type};
use gossamer_ast::{Expr, ExprKind, Item, ItemKind, NodeId, SourceFile, Type, TypeKind, UseTarget};
use gossamer_lex::Span;

/// Each program-text path whose first segment is a module only a synthetic
/// import binds in the scope it is written in, with that segment's name.
#[must_use]
pub(crate) fn program_reaches_through_synthetic_imports(
    source: &SourceFile,
    local_modules: &HashSet<String>,
) -> Vec<(String, Span)> {
    let mut synthetic: HashMap<String, HashSet<String>> = HashMap::new();
    let mut written: HashMap<String, HashSet<String>> = HashMap::new();
    for use_decl in &source.uses {
        let last = match &use_decl.target {
            UseTarget::Module(path) => path.segments.last().map(|s| s.name.clone()),
            UseTarget::Project { .. } => None,
        };
        // A grouped import binds each entry's name, and `self` in it the
        // module the group is written under.
        let names: Vec<String> = match &use_decl.list {
            None => use_decl
                .alias
                .as_ref()
                .map(|alias| alias.name.clone())
                .or(last)
                .into_iter()
                .collect(),
            Some(entries) => entries
                .iter()
                .filter_map(|entry| match &entry.alias {
                    Some(alias) => Some(alias.name.clone()),
                    None if entry.name.name == "self" && entry.prefix.is_empty() => last.clone(),
                    None if entry.name.name == "self" => {
                        entry.prefix.last().map(|segment| segment.name.clone())
                    }
                    None => Some(entry.name.name.clone()),
                })
                .collect(),
        };
        let scope = use_decl.module.join("::");
        let bound = if use_decl.synthetic {
            &mut synthetic
        } else {
            &mut written
        };
        bound.entry(scope).or_default().extend(names);
    }
    if synthetic.is_empty() {
        return Vec::new();
    }
    let mut walk = Walk {
        source,
        local_modules,
        synthetic: &synthetic,
        written: &written,
        desugared: source.desugared_paths.iter().copied().collect(),
        module: Vec::new(),
        out: Vec::new(),
    };
    walk.visit_source_file(source);
    walk.out
}

struct Walk<'a> {
    source: &'a SourceFile,
    local_modules: &'a HashSet<String>,
    synthetic: &'a HashMap<String, HashSet<String>>,
    written: &'a HashMap<String, HashSet<String>>,
    desugared: HashSet<NodeId>,
    module: Vec<String>,
    out: Vec<(String, Span)>,
}

impl Walk<'_> {
    /// Whether `head`, written in the current module, reaches a module only
    /// through a synthetic import.
    fn only_synthetic(&self, head: &str) -> bool {
        if self.local_modules.contains(head) {
            return false;
        }
        let scopes = (0..=self.module.len()).map(|depth| self.module[..depth].join("::"));
        let mut synthetic = false;
        for scope in scopes {
            if self
                .written
                .get(&scope)
                .is_some_and(|names| names.contains(head))
            {
                return false;
            }
            synthetic |= self
                .synthetic
                .get(&scope)
                .is_some_and(|names| names.contains(head));
        }
        synthetic
    }

    fn check(&mut self, id: NodeId, span: Span, head: Option<&str>) {
        let Some(head) = head else {
            return;
        };
        if self.source.is_program_text(span)
            && !self.desugared.contains(&id)
            && self.only_synthetic(head)
        {
            self.out.push((head.to_string(), span));
        }
    }
}

impl Visitor for Walk<'_> {
    fn visit_item(&mut self, item: &Item) {
        // An item under the reserved `__gos_` prefix is the toolchain's,
        // wherever its span places it.
        if item_name(item).is_some_and(|name| name.starts_with("__gos_")) {
            return;
        }
        if let ItemKind::Mod(decl) = &item.kind {
            self.module.push(decl.name.name.clone());
            walk_item(self, item);
            self.module.pop();
            return;
        }
        walk_item(self, item);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let ExprKind::Path(path) = &expr.kind
            && path.segments.len() > 1
        {
            let head = path.segments.first().map(|s| s.name.name.as_str());
            self.check(expr.id, expr.span, head);
        }
        walk_expr(self, expr);
    }

    fn visit_type(&mut self, ty: &Type) {
        if let TypeKind::Path(path) = &ty.kind
            && path.segments.len() > 1
        {
            let head = path.segments.first().map(|s| s.name.name.as_str());
            self.check(ty.id, ty.span, head);
        }
        walk_type(self, ty);
    }
}

/// The name an item declares, when it declares one.
fn item_name(item: &Item) -> Option<&str> {
    Some(match &item.kind {
        ItemKind::Fn(decl) => decl.name.name.as_str(),
        ItemKind::Struct(decl) => decl.name.name.as_str(),
        ItemKind::Enum(decl) => decl.name.name.as_str(),
        ItemKind::Mod(decl) => decl.name.name.as_str(),
        _ => return None,
    })
}
