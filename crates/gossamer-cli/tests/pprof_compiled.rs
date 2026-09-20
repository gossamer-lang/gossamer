//! `std::pprof` across the VM, the Cranelift build, and the LLVM
//! release build.
//!
//! The profile generators live in `gossamer_runtime::pprof`, which both
//! the interpreter builtins and the `gos_rt_pprof_*` C-ABI shims call, so
//! a profile taken under `gos run` and one taken from a `gos build
//! --release` binary must render the same shape. Sample counts move
//! between runs; the format header, the Chrome-trace envelope, and the
//! router's answers do not.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fresh_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-pprof-{pid}-{n}-{tag}",
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

fn build_native(src: &Path, release: bool, scratch: &Path) -> PathBuf {
    let mut cmd = Command::new(gos_bin());
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--out-dir").arg(scratch).arg(src);
    let out = cmd.output().expect("spawn gos build");
    assert!(
        out.status.success(),
        "gos build {flag} failed: {}",
        String::from_utf8_lossy(&out.stderr),
        flag = if release { "--release" } else { "" },
    );
    fs::read_dir(scratch)
        .expect("read scratch")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file() && is_executable(p))
        .unwrap_or_else(|| panic!("no binary in {}", scratch.display()))
}

fn stdout_of(mut cmd: Command) -> String {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .expect("run gossamer program");
    assert!(
        out.status.success(),
        "program exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

const PROBE: &str = r##"
use std::pprof

fn main() {
    println("goroutine: {}", pprof::goroutine_profile().starts_with("# pprof text format"))
    println("cpu: {}", pprof::cpu_profile(50).starts_with("# pprof text format"))
    println("heap: {}", pprof::heap_profile(50).starts_with("# pprof text format"))
    println("mutex: {}", pprof::mutex_profile().starts_with("# pprof text format"))
    println("block: {}", pprof::block_profile().starts_with("# pprof text format"))
    let trace = pprof::execution_trace(0)
    println("trace: {}", trace.starts_with("{\"traceEvents\":[") && trace.ends_with("]}"))
    match pprof::route("/debug/pprof/goroutine", "") {
        Some(body) => println("routed: {}", body.starts_with("# pprof text format"))
        None => println("routed: missing")
    }
    match pprof::route("/debug/pprof/nope", "") {
        Some(_) => println("unknown: routed")
        None => println("unknown: none")
    }
}
"##;

const EXPECTED: &str = "goroutine: true\ncpu: true\nheap: true\nmutex: true\nblock: true\ntrace: true\nrouted: true\nunknown: none";

#[test]
fn pprof_profiles_render_the_same_shape_on_every_tier() {
    let dir = fresh_dir("shape");
    let src = dir.join("probe.gos");
    fs::File::create(&src)
        .expect("create probe")
        .write_all(PROBE.as_bytes())
        .expect("write probe");

    let mut vm = Command::new(gos_bin());
    vm.arg("run").arg(&src);
    assert_eq!(stdout_of(vm).trim_end(), EXPECTED, "bytecode VM");

    for (tag, release) in [("debug", false), ("release", true)] {
        let out_dir = dir.join(tag);
        fs::create_dir_all(&out_dir).expect("create out dir");
        let bin = build_native(&src, release, &out_dir);
        assert_eq!(
            stdout_of(Command::new(&bin)).trim_end(),
            EXPECTED,
            "native {tag} build"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

/// A heap profile of a native binary carries sampled allocation stacks, not
/// just the format header.
///
/// The sampler reaches the allocating code by following the frame-pointer
/// chain out of the global allocator. The runtime shims it climbs through
/// are built with `force-frame-pointers`; without that the chain breaks at
/// the first shim, every walk records nothing, and `heap_profile` renders a
/// lone header - which is what a header-only assertion accepts. The
/// recorder owns the innermost link, so a stack of one frame is what a
/// severed chain also produces: the depth is what proves the walk climbed.
#[test]
fn a_native_heap_profile_carries_allocation_stacks() {
    let dir = fresh_dir("heap");
    let src = dir.join("heap.gos");
    fs::File::create(&src)
        .expect("create heap probe")
        .write_all(HEAP_PROBE.as_bytes())
        .expect("write heap probe");

    let out_dir = dir.join("release");
    fs::create_dir_all(&out_dir).expect("create out dir");
    let bin = build_native(&src, true, &out_dir);
    let out = stdout_of(Command::new(&bin));

    let frames: usize = out
        .lines()
        .find_map(|line| line.strip_prefix("frames="))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("probe reported no frame count; got:\n{out}"));
    assert!(
        frames > 0,
        "a heap profile of an allocating program has sampled stacks; got:\n{out}"
    );

    let deepest: usize = out
        .lines()
        .find_map(|line| line.strip_prefix("deepest="))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("probe reported no stack depth; got:\n{out}"));
    assert!(
        deepest > 1,
        "the walk climbs past the frame record the recorder owns; got:\n{out}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Allocates well past the sampling interval on one goroutine while another
/// holds the profile window open, then reports how many stack lines the
/// rendered profile carries.
const HEAP_PROBE: &str = r#"use std::pprof
use std::sync::channel

fn churn(n: i64) -> i64 {
    let mut total = 0
    for i in 0..n {
        let mut v: Vec<i64> = Vec::new()
        for k in 0..256 { v.push(k + i) }
        total += v[0]
    }
    total
}

fn profiler(tx: Sender<String>) {
    let p = pprof::heap_profile(300)
    tx.send(p)
    tx.close()
}

fn main() {
    let tx, rx = channel()
    spawn(|| profiler(tx))
    let _ = churn(200000)
    while let Some(p) = rx.recv() {
        let mut frames = 0
        let mut depth = 0
        let mut deepest = 0
        for line in p.split("\n") {
            if line.starts_with("  ") {
                frames += 1
                depth += 1
                if depth > deepest { deepest = depth }
            } else {
                depth = 0
            }
        }
        println("frames={}", frames)
        println("deepest={}", deepest)
    }
}
"#;
