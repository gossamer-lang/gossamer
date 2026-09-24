#![allow(missing_docs)]

use gossamer_ast::{ExprKind, ItemKind, StmtKind};
use gossamer_lex::SourceMap;
use gossamer_parse::{ParseError, parse_source_file};

fn example(name: &str) -> String {
    format!("{}/../../examples/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn hello_world_parses_cleanly() {
    let path = example("hello_world.gos");
    let source = std::fs::read_to_string(&path).unwrap();
    let mut map = SourceMap::new();
    let file = map.add_file(&path, source.clone());
    let (sf, diags) = parse_source_file(&source, file);
    eprintln!(
        "hello_world: {} uses, {} items, {} diags",
        sf.uses.len(),
        sf.items.len(),
        diags.len()
    );
    for diag in &diags {
        eprintln!("  {diag}");
    }
    assert!(diags.is_empty(), "diagnostics should be empty");
}

#[test]
fn web_server_parses_cleanly() {
    let path = example("web_server.gos");
    let source = std::fs::read_to_string(&path).unwrap();
    let mut map = SourceMap::new();
    let file = map.add_file(&path, source.clone());
    let (sf, diags) = parse_source_file(&source, file);
    eprintln!(
        "web_server: {} uses, {} items, {} diags",
        sf.uses.len(),
        sf.items.len(),
        diags.len()
    );
    for diag in &diags {
        eprintln!("  {diag}");
    }
    assert!(diags.is_empty(), "diagnostics should be empty");
}

#[test]
fn line_count_parses_cleanly() {
    let path = example("line_count.gos");
    let source = std::fs::read_to_string(&path).unwrap();
    let mut map = SourceMap::new();
    let file = map.add_file(&path, source.clone());
    let (sf, diags) = parse_source_file(&source, file);
    eprintln!(
        "line_count: {} uses, {} items, {} diags",
        sf.uses.len(),
        sf.items.len(),
        diags.len()
    );
    for diag in &diags {
        eprintln!("  {diag}");
    }
    assert!(diags.is_empty(), "diagnostics should be empty");
}

/// Regression: `expr as i64 < width` must parse as a comparison, not as
/// the start of a generic argument list on `i64`. The bug surfaced
/// when the formatter stripped redundant parens from
/// `(out.len() as i64) < width` in `examples/list_dir.gos`. The fix
/// restricts `parse_type_path_segment` from consuming `<` after a
/// primitive type name (primitives never carry generics).
#[test]
fn cast_to_primitive_followed_by_lt_parses_as_comparison() {
    let source = "fn pad(s: i64, width: i64) {\n    while s as i64 < width {\n    }\n}\n";
    let mut map = SourceMap::new();
    let file = map.add_file("cast_lt.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    for diag in &diags {
        eprintln!("  {diag}");
    }
    assert!(
        diags.is_empty(),
        "cast-then-comparison must not produce parse diagnostics; got {} diag(s)",
        diags.len()
    );
    assert_eq!(sf.items.len(), 1, "expected exactly one item (`fn pad`)");
}

/// Companion regression: `Vec<i64>` and friends must still parse as a
/// generic type argument list. The primitive-only narrowing in the
/// fix above should not regress generics on user / stdlib types.
#[test]
fn generic_arg_list_on_user_type_still_parses() {
    let source = "fn build() -> Vec<i64> {\n    Vec::new()\n}\n";
    let mut map = SourceMap::new();
    let file = map.add_file("vec_generic.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    for diag in &diags {
        eprintln!("  {diag}");
    }
    assert!(
        diags.is_empty(),
        "Vec<i64> must still parse cleanly; got {} diag(s)",
        diags.len()
    );
    assert_eq!(sf.items.len(), 1);
}

#[test]
fn fixed_array_literal_at_statement_start_is_not_parsed_as_attribute() {
    let source = "fn fixed() -> [i64; 2] {\n    #[1, 2]\n}\n";
    let mut map = SourceMap::new();
    let file = map.add_file("fixed_literal.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    for diag in &diags {
        eprintln!("  {diag}");
    }
    assert!(
        diags.is_empty(),
        "fixed array literal must parse cleanly; got {} diag(s)",
        diags.len()
    );
    assert_eq!(sf.items.len(), 1);
}

#[test]
fn empty_braces_stay_hash_map_in_let_initializer() {
    let source = "fn main() { let b = {} }\n";
    let mut map = SourceMap::new();
    let file = map.add_file("empty_map_literal.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    assert!(diags.is_empty(), "unexpected parse diagnostics: {diags:?}");
    let ItemKind::Fn(function) = &sf.items[0].kind else {
        panic!("expected function item");
    };
    let Some(body) = function.body.as_deref() else {
        panic!("expected function body");
    };
    let ExprKind::Block(block) = &body.kind else {
        panic!("expected function block");
    };
    let StmtKind::Let {
        init: Some(init), ..
    } = &block.stmts[0].kind
    else {
        panic!("expected initialized let statement");
    };
    assert!(
        matches!(init.kind, ExprKind::MapLiteral(ref entries) if entries.is_empty()),
        "expected empty map literal, got {:?}",
        init.kind
    );
}

#[test]
fn empty_braces_in_match_arm_are_empty_block() {
    let source = "fn main() { match Some(1) { Some(_) => {}, None => {} } }\n";
    let mut map = SourceMap::new();
    let file = map.add_file("empty_match_arm_block.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    assert!(diags.is_empty(), "unexpected parse diagnostics: {diags:?}");
    let ItemKind::Fn(function) = &sf.items[0].kind else {
        panic!("expected function item");
    };
    let Some(body) = function.body.as_deref() else {
        panic!("expected function body");
    };
    let ExprKind::Block(block) = &body.kind else {
        panic!("expected function block");
    };
    let Some(expr) = block.tail.as_deref() else {
        panic!("expected match expression tail");
    };
    let ExprKind::Match { arms, .. } = &expr.kind else {
        panic!("expected match expression");
    };
    for arm in arms {
        assert!(
            matches!(arm.body.kind, ExprKind::Block(ref block) if block.stmts.is_empty() && block.tail.is_none()),
            "expected empty block arm, got {:?}",
            arm.body.kind
        );
    }
}

/// A leading UTF-8 BOM (the Windows-editor default) is stripped at the
/// parse entry, so a BOM-prefixed file parses identically to a plain
/// one rather than choking on the marker.
#[test]
fn leading_bom_parses_like_plain_source() {
    let with = "\u{feff}fn main() {\n    let x = 1\n}\n";
    let without = "fn main() {\n    let x = 1\n}\n";
    let mut map = SourceMap::new();
    let fa = map.add_file("with_bom.gos", with.to_string());
    let fb = map.add_file("plain.gos", without.to_string());
    let (sf_a, diags_a) = parse_source_file(with, fa);
    let (sf_b, diags_b) = parse_source_file(without, fb);
    assert!(
        diags_a.is_empty(),
        "BOM-prefixed source must parse cleanly; got {} diag(s)",
        diags_a.len()
    );
    assert_eq!(sf_a.items.len(), sf_b.items.len());
    assert_eq!(diags_a.len(), diags_b.len());
}

/// Regression: a leading BOM shifted token spans off the parser's
/// source basis, so `Parser::slice` split the BOM mid-char and
/// panicked on this fuzz input (`fuzz/artifacts/typecheck/crash-fbf8…`).
/// Parsing arbitrary bytes must never panic.
#[test]
fn bom_prefixed_malformed_input_does_not_panic() {
    let src = "\u{feff}fn\"\u{4}n\"\u{4}";
    let mut map = SourceMap::new();
    let file = map.add_file("crash.gos", src.to_string());
    let _ = parse_source_file(src, file);
}

/// Regression: nested generic argument lists close with a maximal-munch
/// `>>` (or `>>=` / `>=`) token. The type/turbofish/generic-param parsers
/// must split that token into the closing `>` for each level instead of
/// rejecting it (`Vec<Vec<String>>`, `HashMap<String, Vec<i64>>`).
#[test]
fn nested_generics_closing_shift_right_parses() {
    let source = concat!(
        "fn f(a: Vec<Vec<String>>, b: Vec<Vec<Vec<i64>>>) -> Vec<Vec<i64>> {\n",
        "    b\n",
        "}\n",
    );
    let mut map = SourceMap::new();
    let file = map.add_file("nested_generics.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    for diag in &diags {
        eprintln!("  {diag}");
    }
    assert!(
        diags.is_empty(),
        "nested generics with `>>` must parse cleanly; got {} diag(s)",
        diags.len()
    );
    assert_eq!(sf.items.len(), 1, "expected exactly one item (`fn f`)");
}

#[test]
fn unit_struct_declaration_accepts_bare_form() {
    let source = "struct Unit\n";
    let mut map = SourceMap::new();
    let file = map.add_file("unit_struct.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "`struct Unit` must parse cleanly; got {diags:?}"
    );
    assert_eq!(sf.items.len(), 1);
    let gossamer_ast::ItemKind::Struct(decl) = &sf.items[0].kind else {
        panic!("expected a struct item");
    };
    assert!(matches!(decl.body, gossamer_ast::StructBody::Unit));
}

#[test]
fn multiline_lists_use_newlines_and_tolerate_legacy_commas() {
    for source in [
        "struct Point {\n    x: i64\n    y: i64\n}\n",
        "struct Point {\n    x: i64,\n    y: i64,\n}\n",
        "enum Choice {\n    One\n    Two(i64)\n}\n",
        "fn add(\n    x: i64\n    y: i64\n) -> i64 { x + y }\nfn main() { add(\n    1\n    2\n) }\n",
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("newline_lists.gos", source.to_string());
        let (_, diags) = parse_source_file(source, file);
        assert!(diags.is_empty(), "`{source}` produced {diags:?}");
    }
}

#[test]
fn every_delimited_list_accepts_a_newline_separator() {
    for source in [
        "fn main() { let t = (\n    1\n    2\n)\n let _ = t }\n",
        "fn f() -> (\n    i64\n    i64\n) { (1, 2) }\n",
        "fn main() { let m = {\n    \"a\": 1\n    \"b\": 2\n}\n let _ = m }\n",
        "fn main() { let s = #{\n    1\n    2\n}\n let _ = s }\n",
        "fn main() { let v = #[\n    1\n    2\n]\n let _ = v }\n",
        "fn main() { let a: [i64; 2] = [\n    1\n    2\n]\n let _ = a }\n",
        "fn main() { let xs = #[1, 2]\n if let [\n    a\n    b\n] = xs { let _ = a + b } }\n",
        "struct P { x: i64, y: i64 }\nfn main() { let P {\n    x\n    y\n} = P { x: 1, y: 2 }\n let _ = x + y }\n",
        "use std::{\n    fs\n    env\n}\nfn main() {}\n",
        "fn f<\n    A\n    B\n>(a: A, b: B) -> i64 { 1 }\n",
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("newline_delimited.gos", source.to_string());
        let (_, diags) = parse_source_file(source, file);
        assert!(diags.is_empty(), "`{source}` produced {diags:?}");
    }
}

/// A newline separates list elements; it never turns the parenthesised
/// forms into one-element aggregates.
/// A newline separates list elements but consumes nothing, so a list loop
/// has to stop when the element between two of them consumed nothing either.
/// These inputs end mid-list, where an unguarded loop collects the same
/// failed element until it exhausts memory.
#[test]
fn an_unterminated_list_stops_instead_of_collecting_forever() {
    for source in [
        // A parameter pattern that opens a tuple and never closes it.
        "fn a&(\n ",
        // A generic list left open at a closing brace.
        "struct \nenum|+ name\n type< \n}\n",
        // Nested parentheses truncated at end of input.
        "((((((((((((((((1))))\n",
        // Every delimiter shape, opened and abandoned.
        "fn main() { let t = (1\n",
        "fn main() { let v = #[1\n",
        "fn main() { let m = {\"a\": 1\n",
        "fn main() { let s = #{1\n",
        "use std::{fs\n",
        "fn f<A\n",
        "fn main() { let [a\n",
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("unterminated.gos", source.to_string());
        // Reaching this assertion at all is the point: an unguarded list loop
        // never returns from `parse_source_file`.
        let (_, diags) = parse_source_file(source, file);
        assert!(
            !diags.is_empty(),
            "`{source}` is not a complete program and must report why"
        );
    }
}

#[test]
fn a_binding_lists_its_targets_without_parentheses() {
    for source in [
        "fn main() { let a, b = (1, 2)\n let _ = a + b }\n",
        "fn main() { let mut a, mut b = 7, 8\n a, b += 1, 2\n let _ = a + b }\n",
        "fn main() { let a, (b, c) = 1, (2, 3)\n let _ = a + b + c }\n",
        "fn main() { let mut a = 1\n let mut b = 2\n a, b = b, a\n let _ = a }\n",
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("target_list.gos", source.to_string());
        let (_, diags) = parse_source_file(source, file);
        assert!(diags.is_empty(), "`{source}` produced {diags:?}");
    }
}

#[test]
fn a_parenthesised_target_list_is_rejected_with_its_rewrite() {
    for (source, replacement) in [
        ("fn main() { let (a, b) = (1, 2)\n let _ = a }\n", "a, b"),
        (
            "fn main() { let mut a = 1\n let mut b = 2\n (a, b) = (b, a)\n let _ = a }\n",
            "a, b",
        ),
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("parenthesised_targets.gos", source.to_string());
        let (_, diags) = parse_source_file(source, file);
        let found = diags.iter().any(|d| {
            matches!(
                &d.error,
                gossamer_parse::ParseError::LetPatternParens { replacement: r } if r == replacement
            )
        });
        assert!(found, "`{source}` produced {diags:?}");
    }
}

#[test]
fn a_newline_does_not_create_a_one_element_tuple() {
    for source in [
        "fn main() { let x = (\n    1 + 2\n)\n let _ = x + 1 }\n",
        "fn main() { let (\n    a\n) = 5\n let _ = a + 1 }\n",
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("paren_newline.gos", source.to_string());
        let (_, diags) = parse_source_file(source, file);
        assert!(diags.is_empty(), "`{source}` produced {diags:?}");
    }
}

#[test]
fn same_line_lists_require_commas() {
    for source in [
        "struct Point { x: i64 y: i64 }\n",
        "enum Choice { One Two }\n",
        "fn add(x: i64 y: i64) { }\n",
        "fn main() { add(1 2) }\n",
        "fn main() { let t = (1 2) }\n",
        "fn main() { let v = #[1 2] }\n",
        "fn main() { let a = [1 2] }\n",
        "fn main() { let s = #{1 2} }\n",
        "fn main() { let m = {\"a\": 1 \"b\": 2} }\n",
        "use std::{fs env}\nfn main() {}\n",
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("comma_lists.gos", source.to_string());
        let (_, diags) = parse_source_file(source, file);
        assert!(!diags.is_empty(), "`{source}` unexpectedly parsed");
    }
}

#[test]
fn statement_semicolons_separate_same_line_statements() {
    for source in [
        "fn main() { let x = 9; println(x) }\n",
        "fn main() { println(1); println(2) }\n",
        "let x = 4; println(x)\n",
        "use std::strings; use std::iter\nfn main() {}\n",
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("semicolon_separator.gos", source.to_string());
        let (_, diags) = parse_source_file(source, file);
        assert!(diags.is_empty(), "`{source}` produced {diags:?}");
    }
}

#[test]
fn same_line_non_block_statements_require_semicolons() {
    for source in [
        "fn main() { for e in Vec::from([1, 2, 3]) { println e } }\n",
        "fn main() { println 1 }\n",
        "fn main() { let x = 1 let y = 2 }\n",
        "fn main() { defer cleanup() println(1) }\n",
        "fn main() { spawn(|| { work() println(1) }) }\n",
        "println 1\n",
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("missing_statement_separator.gos", source.to_string());
        let (_, diags) = parse_source_file(source, file);
        assert!(
            diags.iter().any(|diag| {
                diag.error
                    .to_string()
                    .contains("`;` or a newline between statements")
            }),
            "`{source}` unexpectedly parsed without a statement-separator diagnostic: {diags:?}"
        );
    }
}

#[test]
fn block_statements_and_function_values_keep_rust_like_boundaries() {
    for source in [
        "fn main() { for e in Vec::from([1, 2, 3]) { println; e } }\n",
        "fn main() { let output = println; output(1) }\n",
        "fn main() { if true {} println(1) }\n",
        "fn main() { while false {} println(1) }\n",
        "fn main() { println\n1 }\n",
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("statement_boundaries.gos", source.to_string());
        let (_, diags) = parse_source_file(source, file);
        assert!(diags.is_empty(), "`{source}` produced {diags:?}");
    }
}

#[test]
fn statement_semicolon_terminators_are_optional() {
    for source in [
        "use example;\n",
        "let x = 1;\n",
        "fn main() { let x = 9;\nprintln(x) }\n",
        "fn main() { println(1);\nprintln(2) }\n",
        "fn main() { println(1); }\n",
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("semicolon.gos", source.to_string());
        let (_, diags) = parse_source_file(source, file);
        assert!(diags.is_empty(), "`{source}` produced {diags:?}");
    }
}

#[test]
fn empty_named_struct_declaration_accepts_braces() {
    let source = "struct Unit {}\n";
    let mut map = SourceMap::new();
    let file = map.add_file("empty_named_struct.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "`struct Unit {{}}` must parse cleanly; got {diags:?}"
    );
    let gossamer_ast::ItemKind::Struct(decl) = &sf.items[0].kind else {
        panic!("expected a struct item");
    };
    assert!(matches!(
        &decl.body,
        gossamer_ast::StructBody::Named(fields) if fields.is_empty()
    ));
}

#[test]
fn empty_tuple_struct_declaration_accepts_parentheses() {
    let source = "struct Unit()\n";
    let mut map = SourceMap::new();
    let file = map.add_file("empty_tuple_struct.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "`struct Unit()` must parse cleanly; got {diags:?}"
    );
    let gossamer_ast::ItemKind::Struct(decl) = &sf.items[0].kind else {
        panic!("expected a struct item");
    };
    assert!(matches!(
        &decl.body,
        gossamer_ast::StructBody::Tuple(fields) if fields.is_empty()
    ));
}

/// Regression: the match-scrutinee / loop-condition struct-literal
/// restriction must be suspended inside delimited sub-expressions -
/// call arguments, parentheses, index brackets, array literals, and
/// blocks. `match http::serve("addr", App { }) { .. }` used to fail
/// with "unexpected `{`, expected `)` to close argument list".
#[test]
fn struct_literal_in_delimited_scrutinee_positions_parses() {
    let source = concat!(
        "struct App { n: i64 }\n",
        "fn pick(a: App) -> i64 { a.n }\n",
        "fn f() -> i64 {\n",
        "    match pick(App { n: 1 }) {\n",
        "        1 => 10,\n",
        "        _ => 0,\n",
        "    }\n",
        "}\n",
        "fn g() -> i64 {\n",
        "    if pick(App { n: 2 }) == 2 { 1 } else { 0 }\n",
        "}\n",
        "fn h() -> i64 {\n",
        "    while pick(App { n: 0 }) == 99 { }\n",
        "    match (pick(App { n: 3 }), [pick(App { n: 4 })]) {\n",
        "        (3, _) => 3,\n",
        "        _ => 0,\n",
        "    }\n",
        "}\n",
    );
    let mut map = SourceMap::new();
    let file = map.add_file("struct_lit_scrutinee.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    for diag in &diags {
        eprintln!("  {diag}");
    }
    assert!(
        diags.is_empty(),
        "struct literals inside delimited scrutinee sub-expressions must parse; \
         got {} diag(s)",
        diags.len()
    );
    assert_eq!(sf.items.len(), 5, "expected five items");
}

#[test]
fn use_brace_group_accepts_multi_segment_paths() {
    // `use std::{env, encoding::json, strings}` - a multi-segment path
    // inside a brace group must parse cleanly and expand to the same
    // entries as the split form.
    let source = "use std::{env, encoding::json, strings}\n";
    let mut map = SourceMap::new();
    let file = map.add_file("grouped.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "grouped multi-segment use must parse: {diags:?}"
    );
    assert_eq!(sf.uses.len(), 1);
    let list = sf.uses[0].list.as_ref().expect("brace list");
    assert_eq!(list.len(), 3);
    // `env` and `strings` are single-segment; `encoding::json` carries the
    // `encoding` prefix with `json` as the bound name.
    assert!(list[0].prefix.is_empty() && list[0].name.name == "env");
    assert_eq!(
        list[1]
            .prefix
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        vec!["encoding"]
    );
    assert_eq!(list[1].name.name, "json");
    assert!(list[2].prefix.is_empty() && list[2].name.name == "strings");
}

#[test]
fn use_brace_group_accepts_nested_groups() {
    // `use std::{encoding::{json, yaml}, strings}` - a nested brace group.
    let source = "use std::{encoding::{json, yaml}, strings}\n";
    let mut map = SourceMap::new();
    let file = map.add_file("nested.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    assert!(diags.is_empty(), "nested group must parse: {diags:?}");
    let list = sf.uses[0].list.as_ref().expect("brace list");
    let names: Vec<(Vec<&str>, &str)> = list
        .iter()
        .map(|e| {
            (
                e.prefix.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
                e.name.name.as_str(),
            )
        })
        .collect();
    assert_eq!(
        names,
        vec![
            (vec!["encoding"], "json"),
            (vec!["encoding"], "yaml"),
            (vec![], "strings"),
        ]
    );
}

/// A closure step names the slot the piped value fills, including the
/// non-trailing one that a bare callable cannot reach.
#[test]
fn pipe_closure_step_parses() {
    let source = "use std::strings\nfn main() {\n\
        let greeting = \"world\" |> |v| format(\"hello, {}\", v)\n\
        let part = \"world\" |> |v| strings::slice(v, 1, 4)\n\
    }\n";
    let mut map = SourceMap::new();
    let file = map.add_file("pipe_args.gos", source.to_string());
    let (_sf, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "a closure step is the way a slot is named: {diags:?}"
    );
}

#[test]
fn pipe_rejects_the_retired_dollar_placeholder() {
    let source = "fn main() {\n\
        let _ = 1 |> pair($, $)\n\
        let _ = 1 |> outer(inner($), $)\n\
    }\n";
    let mut map = SourceMap::new();
    let file = map.add_file("pipe_many_args.gos", source.to_string());
    let (_sf, diags) = parse_source_file(source, file);
    assert!(
        diags
            .iter()
            .any(|diag| matches!(diag.error, ParseError::PipePlaceholderRetired)),
        "the retired `$` placeholder needs a focused parse error: {diags:?}"
    );
}

#[test]
fn pipe_accepts_dotdot_as_range_argument() {
    let source = "fn main() { let _ = [1, 2] |> |v| iter::zip(.., v).collect() }\n";
    let mut map = SourceMap::new();
    let file = map.add_file("pipe_dotdot.gos", source.to_string());
    let (_sf, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "`..` should remain a range argument in pipe calls: {diags:?}"
    );
}

#[test]
fn method_turbofish_shorthand_parses_only_before_calls() {
    let source = concat!(
        "struct Point { x: i64 }\n",
        "fn main() {\n",
        "    let left = Point { x: 1 }\n",
        "    let right = Point { x: 2 }\n",
        "    let a = \"12\".parse<i64>()\n",
        "    let b = \"34\".parse::<i64>()\n",
        "    let shorter = a.len < 3\n",
        "    let ordered = left.x < right.x\n",
        "}\n",
    );
    let mut map = SourceMap::new();
    let file = map.add_file("method_turbofish.gos", source.to_string());
    let (_sf, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "method turbofish shorthand and field comparisons should parse cleanly: {diags:?}"
    );
}

#[test]
fn format_macro_family_rejects_missing_and_unused_positional_arguments() {
    for macro_name in ["format", "println", "print", "eprintln", "eprint", "panic"] {
        for (template, expected, found) in [("one", 0, 1), ("{}", 1, 0)] {
            let source = format!("fn main() {{ {macro_name}!(\"{template}\", \"two\") }}");
            let source = if found == 0 {
                format!("fn main() {{ {macro_name}!(\"{template}\") }}")
            } else {
                source
            };
            let mut map = SourceMap::new();
            let file = map.add_file("format_args.gos", source.clone());
            let (_sf, diags) = parse_source_file(&source, file);
            assert!(
                diags.iter().any(|diag| matches!(
                    diag.error,
                    ParseError::FormatArgumentCount {
                        expected: actual_expected,
                        found: actual_found,
                    } if actual_expected == expected && actual_found == found
                )),
                "{macro_name}!({template:?}) should reject its positional arguments: {diags:?}"
            );
        }
    }
}

#[test]
fn format_macro_family_requires_literal_templates() {
    for macro_name in ["format", "println", "print", "eprintln", "eprint", "panic"] {
        let source =
            format!("fn main() {{ let template = \"{{}}\"; {macro_name}!(template, \"value\") }}");
        let mut map = SourceMap::new();
        let file = map.add_file("format_literal.gos", source.clone());
        let (_sf, diags) = parse_source_file(&source, file);
        assert!(
            diags
                .iter()
                .any(|diag| matches!(diag.error, ParseError::FormatStringMustBeLiteral)),
            "{macro_name}! must require a literal template: {diags:?}"
        );
    }
}

#[test]
fn a_format_macro_is_not_a_pipe_step() {
    let source = "fn main() {\n\
        let value = \"world\"\n\
        value |> |v| println(\"hello, {}\", v)\n\
    }\n";
    let mut map = SourceMap::new();
    let file = map.add_file("pipe_format_closure.gos", source.to_string());
    let (_sf, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "a closure step reaches a formatting macro: {diags:?}"
    );

    // A formatting call is an ordinary call, so an argument-taking step
    // built from one reports what any other argument-taking step does.
    for name in ["format", "println", "print", "eprintln", "eprint", "panic"] {
        let source = format!("fn main() {{ \"world\" |> {name}(\"hello\") }}");
        let mut map = SourceMap::new();
        let file = map.add_file("pipe_format_step.gos", source.clone());
        let (_sf, diags) = parse_source_file(&source, file);
        assert!(
            diags
                .iter()
                .any(|diag| matches!(diag.error, ParseError::PipeStepNeedsClosure { .. })),
            "`{name}` with arguments must not be a pipe step: {diags:?}"
        );
    }
}

#[test]
fn open_end_range_pattern_must_not_use_the_inclusive_marker() {
    let valid = "fn main() { let _ = match 1 { 1.. => 0, _ => 1 } }";
    let mut map = SourceMap::new();
    let file = map.add_file("open_end_range.gos", valid.to_string());
    let (_sf, diags) = parse_source_file(valid, file);
    assert!(
        diags.is_empty(),
        "`lo..` should be a valid open-end range: {diags:?}"
    );

    let invalid = "fn main() { let _ = match 1 { 1..= => 0, _ => 1 } }";
    let mut map = SourceMap::new();
    let file = map.add_file("invalid_open_end_range.gos", invalid.to_string());
    let (_sf, diags) = parse_source_file(invalid, file);
    assert!(
        diags
            .iter()
            .any(|diag| matches!(&diag.error, ParseError::InclusiveRangeMissingEnd)),
        "`lo..=` should require an upper bound: {diags:?}"
    );
}

#[test]
fn inclusive_value_range_requires_an_upper_bound() {
    for source in ["fn main() { 10..= }", "fn main() { ..= }"] {
        let mut map = SourceMap::new();
        let file = map.add_file("missing_range_end.gos", source.to_string());
        let (_sf, diags) = parse_source_file(source, file);
        assert_eq!(
            diags
                .iter()
                .filter(|diag| matches!(diag.error, ParseError::InclusiveRangeMissingEnd))
                .count(),
            1,
            "expected one precise inclusive-range diagnostic: {diags:?}"
        );
        let rendered = diags
            .iter()
            .find(|diag| matches!(diag.error, ParseError::InclusiveRangeMissingEnd))
            .expect("inclusive range diagnostic")
            .to_diagnostic();
        assert_eq!(rendered.code.as_str(), "GP0026");
        assert!(rendered.title.contains("requires an upper bound"));
        assert!(rendered.helps.iter().any(|help| help.contains("use `..`")));
    }
}

#[test]
fn range_pattern_bounds_take_primitive_limits() {
    let valid = "fn f(n: i64) -> i64 { match n { i64::MIN..=-1 => 0, 1..=i64::MAX => 1, _ => 2 } }";
    let mut map = SourceMap::new();
    let file = map.add_file("limit_bounds.gos", valid.to_string());
    let (_sf, diags) = parse_source_file(valid, file);
    assert!(diags.is_empty(), "limit bounds should parse: {diags:?}");
}

#[test]
fn range_pattern_path_bound_is_one_diagnostic() {
    for source in [
        "fn f(n: i64) -> i64 { match n { LOW..=9 => 0, _ => 1 } }",
        "fn f(n: i64) -> i64 { match n { 0..=LOW => 0, _ => 1 } }",
        "fn f(n: i64) -> i64 { match n { ..=m::LOW => 0, _ => 1 } }",
    ] {
        let mut map = SourceMap::new();
        let file = map.add_file("path_bound.gos", source.to_string());
        let (_sf, diags) = parse_source_file(source, file);
        assert_eq!(
            diags.len(),
            1,
            "expected one diagnostic for {source}: {diags:?}"
        );
        assert_eq!(diags[0].to_diagnostic().code.as_str(), "GP0059");
    }
}

#[test]
fn open_end_range_stops_before_for_body() {
    let source = "fn main() { for i in 3.. { println(i) } }";
    let mut map = SourceMap::new();
    let file = map.add_file("open_end_for.gos", source.to_string());
    let (_, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "open-ended range should stop before the `for` body: {diags:?}"
    );
}

#[test]
fn open_end_range_stops_before_next_line_expression() {
    let source = "fn main() {\n let b = 3..\n ()\n}";
    let mut map = SourceMap::new();
    let file = map.add_file("open_end_statement.gos", source.to_string());
    let (_, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "open-ended range should stop at the newline: {diags:?}"
    );
}

#[test]
fn match_arm_commas_are_optional_at_line_boundaries() {
    let source = concat!(
        "fn classify(n: i64) -> String {\n",
        "    match n {\n",
        "        ..0 => \"negative\"\n",
        "        0 => \"zero\",\n",
        "        _ => { \"positive\" }\n",
        "    }\n",
        "}\n",
    );
    let mut map = SourceMap::new();
    let file = map.add_file("comma_optional_match.gos", source.to_string());
    let (_sf, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "comma-free match arms must parse: {diags:?}"
    );
}

#[test]
fn comma_free_match_arm_before_open_start_range_pattern_parses() {
    let source = concat!(
        "fn classify(n: i64) -> String {\n",
        "    match n {\n",
        "        0.. => \"non-negative\"\n",
        "        ..0 => \"negative\"\n",
        "        _ => \"other\"\n",
        "    }\n",
        "}\n",
    );
    let mut map = SourceMap::new();
    let file = map.add_file("open_range_comma_optional_match.gos", source.to_string());
    let (_sf, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "comma-free open range match arms must parse: {diags:?}"
    );
}

#[test]
fn malformed_match_arms_report_one_local_error_each() {
    let cases = [
        ("fn main() { match 1 { 1 10 } }", 0),
        ("fn main() { match 1 { 1 => } }", 1),
        ("fn main() { match 1 { 1 => 10 2 => 20 } }", 2),
    ];
    for (source, expected) in cases {
        let mut map = SourceMap::new();
        let file = map.add_file("malformed_match.gos", source.to_string());
        let (_sf, diags) = parse_source_file(source, file);
        assert_eq!(diags.len(), 1, "unexpected diagnostic cascade: {diags:?}");
        let expected = match expected {
            0 => matches!(diags[0].error, ParseError::MatchArmMissingArrow { .. }),
            1 => matches!(diags[0].error, ParseError::MatchArmMissingBody),
            _ => matches!(diags[0].error, ParseError::MatchArmMissingSeparator),
        };
        assert!(expected, "wrong diagnostic: {diags:?}");
        let structured = diags[0].to_diagnostic();
        assert_eq!(
            structured.code.as_str(),
            match expected {
                true if matches!(diags[0].error, ParseError::MatchArmMissingArrow { .. }) => {
                    "GP0029"
                }
                true if matches!(diags[0].error, ParseError::MatchArmMissingBody) => "GP0030",
                _ => "GP0031",
            }
        );
        assert!(
            !structured.helps.is_empty(),
            "diagnostic needs actionable help"
        );
        assert!(
            !matches!(diags[0].error, ParseError::MixedEntryForms),
            "a match-arm error must not be misreported as an entry-form error"
        );
    }
}

#[test]
fn newline_separates_break_match_arm_without_comma() {
    let source = "fn main() {\n    match 7 {\n        7 => break\n        _ => 0\n    }\n}\n";
    let mut map = SourceMap::new();
    let file = map.add_file("break_match_arm.gos", source.to_string());
    let (_sf, diags) = parse_source_file(source, file);
    assert!(
        diags.is_empty(),
        "newline must terminate a valueless break arm: {diags:?}"
    );
}

#[test]
fn assoc_type_binding_parses_into_the_bound() {
    let source = "trait Holder { type Item }\nfn f<T: Holder<Item = i64>>(x: T) -> i64 { 1 }\n";
    let mut map = SourceMap::new();
    let file = map.add_file("assoc_binding.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    assert!(diags.is_empty(), "diagnostics: {diags:?}");
    let ItemKind::Fn(decl) = &sf.items[1].kind else {
        panic!("expected a function item");
    };
    let gossamer_ast::GenericParam::Type { bounds, .. } = &decl.generics.params[0] else {
        panic!("expected a type parameter");
    };
    assert_eq!(bounds[0].trait_name(), Some("Holder"));
    assert!(
        bounds[0].path.segments[0].generics.is_empty(),
        "the constraint must not land in the ordinary argument list"
    );
    assert_eq!(bounds[0].bindings.len(), 1);
    assert_eq!(bounds[0].bindings[0].name.name, "Item");
}

#[test]
fn assoc_type_binding_coexists_with_a_type_argument() {
    let source = "trait Pair<A> { type Item }\nfn f<T: Pair<i64, Item = String>>(x: T) {}\n";
    let mut map = SourceMap::new();
    let file = map.add_file("assoc_binding_mixed.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    assert!(diags.is_empty(), "diagnostics: {diags:?}");
    let ItemKind::Fn(decl) = &sf.items[1].kind else {
        panic!("expected a function item");
    };
    let gossamer_ast::GenericParam::Type { bounds, .. } = &decl.generics.params[0] else {
        panic!("expected a type parameter");
    };
    assert_eq!(bounds[0].path.segments[0].generics.len(), 1);
    assert_eq!(bounds[0].bindings.len(), 1);
    assert_eq!(bounds[0].bindings[0].name.name, "Item");
}

/// Extracts the value of the first string literal in a parsed source file.
fn first_string_literal(source: &str) -> String {
    use gossamer_ast::Literal;
    let mut map = SourceMap::new();
    let file = map.add_file("triple.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    let rendered = format!("{sf:?}");
    let _ = Literal::Unit;
    let marker = "String(\"";
    let start = rendered.find(marker).expect("a string literal") + marker.len();
    let tail = &rendered[start..];
    let mut value = String::new();
    let mut chars = tail.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => value.push('\n'),
                Some('t') => value.push('\t'),
                Some('\\') => value.push('\\'),
                Some('"') => value.push('"'),
                Some(other) => value.push(other),
                None => break,
            },
            other => value.push(other),
        }
    }
    value
}

#[test]
fn triple_quoted_literal_folds_to_a_dedented_string() {
    let source = "fn main() {\n    let text = \"\"\"\n    <html>\n        <body>\n    </html>\n    \"\"\"\n}\n";
    assert_eq!(first_string_literal(source), "<html>\n    <body>\n</html>");
}

#[test]
fn triple_quoted_literal_decodes_escapes_after_dedenting() {
    let source = "fn main() {\n    let text = \"\"\"\n    a\\tb\n    \"\"\"\n}\n";
    assert_eq!(first_string_literal(source), "a\tb");
}

#[test]
fn single_line_triple_quoted_literal_keeps_its_body() {
    let source = "fn main() {\n    let text = \"\"\"a\"b\"\"\"\n}\n";
    assert_eq!(first_string_literal(source), "a\"b");
}

#[test]
fn text_after_the_opening_delimiter_is_gp0033() {
    let source = "fn main() {\n    let text = \"\"\"oops\n    a\n    \"\"\"\n}\n";
    let mut map = SourceMap::new();
    let file = map.add_file("bad.gos", source.to_string());
    let (_sf, diags) = parse_source_file(source, file);
    let codes: Vec<String> = diags
        .iter()
        .map(|d| d.to_diagnostic().code.0.to_string())
        .collect();
    assert_eq!(codes, vec!["GP0033".to_string()]);
}

#[test]
fn a_triple_quoted_literal_works_as_a_match_pattern() {
    let source = concat!(
        "fn main() {\n",
        "    let text = \"\"\"\n    hi\n    \"\"\"\n",
        "    match text {\n",
        "        \"\"\"\n        hi\n        \"\"\" => println(\"yes\")\n",
        "        _ => println(\"no\")\n",
        "    }\n",
        "}\n"
    );
    let mut map = SourceMap::new();
    let file = map.add_file("pat.gos", source.to_string());
    let (_sf, diags) = parse_source_file(source, file);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

/// A file's imports precede its items, so a later `use` is a placement
/// error. Naming `use` among the accepted continuations would contradict
/// the rejection, so the report says where the import belongs.
#[test]
fn an_import_after_an_item_reports_where_it_belongs() {
    let source = "struct Point { x: i64 }\n\nuse std::env\n";
    let mut map = SourceMap::new();
    let file = map.add_file("late-use.gos", source.to_string());
    let (_, diags) = parse_source_file(source, file);
    let rendered: Vec<String> = diags.iter().map(ToString::to_string).collect();
    assert!(
        rendered
            .iter()
            .any(|text| text.contains("expected an item")),
        "expected a placement report, got: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|text| text.contains("`use`, or `mod`")),
        "the rejected keyword must not be listed as accepted: {rendered:?}"
    );
}
