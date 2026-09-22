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
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

mod common;

const SOURCE: &str = r#"
use std::{env, http}
use std::http::router

fn hello(_r: http::Request) -> Result<http::Response, errors::Error> {
    Ok(http::Response::json(200, "{\"ok\":true}"))
}

let port = env::args()[0].to_i64().unwrap_or(18777)
let rt = router::Router::new().get("/x", hello)
println("listening")
http::serve("127.0.0.1:" + port.to_string(), rt)?
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

/// A port nothing is listening on, learned by binding and letting go.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn wait_ready(port: u16) -> bool {
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

/// Drives `count` keep-alive GETs down one connection, returning when every
/// response has been read.
fn drive(port: u16, count: usize) {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to server");
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

/// Runs the measured load against `binary`, which is started with `arg` and
/// listens on `port`, and answers the CPU microseconds it spent per request.
fn measure_cpu_per_request(label: &str, binary: &Path, arg: String, port: u16) -> f64 {
    let child = Command::new(binary)
        .arg(arg)
        .spawn()
        .expect("start the server");
    let server = ServerGuard(child);
    let pid = server.0.id();
    assert!(wait_ready(port), "server never accepted a connection");

    // One warm pass so the measured window excludes first-connection setup.
    drive(port, 200);

    let before = common::proc_cpu_ticks(pid).expect("read the server's CPU");
    let switches_before = common::proc_context_switches(pid);
    let faults_before = common::proc_minor_faults(pid);
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| drive(port, PER_WORKER));
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
fn release_http_server_holds_its_per_request_cpu_ceiling() {
    let http_port = free_port();
    let http = build_release("httpbase", SOURCE);
    let served = measure_cpu_per_request(
        "http constant handler",
        &http,
        http_port.to_string(),
        http_port,
    );

    let floor_port = free_port();
    let floor_binary = build_release("tcpfloor", common::TCP_RESPONSE_FLOOR_SOURCE);
    let floor = measure_cpu_per_request(
        "raw socket floor",
        &floor_binary,
        format!("127.0.0.1:{floor_port}"),
        floor_port,
    );

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
