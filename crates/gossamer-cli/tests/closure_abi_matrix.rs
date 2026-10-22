//! ABI matrix for `Fn(...) -> ...` closure invocation across all
//! shape combinations the compiled tiers must handle.
//!
//! Pins the call boundary that the 2026-05-14 Windows `halved[0] = 2`
//! incident exposed: `gos_rt_vec_get_i64` is the i64-shaped read used
//! for every `Vec<T>` element load. The LLVM lowerer used to emit
//! `call double @gos_rt_vec_get_i64(...)` for `Vec<f64>` reads because
//! the destination's MIR type drove the call instruction's return
//! type. On `SysV` LLVM 18 that mismatch was silently normalised; on
//! mingw-w64-x86_64-llvm 18 the caller honoured the call-site type
//! literally and read `xmm0` while the function had written `rax`,
//! yielding stale FP state and a wrong displayed value. Now the call
//! emits `call i64 @gos_rt_vec_get_i64` + `bitcast i64 → double` so
//! the calling convention picks the correct return register on every
//! `x86_64` ABI.
//!
//! Each entry exercises one cell of the matrix:
//!   inputs:  `i64`, `f64`, `(f64, f64)`
//!   output:  `i64`, `f64`
//!   closure: bare-fn item, non-capturing closure, capturing closure
//!   indirect: through `Fn(...)` trait param
//!   sink:    return + store into `Vec<T>` element (the read is the
//!            failure point in the original incident)
//!
//! The check is three-tier parity. The bug only surfaced because the
//! interpreter (VM) computed the right value while the two compiled
//! tiers diverged - pinning all three keeps the regression visible
//! the moment any cell drifts.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fresh_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-abi-matrix-{pid}-{n}-{tag}",
        pid = std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch");
    dir
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &std::path::Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

fn build_native(src: &std::path::Path, release: bool, scratch: &std::path::Path) -> PathBuf {
    let mut cmd = Command::new(gos_bin());
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--out-dir").arg(scratch).arg(src);
    let out = cmd.output().expect("spawn gos build");
    assert!(
        out.status.success(),
        "gos build {flag} failed:\n  stdout: {stdout}\n  stderr: {stderr}",
        flag = if release { "--release" } else { "" },
        stdout = String::from_utf8_lossy(&out.stdout),
        stderr = String::from_utf8_lossy(&out.stderr),
    );
    fs::read_dir(scratch)
        .expect("read_dir scratch")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file() && is_executable(p))
        .unwrap_or_else(|| panic!("gos build produced no executable in {}", scratch.display()))
}

fn run_vm(src: &std::path::Path) -> String {
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn gos");
    assert!(
        out.status.success(),
        "vm run failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

fn run_native(bin: &std::path::Path) -> String {
    let out = Command::new(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn binary");
    assert!(
        out.status.success(),
        "native run failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

/// Runs `source` through all three tiers and asserts the captured
/// stdout matches `expected`. The fix this suite pins works only when
/// every tier produces the same bytes; a regression in the LLVM call
/// signature would diverge LLVM from VM, and a regression in
/// Cranelift's per-shape thunk would diverge Cranelift from VM.
fn assert_three_tier(tag: &str, source: &str, expected: &str) {
    let dir = fresh_dir(tag);
    let src = dir.join(format!("{tag}.gos"));
    fs::File::create(&src)
        .expect("create src")
        .write_all(source.as_bytes())
        .expect("write src");

    let vm = run_vm(&src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let cl_bin = build_native(&src, false, &cl_dir);
    let cl = run_native(&cl_bin);
    let ll_dir = dir.join("ll");
    fs::create_dir_all(&ll_dir).unwrap();
    let ll_bin = build_native(&src, true, &ll_dir);
    let ll = run_native(&ll_bin);

    let _ = fs::remove_dir_all(&dir);

    for (tier, got) in [("vm", &vm), ("cranelift", &cl), ("llvm", &ll)] {
        assert_eq!(
            got.trim_end(),
            expected.trim_end(),
            "[{tag}/{tier}] stdout diverges:\nexpected:\n{expected}\ngot:\n{got}",
        );
    }
}

#[test]
fn fn_i64_to_i64_indirect_through_vec_read() {
    // Baseline: integer closure threaded through a Vec<i64>. The
    // pre-fix LLVM emitter typed the call as `i64` already (since the
    // destination was i64), so this cell was always correct - but the
    // matrix needs to pin the baseline to catch any future drift that
    // would erase it back to a generic shape.
    let src = r#"
fn map_i64(xs: Vec<i64>, f: Fn(i64) -> i64) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::from([])
    for x in xs { out.push(f(x)) }
    out
}

fn main() {
    let xs = Vec::from([1, 2, 3, 4])
    let mapped = map_i64(xs, |x| x * 10)
    println("mapped[0]={}", mapped[0])
    println("mapped[3]={}", mapped[3])
}
"#;
    assert_three_tier("fn_i64_to_i64", src, "mapped[0]=10\nmapped[3]=40");
}

#[test]
fn fn_f64_to_f64_indirect_through_vec_read() {
    // The exact failing shape: read an f64 element written via a
    // closure result through `gos_rt_vec_get_i64`. The original bug
    // emitted `call double @gos_rt_vec_get_i64` here; the fix forces
    // the call to be `call i64 ...` + `bitcast i64 to double`, so the
    // caller reads `rax` (the actual return register) instead of
    // `xmm0` (stale).
    let src = r#"
fn map_f64(xs: Vec<f64>, f: Fn(f64) -> f64) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::from([])
    for x in xs { out.push(f(x)) }
    out
}

fn main() {
    let xs: Vec<f64> = [1.0, 2.0, 3.0, 4.0].to_vec()
    let halved = map_f64(xs, |x| x * 0.5)
    println("halved[0]={}", halved[0])
    println("halved[3]={}", halved[3])
}
"#;
    assert_three_tier("fn_f64_to_f64", src, "halved[0]=0.5\nhalved[3]=2");
}

#[test]
fn fn_f64_to_i64_indirect_through_vec_read() {
    // f64 input, i64 output: closure dispatches on f64 magnitude and
    // returns an integer. Catches the "double → i64" bitcast
    // direction at the Vec push site (the closure's return is
    // bitcast into i64 storage shape before vec_push). Avoid `x as
    // i64` so the closure-return bitcast - not scalar-cast lowering
    // - is the path under test.
    let src = r#"
fn map_f64_to_i64(xs: Vec<f64>, f: Fn(f64) -> i64) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::from([])
    for x in xs { out.push(f(x)) }
    out
}

fn main() {
    let xs: Vec<f64> = [1.5, 2.5, 3.5, 4.5].to_vec()
    let buckets = map_f64_to_i64(xs, |x| if x > 3.0 { 100 } else { 7 })
    println("buckets[0]={}", buckets[0])
    println("buckets[3]={}", buckets[3])
}
"#;
    assert_three_tier("fn_f64_to_i64", src, "buckets[0]=7\nbuckets[3]=100");
}

#[test]
fn fn_i64_to_f64_indirect_through_vec_read() {
    // i64 input, f64 output: opposite direction, exercises the
    // "i64 → double" bitcast on the closure result before it lands in
    // a Vec<f64> slot. Avoid `as f64` so the closure-result bitcast
    // - not scalar-cast lowering - is the path under test.
    let src = r#"
fn map_i64_to_f64(xs: Vec<i64>, f: Fn(i64) -> f64) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::from([])
    for x in xs { out.push(f(x)) }
    out
}

fn main() {
    let xs = Vec::from([2, 4, 6, 8])
    let weights = map_i64_to_f64(xs, |x| if x > 5 { 1.5 } else { 0.25 })
    println("weights[0]={}", weights[0])
    println("weights[3]={}", weights[3])
}
"#;
    assert_three_tier("fn_i64_to_f64", src, "weights[0]=0.25\nweights[3]=1.5");
}

#[test]
fn fn_two_f64_to_f64_indirect_through_vec_read() {
    // Two f64 inputs: the per-shape thunk `__fn_thunk_ff_f` forwards
    // both args (xmm1, xmm2 on Win64 - note Win64 skips xmm0 for
    // the env-pointer slot) before tail-jumping. A regression in the
    // multi-arg float case would show up here distinct from the
    // single-arg case above.
    let src = r#"
fn reduce_f64(xs: Vec<f64>, init: f64, f: Fn(f64, f64) -> f64) -> f64 {
    let mut acc = init
    for x in xs { acc = f(acc, x) }
    acc
}

fn main() {
    let xs: Vec<f64> = [1.5, 2.5, 3.5, 4.5].to_vec()
    let total = reduce_f64(xs, 0.0, |acc, x| acc + x)
    println("total={}", total)
    let scaled = reduce_f64(xs, 1.0, |acc, x| acc * x)
    println("scaled={}", scaled)
}
"#;
    assert_three_tier("fn_ff_to_f", src, "total=12\nscaled=59.0625");
}

#[test]
fn bare_fn_item_f64_through_vec_read() {
    // Non-closure: a free function passed where a `Fn(f64) -> f64` is
    // expected. The MIR coerces the bare fn item into a closure env
    // whose env[0] is the per-shape thunk (`__fn_thunk_f_f`) and
    // env[8] is the bare function's address. The thunk tail-jumps
    // forwarding the f64 in xmm0 (SysV) / xmm1 → xmm0 (Win64). This
    // is the path that previously read `xmm0` for the result through
    // the broken `gos_rt_vec_get_i64` call site.
    let src = r#"
fn double(x: f64) -> f64 { x * 2.0 }

fn map_f64(xs: Vec<f64>, f: Fn(f64) -> f64) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::from([])
    for x in xs { out.push(f(x)) }
    out
}

fn main() {
    let xs: Vec<f64> = [0.25, 0.5, 1.0, 2.0].to_vec()
    let doubled = map_f64(xs, double)
    println("doubled[0]={}", doubled[0])
    println("doubled[3]={}", doubled[3])
}
"#;
    assert_three_tier("bare_fn_f", src, "doubled[0]=0.5\ndoubled[3]=4");
}

#[test]
fn capturing_closure_f64_through_vec_read() {
    // Capturing closure: the env holds [thunk_ptr, body_addr,
    // captured_value...] and the body is a lifted function whose
    // first param is the env. The call site loads env[0] as the
    // thunk address and forwards `(env, x)`; the thunk loads the
    // body from env[8] and tail-jumps with `(env, x)`. f64 args
    // travel in xmm1 across both ABIs because env occupies slot 0.
    let src = r#"
fn map_f64(xs: Vec<f64>, f: Fn(f64) -> f64) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::from([])
    for x in xs { out.push(f(x)) }
    out
}

fn main() {
    let xs: Vec<f64> = [1.0, 2.0, 3.0, 4.0].to_vec()
    let bias = 100.0
    let shifted = map_f64(xs, |x| x + bias)
    println("shifted[0]={}", shifted[0])
    println("shifted[3]={}", shifted[3])
}
"#;
    assert_three_tier("capture_f", src, "shifted[0]=101\nshifted[3]=104");
}
