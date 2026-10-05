//! Parser from Gossamer tokens to AST.
//! The parser is a hand-written recursive descent driver with a Pratt
//! loop for expressions. It consumes the lexer's token stream directly
//! and emits a best-effort `SourceFile` alongside a list of diagnostics.
//! The parser never panics on malformed input - unexpected tokens
//! resynchronise to the next item or statement boundary and are
//! reported via `ParseDiagnostic`.

#![forbid(unsafe_code)]

/// Compile-time source augmentation for derives, serde helpers, and rewrites.
pub mod autoderive;
pub mod builtin_macros;
mod cohort;
mod declined;
mod diagnostic;
mod entry_main;
mod expressions;
mod format;
mod generics;
mod items;
mod parser;
mod patterns;
mod recovery;
mod statements;
mod stream;
mod types;
mod use_decls;

pub use diagnostic::{ParseDiagnostic, ParseError, SerdeTargetRefusal};
pub use entry_main::synthesize_entry_main;
pub use format::{FormatError, format_source};
pub use parser::Parser;
pub use stream::{DocKind, StoredComment, TokenStream};

use gossamer_ast::SourceFile;
use gossamer_lex::{FileId, Keyword};

/// Parses `source` into a `SourceFile` AST and returns any diagnostics
/// collected along the way.
#[must_use]
pub fn parse_source_file(source: &str, file: FileId) -> (SourceFile, Vec<ParseDiagnostic>) {
    let mut parser = Parser::new(source, file);
    let file_attrs = parser.parse_file_attrs_public();
    let mut uses = Vec::new();
    loop {
        if parser.at_keyword_public(Keyword::Use) {
            uses.push(parser.parse_use_decl());
        } else if parser.at_attributed_use() {
            uses.push(parser.parse_attributed_use());
        } else {
            break;
        }
    }
    let mut items = Vec::new();
    let mut top_level_stmts = Vec::new();
    while !parser.at_eof_public() {
        let before = parser.checkpoint_public();
        if crate::recovery::is_item_start(&parser) {
            items.extend(parser.parse_item_group());
        } else {
            // At file scope a non-item token begins a bare statement: the
            // entry file is implicitly `fn main`, so its top-level code is
            // collected here and wrapped by `synthesize_entry_main`. In a
            // non-entry file every such token sits inside a `mod { }` body,
            // which is parsed elsewhere as items only.
            top_level_stmts.push(parser.parse_stmt());
        }
        if parser.checkpoint_public() == before {
            // The item/stmt parser left us where we started - guarantee
            // forward progress so an adversarial input cannot pin the loop
            // and blow the buffers up to gigabytes of stub allocations.
            parser.bump_public();
            parser.recover_to_item_start_public();
        }
    }
    // Pull `use` decls hoisted out of inline `mod ... { ... }` bodies
    // up to the source-file level so the resolver's top-level
    // `collect_imports` walk picks them up. Whether a single-segment
    // `use NAME` names a sibling module or a dependency package is a
    // question about the unit's module table, so the resolver decides
    // what such an import binds.
    uses.extend(parser.take_hoisted_uses());
    let next_node_id = parser.ids.issued();
    let named_args = parser.take_named_args();
    let mut source_file = SourceFile::new(file, uses, items);
    source_file.attrs = file_attrs;
    source_file.top_level_stmts = top_level_stmts;
    source_file.next_node_id = next_node_id;
    source_file.named_args = named_args;
    gossamer_ast::cfg::apply_package_features(&mut source_file);
    let diagnostics = parser.take_diagnostics();
    (source_file, diagnostics)
}

// Public shims so `parse_source_file` can talk to the parser across the
// module boundary without exposing internal helpers as part of the
// `Parser` type's public API.
impl Parser<'_> {
    /// Returns `true` when the cursor is at `keyword`. Public facade
    /// used by `parse_source_file`.
    #[must_use]
    pub fn at_keyword_public(&self, keyword: Keyword) -> bool {
        self.at_keyword(keyword)
    }

    /// Consumes the file's leading `#![..]` attributes. Public facade
    /// used by `parse_source_file`.
    pub fn parse_file_attrs_public(&mut self) -> gossamer_ast::Attrs {
        self.parse_file_attrs()
    }

    /// Returns `true` when the cursor is at end of input. Public facade
    /// used by `parse_source_file`.
    #[must_use]
    pub fn at_eof_public(&self) -> bool {
        self.at_eof()
    }

    /// Captures the current position for progress detection at the
    /// top-level item loop.
    #[must_use]
    pub fn checkpoint_public(&self) -> usize {
        self.tokens.checkpoint()
    }

    /// Public facade that forwards to the item-start recovery helper.
    pub fn recover_to_item_start_public(&mut self) {
        self.recover_to_item_start();
    }

    /// Single-token advance used as the outer-loop's last-resort
    /// progress guarantee when an item-parser is unable to recover.
    pub fn bump_public(&mut self) {
        if !self.at_eof() {
            self.bump();
        }
    }
}

/// The injected function a `{:+}` placeholder renders through.
pub(crate) const FORMAT_SIGN_HELPER: &str = "__gos_fmt_sign";

/// The injected function a `{:e}` placeholder renders through.
pub(crate) const FORMAT_EXPONENT_HELPER: &str = "__gos_fmt_exp";

#[cfg(test)]
mod top_level_stmt_tests {
    use super::*;
    use gossamer_lex::SourceMap;

    const RETAINED_OOM_REPRO: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fuzz/artifacts/parse/oom-97f06d27482345e26b63de65f5dc42de1cfc4a1c"
    ));

    fn parse(src: &str) -> (SourceFile, Vec<ParseDiagnostic>) {
        let mut map = SourceMap::new();
        let file = map.add_file("t.gos", src.to_string());
        parse_source_file(src, file)
    }

    #[test]
    fn top_level_statement_is_collected_not_error() {
        let (sf, diags) = parse("println(\"hi\")\n");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        assert_eq!(sf.top_level_stmts.len(), 1);
        assert!(sf.next_node_id > 0);
    }

    #[test]
    fn top_level_items_still_parse_alongside_statements() {
        let src = "fn helper() -> i64 { 1 }\nlet x = helper()\nprintln(\"{}\", x)\n";
        let (sf, diags) = parse(src);
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        assert_eq!(sf.items.len(), 1, "helper() should be a hoisted item");
        assert_eq!(sf.top_level_stmts.len(), 2, "let + println are statements");
    }

    #[test]
    fn statement_in_mod_body_is_clear_error() {
        let (_sf, diags) = parse("mod helper {\n    println(\"no\")\n}\n");
        assert!(!diags.is_empty(), "expected a diagnostic");
        assert!(
            diags
                .iter()
                .any(|d| matches!(d.error, ParseError::StatementOutsideEntry)),
            "expected StatementOutsideEntry, got: {diags:?}"
        );
    }

    #[test]
    fn missing_item_bodies_do_not_consume_following_items() {
        for source in [
            "enum Nothing\nfn next() {}\n",
            "trait Missing\nfn next() {}\n",
            "impl Missing\nfn next() {}\n",
        ] {
            let (sf, diags) = parse(source);
            assert_eq!(
                diags.len(),
                1,
                "a missing item body should report only its opening delimiter: {source:?}; {diags:?}"
            );
            assert_eq!(
                sf.items.len(),
                2,
                "following item should still parse: {source:?}"
            );
        }
    }

    /// `mod name` with no body names a file-backed module, so it is a
    /// declaration rather than a missing body; the resolver reports
    /// GR0010 when no file stands behind it.
    #[test]
    fn bodiless_mod_declaration_parses() {
        let (sf, diags) = parse("mod missing\nfn next() {}\n");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(sf.items.len(), 2);
    }

    /// A formatting call is an ordinary call, so a step built from one is
    /// judged by the rule every argument-taking step is judged by.
    #[test]
    fn a_format_step_with_arguments_reports_the_step_shape() {
        let (_sf, diags) = parse("\"two\" |> println(\"one\")\n");
        assert!(
            diags
                .iter()
                .any(|diag| matches!(diag.error, ParseError::PipeStepNeedsClosure { .. })),
            "expected the step-shape diagnostic: {diags:?}"
        );
    }

    #[test]
    fn a_dollar_reports_that_the_placeholder_is_retired() {
        let (_sf, diags) =
            parse("fn main() { let xs = [7, 8, 9]\n println(\"{}\", xs |> $[1]) }\n");
        assert!(
            diags
                .iter()
                .any(|diag| matches!(diag.error, ParseError::PipePlaceholderRetired)),
            "expected the retired-placeholder diagnostic: {diags:?}"
        );
    }

    #[test]
    fn a_step_that_writes_its_arguments_reports_the_closure_rewrite() {
        let (_sf, diags) = parse("fn main() { let n = 7 |> two(100) }\n");
        let step = diags
            .iter()
            .find_map(|diag| match &diag.error {
                ParseError::PipeStepNeedsClosure { replacement } => Some(replacement.clone()),
                _ => None,
            })
            .expect("expected the closure-step diagnostic");
        assert_eq!(step.as_deref(), Some("|v| two(v, 100)"));
    }

    #[test]
    fn retained_parse_oom_reproducer_terminates_with_diagnostics() {
        let source = std::str::from_utf8(RETAINED_OOM_REPRO).unwrap();
        let mut map = SourceMap::new();
        let file = map.add_file("parse-oom-repro.gos", source.to_owned());
        let (_ast, diagnostics) = parse_source_file(source, file);
        assert!(!diagnostics.is_empty());
    }

    #[test]
    fn malformed_map_value_at_eof_does_not_loop() {
        let source = "upb {\na, \0\0\0\0\u{e} {\na>x:>>";
        let mut map = SourceMap::new();
        let file = map.add_file("map-eof-oom-repro.gos", source.to_owned());
        let (ast, diagnostics) = parse_source_file(source, file);
        assert!(!diagnostics.is_empty());
        assert!(
            ast.next_node_id < 100,
            "malformed input synthesized too many AST nodes: {}",
            ast.next_node_id
        );
    }
}
