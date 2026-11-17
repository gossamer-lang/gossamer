//! A compiled HTTP server shuts down gracefully on SIGTERM and SIGINT: the
//! signal relay stops the accept loop, `serve` answers `Ok(())`, and the
//! program carries on to exit normally instead of taking the signal's
//! default action.

#![cfg(unix)]
#![allow(missing_docs)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;

const PROGRAM: &str = r#"
use std::{errors, http}

fn hello(_r: http::Request) -> Result<http::Response, errors::Error> {
    Ok(http::Response::text(200, "hi"))
}

fn main() -> Result<(), errors::Error> {
    let s = http::Server::new()
    s.listen("127.0.0.1:0")?
    println(s.addr())
    let served = s.serve(hello)
    println(f"serve returned: {served}")
    Ok(())
}
"#;

fn build(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gos-http-signal-shutdown-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let src = dir.join("server.gos");
    std::fs::write(&src, PROGRAM).expect("write the program");
    let out = Command::new(env!("CARGO_BIN_EXE_gos"))
        .args(["build", "--release", "--out-dir"])
        .arg(&dir)
        .arg(&src)
        .output()
        .expect("run gos build");
    assert!(
        out.status.success(),
        "gos build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    common::native_executable(&dir, "server")
}

/// Starts the server, proves its accept loop runs with one request, sends
/// `signal`, and answers the exit status and the rest of its output.
fn serve_then_signal(binary: &std::path::Path, signal: i32) -> (std::process::ExitStatus, String) {
    let mut child = Command::new(binary)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the server");
    let mut stdout = child.stdout.take().expect("the server's stdout is piped");
    let mut first = Vec::new();
    let mut byte = [0u8; 1];
    while stdout.read(&mut byte).expect("read the bound address") == 1 && byte[0] != b'\n' {
        first.push(byte[0]);
    }
    let addr = String::from_utf8(first).expect("the address is text");
    // The listener is bound, so this connection queues until `serve` accepts
    // it; a response therefore proves the accept loop, and with it the
    // shutdown handler, is in place before the signal is sent.
    let mut conn = TcpStream::connect(&addr).expect("connect to the server");
    conn.write_all(b"GET / HTTP/1.0\r\nHost: test\r\n\r\n")
        .expect("send a request");
    let mut response = String::new();
    conn.read_to_string(&mut response)
        .expect("read the response");
    assert!(response.ends_with("hi"), "unexpected response: {response}");
    let sent = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(sent.success(), "send the signal");
    let status = child.wait().expect("wait for the server");
    let mut rest = String::new();
    stdout
        .read_to_string(&mut rest)
        .expect("read the rest of the output");
    (status, rest)
}

#[test]
fn sigterm_and_sigint_stop_a_compiled_server_gracefully() {
    let binary = build("release");
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let (status, rest) = serve_then_signal(&binary, signal);
        assert!(status.success(), "signal {signal}: {status:?}");
        assert_eq!(rest, "serve returned: Ok(())\n", "signal {signal}");
    }
}
