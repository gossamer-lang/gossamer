//! A plain `as` whose value does not fit its target stops the program with
//! the same `cast overflow` panic on the bytecode VM, the JIT, and both
//! native profiles.

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

/// Runs `statement` on every tier with `v` an `i64` and `f` an `f64` read
/// at run time, and checks each stops with `cast overflow: {shown} does not
/// fit {target}`.
fn assert_overflows(name: &str, statement: &str, shown: &str, target: &str) {
    let source = format!(
        "fn main() {{\n    let v = \"300\".to_i64().unwrap() - 301 + 1\n    let f = \"1e20\".to_f64().unwrap()\n    let big = v + 299\n    let _ = f\n    let _ = big\n    {statement}\n}}\n"
    );
    let dir = std::env::temp_dir().join(format!("gos-cast-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let expected = format!("error[GX0005]: panic: cast overflow: {shown} does not fit {target}\n");
    for (tier, output) in run_every_tier(&dir, name, &source) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let report = stderr
            .lines()
            .skip_while(|line| !line.starts_with("error["))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(output.status.code(), Some(101), "{tier} stderr:\n{stderr}");
        assert!(
            format!("{report}\n").starts_with(&expected),
            "{tier} report differs:\n{stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_narrowing_cast_past_the_target_panics_on_every_tier() {
    assert_overflows("narrow", "println(big as u8)", "299", "u8");
}

#[test]
fn a_negative_value_cast_to_unsigned_panics_on_every_tier() {
    assert_overflows("unsigned", "println((v - 1) as u64)", "-1", "u64");
}

#[test]
fn a_float_past_the_target_panics_on_every_tier() {
    assert_overflows("float", "println(f as i64)", "1e20", "i64");
}

#[test]
fn a_nan_cast_to_an_integer_panics_on_every_tier() {
    assert_overflows("nan", "println(((f - f) / (f - f)) as i64)", "NaN", "i64");
}
