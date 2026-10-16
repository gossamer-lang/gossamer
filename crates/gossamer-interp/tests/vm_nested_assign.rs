//! VM-native nested / complex assignment tests (destroy-tree-walker
//! Phase 7). Chained field / index places (`a.b.c = x`, `grid[i][j] =
//! x`, `*p = v`) lower through the recursive `compile_place_store`.

use std::cell::RefCell;

use gossamer_hir::lower_source_file;
use gossamer_interp::{Vm, set_stdout_writer};
use gossamer_lex::SourceMap;
use gossamer_parse::autoderive::parse_with_autoderive;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

thread_local! {
    static CAPTURED: RefCell<String> = const { RefCell::new(String::new()) };
}

fn capture_writer(text: &str) {
    CAPTURED.with(|cell| cell.borrow_mut().push_str(text));
}

fn run_main(source: &str) -> String {
    let mut map = SourceMap::new();
    let file = map.add_file("nested_assign.gos", source.to_string());
    let (mut sf, parse_diags) = parse_with_autoderive(source, file);
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
fn nested_field_assignment() {
    let src = r#"
struct Inner { v: i64 }
struct Outer { inner: Inner, tag: i64 }
fn main() {
    let mut o = Outer { inner: Inner { v: 1 }, tag: 9 }
    o.inner.v = 42
    println("{} {}", o.inner.v, o.tag)
}
"#;
    assert_eq!(run_main(src), "42 9\n");
}

#[test]
fn nested_index_assignment() {
    let src = r#"
fn main() {
    let mut grid = [[1, 2], [3, 4]]
    grid[0][1] = 99
    grid[1][0] = 77
    println("{} {} {} {}", grid[0][0], grid[0][1], grid[1][0], grid[1][1])
}
"#;
    assert_eq!(run_main(src), "1 99 77 4\n");
}

#[test]
fn field_of_indexed_assignment() {
    let src = r#"
struct Cell { v: i64 }
fn main() {
    let mut row = [Cell { v: 1 }, Cell { v: 2 }]
    row[1].v = 55
    println("{} {}", row[0].v, row[1].v)
}
"#;
    assert_eq!(run_main(src), "1 55\n");
}
