//! A file a `comptime` region reads is an input of the build.
//!
//! The bytes a compile-time region embeds are compiled into the
//! artifact, so an artifact built against one version of an embedded
//! file is not current for another. Which files a region reads is
//! decided by running it, which happens after the build key is formed,
//! so the paths are recorded during the fold and re-checked by the
//! next build.

use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn workspace(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("gos-comptime-inputs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("profiles")).expect("create fixture directory");
    dir
}

/// Reads an embedded profile at compile time, so the value it prints
/// is a literal in the artifact rather than a file read at run time.
const SOURCE: &str = r#"use std::fs

comptime fn embed(name: String) -> String {
    fs::read_to_string(format("profiles/{}.toml", name)).unwrap_or("missing")
}

fn main() {
    println("{}", embed("standard").trim())
}
"#;

struct Built {
    report: String,
    printed: String,
}

fn build_and_run(dir: &Path) -> Built {
    let build = Command::new(gos_bin())
        .current_dir(dir)
        .args(["build", "app.gos"])
        .output()
        .expect("gos build");
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(build.status.success(), "{report}");
    let binary =
        dir.join("target")
            .join("debug")
            .join(if cfg!(windows) { "app.exe" } else { "app" });
    let run = Command::new(&binary)
        .current_dir(dir)
        .output()
        .expect("run the built artifact");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    Built {
        report,
        printed: String::from_utf8_lossy(&run.stdout).trim().to_string(),
    }
}

#[test]
fn editing_a_file_a_comptime_region_read_rebuilds_the_artifact() {
    let dir = workspace("edited");
    std::fs::write(dir.join("app.gos"), SOURCE).expect("write source");
    let profile = dir.join("profiles").join("standard.toml");
    std::fs::write(&profile, "level = \"one\"\n").expect("write profile");

    let first = build_and_run(&dir);
    assert_eq!(first.printed, "level = \"one\"");

    // Nothing changed: the artifact is still current and the build
    // says so rather than relinking.
    let unchanged = build_and_run(&dir);
    assert!(
        unchanged.report.contains("unchanged"),
        "an untouched build must still hit the stamp: {}",
        unchanged.report
    );

    std::fs::write(&profile, "level = \"two\"\n").expect("edit profile");
    let second = build_and_run(&dir);
    assert!(
        !second.report.contains("unchanged"),
        "an edited embedded file must invalidate the artifact: {}",
        second.report
    );
    assert_eq!(
        second.printed, "level = \"two\"",
        "the rebuilt artifact must carry the edited bytes"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A listing is an input too: which files a directory holds decides
/// what the region compiled in.
#[test]
fn adding_a_file_a_comptime_listing_counted_rebuilds_the_artifact() {
    let dir = workspace("listing");
    std::fs::write(
        dir.join("app.gos"),
        r#"use std::fs

comptime fn profile_count() -> i64 {
    match fs::read_dir("profiles") {
        Ok(entries) => entries.len(),
        Err(_) => -1,
    }
}

fn main() {
    println("{}", profile_count())
}
"#,
    )
    .expect("write source");
    std::fs::write(dir.join("profiles").join("one.toml"), "a = 1\n").expect("write profile");

    let first = build_and_run(&dir);
    assert_eq!(first.printed, "1");

    std::fs::write(dir.join("profiles").join("two.toml"), "b = 2\n").expect("add profile");
    let second = build_and_run(&dir);
    assert_eq!(
        second.printed, "2",
        "a directory a comptime region listed is an input of the build: {}",
        second.report
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A check's verdict depends on what a comptime region read, so a warm
/// `gos check` and `gos run` both observe an edited input.
#[test]
fn editing_a_file_a_comptime_region_read_rechecks_and_reruns_the_unit() {
    let dir = workspace("verdict");
    std::fs::write(
        dir.join("app.gos"),
        r#"use std::fs

comptime fn level() -> String {
    let text = fs::read_to_string("profiles/standard.toml").unwrap_or("missing")
    if text.trim() == "broken" {
        panic("the profile is broken")
    }
    text.trim()
}

fn main() {
    println("{}", level())
}
"#,
    )
    .expect("write source");
    let profile = dir.join("profiles").join("standard.toml");
    std::fs::write(&profile, "one\n").expect("write profile");
    let gos = |command: &str| {
        Command::new(gos_bin())
            .current_dir(&dir)
            .args([command, "app.gos"])
            .env_remove("GOS_NO_CACHE")
            .output()
            .expect("spawn gos")
    };
    assert!(gos("check").status.success());
    assert!(gos("check").status.success());
    assert_eq!(String::from_utf8_lossy(&gos("run").stdout).trim(), "one");

    std::fs::write(&profile, "two\n").expect("edit profile");
    assert_eq!(
        String::from_utf8_lossy(&gos("run").stdout).trim(),
        "two",
        "a warm run must fold the edited bytes"
    );

    std::fs::write(&profile, "broken\n").expect("break profile");
    let broken = gos("check");
    assert!(
        !broken.status.success(),
        "a warm check must fold the edited bytes: {}",
        String::from_utf8_lossy(&broken.stdout)
    );
    assert!(String::from_utf8_lossy(&broken.stderr).contains("the profile is broken"));
    let _ = std::fs::remove_dir_all(&dir);
}
