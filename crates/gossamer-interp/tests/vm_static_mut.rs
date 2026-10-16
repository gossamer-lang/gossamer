//! Bytecode-VM coverage for `static mut` storage.
//!
//! A mutable static is backed by a single shared cell (`Global::MutStatic`)
//! that every reader and writer - including spawned goroutines, which clone
//! the globals `Arc` - observes. These tests pin the single-threaded
//! read/write semantics and the cross-goroutine sharing.

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

fn run_vm_main(source: &str) -> String {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
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
fn static_mut_write_is_observable_across_functions() {
    let source = r#"
static mut COUNTER: i64 = 0

fn bump() { COUNTER += 1 }
fn read() -> i64 { COUNTER }

fn main() {
    println("start = {}", read())
    bump()
    bump()
    bump()
    println("after 3 bumps = {}", read())
    COUNTER = 100
    println("after assign 100 = {}", read())
}
"#;
    let out = run_vm_main(source);
    assert_eq!(
        out,
        "start = 0\nafter 3 bumps = 3\nafter assign 100 = 100\n"
    );
}

#[test]
fn static_mut_shared_across_goroutines() {
    // Two goroutines each bump a shared `static mut` 1000 times. They are
    // joined in sequence so they never overlap on the non-atomic
    // read-modify-write, which makes the total deterministic. Each
    // goroutine runs on a pool thread whose child `Vm` shares the globals
    // `Arc` (and thus the one cell): goroutine 2 observes goroutine 1's
    // writes, and `main` observes both. If the cell were not shared the
    // total would not reach 2000.
    let source = r#"
static mut COUNTER: i64 = 0

fn bump_n(n: i64) -> i64 {
    let mut i = 0
    while i < n {
        COUNTER += 1
        i += 1
    }
    COUNTER
}

fn main() {
    let h1 = spawn(|| bump_n(1000))
    let mid = h1.join()
    let h2 = spawn(|| bump_n(1000))
    let _ = h2.join()
    println("mid = {}", mid)
    println("counter = {}", COUNTER)
}
"#;
    let out = run_vm_main(source);
    assert_eq!(out, "mid = Ok(1000)\ncounter = 2000\n");
}
