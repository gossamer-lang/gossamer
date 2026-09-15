//! Codegen integration suite (Track A · A2).
//!
//! Walks every `.gos` source file under `tests/correct/`, looks for
//! a sibling `<name>.expected` containing the expected stdout, and
//! runs the program three ways:
//!
//! 1. `gos run <name>.gos` (bytecode VM).
//! 2. `gos build <name>.gos` + execute (Cranelift debug).
//! 3. `gos build --release <name>.gos` + execute (LLVM with
//!    Cranelift fallback).
//!
//! Each tier's stdout is asserted byte-equal to `<name>.expected`.
//! The test is data-driven: drop a new pair of files into
//! `tests/correct/` and the next `cargo test` run picks it up.
//!
//! Keep this suite small enough that `cargo test` stays under a
//! minute on a laptop. Hot-loop / IO-heavy benchmarks belong in
//! `benchmarks/`, not here.
//!
//! Default `cargo test` runs a stride-sampled subset (~13 of the
//! ~100 programs) so each tier compile + execution stays cheap.
//! Set `GOSSAMER_TESTS_FULL=1` (or run the workspace-root
//! `exhaustive_test.sh`) to walk every program.

#![allow(missing_docs)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())
        .expect("workspace root not found")
        .to_path_buf()
}

fn correct_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("correct")
}

fn gos_binary() -> PathBuf {
    // The release binary is what `gos` / `gos build` reach.
    // Built once by the workspace, shared across tests.
    // The test executable sits at `<target>/<profile>/deps/<name>`, so its
    // ancestors name the target directory cargo is building into, which is
    // where `ensure_gos_built` places the binary too.
    let target = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.parent()?.to_path_buf()))
        .unwrap_or_else(|| workspace_root().join("target"));
    let mut p = target.join("release").join("gos");
    if !std::env::consts::EXE_EXTENSION.is_empty() {
        p.set_extension(std::env::consts::EXE_EXTENSION);
    }
    p
}

fn ensure_gos_built() {
    let gos = gos_binary();
    if gos.exists() {
        return;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(&cargo)
        .args(["build", "--release", "--bin", "gos"])
        .current_dir(workspace_root())
        .status()
        .expect("spawn cargo build");
    assert!(status.success(), "failed to build gos");
}

fn read_expected(expected_path: &Path) -> String {
    std::fs::read_to_string(expected_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", expected_path.display()))
}

#[derive(Debug)]
#[allow(
    dead_code,
    reason = "fields are surfaced in failure messages via Debug"
)]
struct TierOutcome {
    stdout: String,
    stderr: String,
    exit: Option<i32>,
}

fn run_interp(src: &Path) -> TierOutcome {
    let out = Command::new(gos_binary())
        .arg("run")
        .arg(src)
        .output()
        .expect("spawn gos");
    TierOutcome {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        exit: out.status.code(),
    }
}

fn run_compiled(src: &Path, release: bool, scratch: &Path) -> TierOutcome {
    // `gos build` emits the binary at `<source-dir>/target/<profile>/<stem>`.
    // We give it a scratch directory so concurrent tests don't
    // clobber each other.
    let stem = src.file_stem().unwrap();
    let copied = scratch.join(format!("{}.gos", stem.to_string_lossy()));
    std::fs::copy(src, &copied).expect("copy source to scratch");
    let mut cmd = Command::new(gos_binary());
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.arg(&copied);
    let build = cmd.output().expect("spawn gos build");
    if !build.status.success() {
        return TierOutcome {
            stdout: String::new(),
            stderr: format!("build failed: {}", String::from_utf8_lossy(&build.stderr)),
            exit: build.status.code(),
        };
    }
    let profile = if release { "release" } else { "debug" };
    let mut bin = scratch.join("target").join(profile).join(stem);
    if !std::env::consts::EXE_EXTENSION.is_empty() {
        bin.set_extension(std::env::consts::EXE_EXTENSION);
    }
    if !bin.exists() {
        return TierOutcome {
            stdout: String::new(),
            stderr: format!(
                "build artifact missing at {}: {}",
                bin.display(),
                String::from_utf8_lossy(&build.stderr)
            ),
            exit: None,
        };
    }
    let run = Command::new(&bin).output().expect("spawn compiled binary");
    TierOutcome {
        stdout: String::from_utf8_lossy(&run.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&run.stderr).into_owned(),
        exit: run.status.code(),
    }
}

fn collect_programs() -> Vec<PathBuf> {
    let dir = correct_dir();
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|s| s.to_str()) == Some("gos") {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Picks every Nth program from `all` so the default test run covers
/// a representative slice of the codegen-correct corpus without
/// shelling out to `gos build --release` for every entry. The
/// `GOSSAMER_TESTS_FULL=1` env var (set by `exhaustive_test.sh`)
/// short-circuits the stride and runs the whole suite - the same
/// flag is also consulted by the parity tests in
/// `crates/gossamer-cli/tests/parity.rs`.
fn sample_programs(all: Vec<PathBuf>) -> Vec<PathBuf> {
    if std::env::var_os("GOSSAMER_TESTS_FULL").is_some() {
        return all;
    }
    // Stride 8 over the alphabetically-sorted list yields ~13 of
    // the 101 programs - enough variety to catch a regression
    // touching most lowering paths, fast enough that the test
    // routinely finishes inside ~30 seconds on a laptop.
    all.into_iter().step_by(8).collect()
}

#[test]
fn every_correct_program_matches_across_tiers() {
    ensure_gos_built();
    let programs = sample_programs(collect_programs());
    assert!(
        !programs.is_empty(),
        "no .gos programs found in {}",
        correct_dir().display()
    );

    let scratch = std::env::temp_dir().join(format!("gossamer-correct-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("create scratch dir");

    let mut failures: Vec<String> = Vec::new();
    let mut counted = 0_usize;

    for src in &programs {
        let stem = src.file_stem().unwrap().to_string_lossy().into_owned();
        let expected_path = src.with_extension("expected");
        if !expected_path.exists() {
            failures.push(format!(
                "{stem}: missing sibling .expected file at {}",
                expected_path.display()
            ));
            continue;
        }
        let expected = read_expected(&expected_path);

        let interp = run_interp(src);
        let debug = run_compiled(src, false, &scratch);
        let release = run_compiled(src, true, &scratch);

        for (tier, outcome) in [
            ("interp", &interp),
            ("debug", &debug),
            ("release", &release),
        ] {
            if outcome.stdout != expected {
                failures.push(format!(
                    "{stem} ({tier}):\n  expected: {:?}\n  got:      {:?}\n  stderr:   {}",
                    expected, outcome.stdout, outcome.stderr
                ));
            }
        }
        counted += 1;
    }

    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        failures.is_empty(),
        "codegen-correct mismatches ({} programs, {} mismatches):\n\n{}",
        counted,
        failures.len(),
        failures.join("\n\n")
    );
}
