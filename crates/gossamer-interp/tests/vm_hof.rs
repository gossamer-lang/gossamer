//! VM-native higher-order-function tests (destroy-tree-walker Phase 2).
//!
//! Every program here runs entirely through the bytecode [`Vm`].
//! The higher-order stdlib builtins they exercise
//! (`iter::for_each` / `map` / `filter` / `fold`, `sort_by`,
//! `result::map`, `option::map`) receive a `&mut dyn NativeDispatch`
//! and invoke the user callables passed to them through it. With
//! Phase 2's `impl NativeDispatch for Vm` (via the `VmDispatch`
//! adapter), those callbacks run on the VM rather than the bundled
//! tree-walker - so a correct result here proves the callback path
//! reaches user code through the VM's own call machinery.

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
    let file = map.add_file("hof.gos", source.to_string());
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
fn for_each_runs_closure_once_per_element() {
    // `iter::for_each` invokes the closure for every element through
    // `NativeDispatch::call_value` - each side-effecting print proves
    // the callback fired on the VM.
    let src = r#"
use std::iter
fn main() {
    let xs = [1, 2, 3, 4, 5]
    xs |> |v| iter::for_each(v, |n| println("each={}", n))
}
"#;
    assert_eq!(run_main(src), "each=1\neach=2\neach=3\neach=4\neach=5\n");
}

#[test]
fn pipe_into_format_calls_through_a_closure_step() {
    let src = r#"
fn main() {
    "two" |> |v| println("one {}", v)
    let text = "four" |> |v| format("three {}", v)
    println("{}", text)
    println("{:#x} {:#b} {:#o}", 255, 5, 8)
    println("{} {}", "plain", "function")
}
"#;
    assert_eq!(
        run_main(src),
        "one two\nthree four\n0xff 0b101 0o10\nplain function\n"
    );
}

#[test]
fn map_filter_fold_thread_closures_through_the_vm() {
    let src = r#"
use std::iter
fn main() {
    let xs = [1, 2, 3, 4, 5]
    let doubled = xs |> |v| iter::map(v, |n| n * 2)
    println("doubled={:?}", doubled)
    let evens = xs |> |v| iter::filter(v, |n| n % 2 == 0)
    println("evens={:?}", evens)
    let product = xs |> |v| iter::fold(v, 1, |acc, n| acc * n)
    println("product={}", product)
}
"#;
    assert_eq!(
        run_main(src),
        "doubled=#[2, 4, 6, 8, 10]\nevens=#[2, 4]\nproduct=120\n"
    );
}

#[test]
fn closure_tuple_parameter_destructuring_runs_through_hof() {
    let src = r#"
fn main() {
    let values = [1, 2, 3, 4]
    let shifted = values.iter().enumerate().map(|(i, value)| value + i).collect()
    println("{:?}", shifted)
}
"#;
    assert_eq!(run_main(src), "#[1, 3, 5, 7]\n");
}

#[test]
fn bare_fn_passed_to_a_hof_resolves_through_vm_globals() {
    // A top-level fn used as a value reaches the HOF as a
    // `Value::String` surrogate; `VmDispatch::call_value` resolves it
    // against the VM globals and applies the native chunk.
    let src = r#"
use std::iter
fn triple(n: i64) -> i64 { n * 3 }
fn main() {
    let xs = [1, 2, 3, 4, 5]
    let tripled = xs |> |v| iter::map(v, triple)
    println("tripled={:?}", tripled)
}
"#;
    assert_eq!(run_main(src), "tripled=#[3, 6, 9, 12, 15]\n");
}

#[test]
fn sort_by_with_a_closure_comparator() {
    // `sort_by` calls the comparator closure through the VM for every
    // compared pair; capturing closures and bare closures both work.
    let src = r#"
fn main() {
    let mut names = ["charlie", "alice", "bob"]
    names.sort_by(|a, b| if a < b { -1 } else if a > b { 1 } else { 0 })
    println("ascending={:?}", names)

    let mut nums: [i64; 5] = [7, 2, 9, 1, 5]
    nums.sort_by(|a, b| if a > b { -1 } else if a < b { 1 } else { 0 })
    println("descending={:?}", nums)
}
"#;
    assert_eq!(
        run_main(src),
        "ascending=[\"alice\", \"bob\", \"charlie\"]\ndescending=[9, 7, 5, 2, 1]\n"
    );
}

#[test]
fn result_map_and_option_map_callbacks_run_on_the_vm() {
    // `result::map` / `option::map` are non-`iter` HOFs that invoke
    // their closure argument via `NativeDispatch::call_value`.
    let src = r#"
use std::{result, option}
fn main() {
    let r: Result<i64, String> = Ok(21)
    let mapped = r |> |value| result::map(value, |v| v * 2)
    println("result_map={:?}", mapped)

    let o = Some(10)
    let omap = o |> |value| option::map(value, |v| v + 5)
    println("option_map={:?}", omap)
}
"#;
    assert_eq!(run_main(src), "result_map=Ok(42)\noption_map=Some(15)\n");
}

#[test]
fn capturing_closure_through_a_hof_uses_native_capture() {
    // The closure captures `factor`; routed through `iter::map`, the
    // capture rides the `Value::Closure` and the body reads it on the
    // VM's native invocation path (Phase 1 capture semantics).
    let src = r#"
use std::iter
fn main() {
    let factor = 4
    let scaled = [1, 2, 3] |> |v| iter::map(v, |n| n * factor)
    println("{:?}", scaled)
}
"#;
    assert_eq!(run_main(src), "#[4, 8, 12]\n");
}
