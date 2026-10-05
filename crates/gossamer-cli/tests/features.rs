#![allow(missing_docs)]

//! Build features: `[features]`, `#[cfg(feature = "..")]` per package, a
//! dependency's features chosen by its dependent, optional dependencies
//! behind `dep:`, `#[cfg]` on an import, `--features` and
//! `--no-default-features`, and a `[native]` table that belongs to a
//! feature, on the bytecode VM, the JIT, and a native build.

use std::path::{Path, PathBuf};
use std::process::Command;

fn gos() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("create a directory");
    }
    std::fs::write(path, text).expect("write a file");
}

/// A project with a default feature, a dependency whose feature a project
/// feature turns on, and an optional dependency.
fn project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-features-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir.join("project.toml"),
        r#"[project]
id = "example.com/feat"
version = "0.1.0"

[features]
default = ["loud"]
loud = []
fancy = ["extra/shiny", "dep:opt"]

[dependencies]
"example.com/extra" = { path = "deps/extra" }
"example.com/opt" = { path = "deps/opt", optional = true }
"#,
    );
    write(
        &dir.join("deps/extra/project.toml"),
        "[project]\nid = \"example.com/extra\"\nversion = \"0.1.0\"\n\n[features]\nshiny = []\n",
    );
    write(
        &dir.join("deps/extra/src/lib.gos"),
        r#"#[cfg(feature = "shiny")]
pub fn look() -> String {
    "shiny"
}

#[cfg(not(feature = "shiny"))]
pub fn look() -> String {
    "plain"
}

// The dependency's own `loud` is not the project's.
#[cfg(feature = "loud")]
pub fn echo() -> String {
    "dependency loud"
}

#[cfg(not(feature = "loud"))]
pub fn echo() -> String {
    "dependency quiet"
}
"#,
    );
    write(
        &dir.join("deps/opt/project.toml"),
        "[project]\nid = \"example.com/opt\"\nversion = \"0.1.0\"\n",
    );
    write(
        &dir.join("deps/opt/src/lib.gos"),
        "pub fn hello() -> String {\n    \"optional here\"\n}\n",
    );
    write(
        &dir.join("src/main.gos"),
        r#"use extra
#[cfg(feature = "fancy")]
use opt

#[cfg(feature = "loud")]
fn volume() -> String {
    "LOUD"
}

#[cfg(not(feature = "loud"))]
fn volume() -> String {
    "quiet"
}

#[cfg(feature = "fancy")]
fn optional() -> String {
    opt::hello()
}

#[cfg(not(feature = "fancy"))]
fn optional() -> String {
    "no optional"
}

fn main() {
    println(f"{volume()} {extra::look()} {extra::echo()} {optional()}")
}
"#,
    );
    dir
}

/// What the program prints on each tier with `flags`; every tier must
/// agree.
fn run_everywhere(dir: &Path, flags: &[&str]) -> String {
    let vm = Command::new(gos())
        .current_dir(dir)
        .args(flags)
        .args(["run", "src/main.gos"])
        .env("GOS_JIT", "0")
        .output()
        .expect("run gos");
    assert!(
        vm.status.success(),
        "{}",
        String::from_utf8_lossy(&vm.stderr)
    );
    let jit = Command::new(gos())
        .current_dir(dir)
        .args(flags)
        .args(["run", "src/main.gos"])
        .env("GOSSAMER_JIT_THRESHOLD", "1")
        .output()
        .expect("run gos");
    assert!(
        jit.status.success(),
        "{}",
        String::from_utf8_lossy(&jit.stderr)
    );
    let built = Command::new(gos())
        .current_dir(dir)
        .args(flags)
        .args(["build", "--release", "--out-dir", "out", "src/main.gos"])
        .output()
        .expect("run gos build");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let exe = dir
        .join("out")
        .join(format!("feat{}", std::env::consts::EXE_SUFFIX));
    let native = Command::new(exe).output().expect("run the binary");
    let vm = String::from_utf8_lossy(&vm.stdout).into_owned();
    assert_eq!(vm, String::from_utf8_lossy(&jit.stdout), "the JIT differs");
    assert_eq!(
        vm,
        String::from_utf8_lossy(&native.stdout),
        "the binary differs"
    );
    vm
}

#[test]
fn default_features_apply_to_the_project_alone() {
    let dir = project("default");
    assert_eq!(
        run_everywhere(&dir, &[]),
        "LOUD plain dependency quiet no optional\n"
    );
}

#[test]
fn a_feature_turns_on_a_dependency_feature_and_an_optional_dependency() {
    let dir = project("fancy");
    assert_eq!(
        run_everywhere(&dir, &["--features", "fancy"]),
        "LOUD shiny dependency quiet optional here\n"
    );
}

#[test]
fn no_default_features_leaves_the_defaults_off() {
    let dir = project("bare");
    assert_eq!(
        run_everywhere(&dir, &["--no-default-features"]),
        "quiet plain dependency quiet no optional\n"
    );
}

#[test]
fn an_unknown_feature_is_refused() {
    let dir = project("unknown");
    let out = Command::new(gos())
        .current_dir(&dir)
        .args(["--features", "nope", "check", "src/main.gos"])
        .output()
        .expect("run gos");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("the project has no feature `nope`"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_native_table_compiles_only_with_its_feature() {
    let dir = project("native");
    let manifest = std::fs::read_to_string(dir.join("project.toml")).expect("read");
    write(
        &dir.join("project.toml"),
        &format!(
            "{}\n[native]\nsources = [\"csrc/fast.c\"]\nfeature = \"fast\"\n",
            manifest.replace("loud = []\n", "loud = []\nfast = []\n")
        ),
    );
    write(
        &dir.join("csrc/fast.c"),
        "int fast_twice(int x) { return 2 * x; }\n",
    );
    write(
        &dir.join("src/main.gos"),
        r#"#[cfg(feature = "fast")]
unsafe extern "C" {
    fn fast_twice(x: i32) -> i32
}

#[cfg(feature = "fast")]
fn twice(x: i32) -> i32 {
    unsafe { fast_twice(x) }
}

#[cfg(not(feature = "fast"))]
fn twice(x: i32) -> i32 {
    x + x
}

fn main() {
    println(twice(21))
}
"#,
    );
    assert_eq!(run_everywhere(&dir, &[]), "42\n");
    assert_eq!(run_everywhere(&dir, &["--features", "fast"]), "42\n");
    // Without the feature nothing was compiled from the C source.
    std::fs::write(dir.join("csrc/fast.c"), "this is not C\n").expect("break the C source");
    assert_eq!(run_everywhere(&dir, &[]), "42\n");
}
