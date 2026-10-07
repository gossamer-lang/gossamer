//! `gos check --fix` applies the rewrites the diagnostics carry.
//!
//! A suggestion is a guess - the diagnostic carries no applicability
//! marker separating a certain rewrite from a speculative one - so the
//! command keeps an edit only when a re-check proves the file got
//! better. These tests pin both directions: a correct suggestion lands,
//! a wrong one leaves the source alone.

#![allow(missing_docs)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

fn gos_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

/// One directory per case: `gos check` bundles every sibling `.gos`, so
/// two cases sharing a directory would check each other's source.
fn case(name: &str, source: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "gos-check-fix-{}-{}-{name}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create case dir");
    let file = dir.join(format!("{name}.gos"));
    std::fs::write(&file, source).expect("write case source");
    file
}

fn run_fix(file: &PathBuf) -> String {
    let out = Command::new(gos_bin())
        .args(["check", "--fix"])
        .arg(file)
        .output()
        .expect("spawn gos check --fix");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_resolvable_name_suggestion_is_applied() {
    let file = case(
        "typo",
        "fn main() {\n    let total = 3\n    println(\"{}\", totl)\n}\n",
    );
    let report = run_fix(&file);
    assert!(report.contains("fix: 1 edit"), "report was: {report}");
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(after.contains("println(\"{}\", total)"), "after: {after}");
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

/// `gos check` bundles every sibling module into one unit, and a
/// suggestion raised in a sibling carries offsets into that unit. The
/// edit has to land in the sibling's own file at its own offsets - and
/// never in the entry, whose text those offsets do not address.
#[test]
fn a_suggestion_in_a_sibling_module_is_applied_to_that_module() {
    let entry = case(
        "sibling-entry",
        "use helper::total\n\nfn main() {\n    println(\"{}\", total())\n}\n",
    );
    let dir = entry.parent().unwrap();
    // A sibling `.gos` is a module of a project, so the manifest is what
    // makes `helper` reachable from the entry at all.
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/sibling\"\nversion = \"0.1.0\"\nentry = \"sibling-entry.gos\"\n",
    )
    .expect("write manifest");
    let helper = dir.join("helper.gos");
    let before = "// A helper module, long enough that its offsets overrun the entry.\n\n\
                  pub fn total() -> i64 {\n    let value = 3\n    valu\n}\n";
    std::fs::write(&helper, before).expect("write helper");
    let entry_before = std::fs::read_to_string(&entry).unwrap();

    let report = run_fix(&entry);
    assert!(report.contains("fix: 1 edit"), "report was: {report}");
    let after = std::fs::read_to_string(&helper).unwrap();
    assert!(after.contains("    value\n}"), "helper after: {after}");
    assert_eq!(
        std::fs::read_to_string(&entry).unwrap(),
        entry_before,
        "the entry carries no suggestion and must not change"
    );
    let _ = std::fs::remove_dir_all(entry.parent().unwrap());
}

#[test]
fn a_suggestion_that_does_not_resolve_the_error_is_refused() {
    // `coun` draws a did-you-mean pointing at `count`, a `String`;
    // substituting it trades the unresolved name for a type error, so
    // the source must survive untouched.
    let before = "fn main() {\n    let count = \"text\"\n    println(\"{}\", count)\n    \
                  let total: i64 = coun + 1\n    println(\"{}\", total)\n}\n";
    let file = case("wrong_suggestion", before);
    let report = run_fix(&file);
    assert!(report.contains("fix: 0 edit"), "report was: {report}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        before,
        "a refused suggestion must leave the file byte-identical"
    );
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[test]
fn lint_fixes_are_applied_alongside_diagnostic_suggestions() {
    let file = case(
        "lints",
        "fn main() {\n    let unused = 1\n    let x = !!true\n    println(\"{}\", x)\n}\n",
    );
    let report = run_fix(&file);
    assert!(report.contains("fix: 2 edit"), "report was: {report}");
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(after.contains("let _unused = 1"), "after: {after}");
    assert!(after.contains("let x = true"), "after: {after}");
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[test]
fn a_clean_file_is_left_untouched() {
    let before = "fn main() {\n    println(\"{}\", 42)\n}\n";
    let file = case("clean", before);
    let report = run_fix(&file);
    assert!(report.contains("fix: 0 edit"), "report was: {report}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

/// A source that does not parse reports nothing past its parse errors, so
/// the type error the repair unmasks was already there and the repair lands.
#[test]
fn a_parse_repair_lands_although_it_unmasks_a_type_error() {
    let file = case(
        "unmasks",
        "fn main() {\n    let x = 1\n    println!(\"{}\", x)\n    let y: i64 = \"s\"\n}\n",
    );
    let report = run_fix(&file);
    assert!(
        report.contains("fix: 2 edit"),
        "the `!` and the unused `y`: {report}"
    );
    assert!(
        report.contains("GT0001"),
        "the unmasked error is reported: {report}"
    );
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(after.contains("    println(\"{}\", x)\n"), "after: {after}");
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

/// The verdict is the rewritten source's: a run whose edits leave the file
/// clean succeeds, here rewriting the retired `go expr` into `spawn`.
#[test]
fn a_fix_that_leaves_the_file_clean_succeeds() {
    let file = case(
        "go_statement",
        "fn work() {\n    println(\"ran\")\n}\n\nfn main() {\n    go work()\n    go || { work() }\n}\n",
    );
    let out = Command::new(gos_bin())
        .args(["check", "--fix"])
        .arg(&file)
        .output()
        .expect("spawn gos check --fix");
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "report was: {report}");
    assert!(report.contains("check: ok"), "report was: {report}");
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(after.contains("    spawn(|| work())\n"), "after: {after}");
    assert!(
        after.contains("    spawn(|| { work() })\n"),
        "after: {after}"
    );
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[test]
fn a_line_leading_operator_moves_to_the_end_of_the_line_above() {
    let file = case(
        "leading-operator",
        "fn main() {\n    let a = 2\n    let ok = a > 1 // first\n        && a < 5\n    let sum = a\n        + 1\n    println(\"{} {}\", ok, sum)\n}\n",
    );
    let report = run_fix(&file);
    assert!(report.contains("fix: 2 edit"), "report was: {report}");
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(
        after.contains("let ok = a > 1 && // first\n        a < 5\n"),
        "after: {after}"
    );
    assert!(
        after.contains("let sum = a +\n        1\n"),
        "after: {after}"
    );
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}
