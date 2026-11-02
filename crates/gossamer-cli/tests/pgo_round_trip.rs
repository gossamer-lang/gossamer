//! Profile-guided optimisation end to end: `--pgo-collect` builds a binary
//! that writes its raw profile to the requested path when it exits, and
//! `--pgo-profile` builds from the merged profile with the same output.
//!
//! The test needs the LLVM `gos build` drives together with its compiler-rt
//! profile runtime and `llvm-profdata`; it says so and returns when the host
//! lacks them.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

const PROGRAM: &str = "fn main() {\n    let mut total = 0\n    for i in 0..1000 {\n        if i % 3 == 0 { total += i }\n    }\n    println(f\"total {total}\")\n}\n";

/// The first eight bytes of an LLVM raw profile, little-endian.
const RAW_PROFILE_MAGIC: [u8; 8] = [0x81, b'r', b'f', b'o', b'r', b'p', b'l', 0xff];

fn gos() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gos"))
}

/// The LLVM major `gos build` resolves `opt` at, from `gos env`.
fn llvm_major() -> Option<u32> {
    let out = gos().arg("env").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.starts_with("llvm_opt"))?;
    let digits = line.split("(LLVM ").nth(1)?;
    digits.trim_end_matches(')').trim().parse().ok()
}

/// `llvm-profdata` of that major: beside a pinned `GOS_LLVM_OPT` (a keg-only
/// Homebrew LLVM), else on `PATH`.
fn llvm_profdata(major: u32) -> Option<String> {
    let pinned = std::env::var_os("GOS_LLVM_OPT").and_then(|opt| {
        let sibling = Path::new(&opt).with_file_name("llvm-profdata");
        sibling.is_file().then(|| sibling.display().to_string())
    });
    pinned
        .into_iter()
        .chain([
            format!("llvm-profdata-{major}"),
            "llvm-profdata".to_string(),
        ])
        .find(|tool| {
            Command::new(tool)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        })
}

fn build(dir: &Path, flag: &str, value: &Path) -> std::process::Output {
    gos()
        .current_dir(dir)
        .args(["build", "--release", flag])
        .arg(value)
        .arg("main.gos")
        .output()
        .expect("spawn gos build")
}

/// Runs the built binary, answering its stdout and stderr.
fn run_binary(dir: &Path) -> (String, String) {
    let out = Command::new(common::native_executable(
        &dir.join("target/release"),
        "main",
    ))
    .current_dir(dir)
    .output()
    .expect("run the built binary");
    assert!(out.status.success(), "binary failed: {out:?}");
    (
        String::from_utf8(out.stdout).expect("utf-8 stdout"),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn collected_profile_lands_at_the_requested_path_and_guides_a_rebuild() {
    let Some(major) = llvm_major() else {
        eprintln!("skipping: gos resolves no LLVM opt on this host");
        return;
    };
    let Some(profdata) = llvm_profdata(major) else {
        eprintln!("skipping: no llvm-profdata for LLVM {major}");
        return;
    };
    let source = common::source_file(PROGRAM);
    let dir = source
        .parent()
        .expect("the source sits in its own directory");
    let raw: PathBuf = dir.join("collected.profraw");

    let collect = build(dir, "--pgo-collect", &raw);
    let stderr = String::from_utf8_lossy(&collect.stderr);
    if stderr.contains("pgo: warning:") && stderr.contains("not found") {
        eprintln!("skipping: LLVM {major} has no compiler-rt profile runtime here");
        return;
    }
    assert!(collect.status.success(), "collect build failed: {stderr}");
    let (stdout, run_stderr) = run_binary(dir);
    assert_eq!(stdout, "total 166833\n");

    let bytes = std::fs::read(&raw).unwrap_or_else(|e| {
        panic!(
            "the instrumented binary writes the requested profile: {e}; its stderr: {run_stderr}"
        )
    });
    assert!(
        bytes.starts_with(&RAW_PROFILE_MAGIC),
        "not an LLVM raw profile: {:02x?}; the instrumented binary's stderr: {run_stderr}",
        &bytes[..bytes.len().min(8)]
    );
    assert!(
        !dir.join("default.profraw").exists(),
        "the profile went to the runtime's default name instead of the requested path"
    );

    let merged = dir.join("merged.profdata");
    let merge = Command::new(&profdata)
        .args(["merge", "-output"])
        .arg(&merged)
        .arg(&raw)
        .output()
        .expect("spawn llvm-profdata");
    assert!(
        merge.status.success(),
        "llvm-profdata merge failed: {merge:?}"
    );

    let guided = build(dir, "--pgo-profile", &merged);
    assert!(
        guided.status.success(),
        "guided build failed: {}",
        String::from_utf8_lossy(&guided.stderr)
    );
    assert_eq!(run_binary(dir).0, "total 166833\n");
    let _ = std::fs::remove_dir_all(dir);
}
