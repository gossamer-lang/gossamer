//! Memory-leak gate matrix.
//!
//! Each shape allocates heap memory inside a loop that runs many iterations.
//! If the per-iteration heap is reclaimed at scope end, peak RSS stays bounded
//! regardless of the iteration count; if it leaks, RSS grows with N and blows
//! past the cap. Every shape is gated three ways: peak RSS under the cap, no
//! reference-counted object left at exit (`RC_LIVE_AT_EXIT`), and the leak
//! ledger at the compiled baseline - one counter family alone misses leaks
//! the others see. The caught-panic shape is the one documented exemption:
//! compiled code does not reclaim the values of frames a contained panic
//! unwinds, and the gate pins that to the exact counts so it cannot grow.
//!
//! Run just this gate: `cargo test -p gossamer-cli --test leak_matrix -- --nocapture`

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

/// Peak RSS below this is "bounded"; a per-iteration leak at these iteration
/// counts pushes well past it (millions of small heap objects retained).
const CAP_KB: u64 = 60_000;

/// Shapes whose per-iteration heap MUST be reclaimed. Grows as phases land.
/// Phase 0: only the RC-managed control. Strings/Vec/Map/payloads are added by
/// their phases.
/// What a shape may leave behind at exit, beyond the runtime's own baseline.
struct Allowance {
    /// Live `String`s the leak ledger may report.
    strings: u64,
    /// Live `Vec`s the leak ledger may report.
    vecs: u64,
}

/// The compiled baseline: the runtime keeps one string and one vector.
const BASELINE: Allowance = Allowance {
    strings: 1,
    vecs: 1,
};

/// Shapes whose residue is documented rather than reclaimed, with the exact
/// counts the gate holds them to.
fn allowance(name: &str) -> Allowance {
    match name {
        // Forty contained panics, each unwinding a frame that holds two
        // strings in a vector: compiled code does not reclaim them (SPEC
        // section 8.5), and these counts must not grow.
        "caught_panic_frames" => Allowance {
            strings: 81,
            vecs: 41,
        },
        _ => BASELINE,
    }
}

/// (name, source). N is baked into each source, sized so a leak clears the cap.
const SHAPES: &[(&str, &str)] = &[
    (
        "enum_tree_control",
        r#"
enum Tree { Node(i64, Tree, Tree), Leaf }
fn build(d: i64) -> Tree {
    if d == 0 { Tree::Leaf } else { Tree::Node(d, build(d - 1), build(d - 1)) }
}
fn count(t: Tree) -> i64 {
    match t { Tree::Node(v, l, r) => v + count(l) + count(r), Tree::Leaf => 0 }
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 200000 {
        let t = build(12)
        total += count(t)
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "transient_string",
        r#"
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 4000000 {
        let s = format("value-{}", i)
        total += s.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "returned_string",
        r#"
fn make(i: i64) -> String { format("value-{}", i) }
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 4000000 {
        let s = make(i)
        total += s.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "string_in_struct",
        r#"
struct Holder { s: String }
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 4000000 {
        let h = Holder { s: format("value-{}", i) }
        total += h.s.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "string_in_enum",
        r#"
enum E { S(String), N }
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 4000000 {
        let e = E::S(format("value-{}", i))
        match e { E::S(s) => total += s.len(), E::N => {} }
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "string_in_option",
        r#"
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 4000000 {
        let o: Option<String> = Some(format("value-{}", i))
        match o { Some(s) => total += s.len(), None => {} }
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "string_in_vec",
        r#"
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 1500000 {
        let mut v: Vec<String> = Vec::from([])
        v.push(format("key-{}", i))
        v.push(format("val-{}", i))
        total += v[0].len() + v[1].len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "nested_vec_string",
        r#"
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 1000000 {
        let mut outer: Vec<Vec<String>> = Vec::from([])
        let mut inner: Vec<String> = Vec::from([])
        inner.push(format("value-{}", i))
        outer.push(inner)
        total += outer[0][0].len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        // 0.18.1: a `String` nested inside a by-value sub-struct was never
        // released when the outer struct died (the per-field RC teardown
        // walked only the outer struct's direct fields), so RSS grew with N.
        // The teardown now recurses into by-value sub-structs, with matching
        // recursive retains at every sub-aggregate copy / extract / `..base`
        // site so the nested share is freed exactly once.
        "string_in_nested_struct",
        r#"
struct Inner { name: String, tag: String }
struct Outer { inner: Inner, id: i64 }
fn make(i: i64) -> i64 {
    let o = Outer { inner: Inner { name: format("n-{}", i), tag: format("t-{}", i) }, id: i }
    o.id
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 3000000 {
        total += make(i)
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        // A container field takes a table of its own from the answer a call
        // hands the struct literal, so the frame that built that answer keeps
        // its release: without it every table a loop ever built stayed live.
        "map_in_struct_in_vec",
        r#"
struct Holder { m: Map<i64, i64> }
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 400000 {
        let v = #[Holder { m: {1: 1} }]
        total += v[0].m.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "set_in_struct_in_vec",
        r#"
struct Holder { s: Set<i64> }
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 400000 {
        let v = #[Holder { s: #{1, 2, 3} }]
        let w = v.clone()
        total += v[0].s.len() + w[0].s.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "weak_in_vec",
        r#"
enum Node { Leaf(i64), Pair(Node, Node) }
fn fan(k: i64) -> i64 {
    let n = Node::Leaf(k)
    let mut ws = #[]
    ws.push(n.downgrade())
    ws.push(n.downgrade())
    ws.len()
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 400000 {
        total += fan(i)
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "weak_fanout_past_the_header",
        r#"
enum Node { Leaf(i64), Pair(Node, Node) }
fn fan(k: i64) -> i64 {
    let n = Node::Leaf(k)
    let mut ws = #[]
    for _ in 0..300 { ws.push(n.downgrade()) }
    ws.len()
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 2000 {
        total += fan(i)
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "weak_in_map_and_option",
        r#"
enum Node { Leaf(i64), Pair(Node, Node) }
struct Holder { w: Weak<Node>, tag: i64 }
fn fan(k: i64) -> i64 {
    let n = Node::Leaf(k)
    let mut m = Map::new()
    m[k] = n.downgrade()
    let mut hm = Map::new()
    hm[k] = Holder { w: n.downgrade(), tag: k }
    let o = Some(n.downgrade())
    let mut hs = #[Holder { w: n.downgrade(), tag: k }]
    hs.push(Holder { w: n.downgrade(), tag: k })
    if o.is_some() { m.len() + hm.len() + hs.len() } else { 0 }
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 200000 {
        total += fan(i)
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "lent_carrier_payloads",
        r#"
use std::errors
fn mk(n: i64) -> Vec<u8> {
    let mut v: Vec<u8> = Vec::from([])
    for i in 0..n { v.push(i as u8) }
    v
}
fn q(r: Result<Vec<u8>, errors::Error>) -> Result<i64, errors::Error> {
    let v = r?
    Ok(v.len())
}
fn keep(r: Result<Vec<u8>, errors::Error>) -> Vec<u8> {
    match r {
        Ok(v) => v,
        Err(_) => Vec::from([]),
    }
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 300000 {
        match q(Ok(mk(8))) { Ok(n) => total += n, Err(_) => {} }
        total += keep(Ok(mk(4))).len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "returned_tables",
        r#"
fn pass_map(m: Map<i64, i64>) -> Map<i64, i64> { m }
fn pass_set(s: Set<i64>) -> Set<i64> { s }
fn pass_deque(d: Deque<i64>) -> Deque<i64> { d }
fn pass_heap(h: MinHeap<i64>) -> MinHeap<i64> { h }
fn pick(m: Map<i64, i64>, fresh: bool) -> Map<i64, i64> {
    if fresh { {1: 1} } else { m }
}
fn main() {
    let m = {1: 10, 2: 20}
    let s = #{1, 2, 3}
    let mut d = Deque::new()
    d.push_back(1)
    let h = MinHeap::from([5, 3, 9])
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 100000 {
        let a = pass_map(m)
        let b = pass_set(s)
        let c = pass_deque(d)
        let e = pass_heap(h)
        let p = pick(m, i % 2 == 0)
        let lit = {1: (2, "x")}
        total += a.len() + b.len() + c.len() + e.len() + p.len() + lit.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "borrowed_tables",
        r#"
struct State { s: Set<i64>, d: Deque<i64> }
impl State {
    fn set(&self) -> Set<i64> { self.s }
}
fn first(v: Vec<Set<i64>>) -> Set<i64> { v[0] }
fn value_of(m: Map<i64, Set<i64>>) -> Set<i64> { m[1] }
fn unwrap_deque(o: Option<Deque<i64>>) -> Deque<i64> {
    match o {
        Some(d) => d,
        None => Deque::new(),
    }
}
fn local_field() -> Set<i64> {
    let st = State { s: #{1}, d: Deque::new() }
    st.s
}
fn main() {
    let st = State { s: #{1, 2}, d: Deque::new() }
    let vs = #[#{7}]
    let ms = {1: #{3}}
    let od = Some(st.d)
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 100000 {
        let a = st.set()
        let b = first(vs)
        let c = value_of(ms)
        let d = unwrap_deque(od)
        let e = local_field()
        let read = || st.s
        let f = read()
        let mut held = Vec::new()
        held.push(Some(#{i}))
        total += a.len() + b.len() + c.len() + d.len() + e.len() + f.len() + held.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "caught_panic_frames",
        r#"
fn work(fail: bool) -> i64 {
    let s = "x".repeat(10000)
    let xs = #[s, s]
    if fail { panic("boom {}", xs.len()) }
    xs.len()
}
fn main() {
    let mut errors = 0
    for i in 0..40 {
        let h = spawn(|| work(true))
        match h.join() {
            Ok(_) => {}
            Err(_) => { errors += 1 }
        }
    }
    println("{}", errors)
}
"#,
    ),
];

fn gnu_time_ok() -> bool {
    if !Path::new("/usr/bin/time").exists() {
        return false;
    }
    Command::new("/usr/bin/time")
        .arg("-v")
        .arg("true")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stderr).contains("Maximum resident set size"))
}

fn build_release(dir: &Path, name: &str, source: &str) -> Option<PathBuf> {
    let src = dir.join(format!("{name}.gos"));
    std::fs::write(&src, source).unwrap();
    let out = Command::new(gos_bin())
        .arg("build")
        .arg("--release")
        .arg(&src)
        .output()
        .expect("spawn gos build");
    if !out.status.success() {
        return None;
    }
    let bin = dir
        .join("target")
        .join("release")
        .join(format!("{name}{}", env::consts::EXE_SUFFIX));
    bin.exists().then_some(bin)
}

/// What a run left behind: peak RSS, reference-counted objects still live,
/// and the leak ledger's live `String` and `Vec` counts with the rest of the
/// ledger line.
struct Residue {
    rss_kb: u64,
    rc_live: u64,
    strings: u64,
    vecs: u64,
    ledger: String,
}

fn ledger_field(line: &str, name: &str) -> u64 {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(&format!("{name}=")))
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX)
}

fn measure(bin: &Path) -> Residue {
    let out = Command::new("/usr/bin/time")
        .arg("-v")
        .arg(bin)
        .env("GOS_RC_DEBUG", "1")
        .env("GOS_LEAK_LEDGER", "1")
        .output()
        .expect("spawn /usr/bin/time");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "binary {} failed: {stderr}",
        bin.display()
    );
    let rss_kb = stderr
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("Maximum resident set size (kbytes):")
                .and_then(|rest| rest.trim().parse().ok())
        })
        .unwrap_or_else(|| panic!("no RSS line for {}", bin.display()));
    let rc_live = stderr
        .lines()
        .find_map(|line| line.strip_prefix("RC_LIVE_AT_EXIT="))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX);
    let ledger = stderr
        .lines()
        .find(|line| line.starts_with("LEAK LEDGER"))
        .unwrap_or("")
        .to_string();
    let others_clear = ["aggr", "rc", "map", "set", "deque"]
        .iter()
        .all(|name| ledger_field(&ledger, name) == 0);
    Residue {
        rss_kb,
        rc_live,
        strings: ledger_field(&ledger, "str"),
        vecs: if others_clear {
            ledger_field(&ledger, "vec")
        } else {
            u64::MAX
        },
        ledger,
    }
}

#[test]
fn leak_matrix_report() {
    if !gnu_time_ok() {
        // Linux CI is where this gate is required, so a missing measurement
        // there is a failure rather than a skip.
        assert!(
            !(cfg!(target_os = "linux") && env::var_os("CI").is_some()),
            "GNU /usr/bin/time -v is required for the leak gate on Linux CI"
        );
        eprintln!("skipping: GNU /usr/bin/time -v not available");
        return;
    }
    let dir = env::temp_dir().join(format!("gos-leak-matrix-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut failures = Vec::new();
    eprintln!("\n  leak matrix (cap {CAP_KB} KB)");
    for (name, source) in SHAPES {
        let Some(bin) = build_release(&dir, name, source) else {
            eprintln!("  {name:<28} BUILD FAIL");
            failures.push(format!("{name}: build failed"));
            continue;
        };
        let residue = measure(&bin);
        let allowed = allowance(name);
        let ok = residue.rss_kb < CAP_KB
            && residue.rc_live == 0
            && residue.strings <= allowed.strings
            && residue.vecs <= allowed.vecs;
        eprintln!(
            "  {name:<28} {:>8} KB  rc_live={}  {}  {}",
            residue.rss_kb,
            residue.rc_live,
            residue.ledger,
            if ok { "ok" } else { "LEAK" }
        );
        if !ok {
            failures.push(format!(
                "{name}: rss {} KB, rc_live {}, {}",
                residue.rss_kb, residue.rc_live, residue.ledger
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "leaking shapes:\n{}",
        failures.join("\n")
    );
}
