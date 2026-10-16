//! A `match` that runs off the end without any arm matching must panic
//! cleanly on the bytecode VM, with the same message and exit behaviour
//! the compiled tiers produce, rather than falling through to a zero
//! value. The exhaustiveness checker covers well-typed programs, but a
//! blind spot (it cannot enumerate integer payloads) lets a value like
//! `Some(2)` reach the fall-through of `Some(0) | Some(1) | None`.

use gossamer_hir::lower_source_file;
use gossamer_interp::{Value, Vm, is_panic_error, panic_message, set_stdout_writer};
use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

/// Message the compiled tiers (`gossamer-mir`) emit for a non-exhaustive
/// match. The VM is asserted bit-identical to this literal.
const NON_EXHAUSTIVE_MATCH_MESSAGE: &str = "non-exhaustive match: no pattern matched the value";

fn run_main_result(source: &str) -> gossamer_interp::RuntimeResult<Value> {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, _resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);

    let mut interp = Vm::new();
    interp.load(&program, tcx, true).expect("vm load");

    // Swallow any println output; this test only inspects the result.
    let prev = set_stdout_writer(|_| {});
    let result = interp.call("main", Vec::new());
    set_stdout_writer(prev);
    result
}

#[test]
fn vm_non_exhaustive_match_panics_with_canonical_message() {
    let source = r#"
fn f(o: Option<i64>) -> i64 {
    match o {
        Some(0) => 10,
        Some(1) => 20,
        None => 0,
    }
}

fn main() {
    println("{}", f(Some(2)))
}
"#;

    let result = run_main_result(source);
    let err = result.expect_err("blind-spot match must panic, not return a zero value");
    assert!(is_panic_error(&err), "expected a GX panic, got: {err}");
    assert_eq!(
        panic_message(&err),
        NON_EXHAUSTIVE_MATCH_MESSAGE,
        "VM panic text must be bit-identical to the compiled tiers"
    );
}

#[test]
fn vm_exhaustive_match_with_wildcard_does_not_panic() {
    let source = r#"
fn f(o: Option<i64>) -> i64 {
    match o {
        Some(0) => 10,
        Some(1) => 20,
        Some(_) => 99,
        None => 0,
    }
}

fn main() {
    println("{}", f(Some(2)))
}
"#;

    let result = run_main_result(source);
    assert!(
        result.is_ok(),
        "exhaustive match must run, got: {:?}",
        result.err()
    );
}

#[test]
fn vm_full_variant_match_runs_normally() {
    let source = r#"
enum Shape { Circle(i64), Square(i64) }

fn area(s: Shape) -> i64 {
    match s {
        Shape::Circle(r) => 3 * r * r,
        Shape::Square(w) => w * w,
    }
}

fn main() {
    println("{}", area(Shape::Square(4)))
}
"#;

    let result = run_main_result(source);
    assert!(
        result.is_ok(),
        "full-variant match must run, got: {:?}",
        result.err()
    );
}
