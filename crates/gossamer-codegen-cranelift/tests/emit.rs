//! Emits CLIF-style text for a handful of representative MIR bodies
//! and snapshots the essential structural claims (function signature,
//! presence of key mnemonics).

use gossamer_codegen_cranelift::{emit_function, emit_module};
use gossamer_hir::lower_source_file;
use gossamer_lex::SourceMap;
use gossamer_mir::{lower_program, optimise};
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

fn build_mir(source: &str) -> Vec<gossamer_mir::Body> {
    build_mir_with_tcx(source).0
}

fn build_mir_with_tcx(source: &str) -> (Vec<gossamer_mir::Body>, TyCtxt) {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let hir = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let bodies = lower_program(&hir, &mut tcx);
    (bodies, tcx)
}

#[test]
fn identity_function_renders_signature_and_return() {
    let bodies = build_mir("fn id(x: i64) -> i64 { x }\n");
    let clif = emit_function(&bodies[0]);
    assert_eq!(clif.name, "id");
    assert_eq!(clif.arity, 1);
    assert!(clif.text.starts_with("function %id("));
    assert!(clif.text.contains("v1: i64"));
    assert!(clif.text.contains("return v0"));
}

#[test]
fn binary_operator_emits_iadd_mnemonic() {
    let (mut bodies, tcx) = build_mir_with_tcx("fn add(a: i64, b: i64) -> i64 { a + b }\n");
    optimise(&mut bodies[0], &tcx);
    let clif = emit_function(&bodies[0]);
    // After copy-prop + const-fold over non-constant operands, the
    // `iadd` mnemonic is still present on the one surviving `Add`.
    assert!(clif.text.contains("iadd"), "missing iadd:\n{}", clif.text);
}

#[test]
fn if_emits_switch_int_and_multiple_blocks() {
    let bodies = build_mir("fn pick(b: bool) -> i64 { if b { 1i64 } else { 0i64 } }\n");
    let clif = emit_function(&bodies[0]);
    assert!(clif.block_count >= 3, "expected several blocks");
    assert!(clif.text.contains("switch_int"));
}

#[test]
fn direct_call_emits_call_terminator_text() {
    let bodies = build_mir("fn helper() -> i64 { 7i64 }\nfn caller() -> i64 { helper() }\n");
    let caller = bodies
        .iter()
        .find(|b| b.name == "caller")
        .expect("caller body");
    let clif = emit_function(caller);
    assert!(clif.text.contains("call "));
    assert!(clif.text.contains("-> block"));
}

#[test]
fn module_emits_one_function_per_body() {
    let bodies =
        build_mir("fn a() -> i64 { 1i64 }\nfn b() -> i64 { 2i64 }\nfn c() -> i64 { 3i64 }\n");
    let module = emit_module(&bodies);
    assert_eq!(module.functions.len(), 3);
    let names: Vec<_> = module.functions.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "c"]);
}

#[test]
fn constant_folding_collapses_binary_to_use() {
    let (mut bodies, tcx) = build_mir_with_tcx("fn compute() -> i64 { 1i64 + 2i64 }\n");
    optimise(&mut bodies[0], &tcx);
    let clif = emit_function(&bodies[0]);
    assert!(
        !clif.text.contains("iadd"),
        "iadd should have been folded away:\n{}",
        clif.text
    );
    assert!(
        clif.text.contains(" = 3"),
        "expected Int(3) literal in output:\n{}",
        clif.text
    );
}

#[test]
fn while_loop_emits_jump_and_switch_to_exit() {
    let source = r"fn main() { let mut n = 3i64
    while n > 0i64 {
        n = n - 1i64
    }
}
";
    let bodies = build_mir(source);
    let clif = emit_function(&bodies[0]);
    assert!(clif.text.contains("jump block"));
    assert!(clif.text.contains("switch_int"));
}
