//! `project.ffi` end to end.
//!
//! A project calls native code unless its manifest says `ffi = false`. Each case
//! lays out a project (and, where it matters, a path dependency) in a
//! fresh directory and asserts what `gos check`, `run`, `test`, and
//! `build` answer.

use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

struct Outcome {
    ok: bool,
    output: String,
}

fn gos(dir: &Path, args: &[&str]) -> Outcome {
    let out = Command::new(gos_binary())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run gos");
    Outcome {
        ok: out.status.success(),
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

fn workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-foreign-opt-in-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture parent");
    }
    std::fs::write(path, body).expect("write fixture");
}

fn manifest(id: &str, extra: &str) -> String {
    format!("[project]\nid = \"example.com/{id}\"\nversion = \"0.1.0\"\n{extra}")
}

const CALLS_ABS: &str = r#"unsafe extern "C" {
    fn abs(x: i32) -> i32
}

fn main() {
    println(unsafe { abs(-4) })
}

#[cfg(test)]
mod tests {
    #[test]
    fn four() {
        assert_eq(super::magnitude(), 4)
    }
}

fn magnitude() -> i32 {
    unsafe { abs(-4) }
}
"#;

fn app(tag: &str, extra: &str) -> PathBuf {
    let dir = workspace(tag);
    write(&dir.join("project.toml"), &manifest("app", extra));
    write(&dir.join("src/main.gos"), CALLS_ABS);
    dir
}

fn assert_not_allowed(outcome: &Outcome) {
    assert!(!outcome.ok, "expected a failure, got:\n{}", outcome.output);
    assert!(
        outcome.output.contains("GT0102"),
        "expected GT0102, got:\n{}",
        outcome.output
    );
}

#[test]
fn a_project_that_says_false_stops_every_command() {
    let dir = app("false", "ffi = false\n");
    assert_not_allowed(&gos(&dir, &["check", "src/main.gos"]));
    assert_not_allowed(&gos(&dir, &["run", "src/main.gos"]));
    assert_not_allowed(&gos(&dir, &["test"]));
    assert_not_allowed(&gos(&dir, &["build", "src/main.gos"]));
}

#[test]
fn a_project_without_the_key_calls_c() {
    let dir = app("absent", "");
    let run = gos(&dir, &["run", "src/main.gos"]);
    assert!(run.ok, "{}", run.output);
    assert_eq!(run.output.trim(), "4");
    let check = gos(&dir, &["check", "src/main.gos"]);
    assert!(check.ok, "{}", check.output);
}

#[test]
fn a_project_that_says_true_calls_c() {
    let dir = app("true", "ffi = true\n");
    let run = gos(&dir, &["run", "src/main.gos"]);
    assert!(run.ok, "{}", run.output);
    assert_eq!(run.output.trim(), "4");
}

#[test]
fn the_message_names_the_manifest_to_change() {
    let dir = app("names", "ffi = false\n");
    let run = gos(&dir, &["run", "src/main.gos"]);
    assert_not_allowed(&run);
    assert!(run.output.contains("ffi = false"), "{}", run.output);
    assert!(run.output.contains("project.toml"), "{}", run.output);
}

#[test]
fn a_non_boolean_value_is_a_manifest_error() {
    let dir = app("string", "ffi = \"yes\"\n");
    let run = gos(&dir, &["run", "src/main.gos"]);
    assert!(!run.ok, "{}", run.output);
    assert!(run.output.contains("ffi"), "{}", run.output);
}

const LIB_WITH_EXTERN: &str = r#"unsafe extern "C" {
    fn labs(x: i64) -> i64
}

pub fn magnitude(x: i64) -> i64 {
    unsafe { labs(x) }
}
"#;

const LIB_HIDING_BEHIND_A_MARKER: &str = r#"pub fn plain(x: i64) -> i64 {
    x
}
// gossamer: generated below
unsafe extern "C" {
    fn labs(x: i64) -> i64
}

pub fn magnitude(x: i64) -> i64 {
    unsafe { labs(x) }
}
"#;

const USES_LIB: &str = "use natlib

fn main() {
    println(natlib::magnitude(-9))
}
";

fn app_with_dependency(tag: &str, extra: &str, lib_source: &str) -> PathBuf {
    let root = workspace(tag);
    let lib = root.join("lib");
    write(&lib.join("project.toml"), &manifest("natlib", ""));
    write(&lib.join("src/lib.gos"), lib_source);
    let app = root.join("app");
    let deps = "\n[dependencies]\nnatlib = { path = \"../lib\" }\n";
    write(
        &app.join("project.toml"),
        &manifest("app", &format!("{extra}{deps}")),
    );
    write(&app.join("src/main.gos"), USES_LIB);
    app
}

#[test]
fn a_dependency_that_declares_c_functions_follows_the_root() {
    let refused = app_with_dependency("dep-false", "ffi = false\n", LIB_WITH_EXTERN);
    let run = gos(&refused, &["run", "src/main.gos"]);
    assert_not_allowed(&run);
    assert!(run.output.contains("labs"), "{}", run.output);

    let allowed = app_with_dependency("dep-absent", "", LIB_WITH_EXTERN);
    let run = gos(&allowed, &["run", "src/main.gos"]);
    assert!(run.ok, "{}", run.output);
    assert_eq!(run.output.trim(), "9");
}

#[test]
fn a_dependency_cannot_pass_its_declarations_off_as_generated_code() {
    let app = app_with_dependency("dep-marker", "ffi = false\n", LIB_HIDING_BEHIND_A_MARKER);
    assert_not_allowed(&gos(&app, &["run", "src/main.gos"]));
}

#[test]
fn the_standard_librarys_own_declarations_are_never_refused() {
    let dir = workspace("stdlib");
    write(&dir.join("project.toml"), &manifest("app", "ffi = false\n"));
    write(
        &dir.join("src/main.gos"),
        "use std::term\n\nfn main() {\n    println(term::is_terminal(term::STDOUT))\n}\n",
    );
    let run = gos(&dir, &["run", "src/main.gos"]);
    assert!(run.ok, "{}", run.output);
    assert_eq!(run.output.trim(), "false");
}

#[test]
fn a_file_outside_any_project_is_not_governed() {
    let dir = workspace("loose");
    write(&dir.join("main.gos"), CALLS_ABS);
    let run = gos(&dir, &["run", "main.gos"]);
    assert!(run.ok, "{}", run.output);
    assert_eq!(run.output.trim(), "4");
}

/// A project that declares foreign functions in two of its files, beside
/// four dependencies: `alpha` in one file, `beta` across two, `gamma` reached
/// only through `alpha`, and `plain`, which declares none.
fn layered(tag: &str, extra: &str) -> PathBuf {
    let root = workspace(tag);
    let app = root.join("app");
    let deps = "\n[dependencies]\nalpha = { path = \"../alpha\" }\n\
                beta = { path = \"../beta\" }\nplain = { path = \"../plain\" }\n";
    write(
        &app.join("project.toml"),
        &manifest("app", &format!("{extra}{deps}")),
    );
    write(
        &app.join("src/main.gos"),
        r#"use alpha
use beta
use plain
use util

unsafe extern "C" {
    fn abs(x: i32) -> i32
}

fn main() {
    let own = unsafe { abs(-1) } as i64
    println(own + util::twice(1) + alpha::a(1) + beta::b(1) + beta::helpers::c(1) + plain::p(1))
}
"#,
    );
    write(
        &app.join("src/util.gos"),
        r#"unsafe extern "C" {
    fn labs(x: i64) -> i64
}

#[cfg(target_os = "none")]
unsafe extern "C" {
    fn never_active(x: i64) -> i64
}

pub fn twice(x: i64) -> i64 {
    unsafe { labs(x) } * 2
}
"#,
    );

    let alpha = root.join("alpha");
    write(
        &alpha.join("project.toml"),
        &manifest(
            "alpha",
            "ffi = true\n\n[dependencies]\ngamma = { path = \"../gamma\" }\n",
        ),
    );
    write(
        &alpha.join("src/lib.gos"),
        r#"use gamma

unsafe extern "C" {
    fn llabs(x: i64) -> i64
    fn labs(x: i64) -> i64
}

pub fn a(x: i64) -> i64 {
    unsafe { llabs(x) } + unsafe { labs(x) } - gamma::g(x)
}
"#,
    );

    let beta = root.join("beta");
    write(&beta.join("project.toml"), &manifest("beta", ""));
    write(
        &beta.join("src/lib.gos"),
        r#"unsafe extern "C" {
    fn labs(x: i64) -> i64
}

pub fn b(x: i64) -> i64 {
    unsafe { labs(x) }
}
"#,
    );
    write(
        &beta.join("src/helpers.gos"),
        r#"unsafe extern "C" {
    fn llabs(x: i64) -> i64
}

pub fn c(x: i64) -> i64 {
    unsafe { llabs(x) }
}
"#,
    );

    let gamma = root.join("gamma");
    write(&gamma.join("project.toml"), &manifest("gamma", ""));
    write(
        &gamma.join("src/lib.gos"),
        r#"unsafe extern "C" {
    fn abs(x: i32) -> i32
}

pub fn g(x: i64) -> i64 {
    unsafe { abs(x as i32) } as i64
}
"#,
    );

    let plain = root.join("plain");
    write(&plain.join("project.toml"), &manifest("plain", ""));
    write(
        &plain.join("src/lib.gos"),
        "pub fn p(x: i64) -> i64 {\n    x\n}\n",
    );
    app
}

/// The GT0102 reports in `output`, one per library, with path separators
/// written as `/` so a Windows run reads the same.
fn reports(output: &str) -> Vec<String> {
    output
        .replace('\\', "/")
        .split("error[")
        .filter(|block| block.starts_with("GT0102]"))
        .map(str::to_string)
        .collect()
}

/// The one report that names `library`.
fn report_for<'a>(reports: &'a [String], library: &str) -> &'a str {
    let matching: Vec<&String> = reports
        .iter()
        .filter(|report| {
            report
                .lines()
                .next()
                .is_some_and(|title| title.contains(library))
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "one report for {library} in {reports:#?}"
    );
    matching[0]
}

fn assert_names(report: &str, files: &[&str], functions: &[&str]) {
    for file in files {
        assert!(report.contains(file), "`{file}` missing from:\n{report}");
    }
    for function in functions {
        assert!(
            report.contains(&format!("`{function}`")),
            "`{function}` missing from:\n{report}"
        );
    }
}

#[test]
fn the_report_names_every_library_and_file_that_declares_foreign_functions() {
    let app = layered("layered-false", "ffi = false\n");
    for command in ["check", "run", "build"] {
        let outcome = gos(&app, &[command, "src/main.gos"]);
        assert_not_allowed(&outcome);
        let reports = reports(&outcome.output);
        assert_eq!(
            reports.len(),
            4,
            "`gos {command}`: one report per library that declares foreign functions:\n{}",
            outcome.output
        );

        let project = report_for(&reports, "the project `example.com/app`");
        assert_names(
            project,
            &["src/main.gos", "app/src/util.gos"],
            &["abs", "labs"],
        );
        assert!(!project.contains("never_active"), "{project}");

        let alpha = report_for(&reports, "the dependency `example.com/alpha`");
        assert_names(alpha, &["alpha/src/lib.gos"], &["llabs", "labs"]);

        let beta = report_for(&reports, "the dependency `example.com/beta`");
        assert_names(
            beta,
            &["beta/src/lib.gos", "beta/src/helpers.gos"],
            &["labs", "llabs"],
        );

        let gamma = report_for(&reports, "the dependency `example.com/gamma`");
        assert_names(gamma, &["gamma/src/lib.gos"], &["abs"]);

        assert!(
            !outcome.output.contains("example.com/plain"),
            "a library without foreign functions is not reported:\n{}",
            outcome.output
        );
        for report in &reports {
            assert!(report.contains("app/project.toml"), "{report}");
        }
    }
}

#[test]
fn a_dependency_opting_in_for_itself_does_not_override_the_root() {
    let app = layered("layered-dep-only", "ffi = false\n");
    let run = gos(&app, &["run", "src/main.gos"]);
    let reports = reports(&run.output);
    report_for(&reports, "the dependency `example.com/alpha`");
}

#[test]
fn the_root_allowing_native_code_covers_every_library() {
    let app = layered("layered-absent", "");
    let run = gos(&app, &["run", "src/main.gos"]);
    assert!(run.ok, "{}", run.output);
    assert_eq!(run.output.trim(), "7");
}
