//! `server::run_inline`: requests answered on the pool's threads, idle
//! keep-alive connections waiting in the poller, and known-length streamed
//! bodies framed by their length.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gossamer_std::http::server::{Config, run_inline};
use gossamer_std::http::{BodyStream, Headers, Request, Response, StatusCode};

/// Starts `run_inline` on a loopback port with `idle_timeout`, answering
/// every request with its path; `/stream` answers a streamed body of known
/// length. Returns the address and the flag that stops it.
fn start(idle_timeout: Duration) -> (SocketAddr, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let config = Config {
        idle_timeout: Some(idle_timeout),
        shutdown: Arc::clone(&shutdown),
        ..Config::default()
    };
    std::thread::spawn(move || {
        run_inline(listener, &config, |request: Request| {
            if request.path == "/stream" {
                let body = vec![b'x'; 200_000];
                let mut headers = Headers::new();
                headers.insert("content-length", &body.len().to_string());
                return Response {
                    status: StatusCode::OK,
                    headers,
                    body: Vec::new(),
                    raw_header_pairs: Vec::new(),
                    body_stream: Some(BodyStream(Box::new(std::io::Cursor::new(body)))),
                };
            }
            Response {
                status: StatusCode::OK,
                headers: Headers::new(),
                body: request.path.clone().into_bytes(),
                raw_header_pairs: Vec::new(),
                body_stream: None,
            }
        })
        .unwrap();
    });
    (addr, shutdown)
}

fn stop(addr: SocketAddr, shutdown: &AtomicBool) {
    shutdown.store(true, Ordering::Release);
    let _ = TcpStream::connect(addr);
}

/// Reads one response: its lowercased header lines and its body.
fn read_response(reader: &mut BufReader<TcpStream>) -> (Vec<String>, Vec<u8>) {
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    assert!(status.starts_with("HTTP/1.1 200"), "{status:?}");
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end().to_lowercase();
        if line.is_empty() {
            break;
        }
        headers.push(line);
    }
    let len: usize = headers
        .iter()
        .find_map(|h| h.strip_prefix("content-length: "))
        .expect("content-length")
        .parse()
        .unwrap();
    let mut body = vec![0; len];
    reader.read_exact(&mut body).unwrap();
    (headers, body)
}

#[test]
fn a_kept_alive_connection_is_answered_from_the_poller() {
    let (addr, shutdown) = start(Duration::from_secs(5));
    let stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    for path in ["/first", "/second"] {
        reader
            .get_mut()
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes())
            .unwrap();
        let (_, body) = read_response(&mut reader);
        assert_eq!(body, path.as_bytes());
    }
    stop(addr, &shutdown);
}

#[test]
fn pipelined_requests_are_answered_in_order() {
    let (addr, shutdown) = start(Duration::from_secs(5));
    let stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(b"GET /a HTTP/1.1\r\nHost: t\r\n\r\nGET /b HTTP/1.1\r\nHost: t\r\n\r\n")
        .unwrap();
    assert_eq!(read_response(&mut reader).1, b"/a");
    assert_eq!(read_response(&mut reader).1, b"/b");
    stop(addr, &shutdown);
}

#[test]
fn an_idle_connection_closes_at_the_idle_timeout() {
    let (addr, shutdown) = start(Duration::from_millis(200));
    let stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(b"GET /once HTTP/1.1\r\nHost: t\r\n\r\n")
        .unwrap();
    assert_eq!(read_response(&mut reader).1, b"/once");
    let mut rest = Vec::new();
    let read = reader.read_to_end(&mut rest).unwrap();
    assert_eq!(read, 0, "the server closes an idle connection");
    stop(addr, &shutdown);
}

#[test]
fn a_streamed_body_of_known_length_is_framed_by_it() {
    let (addr, shutdown) = start(Duration::from_secs(5));
    let stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(b"GET /stream HTTP/1.1\r\nHost: t\r\n\r\n")
        .unwrap();
    let (headers, body) = read_response(&mut reader);
    assert!(
        headers.iter().all(|h| !h.starts_with("transfer-encoding")),
        "{headers:?}"
    );
    assert_eq!(body.len(), 200_000);
    assert!(body.iter().all(|&b| b == b'x'));
    stop(addr, &shutdown);
}
