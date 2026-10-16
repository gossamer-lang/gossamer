//! Stream A.2 - end-to-end test that a Gossamer handler is actually
//! dispatched when a real HTTP request lands on `http::serve`.
//! The test drives the VM on a small source program. It
//! picks a free port, launches the server in a background thread via
//! `GOSSAMER_HTTP_MAX_REQUESTS=1`, fires a real HTTP GET, and asserts
//! that the handler closure was invoked by inspecting the response.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use gossamer_hir::lower_source_file;
use gossamer_interp::Vm;
use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

fn run_interp(source: &str) -> Result<(), String> {
    let mut map = SourceMap::new();
    let file = map.add_file("server.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    if !parse_diags.is_empty() {
        return Err(format!("parse: {parse_diags:?}"));
    }
    let (mut resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut interp = Vm::new();
    interp.load(&program, tcx, true).expect("vm load");
    interp
        .call("main", Vec::new())
        .map(|_| ())
        .map_err(|e| format!("runtime: {e}"))
}

/// Serializes the networking tests below. They share two process-global
/// resources: `set_http_max_requests` (a single override every server in
/// the process reads) and the OS ephemeral-port handoff - each test
/// probes a free port with `bind("127.0.0.1:0")`, drops it, then has the
/// interpreter rebind that same address. Run concurrently, one test's
/// server can claim the port another just probed, so a client reaches the
/// wrong server (or none, once the thief consumes the single permitted
/// request). Held for each test's duration, this lock keeps the
/// probe-to-teardown window exclusive within the process.
static NET_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn net_guard() -> std::sync::MutexGuard<'static, ()> {
    NET_TEST_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn native_http_serve_dispatches_user_handler() {
    let _net = net_guard();
    // The interpreter's `http::serve` exits after
    // `gossamer_interp::set_http_max_requests(n)`; without it the
    // server loops forever and the test's `server_thread.join()`
    // deadlocks.
    gossamer_interp::set_http_max_requests(1);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let source = format!(
        "struct App {{ }}\n\
         impl App {{\n\
             fn new() -> App {{ App {{ }} }}\n\
         }}\n\
         impl http::Handler for App {{\n\
             fn serve(&self, request: http::Request) -> http::Response {{\n\
                 http::Response::text(200, \"alive\")\n\
             }}\n\
         }}\n\
         fn main() {{\n\
             let app = App::new()\n\
             http::serve(\"{addr}\", app)\n\
         }}\n",
    );

    let ready = Arc::new(AtomicBool::new(false));
    let ready_clone = Arc::clone(&ready);
    let server_thread = thread::spawn(move || {
        ready_clone.store(true, Ordering::Relaxed);
        run_interp(&source)
    });

    while !ready.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(5));
    }
    // Poll for the listener to actually bind rather than guessing with a
    // fixed sleep: the `ready` flag only marks "about to serve", so retry
    // connect until it succeeds or a generous deadline elapses (robust
    // under CPU contention, where a fixed-count retry could exhaust).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut stream = loop {
        match TcpStream::connect(addr) {
            Ok(s) => break s,
            Err(_) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("connect to interpreter-hosted server: {e}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    assert!(
        status_line.starts_with("HTTP/1.1 "),
        "unexpected status: {status_line:?}"
    );
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();

    let result = server_thread.join().expect("server thread panicked");
    assert!(result.is_ok(), "interpreter reported: {result:?}");
}

/// Serves exactly one request: reads the full request (headers +
/// Content-Length body), echoes the body back as `text/plain`, and
/// returns the raw request bytes for assertions.
fn spawn_echo_server() -> (std::net::SocketAddr, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1024];
        let body_start = loop {
            let n = stream.read(&mut buf).unwrap();
            request.extend_from_slice(&buf[..n]);
            if let Some(split) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                break split + 4;
            }
        };
        let content_length = String::from_utf8_lossy(&request[..body_start])
            .lines()
            .find_map(|l| {
                let (name, value) = l.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        while request.len() < body_start + content_length {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
        }
        let body = &request[body_start..];
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        request
    });
    (addr, handle)
}

/// Serves exactly one GET with a canned response carrying a custom
/// header, so the test can prove `Request::send` surfaces response
/// headers (the deleted legacy TCP fast path never populated them).
fn spawn_custom_header_server() -> (std::net::SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let mut request: Vec<u8> = Vec::new();
        loop {
            let n = stream.read(&mut buf).unwrap();
            request.extend_from_slice(&buf[..n]);
            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Custom: hello\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            )
            .unwrap();
    });
    (addr, handle)
}

/// Serves exactly one GET with a canned 200 response carrying the
/// given body, written in a single `write_all` so the client sees
/// the whole body on its first fill.
fn spawn_canned_body_server(body: &'static [u8]) -> (std::net::SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let mut request: Vec<u8> = Vec::new();
        loop {
            let n = stream.read(&mut buf).unwrap();
            request.extend_from_slice(&buf[..n]);
            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        stream.write_all(&response).unwrap();
    });
    (addr, handle)
}

#[test]
fn response_stream_next_chunk_drains_body_in_max_byte_chunks() {
    let _net = net_guard();
    let (addr, server) = spawn_canned_body_server(b"0123456789");

    // Byte sum of "0123456789" = 10 * 0x30 + (0+..+9) = 525.
    let source = format!(
        "fn main() {{\n\
             match http::stream(\"GET\", \"http://{addr}/chunks\", \"\", []) {{\n\
                 Ok(s) => {{\n\
                     let mut total = 0\n\
                     let mut chunks = 0\n\
                     let mut sum = 0\n\
                     while let Some(chunk) = s.next_chunk(4) {{\n\
                         total += chunk.len()\n\
                         chunks += 1\n\
                         for b in chunk {{ sum += b }}\n\
                     }}\n\
                     if total != 10 {{ panic(\"total: {{}}\", total) }}\n\
                     if chunks != 3 {{ panic(\"chunks: {{}}\", chunks) }}\n\
                     if sum != 525 {{ panic(\"sum: {{}}\", sum) }}\n\
                 }},\n\
                 Err(e) => panic(\"stream failed: {{}}\", e),\n\
             }}\n\
         }}\n",
    );
    let result = run_interp(&source);
    assert!(result.is_ok(), "interpreter reported: {result:?}");
    server.join().expect("canned body server thread panicked");
}

#[test]
fn response_stream_next_line_then_next_chunk_share_one_cursor() {
    let _net = net_guard();
    let (addr, server) = spawn_canned_body_server(b"alpha\nbeta!");

    let source = format!(
        "fn main() {{\n\
             match http::stream(\"GET\", \"http://{addr}/mixed\", \"\", []) {{\n\
                 Ok(s) => {{\n\
                     match s.next_line() {{\n\
                         Some(line) => {{\n\
                             if line != \"alpha\" {{ panic(\"line: {{}}\", line) }}\n\
                         }},\n\
                         None => panic(\"missing first line\"),\n\
                     }}\n\
                     let mut rest = 0\n\
                     while let Some(chunk) = s.next_chunk(4) {{\n\
                         rest += chunk.len()\n\
                     }}\n\
                     if rest != 5 {{ panic(\"rest: {{}}\", rest) }}\n\
                 }},\n\
                 Err(e) => panic(\"stream failed: {{}}\", e),\n\
             }}\n\
         }}\n",
    );
    let result = run_interp(&source);
    assert!(result.is_ok(), "interpreter reported: {result:?}");
    server.join().expect("canned body server thread panicked");
}

#[test]
fn client_get_send_returns_response_with_populated_headers() {
    let _net = net_guard();
    let (addr, server) = spawn_custom_header_server();

    let source = format!(
        "fn main() {{\n\
             let client = http::Client::new()\n\
             let result = client.get(\"http://{addr}/hdr\").send()\n\
             match result {{\n\
                 Ok(resp) => {{\n\
                     if resp.status != 200 {{ panic(\"bad status: {{}}\", resp.status) }}\n\
                     let mut found = false\n\
                     for (k, v) in resp.headers {{\n\
                         if k == \"x-custom\" {{\n\
                             if v == \"hello\" {{ found = true }}\n\
                         }}\n\
                     }}\n\
                     if !found {{ panic(\"x-custom missing from resp.headers\") }}\n\
                 }},\n\
                 Err(e) => panic(\"send failed: {{}}\", e),\n\
             }}\n\
         }}\n",
    );
    let result = run_interp(&source);
    assert!(result.is_ok(), "interpreter reported: {result:?}");
    server.join().expect("header server thread panicked");
}

#[test]
fn http_request_bytes_posts_binary_body_and_returns_ok_response() {
    let _net = net_guard();
    let (addr, server) = spawn_echo_server();

    let source = format!(
        "fn main() {{\n\
             let body = [104, 105]\n\
             let headers = [(\"x-test\", \"yes\")]\n\
             let result = http::request_bytes(\"POST\", \"http://{addr}/echo\", body, headers)\n\
             match result {{\n\
                 Ok(resp) => {{\n\
                     if resp.status != 200 {{ panic(\"bad status: {{}}\", resp.status) }}\n\
                     if resp.body != \"hi\" {{ panic(\"bad body: {{}}\", resp.body) }}\n\
                 }},\n\
                 Err(e) => panic(\"request_bytes failed: {{}}\", e),\n\
             }}\n\
         }}\n",
    );
    let result = run_interp(&source);
    assert!(result.is_ok(), "interpreter reported: {result:?}");

    let request = server.join().expect("echo server thread panicked");
    let request_text = String::from_utf8_lossy(&request).into_owned();
    assert!(
        request_text.starts_with("POST /echo HTTP/1.1"),
        "unexpected request line: {request_text}"
    );
    assert!(request_text.to_ascii_lowercase().contains("x-test: yes"));
    let body_start = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert_eq!(
        &request[body_start..],
        b"hi",
        "echoed upload must be the exact bytes [104, 105]"
    );
}

#[test]
fn server_request_raw_body_preserves_binary_post_bytes() {
    let _net = net_guard();
    gossamer_interp::set_http_max_requests(1);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    // The handler reports `raw_body`'s length and byte sum. The
    // payload [0x68, 0xFF, 0x00, 0x69] contains an invalid-UTF-8
    // byte (the lossy `body` field would inflate it to a 3-byte
    // replacement char) and a NUL (which truncates c-strings on the
    // compiled tier) - `raw_body` must survive both: len 4, sum 464.
    let source = format!(
        "struct App {{ }}\n\
         impl App {{\n\
             fn new() -> App {{ App {{ }} }}\n\
         }}\n\
         impl http::Handler for App {{\n\
             fn serve(&self, request: http::Request) -> http::Response {{\n\
                 let mut sum = 0\n\
                 for b in request.raw_body {{ sum += b }}\n\
                 http::Response::text(200, format(\"{{}} {{}}\", request.raw_body.len(), sum))\n\
             }}\n\
         }}\n\
         fn main() {{\n\
             let app = App::new()\n\
             http::serve(\"{addr}\", app)\n\
         }}\n",
    );

    let ready = Arc::new(AtomicBool::new(false));
    let ready_clone = Arc::clone(&ready);
    let server_thread = thread::spawn(move || {
        ready_clone.store(true, Ordering::Relaxed);
        run_interp(&source)
    });

    while !ready.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(5));
    }
    // Poll for the listener to actually bind rather than guessing with a
    // fixed sleep: the `ready` flag only marks "about to serve", so retry
    // connect until it succeeds or a generous deadline elapses (robust
    // under CPU contention, where a fixed-count retry could exhaust).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut stream = loop {
        match TcpStream::connect(addr) {
            Ok(s) => break s,
            Err(_) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("connect to interpreter-hosted server: {e}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request_bytes =
        b"POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\n".to_vec();
    request_bytes.extend_from_slice(&[0x68, 0xFF, 0x00, 0x69]);
    stream.write_all(&request_bytes).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    assert!(
        status_line.starts_with("HTTP/1.1 200"),
        "unexpected status: {status_line:?}"
    );
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    let rest_text = String::from_utf8_lossy(&rest).into_owned();
    let body = rest_text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or(&rest_text)
        .trim();
    assert_eq!(
        body, "4 464",
        "raw_body must see the exact 4 bytes; full response: {rest_text:?}"
    );

    let result = server_thread.join().expect("server thread panicked");
    assert!(result.is_ok(), "interpreter reported: {result:?}");
}

#[test]
fn handler_with_header_chain_reaches_the_wire_with_replace_semantics() {
    let _net = net_guard();
    gossamer_interp::set_http_max_requests(1);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let source = format!(
        "struct App {{ }}\n\
         impl http::Handler for App {{\n\
             fn serve(&self, _r: http::Request) -> Result<http::Response, http::Error> {{\n\
                 Ok(http::Response::text(201, \"created\")\n\
                     .with_header(\"x-a\", \"1\")\n\
                     .with_header(\"X-A\", \"2\")\n\
                     .with_header(\"x-b\", \"3\"))\n\
             }}\n\
         }}\n\
         fn main() {{\n\
             http::serve(\"{addr}\", App {{ }})\n\
         }}\n",
    );

    let ready = Arc::new(AtomicBool::new(false));
    let ready_clone = Arc::clone(&ready);
    let server_thread = thread::spawn(move || {
        ready_clone.store(true, Ordering::Relaxed);
        run_interp(&source)
    });
    while !ready.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(5));
    }
    // Poll for the listener to actually bind rather than guessing with a
    // fixed sleep: the `ready` flag only marks "about to serve", so retry
    // connect until it succeeds or a generous deadline elapses (robust
    // under CPU contention, where a fixed-count retry could exhaust).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut stream = loop {
        match TcpStream::connect(addr) {
            Ok(s) => break s,
            Err(_) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("connect to interpreter-hosted server: {e}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    assert!(
        status_line.starts_with("HTTP/1.1 201"),
        "unexpected status: {status_line:?}"
    );
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    let rest_text = String::from_utf8_lossy(&rest).to_ascii_lowercase();
    let header_section = rest_text.split("\r\n\r\n").next().unwrap_or(&rest_text);
    assert!(
        header_section.contains("x-a: 2"),
        "second same-name with_header must win: {header_section:?}"
    );
    assert!(
        !header_section.contains("x-a: 1"),
        "replaced header value must not also render: {header_section:?}"
    );
    assert!(
        header_section.contains("x-b: 3"),
        "distinct header must render: {header_section:?}"
    );
    assert!(
        header_section.contains("content-type: text/plain; charset=utf-8"),
        "constructor content type must survive custom headers: {header_section:?}"
    );

    let result = server_thread.join().expect("server thread panicked");
    assert!(result.is_ok(), "interpreter reported: {result:?}");
}

/// Connects to `addr`, GETs `/`, and returns (status line, header
/// section lowercased, raw body bytes after the blank line).
fn raw_get(addr: std::net::SocketAddr) -> (String, String, Vec<u8>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut stream = loop {
        match TcpStream::connect(addr) {
            Ok(s) => break s,
            Err(_) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("connect to interpreter-hosted server: {e}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    let split = rest
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header terminator");
    let headers = String::from_utf8_lossy(&rest[..split]).to_ascii_lowercase();
    (status_line, headers, rest[split + 4..].to_vec())
}

#[test]
fn proxy_handler_streams_upstream_body_as_chunked_passthrough() {
    let _net = net_guard();
    gossamer_interp::set_http_max_requests(1);
    let (upstream_addr, upstream) = spawn_canned_body_server(b"proxied payload bytes");

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let source = format!(
        "struct App {{ }}\n\
         impl http::Handler for App {{\n\
             fn serve(&self, _r: http::Request) -> Result<http::Response, http::Error> {{\n\
                 match http::stream(\"GET\", \"http://{upstream_addr}/data\", \"\", []) {{\n\
                     Ok(up) => Ok(http::Response::stream(up.status, up.content_type, up)),\n\
                     Err(e) => Ok(http::Response::text(502, \"bad upstream\")),\n\
                 }}\n\
             }}\n\
         }}\n\
         fn main() {{\n\
             http::serve(\"{addr}\", App {{ }})\n\
         }}\n",
    );

    let ready = Arc::new(AtomicBool::new(false));
    let ready_clone = Arc::clone(&ready);
    let server_thread = thread::spawn(move || {
        ready_clone.store(true, Ordering::Relaxed);
        run_interp(&source)
    });
    while !ready.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(5));
    }
    let (status_line, headers, body) = raw_get(addr);
    assert!(
        status_line.starts_with("HTTP/1.1 200"),
        "upstream status must pass through: {status_line:?}"
    );
    assert!(
        headers.contains("transfer-encoding: chunked"),
        "streamed response must be chunked: {headers:?}"
    );
    assert!(
        !headers.contains("content-length"),
        "chunked response must not carry Content-Length: {headers:?}"
    );
    assert!(
        headers.contains("content-type: application/octet-stream"),
        "upstream content type must pass through: {headers:?}"
    );
    let decoded = gossamer_std::http_chunked::decode_all(&body).expect("valid chunked body");
    assert_eq!(decoded, b"proxied payload bytes");

    let result = server_thread.join().expect("server thread panicked");
    assert!(result.is_ok(), "interpreter reported: {result:?}");
    upstream.join().expect("upstream thread panicked");
}

#[test]
fn response_stream_construction_consumes_the_client_stream() {
    let _net = net_guard();
    gossamer_interp::set_http_max_requests(1);
    let (upstream_addr, upstream) = spawn_canned_body_server(b"consume-me");

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    // After `Response::stream(...)`, the ResponseStream handle is
    // consumed: `next_chunk` must yield None. If it still yields
    // data the handler answers 500 and the assertion below trips.
    let source = format!(
        "struct App {{ }}\n\
         impl http::Handler for App {{\n\
             fn serve(&self, _r: http::Request) -> Result<http::Response, http::Error> {{\n\
                 match http::stream(\"GET\", \"http://{upstream_addr}/data\", \"\", []) {{\n\
                     Ok(up) => {{\n\
                         let streamed = http::Response::stream(up.status, up.content_type, up)\n\
                         if let Some(_) = up.next_chunk(64) {{\n\
                             return Ok(http::Response::text(500, \"stream not consumed\"))\n\
                         }}\n\
                         Ok(streamed)\n\
                     }},\n\
                     Err(e) => Ok(http::Response::text(502, \"bad upstream\")),\n\
                 }}\n\
             }}\n\
         }}\n\
         fn main() {{\n\
             http::serve(\"{addr}\", App {{ }})\n\
         }}\n",
    );

    let ready = Arc::new(AtomicBool::new(false));
    let ready_clone = Arc::clone(&ready);
    let server_thread = thread::spawn(move || {
        ready_clone.store(true, Ordering::Relaxed);
        run_interp(&source)
    });
    while !ready.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(5));
    }
    let (status_line, headers, body) = raw_get(addr);
    assert!(
        status_line.starts_with("HTTP/1.1 200"),
        "next_chunk after Response::stream must be None (handler would \
         answer 500 otherwise): {status_line:?}"
    );
    assert!(
        headers.contains("transfer-encoding: chunked"),
        "{headers:?}"
    );
    let decoded = gossamer_std::http_chunked::decode_all(&body).expect("valid chunked body");
    assert_eq!(
        decoded, b"consume-me",
        "the response still owns the full stream"
    );

    let result = server_thread.join().expect("server thread panicked");
    assert!(result.is_ok(), "interpreter reported: {result:?}");
    upstream.join().expect("upstream thread panicked");
}
