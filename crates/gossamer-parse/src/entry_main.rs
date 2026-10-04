//! Synthesis of the implicit `fn main` from an entry file's top-level
//! statements, plus the supporting `?`-scope scan.
//!
//! The entry file is implicitly wrapped in `fn main()`: its bare top-level
//! statements become that function's body, while items declared alongside
//! stay hoisted at file scope. This is a pure front-end desugar into an
//! ordinary `fn main`, so every later stage and every codegen tier handles
//! it without change.

use gossamer_ast::visitor::{Visitor, walk_expr};
use gossamer_ast::{
    Attrs, Block, Expr, ExprKind, FnDecl, GenericArg, Generics, Ident, Item, ItemKind, Literal,
    ModulePath, NodeId, PathExpr, SourceFile, Stmt, Type, TypeKind, TypePath, TypePathSegment,
    UseDecl, UseTarget, Visibility, WhereClause,
};
use gossamer_lex::Span;

use crate::diagnostic::{ParseDiagnostic, ParseError};

/// If `sf` carries top-level statements, wrap them into the body of an
/// implicit `fn main` appended to `sf.items`, and clear the statement list.
///
/// Returns a single conflict diagnostic (synthesizing nothing) when the file
/// also declares an explicit `fn main` - an entry file uses exactly one entry
/// form. A no-op (empty diagnostics) when there are no top-level statements.
///
/// The signature is `()` unless a `?` is used at main scope, in which case it
/// is `Result<(), errors::Error>` with an implicit `Ok(())` tail so the
/// fall-through path returns a `Result`.
#[must_use]
pub fn synthesize_entry_main(sf: &mut SourceFile) -> Vec<ParseDiagnostic> {
    if sf.top_level_stmts.is_empty() {
        return Vec::new();
    }

    if let Some(item) = sf
        .items
        .iter()
        .find(|i| matches!(&i.kind, ItemKind::Fn(f) if f.name.name == "main"))
    {
        return vec![ParseDiagnostic::new(ParseError::MixedEntryForms, item.span)];
    }

    let stmts = std::mem::take(&mut sf.top_level_stmts);
    let span = stmts[0].span;

    let mut next = sf.next_node_id;
    let mut id = || {
        let n = NodeId::from_raw(next);
        next = next.saturating_add(1);
        n
    };

    let uses_try = body_uses_try(&stmts);
    let (ret, tail) = if uses_try {
        (
            Some(result_unit_error_type(&mut id, span)),
            Some(Box::new(ok_unit_expr(&mut id, span))),
        )
    } else {
        (None, None)
    };
    if uses_try && !has_std_errors_use(sf) {
        // The synthesized signature names `errors::Error`; bring it into scope.
        sf.uses.push(UseDecl::simple(
            id(),
            span,
            UseTarget::Module(ModulePath::from_names(["std", "errors"])),
        ));
    }

    let body = Expr::new(
        id(),
        span,
        ExprKind::Block(Block {
            stmts,
            tail,
            synthetic: false,
            kind: gossamer_ast::BlockKind::Plain,
        }),
    );

    let decl = FnDecl {
        attrs: gossamer_ast::Attrs::default(),
        span,
        is_unsafe: false,
        is_comptime: false,
        visibility: gossamer_ast::Visibility::Public,
        name: Ident::new("main"),
        generics: Generics::default(),
        params: Vec::new(),
        ret,
        where_clause: WhereClause::default(),
        body: Some(Box::new(body)),
        extern_abi: None,
    };

    sf.items.push(Item::new(
        id(),
        span,
        Attrs::default(),
        Visibility::Inherited,
        ItemKind::Fn(decl),
    ));
    sf.next_node_id = next;
    Vec::new()
}

/// Returns `true` when any `?` operator appears directly in `stmts` (the
/// implicit main's body), not nested inside a closure or a `fn` item - those
/// bind `?` to their own return type, not main's.
#[must_use]
pub(crate) fn body_uses_try(stmts: &[Stmt]) -> bool {
    let mut scanner = TryScanner { found: false };
    for stmt in stmts {
        scanner.visit_stmt(stmt);
    }
    scanner.found
}

/// Returns `true` when `sf` already imports `errors` from `std` (as
/// `use std::errors` or `use std::{ errors, ... }`), so synthesis does not
/// add a duplicate import.
fn has_std_errors_use(sf: &SourceFile) -> bool {
    sf.uses.iter().any(|u| match &u.target {
        UseTarget::Module(path) => {
            let segs = &path.segments;
            if segs.first().map(|s| s.name.as_str()) != Some("std") {
                return false;
            }
            if segs.len() == 2 && segs[1].name == "errors" {
                return true;
            }
            segs.len() == 1
                && u.list
                    .as_ref()
                    .is_some_and(|list| list.iter().any(|e| e.name.name == "errors"))
        }
        UseTarget::Project { .. } => false,
    })
}

struct TryScanner {
    found: bool,
}

impl Visitor for TryScanner {
    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Try(_) => self.found = true,
            // A closure has its own return scope; a `?` inside it binds there.
            ExprKind::Closure { .. } => {}
            _ => walk_expr(self, expr),
        }
    }

    // A nested `fn` (declared among the statements) has its own return scope;
    // never descend into items when scanning main's body.
    fn visit_item(&mut self, _item: &Item) {}
}

/// Builds the `Result<(), errors::Error>` return type for a `?`-using main.
fn result_unit_error_type(id: &mut impl FnMut() -> NodeId, span: Span) -> Type {
    let unit = Type::new(id(), span, TypeKind::Unit);
    let err = Type::new(
        id(),
        span,
        TypeKind::Path(TypePath {
            segments: vec![
                TypePathSegment::new("errors"),
                TypePathSegment::new("Error"),
            ],
        }),
    );
    Type::new(
        id(),
        span,
        TypeKind::Path(TypePath {
            segments: vec![TypePathSegment::with_generics(
                "Result",
                vec![GenericArg::Type(unit), GenericArg::Type(err)],
            )],
        }),
    )
}

/// Builds the `Ok(())` expression used as the implicit tail of a `?`-using
/// main, so the fall-through path returns `Ok(())`.
fn ok_unit_expr(id: &mut impl FnMut() -> NodeId, span: Span) -> Expr {
    let unit = Expr::new(id(), span, ExprKind::Literal(Literal::Unit));
    let ok = Expr::new(id(), span, ExprKind::Path(PathExpr::single("Ok")));
    Expr::new(
        id(),
        span,
        ExprKind::Call {
            callee: Box::new(ok),
            args: vec![unit],
        },
    )
}

#[cfg(test)]
mod entry_main_tests {
    use super::*;
    use gossamer_lex::SourceMap;

    fn parse(src: &str) -> SourceFile {
        let mut map = SourceMap::new();
        let file = map.add_file("t.gos", src.to_string());
        crate::parse_source_file(src, file).0
    }

    fn find_main(sf: &SourceFile) -> Option<&FnDecl> {
        sf.items.iter().find_map(|i| match &i.kind {
            ItemKind::Fn(f) if f.name.name == "main" => Some(f),
            _ => None,
        })
    }

    #[test]
    fn detects_top_level_try() {
        assert!(body_uses_try(&parse("let x = f()?\n").top_level_stmts));
    }

    #[test]
    fn ignores_try_inside_closure() {
        assert!(!body_uses_try(&parse("let g = || f()?\n").top_level_stmts));
    }

    #[test]
    fn no_try_means_false() {
        assert!(!body_uses_try(&parse("println(\"hi\")\n").top_level_stmts));
    }

    #[test]
    fn synthesizes_unit_main_from_statements() {
        let mut sf = parse("println(\"hi\")\n");
        let diags = synthesize_entry_main(&mut sf);
        assert!(diags.is_empty());
        assert!(sf.top_level_stmts.is_empty(), "statements moved into main");
        let main = find_main(&sf).expect("a synthesized main");
        assert!(main.ret.is_none(), "unit return when no ?");
        assert!(main.body.is_some());
    }

    #[test]
    fn synthesizes_result_main_when_try_used() {
        let mut sf = parse("let _ = f()?\n");
        let _ = synthesize_entry_main(&mut sf);
        let main = find_main(&sf).expect("a synthesized main");
        assert!(main.ret.is_some(), "Result return when ? present");
    }

    #[test]
    fn mixing_statements_with_explicit_main_errors() {
        let mut sf = parse("println(\"hi\")\nfn main() { }\n");
        let diags = synthesize_entry_main(&mut sf);
        assert_eq!(diags.len(), 1, "one conflict diagnostic");
        assert!(matches!(diags[0].error, ParseError::MixedEntryForms));
    }

    #[test]
    fn no_statements_is_a_noop() {
        let mut sf = parse("fn main() { }\n");
        let diags = synthesize_entry_main(&mut sf);
        assert!(diags.is_empty());
    }
}
