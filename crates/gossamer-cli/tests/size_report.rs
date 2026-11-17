//! `gos build --size-report` prints what each part of the executable costs,
//! read from its full symbol table before a stripped build trims it.

use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

#[test]
fn size_report_names_the_program_and_the_runtime() {
    let dir = std::env::temp_dir().join(format!("gos-size-report-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(
        dir.join("hello.gos"),
        "fn shout(s: String) -> String {\n    s.to_uppercase()\n}\n\nfn main() {\n    println(shout(\"hi\"))\n}\n",
    )
    .expect("write program");
    let build = |extra: &[&str]| {
        Command::new(gos_bin())
            .current_dir(&dir)
            .env("GOSSAMER_CACHE_DIR", dir.join("cache"))
            .args(["build", "--release", "hello.gos", "--out-dir", "out"])
            .args(extra)
            .output()
            .expect("spawn gos build")
    };
    assert!(build(&[]).status.success());
    // The artifact is current, and the report still prints.
    let output = build(&["--size-report"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("size report: "), "{stdout}");
    assert!(stdout.contains("  sections:\n"), "{stdout}");
    let owners: Vec<&str> = stdout
        .lines()
        .skip_while(|line| *line != "  code and data by owner:")
        .skip(1)
        .take_while(|line| line.starts_with("    "))
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    if cfg!(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "macos"
    )) {
        for owner in ["program", "gossamer_runtime", "core"] {
            assert!(owners.contains(&owner), "{owner} missing from {stdout}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
