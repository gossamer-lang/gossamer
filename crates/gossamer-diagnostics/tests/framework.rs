//! Framework-level tests for gossamer-diagnostics (Stream B.2).

use gossamer_diagnostics::{
    Code, Diagnostic, Label, Location, RenderOptions, Sink, Suggestion, edit_distance, render,
    render_plain, suggest,
};
use gossamer_lex::{SourceMap, Span};

#[test]
fn diagnostic_builder_threads_labels_notes_helps_and_fixes() {
    let code = Code("GP0001");
    let mut map = SourceMap::new();
    let file = map.add_file("snippet.gos", "let x = 1\n");
    let loc = Location::new(file, Span::new(file, 4, 5));
    let diag = Diagnostic::error(code, "expected identifier")
        .with_primary(loc, "here")
        .with_secondary(loc, "declared here")
        .with_note("identifiers must start with a letter")
        .with_help("use `let x`")
        .with_suggestion(Suggestion::replacement(loc, "rename", "ident"));
    assert_eq!(diag.code, code);
    assert_eq!(diag.labels.len(), 2);
    assert_eq!(diag.notes.len(), 1);
    assert_eq!(diag.helps.len(), 1);
    assert_eq!(diag.suggestions.len(), 1);
    assert!(diag.primary_label().is_some());
}

#[test]
fn sink_counts_errors_and_drains() {
    let mut sink = Sink::new();
    sink.push(Diagnostic::error(Code("GT0001"), "type mismatch"));
    sink.push(Diagnostic::warning(Code("GL0003"), "unused variable"));
    assert_eq!(sink.len(), 2);
    assert_eq!(sink.error_count(), 1);
    let drained = sink.drain();
    assert_eq!(drained.len(), 2);
    assert!(sink.is_empty());
}

#[test]
fn render_plain_includes_code_severity_and_primary_label() {
    let mut map = SourceMap::new();
    let file = map.add_file("x.gos", "fn main() {}\n");
    let loc = Location::new(file, Span::new(file, 0, 2));
    let diag =
        Diagnostic::error(Code("GP0002"), "unexpected token").with_primary(loc, "unexpected `fn`");
    let text = render_plain(&diag);
    assert!(text.contains("error"));
    assert!(text.contains("GP0002"));
    assert!(text.contains("unexpected token"));
    assert!(text.contains("unexpected `fn`"));
}

#[test]
fn render_puts_file_line_column_on_primary_label() {
    let mut map = SourceMap::new();
    let file = map.add_file("snippet.gos", "fn main() {\n    let x = 1\n}\n");
    let loc = Location::new(file, Span::new(file, 16, 17));
    let diag = Diagnostic::error(Code("GP0003"), "missing semicolon")
        .with_primary(loc, "expected after statement");
    let rendered = render(&diag, &map, RenderOptions::default());
    assert!(rendered.contains("snippet.gos:2:"));
    assert!(rendered.contains("expected after statement"));
    assert!(rendered.contains("error[GP0003]"));
}

#[test]
fn edit_distance_matches_known_values() {
    assert_eq!(edit_distance("", ""), 0);
    assert_eq!(edit_distance("kitten", "sitting"), 3);
    assert_eq!(edit_distance("foo", "foo"), 0);
    assert_eq!(edit_distance("foo", "bar"), 3);
}

#[test]
fn suggest_returns_closest_within_budget() {
    let candidates = ["foo", "fooo", "bar", "baz"];
    let best = suggest("foa", candidates.iter().copied(), 2);
    assert_eq!(best, Some("foo"));

    let none = suggest("qux", candidates.iter().copied(), 1);
    assert!(none.is_none());
}

#[test]
fn suggest_settles_an_equal_distance_by_name_whatever_the_order() {
    let forward = suggest("lint", ["print", "min"].iter().copied(), 2);
    let backward = suggest("lint", ["min", "print"].iter().copied(), 2);
    assert_eq!(forward, Some("min"));
    assert_eq!(backward, Some("min"));
}

#[test]
fn primary_and_secondary_labels_use_distinct_prefixes() {
    let mut map = SourceMap::new();
    let file = map.add_file("x.gos", "abc\n");
    let primary = Label::primary(Location::new(file, Span::new(file, 0, 1)), "primary");
    let secondary = Label::secondary(Location::new(file, Span::new(file, 2, 3)), "secondary");
    let diag = Diagnostic::error(Code("GP0001"), "demo")
        .with_label(primary)
        .with_label(secondary);
    let text = render(&diag, &map, RenderOptions::default());
    assert!(text.contains("-->"), "primary marker missing: {text}");
    assert!(text.contains("::>"), "secondary marker missing: {text}");
}
