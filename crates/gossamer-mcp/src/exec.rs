//! Subprocess runner for the exec-family tools. Spawning the `gos`
//! binary keeps user-program panics and `process::exit` out of the
//! server process.
//!
//! Every tool here drives the toolchain over a project the caller
//! pointed at, and runs as the caller does.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Per-stream capture cap; a chatty program must not flood the client.
const STREAM_CAP: usize = 256 * 1024;

/// Result of one subprocess run.
pub struct ExecOutcome {
    /// Process exit code; `None` when killed by a signal.
    pub exit_code: Option<i64>,
    /// Captured stdout, truncated at 256 KiB.
    pub stdout: String,
    /// Captured stderr, truncated at 256 KiB.
    pub stderr: String,
    /// True when the timeout expired and the process was killed.
    pub timed_out: bool,
}

/// Runs `exe args...` with stdin closed, bounded by `timeout`.
pub(crate) fn run_gos(
    exe: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<ExecOutcome, String> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_process_group(&mut command);
    let child = command
        .spawn()
        .map_err(|e| format!("spawning {}: {e}", exe.display()))?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let (output, timed_out) = match rx.recv_timeout(timeout) {
        Ok(result) => (result, false),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_pid(pid);
            // The child cannot outlive the kill and its pid cannot be
            // reused before the waiter reaps it, so this recv returns. The
            // pipes close once every member of the group is gone.
            (rx.recv().map_err(|e| format!("reaping child: {e}"))?, true)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err("waiter thread exited without a result".to_string());
        }
    };
    let output = output.map_err(|e| format!("waiting for {}: {e}", exe.display()))?;
    Ok(ExecOutcome {
        exit_code: output.status.code().map(i64::from),
        stdout: truncate(&output.stdout),
        stderr: truncate(&output.stderr),
        timed_out,
    })
}

/// Starts the child as the leader of a process group of its own, so a
/// kill reaches what it spawned (`gos build` runs llc and cc, a program
/// its own children) as well as the child.
#[cfg(unix)]
fn own_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// `taskkill /T` walks the child's tree instead.
#[cfg(windows)]
fn own_process_group(_command: &mut Command) {}

/// Force-kills the process group `pid` leads via the platform's standard
/// process-control utility, keeping this crate dependency free and unsafe
/// free. The utility's streams are detached: the server's inherited
/// stdout IS the JSON-RPC transport, and these tools report their outcome
/// to stdout/stderr.
#[cfg(unix)]
fn kill_pid(pid: u32) {
    let _ = Command::new("kill")
        .args(["-9", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `/T` fells the whole child tree (`gos build` spawns llc / cc).
#[cfg(windows)]
fn kill_pid(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn truncate(bytes: &[u8]) -> String {
    let total = bytes.len();
    let mut text = String::from_utf8_lossy(&bytes[..total.min(STREAM_CAP)]).into_owned();
    if total > STREAM_CAP {
        text.push_str(&format!("\n[truncated {} bytes]", total - STREAM_CAP));
    }
    text
}
