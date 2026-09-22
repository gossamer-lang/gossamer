//! How long a served connection lives, and what the answer says about it.
//!
//! HTTP/1.1 keeps a connection open unless the request says otherwise;
//! HTTP/1.0 ends it unless the request asks to keep it. A client that wrote
//! `Connection: close` and is told `keep-alive` waits for a close that never
//! comes, so the two tiers are asked the same questions here rather than
//! trusted to have made the same choice.

#![allow(missing_docs)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn gos_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push("gos");
    p
}

fn write_source() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let path = std::env::temp_dir().join(format!(
        "gos-conn-lifetime-{}-{nanos}.gos",
        std::process::id()
    ));
    std::fs::write(
        &path,
        r#"
use std::http

fn health(_r: http::Request) -> Result<http::Response, http::Error> {
    Ok(http::Response::text(200, "ok"))
}

fn main() {
    let s = http::Server::new()
    match s.listen("127.0.0.1:0") {
        Ok(_) => println("{}", s.addr())
        Err(e) => eprintln("listen: {}", e)
    }
    let _ = s.serve(health)
}
"#,
    )
    .unwrap();
    path
}

/// Ends the server whatever exit the test takes.
struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(command: &mut Command) -> (Server, SocketAddr) {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the server");
    let mut announced = String::new();
    let bound = BufReader::new(child.stdout.take().expect("the child's stdout"))
        .read_line(&mut announced)
        .expect("read the address the server bound");
    assert!(bound > 0, "the server ended before it bound");
    let addr = announced.trim().parse().expect("the announced address");
    (Server { child }, addr)
}

/// The `connection` header of one answer, and whether the server then
/// closed. A read that times out means the connection stayed open.
fn answer(addr: SocketAddr, request: &str) -> (String, bool) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_millis(750)))
        .expect("read timeout");
    stream.write_all(request.as_bytes()).expect("write");
    let mut answer = Vec::new();
    let closed = stream.read_to_end(&mut answer).is_ok();
    let text = String::from_utf8_lossy(&answer).into_owned();
    let header = text
        .lines()
        .find(|line| {
            line.split_once(':')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("connection"))
        })
        .unwrap_or("<none>")
        .trim()
        .to_string();
    (header, closed)
}

/// Every question one tier is asked, with the answer it owes.
fn check(addr: SocketAddr, tier: &str) {
    let (header, closed) = answer(
        addr,
        "GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(header, "connection: close", "{tier}: asked to close");
    assert!(closed, "{tier}: asked to close and did not");

    let (header, closed) = answer(addr, "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(
        header, "<none>",
        "{tier}: HTTP/1.1 persists without saying so"
    );
    assert!(!closed, "{tier}: closed an HTTP/1.1 connection unasked");

    let (header, closed) = answer(addr, "GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n");
    assert_eq!(header, "connection: close", "{tier}: HTTP/1.0 default");
    assert!(closed, "{tier}: left an HTTP/1.0 connection open");

    let (header, closed) = answer(
        addr,
        "GET /health HTTP/1.0\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n",
    );
    assert_eq!(
        header, "connection: keep-alive",
        "{tier}: HTTP/1.0 asked to persist"
    );
    assert!(
        !closed,
        "{tier}: closed an HTTP/1.0 connection asked to persist"
    );

    let (header, closed) = answer(
        addr,
        "GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive, close\r\n\r\n",
    );
    assert_eq!(
        header, "connection: close",
        "{tier}: close among the tokens"
    );
    assert!(closed, "{tier}: a close token left the connection open");
}

#[test]
fn both_tiers_end_a_connection_when_the_request_says_so() {
    let source = write_source();

    let mut interp = Command::new(gos_bin());
    interp.arg("run").arg(&source);
    let (server, addr) = start(&mut interp);
    check(addr, "interp");
    drop(server);

    let build = Command::new(gos_bin())
        .arg("build")
        .arg(&source)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let stem = source.file_stem().expect("a stem").to_string_lossy();
    let binary = std::env::temp_dir()
        .join("target")
        .join("debug")
        .join(stem.as_ref());
    let (server, addr) = start(&mut Command::new(&binary));
    check(addr, "compiled");
    drop(server);

    let _ = std::fs::remove_file(&source);
}
