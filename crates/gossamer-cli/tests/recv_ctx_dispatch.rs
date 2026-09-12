//! End-to-end `.gos`-level `rx.recv_ctx(&ctx)` dispatch test.
//!
//! Verifies the surface `rx.recv_ctx(&ctx)` typechecks, lowers
//! through MIR, runs in the bytecode VM, and links cleanly
//! under both compiled tiers. The Context comes from an
//! HTTP handler argument (the only Context surface currently
//! exposed at the .gos source level); the test's body sends
//! a value and recvs it so the dispatch path is actually
//! exercised at runtime.

#![allow(missing_docs)]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

const RECV_CTX_SRC: &str = "\
use std::http
use std::sync::channel

fn handler(r: http::Request) -> Result<http::Response, http::Error> {
    let pair = channel()
    let tx = pair.0
    let rx = pair.1
    tx.send(42)
    let v = rx.recv_ctx(r.context)
    match v {
        Some(n) => Ok(http::Response::text(200, format(\"got {}\", n))),
        None => Ok(http::Response::text(200, \"cancelled\")),
    }
}

fn main() {
    println(\"ok\")
}
";

fn write_fixture() -> PathBuf {
    // Test threads share this process, and two of them can read the clock in
    // the same tick, so the counter is what makes each fixture path its own.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "gos-recv-ctx-{}-{}-{}.gos",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    std::fs::write(&path, RECV_CTX_SRC).expect("write fixture");
    path
}

fn run_with_timeout(mut cmd: Command, label: &str) -> (String, Option<i32>) {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {label}: {e}"));
    let deadline = std::time::Instant::now() + Duration::from_mins(1);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{label} did not finish in 60s");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("{label} wait: {e}"),
        }
    }
    let out = child.wait_with_output().expect("output");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code(),
    )
}

#[test]
fn recv_ctx_typechecks_runs_and_compiles_across_every_tier() {
    let src = write_fixture();

    let mut check_cmd = Command::new(gos_bin());
    check_cmd.arg("check").arg(&src);
    let (_, check_code) = run_with_timeout(check_cmd, "check");
    assert_eq!(check_code, Some(0), "check must succeed");

    let mut vm_cmd = Command::new(gos_bin());
    vm_cmd.arg("run").arg(&src);
    let (vm_out, vm_code) = run_with_timeout(vm_cmd, "vm");
    assert_eq!(vm_code, Some(0), "vm run must succeed (stdout={vm_out:?})");

    let scratch = std::env::temp_dir().join(format!("gos-recv-ctx-build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("scratch");

    let mut dbg_cmd = Command::new(gos_bin());
    dbg_cmd
        .arg("build")
        .arg(&src)
        .arg("--out-dir")
        .arg(&scratch);
    let (_, dbg_code) = run_with_timeout(dbg_cmd, "build debug");
    assert_eq!(dbg_code, Some(0), "debug build must succeed");

    let rel_scratch = scratch.join("rel");
    std::fs::create_dir_all(&rel_scratch).expect("rel scratch");
    let mut rel_cmd = Command::new(gos_bin());
    rel_cmd
        .arg("build")
        .arg("--release")
        .arg(&src)
        .arg("--out-dir")
        .arg(&rel_scratch);
    let (_, rel_code) = run_with_timeout(rel_cmd, "build release");
    assert_eq!(rel_code, Some(0), "release build must succeed");

    let _ = std::fs::remove_dir_all(&scratch);
    let _ = std::fs::remove_file(&src);
}
