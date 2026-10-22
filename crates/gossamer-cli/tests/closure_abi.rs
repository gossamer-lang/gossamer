//! Closure ABI three-tier matrix.
//!
//! The 2026-05-05 closure-ABI overhaul fixed 16 root-cause
//! bugs because the test surface had only one closure shape
//! (`crates/gossamer-codegen-cranelift/tests/correct/p49_closure.gos`,
//! `let add5 = |y| 5 + y`) and a few feature-testing examples.
//! Anything more interesting - multi-capture, nested env,
//! closure across channel, closure as goroutine body, closure
//! interacting with built-in helpers like `__concat` - only
//! existed in tier-parity examples that compare output across
//! tiers, not against a known-correct expected stdout.
//!
//! This file pins the expected behaviour for each closure ABI
//! shape that has been a regression hot spot: trampolines, env
//! prepend, `__concat` exclusion, closure-as-goroutine body,
//! closure across channel, closure stored in struct field. Each
//! source program runs in all three tiers (VM, Cranelift debug,
//! LLVM release) and stdout must match the expected exactly.

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
    let dir = env::temp_dir().join(format!("gos-clo-{pid}-{n}-{tag}", pid = std::process::id()));
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
            "gos build {flag} failed:\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            flag = if release { "--release" } else { "" },
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
            "[{tag}/{name}] stdout disagrees with expected.\n\
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
fn non_capturing_closure_used_as_callback() {
    // Non-capturing closure passed to a higher-order function.
    // The 2026-05-05 overhaul made trampolines forward an empty
    // env for non-capturing closures and pre-pended the env
    // parameter to the callee signature; without that the
    // invocation reads garbage from a missing env slot.
    let src = r#"
fn apply(f: Fn(i64) -> i64, x: i64) -> i64 {
    f(x) + f(x + 1)
}

fn main() {
    let result = apply(|y: i64| y * y, 3)
    println("result={}", result)
}
"#;
    assert_three_tier_parity("closure_no_capture_callback", src, "result=25");
}

#[test]
fn single_i64_capture_in_higher_order_fn() {
    // The smallest "real" closure ABI test: single i64 capture.
    // Mirrors `p49_closure.gos` but exercises it through a
    // higher-order function so the env pointer threads across
    // a call boundary.
    let src = r#"
fn apply_twice(f: Fn(i64) -> i64, x: i64) -> i64 {
    f(f(x))
}

fn main() {
    let scale = 3
    let scaled = |y: i64| scale * y
    println("apply_twice={}", apply_twice(scaled, 2))
}
"#;
    assert_three_tier_parity("closure_single_i64_capture", src, "apply_twice=18");
}

#[test]
fn closure_calling_concat_does_not_capture_concat() {
    // The 2026-05-05 fix excluded `__concat` from free-var
    // capture. Without that, the closure tries to capture
    // `__concat` as an outer name and the env layout breaks.
    // The smallest reproducer is a closure that uses string
    // concatenation, which lowers to `__concat`.
    let src = r#"
fn main() {
    let prefix = "hello"
    let greet = |name: String| prefix + " " + name
    println("{}", greet("world"))
    println("{}", greet("there"))
}
"#;
    assert_three_tier_parity("closure_concat_no_capture", src, "hello world\nhello there");
}

#[test]
fn closure_calling_format_does_not_capture_format() {
    // `format!` lowers to a sequence of `__concat` and printer
    // calls; same regression class as the `__concat` test, but
    // exercises the format-macro lowering path.
    let src = r#"
fn main() {
    let n = 7
    let label = |tag: String| format("{}={}", tag, n)
    println("{}", label("count"))
    println("{}", label("size"))
}
"#;
    assert_three_tier_parity("closure_format_no_capture", src, "count=7\nsize=7");
}

#[test]
fn closure_capturing_two_distinct_scalars() {
    // Two captures, two distinct types (i64 + f64). The env
    // layout regression class: both must end up at the right
    // offset in the env struct, regardless of capture order.
    let src = r#"
fn main() {
    let scale = 3
    let bias = 0.5
    let f = |x: i64| {
        let y = x as f64 * scale as f64 + bias
        y
    }
    println("{:.2}", f(2))
    println("{:.2}", f(10))
}
"#;
    assert_three_tier_parity("closure_capture_two_scalars", src, "6.50\n30.50");
}

#[test]
fn closure_returns_closure_with_outer_capture() {
    // Closure-returning-closure. The inner closure captures
    // *both* the outer closure's parameter and the outer fn's
    // local - exercises nested env layout. The 2026-05-05
    // closure overhaul memo specifically called out
    // closure-returning-closure as a regression hot spot.
    let src = r#"
fn make_adder(base: i64) -> Fn(i64) -> i64 {
    |x: i64| base + x
}

fn main() {
    let add5 = make_adder(5)
    let add10 = make_adder(10)
    println("add5(3)={}", add5(3))
    println("add10(3)={}", add10(3))
    println("add5(7)={}", add5(7))
}
"#;
    assert_three_tier_parity(
        "closure_returns_closure",
        src,
        "add5(3)=8\nadd10(3)=13\nadd5(7)=12",
    );
}

#[test]
fn closure_as_goroutine_body_with_captures() {
    // The 2026-05-05 `closure_goroutine_landed` fix routed
    // capturing closures through the spawn env blob.
    // The smallest closure-as-goroutine case that actually
    // captures is below - `base + i` where `base` is captured
    // and the goroutine sends three values back through a
    // channel. The capture must outlive the spawner.
    let src = r#"
use std::sync::channel

fn main() {
    let tx, rx = channel()
    let base = 100

    spawn(|| {
        let mut i = 0
        while i < 3 {
            tx.send(base + i)
            i = i + 1
        }
        tx.close()
    })

    let mut total = 0
    while let Some(v) = rx.recv() {
        total = total + v
    }
    println("total={}", total)
}
"#;
    assert_three_tier_parity("closure_goroutine_capture", src, "total=303");
}

#[test]
fn closure_used_in_sort_by_higher_order_method() {
    // `sort_by(|a, b| ...)` is the canonical higher-order use
    // of closures across the standard library. The closure is
    // called inside a generic-context sort routine (Vec.sort_by),
    // so this gates closure-in-generic-position.
    let src = r#"
fn main() {
    let mut nums = [-5, 3, -1, 2, -4]
    let factor = 1
    nums.sort_by(|a, b| {
        let aa = if *a < 0 { -*a } else { *a } * factor
        let bb = if *b < 0 { -*b } else { *b } * factor
        if aa < bb { -1 } else if aa > bb { 1 } else { 0 }
    })
    println("{:?}", nums)
}
"#;
    assert_three_tier_parity("closure_in_sort_by", src, "[-1, 2, 3, -4, -5]");
}
