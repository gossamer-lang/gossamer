//! Direct latency probe - boots an h2c server in a goroutine,
//! issues a single curl request, asserts the round-trip is below
//! a sane bound. Gated on `GOS_H2_BENCH=1`.

use std::process::Command;
use std::time::{Duration, Instant};

use gossamer_std::http::{Headers, Request, Response, StatusCode};
use gossamer_std::http_h2 as h2;

#[test]
fn single_h2c_request_completes_quickly() {
    if std::env::var("GOS_H2_BENCH").is_err() {
        eprintln!("skipping: set GOS_H2_BENCH=1");
        return;
    }
    // The server takes the bound listener, so no other process can take the port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}");

    std::thread::spawn(move || {
        let _ = h2::run_h2c(
            listener,
            |_req: Request| -> Response {
                Response {
                    status: StatusCode(200),
                    headers: {
                        let mut h = Headers::new();
                        h.insert("content-type", "text/plain");
                        h
                    },
                    body: b"hi".to_vec(),
                    raw_header_pairs: Vec::new(),
                    body_stream: None,
                }
            },
            h2::Config::default(),
        );
    });

    // Warmup + 10 timed requests.
    for _ in 0..2 {
        let _ = Command::new("curl")
            .arg("--http2-prior-knowledge")
            .arg("--silent")
            .arg("--max-time")
            .arg("3")
            .arg(format!("http://{addr}/"))
            .output();
    }

    let iters = 10;
    let mut total = Duration::ZERO;
    for i in 0..iters {
        let t = Instant::now();
        let out = Command::new("curl")
            .arg("--http2-prior-knowledge")
            .arg("--silent")
            .arg("--max-time")
            .arg("3")
            .arg(format!("http://{addr}/"))
            .output()
            .unwrap();
        let elapsed = t.elapsed();
        eprintln!("iter {i}: {:?} status_ok={}", elapsed, out.status.success());
        total += elapsed;
    }
    let avg = total / iters;
    eprintln!("avg over {iters} reqs (fresh-connection): {avg:?}");

    // Multiplexed: 10 GETs on ONE connection. This is the case
    // h2 is designed to win - no fresh handshake per request.
    let t = Instant::now();
    let mut args: Vec<String> = vec![
        "--http2-prior-knowledge".into(),
        "--silent".into(),
        "--max-time".into(),
        "5".into(),
    ];
    for _ in 0..10 {
        args.push("-o".into());
        args.push("/dev/null".into());
        args.push(format!("http://{addr}/"));
    }
    let out = Command::new("curl").args(&args).output().unwrap();
    let multiplexed = t.elapsed();
    eprintln!(
        "10 GETs over one connection: {multiplexed:?} = {:?}/req, status={}",
        multiplexed / 10,
        out.status.success()
    );

    // Fresh-connection bound - curl overhead dominates, but
    // anything well above 50ms means the kernel-to-goroutine
    // wakeup path is taking too long.
    assert!(
        avg < Duration::from_millis(50),
        "h2c fresh-connection latency too high: {avg:?}"
    );
    // Multiplexed bound - this is where h2 wins. Should be sub-
    // 10ms per request when the connection is reused.
    assert!(
        multiplexed / 10 < Duration::from_millis(10),
        "h2c multiplexed latency too high: {:?}/req",
        multiplexed / 10
    );
}
