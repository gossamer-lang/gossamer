//! Generic struct support: parameterised type definitions +
//! monomorphisation across instantiation sites.
//!
//! Pins the surface for `struct Pair<A, B> { ... }`. The
//! typechecker substitutes each `TyKind::Param` slot in the
//! declared field types with a fresh inference variable at the
//! literal construction site, drives inference of the generic
//! arguments from the field values, and substitutes back at
//! field-read sites so the per-instance concrete type is
//! available for arithmetic / method dispatch.
//!
//! Each test runs the program through the bytecode VM and asserts
//! its stdout. Release-tier parity is covered by the
//! `release_stability` suite which exercises a representative set
//! of programs through `gos build --release`.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_fixture(tag: &str, body: &str) -> PathBuf {
    let serial = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-generic-struct-{}-{}-{}",
        tag,
        std::process::id(),
        serial,
    ));
    fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join(format!("{tag}.gos"));
    fs::write(&path, body).expect("write");
    path
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> (String, String, bool) {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("subprocess did not terminate within {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("wait error: {e}"),
        }
    }
    let out = child.wait_with_output().expect("wait_with_output");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn run_vm(src: &Path) -> String {
    let mut cmd = Command::new(gos_bin());
    cmd.arg("run").arg(src);
    let (stdout, _stderr, _ok) = run_with_timeout(cmd, Duration::from_secs(30));
    stdout
}

fn assert_vm(tag: &str, src: &str, expected: &str) {
    let path = write_fixture(tag, src);
    let vm = run_vm(&path);
    assert_eq!(
        vm.trim_end(),
        expected.trim_end(),
        "[{tag}/vm] expected {expected:?}, got {vm:?}",
    );
}

#[test]
fn generic_pair_two_distinct_params() {
    let src = r#"
struct Pair<A, B> { fst: A, snd: B }
fn main() {
    let p = Pair { fst: 42, snd: "answer" }
    println("{} = {}", p.fst, p.snd)
}
"#;
    assert_vm("pair_two_params", src, "42 = answer");
}

#[test]
fn generic_struct_multiple_instantiations_in_one_program() {
    let src = r#"
struct Pair<A, B> { fst: A, snd: B }
fn main() {
    let a = Pair { fst: 1, snd: 2 }
    let b = Pair { fst: "x", snd: "y" }
    let c = Pair { fst: 7, snd: "seven" }
    println("{}/{}", a.fst, a.snd)
    println("{}/{}", b.fst, b.snd)
    println("{}/{}", c.fst, c.snd)
}
"#;
    assert_vm("pair_multi_instantiation", src, "1/2\nx/y\n7/seven");
}

#[test]
fn field_arithmetic_on_concrete_instance() {
    // Reading two `A`-typed fields of `Pair<A, B>` from the
    // same instance must produce the same concrete type so
    // `nums.fst + nums.snd` typechecks. Without per-instance
    // substitution at field-read sites this errors with
    // `expected A, found B` even though both fields are
    // `i64` in this instance.
    let src = r#"
struct Pair<A, B> { fst: A, snd: B }
fn main() {
    let p = Pair { fst: 10, snd: 32 }
    println("{}", p.fst + p.snd)
}
"#;
    assert_vm("pair_arith", src, "42");
}

#[test]
fn single_param_generic_box() {
    // Smallest possible generic: one type parameter, one field.
    let src = r#"
struct Cell<T> { value: T }
fn main() {
    let c = Cell { value: 99 }
    let s = Cell { value: "ninety-nine" }
    println("{} {}", c.value, s.value)
}
"#;
    assert_vm("cell_single_param", src, "99 ninety-nine");
}

#[test]
fn triple_param_generic_struct() {
    // Three type parameters; tests that ParamIdx tracking
    // distinguishes positions 0, 1, 2 correctly.
    let src = r#"
struct Triple<A, B, C> { a: A, b: B, c: C }
fn main() {
    let t = Triple { a: 1, b: "two", c: 3 }
    println("{} {} {}", t.a, t.b, t.c)
}
"#;
    assert_vm("triple_params", src, "1 two 3");
}

#[test]
fn same_param_twice_in_struct() {
    // Both fields share the same parameter `A`. Construction
    // succeeds when both values have the same concrete type;
    // a different-type construction would fail unification -
    // pinned here as the positive case.
    let src = r#"
struct SameType<A> { left: A, right: A }
fn main() {
    let pair = SameType { left: 7, right: 13 }
    println("{} + {} = {}", pair.left, pair.right, pair.left + pair.right)
}
"#;
    assert_vm("same_type_twice", src, "7 + 13 = 20");
}
