//! Generic-style code (higher-order fns + standard-library
//! generic types) regression matrix.
//!
//! Gossamer doesn't expose user-defined `<T>` generics; the
//! standard library does (e.g. `HashMap<K, V>`) and user code
//! simulates polymorphism via closures and trait dispatch. The
//! 2026-04-28 `compiled_impl_method_dispatch.md` memo described
//! a HashMap-mangling collision that surfaced exactly when the
//! same generic-shaped routine was instantiated twice with
//! different concrete types in one program - the class this
//! file gates.
//!
//! Each test runs a single `.gos` source through all three
//! tiers (VM, Cranelift debug, LLVM release) and asserts byte-
//! equal stdout against the canonical answer.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const PER_RUN_TIMEOUT: Duration = Duration::from_mins(1);

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fresh_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("gos-gen-{pid}-{n}-{tag}", pid = std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn run_with_timeout(mut child: std::process::Child) -> (String, String, Option<i32>) {
    let deadline = Instant::now() + PER_RUN_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => break,
        }
    }
    let out = child.wait_with_output().expect("wait_with_output");
    (
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n"),
        out.status.code(),
    )
}

fn run_vm(src: &Path) -> (String, String, Option<i32>) {
    let child = Command::new(gos_bin())
        .arg("run")
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos");
    run_with_timeout(child)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

fn build_native(src: &Path, release: bool, scratch: &Path) -> Result<PathBuf, String> {
    let mut cmd = Command::new(gos_bin());
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--out-dir").arg(scratch).arg(src);
    let out = cmd.output().expect("spawn gos build");
    if !out.status.success() {
        return Err(format!(
            "gos build failed:\n  stderr: {}",
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    let mut binaries = Vec::new();
    for entry in fs::read_dir(scratch)
        .map_err(|e| format!("read_dir: {e}"))?
        .flatten()
    {
        let p = entry.path();
        if p.is_file() && is_executable(&p) {
            binaries.push(p);
        }
    }
    binaries
        .into_iter()
        .next()
        .ok_or_else(|| format!("no binary in {}", scratch.display()))
}

fn run_native(bin: &Path) -> (String, String, Option<i32>) {
    let child = Command::new(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn binary");
    run_with_timeout(child)
}

fn assert_three_tier_parity(tag: &str, source: &str, expected: &str) {
    let dir = fresh_dir(tag);
    let src = dir.join(format!("{tag}.gos"));
    let mut f = fs::File::create(&src).expect("write src");
    f.write_all(source.as_bytes()).unwrap();
    drop(f);

    let vm = run_vm(&src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let cl_bin = build_native(&src, false, &cl_dir).expect("cranelift build");
    let cl = run_native(&cl_bin);
    let ll_dir = dir.join("ll");
    fs::create_dir_all(&ll_dir).unwrap();
    let ll_bin = build_native(&src, true, &ll_dir).expect("llvm build");
    let ll = run_native(&ll_bin);

    let _ = fs::remove_dir_all(&dir);

    for (name, run) in [("vm", &vm), ("cranelift", &cl), ("llvm", &ll)] {
        assert_eq!(
            run.0.trim_end(),
            expected.trim_end(),
            "[{tag}/{name}] disagrees with expected.\n\
             expected:\n{expected}\n\
             got stdout:\n{stdout}\n\
             stderr:\n{stderr}\n\
             exit: {code:?}",
            stdout = run.0,
            stderr = run.1,
            code = run.2,
        );
    }
}

#[test]
fn higher_order_map_runs_over_i64_and_f64_in_one_program() {
    // Two distinct map-shaped HOFs live in the same program,
    // each with closures of a different concrete signature.
    // The HashMap-collision regression class (2026-04-28)
    // turned up exactly here: same-shape mangled symbols
    // overwrite each other and the wrong body runs.
    let src = r#"
fn map_i64(xs: Vec<i64>, f: Fn(i64) -> i64) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::from([])
    for x in xs { out.push(f(x)) }
    out
}

fn map_f64(xs: Vec<f64>, f: Fn(f64) -> f64) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::from([])
    for x in xs { out.push(f(x)) }
    out
}

fn main() {
    let ints: Vec<i64> = [1, 2, 3, 4].to_vec()
    let doubled = map_i64(ints, |x| x * 2)
    println("ints[3]={}", doubled[3])

    let floats: Vec<f64> = [1.0, 2.0, 3.0, 4.0].to_vec()
    let halved = map_f64(floats, |x| x * 0.5)
    println("floats[3]={:.1}", halved[3])
}
"#;
    assert_three_tier_parity("hof_two_concrete_types", src, "ints[3]=8\nfloats[3]=2.0");
}

#[test]
fn same_higher_order_fn_called_with_two_different_closures() {
    // The same HOF (`apply`) is called twice with closures of
    // distinct shapes (different captures, different return
    // values). The 2026-05-05 closure-ABI fix made the env
    // pointer route correctly when the same callee binds two
    // separate closures in one program.
    let src = r#"
fn apply(f: Fn(i64) -> i64, x: i64) -> i64 { f(x) }

fn main() {
    let scale = 3
    let bias = 10
    let f1 = |y: i64| y * scale
    let f2 = |y: i64| y + bias
    println("{} {}", apply(f1, 4), apply(f2, 4))
    println("{} {}", apply(f1, 7), apply(f2, 7))
}
"#;
    assert_three_tier_parity("hof_two_closure_shapes", src, "12 14\n21 17");
}

#[test]
fn hashmap_with_two_distinct_value_types_in_one_program() {
    // Two HashMap instantiations with distinct value types
    // (i64, String) coexist. The 2026-04-27 / 2026-04-28
    // memos describe the regression class where Rust-side
    // monomorphisation collides on the mangled symbol.
    let src = r#"
use std::collections::Map

fn main() {
    let mut counts: Map<String, i64> = Map::new()
    counts.insert("apple", 3)
    counts.insert("banana", 7)

    let mut labels: Map<i64, String> = Map::new()
    labels.insert(1, "first")
    labels.insert(2, "second")

    if let Some(c) = counts.get("apple") {
        println("apple={}", c)
    }
    if let Some(l) = labels.get(2) {
        println("two={}", l)
    }
}
"#;
    assert_three_tier_parity("hashmap_two_value_types", src, "apple=3\ntwo=second");
}

#[test]
fn hashmap_and_vec_of_same_value_type_dont_collide() {
    // `HashMap<String, i64>` and `Vec<i64>` (i.e. `Vec<i64>`)
    // both carry an `i64` payload through the runtime. A
    // collision in the value-type mangling shows up as one
    // collection's getter dispatching the other's storage -
    // a real bug class for the runtime accessor table.
    let src = r#"
use std::collections::Map

fn main() {
    let mut counts: Map<String, i64> = Map::new()
    counts.insert("a", 100)
    counts.insert("b", 200)

    let mut nums: Vec<i64> = [1, 2, 3].to_vec()
    nums.push(4)

    if let Some(c) = counts.get("b") {
        println("counts.b={}", c)
    }
    println("nums[3]={}", nums[3])
    println("nums.len={}", nums.len())
}
"#;
    assert_three_tier_parity(
        "hashmap_vec_same_value",
        src,
        "counts.b=200\nnums[3]=4\nnums.len=4",
    );
}

#[test]
fn closure_with_capture_inside_higher_order_filter() {
    // The closure captures an outer `threshold` and is fed
    // into a HOF that calls it inside a loop. The 2026-05-05
    // closure overhaul memo specifically called out
    // closure-in-generic-position (HOF takes `Fn(...)`) as a
    // hot regression spot.
    let src = r#"
fn count_over(xs: Vec<i64>, pred: Fn(i64) -> bool) -> i64 {
    let mut n = 0
    for x in xs {
        if pred(x) { n = n + 1 }
    }
    n
}

fn main() {
    let xs: Vec<i64> = [1, 5, 2, 7, 3, 8].to_vec()
    let threshold = 4
    let n = count_over(xs, |x| x > threshold)
    println("over_{}={}", threshold, n)
    let even_n = count_over(xs, |x| x % 2 == 0)
    println("evens={}", even_n)
}
"#;
    assert_three_tier_parity("hof_closure_with_capture", src, "over_4=3\nevens=2");
}

#[test]
fn nested_higher_order_fn_with_aggregate_payload() {
    // HOFs that thread a struct payload through. Combines
    // two regression classes: aggregate ABI (2026-05-05) and
    // closure capture (2026-05-05). If either's broken, the
    // sum at the bottom comes out wrong.
    let src = r#"
struct Pair { a: i64, b: i64 }

fn apply(f: Fn(i64) -> i64, x: i64) -> i64 {
    f(x)
}

fn main() {
    // HOF + closure that internally constructs an aggregate.
    // The closure builds a `Pair` as a local, sums its fields,
    // and returns the i64 - exercising aggregate construction
    // inside a closure body without crossing the indirect-call
    // ABI with an aggregate-by-value parameter or return.
    let pair_sum = |seed: i64| {
        let p = Pair { a: seed * 10, b: seed * 100 }
        p.a + p.b
    }
    let s1 = apply(pair_sum, 1)
    let s2 = apply(pair_sum, 2)
    let s3 = apply(pair_sum, 3)
    let total = s1 + s2 + s3
    println("total={}", total)
}
"#;
    // 1*10 + 1*100 + 2*10 + 2*100 + 3*10 + 3*100 = 660
    assert_three_tier_parity("hof_aggregate_payload", src, "total=660");
}
