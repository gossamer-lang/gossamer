//! What Cargo builds a `[rust-bindings]` archive from is an input of the
//! program that links it: a file the binding reads from outside its own
//! crate, and the Cargo configuration its build directory sees, each relink
//! the program when they change.

use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

struct Built {
    report: String,
    printed: String,
}

fn build_and_run(app: &Path, cache: &Path) -> Built {
    let build = Command::new(gos_bin())
        .args(["build", "."])
        .current_dir(app)
        .env("GOSSAMER_CACHE", cache)
        .env("GOSSAMER_CACHE_DIR", cache)
        .env_remove("PROBE_SUFFIX")
        .output()
        .expect("gos build");
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(build.status.success(), "{report}");
    let binary =
        app.join("target")
            .join("debug")
            .join(if cfg!(windows) { "app.exe" } else { "app" });
    let run = Command::new(&binary).output().expect("run the artifact");
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
fn what_cargo_builds_the_bindings_from_relinks_the_program() {
    let dir = std::env::temp_dir().join(format!("gos-binding-inputs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let app = dir.join("app");
    let cache = dir.join("cache");
    std::fs::create_dir_all(app.join("probe").join("src")).expect("create binding crate");
    std::fs::create_dir_all(app.join("src")).expect("create source directory");
    std::fs::create_dir_all(dir.join("outside")).expect("create outside directory");
    std::fs::write(
        app.join("project.toml"),
        "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n\n\
         [rust-bindings]\nprobe = { path = \"probe\" }\n",
    )
    .expect("write project manifest");
    let binding_version = gossamer_binding_abi::SDK_VERSION;
    std::fs::write(
        app.join("probe").join("Cargo.toml"),
        format!(
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             publish = false\n\n[workspace]\n\n[lib]\ncrate-type = [\"rlib\"]\n\n\
             [dependencies]\ngossamer-binding = \"{binding_version}\"\n"
        ),
    )
    .expect("write binding manifest");
    // The answer lives outside the binding's crate, so only Cargo's record
    // of what it read ties the archive to it.
    std::fs::write(
        app.join("probe").join("src").join("lib.rs"),
        r#"use gossamer_binding::register_module;

register_module!(
    name: probe,
    doc: "Reports what its archive was built from.",
    fn answer() -> String {
        let suffix = option_env!("PROBE_SUFFIX").unwrap_or("");
        format!("{}{suffix}", include_str!("../../../outside/answer.txt").trim())
    }
);
"#,
    )
    .expect("write binding source");
    std::fs::write(
        app.join("src").join("main.gos"),
        "use probe::answer\n\nfn main() {\n    println(answer())\n}\n",
    )
    .expect("write main");
    let answer = dir.join("outside").join("answer.txt");
    std::fs::write(&answer, "first\n").expect("write answer");

    let first = build_and_run(&app, &cache);
    assert_eq!(first.printed, "first", "{}", first.report);

    let again = build_and_run(&app, &cache);
    assert!(
        again.report.contains("unchanged"),
        "an untouched build must still hit the stamp: {}",
        again.report
    );

    std::fs::write(&answer, "second\n").expect("edit answer");
    let edited = build_and_run(&app, &cache);
    assert!(
        !edited.report.contains("unchanged"),
        "a file the bindings archive was built from must relink the program: {}",
        edited.report
    );
    assert_eq!(edited.printed, "second", "{}", edited.report);

    let config = cache.join("gossamer").join("bindings").join(".cargo");
    std::fs::create_dir_all(&config).expect("create cargo config directory");
    std::fs::write(
        config.join("config.toml"),
        "[env]\nPROBE_SUFFIX = \"+configured\"\n",
    )
    .expect("write cargo config");
    let configured = build_and_run(&app, &cache);
    assert!(
        !configured.report.contains("unchanged"),
        "the Cargo configuration the bindings build reads must relink the program: {}",
        configured.report
    );
    assert_eq!(
        configured.printed, "second+configured",
        "{}",
        configured.report
    );

    let settled = build_and_run(&app, &cache);
    assert!(
        settled.report.contains("unchanged"),
        "a configured build must hit its stamp once built: {}",
        settled.report
    );
    let _ = std::fs::remove_dir_all(&dir);
}
