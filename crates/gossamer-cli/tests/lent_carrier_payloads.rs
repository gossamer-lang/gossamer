//! A `Result` or `Option` handed to a Gossamer function is lent: the callee
//! reads, binds, or returns its payload without the caller's share being
//! given back twice. The runtime's reference trace reports every `Vec` free
//! with the count it found, and a free that finds zero is a second free of an
//! object already gone.

#![allow(missing_docs)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/lent_carrier_payloads.gos")
}

#[test]
fn lent_carrier_payloads_are_freed_once_on_native_debug_and_release() {
    let vm = Command::new(gos_bin())
        .arg("run")
        .arg(fixture())
        .output()
        .expect("spawn gos run");
    assert!(
        vm.status.success(),
        "{}",
        String::from_utf8_lossy(&vm.stderr)
    );
    for (profile, tag) in [(&[][..], "debug"), (&["--release"][..], "release")] {
        let dir = std::env::temp_dir().join(format!("gos-lent-{}-{tag}", std::process::id()));
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
            "{tag}: {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let exe = dir.join(if cfg!(windows) {
            "lent_carrier_payloads.exe"
        } else {
            "lent_carrier_payloads"
        });
        let run = Command::new(&exe)
            .env("GOS_RC_TRACE", "1")
            .output()
            .expect("run native binary");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(run.status.success(), "{tag}: {:?} {stderr}", run.status);
        assert_eq!(run.stdout, vm.stdout, "{tag} output differs from the VM");
        let second_frees = stderr.lines().filter(|l| l.contains("old_rc=0")).count();
        assert_eq!(
            second_frees, 0,
            "{tag}: {second_frees} frees of an already-freed Vec"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
