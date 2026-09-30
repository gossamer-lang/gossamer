//! `gos fix` - the toolchain's source migrations.
//!
//! A migration is a mechanical upgrade, so the bar is higher than for a
//! lint: it must be deterministic, idempotent, and behaviour-preserving.
//! These tests pin all three, plus the `--check` gate CI runs.

#![allow(missing_docs)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

fn gos_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

/// One directory per case: `gos` bundles sibling `.gos` files.
fn case(name: &str, source: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "gos-fix-{}-{}-{name}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create case dir");
    let file = dir.join(format!("{name}.gos"));
    std::fs::write(&file, source).expect("write source");
    file
}

fn gos(args: &[&str], file: &PathBuf) -> (String, bool) {
    let out = Command::new(gos_bin())
        .args(args)
        .arg(file)
        .output()
        .expect("spawn gos");
    (
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        out.status.success(),
    )
}

/// A well-formed source with no registered migration applying to it.
const UNTOUCHED: &str = "use std::iter\n\nfn dbl(n: i64) -> i64 { n * 2 }\n\nfn main() {\n    let xs = #[1, 2, 3]\n    println(\"{}\", iter::sum(iter::map(xs, dbl)))\n}\n";

#[test]
fn fix_leaves_a_source_no_migration_applies_to() {
    let file = case("untouched", UNTOUCHED);
    let (report, ok) = gos(&["fix"], &file);
    assert!(ok, "{report}");
    assert!(report.contains("0 edit"), "{report}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), UNTOUCHED);

    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[test]
fn check_passes_when_no_migration_is_pending() {
    let file = case("pending", UNTOUCHED);
    let (report, ok) = gos(&["fix", "--check"], &file);
    assert!(ok, "no pending migration must pass --check: {report}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        UNTOUCHED,
        "--check must not write"
    );

    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[test]
fn list_names_every_rewriter() {
    let out = Command::new(gos_bin())
        .args(["fix", "--list"])
        .output()
        .expect("spawn gos fix --list");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(out.status.success(), "{text}");
}

/// An integer receiver's power used as a float, and one with a float exponent.
const INTEGER_POW: &str = "fn main() {\n    let n: i64 = 3\n    let x: f64 = 2.5\n    let a: f64 = n.pow(2)\n    let b = n.pow(2.0)\n    let c = x.pow(2.0)\n    let d: i64 = n.pow(3)\n    println(\"{} {} {} {}\", a, b, c, d)\n}\n";

#[test]
fn integer_pow_migration_repairs_only_the_float_uses() {
    let file = case("integer_pow", INTEGER_POW);
    let (report, ok) = gos(&["fix", "--check"], &file);
    assert!(!ok, "a pending repair fails --check: {report}");
    let (report, ok) = gos(&["fix"], &file);
    assert!(ok, "{report}");
    let fixed = std::fs::read_to_string(&file).unwrap();
    assert!(fixed.contains("let a: f64 = (n as f64).pow(2)"), "{fixed}");
    assert!(fixed.contains("let b = (n as f64).pow(2.0)"), "{fixed}");
    assert!(
        fixed.contains("let c = x.pow(2.0)"),
        "a float receiver is left alone: {fixed}"
    );
    assert!(
        fixed.contains("let d: i64 = n.pow(3)"),
        "an integer power is left alone: {fixed}"
    );
    let (report, ok) = gos(&["run"], &file);
    assert!(ok && report.contains("9 9 6.25 27"), "{report}");
    let (report, ok) = gos(&["fix", "--check"], &file);
    assert!(ok, "a second pass keeps nothing: {report}");

    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

/// The 0.65.0 API changes: a literal `regex::compile` answers the pattern,
/// a runtime pattern and `Pattern::compile` are `new`, hash hex functions
/// take bytes, and a checksum answers `u32`.
const API_0650: &str = r#"use std::regex
use std::crypto::sha256
use std::hash::crc32

fn find(text: String, spec: String) -> Result<bool, regex::Error> {
    let p = regex::compile(spec)?
    Ok(regex::is_match(p, text))
}

fn main() {
    let lit = regex::compile("a+").unwrap()
    let typed = regex::Pattern::compile("b+").unwrap()
    let digest = sha256::hex("abc")
    let sum: i64 = crc32::checksum("abc".as_bytes())
    println("{} {} {} {}", regex::is_match(lit, "aa"), regex::is_match(typed, "bb"), digest.len(), sum)
    println("{}", find("cc", "c+").unwrap())
}
"#;

#[test]
fn api_0650_migrations_repair_regex_hash_and_checksum_uses() {
    let file = case("api_0650", API_0650);
    let (report, ok) = gos(&["fix", "--check"], &file);
    assert!(!ok, "a pending repair fails --check: {report}");
    let (report, ok) = gos(&["fix"], &file);
    assert!(ok, "{report}");
    let fixed = std::fs::read_to_string(&file).unwrap();
    assert!(fixed.contains("let p = regex::new(spec)?"), "{fixed}");
    assert!(
        fixed.contains("let lit = regex::compile(\"a+\")\n"),
        "{fixed}"
    );
    assert!(
        fixed.contains("let typed = regex::Pattern::new(\"b+\").unwrap()"),
        "{fixed}"
    );
    assert!(fixed.contains("sha256::hex(\"abc\".as_bytes())"), "{fixed}");
    assert!(
        fixed.contains("let sum: i64 = (crc32::checksum(\"abc\".as_bytes()) as i64)"),
        "{fixed}"
    );
    let (report, ok) = gos(&["run"], &file);
    assert!(ok && report.contains("true true 64 891568578"), "{report}");
    let (report, ok) = gos(&["fix", "--check"], &file);
    assert!(ok, "a second pass keeps nothing: {report}");

    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}
