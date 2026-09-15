//! A release build reuses the object of every LLVM module whose input did
//! not change, and rebuilds every module whose input did.
//!
//! A module's input includes the `available_externally` copies of the
//! callees it inlines from other modules, so editing a callee has to reach
//! its callers' objects. These tests edit a project between builds and check
//! both halves: the artifact prints what the edited source says, and the
//! modules the edit did not reach come from the cache.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gos-incremental-release-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("create project");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/incr\"\nversion = \"0.1.0\"\nentry = \"src/main.gos\"\n",
    )
    .expect("write manifest");
    dir
}

/// Enough functions per module that the program is split into one LLVM
/// module per source module.
fn filler(prefix: &str) -> String {
    (0..8).fold(String::new(), |mut out, i| {
        writeln!(
            out,
            "pub fn {prefix}_{i}(x: i64) -> i64 {{ x * {i} + {i} }}"
        )
        .expect("writing to a String cannot fail");
        out
    })
}

fn util_source(scale: i64, offset: i64) -> String {
    format!(
        "pub const OFFSET: i64 = {offset}\n\n\
         pub fn scaled(x: i64) -> i64 {{ x * {scale} }}\n\n{}",
        filler("util")
    )
}

fn shapes_source(fields: &str, area: &str) -> String {
    format!(
        "pub struct Rect {{\n{fields}\n}}\n\n\
         pub fn area(r: Rect) -> i64 {{ {area} }}\n\n\
         pub fn make(w: i64, h: i64) -> Rect {{ Rect {{ w: w, h: h }} }}\n\n{}",
        filler("shapes")
    )
}

/// Calls every function so none is pruned before codegen.
fn main_source() -> String {
    let calls = (0..8).fold(String::new(), |mut out, i| {
        writeln!(
            out,
            "    fill += util::util_{i}({i}) + shapes::shapes_{i}({i})"
        )
        .expect("writing to a String cannot fail");
        out
    });
    format!(
        "use util\nuse shapes\n\nfn main() {{\n    let mut total = 0\n    \
         for i in 0..10 {{ total += util::scaled(i) }}\n    let r = shapes::make(3, 4)\n    \
         let mut fill = 0\n{calls}    \
         println(\"{{}} {{}} {{}} {{}}\", total, util::OFFSET, shapes::area(r), fill)\n}}\n"
    )
}

struct Build {
    printed: String,
    compiled: usize,
    cached: usize,
}

fn build_and_run(dir: &Path) -> Build {
    build_and_run_profile(dir, true)
}

fn build_and_run_profile(dir: &Path, release: bool) -> Build {
    let args: &[&str] = if release {
        &["build", "--release"]
    } else {
        &["build"]
    };
    let build = Command::new(gos_bin())
        .current_dir(dir)
        .env("GOSSAMER_CACHE_DIR", dir.join("frontend-cache"))
        .env("GOS_PIPELINE_TRACE", "1")
        .args(args)
        .output()
        .expect("gos build");
    let stdout = String::from_utf8_lossy(&build.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&build.stderr).into_owned();
    assert!(build.status.success(), "{stdout}{stderr}");
    let chunk_lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("llvm pipeline: chunk"))
        .collect();
    let compiled = chunk_lines
        .iter()
        .filter(|line| line.ends_with("compiling"))
        .count();
    let cached = chunk_lines
        .iter()
        .filter(|line| line.ends_with("cached"))
        .count();
    let binary = stdout
        .lines()
        .find_map(|line| {
            let (_, rest) = line.split_once("native executable at ")?;
            Some(PathBuf::from(rest.split(" (target").next()?.trim()))
        })
        .unwrap_or_else(|| panic!("no artifact path in: {stdout}"));
    let run = Command::new(&binary).output().expect("run the artifact");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    Build {
        printed: String::from_utf8_lossy(&run.stdout).trim().to_string(),
        compiled,
        cached,
    }
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join("src").join(name), text).expect("write module");
}

#[test]
fn release_objects_follow_every_edit_and_only_those_edits() {
    let dir = project("edits");
    write(&dir, "main.gos", &main_source());
    write(&dir, "util.gos", &util_source(2, 7));
    write(
        &dir,
        "shapes.gos",
        &shapes_source("    w: i64\n    h: i64", "r.w * r.h"),
    );

    let first = build_and_run(&dir);
    assert_eq!(first.printed, "90 7 12 336");
    assert!(first.compiled >= 3, "one LLVM module per source module");
    let modules = first.compiled + first.cached;

    // A comment moves every later byte offset and changes no IR.
    let shapes = std::fs::read_to_string(dir.join("src").join("shapes.gos")).expect("read");
    write(&dir, "shapes.gos", &format!("// a note\n{shapes}"));
    let comment = build_and_run(&dir);
    assert_eq!(comment.printed, "90 7 12 336");
    assert_eq!(comment.compiled, 0, "a comment reached LLVM");
    assert_eq!(comment.cached, modules);

    // A callee body inlined into `main` from another module.
    write(&dir, "util.gos", &util_source(3, 7));
    let callee = build_and_run(&dir);
    assert_eq!(
        callee.printed, "135 7 12 336",
        "a stale inlined callee survived"
    );
    assert!(
        callee.compiled < modules,
        "an edit to one module rebuilt every module"
    );

    // A constant another module reads.
    write(&dir, "util.gos", &util_source(3, 11));
    let constant = build_and_run(&dir);
    assert_eq!(
        constant.printed, "135 11 12 336",
        "a stale constant survived"
    );

    // A struct layout both modules depend on.
    write(
        &dir,
        "shapes.gos",
        &shapes_source("    h: i64\n    pad: i64\n    w: i64", "r.w * r.h + r.pad")
            .replace("Rect { w: w, h: h }", "Rect { w: w, h: h, pad: 100 }"),
    );
    let layout = build_and_run(&dir);
    assert_eq!(
        layout.printed, "135 11 112 336",
        "a stale struct layout survived"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A debug build names each frame's file and line in its generated code, so a
/// module's object depends on its own file's lines and on nothing assembled
/// around it: an edit to one module rebuilds that module alone.
#[test]
fn debug_objects_follow_only_the_edited_module() {
    let dir = project("debug-edits");
    write(&dir, "main.gos", &main_source());
    write(&dir, "util.gos", &util_source(2, 7));
    write(
        &dir,
        "shapes.gos",
        &shapes_source("    w: i64\n    h: i64", "r.w * r.h"),
    );

    let first = build_and_run_profile(&dir, false);
    assert_eq!(first.printed, "90 7 12 336");
    assert!(first.compiled >= 3, "one LLVM module per source module");
    let modules = first.compiled + first.cached;

    // A comment moves the lines of `util.gos` alone.
    let util = std::fs::read_to_string(dir.join("src").join("util.gos")).expect("read");
    write(&dir, "util.gos", &format!("// a note\n{util}"));
    let comment = build_and_run_profile(&dir, false);
    assert_eq!(comment.printed, "90 7 12 336");
    assert!(
        comment.compiled <= 1,
        "a comment in one module rebuilt {} modules",
        comment.compiled
    );
    assert_eq!(comment.compiled + comment.cached, modules);

    // A body edit reaches the program.
    write(&dir, "util.gos", &util_source(3, 7));
    let body = build_and_run_profile(&dir, false);
    assert_eq!(body.printed, "135 7 12 336", "a stale body survived");
    assert!(
        body.compiled < modules,
        "an edit to one module rebuilt every module"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
