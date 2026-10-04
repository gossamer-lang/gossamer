//! `signal::stop` on every tier: a delivered signal wakes every notifier
//! subscribed to it, and once the last one stops, the signal takes its
//! default disposition again and ends the process, as Go's `signal.Stop`
//! leaves it.

#![cfg(unix)]
#![allow(missing_docs)]

use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::{Command, Output};

const PROGRAM: &str = r#"
use std::os::signal
use std::process
use std::time

fn main() {
    let n = signal::on(signal::SIGUSR1)
    let m = signal::on(signal::SIGUSR1)
    process::signal(process::id(), signal::SIGUSR1)
    println(f"delivered {n.wait()} {m.wait()}")
    m.stop()
    println(f"one left {signal::try_wait(m)}")
    n.stop()
    println("stopped")
    process::signal(process::id(), signal::SIGUSR1)
    time::sleep(time::Duration::from_secs(30))
    println("still running")
}
"#;

fn gos() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-signal-stop-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let src = dir.join("signal_stop.gos");
    std::fs::write(&src, PROGRAM).expect("write the program");
    src
}

fn assert_ended_by_default_disposition(tier: &str, out: &Output) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout,
        "delivered true true\none left false\nstopped\n",
        "{tier}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGUSR1),
        "{tier}: {:?}",
        out.status
    );
}

#[test]
fn stopping_the_last_notifier_restores_the_default_disposition_on_the_vm() {
    let src = scratch("vm");
    let out = Command::new(gos())
        .args(["run", "--no-jit"])
        .arg(&src)
        .output()
        .expect("run gos");
    assert_ended_by_default_disposition("vm", &out);
}

#[test]
fn stopping_the_last_notifier_restores_the_default_disposition_on_the_jit() {
    let src = scratch("jit");
    let out = Command::new(gos())
        .arg("run")
        .arg(&src)
        .env("GOSSAMER_JIT_THRESHOLD", "1")
        .output()
        .expect("run gos");
    assert_ended_by_default_disposition("jit", &out);
}

#[test]
fn stopping_the_last_notifier_restores_the_default_disposition_as_a_native_binary() {
    let src = scratch("native");
    let dir = src.parent().expect("scratch dir").to_path_buf();
    for release in [false, true] {
        let out_dir = dir.join(if release { "release" } else { "debug" });
        let mut build = Command::new(gos());
        build.arg("build").arg("--out-dir").arg(&out_dir).arg(&src);
        if release {
            build.arg("--release");
        }
        let outcome = build.output().expect("run gos build");
        assert!(
            outcome.status.success(),
            "{}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        let out = Command::new(out_dir.join("signal_stop"))
            .output()
            .expect("run the binary");
        assert_ended_by_default_disposition("native", &out);
    }
}
