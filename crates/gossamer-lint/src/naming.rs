//! Rust's naming conventions: `UpperCamelCase` types and variants,
//! `snake_case` functions, parameters, locals, fields, and modules, and
//! `SCREAMING_SNAKE_CASE` consts and statics.
//!
//! A declaration mirroring a C name keeps its spelling: a foreign function,
//! type, or static, an `#[export]` function, and a `#[repr(C)]` struct's
//! fields are not reported.

use std::collections::BTreeSet;

use gossamer_ast::visitor::{Visitor, walk_expr, walk_match_arm, walk_stmt};
use gossamer_ast::{
    Attrs, EnumDecl, Expr, ExprKind, FOREIGN_STATIC_ATTR, FOREIGN_TYPE_ATTR, FnDecl, FnParam,
    GenericParam, Generics, ImplItem, Item, ItemKind, MatchArm, ModBody, Pattern, PatternKind,
    SourceFile, Stmt, StmtKind, StructBody, TraitItem,
};
use gossamer_lex::Span;

use crate::Finding;

/// Which of the three conventions a lint run reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Convention {
    /// `UpperCamelCase`: types, traits, variants, type parameters.
    Camel,
    /// `snake_case`: functions, parameters, locals, fields, modules.
    Snake,
    /// `SCREAMING_SNAKE_CASE`: consts and statics.
    Upper,
}

/// Every name `sf` declares that breaks `convention`.
pub(crate) fn lint_naming(sf: &SourceFile, convention: Convention) -> Vec<Finding> {
    let mut names = Names {
        convention,
        values: BTreeSet::new(),
        out: Vec::new(),
    };
    collect_pattern_values(&sf.items, &mut names.values);
    names.items(&sf.items);
    names.out
}

/// The names this file declares that a single-name pattern matches rather
/// than binds: variants, structs, and consts.
fn collect_pattern_values(items: &[Item], out: &mut BTreeSet<String>) {
    for item in items {
        match &item.kind {
            ItemKind::Enum(decl) => {
                out.extend(decl.variants.iter().map(|v| v.name.name.clone()));
            }
            ItemKind::Struct(decl) => {
                out.insert(decl.name.name.clone());
            }
            ItemKind::Const(decl) => {
                out.insert(decl.name.name.clone());
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    collect_pattern_values(inner, out);
                }
            }
            _ => {}
        }
    }
}

struct Names {
    convention: Convention,
    values: BTreeSet<String>,
    out: Vec<Finding>,
}

impl Names {
    fn report(&mut self, convention: Convention, what: &str, name: &str, span: Span) {
        if convention != self.convention || name.starts_with("__gos_") || follows(convention, name)
        {
            return;
        }
        let (style, spelled) = match convention {
            Convention::Camel => ("an upper camel case", to_camel(name)),
            Convention::Snake => ("a snake case", to_snake(name)),
            Convention::Upper => ("an upper case", to_snake(name).to_uppercase()),
        };
        self.out.push((
            span,
            format!("{what} `{name}` should have {style} name"),
            Some(format!("rename it `{spelled}`")),
        ));
    }

    fn items(&mut self, items: &[Item]) {
        for item in items {
            self.item(item);
        }
    }

    fn item(&mut self, item: &Item) {
        let span = item.span;
        match &item.kind {
            ItemKind::Fn(decl) => self.function(decl, &item.attrs, "function"),
            ItemKind::Struct(decl) => {
                if has_attr(&item.attrs, FOREIGN_TYPE_ATTR) {
                    return;
                }
                self.report(Convention::Camel, "type", &decl.name.name, span);
                self.generics(&decl.generics, span);
                if let StructBody::Named(fields) = &decl.body
                    && !is_repr_c(&item.attrs)
                {
                    for field in fields {
                        self.report(Convention::Snake, "field", &field.name.name, span);
                    }
                }
            }
            ItemKind::Enum(decl) => self.enumeration(decl, span),
            ItemKind::Trait(decl) => {
                self.report(Convention::Camel, "trait", &decl.name.name, span);
                self.generics(&decl.generics, span);
                for member in &decl.items {
                    match member {
                        TraitItem::Fn(f) => self.function(f, &f.attrs, "method"),
                        TraitItem::Const { name, .. } => {
                            self.report(Convention::Upper, "constant", &name.name, span);
                        }
                        TraitItem::Type { name, .. } => {
                            self.report(Convention::Camel, "associated type", &name.name, span);
                        }
                    }
                }
            }
            ItemKind::Impl(decl) => {
                self.generics(&decl.generics, span);
                for member in &decl.items {
                    match member {
                        ImplItem::Fn(f) => self.function(f, &f.attrs, "method"),
                        ImplItem::Const { name, .. } => {
                            self.report(Convention::Upper, "constant", &name.name, span);
                        }
                        ImplItem::Type { .. } => {}
                    }
                }
            }
            ItemKind::TypeAlias(decl) => {
                self.report(Convention::Camel, "type", &decl.name.name, span);
                self.generics(&decl.generics, span);
            }
            ItemKind::Const(decl) => {
                self.report(Convention::Upper, "constant", &decl.name.name, span);
                self.expr(&decl.value);
            }
            ItemKind::Static(decl) => {
                if !has_attr(&item.attrs, FOREIGN_STATIC_ATTR) {
                    self.report(Convention::Upper, "static", &decl.name.name, span);
                }
            }
            ItemKind::Mod(decl) => {
                self.report(Convention::Snake, "module", &decl.name.name, span);
                if let ModBody::Inline(inner) = &decl.body {
                    self.items(inner);
                }
            }
            ItemKind::AttrItem(_) => {}
        }
    }

    fn enumeration(&mut self, decl: &EnumDecl, span: Span) {
        self.report(Convention::Camel, "type", &decl.name.name, span);
        self.generics(&decl.generics, span);
        for variant in &decl.variants {
            self.report(Convention::Camel, "variant", &variant.name.name, span);
        }
    }

    fn generics(&mut self, generics: &Generics, span: Span) {
        for param in &generics.params {
            if let GenericParam::Type { name, .. } = param {
                self.report(Convention::Camel, "type parameter", &name.name, span);
            }
        }
    }

    fn function(&mut self, decl: &FnDecl, attrs: &Attrs, what: &str) {
        // A foreign function or an exported one is named by its C symbol.
        let c_named = decl.extern_abi.is_some() && decl.body.is_none() || has_attr(attrs, "export");
        if !c_named {
            self.report(Convention::Snake, what, &decl.name.name, decl.span);
        }
        self.generics(&decl.generics, decl.span);
        for param in &decl.params {
            if let FnParam::Typed { pattern, .. } = param
                && !c_named
            {
                self.binding_pattern(pattern, true);
            }
        }
        if let Some(body) = &decl.body {
            self.expr(body);
        }
    }

    /// Reports the bindings `pattern` introduces. Where the pattern must
    /// match every value (`irrefutable`), a single uppercase name the file
    /// declares no variant, struct, or const for is a binding too.
    fn binding_pattern(&mut self, pattern: &Pattern, irrefutable: bool) {
        match &pattern.kind {
            PatternKind::Ident {
                name, subpattern, ..
            } => {
                self.report(Convention::Snake, "variable", &name.name, pattern.span);
                if let Some(sub) = subpattern {
                    self.binding_pattern(sub, irrefutable);
                }
            }
            PatternKind::Path(path)
                if irrefutable
                    && path.segments.len() == 1
                    && !self.values.contains(&path.segments[0].name.name)
                    && !matches!(
                        path.segments[0].name.name.as_str(),
                        "None" | "Some" | "Ok" | "Err"
                    ) =>
            {
                self.report(
                    Convention::Snake,
                    "variable",
                    &path.segments[0].name.name,
                    pattern.span,
                );
            }
            PatternKind::Tuple(parts) | PatternKind::Or(parts) => {
                for part in parts {
                    self.binding_pattern(part, irrefutable);
                }
            }
            PatternKind::TupleStruct { elems, .. } => {
                for elem in elems {
                    self.binding_pattern(elem, irrefutable);
                }
            }
            PatternKind::Struct { fields, .. } => {
                for field in fields {
                    if let Some(part) = &field.pattern {
                        self.binding_pattern(part, irrefutable);
                    }
                }
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for part in prefix.iter().chain(rest.as_deref()).chain(suffix) {
                    self.binding_pattern(part, irrefutable);
                }
            }
            PatternKind::Ref { inner, .. } => self.binding_pattern(inner, irrefutable),
            PatternKind::Wildcard
            | PatternKind::Literal(_)
            | PatternKind::Rest
            | PatternKind::Range { .. }
            | PatternKind::Path(_)
            | PatternKind::Error => {}
        }
    }

    fn expr(&mut self, expr: &Expr) {
        self.visit_expr(expr);
    }
}

impl Visitor for Names {
    fn visit_item(&mut self, item: &Item) {
        self.item(item);
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let StmtKind::Let { pattern, .. } = &stmt.kind {
            self.binding_pattern(pattern, true);
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::For { pattern, .. } => self.binding_pattern(pattern, true),
            ExprKind::Closure { params, .. } => {
                for param in params {
                    self.binding_pattern(&param.pattern, true);
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }

    // An arm's single uppercase name may be a variant another file
    // declares, so only a name that is a binding whatever it resolves to is
    // judged here.
    fn visit_match_arm(&mut self, arm: &MatchArm) {
        self.binding_pattern(&arm.pattern, false);
        walk_match_arm(self, arm);
    }

    fn visit_pattern(&mut self, _pattern: &Pattern) {}
}

fn has_attr(attrs: &Attrs, name: &str) -> bool {
    attrs.outer.iter().chain(&attrs.inner).any(|attr| {
        attr.path
            .segments
            .last()
            .is_some_and(|segment| segment.name.name == name)
    })
}

fn is_repr_c(attrs: &Attrs) -> bool {
    attrs.outer.iter().chain(&attrs.inner).any(|attr| {
        attr.path
            .segments
            .last()
            .is_some_and(|segment| segment.name.name == "repr")
            && attr.tokens.as_deref().is_some_and(|t| t.trim() == "C")
    })
}

/// Whether `name` already follows `convention`. Leading and trailing
/// underscores are ignored, as Rust ignores them.
fn follows(convention: Convention, name: &str) -> bool {
    let name = name.trim_matches('_');
    if name.is_empty() {
        return true;
    }
    match convention {
        Convention::Camel => {
            let chars: Vec<char> = name.chars().collect();
            !chars[0].is_lowercase()
                && !name.contains("__")
                && !chars.windows(2).any(|pair| {
                    (pair[0].is_alphabetic() && pair[1] == '_')
                        || (pair[0] == '_' && pair[1].is_alphabetic())
                })
        }
        Convention::Snake => !name.chars().any(char::is_uppercase),
        Convention::Upper => !name.chars().any(char::is_lowercase),
    }
}

/// `name` respelled in `snake_case`.
fn to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let prev_upper = i > 0 && chars[i - 1].is_uppercase();
            if i > 0 && chars[i - 1] != '_' && (prev_lower || (prev_upper && next_lower)) {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `name` respelled in `UpperCamelCase`.
fn to_camel(name: &str) -> String {
    let lower_tail = name.chars().any(char::is_lowercase);
    name.split('_')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            let first: String = chars
                .next()
                .map(|c| c.to_uppercase().collect())
                .unwrap_or_default();
            let rest: String = if lower_tail {
                chars.collect()
            } else {
                chars.as_str().to_lowercase()
            };
            first + &rest
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Convention, follows, to_camel, to_snake};

    #[test]
    fn conventions_follow_rust() {
        assert!(follows(Convention::Camel, "HttpServer"));
        assert!(follows(Convention::Camel, "HTTP"));
        assert!(!follows(Convention::Camel, "KEY_A"));
        assert!(!follows(Convention::Camel, "point"));
        assert!(follows(Convention::Snake, "_unused"));
        assert!(!follows(Convention::Snake, "Yplane"));
        assert!(follows(Convention::Upper, "MAX_LEN"));
        assert!(!follows(Convention::Upper, "limit"));
    }

    #[test]
    fn respellings_name_the_conventional_form() {
        assert_eq!(to_snake("Compute"), "compute");
        assert_eq!(to_snake("parseHTTPHeader"), "parse_http_header");
        assert_eq!(to_snake("UVpitch"), "u_vpitch");
        assert_eq!(to_camel("point"), "Point");
        assert_eq!(to_camel("KEY_A"), "KeyA");
        assert_eq!(to_camel("http_server"), "HttpServer");
    }
}
