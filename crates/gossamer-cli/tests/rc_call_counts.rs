//! The reference-count traffic in the uniqueness benchmarks' functions is
//! pinned, so a change that reintroduces it fails here rather than in a
//! benchmark someone has to notice.
//!
//! The numbers are counts of call sites in the optimised MIR, not calls made
//! at run time. Changing one is a deliberate act: the commit that does it
//! says which handoff it gained or lost and why.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// Retain, release, and deep-copy call sites in the named functions.
#[derive(Debug, Default, PartialEq, Eq)]
struct RcCounts {
    retain: usize,
    release: usize,
    clone: usize,
}

const RETAINS: &[&str] = &[
    "gos_rt_rc_retain",
    "gos_rt_vec_retain",
    "gos_rt_str_retain_typed",
    "gos_rt_str_retain",
];
const RELEASES: &[&str] = &[
    "gos_rt_rc_release",
    "gos_rt_vec_free",
    "gos_rt_str_free_typed",
    "gos_rt_str_free",
];
const CLONES: &[&str] = &["gos_rt_vec_clone", "gos_rt_map_clone", "gos_rt_set_clone"];

/// Builds `benchmark` with the release pipeline and counts the RC call sites
/// in each of `functions` in the MIR the backend receives.
fn rc_call_counts(benchmark: &str, functions: &[&str]) -> BTreeMap<String, RcCounts> {
    let root = workspace_root();
    let out_dir = std::env::temp_dir().join(format!(
        "gos-rc-counts-{}-{}",
        std::process::id(),
        benchmark.replace('/', "_")
    ));
    let _ = std::fs::remove_dir_all(&out_dir);
    let output = Command::new(env!("CARGO_BIN_EXE_gos"))
        .args(["build", "--release", "--out-dir"])
        .arg(&out_dir)
        .arg(root.join(benchmark))
        .env("GOS_LLVM_DUMP_MIR", "1")
        .output()
        .expect("run gos build");
    let _ = std::fs::remove_dir_all(&out_dir);
    assert!(
        output.status.success(),
        "gos build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dump = String::from_utf8_lossy(&output.stderr);
    let mut counts: BTreeMap<String, RcCounts> = BTreeMap::new();
    let mut current: Option<&str> = None;
    for line in dump.lines() {
        if let Some(name) = line
            .strip_prefix("=== MIR ")
            .and_then(|rest| rest.strip_suffix(" ==="))
        {
            current = functions.iter().copied().find(|f| *f == name);
            continue;
        }
        let Some(function) = current else {
            continue;
        };
        let entry = counts.entry(function.to_string()).or_default();
        let calls = |names: &[&str]| {
            names
                .iter()
                .filter(|name| line.contains(&format!("\"{name}\"")))
                .count()
        };
        entry.retain += calls(RETAINS);
        entry.release += calls(RELEASES);
        entry.clone += calls(CLONES);
    }
    for function in functions {
        assert!(
            counts.contains_key(*function),
            "{benchmark}: no MIR for {function} in the dump"
        );
    }
    counts
}

fn expect(benchmark: &str, pinned: &[(&str, RcCounts)]) {
    let functions: Vec<&str> = pinned.iter().map(|(name, _)| *name).collect();
    let counts = rc_call_counts(benchmark, &functions);
    for (function, want) in pinned {
        assert_eq!(
            counts.get(*function),
            Some(want),
            "{benchmark}: RC call sites in {function} drifted"
        );
    }
}

#[test]
fn vector_pipeline_rc_calls_are_pinned() {
    expect(
        "benchmarks/uniqueness/vector_pipeline.gos",
        &[
            (
                "build",
                RcCounts {
                    retain: 0,
                    release: 0,
                    clone: 0,
                },
            ),
            (
                "scale",
                RcCounts {
                    retain: 0,
                    release: 0,
                    clone: 0,
                },
            ),
            (
                "keep_even",
                RcCounts {
                    retain: 0,
                    release: 0,
                    clone: 0,
                },
            ),
            (
                "stage",
                RcCounts {
                    retain: 0,
                    release: 4,
                    clone: 1,
                },
            ),
        ],
    );
}

#[test]
fn aggregate_walk_rc_calls_are_pinned() {
    expect(
        "benchmarks/uniqueness/aggregate_walk.gos",
        &[
            (
                "point",
                RcCounts {
                    retain: 0,
                    release: 1,
                    clone: 0,
                },
            ),
            (
                "shape",
                RcCounts {
                    retain: 0,
                    release: 8,
                    clone: 0,
                },
            ),
            (
                "walk",
                RcCounts {
                    retain: 1,
                    release: 1,
                    clone: 0,
                },
            ),
            (
                "main",
                RcCounts {
                    retain: 0,
                    release: 16,
                    clone: 4,
                },
            ),
        ],
    );
}

#[test]
fn string_builder_rc_calls_are_pinned() {
    expect(
        "benchmarks/uniqueness/string_builder.gos",
        &[
            (
                "render",
                RcCounts {
                    retain: 0,
                    release: 6,
                    clone: 0,
                },
            ),
            (
                "page",
                RcCounts {
                    retain: 0,
                    release: 8,
                    clone: 0,
                },
            ),
            (
                "main",
                RcCounts {
                    retain: 0,
                    release: 12,
                    clone: 0,
                },
            ),
        ],
    );
}
