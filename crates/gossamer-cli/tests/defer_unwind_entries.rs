#![allow(missing_docs)]

//! A native build reaches a body's landing pads by `invoke` where Rust panics
//! unwind through DWARF tables, and by catching calls on Windows, where they
//! unwind through SEH. `GOS_LLVM_UNWIND=catching` selects the Windows form on
//! any host, so both are checked against the bytecode VM wherever the suite
//! runs.

mod common;

use common::{Tier, gos_run_on, native_executable, stdout};

/// A program whose panics cross frames with pending deferred expressions:
/// through a closure, a loop, and a deferred expression that panics itself.
const PROGRAM: &str = r#"
fn apply(f: Fn(i64) -> i64, x: i64) -> i64 {
    defer println("apply defer ran")
    f(x)
}

fn checksum(xs: Vec<i64>, limit: i64) -> i64 {
    defer println("checksum defer ran")
    let mut total = 0
    for x in xs {
        total += x
        if total > limit {
            panic("checksum passed {}", limit)
        }
    }
    total
}

fn guarded(xs: Vec<i64>, label: String) -> i64 {
    let mut seen = 0
    defer println("guarded defer saw {} for {}", seen, label)
    defer panic("guarded defer exploded")
    for x in xs {
        seen += x
    }
    checksum(xs, seen - 1)
}

fn main() {
    defer println("main defer ran")
    println("{}", apply(|v| v * 2, 4))
    let h = spawn(|| apply(|v| {
        if v > 1 {
            panic("closure refused {}", v)
        }
        v
    }, 5))
    println("{}", h.join())
    println("{}", guarded(#[4, 5], "pair"))
}
"#;

/// Builds `PROGRAM` with `unwind` as the landing-pad entry and runs it.
fn native_run(unwind: Option<&str>, release: bool) -> std::process::Output {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "gos-unwind-entries-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    let file = dir.join("main.gos");
    std::fs::write(&file, PROGRAM).expect("write the source");
    let out_dir = dir.join("out");
    let mut build = std::process::Command::new(env!("CARGO_BIN_EXE_gos"));
    build.arg("build");
    if release {
        build.arg("--release");
    }
    build.arg("--out-dir").arg(&out_dir).arg(&file);
    match unwind {
        Some(entry) => build.env("GOS_LLVM_UNWIND", entry),
        None => build.env_remove("GOS_LLVM_UNWIND"),
    };
    let build_output = build.output().expect("spawn gos build");
    assert!(
        build_output.status.success(),
        "gos build failed:\n{}",
        String::from_utf8_lossy(&build_output.stderr)
    );
    let out = std::process::Command::new(native_executable(&out_dir, "main"))
        .output()
        .expect("spawn the program");
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// The report's message and notes, without the call stack, which a release
/// build renders without source positions.
fn report(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .take_while(|line| !line.contains("call stack"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_landing_pad_entry_runs_the_deferred_expressions_a_panic_leaves() {
    let vm = gos_run_on(Tier::Bytecode, PROGRAM, None, &[]);
    assert_eq!(vm.status.code(), Some(101));
    assert!(
        stdout(&vm).contains("guarded defer saw 9 for pair"),
        "{}",
        stdout(&vm)
    );
    for unwind in [None, Some("catching")] {
        for release in [false, true] {
            let native = native_run(unwind, release);
            let label = format!("{unwind:?} release={release}");
            assert_eq!(native.status.code(), Some(101), "{label}");
            assert_eq!(stdout(&native), stdout(&vm), "{label}");
            assert_eq!(report(&native), report(&vm), "{label}");
        }
    }
}
