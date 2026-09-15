//! Auto-applicable suggestions for a subset of the lint set.
//!
//! A [`Fix`] is a `(span, replacement)` pair tagged with the lint
//! that produced it. `gos lint --fix` collects every fix whose
//! originating lint is enabled at warn/deny in the registry,
//! applies them in reverse order of span start, and writes the
//! result back. Conservative on purpose: a lint only ships a fix
//! when the rewrite is unambiguous and span-local.

use gossamer_ast::{
    BinaryOp, Block, Expr, ExprKind, ItemKind, Literal, Mutability, Pattern, PatternKind,
    SourceFile, StmtKind, UnaryOp,
};
use gossamer_lex::Span;

use crate::Registry;
use crate::lints::{block_reassigns, walk_block, walk_expr};

/// An auto-applicable source edit.
#[derive(Debug, Clone)]
pub struct Fix {
    /// Source range to replace.
    pub span: Span,
    /// Text to substitute in place of `span`.
    pub replacement: String,
    /// Lint id that produced the fix.
    pub lint_id: &'static str,
}

/// Collects every auto-fix emitted by enabled lints.
#[must_use]
pub fn fixes(sf: &SourceFile, registry: &Registry, source: &str) -> Vec<Fix> {
    let mut out = Vec::new();
    for (id, level) in registry.entries() {
        if matches!(level, crate::Level::Allow) {
            continue;
        }
        match id {
            // Safe to apply: the rewrite is span-local and the migration
            // that shares this rule refuses any call whose data argument
            // came from a pipe. The sibling `i64_only_container_family`
            // is deliberately not auto-fixable - the two container
            // families differ in what an empty container means.
            "double_negation" => fix_double_negation(sf, source, &mut out),
            "unused_variable" => fix_unused_variable(sf, source, &mut out),
            "unused_mut_variable" => fix_unused_mut_variable(sf, source, &mut out),
            "needless_bool" => fix_needless_bool(sf, source, &mut out),
            "comparison_to_bool_literal" => fix_bool_cmp(sf, source, &mut out),
            _ => {}
        }
    }
    out
}

/// Applies `fixes` to `source`, returning the rewritten text.
/// Overlapping edits are resolved by "left-most wins"; later edits
/// that start inside an earlier applied span are skipped so the
/// result is well-defined.
#[must_use]
pub fn apply(source: &str, fixes: &[Fix]) -> String {
    let mut ordered: Vec<&Fix> = fixes.iter().collect();
    // Of two edits starting at one offset, the wider one is applied: it
    // rewrites the whole construct the narrower one sits inside.
    ordered.sort_by_key(|f| (f.span.start, std::cmp::Reverse(f.span.end)));
    let mut out = String::with_capacity(source.len());
    let mut cursor: usize = 0;
    for fix in ordered {
        let start = fix.span.start as usize;
        let end = fix.span.end as usize;
        if start < cursor {
            continue;
        }
        out.push_str(&source[cursor..start]);
        out.push_str(&fix.replacement);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    out
}

fn fix_double_negation(sf: &SourceFile, source: &str, out: &mut Vec<Fix>) {
    each_fn_body(sf, |body| {
        walk_expr(body, &mut |expr| {
            if let ExprKind::Unary {
                op: UnaryOp::Not,
                operand,
            } = &expr.kind
            {
                if let ExprKind::Unary {
                    op: UnaryOp::Not,
                    operand: inner,
                } = &operand.kind
                {
                    let inner_text = slice(source, inner.span);
                    out.push(Fix {
                        span: expr.span,
                        replacement: inner_text.to_string(),
                        lint_id: "double_negation",
                    });
                }
            }
        });
    });
}

fn fix_unused_variable(sf: &SourceFile, source: &str, out: &mut Vec<Fix>) {
    use std::collections::BTreeSet;
    each_fn_body(sf, |body| {
        let Some(block) = as_block(body) else {
            return;
        };
        let mut bindings: Vec<(String, Span)> = Vec::new();
        for stmt in &block.stmts {
            if let StmtKind::Let { pattern, .. } = &stmt.kind {
                if let Some((name, span, _)) = ident_name(pattern) {
                    if !name.starts_with('_') {
                        bindings.push((name.to_string(), span));
                    }
                }
            }
        }
        let mut used: BTreeSet<String> = BTreeSet::new();
        walk_block(block, &mut |expr| {
            if let ExprKind::Path(path) = &expr.kind {
                if let Some(seg) = path.segments.first() {
                    used.insert(seg.name.name.clone());
                }
            }
        });
        for (name, span) in bindings {
            if !used.contains(&name) {
                let Some(relative) = slice(source, span).rfind(&name) else {
                    continue;
                };
                let insertion = span.start + relative as u32;
                out.push(Fix {
                    span: Span::new(span.file, insertion, insertion),
                    replacement: "_".to_string(),
                    lint_id: "unused_variable",
                });
            }
        }
    });
}

fn fix_unused_mut_variable(sf: &SourceFile, source: &str, out: &mut Vec<Fix>) {
    each_fn_body(sf, |body| {
        let Some(block) = as_block(body) else {
            return;
        };
        for stmt in &block.stmts {
            if let StmtKind::Let { pattern, .. } = &stmt.kind {
                if let Some((name, ident_span, mutability)) = ident_name(pattern) {
                    if matches!(mutability, Mutability::Mutable)
                        && !name.starts_with('_')
                        && !block_reassigns(block, name)
                    {
                        if let Some(mut_span) = find_mut_span(source, stmt.span, ident_span) {
                            let cut_end = consume_whitespace(source, mut_span.end as usize) as u32;
                            out.push(Fix {
                                span: Span::new(mut_span.file, mut_span.start, cut_end),
                                replacement: String::new(),
                                lint_id: "unused_mut_variable",
                            });
                        }
                    }
                }
            }
        }
    });
}

fn fix_needless_bool(sf: &SourceFile, source: &str, out: &mut Vec<Fix>) {
    each_fn_body(sf, |body| {
        walk_expr(body, &mut |expr| {
            let ExprKind::If {
                condition,
                then_branch,
                else_branch: Some(else_branch),
            } = &expr.kind
            else {
                return;
            };
            let Some(then_expr) = tail_of(then_branch) else {
                return;
            };
            let Some(else_expr) = tail_of(else_branch) else {
                return;
            };
            let (Some(then_bool), Some(else_bool)) =
                (bool_literal(then_expr), bool_literal(else_expr))
            else {
                return;
            };
            let cond_text = slice(source, condition.span);
            let replacement = match (then_bool, else_bool) {
                (true, false) => cond_text.to_string(),
                (false, true) => format!("!({cond_text})"),
                _ => return,
            };
            out.push(Fix {
                span: expr.span,
                replacement,
                lint_id: "needless_bool",
            });
        });
    });
}

fn fix_bool_cmp(sf: &SourceFile, source: &str, out: &mut Vec<Fix>) {
    each_fn_body(sf, |body| {
        walk_expr(body, &mut |expr| {
            let ExprKind::Binary { op, lhs, rhs } = &expr.kind else {
                return;
            };
            let (other, literal) = match (&lhs.kind, &rhs.kind) {
                (ExprKind::Literal(Literal::Bool(b)), _) => (rhs, *b),
                (_, ExprKind::Literal(Literal::Bool(b))) => (lhs, *b),
                _ => return,
            };
            let other_text = slice(source, other.span);
            let replacement = match (op, literal) {
                (BinaryOp::Eq, true) | (BinaryOp::Ne, false) => other_text.to_string(),
                (BinaryOp::Eq, false) | (BinaryOp::Ne, true) => format!("!({other_text})"),
                _ => return,
            };
            out.push(Fix {
                span: expr.span,
                replacement,
                lint_id: "comparison_to_bool_literal",
            });
        });
    });
}

fn slice(source: &str, span: Span) -> &str {
    let start = (span.start as usize).min(source.len());
    let end = (span.end as usize).min(source.len()).max(start);
    &source[start..end]
}

fn find_mut_span(source: &str, stmt_span: Span, _ident_span: Span) -> Option<Span> {
    let stmt_text = slice(source, stmt_span);
    let idx = stmt_text.find("mut")?;
    let byte_start = stmt_span.start + idx as u32;
    let byte_end = byte_start + "mut".len() as u32;
    Some(Span::new(stmt_span.file, byte_start, byte_end))
}

fn consume_whitespace(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = start;
    while cursor < bytes.len() && (bytes[cursor] == b' ' || bytes[cursor] == b'\t') {
        cursor += 1;
    }
    cursor
}

fn tail_of(expr: &Expr) -> Option<&Expr> {
    if let ExprKind::Block(block) = &expr.kind {
        if block.stmts.is_empty() {
            return block.tail.as_deref();
        }
        return None;
    }
    Some(expr)
}

fn bool_literal(expr: &Expr) -> Option<bool> {
    if let ExprKind::Literal(Literal::Bool(b)) = &expr.kind {
        Some(*b)
    } else {
        None
    }
}

fn each_fn_body(sf: &SourceFile, mut visit: impl FnMut(&Expr)) {
    for item in &sf.items {
        if let ItemKind::Fn(func) = &item.kind {
            if let Some(body) = &func.body {
                visit(body);
            }
        }
    }
}

fn as_block(expr: &Expr) -> Option<&Block> {
    if let ExprKind::Block(block) = &expr.kind {
        Some(block)
    } else {
        None
    }
}

fn ident_name(pattern: &Pattern) -> Option<(&str, Span, Mutability)> {
    if let PatternKind::Ident {
        mutability, name, ..
    } = &pattern.kind
    {
        Some((&name.name, pattern.span, *mutability))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gossamer_lex::SourceMap;

    fn run(source: &str) -> String {
        let mut map = SourceMap::new();
        let file_id = map.add_file("t.gos".to_string(), source.to_string());
        let (sf, _) = gossamer_parse::parse_source_file(source, file_id);
        let registry = Registry::with_defaults();
        let fx = fixes(&sf, &registry, source);
        apply(source, &fx)
    }

    #[test]
    fn the_wider_of_two_edits_at_one_offset_is_applied() {
        let mut map = SourceMap::new();
        let file = map.add_file("t.gos".to_string(), String::new());
        let edit = |start, end, replacement: &str| Fix {
            span: gossamer_lex::Span::new(file, start, end),
            replacement: replacement.to_string(),
            lint_id: "diagnostic",
        };
        let fixes = [edit(0, 4, "inner"), edit(0, 8, "outer")];
        assert_eq!(apply("abcdefgh tail", &fixes), "outer tail");
    }

    #[test]
    fn double_negation_is_collapsed_to_the_inner_operand() {
        let src = "fn f() -> bool { let x = true; !!x }\n";
        let out = run(src);
        assert!(!out.contains("!!x"), "not stripped: {out}");
        assert!(out.contains(" x }"), "inner missing: {out}");
    }

    #[test]
    fn unused_variable_gets_underscore_prefix() {
        let src = "fn f() { let foo = 1; }\n";
        let out = run(src);
        assert!(out.contains("let _foo = 1"), "got: {out}");
    }

    #[test]
    fn unused_mutable_variable_prefixes_the_name() {
        let src = "fn f() { let mut foo = 1; }\n";
        let out = run(src);
        assert!(out.contains("let _foo = 1"), "got: {out}");
        assert!(!out.contains("_mut"), "got: {out}");
    }

    #[test]
    fn unused_mut_keyword_is_removed() {
        // The binding is only read as a value, so nothing here needs the
        // `mut`. A method call on it would, because a receiver-mutating
        // method is indistinguishable from a reading one at this level.
        let src = "fn f() -> i64 { let mut x = 1; x + 1 }\n";
        let out = run(src);
        assert!(!out.contains("mut x"), "still has mut: {out}");
        assert!(out.contains("let x = 1"), "got: {out}");
    }

    #[test]
    fn comparison_to_bool_literal_drops_the_equality() {
        let src = "fn f(a: bool) -> bool { a == true }\n";
        let out = run(src);
        assert!(out.contains("{ a }"), "got: {out}");
    }

    #[test]
    fn needless_bool_collapses_to_the_condition() {
        let src = "fn f(a: bool) -> bool { if a { true } else { false } }\n";
        let out = run(src);
        assert!(out.contains("{ a }"), "got: {out}");
    }

    #[test]
    fn mut_survives_a_mutating_method_call() {
        let src = "fn f() { let mut xs: Vec<i64> = #[]\n xs.push(1) }\n";
        let out = run(src);
        assert!(out.contains("let mut xs"), "got: {out}");
    }

    #[test]
    fn mut_survives_a_mutable_borrow() {
        let src =
            "fn g(v: &mut Vec<u8>) { }\nfn f() { let mut out: Vec<u8> = #[]\n g(&mut out) }\n";
        let out = run(src);
        assert!(out.contains("let mut out"), "got: {out}");
    }

    #[test]
    fn mut_survives_a_compound_assignment() {
        let src = "fn f() { let mut n = 0\n n += 1 }\n";
        let out = run(src);
        assert!(out.contains("let mut n"), "got: {out}");
    }
}
