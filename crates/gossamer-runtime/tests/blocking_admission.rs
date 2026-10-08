//! Goroutines submitting blocking operations faster than the pool's threads
//! finish them wait for room in the queue, parked rather than holding a
//! scheduler worker, and every operation still runs once the threads free.

use std::sync::{Arc, Mutex};

use gossamer_runtime::blocking_pool::{set_blocking_queue_cap, set_blocking_thread_cap, stats};
use gossamer_runtime::sched_global;

const SUBMITTERS: usize = 32;

#[test]
fn submitters_past_the_queue_cap_park_until_a_thread_takes_a_job() {
    // SAFETY: set before this test binary's first scheduler call.
    unsafe { std::env::set_var("GOSSAMER_MAX_PROCS", "2") };
    set_blocking_thread_cap(2);
    set_blocking_queue_cap(2);
    // Every operation waits on the gate, so the two threads stay held and
    // the queue stays full until the test opens it.
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    for i in 0..SUBMITTERS {
        let gate = Arc::clone(&gate);
        let done_tx = done_tx.clone();
        sched_global::spawn(Box::new(move || {
            let answer = sched_global::run_blocking("gate", move || {
                let (open, cv) = &*gate;
                let mut open = open.lock().expect("gate lock");
                while !*open {
                    open = cv.wait(open).expect("gate wait");
                }
                i
            });
            let _ = done_tx.send(answer);
        }));
    }
    let held_back = SUBMITTERS - 4;
    while stats().admission_waiters < held_back {
        std::thread::yield_now();
    }
    let load = stats();
    assert_eq!(
        load.queued, 2,
        "the queue holds no more than its cap: {load:?}"
    );
    assert!(
        load.threads <= 2,
        "the pool holds no more than its cap: {load:?}"
    );
    {
        let (open, cv) = &*gate;
        *open.lock().expect("gate lock") = true;
        cv.notify_all();
    }
    let mut answers: Vec<usize> = (0..SUBMITTERS)
        .map(|_| {
            done_rx
                .recv()
                .expect("goroutine reports")
                .expect("operation ran")
        })
        .collect();
    answers.sort_unstable();
    assert_eq!(answers, (0..SUBMITTERS).collect::<Vec<_>>());
    assert_eq!(stats().admission_waiters, 0);
}
