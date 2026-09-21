//! The ordered surface through `gos build --release`: the full `-O3`
//! pipeline reaches the B+ tree through runtime shims, and a missing
//! dispatch entry there would answer an empty range rather than fail to
//! link. This checks the range fixture's transcript in a release build.

#![allow(missing_docs)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn release_transcript(fixture: &str, binary: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let src = root.join("feature-testing-examples").join(fixture);
    let dir = env::temp_dir().join(format!("gos-btree-release-{}-{binary}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");

    let built = Command::new(env!("CARGO_BIN_EXE_gos"))
        .args(["build", "--release", "--out-dir"])
        .arg(&dir)
        .arg(&src)
        .output()
        .expect("spawn gos build");
    assert!(
        built.status.success(),
        "gos build --release failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = Command::new(dir.join(binary))
        .output()
        .expect("run release binary");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        run.status.success(),
        "release binary exited {:?}: stderr={}",
        run.status.code(),
        String::from_utf8_lossy(&run.stderr)
    );
    String::from_utf8_lossy(&run.stdout).into_owned()
}

/// The transcript the bytecode VM answers for the same fixture, which the
/// tier-parity suite already holds every other tier to.
fn vm_transcript(fixture: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let run = Command::new(env!("CARGO_BIN_EXE_gos"))
        .arg("run")
        .arg(root.join("feature-testing-examples").join(fixture))
        .output()
        .expect("spawn gos run");
    assert!(run.status.success(), "gos run failed");
    String::from_utf8_lossy(&run.stdout).into_owned()
}

#[test]
fn btreemap_ranges_run_in_a_release_build() {
    let fixture = "btreemap_range_api.gos";
    assert_eq!(
        release_transcript(fixture, "btreemap_range_api"),
        vm_transcript(fixture)
    );
}

#[test]
fn btreeset_ranges_run_in_a_release_build() {
    let fixture = "btreeset_range_api.gos";
    assert_eq!(
        release_transcript(fixture, "btreeset_range_api"),
        vm_transcript(fixture)
    );
}
