//! Stateful Router parity: same `.gos` source dispatches an
//! HTTP server in both `gos` (interp) and `gos build` →
//! native, with multiple `impl Handler` types registered for
//! different paths. Verifies the constructor + method-chain
//! shape works identically across tiers.

use std::io::{BufRead, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn gos_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push("gos");
    p
}

/// A source path of this run's own, so two tests never write one file.
fn source_path(tag: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    std::env::temp_dir().join(format!("{tag}-{pid}-{nanos}.gos"))
}

fn write_source() -> PathBuf {
    let path = source_path("gos-router-parity");
    let src = r#"
use std::http
use std::http::router

struct Health { }
impl Health {
    fn serve(&self, _r: http::Request) -> Result<http::Response, http::Error> {
        Ok(http::Response::text(200, "ok"))
    }
}

struct Echo { }
impl Echo {
    fn serve(&self, r: http::Request) -> Result<http::Response, http::Error> {
        Ok(http::Response::text(200, format("echo {}", r.path)))
    }
}

fn main() {
    let r = router::Router::new()
    r.get("/health", Health { })
    r.get("/echo", Echo { })
    let s = http::Server::new()
    match s.listen("127.0.0.1:0") {
        Ok(_) => println("{}", s.addr())
        Err(e) => eprintln("listen: {}", e)
    }
    // A server that stops answering says why: `serve` reports a listener
    // that stopped, and reaching past it at all means the loop ended.
    match s.serve(r) {
        Ok(_) => eprintln("serve returned with no error")
        Err(e) => eprintln("serve: {}", e)
    }
}
"#;
    std::fs::write(&path, src).unwrap();
    path
}

fn write_source_bare_fn() -> PathBuf {
    let path = source_path("gos-router-parity-fn");
    let src = r#"
use std::http
use std::http::router

fn health(_r: http::Request) -> Result<http::Response, http::Error> {
    Ok(http::Response::text(200, "ok"))
}

fn echo(r: http::Request) -> Result<http::Response, http::Error> {
    Ok(http::Response::text(200, format("echo {}", r.path)))
}

fn main() {
    let r = router::Router::new()
    r.get("/health", health)
    r.get("/echo", echo)
    let s = http::Server::new()
    match s.listen("127.0.0.1:0") {
        Ok(_) => println("{}", s.addr())
        Err(e) => eprintln("listen: {}", e)
    }
    // A server that stops answering says why: `serve` reports a listener
    // that stopped, and reaching past it at all means the loop ended.
    match s.serve(r) {
        Ok(_) => eprintln("serve returned with no error")
        Err(e) => eprintln("serve: {}", e)
    }
}
"#;
    std::fs::write(&path, src).unwrap();
    path
}

fn write_source_returned_router() -> PathBuf {
    let path = source_path("gos-router-parity-owned");
    let src = r#"
use std::http
use std::http::router

struct Health { body: String }
impl Health {
    fn serve(&self, _r: http::Request) -> Result<http::Response, http::Error> {
        Ok(http::Response::text(200, self.body))
    }
}

struct Echo { prefix: String }
impl Echo {
    fn serve(&self, r: http::Request) -> Result<http::Response, http::Error> {
        Ok(http::Response::text(200, format("{}{}", self.prefix, r.path)))
    }
}

fn routes() -> router::Router {
    let r = router::Router::new()
    r.get("/health", Health { body: "ok" })
    r.get("/echo", Echo { prefix: "echo " })
    r
}

fn main() {
    let r = routes()
    let s = http::Server::new()
    match s.listen("127.0.0.1:0") {
        Ok(_) => println("{}", s.addr())
        Err(e) => eprintln("listen: {}", e)
    }
    // A server that stops answering says why: `serve` reports a listener
    // that stopped, and reaching past it at all means the loop ended.
    match s.serve(r) {
        Ok(_) => eprintln("serve returned with no error")
        Err(e) => eprintln("serve: {}", e)
    }
}
"#;
    std::fs::write(&path, src).unwrap();
    path
}

/// One request's status code and body, or the error that stopped it in
/// place of a code.
///
/// The request is made in this process rather than through `curl`: a
/// loopback request that fails has one reason, and reading it as an
/// `io::Error` names that reason where an external client's exit status
/// leaves a bare zero to guess at.
fn request(addr: SocketAddr, path: &str) -> (String, i32, String) {
    let attempt = || -> std::io::Result<(String, i32)> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut conn = std::io::BufReader::new(stream);
        conn.get_mut().write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )?;
        // The message is framed by its own headers, so the body is taken by
        // length rather than by waiting for a close: a server keeping the
        // connection alive has still answered in full.
        let mut status = String::new();
        conn.read_line(&mut status)?;
        let code = status
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            if conn.read_line(&mut header)? == 0 || header.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; length];
        conn.read_exact(&mut body)?;
        Ok((String::from_utf8_lossy(&body).into_owned(), code))
    };
    // A loopback connect is refused outright when the kernel has no local
    // endpoint to give it, which says nothing about the server it was aimed
    // at, so such an attempt is made again rather than recorded as the
    // server's answer. Every other error is the answer: a refused connection
    // means the listener is gone, and a short read means it stopped talking.
    let mut note = String::new();
    for attempt_index in 0..RETRIES {
        match attempt() {
            Ok((body, code)) => return (body, code, note),
            Err(e) if transient_local(&e) => {
                note = format!("{e} (retried {})", attempt_index + 1);
                std::thread::sleep(Duration::from_millis(20 * (attempt_index + 1) as u64));
            }
            Err(e) => return (String::new(), 0, e.to_string()),
        }
    }
    (
        String::new(),
        0,
        format!("no local endpoint after {RETRIES} attempts: {note}"),
    )
}

/// How many times a request may be re-aimed at the same server before its
/// silence is taken as the answer.
const RETRIES: usize = 6;

/// `true` when the error is the kernel declining a local endpoint for this
/// connection rather than anything the server did. These are the states that
/// clear on their own once an ephemeral port or an interrupted call is free
/// again.
fn transient_local(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::AddrNotAvailable
            | std::io::ErrorKind::AddrInUse
            | std::io::ErrorKind::Interrupted
    )
}

/// Runs one server and asks it for every route.
///
/// The server binds port zero and prints the address the kernel gave
/// it, which is both where to send the requests and the fact that the
/// listener is up: a test that picked the port itself would be talking
/// to whatever else took it in the meantime.
fn run_and_check(cmd: &mut Command) {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut announced = String::new();
    let bound = std::io::BufReader::new(child.stdout.take().expect("the child's stdout"))
        .read_line(&mut announced)
        .expect("read the address the server bound");
    let mut complaint = String::new();
    if bound == 0 {
        // How the server ended is half the answer when it said nothing on the
        // way out: a status names the signal that stopped it, where an empty
        // stderr on its own leaves a crash and a clean early return alike.
        let ended = child
            .wait()
            .map_or_else(|e| e.to_string(), |status| status.to_string());
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut complaint);
        }
        panic!("the server ended before it bound: {ended}; stderr: {complaint}");
    }
    let announced = announced.trim().to_string();
    let addr: SocketAddr = match announced.parse() {
        Ok(addr) => addr,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut complaint);
            }
            panic!(
                "the server announced {announced:?}, which is no address: {e}; stderr: {complaint}"
            );
        }
    };
    let (h_body, h_code, h_note) = request(addr, "/health");
    let (e_body, e_code, e_note) = request(addr, "/echo");
    let (m_body, m_code, m_note) = request(addr, "/missing");
    // What the server was doing while those requests ran is half of any
    // failure here, so it is read before the assertions rather than left
    // for a rerun to guess at.
    let _ = child.kill();
    let ended = child
        .wait()
        .map_or_else(|e| e.to_string(), |s| s.to_string());
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut complaint);
    }
    let server = format!(
        "addr {announced}; server {ended}; stderr: {}; notes: {h_note}|{e_note}|{m_note}",
        complaint.trim()
    );

    assert_eq!(h_code, 200, "/health status ({server})");
    assert_eq!(h_body, "ok", "/health body ({server})");
    assert_eq!(e_code, 200, "/echo status ({server})");
    assert_eq!(e_body, "echo /echo", "/echo body ({server})");
    assert_eq!(m_code, 404, "/missing status ({server})");
    assert_eq!(m_body, "not found", "/missing body ({server})");
}

/// A server that is not there is the answer, not a state to wait out: the
/// retry exists for a kernel that has no local endpoint to give, and must not
/// turn a refused connection into six attempts and a different complaint.
#[test]
fn a_refused_connection_is_reported_rather_than_retried() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port to release");
    let addr = listener.local_addr().expect("the bound address");
    drop(listener);
    let (body, code, note) = request(addr, "/health");
    assert_eq!(code, 0, "nothing answered on a released port");
    assert!(body.is_empty(), "no body came back: {body:?}");
    assert!(
        !note.contains("no local endpoint"),
        "the refusal is reported as itself, not as an exhausted retry: {note}"
    );
}

#[test]
fn router_interp_matches_compiled() {
    let src = write_source();

    // Interp run.
    let mut interp = Command::new(gos_bin());
    interp.arg("run").arg(&src);
    run_and_check(&mut interp);

    // Compiled build + run.
    let build = Command::new(gos_bin())
        .arg("build")
        .arg(&src)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let stem = src.file_stem().unwrap().to_str().unwrap();
    let bin = std::env::temp_dir().join("target").join("debug").join(stem);
    let mut compiled = Command::new(&bin);
    run_and_check(&mut compiled);
    let _ = std::fs::remove_file(&src);
}

#[test]
fn router_bare_fn_interp_matches_compiled() {
    let src = write_source_bare_fn();

    // Interp run with bare-function handlers (no struct + impl).
    let mut interp = Command::new(gos_bin());
    interp.arg("run").arg(&src);
    run_and_check(&mut interp);

    // Compiled build + run.
    let build = Command::new(gos_bin())
        .arg("build")
        .arg(&src)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let stem = src.file_stem().unwrap().to_str().unwrap();
    let bin = std::env::temp_dir().join("target").join("debug").join(stem);
    let mut compiled = Command::new(&bin);
    run_and_check(&mut compiled);
    let _ = std::fs::remove_file(&src);
}

/// A router outlives the function that built it, so the handler it answers
/// from does too: the value registered as a route is the route's for as long
/// as the router lives, not the registering frame's.
#[test]
fn router_built_by_a_helper_outlives_its_frame() {
    let src = write_source_returned_router();

    let mut interp = Command::new(gos_bin());
    interp.arg("run").arg(&src);
    run_and_check(&mut interp);

    let build = Command::new(gos_bin())
        .arg("build")
        .arg(&src)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let stem = src.file_stem().unwrap().to_str().unwrap();
    let bin = std::env::temp_dir().join("target").join("debug").join(stem);
    let mut compiled = Command::new(&bin);
    run_and_check(&mut compiled);
    let _ = std::fs::remove_file(&src);
}
