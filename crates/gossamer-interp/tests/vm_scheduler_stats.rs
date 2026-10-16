//! Bytecode-VM coverage for `runtime::scheduler_stats_json()`.
//!
//! The counters must describe the scheduler the caller's goroutines run on.
//! Under the interpreter that is its own worker pool, not the
//! `MultiScheduler` the compiled tiers use - which never sees a VM
//! goroutine, so reading it reported a program's own children as zero while
//! `runtime::root()` reported them correctly on the same line.

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

const STATS_SRC: &str = r#"
use std::runtime

fn tick() {
    let _ = cohort {
        let h1 = spawn(|| 1)
        let h2 = spawn(|| 2)
        let _ = h1.join()
        let _ = h2.join()
    }
    println("{}", runtime::scheduler_stats_json())
}
"#;

/// Reads `"name":<integer>` out of the snapshot.
fn counter(json: &str, name: &str) -> u64 {
    let key = format!("\"{name}\":");
    let start = json.find(&key).expect("counter present") + key.len();
    let rest = &json[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().expect("counter is an integer")
}

#[test]
fn scheduler_stats_count_the_vm_pool_goroutines() {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", STATS_SRC.to_string());
    let (mut sf, parse_diags) = parse_source_file(STATS_SRC, file);
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
    CAPTURED.with(|cell| cell.borrow_mut().clear());
    vm.call("tick", Vec::new()).expect("tick failed");
    let out = CAPTURED.with(|cell| cell.borrow().clone());
    set_stdout_writer(prev);

    let spawned = counter(&out, "spawned");
    let finished = counter(&out, "finished");
    let live = counter(&out, "live_goroutines");
    let injects = counter(&out, "injects");
    assert!(spawned >= 2, "two goroutines were spawned: {out}");
    assert!(
        injects >= 2,
        "each task is pulled from the shared queue: {out}"
    );
    // A worker settles `finished` after the task body returns, which is
    // after the `join` it releases, so a joined goroutine can still be
    // counted live for an instant. Every spawned task is on one side or
    // the other.
    assert!(
        finished + live >= 2,
        "each spawned task is finished or still live: {out}"
    );
    assert!(
        counter(&out, "worker_count") > 0,
        "the pool has workers once anything is spawned: {out}"
    );
    assert!(
        counter(&out, "worker_count_cap") > 0,
        "the worker ceiling is reported: {out}"
    );
}
