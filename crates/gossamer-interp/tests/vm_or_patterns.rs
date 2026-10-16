//! Bytecode-VM regression tests for or-patterns whose alternatives
//! bind variables (`A(x) | B(x) => use(x)`). These previously routed
//! the whole `match` through the bundled tree-walker; the VM now
//! lowers them natively by allocating one shared register per bound
//! name and copying each alternative's extraction into it on the
//! winning branch. Outputs are byte-identical to the LLVM tier.

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
    let (table, type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    assert!(type_diags.is_empty(), "typecheck: {type_diags:?}");
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
fn flat_binding_or_pattern_binds_each_alternative() {
    let output = run_vm_main(
        r#"
enum Tok { Plus(i64), Minus(i64), Times(i64) }

fn classify(t: Tok) -> i64 {
    match t {
        Tok::Plus(n) | Tok::Times(n) => *n,
        Tok::Minus(n) => -*n,
    }
}

fn main() {
    println("{}", classify(Tok::Plus(5)))
    println("{}", classify(Tok::Times(7)))
    println("{}", classify(Tok::Minus(3)))
}
"#,
    );
    assert_eq!(output, "5\n7\n-3\n");
}

#[test]
fn nested_binding_or_pattern_under_some() {
    let output = run_vm_main(
        r#"
enum Wrap { Left(i64), Right(i64) }

fn inner(o: Option<Wrap>) -> i64 {
    match o {
        Some(Wrap::Left(x)) | Some(Wrap::Right(x)) => *x,
        None => -1,
    }
}

fn main() {
    println("{}", inner(Some(Wrap::Left(11))))
    println("{}", inner(Some(Wrap::Right(22))))
    println("{}", inner(None))
}
"#,
    );
    assert_eq!(output, "11\n22\n-1\n");
}

#[test]
fn binding_or_pattern_with_guard_reads_shared_slot() {
    let output = run_vm_main(
        r#"
enum Tok { Plus(i64), Times(i64) }

fn label(t: Tok) -> String {
    match t {
        Tok::Plus(n) | Tok::Times(n) if *n > 0 => format("pos {}", n),
        Tok::Plus(n) | Tok::Times(n) => format("nonpos {}", n),
    }
}

fn main() {
    println("{}", label(Tok::Plus(4)))
    println("{}", label(Tok::Times(-2)))
    println("{}", label(Tok::Plus(0)))
}
"#,
    );
    assert_eq!(output, "pos 4\nnonpos -2\nnonpos 0\n");
}

#[test]
fn binding_or_pattern_with_arc_backed_payload() {
    // A `String` payload is Arc-backed: the alternative's extraction is
    // cloned into the shared register, so RC must stay balanced as the
    // losing alternative's branch is skipped.
    let output = run_vm_main(
        r#"
enum Msg { Hello(String), Bye(String) }

fn shout(m: Msg) -> String {
    match m {
        Msg::Hello(s) | Msg::Bye(s) => format(">> {}", s),
    }
}

fn main() {
    println("{}", shout(Msg::Hello("hi")))
    println("{}", shout(Msg::Bye("later")))
}
"#,
    );
    assert_eq!(output, ">> hi\n>> later\n");
}
