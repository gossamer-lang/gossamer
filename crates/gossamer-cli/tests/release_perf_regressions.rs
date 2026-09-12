//! Release-tier performance-shape regressions for production-like hot loops.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

mod common;

static SERIAL: AtomicU64 = AtomicU64::new(0);

fn fixture(name: &str, source: &str) -> (PathBuf, PathBuf) {
    let id = SERIAL.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-release-perf-{name}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create fixture directory");
    let source_path = dir.join(format!("{name}.gos"));
    fs::write(&source_path, source).expect("write fixture");
    (dir, source_path)
}

fn build_release(name: &str, source: &str) -> PathBuf {
    let (dir, source_path) = fixture(name, source);
    let output = Command::new(env!("CARGO_BIN_EXE_gos"))
        .args(["build", "--release", "--out-dir"])
        .arg(&dir)
        .arg(&source_path)
        .output()
        .expect("run gos build --release");
    assert!(
        output.status.success(),
        "release build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    dir.join(name)
}

fn timed(binary: &Path, n: usize) -> Duration {
    let start = Instant::now();
    let output = Command::new(binary)
        .arg(n.to_string())
        .output()
        .expect("run release fixture");
    let elapsed = start.elapsed();
    assert!(
        output.status.success(),
        "fixture failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    elapsed
}

fn assert_linear_and_fast(name: &str, small: Duration, large: Duration, ceiling: Duration) {
    assert!(
        large <= ceiling,
        "{name} exceeded its release speed ceiling: {large:?} > {ceiling:?}"
    );
    // Include a fixed allowance for process startup and noisy shared CI hosts.
    assert!(
        large <= small.saturating_mul(3) + Duration::from_millis(250),
        "{name} lost near-linear scaling: small={small:?}, large={large:?}"
    );
}

fn assert_bounded_live_vecs(binary: &Path) {
    let output = Command::new(binary)
        .arg("1000")
        .env("GOS_LEAK_LEDGER", "1")
        .output()
        .expect("run release fixture with allocation ledger");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let live = stderr
        .split("vec=")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .and_then(|n| n.parse::<usize>().ok())
        .expect("allocation ledger must report live Vec count");
    assert!(
        live <= 3,
        "radix ping-pong retained per-pass Vec clones: {stderr}"
    );
}

/// A port nothing is listening on, learned by binding and letting go.
#[cfg(target_os = "linux")]
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Runs the measured load against a server binary started with `arg` on
/// `port`, and answers the CPU microseconds it spent per request.
#[cfg(target_os = "linux")]
fn server_cpu_per_request(label: &str, binary: &Path, arg: String, port: u16) -> f64 {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let mut server = Command::new(binary)
        .arg(arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the server");

    // Wait for the listener rather than sleeping a fixed amount.
    let addr = format!("127.0.0.1:{port}");
    let mut ready = false;
    for _ in 0..600 {
        if TcpStream::connect(&addr).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "the server never accepted a connection");

    let request = b"GET /ping HTTP/1.1\r\nHost: x\r\n\r\n";
    let per_worker = 4_000usize;
    let workers = 4usize;
    let drive = |count: usize| {
        let addr = addr.clone();
        std::thread::spawn(move || {
            let mut sock = TcpStream::connect(&addr).expect("connect");
            sock.set_nodelay(true).ok();
            let mut buf = [0u8; 4096];
            for _ in 0..count {
                if sock.write_all(request).is_err() {
                    return 0usize;
                }
                // One keep-alive response arrives whole on loopback; a short
                // read would desynchronise the next request, so a failure ends
                // this worker rather than being retried.
                match sock.read(&mut buf) {
                    Ok(n) if n > 0 => {}
                    _ => return 0usize,
                }
            }
            count
        })
    };

    // A warm pass first, so the measured one is not paying for the accept or
    // the first-touch of the connection scratch.
    let warm: Vec<_> = (0..workers).map(|_| drive(500)).collect();
    let warmed: usize = warm.into_iter().map(|h| h.join().unwrap_or(0)).sum();
    assert!(warmed > 0, "no request was answered");

    let before = common::proc_cpu_ticks(server.id()).expect("read the server's CPU");
    let handles: Vec<_> = (0..workers).map(|_| drive(per_worker)).collect();
    let answered: usize = handles.into_iter().map(|h| h.join().unwrap_or(0)).sum();
    let after = common::proc_cpu_ticks(server.id()).expect("read the server's CPU");
    let _ = server.kill();
    let _ = server.wait();

    assert_eq!(
        answered,
        workers * per_worker,
        "every worker must finish its requests"
    );
    let micros = common::cpu_micros_per_request(after - before, answered);
    eprintln!("{label}: {answered} requests, {micros:.2} us of CPU each");
    micros
}

/// The CPU an HTTP server spends per request it answers.
///
/// Most of a request is two system calls - one read, one write - so the figure
/// to hold is the ratio against a raw-socket server measured on the same
/// machine, not a microsecond count: syscalls on a virtualised CI runner cost
/// several times what they cost on a workstation. The bound catches a change
/// that multiplies the HTTP layer's own work, not a few hundred nanoseconds.
#[test]
#[cfg(target_os = "linux")]
fn http_server_cpu_per_request_stays_near_its_syscall_floor() {
    let port = free_port();
    let binary = build_release(
        "http_cpu_floor",
        r#"
use std::{env, http}
use std::http::router
let addr = env::args()[0]
let routes = router::Router::new()
    .get("/ping", |_req, _p| http::Response::text(200, "ok"))
let _ = http::serve(addr, routes)
"#,
    );
    let served = server_cpu_per_request("http floor", &binary, format!("127.0.0.1:{port}"), port);

    let raw_port = free_port();
    let raw = build_release("tcp_response_floor", common::TCP_RESPONSE_FLOOR_SOURCE);
    let floor = server_cpu_per_request(
        "raw socket floor",
        &raw,
        format!("127.0.0.1:{raw_port}"),
        raw_port,
    );

    assert!(
        served <= floor * 3.0,
        "HTTP server spent {served:.1} us of CPU per request against a \
         {floor:.1} us raw-socket floor on this machine, so the HTTP layer \
         costs a multiple of the two system calls a request makes"
    );
}

#[test]
fn json_serde_like_release_work_stays_linear_and_fast() {
    let binary = build_release(
        "json_serde_shape",
        r#"
use std::{encoding::json, env}

let n: i64 = env::args()[0].to_i64().unwrap_or(3000)
let mut text = String::with_capacity(1024)
text.push_str("[")
for i in 0..n {
    if i > 0 { text.push_str(",") }
    text.push_str("{\"id\":")
    text.push_str(i.to_string())
    text.push_str(",\"name\":\"user-")
    text.push_str(format("{:06}", i))
    text.push_str("\",\"tags\":[\"a\",\"b\"]}")
}
text.push_str("]")
let parsed = json::parse(text)?
let rendered = json::render(parsed)
let mut checksum = 0
let mut i = 0
while i < rendered.len() {
    checksum += rendered.byte_at(i)
    i += 1
}
println("{} {}", rendered.len(), checksum)
"#,
    );
    let small = timed(&binary, 3_000);
    let large = timed(&binary, 6_000);
    assert_linear_and_fast("json-serde-like", small, large, Duration::from_secs(3));
}

/// A read-only `Vec<u8>` parameter crosses a call as the handle, so a fixed
/// number of calls costs the same whatever the buffer holds. A body that binds
/// a scalar element of the parameter (`let byte = buf[i]`) and hands it on is
/// the shape that used to withdraw the parameter and clone the whole buffer
/// once per call, which turns a row read into a pass over the file it sits in.
#[test]
fn read_only_buffer_parameter_cost_is_independent_of_its_size() {
    let binary = build_release(
        "shared_buffer_param",
        r#"
use std::env

fn classify(byte: u8) -> i64 { if byte > 128u8 { 1 } else { 0 } }

fn scan(buf: Vec<u8>, start: i64, end: i64) -> i64 {
    let mut total = 0
    let mut i = start
    while i < end {
        let byte = buf[i]
        total += classify(byte) + (byte as i64)
        i += 1
    }
    total
}

let bytes: i64 = env::args()[0].to_i64().unwrap_or(65536)
let buf: Vec<u8> = #[7u8; bytes]
let mut acc = 0
for i in 0..20_000 { acc += scan(buf, 0, 16) }
println("{}", acc)
"#,
    );
    let small = timed(&binary, 64 * 1024);
    let large = timed(&binary, 4 * 1024 * 1024);
    assert!(
        large <= small.saturating_mul(3) + Duration::from_millis(250),
        "a read-only buffer parameter is being copied per call: \
         64 KiB={small:?}, 4 MiB={large:?}"
    );
}

#[test]
fn radix_sort_like_release_work_stays_linear_and_fast() {
    let binary = build_release(
        "radix_sort_shape",
        r#"
use std::env

let n: i64 = env::args()[0].to_i64().unwrap_or(500000)
let mut src: Vec<i64> = Vec::with_capacity(n)
let mut dst: Vec<i64> = Vec::with_capacity(n)
for i in 0..n {
    src.push(i * 6364136223846793005 + 1442695040888963407)
    dst.push(0)
}
let mut pass = 0
while pass < 8 {
    let shift = pass * 8
    let mut count = [0; 256]
    for i in 0..n { count[(src[i] >> shift) & 0xff] += 1 }
    let mut total = 0
    for k in 0..256 {
        let c = count[k]
        count[k] = total
        total += c
    }
    for i in 0..n {
        let byte = (src[i] >> shift) & 0xff
        let pos = count[byte]
        count[byte] = pos + 1
        dst[pos] = src[i]
    }
    let tmp = src
    src = dst
    dst = tmp
    pass += 1
}
println("{}", src[0])
"#,
    );
    let small = timed(&binary, 500_000);
    let large = timed(&binary, 1_000_000);
    assert_linear_and_fast("radix-sort-like", small, large, Duration::from_secs(3));
    assert_bounded_live_vecs(&binary);
}

/// Runs `binary` twice with the ledger on and asserts the named family's
/// live count at exit does not grow with the iteration count.
fn assert_ledger_flat(binary: &Path, family: &str, small: usize, large: usize) {
    let live = |n: usize| -> usize {
        let output = Command::new(binary)
            .arg(n.to_string())
            .env("GOS_LEAK_LEDGER", "1")
            .output()
            .expect("run release fixture with allocation ledger");
        let stderr = String::from_utf8_lossy(&output.stderr);
        stderr
            .split(&format!("{family}="))
            .nth(1)
            .and_then(|tail| tail.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("ledger must report {family}: {stderr}"))
    };
    let (a, b) = (live(small), live(large));
    assert!(
        b <= a + 4,
        "{family} live count grows with N: {small} -> {a}, {large} -> {b}"
    );
}

/// Asserts the fixture's own output, so a reclaim that frees storage still in
/// use fails here rather than passing as a flat ledger over wrong answers.
fn assert_output(binary: &Path, n: usize, expected: &str) {
    let output = Command::new(binary)
        .arg(n.to_string())
        .output()
        .expect("run release fixture");
    assert!(
        output.status.success(),
        "fixture failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        expected,
        "fixture answered the wrong value"
    );
}

#[test]
fn bool_to_string_releases_its_answer() {
    let binary = build_release(
        "bool_to_string_release",
        r#"
use std::env
let n: i64 = env::args()[0].to_i64().unwrap_or(1000)
let mut total: i64 = 0
for i in 0..n { total += true.to_string().len() + (i % 2) }
println("{}", total)
"#,
    );
    assert_ledger_flat(&binary, "str", 1_000, 20_000);
}

#[test]
fn scalar_and_stdlib_string_answers_are_released() {
    let binary = build_release(
        "stdlib_string_release",
        r#"
use std::{encoding::base64, encoding::hex, env, net::url, path, strconv, strings, uuid}
let n: i64 = env::args()[0].to_i64().unwrap_or(1000)
let mut total: i64 = 0
for i in 0..n {
    total += 'x'.to_string().len()
    total += strconv::format_i64(i).len()
    total += strconv::format_f64(1.5).len()
    total += base64::encode("abc".as_bytes()).len()
    total += hex::encode("abc".as_bytes()).len()
    total += path::join("a", "b").len()
    total += uuid::v4().len()
    total += url::query_escape("a b").len()
    total += format("{:>8}", i).len()
    total += format("{:x}", i).len()
    total += strings::join(#["a", "b"], ",").len()
    total += #[1, 2, 3].to_string().len()
    total += (1, 2).to_string().len()
}
println("{}", total)
"#,
    );
    assert_ledger_flat(&binary, "str", 1_000, 20_000);
}

#[test]
fn field_reassignment_through_mut_ref_releases_old_value() {
    let binary = build_release(
        "mut_ref_field_reassign",
        r#"
use std::env
struct Holder { pending: Vec<u8> }
fn shift(h: &mut Holder) { h.pending = h.pending.slice(1, h.pending.len()).unwrap_or(#[]) }
let n: i64 = env::args()[0].to_i64().unwrap_or(1000)
let mut h = Holder { pending: #[] }
let mut total: i64 = 0
for i in 0..n {
    h.pending.extend("line\n".as_bytes())
    shift(&mut h)
    total += h.pending.len() + i
}
println("{}", total)
"#,
    );
    assert_ledger_flat(&binary, "vec", 1_000, 20_000);
}

#[test]
fn vec_field_read_through_mut_ref_releases_its_share() {
    let binary = build_release(
        "mut_ref_vec_field_read",
        r#"
use std::env
struct Tk { kind: u8, text: String }
struct Parser { toks: Vec<Tk>, pos: i64 }
fn peek(p: &mut Parser) -> i64 {
    if p.pos >= p.toks.len() { return 0 }
    p.toks[p.pos].kind as i64
}
fn bump(p: &mut Parser) -> String {
    let t = p.toks[p.pos]
    p.pos += 1
    t.text
}
let n: i64 = env::args()[0].to_i64().unwrap_or(1000)
let mut total: i64 = 0
for i in 0..n {
    let mut p = Parser { toks: #[Tk { kind: 1, text: "select" }, Tk { kind: 2, text: "name" }], pos: 0 }
    total += peek(&mut p) + bump(&mut p).len()
}
println("{}", total)
"#,
    );
    assert_ledger_flat(&binary, "vec", 1_000, 20_000);
    assert_output(&binary, 1_000, "7000");
}

#[test]
fn struct_copied_from_vec_index_releases_moved_field() {
    let binary = build_release(
        "vec_index_struct_field_move",
        r#"
use std::env
struct Tk { kind: u8, text: String }
struct Parser { toks: Vec<Tk>, pos: i64 }
fn take_word(p: &mut Parser) -> Result<String, String> {
    let t = p.toks[p.pos]
    p.pos += 1
    if t.kind == 1 { return Ok(t.text) }
    Err("not a word")
}
let n: i64 = env::args()[0].to_i64().unwrap_or(1000)
let mut total: i64 = 0
let mut p = Parser { toks: #[Tk { kind: 1, text: "select" + n.to_string() }, Tk { kind: 1, text: "name" + n.to_string() }], pos: 0 }
for i in 0..n {
    p.pos = i % 2
    total += take_word(&mut p).unwrap_or("").len()
}
println("{}", total)
"#,
    );
    assert_ledger_flat(&binary, "str", 1_000, 20_000);
}

#[test]
fn loop_local_struct_releases_its_vec_field() {
    let binary = build_release(
        "loop_local_struct_vec_field",
        r#"
use std::env
struct Tk { kind: u8, text: String }
struct Parser { toks: Vec<Tk>, pos: i64 }
let n: i64 = env::args()[0].to_i64().unwrap_or(1000)
let mut total: i64 = 0
for i in 0..n {
    let p = Parser { toks: #[Tk { kind: 1, text: "a" }, Tk { kind: 1, text: "b" }], pos: i }
    total += p.toks.len() + p.pos
}
println("{}", total)
"#,
    );
    assert_ledger_flat(&binary, "vec", 1_000, 20_000);
}

#[test]
fn enum_payload_vecs_release_after_question_mark_and_match() {
    let binary = build_release(
        "enum_payload_vec_release",
        r#"
use std::env
enum Stmt { Select { table: String, cols: Vec<String>, conds: Vec<i64>, limit: i64 }, Compact }
fn make(i: i64) -> Result<Stmt, String> {
    Ok(Stmt::Select { table: "t", cols: #["a", "b" + i.to_string()], conds: #[i], limit: i })
}
fn consume(i: i64) -> Result<i64, String> {
    let s = make(i)?
    match s {
        Stmt::Select { table, cols, conds, limit } => Ok(cols.len() + conds.len() + limit + table.len()),
        Stmt::Compact => Ok(0),
    }
}
let n: i64 = env::args()[0].to_i64().unwrap_or(1000)
let mut total: i64 = 0
for i in 0..n { total += consume(i).unwrap_or(0) }
println("{}", total)
"#,
    );
    assert_ledger_flat(&binary, "vec", 1_000, 20_000);
    assert_ledger_flat(&binary, "str", 1_000, 20_000);
}

#[test]
fn enum_payload_matched_inline_releases_its_node() {
    let binary = build_release(
        "enum_payload_inline_match",
        r#"
use std::env
enum Stmt { Select { table: String, cols: Vec<String>, conds: Vec<i64>, limit: i64 }, Compact }
fn make(i: i64) -> Result<Stmt, String> {
    Ok(Stmt::Select { table: "t", cols: #["a", "b" + i.to_string()], conds: #[i], limit: i })
}
fn consume(i: i64) -> Result<i64, String> {
    match make(i) {
        Ok(s) => match s {
            Stmt::Select { table, cols, conds, limit } => Ok(cols.len() + conds.len() + limit + table.len()),
            Stmt::Compact => Ok(0),
        },
        Err(e) => Err(e),
    }
}
let n: i64 = env::args()[0].to_i64().unwrap_or(1000)
let mut total: i64 = 0
for i in 0..n { total += consume(i).unwrap_or(0) }
println("{}", total)
"#,
    );
    assert_ledger_flat(&binary, "vec", 1_000, 20_000);
    assert_ledger_flat(&binary, "str", 1_000, 20_000);
}

#[test]
fn carrier_returned_field_payload_outlives_the_frame_that_built_it() {
    let binary = build_release(
        "carrier_field_payload",
        r#"
use std::env
struct Rec { data: Vec<i64>, name: String }
fn take(r: &mut Rec) -> Result<Vec<i64>, String> { Ok(r.data) }
fn takes(r: &mut Rec) -> Result<String, String> { Ok(r.name) }
let n: i64 = env::args()[0].to_i64().unwrap_or(50)
let mut keep: Vec<Vec<i64>> = #[]
let mut names: Vec<String> = #[]
for i in 0..n {
    let mut r = Rec { data: #[i, i + 1, i + 2], name: "n" + i.to_string() }
    keep.push(take(&mut r).unwrap_or(#[]))
    names.push(takes(&mut r).unwrap_or(""))
}
let mut total: i64 = 0
for v in keep { total += v[0] + v[1] + v[2] }
for s in names { total += s.len() }
println("{}", total)
"#,
    );
    assert_output(&binary, 2_000, "6011890");
    assert_ledger_flat(&binary, "vec", 200, 2_000);
}

#[test]
fn http_response_constructors_release_the_body_they_copy() {
    let binary = build_release(
        "http_response_body_release",
        r#"
use std::{env, http}
let n: i64 = env::args()[0].to_i64().unwrap_or(1000)
let mut total: i64 = 0
for i in 0..n {
    let r = http::Response::json(200, "{\"id\":" + i.to_string() + "}")
    let t = http::Response::text(200, "x" + i.to_string())
    total += r.status + t.status
}
println("{}", total)
"#,
    );
    assert_ledger_flat(&binary, "str", 1_000, 20_000);
}

#[test]
fn closure_capturing_a_map_struct_does_not_copy_it_per_call() {
    let binary = build_release(
        "closure_capture_map_struct",
        r#"
use std::env
struct Store { index: Map<String, i64>, name: String }
fn hit(s: &mut Store, key: String) -> i64 { s.index.get(key).unwrap_or(0) }
let n: i64 = env::args()[0].to_i64().unwrap_or(1000)
let mut st = Store { index: Map::new(), name: "s" }
for i in 0..50000 { st.index.insert("k" + i.to_string(), i) }
let label = env::args().len().to_string()
let probe = |i: i64| hit(&mut st, "k" + (i % 50000).to_string()) + label.len()
let mut total: i64 = 0
for i in 0..n { total += probe(i) }
println("{}", total)
"#,
    );
    assert_output(&binary, 2_000, "2001000");
    let small = timed(&binary, 2_000);
    let large = timed(&binary, 4_000);
    assert_linear_and_fast(
        "closure-capture-map-struct",
        small,
        large,
        Duration::from_millis(800),
    );
}

/// A struct passed by value to a callee that only reads it shares the caller's
/// storage, so the call costs the same whatever its `Vec` field holds.
///
/// The clone that gives a by-value copy its own growable storage is observable
/// only when the callee can write the parameter or let it outlive the call.
/// When it can do neither, the clone is pure cost, and it is paid per call, so
/// it scales with the field rather than with the work: the wide run below ran
/// sixty times the narrow one when every call copied the column vector.
#[test]
fn struct_passed_by_value_to_a_reading_callee_shares_its_vec_field() {
    let binary = build_release(
        "struct_byvalue_field_share",
        r#"
use std::env
struct Row { id: i64, cols: Vec<String> }
fn width(r: Row) -> i64 { r.cols.len() }
let w: i64 = env::args()[0].to_i64().unwrap_or(8)
let mut cols: Vec<String> = #[]
for i in 0..w { cols.push("column-" + i.to_string()) }
let row = Row { id: 1, cols: cols }
let mut total: i64 = 0
for _i in 0..200000 { total += width(row) }
println("{}", total)
"#,
    );
    assert_output(&binary, 8, "1600000");
    let narrow = timed(&binary, 8);
    let wide = timed(&binary, 512);
    assert_linear_and_fast(
        "struct-by-value-vec-field",
        narrow,
        wide,
        Duration::from_millis(800),
    );
}

#[test]
fn carrier_payload_discarded_by_a_wildcard_arm_is_released() {
    let binary = build_release(
        "carrier_wildcard_discard",
        r#"
use std::env
enum Stmt { Select { table: String, cols: Vec<String>, conds: Vec<i64>, limit: i64 }, Compact }
fn parse(src: String) -> Result<Stmt, String> {
    Ok(Stmt::Select { table: src, cols: #["a"], conds: #[1], limit: 5 })
}
let n: i64 = env::args()[0].to_i64().unwrap_or(1000)
let mut total: i64 = 0
for i in 0..n {
    match parse("sel" + (i % 3).to_string()) {
        Ok(_) => total += 1
        Err(_) => total += 0
    }
}
println("{}", total)
"#,
    );
    assert_output(&binary, 1_000, "1000");
    assert_ledger_flat(&binary, "str", 1_000, 20_000);
    assert_ledger_flat(&binary, "vec", 1_000, 20_000);
}

#[test]
fn returned_aggregate_map_field_survives_the_builder() {
    let binary = build_release(
        "aggr_map_field_return",
        r#"
use std::env
struct C { memory: Map<i64, i64>, _a: i64, _b: i64, v1: Vec<i64>, v2: Vec<i64> }
impl C {
    fn new(program: Vec<i64>) -> Self {
        let mut m = Map::new()
        for (addr, value) in program.enumerate() { m.insert(addr, value) }
        C { memory: m, _a: 0, _b: 0, v1: #[], v2: #[] }
    }
    fn feed(&mut self, items: [i64]) { self.v1.extend(items) }
}
let n: i64 = env::args()[0].to_i64().unwrap_or(3)
let p: Vec<i64> = #[104, 5, 99]
let mut total: i64 = 0
for _ in 0..n {
    let mut c = C::new(p)
    c.feed([])
    total += c.memory.get_or(0, 0)
}
println("{}", total)
"#,
    );
    assert_output(&binary, 3, "312");
}

/// A window read costs the window, not the buffer it sits in - on every tier.
///
/// `crc32::update_window` and `String::push_utf8` are given a buffer and a
/// `[start, end)` inside it. Reading the whole buffer to reach the window
/// makes each call scale with the buffer, which a store that keeps a file
/// resident pays on every record it checks. The interpreter is the tier that
/// can regress here alone: the compiled tiers reach the bytes through the
/// runtime's window accessor.
///
/// The fixture times its own read loop, because building the buffer scales
/// with the buffer on any tier and would otherwise be what the test measured.
#[test]
fn window_reads_cost_the_window_not_the_buffer() {
    const SOURCE: &str = r#"
use std::{env, time}
use std::hash::crc32

fn main() {
    let mb = env::args().first().unwrap_or("1").to_i64().unwrap_or(1)
    let mut buf: Vec<u8> = #[]
    let mut i = 0
    while i < mb * 1048576 { buf.push((i & 255) as u8); i += 1 }

    let start = time::monotonic_ms()
    let mut crc = 0
    let mut out = ""
    let mut k = 0
    while k < 2000 {
        crc = crc32::update_window(crc, buf, 0, 45)
        let _ = out.push_utf8(buf, 0, 8)
        out = ""
        k += 1
    }
    println("{} {}", time::monotonic_ms() - start, crc)
}
"#;
    let (dir, source_path) = fixture("window_cost", SOURCE);
    let read_ms = |mb: &str| -> i64 {
        let output = Command::new(env!("CARGO_BIN_EXE_gos"))
            .env("GOS_JIT", "0")
            .arg("run")
            .arg(&source_path)
            .arg(mb)
            .output()
            .expect("run the window fixture on the interpreter");
        assert!(
            output.status.success(),
            "window fixture failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<i64>().ok())
            .expect("fixture must report the milliseconds its read loop took")
    };
    // The window is the same size in both runs, so a buffer eight times the
    // size must not make the same 2000 reads take longer.
    let small = read_ms("1");
    let large = read_ms("8");
    let _ = fs::remove_dir_all(&dir);
    assert!(
        large <= small.max(20) * 4,
        "window reads scaled with the buffer rather than the window: \
         1 MiB={small}ms, 8 MiB={large}ms"
    );
}

/// A by-value `self` accessor that reaches the aggregate's `Map` field through
/// a SECOND by-value `self` call shares that field with its caller, so the
/// fixed number of calls below costs the lookups it performs and not the table
/// they read: a `GosMap` carries no reference count, so a share minted for the
/// nested parameter copies the whole table on every call. The wide run below
/// took fifty times the narrow one while the nested frame owned a share.
#[test]
fn nested_by_value_self_shares_its_map_field() {
    let binary = build_release(
        "nested_byvalue_map_field_share",
        r#"
use std::env
struct K { mem: Map<i64, i64>, pos: i64 }
impl K {
    fn read(self, a: i64) -> i64 { self.mem.get_or(a, 0) }
    fn read2(self, a: i64) -> i64 { self.read(self.read(a)) }
    fn run(&mut self, n: i64) {
        for _ in 0..n { self.pos = self.read2(self.pos + 1) % 2000 }
    }
}
let size: i64 = env::args()[0].to_i64().unwrap_or(64)
let mut mem = Map::new()
for i in 0..size * 2 { mem.insert(i, (i * 3) % 1_000_000) }
let mut k = K { mem: mem, pos: 0 }
k.run(100_000)
println("{}", k.pos)
"#,
    );
    assert_output(&binary, 64, "9");
    let narrow = timed(&binary, 64);
    let wide = timed(&binary, 4096);
    assert_linear_and_fast(
        "nested-by-value-self-map-field",
        narrow,
        wide,
        Duration::from_millis(800),
    );
}

/// A by-value `self` method that reads only the aggregate's SCALAR field pays
/// for that field, not for the `Map` beside it: the parameter's heap fields
/// are what a share would be minted for, and a scalar in a tuple, a struct
/// literal, or a container names none of them. The wide run below scaled with
/// the table while any field read at all booked its clone.
#[test]
fn scalar_field_read_does_not_copy_the_map_beside_it() {
    let binary = build_release(
        "scalar_field_beside_map",
        r#"
use std::env
struct K { mem: Map<i64, i64>, pos: i64 }
impl K {
    fn pair(self, a: i64) -> i64 {
        let t = (self.pos, a)
        let v = #[self.pos, a]
        t.0 + t.1 + v[0]
    }
}
let size: i64 = env::args()[0].to_i64().unwrap_or(64)
let mut mem = Map::new()
for i in 0..size { mem.insert(i, i) }
let k = K { mem: mem, pos: 3 }
let mut total: i64 = 0
for i in 0..200000 { total += k.pair(i % 8) }
println("{}", total)
"#,
    );
    assert_output(&binary, 64, "1900000");
    let narrow = timed(&binary, 64);
    let wide = timed(&binary, 4096);
    assert_linear_and_fast(
        "scalar-field-beside-map",
        narrow,
        wide,
        Duration::from_millis(800),
    );
}

/// A `mut` by-value aggregate parameter is copied once per call, not twice.
/// The callee's own frame swaps its value-container fields for copies of its
/// own on entry, which is the independent storage a caller-side copy would be
/// for, so a caller that copies the aggregate first pays for the same table
/// again - on every call, against the whole table, because a `GosMap` carries
/// no reference count.
///
/// The claim is about which frame books the copy, so the post-RC MIR answers
/// it directly and the same way on every machine: the caller hands the
/// aggregate over booking nothing, and the callee books exactly one.
#[test]
fn mut_by_value_parameter_copies_its_table_once() {
    const SOURCE: &str = r#"
struct S { m: Map<i64, i64>, n: i64 }
fn writer(mut s: S, k: i64) -> i64 { s.n = k; s.m.get_or(k, 0) }
fn drive(s: S, rounds: i64) -> i64 {
    let mut total = 0
    for i in 0..rounds { total += writer(s, i % 8) }
    total
}
fn main() {
    let mut m = Map::new()
    for i in 0..8 { m.insert(i, i) }
    println("{}", drive(S { m: m, n: 0 }, 200000))
}
"#;
    let (dir, source_path) = fixture("byvalue_copy_shape", SOURCE);
    let output = Command::new(env!("CARGO_BIN_EXE_gos"))
        .env("GOS_DUMP_MIR_RC", "1")
        .arg("run")
        .arg(&source_path)
        .output()
        .expect("run the by-value fixture with the post-RC MIR dump");
    assert!(
        output.status.success(),
        "fixture failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dump = String::from_utf8_lossy(&output.stderr).into_owned();
    let _ = fs::remove_dir_all(&dir);

    // Table clones booked by one body, counted between its header and the next.
    let clones = |name: &str| -> Option<usize> {
        let header = format!("=== MIR(post-rc) {name} ===");
        let body = dump.split(&header).nth(1)?;
        let body = body.split("=== MIR(post-rc) ").next().unwrap_or(body);
        Some(body.matches("gos_rt_map_field_clone").count())
    };

    // A body that never reached the dump would make every count below vacuous.
    let in_drive = clones("drive").unwrap_or_else(|| {
        panic!("the calling body was never lowered, so this test proved nothing:\n{dump}")
    });
    let in_writer = clones("writer").unwrap_or_else(|| {
        panic!("the called body was never lowered, so this test proved nothing:\n{dump}")
    });
    assert_eq!(
        in_drive, 0,
        "the caller copied the table its callee goes on to copy again"
    );
    assert_eq!(
        in_writer, 1,
        "a callee that writes its by-value parameter needs exactly one copy"
    );
}
