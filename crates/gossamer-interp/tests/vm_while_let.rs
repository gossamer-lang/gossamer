//! VM-native `while let` and pattern-bound collection for-loop tests
//! (destroy-tree-walker Phase 7). A loop body containing a `match`
//! (notably the `while let` desugar) compiles natively; a `for x in
//! <pattern-bound collection>` iterates by index rather than deferring.

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

fn run_main(source: &str) -> String {
    let mut map = SourceMap::new();
    let file = map.add_file("while_let.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
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
fn while_let_drains_channel() {
    let src = r#"
use std::sync::channel
fn produce(tx: Sender<i64>) {
    tx.send(1)
 tx.send(2)
 tx.send(3)
 tx.close()
}
fn main() {
    let tx, rx = channel()
    spawn(|| produce(tx))
    let mut total = 0
    while let Some(v) = rx.recv() { total += v }
    println("{}", total)
}
"#;
    assert_eq!(run_main(src), "6\n");
}

#[test]
fn for_over_pattern_bound_vec_iterates_by_index() {
    let src = r#"
fn main() {
    let o: Option<Vec<i64>> = Some([4, 5, 6])
    let mut sum = 0
    if let Some(arr) = o {
        for x in arr { sum += x }
    }
    println("{}", sum)
}
"#;
    assert_eq!(run_main(src), "15\n");
}

#[test]
fn for_over_plain_string_array() {
    let src = r#"
fn main() {
    let xs = ["a", "b", "c"]
    for x in xs { print("{}", x) }
    println("")
}
"#;
    assert_eq!(run_main(src), "abc\n");
}
