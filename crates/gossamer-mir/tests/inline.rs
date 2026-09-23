//! Structural tests for the MIR inliners.

use gossamer_hir::lower_source_file;
use gossamer_lex::SourceMap;
use gossamer_mir::{
    Body, ConstValue, Operand, Terminator, inline_small_callees, inline_trivial_wrappers, optimise,
};
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

/// Lowers `source` to MIR with no optimisation, returning the bodies
/// and their `TyCtxt`.
fn lower(source: &str) -> (Vec<Body>, TyCtxt) {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let hir = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let bodies = gossamer_mir::lower_program(&hir, &mut tcx);
    (bodies, tcx)
}

/// Counts user-function `Call` terminators across all blocks of the
/// named body. A user-function call is either a by-name `Const(Str)`
/// targeting a known body or an `FnRef` (monomorphic user calls lower
/// to `FnRef`, so both forms must be counted).
fn call_count(bodies: &[Body], name: &str) -> usize {
    let body_names: std::collections::HashSet<&str> =
        bodies.iter().map(|b| b.name.as_str()).collect();
    let body = bodies.iter().find(|b| b.name == name).expect("body");
    body.blocks
        .iter()
        .filter(|b| match &b.terminator {
            Terminator::Call {
                callee: Operand::FnRef { .. },
                ..
            } => true,
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(n)),
                ..
            } => body_names.contains(n.as_str()),
            _ => false,
        })
        .count()
}

#[test]
fn small_callee_is_inlined_today() {
    let (mut bodies, tcx) = lower(
        "fn dbl(x: i64) -> i64 { x * 2 }\n\
         fn use_it(n: i64) -> i64 { dbl(n) + 1 }\n",
    );
    inline_trivial_wrappers(&mut bodies);
    inline_small_callees(&mut bodies, &tcx);
    for b in &mut bodies {
        optimise(b, &tcx);
    }
    assert_eq!(call_count(&bodies, "use_it"), 0, "dbl should inline away");
}

#[test]
fn inliner_is_behaviour_neutral_smoke() {
    // Structural proxy for the end-to-end differential: the same bodies
    // lowered with inlining on vs off must produce identical output
    // under the bytecode interpreter. The full cross-tier differential
    // lives in tier_parity (every feature-testing-examples fixture runs
    // with the inliner on); this in-crate test guards the MIR layer.
    let src = "fn add(a: i64, b: i64) -> i64 { a + b }\n\
               fn main() { let _ = add(2, 3)\n }\n";
    let (mut on, tcx) = lower(src);
    let (off, _) = lower(src);
    inline_small_callees(&mut on, &tcx);
    for b in &mut on {
        optimise(b, &tcx);
    }
    // `add` must be gone from any caller after inlining-on; off keeps it.
    assert_eq!(call_count(&on, "main"), 0);
    assert_eq!(call_count(&off, "main"), 1);
}

#[test]
fn six_stmt_leaf_callee_inlines_under_cost_model() {
    let (mut bodies, tcx) = lower(
        "fn poly(x: i64) -> i64 {\n\
            let a = x * x\n\
            let b = a + x\n\
            let c = b - 1\n\
            let d = c * 2\n\
            let e = d + 7\n\
            e + a\n\
         }\n\
         fn caller(n: i64) -> i64 { poly(n) + poly(n + 1) }\n",
    );
    inline_small_callees(&mut bodies, &tcx);
    for b in &mut bodies {
        optimise(b, &tcx);
    }
    assert_eq!(
        call_count(&bodies, "caller"),
        0,
        "6-stmt poly should inline"
    );
}

#[test]
fn callee_that_calls_another_fn_is_inlined() {
    let (mut bodies, tcx) = lower(
        "fn lo(x: i64) -> i64 { x + 1 }\n\
         fn mid(x: i64) -> i64 { if x > 0 { lo(x) } else { lo(-x) } }\n\
         fn top(n: i64) -> i64 { mid(n) * 2 }\n",
    );
    gossamer_mir::inline_general(&mut bodies, &tcx);
    for b in &mut bodies {
        optimise(b, &tcx);
    }
    // `mid` (a call to `lo`) inlines into `top`; `lo` then inlines into
    // the spliced body. `top` ends call-free.
    assert_eq!(
        call_count(&bodies, "top"),
        0,
        "mid+lo should inline into top"
    );
}

#[test]
fn intcode_style_indexed_helpers_inline_into_the_hot_loop() {
    let (mut bodies, tcx) = lower(
        "struct Computer { memory: Vec<i64>, position: i64 }\n\
         impl Computer {\n\
           fn get_param(self, offset: i64, mode: i64) -> i64 {\n\
             let value = self.memory[self.position + offset]\n\
             if mode == 0 { self.memory[value] } else { value }\n\
           }\n\
           fn set_memory(&mut self, offset: i64, value: i64) {\n\
             let pos = self.memory[self.position + offset]\n\
             self.memory[pos] = value\n\
           }\n\
           fn run(&mut self) {\n\
             let value = self.get_param(1, 0)\n\
             self.set_memory(3, value)\n\
           }\n\
         }\n",
    );
    gossamer_mir::inline_general(&mut bodies, &tcx);
    for body in &mut bodies {
        optimise(body, &tcx);
    }
    assert_eq!(
        call_count(&bodies, "Computer::run"),
        0,
        "small indexed helpers should not remain calls in the hot interpreter loop"
    );
}

#[test]
fn aggregate_returning_callee_inlines_and_keeps_field_types() {
    let (mut bodies, tcx) = lower(
        "struct P { x: i64, y: i64 }\n\
         fn mk(a: i64, b: i64) -> P { P { x: a, y: b } }\n\
         fn use_p(n: i64) -> i64 { let p = mk(n, n + 1)\n p.x + p.y }\n",
    );
    gossamer_mir::inline_general(&mut bodies, &tcx);
    for b in &mut bodies {
        optimise(b, &tcx);
    }
    assert_eq!(
        call_count(&bodies, "use_p"),
        0,
        "mk should inline; p.x/p.y stay typed"
    );
}

#[test]
fn self_recursive_callee_is_not_inlined() {
    let (mut bodies, tcx) = lower(
        "fn fac(n: i64) -> i64 { if n <= 0 { 1 } else { n * fac(n - 1) } }\n\
         fn run(n: i64) -> i64 { fac(n) + 1 }\n",
    );
    gossamer_mir::inline_general(&mut bodies, &tcx);
    for b in &mut bodies {
        optimise(b, &tcx);
    }
    // `fac` is self-recursive, so it is never registered as a callee; `run`
    // keeps a real call rather than splicing the body one level per pass.
    assert!(
        call_count(&bodies, "run") >= 1,
        "self-recursive callee must stay a real call"
    );
}

#[test]
fn callee_with_const_and_indexed_args_inlines() {
    let (mut bodies, tcx) = lower(
        "fn pick(xs: [i64], i: i64, bias: i64) -> i64 { xs[i] + bias }\n\
         fn run(xs: [i64]) -> i64 { pick(xs, 0, 100) + pick(xs, 1, 200) }\n",
    );
    gossamer_mir::inline_general(&mut bodies, &tcx);
    for b in &mut bodies {
        optimise(b, &tcx);
    }
    assert_eq!(
        call_count(&bodies, "run"),
        0,
        "const + indexed args still inline"
    );
}
