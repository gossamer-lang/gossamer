//! VM-native `spawn` tests.
//!
//! Every program runs entirely through the bytecode [`Vm`]. A
//! `spawn(|| { .. })` attaches to the cohort it is written in - `main`
//! runs inside an implicit root cohort - and the closure runs on the
//! goroutine pool. Channel + drain synchronization keeps output
//! deterministic, so a magic sleep is never needed to observe the
//! goroutine's effect.

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
    let file = map.add_file("spawn_block.gos", source.to_string());
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
fn single_producer_spawn_drains_in_order() {
    // One spawned closure produces an ordered sequence and closes the
    // channel; the `while let` drain sees the values in send order, so
    // the output is fully deterministic.
    let src = r#"
use std::sync::channel

fn compute(n: i64) -> i64 { n * n }

fn main() {
    let tx, rx = channel()
    let base = 10
    let producer = spawn(|| {
        let mut i = 0
        while i < 4 {
            tx.send(base + compute(i))
            i += 1
        }
        tx.close()
    })
    while let Some(v) = rx.recv() {
        println("got {}", v)
    }
    println("done")
}
"#;
    assert_eq!(run_main(src), "got 10\ngot 11\ngot 14\ngot 19\ndone\n");
}

#[test]
fn loop_spawning_closures_stays_native_and_aggregates() {
    // A loop body that spawns a closure each iteration must stay
    // on the bytecode path (no whole-loop defer). Each goroutine sends
    // one value; main drains a known count, so the order-independent
    // sum is deterministic regardless of scheduling.
    let src = r#"
use std::sync::channel

fn main() {
    let tx, rx = channel()
    let n = 5
    let mut i = 0
    while i < n {
        let k = i
        let worker = spawn(|| {
            tx.send(k * k)
        })
        i += 1
    }
    let mut total = 0
    let mut got = 0
    while got < n {
        if let Some(v) = rx.recv() {
            total += v
            got += 1
        }
    }
    println("total {}", total)
}
"#;
    assert_eq!(run_main(src), "total 30\n");
}

#[test]
fn spawn_in_expression_position_runs() {
    // A `spawn` used in expression position must lower natively and run
    // the goroutine.
    let src = r#"
use std::sync::channel

fn main() {
    let tx, rx = channel()
    let sender = spawn(|| {
        tx.send(7)
        tx.send(35)
        tx.close()
    })
    let mut sum = 0
    while let Some(v) = rx.recv() {
        sum += v
    }
    println("sum {}", sum)
}
"#;
    assert_eq!(run_main(src), "sum 42\n");
}
