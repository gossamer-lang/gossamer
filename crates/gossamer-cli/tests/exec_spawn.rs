//! `exec::spawn` non-blocking process launch + `exec::kill`
//! teardown across the three tiers.
//!
//! Regression coverage for the 2026-05-07 daemon-launch report:
//! tools that need to background-launch a long-running child
//! (LLM server, SSE relay, etc.) used to shell out via
//! `exec::run "sh -c '... &'"` because the stdlib's only
//! process primitive blocked on `wait`. The new
//! `exec::spawn(prog, args) -> Result<i64, errors::Error>`
//! returns the child PID immediately; `exec::kill(pid)` SIGTERMs
//! the daemon. Each test asserts both spawn-then-kill and
//! spawn-then-await-output across `gos`, `gos build`, and
//! `gos build --release`.

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
        "gos-spawn-{pid}-{n}-{tag}",
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

// --- Unix tests -----------------------------------------------------------

#[test]
#[cfg(unix)]
fn exec_spawn_returns_positive_pid_for_long_running_sleep() {
    // Smoke test: spawn /bin/sleep 30, assert PID is > 0,
    // SIGTERM it via exec::kill, assert kill returned true.
    // Across all three tiers - the runtime helpers are
    // backend-agnostic, so cranelift / LLVM dispatch must
    // resolve to the same gos_rt_exec_spawn / gos_rt_exec_kill
    // symbols and produce the same output.
    let src = r#"
use std::os::exec
fn main() {
    let args: Vec<String> = ["30"].to_vec()
    match exec::spawn("/bin/sleep", args) {
        Ok(pid) => {
            if pid > 0 { println("spawned") } else { println("zero pid") }
            let killed = exec::kill(pid)
            if killed { println("killed") } else { println("kill failed") }
        }
        Err(e) => println("error: {}", e.message()),
    }
}
"#;
    assert_three_tier_stdout("spawn_sleep_kill", src, "spawned\nkilled");
}

#[test]
#[cfg(unix)]
fn exec_spawn_returns_error_for_nonexistent_program() {
    // Spawning a path that doesn't exist must surface an Err
    // payload, not a zero PID or a silent zero-exit. Common
    // mistake the runtime helper has to guard against.
    let src = r#"
use std::os::exec
fn main() {
    let args: Vec<String> = Vec::from([])
    match exec::spawn("/this/does/not/exist/please", args) {
        Ok(_) => println("unexpected ok"),
        Err(_) => println("err"),
    }
}
"#;
    assert_three_tier_stdout("spawn_missing_program", src, "err");
}

#[test]
#[cfg(unix)]
fn exec_spawn_then_kill_round_trips_through_a_named_var() {
    // The PID is bound to a `let` and used twice - once to print,
    // once to pass to `exec::kill`. Catches any single-use
    // miscompile in the Result<i64, _> unwrap path.
    let src = r#"
use std::os::exec
fn main() {
    let args: Vec<String> = ["10"].to_vec()
    let r = exec::spawn("/bin/sleep", args)
    match r {
        Ok(pid) => {
            println("got pid")
            let _ = exec::kill(pid)
            println("done")
        }
        Err(e) => println("error: {}", e.message()),
    }
}
"#;
    assert_three_tier_stdout("spawn_pid_round_trip", src, "got pid\ndone");
}

// --- Windows tests --------------------------------------------------------

#[test]
#[cfg(windows)]
fn exec_spawn_returns_positive_pid_for_long_running_ping() {
    // Windows equivalent: `ping 127.0.0.1 -n 31` takes ~30 s.
    let src = r#"
use std::os::exec
fn main() {
    let args: Vec<String> = ["127.0.0.1", "-n", "31"].to_vec()
    match exec::spawn("ping", args) {
        Ok(pid) => {
            if pid > 0 { println("spawned") } else { println("zero pid") }
            let killed = exec::kill(pid)
            if killed { println("killed") } else { println("kill failed") }
        }
        Err(e) => println("error: {}", e.message()),
    }
}
"#;
    assert_three_tier_stdout("spawn_ping_kill", src, "spawned\nkilled");
}

#[test]
#[cfg(windows)]
fn exec_spawn_returns_error_for_nonexistent_program() {
    let src = r#"
use std::os::exec
fn main() {
    let args: Vec<String> = Vec::from([])
    match exec::spawn("C:\\this\\does\\not\\exist\\please.exe", args) {
        Ok(_) => println("unexpected ok"),
        Err(_) => println("err"),
    }
}
"#;
    assert_three_tier_stdout("spawn_missing_program_win", src, "err");
}

#[test]
#[cfg(windows)]
fn exec_spawn_then_kill_round_trips_through_a_named_var() {
    let src = r#"
use std::os::exec
fn main() {
    let args: Vec<String> = ["127.0.0.1", "-n", "11"].to_vec()
    let r = exec::spawn("ping", args)
    match r {
        Ok(pid) => {
            println("got pid")
            let _ = exec::kill(pid)
            println("done")
        }
        Err(e) => println("error: {}", e.message()),
    }
}
"#;
    assert_three_tier_stdout("spawn_pid_round_trip_win", src, "got pid\ndone");
}
