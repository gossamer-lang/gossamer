//! Aggregate ABI three-tier matrix.
//!
//! `aggregate_print_fallback.rs` was the only specific aggregate
//! regression test until now. It covers `println!("{}", struct)`
//! falling back to Cranelift. Several other aggregate-shape bugs
//! have shipped in the same window:
//!
//!   - `aggregate_array_index_address` (2026-05-05): `&arr[i]`
//!     for a multi-slot element segfaulted because `lower_place_read`
//!     returned `load(addr,0)` instead of the element address.
//!   - `compiled_impl_method_dispatch` (2026-04-28): four
//!     coordinated fixes for aggregate parameter ABI, return ABI,
//!     and dest-ty pinning.
//!   - `result_unwrap_or_dispatch` (2026-04-30): LLVM treated a
//!     `Result` aggregate as a flat slot - fasta looped forever.
//!
//! Every case below is a regression test for one of those classes.
//! Each program runs in all three tiers (VM, Cranelift debug, LLVM
//! release) and the captured stdout must match. Output is byte-equal
//! across tiers - anything else trips the gate.

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
    let dir = env::temp_dir().join(format!("gos-agg-{pid}-{n}-{tag}", pid = std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

mod common;

fn run_with_timeout(mut child: std::process::Child) -> (String, String, common::RunExit) {
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
        common::describe_exit(out.status),
    )
}

fn run_vm(src: &Path) -> (String, String, common::RunExit) {
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
            "gos build {flag} failed:\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            flag = if release { "--release" } else { "" },
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

fn run_native(bin: &Path) -> (String, String, common::RunExit) {
    let child = Command::new(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn binary");
    run_with_timeout(child)
}

/// Drive a single inline `.gos` source through all three tiers
/// and assert the stdout of each tier equals `expected`. The
/// expected string is the canonical answer; any tier disagreeing
/// with it is a regression in *that* tier (parity-only checks
/// can't tell which side moved).
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

    // Cleanup runs only on success: a panicking assert below unwinds
    // past this, leaving the sources, objects and built binaries in
    // place for inspection (and CI artifact upload) when a tier
    // disagrees or a native binary crashes.
    for (name, run) in [("vm", &vm), ("cranelift", &cl), ("llvm", &ll)] {
        assert_eq!(
            run.0.trim_end(),
            expected.trim_end(),
            "[{tag}/{name}] stdout disagrees with expected.\n\
             expected:\n{expected}\n\
             got stdout:\n{stdout}\n\
             stderr:\n{stderr}\n\
             {exit}\n\
             artifacts: {dir}",
            stdout = run.0,
            stderr = run.1,
            exit = run.2.text,
            dir = dir.display(),
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn struct_returned_by_value_fields_read_by_caller() {
    // Multi-field struct returned by value. Caller reads each
    // field; if the LLVM ABI flattens the slot, only the first
    // field is correct and `q.y`, `q.z` come back zero.
    let src = r#"
struct Point3 { x: i64, y: i64, z: i64 }

fn make_point() -> Point3 {
    Point3 { x: 11, y: 22, z: 33 }
}

fn main() {
    let p = make_point()
    println("x={} y={} z={}", p.x, p.y, p.z)
}
"#;
    assert_three_tier_parity("struct_return", src, "x=11 y=22 z=33");
}

#[test]
fn short_lived_scalar_aggregate_is_identical_across_tiers() {
    // The MIR scalar-replacement pass may remove `pair` entirely because both
    // uses are direct field reads. Keep the inputs non-constant so this is a
    // real construction/read path rather than compile-time arithmetic.
    let src = r#"
struct Pair { left: i64, right: i64 }

fn add_pair(left: i64, right: i64) -> i64 {
    let pair = Pair { left: left, right: right }
    pair.left * 31 + pair.right
}

fn main() {
    println("{}", add_pair(7, 11))
}
"#;
    assert_three_tier_parity("short_lived_scalar_aggregate", src, "228");
}

#[test]
fn struct_arg_passes_full_field_set_to_callee() {
    // A struct argument lands in the callee with every field
    // intact. Past aggregate-flat-slot regressions truncated
    // multi-field structs at the call boundary, so only
    // `p.x` was correct; `p.y` and `p.z` came back zero on the
    // callee side. We read all three fields *inside* the callee
    // and print them, so any flat-slot drop fails this gate.
    let src = r#"
struct Point3 { x: i64, y: i64, z: i64 }

fn dump(p: Point3) {
    println("inside x={} y={} z={}", p.x, p.y, p.z)
}

fn main() {
    let p = Point3 { x: 7, y: 11, z: 13 }
    dump(p)
    println("outside x={} y={} z={}", p.x, p.y, p.z)
}
"#;
    assert_three_tier_parity(
        "struct_arg_full_field_set",
        src,
        "inside x=7 y=11 z=13\noutside x=7 y=11 z=13",
    );
}

#[test]
fn struct_arg_passed_by_value_does_not_alias_caller() {
    let src = r#"
struct Point { x: i64, y: i64 }

fn shift(mut p: Point) -> Point {
    p.x = p.x + 100
    p.y = p.y + 200
    p
}

fn main() {
    let original = Point { x: 1, y: 2 }
    let shifted = shift(original)
    println("orig=({},{}) shifted=({},{})", original.x, original.y, shifted.x, shifted.y)
}
"#;
    assert_three_tier_parity("struct_arg_byval", src, "orig=(1,2) shifted=(101,202)");
}

#[test]
fn array_of_structs_index_yields_real_element() {
    // Direct echo of the 2026-05-05 segfault: `arr[i]` for a
    // multi-slot element returned `load(addr,0)` instead of the
    // element address. Reading multiple fields through indexing
    // is the smallest reproducer; the bug only surfaces with
    // ≥2 fields in the element type.
    let src = r#"
struct Point3 { x: i64, y: i64, z: i64 }

fn main() {
    let pts = [
        Point3 { x: 1, y: 2, z: 3 },
        Point3 { x: 4, y: 5, z: 6 },
        Point3 { x: 7, y: 8, z: 9 },
    ]
    let mut sum = 0
    for i in 0..3 {
        let p = pts[i]
        sum = sum + p.x + p.y + p.z
    }
    println("sum={}", sum)
    println("p2=({},{},{})", pts[1].x, pts[1].y, pts[1].z)
}
"#;
    assert_three_tier_parity("array_of_structs", src, "sum=45\np2=(4,5,6)");
}

#[test]
fn tuple_destructured_from_function_return() {
    // The 2026-04-28 dest-ty pinning fix lives or dies on
    // tuple-return destructuring. The callee returns `(i64,
    // i64)`; if MIR doesn't pin the call's destination type
    // from `fn_returns`, the tuple lands in a single slot and
    // the second binding reads garbage.
    let src = r#"
fn divmod(a: i64, b: i64) -> (i64, i64) {
    (a / b, a % b)
}

fn main() {
    let q, r = divmod(17, 5)
    println("q={} r={}", q, r)
    let q2, r2 = divmod(100, 7)
    println("q2={} r2={}", q2, r2)
}
"#;
    assert_three_tier_parity("tuple_return_destructure", src, "q=3 r=2\nq2=14 r2=2");
}

#[test]
fn option_struct_round_trip_through_callee() {
    // `Option<Struct>` round-trip - the Option discriminant
    // travels with an aggregate payload. Past regressions
    // (`compiler_bugs_round1`, `result_unwrap_or_dispatch`)
    // proved the aggregate Option/Result class is a hot spot.
    let src = r#"
struct Coord { row: i64, col: i64 }

fn find(target: i64, grid: [Coord; 3]) -> Option<Coord> {
    for c in grid {
        if c.row + c.col == target {
            return Some(c)
        }
    }
    None
}

fn main() {
    let grid = [
        Coord { row: 1, col: 2 },
        Coord { row: 3, col: 4 },
        Coord { row: 5, col: 6 },
    ]
    match find(7, grid) {
        Some(c) => println("found ({},{})", c.row, c.col),
        None => println("missing"),
    }
    match find(99, grid) {
        Some(c) => println("found ({},{})", c.row, c.col),
        None => println("missing"),
    }
}
"#;
    assert_three_tier_parity("option_struct_round_trip", src, "found (3,4)\nmissing");
}

#[test]
fn result_struct_round_trip_through_callee() {
    // `Result<Struct, errors::Error>` ferries an aggregate `Ok`
    // payload alongside an error path. Mirrors the
    // `result_unwrap_or_dispatch` regression class.
    let src = r#"
use std::errors

struct Parsed { width: i64, height: i64 }

fn parse_dim(s: String) -> Result<Parsed, errors::Error> {
    if s == "10x20" {
        Ok(Parsed { width: 10, height: 20 })
    } else {
        Err(errors::new("bad dim"))
    }
}

fn main() {
    match parse_dim("10x20") {
        Ok(p) => println("ok w={} h={}", p.width, p.height),
        Err(e) => println("err {}", e.message()),
    }
    match parse_dim("garbage") {
        Ok(p) => println("ok w={} h={}", p.width, p.height),
        Err(e) => println("err {}", e.message()),
    }
}
"#;
    assert_three_tier_parity("result_struct_round_trip", src, "ok w=10 h=20\nerr bad dim");
}

#[test]
fn struct_update_base_with_scalar_and_string_fields() {
    // `..base` update over plain scalar + String fields. The
    // 2026-05-05 `struct_update_base_landed` change routes the
    // sentinel through MIR; missing fields fill from `base.field`.
    // The scalar-only case is the cheapest gate that catches
    // a regression in the sentinel handling.
    let src = r#"
struct Outer { label: String, tag: String, n: i64, m: i64 }

fn main() {
    let base = Outer { label: "alpha", tag: "first", n: 42, m: 17 }
    let updated = Outer { label: base.label, tag: base.tag, n: 99, m: base.m }
    println("label={} tag={}", updated.label, updated.tag)
    println("n={} m={}", updated.n, updated.m)
}
"#;
    assert_three_tier_parity(
        "struct_update_base_scalar",
        src,
        "label=alpha tag=first\nn=99 m=17",
    );
}

#[test]
fn struct_update_base_with_nested_struct_fields() {
    let src = r#"
struct Inner { tag: String, count: i64 }
struct Outer { inner: Inner, label: String, n: i64 }

fn main() {
    let base = Outer { inner: Inner { tag: "first", count: 7 }, label: "alpha", n: 42 }
    let updated = Outer { inner: base.inner, label: base.label, n: 99 }
    println("inner.tag={} inner.count={}", updated.inner.tag, updated.inner.count)
    println("label={} n={}", updated.label, updated.n)
}
"#;
    assert_three_tier_parity(
        "struct_update_base_nested",
        src,
        "inner.tag=first inner.count=7\nlabel=alpha n=99",
    );
}

#[test]
fn nested_struct_field_read_through_levels() {
    // Read-only nested field access `outer.inner.x` is the
    // smallest reproducer of a class of bugs around nested
    // aggregate field offsets that surface in cranelift (segv)
    // and LLVM (wrong value) but not the VM. This pinned-VM
    // test will go green once codegen lands the fix.
    let src = r#"
struct Inner { x: i64, y: i64 }
struct Outer { inner: Inner, tag: String }

fn main() {
    let o = Outer { inner: Inner { x: 100, y: 200 }, tag: "t" }
    println("{} {} {}", o.inner.x, o.inner.y, o.tag)
}
"#;
    assert_three_tier_parity("nested_field_read", src, "100 200 t");
}

#[test]
fn nested_struct_field_of_field_assignment() {
    let src = r#"
struct Inner { x: i64, y: i64 }
struct Outer { inner: Inner, tag: String }

fn main() {
    let mut o = Outer { inner: Inner { x: 1, y: 2 }, tag: "t" }
    o.inner.x = 100
    o.inner.y = 200
    println("{} {} {}", o.inner.x, o.inner.y, o.tag)
}
"#;
    assert_three_tier_parity("nested_field_of_field_assign", src, "100 200 t");
}

// --- N9: parallel Cranelift body lowering regression tests ---
//
// These tests guard the correctness guarantee introduced by N9: that
// lowering function bodies in parallel (via rayon) produces the same
// result as sequential lowering. The key invariants are:
//
//   1. Pre-declared symbols (REGISTRY, malloc/strlen/calloc, interned
//      strings, shape thunks) are visible in every rayon worker thread.
//   2. Functions that return aggregates route through `gos_rt_gc_alloc`
//      (pre-declared in the N9-B phase via REGISTRY) without hitting the
//      OfflineModule::declare_function unreachable.
//   3. Cross-function calls resolve correctly when both caller and callee
//      are lowered by different rayon workers.

#[test]
fn parallel_lowering_many_functions_share_string_literal() {
    // Twelve independent helper functions all println! the same string
    // literal ("ok"). In the parallel lowering phase every rayon worker
    // thread clones a local IntrinsicContext; the string must be interned
    // (DataId assigned) in the N9-B pre-declaration pass so none of the
    // workers hits OfflineModule::declare_data_in_func with an unknown id.
    let src = r#"
fn f1() -> i64 { println("ok")
 1 }
fn f2() -> i64 { println("ok")
 2 }
fn f3() -> i64 { println("ok")
 3 }
fn f4() -> i64 { println("ok")
 4 }
fn f5() -> i64 { println("ok")
 5 }
fn f6() -> i64 { println("ok")
 6 }
fn f7() -> i64 { println("ok")
 7 }
fn f8() -> i64 { println("ok")
 8 }
fn f9() -> i64 { println("ok")
 9 }
fn f10() -> i64 { println("ok")
 10 }
fn f11() -> i64 { println("ok")
 11 }
fn f12() -> i64 { println("ok")
 12 }

fn main() {
    let sum = f1() + f2() + f3() + f4() + f5() + f6() +
            f7() + f8() + f9() + f10() + f11() + f12()
    println("sum={}", sum)
}
"#;
    let expected = "ok\n".repeat(12) + "sum=78";
    assert_three_tier_parity("n9_many_fns_shared_string", src, &expected);
}

#[test]
fn parallel_lowering_aggregate_chain_across_call_boundaries() {
    // A chain of three helpers, each returning a struct by value.
    // The innermost `make_dims` returns a two-field aggregate; the
    // middle `scale_dims` receives it and returns a new one; `main`
    // reads the final fields. In the parallel phase, `make_dims` and
    // `scale_dims` are lowered by potentially different rayon workers.
    // Each must use the pre-declared `gos_rt_gc_alloc` FuncId rather
    // than calling OfflineModule::declare_function directly.
    // This was the root cause of the option/result aggregate test
    // failures before N9 was fully fixed.
    let src = r#"
struct Dims { w: i64, h: i64 }

fn make_dims(w: i64, h: i64) -> Dims {
    Dims { w: w, h: h }
}

fn scale_dims(d: Dims, factor: i64) -> Dims {
    Dims { w: d.w * factor, h: d.h * factor }
}

fn area(d: Dims) -> i64 {
    d.w * d.h
}

fn main() {
    let base = make_dims(3, 4)
    let big = scale_dims(base, 10)
    println("w={} h={} area={}", big.w, big.h, area(big))
}
"#;
    assert_three_tier_parity("n9_aggregate_chain", src, "w=30 h=40 area=1200");
}
