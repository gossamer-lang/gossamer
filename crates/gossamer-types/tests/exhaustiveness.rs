//! End-to-end tests for match exhaustiveness and unreachable-arm
//! detection.

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{ExhaustivenessError, TyCtxt, check_exhaustiveness, typecheck_source_file};

fn run(source: &str) -> Vec<gossamer_types::ExhaustivenessDiagnostic> {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (resolutions, _resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    check_exhaustiveness(&sf, &resolutions, &table, &tcx)
}

#[test]
fn bool_match_missing_false_is_reported() {
    let source = r"
fn main() {
    let x = true
    match x {
        true => 1i32,
    }
}
";
    let diagnostics = run(source);
    assert!(
        diagnostics
            .iter()
            .any(|d| matches!(&d.error, ExhaustivenessError::NonExhaustive { missing } if missing.iter().any(|m| m == "false"))),
        "expected missing `false`: {diagnostics:?}"
    );
}

#[test]
fn bool_match_with_wildcard_is_exhaustive() {
    let source = r"
fn main() {
    let x = true
    match x {
        true => 1i32,
        _ => 0i32,
    }
}
";
    let diagnostics = run(source);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn bool_match_with_both_literals_is_exhaustive() {
    let source = r"
fn main() {
    let x = true
    match x {
        true => 1i32,
        false => 0i32,
    }
}
";
    let diagnostics = run(source);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn enum_match_missing_variant_is_reported() {
    let source = r"
enum Dir { North, South, East, West }

fn main() {
    let d = Dir::North
    match d {
        Dir::North => 0i32,
        Dir::South => 1i32,
    }
}
";
    let diagnostics = run(source);
    assert!(
        diagnostics.iter().any(|d| matches!(
            &d.error,
            ExhaustivenessError::NonExhaustive { missing } if missing.iter().any(|m| m == "East") && missing.iter().any(|m| m == "West")
        )),
        "expected missing East+West: {diagnostics:?}"
    );
}

#[test]
fn enum_match_with_all_variants_is_exhaustive() {
    let source = r"
enum Dir { North, South, East, West }

fn main() {
    let d = Dir::North
    match d {
        Dir::North => 0i32,
        Dir::South => 1i32,
        Dir::East => 2i32,
        Dir::West => 3i32,
    }
}
";
    let diagnostics = run(source);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn arm_after_wildcard_is_unreachable() {
    let source = r"
fn main() {
    let x = true
    match x {
        _ => 0i32,
        true => 1i32,
    }
}
";
    let diagnostics = run(source);
    assert!(
        diagnostics
            .iter()
            .any(|d| matches!(d.error, ExhaustivenessError::UnreachableArm)),
        "expected unreachable diagnostic: {diagnostics:?}"
    );
}

#[test]
fn guarded_wildcard_does_not_trigger_unreachable() {
    let source = r"
fn main() {
    let x = true
    match x {
        _ if x => 0i32,
        true => 1i32,
        false => 2i32,
    }
}
";
    let diagnostics = run(source);
    assert!(
        diagnostics
            .iter()
            .all(|d| !matches!(d.error, ExhaustivenessError::UnreachableArm)),
        "unexpected unreachable: {diagnostics:?}"
    );
}

#[test]
fn duplicate_bool_literal_is_unreachable() {
    let source = r"
fn main() {
    let x = true
    match x {
        true => 1i32,
        true => 2i32,
        false => 0i32,
    }
}
";
    let diagnostics = run(source);
    assert!(
        diagnostics
            .iter()
            .any(|d| matches!(d.error, ExhaustivenessError::UnreachableArm)),
        "expected duplicate-literal diagnostic: {diagnostics:?}"
    );
}

fn non_exhaustive(diagnostics: &[gossamer_types::ExhaustivenessDiagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|d| matches!(d.error, ExhaustivenessError::NonExhaustive { .. }))
}

// 0.18.1: an `i64` scrutinee with no wildcard arm was treated as
// exhaustive, so the compiled tier ran off the end of the dispatch and
// SIGSEGV'd. A non-enumerable scalar now requires a catch-all.
#[test]
fn int_match_without_wildcard_is_non_exhaustive() {
    let diagnostics = run("fn f(n: i64) -> i64 { match n { 0 => 10, 1 => 20, } }\n");
    assert!(non_exhaustive(&diagnostics), "{diagnostics:?}");
}

#[test]
fn int_match_with_wildcard_is_exhaustive() {
    let diagnostics = run("fn f(n: i64) -> i64 { match n { 0 => 10, _ => 0, } }\n");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn int_match_with_binding_catch_all_is_exhaustive() {
    let diagnostics = run("fn f(n: i64) -> i64 { match n { 0 => 10, other => other, } }\n");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn string_match_without_wildcard_is_non_exhaustive() {
    let diagnostics = run("fn f(s: String) -> i64 { match s { \"a\" => 1, \"b\" => 2, } }\n");
    assert!(non_exhaustive(&diagnostics), "{diagnostics:?}");
}

// 0.18.1: `Option` / `Result` are built-in sentinel ADTs absent from the
// user-enum table, so a missing-variant match was treated as exhaustive
// and the compiled tier read an uninitialised discriminant (garbage).
#[test]
fn option_match_missing_none_is_non_exhaustive() {
    let diagnostics = run("fn f(o: Option<i64>) -> i64 { match o { Some(n) => n, } }\n");
    assert!(non_exhaustive(&diagnostics), "{diagnostics:?}");
}

#[test]
fn option_match_both_arms_is_exhaustive() {
    let diagnostics = run("fn f(o: Option<i64>) -> i64 { match o { Some(n) => n, None => 0, } }\n");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn result_match_missing_err_is_non_exhaustive() {
    let diagnostics = run("fn f(r: Result<i64, i64>) -> i64 { match r { Ok(n) => n, } }\n");
    assert!(non_exhaustive(&diagnostics), "{diagnostics:?}");
}

#[test]
fn result_match_both_arms_is_exhaustive() {
    let diagnostics =
        run("fn f(r: Result<i64, i64>) -> i64 { match r { Ok(n) => n, Err(e) => e, } }\n");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

// 0.18.1: a guarded-only `Some` arm does not prove `Some` is covered, so
// `Some(0)` fell through; with `Option` now enumerable this is reported.
#[test]
fn guarded_only_some_arm_leaves_some_uncovered() {
    let diagnostics =
        run("fn f(o: Option<i64>) -> i64 { match o { Some(n) if n > 0 => n, None => 0, } }\n");
    assert!(non_exhaustive(&diagnostics), "{diagnostics:?}");
}

#[test]
fn example_programs_have_no_spurious_exhaustiveness_errors() {
    for name in ["hello_world.gos", "line_count.gos", "web_server.gos"] {
        let path = format!("{}/../../examples/{name}", env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(&path).expect("read example");
        let diagnostics = run(&source);
        let non_exhaustive: Vec<_> = diagnostics
            .iter()
            .filter(|d| matches!(d.error, ExhaustivenessError::NonExhaustive { .. }))
            .collect();
        assert!(
            non_exhaustive.is_empty(),
            "{path}: spurious non-exhaustive: {non_exhaustive:?}"
        );
    }
}

#[test]
fn tuple_of_bools_reports_the_missing_combination() {
    let source = r#"
fn main() {
    let a = true
    let b = true
    let r = match (a, b) { (true, true) => 1 }
    println("{}", r)
}
"#;
    let diagnostics = run(source);
    assert!(
        diagnostics.iter().any(|d| matches!(
            &d.error,
            ExhaustivenessError::NonExhaustive { missing } if missing.iter().any(|m| m.starts_with('('))
        )),
        "expected a tuple witness: {diagnostics:?}"
    );
}

#[test]
fn tuple_of_bools_covering_every_combination_is_exhaustive() {
    let source = r#"
fn main() {
    let a = true
    let b = true
    let r = match (a, b) {
        (true, true) => 1,
        (true, false) => 2,
        (false, true) => 3,
        (false, false) => 4,
    }
    println("{}", r)
}
"#;
    let diagnostics = run(source);
    assert!(
        !diagnostics
            .iter()
            .any(|d| matches!(d.error, ExhaustivenessError::NonExhaustive { .. })),
        "{diagnostics:?}"
    );
}

#[test]
fn tuple_with_a_wildcard_column_is_exhaustive() {
    let source = r#"
fn main() {
    let a = 1
    let b = true
    let r = match (a, b) {
        (1, true) => 1,
        (_, true) => 2,
        (_, false) => 3,
    }
    println("{}", r)
}
"#;
    let diagnostics = run(source);
    assert!(
        !diagnostics
            .iter()
            .any(|d| matches!(d.error, ExhaustivenessError::NonExhaustive { .. })),
        "{diagnostics:?}"
    );
}

#[test]
fn nested_option_of_result_reports_the_missing_payload_shape() {
    let source = r#"
fn main() {
    let o: Option<Result<i64, String>> = Some(Ok(1))
    let r = match o { Some(Ok(x)) => x }
    println("{}", r)
}
"#;
    let diagnostics = run(source);
    let missing: Vec<String> = diagnostics
        .iter()
        .filter_map(|d| match &d.error {
            ExhaustivenessError::NonExhaustive { missing } => Some(missing.clone()),
            ExhaustivenessError::UnreachableArm => None,
        })
        .flatten()
        .collect();
    assert!(
        missing.iter().any(|m| m == "Some(Err(_))"),
        "expected `Some(Err(_))`: {missing:?}"
    );
    assert!(
        missing.iter().any(|m| m == "None"),
        "expected `None`: {missing:?}"
    );
}

#[test]
fn nested_option_of_result_covering_every_shape_is_exhaustive() {
    let source = r#"
fn main() {
    let o: Option<Result<i64, String>> = Some(Ok(1))
    let r = match o {
        Some(Ok(x)) => x,
        Some(Err(_)) => 0,
        None => -1,
    }
    println("{}", r)
}
"#;
    let diagnostics = run(source);
    assert!(
        !diagnostics
            .iter()
            .any(|d| matches!(d.error, ExhaustivenessError::NonExhaustive { .. })),
        "{diagnostics:?}"
    );
}

#[test]
fn enum_payload_gap_is_reported_through_the_variant() {
    let source = r#"
enum Flag { On(bool), Off }

fn main() {
    let f = Flag::On(true)
    let r = match f {
        Flag::On(true) => 1,
        Flag::Off => 2,
    }
    println("{}", r)
}
"#;
    let diagnostics = run(source);
    let missing: Vec<String> = diagnostics
        .iter()
        .filter_map(|d| match &d.error {
            ExhaustivenessError::NonExhaustive { missing } => Some(missing.clone()),
            ExhaustivenessError::UnreachableArm => None,
        })
        .flatten()
        .collect();
    assert!(
        missing.iter().any(|m| m == "On(false)"),
        "expected `On(false)`: {missing:?}"
    );
}

#[test]
fn fixed_array_length_patterns_are_exhaustive_together() {
    let source = r#"
fn pick(xs: [i64; 2]) -> i64 {
    match xs {
        [a, b] => a + b,
    }
}

fn main() { println("{}", pick([1, 2])) }
"#;
    let diagnostics = run(source);
    assert!(
        !diagnostics
            .iter()
            .any(|d| matches!(d.error, ExhaustivenessError::NonExhaustive { .. })),
        "{diagnostics:?}"
    );
}

#[test]
fn fixed_array_rest_pattern_covers_every_length_it_fits() {
    let source = r#"
fn head(xs: [i64; 3]) -> i64 {
    match xs {
        [first, ..rest] => first + rest.len(),
        [] => 0,
    }
}

fn main() { println("{}", head([1, 2, 3])) }
"#;
    let diagnostics = run(source);
    assert!(
        !diagnostics
            .iter()
            .any(|d| matches!(d.error, ExhaustivenessError::NonExhaustive { .. })),
        "{diagnostics:?}"
    );
}

#[test]
fn a_range_that_leaves_values_out_reports_the_uncovered_ranges() {
    let source = r#"
fn main() {
    let n = 3
    let r = match n { 1..=5 => 1 }
    println("{}", r)
}
"#;
    let diagnostics = run(source);
    assert_eq!(
        missing(&diagnostics),
        vec!["i64::MIN..=0".to_string(), "6..=i64::MAX".to_string()]
    );
}

#[test]
fn name_only_variant_patterns_cover_their_payload() {
    let source = r#"
enum Shape { Dot, Line(i64), Box(i64, i64) }

fn main() {
    let kind = match Shape::Box(7, 8) {
        Shape::Dot => 0,
        Shape::Line(..) => 1,
        Shape::Box(..) => 2,
    }
    println("{}", kind)
}
"#;
    let diagnostics = run(source);
    assert!(
        !diagnostics
            .iter()
            .any(|d| matches!(d.error, ExhaustivenessError::NonExhaustive { .. })),
        "{diagnostics:?}"
    );
}

fn unreachable_count(diagnostics: &[gossamer_types::ExhaustivenessDiagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|d| matches!(d.error, ExhaustivenessError::UnreachableArm))
        .count()
}

fn missing(diagnostics: &[gossamer_types::ExhaustivenessDiagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .find_map(|d| match &d.error {
            ExhaustivenessError::NonExhaustive { missing } => Some(missing.clone()),
            ExhaustivenessError::UnreachableArm => None,
        })
        .unwrap_or_default()
}

#[test]
fn ranges_that_tile_an_unsigned_type_are_exhaustive() {
    let diagnostics = run(
        "fn f(n: u8) -> i64 { match n { 0..=127 => 1, 128..=255 => 2 } }\n\
         fn g(n: u8) -> i64 { match n { 0 => 0, 1..=254 => 1, 255 => 2 } }\n\
         fn h(n: i8) -> i64 { match n { ..=-1 => 0, 0 => 1, 1.. => 2 } }\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn a_gap_between_ranges_is_the_witness() {
    let diagnostics = run("fn f(n: u8) -> i64 { match n { 0..=99 => 1, 200..=255 => 2 } }\n");
    assert_eq!(missing(&diagnostics), vec!["100..=199".to_string()]);
    let diagnostics = run("fn f(n: i64) -> i64 { match n { 0.. => 1 } }\n");
    assert_eq!(missing(&diagnostics), vec!["i64::MIN..=-1".to_string()]);
}

#[test]
fn guarded_ranges_do_not_count_toward_coverage() {
    let diagnostics =
        run("fn f(n: u8, b: bool) -> i64 { match n { 0..=127 if b => 1, 128..=255 => 2 } }\n");
    assert_eq!(missing(&diagnostics), vec!["0..=127".to_string()]);
}

#[test]
fn chars_are_covered_by_ranges_around_the_surrogates() {
    let diagnostics = run(
        "fn f(c: char) -> i64 { match c { '\\u{0}'..='\\u{d7ff}' => 1, '\\u{e000}'..='\\u{10ffff}' => 2 } }\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let diagnostics = run("fn f(c: char) -> i64 { match c { 'a'..='z' => 1 } }\n");
    assert!(non_exhaustive(&diagnostics), "{diagnostics:?}");
}

#[test]
fn an_arm_inside_earlier_ranges_is_unreachable() {
    let diagnostics = run(
        "fn f(n: u8) -> i64 { match n { 0..=127 => 1, 100..=127 => 2, 128..=255 => 3, 200 => 4 } }\n",
    );
    assert_eq!(unreachable_count(&diagnostics), 2, "{diagnostics:?}");
    let diagnostics = run(
        "fn f(n: u8) -> i64 { match n { 0..=99 => 1, 100..=199 => 2, 50..=150 => 3, _ => 4 } }\n",
    );
    assert_eq!(unreachable_count(&diagnostics), 1, "{diagnostics:?}");
}

#[test]
fn an_overlapping_range_that_adds_values_is_reachable() {
    let diagnostics =
        run("fn f(n: u8) -> i64 { match n { 0..=127 => 1, 100..=200 => 2, _ => 3 } }\n");
    assert_eq!(unreachable_count(&diagnostics), 0, "{diagnostics:?}");
}

#[test]
fn a_catch_all_after_ranges_that_tile_the_type_is_unreachable() {
    let diagnostics =
        run("fn f(n: u8) -> i64 { match n { 0..=127 => 1, 128..=255 => 2, _ => 3 } }\n");
    assert_eq!(unreachable_count(&diagnostics), 1, "{diagnostics:?}");
}
