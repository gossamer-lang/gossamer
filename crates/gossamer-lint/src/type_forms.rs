//! Rewrites between the two spellings of a type's declaration: field
//! defaults a literal restates, and methods written in a standalone `impl`
//! block or in the type's own body. Both spellings mean the same program, so
//! every rewrite here only moves text; `gos fix` re-checks the result before
//! keeping it.

use std::collections::{HashMap, HashSet};

use gossamer_ast::visitor::{Visitor, walk_expr};
use gossamer_ast::{
    Expr, ExprKind, ImplDecl, ImplItem, Item, ItemKind, ModBody, SourceFile, StructBody,
    StructField, TraitItem, TypeKind,
};
use gossamer_lex::Span;

use crate::Fix;

/// The source text of `span`.
fn text(source: &str, span: Span) -> &str {
    source
        .get(span.start as usize..span.end as usize)
        .unwrap_or("")
}

/// Every named struct and struct variant of the file whose fields declare a
/// default, by the name a literal spells (`S`, or `E::V` written `V` too).
fn defaulted_shapes(items: &[Item], out: &mut HashMap<String, Vec<StructField>>) {
    for item in items {
        match &item.kind {
            ItemKind::Struct(decl) => {
                if let StructBody::Named(fields) = &decl.body
                    && fields.iter().any(|f| f.default.is_some())
                {
                    out.insert(decl.name.name.clone(), fields.clone());
                }
            }
            ItemKind::Enum(decl) => {
                for variant in &decl.variants {
                    if let StructBody::Named(fields) = &variant.body
                        && fields.iter().any(|f| f.default.is_some())
                    {
                        out.insert(
                            format!("{}::{}", decl.name.name, variant.name.name),
                            fields.clone(),
                        );
                    }
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    defaulted_shapes(inner, out);
                }
            }
            _ => {}
        }
    }
}

/// One literal field that writes exactly what its declaration defaults to:
/// the span to report, and the span whose removal drops it from the literal.
pub(crate) struct RestatedDefault {
    pub(crate) report: Span,
    pub(crate) removal: Span,
    pub(crate) field: String,
}

/// Every struct-literal field in `sf` whose value is spelled the way the
/// field's declared default is.
pub(crate) fn restated_defaults(sf: &SourceFile, source: &str) -> Vec<RestatedDefault> {
    struct Scan<'a> {
        shapes: &'a HashMap<String, Vec<StructField>>,
        source: &'a str,
        out: Vec<RestatedDefault>,
    }
    impl Visitor for Scan<'_> {
        fn visit_expr(&mut self, expr: &Expr) {
            if let ExprKind::Struct {
                path,
                fields,
                base: None,
                ..
            } = &expr.kind
            {
                let names: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
                let key = match names.as_slice() {
                    [.., owner, variant] if !owner.is_empty() => format!("{owner}::{variant}"),
                    _ => String::new(),
                };
                let declared = self
                    .shapes
                    .get(&key)
                    .or_else(|| names.last().and_then(|n| self.shapes.get(*n)));
                if let Some(declared) = declared {
                    for field in fields {
                        let Some(value) = &field.value else { continue };
                        let Some(default) = declared
                            .iter()
                            .find(|d| d.name.name == field.name.name)
                            .and_then(|d| d.default.as_deref())
                        else {
                            continue;
                        };
                        if text(self.source, value.span).trim()
                            != text(self.source, default.span).trim()
                        {
                            continue;
                        }
                        if let Some(removal) =
                            field_removal(self.source, &field.name.name, value.span)
                        {
                            self.out.push(RestatedDefault {
                                report: Span::new(value.span.file, removal.start, value.span.end),
                                removal,
                                field: field.name.name.clone(),
                            });
                        }
                    }
                }
            }
            walk_expr(self, expr);
        }
    }
    let mut shapes = HashMap::new();
    defaulted_shapes(&sf.items, &mut shapes);
    if shapes.is_empty() {
        return Vec::new();
    }
    let mut scan = Scan {
        shapes: &shapes,
        source,
        out: Vec::new(),
    };
    scan.visit_source_file(sf);
    scan.out
}

/// The range that removes `name: value` from a struct literal, with the
/// separator that follows it: a comma and the space after it on one line, or
/// the whole line when the field stands on a line of its own.
fn field_removal(source: &str, name: &str, value: Span) -> Option<Span> {
    let bytes = source.as_bytes();
    let mut start = value.start as usize;
    while start > 0 && bytes[start - 1].is_ascii_whitespace() && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    if start == 0 || bytes[start - 1] != b':' {
        return None;
    }
    start -= 1;
    while start > 0 && (bytes[start - 1] == b' ' || bytes[start - 1] == b'\t') {
        start -= 1;
    }
    let name_start = start.checked_sub(name.len())?;
    if source.get(name_start..start) != Some(name) {
        return None;
    }
    start = name_start;
    let mut end = value.end as usize;
    // A separating comma and the blanks after it go with the field.
    let mut cursor = end;
    while cursor < bytes.len() && (bytes[cursor] == b' ' || bytes[cursor] == b'\t') {
        cursor += 1;
    }
    if cursor < bytes.len() && bytes[cursor] == b',' {
        cursor += 1;
        while cursor < bytes.len() && (bytes[cursor] == b' ' || bytes[cursor] == b'\t') {
            cursor += 1;
        }
        end = cursor;
    }
    // A field alone on its line takes the line with it.
    let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
    let alone_before = source[line_start..start].trim().is_empty();
    let after = &source[end..];
    let line_end = after.find('\n').map_or(source.len(), |i| end + i);
    let alone_after = source[end..line_end].trim().is_empty();
    if alone_before && alone_after && line_end < source.len() {
        return Some(Span::new(
            value.file,
            line_start as u32,
            (line_end + 1) as u32,
        ));
    }
    // Otherwise a field after a comma takes that comma instead.
    if end == value.end as usize {
        let mut back = start;
        while back > 0 && bytes[back - 1] == b' ' {
            back -= 1;
        }
        if back > 0 && bytes[back - 1] == b',' {
            return Some(Span::new(value.file, (back - 1) as u32, end as u32));
        }
    }
    Some(Span::new(value.file, start as u32, end as u32))
}

/// Each `#[derive(Default)]` on a struct whose every field declares a
/// default, which gives the struct its `default()` already: the span of the
/// attribute and the edit that drops `Default` from it.
pub(crate) fn redundant_default_derives(sf: &SourceFile, source: &str) -> Vec<(Span, Fix)> {
    let mut out = Vec::new();
    collect_redundant_derives(&sf.items, source, &mut out);
    out
}

fn collect_redundant_derives(items: &[Item], source: &str, out: &mut Vec<(Span, Fix)>) {
    for item in items {
        match &item.kind {
            ItemKind::Struct(decl) => {
                let every = matches!(&decl.body, StructBody::Named(fields)
                    if !fields.is_empty() && fields.iter().all(|f| f.default.is_some()));
                if !every {
                    continue;
                }
                if let Some(found) = derive_default_edit(item, source) {
                    out.push(found);
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    collect_redundant_derives(inner, source, out);
                }
            }
            _ => {}
        }
    }
}

/// The `#[derive(..)]` naming `Default` written before `item`, and the edit
/// that leaves it out: the whole attribute line when it is the only derive.
fn derive_default_edit(item: &Item, source: &str) -> Option<(Span, Fix)> {
    if !item.attrs.outer.iter().any(|attr| {
        attr.is_named("derive")
            && attr
                .tokens
                .as_deref()
                .is_some_and(|t| t.split(',').any(|d| d.trim() == "Default"))
    }) {
        return None;
    }
    // An item's span opens at its first attribute.
    let item_text = text(source, item.span);
    let keyword = item_text.find("struct")?;
    let at = item.span.start as usize + item_text[..keyword].find("#[derive(")?;
    let close = at + source[at..].find(")]")? + 2;
    let inside = &source[at + "#[derive(".len()..close - 2];
    let kept: Vec<&str> = inside
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty() && *d != "Default")
        .collect();
    let file = item.span.file;
    let report = Span::new(file, at as u32, close as u32);
    let fix = if kept.is_empty() {
        let line_end = source[close..]
            .find('\n')
            .map_or(source.len(), |i| close + i + 1);
        let line_start = source[..at].rfind('\n').map_or(0, |i| i + 1);
        let whole_line =
            source[line_start..at].trim().is_empty() && source[close..line_end].trim().is_empty();
        let span = if whole_line {
            Span::new(file, line_start as u32, line_end as u32)
        } else {
            report
        };
        Fix {
            span,
            replacement: String::new(),
            lint_id: "redundant_default_derive",
        }
    } else {
        Fix {
            span: report,
            replacement: format!("#[derive({})]", kept.join(", ")),
            lint_id: "redundant_default_derive",
        }
    };
    Some((report, fix))
}

/// Item names of each trait the file declares, by trait name.
pub(crate) fn file_trait_items(items: &[Item], out: &mut HashMap<String, HashSet<String>>) {
    for item in items {
        match &item.kind {
            ItemKind::Trait(decl) => {
                let names = decl
                    .items
                    .iter()
                    .map(|item| match item {
                        TraitItem::Fn(f) => f.name.name.clone(),
                        TraitItem::Type { name, .. } | TraitItem::Const { name, .. } => {
                            name.name.clone()
                        }
                    })
                    .collect();
                out.insert(decl.name.name.clone(), names);
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    file_trait_items(inner, out);
                }
            }
            _ => {}
        }
    }
}

/// The item names of trait `name`: one this file declares, else one the
/// language knows. `None` when neither says.
fn trait_item_names(
    name: &str,
    declared: &HashMap<String, HashSet<String>>,
) -> Option<HashSet<String>> {
    declared.get(name).cloned().or_else(|| {
        gossamer_ast::LANGUAGE_TRAIT_ITEMS
            .iter()
            .find(|(trait_name, _)| *trait_name == name)
            .map(|(_, names)| names.iter().map(|n| (*n).to_string()).collect())
    })
}

fn impl_item_name(item: &ImplItem) -> String {
    match item {
        ImplItem::Fn(f) => f.name.name.clone(),
        ImplItem::Type { name, .. } | ImplItem::Const { name, .. } => name.name.clone(),
    }
}

/// The text of a block's body: what sits between its first `{` and its
/// last `}`, without the blank lines at either end.
fn block_body(text: &str) -> Option<&str> {
    let open = text.find('{')? + 1;
    let close = text.rfind('}')?;
    let inner = text.get(open..close)?;
    let start = inner
        .char_indices()
        .take_while(|(_, c)| c.is_whitespace())
        .filter(|(_, c)| *c == '\n')
        .last()
        .map_or(0, |(i, _)| i + 1);
    Some(inner[start..].trim_end())
}

/// The start of the line holding `offset`.
fn line_start(source: &str, offset: usize) -> usize {
    source[..offset].rfind('\n').map_or(0, |i| i + 1)
}

/// The offset just past the line holding `offset`, its newline included.
fn line_end(source: &str, offset: usize) -> usize {
    source[offset..]
        .find('\n')
        .map_or(source.len(), |i| offset + i + 1)
}

/// The range that removes the item spanning `span`: its whole lines, and one
/// blank line before it when there is one.
fn item_removal(source: &str, span: Span) -> Span {
    let mut start = line_start(source, span.start as usize);
    let end = line_end(source, (span.end as usize).saturating_sub(1));
    if start > 0 {
        let previous = line_start(source, start - 1);
        if source[previous..start].trim().is_empty() {
            start = previous;
        }
    }
    Span::new(span.file, start as u32, end as u32)
}

/// What a type's header looks like where traits are listed: the offset just
/// past its name and generics, and whether a list (or an enum width) is
/// already there.
fn header_end(item: &Item, source: &str, body_open: usize) -> usize {
    let start = item.span.start as usize;
    let header = &source[start..body_open];
    start + header.trim_end().len()
}

/// Proposes moving each standalone `impl` block of a type declared in the
/// same module into the type's body, listing its trait on the type. A block
/// whose move would change what any function implements, or that carries
/// attributes, bounds, or a where clause of its own, is left alone.
pub(crate) fn merge_impls_into_bodies(
    sf: &SourceFile,
    source: &str,
    context: &crate::migrate::RewriteContext,
    out: &mut Vec<Fix>,
) {
    let mut traits = context.traits.clone();
    file_trait_items(&sf.items, &mut traits);
    merge_in(&sf.items, source, &traits, out);
}

fn merge_in(
    items: &[Item],
    source: &str,
    traits: &HashMap<String, HashSet<String>>,
    out: &mut Vec<Fix>,
) {
    for item in items {
        if let ItemKind::Mod(decl) = &item.kind
            && let ModBody::Inline(inner) = &decl.body
        {
            merge_in(inner, source, traits, out);
        }
    }
    for ty in items {
        let Some(target) = MergeTarget::of(ty, source, traits) else {
            continue;
        };
        let merged = target.collect(items, source, traits);
        if !merged.moved.is_empty() {
            target.emit(source, &merged, out);
        }
    }
}

/// A type whose body standalone `impl` blocks may move into.
struct MergeTarget<'a> {
    ty: &'a Item,
    name: &'a str,
    params: Vec<&'a str>,
    listed: &'a [gossamer_ast::TraitBound],
    shape: Shape,
    /// The fields of a struct written on one line, opened onto lines of
    /// their own, which the merge writes in place of its one-line body.
    opened: Option<String>,
    /// Item names the listed traits declare.
    taken: HashSet<String>,
    /// Names of the functions already in the body.
    inherent: HashSet<String>,
}

/// What moves into one type's body.
struct Merged<'s> {
    new_traits: Vec<String>,
    moved: Vec<&'s str>,
    removals: Vec<Span>,
}

impl<'a> MergeTarget<'a> {
    fn of(ty: &'a Item, source: &str, traits: &HashMap<String, HashSet<String>>) -> Option<Self> {
        let (name, generics, where_clause, listed, body_items, shape) = match &ty.kind {
            ItemKind::Struct(decl) => (
                &decl.name.name,
                &decl.generics,
                &decl.where_clause,
                &decl.traits,
                &decl.items,
                match &decl.body {
                    StructBody::Named(_) => Shape::Braced,
                    StructBody::Tuple(_) => Shape::Tuple,
                    StructBody::Unit => return None,
                },
            ),
            ItemKind::Enum(decl) => (
                &decl.name.name,
                &decl.generics,
                &decl.where_clause,
                &decl.traits,
                &decl.items,
                Shape::Braced,
            ),
            _ => return None,
        };
        if !where_clause.predicates.is_empty() || ty.attrs.outer.iter().any(|a| a.is_named("cfg")) {
            return None;
        }
        // A struct written on one line opens onto lines of its own to take
        // functions; an enum written on one line keeps its form.
        let opened = if text(source, ty.span).contains('\n') {
            None
        } else {
            match (&ty.kind, shape) {
                (ItemKind::Struct(decl), Shape::Braced) => match &decl.body {
                    StructBody::Named(fields) => Some(opened_fields(source, ty, fields)?),
                    _ => return None,
                },
                (_, Shape::Braced) => return None,
                _ => None,
            }
        };
        let params: Vec<&str> = generics
            .params
            .iter()
            .filter_map(|p| match p {
                gossamer_ast::GenericParam::Type { name, bounds, .. } if bounds.is_empty() => {
                    Some(name.name.as_str())
                }
                _ => None,
            })
            .collect();
        if params.len() != generics.params.len() {
            return None;
        }
        let mut taken = HashSet::new();
        for bound in listed {
            taken.extend(trait_item_names(
                &bound.path.segments.last()?.name.name,
                traits,
            )?);
        }
        Some(Self {
            ty,
            name,
            params,
            listed,
            shape,
            opened,
            taken,
            inherent: body_items.iter().map(impl_item_name).collect(),
        })
    }

    /// The blocks among `items` for this type that can move into its body
    /// without changing what any function implements.
    fn collect<'s>(
        &self,
        items: &[Item],
        source: &'s str,
        traits: &HashMap<String, HashSet<String>>,
    ) -> Merged<'s> {
        let mut taken = self.taken.clone();
        let mut merged = Merged {
            new_traits: Vec::new(),
            moved: Vec::new(),
            removals: Vec::new(),
        };
        for candidate in items {
            let ItemKind::Impl(decl) = &candidate.kind else {
                continue;
            };
            if !candidate.attrs.outer.is_empty() || !impl_targets(decl, self.name, &self.params) {
                continue;
            }
            let names: HashSet<String> = decl.items.iter().map(impl_item_name).collect();
            if let Some(bound) = &decl.trait_ref {
                let Some(declares) = bound
                    .path
                    .segments
                    .last()
                    .and_then(|s| trait_item_names(&s.name.name, traits))
                else {
                    continue;
                };
                // Every function must stay with this trait: no other listed
                // trait may declare it, nor may it collide with an inherent
                // function already in the body.
                if !declares.is_disjoint(&taken)
                    || !names.is_disjoint(&self.inherent)
                    || !names.iter().all(|n| declares.contains(n))
                {
                    continue;
                }
                taken.extend(declares);
                merged
                    .new_traits
                    .push(text_of_bound(source, candidate, decl));
            } else if !names.is_disjoint(&taken) {
                // An inherent function whose name a listed trait declares
                // would become that trait's.
                continue;
            }
            let Some(body) = block_body(text(source, candidate.span)) else {
                continue;
            };
            merged.moved.push(body);
            merged.removals.push(item_removal(source, candidate.span));
        }
        merged
    }

    /// The edits that list the moved traits on the type, put the moved
    /// functions in its body, and drop the blocks they came from.
    fn emit(&self, source: &str, merged: &Merged<'_>, out: &mut Vec<Fix>) {
        let ty = self.ty;
        let file = ty.span.file;
        let end = ty.span.end as usize;
        let items_text = merged.moved.join("\n\n");
        let indent = &source[line_start(source, ty.span.start as usize)..ty.span.start as usize];
        let joined = merged.new_traits.join(" + ");
        let has_list = !self.listed.is_empty()
            || matches!(&ty.kind, ItemKind::Enum(decl) if decl.repr.declared_bits.is_some());
        let header = match (merged.new_traits.is_empty(), has_list) {
            (true, _) => String::new(),
            (false, true) => format!(" + {joined}"),
            (false, false) => format!(": {joined}"),
        };
        let body_open = ty.span.start as usize + text(source, ty.span).find('{').unwrap_or(0);
        match (self.shape, &self.opened) {
            (Shape::Braced, Some(fields)) => {
                let at = header_end(ty, source, body_open);
                let separator = if fields.is_empty() { "" } else { "\n" };
                out.push(fix(
                    file,
                    at,
                    end,
                    format!("{header} {{\n{fields}{separator}{items_text}\n{indent}}}"),
                ));
            }
            (Shape::Braced, None) => {
                if !header.is_empty() {
                    let at = header_end(ty, source, body_open);
                    out.push(fix(file, at, at, header));
                }
                let close_line = line_start(source, end.saturating_sub(1));
                out.push(fix(
                    file,
                    close_line,
                    close_line,
                    format!("\n{items_text}\n"),
                ));
            }
            (Shape::Tuple, _) => {
                let at = if source[..end].ends_with(';') {
                    end - 1
                } else {
                    end
                };
                out.push(fix(
                    file,
                    at,
                    end,
                    format!("{header} {{\n{items_text}\n{indent}}}"),
                ));
            }
        }
        out.extend(merged.removals.iter().map(|removal| Fix {
            span: *removal,
            replacement: String::new(),
            lint_id: "impl-into-type-body",
        }));
    }
}

/// The fields of a struct written on one line, each on a line of its own
/// one level in, ending with a newline; empty for a struct with none. `None`
/// when a field carries attributes, which this does not move.
fn opened_fields(source: &str, ty: &Item, fields: &[StructField]) -> Option<String> {
    let indent = &source[line_start(source, ty.span.start as usize)..ty.span.start as usize];
    let mut out = String::new();
    for field in fields {
        if !field.attrs.outer.is_empty() || !field.attrs.inner.is_empty() {
            return None;
        }
        let visibility = match field.visibility {
            gossamer_ast::Visibility::Public => "pub ",
            gossamer_ast::Visibility::Package => "pub(package) ",
            gossamer_ast::Visibility::Inherited => "",
        };
        let default = field
            .default
            .as_ref()
            .map(|d| format!(" = {}", text(source, d.span)))
            .unwrap_or_default();
        out.push_str(&format!(
            "{indent}    {visibility}{}: {}{default}\n",
            field.name.name,
            text(source, field.ty.span)
        ));
    }
    Some(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Braced,
    Tuple,
}

fn fix(file: gossamer_lex::FileId, start: usize, end: usize, replacement: String) -> Fix {
    Fix {
        span: Span::new(file, start as u32, end as u32),
        replacement,
        lint_id: "impl-into-type-body",
    }
}

/// Whether `decl` is a block for the type `name` applied to exactly its own
/// parameters, with no parameters, bounds, or where clause of its own.
fn impl_targets(decl: &ImplDecl, name: &str, params: &[&str]) -> bool {
    if !decl.where_clause.predicates.is_empty() {
        return false;
    }
    let own: Vec<&str> = decl
        .generics
        .params
        .iter()
        .filter_map(|p| match p {
            gossamer_ast::GenericParam::Type { name, bounds, .. } if bounds.is_empty() => {
                Some(name.name.as_str())
            }
            _ => None,
        })
        .collect();
    if own.len() != decl.generics.params.len() || own != params {
        return false;
    }
    let TypeKind::Path(path) = &decl.self_ty.kind else {
        return false;
    };
    let [segment] = path.segments.as_slice() else {
        return false;
    };
    if segment.name.name != name || segment.generics.len() != params.len() {
        return false;
    }
    segment.generics.iter().zip(params).all(|(arg, param)| {
        matches!(arg, gossamer_ast::GenericArg::Type(ty)
            if matches!(&ty.kind, TypeKind::Path(p) if p.segments.len() == 1
                && p.segments[0].name.name == *param && p.segments[0].generics.is_empty()))
    })
}

/// The trait an `impl Trait for T` header names, as written.
fn text_of_bound(source: &str, item: &Item, decl: &ImplDecl) -> String {
    let header = text(source, item.span);
    let after_impl = header.trim_start().strip_prefix("impl").unwrap_or(header);
    let after_generics = if decl.generics.params.is_empty() {
        after_impl
    } else {
        after_impl
            .find('>')
            .map_or(after_impl, |i| &after_impl[i + 1..])
    };
    after_generics
        .split(" for ")
        .next()
        .map_or_else(String::new, |t| t.trim().to_string())
}

/// Proposes moving every function, associated type, and constant written in
/// a type's body into the standalone `impl` blocks it stands for, and
/// dropping the traits the type lists.
pub(crate) fn split_bodies_into_impls(
    sf: &SourceFile,
    source: &str,
    _: &crate::migrate::RewriteContext,
    out: &mut Vec<Fix>,
) {
    let mut desugared = sf.clone();
    let _ = gossamer_parse::desugar_type_bodies(&mut desugared);
    split_in(&sf.items, &desugared.items, source, out);
}

fn split_in(items: &[Item], desugared: &[Item], source: &str, out: &mut Vec<Fix>) {
    for item in items {
        if let ItemKind::Mod(decl) = &item.kind
            && let ModBody::Inline(inner) = &decl.body
            && let Some(ItemKind::Mod(after)) = desugared
                .iter()
                .find(|d| matches!(&d.kind, ItemKind::Mod(m) if m.name.name == decl.name.name))
                .map(|d| &d.kind)
            && let ModBody::Inline(after_inner) = &after.body
        {
            split_in(inner, after_inner, source, out);
        }
        let (name, listed, body_items, last_member_end) = match &item.kind {
            ItemKind::Struct(decl) => (
                &decl.name.name,
                &decl.traits,
                &decl.items,
                match &decl.body {
                    StructBody::Named(fields) => fields
                        .last()
                        .map(|f| f.default.as_ref().map_or(f.ty.span.end, |d| d.span.end)),
                    _ => None,
                },
            ),
            ItemKind::Enum(decl) => (&decl.name.name, &decl.traits, &decl.items, None),
            _ => continue,
        };
        if listed.is_empty() && body_items.is_empty() {
            continue;
        }
        let blocks: Vec<&Item> = desugared
            .iter()
            .filter(|d| {
                d.span == item.span
                    && matches!(&d.kind, ItemKind::Impl(decl)
                        if matches!(&decl.self_ty.kind, TypeKind::Path(p)
                            if p.segments.last().is_some_and(|s| s.name.name == *name)))
            })
            .collect();
        let file = item.span.file;
        let type_text = text(source, item.span);
        let indent =
            &source[line_start(source, item.span.start as usize)..item.span.start as usize];
        let generics_text = generics_text(type_text, name);
        let mut rendered = String::new();
        for block in blocks {
            let ItemKind::Impl(decl) = &block.kind else {
                continue;
            };
            let head = match &decl.trait_ref {
                Some(bound) => format!(
                    "impl{generics_text} {} for {name}{} {{",
                    bound
                        .path
                        .segments
                        .iter()
                        .map(|s| s.name.name.as_str())
                        .collect::<Vec<_>>()
                        .join("::"),
                    applied(generics_text)
                ),
                None => format!("impl{generics_text} {name}{} {{", applied(generics_text)),
            };
            let mut body = String::new();
            let mut previous: Option<&ImplItem> = None;
            for member in &decl.items {
                if let Some(previous) = previous {
                    body.push_str(if blank_between(source, previous, member) {
                        "\n\n"
                    } else {
                        "\n"
                    });
                }
                body.push_str(&member_text(source, member, indent));
                previous = Some(member);
            }
            rendered.push_str(&format!("\n\n{indent}{head}\n{body}\n{indent}}}"));
        }
        // Drop the listed traits from the header.
        if !listed.is_empty()
            && let Some(colon) = header_colon(type_text, name)
        {
            let start = item.span.start as usize + colon;
            let open =
                item.span.start as usize + type_text.find(['{', ';']).unwrap_or(type_text.len());
            let keeps_width =
                matches!(&item.kind, ItemKind::Enum(decl) if decl.repr.declared_bits.is_some());
            let header_tail = &source[start..open];
            let replacement = if keeps_width {
                header_tail
                    .split('+')
                    .next()
                    .map_or_else(String::new, |w| format!("{} ", w.trim_end()))
            } else {
                " ".to_string()
            };
            out.push(Fix {
                span: Span::new(file, start as u32, open as u32),
                replacement,
                lint_id: "type-body-into-impl",
            });
        }
        // Drop the body's functions, keeping its fields and closing brace.
        if !body_items.is_empty() {
            let close = (item.span.end as usize).saturating_sub(1);
            let from = match last_member_end {
                Some(end) => line_end(source, end as usize),
                None => body_items_start(source, item, body_items),
            };
            let to = line_start(source, close);
            if from < to {
                out.push(Fix {
                    span: Span::new(file, from as u32, to as u32),
                    replacement: String::new(),
                    lint_id: "type-body-into-impl",
                });
            }
        }
        let end = item.span.end;
        out.push(Fix {
            span: Span::new(file, end, end),
            replacement: rendered,
            lint_id: "type-body-into-impl",
        });
    }
}

/// Whether a blank line separates two body members as written.
fn blank_between(source: &str, first: &ImplItem, second: &ImplItem) -> bool {
    match (first, second) {
        (ImplItem::Fn(a), ImplItem::Fn(b)) if a.span.end <= b.span.start => source
            .get(a.span.end as usize..b.span.start as usize)
            .is_some_and(|gap| gap.matches('\n').count() > 1),
        _ => false,
    }
}

/// The `<..>` written after a type's name in its declaration, or nothing.
fn generics_text<'a>(type_text: &'a str, name: &str) -> &'a str {
    let Some(at) = type_text.find(name) else {
        return "";
    };
    let rest = &type_text[at + name.len()..];
    if !rest.starts_with('<') {
        return "";
    }
    let mut depth = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[..=i];
                }
            }
            _ => {}
        }
    }
    ""
}

/// The parameters of a `<..>` list applied as arguments: `<T, U>` from
/// `<T: Ord, U>`.
fn applied(generics: &str) -> String {
    if generics.is_empty() {
        return String::new();
    }
    let inner = &generics[1..generics.len() - 1];
    let names: Vec<&str> = inner
        .split(',')
        .map(|p| {
            p.trim()
                .trim_start_matches("const ")
                .split([':', '='])
                .next()
                .unwrap_or("")
                .trim()
        })
        .collect();
    format!("<{}>", names.join(", "))
}

/// The offset in `type_text` of the `:` that opens the trait list.
fn header_colon(type_text: &str, name: &str) -> Option<usize> {
    let at = type_text.find(name)? + name.len();
    let generics = generics_text(type_text, name).len();
    let mut i = at + generics;
    // A tuple struct lists its traits after its fields.
    if type_text[i..].starts_with('(') {
        let mut depth = 0usize;
        for (j, c) in type_text[i..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        i += j + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    let rest = &type_text[i..];
    let trimmed = rest.trim_start();
    trimmed
        .starts_with(':')
        .then(|| i + (rest.len() - trimmed.len()))
}

/// Where a body's functions start when no field precedes them: the line of
/// the first one.
fn body_items_start(source: &str, item: &Item, body_items: &[ImplItem]) -> usize {
    body_items
        .iter()
        .find_map(|member| match member {
            ImplItem::Fn(f) => Some(line_start(source, f.span.start as usize)),
            _ => None,
        })
        .unwrap_or_else(|| line_end(source, item.span.start as usize))
}

/// A body member's text, as written for a function, with the comment lines
/// directly above it; printed for an associated type or constant.
fn member_text(source: &str, member: &ImplItem, indent: &str) -> String {
    match member {
        ImplItem::Fn(f) => {
            let mut start = line_start(source, f.span.start as usize);
            while start > 0 {
                let previous = line_start(source, start - 1);
                if source[previous..start].trim_start().starts_with("//") {
                    start = previous;
                } else {
                    break;
                }
            }
            let end = line_end(source, (f.span.end as usize).saturating_sub(1));
            source[start..end].trim_end().to_string()
        }
        ImplItem::Type { name, ty, .. } => {
            let mut printer = gossamer_ast::Printer::new();
            printer.print_type(ty);
            format!("{indent}    type {} = {}", name.name, printer.finish())
        }
        ImplItem::Const {
            name, ty, value, ..
        } => {
            let mut printer = gossamer_ast::Printer::new();
            printer.print_type(ty);
            let ty = printer.finish();
            let mut printer = gossamer_ast::Printer::new();
            printer.print_expr(value);
            format!(
                "{indent}    const {}: {ty} = {}",
                name.name,
                printer.finish()
            )
        }
    }
}

/// Whether `expr` builds a value from nothing the place it is written
/// binds: literals, collection literals of such values, paths that name an
/// item rather than a local (`None`, `Mode::Fast`, `helper::Rng`), and calls
/// of such paths with such arguments. Only such a value means the same as a
/// field default as it does in a literal.
fn self_contained(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Literal(_) => true,
        ExprKind::Unary { operand, .. } => self_contained(operand),
        ExprKind::Path(path) => {
            path.segments.len() > 1
                || path
                    .segments
                    .first()
                    .is_some_and(|s| s.name.name.chars().next().is_some_and(char::is_uppercase))
        }
        ExprKind::Call { callee, args } => {
            matches!(&callee.kind, ExprKind::Path(_))
                && self_contained(callee)
                && args.iter().all(self_contained)
        }
        ExprKind::Tuple(elems) | ExprKind::MapLiteral(elems) | ExprKind::SetLiteral(elems) => {
            elems.iter().all(self_contained)
        }
        ExprKind::Array(arr) | ExprKind::FixedArray(arr) => match arr {
            gossamer_ast::ArrayExpr::List(elems) => elems.iter().all(self_contained),
            gossamer_ast::ArrayExpr::Repeat { value, count } => {
                self_contained(value) && self_contained(count)
            }
        },
        _ => false,
    }
}

/// One literal's fields: each name, its value as written, the range that
/// drops it, and whether the value is self-contained.
type LiteralFields = Vec<(String, String, Option<Span>, bool)>;

/// Proposes a default for each struct field that every literal of the
/// struct in the file writes with one self-contained value, and drops that
/// value from the literals.
pub(crate) fn fields_take_literal_defaults(
    sf: &SourceFile,
    source: &str,
    _: &crate::migrate::RewriteContext,
    out: &mut Vec<Fix>,
) {
    // Each struct's literals in this file, by struct name: (field, value text, removal).
    struct Scan<'a> {
        source: &'a str,
        literals: HashMap<String, Vec<LiteralFields>>,
    }
    impl Visitor for Scan<'_> {
        fn visit_expr(&mut self, expr: &Expr) {
            if let ExprKind::Struct {
                path, fields, base, ..
            } = &expr.kind
                && path.segments.len() == 1
            {
                let name = &path.segments[0].name.name;
                let written = fields
                    .iter()
                    .map(|field| match &field.value {
                        Some(value) if !field.shorthand => (
                            field.name.name.clone(),
                            text(self.source, value.span).trim().to_string(),
                            field_removal(self.source, &field.name.name, value.span),
                            self_contained(value),
                        ),
                        _ => (field.name.name.clone(), String::new(), None, false),
                    })
                    .collect::<Vec<_>>();
                let entry = self.literals.entry(name.clone()).or_default();
                if base.is_some() {
                    // A spread literal leaves fields to its base: no default
                    // can be read off it, so the struct is left alone.
                    entry.push(Vec::new());
                    entry.push(vec![(String::from("\0"), String::new(), None, false)]);
                } else {
                    entry.push(written);
                }
            }
            walk_expr(self, expr);
        }
    }
    let mut scan = Scan {
        source,
        literals: HashMap::new(),
    };
    scan.visit_source_file(sf);
    collect_literal_defaults(&sf.items, &scan.literals, out);
}

fn collect_literal_defaults(
    items: &[Item],
    literals: &HashMap<String, Vec<LiteralFields>>,
    out: &mut Vec<Fix>,
) {
    for item in items {
        match &item.kind {
            ItemKind::Struct(decl) if decl.generics.params.is_empty() => {
                let StructBody::Named(fields) = &decl.body else {
                    continue;
                };
                let Some(uses) = literals.get(&decl.name.name) else {
                    continue;
                };
                for field in fields.iter().filter(|f| f.default.is_none()) {
                    let mut agreed: Option<&str> = None;
                    let mut removals = Vec::new();
                    let every = uses.iter().all(|literal| {
                        let Some((_, value, removal, contained)) =
                            literal.iter().find(|(name, ..)| *name == field.name.name)
                        else {
                            return false;
                        };
                        if !*contained || agreed.is_some_and(|a| a != value) {
                            return false;
                        }
                        agreed = Some(value);
                        removal.map(|r| removals.push(r)).is_some()
                    });
                    let Some(value) = agreed.filter(|_| every) else {
                        continue;
                    };
                    let file = field.ty.span.file;
                    let end = field.ty.span.end;
                    out.push(Fix {
                        span: Span::new(file, end, end),
                        replacement: format!(" = {value}"),
                        lint_id: "fields-take-literal-defaults",
                    });
                    out.extend(removals.into_iter().map(|span| Fix {
                        span,
                        replacement: String::new(),
                        lint_id: "fields-take-literal-defaults",
                    }));
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    collect_literal_defaults(inner, literals, out);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> SourceFile {
        let mut map = gossamer_lex::SourceMap::new();
        let file = map.add_file("t.gos", source.to_string());
        let (sf, diags) = gossamer_parse::parse_source_file(source, file);
        assert!(diags.is_empty(), "{diags:?}");
        sf
    }

    fn apply(
        source: &str,
        collect: fn(&SourceFile, &str, &crate::migrate::RewriteContext, &mut Vec<Fix>),
    ) -> String {
        let sf = parse(source);
        let mut fixes = Vec::new();
        collect(
            &sf,
            source,
            &crate::migrate::RewriteContext::default(),
            &mut fixes,
        );
        crate::fix::apply(source, &fixes)
    }

    #[test]
    fn a_literal_field_restating_its_default_is_dropped() {
        let source = "struct S {\n    a: i64\n    b: u32 = 0\n}\n\nfn main() {\n    let s = S { a: 1, b: 0 }\n    let t = S {\n        a: 2\n        b: 0\n    }\n    let u = S { a: 3, b: 4 }\n}\n";
        let sf = parse(source);
        let found = restated_defaults(&sf, source);
        assert_eq!(found.len(), 2);
        let edits: Vec<Fix> = found
            .into_iter()
            .map(|f| Fix {
                span: f.removal,
                replacement: String::new(),
                lint_id: "restated_field_default",
            })
            .collect();
        let rewritten = crate::fix::apply(source, &edits);
        assert!(rewritten.contains("let s = S { a: 1 }"), "{rewritten}");
        assert!(
            rewritten.contains("let t = S {\n        a: 2\n    }"),
            "{rewritten}"
        );
        assert!(
            rewritten.contains("let u = S { a: 3, b: 4 }"),
            "{rewritten}"
        );
        parse(&rewritten);
    }

    #[test]
    fn a_restated_default_inside_a_body_method_is_found() {
        let source = "struct S {\n    a: i64\n    b: u32 = 0\n\n    fn new(a: i64) -> S { S { a: a, b: 0 } }\n}\n\nenum E {\n    One\n\n    fn make() -> S { S { a: 1, b: 0 } }\n}\n";
        let sf = parse(source);
        assert_eq!(restated_defaults(&sf, source).len(), 2);
    }

    #[test]
    fn a_default_derive_on_a_fully_defaulted_struct_is_dropped() {
        let source = "#[derive(Debug, Default)]\nstruct A {\n    x: i64 = 1\n}\n\n#[derive(Default)]\nstruct B {\n    y: i64 = 2\n}\n";
        let sf = parse(source);
        let found = redundant_default_derives(&sf, source);
        assert_eq!(found.len(), 2);
        let edits: Vec<Fix> = found.into_iter().map(|(_, fix)| fix).collect();
        let rewritten = crate::fix::apply(source, &edits);
        assert_eq!(
            rewritten,
            "#[derive(Debug)]\nstruct A {\n    x: i64 = 1\n}\n\nstruct B {\n    y: i64 = 2\n}\n"
        );
    }

    const STANDALONE: &str = "trait Bench {\n    fn run(&mut self)\n    fn checksum(&self) -> u32\n}\n\nstruct Sieve {\n    limit: i64\n    result: u32 = 0\n}\n\nimpl Sieve {\n    fn new(limit: i64) -> Sieve { Sieve { limit } }\n}\n\nimpl Bench for Sieve {\n    fn run(&mut self) { self.result += 1 }\n    fn checksum(&self) -> u32 { self.result }\n}\n";

    const IN_BODY: &str = "trait Bench {\n    fn run(&mut self)\n    fn checksum(&self) -> u32\n}\n\nstruct Sieve: Bench {\n    limit: i64\n    result: u32 = 0\n\n    fn new(limit: i64) -> Sieve { Sieve { limit } }\n\n    fn run(&mut self) { self.result += 1 }\n    fn checksum(&self) -> u32 { self.result }\n}\n";

    #[test]
    fn standalone_impls_move_into_the_type_body() {
        let merged = apply(STANDALONE, merge_impls_into_bodies);
        assert_eq!(merged, IN_BODY);
    }

    #[test]
    fn a_type_body_moves_back_into_standalone_impls() {
        let split = apply(IN_BODY, split_bodies_into_impls);
        let sf = parse(&split);
        let impls = sf
            .items
            .iter()
            .filter(|item| matches!(item.kind, ItemKind::Impl(_)))
            .count();
        assert_eq!(impls, 2, "{split}");
        assert!(
            split.contains("struct Sieve {\n    limit: i64\n    result: u32 = 0\n}"),
            "{split}"
        );
        assert!(split.contains("impl Bench for Sieve {\n    fn run(&mut self) { self.result += 1 }\n    fn checksum(&self) -> u32 { self.result }\n}"), "{split}");
        // Merging the split form back gives the body form again.
        assert_eq!(
            apply(&split, merge_impls_into_bodies)
                .matches("impl")
                .count(),
            0
        );
    }

    #[test]
    fn an_impl_whose_function_another_listed_trait_declares_stays_standalone() {
        let source = "struct P: Display {\n    x: i64\n\n    fn fmt(&self) -> String { \"p\" }\n}\n\nimpl Debug for P {\n    fn fmt(&self) -> String { \"P\" }\n}\n";
        assert_eq!(apply(source, merge_impls_into_bodies), source);
    }
    #[test]
    fn a_struct_written_on_one_line_opens_to_take_its_functions() {
        let source = "trait T {\n    fn go(&self) -> i64\n}\n\npub struct P { inner: Map<String, i64>, pub n: i64 = 2 }\n\nimpl T for P {\n    fn go(&self) -> i64 { self.n }\n}\n\nstruct E {}\n\nimpl E {\n    fn new() -> E { E {} }\n}\n";
        let merged = apply(source, merge_impls_into_bodies);
        assert_eq!(
            merged,
            "trait T {\n    fn go(&self) -> i64\n}\n\npub struct P: T {\n    inner: Map<String, i64>\n    pub n: i64 = 2\n\n    fn go(&self) -> i64 { self.n }\n}\n\nstruct E {\n    fn new() -> E { E {} }\n}\n"
        );
        parse(&merged);
    }

    #[test]
    fn a_value_every_literal_writes_becomes_the_field_default() {
        let source = "struct S {\n    limit: i64\n    result: u32\n    rng: Rng\n    seen: Vec<i64>\n}\n\nfn make(n: i64) -> S { S { limit: n, result: 0, rng: Rng::new(), seen: #[] } }\nfn other() -> S { S { limit: 2, result: 0, rng: Rng::new(), seen: #[1] } }\n";
        let rewritten = apply(source, fields_take_literal_defaults);
        assert_eq!(
            rewritten,
            "struct S {\n    limit: i64\n    result: u32 = 0\n    rng: Rng = Rng::new()\n    seen: Vec<i64>\n}\n\nfn make(n: i64) -> S { S { limit: n, seen: #[] } }\nfn other() -> S { S { limit: 2, seen: #[1] } }\n"
        );
        parse(&rewritten);
    }
}
