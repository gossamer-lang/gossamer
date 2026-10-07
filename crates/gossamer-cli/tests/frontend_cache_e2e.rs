//! End-to-end behaviour of the front-end cache across `gos` processes.
//!
//! A warm run must skip the front end, and every edit that changes the
//! program - including one in a sibling module the entry imports - must be
//! observed on the next invocation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

struct Project {
    root: PathBuf,
    cache: PathBuf,
}

impl Project {
    fn new(name: &str) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let root = std::env::temp_dir().join(format!(
            "gossamer-frontend-e2e-{name}-{}-{nonce}",
            std::process::id()
        ));
        let cache = root.join("cache");
        fs::create_dir_all(root.join("src")).expect("create project");
        fs::create_dir_all(&cache).expect("create cache dir");
        fs::write(
            root.join("project.toml"),
            "[project]\nid = \"example.com/cachedemo\"\nversion = \"0.1.0\"\nentry = \"src/main.gos\"\n",
        )
        .expect("write manifest");
        fs::write(
            root.join("src").join("util.gos"),
            "pub fn double(x: i64) -> i64 { x * 2 }\n",
        )
        .expect("write module");
        fs::write(
            root.join("src").join("main.gos"),
            "fn main() { println(\"{}\", util::double(21)) }\n",
        )
        .expect("write entry");
        Self { root, cache }
    }

    fn check(&self) -> Output {
        Command::new(gos_bin())
            .arg("check")
            .arg(".")
            .current_dir(&self.root)
            .env("GOSSAMER_CACHE_DIR", &self.cache)
            .env("GOSSAMER_CACHE_TRACE", "1")
            .env_remove("GOS_NO_CACHE")
            .output()
            .expect("spawn gos check")
    }

    fn write(&self, name: &str, body: &str) {
        fs::write(self.root.join("src").join(name), body).expect("write source");
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn restored(out: &Output) -> bool {
    String::from_utf8_lossy(&out.stderr).contains("cache: frontend restored")
}

fn assert_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_second_check_of_an_unchanged_project_restores_the_front_end() {
    let project = Project::new("warm");

    let cold = project.check();
    assert_ok(&cold, "cold check");
    assert!(!restored(&cold), "the first check cannot be a cache hit");

    let warm = project.check();
    assert_ok(&warm, "warm check");
    assert!(restored(&warm), "the second check must restore from cache");
    assert_eq!(
        String::from_utf8_lossy(&cold.stdout),
        String::from_utf8_lossy(&warm.stdout),
        "a cache hit changed the reported result"
    );
    assert!(blob_count(&project.cache) >= 1, "no blob was published");
}

#[test]
fn editing_the_entry_invalidates_the_entry_and_a_fix_clears_it() {
    let project = Project::new("entry-edit");
    assert_ok(&project.check(), "cold check");
    assert!(restored(&project.check()), "warm check");

    project.write("main.gos", "fn main() { let x: bool = util::double(21) }\n");
    let broken = project.check();
    assert!(
        !broken.status.success(),
        "a type error must survive the cache:\n{}",
        String::from_utf8_lossy(&broken.stdout)
    );
    assert!(
        String::from_utf8_lossy(&broken.stderr).contains("GT0001"),
        "expected a type mismatch, got:\n{}",
        String::from_utf8_lossy(&broken.stderr)
    );

    project.write(
        "main.gos",
        "fn main() { println(\"{}\", util::double(21)) }\n",
    );
    let fixed = project.check();
    assert_ok(&fixed, "check after the fix");
    assert!(
        restored(&fixed),
        "restoring the original text must hit the entry written for it"
    );
}

#[test]
fn editing_an_imported_sibling_module_invalidates_the_cache() {
    let project = Project::new("sibling-edit");
    assert_ok(&project.check(), "cold check");
    assert!(restored(&project.check()), "warm check");

    project.write("util.gos", "pub fn double(x: i64) -> bool { x * 2 }\n");
    let broken = project.check();
    assert!(
        !broken.status.success(),
        "an edit to an imported module must invalidate the cache:\n{}",
        String::from_utf8_lossy(&broken.stdout)
    );
}

#[test]
fn gos_no_cache_writes_nothing() {
    let project = Project::new("disabled");
    let out = Command::new(gos_bin())
        .arg("check")
        .arg(".")
        .current_dir(&project.root)
        .env("GOSSAMER_CACHE_DIR", &project.cache)
        .env("GOS_NO_CACHE", "1")
        .output()
        .expect("spawn gos check");
    assert_ok(&out, "check with the cache disabled");
    assert_eq!(blob_count(&project.cache), 0, "a blob was published anyway");
}

fn blob_count(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "bin"))
            .count()
    })
}

#[test]
fn a_warm_check_reports_the_warnings_a_cold_one_did() {
    let project = Project::new("warnings");
    project.write(
        "main.gos",
        "fn main() {\n    let x: u8 = 5\n    match x { 0..=255 => println(\"all\"), _ => println(\"never\") }\n}\n",
    );
    let warnings = |out: &Output| -> Vec<String> {
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter(|line| line.starts_with("warning["))
            .map(str::to_string)
            .collect()
    };
    let cold = project.check();
    assert_ok(&cold, "cold check");
    assert!(!restored(&cold), "the first check cannot be a cache hit");
    let warm = project.check();
    assert_ok(&warm, "warm check");
    assert!(
        restored(&warm),
        "the second check must restore the front end"
    );
    assert!(
        warnings(&cold).iter().any(|line| line.contains("GM0002")),
        "{}",
        String::from_utf8_lossy(&cold.stderr)
    );
    assert_eq!(warnings(&warm), warnings(&cold));
}
