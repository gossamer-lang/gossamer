//! Lightweight latency comparison: 100 sequential GET requests
//! via curl against an h1 server vs an h2c server, same handler
//! shape, same payload. Gated on `GOS_H2_BENCH=1`.
//!
//! This is a smoke benchmark - not a load test. The point is to
//! verify the h2 path is in the same order of magnitude as h1,
//! catching catastrophic regressions in either path.

use std::process::Command;
use std::time::{Duration, Instant};

use gossamer_std::http::server::Config as HConfig;
use gossamer_std::http::{self, Headers, Request, Response, StatusCode, server};
use gossamer_std::http_h2 as h2;

fn curl_has_http2() -> bool {
    Command::new("curl")
        .arg("--version")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("nghttp2"))
}

fn boot_h1() -> u16 {
    // The server takes the bound listener, so no other process can take the port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server::run(
            listener,
            &HConfig::default(),
            |_req: http::Request| -> http::Response {
                let mut headers = Headers::new();
                headers.insert("content-type", "text/plain");
                http::Response {
                    status: StatusCode(200),
                    headers,
                    body: b"hello h1".to_vec(),
                    raw_header_pairs: Vec::new(),
                    body_stream: None,
                }
            },
        );
    });
    port
}

fn boot_h2() -> u16 {
    // The server takes the bound listener, so no other process can take the port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = h2::run_h2c(
            listener,
            |_req: Request| -> Response {
                let mut headers = Headers::new();
                headers.insert("content-type", "text/plain");
                Response {
                    status: StatusCode(200),
                    headers,
                    body: b"hello h2".to_vec(),
                    raw_header_pairs: Vec::new(),
                    body_stream: None,
                }
            },
            h2::Config::default(),
        );
    });
    port
}

fn timed_curl(url: &str, http2: bool, iters: usize) -> Duration {
    let start = Instant::now();
    for _ in 0..iters {
        let mut cmd = Command::new("curl");
        cmd.arg("--silent").arg("--max-time").arg("5");
        if http2 {
            cmd.arg("--http2-prior-knowledge");
        }
        cmd.arg(url);
        let out = cmd.output().expect("curl");
        assert!(out.status.success(), "curl failed during bench: {out:?}");
    }
    start.elapsed()
}

#[test]
fn h1_vs_h2_latency_smoke() {
    if std::env::var("GOS_H2_BENCH").is_err() {
        eprintln!("skipping: set GOS_H2_BENCH=1 to run");
        return;
    }
    if !curl_has_http2() {
        eprintln!("skipping: curl missing HTTP/2 support");
        return;
    }
    let iters = 100;
    let h1_port = boot_h1();
    let h2_port = boot_h2();
    let h1 = timed_curl(&format!("http://127.0.0.1:{h1_port}/"), false, iters);
    let h2 = timed_curl(&format!("http://127.0.0.1:{h2_port}/"), true, iters);
    let h1_avg = h1.as_secs_f64() * 1000.0 / iters as f64;
    let h2_avg = h2.as_secs_f64() * 1000.0 / iters as f64;
    eprintln!("h1 fresh-connection: {h1:?} total ({h1_avg:.2} ms/req)");
    eprintln!("h2 fresh-connection: {h2:?} total ({h2_avg:.2} ms/req)");

    // Apples-to-apples comparison: bundle N GETs into one curl
    // invocation so the h2 multiplex / h1 keep-alive paths
    // amortise across all of them. This is the workload h2 is
    // designed for.
    let h2_port_mp = boot_h2();
    let h1_port_mp = boot_h1();
    let mp_n: usize = 50;

    let mut h1_args: Vec<String> = vec!["--silent".into(), "--max-time".into(), "10".into()];
    let mut h2_args: Vec<String> = vec![
        "--http2-prior-knowledge".into(),
        "--silent".into(),
        "--max-time".into(),
        "10".into(),
    ];
    for _ in 0..mp_n {
        h1_args.push("-o".into());
        h1_args.push("/dev/null".into());
        h1_args.push(format!("http://127.0.0.1:{h1_port_mp}/"));
        h2_args.push("-o".into());
        h2_args.push("/dev/null".into());
        h2_args.push(format!("http://127.0.0.1:{h2_port_mp}/"));
    }
    let t = Instant::now();
    let _ = Command::new("curl")
        .args(&h1_args)
        .output()
        .expect("curl h1");
    let h1_mp = t.elapsed();
    let t = Instant::now();
    let _ = Command::new("curl")
        .args(&h2_args)
        .output()
        .expect("curl h2");
    let h2_mp = t.elapsed();
    let h1_mp_avg = h1_mp.as_secs_f64() * 1000.0 / mp_n as f64;
    let h2_mp_avg = h2_mp.as_secs_f64() * 1000.0 / mp_n as f64;
    eprintln!(
        "h1 multiplexed (keep-alive over 1 conn, {mp_n} reqs): {h1_mp:?} ({h1_mp_avg:.2} ms/req)"
    );
    eprintln!(
        "h2 multiplexed (one h2 stream-mux conn, {mp_n} reqs): {h2_mp:?} ({h2_mp_avg:.2} ms/req)"
    );

    // h2 multiplexed must be in the same ballpark as h1
    // keep-alive - both reuse one TCP connection across many
    // requests, so curl startup is paid once. 3x bound catches
    // protocol-level regressions while tolerating jitter.
    assert!(
        h2_mp_avg < h1_mp_avg * 3.0,
        "h2 multiplexed slower than h1 keep-alive: h1={h1_mp_avg:.2}ms h2={h2_mp_avg:.2}ms"
    );
}
