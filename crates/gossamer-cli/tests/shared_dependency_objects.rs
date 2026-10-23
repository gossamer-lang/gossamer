//! A debug build compiles a dependency to the same objects in every program
//! that uses it, so a second program built on a library links the objects the
//! first one left and compiles only its own modules. The library here holds
//! the shapes whose names used to be numbered per program: a generic it
//! specialises itself, closures, a mutable static, derived methods, and a
//! function the two programs specialise at different types.

use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent directory")).expect("create dir");
    std::fs::write(path, text).expect("write file");
}

const LIBRARY: &str = r#"pub mod shapes {
    #[derive(Debug, PartialEq)]
    pub struct Rect { pub w: i64, pub h: i64 }

    static mut BUILT: i64 = 0

    pub fn make(w: i64, h: i64) -> Rect {
        BUILT += 1
        Rect { w: w, h: h }
    }

    pub fn built() -> i64 { BUILT }

    pub fn area(r: Rect) -> i64 { r.w * r.h }

    pub fn largest(rects: Vec<Rect>) -> i64 {
        let areas = rects.map(|r| area(r))
        biggest(areas, 0)
    }

    pub fn biggest<T: PartialOrd>(items: Vec<T>, fallback: T) -> T {
        let mut best = fallback
        for item in items {
            if item > best { best = item }
        }
        best
    }

    pub fn scaled(rects: Vec<Rect>, k: i64) -> Vec<Rect> {
        rects.map(|r| Rect { w: r.w * k, h: r.h * k })
    }

    pub fn describe(r: Rect) -> String { f"{r.w}x{r.h}" }
    pub fn one(x: i64) -> i64 { x + 1 }
    pub fn two(x: i64) -> i64 { x + 2 }
    pub fn three(x: i64) -> i64 { x + 3 }
    pub fn four(x: i64) -> i64 { x + 4 }
    pub fn five(x: i64) -> i64 { x + 5 }
    pub fn six(x: i64) -> i64 { x + 6 }
    pub fn seven(x: i64) -> i64 { x + 7 }
    pub fn eight(x: i64) -> i64 { x + 8 }
    pub fn nine(x: i64) -> i64 { x + 9 }
    pub fn ten(x: i64) -> i64 { x + 10 }
    pub fn eleven(x: i64) -> i64 { x + 11 }
    pub fn twelve(x: i64) -> i64 { x + 12 }
}
"#;

const FIRST_APP: &str = r#"use geom
use geom::shapes

fn main() {
    let rects = #[shapes::make(2, 3), shapes::make(4, 5)]
    let big = shapes::scaled(rects, 2)
    println(f"{shapes::largest(big)} {shapes::built()}")
    println(shapes::biggest(#[3, 9, 4], 0))
    println(shapes::describe(shapes::make(1, 7)))
}
"#;

const SECOND_APP: &str = r##"use geom
use geom::shapes

fn label(n: i64) -> String { f"#{n}" }

fn main() {
    let rects = #[shapes::make(6, 1), shapes::make(2, 2), shapes::make(1, 1)]
    println(f"{shapes::largest(rects)} {shapes::built()}")
    println(shapes::biggest(#["kiwi", "apple", "pear"], ""))
    println(shapes::biggest(#[1.5, 0.5], 0.0))
    println(label(shapes::twelve(shapes::one(0))))
    println(shapes::make(2, 2) == shapes::make(2, 2))
}
"##;

fn app(root: &Path, name: &str, source: &str) -> PathBuf {
    let dir = root.join(name);
    write(
        &dir.join("project.toml"),
        &format!(
            "[project]\nid = \"example.com/{name}\"\nversion = \"0.1.0\"\nentry = \"src/main.gos\"\n\n\
             [dependencies]\n\"example.com/geom\" = {{ path = \"../geom\" }}\n"
        ),
    );
    write(&dir.join("src/main.gos"), source);
    dir
}

/// Builds `dir` with the debug profile, answering what it printed when run
/// and the modules the build served from the object cache and compiled.
fn build_and_run(dir: &Path, cache: &Path) -> (String, Vec<String>, Vec<String>) {
    let out = Command::new(gos_bin())
        .arg("build")
        .arg(dir.join("src/main.gos"))
        .env("XDG_CACHE_HOME", cache)
        .env("LOCALAPPDATA", cache)
        .env("GOS_PIPELINE_TRACE", "1")
        .output()
        .expect("spawn gos build");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "gos build failed: {stderr}");
    let module = |line: &str| line.split('`').nth(1).unwrap_or_default().to_string();
    let cached = stderr
        .lines()
        .filter(|line| line.ends_with(", cached"))
        .map(module)
        .collect();
    let compiled = stderr
        .lines()
        .filter(|line| line.ends_with(", compiling"))
        .map(module)
        .collect();
    let name = dir.file_name().expect("a project name").to_string_lossy();
    let exe = dir
        .join("target/debug")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let ran = Command::new(&exe).output().expect("run the program");
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    (
        String::from_utf8_lossy(&ran.stdout).into_owned(),
        cached,
        compiled,
    )
}

#[test]
fn a_second_program_links_the_objects_the_first_built_for_their_library() {
    let root = std::env::temp_dir().join(format!("gos-shared-objects-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    write(
        &root.join("geom/project.toml"),
        "[project]\nid = \"example.com/geom\"\nversion = \"0.1.0\"\n\n[lib]\nname = \"geom\"\n",
    );
    write(&root.join("geom/src/lib.gos"), LIBRARY);
    let cache = root.join("cache");

    let first = app(&root, "first", FIRST_APP);
    let (printed, _, compiled) = build_and_run(&first, &cache);
    assert_eq!(printed, "80 2\n9\n1x7\n");
    assert!(
        compiled.iter().any(|module| module.starts_with("geom")),
        "the first program compiles the library: {compiled:?}"
    );

    let second = app(&root, "second", SECOND_APP);
    let (printed, cached, compiled) = build_and_run(&second, &cache);
    assert_eq!(printed, "6 3\npear\n1.5\n#13\ntrue\n");
    assert!(
        cached.iter().any(|module| module.starts_with("geom")),
        "the library's objects come from the cache: {cached:?}"
    );
    assert!(
        !compiled.iter().any(|module| module.starts_with("geom")),
        "no library module compiles again: {compiled:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
