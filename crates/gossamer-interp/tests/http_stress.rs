//! Multi-connection stress test for the native HTTP server.
//! Spins up the VM-hosted server, fires N sequential
//! connections from several client threads, and asserts the server
//! answered each request. The goal is not to saturate a real
//! production server - it is to catch regressions in the per-
//! connection worker path that the single-request end-to-end test
//! cannot surface.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use gossamer_hir::lower_source_file;
use gossamer_interp::Vm;
use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

const WORKERS: usize = 4;
const REQUESTS_PER_WORKER: u64 = 10;

fn run_server(source: &str) -> Result<(), String> {
    let mut map = SourceMap::new();
    let file = map.add_file("server.gos", source.to_string());
    let (mut sf, _) = parse_source_file(source, file);
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
        .map_err(|e| format!("{e}"))
}

#[test]
fn sequential_multi_connection_server_serves_every_request() {
    let total_requests = (WORKERS as u64) * REQUESTS_PER_WORKER;
    gossamer_interp::set_http_max_requests(total_requests);

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
                 http::Response::text(200, \"stress\")\n\
             }}\n\
         }}\n\
         fn main() {{\n\
             let app = App::new()\n\
             http::serve(\"{addr}\", app)\n\
         }}\n",
    );

    let (server_done_tx, server_done_rx) = mpsc::channel();
    let server_thread = thread::spawn(move || {
        let result = run_server(&source);
        let _ = server_done_tx.send(result);
    });

    // The server thread starts before its listener is bound. Poll the
    // socket instead of relying on a fixed sleep, which can expire before
    // a loaded CI runner schedules and initializes the interpreter.
    let ready_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => {
                drop(stream);
                break;
            }
            Err(_) if Instant::now() < ready_deadline => {
                match server_done_rx.try_recv() {
                    Ok(result) => panic!("server exited before accepting connections: {result:?}"),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        panic!("server completion channel disconnected")
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("server did not accept connections before deadline: {error}"),
        }
    }

    let request_deadline = Instant::now() + Duration::from_secs(20);
    let mut handles = Vec::new();
    for _ in 0..WORKERS {
        let client_addr = addr;
        handles.push(thread::spawn(move || {
            let mut successes = 0u64;
            while successes < REQUESTS_PER_WORKER && Instant::now() < request_deadline {
                let Ok(mut stream) = TcpStream::connect(client_addr) else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                if stream
                    .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .is_err()
                {
                    continue;
                }
                stream.shutdown(std::net::Shutdown::Write).ok();
                let mut reader = BufReader::new(stream);
                let mut status = String::new();
                if reader.read_line(&mut status).is_err() {
                    continue;
                }
                let mut rest = Vec::new();
                reader.read_to_end(&mut rest).ok();
                if status.starts_with("HTTP/1.1 200") {
                    successes += 1;
                }
            }
            successes
        }));
    }

    let total_ok: u64 = handles
        .into_iter()
        .map(|h| h.join().expect("client thread panicked"))
        .sum();

    let server_result = server_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server did not stop after the configured request count");
    server_result.expect("server failed");
    server_thread.join().expect("server thread panicked");

    assert_eq!(total_ok, total_requests, "not every request was served");
}
