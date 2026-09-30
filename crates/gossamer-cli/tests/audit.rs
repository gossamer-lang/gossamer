//! `gos audit` - advisories filtered by what the project can reach.
//!
//! The filter is the point. An advisory naming an item the project never
//! references is not actionable, and a report full of those teaches a
//! reader to skip the output - which is how a security tool stops being
//! run at all.

#![allow(missing_docs)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

fn gos_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

const FEED: &str = r#"[
  {"id":"GOSA-REACHED","package":"example.com/lib","affected_from":"1.0.0",
   "fixed_in":"1.0.1","affected_items":["lib::parse"],
   "severity":"high","summary":"parse accepts a malformed header"},
  {"id":"GOSA-UNREACHED","package":"example.com/lib","affected_from":"1.0.0",
   "fixed_in":"1.0.1","affected_items":["lib::render"],
   "severity":"medium","summary":"render leaks a path"},
  {"id":"GOSA-OLD","package":"example.com/lib","affected_from":"0.1.0",
   "fixed_in":"0.9.0","affected_items":[],
   "severity":"critical","summary":"fixed long before this version"}
]"#;

/// A project pinning `example.com/lib@1.0.0` and importing `lib::parse`.
fn project(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "gos-audit-{}-{}-{name}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("create project");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/audited\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("project.lock"),
        "# gossamer project.lock v1\n[[project]]\nid = \"example.com/lib\"\nsource = \"registry\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("advisories.json"), FEED).unwrap();
    std::fs::write(
        dir.join("src/main.gos"),
        "use lib::parse\n\nfn main() {\n    println(\"{}\", 1)\n}\n",
    )
    .unwrap();
    dir
}

fn audit(dir: &PathBuf, args: &[&str]) -> (String, bool) {
    let out = Command::new(gos_bin())
        .arg("audit")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn gos audit");
    (
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        out.status.success(),
    )
}

#[test]
fn only_a_reachable_advisory_is_reported() {
    let dir = project("reach");
    let (report, ok) = audit(&dir, &[]);

    assert!(!ok, "a reachable advisory must fail the audit: {report}");
    assert!(report.contains("GOSA-REACHED"), "{report}");
    assert!(
        !report.contains("advisory[GOSA-UNREACHED]"),
        "an advisory on an item the project never references must not be reported: {report}"
    );
    assert!(
        report.contains("1 advisory(ies) affect a resolved version but name no item"),
        "the suppressed count has to be visible, or the filter is hiding things: {report}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_version_outside_the_range_is_not_reported() {
    let dir = project("range");
    let (report, _) = audit(&dir, &["--all"]);
    assert!(
        !report.contains("GOSA-OLD"),
        "an advisory fixed before the resolved version must not appear: {report}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn all_lifts_the_reachability_filter() {
    let dir = project("all");
    let (report, _) = audit(&dir, &["--all"]);
    assert!(report.contains("GOSA-REACHED"), "{report}");
    assert!(report.contains("GOSA-UNREACHED"), "{report}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn json_output_is_the_shared_diagnostic_schema() {
    let dir = project("json");
    let (report, _) = audit(&dir, &["--format", "json"]);
    let line = report
        .lines()
        .find(|l| l.starts_with('{'))
        .expect("a JSON diagnostic line");
    let parsed = gossamer_std::json::parse(line).expect("valid JSON");
    assert_eq!(
        gossamer_std::json::get(&parsed, "code").and_then(gossamer_std::json::as_str),
        Some("GOSA-REACHED")
    );
    assert!(
        gossamer_std::json::get(&parsed, "suggestions").is_some(),
        "the shared schema carries a suggestions array: {line}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_project_with_no_feed_is_quiet_and_clean() {
    let dir = project("nofeed");
    std::fs::remove_file(dir.join("advisories.json")).unwrap();
    let (report, ok) = audit(&dir, &[]);
    assert!(ok, "no feed is not a failure: {report}");
    assert!(report.contains("no advisory feed"), "{report}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Publishing a package whose dependencies carry a reachable advisory
/// says so, and publishes anyway. Refusing would put the registry's feed
/// in the path of every release, where an entry added in error becomes
/// an outage.
#[test]
fn publish_warns_about_a_reachable_advisory_without_blocking() {
    let dir = project("preflight");
    // A dry run packs and signs as a real publish does, so it needs a
    // registry to name and a key to sign with; it contacts neither.
    let out = Command::new(gos_bin())
        .args(["publish", "--dry-run"])
        .current_dir(&dir)
        .env("GOS_REGISTRY_URL", "http://127.0.0.1:9/")
        .env("GOS_PUBLISH_KEY", "11".repeat(32))
        .output()
        .expect("spawn gos publish");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("warning: advisory[GOSA-REACHED]"),
        "the pre-flight must name the advisory: {text}"
    );
    assert!(
        text.contains("skipping upload"),
        "publishing must continue: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
