//! Writes the function each defaulted field's default is evaluated by.
//!
//! `struct S { result: u32 = 0 }` gets `fn __gos_fd__S__result() -> u32 { 0 }`
//! beside it in its module. A struct literal that leaves the field out calls
//! the function, and so does a decode of a document that leaves its key out,
//! so the default resolves and checks once, where the type is declared, and
//! builds its value afresh at every construction. The body is the
//! declaration's own expression, node ids included, so a tool reading the
//! field's default reads what the function holds.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use gossamer_ast::{
    Attribute, Attrs, Block, BlockKind, Expr, ExprKind, FIELD_DEFAULT_ATTR, FnDecl, Generics,
    Ident, Item, ItemKind, ModBody, NodeIdGenerator, PathExpr, SourceFile, StructBody, StructField,
    Visibility, WhereClause, field_default_fn_name,
};

/// Writes the default functions for every defaulted field of `sf` that has
/// none yet.
pub fn synthesize_field_default_fns(sf: &mut SourceFile) {
    if !has_field_default(&sf.items) {
        return;
    }
    let mut ids = NodeIdGenerator::new();
    while ids.issued() < sf.next_node_id {
        let _ = ids.next();
    }
    synthesize_in(&mut sf.items, &mut ids);
    sf.next_node_id = ids.issued();
}

pub(crate) fn has_field_default(items: &[Item]) -> bool {
    let named_defaults = |body: &StructBody| matches!(body, StructBody::Named(fields) if fields.iter().any(|f| f.default.is_some()));
    items.iter().any(|item| match &item.kind {
        ItemKind::Struct(decl) => named_defaults(&decl.body),
        ItemKind::Enum(decl) => decl.variants.iter().any(|v| named_defaults(&v.body)),
        ItemKind::Mod(decl) => {
            matches!(&decl.body, ModBody::Inline(inner) if has_field_default(inner))
        }
        _ => false,
    })
}

fn synthesize_in(items: &mut Vec<Item>, ids: &mut NodeIdGenerator) {
    let existing: HashSet<String> = items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Fn(decl) => Some(decl.name.name.clone()),
            _ => None,
        })
        .collect();
    let mut out = Vec::with_capacity(items.len());
    for mut item in items.drain(..) {
        if let ItemKind::Mod(decl) = &mut item.kind
            && let ModBody::Inline(inner) = &mut decl.body
        {
            synthesize_in(inner, ids);
        }
        let generated = field_default_fns(&item, &existing, ids);
        out.push(item);
        out.extend(generated);
    }
    *items = out;
}

/// One function per defaulted field of a struct or enum `item` that has
/// none in `existing`, carrying the item's `#[cfg]` so it lives exactly where
/// its type does.
fn field_default_fns(
    item: &Item,
    existing: &HashSet<String>,
    ids: &mut NodeIdGenerator,
) -> Vec<Item> {
    let mut out = Vec::new();
    let mut add = |owner: &str,
                   variant: Option<&str>,
                   fields: &[StructField],
                   generics: &Generics,
                   where_clause: &WhereClause| {
        for field in fields {
            let Some(default) = field.default.as_deref() else {
                continue;
            };
            let name = field_default_fn_name(owner, variant, &field.name.name);
            if existing.contains(&name) {
                continue;
            }
            out.push(field_default_fn(
                name,
                default,
                field,
                fields,
                generics,
                where_clause,
                ids,
            ));
        }
    };
    match &item.kind {
        ItemKind::Struct(decl) => {
            if let StructBody::Named(fields) = &decl.body {
                add(
                    &decl.name.name,
                    None,
                    fields,
                    &decl.generics,
                    &decl.where_clause,
                );
            }
        }
        ItemKind::Enum(decl) => {
            for variant in &decl.variants {
                if let StructBody::Named(fields) = &variant.body {
                    add(
                        &decl.name.name,
                        Some(&variant.name.name),
                        fields,
                        &decl.generics,
                        &decl.where_clause,
                    );
                }
            }
        }
        _ => {}
    }
    let cfgs: Vec<Attribute> = item
        .attrs
        .outer
        .iter()
        .filter(|attr| attr.is_named("cfg"))
        .cloned()
        .collect();
    for generated in &mut out {
        generated.attrs.outer.extend(cfgs.iter().cloned());
        if let ItemKind::Fn(decl) = &mut generated.kind {
            decl.attrs.outer.extend(cfgs.iter().cloned());
        }
    }
    out
}

fn field_default_fn(
    name: String,
    default: &Expr,
    field: &StructField,
    fields: &[StructField],
    generics: &Generics,
    where_clause: &WhereClause,
    ids: &mut NodeIdGenerator,
) -> Item {
    let span = default.span;
    // The resolver binds these names to a placeholder in the body, so a
    // default that reads one is reported rather than resolved elsewhere.
    let siblings = fields
        .iter()
        .map(|f| f.name.name.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let attrs = Attrs {
        outer: vec![Attribute {
            path: PathExpr::single(FIELD_DEFAULT_ATTR),
            tokens: Some(siblings),
        }],
        inner: Vec::new(),
    };
    let block = Block {
        stmts: Vec::new(),
        tail: Some(Box::new(default.clone())),
        synthetic: true,
        kind: BlockKind::Plain,
    };
    let body = Expr::new(ids.next(), span, ExprKind::Block(block));
    let decl = FnDecl {
        attrs: attrs.clone(),
        span,
        is_unsafe: false,
        is_comptime: false,
        visibility: Visibility::Public,
        name: Ident::new(name),
        generics: generics.clone(),
        params: Vec::new(),
        ret: Some(field.ty.clone()),
        where_clause: where_clause.clone(),
        body: Some(Box::new(body)),
        extern_abi: None,
    };
    Item::new(
        ids.next(),
        span,
        attrs,
        Visibility::Public,
        ItemKind::Fn(decl),
    )
}
