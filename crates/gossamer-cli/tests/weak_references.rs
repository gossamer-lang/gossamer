//! Native builds reclaim every object a `Weak` observes once its last owner
//! and its last weak reference are gone, wherever the `Weak` is stored, and a
//! stored `Weak` never keeps its target alive.

#![allow(missing_docs)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/weak_references_in_containers.gos")
}

fn vm_output() -> String {
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(fixture())
        .output()
        .expect("spawn gos run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Builds the fixture with `profile` flags and answers its stdout and the
/// count of reference-counted objects still allocated at exit.
fn native_run(profile: &[&str], tag: &str) -> (String, u64) {
    let dir = std::env::temp_dir().join(format!("gos-weak-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let build = Command::new(gos_bin())
        .arg("build")
        .args(profile)
        .arg(fixture())
        .arg("--out-dir")
        .arg(&dir)
        .output()
        .expect("spawn gos build");
    assert!(
        build.status.success(),
        "build {tag}: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let exe = dir.join(if cfg!(windows) {
        "weak_references_in_containers.exe"
    } else {
        "weak_references_in_containers"
    });
    let run = Command::new(&exe)
        .env("GOS_RC_DEBUG", "1")
        .output()
        .expect("run native binary");
    assert!(run.status.success(), "run {tag}: {:?}", run.status);
    let stderr = String::from_utf8_lossy(&run.stderr);
    let live = stderr
        .lines()
        .find_map(|line| line.strip_prefix("RC_LIVE_AT_EXIT="))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{tag}: no RC_LIVE_AT_EXIT line in {stderr}"));
    let _ = std::fs::remove_dir_all(&dir);
    (String::from_utf8_lossy(&run.stdout).into_owned(), live)
}

#[test]
fn weak_references_reclaim_on_native_debug_and_release() {
    let expected = vm_output();
    assert!(expected.contains("struct in vec: dead"), "{expected}");
    for (profile, tag) in [(&[][..], "debug"), (&["--release"][..], "release")] {
        let (stdout, live) = native_run(profile, tag);
        assert_eq!(stdout, expected, "{tag} output differs from the VM");
        assert_eq!(live, 0, "{tag}: objects left allocated at exit");
    }
}
