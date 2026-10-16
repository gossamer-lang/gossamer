//! VM-native `select { … }` tests (destroy-tree-walker Phase 3).
//!
//! Every program here runs entirely through the bytecode [`Vm`],
//! exercising native `select` lowering (`Op::Select`
//! over a `select_arms` table) and the runtime poll/park loop over
//! `Value::Channel`. The four scenarios pin the `select`
//! semantics: a ready recv arm beats `default`, a send arm on an
//! explicit unbounded channel is always ready, `default` fires when
//! nothing is ready, and a `default`-less select parks until a producer
//! goroutine sends.

use gossamer_hir::lower_source_file;
use gossamer_interp::{Vm, set_stdout_writer};
use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};
use std::cell::RefCell;

thread_local! {
    static CAPTURED: RefCell<String> = const { RefCell::new(String::new()) };
}

fn capture_writer(text: &str) {
    CAPTURED.with(|cell| cell.borrow_mut().push_str(text));
}

fn run_main(source: &str) -> String {
    let mut map = SourceMap::new();
    let file = map.add_file("select.gos", source.to_string());
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
fn ready_recv_arm_beats_default() {
    let src = r#"
use std::sync::channel
fn main() {
    let tx, rx = channel::unbounded()
    tx.send(7)
    let v = select {
        x = rx.recv() => x,
        default => -1,
    }
    println("v={}", v)
}
"#;
    assert_eq!(run_main(src), "v=7\n");
}

#[test]
fn send_arm_on_unbounded_channel_is_ready() {
    let src = r#"
use std::sync::channel
fn main() {
    let tx, rx = channel::unbounded()
    select {
        tx.send(99) => println("sent"),
        default => println("not sent"),
    }
    if let Some(got) = rx.recv() {
        println("got={}", got)
    }
}
"#;
    assert_eq!(run_main(src), "sent\ngot=99\n");
}

#[test]
fn default_fires_when_nothing_ready() {
    let src = r#"
use std::sync::channel
fn main() {
    let _tx, rx = channel()
    let v = select {
        x = rx.recv() => x,
        default => -1,
    }
    println("v={}", v)
}
"#;
    assert_eq!(run_main(src), "v=-1\n");
}

#[test]
fn defaultless_select_parks_until_producer_sends() {
    // No `default` arm: the select must park on the recv channel's
    // condvar and wake when the producer goroutine sends. The producer
    // closes the channel afterward; output is deterministic regardless
    // of whether the value lands before or after the park begins.
    let src = r#"
use std::sync::channel
use std::time

fn producer(tx: channel::Sender<i64>) {
    time::sleep(20)
    tx.send(42)
    tx.close()
}

fn main() {
    let tx, rx = channel()
    spawn(|| producer(tx))
    let v = select {
        x = rx.recv() => x,
    }
    println("got={}", v)
}
"#;
    assert_eq!(run_main(src), "got=42\n");
}

#[test]
fn context_timeout_done_chan_wakes_select() {
    let src = r#"
use std::context

fn main() {
    let root = context::Context::background()
    let ctx = context::Context::with_timeout(root, 1)
    let v = select {
        _ = ctx.done_chan().recv() => 1,
    }
    println("timeout={}", v)
}
"#;
    assert_eq!(run_main(src), "timeout=1\n");
}

#[test]
fn context_timeout_cancels_recv_ctx() {
    let src = r#"
use std::context
use std::sync::channel

fn main() {
    let root = context::Context::background()
    let ctx = context::Context::with_timeout(root, 1)
    let _tx, rx = channel()
    match rx.recv_ctx(ctx) {
        Some(_) => println("value"),
        None => println("cancelled"),
    }
}
"#;
    assert_eq!(run_main(src), "cancelled\n");
}
