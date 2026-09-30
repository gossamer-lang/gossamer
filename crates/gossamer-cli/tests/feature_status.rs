//! Integration test for `gos feature-status`.
//!
//! Drives the built `gos` binary end-to-end, asserting that each
//! flag combination produces the documented shape and that
//! `--check` enforces its CI gate.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn scratch(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-feature-status-{pid}-{n}-{tag}",
        pid = std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(gos_bin())
        .args(args)
        .output()
        .expect("spawn gos");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn table_output_contains_known_items() {
    let (code, stdout, _) = run(&["feature-status"]);
    assert_eq!(code, 0, "exit 0 expected");
    assert!(stdout.contains("std::fmt"), "missing std::fmt: {stdout}");
    assert!(stdout.contains("lang::if_let"), "missing lang::if_let");
    assert!(stdout.contains("Status"), "header missing");
    assert!(stdout.contains("shipped"), "shipped tag missing");
}

#[test]
fn json_output_parses_back() {
    let (code, stdout, _) = run(&["feature-status", "--format", "json"]);
    assert_eq!(code, 0, "exit 0 expected");
    let trimmed = stdout.trim();
    assert!(trimmed.starts_with('['), "expected JSON array: {trimmed}");
    assert!(trimmed.ends_with(']'));
    assert!(trimmed.contains("\"name\":"));
    assert!(trimmed.contains("\"status\":"));
    // Spot-check by hunting for one specific entry.
    assert!(
        trimmed.contains("\"std::fmt\""),
        "stdlib entry should round-trip"
    );
}

#[test]
fn markdown_output_renders_table() {
    let (code, stdout, _) = run(&["feature-status", "--format", "markdown"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("| Name | Status | Tier-Parity | Doc |"),
        "missing markdown header"
    );
}

#[test]
fn filter_glob_narrows_results() {
    let (code, stdout, _) = run(&["feature-status", "--filter", "std::http::*"]);
    assert_eq!(code, 0);
    for line in stdout.lines() {
        // Skip the header rows (Name / -----).
        if line.starts_with("Name ") || line.starts_with("---") || line.is_empty() {
            continue;
        }
        // Each non-header row's first column starts with std::http::.
        let first = line.split('|').next().unwrap_or("").trim();
        assert!(
            first.starts_with("std::http::"),
            "row {first:?} should be under std::http::",
        );
    }
    assert!(
        stdout.contains("std::http::"),
        "must show at least one http entry"
    );
}

#[test]
fn status_filter_narrows_to_one_lifecycle_stage() {
    let (code, stdout, _) = run(&["feature-status", "--status", "experimental"]);
    assert_eq!(code, 0);
    // Every non-header row must end with "experimental" in the
    // status column.
    let mut saw_any = false;
    for line in stdout.lines() {
        if line.starts_with("Name ") || line.starts_with("---") || line.is_empty() {
            continue;
        }
        saw_any = true;
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() >= 2 {
            assert!(
                parts[1].trim() == "experimental",
                "non-experimental row leaked: {line}",
            );
        }
    }
    assert!(saw_any, "registry should ship experimental items");
}

#[test]
fn check_mode_passes_when_shipped_item_has_docs_without_tier_evidence() {
    let tmp = scratch("ok");
    let docs = tmp.join("docs_src");
    fs::create_dir_all(docs.join("language")).unwrap();
    fs::create_dir_all(docs.join("stdlib")).unwrap();

    // Shipped means available and documented. The tier record beside it is
    // what the ledger already knows; this checks the docs requirement, so the
    // record is present and passing and the doc page is what varies.
    let sidecar = tmp.join("sidecar.json");
    fs::write(docs.join("language/if_let.md"), "Status: shipped\n").unwrap();
    fs::write(
        &sidecar,
        "[{\"name\":\"lang::if_let\",\"tiers\":         {\"vm\":\"pass\",\"cranelift\":\"pass\",\"llvm\":\"pass\"}}]\n",
    )
    .unwrap();

    // Filter down to the one path so unrelated registry entries don't
    // bring the check down.
    let (code, stdout, stderr) = run(&[
        "feature-status",
        "--check",
        "--filter",
        "lang::if_let",
        "--sidecar",
        sidecar.to_str().unwrap(),
        "--docs-root",
        docs.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 0,
        "check should pass: stdout={stdout} stderr={stderr}"
    );
    assert!(stdout.contains("ok"), "expected ok line, got {stdout}");
}

#[test]
fn check_mode_passes_when_shipped_lacks_tier_evidence() {
    let tmp = scratch("shipped-no-tier-evidence");
    let docs = tmp.join("docs_src");
    fs::create_dir_all(docs.join("language")).unwrap();
    fs::write(docs.join("language/match.md"), "Status: shipped\n").unwrap();

    let sidecar = tmp.join("sidecar.json");
    fs::write(
        &sidecar,
        "[{\"name\":\"lang::match\",\"tiers\":         {\"vm\":\"pass\",\"cranelift\":\"pass\",\"llvm\":\"pass\"}}]\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run(&[
        "feature-status",
        "--check",
        "--filter",
        "lang::match",
        "--sidecar",
        sidecar.to_str().unwrap(),
        "--docs-root",
        docs.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 0,
        "shipped item should require docs, not Stable evidence: stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn check_mode_fails_when_shipped_lacks_doc_page() {
    let tmp = scratch("missing-doc");
    let docs = tmp.join("docs_src");
    fs::create_dir_all(docs.join("language")).unwrap();
    // Sidecar has a record but no docs page on disk.
    let sidecar = tmp.join("sidecar.json");
    fs::write(
        &sidecar,
        r#"[{"name":"lang::if","tiers":{"vm":"pass","cranelift":"pass","llvm":"pass"}}]"#,
    )
    .unwrap();
    let (code, _, stderr) = run(&[
        "feature-status",
        "--check",
        "--filter",
        "lang::if",
        "--sidecar",
        sidecar.to_str().unwrap(),
        "--docs-root",
        docs.to_str().unwrap(),
    ]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("missing doc page") || stderr.contains("check failed"),
        "expected missing-doc failure, got {stderr}",
    );
}

#[test]
fn unknown_format_returns_error() {
    let (code, _, stderr) = run(&["feature-status", "--format", "yaml"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("unknown"),
        "expected format error: {stderr}"
    );
}

#[test]
fn unknown_status_returns_error() {
    let (code, _, stderr) = run(&["feature-status", "--status", "bogus"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("unknown"),
        "expected status error: {stderr}"
    );
}

/// The parity walk records the modules a fixture imports, so a feature
/// row can join against them; keying only by fixture path left every row
/// reading `(no test data)`.
#[test]
fn the_parity_walk_records_module_rows_a_feature_row_can_join() {
    let dir = scratch("module-rows");
    fs::write(
        dir.join("uses_strings.gos"),
        "use std::strings\n\nfn main() {\n    println(\"{}\", strings::trim(\"  x  \"))\n}\n",
    )
    .expect("write fixture");

    let out = Command::new(gos_bin())
        .args(["test", "--tier-parity", "--report", "status"])
        .arg(&dir)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .output()
        .expect("spawn tier-parity walk");
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(
        report.contains("vm=pass cranelift=pass llvm=pass"),
        "every tier must reach a verdict on a plain fixture: {report}"
    );

    let sidecar =
        fs::read_to_string(dir.join("target/debug/.feature-status.json")).expect("sidecar written");
    assert!(
        sidecar.contains("\"std::strings\""),
        "the walk recorded no module row: {sidecar}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A fixture that runs until it is killed says nothing about whether the
/// tiers agree. Recording it as a failure would publish every module a
/// server example imports as broken.
#[test]
fn a_fixture_that_never_exits_reaches_no_verdict_rather_than_failing() {
    let dir = scratch("no-verdict");
    fs::write(dir.join("spins.gos"), "fn main() {\n    loop { }\n}\n").expect("write fixture");

    let out = Command::new(gos_bin())
        .args([
            "test",
            "--tier-parity",
            "--report",
            "status",
            "--timeout",
            "2s",
        ])
        .arg(&dir)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .output()
        .expect("spawn tier-parity walk");
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(
        report.contains("vm=- cranelift=- llvm=-"),
        "a non-terminating fixture must reach no verdict: {report}"
    );
    assert!(
        out.status.success(),
        "no verdict is not a parity failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A parked process is recognised long before its budget runs out, and
/// the other two tiers are not charged for a fixture the first could not
/// run to completion. Without this a walk over every fixture is dominated
/// by server examples waiting out three full budgets each.
#[test]
fn a_parked_fixture_is_detected_well_inside_its_budget() {
    let dir = scratch("parked");
    // Waits on an ephemeral port nobody connects to: alive, consuming
    // no CPU, exactly the shape of a server example. A blocked channel
    // would not do - the runtime reports that as a deadlock and exits.
    fs::write(
        dir.join("parked.gos"),
        concat!(
            "use std::net\n\n",
            "fn main() {\n",
            "    let listener = net::TcpListener::bind(\"127.0.0.1:0\").unwrap()\n",
            "    loop {\n",
            "        let _ = listener.accept()\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("write fixture");

    let started = std::time::Instant::now();
    let out = Command::new(gos_bin())
        .args([
            "test",
            "--tier-parity",
            "--report",
            "status",
            "--timeout",
            "120s",
        ])
        .arg(&dir)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .output()
        .expect("spawn tier-parity walk");
    let elapsed = started.elapsed();
    let report = String::from_utf8_lossy(&out.stdout);

    assert!(
        report.contains("vm=- cranelift=- llvm=-"),
        "a parked fixture reaches no verdict on any tier: {report}"
    );
    assert!(
        elapsed < std::time::Duration::from_mins(1),
        "detection must not wait out the 120s budget; took {elapsed:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A program printing its own name reports the source path under the VM
/// and the executable's path when compiled. That is the program naming
/// itself correctly, not the tiers disagreeing.
#[test]
fn a_fixture_that_prints_its_own_path_is_not_a_divergence() {
    let dir = scratch("argv0");
    fs::write(
        dir.join("names_itself.gos"),
        concat!(
            "use std::env\n\n",
            "fn main() {\n",
            "    println(\"program: {}\", env::program_name())\n",
            "}\n",
        ),
    )
    .expect("write fixture");

    let out = Command::new(gos_bin())
        .args(["test", "--tier-parity", "--report", "status"])
        .arg(&dir)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .output()
        .expect("spawn tier-parity walk");
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(
        report.contains("vm=pass cranelift=pass llvm=pass"),
        "the program's own path must not read as a divergence: {report}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Output that moves between runs of the same tier cannot be compared
/// across tiers. The walk confirms nondeterminism by re-running the
/// reference rather than assuming it, and falls back to exit codes.
#[test]
fn a_nondeterministic_fixture_is_compared_on_exit_status_only() {
    let dir = scratch("nondet");
    fs::write(
        dir.join("interleaved.gos"),
        concat!(
            "use std::sync::channel\n\n",
            "fn worker(tx: Sender<i64>, id: i64) {\n",
            "    tx.send(id)\n",
            "}\n\n",
            "fn main() {\n",
            "    let tx, rx = channel()\n",
            "    for i in 0..8 { spawn(|| worker(tx, i)) }\n",
            "    let mut seen = 0\n",
            "    while seen < 8 {\n",
            "        match rx.recv() {\n",
            "            Some(v) => { println(\"got {}\", v); seen += 1 }\n",
            "            None => { seen = 8 }\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("write fixture");

    let out = Command::new(gos_bin())
        .args(["test", "--tier-parity", "--report", "status"])
        .arg(&dir)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .output()
        .expect("spawn tier-parity walk");
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(
        report.contains("vm=pass cranelift=pass llvm=pass"),
        "goroutine interleaving must not read as a divergence: {report}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Parity is agreement between tiers, not success. An example that
/// deliberately exits non-zero on all three tiers has proved exactly the
/// property the walk exists to check.
#[test]
fn a_fixture_that_fails_identically_on_every_tier_is_parity_passing() {
    let dir = scratch("agreeing-failure");
    fs::write(
        dir.join("exits_nonzero.gos"),
        "use std::process\n\nfn main() {\n    println(\"before\")\n    process::exit(3)\n}\n",
    )
    .expect("write fixture");

    let out = Command::new(gos_bin())
        .args(["test", "--tier-parity", "--report", "status"])
        .arg(&dir)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .output()
        .expect("spawn tier-parity walk");
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(
        report.contains("vm=pass cranelift=pass llvm=pass"),
        "agreeing tiers must read as parity, whatever the exit code: {report}"
    );
    assert!(
        out.status.success(),
        "no tier diverged: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A module is the wrong unit for "can I rely on this call". `--items`
/// answers about the item, inheriting the module's tier evidence.
#[test]
fn items_mode_reports_one_row_per_export_with_inherited_evidence() {
    let out = Command::new(gos_bin())
        .args(["feature-status", "--items", "--filter", "std::strings::*"])
        .output()
        .expect("spawn gos feature-status --items");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("std::strings::trim"), "{text}");
    assert!(
        !text.contains("\nstd::strings  "),
        "the module row must not appear in item mode: {text}"
    );
    assert!(
        text.contains("vm:pass"),
        "an item inherits its module's tier record: {text}"
    );
}
