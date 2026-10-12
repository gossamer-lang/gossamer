// Integration tests are binaries without a crate-level doc; the test names
// document them.
#![allow(missing_docs)]

use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/frame_array_views.gos")
}

fn vm_output() -> String {
    let output = Command::new(gos_bin())
        .arg("run")
        .arg(fixture())
        .env("GOS_JIT", "0")
        .output()
        .expect("run the fixture on the VM");
    assert!(output.status.success(), "VM run failed: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn compiled_run(release: bool) -> (String, String) {
    let out_dir = std::env::temp_dir().join(format!(
        "gos-frame-views-{}-{}",
        std::process::id(),
        if release { "release" } else { "debug" }
    ));
    let mut compile = Command::new(gos_bin());
    compile.arg("build");
    if release {
        compile.arg("--release");
    }
    let compiled = compile
        .arg("--out-dir")
        .arg(&out_dir)
        .arg(fixture())
        .output()
        .expect("build the fixture");
    assert!(compiled.status.success(), "build failed: {compiled:?}");
    let run = Command::new(out_dir.join("frame_array_views"))
        .env("GOS_RC_DEBUG", "1")
        .output()
        .expect("run the built fixture");
    let _ = std::fs::remove_dir_all(&out_dir);
    assert!(run.status.success(), "compiled run failed: {run:?}");
    (
        String::from_utf8_lossy(&run.stdout).into_owned(),
        String::from_utf8_lossy(&run.stderr).into_owned(),
    )
}

#[test]
fn a_frame_view_reused_by_a_loop_is_released_once_per_pass_in_release_builds() {
    let (stdout, stderr) = compiled_run(true);
    assert_eq!(stdout, vm_output());
    assert!(!stderr.contains("already zero"), "{stderr}");
}

#[test]
fn a_frame_view_reused_by_a_loop_is_released_once_per_pass_in_debug_builds() {
    let (stdout, stderr) = compiled_run(false);
    assert_eq!(stdout, vm_output());
    assert!(!stderr.contains("already zero"), "{stderr}");
}
