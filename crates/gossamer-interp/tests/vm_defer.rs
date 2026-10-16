//! VM-native `defer` tests (destroy-tree-walker Phase 4).
//!
//! Every program here runs entirely through the bytecode [`Vm`].
//! They pin the block-scoped LIFO `defer` contract the
//! compiler now lowers natively: deferred expressions run when control
//! leaves their enclosing `{ }` block by any path - normal fall-through,
//! `return`, `break`, `continue`, and the `?` error-propagation early
//! return - in last-in-first-out order, once per loop iteration.

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
    let file = map.add_file("defer.gos", source.to_string());
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
fn defers_run_lifo_at_block_exit() {
    // Both defers run after the block body, in reverse registration order.
    let src = r#"
fn main() {
    defer println("A")
    defer println("B")
    println("body")
}
"#;
    assert_eq!(run_main(src), "body\nB\nA\n");
}

#[test]
fn defer_in_nested_block_runs_before_outer() {
    // The inner block's defer fires when control leaves the inner block,
    // before the outer block's own defers.
    let src = r#"
fn main() {
    defer println("outer")
    {
        defer println("inner")
        println("in block")
    }
    println("after block")
}
"#;
    assert_eq!(run_main(src), "in block\ninner\nafter block\nouter\n");
}

#[test]
fn defer_runs_each_loop_iteration() {
    // A defer scoped to the loop body fires once per iteration, after the
    // body's statements, reading the loop variable at the moment it runs.
    let src = r#"
fn main() {
    for i in 0..3 {
        defer println("end {}", i)
        println("iter {}", i)
    }
}
"#;
    assert_eq!(
        run_main(src),
        "iter 0\nend 0\niter 1\nend 1\niter 2\nend 2\n"
    );
}

#[test]
fn defer_runs_each_while_iteration() {
    // Same per-iteration contract through the generic `while` lowering.
    let src = r#"
fn main() {
    let mut i = 0
    while i < 3 {
        let cur = i
        defer println("end {}", cur)
        println("iter {}", cur)
        i += 1
    }
}
"#;
    assert_eq!(
        run_main(src),
        "iter 0\nend 0\niter 1\nend 1\niter 2\nend 2\n"
    );
}

#[test]
fn defer_runs_on_early_return() {
    // The defer fires on both the early-`return` path and the
    // fall-through path.
    let src = r#"
fn pick(open: bool) -> i64 {
    defer println("cleanup")
    if !open {
        return 1
    }
    2
}
fn main() {
    println("got {}", pick(false))
    println("got {}", pick(true))
}
"#;
    assert_eq!(run_main(src), "cleanup\ngot 1\ncleanup\ngot 2\n");
}

#[test]
fn defer_runs_on_break() {
    // Each iteration runs its defer; `break` runs the in-flight body's
    // defer before leaving the loop.
    let src = r#"
fn main() {
    for i in 0..5 {
        defer println("cleanup {}", i)
        if i == 2 {
            break
        }
        println("work {}", i)
    }
    println("done")
}
"#;
    assert_eq!(
        run_main(src),
        "work 0\ncleanup 0\nwork 1\ncleanup 1\ncleanup 2\ndone\n"
    );
}

#[test]
fn defer_runs_on_continue() {
    // `continue` runs the body's defer before jumping to the next
    // iteration; the skipped statement after `continue` does not run.
    let src = r#"
fn main() {
    for i in 0..3 {
        defer println("cleanup {}", i)
        if i == 1 {
            continue
        }
        println("work {}", i)
    }
    println("done")
}
"#;
    assert_eq!(
        run_main(src),
        "work 0\ncleanup 0\ncleanup 1\nwork 2\ncleanup 2\ndone\n"
    );
}

#[test]
fn defer_runs_on_question_mark_error_path() {
    // The HIR desugars `?` into a `match` with an early `return Err(..)`.
    // The defers above the `?` site must run before the error propagates,
    // on both the Ok (fall-through) and Err (early-return) paths.
    let src = r#"
fn parse(ok: bool) -> Result<i64, String> {
    if ok {
        Ok(7)
    } else {
        Err("boom")
    }
}
fn run(ok: bool) -> Result<i64, String> {
    defer println("run cleanup")
    let v = parse(ok)?
    Ok(v + 1)
}
fn main() {
    match run(true) {
        Ok(v) => println("ok {}", v),
        Err(e) => println("err {}", e),
    }
    match run(false) {
        Ok(v) => println("ok {}", v),
        Err(e) => println("err {}", e),
    }
}
"#;
    assert_eq!(run_main(src), "run cleanup\nok 8\nrun cleanup\nerr boom\n");
}

#[test]
fn defer_lifo_through_mutated_state() {
    // A defer reads the current value of any variable it touches at the
    // moment it runs (not at registration), and multiple defers unwind
    // LIFO. Pushing into a captured Vec proves aggregate mutation through
    // a deferred expression is visible after the block.
    let src = r#"
fn main() {
    let mut order: Vec<i64> = Vec::new()
    order.push(0)
    {
        defer order.push(1)
        defer order.push(2)
        order.push(9)
    }
    println("{} {} {} {}", order[0], order[1], order[2], order[3])
}
"#;
    assert_eq!(run_main(src), "0 9 2 1\n");
}
