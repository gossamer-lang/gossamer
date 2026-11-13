//! Faithfulness tests for the token-stream formatter behind `gos fmt`.
//!
//! Runs `format_source` over the whole `.gos` corpus (`examples/` and
//! `feature-testing-examples/`) and asserts the three core guarantees:
//! token equivalence (no code token altered, merged, or dropped),
//! comment preservation (count and content unchanged), and
//! idempotency (`fmt(fmt(x)) == fmt(x)`), plus targeted regressions
//! for the constructs the old AST printer used to destroy.

use std::path::PathBuf;

use gossamer_lex::{FileId, SourceMap, TokenKind, tokenize};
use gossamer_parse::format_source;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn corpus_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in ["examples", "feature-testing-examples", "conformance"] {
        collect_gos(&workspace_root().join(dir), &mut files);
    }
    files.sort();
    files
}

/// Walks `dir` for `.gos` sources, nested project layouts included, so a
/// source under `examples/projects/*/src/` is held to the same
/// formatting contract as one at the top level.
fn collect_gos(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_gos(&path, files);
        } else if path.extension().and_then(|s| s.to_str()) == Some("gos") {
            files.push(path);
        }
    }
}

fn file_id(map: &mut SourceMap, name: &str, source: &str) -> FileId {
    map.add_file(name.to_string(), source.to_string())
}

fn fmt(source: &str) -> String {
    let mut map = SourceMap::new();
    let file = file_id(&mut map, "fixture.gos", source);
    format_source(source, file).expect("format fixture")
}

#[test]
fn removes_optional_line_ending_semicolons() {
    let source = "use std::strings;\nfn main() {\n    let value = 1;\n    println(value);\n}\n";
    let formatted = fmt(source);
    assert_eq!(
        formatted,
        "use std::strings\n\nfn main() {\n    let value = 1\n    println(value)\n}\n"
    );
}

fn significant(source: &str) -> Vec<(TokenKind, String)> {
    let mut map = SourceMap::new();
    let file = file_id(&mut map, "sig.gos", source);
    let (tokens, errs) = tokenize(source, file);
    assert!(errs.is_empty(), "lex errors in input");
    tokens
        .iter()
        .enumerate()
        .filter(|(index, token)| {
            if matches!(token.kind, TokenKind::Whitespace | TokenKind::Eof) {
                return false;
            }
            if token.kind == TokenKind::Punct(gossamer_lex::Punct::Semi) {
                let next = tokens[index + 1..]
                    .iter()
                    .find(|next| !matches!(next.kind, TokenKind::Whitespace));
                let next_start = next.map_or(source.len(), |next| next.span.start as usize);
                if source[token.span.end as usize..next_start].contains('\n')
                    || next.is_none_or(|next| {
                        matches!(
                            next.kind,
                            TokenKind::Punct(gossamer_lex::Punct::RBrace) | TokenKind::Eof
                        )
                    })
                {
                    return false;
                }
            }
            if token.kind != TokenKind::Punct(gossamer_lex::Punct::Comma) {
                return true;
            }
            let next_start = tokens[index + 1..]
                .iter()
                .find(|next| {
                    !matches!(
                        next.kind,
                        TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment
                    )
                })
                .map_or(source.len(), |next| next.span.start as usize);
            !source[token.span.end as usize..next_start].contains('\n')
        })
        .map(|(_, t)| {
            let text = &source[t.span.start as usize..t.span.end as usize];
            // A triple-quoted literal's indentation is layout the
            // formatter owns, so compare what the literal means.
            let value = if t.kind == TokenKind::TripleStringLit {
                let parsed = gossamer_lex::triple_string(text);
                format!("{}\u{0}{}", parsed.body(), parsed.closer_on_own_line)
            } else {
                text.to_string()
            };
            (t.kind, value)
        })
        .collect()
}

fn comments(source: &str) -> Vec<String> {
    significant(source)
        .into_iter()
        .filter(|(kind, _)| matches!(kind, TokenKind::LineComment | TokenKind::BlockComment))
        .map(|(_, text)| text)
        .collect()
}

/// (a) Every corpus file formats successfully and the output's
/// non-whitespace token stream - comments included - is identical to
/// the input's.
#[test]
fn corpus_token_equivalence_after_fmt() {
    let files = corpus_files();
    assert!(
        files.len() > 150,
        "corpus unexpectedly small: {}",
        files.len()
    );
    let mut formatted_count = 0usize;
    for path in &files {
        let source = std::fs::read_to_string(path).expect("read corpus file");
        let mut map = SourceMap::new();
        let file = file_id(&mut map, &path.to_string_lossy(), &source);
        let formatted = format_source(&source, file)
            .unwrap_or_else(|e| panic!("format {}: {e}", path.display()));
        assert_eq!(
            significant(&source),
            significant(&formatted),
            "token stream changed for {}",
            path.display()
        );
        formatted_count += 1;
    }
    assert_eq!(formatted_count, files.len());
}

/// (b) Formatting is idempotent across the whole corpus.
#[test]
fn corpus_fmt_is_idempotent() {
    for path in corpus_files() {
        let source = std::fs::read_to_string(&path).expect("read corpus file");
        let once = fmt(&source);
        let twice = fmt(&once);
        assert_eq!(once, twice, "fmt not idempotent on {}", path.display());
    }
}

/// (c) Comment count and content survive formatting byte-for-byte on
/// every corpus file.
#[test]
fn corpus_comments_preserved() {
    for path in corpus_files() {
        let source = std::fs::read_to_string(&path).expect("read corpus file");
        let formatted = fmt(&source);
        assert_eq!(
            comments(&source),
            comments(&formatted),
            "comments changed in {}",
            path.display()
        );
    }
}

#[test]
fn trailing_comments_stay_trailing() {
    let source =
        "fn main() {\n    let port = 8080  // matches the nginx upstream\n    serve(port)\n}\n";
    assert_eq!(fmt(source), source);
}

#[test]
fn block_comments_survive_inline_and_standalone() {
    let source = "/* header */\nfn main() {\n    let x = 1 /* inline */ + 2\n    /*\n     * multi-line body\n     */\n    use_it(x)\n}\n";
    assert_eq!(fmt(source), source);
}

#[test]
fn comments_inside_match_arms_and_chains() {
    let source = "fn label(n: i64) -> String {\n    match n {\n        // negative side\n        x if x < 0 => \"neg\",\n        // everything else\n        _ => \"pos\",\n    }\n}\n\nfn chain(input: [i64]) -> i64 {\n    input\n        // keep evens only\n        |> |v| filter(|n: i64| n % 2 == 0, v)\n        |> count\n}\n";
    let expected = "fn label(n: i64) -> String {\n    match n {\n        // negative side\n        x if x < 0 => \"neg\"\n        // everything else\n        _ => \"pos\"\n    }\n}\n\nfn chain(input: [i64]) -> i64 {\n    input\n        // keep evens only\n        |> |v| filter(|n: i64| n % 2 == 0, v)\n        |> count\n}\n";
    assert_eq!(fmt(source), expected);
}

#[test]
fn match_arm_commas_before_trailing_comments_are_removed() {
    let source =
        "match a {\n    1 => a + 1, // line comment\n    2 => a + 2, /* block comment */\n}\n";
    let expected =
        "match a {\n    1 => a + 1 // line comment\n    2 => a + 2 /* block comment */\n}\n";
    assert_eq!(fmt(source), expected);
}

#[test]
fn multiline_parameters_align_after_generic_types() {
    let source = "fn many_params(\n    one: Vec<i64>\n    two: i64\n    three: Vec<Vec<String>>\n    four: i64\n) {\n    one[0] + two + four\n}\n";
    let once = fmt(source);
    assert_eq!(once, source);
    assert_eq!(fmt(&once), once);
}

#[test]
fn println_and_format_macros_never_rewritten() {
    let source = "fn main() {\n    let name = \"jane\"\n    let msg = format(\"hello, {name}!\")\n    println(\"{} -> {}\", name, msg)\n    eprintln(\"warn: {msg}\")\n}\n";
    let out = fmt(source);
    assert_eq!(out, source);
    assert!(!out.contains("__concat"), "macro desugared to __concat");
}

#[test]
fn struct_literals_keep_keyed_forms() {
    let source = "struct Account { owner: String, balance: i64 }\n\nfn main() {\n    let keyed = Account { owner: \"jane\", balance: 1200 }\n}\n";
    assert_eq!(fmt(source), source);
}

#[test]
fn pipe_chains_keep_authored_breaks_and_indent() {
    let source = "fn main() {\n    let words = \"  Hello  World  \"\n        |> split_words\n        |> |v| filter(|w: String| w.len() > 0, v)\n        |> count\n    println(\"words: {words}\")\n}\n";
    assert_eq!(fmt(source), source);
}

/// The historical mangling case: a hand-written forwarder with a port
/// number in a comment. The old AST printer deleted every comment and
/// rewrote `println!` into `__concat`; the faithful formatter must
/// return this file byte-identical.
#[test]
fn port_forwarder_fixture_round_trips_byte_identical() {
    let source = "use std::net\n\n// Local URL forwarder.\n// Listens on 127.0.0.1:8080 and forwards to the upstream below.\nconst UPSTREAM: String = \"127.0.0.1:9090\"  // staging box\n\nfn main() {\n    let listener = net::TcpListener::bind(\"127.0.0.1:8080\")\n    // accept loop: one goroutine per connection\n    loop {\n        let stream = listener.accept()\n        spawn(|| forward(stream))\n    }\n}\n\nfn forward(stream: net::TcpStream) {\n    /* copy bytes both ways until either side closes */\n    let upstream = net::TcpStream::connect(UPSTREAM)\n    println(\"forwarding to {}\", UPSTREAM)\n}\n";
    assert_eq!(fmt(source), source);
    assert_eq!(comments(source).len(), 5);
}

#[test]
fn whitespace_normalization_still_happens() {
    assert_eq!(
        fmt("fn    main(  )   {   let x=1+2\n}\n"),
        "fn main() { let x = 1 + 2\n}\n"
    );
}

#[test]
fn check_failures_do_not_modify_anything() {
    let mut map = SourceMap::new();
    let source = "fn broken( {\n";
    let file = map.add_file("broken.gos".to_string(), source.to_string());
    assert!(format_source(source, file).is_err());
}

/// The dedented body of the first triple-quoted token in `source`.
fn triple_body(source: &str) -> String {
    let mut map = SourceMap::new();
    let file = file_id(&mut map, "body.gos", source);
    let (tokens, _errs) = tokenize(source, file);
    let token = tokens
        .iter()
        .find(|t| t.kind == TokenKind::TripleStringLit)
        .expect("triple-quoted token");
    gossamer_lex::triple_string(&source[token.span.start as usize..token.span.end as usize]).body()
}

#[test]
fn triple_quoted_body_moves_with_the_line_that_opens_it() {
    let source =
        "fn main() {\n  let text = \"\"\"\n  <html>\n      <body>\n  </html>\n  \"\"\"\n}\n";
    assert_eq!(
        fmt(source),
        "fn main() {\n    let text = \"\"\"\n    <html>\n        <body>\n    </html>\n    \"\"\"\n}\n"
    );
}

#[test]
fn triple_quoted_reindent_is_idempotent() {
    let source = "fn main() {\n  let text = \"\"\"\n      deep\n  \"\"\"\n}\n";
    let once = fmt(source);
    let twice = fmt(&once);
    assert_eq!(once, twice, "second pass moved the body again");
}

#[test]
fn triple_quoted_reindent_preserves_the_value() {
    let source = "fn main() {\n  let text = \"\"\"\n  a\n      b\n\n  c\n  \"\"\"\n}\n";
    let formatted = fmt(source);
    assert_eq!(
        triple_body(source),
        triple_body(&formatted),
        "fmt changed the literal's value"
    );
}

#[test]
fn single_line_triple_quoted_literal_is_left_alone() {
    let source = "fn main() {\n    let text = \"\"\"a\"b\"\"\"\n}\n";
    assert_eq!(fmt(source), source);
}

#[test]
fn a_triple_quoted_literal_hugging_its_closer_keeps_that_shape() {
    let source = "fn main() {\n  let text = \"\"\"\n  one\n  two\"\"\"\n}\n";
    let formatted = fmt(source);
    assert_eq!(
        formatted,
        "fn main() {\n    let text = \"\"\"\n    one\n    two\"\"\"\n}\n"
    );
    assert_eq!(triple_body(source), triple_body(&formatted));
}

#[test]
fn a_triple_quoted_literal_in_a_nested_call_follows_its_line() {
    let source =
        "fn main() {\n    println(\n        \"{}\",\n        \"\"\"\n  body\n  \"\"\"\n    )\n}\n";
    let formatted = fmt(source);
    assert!(
        formatted.contains("        \"\"\"\n        body\n        \"\"\""),
        "nested literal did not follow its line:\n{formatted}"
    );
    assert_eq!(triple_body(source), triple_body(&formatted));
}

#[test]
fn an_empty_triple_quoted_literal_does_not_grow_a_line() {
    let source = "fn main() {\n    let text = \"\"\"\n    \"\"\"\n}\n";
    assert_eq!(fmt(source), source);
}

#[test]
fn a_blank_content_line_survives_reindenting() {
    let source = "fn main() {\n    let text = \"\"\"\n\n    \"\"\"\n}\n";
    let formatted = fmt(source);
    assert_eq!(formatted, source);
    assert_eq!(triple_body(source), triple_body(&formatted));
}

/// (d) Every shipped `.gos` source is already in canonical form, so a
/// contributor running `gos fmt` sees no unrelated churn.
#[test]
fn corpus_is_already_formatted() {
    let mut drifted = Vec::new();
    for path in corpus_files() {
        let source = std::fs::read_to_string(&path).expect("read corpus file");
        if fmt(&source) != source {
            drifted.push(path.display().to_string());
        }
    }
    assert!(
        drifted.is_empty(),
        "shipped sources that `gos fmt` would rewrite:\n{}",
        drifted.join("\n")
    );
}

#[test]
fn triple_quoted_block_follows_code_that_moves_right() {
    // The whole literal - opener, body, closer - travels with the
    // statement that opens it, and the value is unchanged.
    let source =
        "fn main() {\nif true {\nlet text = \"\"\"\n<html>\n    <body>\n</html>\n\"\"\"\n}\n}\n";
    let formatted = fmt(source);
    assert_eq!(
        formatted,
        "fn main() {\n    if true {\n        let text = \"\"\"\n        <html>\n            <body>\n        </html>\n        \"\"\"\n    }\n}\n"
    );
    assert_eq!(triple_body(source), triple_body(&formatted));
}

#[test]
fn triple_quoted_block_follows_code_that_moves_left() {
    let source = "fn main() {\n            let text = \"\"\"\n            one\n              two\n            \"\"\"\n}\n";
    let formatted = fmt(source);
    assert_eq!(
        formatted,
        "fn main() {\n    let text = \"\"\"\n    one\n      two\n    \"\"\"\n}\n"
    );
    assert_eq!(triple_body(source), triple_body(&formatted));
}

#[test]
fn a_closer_indented_less_than_the_body_keeps_the_offset_it_declares() {
    // The closing delimiter is the value's baseline, so a body written
    // past it carries that extra indentation into the string. Moving the
    // pair as a unit is what keeps the value the same.
    let source = "fn main() {\n  let text = \"\"\"\n      aaa\n        bbb\n  \"\"\"\n}\n";
    let formatted = fmt(source);
    assert_eq!(
        formatted,
        "fn main() {\n    let text = \"\"\"\n        aaa\n          bbb\n    \"\"\"\n}\n"
    );
    assert_eq!(triple_body(source), triple_body(&formatted));
    assert_eq!(triple_body(source), "    aaa\n      bbb");
}

#[test]
fn triple_quoted_leading_and_trailing_blank_lines_survive_reindenting() {
    let source = "fn main() {\n  let text = \"\"\"\n\n  a\n\n    b\n\n  \"\"\"\n}\n";
    let formatted = fmt(source);
    assert_eq!(
        formatted,
        "fn main() {\n    let text = \"\"\"\n\n    a\n\n      b\n\n    \"\"\"\n}\n"
    );
    assert_eq!(triple_body(source), triple_body(&formatted));
    assert_eq!(triple_body(source), "\na\n\n  b\n");
    assert_eq!(fmt(&formatted), formatted, "second pass moved the body");
}

#[test]
fn a_body_indented_less_than_its_opener_moves_out_to_the_statement() {
    // Every content line and the closer share a zero-width prefix here,
    // so the block reindents to the statement's column with the value
    // untouched.
    let source = "fn main() {\n    let text = \"\"\"\naaa\n  bbb\n\"\"\"\n}\n";
    let formatted = fmt(source);
    assert_eq!(
        formatted,
        "fn main() {\n    let text = \"\"\"\n    aaa\n      bbb\n    \"\"\"\n}\n"
    );
    assert_eq!(triple_body(source), triple_body(&formatted));
    assert_eq!(triple_body(source), "aaa\n  bbb");
}

#[test]
fn a_triple_quoted_argument_keeps_its_value_when_the_call_moves() {
    let source = "fn main() {\nprintln(\"{}\", \"\"\"\none\n two\n\"\"\")\n}\n";
    let formatted = fmt(source);
    assert_eq!(triple_body(source), triple_body(&formatted));
    assert_eq!(fmt(&formatted), formatted, "second pass moved the body");
}

#[test]
fn package_visibility_keeps_its_paren_against_pub() {
    let source = "pub(package) fn f() -> i64 {\n    1\n}\n\npub(package) struct S {\n    pub(package) a: i64\n}\n\nstruct P(pub (i64, i64))\n";
    assert_eq!(fmt(source), source);
    assert_eq!(fmt("pub (package) fn g() {}\n"), "pub(package) fn g() {}\n");
}
