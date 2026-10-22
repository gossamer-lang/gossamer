//! `time::sleep(ms)` honours millisecond units across tiers.
//!
//! Regression coverage for the 2026-05-07 daemon-launch report:
//! the cranelift / LLVM dispatch routed `time::sleep` directly
//! to `gos_rt_sleep_ns`, treating the millisecond argument as
//! nanoseconds. A `time::sleep(1000)` waited ~1 microsecond
//! instead of one second, busy-spinning every poll loop in
//! `gos build` / `gos build --release` builds. The runtime now
//! exposes `gos_rt_sleep_ms` and both backends route the
//! `time::sleep` symbol through it.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

struct TimedOutput {
    stdout: String,
    stderr: String,
    success: bool,
    elapsed_ms: u64,
}

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fresh_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-sleep-{pid}-{n}-{tag}",
        pid = std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
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

fn timed_output(start: Instant, out: std::process::Output) -> TimedOutput {
    TimedOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        success: out.status.success(),
        elapsed_ms: start.elapsed().as_millis() as u64,
    }
}

fn run_timed(bin: &Path) -> TimedOutput {
    let start = Instant::now();
    let out = Command::new(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn binary");
    timed_output(start, out)
}

fn run_vm_timed(src: &Path) -> TimedOutput {
    let start = Instant::now();
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn gos");
    timed_output(start, out)
}

fn write_source(tag: &str, body: &str) -> (PathBuf, PathBuf) {
    let dir = fresh_dir(tag);
    let src = dir.join(format!("{tag}.gos"));
    let mut f = fs::File::create(&src).expect("write src");
    f.write_all(body.as_bytes()).unwrap();
    drop(f);
    (dir, src)
}

#[test]
fn time_sleep_one_second_actually_waits_at_least_900_ms_in_all_tiers() {
    // `time::sleep(1000)` must wait ≥ 0.9 s. The buggy
    // dispatch slept for nanoseconds and the binary returned
    // in well under 5 ms, so any threshold above noise (≥ 500
    // ms) catches the regression.
    let body = r#"
use std::time
fn main() {
    println("before")
    time::sleep(1000)
    println("after")
}
"#;
    let (dir, src) = write_source("sleep_one_second", body);

    let vm = run_vm_timed(&src);
    assert!(vm.success, "vm stderr: {}", vm.stderr);
    assert!(
        vm.stdout.contains("before") && vm.stdout.contains("after"),
        "vm did not print both before/after: {:?}",
        vm.stdout
    );
    assert!(
        vm.elapsed_ms >= 900,
        "vm slept only {} ms; expected ≥ 900",
        vm.elapsed_ms
    );

    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let cl_bin = build_native(&src, false, &cl_dir).expect("cranelift build");
    let cl = run_timed(&cl_bin);
    assert!(cl.success, "cranelift stderr: {}", cl.stderr);
    assert!(
        cl.stdout.contains("before") && cl.stdout.contains("after"),
        "cranelift stdout: {:?}",
        cl.stdout
    );
    assert!(
        cl.elapsed_ms >= 900,
        "cranelift slept only {} ms; expected ≥ 900",
        cl.elapsed_ms
    );

    let ll_dir = dir.join("ll");
    fs::create_dir_all(&ll_dir).unwrap();
    let ll_bin = build_native(&src, true, &ll_dir).expect("llvm build");
    let ll = run_timed(&ll_bin);
    assert!(ll.success, "llvm stderr: {}", ll.stderr);
    assert!(
        ll.stdout.contains("before") && ll.stdout.contains("after"),
        "llvm stdout: {:?}",
        ll.stdout
    );
    assert!(
        ll.elapsed_ms >= 900,
        "llvm slept only {} ms; expected ≥ 900",
        ll.elapsed_ms
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Ceiling for a run that must not have slept.
///
/// The regressions these cases guard against are a zero or negative
/// duration reaching a real timer wait: a negative millisecond count
/// read as unsigned is not two seconds of sleep, it is billions of
/// years. So the bound only has to be shorter than "slept at all",
/// and is set well above what a cold process costs on a loaded CI
/// runner - the alternative is a gate that reports the runner's
/// scheduling rather than the runtime's behaviour.
const DID_NOT_SLEEP_MS: u64 = 30_000;

#[test]
fn time_sleep_zero_does_not_block_in_all_tiers() {
    // Zero duration must not block. Catches a would-be regression where
    // `gos_rt_sleep_ms` routed zero through a real timer wait.
    let body = r#"
use std::time
fn main() {
    time::sleep(0)
    println("done")
}
"#;
    let (dir, src) = write_source("sleep_zero", body);

    let vm = run_vm_timed(&src);
    assert!(vm.success, "vm stderr: {}", vm.stderr);
    assert!(vm.stdout.contains("done"));
    assert!(
        vm.elapsed_ms < DID_NOT_SLEEP_MS,
        "vm should not block on zero sleep, took {} ms",
        vm.elapsed_ms
    );

    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let cl_bin = build_native(&src, false, &cl_dir).expect("cranelift build");
    let cl = run_timed(&cl_bin);
    assert!(cl.success, "cranelift stderr: {}", cl.stderr);
    assert!(cl.stdout.contains("done"));
    assert!(
        cl.elapsed_ms < DID_NOT_SLEEP_MS,
        "cranelift should not block on zero sleep, took {} ms",
        cl.elapsed_ms
    );

    let ll_dir = dir.join("ll");
    fs::create_dir_all(&ll_dir).unwrap();
    let ll_bin = build_native(&src, true, &ll_dir).expect("llvm build");
    let ll = run_timed(&ll_bin);
    assert!(ll.success, "llvm stderr: {}", ll.stderr);
    assert!(ll.stdout.contains("done"));
    assert!(
        ll.elapsed_ms < DID_NOT_SLEEP_MS,
        "llvm should not block on zero sleep, took {} ms",
        ll.elapsed_ms
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn time_sleep_negative_is_rejected_in_all_tiers() {
    let body = r#"
use std::time
fn main() {
    time::sleep(-5)
    println("done")
}
"#;
    let (dir, src) = write_source("sleep_negative", body);
    let expected = "time::sleep: duration_ms must be non-negative";

    let vm = run_vm_timed(&src);
    assert!(!vm.success, "vm should reject negative sleep");
    assert!(
        vm.stderr.contains(expected),
        "vm stderr missing `{expected}`: {}",
        vm.stderr
    );
    assert!(
        !vm.stdout.contains("done"),
        "vm must not continue after negative sleep"
    );
    assert!(
        vm.elapsed_ms < DID_NOT_SLEEP_MS,
        "vm should reject negative sleep quickly, took {} ms",
        vm.elapsed_ms
    );

    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let cl_bin = build_native(&src, false, &cl_dir).expect("cranelift build");
    let cl = run_timed(&cl_bin);
    assert!(!cl.success, "cranelift should reject negative sleep");
    assert!(
        cl.stderr.contains(expected),
        "cranelift stderr missing `{expected}`: {}",
        cl.stderr
    );
    assert!(
        !cl.stdout.contains("done"),
        "cranelift must not continue after negative sleep"
    );
    assert!(
        cl.elapsed_ms < DID_NOT_SLEEP_MS,
        "cranelift should reject negative sleep quickly, took {} ms",
        cl.elapsed_ms
    );

    let ll_dir = dir.join("ll");
    fs::create_dir_all(&ll_dir).unwrap();
    let ll_bin = build_native(&src, true, &ll_dir).expect("llvm build");
    let ll = run_timed(&ll_bin);
    assert!(!ll.success, "llvm should reject negative sleep");
    assert!(
        ll.stderr.contains(expected),
        "llvm stderr missing `{expected}`: {}",
        ll.stderr
    );
    assert!(
        !ll.stdout.contains("done"),
        "llvm must not continue after negative sleep"
    );
    assert!(
        ll.elapsed_ms < DID_NOT_SLEEP_MS,
        "llvm should reject negative sleep quickly, took {} ms",
        ll.elapsed_ms
    );

    let _ = fs::remove_dir_all(&dir);
}
