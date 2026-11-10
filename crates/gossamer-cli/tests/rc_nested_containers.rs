//! Regression tests for compiled-tier reference counting of enums whose
//! payloads are containers (`Vec`), tuples, and other enums - the shapes
//! a JSON-value tree exercises. Each program is run on the interpreter
//! (the semantic oracle) and as a `gos build --release` binary under
//! `MALLOC_CHECK_=3`; the two outputs must match and the native binary
//! must exit cleanly (no double-free / use-after-free).
//!
//! These cover three drop-pass / RC bugs fixed together:
//!   1. A `for x in xs` loop element loaded via a terminator-position
//!      `gos_load` was wrongly treated as owned and released each
//!      iteration, freeing the container's elements.
//!   2. A `Vec` stored into a returned enum (`J::Arr(v)`) was freed by
//!      the drop pass, dangling the returned enum's child pointer.
//!   3. The `vec_push` escape-propagation ran in a separate pass from the
//!      `gos_store` rule, so deeply nested `outer.push(J::Arr(inner))`
//!      shapes lost the innermost container.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        // On Windows the built binary is `<stem>.exe`; match that and
        // exclude the `.gos` source / `.pdb` debug file that share the dir.
        p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    }
}

/// Runs `src` on the VM and as a native release binary (under
/// `MALLOC_CHECK_=3`), asserting identical trimmed stdout and a clean
/// native exit.
fn assert_vm_matches_native(tag: &str, src: &str) {
    let dir = env::temp_dir().join(format!("gos-rcnest-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join(format!("{tag}.gos"));
    std::fs::write(&source, src).unwrap();

    let vm = Command::new(gos_bin())
        .arg("run")
        .arg(&source)
        .output()
        .expect("spawn gos");
    assert!(
        vm.status.success(),
        "[{tag}] vm run failed: {}",
        String::from_utf8_lossy(&vm.stderr)
    );
    let vm_out = String::from_utf8_lossy(&vm.stdout).trim_end().to_string();

    let build = Command::new(gos_bin())
        .arg("build")
        .arg("--release")
        .arg("--out-dir")
        .arg(&dir)
        .arg(&source)
        .output()
        .expect("spawn gos build --release");
    assert!(
        build.status.success(),
        "[{tag}] release build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bin = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| is_executable(p))
        .expect("no built binary");

    // Run native twice: with the recycling RC pool (default - exercises
    // the pool's reuse path, validated by output equality) and with
    // `GOS_RC_NO_POOL=1` (every free routes through libc so `MALLOC_CHECK_`
    // retains full double-free / use-after-free detection).
    for (mode, extra) in [("pooled", None), ("no-pool", Some(("GOS_RC_NO_POOL", "1")))] {
        let mut cmd = Command::new(&bin);
        cmd.env("MALLOC_CHECK_", "3");
        if let Some((k, v)) = extra {
            cmd.env(k, v);
        }
        let native = cmd.output().expect("run native");
        assert!(
            native.status.success(),
            "[{tag}/{mode}] native binary crashed (exit {:?}); stderr: {}",
            native.status.code(),
            String::from_utf8_lossy(&native.stderr)
        );
        let native_out = String::from_utf8_lossy(&native.stdout)
            .trim_end()
            .to_string();
        assert_eq!(
            vm_out, native_out,
            "[{tag}/{mode}] vm vs native output mismatch"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn iterate_vec_of_enums_from_enum_payload() {
    // Bug 1: `for x in xs` over a `Vec<J>` extracted from an enum match
    // arm released each borrowed element.
    assert_vm_matches_native(
        "iter",
        r#"
enum J { Int(i64), Arr(Vec<J>) }
fn sz(j: J) -> i64 {
    match j { J::Int(n) => *n, J::Arr(xs) => { let mut t = 0
 for x in xs { t += sz(x) }
 t } }
}
fn mkarr() -> J { let mut v: Vec<J> = Vec::from([])
 v.push(J::Int(1))
 v.push(J::Int(2))
 v.push(J::Int(3))
 J::Arr(v) }
fn main() {
    let mut i = 0
    let mut total = 0
    while i < 100 { let a = mkarr()
 total += sz(a)
 i += 1 }
    println("{}", total)
}
"#,
    );
}

#[test]
fn vec_stored_into_returned_enum_survives() {
    // Bug 2: the `Vec` stored into the returned `J::Arr(v)` was freed at
    // the constructing function's return.
    assert_vm_matches_native(
        "ret",
        r#"
enum J { Int(i64), Arr(Vec<J>), Obj(Vec<(String, J)>) }
fn sumj(j: J) -> i64 {
    match j {
        J::Int(n) => *n,
        J::Arr(xs) => { let mut t = 0
 for x in xs { t += sumj(x) }
 t }
        J::Obj(ps) => { let mut t = 0
 for p in ps { t += sumj(p.1) }
 t }
    }
}
fn mkobj(i: i64) -> J {
    let mut v: Vec<(String, J)> = Vec::from([])
    v.push((format("k{}", i), J::Int(10)))
    J::Obj(v)
}
fn main() {
    let mut i = 0
    let mut total = 0
    while i < 100 { let o = mkobj(i)
 total += sumj(o)
 i += 1 }
    println("{}", total)
}
"#,
    );
}

#[test]
fn deeply_nested_enum_in_vec_in_enum() {
    // Bug 3: `outer.push(J::Arr(inner))` then `J::Arr(outer)` - the
    // innermost `Vec` was freed because vec-push escape did not compose
    // with the gos_store escape rule.
    assert_vm_matches_native(
        "nest",
        r#"
enum J { Int(i64), Arr(Vec<J>) }
fn cnt(j: J) -> i64 {
    match j { J::Int(_) => 1, J::Arr(xs) => { let mut t = 0
 for x in xs { t += cnt(x) }
 t } }
}
fn build() -> J {
    let mut inner: Vec<J> = Vec::from([])
    inner.push(J::Int(1))
    let mut outer: Vec<J> = Vec::from([])
    outer.push(J::Arr(inner))
    J::Arr(outer)
}
fn main() {
    let mut i = 0
    let mut total = 0
    while i < 100 { let v = build()
 total += cnt(v)
 i += 1 }
    println("{}", total)
}
"#,
    );
}

#[test]
fn vec_valued_map_overwrite_and_remove_balance_ownership() {
    // A map owns a Vec share per stored entry. This exercises the complete
    // lifecycle: source binding cleanup, overwrite of the old map entry,
    // explicit removal, and final map teardown. Run enough repetitions to
    // make an old leak-suppression implementation observable under the native
    // allocator checks as well as comparing VM/AOT output.
    assert_vm_matches_native(
        "map-vec",
        r#"
use std::collections::Map

fn main() {
    let mut total = 0i64
    let mut round = 0i64
    while round < 100i64 {
        let mut m: Map<i64, Vec<i64>> = Map::new()
        let first: Vec<i64> = Vec::from([round, round + 1i64])
        m.insert(1i64, first)
        m.insert(1i64, Vec::from([round + 2i64, round + 3i64]))
        if let Some(v) = m.get(1i64) { total += v[0] }
        m.remove(1i64)
        round += 1i64
    }
    println("{}", total)
}
"#,
    );
}

#[test]
fn container_pop_and_error_exit_release_every_owner_once() {
    // Covers the two transition edges that are easiest to get wrong when a
    // container value changes hands: `pop` transfers the map/Vec share to the
    // receiving binding, while an early `Err` must release every still-local
    // owner without touching an entry that was already popped. Repetition and
    // the no-pool allocator mode make both leaks and double releases visible.
    assert_vm_matches_native(
        "pop-error",
        r#"
use std::{collections::{Map, Set}, errors}

fn one_round(round: i64) -> Result<i64, errors::Error> {
    let mut rows: Vec<Vec<i64>> = Vec::from([])
    let first: Vec<i64> = Vec::from([round, round + 1i64])
    rows.push(first)
    let tail: Vec<i64> = Vec::from([round + 2i64, round + 3i64])
    rows.push(tail)
    let popped_row = rows.pop().unwrap()

    let mut m: Map<i64, Vec<i64>> = Map::new()
    m.insert(1i64, popped_row)
    let retained: Vec<i64> = Vec::from([round + 4i64])
    m.insert(2i64, retained)
    let popped_map = Map::pop(m, 1i64).unwrap()
    let mut tags: Set<String> = Set::new()
    tags.insert(format("round-{}", round))
    tags.insert(format("next-{}", round + 1i64))
    if round % 2i64 == 0i64 {
        return Err(errors::new("expected short-circuit"))
    }
    Ok(popped_map[0] + rows[0][1])
}

fn main() {
    let mut total = 0i64
    let mut round = 0i64
    while round < 200i64 {
        match one_round(round) {
            Ok(v) => total += v,
            Err(_) => total += 1i64,
        }
        round += 1i64
    }
    println("{}", total)
}
"#,
    );
}

#[test]
fn result_returning_recursive_builder_keeps_payload() {
    // Bug 4: a value wrapped in `Ok(...)` and returned (the
    // `self.parse()?` recursive-descent shape) had its RC payload
    // released while the returned Result still referenced it, dropping a
    // node from every parsed tree. `gos_rt_result_new` now acquires its
    // payload.
    assert_vm_matches_native(
        "result",
        r#"
use std::errors
enum J { Int(i64), Arr(Vec<J>), Obj(Vec<(String, J)>) }
struct P { step: i64 }
impl P {
    fn pval(&mut self) -> Result<J, errors::Error> {
        self.step += 1
        let s = self.step
        if s == 2 { self.parr() } else if s == 3 { self.pobj() } else { Ok(J::Int(1)) }
    }
    fn parr(&mut self) -> Result<J, errors::Error> {
        let mut xs: Vec<J> = Vec::from([])
        xs.push(J::Int(1))
 xs.push(J::Int(1))
 xs.push(J::Int(1))
        Ok(J::Arr(xs))
    }
    fn pobj(&mut self) -> Result<J, errors::Error> {
        let mut ps: Vec<(String, J)> = Vec::from([])
        ps.push((format("d"), J::Int(1)))
        Ok(J::Obj(ps))
    }
    fn top(&mut self) -> Result<J, errors::Error> {
        let mut ps: Vec<(String, J)> = Vec::from([])
        let mut i = 0
        loop { let k = format("k{}", i)
 let v = self.pval()?
 ps.push((k, v))
 i += 1
 if i >= 3 { break } }
        Ok(J::Obj(ps))
    }
}
fn cnt(j: J) -> i64 {
    match j {
        J::Int(n) => *n,
        J::Arr(xs) => { let mut t = 0
 for x in xs { t += cnt(x) }
 t }
        J::Obj(ps) => { let mut t = 0
 for p in ps { t += cnt(p.1) }
 t }
    }
}
fn main() -> Result<(), errors::Error> {
    let mut total = 0
    let mut i = 0
    while i < 50 { let mut p = P { step: 0 }
 let r = p.top()?
 total += cnt(r)
 i += 1 }
    println("{}", total)
    Ok(())
}
"#,
    );
}

#[test]
fn by_value_transform_rebuilds_tree() {
    // By-value enum consumption that moves the Vec and each element out,
    // recursively rebuilding a new tree.
    assert_vm_matches_native(
        "xform",
        r#"
enum J { Int(i64), Arr(Vec<J>) }
fn transform(v: J) -> J {
    match v {
        J::Int(n) => J::Int(n + 1),
        J::Arr(xs) => { let mut out: Vec<J> = Vec::from([])
 for x in xs { out.push(transform(x)) }
 J::Arr(out) }
    }
}
fn cnt(j: J) -> i64 {
    match j { J::Int(n) => *n, J::Arr(xs) => { let mut t = 0
 for x in xs { t += cnt(x) }
 t } }
}
fn build() -> J { let mut a: Vec<J> = Vec::from([])
 a.push(J::Int(1))
 a.push(J::Int(2))
 J::Arr(a) }
fn main() {
    let mut i = 0
    let mut total = 0
    while i < 100 { let p = build()
 let t = transform(p)
 total += cnt(t)
 i += 1 }
    println("{}", total)
}
"#,
    );
}

#[test]
fn json_values_inside_aggregates_pushed_in_a_loop_keep_their_own_handles() {
    // A pushed tuple or struct copies each JSON field into the vector's slot,
    // which must hold a handle of its own: the local the element was built
    // in releases its field on its next overwrite, and a cloned or
    // overwritten vector releases what its slots held.
    let src = r#"
use std::encoding::json

struct Entry { key: String, value: json::Value }

fn main() {
    let mut pairs: Vec<(String, json::Value)> = #[]
    let mut entries: Vec<Entry> = #[]
    for i in 0..6 {
        pairs.push((f"p{i}", json::Value::Int(i)))
        entries.push(Entry { key: f"e{i}", value: json::Value::object(#[("n", json::Value::Int(i * 10))]) })
    }
    let copy = pairs.clone()
    pairs[0] = ("replaced", json::Value::String("new"))
    println(json::Value::object(pairs).to_string())
    println(json::Value::object(copy).to_string())
    for e in entries { println(f"{e.key} {e.value.to_string()}") }
}
"#;
    assert_vm_matches_native("json-aggregates", src);
    let dir = env::temp_dir().join(format!("gos-rcnest-json-expect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("json_expect.gos");
    std::fs::write(&source, src).unwrap();
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&source)
        .output()
        .expect("spawn gos");
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        text.lines().take(3).collect::<Vec<_>>(),
        vec![
            r#"{"p1":1,"p2":2,"p3":3,"p4":4,"p5":5,"replaced":"new"}"#,
            r#"{"p0":0,"p1":1,"p2":2,"p3":3,"p4":4,"p5":5}"#,
            r#"e0 {"n":0}"#,
        ],
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
