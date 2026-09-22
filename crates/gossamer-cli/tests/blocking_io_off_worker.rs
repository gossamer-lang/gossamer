//! A goroutine waiting on the OS must not hold its scheduler worker.
//!
//! Goroutines are pinned to the worker they start on, so a goroutine blocked
//! in a system call on its worker strands every goroutine placed behind it.
//! Each program here needs a second goroutine to run while a first waits on a
//! socket, and runs on one worker so no other worker can hide a stranding.

mod common;

use common::{TIERS, gos_run_on, stderr, stdout};

fn on_one_worker(src: &str) -> String {
    let mut answer: Option<String> = None;
    for tier in TIERS {
        let out = gos_run_on(tier, src, Some(1), &[]);
        assert!(out.status.success(), "{tier:?} failed: {}", stderr(&out));
        let text = stdout(&out);
        if let Some(first) = &answer {
            assert_eq!(&text, first, "{tier:?} answered differently");
        } else {
            answer = Some(text);
        }
    }
    answer.unwrap_or_default()
}

/// Echo connections are served while the accepting goroutine waits for the
/// next one.
const TCP_ECHO: &str = r#"use std::{errors, net}

fn echo(s: net::TcpStream) {
    if let Ok(b) = s.read(64) {
        let _ = s.write(b)
    }
}

fn serve(l: net::TcpListener, n: i64) {
    let _ = cohort {
        for _ in 0..n {
            match l.accept() {
                Ok(pair) => {
                    let s, _ = pair
                    spawn(|| echo(s))
                }
                Err(_) => break,
            }
        }
    }
}

fn main() -> Result<(), errors::Error> {
    let l = net::TcpListener::bind("127.0.0.1:0")?
    let addr = l.local_addr()?
    spawn(|| serve(l, 8))
    let mut echoed = 0
    for i in 0..8 {
        let c = net::TcpStream::connect(addr)?
        let _ = c.write(format("m{}", i).as_bytes())
        if c.read(64)?.len() == 2 {
            echoed += 1
        }
    }
    println("echoed {}", echoed)
    Ok(())
}"#;

#[test]
fn a_tcp_accept_loop_leaves_its_worker_to_the_connections_it_spawns() {
    assert_eq!(on_one_worker(TCP_ECHO), "echoed 8\n");
}

/// A server's connection goroutines run while its accept loop waits, a
/// keep-alive connection between requests is not in flight at shutdown, and
/// it does not hold the process at exit.
const HTTP_SERVER: &str = r#"use std::{errors, http}

fn answer(r: http::Request) -> Result<http::Response, errors::Error> {
    Ok(http::Response::text(200, r.path))
}

fn main() -> Result<(), errors::Error> {
    let s = http::Server::new()
    s.listen("127.0.0.1:0")?
    let addr = s.addr()
    spawn(|| run(s))
    for i in 0..3 {
        match http::get(format("http://{}/p{}", addr, i), Vec::from([])) {
            Ok(r) => println("{}", r.body)
            Err(e) => println("error: {}", e)
        }
    }
    println("drained {}", s.shutdown(1000))
    Ok(())
}

fn run(s: http::Server) { let _ = s.serve(answer) }"#;

#[test]
fn an_http_server_serves_while_accepting_and_drains_idle_connections() {
    assert_eq!(on_one_worker(HTTP_SERVER), "/p0\n/p1\n/p2\ndrained true\n");
}

/// A listener stays usable from another goroutine while one is parked in
/// its `accept`.
const LISTENER_SHARED: &str = r#"use std::{errors, net, time}

fn main() -> Result<(), errors::Error> {
    let l = net::TcpListener::bind("127.0.0.1:0")?
    let addr = l.local_addr()?
    spawn(|| { let _ = l.accept() })
    time::sleep(100)
    println("addr again: {}", l.local_addr()? == addr)
    let _c = net::TcpStream::connect(addr)?
    println("done")
    Ok(())
}"#;

#[test]
fn a_listener_answers_local_addr_while_another_goroutine_accepts_on_it() {
    assert_eq!(on_one_worker(LISTENER_SHARED), "addr again: true\ndone\n");
}
