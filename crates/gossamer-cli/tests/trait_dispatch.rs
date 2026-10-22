//! Trait dispatch correctness matrix.
//!
//! `feature-testing-examples/trait_object_dispatch.gos` is the
//! single trait-related example today. The 2026-04-28
//! `compiled_impl_method_dispatch.md` memo described four
//! coordinated fixes for impl-method dispatch through traits;
//! this file pins each shape so a regression in any of the four
//! turns the test red.
//!
//! Every test runs in all three tiers (VM, Cranelift debug, LLVM
//! release) and stdout must match the canonical expected output
//! byte-for-byte.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const PER_RUN_TIMEOUT: Duration = Duration::from_mins(1);

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fresh_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-trait-{pid}-{n}-{tag}",
        pid = std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn run_with_timeout(mut child: std::process::Child) -> (String, String, Option<i32>) {
    let deadline = Instant::now() + PER_RUN_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => break,
        }
    }
    let out = child.wait_with_output().expect("wait_with_output");
    (
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n"),
        out.status.code(),
    )
}

fn run_vm(src: &Path) -> (String, String, Option<i32>) {
    let child = Command::new(gos_bin())
        .arg("run")
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos");
    run_with_timeout(child)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

fn build_native(src: &Path, release: bool, scratch: &Path) -> Result<PathBuf, String> {
    let mut cmd = Command::new(gos_bin());
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--out-dir").arg(scratch).arg(src);
    let out = cmd.output().expect("spawn gos build");
    if !out.status.success() {
        return Err(format!(
            "gos build failed:\n  stderr: {}",
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    let mut binaries = Vec::new();
    for entry in fs::read_dir(scratch)
        .map_err(|e| format!("read_dir: {e}"))?
        .flatten()
    {
        let p = entry.path();
        if p.is_file() && is_executable(&p) {
            binaries.push(p);
        }
    }
    binaries
        .into_iter()
        .next()
        .ok_or_else(|| format!("no binary in {}", scratch.display()))
}

fn run_native(bin: &Path) -> (String, String, Option<i32>) {
    let child = Command::new(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn binary");
    run_with_timeout(child)
}

fn assert_three_tier_parity(tag: &str, source: &str, expected: &str) {
    let dir = fresh_dir(tag);
    let src = dir.join(format!("{tag}.gos"));
    let mut f = fs::File::create(&src).expect("write src");
    f.write_all(source.as_bytes()).unwrap();
    drop(f);

    let vm = run_vm(&src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let cl_bin = build_native(&src, false, &cl_dir).expect("cranelift build");
    let cl = run_native(&cl_bin);
    let ll_dir = dir.join("ll");
    fs::create_dir_all(&ll_dir).unwrap();
    let ll_bin = build_native(&src, true, &ll_dir).expect("llvm build");
    let ll = run_native(&ll_bin);

    let _ = fs::remove_dir_all(&dir);

    for (name, run) in [("vm", &vm), ("cranelift", &cl), ("llvm", &ll)] {
        assert_eq!(
            run.0.trim_end(),
            expected.trim_end(),
            "[{tag}/{name}] disagrees with expected.\n\
             expected:\n{expected}\n\
             got stdout:\n{stdout}\n\
             stderr:\n{stderr}\n\
             exit: {code:?}",
            stdout = run.0,
            stderr = run.1,
            code = run.2,
        );
    }
}

#[test]
fn static_method_dispatch_on_concrete_struct() {
    // Methods on a concrete struct type are dispatched directly.
    // No trait, no dyn - the simplest case.
    let src = r#"
struct Counter { n: i64 }

impl Counter {
    fn step(&self) -> i64 { self.n + 1 }
    fn doubled(&self) -> i64 { self.n * 2 }
}

fn main() {
    let c = Counter { n: 7 }
    println("step={} doubled={}", c.step(), c.doubled())
}
"#;
    assert_three_tier_parity("static_method_dispatch", src, "step=8 doubled=14");
}

#[test]
fn unary_not_routes_to_the_not_impl_on_every_tier() {
    // `!value` on a user type reaches its `not` impl the way `-value`
    // reaches `neg`, and both route through the same nominal dispatch.
    let src = r#"
struct Mask { bits: i64 }

impl Not for Mask {
    fn not(&self) -> Mask { Mask { bits: 255 - self.bits } }
}

impl Neg for Mask {
    fn neg(&self) -> Mask { Mask { bits: 0 - self.bits } }
}

fn main() {
    let m = Mask { bits: 12 }
    println("not={} neg={}", (!m).bits, (-m).bits)
}
"#;
    assert_three_tier_parity("unary_not_impl", src, "not=243 neg=-12");
}

#[test]
fn trait_method_dispatched_via_concrete_self_type() {
    // Trait method dispatched on concrete self type. The 2026-
    // 04-28 `compiled_impl_method_dispatch.md` regression
    // (HashMap collision in the symbol table) showed up here:
    // the same trait method name on two different impls
    // collided into one symbol and the wrong body ran.
    let src = r#"
trait Greet {
    fn greet(&self) -> String
}

struct Cat { name: String }
struct Dog { name: String }

impl Greet for Cat {
    fn greet(&self) -> String { format("meow {}", self.name) }
}

impl Greet for Dog {
    fn greet(&self) -> String { format("woof {}", self.name) }
}

fn main() {
    let c = Cat { name: "tabby" }
    let d = Dog { name: "rex" }
    println("{}", c.greet())
    println("{}", d.greet())
}
"#;
    assert_three_tier_parity("trait_concrete_dispatch", src, "meow tabby\nwoof rex");
}

#[test]
fn trait_with_multi_field_self_routes_each_field() {
    // Trait impl on a multi-field struct (String + two i64s).
    // The 2026-04-28 memo's "aggregate param ABI" fix had the
    // wrong receiver field reaching the impl method when self
    // had ≥2 slots - the trait method read field 0 for every
    // field access. Test verifies each field threads through.
    let src = r#"
struct Inventory {
    name: String,
    stock: i64,
    weight: i64,
}

trait Summarize {
    fn summary(&self) -> String
}

impl Summarize for Inventory {
    fn summary(&self) -> String {
        format("{} stock={} weight={}", self.name, self.stock, self.weight)
    }
}

fn main() {
    let inv = Inventory { name: "widgets", stock: 42, weight: 17 }
    println("{}", inv.summary())
}
"#;
    assert_three_tier_parity("trait_multi_field_self", src, "widgets stock=42 weight=17");
}

#[test]
fn multiple_trait_methods_chosen_by_call_site() {
    // Multiple methods on the same trait, each impl has its own
    // body. The dispatch table must route each call to the
    // matching impl method without cross-contamination.
    let src = r#"
trait Shape {
    fn area(&self) -> f64
    fn name(&self) -> String
}

struct Square { side: f64 }
struct Circle { radius: f64 }

impl Shape for Square {
    fn area(&self) -> f64 { self.side * self.side }
    fn name(&self) -> String { "square" }
}

impl Shape for Circle {
    fn area(&self) -> f64 { 3.14159 * self.radius * self.radius }
    fn name(&self) -> String { "circle" }
}

fn main() {
    let s = Square { side: 4.0 }
    let c = Circle { radius: 1.0 }
    println("{}={:.2}", s.name(), s.area())
    println("{}={:.5}", c.name(), c.area())
}
"#;
    assert_three_tier_parity(
        "trait_multiple_methods",
        src,
        "square=16.00\ncircle=3.14159",
    );
}

#[test]
fn impl_methods_with_int_and_float_returns_coexist() {
    // Same struct with two methods returning different scalar
    // types (i64 and f64). The call-site type-pinning fix from
    // `compiled_impl_method_dispatch.md` ensures each method's
    // return ABI is correctly inferred at the dispatch boundary.
    let src = r#"
struct Mix { x: i64, y: f64 }

impl Mix {
    fn count(&self) -> i64 { self.x + 100 }
    fn ratio(&self) -> f64 { self.y * 2.5 }
}

fn main() {
    let m = Mix { x: 7, y: 4.0 }
    println("count={} ratio={:.1}", m.count(), m.ratio())
}
"#;
    assert_three_tier_parity("impl_mixed_returns", src, "count=107 ratio=10.0");
}
