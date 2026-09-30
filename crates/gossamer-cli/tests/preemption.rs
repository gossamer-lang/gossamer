//! A compiled loop that calls nothing is preempted: with one scheduler worker,
//! a peer goroutine still runs while another spins, on the bytecode VM, the
//! JIT, and a native release build.

#![allow(missing_docs)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

/// How long a run may take before it counts as held by the spinner. The
/// program itself finishes in milliseconds once the spinner yields.
const LIVENESS_BOUND: Duration = Duration::from_mins(1);

fn gos() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/preempt_tight_loop.gos")
}

/// Runs `command` on one worker, failing the test if it does not finish.
fn run_on_one_worker(mut command: Command, tier: &str) -> Output {
    let mut child = command
        .env("GOSSAMER_MAX_PROCS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start program");
    let finished = child
        .wait_timeout(LIVENESS_BOUND)
        .expect("wait for program")
        .is_some();
    if !finished {
        let _ = child.kill();
        let _ = child.wait();
        panic!("{tier}: the spinning goroutine held the only worker");
    }
    child.wait_with_output().expect("collect output")
}

fn assert_peer_ran(output: &Output, tier: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "peer ran: Some(1)",
        "{tier}: stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_spinning_goroutine_yields_on_the_vm() {
    let mut command = Command::new(gos());
    command.arg("run").arg("--no-jit").arg(fixture());
    assert_peer_ran(&run_on_one_worker(command, "vm"), "vm");
}

#[test]
fn a_spinning_goroutine_yields_under_the_jit() {
    let mut command = Command::new(gos());
    command
        .env("GOSSAMER_JIT_THRESHOLD", "1")
        .arg("run")
        .arg(fixture());
    assert_peer_ran(&run_on_one_worker(command, "jit"), "jit");
}

#[test]
fn a_spinning_goroutine_yields_in_a_release_build() {
    let dir = std::env::temp_dir().join(format!("gos-preempt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let build = Command::new(gos())
        .current_dir(&dir)
        .arg("build")
        .arg("--release")
        .arg(fixture())
        .arg("--out-dir")
        .arg(&dir)
        .output()
        .expect("run gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join(if cfg!(windows) {
        "preempt_tight_loop.exe"
    } else {
        "preempt_tight_loop"
    });
    let output = run_on_one_worker(Command::new(&binary), "release");
    let _ = std::fs::remove_dir_all(&dir);
    assert_peer_ran(&output, "release");
}
