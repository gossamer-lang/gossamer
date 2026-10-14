//! Typed serde through `gos build --release`: the encode and decode walks the
//! HIR serde pass generates, over the runtime's typed reader and writer. The
//! transcript is pinned, so a changed byte of output or of an error message
//! fails here, and the tier-parity walk holds every other tier to it.

#![allow(missing_docs)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn the_typed_serde_fixture_prints_its_pinned_transcript_in_a_release_build() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let src = root.join("feature-testing-examples/serde_typed_reader.gos");
    let expected = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/serde_typed_reader.stdout"),
    )
    .expect("read the pinned transcript");
    let dir = env::temp_dir().join(format!("gos-serde-release-{}", std::process::id()));
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
    let run = Command::new(dir.join("serde_typed_reader"))
        .output()
        .expect("run release binary");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        run.status.success(),
        "release binary exited {:?}: stderr={}",
        run.status.code(),
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert_eq!(stdout, expected);
}
