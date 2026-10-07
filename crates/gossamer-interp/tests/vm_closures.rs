//! VM-native closure tests (destroy-tree-walker Phase 1).
//!
//! Every program here runs entirely through the bytecode [`Vm`],
//! exercising native closure creation
//! (`Op::MakeClosure`) and native invocation (the closure's compiled
//! body chunk runs with `captures ++ args` in its leading registers).

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
    let file = map.add_file("closures.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (resolutions, _resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
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
fn capturing_closure_through_hof_and_when_invoked_inline() {
    let src = r#"
fn apply_twice(f: Fn(i64) -> i64, x: i64) -> i64 { f(f(x)) }
fn main() {
    let scale = 3
    let scaled = |y: i64| scale * y
    println("inline={}", scaled(7))
    println("hof={}", apply_twice(scaled, 2))
}
"#;
    assert_eq!(run_main(src), "inline=21\nhof=18\n");
}

#[test]
fn scalar_capture_reads_the_binding_when_called() {
    // A closure names the binding it captures, so a write to the local
    // after the closure is built is what the closure reads.
    let src = r#"
fn call(f: Fn() -> i64) -> i64 { f() }
fn main() {
    let mut x = 5
    let f = || x
    x = 99
    println("{} {}", call(f), x)
}
"#;
    assert_eq!(run_main(src), "99 99\n");
}

#[test]
fn aggregate_capture_reads_contents_at_capture_time() {
    // A captured aggregate flows into the closure body: indexing the
    // captured array returns its element values.
    let src = r#"
fn at(f: Fn(i64) -> i64, i: i64) -> i64 { f(i) }
fn main() {
    let xs = [10, 20, 30]
    let get = |i: i64| xs[i]
    println("{} {} {}", at(get, 0), at(get, 1), at(get, 2))
}
"#;
    assert_eq!(run_main(src), "10 20 30\n");
}

#[test]
fn captured_sequence_mutation_reaches_the_enclosing_binding() {
    // A heap sequence is captured by managed reference (SPEC 4.6): the
    // closure's upvalue and the enclosing binding name one buffer, so a
    // push made through the closure - even one invoked from another
    // frame - is observed by the binding, matching the compiled tiers.
    let src = r#"
fn run(f: Fn()) { f() }
fn main() {
    let mut v: Vec<i64> = #[1, 2]
    let pusher = || { v.push(3) }
    run(pusher)
    println("{:?}", v)
}
"#;
    assert_eq!(run_main(src), "#[1, 2, 3]\n");
}

#[test]
fn escaping_closure_called_after_its_creator_returns() {
    // Each `make_adder` returns a closure that outlives the call; the
    // captured `n` rides the `Value::Closure` Arc.
    let src = r#"
fn make_adder(n: i64) -> Fn(i64) -> i64 { |x| x + n }
fn main() {
    let add5 = make_adder(5)
    let add10 = make_adder(10)
    println("{} {}", add5(1), add10(1))
}
"#;
    assert_eq!(run_main(src), "6 11\n");
}

#[test]
fn closures_compose_capturing_other_closures() {
    let src = r#"
fn make_adder(n: i64) -> Fn(i64) -> i64 { |x| x + n }
fn make_mul(n: i64) -> Fn(i64) -> i64 { |x| x * n }
fn compose(f: Fn(i64) -> i64, g: Fn(i64) -> i64) -> Fn(i64) -> i64 { |x| f(g(x)) }
fn main() {
    let h = compose(make_adder(5), make_mul(3))
    println("{}", h(4))
}
"#;
    // h(4) = (4 * 3) + 5 = 17
    assert_eq!(run_main(src), "17\n");
}

#[test]
fn deep_non_tail_closure_calls_use_explicit_vm_frames() {
    // Every recursive step first calls a closure, then performs work after
    // that call. This used to build `run -> dispatch_call -> invoke_closure`
    // chains on the Rust stack; the closure call is now a suspended VM frame.
    let src = r#"
fn count_down(n: i64) -> i64 {
    if n <= 0i64 { return 0i64 }
    let recurse = |x: i64| count_down(x)
    let below = recurse(n - 1i64)
    return below + 1i64
}
fn main() { println("{}", count_down(1000)) }
"#;
    assert_eq!(run_main(src), "1000\n");
}
