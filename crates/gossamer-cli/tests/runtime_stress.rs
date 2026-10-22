//! End-to-end runtime stress through the language surface.
//!
//! `crates/gossamer-sched/tests/stress.rs` covers the scheduler
//! crate at scale via its public Rust API. This file exercises
//! the same regression class one layer up: a Gossamer program
//! using `go fn()` and channel send/recv at thousand-goroutine
//! scale must complete deterministically across all three
//! tiers.
//!
//! Each scenario runs in VM, Cranelift debug, and LLVM release
//! tiers. Stdout is checked against the canonical answer (a
//! single counted total, not the per-goroutine output) so the
//! tests stay deterministic even though goroutine ordering is
//! not.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const PER_RUN_TIMEOUT: Duration = Duration::from_mins(2);

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fresh_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-rstress-{pid}-{n}-{tag}",
        pid = std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

mod common;

fn run_with_timeout(mut child: std::process::Child) -> (String, String, common::RunExit) {
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
        common::describe_exit(out.status),
    )
}

fn run_vm(src: &Path) -> (String, String, common::RunExit) {
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

fn run_native(bin: &Path) -> (String, String, common::RunExit) {
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

    // Cleanup runs only on success: a panicking assert below unwinds
    // past it, preserving sources/objects/binaries for inspection and
    // CI artifact upload when a tier disagrees or a binary crashes.
    for (name, run) in [("vm", &vm), ("cranelift", &cl), ("llvm", &ll)] {
        assert_eq!(
            run.0.trim_end(),
            expected.trim_end(),
            "[{tag}/{name}] disagrees with expected.\n\
             expected:\n{expected}\n\
             got stdout:\n{stdout}\n\
             stderr:\n{stderr}\n\
             {exit}\n\
             artifacts: {dir}",
            stdout = run.0,
            stderr = run.1,
            exit = run.2.text,
            dir = dir.display(),
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn auto_region_defers_cycle_collection_until_after_the_iteration() {
    // `collect_cycles` is deliberately in the allocating loop. The MIR
    // lowerer must defer it until after the automatic arena pop: collecting
    // while region-owned nodes are still live can retain a region pointer in
    // cycle bookkeeping, while dropping the call would change the public
    // runtime contract. Run the source through the default VM/deferred-JIT
    // command and both native tiers so the ordering stays tier-safe.
    let src = r"
use std::runtime

enum Node {
    Leaf(i64),
    Pair(Node, Node),
}

fn build(depth: i64) -> Node {
    if depth == 0 { return Node::Leaf(1) }
    Node::Pair(build(depth - 1), build(depth - 1))
}

fn count(n: Node) -> i64 {
    match n {
        Node::Leaf(_) => 1,
        Node::Pair(left, right) => 1 + count(left) + count(right),
    }
}

fn main() {
    let mut total = 0
    for _ in 0..3 {
        let tree = build(5)
        total += count(tree)
        runtime::collect_cycles()
    }
    println(total)
}
";
    assert_three_tier_parity("auto_region_collect_cycles", src, "189");
}

#[test]
fn cycle_collection_capability_is_explicit_on_every_tier() {
    let dir = fresh_dir("cycle-collection-capability");
    let src = dir.join("capability.gos");
    fs::write(
        &src,
        r"
use std::runtime

fn main() {
    println(runtime::cycle_collection_supported())
}
",
    )
    .expect("write source");

    let vm = run_vm(&src);
    assert_eq!(vm.0.trim(), "false", "VM stderr: {}", vm.1);

    for (tag, release) in [("cranelift", false), ("llvm", true)] {
        let out = dir.join(tag);
        fs::create_dir_all(&out).unwrap();
        let bin = build_native(&src, release, &out).expect("build native capability probe");
        let native = run_native(&bin);
        assert_eq!(native.0.trim(), "true", "{tag} stderr: {}", native.1);
    }

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn lexical_nonescaping_region_has_vm_and_native_parity() {
    let src = r"
enum Node { Leaf(i64), Pair(Node, Node) }

fn count(n: Node) -> i64 {
    match n { Node::Leaf(_) => 1, Node::Pair(a, b) => count(a) + count(b) }
}

fn main() {
    let answer = {
        let tree = Node::Pair(Node::Leaf(1), Node::Pair(Node::Leaf(2), Node::Leaf(3)))
        count(tree)
    }
    println(answer)
}
";
    assert_three_tier_parity("lexical_auto_region", src, "3");
}

#[test]
fn ten_thousand_goroutines_send_one_value_each_and_join() {
    // 10_000 goroutines each send one i64 down a shared
    // channel; the main goroutine sums them. The channel is
    // closed after every spawn finishes via a WaitGroup-style
    // counter on the receive side.
    //
    // This is the "advertised capability" gate. Any regression
    // that strands a goroutine, drops a send, or hangs the
    // scheduler past the deadline turns this test red.
    let src = r#"
use std::sync::channel

fn main() {
    let n = 10000
    let tx, rx = channel()

    let mut i = 0
    while i < n {
        let me = i
        let txc = tx
        spawn(|| {
            txc.send(me)
        })
        i = i + 1
    }

    let mut received = 0
    let mut total = 0
    while received < n {
        match rx.recv() {
            Some(v) => {
                total = total + v
                received = received + 1
            }
            None => break,
        }
    }
    println("received={} total={}", received, total)
}
"#;
    let n = 10_000_i64;
    let total = (n * (n - 1)) / 2;
    let expected = format!("received={n} total={total}");
    assert_three_tier_parity("ten_thousand_send_each", src, &expected);
}

#[test]
fn one_thousand_goroutines_send_one_value_each_and_join() {
    // Smaller default-CI variant of the 10k test (1k). Catches
    // the same regression class but stays under a minute on
    // the slowest tier (VM).
    let src = r#"
use std::sync::channel

fn main() {
    let n = 1000
    let tx, rx = channel()

    let mut i = 0
    while i < n {
        let me = i
        let txc = tx
        spawn(|| {
            txc.send(me)
        })
        i = i + 1
    }

    let mut received = 0
    let mut total = 0
    while received < n {
        match rx.recv() {
            Some(v) => {
                total = total + v
                received = received + 1
            }
            None => break,
        }
    }
    println("received={} total={}", received, total)
}
"#;
    let n = 1_000_i64;
    let total = (n * (n - 1)) / 2;
    let expected = format!("received={n} total={total}");
    assert_three_tier_parity("one_thousand_send_each", src, &expected);
}
