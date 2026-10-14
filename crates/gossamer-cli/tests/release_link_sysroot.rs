//! A static-musl release link reads its CRT from the toolchain `gos` was
//! built with, so an up-to-date `gos build --release` answers without
//! running `rustc`, and a link still finds the CRT when the `rustc` on
//! `PATH` cannot answer.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

#[test]
fn a_release_link_reads_the_build_toolchain_sysroot_without_asking_rustc() {
    if option_env!("GOSSAMER_RUNTIME_LIB_PATH_MUSL").is_none() {
        eprintln!("skipping: this gos was built without the static-musl runtime archive");
        return;
    }
    let dir = std::env::temp_dir().join(format!("gos-release-sysroot-{}", std::process::id()));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("create the scratch directory");
    let failing_rustc = bin.join("rustc");
    std::fs::write(&failing_rustc, "#!/bin/sh\nexit 1\n").expect("write the stand-in rustc");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&failing_rustc, std::fs::Permissions::from_mode(0o755))
            .expect("make the stand-in rustc executable");
    }
    let source = dir.join("hello.gos");
    std::fs::write(&source, "fn main() {\n    println(\"hello\")\n}\n").expect("write the program");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut search = vec![bin.clone()];
    search.extend(std::env::split_paths(&path));

    let output = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_gos")))
        .args(["build", "--release", "--out-dir"])
        .arg(dir.join("out"))
        .arg(&source)
        .env("PATH", std::env::join_paths(search).expect("join PATH"))
        .env_remove("RUSTC")
        .env("GOS_NO_CACHE", "1")
        .output()
        .expect("run gos build");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(output.status.success(), "build failed:\n{stdout}\n{stderr}");
    assert!(
        !stderr.contains("static-musl release link is unavailable"),
        "the link asked rustc for the musl CRT:\n{stderr}"
    );
    assert!(stdout.contains("static-musl"), "{stdout}");
}
