//! `env::set_var` / `env::unset_var` parity across the three tiers.
//!
//! Regression coverage for the 2026-05-07 daemon-launch report:
//! the compiled tier silently no-op'd `env::set_var` because MIR
//! had no dispatch arm for the call, so a daemon spawned via
//! `exec::spawn` after `set_env("LD_LIBRARY_PATH", ...)` couldn't
//! find its libraries (the env var the parent thought it set was
//! never actually written to the process env table). The VM
//! routed through `safe_env::set_env` correctly, so behaviour
//! diverged only under `gos build` / `gos build --release`.

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
        "gos-osenv-{pid}-{n}-{tag}",
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
fn os_set_env_round_trips_through_os_env_in_all_tiers() {
    // Set a unique env var, read it back. The compiled tier
    // must hit the runtime's `safe_env::set_env` path so
    // `env::var` (which also routes through libc / safe_env)
    // returns the value just written. No Unix-specific system
    // calls involved - runs on all platforms.
    let src = r#"
use std::env
fn main() {
    env::set_var("GOS_ENV_PROBE_2026", "yes-set-2026")
    let v = env::var("GOS_ENV_PROBE_2026").unwrap_or("MISSING")
    println("got={}", v)
}
"#;
    assert_three_tier_stdout("set_env_round_trip", src, "got=yes-set-2026");
}

#[test]
#[cfg(unix)]
fn os_set_env_propagates_to_a_spawned_child_in_all_tiers() {
    // Set LD-style env var, spawn `/usr/bin/env`, capture
    // stdout, look for the var. Mirrors the daemon-launch
    // pattern (e.g. setting a runtime path before spawning a
    // shared-library-dependent child).
    let src = r#"
use std::env
use std::os::exec
fn main() {
    env::set_var("GOS_PROBE_CHILD_2026", "propagated")
    let args: Vec<String> = Vec::from([])
    match exec::run("/usr/bin/env", args) {
        Ok(o) => {
            for line in o.stdout.lines() {
                if line.starts_with("GOS_PROBE_CHILD_2026=") {
                    println("{}", line)
                }
            }
        }
        Err(e) => println("err: {}", e.message()),
    }
}
"#;
    assert_three_tier_stdout(
        "set_env_propagates_to_child",
        src,
        "GOS_PROBE_CHILD_2026=propagated",
    );
}

#[test]
#[cfg(windows)]
fn os_set_env_propagates_to_a_spawned_child_in_all_tiers_windows() {
    // Windows equivalent: `cmd /c set GOS_PROBE_CHILD_2026` prints
    // `GOS_PROBE_CHILD_2026=propagated` if the var is in the environment.
    let src = r#"
use std::env
use std::os::exec
fn main() {
    env::set_var("GOS_PROBE_CHILD_2026", "propagated")
    let args: Vec<String> = ["/c", "set", "GOS_PROBE_CHILD_2026"].to_vec()
    match exec::run("cmd", args) {
        Ok(o) => {
            for line in o.stdout.lines() {
                if line.starts_with("GOS_PROBE_CHILD_2026=") {
                    println("{}", line)
                }
            }
        }
        Err(e) => println("err: {}", e.message()),
    }
}
"#;
    assert_three_tier_stdout(
        "set_env_propagates_to_child_win",
        src,
        "GOS_PROBE_CHILD_2026=propagated",
    );
}
