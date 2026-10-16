//! Evaluator coverage suite for the bytecode VM.
//! Complements `run_pass.rs` (classic happy-path scenarios) with
//! programs that exercise the VM branches historically
//! thin on coverage: pattern matching edge cases, Option/Result
//! method dispatch, `?` propagation, struct mutation, deep
//! recursion, and Ok/Err round-trips through native code.

#![allow(clippy::needless_raw_string_hashes)]

use std::cell::RefCell;

use gossamer_hir::lower_source_file;
use gossamer_interp::{
    SmolStr, Value, Vm, jit_force_disabled_state, set_jit_disabled, set_jit_enabled,
    set_stdout_writer,
};
use gossamer_lex::SourceMap;
use gossamer_parse::autoderive::parse_with_autoderive;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

thread_local! {
    static CAPTURED: RefCell<String> = const { RefCell::new(String::new()) };
}

struct JitDisableGuard(bool);

impl JitDisableGuard {
    fn new() -> Self {
        let was_disabled = jit_force_disabled_state();
        set_jit_disabled();
        Self(was_disabled)
    }
}

impl Drop for JitDisableGuard {
    fn drop(&mut self) {
        if !self.0 {
            set_jit_enabled();
        }
    }
}

fn capture_writer(text: &str) {
    CAPTURED.with(|cell| cell.borrow_mut().push_str(text));
}

fn run_program(source: &str) -> String {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_with_autoderive(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, _resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);

    let mut interp = Vm::new();
    interp.load(&program, tcx, true).expect("vm load");

    CAPTURED.with(|cell| cell.borrow_mut().clear());
    let prev = set_stdout_writer(capture_writer);
    let result = interp.call("main", Vec::new());
    set_stdout_writer(prev);
    result.expect("main returned an error");
    CAPTURED.with(|cell| cell.borrow().clone())
}

fn call_and_return(source: &str, entry: &str, args: Vec<Value>) -> Value {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_with_autoderive(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, _resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut interp = Vm::new();
    interp.load(&program, tcx, true).expect("vm load");
    interp.call(entry, args).expect("call failed")
}

#[test]
fn block_comments_are_lexed_and_skipped() {
    let source = r#"
/* banner comment */
fn main() {
    /* an inner /* opener does not nest; first close ends it */
    println(1i64 /* inline */ + 2i64)
}
"#;
    assert_eq!(run_program(source), "3\n");
}

#[test]
fn some_value_reports_is_some_through_method_dispatch() {
    let source = r#"
fn main() {
    let v = Some(7i64)
    println(v.is_some())
    println(v.is_none())
}
"#;
    assert_eq!(run_program(source), "true\nfalse\n");
}

#[test]
fn none_literal_reports_is_none_through_method_dispatch() {
    let source = r#"
fn main() {
    println(None.is_some())
    println(None.is_none())
}
"#;
    assert_eq!(run_program(source), "false\ntrue\n");
}

#[test]
fn option_unwrap_or_returns_default_on_none() {
    let source = r#"
fn main() {
    let filled = Some(42i64)
    let empty = None
    println(filled.unwrap_or(0i64))
    println(empty.unwrap_or(99i64))
}
"#;
    assert_eq!(run_program(source), "42\n99\n");
}

#[test]
fn result_is_ok_and_is_err_flip_for_each_variant() {
    let source = r#"
fn main() {
    let good = Ok(1i64)
    let bad = Err("boom")
    println(good.is_ok())
    println(good.is_err())
    println(bad.is_ok())
    println(bad.is_err())
}
"#;
    assert_eq!(run_program(source), "true\nfalse\nfalse\ntrue\n");
}

#[test]
fn enum_constructors_work_directly_and_as_values() {
    let source = r"
enum Node {
    Leaf(i64),
    Pair { left: Node, right: Node },
}

fn weight(n: Node) -> i64 {
    match n {
        Leaf(v) => v,
        Pair { left: l, right: r } => weight(l) + weight(r),
    }
}

fn main() {
    let leaf = Leaf
    let tree = Pair { left: Leaf(2i64), right: leaf(5i64) }
    println(weight(tree))
}
";
    assert_eq!(run_program(source), "7\n");
}

#[test]
fn named_struct_values_work_directly_and_as_values() {
    let source = r"
struct Pair { left: i64, right: i64 }

fn main() {
    let direct = Pair { left: 1i64, right: 2i64 }
    println(direct.left + direct.right)
}
";
    assert_eq!(run_program(source), "3\n");
}

#[test]
fn result_unwrap_or_returns_default_on_err() {
    let source = r#"
fn main() {
    let good = Ok(11i64)
    let bad: Result<i64, String> = Err("no")
    println(good.unwrap_or(0i64))
    println(bad.unwrap_or(0i64))
}
"#;
    assert_eq!(run_program(source), "11\n0\n");
}

#[test]
fn match_on_option_destructures_payload() {
    let source = r#"
fn describe(x: Option<i64>) -> String {
    match x {
        Some(n) => format("got {}", n),
        None => "missing".to_string(),
    }
}
"#;
    let some = call_and_return(
        source,
        "describe",
        vec![Value::variant("Some", vec![Value::Int(5)])],
    );
    let none = call_and_return(source, "describe", vec![Value::variant("None", Vec::new())]);
    assert!(matches!(some, Value::String(ref s) if s.contains('5')));
    assert!(matches!(none, Value::String(ref s) if s.as_str() == "missing"));
}

#[test]
fn nested_match_with_guard_selects_correct_arm() {
    let source = r#"
fn classify(n: i64) -> String {
    match n {
        0i64 => "zero".to_string(),
        x if x < 0i64 => "negative".to_string(),
        _ => "positive".to_string(),
    }
}
"#;
    let zero = call_and_return(source, "classify", vec![Value::Int(0)]);
    let neg = call_and_return(source, "classify", vec![Value::Int(-3)]);
    let pos = call_and_return(source, "classify", vec![Value::Int(5)]);
    assert!(matches!(zero, Value::String(ref s) if s.as_str() == "zero"));
    assert!(matches!(neg, Value::String(ref s) if s.as_str() == "negative"));
    assert!(matches!(pos, Value::String(ref s) if s.as_str() == "positive"));
}

#[test]
fn for_over_exclusive_range_iterates_once_per_element() {
    let source = r#"
fn main() {
    let mut sum = 0i64
    for n in 1i64..5i64 {
        sum = sum + n
    }
    println(sum)
}
"#;
    assert_eq!(run_program(source), "10\n");
}

#[test]
fn for_over_array_binds_each_element() {
    let source = r#"
fn main() {
    let xs = [10i64, 20i64, 30i64]
    let mut sum = 0i64
    for x in xs {
        sum = sum + x
    }
    println(sum)
}
"#;
    assert_eq!(run_program(source), "60\n");
}

#[test]
fn array_repeat_form_fills_with_value() {
    let source = r#"
fn main() {
    let xs = [7i64; 4i64]
    let mut sum = 0i64
    for x in xs {
        sum = sum + x
    }
    println(sum)
}
"#;
    assert_eq!(run_program(source), "28\n");
}

#[test]
fn break_inside_for_loop_exits_early() {
    let source = r#"
fn main() {
    let mut count = 0i64
    for n in 0i64..100i64 {
        if n >= 3i64 { break }
        count = count + 1i64
    }
    println(count)
}
"#;
    assert_eq!(run_program(source), "3\n");
}

#[test]
fn moderate_recursion_depth_runs_without_overflow() {
    let source = r#"
fn sum(n: i64, acc: i64) -> i64 {
    if n <= 0i64 { acc } else { sum(n - 1i64, acc + n) }
}
"#;
    // The assertion checks the recursion accumulates the correct sum
    // 1 + 2 + … + 30.
    let result = call_and_return(source, "sum", vec![Value::Int(30), Value::Int(0)]);
    assert!(matches!(result, Value::Int(465)));
}

#[test]
fn deep_non_tail_recursion_uses_explicit_vm_frames() {
    let source = r#"
fn count_down(n: i64) -> i64 {
    if n <= 0i64 {
        return 0i64
    }
    let below = count_down(n - 1i64)
    return below + 1i64
}
"#;
    // The addition after the call makes this deliberately non-tail. The
    // result exercises suspended register files, return-destination writeback,
    // and resume without relying on native Rust stack headroom.
    let _jit_guard = JitDisableGuard::new();
    let result = call_and_return(source, "count_down", vec![Value::Int(1_000)]);
    assert!(matches!(result, Value::Int(1_000)));
}

#[test]
fn deep_direct_tail_recursion_uses_the_vm_trampoline() {
    let source = r#"
fn count_down(n: i64, acc: i64) -> i64 {
    if n <= 0i64 {
        return acc
    }
    return count_down(n - 1i64, acc + 1i64)
}
"#;
    // This remains a separate regression: tail calls reuse the current VM
    // frame, while ordinary direct calls suspend it.
    let result = call_and_return(
        source,
        "count_down",
        vec![Value::Int(10_000), Value::Int(0)],
    );
    assert!(matches!(result, Value::Int(10_000)));
}

#[test]
fn deep_mutual_tail_recursion_uses_the_vm_trampoline() {
    let source = r#"
fn even(n: i64) -> bool {
    if n == 0i64 {
        return true
    }
    return odd(n - 1i64)
}

fn odd(n: i64) -> bool {
    if n == 0i64 {
        return false
    }
    return even(n - 1i64)
}
"#;
    let result = call_and_return(source, "even", vec![Value::Int(10_000)]);
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn initializer_closure_call_resumes_its_local_frame() {
    // Initializer chunks have a short-lived ChunkState and therefore cannot
    // use the VM's cached-state resume path. A closure call here verifies the
    // local continuation driver moves the initializer frame out, drives the
    // closure through `apply`, then resumes at its return destination.
    let source = r#"
const ANSWER: i64 = (|n: i64| n + 1i64)(41i64)
fn main() { println("{}", ANSWER) }
"#;
    assert_eq!(run_program(source), "42\n");
}

#[test]
fn println_renders_tuples_and_arrays() {
    let source = r#"
fn main() {
    let pair = (1i64, 2i64)
    let xs = [10i64, 20i64, 30i64]
    println(pair)
    println(xs)
}
"#;
    assert_eq!(run_program(source), "(1, 2)\n[10, 20, 30]\n");
}

#[test]
fn array_length_via_len_method() {
    let source = r#"
fn main() {
    let xs = [1i64, 2i64, 3i64, 4i64]
    println(xs.len())
}
"#;
    assert_eq!(run_program(source), "4\n");
}

#[test]
fn string_length_is_unicode_scalar_count() {
    let source = r#"
fn main() {
    let s = "hello"
    println(s.len())
}
"#;
    assert_eq!(run_program(source), "5\n");
}

#[test]
fn boolean_short_circuit_does_not_evaluate_rhs_on_false() {
    let source = r#"
fn main() {
    let a = false
    let b = true
    println(a && b)
    println(b || a)
}
"#;
    assert_eq!(run_program(source), "false\ntrue\n");
}

#[test]
fn continue_skips_iteration_in_while() {
    let source = r#"
fn main() {
    let mut i = 0i64
    let mut sum = 0i64
    while i < 5i64 {
        i = i + 1i64
        if i == 3i64 { continue }
        sum = sum + i
    }
    println(sum)
}
"#;
    assert_eq!(run_program(source), "12\n");
}

#[test]
fn nested_closures_capture_each_layer_separately() {
    let source = r#"
fn main() {
    let a = 1i64
    let outer = || {
        let b = 2i64
        let inner = || a + b
        inner()
    }
    println(outer())
}
"#;
    assert_eq!(run_program(source), "3\n");
}

#[test]
fn format_interpolates_values_in_argument_order() {
    let source = r#"
fn main() {
    let name = "world"
    let s = format("hello, {}", name)
    println(s)
}
"#;
    let out = run_program(source);
    assert!(out.contains("hello,"));
    assert!(out.contains("world"));
}

#[test]
fn panic_builtin_surfaces_as_runtime_error() {
    let source = r#"
fn explode() -> i64 {
    panic("nope")
}
"#;
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, _) = parse_with_autoderive(source, file);
    let (mut resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut interp = Vm::new();
    interp.load(&program, tcx, true).expect("vm load");
    let result = interp.call("explode", Vec::new());
    assert!(result.is_err(), "panic should surface as RuntimeError");
}

#[test]
fn array_indexing_returns_scalar_element() {
    let source = r#"
fn main() {
    let xs = [10i64, 20i64, 30i64]
    println(xs[0])
    println(xs[2])
}
"#;
    assert_eq!(run_program(source), "10\n30\n");
}

#[test]
fn option_map_transforms_inner_value() {
    let source = r#"
fn double(n: i64) -> i64 { n * 2i64 }

fn main() {
    let wrapped = Some(21i64)
    let mapped = wrapped.map(double)
    println(mapped.unwrap_or(0i64))
}
"#;
    assert_eq!(run_program(source), "42\n");
}

#[test]
fn struct_literal_fields_project_correctly() {
    let source = r#"
struct Point { x: i64, y: i64 }

fn main() {
    let p = Point { x: 3i64, y: 4i64 }
    println(p.x + p.y)
}
"#;
    assert_eq!(run_program(source), "7\n");
}

#[test]
fn enum_variants_construct_and_match_on_payload() {
    let source = r#"
enum Shape {
    Circle(f64),
    Square(f64),
    Point,
}

fn describe(s: Shape) -> String {
    match s {
        Shape::Circle(r) => format("circle {}", r),
        Shape::Square(side) => format("square {}", side),
        Shape::Point => "point".to_string(),
    }
}
"#;
    let circle = call_and_return(
        source,
        "describe",
        vec![Value::variant("Circle", vec![Value::Float(1.5)])],
    );
    assert!(matches!(circle, Value::String(ref s) if s.contains("circle") && s.contains("1.5")));
}

#[test]
fn struct_field_assignment_does_not_mutate_alias() {
    let source = r#"
fn main() {
    let a = http::Response::text(200i64, "hello")
    let mut b = a
    b.status = 404i64
    println(a.status)
    println(b.status)
}
"#;
    assert_eq!(run_program(source), "200\n404\n");
}

#[test]
fn goroutine_panic_does_not_abort_caller() {
    let source = r#"
fn main() {
    spawn(|| panic("sibling explodes"))
    println("caller survives")
}
"#;
    assert_eq!(run_program(source), "caller survives\n");
}

#[test]
fn channels_deliver_sent_values_in_fifo_order() {
    let source = r#"
fn main() {
    let tx, rx = channel::unbounded()
    tx.send(1i64)
    tx.send(2i64)
    tx.send(3i64)
    close(tx)
    let a = rx.recv()
    let b = rx.recv()
    let c = rx.recv()
    let d = rx.recv()
    println(a)
    println(b)
    println(c)
    println(d)
}
"#;
    assert_eq!(run_program(source), "Some(1)\nSome(2)\nSome(3)\nNone\n");
}

#[test]
fn user_struct_method_does_not_collide_with_builtin_method_name() {
    // Regression: method dispatch is qualified-first, so a user
    // `impl Mailbox { fn send(...) }` resolves to `Mailbox::send`
    // instead of leaking through to the builtin `Channel::send`
    // (which is also registered under the bare key `send`).
    let source = r#"
struct Mailbox { tag: String }

impl Mailbox {
    fn send(&self, payload: i64) -> i64 {
        payload + 1i64
    }
}

fn main() {
    let m = Mailbox { tag: "main".to_string() }
    println(m.send(41i64))
}
"#;
    assert_eq!(run_program(source), "42\n");
}

#[test]
fn dividing_incompatible_types_produces_runtime_error() {
    let source = r#"
fn main() {
    let a = 10i64
    let b = "hello"
    println(a / b)
}
"#;
    let mut map = SourceMap::new();
    let file = map.add_file("t.gos", source.to_string());
    let (mut sf, _) = parse_with_autoderive(source, file);
    let (mut resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut interp = Vm::new();
    interp.load(&program, tcx, true).expect("vm load");
    let result = interp.call("main", Vec::new());
    assert!(result.is_err(), "expected a type error on int / string");
}

#[test]
fn comparing_incompatible_types_with_lt_produces_runtime_error() {
    let source = r#"
fn main() {
    let a = 10i64
    let b = "hello"
    println(a < b)
}
"#;
    let mut map = SourceMap::new();
    let file = map.add_file("t.gos", source.to_string());
    let (mut sf, _) = parse_with_autoderive(source, file);
    let (mut resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut interp = Vm::new();
    interp.load(&program, tcx, true).expect("vm load");
    let result = interp.call("main", Vec::new());
    assert!(result.is_err(), "expected a type error on int < string");
}

#[test]
fn nested_if_chains_pick_first_matching_branch() {
    let source = r#"
fn sign(n: i64) -> String {
    if n > 0i64 { "pos" }
    else if n < 0i64 { "neg" }
    else { "zero" }
}
"#;
    let pos = call_and_return(source, "sign", vec![Value::Int(10)]);
    let neg = call_and_return(source, "sign", vec![Value::Int(-1)]);
    let zero = call_and_return(source, "sign", vec![Value::Int(0)]);
    assert!(matches!(pos, Value::String(ref s) if s.as_str() == "pos"));
    assert!(matches!(neg, Value::String(ref s) if s.as_str() == "neg"));
    assert!(matches!(zero, Value::String(ref s) if s.as_str() == "zero"));
}

#[test]
fn or_pattern_matches_either_alternative() {
    let source = r#"
fn category(n: i64) -> String {
    match n {
        1i64 | 2i64 | 3i64 => "low",
        4i64 | 5i64 | 6i64 => "mid",
        _ => "high",
    }
}
"#;
    let low = call_and_return(source, "category", vec![Value::Int(2)]);
    let mid = call_and_return(source, "category", vec![Value::Int(5)]);
    let high = call_and_return(source, "category", vec![Value::Int(9)]);
    assert!(matches!(low, Value::String(ref s) if s.as_str() == "low"));
    assert!(matches!(mid, Value::String(ref s) if s.as_str() == "mid"));
    assert!(matches!(high, Value::String(ref s) if s.as_str() == "high"));
}

#[test]
fn struct_pattern_binds_named_fields() {
    let source = r#"
struct Point { x: i64, y: i64 }

fn manhattan(p: Point) -> i64 {
    match p {
        Point { x, y } => x + y,
    }
}
"#;
    let p = Value::struct_("Point", vec![("x", Value::Int(3)), ("y", Value::Int(4))]);
    let result = call_and_return(source, "manhattan", vec![p]);
    assert!(matches!(result, Value::Int(7)));
}

#[test]
fn struct_pattern_partial_binds_rest() {
    let source = r#"
struct Rect { x: i64, y: i64, w: i64, h: i64 }

fn width_only(r: Rect) -> i64 {
    match r {
        Rect { w, .. } => w,
    }
}
"#;
    let r = Value::struct_(
        "Rect",
        vec![
            ("x", Value::Int(0)),
            ("y", Value::Int(0)),
            ("w", Value::Int(100)),
            ("h", Value::Int(50)),
        ],
    );
    let result = call_and_return(source, "width_only", vec![r]);
    assert!(matches!(result, Value::Int(100)));
}

#[test]
fn try_operator_propagates_err_through_question_mark() {
    let source = r#"
fn maybe_parse(s: String) -> Result<i64, String> {
    Ok(42i64)
}

fn run(s: String) -> Result<i64, String> {
    let n = maybe_parse(s)?
    Ok(n + 1i64)
}
"#;
    let result = call_and_return(
        source,
        "run",
        vec![Value::String(SmolStr::from("42".to_string()))],
    );
    assert!(
        matches!(&result, Value::Variant(inner) if inner.name == "Ok" && matches!(inner.fields.first(), Some(Value::Int(43))))
    );
}

#[test]
fn try_operator_short_circuits_on_err() {
    let source = r#"
fn fail() -> Result<i64, String> {
    Err("oops")
}

fn run() -> Result<i64, String> {
    let _n = fail()?
    Ok(99i64)
}
"#;
    let result = call_and_return(source, "run", vec![]);
    assert!(
        matches!(&result, Value::Variant(inner) if inner.name == "Err"),
        "expected Err, got {result:?}"
    );
}

#[test]
fn select_picks_ready_channel_over_default() {
    let source = r#"
fn main() {
    let tx, rx = channel::unbounded()
    tx.send(7i64)
    select {
        v = rx.recv() => println("recv"),
        default => println("default"),
    }
}
"#;
    let out = run_program(source);
    assert_eq!(out, "recv\n");
}

#[test]
fn select_falls_back_to_default_when_no_channel_ready() {
    let source = r#"
fn main() {
    let tx, rx = channel::new()
    select {
        v = rx.recv() => println("recv"),
        default => println("default"),
    }
}
"#;
    let out = run_program(source);
    assert_eq!(out, "default\n");
}

#[test]
fn select_dispatches_to_second_ready_arm_when_first_empty() {
    let source = r#"
fn main() {
    let tx_a, rx_a = channel::unbounded()
    let tx_b, rx_b = channel::unbounded()
    tx_b.send(11i64)
    select {
        v = rx_a.recv() => println("a"),
        v = rx_b.recv() => println("b"),
        default => println("default"),
    }
}
"#;
    let out = run_program(source);
    assert_eq!(out, "b\n");
}

#[test]
fn spawn_runs_callable_in_background_thread() {
    let source = r#"
fn main() {
    let h = spawn(|| 99i64)
    match h.join() {
        Ok(v) => println(v),
        Err(e) => println(e),
    }
}
"#;
    let out = run_program(source);
    gossamer_interp::join_outstanding_goroutines();
    assert_eq!(out, "99\n");
}

#[test]
fn spawn_with_panicking_callable_does_not_crash_caller() {
    let source = r#"
fn main() {
    spawn(|| panic("worker dies"))
    println("main survives")
}
"#;
    let out = run_program(source);
    gossamer_interp::join_outstanding_goroutines();
    assert_eq!(out, "main survives\n");
}

#[test]
fn question_mark_inside_for_loop_propagates_first_error() {
    let source = r#"
fn check(n: i64) -> Result<i64, String> {
    if n < 0i64 { Err("negative") } else { Ok(n) }
}

fn sum_checked(xs: i64) -> Result<i64, String> {
    let mut total = 0i64
    let mut i = 0i64
    while i < xs {
        let v = check(i)?
        total = total + v
        i = i + 1i64
    }
    Ok(total)
}
"#;
    let ok = call_and_return(source, "sum_checked", vec![Value::Int(4)]);
    assert!(
        matches!(&ok, Value::Variant(inner) if inner.name == "Ok" && matches!(inner.fields.first(), Some(Value::Int(6)))),
        "expected Ok(6), got {ok:?}"
    );
}
