//! Profiler workload runner. Runs each representative benchmark
//! through the bytecode VM with the `profile` feature enabled,
//! prints a per-workload report, and writes one combined report
//! to `~/dev/contexts/lang/interpreter_profile.txt`.
//!
//! Invocation:
//!   `cargo test --release -p gossamer-interp --features profile`
//!       `--test profile_workloads -- --nocapture profile_all`
//!
//! Skips itself silently when the feature is not enabled, so
//! `cargo test` without the feature still passes.

use std::fmt::Write;
use std::time::Instant;

use gossamer_hir::lower_source_file;
use gossamer_interp::Vm;
use gossamer_lex::SourceMap;
use gossamer_parse::autoderive::parse_with_autoderive;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

fn compile(src: &str) -> (gossamer_hir::HirProgram, TyCtxt) {
    let mut map = SourceMap::new();
    let file = map.add_file("workload.gos", src.to_string());
    let (mut sf, _) = parse_with_autoderive(src, file);
    let (mut res, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut res);
    let mut tcx = TyCtxt::new();
    let (tbl, _) = typecheck_source_file(&sf, &res, &mut tcx);
    let program = lower_source_file(&sf, &res, &tbl, &mut tcx);
    (program, tcx)
}

fn run(label: &str, src: &str) -> String {
    gossamer_interp::profile::reset();
    // Profiler audits the bytecode VM - disable JIT so hot
    // functions stay in the dispatch loop rather than
    // tier-up to native after the hot counter trips.
    gossamer_interp::set_jit_disabled();
    let (program, tcx) = compile(src);
    let mut vm = Vm::new();
    vm.load(&program, tcx, true).unwrap();
    let t0 = Instant::now();
    let _ = vm.call("main", Vec::new()).unwrap();
    let dur = t0.elapsed();
    let mut out = String::new();
    let _ = writeln!(out, "================ {label} ================");
    let _ = writeln!(out, "wall: {dur:?}");
    out.push_str(&gossamer_interp::profile::dump_report());
    out.push('\n');
    out
}

const FIB_SRC: &str = r"
fn fib(n: i64) -> i64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}
fn main() -> i64 { fib(28) }
";

const INT_LOOP_SRC: &str = r"
fn main() -> i64 {
    let mut s: i64 = 0
    let mut i: i64 = 0
    while i < 5_000_000 {
        s = s + i
        i = i + 1
    }
    s
}
";

const FLOAT_LOOP_SRC: &str = r"
fn main() -> f64 {
    let mut s: f64 = 0.0
    let mut i: i64 = 0
    while i < 2_000_000 {
        s = s + 1.5
        i = i + 1
    }
    s
}
";

const FOR_SUM_SRC: &str = r"
fn main() -> i64 {
    let mut s: i64 = 0
    for i in 0..2_000_000 {
        s = s + i
    }
    s
}
";

const PAIR_SUM_SRC: &str = r"
fn pair_sum(p: (i64, i64)) -> i64 { p.0 + p.1 }
fn main() -> i64 {
    let mut s: i64 = 0
    let mut i: i64 = 0
    while i < 200_000 {
        s = pair_sum((i, i + 1))
        i = i + 1
    }
    s
}
";

const STRUCT_FIELD_SRC: &str = r"
struct P { x: i64, y: i64 }
fn main() -> i64 {
    let mut p = P { x: 0, y: 0 }
    let mut i: i64 = 0
    while i < 500_000 {
        p.x = p.x + i
        p.y = p.y + 1
        i = i + 1
    }
    p.x + p.y
}
";

const FNV_LOOP_SRC: &str = r#"
fn fnv1a(s: str) -> i64 {
    let mut h: i64 = -3750763034362895579
    for byte in s.as_bytes().iter() {
        h ^= *byte as i64
        h *= 1099511628211
    }
    h
}
fn main() -> i64 {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 5000 {
        total ^= fnv1a("the quick brown fox jumps over the lazy dog")
        i = i + 1
    }
    total
}
"#;

const FACTORIAL_SRC: &str = r"
fn fact(n: i64) -> i64 { if n < 2 { 1 } else { n * fact(n - 1) } }
fn main() -> i64 {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 50_000 {
        total = total + fact(15)
        i = i + 1
    }
    total
}
";

const HASH_LOOP_SRC: &str = r"
fn main() -> i64 {
    let mut m: Map<i64, i64> = Map::new()
    let mut i: i64 = 0
    while i < 100_000 {
        m.insert(i % 256, i)
        i = i + 1
    }
    let mut total: i64 = 0
    let mut k: i64 = 0
    while k < 256 {
        total = total + m.get_or(k, 0)
        k = k + 1
    }
    total
}
";

#[test]
fn profile_all() {
    if !cfg!(feature = "profile") {
        eprintln!(
            "profile_all: feature `profile` not enabled, skipping. \
             Re-run with `--features profile`."
        );
        return;
    }
    let workloads: &[(&str, &str)] = &[
        ("fib(28)", FIB_SRC),
        ("int_loop_5M", INT_LOOP_SRC),
        ("float_loop_2M", FLOAT_LOOP_SRC),
        ("for_sum_2M", FOR_SUM_SRC),
        ("pair_sum_200k", PAIR_SUM_SRC),
        ("struct_field_500k", STRUCT_FIELD_SRC),
        ("fnv_loop_5k", FNV_LOOP_SRC),
        ("factorial_50k", FACTORIAL_SRC),
        ("hashmap_100k", HASH_LOOP_SRC),
    ];

    let mut combined = String::new();
    for (name, src) in workloads {
        let report = run(name, src);
        eprintln!("{report}");
        combined.push_str(&report);
    }

    // Try writing to ~/dev/contexts/lang/. Best-effort - don't
    // fail the test if the parent dir isn't writable.
    if let Some(home) = std::env::var_os("HOME") {
        let mut path = std::path::PathBuf::from(home);
        path.push("dev");
        path.push("contexts");
        path.push("lang");
        let _ = std::fs::create_dir_all(&path);
        path.push("interpreter_profile.txt");
        let _ = std::fs::write(&path, &combined);
        eprintln!("wrote combined report to {}", path.display());
    }
}
