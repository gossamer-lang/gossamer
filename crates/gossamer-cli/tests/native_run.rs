//! `gos run` runs the program's native executable: a stale build is redone
//! with the debug profile before the run, the release build follows in the
//! background, and the VM runs the program only when asked for or when the
//! host cannot build native code. The program reports which executable runs
//! it.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-native-run-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn project(tag: &str) -> PathBuf {
    let dir = scratch(tag);
    std::fs::create_dir_all(dir.join("src")).expect("create project");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/nativerun\"\nversion = \"0.1.0\"\nentry = \"src/main.gos\"\n",
    )
    .expect("write manifest");
    dir
}

/// A program printing `label` and the name of the executable running it.
fn program(label: &str) -> String {
    format!(
        "use std::fs\n\nfn main() {{\n    \
         let name = fs::read_to_string(\"/proc/self/comm\").unwrap()\n    \
         println(f\"{label} {{name.trim()}}\")\n}}\n"
    )
}

fn run(entry: &Path, cache: &Path, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(gos_bin());
    command
        .arg("run")
        .arg(entry)
        .env_remove("GOS_RUN_ENGINE")
        .env("XDG_CACHE_HOME", cache);
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().expect("spawn gos run")
}

fn stdout_of(out: &Output) -> String {
    assert!(
        out.status.success(),
        "gos run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Waits for the background release build the last run started, by taking
/// the lock it holds while it builds.
fn wait_for_release_build(build_dir: &Path) {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(build_dir.join("build.lock"))
        .expect("open build lock");
    lock.lock().expect("take build lock");
}

#[test]
fn a_stale_project_builds_and_runs_natively_and_the_release_build_follows() {
    let dir = project("stale");
    let cache = dir.join("cache");
    let entry = dir.join("src/main.gos");
    std::fs::write(&entry, program("first")).expect("write main");

    let first = stdout_of(&run(&entry, &cache, &[]));
    assert_eq!(first, "first nativerun", "a stale build runs natively");
    assert!(dir.join(".gos-cache/run/debug/nativerun").is_file());

    let build_dir = dir.join(".gos-cache/run");
    wait_for_release_build(&build_dir);
    assert!(
        build_dir.join("nativerun").is_file(),
        "the release build follows the run: {}",
        std::fs::read_to_string(build_dir.join("build.log")).unwrap_or_default()
    );
    assert_eq!(stdout_of(&run(&entry, &cache, &[])), "first nativerun");

    std::fs::write(&entry, program("second")).expect("rewrite main");
    assert_eq!(
        stdout_of(&run(&entry, &cache, &[])),
        "second nativerun",
        "an edit is built before the run"
    );
    wait_for_release_build(&build_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_vm_runs_the_program_when_asked_for() {
    let dir = project("vm");
    let entry = dir.join("src/main.gos");
    std::fs::write(&entry, program("vm")).expect("write main");
    let cache = dir.join("cache");
    assert_eq!(
        stdout_of(&run(&entry, &cache, &[("GOS_RUN_ENGINE", "vm")])),
        "vm gos"
    );
    let out = Command::new(gos_bin())
        .args(["run", "--no-jit"])
        .arg(&entry)
        .env("XDG_CACHE_HOME", &cache)
        .output()
        .expect("spawn gos run");
    assert_eq!(stdout_of(&out), "vm gos");
    assert!(
        !dir.join(".gos-cache/run").exists(),
        "a VM run builds nothing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_outside_any_project_builds_into_the_user_cache() {
    let dir = scratch("single");
    let cache = dir.join("cache");
    let entry = dir.join("tool.gos");
    std::fs::write(&entry, program("single")).expect("write file");
    assert_eq!(stdout_of(&run(&entry, &cache, &[])), "single tool");
    assert!(
        !dir.join(".gos-cache").exists(),
        "nothing is written beside the file"
    );
    let runs = cache.join("gossamer/run");
    let builds: Vec<_> = std::fs::read_dir(&runs)
        .expect("the run cache exists")
        .flatten()
        .collect();
    assert_eq!(builds.len(), 1, "one build directory per file");
    wait_for_release_build(&builds[0].path());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_host_without_llvm_runs_the_program_on_the_vm_and_says_so() {
    let dir = project("nollvm");
    let entry = dir.join("src/main.gos");
    std::fs::write(&entry, program("fallback")).expect("write main");
    let cache = dir.join("cache");
    let missing = dir.join("no-such-opt");
    let out = run(
        &entry,
        &cache,
        &[("GOS_LLVM_OPT", missing.to_str().expect("utf-8 path"))],
    );
    assert_eq!(stdout_of(&out), "fallback gos");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("note: running on the VM"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_program_that_does_not_check_reports_its_errors_and_does_not_run() {
    let dir = project("broken");
    let entry = dir.join("src/main.gos");
    std::fs::write(
        &entry,
        "fn main() {\n    let x: i64 = \"text\"\n    println(x)\n}\n",
    )
    .expect("write main");
    let out = run(&entry, &dir.join("cache"), &[]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "nothing ran");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}
