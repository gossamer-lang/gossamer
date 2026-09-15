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
