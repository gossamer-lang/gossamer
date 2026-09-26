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
