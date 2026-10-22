//! The frontend blob is a function of the program alone: two processes, a
//! cold pass and a cache restore, all write the same bytes, and a build from
//! a restored blob lowers the same MIR and links the same binary as a cold
//! one.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/gossamer-cli sits two levels below the workspace")
        .to_path_buf()
}

/// A fresh directory of this test's own, emptied first.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!(
        "gos-blob-determinism-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn corpus() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut files = Vec::new();
    for dir in ["feature-testing-examples", "examples"] {
        let Ok(entries) = fs::read_dir(root.join(dir)) else {
            continue;
        };
        files.extend(
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "gos")),
        );
    }
    files.sort();
    files
}

/// The blob `gos check` writes for `file`, or `None` for a unit the
/// frontend rejects.
fn blob(file: &Path, out: &Path, cache: Option<&Path>) -> Option<Vec<u8>> {
    let _ = fs::remove_file(out);
    let mut cmd = Command::new(gos_bin());
    cmd.arg("check")
        .arg("--emit-frontend-blob")
        .arg(out)
        .arg(file)
        .current_dir(workspace_root());
    match cache {
        Some(dir) => cmd
            .env("GOSSAMER_CACHE_DIR", dir)
            .env_remove("GOS_NO_CACHE"),
        None => cmd.env("GOS_NO_CACHE", "1"),
    };
    cmd.output().expect("gos runs");
    fs::read(out).ok()
}

#[test]
fn every_accepted_unit_writes_one_blob_across_processes_and_cache_restores() {
    let files = corpus();
    let scratch = scratch_dir("units");
    let workers = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let chunk = files.len().div_ceil(workers).max(1);
    let results: Vec<(usize, Vec<String>)> = std::thread::scope(|scope| {
        files
            .chunks(chunk)
            .enumerate()
            .map(|(w, part)| {
                let dir = scratch.join(format!("w{w}"));
                scope.spawn(move || {
                    fs::create_dir_all(&dir).expect("worker dir");
                    let out = dir.join("unit.blob");
                    let cache = dir.join("cache");
                    let mut compared = 0;
                    let mut problems = Vec::new();
                    for file in part {
                        let Some(first) = blob(file, &out, None) else {
                            continue;
                        };
                        let runs = [
                            ("a second process", blob(file, &out, None)),
                            ("a cold cached pass", blob(file, &out, Some(&cache))),
                            ("a cache restore", blob(file, &out, Some(&cache))),
                        ];
                        compared += 1;
                        for (what, other) in runs {
                            if other.as_ref() != Some(&first) {
                                problems.push(format!("{}: {what} differs", file.display()));
                            }
                        }
                    }
                    (compared, problems)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().expect("worker"))
            .collect()
    });
    let _ = fs::remove_dir_all(&scratch);
    let compared: usize = results.iter().map(|(n, _)| n).sum();
    let problems: Vec<&String> = results.iter().flat_map(|(_, p)| p).collect();
    assert!(compared > 200, "only {compared} units wrote a blob");
    assert!(
        problems.is_empty(),
        "{} units differ:\n{problems:#?}",
        problems.len()
    );
}

/// MIR dump and binary of a debug build of `file` in `dir`.
fn build(file: &Path, dir: &Path, cache: &Path, out: &str) -> (String, Vec<u8>) {
    let output = Command::new(gos_bin())
        .arg("build")
        .arg(file)
        .arg("--out-dir")
        .arg(dir.join(out))
        .current_dir(dir)
        .env("GOSSAMER_CACHE_DIR", cache)
        .env("GOS_BUILD_CACHE", cache.join("objects"))
        .env("GOS_LLVM_DUMP_MIR", "1")
        .env_remove("GOS_NO_CACHE")
        .output()
        .expect("gos build runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stem = file.file_stem().expect("file stem");
    let binary = fs::read(dir.join(out).join(stem))
        .or_else(|_| fs::read(dir.join(out).join(stem).with_extension("exe")))
        .expect("binary");
    // Build notes print only when a compile runs, so a build served from
    // the object cache prints fewer of them; the dump is the header lines
    // and the indented lines under them.
    let mir = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| line.starts_with("=== MIR ") || line.starts_with(' '))
        .collect::<Vec<_>>()
        .join("\n");
    (mir, binary)
}

#[test]
fn a_build_from_a_restored_blob_lowers_and_links_as_a_cold_one() {
    let root = workspace_root();
    for name in [
        "vec_range_copies",
        "csv_indexed_reads",
        "map_slice_key_lookups",
    ] {
        let scratch = scratch_dir(name);
        let file = scratch.join(format!("{name}.gos"));
        fs::copy(
            root.join("feature-testing-examples")
                .join(format!("{name}.gos")),
            &file,
        )
        .expect("copy fixture");
        let cache = scratch.join("cache");
        let (cold_mir, cold_bin) = build(&file, &scratch, &cache, "cold");
        assert!(cold_mir.contains("=== MIR "), "{name}: no MIR dump");
        let (warm_mir, warm_bin) = build(&file, &scratch, &cache, "warm");
        assert_eq!(
            cold_mir, warm_mir,
            "{name}: MIR differs after a cache restore"
        );
        // PE and Mach-O images carry build timestamps and UUIDs; ELF ones
        // built from the same input are identical.
        if cfg!(target_os = "linux") {
            assert!(
                cold_bin == warm_bin,
                "{name}: binary differs after a cache restore"
            );
        }
        let _ = fs::remove_dir_all(&scratch);
    }
}
