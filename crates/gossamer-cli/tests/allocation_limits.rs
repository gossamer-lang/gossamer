//! A buffer the system can never back - asked for by `#[x; n]`,
//! `Vec::with_capacity(n)`, or `strings::repeat` - stops the program with
//! the same out-of-memory panic on the bytecode VM, the JIT, and both native
//! profiles, naming the statement that asked for it.
//!
//! The limit is read on Linux and Windows; elsewhere the system may map such
//! a buffer lazily, so the report is only checked where it is decided.
#![cfg(any(target_os = "linux", windows))]

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

fn assert_out_of_memory(name: &str, statement: &str, bytes: &str) {
    let source = format!(
        "use std::strings\n\nfn main() {{\n    let n = \"10000000000000\".to_i64().unwrap()\n    {statement}\n    println(v.len())\n}}\n"
    );
    let dir = std::env::temp_dir().join(format!("gos-oom-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let expected = format!("error[GX0005]: panic: out of memory: cannot allocate {bytes} bytes\n");
    for (tier, output) in run_every_tier(&dir, name, &source) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(101), "{tier} stderr:\n{stderr}");
        assert!(
            stderr.starts_with(&expected),
            "{tier} report differs:\n{stderr}"
        );
        // The release profile carries no line table, so only the others
        // can name the statement.
        if tier != "release" {
            assert!(
                stderr.contains(&format!("{name}.gos:5:")),
                "{tier} does not name the statement:\n{stderr}"
            );
        }
        assert!(output.stdout.is_empty(), "{tier} wrote stdout");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_byte_array_past_memory_panics_on_every_tier() {
    assert_out_of_memory("bytes", "let v = #[0u8; n]", "10000000000000");
}

#[test]
fn an_integer_array_past_memory_panics_on_every_tier() {
    assert_out_of_memory("ints", "let v = #[7; n]", "80000000000000");
}

#[test]
fn a_capacity_past_memory_panics_on_every_tier() {
    assert_out_of_memory(
        "capacity",
        "let v: Vec<i64> = Vec::with_capacity(n)",
        "80000000000000",
    );
}

#[test]
fn a_repeated_string_past_memory_panics_on_every_tier() {
    assert_out_of_memory(
        "repeat",
        "let v = strings::repeat(\"ab\", n)",
        "20000000000000",
    );
}
