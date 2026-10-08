#![allow(missing_docs)]

//! `main` waiting on a receive or a wait group under a context, and in a
//! `select` whose ready arm is not the first, wakes when the context is
//! cancelled or the arm's channel fills, and is never read as a deadlock
//! while the cancellation that releases it is under way.

mod common;

use common::{TIERS, gos_run_on, stderr, stdout};

const PROGRAM: &str = r#"
use std::{context, sync, time}
use std::sync::channel

fn main() {
    let ctx = context::Context::with_cancel(context::Context::background())
    let tx, rx = channel::<i64>()
    spawn(|| {
        time::sleep(time::Duration::from_millis(20))
        ctx.cancel()
    })
    println(f"{rx.recv_ctx(ctx)}")
    let group_ctx = context::Context::with_cancel(context::Context::background())
    let wg = sync::WaitGroup::new()
    wg.add(1)
    spawn(|| {
        time::sleep(time::Duration::from_millis(20))
        group_ctx.cancel()
    })
    println(f"{wg.wait_ctx(group_ctx)}")
    let a_tx, a_rx = channel::<i64>()
    let b_tx, b_rx = channel::<i64>()
    spawn(|| {
        time::sleep(time::Duration::from_millis(20))
        b_tx.send(7)
    })
    select {
        v = a_rx.recv() => println(f"a {v}"),
        v = b_rx.recv() => println(f"b {v}"),
    }
    tx.close()
    wg.done()
    a_tx.close()
}
"#;

/// Whether the cancelling goroutine finishes before or after `main` wakes
/// varies between runs, so each tier runs the program several times.
#[test]
fn context_waits_on_main_wake_on_cancel_and_on_any_arm() {
    for tier in TIERS {
        for _ in 0..5 {
            let out = gos_run_on(tier, PROGRAM, None, &[]);
            assert!(out.status.success(), "{tier:?}:\n{}", stderr(&out));
            assert_eq!(stdout(&out), "None\nfalse\nb 7\n", "{tier:?}");
        }
    }
}
