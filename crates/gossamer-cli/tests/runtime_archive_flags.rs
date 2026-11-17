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
    "--cfg gos_program_archive",
    "-C llvm-args=-align-all-functions=6",
    "-C relocation-model=static",
];

/// What only a runtime built for `--frame-pointers` carries.
const FRAME_POINTER_FLAGS: &[&str] = &["-C force-frame-pointers=yes", "--cfg gos_frame_pointers"];

#[test]
fn release_archive_script_carries_the_build_script_flags() {
    let shared_flags = read("crates/gossamer-driver/src/archive_flags.rs");
    let archive_script = read("scripts/build-runtime-archive.sh");
    assert!(
        read("crates/gossamer-cli/build.rs").contains("archive_flags::program_archive_rustflags"),
        "build.rs no longer takes its flags from archive_flags.rs"
    );
    for flag in ARCHIVE_FLAGS {
        assert!(
            shared_flags.contains(flag),
            "archive_flags.rs no longer passes `{flag}`"
        );
        assert!(
            archive_script.contains(flag),
            "scripts/build-runtime-archive.sh does not pass `{flag}`"
        );
    }
}

#[test]
fn shipped_archives_leave_frame_pointers_to_the_opt_in() {
    let shared_flags = read("crates/gossamer-driver/src/archive_flags.rs");
    let archive_script = read("scripts/build-runtime-archive.sh");
    for flag in FRAME_POINTER_FLAGS {
        assert!(
            !archive_script.contains(flag),
            "scripts/build-runtime-archive.sh passes `{flag}`, which only `--frame-pointers` asks for"
        );
        let in_opt_in = shared_flags
            .split("FRAME_POINTER_RUSTFLAGS")
            .nth(1)
            .is_some_and(|rest| rest.contains(flag));
        assert!(in_opt_in, "FRAME_POINTER_RUSTFLAGS does not pass `{flag}`");
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
