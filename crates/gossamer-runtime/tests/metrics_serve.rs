//! Compiled-tier `metrics::serve_metrics` end-to-end.
//!
//! Drives the exact C-ABI shims a `gos build` binary runs:
//! `gos_rt_metrics_serve` mounts a registry on `/metrics` over the
//! runtime's own HTTP server (`gos_rt_http_serve`), rendering the
//! registered metrics with `render_registry`. The test builds a
//! registry through the same constructor shims, serves it on an
//! ephemeral loopback port from a background thread, and asserts a
//! real `GET /metrics` returns the Prometheus exposition while every
//! other path returns `404 not found` - the behaviour parity source
//! is `gossamer_std::metrics::serve_metrics`.

#![cfg(not(target_arch = "wasm32"))]
#![allow(missing_docs)]

use std::ffi::CString;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use gossamer_runtime::c_abi::metrics::{
    gos_rt_metrics_counter_inc, gos_rt_metrics_counter_new, gos_rt_metrics_registry_new,
    gos_rt_metrics_registry_register, gos_rt_metrics_serve,
};

/// Picks a free loopback port by binding `:0` and releasing it. The
/// serve loop rebinds the same address moments later; the window is
/// negligible on a serial loopback test.
fn free_loopback_port() -> u16 {
    let probe = TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = probe.local_addr().expect("local_addr").port();
    drop(probe);
    port
}

/// Issues one HTTP/1.1 request, returning `(status_line, body)`. Reads
/// the header block, parses `Content-Length`, then reads exactly that
/// many body bytes (the compiled server answers keep-alive, so a
/// read-to-EOF would block until the timeout).
fn http_get(addr: &str, path: &str) -> std::io::Result<(String, String)> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read until the header terminator is present.
    let header_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break buf.len();
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let status_line = head.lines().next().unwrap_or_default().to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    let mut body: Vec<u8> = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok((status_line, String::from_utf8_lossy(&body).into_owned()))
}

/// Connect-retries until the background serve loop has bound the port,
/// then performs the request. Polling for the listener to come up is
/// the correct startup synchronization with a server on another
/// thread; it carries no fixed sleep.
fn get_with_startup_retry(addr: &str, path: &str) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match http_get(addr, path) {
            Ok(pair) => return pair,
            Err(_) if Instant::now() < deadline => {
                thread::yield_now();
            }
            Err(e) => panic!("GET {path} never succeeded: {e}"),
        }
    }
}

#[test]
fn compiled_tier_serves_registry_on_metrics_path() {
    let port = free_loopback_port();
    let addr = format!("127.0.0.1:{port}");

    // Build a registry with one counter through the same C-ABI
    // constructor shims the compiled tier emits.
    let reg = gos_rt_metrics_registry_new();
    assert!(!reg.is_null(), "registry handle");
    let name = CString::new("http_requests_total").unwrap();
    let help = CString::new("total HTTP requests").unwrap();
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    let counter = unsafe { gos_rt_metrics_counter_new(name.as_ptr(), help.as_ptr()) };
    assert!(!counter.is_null(), "counter handle");
    for _ in 0..3 {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { gos_rt_metrics_counter_inc(counter) };
    }
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    unsafe { gos_rt_metrics_registry_register(reg, counter) };

    // Serve on a background thread; the registry pointer is leaked
    // (Box::into_raw) for the process lifetime, so the address is
    // valid across the move.
    let reg_addr = reg as usize;
    let serve_addr = CString::new(addr.clone()).unwrap();
    thread::spawn(move || {
        let reg = reg_addr as *mut _;
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { gos_rt_metrics_serve(serve_addr.as_ptr(), reg) };
    });

    let (status, body) = get_with_startup_retry(&addr, "/metrics");
    assert!(status.starts_with("HTTP/1.1 200"), "status: {status:?}");
    assert!(
        body.contains("# TYPE http_requests_total counter"),
        "missing TYPE line in exposition:\n{body}"
    );
    assert!(
        body.contains("http_requests_total 3"),
        "missing counter value in exposition:\n{body}"
    );

    // Any non-/metrics path is a 404, matching serve_metrics.
    let (status_404, body_404) = http_get(&addr, "/nope").expect("404 request");
    assert!(
        status_404.starts_with("HTTP/1.1 404"),
        "expected 404, got: {status_404:?}"
    );
    assert_eq!(body_404, "not found");
}
