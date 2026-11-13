//! The 0.78.7 std surface through `gos build --release`: descriptors that
//! foreign code answers as integers (`fs::File::from_raw`,
//! `fd::Borrowed::from_raw`), `fs::temp_dir` under a parent, and
//! `process::look_path`, each the transcript of its tier-parity fixture.

#![allow(missing_docs)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn run_release(fixture: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let src = root.join(format!("feature-testing-examples/{fixture}.gos"));
    let dir = env::temp_dir().join(format!("gos-{fixture}-release-{}", std::process::id()));
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

    let run = Command::new(dir.join(fixture))
        .output()
        .expect("run release binary");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        run.status.success(),
        "release binary exited {:?}: stderr={}",
        run.status.code(),
        String::from_utf8_lossy(&run.stderr)
    );
    String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n")
}

#[test]
fn raw_descriptors_wait_in_a_release_build() {
    assert_eq!(
        run_release("fd_raw_descriptors"),
        "ready before: false\nlent writable: true\nready after: true\nlent raw kept: true\n\
         writable rounds: 50\nnegative borrowed: refused\nnegative owned: refused\n"
    );
}

#[test]
fn temp_dir_under_a_parent_runs_in_a_release_build() {
    assert_eq!(
        run_release("fs_temp_dir_parent"),
        "cache under temp root: true\nmade under parent: true\nnamed by prefix: true\n\
         distinct: true\nboth folders: true\nmade in rounds: 20\nmissing parent: refused\nnested prefix: refused\n\
         cleaned: true\n"
    );
}

#[test]
fn look_path_runs_in_a_release_build() {
    assert_eq!(
        run_release("process_look_path"),
        "found: true\nfound path resolves to itself: true\nfound in rounds: 20\nmissing: None\n\
         empty: None\n"
    );
}
