//! Moves the functions written in a type's body into the `impl` blocks they
//! stand for.
//!
//! `struct S: Bench { limit: i64  fn run(&mut self) { .. } }` is the same
//! declaration as `struct S { limit: i64 }` followed by
//! `impl Bench for S { fn run(&mut self) { .. } }`: a body function whose
//! name exactly one listed trait declares implements that trait, and any
//! other is an inherent method. This pass writes those blocks beside the type
//! before anything resolves, keeping every item's own spans, so no later pass
//! can tell which form the program used. A name more than one listed trait
//! declares is an error rather than a choice.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use gossamer_ast::{
    GenericArg, GenericParam, Generics, ImplDecl, ImplItem, Item, ItemKind, ModBody,
    NodeIdGenerator, SourceFile, TraitBound, TraitItem, Type, TypeKind, TypePath, TypePathSegment,
    Visibility,
};
use gossamer_lex::Span;

use crate::diagnostic::{ParseDiagnostic, ParseError};

/// Every trait the program declares: its module and the names of its items.
/// Each declaration of one trait name: its module and its item names.
type TraitHomes = Vec<(Vec<String>, HashSet<String>)>;

#[derive(Default)]
struct TraitTable {
    by_name: HashMap<String, TraitHomes>,
}

impl TraitTable {
    fn collect(&mut self, items: &[Item], module: &[String]) {
        for item in items {
            match &item.kind {
                ItemKind::Trait(decl) => {
                    let names = decl
                        .items
                        .iter()
                        .map(|item| match item {
                            TraitItem::Fn(decl) => decl.name.name.clone(),
                            TraitItem::Type { name, .. } | TraitItem::Const { name, .. } => {
                                name.name.clone()
                            }
                        })
                        .collect();
                    self.by_name
                        .entry(decl.name.name.clone())
                        .or_default()
                        .push((module.to_vec(), names));
                }
                ItemKind::Mod(decl) => {
                    if let ModBody::Inline(inner) = &decl.body {
                        let mut path = module.to_vec();
                        path.push(decl.name.name.clone());
                        self.collect(inner, &path);
                    }
                }
                _ => {}
            }
        }
    }

    /// The item names of the trait `bound` names from `module`: a trait the
    /// program declares (the one its path's modules spell, else the one in
    /// `module`, else the only one of that name), else a language trait.
    fn items_of(&self, bound: &TraitBound, module: &[String]) -> Option<HashSet<String>> {
        let segments: Vec<&str> = bound
            .path
            .segments
            .iter()
            .map(|segment| segment.name.name.as_str())
            .collect();
        let (name, qualifier) = segments.split_last()?;
        if let Some(declared) = self.by_name.get(*name) {
            let qualifier: Vec<&str> = qualifier
                .iter()
                .copied()
                .filter(|segment| !matches!(*segment, "crate" | "self" | "super"))
                .collect();
            let found = declared
                .iter()
                .find(|(home, _)| {
                    !qualifier.is_empty()
                        && home
                            .iter()
                            .map(String::as_str)
                            .eq(qualifier.iter().copied())
                })
                .or_else(|| declared.iter().find(|(home, _)| home.as_slice() == module))
                .or_else(|| (declared.len() == 1).then(|| &declared[0]));
            if let Some((_, names)) = found {
                return Some(names.clone());
            }
        }
        gossamer_ast::LANGUAGE_TRAIT_ITEMS
            .iter()
            .find(|(trait_name, _)| trait_name == name)
            .map(|(_, names)| names.iter().map(|name| (*name).to_string()).collect())
    }
}

/// Writes the `impl` blocks every type body stands for beside its type and
/// empties the body, answering one diagnostic per body function whose name
/// more than one listed trait declares.
pub fn desugar_type_bodies(sf: &mut SourceFile) -> Vec<ParseDiagnostic> {
    if !has_type_body(&sf.items) {
        return Vec::new();
    }
    let mut traits = TraitTable::default();
    traits.collect(&sf.items, &[]);
    let mut ids = NodeIdGenerator::new();
    while ids.issued() < sf.next_node_id {
        let _ = ids.next();
    }
    let mut diagnostics = Vec::new();
    let mut pass = Desugar {
        traits: &traits,
        ids: &mut ids,
        diagnostics: &mut diagnostics,
    };
    pass.items(&mut sf.items, &[]);
    sf.next_node_id = ids.issued();
    diagnostics
}

pub(crate) fn has_type_body(items: &[Item]) -> bool {
    items.iter().any(|item| match &item.kind {
        ItemKind::Struct(decl) => !decl.traits.is_empty() || !decl.items.is_empty(),
        ItemKind::Enum(decl) => !decl.traits.is_empty() || !decl.items.is_empty(),
        ItemKind::Mod(decl) => matches!(&decl.body, ModBody::Inline(inner) if has_type_body(inner)),
        _ => false,
    })
}

struct Desugar<'a> {
    traits: &'a TraitTable,
    ids: &'a mut NodeIdGenerator,
    diagnostics: &'a mut Vec<ParseDiagnostic>,
}

impl Desugar<'_> {
    fn items(&mut self, items: &mut Vec<Item>, module: &[String]) {
        let mut out = Vec::with_capacity(items.len());
        for mut item in items.drain(..) {
            if let ItemKind::Mod(decl) = &mut item.kind
                && let ModBody::Inline(inner) = &mut decl.body
            {
                let mut path = module.to_vec();
                path.push(decl.name.name.clone());
                self.items(inner, &path);
            }
            let body = match &mut item.kind {
                ItemKind::Struct(decl) => Some((
                    decl.name.name.clone(),
                    decl.generics.clone(),
                    decl.where_clause.clone(),
                    std::mem::take(&mut decl.traits),
                    std::mem::take(&mut decl.items),
                )),
                ItemKind::Enum(decl) => Some((
                    decl.name.name.clone(),
                    decl.generics.clone(),
                    decl.where_clause.clone(),
                    std::mem::take(&mut decl.traits),
                    std::mem::take(&mut decl.items),
                )),
                _ => None,
            };
            let span = item.span;
            let cfgs: Vec<_> = item
                .attrs
                .outer
                .iter()
                .filter(|attr| attr.is_named("cfg"))
                .cloned()
                .collect();
            out.push(item);
            let Some((name, generics, where_clause, listed, body_items)) = body else {
                continue;
            };
            if listed.is_empty() && body_items.is_empty() {
                continue;
            }
            for (trait_ref, items) in self.assign(&name, &listed, body_items, module) {
                let decl = ImplDecl {
                    generics: generics.clone(),
                    trait_ref,
                    self_ty: self.self_type(&name, &generics, span),
                    where_clause: where_clause.clone(),
                    items,
                };
                let mut attrs = gossamer_ast::Attrs::default();
                attrs.outer.extend(cfgs.iter().cloned());
                out.push(Item::new(
                    self.ids.next(),
                    span,
                    attrs,
                    Visibility::Inherited,
                    ItemKind::Impl(decl),
                ));
            }
        }
        *items = out;
    }

    /// The blocks a body stands for: one per listed trait, in listed order,
    /// then the inherent one when any function implements no listed trait.
    fn assign(
        &mut self,
        ty: &str,
        listed: &[TraitBound],
        body: Vec<ImplItem>,
        module: &[String],
    ) -> Vec<(Option<TraitBound>, Vec<ImplItem>)> {
        let declared: Vec<Option<HashSet<String>>> = listed
            .iter()
            .map(|bound| self.traits.items_of(bound, module))
            .collect();
        let mut per_trait: Vec<Vec<ImplItem>> = vec![Vec::new(); listed.len()];
        let mut inherent = Vec::new();
        for item in body {
            let (name, span) = impl_item_name(&item);
            let owners: Vec<usize> = declared
                .iter()
                .enumerate()
                .filter(|(_, names)| names.as_ref().is_some_and(|names| names.contains(&name)))
                .map(|(index, _)| index)
                .collect();
            match owners.as_slice() {
                [] => inherent.push(item),
                [owner] => per_trait[*owner].push(item),
                _ => {
                    self.diagnostics.push(ParseDiagnostic::new(
                        ParseError::TypeBodyAmbiguousItem {
                            ty: ty.to_string(),
                            name,
                            traits: owners.iter().map(|&i| bound_name(&listed[i])).collect(),
                        },
                        span,
                    ));
                    inherent.push(item);
                }
            }
        }
        let mut blocks: Vec<(Option<TraitBound>, Vec<ImplItem>)> =
            listed.iter().cloned().map(Some).zip(per_trait).collect();
        if !inherent.is_empty() {
            blocks.push((None, inherent));
        }
        blocks
    }

    /// The type an `impl` of `name` is for: the name applied to its own
    /// parameters, `Stack<T>` for `struct Stack<T>`.
    // A const parameter is spelled as a type path, the way the parser reads
    // `Ring<N>` in a written `impl`, so it resolves to the impl's parameter.
    fn self_type(&mut self, name: &str, generics: &Generics, span: Span) -> Type {
        let args = generics
            .params
            .iter()
            .filter_map(|param| match param {
                GenericParam::Type { name, .. } | GenericParam::Const { name, .. } => {
                    Some(GenericArg::Type(Type::new(
                        self.ids.next(),
                        span,
                        TypeKind::Path(TypePath::single(name.name.clone())),
                    )))
                }
                GenericParam::Lifetime { .. } => None,
            })
            .collect();
        let mut segment = TypePathSegment::new(name);
        segment.generics = args;
        Type::new(
            self.ids.next(),
            span,
            TypeKind::Path(TypePath {
                segments: vec![segment],
            }),
        )
    }
}

fn impl_item_name(item: &ImplItem) -> (String, Span) {
    match item {
        ImplItem::Fn(decl) => (decl.name.name.clone(), decl.span),
        ImplItem::Type { name, .. } | ImplItem::Const { name, .. } => {
            (name.name.clone(), Span::default())
        }
    }
}

fn bound_name(bound: &TraitBound) -> String {
    bound
        .path
        .segments
        .iter()
        .map(|segment| segment.name.name.as_str())
        .collect::<Vec<_>>()
        .join("::")
}
