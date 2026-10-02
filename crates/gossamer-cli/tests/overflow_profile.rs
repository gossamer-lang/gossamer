//! Integer overflow is the same on every tier and in every build profile: the
//! bytecode VM, the JIT, a debug build, and a release build all raise the
//! language's overflow panic, and the wrapping operators wrap on all of them.
//! An integer `sum` or `product` overflows exactly where the `+` or `*` it
//! folds with would, and so does a running sum in a counted loop.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
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
    } else if case == "counted_sum" {
        let n = 3_000_000
        let mut acc = 0
        let mut i = 0
        while i < n {
            acc = acc + i * i * i
            i = i + 1
        }
        println("{}", acc)
    } else if case == "counted_down_sum" {
        let mut acc = 0
        let mut i = 4_000_000
        while i > 0 {
            acc -= i * i
            i -= 1
        }
        println("{}", acc)
    } else if case == "wrapping_add" {
        let x = 9_223_372_036_854_775_807
        println("{}", x +% 1)
    } else if case == "wrapping_mul" {
        let x = 4_611_686_018_427_387_904
        println("{}", x *% 2)
    }
}
"#;

/// Each overflowing case with the panic text every tier raises.
const CASES: &[(&str, &str)] = &[
    ("add", "attempt to add with overflow"),
    ("lazy_sum", "attempt to add with overflow"),
    ("eager_sum", "attempt to add with overflow"),
    ("lazy_product", "attempt to multiply with overflow"),
    ("eager_product", "attempt to multiply with overflow"),
    (
        "open_range_take",
        "attempt to add with overflow in open integer range",
    ),
    // A counted loop bounds a running sum only when its terms keep it in range.
    ("counted_sum", "attempt to add with overflow"),
    ("counted_down_sum", "attempt to subtract with overflow"),
];

/// Each wrapping-operator case with the value every tier prints.
const WRAPPING_CASES: &[(&str, &str)] = &[
    ("wrapping_add", "-9223372036854775808"),
    ("wrapping_mul", "-9223372036854775808"),
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

/// Runs the program under `dir` on the VM, the JIT, and both native
/// profiles, handing each one `case`.
fn every_tier(source: &Path, dir: &Path, cache: &Path, case: &str) -> Vec<(&'static str, Output)> {
    let debug = build(source, &dir.join("debug"), false, cache);
    let release = build(source, &dir.join("release"), true, cache);
    let mut vm = Command::new(gos_bin());
    vm.env("GOS_JIT", "0")
        .env("GOSSAMER_CACHE_DIR", cache)
        .current_dir(dir)
        .arg("run")
        .arg(source)
        .arg(case);
    let mut jit = Command::new(gos_bin());
    jit.env("GOSSAMER_CACHE_DIR", cache)
        .current_dir(dir)
        .arg("run")
        .arg(source)
        .arg(case);
    let mut debug_run = Command::new(&debug);
    debug_run.arg(case);
    let mut release_run = Command::new(&release);
    release_run.arg(case);
    vec![
        ("vm", run(vm)),
        ("jit", run(jit)),
        ("debug build", run(debug_run)),
        ("release build", run(release_run)),
    ]
}

#[test]
fn integer_overflow_panics_on_every_tier_and_profile() {
    let dir = std::env::temp_dir().join(format!("gos-overflow-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let source = dir.join("overflow.gos");
    std::fs::write(&source, PROGRAM).expect("write program");
    let cache = dir.join("cache");
    for (case, message) in CASES {
        for (tier, output) in every_tier(&source, &dir, &cache, case) {
            assert_checked_panic(tier, case, &output, message);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wrapping_operators_wrap_on_every_tier_and_profile() {
    let dir = std::env::temp_dir().join(format!("gos-overflow-wrapping-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let source = dir.join("overflow.gos");
    std::fs::write(&source, PROGRAM).expect("write program");
    let cache = dir.join("cache");
    for (case, wrapped) in WRAPPING_CASES {
        for (tier, output) in every_tier(&source, &dir, &cache, case) {
            assert!(
                output.status.success(),
                "{tier} {case}: expected exit 0, stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                format!("before\n{wrapped}\n"),
                "{tier} {case}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
