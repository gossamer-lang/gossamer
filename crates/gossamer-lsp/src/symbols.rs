//! Document- and workspace-symbol collection.
//!
//! Document symbols power the editor outline; workspace symbols power
//! "go to symbol" across every open file. Both walk the AST top-level
//! item list and emit [LSP `SymbolKind`][1]-tagged entries with
//! `range` (the whole item) and `selectionRange` (just the name).
//!
//! [1]: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#symbolKind

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use gossamer_ast::{
    EnumDecl, EnumVariant, ImplDecl, ImplItem, Item, ItemKind, ModBody, StructBody, StructDecl,
    TraitDecl, TraitItem,
};
use gossamer_lex::Span;
use gossamer_std::json::Value;

use crate::navigation::declared_name_span;
use crate::session::DocumentAnalysis;

// LSP wire numbers for the SymbolKind enum. Kept verbatim from the
// LSP spec so the table is self-documenting; the unused entries
// (`FILE`, `CONSTRUCTOR`) round out the kind set.
#[allow(
    dead_code,
    reason = "wire-number table mirroring the LSP SymbolKind spec"
)]
const SYMBOL_FILE: f64 = 1.0;
const SYMBOL_MODULE: f64 = 2.0;
const SYMBOL_CLASS: f64 = 5.0;
const SYMBOL_METHOD: f64 = 6.0;
const SYMBOL_FIELD: f64 = 8.0;
#[allow(
    dead_code,
    reason = "wire-number table mirroring the LSP SymbolKind spec"
)]
const SYMBOL_CONSTRUCTOR: f64 = 9.0;
const SYMBOL_ENUM: f64 = 10.0;
const SYMBOL_INTERFACE: f64 = 11.0;
const SYMBOL_FUNCTION: f64 = 12.0;
const SYMBOL_VARIABLE: f64 = 13.0;
const SYMBOL_CONSTANT: f64 = 14.0;
const SYMBOL_ENUM_MEMBER: f64 = 22.0;
const SYMBOL_STRUCT: f64 = 23.0;
const SYMBOL_TYPE_PARAMETER: f64 = 26.0;

/// Builds the `textDocument/documentSymbol` response - a hierarchical
/// outline of every top-level item, with impl methods nested under
/// their type.
pub(crate) fn document_symbols(doc: &DocumentAnalysis) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for item in doc.document_items() {
        emit_item(doc, item, &mut out);
    }
    Value::Array(out)
}

/// Builds the `workspace/symbol` response - flat list of every
/// top-level (or impl-method) symbol across `documents` whose name
/// contains the query case-insensitively.
pub(crate) fn workspace_symbols(documents: &[&DocumentAnalysis], query: &str) -> Value {
    let mut out: Vec<Value> = Vec::new();
    let needle = query.to_ascii_lowercase();
    for doc in documents {
        for item in doc.document_items() {
            collect_workspace_symbols(doc, item, None, &needle, &mut out);
        }
    }
    Value::Array(out)
}

fn emit_item(doc: &DocumentAnalysis, item: &Item, out: &mut Vec<Value>) {
    match &item.kind {
        ItemKind::Fn(decl) => out.push(symbol(
            doc,
            &decl.name.name,
            None,
            SYMBOL_FUNCTION,
            item.span,
            declared_name_span(doc.source(), item.span, &decl.name.name),
            Vec::new(),
        )),
        ItemKind::Struct(decl) => out.push(struct_symbol(doc, item, decl)),
        ItemKind::Enum(decl) => out.push(enum_symbol(doc, item, decl)),
        ItemKind::Trait(decl) => out.push(trait_symbol(doc, item, decl)),
        ItemKind::Impl(decl) => emit_impl(doc, item, decl, out),
        ItemKind::TypeAlias(decl) => out.push(symbol(
            doc,
            &decl.name.name,
            None,
            SYMBOL_TYPE_PARAMETER,
            item.span,
            declared_name_span(doc.source(), item.span, &decl.name.name),
            Vec::new(),
        )),
        ItemKind::Const(decl) => out.push(symbol(
            doc,
            &decl.name.name,
            None,
            SYMBOL_CONSTANT,
            item.span,
            declared_name_span(doc.source(), item.span, &decl.name.name),
            Vec::new(),
        )),
        ItemKind::Static(decl) => out.push(symbol(
            doc,
            &decl.name.name,
            None,
            SYMBOL_VARIABLE,
            item.span,
            declared_name_span(doc.source(), item.span, &decl.name.name),
            Vec::new(),
        )),
        ItemKind::Mod(decl) => {
            let mut children: Vec<Value> = Vec::new();
            if let ModBody::Inline(items) = &decl.body {
                for nested in items {
                    emit_item(doc, nested, &mut children);
                }
            }
            out.push(symbol(
                doc,
                &decl.name.name,
                None,
                SYMBOL_MODULE,
                item.span,
                declared_name_span(doc.source(), item.span, &decl.name.name),
                children,
            ));
        }
        ItemKind::AttrItem(_) => {}
    }
}

fn struct_symbol(doc: &DocumentAnalysis, item: &Item, decl: &StructDecl) -> Value {
    let mut children: Vec<Value> = Vec::new();
    if let StructBody::Named(fields) = &decl.body {
        for field in fields {
            children.push(symbol(
                doc,
                &field.name.name,
                None,
                SYMBOL_FIELD,
                item.span,
                declared_name_span(doc.source(), item.span, &field.name.name),
                Vec::new(),
            ));
        }
    }
    symbol(
        doc,
        &decl.name.name,
        None,
        SYMBOL_STRUCT,
        item.span,
        declared_name_span(doc.source(), item.span, &decl.name.name),
        children,
    )
}

fn enum_symbol(doc: &DocumentAnalysis, item: &Item, decl: &EnumDecl) -> Value {
    let children: Vec<Value> = decl
        .variants
        .iter()
        .map(|v: &EnumVariant| {
            symbol(
                doc,
                &v.name.name,
                None,
                SYMBOL_ENUM_MEMBER,
                item.span,
                declared_name_span(doc.source(), item.span, &v.name.name),
                Vec::new(),
            )
        })
        .collect();
    symbol(
        doc,
        &decl.name.name,
        None,
        SYMBOL_ENUM,
        item.span,
        declared_name_span(doc.source(), item.span, &decl.name.name),
        children,
    )
}

fn trait_symbol(doc: &DocumentAnalysis, item: &Item, decl: &TraitDecl) -> Value {
    let children: Vec<Value> = decl
        .items
        .iter()
        .filter_map(|trait_item| match trait_item {
            TraitItem::Fn(fn_decl) => Some(symbol(
                doc,
                &fn_decl.name.name,
                None,
                SYMBOL_METHOD,
                item.span,
                declared_name_span(doc.source(), item.span, &fn_decl.name.name),
                Vec::new(),
            )),
            _ => None,
        })
        .collect();
    symbol(
        doc,
        &decl.name.name,
        None,
        SYMBOL_INTERFACE,
        item.span,
        declared_name_span(doc.source(), item.span, &decl.name.name),
        children,
    )
}

fn emit_impl(doc: &DocumentAnalysis, item: &Item, decl: &ImplDecl, out: &mut Vec<Value>) {
    let mut printer = gossamer_ast::Printer::new();
    printer.print_type(&decl.self_ty);
    let self_ty = printer.finish();
    let label = decl.trait_ref.as_ref().map_or_else(
        || format!("impl {self_ty}"),
        |trait_ref| {
            let mut p = gossamer_ast::Printer::new();
            p.print_type_path(&trait_ref.path);
            let trait_name = p.finish();
            format!("impl {trait_name} for {self_ty}")
        },
    );
    let children: Vec<Value> = decl
        .items
        .iter()
        .map(|impl_item| match impl_item {
            ImplItem::Fn(fn_decl) => symbol(
                doc,
                &fn_decl.name.name,
                None,
                SYMBOL_METHOD,
                item.span,
                declared_name_span(doc.source(), item.span, &fn_decl.name.name),
                Vec::new(),
            ),
            ImplItem::Const { name, .. } => symbol(
                doc,
                &name.name,
                None,
                SYMBOL_CONSTANT,
                item.span,
                declared_name_span(doc.source(), item.span, &name.name),
                Vec::new(),
            ),
            ImplItem::Type { name, .. } => symbol(
                doc,
                &name.name,
                None,
                SYMBOL_TYPE_PARAMETER,
                item.span,
                declared_name_span(doc.source(), item.span, &name.name),
                Vec::new(),
            ),
        })
        .collect();
    out.push(symbol(
        doc,
        &label,
        None,
        SYMBOL_CLASS,
        item.span,
        item.span,
        children,
    ));
}

#[allow(
    clippy::too_many_lines,
    reason = "flat-shape dispatch / lowering - splitting hides the per-arm intent"
)]
fn collect_workspace_symbols(
    doc: &DocumentAnalysis,
    item: &Item,
    container: Option<&str>,
    needle: &str,
    out: &mut Vec<Value>,
) {
    let push = |out: &mut Vec<Value>, name: &str, kind: f64, name_span: Span| {
        if !name.to_ascii_lowercase().contains(needle) {
            return;
        }
        out.push(workspace_symbol_entry(
            doc, name, container, kind, name_span,
        ));
    };
    match &item.kind {
        ItemKind::Fn(decl) => push(
            out,
            &decl.name.name,
            SYMBOL_FUNCTION,
            declared_name_span(doc.source(), item.span, &decl.name.name),
        ),
        ItemKind::Struct(decl) => {
            push(
                out,
                &decl.name.name,
                SYMBOL_STRUCT,
                declared_name_span(doc.source(), item.span, &decl.name.name),
            );
            if let StructBody::Named(fields) = &decl.body {
                for field in fields {
                    push(
                        out,
                        &field.name.name,
                        SYMBOL_FIELD,
                        declared_name_span(doc.source(), item.span, &field.name.name),
                    );
                }
            }
        }
        ItemKind::Enum(decl) => {
            push(
                out,
                &decl.name.name,
                SYMBOL_ENUM,
                declared_name_span(doc.source(), item.span, &decl.name.name),
            );
            for v in &decl.variants {
                push(
                    out,
                    &v.name.name,
                    SYMBOL_ENUM_MEMBER,
                    declared_name_span(doc.source(), item.span, &v.name.name),
                );
            }
        }
        ItemKind::Trait(decl) => {
            push(
                out,
                &decl.name.name,
                SYMBOL_INTERFACE,
                declared_name_span(doc.source(), item.span, &decl.name.name),
            );
        }
        ItemKind::Impl(decl) => {
            for impl_item in &decl.items {
                if let ImplItem::Fn(fn_decl) = impl_item {
                    push(
                        out,
                        &fn_decl.name.name,
                        SYMBOL_METHOD,
                        declared_name_span(doc.source(), item.span, &fn_decl.name.name),
                    );
                }
            }
        }
        ItemKind::TypeAlias(decl) => push(
            out,
            &decl.name.name,
            SYMBOL_TYPE_PARAMETER,
            declared_name_span(doc.source(), item.span, &decl.name.name),
        ),
        ItemKind::Const(decl) => push(
            out,
            &decl.name.name,
            SYMBOL_CONSTANT,
            declared_name_span(doc.source(), item.span, &decl.name.name),
        ),
        ItemKind::Static(decl) => push(
            out,
            &decl.name.name,
            SYMBOL_VARIABLE,
            declared_name_span(doc.source(), item.span, &decl.name.name),
        ),
        ItemKind::Mod(decl) => {
            push(
                out,
                &decl.name.name,
                SYMBOL_MODULE,
                declared_name_span(doc.source(), item.span, &decl.name.name),
            );
            if let ModBody::Inline(items) = &decl.body {
                for nested in items {
                    collect_workspace_symbols(doc, nested, Some(&decl.name.name), needle, out);
                }
            }
        }
        ItemKind::AttrItem(_) => {}
    }
}

fn workspace_symbol_entry(
    doc: &DocumentAnalysis,
    name: &str,
    container: Option<&str>,
    kind: f64,
    name_span: Span,
) -> Value {
    let mut entry = BTreeMap::new();
    entry.insert("name".to_string(), Value::String(name.to_string()));
    entry.insert("kind".to_string(), Value::Number(kind));
    let mut location = BTreeMap::new();
    location.insert("uri".to_string(), Value::String(doc.uri.clone()));
    location.insert("range".to_string(), span_to_range(doc, name_span));
    entry.insert("location".to_string(), Value::Object(location));
    if let Some(container) = container {
        entry.insert(
            "containerName".to_string(),
            Value::String(container.to_string()),
        );
    }
    Value::Object(entry)
}

#[allow(
    clippy::too_many_arguments,
    reason = "lowering plumbing - every parameter is needed by the surrounding pipeline"
)]
fn symbol(
    doc: &DocumentAnalysis,
    name: &str,
    detail: Option<&str>,
    kind: f64,
    range: Span,
    selection: Span,
    children: Vec<Value>,
) -> Value {
    let mut entry = BTreeMap::new();
    entry.insert("name".to_string(), Value::String(name.to_string()));
    if let Some(detail) = detail {
        entry.insert("detail".to_string(), Value::String(detail.to_string()));
    }
    entry.insert("kind".to_string(), Value::Number(kind));
    entry.insert("range".to_string(), span_to_range(doc, range));
    entry.insert("selectionRange".to_string(), span_to_range(doc, selection));
    if !children.is_empty() {
        entry.insert("children".to_string(), Value::Array(children));
    }
    Value::Object(entry)
}

fn span_to_range(doc: &DocumentAnalysis, span: Span) -> Value {
    let (start_line, start_col) = doc.offset_to_position(span.start);
    let (end_line, end_col) = doc.offset_to_position(span.end);
    let mut start = BTreeMap::new();
    start.insert("line".to_string(), Value::Number(f64::from(start_line)));
    start.insert("character".to_string(), Value::Number(f64::from(start_col)));
    let mut end = BTreeMap::new();
    end.insert("line".to_string(), Value::Number(f64::from(end_line)));
    end.insert("character".to_string(), Value::Number(f64::from(end_col)));
    let mut range = BTreeMap::new();
    range.insert("start".to_string(), Value::Object(start));
    range.insert("end".to_string(), Value::Object(end));
    Value::Object(range)
}

/// Folding ranges from the `SourceFile`: each item span and inline
/// module body becomes one folding region.
pub(crate) fn folding_ranges(doc: &DocumentAnalysis) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for item in doc.document_items() {
        push_folding(doc, item, &mut out);
    }
    Value::Array(out)
}

fn push_folding(doc: &DocumentAnalysis, item: &Item, out: &mut Vec<Value>) {
    let (start_line, _) = doc.offset_to_position(item.span.start);
    let end_offset = item.span.end.saturating_sub(1);
    let (end_line, _) = doc.offset_to_position(end_offset);
    if start_line < end_line {
        let mut entry = BTreeMap::new();
        entry.insert(
            "startLine".to_string(),
            Value::Number(f64::from(start_line)),
        );
        entry.insert("endLine".to_string(), Value::Number(f64::from(end_line)));
        out.push(Value::Object(entry));
    }
    if let ItemKind::Mod(decl) = &item.kind {
        if let ModBody::Inline(items) = &decl.body {
            for nested in items {
                push_folding(doc, nested, out);
            }
        }
    }
}
