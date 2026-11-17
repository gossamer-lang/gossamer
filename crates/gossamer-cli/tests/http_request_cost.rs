//! CPU cost per request for the release-tier HTTP server.
//!
//! The floor a handler cannot go below is what the server itself spends:
//! accept, parse, route, dispatch, and write. This measures it against a
//! constant handler over keep-alive connections and holds it to a multiple of
//! the same machine's raw-socket floor, so a change that adds per-request work
//! to the server path is caught here rather than in a downstream benchmark.

#![allow(missing_docs)]
// `/proc/<pid>/stat` is the only CPU-per-process source this measurement has,
// so the whole fixture is Linux-only.
#![cfg(target_os = "linux")]

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Instant;

mod common;

const SOURCE: &str = r#"
use std::http
use std::http::router

fn hello(_r: http::Request) -> Result<http::Response, errors::Error> {
    Ok(http::Response::json(200, "{\"ok\":true}"))
}

let rt = router::Router::new().get("/x", hello)
let server = http::Server::new()
server.listen("127.0.0.1:0")?
println(server.addr())
server.serve(rt)?
"#;

fn build_release(name: &str, source: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("gos-http-cost-{name}-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create fixture directory");
    let source_path = dir.join(format!("{name}.gos"));
    fs::write(&source_path, source).expect("write fixture");
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

/// Drives `count` keep-alive GETs down one connection, returning when every
/// response has been read.
fn drive(addr: &str, count: usize) {
    let stream = TcpStream::connect(addr).expect("connect to server");
    stream.set_nodelay(true).expect("nodelay");
    let mut writer = stream.try_clone().expect("clone stream");
    let mut reader = BufReader::new(stream);
    let request = b"GET /x HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\n\r\n";
    let mut line = String::new();
    for _ in 0..count {
        writer.write_all(request).expect("write request");
        writer.flush().expect("flush request");
        let mut content_length: Option<usize> = None;
        loop {
            line.clear();
            let n = reader.read_line(&mut line).expect("read header line");
            assert!(n > 0, "server closed the connection mid-response");
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some(value) = trimmed
                .strip_prefix("Content-Length:")
                .or_else(|| trimmed.strip_prefix("content-length:"))
            {
                content_length = Some(value.trim().parse().expect("content length"));
            }
        }
        let body_len = content_length.expect("server must send Content-Length");
        let mut body = vec![0u8; body_len];
        std::io::Read::read_exact(&mut reader, &mut body).expect("read body");
    }
}

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const WORKERS: usize = 8;
const PER_WORKER: usize = 4_000;
const TOTAL: usize = WORKERS * PER_WORKER;

/// Runs the measured load against a server fixture (see
/// [`common::spawn_listening`]), and answers the CPU microseconds it spent
/// per request.
/// Confines this thread, and so the servers and load threads it starts, to
/// at most four of the CPUs it may run on. Both servers are then measured on
/// the same CPUs whatever the host: spread across many cores, the floor's
/// connections hand each request between threads on different cores, and it
/// costs more than the server it is the floor for.
// std has no API for a thread's CPU affinity; `sched_setaffinity` is the call.
#[allow(unsafe_code)]
fn pin_to_four_cpus() {
    // SAFETY: `set` is a plain bitmask both calls read and write in place,
    // sized as they are told; pid 0 names the calling thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        let size = std::mem::size_of::<libc::cpu_set_t>();
        assert_eq!(
            libc::sched_getaffinity(0, size, &raw mut set),
            0,
            "read affinity"
        );
        let mut pinned: libc::cpu_set_t = std::mem::zeroed();
        let mut kept = 0;
        for cpu in 0..libc::CPU_SETSIZE as usize {
            if kept < 4 && libc::CPU_ISSET(cpu, &set) {
                libc::CPU_SET(cpu, &mut pinned);
                kept += 1;
            }
        }
        assert_eq!(
            libc::sched_setaffinity(0, size, &raw const pinned),
            0,
            "set affinity"
        );
    }
}

fn measure_cpu_per_request(label: &str, binary: &Path) -> f64 {
    let (child, addr) = common::spawn_listening(binary);
    let server = ServerGuard(child);
    let pid = server.0.id();
    let addr = addr.as_str();

    // One warm pass so the measured window excludes first-connection setup.
    drive(addr, 200);

    let before = common::proc_cpu_ticks(pid).expect("read the server's CPU");
    let switches_before = common::proc_context_switches(pid);
    let faults_before = common::proc_minor_faults(pid);
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| drive(addr, PER_WORKER));
        }
    });
    let wall = start.elapsed();
    let after = common::proc_cpu_ticks(pid).expect("read the server's CPU");

    let switches = common::proc_context_switches(pid) - switches_before;
    let faults = common::proc_minor_faults(pid) - faults_before;
    let micros = common::cpu_micros_per_request(after - before, TOTAL);
    let per = |n: u64| n as f64 / TOTAL as f64;
    println!(
        "{label}: {micros:.2} us cpu/request over {TOTAL} requests in {wall:?} \
         ({:.2} context switches, {:.2} minor faults per request)",
        per(switches),
        per(faults)
    );
    micros
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "measures release-tier cost: a debug `gos` embeds a debug runtime; \
              the perf workflow runs it with --release"
)]
fn release_http_server_holds_its_per_request_cpu_ceiling() {
    pin_to_four_cpus();
    let http = build_release("httpbase", SOURCE);
    let served = measure_cpu_per_request("http constant handler", &http);

    let floor_binary = build_release("tcpfloor", common::TCP_RESPONSE_FLOOR_SOURCE);
    let floor = measure_cpu_per_request("raw socket floor", &floor_binary);

    // Most of a request is the read and the write the floor fixture also
    // makes, so the two sit within about 1.5x of each other; the bound leaves
    // room for a loaded runner while still catching a return to the per-request
    // peer-watch registration, the eager request context, and the per-response
    // HTTP-date rendering, which together cost half as much again.
    assert!(
        served <= floor * 3.0,
        "HTTP server per-request CPU regressed: {served:.2} us against a \
         {floor:.2} us raw-socket floor on this machine"
    );
}
