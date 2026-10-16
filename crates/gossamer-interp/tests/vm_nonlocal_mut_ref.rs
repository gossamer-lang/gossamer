//! VM-native non-local `&mut` call-argument write-back tests
//! (destroy-tree-walker Phase 7). A `&mut <field>` / `&mut <index>` /
//! `&mut <scalar local>` argument rides the write-back cell protocol:
//! the callee unwraps the cell, mutates, and the caller re-stores the
//! final value through the place. Also covers a non-numeric scalar
//! cast, which lowers to `Op::CastScalar` rather than deferring.

use std::cell::RefCell;

use gossamer_hir::lower_source_file;
use gossamer_interp::{Vm, set_stdout_writer};
use gossamer_lex::SourceMap;
use gossamer_parse::autoderive::parse_with_autoderive;
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
    let file = map.add_file("nonlocal_mut_ref.gos", source.to_string());
    let (mut sf, parse_diags) = parse_with_autoderive(source, file);
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
fn mut_ref_to_struct_field_and_index_writes_back() {
    let src = r#"
fn bump(v: &mut Vec<i64>, i: i64) { v.push(i) }
fn setfield(p: &mut i64) { *p = 999 }
struct Holder { vals: Vec<i64>, n: i64 }
fn main() {
    let mut h = Holder { vals: [10, 20], n: 5 }
    bump(&mut h.vals, 30)
    setfield(&mut h.n)
    let mut arr = [1, 2, 3]
    setfield(&mut arr[1])
    println("{} {} {} {}", h.vals.len(), h.vals[2], h.n, arr[1])
}
"#;
    assert_eq!(run_main(src), "3 30 999 999\n");
}

#[test]
fn mut_ref_to_scalar_local_writes_back() {
    let src = r#"
fn setit(p: &mut i64) { *p = 555 }
fn main() {
    let mut x = 1
    setit(&mut x)
    println("{}", x)
}
"#;
    assert_eq!(run_main(src), "555\n");
}

#[test]
fn non_numeric_scalar_casts_lower_natively() {
    let src = r#"
fn main() {
    let b = true as i64
    let c = 65 as u8 as char
    let n = 'A' as i64
    println("{} {} {}", b, c, n)
}
"#;
    assert_eq!(run_main(src), "1 A 65\n");
}
