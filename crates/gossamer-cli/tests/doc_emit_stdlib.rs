//! Tests for `gos doc --emit-stdlib` and `--check`.

use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // deps/
    p.pop(); // debug/
    p.push("gos");
    p
}

fn tmp(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let pid = std::process::id();
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let root = std::env::temp_dir().join(format!(
        "gos-doc-{tag}-{pid}-{n}-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    // `gos doc --emit-stdlib DIR` places language pages beside DIR. Give each
    // test its own parent so parallel test threads never share `/tmp/language`.
    let p = root.join("stdlib");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn cleanup(dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(dir.parent().expect("test output has parent"));
}

#[test]
fn emit_stdlib_writes_one_page_per_module_plus_index() {
    let dir = tmp("emit");
    let out = Command::new(gos_bin())
        .args(["doc", "--emit-stdlib"])
        .arg(&dir)
        .output()
        .expect("spawn gos doc");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("stdlib pages to"));
    assert!(dir.join("index.md").exists());
    // Every module the manifest declares, not a sample of them. A module
    // that reaches the language but not the reference is invisible to
    // anyone reading the docs, and the gap is silent: the module works,
    // so nothing else fails.
    let index = std::fs::read_to_string(dir.join("index.md")).unwrap();
    let missing: Vec<String> = documented_modules()
        .into_iter()
        .filter(|module| !dir.join(format!("{}.md", page_slug(module))).exists())
        .collect();
    assert!(
        missing.is_empty(),
        "manifest modules with no emitted page: {missing:?}"
    );
    assert!(index.contains("std::http"));
    cleanup(&dir);
}

/// The page name `--emit-stdlib` writes for a module path.
fn page_slug(module: &str) -> String {
    module
        .strip_prefix("std::")
        .unwrap_or(module)
        .replace("::", "_")
}

/// Every module path `gos doc std` lists.
fn documented_modules() -> Vec<String> {
    let out = Command::new(gos_bin())
        .args(["doc", "std"])
        .output()
        .expect("spawn gos doc std");
    assert!(out.status.success(), "gos doc std failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("- "))
        .filter_map(|line| line.split(" - ").next())
        .filter(|path| path.starts_with("std::"))
        .map(str::to_string)
        .collect()
}

/// A module a program may import has a page in the reference.
///
/// The two lists are maintained by hand and drift apart silently in one
/// direction: an export added to the resolver works the day it lands, so
/// nothing fails when the manifest entry behind its page is forgotten.
#[test]
fn every_importable_module_reaches_the_reference() {
    let undocumented: Vec<&str> = gossamer_resolve::STDLIB_MODULE_PATHS
        .iter()
        .copied()
        .filter(|path| {
            let full = format!("std::{path}");
            let out = Command::new(gos_bin())
                .args(["doc", &full])
                .output()
                .expect("spawn gos doc");
            !out.status.success()
        })
        .collect();
    assert!(
        undocumented.is_empty(),
        "importable modules missing from the stdlib reference: {undocumented:?}"
    );
}

#[test]
fn emit_stdlib_check_passes_on_fresh_emit() {
    let dir = tmp("check_pass");
    // Emit, then verify with --check.
    let emit = Command::new(gos_bin())
        .args(["doc", "--emit-stdlib"])
        .arg(&dir)
        .output()
        .expect("emit");
    assert!(emit.status.success());

    let check = Command::new(gos_bin())
        .args(["doc", "--emit-stdlib"])
        .arg(&dir)
        .arg("--check")
        .output()
        .expect("check");
    assert!(
        check.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(String::from_utf8_lossy(&check.stdout).contains("in sync"));
    cleanup(&dir);
}

#[test]
fn emit_stdlib_check_fails_on_drift() {
    let dir = tmp("check_fail");
    // Emit, then mutate one page to force drift.
    Command::new(gos_bin())
        .args(["doc", "--emit-stdlib"])
        .arg(&dir)
        .output()
        .expect("emit");
    let mutated_page = dir.join("io.md");
    std::fs::write(&mutated_page, "garbage content").unwrap();

    let check = Command::new(gos_bin())
        .args(["doc", "--emit-stdlib"])
        .arg(&dir)
        .arg("--check")
        .output()
        .expect("check");
    assert!(!check.status.success(), "check must fail on drift");
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert!(stderr.contains("drift") || stderr.contains("io.md"));
    cleanup(&dir);
}

#[test]
fn emit_stdlib_preserves_handwritten_tail_and_check_ignores_it() {
    let dir = tmp("handwritten");
    Command::new(gos_bin())
        .args(["doc", "--emit-stdlib"])
        .arg(&dir)
        .output()
        .expect("emit");
    let page = dir.join("io.md");
    let head = std::fs::read_to_string(&page).unwrap();
    let marker = "<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->";
    std::fs::write(
        &page,
        format!("{head}{marker}\n\n## Notes\n\nHandwritten prose.\n"),
    )
    .unwrap();

    // The drift check compares only the generated head.
    let check = Command::new(gos_bin())
        .args(["doc", "--emit-stdlib"])
        .arg(&dir)
        .arg("--check")
        .output()
        .expect("check");
    assert!(
        check.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&check.stderr)
    );

    // Re-emitting rewrites the head and keeps the handwritten tail.
    Command::new(gos_bin())
        .args(["doc", "--emit-stdlib"])
        .arg(&dir)
        .output()
        .expect("re-emit");
    let after = std::fs::read_to_string(&page).unwrap();
    assert!(after.starts_with(&head), "generated head must be rewritten");
    assert!(
        after.contains("Handwritten prose."),
        "handwritten tail must survive re-emit"
    );
    cleanup(&dir);
}

/// The `GR0005` and `GR0009` diagnostics tell the reader to run
/// `gos doc std` / `gos doc std::<module>`; both must resolve
/// against the stdlib manifest rather than the filesystem.
#[test]
fn doc_std_lists_every_module() {
    let out = Command::new(gos_bin())
        .args(["doc", "std"])
        .output()
        .expect("spawn gos doc std");
    assert!(out.status.success(), "gos doc std failed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("std::strings"), "{text}");
    assert!(text.contains("std::encoding::json"), "{text}");
}

#[test]
fn doc_std_module_lists_its_exports() {
    let out = Command::new(gos_bin())
        .args(["doc", "std::strings"])
        .output()
        .expect("spawn gos doc std::strings");
    assert!(out.status.success(), "gos doc std::strings failed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("std::strings::trim"), "{text}");
}

#[test]
fn doc_std_group_lists_the_modules_under_it() {
    let out = Command::new(gos_bin())
        .args(["doc", "std::crypto"])
        .output()
        .expect("spawn gos doc std::crypto");
    assert!(out.status.success(), "gos doc std::crypto failed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("- std::crypto::subtle - "), "{text}");
    assert!(text.contains("- std::crypto::sha256 - "), "{text}");
}

#[test]
fn doc_std_item_prints_its_documentation() {
    let out = Command::new(gos_bin())
        .args(["doc", "std::strings::trim"])
        .output()
        .expect("spawn gos doc std::strings::trim");
    assert!(out.status.success(), "gos doc std::strings::trim failed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("fn std::strings::trim"), "{text}");
}

#[test]
fn doc_unknown_stdlib_path_errors() {
    let out = Command::new(gos_bin())
        .args(["doc", "std::not_a_module"])
        .output()
        .expect("spawn gos doc std::not_a_module");
    assert!(!out.status.success());
}

#[test]
fn doc_without_file_or_emit_stdlib_errors() {
    let out = Command::new(gos_bin())
        .args(["doc"])
        .output()
        .expect("spawn gos doc");
    assert!(!out.status.success());
}
