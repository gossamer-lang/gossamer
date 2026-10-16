//! Bytecode-VM coverage for `sync::Mutex` mutual exclusion.
//!
//! A `sync::Mutex` must hold the lock from `lock()` until the matching
//! `unlock()`, so a non-atomic read-modify-write performed by user code
//! between the two is serialized across goroutines. `tick()` spawns two
//! goroutines that each bump a shared `static mut` 1000 times under the
//! lock - both spawned before either is joined, so they genuinely
//! overlap on a pool thread each - and must total exactly 2000. Without
//! real exclusion the concurrent read-modify-write loses updates and
//! the total falls short of 2000.
//!
//! The program is loaded once and `tick()` is called repeatedly on the
//! same `Vm` (the execution model `gos test` uses), so a probabilistic
//! lost-update regression is caught across many overlapping runs rather
//! than flaking through a single one.

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

const MUTEX_COUNTER_SRC: &str = r#"
use std::sync::Mutex

static mut COUNTER: i64 = 0

fn bump(m: Mutex) {
    for _ in 0..1000 {
        m.lock()
        COUNTER += 1
        m.unlock()
    }
}

fn tick() {
    COUNTER = 0
    let m = Mutex::new()
    let _ = cohort {
        let h1 = spawn(|| bump(m))
        let h2 = spawn(|| bump(m))
        let _ = h1.join()
        let _ = h2.join()
    }
    println("{}", COUNTER)
}
"#;

#[test]
fn mutex_serializes_concurrent_compound_update() {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", MUTEX_COUNTER_SRC.to_string());
    let (mut sf, parse_diags) = parse_source_file(MUTEX_COUNTER_SRC, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, _resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    assert!(type_diags.is_empty(), "typecheck: {type_diags:?}");
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut vm = Vm::new();
    vm.load(&program, tcx, true).expect("load");

    let prev = set_stdout_writer(capture_writer);
    for run in 0..25 {
        CAPTURED.with(|cell| cell.borrow_mut().clear());
        let result = vm.call("tick", Vec::new());
        result.expect("tick failed");
        let out = CAPTURED.with(|cell| cell.borrow().clone());
        assert_eq!(out, "2000\n", "run {run} lost updates under the mutex");
    }
    set_stdout_writer(prev);
}
