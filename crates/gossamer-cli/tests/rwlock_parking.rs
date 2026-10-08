#![allow(missing_docs)]

//! A goroutine that waits for a `sync::RwLock` parks instead of holding its
//! worker, so a `with_write` callback that blocks while holding the lock
//! still lets the goroutine it is waiting for run, on one worker or many:
//! the reader asks for the lock only once the writer holds it, and the
//! sender the writer waits for is queued behind the reader.

mod common;

use common::{TIERS, gos_run_on, stderr, stdout};

const PROGRAM: &str = r#"
use std::{errors, sync}
use std::sync::channel

fn run() -> Result<(), errors::Error> {
    let lock = sync::RwLock::new(1)
    let tx, rx = channel()
    let held_tx, held_rx = channel()
    let mut total = 0
    cohort {
        let writer = spawn(|| sync::RwLock::with_write(lock, |v| {
            held_tx.send(0)
            v + rx.recv().unwrap()
        }))
        let reader = spawn(|| {
            held_rx.recv()
            lock.read()
        })
        let sender = spawn(|| tx.send(5))
        sender.join().unwrap()
        total = writer.join().unwrap() + reader.join().unwrap()
    }?
    println(f"{total} {lock.read()}")
    Ok(())
}

fn main() {
    run().unwrap()
}
"#;

#[test]
fn a_blocked_write_callback_does_not_stall_its_worker() {
    for workers in [1, 4] {
        for tier in TIERS {
            let out = gos_run_on(tier, PROGRAM, Some(workers), &[]);
            assert!(
                out.status.success(),
                "{tier:?} on {workers}:\n{}",
                stderr(&out)
            );
            assert_eq!(stdout(&out), "12 6\n", "{tier:?} on {workers} worker(s)");
        }
    }
}
