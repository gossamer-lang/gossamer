//! Positional file reads from goroutines through `gos build --release`: the
//! full `-O3` pipeline reaches `read_at` / `read_at_into` through runtime
//! shims, and this checks the transcript of the tier-parity fixture there.

#![allow(missing_docs)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

const EXPECTED: &str =
    "#[(0, 200), (1, 200), (2, 200), (3, 200), (4, 200), (5, 200), (6, 200), (7, 200)]\n";

#[test]
fn goroutine_positional_reads_run_in_a_release_build() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let src = root.join("feature-testing-examples/fs_positional_io_goroutines.gos");
    let dir = env::temp_dir().join(format!("gos-fs-positional-{}", std::process::id()));
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

    let run = Command::new(dir.join("fs_positional_io_goroutines"))
        .output()
        .expect("run release binary");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        run.status.success(),
        "release binary exited {:?}: stderr={}",
        run.status.code(),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), EXPECTED);
}
