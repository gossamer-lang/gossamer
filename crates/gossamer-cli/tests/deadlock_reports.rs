//! A program whose goroutines can no longer wake one another stops with the
//! same report and exit code on the bytecode VM, the JIT, and both native
//! profiles, naming the operation `main` is stopped at.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

/// Every tier's run of `source`, labelled for the failure message.
fn run_every_tier(dir: &Path, name: &str, source: &str) -> Vec<(&'static str, Output)> {
    let file = format!("{name}.gos");
    std::fs::write(dir.join(&file), source).expect("write program");
    let cache = dir.join("cache");
    let run = |envs: &[(&str, &str)]| {
        let mut command = Command::new(gos_bin());
        command
            .current_dir(dir)
            .env("GOSSAMER_CACHE_DIR", &cache)
            .args(["run", &file]);
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().expect("spawn gos run")
    };
    let mut outputs = vec![
        ("vm", run(&[("GOS_JIT", "0")])),
        ("jit", run(&[("GOSSAMER_JIT_THRESHOLD", "1")])),
    ];
    for (label, profile) in [("debug", None), ("release", Some("--release"))] {
        let out_dir = format!("out-{label}");
        let mut command = Command::new(gos_bin());
        command
            .current_dir(dir)
            .env("GOSSAMER_CACHE_DIR", &cache)
            .arg("build");
        if let Some(flag) = profile {
            command.arg(flag);
        }
        let build = command
            .args([file.as_str(), "--out-dir", out_dir.as_str()])
            .output()
            .expect("spawn gos build");
        assert!(
            build.status.success(),
            "{label} build of {name}:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let binary = dir.join(&out_dir).join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        });
        outputs.push((
            label,
            Command::new(&binary).output().expect("run the artifact"),
        ));
    }
    outputs
}

fn assert_reports(name: &str, source: &str, expected: &str) {
    let dir = std::env::temp_dir().join(format!("gos-deadlock-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    for (tier, output) in run_every_tier(&dir, name, source) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(101), "{tier} stderr:\n{stderr}");
        assert!(
            stderr.starts_with(expected),
            "{tier} report differs:\n{stderr}"
        );
        assert!(output.stdout.is_empty(), "{tier} wrote stdout");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn main_receiving_alone_reports_the_receive() {
    assert_reports(
        "main_receive",
        "use std::sync::channel\n\nfn main() {\n    let tx, rx = channel()\n    println(\"{:?}\", rx.recv())\n    tx.send(1)\n}\n",
        "error[GX0005]: panic: all goroutines are asleep - deadlock! (receive can never complete)\n",
    );
}

#[test]
fn a_cohort_whose_child_cannot_send_reports_the_join() {
    assert_reports(
        "cohort_join",
        "use std::sync::channel\n\nfn main() {\n    let tx, rx = channel()\n    let _ = cohort {\n        spawn(|| {\n            tx.send(1)\n        })\n    }\n    println(\"{:?}\", rx.recv())\n}\n",
        "error[GX0005]: panic: all goroutines are asleep - deadlock! (cohort join can never complete)\n",
    );
}

/// The reader finishes on its own while the writer and `main` wait on each
/// other. Whichever of those happens last, the program is left with nothing
/// able to run, so the order the goroutines settle in is varied by running
/// the program several times.
#[test]
fn a_goroutine_finishing_after_the_others_wait_leaves_a_report() {
    let source = r#"use std::{errors, sync}
use std::sync::channel

fn run() -> Result<(), errors::Error> {
    let lock = sync::RwLock::new(1)
    let tx, rx = channel()
    cohort {
        let writer = spawn(|| sync::RwLock::with_write(lock, |v| v + rx.recv().unwrap()))
        let reader = spawn(|| lock.read())
        println(f"{writer.join().unwrap() + reader.join().unwrap()}")
        tx.send(1)
    }?
    Ok(())
}

fn main() {
    run().unwrap()
}
"#;
    let expected = "error[GX0005]: panic: all goroutines are asleep - deadlock! (receive can never complete)\n";
    assert_reports("finished_last", source, expected);
    let dir = std::env::temp_dir().join(format!("gos-deadlock-orders-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("orders.gos"), source).expect("write program");
    for _ in 0..20 {
        let output = Command::new(gos_bin())
            .current_dir(&dir)
            .env("GOSSAMER_CACHE_DIR", dir.join("cache"))
            .env("GOS_JIT", "0")
            .args(["run", "orders.gos"])
            .output()
            .expect("spawn gos run");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(101), "stderr:\n{stderr}");
        assert!(stderr.starts_with(expected), "report differs:\n{stderr}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A goroutine whose unbuffered send was taken leaves its channel with no
/// handoff pending, so a later wait nothing can satisfy is still reported.
#[test]
fn a_completed_unbuffered_send_leaves_no_pending_handoff() {
    assert_reports(
        "completed_send",
        "use std::sync::channel\n\nfn main() {\n    let first_tx, first_rx = channel::<i64>()\n    spawn(|| first_tx.send(1))\n    let _ = first_rx.recv()\n    let tx, rx = channel::<i64>()\n    tx.send(3)\n    println(\"{}\", rx.recv().unwrap())\n}\n",
        "error[GX0005]: panic: all goroutines are asleep - deadlock! (send can never complete)\n",
    );
}
