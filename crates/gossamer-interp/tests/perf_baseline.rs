//! Wall-clock measurements across the workloads the great-leap-forward
//! plan optimizes: call overhead (`fib`), tight numeric loops
//! (`sum_int`, `sum_float`), struct field traffic, and string-heavy
//! short loops.
//!
//! Default `cargo test` runs each bench at a smoke-test iteration
//! count so the suite stays under a minute even in debug mode -
//! these tests gate "the dispatch path doesn't crash on this
//! shape", not absolute timing. For real numbers, set
//! `GOSSAMER_TESTS_FULL=1` (or invoke the workspace-root
//! `exhaustive_test.sh`) and pass `--release`:
//! `GOSSAMER_TESTS_FULL=1 cargo test --release -p gossamer-interp
//! --test perf_baseline -- --nocapture`.
//!
//! `measure(5, ...)` reports the best of 5 runs.

use std::time::{Duration, Instant};

use gossamer_hir::lower_source_file;
use gossamer_interp::{Value, Vm};
use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

/// Multiplier applied to every loop bound below. Default 1 keeps
/// the smoke run fast; `GOSSAMER_TESTS_FULL=1` scales to the
/// original benchmark sizes (1000×) so the printed timings
/// match the historical numbers.
fn scale() -> i64 {
    if std::env::var_os("GOSSAMER_TESTS_FULL").is_some() {
        1000
    } else {
        1
    }
}

fn compile(src: &str) -> (gossamer_hir::HirProgram, TyCtxt) {
    let mut map = SourceMap::new();
    let file = map.add_file("perf.gos", src.to_string());
    let (mut sf, _) = parse_source_file(src, file);
    let (mut res, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut res);
    let mut tcx = TyCtxt::new();
    let (tbl, _) = typecheck_source_file(&sf, &res, &mut tcx);
    let program = lower_source_file(&sf, &res, &tbl, &mut tcx);
    (program, tcx)
}

fn measure<F: FnMut()>(iterations: u32, mut body: F) -> Duration {
    let mut best = Duration::from_secs(u64::MAX);
    for _ in 0..iterations {
        let start = Instant::now();
        body();
        let elapsed = start.elapsed();
        if elapsed < best {
            best = elapsed;
        }
    }
    best
}

fn run_main(program: &gossamer_hir::HirProgram, tcx: &mut TyCtxt) {
    let mut vm = Vm::new();
    // `load` consumes the interner; clone per call so the bench can
    // re-load the same program across `measure` iterations.
    vm.load(program, tcx.clone(), true).unwrap();
    let _ = vm.call("main", Vec::new()).unwrap();
}

#[test]
fn report_value_size() {
    eprintln!(
        "size_of::<Value>() = {} bytes (target ≤ 16 after B1)",
        std::mem::size_of::<Value>()
    );
}

#[test]
fn bench_fib_recursive() {
    // fib has no scalable loop bound; we keep the depth fixed and
    // skip the bench entirely outside FULL mode (recursion in the
    // bytecode VM at debug -O0 is the slowest single line of code
    // in this file).
    if std::env::var_os("GOSSAMER_TESTS_FULL").is_none() {
        return;
    }
    let src = r"
fn fib(n: i64) -> i64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}
fn main() -> i64 { fib(20) }
";
    let (program, mut tcx) = compile(src);
    let dur = measure(5, || run_main(&program, &mut tcx));
    eprintln!("fib(20)            : {dur:?}");
}

#[test]
fn bench_int_loop() {
    let n = 1_000 * scale();
    let src = format!(
        r"
fn main() -> i64 {{
    let mut s: i64 = 0;
    let mut i: i64 = 0;
    while i < {n} {{
        s = s + i;
        i = i + 1;
    }}
    s
}}
"
    );
    let (program, mut tcx) = compile(&src);
    let dur = measure(5, || run_main(&program, &mut tcx));
    eprintln!("int loop {n:>7}   : {dur:?}");
}

#[test]
fn bench_float_loop() {
    let n = 500 * scale();
    let src = format!(
        r"
fn main() -> f64 {{
    let mut s: f64 = 0.0;
    let mut i: i64 = 0;
    while i < {n} {{
        s = s + 1.5;
        i = i + 1;
    }}
    s
}}
"
    );
    let (program, mut tcx) = compile(&src);
    let dur = measure(5, || run_main(&program, &mut tcx));
    eprintln!("float loop {n:>7} : {dur:?}");
}

#[test]
fn bench_call_loop() {
    let n = 200 * scale();
    let src = format!(
        r"
fn add(a: i64, b: i64) -> i64 {{ a + b }}
fn main() -> i64 {{
    let mut s: i64 = 0;
    let mut i: i64 = 0;
    while i < {n} {{
        s = add(s, i);
        i = i + 1;
    }}
    s
}}
"
    );
    let (program, mut tcx) = compile(&src);
    let dur = measure(5, || run_main(&program, &mut tcx));
    eprintln!("call loop {n:>7}  : {dur:?}");
}

/// JIT-aggregate test: function takes/returns a Tuple. With
/// `JitKind::Value`, this should JIT-compile now (provided the
/// codegen can lower the body).
#[test]
fn bench_value_arg() {
    let n = 100 * scale();
    let src = format!(
        r"
fn pair_sum(p: (i64, i64)) -> i64 {{
    p.0 + p.1
}}
fn main() -> i64 {{
    let mut s: i64 = 0;
    let mut i: i64 = 0;
    while i < {n} {{
        s = pair_sum((i, i + 1));
        i = i + 1;
    }}
    s
}}
"
    );
    let (program, mut tcx) = compile(&src);
    let dur = measure(5, || run_main(&program, &mut tcx));
    eprintln!("pair_sum {n:>7}   : {dur:?}");
}

/// Cast in a hot loop - defers when the cast isn't natively
/// lowered.
#[test]
fn bench_cast_loop() {
    let n = 200 * scale();
    let src = format!(
        r"
fn main() -> f64 {{
    let mut s: f64 = 0.0;
    let mut i: i64 = 0;
    while i < {n} {{
        s = s + (i as f64);
        i = i + 1;
    }}
    s
}}
"
    );
    let (program, mut tcx) = compile(&src);
    let dur = measure(5, || run_main(&program, &mut tcx));
    eprintln!("cast loop {n:>7}  : {dur:?}");
}

/// Deferred-heavy: tuples are currently routed through
/// `Op::EvalDeferred`, so this measures the cliff A1 targets.
#[test]
fn bench_tuple_loop() {
    let n = 50 * scale();
    let src = format!(
        r"
fn main() -> i64 {{
    let mut s: i64 = 0;
    let mut i: i64 = 0;
    while i < {n} {{
        let pair = (i, i + 1);
        s = s + pair.0 + pair.1;
        i = i + 1;
    }}
    s
}}
"
    );
    let (program, mut tcx) = compile(&src);
    let dur = measure(5, || run_main(&program, &mut tcx));
    eprintln!("tuple loop {n:>7} : {dur:?}");
}
