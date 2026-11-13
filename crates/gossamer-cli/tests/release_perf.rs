//! Release-tier backend regression gate.
//!
//! The 2026-04-30 spectral-norm incident (`spectral_norm_regression_fix.md`)
//! shipped a malformed `runtime_refs` entry that corrupted the LLVM IR
//! and forced unrelated bodies off the intended LLVM path.
//! Spectral-norm slowed from 0.93s to 21.6s - a 23× regression - and
//! the existing test suite was *green*. The same shape regressed again
//! a few weeks later (`spectral_norm_regression_fix.md` 2026-04-30).
//!
//! `tier_parity.rs::llvm_strict_lower_group_N` gates the case where the
//! build itself refuses a body: an LLVM lowering gap is a hard build
//! error. But a regression that lowers a body to a no-op stub or to
//! wrong-but-runnable code passes that gate. The end-to-end signal for
//! that class is wall-clock: the release
//! LLVM tier must materially outperform the debug LLVM tier on a workload
//! where `-O3` can simplify the hot loop.
//!
//! This test builds a numeric-loop workload twice, runs each, and asserts
//! both a matching answer and a release win. The comparison is the wall clock
//! of the whole process, so the loop is repeated until it dominates what a
//! host spends starting one: a workload that finishes in the time an `exec`
//! takes measures the host, not the back end.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!(
        "gos-relperf-{}-{}-{}",
        std::process::id(),
        tag,
        rand_suffix(),
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
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

fn build(src: &Path, release: bool, scratch: &Path) -> PathBuf {
    let mut cmd = Command::new(gos_bin());
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--out-dir").arg(scratch).arg(src);
    let out = cmd.output().expect("spawn gos build");
    assert!(
        out.status.success(),
        "gos build (release={release}) failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for entry in fs::read_dir(scratch).unwrap().flatten() {
        let p = entry.path();
        if p.is_file() && is_executable(&p) {
            return p;
        }
    }
    panic!("no binary in {}", scratch.display());
}

/// Runs `bin` `runs` times, returns the best (lowest) wall-clock duration and
/// what the workload printed. Best-of-N filters jitter from concurrent CI
/// load, and the answer is what says both tiers ran the same program.
fn time_best(bin: &Path, runs: u32) -> (Duration, String) {
    let mut best = Duration::from_secs(u64::MAX);
    let mut answer = String::new();
    for _ in 0..runs {
        let start = Instant::now();
        let out = Command::new(bin).output().expect("spawn bin");
        let dur = start.elapsed();
        assert!(
            out.status.success(),
            "binary exited non-zero: stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        answer = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if dur < best {
            best = dur;
        }
    }
    (best, answer)
}

/// A numeric loop where the release pipeline has a clear edge over the debug
/// one: an i64 multiply-add chain, no allocations, no calls inside the loop.
/// The final scalar is printed so neither tier can drop the work, and the
/// rounds are what put the debug tier's runtime an order of magnitude above
/// the cost of starting the process it is measured through.
const NUMERIC_LOOP_SOURCE: &str = r#"
fn main() {
    let n: i64 = 2000000
    let rounds: i64 = 16
    let mut total: i64 = 0
    let mut r: i64 = 0
    while r < rounds {
        let mut acc: i64 = 0
        let mut i: i64 = 0
        while i < n {
            acc = acc + i * i - i
            i = i + 1
        }
        total = total + acc % 1000003
        r = r + 1
    }
    println("total={}", total)
}
"#;

#[test]
fn release_tier_is_at_least_as_fast_as_debug_on_numeric_loop() {
    // Skip silently when LLVM tooling isn't on PATH - matches the
    // existing pattern in tier_parity. Without LLVM the release
    // build is just Cranelift again, so the comparison is
    // meaningless.
    if which_llc_missing() {
        eprintln!("skipping: LLVM tooling not on PATH");
        return;
    }
    let dir = fresh_dir("numeric_loop");
    let src = dir.join("loop.gos");
    fs::write(&src, NUMERIC_LOOP_SOURCE).unwrap();
    let dbg_dir = dir.join("dbg");
    fs::create_dir_all(&dbg_dir).unwrap();
    let rel_dir = dir.join("rel");
    fs::create_dir_all(&rel_dir).unwrap();

    let dbg_bin = build(&src, false, &dbg_dir);
    let rel_bin = build(&src, true, &rel_dir);

    let (dbg_time, dbg_answer) = time_best(&dbg_bin, 3);
    let (rel_time, rel_answer) = time_best(&rel_bin, 3);
    let _ = fs::remove_dir_all(&dir);

    eprintln!("debug (llvm):      {dbg_time:?} {dbg_answer}");
    eprintln!("release (llvm):    {rel_time:?} {rel_answer}");

    // A release pipeline that lowered the loop to a stub would be fast and
    // wrong, which the clock alone reads as a pass.
    assert_eq!(
        rel_answer, dbg_answer,
        "the tiers answer the same workload with the same value"
    );

    // A 10% margin leaves room for runner jitter while rejecting the issue
    // #102 fingerprint where release and debug are effectively identical.
    let bound = dbg_time.mul_f64(0.90);
    assert!(
        rel_time <= bound,
        "release tier ({rel_time:?}) did not beat debug tier ({dbg_time:?}) by 10% - \
         inspect `gos build --explain-profile`, then re-run with \
         `GOS_LLVM_DUMP=1` and inspect /tmp/gos-llvm-*/unit.ll for missing \
         user-fn `define` blocks or stale runtime_refs entries.",
    );
}

fn which_llc_missing() -> bool {
    gossamer_codegen_llvm::llvm_toolchain_status()
        .iter()
        .any(|status| status.tool == "llc" && status.resolved.is_err())
}

/// A goroutine parked in `os::fd::wait_readable` on a pipe, and the main
/// goroutine writing one byte to it every few milliseconds: each line of
/// `samples` is how long the waiter took to resume after the write, in
/// nanoseconds. The pipe comes from the C library as plain integers, the way
/// a descriptor from any foreign call arrives.
#[cfg(unix)]
const FD_WAKE_SOURCE: &str = r#"
use std::{fs, os, time}
use std::sync::channel

unsafe extern "C" {
    fn pipe(fds: &mut [i32]) -> i32
    fn write(fd: i32, buf: [u8], n: usize) -> isize
    fn read(fd: i32, buf: &mut [u8], n: usize) -> isize
}

const ROUNDS: i64 = 300

fn waiter(reader: fs::File, raw: i32, tx: Sender<i64>) {
    let mut buf = #[0u8]
    for _ in 0..ROUNDS {
        let Ok(true) = os::fd::wait_readable(reader, -1) else {
            tx.send(-1)
            return
        }
        let woke = time::monotonic_nanos()
        let _ = unsafe { read(raw, &mut buf, 1) }
        tx.send(woke)
    }
}

fn main() {
    let mut ends = #[0i32, 0i32]
    if unsafe { pipe(&mut ends) } != 0 {
        panic("pipe failed")
    }
    let Ok(reader) = fs::File::from_raw(ends[0] as i64) else {
        panic("from_raw failed")
    }
    let raw = ends[0]
    let tx, rx = channel()
    spawn(|| waiter(reader, raw, tx))
    for _ in 0..ROUNDS {
        time::sleep(time::Duration::from_millis(2))
        let sent = time::monotonic_nanos()
        let _ = unsafe { write(ends[1], "x".bytes(), 1) }
        let Some(woke) = rx.recv() else {
            panic("waiter ended")
        }
        if woke < 0 {
            panic("wait failed")
        }
        println(f"{woke - sent}")
    }
}
"#;

/// The median of `samples`.
#[cfg(unix)]
fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// The same handoff between two OS threads, the reader blocked in the kernel
/// on a socket: what the wait costs with no runtime in between.
#[cfg(unix)]
fn blocking_read_wake_latency(rounds: usize) -> Duration {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;

    let (mut writer, mut reader) = UnixStream::pair().expect("socket pair");
    let (woke_tx, woke_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let mut byte = [0u8];
        for _ in 0..rounds {
            reader.read_exact(&mut byte).expect("read a byte");
            woke_tx.send(Instant::now()).expect("report the wake");
        }
    });
    let samples = (0..rounds)
        .map(|_| {
            std::thread::sleep(Duration::from_millis(2));
            let sent = Instant::now();
            writer.write_all(b"x").expect("write a byte");
            let woke = woke_rx.recv().expect("the reader wakes");
            woke.saturating_duration_since(sent)
        })
        .collect();
    thread.join().expect("reader thread");
    median(samples)
}

/// A goroutine waiting on a descriptor resumes about as promptly as a thread
/// blocked in the kernel on one. A wait that only noticed readiness on a
/// periodic tick, or that queued the goroutine behind a sweep, would add the
/// tick to every wake; the bound is loose enough for a loaded host and far
/// below any such tick.
#[cfg(unix)]
#[test]
fn descriptor_wait_resumes_as_promptly_as_a_blocking_read() {
    if which_llc_missing() {
        eprintln!("skipping: LLVM tooling not on PATH");
        return;
    }
    let dir = fresh_dir("fd_wake");
    let src = dir.join("fd_wake.gos");
    fs::write(&src, FD_WAKE_SOURCE).unwrap();
    let bin = build(&src, true, &dir);
    let out = Command::new(&bin).output().expect("run the wake bench");
    let _ = fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "wake bench failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let samples: Vec<Duration> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            let nanos: u64 = line.trim().parse().expect("a sample in nanoseconds");
            Duration::from_nanos(nanos)
        })
        .collect();
    assert_eq!(samples.len(), 300, "one sample per round");
    let goroutine = median(samples);
    let thread = blocking_read_wake_latency(300);
    eprintln!("fd wake median: goroutine {goroutine:?}, blocked thread {thread:?}");
    let bound = thread * 4 + Duration::from_micros(250);
    assert!(
        goroutine <= bound,
        "a goroutine's descriptor wait resumed in {goroutine:?} (median), past {bound:?}; \
         a thread blocked in the kernel resumed in {thread:?}"
    );
}
