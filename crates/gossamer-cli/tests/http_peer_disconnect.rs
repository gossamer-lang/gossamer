//! A compiled server cancels a request's context when its client hangs up
//! mid-request, so the handler stops instead of running to completion.

#![cfg(any(unix, windows))]
#![allow(missing_docs)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const PROGRAM: &str = r#"
use std::{errors, http, time}
use std::sync::channel

fn main() -> Result<(), errors::Error> {
    let tx, rx = channel()
    let s = http::Server::new()
    s.listen("127.0.0.1:0")?
    println(s.addr())
    spawn(|| {
        let _ = s.serve(|r: http::Request| {
            let ctx = r.context
            let started = time::Instant::now()
            while started.elapsed_ms() < 5000 {
                if ctx.is_cancelled() {
                    tx.send("cancelled")
                    return Ok(http::Response::text(200, "stopped"))
                }
                time::sleep(10)
            }
            tx.send("ran to completion")
            Ok(http::Response::text(200, "done"))
        })
    })
    match rx.recv() {
        Some(outcome) => println(outcome)
        None => println("closed")
    }
    Ok(())
}
"#;

fn build() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-http-peer-disconnect-{}", std::process::id()));
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

#[test]
fn a_client_that_hangs_up_cancels_its_request() {
    let binary = build();
    let mut child = Command::new(&binary)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the server");
    let mut stdout = child.stdout.take().expect("the server's stdout is piped");
    let mut addr = Vec::new();
    let mut byte = [0u8; 1];
    while stdout.read(&mut byte).expect("read the bound address") == 1 && byte[0] != b'\n' {
        addr.push(byte[0]);
    }
    let addr = String::from_utf8(addr).expect("the address is text");
    let mut conn = TcpStream::connect(&addr).expect("connect to the server");
    conn.write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .expect("send a request");
    drop(conn);
    let status = child.wait().expect("wait for the server");
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).expect("read the outcome");
    assert!(status.success(), "{status:?}");
    assert_eq!(rest, "cancelled\n");
}
