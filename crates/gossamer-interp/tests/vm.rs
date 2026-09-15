//! Bytecode-VM run-pass tests.
//! Mirrors the interpreter run-pass corpus against the register-based
//! bytecode VM so the two implementations are observed to agree.

use std::cell::RefCell;

use gossamer_hir::lower_source_file;
use gossamer_interp::{Value, Vm, set_stdout_writer};
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

fn build_vm(source: &str) -> Vm {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (resolutions, _resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut vm = Vm::new();
    vm.load(&program, tcx, true).expect("load");
    vm
}

fn run_vm_main(source: &str) -> String {
    let vm = build_vm(source);
    CAPTURED.with(|cell| cell.borrow_mut().clear());
    let prev = set_stdout_writer(capture_writer);
    let result = vm.call("main", Vec::new());
    set_stdout_writer(prev);
    result.expect("main failed");
    CAPTURED.with(|cell| cell.borrow().clone())
}

#[test]
fn vm_prints_hello() {
    let output = run_vm_main("fn main() { println(\"hello\") }\n");
    assert_eq!(output, "hello\n");
}

#[test]
fn vm_evaluates_arithmetic_expression() {
    let vm = build_vm(
        "fn add(a: i64, b: i64) -> i64 { a + b }\nfn main() { println(add(1i64, 2i64)) }\n",
    );
    CAPTURED.with(|cell| cell.borrow_mut().clear());
    let prev = set_stdout_writer(capture_writer);
    vm.call("main", Vec::new()).expect("main failed");
    set_stdout_writer(prev);
    let output = CAPTURED.with(|cell| cell.borrow().clone());
    assert_eq!(output, "3\n");
}

#[test]
fn vm_string_byte_at_loop_preserves_typed_integer_semantics() {
    let output = run_vm_main(
        r#"
fn main() {
    let text = "Gossamer"
    let mut i: i64 = 0
    let mut checksum: i64 = 0
    while i < text.len() {
        checksum = checksum +% text.byte_at(i)
        i += 1
    }
    println("{} {} {}", text.byte_at(-1), checksum, text.byte_at(text.len()))
}
"#,
    );
    assert_eq!(output, "0 833 0\n");
}

#[test]
fn vm_packs_float_struct_arrays_built_by_constructor_helpers() {
    let vm = build_vm(
        r"
struct Body { x: f64, y: f64 }
fn body(x: f64, y: f64) -> Body { Body { x: x, y: y } }
fn sum_bodies() -> f64 {
    let mut bodies: [Body; 2] = [body(1.0, 2.0), body(3.0, 4.0)]
    bodies[1].x = bodies[0].y + 5.0
    bodies[0].x + bodies[1].x + bodies[1].y
}
fn main() {}
",
    );
    let result = vm.call("sum_bodies", Vec::new()).expect("sum_bodies");
    assert!(matches!(result, Value::Float(value) if value == 12.0));
}

#[test]
fn vm_persists_statement_position_byte_vector_pushes() {
    let source = r#"
fn main() {
    let mut values: Vec<u8> = Vec::from([])
    let mut i = 0
    while i < 6 {
        values.push((i * 40 + 3) as u8)
        i += 1
    }
    println("{} {} {} {} {} {} len {}", values[0], values[1], values[2], values[3], values[4], values[5], values.len())
}
"#;

    assert_eq!(run_vm_main(source), "3 43 83 123 163 203 len 6\n");
}

#[test]
fn vm_if_else_picks_correct_branch() {
    let source = r"
fn pick(n: i64) -> i64 {
    if n > 0i64 { n } else { -n }
}
";
    let vm = build_vm(source);
    match vm.call("pick", vec![Value::Int(-5)]).unwrap() {
        Value::Int(v) => assert_eq!(v, 5),
        other => panic!("unexpected result: {other:?}"),
    }
    match vm.call("pick", vec![Value::Int(7)]).unwrap() {
        Value::Int(v) => assert_eq!(v, 7),
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn vm_while_loop_counts_down() {
    let source = r"
fn main() {
    let mut n = 3i64
    while n > 0i64 {
        println(n)
        n = n - 1i64
    }
}
";
    assert_eq!(run_vm_main(source), "3\n2\n1\n");
}

#[test]
fn vm_loop_with_break_returns_value() {
    let source = r"
fn main() {
    let mut n = 0i64
    let r = loop {
        if n >= 3i64 { break n * 2i64 }
        n = n + 1i64
    }
    println(r)
}
";
    assert_eq!(run_vm_main(source), "6\n");
}

#[test]
fn vm_handles_recursive_call() {
    let source = r"
fn factorial(n: i64) -> i64 {
    if n <= 1i64 { 1i64 } else { n * factorial(n - 1i64) }
}
";
    let vm = build_vm(source);
    let result = vm.call("factorial", vec![Value::Int(6)]).unwrap();
    match result {
        Value::Int(v) => assert_eq!(v, 720),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn vm_short_circuits_logical_operators() {
    let source = r"
fn main() {
    let f = false
    let t = true
    println(f && t)
    println(t && t)
    println(f || t)
}
";
    assert_eq!(run_vm_main(source), "false\ntrue\ntrue\n");
}

#[test]
fn vm_arithmetic_evaluates_expression() {
    let source = r"
fn compute(a: i64, b: i64) -> i64 {
    (a + b) * (a - b) + a * b
}
";
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, _) = parse_source_file(source, file);
    let (resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);

    let mut vm = Vm::new();
    vm.load(&program, tcx, true).unwrap();

    for (a, b) in [(1, 2), (3, 4), (10, 3), (-5, 7)] {
        let args = vec![Value::Int(a), Value::Int(b)];
        let expected = (a + b) * (a - b) + a * b;
        let result = vm.call("compute", args).unwrap();
        assert!(
            matches!(&result, Value::Int(x) if *x == expected),
            "mismatch on ({a}, {b}): vm={result:?} expected={expected}"
        );
    }
}

#[test]
fn runtime_collect_cycles_is_callable_on_vm() {
    let source = r#"
use std::runtime

fn main() {
    let mut values: Vec<String> = Vec::new()
    for i in 0i64..1000i64 {
        values.push(format("item-{}", i))
    }
    values = Vec::new()
    runtime::collect_cycles()
    println("collected")
}
"#;
    assert_eq!(run_vm_main(source), "collected\n");
}

#[test]
fn runtime_cycle_collection_capability_reports_vm_limit() {
    let source = r"
use std::runtime

fn main() {
    println(runtime::cycle_collection_supported())
}
";
    assert_eq!(run_vm_main(source), "false\n");
}
