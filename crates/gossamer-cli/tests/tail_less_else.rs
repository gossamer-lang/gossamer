//! Tail-less else branches must goto the if-join.
//!
//! Regression coverage for the 2026-05-07 daemon-launch report:
//! `if cond { ... } else { let _ = fn() }` crashed compiled
//! binaries with `ud2` (illegal instruction) because MIR's
//! `lower_block` returned `None` for "block has no tail" the
//! same way it returned `None` for "block diverged", so
//! `lower_if`'s else arm skipped the join `Goto` and the
//! post-call block's default terminator stayed `Unreachable`.
//! VM ran the body correctly because the bytecode never used
//! the cranelift / LLVM `Unreachable → ud2` lowering.

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
    let dir = env::temp_dir().join(format!(
        "gos-tail-{pid}-{n}-{tag}",
        pid = std::process::id()
    ));
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
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
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
            "gos build {flag} failed:\n  stderr: {}",
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

fn assert_three_tier_stdout(tag: &str, source: &str, expected: &str) {
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
fn else_branch_with_let_underscore_call_at_tail_falls_through() {
    // Smallest repro: `if false { } else { let _ = fn_call() }`.
    // The else branch's last statement is a `let _ = ...`; the
    // block has no tail expression, so MIR's `lower_block` used
    // to return `None`, mimicking a divergent block. `lower_if`
    // then skipped the join `Goto`, leaving the post-call block
    // with `Unreachable` and the cranelift backend emitted
    // `ud2`.
    let src = r#"
fn returns_int() -> i64 { 1 }

fn main() {
    if false { } else { let _ = returns_int() }
    println("done")
}
"#;
    assert_three_tier_stdout("else_let_underscore_tail", src, "done");
}

#[test]
fn else_branch_with_unit_returning_call_at_tail_falls_through() {
    // Same shape but the let target's ty is `()` (unit), to
    // catch any per-type guard that would special-case bool /
    // i64 and miss the unit case.
    let src = r#"
fn unit_call() {
    let _ = 1
}

fn main() {
    if false { } else { unit_call() }
    println("done")
}
"#;
    assert_three_tier_stdout("else_unit_call_tail", src, "done");
}

#[test]
fn else_branch_with_assign_then_let_underscore_at_tail_falls_through() {
    // Mirrors the daemon-launch shape: a `mut local` is set
    // in the else, then a `let _ = …` cleanup runs on the same
    // line. The `let _` was the last statement of the else, so
    // the block came back tail-less and the join Goto was
    // skipped.
    let src = r#"
fn returns_int() -> i64 { 5 }

fn main() {
    let mut x: i64 = 0
    if false {
        eprintln("up")
    } else {
        x = 10
        let _ = returns_int()
    }
    println("x={}", x)
}
"#;
    assert_three_tier_stdout("else_assign_then_let_underscore", src, "x=10");
}

#[test]
fn nested_if_else_chain_with_tail_less_terminal_block() {
    // Chained if/else if/else where the final else is tail-less
    // - exercises the same `lower_block` fix transitively.
    let src = r#"
fn returns_int() -> i64 { 7 }

fn main() {
    let n = 3
    let mut tag: i64 = -1
    if n == 1 {
        tag = 100
    } else if n == 2 {
        tag = 200
    } else {
        tag = 300
        let _ = returns_int()
    }
    println("tag={}", tag)
}
"#;
    assert_three_tier_stdout("nested_if_else_tail_less", src, "tag=300");
}
