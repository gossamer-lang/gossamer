// End-to-end CLI tests.
// Shells out to the `gos` binary Cargo produces for this crate and
// asserts behaviour for `parse`, `check`, `run`, `build`, plus
// cross-compilation via `--target`.

use std::env;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn gos_bin() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo when running tests.
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn write_fixture(name: &str, source: &str) -> PathBuf {
    let mut path = env::temp_dir();
    path.push(format!("gossamer-cli-{}-{}.gos", name, std::process::id()));
    std::fs::write(&path, source).expect("write fixture");
    path
}

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .join("examples")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn version_flag_prints_package_version() {
    let out = Command::new(gos_bin())
        .arg("--version")
        .output()
        .expect("spawn --version");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("gos"));
}

#[test]
fn gos_run_script_accepts_a_hashbang() {
    let fixture = write_fixture(
        "hashbang",
        "#!/bin/env -S gos run\nfn main() { println(\"hashbang works\") }\n",
    );
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("spawn gos run");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("hashbang works"));
}

#[test]
fn gos_run_executes_existing_files_with_any_extension() {
    for suffix in ["", ".txt"] {
        let fixture = env::temp_dir().join(format!(
            "gossamer-cli-any-extension-{}{}",
            std::process::id(),
            suffix
        ));
        std::fs::write(
            &fixture,
            format!("fn main() {{ println(\"extension:{suffix}\") }}\n"),
        )
        .expect("write fixture");
        let out = Command::new(gos_bin())
            .arg("run")
            .arg(&fixture)
            .output()
            .expect("spawn gos run");
        assert!(
            out.status.success(),
            "{}: {}",
            fixture.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("extension:{suffix}")));
        let _ = std::fs::remove_file(fixture);
    }
}

#[test]
fn gos_run_does_not_rewrite_an_existing_non_gos_path() {
    let dir = env::temp_dir().join(format!(
        "gossamer-cli-extension-conflict-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let explicit = dir.join("testy.py");
    let inferred = dir.join("testy.gos");
    std::fs::write(&explicit, "fn main() { println(\"explicit py\") }\n")
        .expect("write explicit fixture");
    std::fs::write(&inferred, "fn main() { println(\"inferred gos\") }\n")
        .expect("write inferred fixture");

    let explicit_out = Command::new(gos_bin())
        .arg("run")
        .arg(&explicit)
        .output()
        .expect("spawn gos run explicit");
    assert!(
        explicit_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&explicit_out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&explicit_out.stdout),
        "explicit py\n"
    );

    let extensionless = dir.join("testy");
    let inferred_out = Command::new(gos_bin())
        .arg("run")
        .arg(&extensionless)
        .output()
        .expect("spawn gos run extensionless");
    assert!(
        inferred_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&inferred_out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&inferred_out.stdout),
        "inferred gos\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gos_run_infers_a_gos_extension_when_omitted() {
    let fixture = env::temp_dir().join(format!(
        "gossamer-cli-inferred-extension-{}.gos",
        std::process::id()
    ));
    std::fs::write(&fixture, "fn main() { println(\"inferred extension\") }\n")
        .expect("write fixture");
    let mut omitted = fixture.clone();
    omitted.set_extension("");
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&omitted)
        .output()
        .expect("spawn gos run");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("inferred extension"));
    let _ = std::fs::remove_file(fixture);
}

#[test]
fn execute_flag_executes_inline_source() {
    for flag in ["-e", "--eval"] {
        let out = Command::new(gos_bin())
            .args([flag, "println(\"inline works\")"])
            .output()
            .expect("spawn inline eval");
        assert!(
            out.status.success(),
            "{flag} stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("inline works"),
            "{flag} stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    for removed in ["-c", "--command"] {
        let out = Command::new(gos_bin())
            .args([removed, "println(\"old flag\")"])
            .output()
            .expect("spawn removed inline flag");
        assert!(!out.status.success(), "removed flag {removed} still worked");
    }
}

#[test]
fn adjacent_expression_is_not_implicit_function_application() {
    let invalid = Command::new(gos_bin())
        .args(["-e", "for e in Vec::from([1, 2, 3]) { println e }"])
        .output()
        .expect("spawn invalid adjacent expressions");
    assert!(!invalid.status.success());
    let stderr = String::from_utf8_lossy(&invalid.stderr);
    assert!(
        stderr.contains("expected `;` or a newline between statements"),
        "{stderr}"
    );

    let first_class = Command::new(gos_bin())
        .args(["-e", "let output = println\noutput(7)"])
        .output()
        .expect("spawn first-class println");
    assert!(
        first_class.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first_class.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&first_class.stdout), "7\n");
}

#[test]
fn help_wraps_to_narrow_terminal_width() {
    let out = Command::new(gos_bin())
        .arg("--help")
        .env("COLUMNS", "40")
        .output()
        .expect("spawn --help");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout
            .lines()
            .all(|line| { line.chars().count() <= 40 || line.split_whitespace().count() == 1 }),
        "wrappable help prose exceeded requested terminal width:\n{stdout}"
    );
    assert!(stdout.contains("The Gossamer toolchain"), "{stdout}");
    assert!(stdout.contains("Commands:"), "{stdout}");
}

#[test]
fn help_distinguishes_run_from_inline_source() {
    let out = Command::new(gos_bin())
        .arg("-h")
        .output()
        .expect("spawn -h");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("gos [OPTIONS] <COMMAND>"), "{stdout}");
    assert!(stdout.contains("gos run [FILE] [ARGS]..."), "{stdout}");
    assert!(stdout.contains("--eval <STRING>"), "{stdout}");
    assert!(!stdout.contains("--command"), "{stdout}");
    assert!(!stdout.contains("<SUBCOMMAND>"), "{stdout}");
}

#[test]
fn run_forwards_script_arguments_without_a_separator() {
    let fixture = write_fixture(
        "run-script-args",
        "use std::env\nprintln(\"{} {}\", env::args()[0], env::args()[1])\n",
    );
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .args(["first", "--second"])
        .output()
        .expect("spawn run with script arguments");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "first --second\n");
}

#[test]
fn bare_file_invocation_is_rejected() {
    let fixture = write_fixture("bare-file-rejected", "fn main() {}\n");
    let out = Command::new(gos_bin())
        .arg(&fixture)
        .output()
        .expect("spawn rejected bare file");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unrecognized subcommand"), "{stderr}");
}

#[test]
fn cache_status_uses_human_readable_sizes_by_default() {
    let root = env::temp_dir().join(format!("gossamer-cache-status-{}", std::process::id()));
    let frontend = root.join("frontend");
    let project = root.join("project");
    std::fs::create_dir_all(&frontend).expect("create frontend cache");
    std::fs::create_dir_all(&project).expect("create project");
    std::fs::write(frontend.join("entry"), vec![0_u8; 1536]).expect("write cache entry");

    let out = Command::new(gos_bin())
        .arg("cache")
        .current_dir(&project)
        .env("GOSSAMER_CACHE_DIR", &frontend)
        .env("GOSSAMER_CACHE", root.join("bindings"))
        .env("HOME", root.join("home"))
        .output()
        .expect("spawn cache status");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("frontend"), "stdout: {stdout}");
    assert!(stdout.contains("1.5K"), "stdout: {stdout}");
    assert!(!stdout.contains("1536 bytes"), "stdout: {stdout}");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn cache_clear_removes_every_known_cache_class() {
    let root = env::temp_dir().join(format!(
        "gossamer-cache-clear-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let frontend = root.join("frontend");
    let binding = root.join("binding-cache");
    let runners = binding.join("gossamer").join("runners");
    let packages = root.join("packages");
    let home = root.join("home");
    // The shared cache root follows the platform convention:
    // `%LOCALAPPDATA%\gossamer` on Windows, `$HOME/.cache/gossamer` elsewhere.
    // Point whichever one this platform reads at the sandbox.
    let local_app_data = root.join("localappdata");
    let user_cache = if cfg!(windows) {
        local_app_data.join("gossamer")
    } else {
        home.join(".cache").join("gossamer")
    };
    let shared_ir = user_cache.join("ir-cache");
    let build = home.join(".gossamer").join("build");
    let project = root.join("project");
    let project_ir = project.join(".gos-cache").join("ir-cache");
    let target = project.join("target");
    let vendor = project.join("vendor");

    // `GOSSAMER_CACHE_DIR` is set below, so the frontend class resolves to
    // that one directory and the conventional frontend locations are not in
    // scope for this run.
    let cache_roots = [
        &frontend,
        &shared_ir,
        &runners,
        &packages,
        &build,
        &project_ir,
    ];
    for path in cache_roots {
        std::fs::create_dir_all(path).expect("create cache root");
        std::fs::write(path.join("entry"), b"cache").expect("write cache entry");
    }
    for path in [&target, &vendor] {
        std::fs::create_dir_all(path).expect("create project directory");
        std::fs::write(path.join("entry"), b"project data").expect("write project data");
    }

    // Every class is reached by the scope that spans both sides; the default
    // scope is the project's own, covered by the test below.
    let out = Command::new(gos_bin())
        .args(["cache", "--clear", "--scope", "all"])
        .current_dir(&project)
        .env("GOSSAMER_CACHE_DIR", &frontend)
        .env("GOSSAMER_CACHE", &binding)
        .env("GOS_CACHE_DIR", &packages)
        .env("HOME", &home)
        .env("LOCALAPPDATA", &local_app_data)
        .env_remove("XDG_CACHE_HOME")
        .output()
        .expect("spawn cache --clear");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cache clear (all): removed"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    for path in cache_roots {
        assert!(!path.exists(), "cache root remains: {}", path.display());
    }
    assert!(target.exists(), "cache clear removed target/");
    assert!(vendor.exists(), "cache clear removed vendor/");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn cache_clear_defaults_to_the_project_cache() {
    // A bare `--clear` empties the checkout's own `.gos-cache/` and leaves
    // the roots every other project on the machine reuses in place.
    let root = env::temp_dir().join(format!(
        "gossamer-cache-scope-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let binding = root.join("binding-cache");
    let runners = binding.join("gossamer").join("runners");
    let packages = root.join("packages");
    let home = root.join("home");
    let local_app_data = root.join("localappdata");
    let user_cache = if cfg!(windows) {
        local_app_data.join("gossamer")
    } else {
        home.join(".cache").join("gossamer")
    };
    let shared_frontend = user_cache.join("frontend");
    let shared_ir = user_cache.join("ir-cache");
    let build = home.join(".gossamer").join("build");
    let project = root.join("project");
    let project_frontend = project.join(".gos-cache").join("frontend");
    let project_ir = project.join(".gos-cache").join("ir-cache");

    let shared_roots = [&shared_frontend, &shared_ir, &runners, &packages, &build];
    let project_roots = [&project_frontend, &project_ir];
    for path in shared_roots.iter().chain(project_roots.iter()) {
        std::fs::create_dir_all(path).expect("create cache root");
        std::fs::write(path.join("entry"), b"cache").expect("write cache entry");
    }

    let run = |args: &[&str]| {
        Command::new(gos_bin())
            .args(args)
            .current_dir(&project)
            .env("GOSSAMER_CACHE", &binding)
            .env("GOS_CACHE_DIR", &packages)
            .env("HOME", &home)
            .env("LOCALAPPDATA", &local_app_data)
            .env_remove("GOSSAMER_CACHE_DIR")
            .env_remove("XDG_CACHE_HOME")
            .output()
            .expect("spawn gos cache")
    };

    let out = run(&["cache", "--clear"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cache clear (local): removed"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    for path in project_roots {
        assert!(!path.exists(), "project root remains: {}", path.display());
    }
    for path in shared_roots {
        assert!(path.exists(), "shared root removed: {}", path.display());
    }

    let out = run(&["cache", "--clear", "--scope", "global"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for path in shared_roots {
        assert!(!path.exists(), "shared root remains: {}", path.display());
    }

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn cache_clear_reaches_every_project_cache_below_the_working_directory() {
    // A checkout builds its examples and integration tests from their own
    // directories, so each anchors a `.gos-cache/` of its own, and a link
    // stamp is cache the same way an object file is.
    let root = env::temp_dir().join(format!(
        "gossamer-cache-nested-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let project = root.join("project");
    let home = root.join("home");
    let caches = [
        project.join(".gos-cache"),
        project.join("examples").join("crud").join(".gos-cache"),
        project.join("tests").join("integration").join(".gos-cache"),
    ];
    for cache in &caches {
        for class in ["frontend", "ir-cache", "link-stamps"] {
            let dir = cache.join(class);
            std::fs::create_dir_all(&dir).expect("create cache class");
            std::fs::write(dir.join("entry"), b"cache").expect("write cache entry");
        }
    }
    let artifact = project.join("target").join("release");
    std::fs::create_dir_all(&artifact).expect("create target");
    std::fs::write(artifact.join("app"), b"binary").expect("write artifact");

    let out = Command::new(gos_bin())
        .args(["cache", "--clear"])
        .current_dir(&project)
        .env("HOME", &home)
        .env("LOCALAPPDATA", root.join("localappdata"))
        .env_remove("GOSSAMER_CACHE_DIR")
        .env_remove("XDG_CACHE_HOME")
        .output()
        .expect("spawn cache --clear");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for cache in &caches {
        assert!(!cache.exists(), "cache remains: {}", cache.display());
    }
    assert!(
        artifact.join("app").exists(),
        "cache clear removed a build artifact"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn parse_subcommand_round_trips_hello_world() {
    let fixture = write_fixture("parse", "fn main() { println(\"hello\") }\n");
    let out = Command::new(gos_bin())
        .args(["parse"])
        .arg(&fixture)
        .output()
        .expect("spawn parse");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("fn main"));
    assert!(stdout.contains("println"));
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn check_subcommand_succeeds_on_simple_program() {
    let fixture = write_fixture(
        "check",
        "fn add(a: i64, b: i64) -> i64 { a + b }\nfn main() { let _ = add(1i64, 2i64) }\n",
    );
    let out = Command::new(gos_bin())
        .args(["check"])
        .arg(&fixture)
        .output()
        .expect("spawn check");
    assert!(
        out.status.success(),
        "check failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("check: ok"));
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn check_subcommand_reports_type_mismatch() {
    let fixture = write_fixture("checkfail", "fn main() { let x: bool = 42i32 }\n");
    let out = Command::new(gos_bin())
        .args(["check"])
        .arg(&fixture)
        .output()
        .expect("spawn check");
    assert!(!out.status.success(), "expected failure");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("type: type mismatch") || stderr.contains("check failed"),
        "stderr: {stderr}"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn run_subcommand_executes_via_vm() {
    let fixture = write_fixture("run", "fn main() { println(\"cli-vm-run\") }\n");
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("spawn run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("cli-vm-run"));
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn queue_and_stack_push_pop_order() {
    let fixture = write_fixture(
        "vec-queue-stack",
        "use std::collections::{Queue, Stack}\n\
         fn main() {\n\
             let mut q: Queue<i64> = Queue::from([1, 2, 3])\n\
             q.push(4)\n\
             println(\"{}\", q)\n\
             println(\"queue {} {} {} {}\", q.len(), q.peek().unwrap_or(0), q.pop().unwrap_or(0), q.pop().unwrap_or(0))\n\
             let mut s: Stack<i64> = Stack::from([1, 2, 3])\n\
             s.push(4)\n\
             println(\"{}\", s)\n\
             println(\"stack {} {} {}\", s.len(), s.peek().unwrap_or(0), s.pop().unwrap_or(0))\n\
         }\n",
    );
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("spawn run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "Queue [1, 2, 3, 4]\nqueue 4 1 1 2\nStack [1, 2, 3, 4]\nstack 4 4 4\n"
    );
    let _ = std::fs::remove_file(&fixture);
}

const LAZY_ITERATOR_TIER_SOURCE: &str = r#"use std::{iter, option}

fn main() {
    let xs = 1..100
        |> |v| iter::map(v, |x| {
            if x > 3 { panic("map was eager") }
            x
        })
        |> |v| iter::take(v, 3)
        |> iter::collect
    println("{}", xs.sum())

    let skipped = (1..8) |> |v| iter::skip(v, 3) |> |v| iter::take(v, 2) |> iter::sum
    println("{skipped}")

    let chained = iter::chain((1..3), (5..7)) |> iter::sum
    println("{chained}")

    let folded = (1..5) |> |v| iter::fold(v, 10i64, |acc: i64, x: i64| acc + x)
    println("{folded}")

    let any_hit = (1..100)
        |> |v| iter::any(v, |x| {
            if x > 4 { panic("any was eager") }
            x == 3
        })
    println("{any_hit}")

    let all_hit = (1..100)
        |> |v| iter::all(v, |x| {
            if x > 4 { panic("all was eager") }
            x < 3
        })
    println("{all_hit}")

    let found = (1..100)
        |> |v| iter::find(v, |x| {
            if x > 5 { panic("find was eager") }
            x == 4
        })
        |> |v| option::unwrap_or(v, -1)
    println("{found}")

    let once_sum = iter::once(41) |> iter::sum
    println("{once_sum}")

    let product = (2..5) |> iter::product
    println("{product}")

    let min_value = (4..7) |> iter::min |> |v| option::unwrap_or(v, -1)
    println("{min_value}")

    let max_value = (4..7) |> iter::max |> |v| option::unwrap_or(v, -1)
    println("{max_value}")

    let enumerated = (3..6) |> iter::enumerate |> iter::collect
    println("{}", enumerated.len())

    let zipped = iter::zip((1..4), (10..20)) |> iter::collect
    println("{}", zipped.len())

    let pair_count = (1..4) |> iter::enumerate |> iter::count
    println("{pair_count}")

    let borrowed = [1, 2, 3, 4]
    let borrowed_total = borrowed
        |> |v| iter::map(v, |x| x * 2)
        |> |v| iter::filter(v, |x| x > 4)
        |> |v| iter::take(v, 2)
        |> iter::sum
    println("{borrowed_total}")

    let mut replaced: Vec<i64> = Vec::from([1, 2, 3])
    let pending_replacement = replaced |> |v| iter::map(v, |x| x)
    replaced[1] = 9
    println("{}", pending_replacement |> iter::sum)

    let open_end = 10..
        |> |v| iter::take(v, 4)
        |> iter::collect
    println("{}", open_end.len())
}
"#;

const LAZY_ITERATOR_TIER_OUTPUT: &str =
    "6\n9\n14\n20\ntrue\nfalse\n4\n41\n24\n4\n6\n3\n3\n3\n14\n6\n4\n";

const EAGER_COLLECTION_SOURCE: &str = r#"use std::iter

fn main() {
    let range = iter::range(2, 6)
    let mapped = range |> |v| iter::map(v, |x| x * 2)
    let filtered = mapped |> |v| iter::filter(v, |x| x > 5)
    let taken = filtered |> |v| iter::take(v, 2)
    println("{} {} {} {}", range[0], mapped[1], taken[0], iter::sum(taken))
}
"#;

const EAGER_COLLECTION_OUTPUT: &str = "2 6 6 14\n";

// Allocation telemetry prints through the runtime's Unix-only `libc::atexit`
// hook; Windows still exercises lazy pipelines in the cross-tier tests.
#[cfg(unix)]
const LAZY_ITERATOR_ALLOCATION_SOURCE: &str = r#"use std::iter

fn main() {
    let out = (0..100)
        |> |v| iter::map(v, |x| x + 1)
        |> |v| iter::filter(v, |x| x % 2 == 0)
        |> |v| iter::take(v, 3)
        |> iter::collect
    println("{}", out.sum())
}
"#;

const LAZY_ITERATOR_INVALIDATION_SOURCE: &str = r#"use std::iter

fn main() {
    let mut xs: Vec<i64> = Vec::from([1, 2, 3])
    let pending = xs.iter() |> |v| iter::map(v, |x| x)
    xs.push(4)
    println("{}", pending |> iter::sum)
}
"#;

const LAZY_ITERATOR_PANIC_SOURCE: &str = r#"use std::iter

fn main() {
    let _ = (0..8)
        |> |v| iter::map(v, |x| {
            if x == 3 { panic("lazy adapter panic sentinel") }
            x
        })
        |> iter::count
}
"#;

#[test]
fn run_absolute_project_runs_lazy_iterator_pipelines() {
    let dir = env::temp_dir().join(format!("gos-lazy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create project dir");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/lazy-pipelines\"\nversion = \"0.1.0\"\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.gos"), LAZY_ITERATOR_TIER_SOURCE).expect("write source");
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&dir)
        .output()
        .expect("spawn run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        LAZY_ITERATOR_TIER_OUTPUT
    );
    let jit_out = Command::new(gos_bin())
        .arg("run")
        .arg(&dir)
        .env("GOSSAMER_JIT_THRESHOLD", "1")
        .output()
        .expect("spawn forced-jit run");
    assert!(
        jit_out.status.success(),
        "{}",
        String::from_utf8_lossy(&jit_out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&jit_out.stdout),
        LAZY_ITERATOR_TIER_OUTPUT
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_absolute_project_builds_lazy_iterator_pipelines() {
    let dir = env::temp_dir().join(format!("gos-lazy-build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create project dir");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/native-lazy-pipelines\"\nversion = \"0.1.0\"\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.gos"), LAZY_ITERATOR_TIER_SOURCE).expect("write source");
    let build = Command::new(gos_bin())
        .args(["build"])
        .arg(&dir)
        .output()
        .expect("spawn build");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bin = dir.join("target").join("debug").join(if cfg!(windows) {
        "native-lazy-pipelines.exe"
    } else {
        "native-lazy-pipelines"
    });
    let out = Command::new(&bin).output().expect("run built binary");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        LAZY_ITERATOR_TIER_OUTPUT
    );

    let release_build = Command::new(gos_bin())
        .args(["build", "--release"])
        .arg(&dir)
        .output()
        .expect("spawn release build");
    assert!(
        release_build.status.success(),
        "{}",
        String::from_utf8_lossy(&release_build.stderr)
    );
    let release_bin = dir.join("target").join("release").join(if cfg!(windows) {
        "native-lazy-pipelines.exe"
    } else {
        "native-lazy-pipelines"
    });
    let release_out = Command::new(release_bin)
        .output()
        .expect("run release binary");
    assert!(
        release_out.status.success(),
        "{}",
        String::from_utf8_lossy(&release_out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&release_out.stdout),
        LAZY_ITERATOR_TIER_OUTPUT
    );
    let _ = std::fs::remove_dir_all(&dir);
}


#[test]
fn the_collection_iterator_surface_remains_eager_on_all_tiers() {
    let dir = env::temp_dir().join(format!("gos-eager-iter-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create project dir");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/eager-iter\"\nversion = \"0.1.0\"\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.gos"), EAGER_COLLECTION_SOURCE).expect("write source");

    for mut command in [
        {
            let mut command = Command::new(gos_bin());
            command.arg("run").arg(&dir);
            command
        },
        {
            let mut command = Command::new(gos_bin());
            command
                .arg("run")
                .arg(&dir)
                .env("GOSSAMER_JIT_THRESHOLD", "1");
            command
        },
    ] {
        let out = command.output().expect("run eager compatibility fixture");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            EAGER_COLLECTION_OUTPUT
        );
    }

    let build = Command::new(gos_bin())
        .args(["build"])
        .arg(&dir)
        .output()
        .expect("build eager compatibility fixture");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bin = dir.join("target").join("debug").join(if cfg!(windows) {
        "eager-iter.exe"
    } else {
        "eager-iter"
    });
    let llvm = Command::new(bin).output().expect("run LLVM fixture");
    assert!(
        llvm.status.success(),
        "{}",
        String::from_utf8_lossy(&llvm.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&llvm.stdout),
        EAGER_COLLECTION_OUTPUT
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
// See `LAZY_ITERATOR_ALLOCATION_SOURCE`: the assertion reads Unix-only exit
// telemetry, while functional lazy-pipeline coverage remains cross-platform.
#[cfg(unix)]
fn lazy_pipeline_allocates_only_its_collected_vec_on_llvm() {
    let dir = env::temp_dir().join(format!("gos-lazy-iter-allocs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create project dir");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/lazy-iter-allocs\"\nversion = \"0.1.0\"\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.gos"), LAZY_ITERATOR_ALLOCATION_SOURCE).expect("write source");

    let build = Command::new(gos_bin())
        .args(["build"])
        .arg(&dir)
        .output()
        .expect("build allocation fixture");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bin = dir.join("target").join("debug").join(if cfg!(windows) {
        "lazy-iter-allocs.exe"
    } else {
        "lazy-iter-allocs"
    });
    let out = Command::new(bin)
        .env("GOS_VEC_ALLOC_STATS", "1")
        .output()
        .expect("run allocation fixture");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "12\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("VEC ALLOC STATS: inline=2 split=0 owner=0 region=0"),
        "expected one process bootstrap Vec plus the final collected Vec, got:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn borrowed_lazy_vec_structural_mutation_fails_on_all_tiers() {
    let dir = env::temp_dir().join(format!("gos-lazy-iter-invalidation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create project dir");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/lazy-iter-invalidation\"\nversion = \"0.1.0\"\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.gos"), LAZY_ITERATOR_INVALIDATION_SOURCE).expect("write source");

    for mut command in [
        {
            let mut command = Command::new(gos_bin());
            command.arg("run").arg(&dir);
            command
        },
        {
            let mut command = Command::new(gos_bin());
            command
                .arg("run")
                .arg(&dir)
                .env("GOSSAMER_JIT_THRESHOLD", "1");
            command
        },
    ] {
        let out = command.output().expect("run invalidation fixture");
        assert!(
            !out.status.success(),
            "unexpected stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("borrowed Vec source was structurally mutated during iteration"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let build = Command::new(gos_bin())
        .args(["build"])
        .arg(&dir)
        .output()
        .expect("build invalidation fixture");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bin = dir.join("target").join("debug").join(if cfg!(windows) {
        "lazy-iter-invalidation.exe"
    } else {
        "lazy-iter-invalidation"
    });
    let llvm = Command::new(bin).output().expect("run LLVM fixture");
    assert!(!llvm.status.success());
    assert!(
        String::from_utf8_lossy(&llvm.stderr)
            .contains("borrowed Vec source was structurally mutated during iteration"),
        "{}",
        String::from_utf8_lossy(&llvm.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lazy_adapter_panic_propagates_on_all_tiers() {
    let dir = env::temp_dir().join(format!("gos-lazy-iter-panic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create project dir");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/lazy-iter-panic\"\nversion = \"0.1.0\"\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.gos"), LAZY_ITERATOR_PANIC_SOURCE).expect("write source");

    for mut command in [
        {
            let mut command = Command::new(gos_bin());
            command.args(["run", "--no-jit"]).arg(&dir);
            command
        },
        {
            let mut command = Command::new(gos_bin());
            command
                .arg("run")
                .arg(&dir)
                .env("GOSSAMER_JIT_THRESHOLD", "1")
                .env("GOSSAMER_JIT_MIN_WORK", "1");
            command
        },
    ] {
        let out = command.output().expect("run adapter panic fixture");
        assert!(
            !out.status.success(),
            "unexpected stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("lazy adapter panic sentinel"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let build = Command::new(gos_bin())
        .args(["build"])
        .arg(&dir)
        .output()
        .expect("build adapter panic fixture");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bin = dir.join("target").join("debug").join(if cfg!(windows) {
        "lazy-iter-panic.exe"
    } else {
        "lazy-iter-panic"
    });
    let llvm = Command::new(bin).output().expect("run LLVM fixture");
    assert!(!llvm.status.success());
    assert!(
        String::from_utf8_lossy(&llvm.stderr).contains("lazy adapter panic sentinel"),
        "{}",
        String::from_utf8_lossy(&llvm.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stdin_read_line_appends_to_mut_string() {
    let fixture = write_fixture(
        "stdin-read-line",
        r#"use std::io

fn main() {
    let mut input = String::new()
    io::stdin().read_line(&mut input).unwrap()
    println("typed={} bytes={}", input.trim(), input.len())
}
"#,
    );
    let mut child = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn run");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"hello\n")
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "typed=hello bytes=6\n"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn stdin_read_line_no_arg_matches_option_in_vm_and_native() {
    let fixture = write_fixture(
        "stdin-read-line-option",
        r#"use std::io

fn main() {
    let mut input = String::new()
    io::stdin().read_line(&mut input).unwrap()
    println("first={}", input.trim())
    match io::stdin().read_line() {
        Some(name) => println("second={}", name),
        None => println("EOF"),
    }
}
"#,
    );

    let mut vm = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn vm run");
    vm.stdin
        .as_mut()
        .expect("vm stdin")
        .write_all(b"Meow!\nDaniel\n")
        .expect("write vm stdin");
    let vm_out = vm.wait_with_output().expect("wait vm run");
    assert!(
        vm_out.status.success(),
        "{}",
        String::from_utf8_lossy(&vm_out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&vm_out.stdout),
        "first=Meow!\nsecond=Daniel\n"
    );

    let out_dir = env::temp_dir().join(format!(
        "gossamer-cli-stdin-read-line-option-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&out_dir).expect("create native out dir");
    let build = Command::new(gos_bin())
        .args(["build"])
        .arg(&fixture)
        .args(["--out-dir"])
        .arg(&out_dir)
        .output()
        .expect("spawn build");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let mut bin = out_dir.join(fixture.file_stem().expect("fixture stem"));
    if cfg!(windows) {
        bin.set_extension("exe");
    }
    let mut native = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn native run");
    native
        .stdin
        .as_mut()
        .expect("native stdin")
        .write_all(b"Meow!\nDaniel\n")
        .expect("write native stdin");
    let native_out = native.wait_with_output().expect("wait native run");
    assert!(
        native_out.status.success(),
        "{}",
        String::from_utf8_lossy(&native_out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&native_out.stdout),
        "first=Meow!\nsecond=Daniel\n"
    );

    let _ = std::fs::remove_file(&fixture);
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
fn intcode_day2_native_matches_vm() {
    let fixture = workspace_root()
        .join("feature-testing-examples")
        .join("intcode_day2_native.gos");
    let expected = "pos0=3500\nsample=30 2\n";

    let vm = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("spawn vm run");
    assert!(
        vm.status.success(),
        "{}",
        String::from_utf8_lossy(&vm.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&vm.stdout), expected);

    let out_dir = env::temp_dir().join(format!(
        "gossamer-cli-intcode-day2-native-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&out_dir).expect("create native out dir");
    let build = Command::new(gos_bin())
        .args(["build"])
        .arg(&fixture)
        .args(["--out-dir"])
        .arg(&out_dir)
        .output()
        .expect("spawn build");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let mut bin = out_dir.join("intcode_day2_native");
    if cfg!(windows) {
        bin.set_extension("exe");
    }
    let native = Command::new(&bin).output().expect("run native");
    assert!(
        native.status.success(),
        "{}",
        String::from_utf8_lossy(&native.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&native.stdout), expected);
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
fn intcode_day2_mut_slice_matches_vm_debug_and_release() {
    let fixture = workspace_root()
        .join("feature-testing-examples")
        .join("intcode_day2_mut_slice_native.gos");
    let expected = "sample=30\npos0=3500\ncopy=99, base=1\npart2=22\npart2_base=1\n";

    let vm = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("spawn vm run");
    assert!(
        vm.status.success(),
        "{}",
        String::from_utf8_lossy(&vm.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&vm.stdout), expected);

    for release in [false, true] {
        let out_dir = env::temp_dir().join(format!(
            "gossamer-cli-intcode-day2-mut-slice-{}-{}",
            if release { "release" } else { "debug" },
            std::process::id()
        ));
        std::fs::create_dir_all(&out_dir).expect("create native out dir");
        let mut build = Command::new(gos_bin());
        build.arg("build");
        if release {
            build.arg("--release");
        }
        let build = build
            .arg(&fixture)
            .args(["--out-dir"])
            .arg(&out_dir)
            .output()
            .expect("spawn build");
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );

        let mut bin = out_dir.join("intcode_day2_mut_slice_native");
        if cfg!(windows) {
            bin.set_extension("exe");
        }
        let native = Command::new(&bin).output().expect("run native");
        assert!(
            native.status.success(),
            "{}",
            String::from_utf8_lossy(&native.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&native.stdout),
            expected,
            "{} build output drifted",
            if release { "release" } else { "debug" }
        );
        let _ = std::fs::remove_dir_all(&out_dir);
    }
}

#[test]
fn run_subcommand_executes_via_vm_by_default() {
    let fixture = write_fixture("runvm", "fn main() { println(\"cli-vm\") }\n");
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("spawn run");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("cli-vm"));
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn build_subcommand_produces_runnable_output() {
    // `gos build` now defaults to native codegen via Cranelift + the
    // host `cc`. The happy-path output is a real executable that
    // exits with the Gossamer `main`'s return code. If native
    // codegen falls back (e.g. unsupported MIR), a launcher-script
    // takes over - both shapes are accepted here.
    let dir = env::temp_dir().join(format!("gos-build-magic-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("build_magic.gos");
    std::fs::write(&source_path, "fn main() -> i64 { 42i64 }\n").unwrap();
    let out = Command::new(gos_bin())
        .arg("build")
        .arg(&source_path)
        .output()
        .expect("spawn build");
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let binary = dir
        .join("target")
        .join("debug")
        .join(format!("build_magic{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.exists(),
        "build output missing at {}",
        binary.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&binary).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "output should be chmod +x: mode {mode:o}"
        );
    }
    // Either path prints a single build: line to stdout.
    assert!(String::from_utf8_lossy(&out.stdout).contains("build:"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
#[test]
fn build_rss_profile_reports_frontend_release_and_backend_peak() {
    let dir = env::temp_dir().join(format!("gos-build-rss-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("rss.gos");
    std::fs::write(&source_path, "fn main() { println(\"rss\") }\n").unwrap();

    let out = Command::new(gos_bin())
        .arg("build")
        .arg(&source_path)
        .env("GOS_PROFILE_RSS", "1")
        .output()
        .expect("spawn build with RSS profiling");
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    for stage in [
        "build_frontend_checked",
        "build_frontend_released",
        "build_backend_emitted",
    ] {
        assert!(stderr.contains(&format!("rss: stage={stage} ")), "{stderr}");
    }
    assert!(stderr.contains("peak_bytes="), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_build_records_15_0_deployment_target() {
    let dir = env::temp_dir().join(format!(
        "gos-macos-deployment-target-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("deployment_target.gos");
    std::fs::write(&source_path, "fn main() { println(\"macos-15\") }\n").unwrap();

    let build = Command::new(gos_bin())
        .arg("build")
        .arg(&source_path)
        .env_remove("MACOSX_DEPLOYMENT_TARGET")
        .output()
        .expect("spawn gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    let binary = dir.join("target").join("debug").join("deployment_target");
    let metadata = Command::new("otool")
        .arg("-l")
        .arg(&binary)
        .output()
        .expect("run otool");
    assert!(
        metadata.status.success(),
        "otool failed: {}",
        String::from_utf8_lossy(&metadata.stderr)
    );
    let metadata = String::from_utf8(metadata.stdout).expect("otool output is UTF-8");
    assert!(
        metadata.lines().any(|line| line.trim() == "minos 15.0"),
        "Mach-O does not record macOS 15.0 as LC_BUILD_VERSION minos:\n{metadata}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_output_handles_empty_argv_for_flag_define_programs() {
    let dir = env::temp_dir().join(format!("gos-build-argv-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("argv_ok.gos");
    std::fs::write(
        &source_path,
        "use std::flag\n\
         fn main() {\n\
             let flags = flag::define(\"argv-ok\", [\n\
                 flag::int(\"port\", 8080, \"port\", 'p'),\n\
                 flag::bool(\"verbose\", false, \"verbose\", 'v'),\n\
             ])\n\
             if *flags.verbose {\n\
                 println(\"verbose\")\n\
             } else {\n\
                 println((*flags.port).to_string())\n\
             }\n\
         }\n",
    )
    .unwrap();
    let build = Command::new(gos_bin())
        .arg("build")
        .arg(&source_path)
        .output()
        .expect("spawn build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir
        .join("target")
        .join("debug")
        .join(format!("argv_ok{}", std::env::consts::EXE_SUFFIX));
    let run = Command::new(&binary).output().expect("run built artifact");
    assert!(
        run.status.success(),
        "native run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "8080\n");
}

#[test]
fn build_output_preserves_http_method_chain_through_send_and_field_access() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
    let addr = listener.local_addr().expect("loopback addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
            .expect("write response");
    });

    let dir = env::temp_dir().join(format!("gos-build-http-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("http_chain.gos");
    std::fs::write(
        &source_path,
        format!(
            "use std::http\n\
             fn main() {{\n\
                 let url = \"http://{addr}/\"\n\
                 match http::Client::new().get(url).send() {{\n\
                     Ok(resp) => println(resp.status.to_string() + \":\" + resp.body),\n\
                     Err(e) => println(\"send failed: \" + e.message()),\n\
                 }}\n\
             }}\n"
        ),
    )
    .unwrap();
    let build = Command::new(gos_bin())
        .arg("build")
        .arg(&source_path)
        .output()
        .expect("spawn build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir
        .join("target")
        .join("debug")
        .join(format!("http_chain{}", std::env::consts::EXE_SUFFIX));
    let run = Command::new(&binary).output().expect("run built artifact");
    assert!(
        run.status.success(),
        "native run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "200:hello\n");
    server.join().expect("join server");
}

/// Serves `count` HTTP requests on `listener`, echoing the `x-test`
/// header and the request body back as `xt=<v> body=<b>` with a 201.
fn serve_builder_echo(listener: &TcpListener, count: usize) {
    for _ in 0..count {
        let (mut stream, _) = listener.accept().expect("accept client");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let body_start = loop {
            let n = stream.read(&mut chunk).expect("read request");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            assert!(n != 0, "connection closed before headers completed");
        };
        let lower = String::from_utf8_lossy(&buf[..body_start]).to_ascii_lowercase();
        let content_len: usize = lower
            .lines()
            .find_map(|l| l.strip_prefix("content-length:").map(str::trim))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        while buf.len() < body_start + content_len {
            let n = stream.read(&mut chunk).expect("read body");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let xt = lower
            .lines()
            .find_map(|l| l.strip_prefix("x-test:").map(str::trim))
            .unwrap_or("<none>");
        let body = String::from_utf8_lossy(&buf[body_start..]).into_owned();
        let reply = format!("xt={xt} body={body}");
        let resp = format!(
            "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            reply.len(),
            reply
        );
        stream.write_all(resp.as_bytes()).expect("write response");
    }
}

/// Tier-parity sentinel for the chained client builder: the same
/// source must produce byte-identical stdout under `gos` (VM)
/// and a `gos build` native binary, with the chained header + body
/// honored and a transport failure surfacing as `Err` on both tiers.
#[test]
fn vm_and_native_client_builder_chain_outputs_match() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
    let addr = listener.local_addr().expect("loopback addr");
    // One POST per tier (VM run + native run).
    let server = std::thread::spawn(move || serve_builder_echo(&listener, 2));

    let source = format!(
        "use std::http\n\
         fn main() {{\n\
             let client = http::Client::new()\n\
             let sent = client\n\
                 .post(\"http://{addr}/echo\")\n\
                 .header(\"x-test\", \"parity\")\n\
                 .body(\"ping\")\n\
                 .send()\n\
             match sent {{\n\
                 Ok(r) => println(\"post: {{}} {{}}\", r.status, r.body),\n\
                 Err(e) => println(\"post err: {{}}\", e),\n\
             }}\n\
             match client.get(\"http://127.0.0.1:1/refused\").send() {{\n\
                 Ok(r) => println(\"refused ok: {{}}\", r.status),\n\
                 Err(e) => println(\"refused err: {{}}\", e),\n\
             }}\n\
         }}\n"
    );
    let dir = env::temp_dir().join(format!("gos-builder-parity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("builder_parity.gos");
    std::fs::write(&source_path, source).unwrap();

    let vm = Command::new(gos_bin())
        .arg("run")
        .arg(&source_path)
        .output()
        .expect("spawn gos");
    assert!(
        vm.status.success(),
        "gos failed: {}",
        String::from_utf8_lossy(&vm.stderr)
    );

    let build = Command::new(gos_bin())
        .arg("build")
        .arg(&source_path)
        .output()
        .expect("spawn build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir
        .join("target")
        .join("debug")
        .join(format!("builder_parity{}", std::env::consts::EXE_SUFFIX));
    let native = Command::new(&binary).output().expect("run built artifact");
    assert!(
        native.status.success(),
        "native run failed: {}",
        String::from_utf8_lossy(&native.stderr)
    );

    let vm_out = String::from_utf8_lossy(&vm.stdout).into_owned();
    let native_out = String::from_utf8_lossy(&native.stdout).into_owned();
    assert_eq!(vm_out, native_out, "tier outputs diverge");
    assert!(
        vm_out.contains("post: 201 xt=parity body=ping"),
        "chained header/body not honored: {vm_out}"
    );
    assert!(
        vm_out.contains("refused err: http: transport:"),
        "transport failure must surface as Err: {vm_out}"
    );
    server.join().expect("join server");
}

#[test]
fn build_subcommand_accepts_known_target_triple_and_rejects_unknown() {
    let dir = env::temp_dir().join(format!("gos-build-cross-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("cross.gos");
    std::fs::write(&source_path, "fn main() -> i64 { 0i64 }\n").unwrap();
    // A registered Linux cross target is routed into the real build
    // path. Without a target runtime archive (and a cross linker) it
    // fails at link resolution with a clear message - never the
    // registration-gate "unknown target" error, and never a stub.
    let known = Command::new(gos_bin())
        .args(["build", "--target", "aarch64-unknown-linux-gnu"])
        .arg(&source_path)
        .output()
        .expect("spawn build --target");
    let known_err = String::from_utf8_lossy(&known.stderr);
    assert!(
        !known_err.contains("unknown target"),
        "a registered Linux target must pass the registration gate: {known_err}"
    );
    // A registered but non-Linux target cannot be cross-produced from
    // any host (no bundled SDK); it is refused with a specific error,
    // not silently stubbed. Pick the darwin triple for the *other*
    // arch: on an Apple Silicon macOS runner (host `aarch64-apple-darwin`,
    // what CI's `xcode-27` runner is) the same-arch triple equals the host
    // and takes the native, non-cross build path instead of being refused.
    let other_arch_darwin = if cfg!(target_arch = "aarch64") {
        "x86_64-apple-darwin"
    } else {
        "aarch64-apple-darwin"
    };
    let darwin = Command::new(gos_bin())
        .args(["build", "--target", other_arch_darwin])
        .arg(&source_path)
        .output()
        .expect("spawn build --target darwin");
    assert!(
        !darwin.status.success(),
        "a non-Linux cross target must be refused"
    );
    let darwin_err = String::from_utf8_lossy(&darwin.stderr);
    assert!(
        darwin_err.contains("only `*-linux-*`"),
        "non-Linux target should be refused with a specific message: {darwin_err}"
    );
    let bad = Command::new(gos_bin())
        .args(["build", "--target", "wat-is-this"])
        .arg(&source_path)
        .output()
        .expect("spawn build --target bad");
    assert!(
        !bad.status.success(),
        "unknown target should fail the build"
    );
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("unknown target"),
        "stderr should name the unknown-target error"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_defaults_output_to_source_stem_without_extension() {
    // `gos build line_count.gos` should write a file called
    // `line_count` (the executable produced by the native codegen
    // pipeline).
    let dir = env::temp_dir().join(format!("gos-build-default-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("line_count.gos");
    std::fs::write(&source_path, "fn main() -> i64 { 0i64 }\n").unwrap();
    let out = Command::new(gos_bin())
        .arg("build")
        .arg(&source_path)
        .output()
        .expect("spawn build");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let binary = dir
        .join("target")
        .join("debug")
        .join(format!("line_count{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.exists(),
        "expected build output at {}",
        binary.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_honours_project_output_field_in_manifest() {
    let dir = env::temp_dir().join(format!("gos-build-manifest-out-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/widget\"\nversion = \"0.1.0\"\noutput = \"custom_name\"\n",
    )
    .unwrap();
    let source_path = dir.join("src/main.gos");
    std::fs::write(&source_path, "fn main() -> i64 { 0i64 }\n").unwrap();
    let out = Command::new(gos_bin())
        .arg("build")
        .arg(&source_path)
        .output()
        .expect("spawn build");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The manifest `output` has no extension; on Windows the linker needs
    // the `.exe` suffix, which `resolve_output_path` adds. Expect the
    // platform executable name, not the bare stem.
    let expected_name = if cfg!(windows) {
        "custom_name.exe"
    } else {
        "custom_name"
    };
    let expected = dir.join(expected_name);
    assert!(
        expected.exists(),
        "expected build output at {}",
        expected.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_creates_the_output_directory_before_linking() {
    // A manifest `output = "target/debug/app"` names a directory no earlier
    // phase creates. Every linker writes temporaries beside the output before
    // the output itself - mold's `.mold-XXXXXX` names the directory in its
    // error - so the link fails outright on a tree with no `target/`.
    let dir = env::temp_dir().join(format!("gos-build-outdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/outdir\"\nversion = \"0.1.0\"\n\
         output = \"build/bin/outdir\"\n",
    )
    .unwrap();
    let source_path = dir.join("src/main.gos");
    std::fs::write(&source_path, "fn main() { println(\"ok\") }\n").unwrap();
    let out = Command::new(gos_bin())
        .arg("build")
        .arg(&source_path)
        .current_dir(&dir)
        .output()
        .expect("spawn build");
    let expected = dir
        .join("build")
        .join("bin")
        .join(format!("outdir{}", std::env::consts::EXE_SUFFIX));
    let ok = out.status.success();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let exists = expected.exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(ok, "build into a missing directory failed: {stderr}");
    assert!(exists, "expected build output at {}", expected.display());
}

#[test]
fn build_inside_project_names_binary_after_project_id_tail() {
    // Rust's convention: `cargo build` writes `target/debug/<package>`,
    // not `target/debug/main`. Gossamer follows the same rule when a
    // `project.toml` is present - the binary takes the last segment
    // of `[project] id`, regardless of which source file holds `main`.
    let dir = env::temp_dir().join(format!("gos-build-id-tail-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"github.com/acme/widget-cli\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let source_path = dir.join("src/main.gos");
    std::fs::write(&source_path, "fn main() { }\n").unwrap();
    let out = Command::new(gos_bin())
        .arg("build")
        .arg(&source_path)
        .output()
        .expect("spawn build");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected = dir
        .join("target")
        .join("debug")
        .join(format!("widget-cli{}", std::env::consts::EXE_SUFFIX));
    assert!(
        expected.exists(),
        "expected build output at {}",
        expected.display()
    );
    let stale = dir
        .join("target")
        .join("debug")
        .join(format!("main{}", std::env::consts::EXE_SUFFIX));
    assert!(
        !stale.exists(),
        "binary must not be named after the source file when a manifest exists"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_rejects_removed_output_flag() {
    let fixture = write_fixture("buildflagremoved", "fn main() { }\n");
    let out = Command::new(gos_bin())
        .arg("build")
        .arg(&fixture)
        .arg("-o")
        .arg("somewhere")
        .output()
        .expect("spawn build");
    assert!(!out.status.success(), "-o should not be accepted");
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn update_is_a_first_class_package_command() {
    let out = Command::new(gos_bin())
        .args(["update", "--help"])
        .output()
        .expect("spawn update help");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("newest dependency versions"), "{stdout}");
    assert!(stdout.contains("--offline"), "{stdout}");
}

#[test]
fn tidy_removes_only_unimported_project_dependencies() {
    let dir = env::temp_dir().join(format!("gos-tidy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let manifest = dir.join("project.toml");
    std::fs::write(
        &manifest,
        "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n\n[dependencies]\n\"example.com/used\" = \"1.0.0\"\n\"example.com/unused\" = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.gos"),
        "use \"example.com/used\" as used\nfn main() { used::run() }\n",
    )
    .unwrap();

    let out = Command::new(gos_bin())
        .args(["tidy", "--manifest"])
        .arg(&manifest)
        .output()
        .expect("spawn tidy");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rewritten = std::fs::read_to_string(&manifest).unwrap();
    assert!(rewritten.contains("example.com/used"), "{rewritten}");
    assert!(!rewritten.contains("example.com/unused"), "{rewritten}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 unused dependency/dependencies removed")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tidy_does_not_edit_manifest_when_a_source_file_has_parse_errors() {
    let dir = env::temp_dir().join(format!("gos-tidy-parse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let manifest = dir.join("project.toml");
    let original = "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n\n[dependencies]\n\"example.com/keep\" = \"1.0.0\"\n";
    std::fs::write(&manifest, original).unwrap();
    std::fs::write(dir.join("src/main.gos"), "fn main( {\n").unwrap();

    let out = Command::new(gos_bin())
        .args(["tidy", "--manifest"])
        .arg(&manifest)
        .output()
        .expect("spawn tidy");
    assert!(!out.status.success(), "tidy must reject malformed source");
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), original);
    let _ = std::fs::remove_dir_all(&dir);
}
