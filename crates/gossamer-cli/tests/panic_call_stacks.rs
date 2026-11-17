//! A panic report names every frame between `main` and the raise, with the
//! line each one stands at, whether the call reached its callee through a
//! frame of its own or through a body the compiler placed in the caller.

use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

const INLINED_CALLS: &str = r#"fn bump(x: i64) -> i64 {
    let limit = 3
    if x > limit {
        panic("too big {}", x)
    }
    x + 1
}

fn outer(x: i64) -> i64 {
    bump(x) * 2
}

fn main() {
    println("{}", outer(1))
    println("{}", outer(9))
}
"#;

#[test]
fn bytecode_report_names_inlined_callees_and_their_call_sites() {
    let dir = std::env::temp_dir().join(format!("gos-call-stacks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("stack.gos"), INLINED_CALLS).expect("write program");
    let output = Command::new(gos_bin())
        .current_dir(&dir)
        .env("GOS_JIT", "0")
        .env("GOSSAMER_CACHE_DIR", dir.join("cache"))
        .args(["run", "stack.gos"])
        .output()
        .expect("spawn gos");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(101), "stderr:\n{stderr}");
    assert!(
        stderr.contains(
            "  call stack (outermost first):\n    at main (stack.gos:15:19)\n    at outer (stack.gos:10:5)\n    at bump (stack.gos:4:9)\n"
        ),
        "stderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

const JIT_INLINED_CALL: &str = r#"fn get(xs: Vec<i64>, i: i64) -> i64 {
    xs[i]
}

fn main() {
    let xs = #[1, 2, 3]
    let mut total = 0
    for i in 0..5 {
        total += get(xs, i)
    }
    println("{}", total)
}
"#;

#[test]
fn jit_report_names_inlined_callees_from_the_machine_stack() {
    let dir = std::env::temp_dir().join(format!("gos-jit-call-stacks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("jit.gos"), JIT_INLINED_CALL).expect("write program");
    let output = Command::new(gos_bin())
        .current_dir(&dir)
        .env("GOSSAMER_JIT_THRESHOLD", "1")
        .env("GOSSAMER_CACHE_DIR", dir.join("cache"))
        .args(["run", "jit.gos"])
        .output()
        .expect("spawn gos");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(101), "stderr:\n{stderr}");
    assert!(
        stderr.contains(
            "  call stack (outermost first):\n    at main (jit.gos:9:18)\n    at get (jit.gos:2:5)\n"
        ),
        "stderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn debug_build_report_matches_the_bytecode_report() {
    let dir = std::env::temp_dir().join(format!("gos-debug-call-stacks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("stack.gos"), INLINED_CALLS).expect("write program");
    let build = Command::new(gos_bin())
        .current_dir(&dir)
        .env("GOSSAMER_CACHE_DIR", dir.join("cache"))
        .args(["build", "stack.gos", "--out-dir", "out"])
        .output()
        .expect("spawn gos build");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir
        .join("out")
        .join(if cfg!(windows) { "stack.exe" } else { "stack" });
    let output = Command::new(&binary).output().expect("run the artifact");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(101), "stderr:\n{stderr}");
    assert!(
        stderr.contains(
            "  call stack (outermost first):\n    at main (stack.gos:15:19)\n    at outer (stack.gos:10:5)\n    at bump (stack.gos:4:9)\n"
        ),
        "stderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

const RECURSIVE_CALL: &str = r"use std::env

fn pick(xs: Vec<i64>, i: i64) -> i64 {
    if i > 0 && i % 7 == 3 {
        return xs[i]
    }
    if i == 0 { 0 } else { pick(xs, i - 1) + 1 }
}

fn main() {
    let n = env::args().len() as i64 + 5
    let xs = #[1, 2, 3]
    let mut total = 0
    for i in 0..n {
        total += pick(xs, i)
    }
    println(total)
}
";

/// A release ELF executable keeps only the symbols a report prints, and
/// its report still names the program's frames through them.
#[test]
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn release_report_names_program_frames_through_the_trimmed_symbol_table() {
    use object::{Object, ObjectSymbol, ObjectSymbolTable};

    let dir = std::env::temp_dir().join(format!("gos-release-call-stacks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("pick.gos"), RECURSIVE_CALL).expect("write program");
    let build = Command::new(gos_bin())
        .current_dir(&dir)
        .env("GOSSAMER_CACHE_DIR", dir.join("cache"))
        .args(["build", "--release", "pick.gos", "--out-dir", "out"])
        .output()
        .expect("spawn gos build");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join("out").join("pick");
    let output = Command::new(&binary).output().expect("run the artifact");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(101), "stderr:\n{stderr}");
    assert!(
        stderr.contains("  call stack (outermost first):\n    at main\n    at pick\n"),
        "stderr:\n{stderr}"
    );

    let data = std::fs::read(&binary).expect("read the artifact");
    let file = object::File::parse(&*data).expect("parse the artifact");
    let names: Vec<String> = file
        .symbol_table()
        .expect("a symbol table")
        .symbols()
        .filter_map(|s| s.name().ok().map(str::to_string))
        .filter(|n| !n.is_empty())
        .collect();
    assert!(names.iter().any(|n| n == "pick"), "{names:?}");
    assert!(
        names
            .iter()
            .all(|n| !n.starts_with("_ZN") && !n.starts_with("_R")),
        "{names:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
