//! `regex::compile` and `sql::statement` check a literal argument at parse time.

use gossamer_lex::SourceMap;
use gossamer_parse::{ParseDiagnostic, ParseError, parse_source_file};

fn diagnostics(body: &str) -> (String, Vec<ParseDiagnostic>) {
    let source = format!("use std::regex\nfn main() {{\n    {body}\n}}\n");
    let mut map = SourceMap::new();
    let file = map.add_file("t.gos", source.clone());
    let (_, diags) = parse_source_file(&source, file);
    (source, diags)
}

#[test]
fn a_literal_pattern_that_does_not_compile_is_reported_at_the_literal() {
    let (source, diags) = diagnostics("let r = regex::compile(\"a(b\")");
    let [diag] = diags.as_slice() else {
        panic!("{diags:?}");
    };
    let ParseError::InvalidRegexLiteral { reason } = &diag.error else {
        panic!("{diags:?}");
    };
    assert_eq!(reason, "unclosed group");
    assert_eq!(
        &source[diag.span.start as usize..diag.span.end as usize],
        "\"a(b\""
    );
}

#[test]
fn a_pattern_that_compiles_or_is_built_at_run_time_is_accepted() {
    for body in [
        "let r = regex::compile(\"^\\\\d{4}-\\\\p{L}+$\")",
        "let p = \"a(b\"\n    let r = regex::compile(p)",
    ] {
        let (_, diags) = diagnostics(body);
        assert!(diags.is_empty(), "{body}: {diags:?}");
    }
}

#[test]
fn a_malformed_literal_statement_is_reported() {
    for (body, expected) in [
        ("let q = sql::statement(\"\")", "the statement is empty"),
        (
            "let q = sql::statement(\"SELECT (1\")",
            "a `(` is never closed",
        ),
        (
            "let q = sql::statement(\"SELECT 1)\")",
            "a `)` closes nothing",
        ),
    ] {
        let (_, diags) = diagnostics(body);
        let [diag] = diags.as_slice() else {
            panic!("{body}: {diags:?}");
        };
        assert!(
            matches!(&diag.error, ParseError::InvalidSqlStatement { reason } if reason == expected),
            "{body}: {diags:?}"
        );
    }
}

#[test]
fn a_well_formed_literal_statement_is_accepted() {
    let (_, diags) = diagnostics("let q = sql::statement(\"SELECT count(id) FROM users\")");
    assert!(diags.is_empty(), "{diags:?}");
}
