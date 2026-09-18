//! Shared helpers for the native-build integration suites.
//!
//! Included by each harness via `mod common;`. Not every harness uses
//! every helper, so dead-code (and the `pub` items it makes
//! unreachable in a given includer) is allowed here rather than
//! per-call.
#![allow(dead_code, unreachable_pub)]

use std::process::ExitStatus;

/// Returns the deterministic executable path produced by `gos build
/// --out-dir`, including the host platform's executable suffix.
pub fn native_executable(out_dir: &std::path::Path, stem: &str) -> std::path::PathBuf {
    out_dir.join(native_executable_name(stem, std::env::consts::EXE_SUFFIX))
}

fn native_executable_name(stem: &str, suffix: &str) -> String {
    format!("{stem}{suffix}")
}

/// Outcome of running a built native binary, with the exit status
/// rendered into a human-readable form that names the crash cause.
pub struct RunExit {
    pub success: bool,
    /// e.g. `exit 0`, `killed by signal 11 (SIGSEGV)`,
    /// `exit code 0xC0000005 (STATUS_ACCESS_VIOLATION)`.
    pub text: String,
}

/// Renders an `ExitStatus` so a crash reads as its cause instead of an
/// opaque number. A native binary miscompiled for a target dies by
/// signal (unix) or with an NTSTATUS exit code (Windows); a bare
/// `exit: Some(-1073741819)` hides that this was an access violation.
pub fn describe_exit(status: ExitStatus) -> RunExit {
    RunExit {
        success: status.success(),
        text: render_exit(status),
    }
}

/// Outcome for a run that never produced an exit status (timed out and
/// was killed, or failed to spawn).
pub fn aborted(reason: &str) -> RunExit {
    RunExit {
        success: false,
        text: format!("no exit status: {reason}"),
    }
}

#[cfg(unix)]
fn render_exit(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        format!("exit {code}")
    } else if let Some(sig) = status.signal() {
        format!("killed by signal {sig} ({})", signal_name(sig))
    } else {
        format!("{status:?}")
    }
}

#[cfg(windows)]
fn render_exit(status: ExitStatus) -> String {
    match status.code() {
        Some(0) => "exit 0".to_string(),
        Some(code) => match ntstatus_name(code as u32) {
            Some(name) => format!("exit code {:#010x} ({name})", code as u32),
            None => format!("exit {code}"),
        },
        None => format!("{status:?}"),
    }
}

#[cfg(not(any(unix, windows)))]
fn render_exit(status: ExitStatus) -> String {
    format!("{status:?}")
}

/// Names the common fatal signals. `SIGBUS` is 7 on Linux but 10 on
/// Darwin, so it is resolved per-OS; the rest share numbers across
/// the unixes Gossamer targets.
fn signal_name(sig: i32) -> &'static str {
    if cfg!(target_os = "linux") && sig == 7 {
        return "SIGBUS";
    }
    if cfg!(target_os = "macos") && sig == 10 {
        return "SIGBUS";
    }
    match sig {
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => "unknown",
    }
}

/// Names the NTSTATUS exit codes a crashing Windows process reports
/// through its exit code. Pure and platform-independent so it can be
/// unit-tested on any host.
fn ntstatus_name(code: u32) -> Option<&'static str> {
    match code {
        0xC000_0005 => Some("STATUS_ACCESS_VIOLATION"),
        0xC000_001D => Some("STATUS_ILLEGAL_INSTRUCTION"),
        0xC000_0025 => Some("STATUS_NONCONTINUABLE_EXCEPTION"),
        0xC000_008C => Some("STATUS_ARRAY_BOUNDS_EXCEEDED"),
        0xC000_0094 => Some("STATUS_INTEGER_DIVIDE_BY_ZERO"),
        0xC000_00FD => Some("STATUS_STACK_OVERFLOW"),
        0xC000_0374 => Some("STATUS_HEAP_CORRUPTION"),
        0xC000_0409 => Some("STATUS_STACK_BUFFER_OVERRUN"),
        0x8000_0003 => Some("STATUS_BREAKPOINT"),
        _ => None,
    }
}

/// A raw-TCP server that answers every read with one fixed HTTP response.
///
/// It shares the runtime, the scheduler, the goroutine-per-connection shape,
/// and the socket ABI with `http::serve`, and does none of the parsing,
/// routing, dispatch, or response building. Per request it costs one read and
/// one write, so the CPU it spends is the floor an HTTP server on the same
/// machine is measured against. That ratio is what a gate can hold to: the
/// absolute figure is a property of the hardware, and a CI runner's syscalls
/// cost several times a workstation's.
pub const TCP_RESPONSE_FLOOR_SOURCE: &str = r#"
use std::{env, net}

fn serve(conn: net::TcpStream) {
    let reply = "HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}"
    let mut buf: Vec<u8> = Vec::with_capacity(4096)
    let mut open = true
    while open {
        match conn.read_into(&mut buf, 4096) {
            Ok(n) => {
                if n == 0 { open = false } else { let _ = conn.write_all(reply.as_bytes()) }
            }
            Err(_) => open = false
        }
    }
    let _ = conn.close()
}

fn main() {
    let listener = match net::TcpListener::bind(env::args()[0]) {
        Ok(l) => l
        Err(e) => {
            eprintln("bind failed: {e}")
            return
        }
    }
    println("listening")
    loop {
        match listener.accept() {
            Ok((conn, _peer)) => spawn(|| serve(conn))
            Err(_) => return
        }
    }
}
"#;

/// `utime + stime` of `pid` in clock ticks, read from `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
pub fn proc_cpu_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field can hold spaces and parentheses, so fields are counted
    // from the closing paren rather than from the start of the line.
    let rest = stat.rsplit_once(')')?.1;
    let cols: Vec<&str> = rest.split_whitespace().collect();
    // utime and stime are fields 14 and 15 of the line, which are 11 and 12
    // past the state field that opens `rest`.
    Some(cols.get(11)?.parse::<u64>().ok()? + cols.get(12)?.parse::<u64>().ok()?)
}

/// Microseconds of CPU per request, from a tick delta over a request count.
/// `/proc` reports CPU in clock ticks, 100 per second on every Linux target
/// this runs on.
#[cfg(target_os = "linux")]
pub fn cpu_micros_per_request(ticks: u64, requests: usize) -> f64 {
    ticks as f64 * 10_000.0 / requests as f64
}

#[cfg(test)]
mod tests {
    use super::{native_executable_name, ntstatus_name, signal_name};

    #[test]
    fn native_executable_name_uses_the_platform_suffix() {
        assert_eq!(native_executable_name("program", ""), "program");
        assert_eq!(native_executable_name("program", ".exe"), "program.exe");
    }

    #[test]
    fn ntstatus_names_access_violation() {
        assert_eq!(ntstatus_name(0xC000_0005), Some("STATUS_ACCESS_VIOLATION"));
    }

    #[test]
    fn ntstatus_names_stack_overflow() {
        assert_eq!(ntstatus_name(0xC000_00FD), Some("STATUS_STACK_OVERFLOW"));
    }

    #[test]
    fn ntstatus_unknown_code_is_none() {
        assert_eq!(ntstatus_name(0), None);
        assert_eq!(ntstatus_name(1), None);
    }

    #[test]
    fn signal_names_segv_and_abort() {
        assert_eq!(signal_name(11), "SIGSEGV");
        assert_eq!(signal_name(6), "SIGABRT");
    }
}

/// The tier a source-string run executes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// `gos run` with `GOS_JIT=0`: the bytecode VM alone.
    Bytecode,
    /// `gos run` with the JIT compiling at its first call.
    Vm,
    /// `gos build --release`, then the binary.
    Llvm,
}

/// Every tier, for tests that assert one answer across all of them.
pub const TIERS: [Tier; 3] = [Tier::Bytecode, Tier::Vm, Tier::Llvm];

fn gos_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

/// A fresh directory holding `src` as `main.gos`, answering the file.
fn source_file(src: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "gos-src-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    let file = dir.join("main.gos");
    std::fs::write(&file, src).expect("write the source");
    file
}

/// Writes `src` to a fresh file and runs `gos check` on it.
pub fn gos_check_str(src: &str) -> std::process::Output {
    let file = source_file(src);
    let out = std::process::Command::new(gos_bin())
        .arg("check")
        .arg(&file)
        .output()
        .expect("spawn gos check");
    if let Some(dir) = file.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
    out
}

/// Runs `src` on `tier`. `workers` pins both the compiled scheduler's
/// `GOSSAMER_MAX_PROCS` and the VM pool's `GOSSAMER_VM_GOROUTINE_WORKERS`
/// when given; `env` is applied last.
pub fn gos_run_on(
    tier: Tier,
    src: &str,
    workers: Option<usize>,
    env: &[(&str, &str)],
) -> std::process::Output {
    let file = source_file(src);
    let dir = file
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mut cmd = match tier {
        Tier::Bytecode | Tier::Vm => {
            let mut cmd = std::process::Command::new(gos_bin());
            cmd.arg("run").arg(&file);
            if tier == Tier::Bytecode {
                cmd.env("GOS_JIT", "0");
            } else {
                cmd.env_remove("GOS_JIT").env("GOSSAMER_JIT_THRESHOLD", "1");
            }
            cmd
        }
        Tier::Llvm => {
            let out_dir = dir.join("out");
            let built = std::process::Command::new(gos_bin())
                .arg("build")
                .arg("--release")
                .arg("--out-dir")
                .arg(&out_dir)
                .arg(&file)
                .output()
                .expect("spawn gos build");
            assert!(
                built.status.success(),
                "gos build --release failed:\n{}",
                String::from_utf8_lossy(&built.stderr)
            );
            std::process::Command::new(native_executable(&out_dir, "main"))
        }
    };
    if let Some(workers) = workers {
        let count = workers.to_string();
        cmd.env("GOSSAMER_MAX_PROCS", &count)
            .env("GOSSAMER_VM_GOROUTINE_WORKERS", &count);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("spawn the program");
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// `gos_run_on(Tier::Vm, src, None, &[])`.
pub fn gos_run_str(src: &str) -> std::process::Output {
    gos_run_on(Tier::Vm, src, None, &[])
}

/// A run's standard output, decoded lossily.
pub fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A run's standard error, decoded lossily.
pub fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
