//! A wake that reaches every channel runs concurrently with waiters that
//! read the channel registry from under their own channel's lock, so the
//! two orders have to agree on which lock is outer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gossamer_interp::Value;
use gossamer_interp::value::{Channel, RecvOutcome, wake_all_channel_waiters};

const CHANNELS: usize = 16;
const MESSAGES: i64 = 200;

#[test]
fn waking_every_waiter_runs_beside_a_waiter_reading_the_registry() {
    // Past `main` a waiter is no longer counted among the participants,
    // which is the condition under which it recomputes readiness across
    // every live channel - the walk this wake has to interleave with.
    gossamer_interp::join_outstanding_goroutines();

    let channels: Vec<Channel> = (0..CHANNELS).map(|_| Channel::unbounded()).collect();
    let stop = Arc::new(AtomicBool::new(false));

    let receivers: Vec<_> = channels
        .iter()
        .cloned()
        .map(|ch| {
            std::thread::spawn(move || {
                let mut taken = 0;
                while matches!(ch.recv(), RecvOutcome::Value(_)) {
                    taken += 1;
                }
                taken
            })
        })
        .collect();

    let senders: Vec<_> = channels
        .iter()
        .cloned()
        .map(|ch| {
            std::thread::spawn(move || {
                for i in 0..MESSAGES {
                    let _ = ch.send(Value::Int(i));
                }
            })
        })
        .collect();

    let waker = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                wake_all_channel_waiters();
            }
        })
    };

    for sender in senders {
        sender.join().expect("sender thread");
    }
    for ch in &channels {
        let _ = ch.close();
    }
    let delivered: i64 = receivers
        .into_iter()
        .map(|receiver| receiver.join().expect("receiver thread"))
        .sum();
    stop.store(true, Ordering::Release);
    waker.join().expect("waker thread");

    assert_eq!(delivered, MESSAGES * CHANNELS as i64);
}
