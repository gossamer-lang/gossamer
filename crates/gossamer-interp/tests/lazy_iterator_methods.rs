//! Regression coverage for lazy iterator method dispatch.

use std::cell::RefCell;

use gossamer_hir::lower_source_file;
use gossamer_interp::{Vm, reset_lazy_iterator_state, set_stdout_writer};
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

fn run_lazy_program(source: &str) -> String {
    let mut map = SourceMap::new();
    let file = map.add_file("lazy_iterator_methods.gos", source.to_string());
    let (mut sf, parse_diags) = parse_with_autoderive(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    assert!(resolve_diags.is_empty(), "resolve: {resolve_diags:?}");
    let mut tcx = TyCtxt::new();
    let (table, type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    assert!(type_diags.is_empty(), "type: {type_diags:?}");
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);

    reset_lazy_iterator_state();
    let mut vm = Vm::new();
    vm.load(&program, tcx, true).expect("vm load");
    CAPTURED.with(|cell| cell.borrow_mut().clear());
    let previous = set_stdout_writer(capture_writer);
    let result = vm.call("main", Vec::new());
    set_stdout_writer(previous);
    result.expect("main returned an error");
    CAPTURED.with(|cell| cell.borrow().clone())
}

#[test]
fn lazy_range_methods_dispatch_like_iter_free_functions() {
    let source = r#"
fn main() {
    let values = (0..).take(2).collect()
    println("{} {} {} {}", values[0], values[1], (0..).take(2).count(), (..0).take(2).count())
}
"#;
    assert_eq!(run_lazy_program(source), "0 1 2 0\n");
}

#[test]
fn lazy_range_terminal_methods_consume_closure_adapters() {
    let source = r#"
fn main() {
    let mapped = (1..5).map(|n| n * n)
    println("{} {} {:?} {:?}", mapped.sum(), (1..5).product(), (1..5).min(), (1..5).max())
}
"#;
    assert_eq!(run_lazy_program(source), "30 24 Some(1) Some(4)\n");
}

#[test]
fn lazy_range_for_loop_survives_an_iterator_typed_parameter() {
    let source = r"
use Iterator

fn list_range(values: Vec<i64>, range: Iterator<i64>) {
    for i in range {
        println(values[i])
    }
}

fn main() {
    list_range(Vec::from([1, 2, 3]), 0..2)
}
";
    assert_eq!(run_lazy_program(source), "1\n2\n");
}
