//! Release-tier stability gauge.
//!
//! Each test below writes a small deterministic Gossamer program,
//! builds it with `gos build --release`, runs the binary, and
//! asserts the produced stdout byte-for-byte against a fixed
//! expected string.
//!
//! The `--release` pipeline is the gold-standard target: interp
//! and `gos build` (debug/Cranelift) are dev-loop tooling, but
//! `--release` is what real deployments ship. So every test here
//! exercises that exact pipeline, no fallback, no skip.
//!
//! Tests that pass are regression gates: a future change that
//! silently breaks (say) `HashMap.inc` or recursive-enum walking
//! in the release tier will turn this suite red. Tests carrying
//! `#[ignore = "release-tier wiring gap: …"]` document a known
//! wiring failure where `gos build --release` accepts the program
//! but the produced binary diverges from the language semantics -
//! those entries form the today-snapshot of the gauge. Removing
//! the `#[ignore]` is the right way to claim a gap is closed.

#![allow(missing_docs)]

mod common;

use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

/// Per-test scratch directory. Concurrent tests must not share a
/// directory: `gos build` writes the produced binary to
/// `<source-dir>/target/release/<stem>`, and a clobber would let
/// one test execute another's bits.
fn fresh_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-rel-stab-{pid}-{n}-{name}",
        pid = std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

struct Program {
    dir: PathBuf,
    bin: PathBuf,
}

impl Drop for Program {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn build_release(name: &str, body: &str) -> Program {
    let dir = fresh_dir(name);
    let source = dir.join(format!("{name}.gos"));
    std::fs::write(&source, body).expect("write source");
    let out = Command::new(gos_bin())
        .arg("build")
        .arg("--release")
        .arg(&source)
        .output()
        .expect("spawn gos build --release");
    assert!(
        out.status.success(),
        "gos build --release {name} failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let bin = dir
        .join("target")
        .join("release")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(bin.exists(), "release binary missing at {}", bin.display());
    Program { dir, bin }
}

fn run(prog: &Program) -> (i32, String, String) {
    let out = Command::new(&prog.bin).output().expect("run binary");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Asserts the released binary exits 0 and emits exactly `expected`
/// on stdout. Failure messages dump both streams plus the exit code
/// so the source of any drift is visible without re-running.
/// Rewrites the loopback ports a fixture's inline source names.
///
/// The kernel picks each replacement, so a fixture binds a port nothing else
/// on the box holds - which is what lets these tests run beside each other and
/// beside whatever a developer has listening.
fn with_free_ports(body: &str, ports: &[&str]) -> String {
    let mut out = body.to_string();
    for port in ports {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let chosen = probe.local_addr().expect("read the bound address").port();
        drop(probe);
        out = out.replace(port, &chosen.to_string());
    }
    out
}

fn assert_release_stdout_eq(name: &str, body: &str, expected: &str) {
    let prog = build_release(name, body);
    let (code, stdout, stderr) = run(&prog);
    assert_eq!(
        code, 0,
        "{name}: exit={code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert_eq!(
        stdout, expected,
        "{name}: stdout drift\n--- expected ---\n{expected}\n--- actual ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}

// ---------------------------------------------------------------
// Passing checks - these are the regression gates. Each one
// covers a behaviour that the release pipeline is supposed to
// honour and has been confirmed working at the time of writing.
// A red light here means a recent change broke something.
// ---------------------------------------------------------------

#[test]
fn release_request_field_string_methods_dispatch_correctly() {
    // Catches method-dispatch regressions on opaque http::Request
    // fields: `r.query` / `r.path` are checker-opaque (inference
    // Vars), and without the MIR field-type promotion `.len()`
    // lands on the len-prefixed `gos_rt_len`, dereferencing the
    // c-string pointer - a misaligned-pointer abort on the first
    // request (the locurlfwd proxy crash shape).
    assert_release_stdout_eq(
        "request_field_methods",
        &with_free_ports(
            r#"
use std::http
use std::process
use std::strings
use std::time

struct App { }

impl http::Handler for App {
    fn serve(&self, r: http::Request) -> http::Response {
        let q = &r.query
        let line = format(
            "qlen={} plen={} starts={} has={} blen={}",
            r.query.len(),
            r.path.len(),
            strings::starts_with(q, "k="),
            strings::contains(q, "n=2"),
            r.body.len(),
        )
        http::Response::text(200, line)
    }
}

fn run_server() {
    if let Err(e) = http::serve("127.0.0.1:23924", App { }) {
        eprintln("serve failed: {}", e)
    }
}

fn await_ready() -> bool {
    let mut tries = 0
    while tries < 1600 {
        let none: Vec<(String, String)> = Vec::from([])
        if let Ok(_) = http::get("http://127.0.0.1:23924/probe", none) {
            return true
        }
        time::sleep(25)
        tries += 1
    }
    false
}

fn main() {
    spawn(|| run_server())
    if !await_ready() {
        println("server never became ready")
        process::exit(0)
    }
    let none: Vec<(String, String)> = Vec::from([])
    match http::get("http://127.0.0.1:23924/echo?k=1&n=2", none) {
        Ok(r) => println("status={} body={}", r.status, r.body),
        Err(e) => println("error: {}", e),
    }
    process::exit(0)
}
"#,
            &["23924"],
        ),
        "status=200 body=qlen=7 plen=5 starts=true has=true blen=0\n",
    );
}

#[test]
fn release_http_bare_response_handler_serves_200() {
    // Catches handler-ABI regressions: a serve method declaring a
    // bare `http::Response` return (no `Result` wrapper) is adapted
    // to the packed-Result handler C-ABI by the MIR-synthesized
    // `::__ok_wrap` thunk. Without it the release server misreads
    // the Response pointer as a Result discriminant and answers 500.
    assert_release_stdout_eq(
        "bare_handler",
        &with_free_ports(
            r#"
use std::http
use std::process
use std::time

struct App { }

impl http::Handler for App {
    fn serve(&self, _r: http::Request) -> http::Response {
        http::Response::text(200, "bare ok")
    }
}

fn run_server() {
    if let Err(e) = http::serve("127.0.0.1:23921", App { }) {
        eprintln("serve failed: {e}")
    }
}

// Polls until the server goroutine accepts connections; binding is
// asynchronous, so readiness is observable only by connecting.
fn await_ready(url: String) -> bool {
    let mut tries = 0
    while tries < 1600 {
        if let Ok(_) = http::get(url, Vec::from([])) {
            return true
        }
        time::sleep(25)
        tries += 1
    }
    false
}

fn main() {
    spawn(|| run_server())
    if !await_ready("http://127.0.0.1:23921/x") {
        println("server never became ready")
        process::exit(0)
    }
    match http::get("http://127.0.0.1:23921/x", Vec::from([])) {
        Ok(r) => println("status={} body={}", r.status, r.body),
        Err(e) => println("error: {}", e),
    }
    process::exit(0)
}
"#,
            &["23921"],
        ),
        "status=200 body=bare ok\n",
    );
}

#[test]
fn release_proxy_stream_passthrough_serves_chunked_body() {
    // Catches streamed-response regressions in the release
    // pipeline: a proxy handler returns
    // `http::Response::stream(up.status, up.content_type, up)` and
    // the native server drains the upstream reader to the client as
    // chunked frames (the locurlfwd proxy-passthrough shape).
    assert_release_stdout_eq(
        "proxy_stream",
        &with_free_ports(
            r#"
use std::http
use std::process
use std::time

struct Upstream { }

impl http::Handler for Upstream {
    fn serve(&self, _r: http::Request) -> Result<http::Response, http::Error> {
        Ok(http::Response::text(200, "release streamed body"))
    }
}

struct Proxy { }

impl http::Handler for Proxy {
    fn serve(&self, r: http::Request) -> Result<http::Response, http::Error> {
        match http::stream(r.method, "http://127.0.0.1:23922/data", r.body, Vec::from([])) {
            Ok(up) => Ok(http::Response::stream(up.status, up.content_type, up)),
            Err(_) => Ok(http::Response::text(502, "bad upstream")),
        }
    }
}

fn run_upstream() {
    if let Err(e) = http::serve("127.0.0.1:23922", Upstream { }) {
        eprintln("serve failed: {e}")
    }
}

fn run_proxy() {
    if let Err(e) = http::serve("127.0.0.1:23923", Proxy { }) {
        eprintln("serve failed: {e}")
    }
}

// Polls until the endpoint answers `want`; binding is asynchronous, so
// readiness is observable only by connecting. The proxy answers 502 - an
// Ok response - for as long as the upstream is down, so the status is
// part of the readiness condition rather than a result to assert on.
// The try budget is a precondition on a runner compiling the sibling
// fixtures at the same time, where a goroutine can be slow to reach
// `bind`; exhausting it reports itself rather than failing a later
// assertion.
fn await_status(url: String, want: i64) -> bool {
    let mut tries = 0
    while tries < 1600 {
        if let Ok(r) = http::get(url, Vec::from([])) {
            if r.status == want {
                return true
            }
        }
        time::sleep(25)
        tries += 1
    }
    false
}

fn main() {
    spawn(|| run_upstream())
    spawn(|| run_proxy())
    if !await_status("http://127.0.0.1:23922/data", 200) {
        println("upstream never became ready")
        process::exit(0)
    }
    if !await_status("http://127.0.0.1:23923/x", 200) {
        println("proxy never served the upstream body")
        process::exit(0)
    }
    match http::get("http://127.0.0.1:23923/x", Vec::from([])) {
        Ok(r) => println("status={} ct={} body={}", r.status, r.content_type, r.body),
        Err(e) => println("error: {}", e),
    }
    process::exit(0)
}
"#,
            &["23922", "23923"],
        ),
        "status=200 ct=text/plain; charset=utf-8 body=release streamed body\n",
    );
}

#[test]
fn release_recursive_enum_walks_full_list() {
    // Catches recursive-enum aggregate-layout regressions: the
    // A recursive payload must round-trip through pass-by-value
    // and the `match` arms must dispatch on the discriminant.
    assert_release_stdout_eq(
        "rec_enum",
        r#"
enum List {
    Nil,
    Cons(i64, List),
}

fn cons(v: i64, rest: List) -> List { List::Cons(v, Box::new(rest)) }

fn length(list: List) -> i64 {
    match list {
        List::Nil => 0,
        List::Cons(_, rest) => 1 + length(rest),
    }
}

fn sum(list: List) -> i64 {
    match list {
        List::Nil => 0,
        List::Cons(v, rest) => *v + sum(rest),
    }
}

fn main() {
    let xs = cons(1, cons(2, cons(3, cons(4, cons(5, List::Nil)))))
    println("len={} sum={}", length(xs), sum(xs))
}
"#,
        "len=5 sum=15\n",
    );
}

#[test]
fn release_closure_captures_value_at_definition() {
    // Defines `bump` *before* mutating `k`, so each call sees `k`
    // as captured (zero), making `bump(k)` collapse to `k + 1`.
    // sum_{k=0..99}(k+1) = 5050. Catches closure-capture ABI
    // regressions where the capture is silently aliased to a
    // mutable upvar.
    assert_release_stdout_eq(
        "closure_capture",
        r#"
fn main() {
    let mut acc: i64 = 0
    let mut k: i64 = 0
    let bump = |x: i64| { k + x + 1 }
    while k < 100 {
        acc = acc + bump(k)
        k = k + 1
    }
    println("acc={}", acc)
}
"#,
        "acc=5050\n",
    );
}

#[test]
fn release_channel_send_recv_drains_in_order() {
    // FIFO bounded-channel semantics - main pushes 5 values, drains 5 via
    // `if let Some(v) = rx.recv()`. Catches Option<T> aggregate
    // construction from runtime returns + channel ABI.
    assert_release_stdout_eq(
        "channel_drain",
        r#"
use std::sync::channel

fn main() {
    let tx, rx = channel(5)
    let mut k = 0
    while k < 5 {
        tx.send(k * 10)
        k = k + 1
    }
    let mut sum: i64 = 0
    let mut n = 0
    while n < 5 {
        if let Some(v) = rx.recv() {
            sum = sum + v
        }
        n = n + 1
    }
    println("sum={}", sum)
}
"#,
        "sum=100\n",
    );
}

#[test]
fn release_waitgroup_blocks_main_until_workers_done() {
    // Spawns three goroutines, each calling `wg.done()`. Main
    // blocks on `wg.wait()`. Catches WaitGroup wiring (add/done/
    // wait) and goroutine spawn-via-block in release.
    assert_release_stdout_eq(
        "wg_block",
        r#"
use std::sync

fn main() {
    let wg = sync::WaitGroup::new()
    wg.add(3)
    let mut k = 0
    while k < 3 {
        spawn(|| {
            wg.done()
        })
        k = k + 1
    }
    wg.wait()
    println("done")
}
"#,
        "done\n",
    );
}

#[test]
fn release_slot_containers_are_values_not_aliases() {
    // A binding taken from a slot container, and a by-value parameter, each
    // get a value of their own. The elements live behind a handle, so this
    // is built rather than inherited - and the release backend is where a
    // missed clone shows as a shared pointer.
    assert_release_stdout_eq(
        "slot_container_values",
        r#"
use std::collections::{Deque, Queue, Stack}

fn grow(mut d: Deque<i64>) -> i64 {
    d.push_back(99)
    d.len()
}

fn main() {
    let d = Deque::from([1, 2])
    let mut d2 = d
    d2.push_back(9)
    let q = Queue::from([1, 2])
    let mut q2 = q
    q2.push(9)
    let s = Stack::from([1, 2])
    let mut s2 = s
    s2.push(9)
    println("{} {} {} {} {} {}", d.len(), d2.len(), q.len(), q2.len(), s.len(), s2.len())
    println("{} {}", grow(d), d.len())
}
"#,
        "2 3 2 3 2 3
3 2
",
    );
}

#[test]
fn release_a_binding_taken_from_a_constructed_container_keeps_its_own_storage() {
    // A `let` may take over a constructor's destination temporary, but only
    // while that temporary is anonymous. Once a name owns the local, the call
    // that defines it belongs to that name: re-pointing it leaves the first
    // binding reading a slot nothing ever writes.
    assert_release_stdout_eq(
        "ctor_rebind_names",
        r#"
fn main() {
    let a = Vec::new()
    let mut b = a
    b.push(9)
    let m = Map::new()
    let mut m2 = m
    m2.insert("k", 1)
    println("{} {} {} {}", a.len(), b.len(), m.len(), m2.len())
}
"#,
        "0 1 0 1\n",
    );
}

#[test]
fn release_hashmap_inc_idiom_counts_words() {
    // Catches the HashMap.inc counter idiom - a known weak point:
    // a recent fix landed for the round-2 String<->i64 lowering.
    // Verifies inc() defaults to +1, increments persist across
    // calls, and `get_or(default)` reads the right slot.
    assert_release_stdout_eq(
        "hm_inc",
        r#"
use std::collections::Map

fn main() {
    let mut tally: Map<String, i64> = Map::new()
    let words = ["apple", "banana", "apple", "apple", "banana", "cherry"]
    for w in words {
        tally.inc(w)
    }
    println("apple={} banana={} cherry={}",
        tally.get_or("apple", 0),
        tally.get_or("banana", 0),
        tally.get_or("cherry", 0))
}
"#,
        "apple=3 banana=2 cherry=1\n",
    );
}

#[test]
fn release_btreemap_iter_yields_sorted_pairs() {
    // BTreeMap's `for (k, v) in m.iter()` shape - destructured
    // iteration over an ordered map. Catches both the
    // tuple-destructuring binding and the sorted-by-key
    // invariant.
    assert_release_stdout_eq(
        "btmap",
        r#"
use std::collections::BTreeMap

fn main() {
    let mut m: BTreeMap<String, i64> = BTreeMap::new()
    m.insert("c", 3)
    m.insert("a", 1)
    m.insert("b", 2)
    let mut sum: i64 = 0
    for (k, v) in m.iter() {
        println("{}={}", k, v)
        sum = sum + v
    }
    println("sum={}", sum)
}
"#,
        "a=1\nb=2\nc=3\nsum=6\n",
    );
}

#[test]
fn release_btreemap_iter_collect_enumerate_is_finite_and_ordered() {
    // `iter()` must materialize map entries before collection. Otherwise a
    // BTreeMap handle is treated as a Vec header and `entries.iter()` can
    // loop indefinitely in native code.
    assert_release_stdout_eq(
        "btmap_iter_collect_enumerate",
        r#"
use std::collections::BTreeMap

fn main() {
    let mut m: BTreeMap<String, i64> = BTreeMap::new()
    m.insert("c", 3)
    m.insert("a", 1)
    m.insert("b", 2)
    let entries = m.iter().collect()
    for (i, entry) in entries.iter().enumerate() {
        println("{}:{}={}", i, entry.0, entry.1)
    }
}
"#,
        "0:a=1\n1:b=2\n2:c=3\n",
    );
}

#[test]
fn release_match_guard_and_range_patterns_classify() {
    // Match arm with guard (`x if x < 0`), exact literal (`0`),
    // inclusive range (`1..=9`), and wildcard. Ensures the
    // pattern compiler in release covers all four shapes.
    assert_release_stdout_eq(
        "patterns",
        r#"
fn classify(n: i64) -> String {
    match n {
        x if x < 0 => "negative",
        0 => "zero",
        1..=9 => "single",
        _ => "many",
    }
}

fn main() {
    for n in [-5, 0, 3, 42] {
        println("{}={}", n, classify(n))
    }
}
"#,
        "-5=negative\n0=zero\n3=single\n42=many\n",
    );
}

#[test]
fn release_trait_impl_dispatches_through_concrete_types() {
    // Two distinct types implement the same trait. Calls via
    // concrete-typed bindings must reach the right impl. Catches
    // method-table generation in release.
    assert_release_stdout_eq(
        "trait_impl",
        r#"
trait Shape {
    fn area(&self) -> f64
    fn name(&self) -> String
}

struct Circle { radius: f64 }
struct Rect { w: f64, h: f64 }

impl Shape for Circle {
    fn area(&self) -> f64 { 3.14159265 * self.radius * self.radius }
    fn name(&self) -> String { "circle" }
}

impl Shape for Rect {
    fn area(&self) -> f64 { self.w * self.h }
    fn name(&self) -> String { "rect" }
}

fn main() {
    let c = Circle { radius: 2.0 }
    let r = Rect { w: 3.0, h: 4.0 }
    println("{} area={:.4}", c.name(), c.area())
    println("{} area={:.4}", r.name(), r.area())
}
"#,
        "circle area=12.5664\nrect area=12.0000\n",
    );
}

#[test]
fn release_struct_methods_chain_returning_value() {
    // Builder-style chain where each method takes `self` and
    // returns a fresh struct. Catches aggregate-return ABI in
    // release (one of the four root causes the compiled-impl
    // method-dispatch fix had to close).
    assert_release_stdout_eq(
        "method_chain",
        r#"
struct Counter { value: i64 }

impl Counter {
    fn new() -> Counter { Counter { value: 0 } }
    fn inc(self, by: i64) -> Counter { Counter { value: self.value + by } }
    fn double(self) -> Counter { Counter { value: self.value * 2 } }
    fn get(self) -> i64 { self.value }
}

fn main() {
    let c = Counter::new().inc(3).double().inc(1).double()
    println("got={}", c.get())
}
"#,
        "got=14\n",
    );
}

#[test]
fn release_recursive_fib_returns_correct_value() {
    // Naive recursive `fib(25)` - a smoke test for stack-rooted
    // values, register reuse, and call-conv across deep call
    // chains.
    assert_release_stdout_eq(
        "fib",
        r#"
fn fib(n: i64) -> i64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

fn main() {
    println("fib(20)={}", fib(20))
    println("fib(25)={}", fib(25))
}
"#,
        "fib(20)=6765\nfib(25)=75025\n",
    );
}

#[test]
fn release_vec_push_iter_sums_correctly() {
    // Push 1000 i64s into a heap-vec, iterate via `.iter()`,
    // dereference each element, accumulate. Catches the i64-vec
    // alloc-set-iter triple and the dereference path.
    assert_release_stdout_eq(
        "vec_iter",
        r#"
fn main() {
    let mut v: Vec<i64> = Vec::from([])
    let mut k: i64 = 0
    while k < 1000 {
        v.push(k)
        k = k + 1
    }
    let mut sum: i64 = 0
    for x in v.iter() {
        sum = sum + *x
    }
    println("len={} sum={}", v.len(), sum)
}
"#,
        "len=1000 sum=499500\n",
    );
}

#[test]
fn release_string_split_trim_parse_totals() {
    // Splits a comma list, trims each piece, parses each into
    // i64, accumulates. Catches the str.split / str.trim /
    // str.to_i64 trio in release plus Option<i64> match in a
    // hot path.
    assert_release_stdout_eq(
        "split_trim_parse",
        r#"
fn main() {
    let line = "1, 2, 3, 4, 5"
    let mut sum: i64 = 0
    let mut count: i64 = 0
    for piece in line.split(',') {
        let trimmed = piece.trim()
        let n: i64 = match trimmed.to_i64() {
            Some(v) => v,
            None => 0,
        }
        sum = sum + n
        count = count + 1
    }
    println("count={} sum={}", count, sum)
}
"#,
        "count=5 sum=15\n",
    );
}

#[test]
fn release_nested_format_macro_handles_precision() {
    // `format!("{:.4}", pi)` nested inside another `format!` -
    // catches the LLVM `__concat` buffering fix and precision
    // wiring on the inner string boundary.
    assert_release_stdout_eq(
        "nested_fmt",
        r#"
fn main() {
    let pi = 3.14159265358979
    let nested = format("[{}]", format("{:.4}", pi))
    println("{}", nested)
    let multi = format("a={:.2} b={:.4}", pi, pi * 2.0)
    println("{}", multi)
}
"#,
        "[3.1416]\na=3.14 b=6.2832\n",
    );
}

#[test]
fn release_float_edges_round_trip_to_expected_booleans() {
    // NaN != NaN, +0 == -0, division-by-zero produces signed
    // infinities. Catches H7 (NaN-boxing low-mantissa loss) any
    // time it might re-emerge as a regression.
    assert_release_stdout_eq(
        "float_edges",
        r#"
fn main() {
    let zero: f64 = 0.0
    let neg_zero: f64 = -0.0
    let inf: f64 = 1.0 / zero
    let neg_inf: f64 = -1.0 / zero
    println("zero==neg_zero: {}", zero == neg_zero)
    println("inf>0: {}", inf > 0.0)
    println("neg_inf<0: {}", neg_inf < 0.0)
    let nan: f64 = inf - inf
    println("nan==nan: {}", nan == nan)
}
"#,
        "zero==neg_zero: true\ninf>0: true\nneg_inf<0: true\nnan==nan: false\n",
    );
}

#[test]
fn release_for_range_inclusive_and_iter_match_exclusive() {
    // Three for-loop shapes: `0..n`, `0..=n`, `vec.iter()`. The
    // VM has a documented fast-path for the exclusive form (H3)
    // and used to silently fall back on the others; release
    // should handle all three.
    assert_release_stdout_eq(
        "for_shapes",
        r#"
fn main() {
    let mut excl: i64 = 0
    for i in 0..5 { excl = excl + i }

    let mut incl: i64 = 0
    for i in 0..=5 { incl = incl + i }

    let v = [10, 20, 30, 40]
    let mut iter_sum: i64 = 0
    for x in v.iter() { iter_sum = iter_sum + *x }

    println("excl={} incl={} iter={}", excl, incl, iter_sum)
}
"#,
        "excl=10 incl=15 iter=100\n",
    );
}

// ---------------------------------------------------------------
// Known release-tier wiring gaps. Each `#[ignore]` reason names
// the surface area where `gos build --release` accepts the
// program but the produced binary diverges from language
// semantics (silent no-op, silent empty, segfault, etc.).
//
// These are the gauge: when a gap is closed, drop the `#[ignore]`
// and the test becomes a permanent regression gate.
// ---------------------------------------------------------------

#[test]
fn release_atomic_fetch_add_persists_across_goroutines() {
    // 100 goroutines each `fetch_add(1)` on a shared AtomicI64.
    // The constructor rename map gained `AtomicI64::new` /
    // `sync::AtomicI64::new` so the receiver is a real
    // `*mut GosAtomicI64` instead of null; without that the
    // helper silently saw `a.is_null()` and returned 0 every
    // time.
    assert_release_stdout_eq(
        "atomic_inc",
        r#"
use std::sync

fn main() {
    let counter = sync::AtomicI64::new(0)
    let wg = sync::WaitGroup::new()
    let mut k = 0
    while k < 100 {
        wg.add(1)
        spawn(|| {
            counter.fetch_add(1)
            wg.done()
        })
        k = k + 1
    }
    wg.wait()
    println("counter={}", counter.load())
}
"#,
        "counter=100\n",
    );
}

#[test]
fn release_owned_string_push_str_holds_value() {
    // `String::new()` lowers to an empty-string literal and
    // `b.push_str(s)` is rewritten to `b = __concat(b, s)`
    // (gossamer-mir/src/lower.rs::lower_method_call). Owned
    // `String` is the runtime's `*const c_char` representation
    // - concat-and-reassign keeps the receiver local rooted to
    // the new bytes pointer.
    assert_release_stdout_eq(
        "owned_str",
        r#"
fn main() {
    let mut b: String = String::new()
    b.push_str("hi")
    println("b={}", b)
}
"#,
        "b=hi\n",
    );
}

#[test]
fn release_result_map_err_replaces_error() {
    // The HIR lift pass turns non-capturing closures into a
    // bare-name path that lowers to a string-literal pointer.
    // `gos_rt_result_map_err` reads the first 8 bytes of its
    // closure arg as a function address, so the raw pointer
    // segfaulted. The MIR lower for `map_err` / `map` now wraps
    // bare-name closure args into a heap blob `[fn_addr, _]`
    // (gossamer-mir/src/lower.rs::lower_method_call).
    assert_release_stdout_eq(
        "map_err",
        r#"
use std::errors

fn main() {
    let raw: String = "oops"
    let r: Result<i64, errors::Error> = raw.to_i64().ok_or(errors::new("bad"))
    let mapped = r.map_err(|_| errors::new("custom"))
    match mapped {
        Ok(n) => println("ok {}", n),
        Err(e) => println("err {}", e.message()),
    }
}
"#,
        "err custom\n",
    );
}

#[test]
fn release_eprintln_goes_to_stderr() {
    // Until 2026-04-30 the cranelift + LLVM lowering of
    // `eprint`/`eprintln` shared the buffered stdout writer -
    // the comment at native.rs:3541 acknowledged the gap. This
    // test gates the fix: stderr-bound output must not appear
    // on stdout, and stdout output must still flush before
    // any stderr output (so user-visible diagnostic order is
    // preserved).
    let prog = build_release(
        "eprintln_to_stderr",
        r#"
fn main() {
    println("on-stdout")
    eprintln("on-stderr-{}", 42)
}
"#,
    );
    let (code, stdout, stderr) = run(&prog);
    assert_eq!(code, 0, "exit {code}\nstderr: {stderr}");
    assert_eq!(stdout, "on-stdout\n", "stdout drift: {stdout:?}");
    assert_eq!(stderr, "on-stderr-42\n", "stderr drift: {stderr:?}");
}

#[test]
fn release_u64_values_print_like_the_vm() {
    // A `u64` / `usize` value renders unsigned by its declared type: a
    // binding typed `u64` at 2^64-1 prints `18446744073709551615`, the same
    // as a value produced by an explicit `as u64` cast, on both the VM and
    // the released binary. (Previously a declared-but-uncast `u64` aliased
    // the signed-i64 `-1` and the tiers disagreed on the comparison form.)
    assert_release_stdout_eq(
        "u64_max",
        r#"
fn main() {
    let n: u64 = 18446744073709551615u64
    println("{}", n)
    let c = (0 - 1) as u64
    println("{}", c)
}
"#,
        "18446744073709551615\n18446744073709551615\n",
    );
}

#[test]
fn release_match_guard_dispatches_through_chain() {
    // Catches the silent always-match miscompile in
    // `lower_match_with_guards`. Each arm uses a different
    // pattern + guard shape; if `lower_pattern_predicate`
    // returns None for any shape and the lowerer falls back
    // to "always matches", later arms become unreachable
    // and the test fails loudly.
    //
    // Uses bare-int scrutinee to isolate *guard* dispatch.
    // Variant-payload literal matching (`Ok(1)` vs `Ok(2)`) is
    // covered by `release_result_option_payload_literal_match`.
    assert_release_stdout_eq(
        "match_guard_chain",
        r#"
fn classify(x: i64) -> &str {
    match x {
        1 | 2 => "small",
        n if n > 5 => "big",
        _ => "mid",
    }
}

fn main() {
    println("{}", classify(1))
    println("{}", classify(2))
    println("{}", classify(3))
    println("{}", classify(10))
    println("{}", classify(0))
}
"#,
        "small\nsmall\nmid\nbig\nmid\n",
    );
}

#[test]
fn release_iter_enumerate_yields_index_value_pairs() {
    // `v.iter().enumerate()` lowering is now in
    // `gossamer-mir/src/lower.rs::lower_for_enumerate`. Strips
    // the `enumerate()` and an inner wrapping `iter()`, then
    // drives the standard array / vec counter loop while binding
    // the per-iteration counter to the tuple's first slot.
    assert_release_stdout_eq(
        "enumerate",
        r#"
fn main() {
    let v = [10, 20, 30, 40]
    for (idx, x) in v.iter().enumerate() {
        println("idx={} x={}", idx, *x)
    }
}
"#,
        "idx=0 x=10\nidx=1 x=20\nidx=2 x=30\nidx=3 x=40\n",
    );
}

#[test]
fn release_result_option_payload_literal_match() {
    // `Ok(1)` / `Ok(2)` / `Some(N)` must route to the correct arm.
    assert_release_stdout_eq(
        "payload_literal",
        r#"
fn classify(r: Result<i64, i64>) -> &str {
    match r {
        Ok(1) => "one",
        Ok(2) => "two",
        Ok(_) => "other-ok",
        Err(_) => "err",
    }
}

fn pick(o: Option<i64>) -> &str {
    match o {
        Some(10) => "ten",
        Some(20) => "twenty",
        None => "none",
        _ => "other",
    }
}

fn main() {
    println("{}", classify(Ok(1)))
    println("{}", classify(Ok(2)))
    println("{}", classify(Ok(99)))
    println("{}", classify(Err(0)))
    println("{}", pick(Some(10)))
    println("{}", pick(Some(20)))
    println("{}", pick(None))
}
"#,
        "one\ntwo\nother-ok\nerr\nten\ntwenty\nnone\n",
    );
}

#[test]
fn release_byte_vec_literal_sums_at_i64_width() {
    // `sum` infers as u8 from `sum += b`, but the runtime integer
    // model is i64 on every tier: 200+200+60+4 must print 464, not
    // 464 mod 256 = 208. Regression gate for the LLVM (and JIT)
    // narrow-width arithmetic miscompile.
    assert_release_stdout_eq(
        "byte_vec_sum",
        r#"
fn main() {
    let body: Vec<u8> = [200, 200, 60, 4].to_vec()
    let mut sum = 0
    for b in body {
        sum += b as i64
    }
    println("sum: {}", sum)
    println("idx: {} {} {}", body[0], body[1], body[3])
}
"#,
        "sum: 464\nidx: 200 200 4\n",
    );
}

#[test]
fn release_read_file_bytes_for_loop_mixed_width_sum() {
    // `total: i64 += b: u8` used to emit `add i64 %a, %b` with an
    // i8 operand - invalid IR that failed the `opt` stage. The
    // file roundtrip also hands the compiled tier a packed
    // elem_bytes=1 vec, so indexing and element writes must honour
    // the header stride.
    assert_release_stdout_eq(
        "read_file_bytes_sum",
        r#"
use std::errors
use std::env
use std::fs
use std::path

fn main() -> Result<(), errors::Error> {
    let payload: Vec<u8> = [0, 255, 1, 254].to_vec()
    let tmp = path::join(env::temp_dir(), "gos_rel_byte_sum_probe.bin")
    fs::write(tmp, payload)?
    let mut bytes = fs::read(tmp)?
    fs::remove_file(tmp)?
    let mut total: i64 = 0
    for b in bytes {
        total += b as i64
    }
    println("total: {}", total)
    bytes[2] = 9
    let mut sum = 0
    for b in bytes {
        sum += b as i64
    }
    println("sum: {}", sum)
    println("idx: {} {} {} {}", bytes[0], bytes[1], bytes[2], bytes[3])
    Ok(())
}
"#,
        "total: 510\nsum: 518\nidx: 0 255 9 254\n",
    );
}

#[test]
fn release_narrow_casts_mask_and_float_casts_saturate() {
    // `as` is the single masking point for narrow int types
    // (`300 as u8` == 44, `200 as i8` == -56); wrapping arithmetic
    // wraps at the declared width (`200u8 +% 200u8` == 144); and
    // float -> int saturates at the TARGET's own range
    // (`300.7 as u8` == 255, `-1.5 as u8` == 0, `1e20 as i64` ==
    // i64::MAX). All match the bytecode VM.
    assert_release_stdout_eq(
        "narrow_casts",
        r#"
fn main() {
    let x = 300
    println("{}", x as u8)
    let y = 200
    println("{}", y as i8)
    let z: u8 = 200
    println("{}", z +% z)
    let f = 300.7
    println("{}", f as u8)
    let g = -1.5
    println("{}", g as u8)
    let h = 1e20
    println("{}", h as i64)
}
"#,
        "44\n-56\n144\n255\n0\n9223372036854775807\n",
    );
}

#[test]
fn release_packed_byte_vec_helpers_honor_stride() {
    // `fs::read` hands the compiled tiers a packed
    // elem_bytes=1 vec. The Vec helper surface (`first` / `last` /
    // `index_of` / `count_of` / `contains` / `rev` / `pop`)
    // must honor the header stride instead of reading 8 bytes per
    // element, and `pop` must return `Option<last>` while
    // shortening the receiver. Expected values match the VM run.
    assert_release_stdout_eq(
        "packed_byte_vec_helpers",
        r#"
use std::errors
use std::env
use std::fs
use std::path

fn main() -> Result<(), errors::Error> {
    let payload: Vec<u8> = [9, 3, 7, 3, 1].to_vec()
    let tmp = path::join(env::temp_dir(), "gos_rel_packed_helpers.bin")
    fs::write(tmp, payload)?
    let mut bytes = fs::read(tmp)?
    fs::remove_file(tmp)?
    if let Some(f) = bytes.first() {
        println("first = {}", f)
    }
    if let Some(l) = bytes.last() {
        println("last = {}", l)
    }
    if let Some(i) = bytes.index_of(7) {
        println("index_of 7 = {}", i)
    }
    println("count_of 3 = {}", bytes.count_of(3))
    println("contains 9 = {}", bytes.contains(9))
    println("contains 8 = {}", bytes.contains(8))
    let r = bytes.iter().rev().collect()
    println("rev = {} {} {} {} {}", r[0], r[1], r[2], r[3], r[4])
    if let Some(p) = bytes.pop() {
        println("pop = {}", p)
    }
    println("len after pop = {}", bytes.len())
    Ok(())
}
"#,
        "first = 9\nlast = 1\nindex_of 7 = 2\ncount_of 3 = 2\ncontains 9 = true\ncontains 8 = false\nrev = 1 3 7 3 9\npop = 1\nlen after pop = 4\n",
    );
}

#[test]
fn release_http_surface_offline_probe_is_byte_exact() {
    // The offline HTTP surface fixture (constructed Response +
    // chained with_header read-back, the struct-literal Response,
    // Client::builder configuration, and http::request's
    // unknown-method error text) must hold byte-exact in the
    // release tier. Reuses the tier-parity fixture source so the
    // two gates cannot drift apart.
    assert_release_stdout_eq(
        "http_surface_offline",
        include_str!("../../../feature-testing-examples/http_surface.gos"),
        "r_status=201 r_body=made r_ct=text/plain; charset=utf-8\n\
         x-tag=v2 x-extra=e\n\
         s_status=202 s_body=lit s_ct=text/plain; charset=utf-8\n\
         client=built\n\
         err=http::request: unknown method `BOGUS`\n",
    );
}

#[test]
fn release_error_chain_renders_outer_mid_root() {
    // `errors::wrap` chains must Display as "outer: mid: root" on
    // the release tier - the cause chain renders colon-separated
    // from the outermost wrap inward, matching the VM.
    assert_release_stdout_eq(
        "error_chain_render",
        r#"
use std::errors

fn main() {
    let e = errors::wrap(errors::wrap(errors::new("root"), "mid"), "outer")
    println("{}", e)
    println("msg={} is_root={}", e.message(), errors::is(e, "root"))
}
"#,
        "outer: mid: root\nmsg=outer is_root=true\n",
    );
}

// ---------------------------------------------------------------
// Rejection gates: programs the toolchain must REFUSE to build,
// with the right diagnostic code. A silent acceptance here means
// the compiled tiers would run the program at wrong semantics
// (64-bit i128, pointer-printed payloads, missing symbols).
// ---------------------------------------------------------------

/// Asserts `gos build --release` refuses `body` and the stderr
/// names diagnostic `code`.
fn assert_release_build_rejects(name: &str, body: &str, code: &str) {
    let dir = fresh_dir(name);
    let source = dir.join(format!("{name}.gos"));
    std::fs::write(&source, body).expect("write source");
    let out = Command::new(gos_bin())
        .arg("build")
        .arg("--release")
        .arg(&source)
        .output()
        .expect("spawn gos build --release");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !out.status.success(),
        "{name}: expected the build to be refused, got success\n--- stderr ---\n{stderr}"
    );
    assert!(
        stderr.contains(code),
        "{name}: refusal must carry {code}\n--- stderr ---\n{stderr}"
    );
}

#[test]
fn release_rejects_i128_with_gt0014() {
    assert_release_build_rejects(
        "reject_i128",
        r#"
fn main() {
    let n: i128 = 1
    println("{}", n)
}
"#,
        "GT0014",
    );
}

#[test]
fn release_rejects_std_fn_value_of_the_wrong_arity() {
    // A std function named in value position becomes the closure that
    // calls it, so `strings::repeat` is usable as a value - but it takes
    // two parameters where `map_err` hands its callback one, and that
    // count is checked here rather than at run time.
    assert_release_build_rejects(
        "reject_std_fn_value",
        r#"
use std::strings

fn main() {
    let r: Result<i64, String> = Err("x")
    let m = r.map_err(strings::repeat)
    if let Err(e) = m { println("{}", e) }
}
"#,
        "GT0001",
    );
}

#[test]
fn release_accepts_a_std_fn_of_matching_arity_as_a_value() {
    // The whole point of the rewrite: a std function the old table did
    // not list now works as a value on the compiled tiers.
    assert_release_stdout_eq(
        "accept_std_fn_value",
        r#"
use std::math

fn main() {
    println("{:?}", #[1.0, -2.0].map(math::abs))
}
"#,
        "#[1.0, 2.0]\n",
    );
}

#[test]
fn release_rejects_swapped_option_combinator_with_gr0021() {
    // Every std free function takes its data first; written the other way
    // round the runtime reads the closure as the data value and silently
    // answers `None`, so the checker rejects the call instead.
    assert_release_build_rejects(
        "reject_swapped_option_combinator",
        r#"
use std::option

fn main() {
    let a = option::and_then(|x: i64| Some(x * 2), Some(5))
    println("{:?}", a)
}
"#,
        "GR0021",
    );
}

#[test]
fn release_rejects_swapped_result_combinator_with_gr0021() {
    assert_release_build_rejects(
        "reject_swapped_result_combinator",
        r#"
use std::result

fn main() {
    let r: Result<i64, String> = Ok(5)
    let m = result::map(|x: i64| x * 2, r)
    println("{:?}", m)
}
"#,
        "GR0021",
    );
}

#[test]
fn release_rejects_count_with_predicate_as_wrong_arity() {
    // `iter::count` now has an edition-aware checker row and remains a
    // one-argument terminal. The realistic mistake is reaching for `count`
    // where `count_by` is the predicate-taking form, so reject the extra
    // closure as an arity error.
    assert_release_build_rejects(
        "reject_closure_param",
        r#"
use std::iter

fn main() {
    let xs = [1, 2, 3]
    let n = iter::count(|x: i64| x > 1, xs)
    println("{}", n)
}
"#,
        "GT0018",
    );
}

#[test]
fn release_rejects_unknown_json_value_ctor_with_gr0001() {
    // `json::Value::string` (lowercase s) binds nothing at runtime;
    // the constructors are a closed, fully-registered set, so an
    // unknown spelling is rejected at check instead of GX0002 at run.
    assert_release_build_rejects(
        "reject_json_value_ctor_typo",
        r#"
use std::encoding::json

fn main() {
    let v = json::Value::string("x")
    println("{}", json::render(v))
}
"#,
        "GR0001",
    );
}

#[test]
fn release_rejects_process_command_builder_with_gr0001() {
    // `process::Command` is Rust-internal surface; no `process::Type`
    // path binds at runtime, so the builder spelling is rejected at
    // check instead of GX0002 at run.
    assert_release_build_rejects(
        "reject_process_command_builder",
        r#"
use std::process

fn main() {
    let c = process::Command::new("echo")
    println("{:?}", c)
}
"#,
        "GR0001",
    );
}
