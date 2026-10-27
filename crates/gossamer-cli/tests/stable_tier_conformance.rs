//! Independent Stable-tier conformance contract.
//!
//! The exhaustive `tier_parity` battery exercises examples and feature probes.
//! This test instead consumes the small, reviewable fixture manifest under
//! `conformance/stable/` and is the release contract for VM-only dispatch,
//! forced Cranelift JIT dispatch, and LLVM AOT output. Keeping the manifest
//! outside the Rust test makes it reusable by release and target CI scripts.

#![allow(missing_docs)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

fn gos_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn stable_root() -> PathBuf {
    workspace_root().join("conformance/stable")
}

struct FixtureRow {
    source: PathBuf,
    expected: String,
}

fn fixture_rows() -> Vec<FixtureRow> {
    let root = stable_root();
    let manifest = fs::read_to_string(root.join("fixtures.tsv")).expect("read fixture manifest");
    manifest
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let fields = line.split('\t').collect::<Vec<_>>();
            assert!(
                matches!(fields.len(), 2 | 3),
                "invalid fixture row {line:?}; expected source<TAB>stdout"
            );
            let source = fields[0];
            let expected = fields[1];
            assert!(
                !source.contains('/')
                    && !source.contains('\\')
                    && !expected.contains('/')
                    && !expected.contains('\\'),
                "fixture paths must remain inside conformance/stable: {line:?}"
            );
            let source_path = root.join(source);
            let expected_path = root.join(expected);
            assert!(
                source_path.is_file(),
                "missing fixture {}",
                source_path.display()
            );
            assert!(
                expected_path.is_file(),
                "missing expected output {}",
                expected_path.display()
            );
            FixtureRow {
                source: source_path,
                expected: fs::read_to_string(expected_path).expect("read expected output"),
            }
        })
        .collect()
}

fn fresh_dir(label: &str) -> PathBuf {
    let sequence = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "gos-stable-tier-{label}-{}-{sequence}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch directory");
    dir
}

fn assert_success(mode: &str, output: Output, expected: &str) {
    assert!(
        output.status.success(),
        "{mode} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        expected,
        "{mode} stdout diverged\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

fn native_binary(output_dir: &Path, stem: &str) -> PathBuf {
    let mut candidates = fs::read_dir(output_dir)
        .expect("read AOT output directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.file_stem()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == stem)
        })
        .collect::<Vec<_>>();
    candidates.sort();
    assert_eq!(
        candidates.len(),
        1,
        "expected one AOT executable named {stem} in {}, found {candidates:?}",
        output_dir.display(),
    );
    candidates.pop().expect("checked candidate count")
}

#[test]
fn stable_fixture_manifest_runs_on_every_execution_tier() {
    let fixtures = fixture_rows();
    assert!(
        fixtures.len() >= 11,
        "keep at least eleven focused Stable conformance fixtures"
    );

    for FixtureRow { source, expected } in fixtures {
        let name = source
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("fixture stem");

        let vm = Command::new(gos_binary())
            .arg("run")
            .arg("--no-jit")
            .arg(&source)
            .output()
            .expect("run pure bytecode VM");
        assert_success(&format!("VM fixture {name}"), vm, &expected);

        let jit = Command::new(gos_binary())
            .arg("run")
            .arg(&source)
            .env("GOSSAMER_JIT_THRESHOLD", "1")
            .env("GOSSAMER_JIT_MIN_WORK", "1")
            .env_remove("GOS_JIT")
            .output()
            .expect("run JIT-enabled VM");
        assert_success(&format!("JIT fixture {name}"), jit, &expected);

        let output_dir = fresh_dir(name);
        let build = Command::new(gos_binary())
            .args(["build", "--release", "--out-dir"])
            .arg(&output_dir)
            .arg(&source)
            .output()
            .expect("build LLVM AOT fixture");
        assert!(
            build.status.success(),
            "AOT build fixture {name} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
            build.status.code(),
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );

        let binary = native_binary(&output_dir, name);
        let aot = Command::new(&binary)
            .output()
            .unwrap_or_else(|error| panic!("run AOT fixture {}: {error}", binary.display()));
        assert_success(&format!("AOT fixture {name}"), aot, &expected);
        let _ = fs::remove_dir_all(output_dir);
    }
}

#[test]
fn target_matrix_is_registered_without_overclaiming() {
    let root = workspace_root();
    let matrix =
        fs::read_to_string(root.join("conformance/target_matrix.tsv")).expect("read target matrix");
    let rows = matrix
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let fields = line.split('\t').collect::<Vec<_>>();
            assert_eq!(fields.len(), 4, "invalid target matrix row {line:?}");
            (fields[0], fields[1], fields[2], fields[3])
        })
        .collect::<Vec<_>>();
    assert!(!rows.is_empty(), "target matrix must not be empty");

    let registered = gossamer_driver::all_targets()
        .map(|target| target.triple.as_str().to_string())
        .collect::<BTreeSet<_>>();
    let mut listed = BTreeSet::new();
    for (tier, triple, evidence, owner) in &rows {
        assert!(
            matches!(*tier, "tier1" | "tier2" | "artifact" | "registered"),
            "unknown support class {tier:?} for {triple}"
        );
        assert!(
            listed.insert(*triple),
            "duplicate target matrix row for {triple}"
        );
        assert!(
            registered.contains(*triple),
            "matrix target is not registered: {triple}"
        );
        assert!(
            !evidence.is_empty() && !owner.is_empty(),
            "missing evidence for {triple}"
        );
    }

    let supported = rows
        .iter()
        .filter(|(tier, _, _, _)| *tier == "tier1" || *tier == "tier2")
        .map(|(_, triple, _, _)| *triple)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        supported,
        BTreeSet::from([
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
        ]),
        "changing supported targets requires an explicit matrix-contract review"
    );

    let spec = fs::read_to_string(root.join("SPEC.md")).expect("read spec");
    assert!(
        spec.contains("conformance/target_matrix.tsv"),
        "SPEC.md must name the executable target matrix"
    );
}
