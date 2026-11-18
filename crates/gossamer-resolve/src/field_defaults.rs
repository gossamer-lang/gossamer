//! Fills the fields a struct literal leaves out with their declared defaults.
//!
//! `struct S { limit: i64, result: u32 = 0 }` lets `S { limit: 10 }` stand for
//! `S { limit: 10, result: 0 }`. The parser writes each default into a
//! function of its own beside the type (`__gos_fd__S__result`); this pass runs
//! right after resolution and writes a call to that function into every
//! literal that leaves the field out, so the checker, HIR, and every tier see
//! a complete literal and nothing after it can tell the two spellings apart.
//! Each construction calls the function, so a default that builds a value
//! builds one per construction, and the default resolves and checks once,
//! where the type is declared.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use gossamer_ast::visitor::{Visitor, VisitorMut, walk_expr, walk_expr_mut, walk_item_mut};
use gossamer_ast::{
    Expr, ExprKind, Item, ItemKind, ModBody, NodeId, NodeIdGenerator, PathExpr, SourceFile,
    StructBody, StructExprField, StructField, field_default_fn_name,
};

use crate::diagnostic::{ResolveDiagnostic, ResolveError};
use crate::named_args::target_def;
use crate::resolutions::{Resolution, Resolutions};
use crate::{DefId, DefKind};

/// The default function of each defaulted field, in declared order.
type Defaults = Vec<(String, DefId)>;

/// Every struct and struct-shaped variant whose fields declare a default.
#[derive(Default)]
struct DefaultTable {
    structs: HashMap<DefId, Defaults>,
    variants: HashMap<(DefId, String), Defaults>,
    /// The defaults themselves, for the check that they read no sibling.
    declared: Vec<StructField>,
}

impl DefaultTable {
    fn collect(&mut self, items: &[Item], resolutions: &Resolutions) {
        let active = || {
            items
                .iter()
                .filter(|item| gossamer_ast::cfg::item_is_active(&item.attrs))
        };
        let fns: HashMap<&str, DefId> = active()
            .filter_map(|item| match &item.kind {
                ItemKind::Fn(decl) => {
                    Some((decl.name.name.as_str(), resolutions.definition_of(item.id)?))
                }
                _ => None,
            })
            .collect();
        let defaults = |owner: &str, variant: Option<&str>, fields: &[StructField]| {
            fields
                .iter()
                .filter(|field| field.default.is_some())
                .filter_map(|field| {
                    let name = field_default_fn_name(owner, variant, &field.name.name);
                    Some((field.name.name.clone(), *fns.get(name.as_str())?))
                })
                .collect::<Defaults>()
        };
        for item in active() {
            match &item.kind {
                ItemKind::Struct(decl) => {
                    let Some(def) = resolutions.definition_of(item.id) else {
                        continue;
                    };
                    if let StructBody::Named(fields) = &decl.body {
                        let found = defaults(&decl.name.name, None, fields);
                        if !found.is_empty() {
                            self.structs.insert(def, found);
                            self.declared.extend(fields.iter().cloned());
                        }
                    }
                }
                ItemKind::Enum(decl) => {
                    let Some(def) = resolutions.definition_of(item.id) else {
                        continue;
                    };
                    for variant in &decl.variants {
                        if let StructBody::Named(fields) = &variant.body {
                            let found = defaults(&decl.name.name, Some(&variant.name.name), fields);
                            if !found.is_empty() {
                                self.variants
                                    .insert((def, variant.name.name.clone()), found);
                                self.declared.extend(fields.iter().cloned());
                            }
                        }
                    }
                }
                ItemKind::Mod(decl) => {
                    if let ModBody::Inline(inner) = &decl.body {
                        self.collect(inner, resolutions);
                    }
                }
                _ => {}
            }
        }
    }

    /// The defaults of the struct or variant a literal names. `owner` is the
    /// definition the literal's path resolves to: the struct itself, or the
    /// enum of a variant, which the path's last segment then names.
    fn defaults_of(&self, owner: DefId, last: &str) -> Option<&Defaults> {
        self.structs
            .get(&owner)
            .or_else(|| self.variants.get(&(owner, last.to_string())))
    }
}

/// Fills every struct literal's omitted defaulted fields and reports each
/// default that reads `self` or another field of its struct.
pub fn fill_field_defaults(
    sf: &mut SourceFile,
    resolutions: &mut Resolutions,
) -> Vec<ResolveDiagnostic> {
    let mut table = DefaultTable::default();
    table.collect(&sf.items, resolutions);
    if table.structs.is_empty() && table.variants.is_empty() {
        return Vec::new();
    }
    let diagnostics = reads_of_siblings(&std::mem::take(&mut table.declared), resolutions);
    let mut ids = NodeIdGenerator::new();
    while ids.issued() < sf.next_node_id {
        let _ = ids.next();
    }
    let mut fill = Fill {
        table,
        resolutions,
        ids: &mut ids,
        self_owner: Vec::new(),
    };
    fill.visit_source_file(sf);
    sf.next_node_id = ids.issued();
    diagnostics
}

/// Each place a default names `self` or another field of its struct. The
/// resolver binds those names to a placeholder local while it resolves the
/// defaults, since a default is evaluated before the value it belongs to
/// exists.
fn reads_of_siblings(fields: &[StructField], resolutions: &Resolutions) -> Vec<ResolveDiagnostic> {
    struct Reads<'a> {
        resolutions: &'a Resolutions,
        found: Vec<(String, gossamer_lex::Span)>,
    }
    impl Visitor for Reads<'_> {
        fn visit_expr(&mut self, expr: &Expr) {
            if let ExprKind::Path(path) = &expr.kind
                && let [segment] = path.segments.as_slice()
                && self.resolutions.get(expr.id) == Some(Resolution::Local(NodeId::DUMMY))
            {
                self.found.push((segment.name.name.clone(), expr.span));
            }
            walk_expr(self, expr);
        }
    }
    let mut out = Vec::new();
    for field in fields {
        let Some(default) = &field.default else {
            continue;
        };
        let mut reads = Reads {
            resolutions,
            found: Vec::new(),
        };
        reads.visit_expr(default);
        out.extend(reads.found.into_iter().map(|(name, span)| {
            ResolveDiagnostic::new(
                ResolveError::FieldDefaultReadsValue {
                    field: field.name.name.clone(),
                    name,
                },
                span,
            )
        }));
    }
    out
}

struct Fill<'a> {
    table: DefaultTable,
    resolutions: &'a mut Resolutions,
    ids: &'a mut NodeIdGenerator,
    /// The type each enclosing `impl` block is for, innermost last, which a
    /// `Self { .. }` literal names.
    self_owner: Vec<Option<DefId>>,
}

impl VisitorMut for Fill<'_> {
    fn visit_item(&mut self, item: &mut Item) {
        if !gossamer_ast::cfg::item_is_active(&item.attrs) {
            return;
        }
        let owner = match &item.kind {
            ItemKind::Impl(decl) => Some(target_def(decl.self_ty.id, self.resolutions)),
            _ => None,
        };
        if let Some(owner) = owner {
            self.self_owner.push(owner);
            walk_item_mut(self, item);
            self.self_owner.pop();
        } else {
            walk_item_mut(self, item);
        }
    }

    fn visit_expr(&mut self, expr: &mut Expr) {
        walk_expr_mut(self, expr);
        let span = expr.span;
        let ExprKind::Struct {
            path,
            fields,
            base: None,
            ..
        } = &mut expr.kind
        else {
            return;
        };
        let Some(last) = path.segments.last().map(|s| s.name.name.clone()) else {
            return;
        };
        let owner = if path.segments.first().is_some_and(|s| s.name.name == "Self") {
            self.self_owner.last().copied().flatten()
        } else {
            target_def(expr.id, self.resolutions)
        };
        let Some(defaults) = owner.and_then(|owner| self.table.defaults_of(owner, &last)) else {
            return;
        };
        let missing: Vec<(String, DefId)> = defaults
            .iter()
            .filter(|(name, _)| !fields.iter().any(|f| f.name.name == **name))
            .map(|(name, def)| (name.clone(), *def))
            .collect();
        for (name, def) in missing {
            let callee_id = self.ids.next();
            self.resolutions.insert(
                callee_id,
                Resolution::Def {
                    def,
                    kind: DefKind::Fn,
                },
            );
            let mut callee_path = PathExpr::single(String::new());
            if let Some(home) = self.resolutions.path_of(def) {
                callee_path = PathExpr::from_names(
                    std::iter::once("crate".to_string()).chain(home.iter().cloned()),
                );
            }
            let callee = Expr::new(callee_id, span, ExprKind::Path(callee_path));
            let call = Expr::new(
                self.ids.next(),
                span,
                ExprKind::Call {
                    callee: Box::new(callee),
                    args: Vec::new(),
                },
            );
            fields.push(StructExprField::explicit(name, call));
        }
    }
}
