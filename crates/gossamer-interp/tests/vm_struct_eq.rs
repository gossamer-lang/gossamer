//! VM-native struct / enum equality tests (destroy-tree-walker Phase 7).
//!
//! A struct `==` / `!=` lowers to a call of the derived `<Type>::eq`
//! method; enums compare structurally via `Op::Eq`. Every program here
//! runs entirely through the bytecode [`Vm`].

use std::cell::RefCell;

use gossamer_hir::lower_source_file;
use gossamer_interp::{Vm, set_stdout_writer};
use gossamer_lex::SourceMap;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

thread_local! {
    static CAPTURED: RefCell<String> = const { RefCell::new(String::new()) };
}

fn capture_writer(text: &str) {
    CAPTURED.with(|cell| cell.borrow_mut().push_str(text));
}

fn run_main(source: &str) -> String {
    // Mirror the driver: synthesize `#[derive]` / serde impls before
    // parsing so `<Type>::eq` resolves exactly as it does under `gos`.
    let augmented = gossamer_parse::autoderive::augment_source(source);
    let mut map = SourceMap::new();
    let file = map.add_file("struct_eq.gos", augmented.clone());
    let (mut sf, parse_diags) = gossamer_parse::autoderive::parse_with_autoderive(&augmented, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, _resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut vm = Vm::new();
    vm.load(&program, tcx, true).expect("load");
    CAPTURED.with(|cell| cell.borrow_mut().clear());
    let prev = set_stdout_writer(capture_writer);
    let result = vm.call("main", Vec::new());
    set_stdout_writer(prev);
    result.expect("main failed");
    CAPTURED.with(|cell| cell.borrow().clone())
}

#[test]
fn struct_equality_routes_to_derived_eq() {
    let src = r#"
#[derive(PartialEq)]
struct Point { x: i64, y: i64 }
fn main() {
    let a = Point { x: 1, y: 2 }
    let b = Point { x: 1, y: 2 }
    let c = Point { x: 3, y: 4 }
    println("{} {} {}", a == b, a == c, a != c)
}
"#;
    assert_eq!(run_main(src), "true false true\n");
}

#[test]
fn enum_equality_is_structural() {
    let src = r#"
#[derive(PartialEq)]
enum Color { Red, Green, Rgb(i64, i64, i64) }
fn main() {
    let a = Color::Rgb(1, 2, 3)
    let b = Color::Rgb(1, 2, 3)
    let g = Color::Green
    println("{} {} {}", a == b, a == g, g == Color::Green)
}
"#;
    assert_eq!(run_main(src), "true false true\n");
}

#[test]
fn option_equality_is_structural() {
    let src = r#"
fn main() {
    println("{} {}", Some(5) == Some(5), Some(5) == Some(6))
}
"#;
    assert_eq!(run_main(src), "true false\n");
}
