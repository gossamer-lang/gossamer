//! Behavioral coverage for the discarded-tail unit-load elision.
//! Eliding the dead `LoadConst(Unit)` on a loop body / statement-position
//! block tail must not change any observable result: loops still run their
//! side effects, and a block / `if` / `match` whose value IS used still
//! produces it.

#![allow(clippy::needless_raw_string_hashes)]

use std::cell::RefCell;

use gossamer_hir::lower_source_file;
use gossamer_interp::{Vm, set_stdout_writer};
use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

thread_local! {
    static CAPTURED: RefCell<String> = const { RefCell::new(String::new()) };
}

fn capture_writer(text: &str) {
    CAPTURED.with(|cell| cell.borrow_mut().push_str(text));
}

fn run_program(source: &str) -> String {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);

    let mut interp = Vm::new();
    interp.load(&program, tcx, true).expect("vm load");

    CAPTURED.with(|cell| cell.borrow_mut().clear());
    let prev = set_stdout_writer(capture_writer);
    let result = interp.call("main", Vec::new());
    set_stdout_writer(prev);
    result.expect("main returned an error");
    CAPTURED.with(|cell| cell.borrow().clone())
}

#[test]
fn while_loop_with_assignment_tail_runs_its_side_effects() {
    let src = r#"
fn main() {
    let mut i = 0
    let mut sum = 0
    while i < 5 {
        sum = sum + i
        i = i + 1
    }
    println("{} {}", i, sum)
}
"#;
    assert_eq!(run_program(src), "5 10\n");
}

#[test]
fn bare_loop_with_assignment_tail_and_conditional_break() {
    let src = r#"
fn main() {
    let mut i = 0
    let total = loop {
        if i >= 4 { break i * 10 }
        i = i + 1
    }
    println("{}", total)
}
"#;
    assert_eq!(run_program(src), "40\n");
}

#[test]
fn for_range_loop_with_compound_assign_tail() {
    let src = r#"
fn main() {
    let mut acc = 0
    for n in 1..=4 {
        acc += n
    }
    println("{}", acc)
}
"#;
    assert_eq!(run_program(src), "10\n");
}

#[test]
fn if_else_as_value_still_produces_a_value() {
    let src = r#"
fn classify(n: i64) -> i64 {
    let label = if n < 0 { -1 } else if n == 0 { 0 } else { 1 }
    label
}
fn main() {
    println("{} {} {}", classify(-7), classify(0), classify(7))
}
"#;
    assert_eq!(run_program(src), "-1 0 1\n");
}

#[test]
fn match_as_value_still_produces_a_value() {
    let src = r#"
enum Shape { Circle(f64), Rect { w: f64, h: f64 } }
fn name(s: Shape) -> String {
    match s {
        Shape::Circle(_) => "round",
        Shape::Rect { .. } => "boxy",
    }
}
fn main() {
    println("{} {}", name(Shape::Circle(1.0)), name(Shape::Rect { w: 2.0, h: 3.0 }))
}
"#;
    assert_eq!(run_program(src), "round boxy\n");
}

#[test]
fn block_expression_bound_to_let_keeps_its_value() {
    let src = r#"
fn main() {
    let v = {
        let a = 3
        let b = 4
        a * b
    }
    println("{}", v)
}
"#;
    assert_eq!(run_program(src), "12\n");
}

#[test]
fn nested_loop_with_inner_assignment_tail() {
    let src = r#"
fn main() {
    let mut total = 0
    let mut i = 0
    while i < 3 {
        let mut j = 0
        while j < 3 {
            total = total + 1
            j = j + 1
        }
        i = i + 1
    }
    println("{}", total)
}
"#;
    assert_eq!(run_program(src), "9\n");
}

#[test]
fn loop_body_with_vec_push_tail_still_mutates() {
    let src = r#"
fn main() {
    let mut xs: Vec<i64> = Vec::from([])
    let mut i = 0
    while i < 4 {
        i = i + 1
        xs.push(i * i)
    }
    println("{} {}", xs.len(), xs[3])
}
"#;
    assert_eq!(run_program(src), "4 16\n");
}

#[test]
fn statement_position_block_with_assignment_tail() {
    // An inner block in statement position whose tail is an assignment:
    // the assignment must still execute even though its value is discarded.
    let src = r#"
fn main() {
    let mut x = 1
    {
        x = x + 41
    }
    println("{}", x)
}
"#;
    assert_eq!(run_program(src), "42\n");
}
