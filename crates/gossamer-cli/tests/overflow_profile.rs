//! Integer overflow follows the build profile on every tier: the bytecode VM,
//! the JIT, and a debug build raise the language's overflow panic, and a
//! release build wraps. An integer `sum` or `product` overflows exactly where
//! the `+` or `*` it folds with would.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("the CLI crate sits two levels below the workspace root")
}

/// One program, one case per argument, so each tier builds once.
const PROGRAM: &str = r#"use std::{env, iter}

fn bump(x: i64) -> i64 {
    x + 1
}

fn main() {
    let case = env::args().first().unwrap_or("none")
    println("before")
    if case == "add" {
        println("{}", bump(9_223_372_036_854_775_807))
    } else if case == "lazy_sum" {
        let xs = #[9_223_372_036_854_775_807, 1]
        println("{}", xs.iter().sum())
    } else if case == "eager_sum" {
        let xs = #[9_223_372_036_854_775_807, 1]
        println("{}", xs.sum())
    } else if case == "lazy_product" {
        let xs = #[4_611_686_018_427_387_904, 2]
        println("{}", xs.iter().product())
    } else if case == "eager_product" {
        let xs = #[4_611_686_018_427_387_904, 2]
        println("{}", iter::product(xs))
    } else if case == "open_range_take" {
        for v in (9_223_372_036_854_775_806..).take(2) {
            println("{}", v)
        }
    }
}
"#;

/// Each case with the checked-tier panic text and the release-build output.
const CASES: &[(&str, &str, &str)] = &[
    (
        "add",
        "attempt to add with overflow",
        "-9223372036854775808",
    ),
    (
        "lazy_sum",
        "attempt to add with overflow",
        "-9223372036854775808",
    ),
    (
        "eager_sum",
        "attempt to add with overflow",
        "-9223372036854775808",
    ),
    (
        "lazy_product",
        "attempt to multiply with overflow",
        "-9223372036854775808",
    ),
    (
        "eager_product",
        "attempt to multiply with overflow",
        "-9223372036854775808",
    ),
    (
        "open_range_take",
        "attempt to add with overflow in open integer range",
        "9223372036854775806\n9223372036854775807",
    ),
];

fn run(mut command: Command) -> Output {
    command.output().expect("spawn")
}

fn built_binary(stdout: &[u8]) -> PathBuf {
    let stdout = String::from_utf8_lossy(stdout);
    stdout
        .lines()
        .find_map(|line| {
            let (_, rest) = line.split_once("native executable at ")?;
            Some(PathBuf::from(rest.split(" (target").next()?.trim()))
        })
        .unwrap_or_else(|| panic!("no artifact path in: {stdout}"))
}

fn build(source: &Path, out_dir: &Path, release: bool, cache: &Path) -> PathBuf {
    let mut command = Command::new(gos_bin());
    command.arg("build");
    if release {
        command.arg("--release");
    }
    command
        .arg(source)
        .arg("--out-dir")
        .arg(out_dir)
        .env("GOSSAMER_CACHE_DIR", cache);
    let output = run(command);
    assert!(
        output.status.success(),
        "build of {} failed:\n{}{}",
        source.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    built_binary(&output.stdout)
}

fn assert_checked_panic(tier: &str, case: &str, output: &Output, message: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(101),
        "{tier} {case}: expected exit 101, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("error[GX0005]: panic: {message}")),
        "{tier} {case}: expected the overflow panic, stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked at"),
        "{tier} {case}: a toolchain panic escaped, stderr:\n{stderr}"
    );
}

#[test]
fn integer_overflow_panics_in_checked_tiers_and_wraps_in_release() {
    let dir = std::env::temp_dir().join(format!("gos-overflow-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let source = dir.join("overflow.gos");
    std::fs::write(&source, PROGRAM).expect("write program");
    let cache = dir.join("cache");
    let debug = build(&source, &dir.join("debug"), false, &cache);
    let release = build(&source, &dir.join("release"), true, &cache);

    for (case, message, wrapped) in CASES {
        let mut vm = Command::new(gos_bin());
        vm.env("GOS_JIT", "0")
            .env("GOSSAMER_CACHE_DIR", &cache)
            .arg("run")
            .arg(&source)
            .arg(case);
        assert_checked_panic("vm", case, &run(vm), message);

        let mut jit = Command::new(gos_bin());
        jit.env("GOSSAMER_CACHE_DIR", &cache)
            .arg("run")
            .arg(&source)
            .arg(case);
        assert_checked_panic("jit", case, &run(jit), message);

        let mut debug_run = Command::new(&debug);
        debug_run.arg(case);
        assert_checked_panic("debug build", case, &run(debug_run), message);

        let mut release_run = Command::new(&release);
        release_run.arg(case);
        let output = run(release_run);
        assert!(
            output.status.success(),
            "release {case}: expected exit 0, stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("before\n{wrapped}\n"),
            "release {case}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The fixtures tier parity skips because their answer depends on the
/// profile: each panics on the checked tiers and runs to completion in a
/// release build.
#[test]
fn profile_dependent_overflow_fixtures_panic_when_checked_and_complete_in_release() {
    let root = workspace_root();
    let dir = std::env::temp_dir().join(format!("gos-overflow-fixtures-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let cache = dir.join("cache");
    for name in [
        "integer_overflow_edges",
        "byte_vec_i64_model",
        "neg_int_min_wraps",
    ] {
        let source = root
            .join("feature-testing-examples")
            .join(format!("{name}.gos"));

        let mut vm = Command::new(gos_bin());
        vm.env("GOS_JIT", "0")
            .env("GOSSAMER_CACHE_DIR", &cache)
            .current_dir(&root)
            .arg("run")
            .arg(&source);
        let vm = run(vm);
        let vm_stderr = String::from_utf8_lossy(&vm.stderr);
        assert_eq!(vm.status.code(), Some(101), "vm {name}:\n{vm_stderr}");
        assert!(
            vm_stderr.contains("error[GX0005]: panic: attempt to")
                && vm_stderr.contains("with overflow"),
            "vm {name}:\n{vm_stderr}"
        );

        let debug = build(&source, &dir.join(format!("{name}-debug")), false, &cache);
        let debug_run = run(Command::new(&debug));
        let debug_stderr = String::from_utf8_lossy(&debug_run.stderr);
        assert_eq!(
            debug_run.status.code(),
            Some(101),
            "debug build {name}:\n{debug_stderr}"
        );
        assert!(
            debug_stderr.contains("with overflow"),
            "debug build {name}:\n{debug_stderr}"
        );

        let release = build(&source, &dir.join(format!("{name}-release")), true, &cache);
        let release_run = run(Command::new(&release));
        assert!(
            release_run.status.success(),
            "release {name}:\n{}",
            String::from_utf8_lossy(&release_run.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
