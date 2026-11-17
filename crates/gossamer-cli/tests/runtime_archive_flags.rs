//! Every runtime archive a release ships is compiled with the codegen flags
//! the CLI build script gives the archives it builds, so a program links the
//! same runtime whichever archive it gets.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(workspace_root().join(relative))
        .unwrap_or_else(|e| panic!("reading {relative}: {e}"))
}

const ARCHIVE_FLAGS: &[&str] = &[
    "-C force-frame-pointers=yes",
    "--cfg gos_program_archive",
    "-C llvm-args=-align-all-functions=6",
    "-C relocation-model=static",
];

#[test]
fn release_archive_script_carries_the_build_script_flags() {
    let build_script = read("crates/gossamer-cli/build.rs");
    let archive_script = read("scripts/build-runtime-archive.sh");
    for flag in ARCHIVE_FLAGS {
        assert!(
            build_script.contains(flag),
            "build.rs no longer passes `{flag}`"
        );
        assert!(
            archive_script.contains(flag),
            "scripts/build-runtime-archive.sh does not pass `{flag}`"
        );
    }
}

#[test]
fn workflows_build_runtime_archives_through_the_script() {
    for workflow in [".github/workflows/release.yml", ".github/workflows/ci.yml"] {
        for line in read(workflow).lines() {
            let line = line.trim();
            let builds_archive = line.starts_with("cargo ")
                && line.contains("--target")
                && line.contains("-p gossamer-runtime");
            assert!(
                !builds_archive,
                "{workflow} builds a runtime archive without scripts/build-runtime-archive.sh: {line}"
            );
        }
    }
}
