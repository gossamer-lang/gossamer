//! The threads `sched_global::run_blocking` runs blocking operations on.
//!
//! A job waits in one queue. A thread is started only while every existing one
//! is busy and the pool is below its cap; at the cap a job waits for a thread
//! to come free rather than starting another, so the blocking-thread count is
//! bounded. A thread idle for the keep-alive period exits.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

/// Default ceiling on blocking threads. Every job holds its thread for the
/// whole operation, so the cap bounds how many operations wait on the OS at
/// once; it is tokio's `max_blocking_threads` default, and far below the
/// process thread and memory-map limits an unbounded count runs into.
pub const DEFAULT_BLOCKING_THREAD_CAP: usize = 512;

/// Default time an idle blocking thread waits for work before it exits. Long
/// enough that a steady stream of operations reuses its threads, short enough
/// that a burst's threads do not linger; tokio's `thread_keep_alive` default.
pub const DEFAULT_BLOCKING_KEEP_ALIVE: Duration = Duration::from_secs(10);

type Job = Box<dyn FnOnce() + Send + 'static>;

struct PoolState {
    queue: VecDeque<Job>,
    /// Threads alive, busy or idle.
    threads: usize,
    /// Threads waiting for a job.
    idle: usize,
}

struct Pool {
    state: Mutex<PoolState>,
    work: Condvar,
}

static POOL: Pool = Pool {
    state: Mutex::new(PoolState {
        queue: VecDeque::new(),
        threads: 0,
        idle: 0,
    }),
    work: Condvar::new(),
};

static CAP: AtomicUsize = AtomicUsize::new(DEFAULT_BLOCKING_THREAD_CAP);
static KEEP_ALIVE_MS: AtomicU64 = AtomicU64::new(10_000);

/// Sets the most blocking threads alive at once; at least one.
pub fn set_blocking_thread_cap(cap: usize) {
    CAP.store(cap.max(1), Ordering::Relaxed);
}

/// Sets how long an idle blocking thread waits for work before exiting.
pub fn set_blocking_keep_alive(keep_alive: Duration) {
    let ms = u64::try_from(keep_alive.as_millis()).unwrap_or(u64::MAX);
    KEEP_ALIVE_MS.store(ms, Ordering::Relaxed);
}

/// Blocking threads alive now, busy or idle.
#[must_use]
pub fn blocking_threads() -> usize {
    POOL.state.lock().threads
}

/// Queues `job` to run on a blocking thread. Errs only when a needed thread
/// could not be started and none is alive to take the job.
pub fn submit(job: Job) -> Result<(), String> {
    let mut state = POOL.state.lock();
    state.queue.push_back(job);
    if state.idle > 0 {
        POOL.work.notify_one();
        return Ok(());
    }
    if state.threads >= CAP.load(Ordering::Relaxed) {
        return Ok(());
    }
    state.threads += 1;
    drop(state);
    let spawned = std::thread::Builder::new()
        .name("gos-blocking".to_string())
        .spawn(worker);
    if let Err(e) = spawned {
        let mut state = POOL.state.lock();
        state.threads -= 1;
        if state.threads == 0 {
            // Nothing alive would ever take the job just queued.
            state.queue.pop_back();
            return Err(format!("spawn blocking worker: {e}"));
        }
    }
    Ok(())
}

fn worker() {
    let mut state = POOL.state.lock();
    loop {
        if let Some(job) = state.queue.pop_front() {
            drop(state);
            job();
            state = POOL.state.lock();
            continue;
        }
        state.idle += 1;
        let keep_alive = Duration::from_millis(KEEP_ALIVE_MS.load(Ordering::Relaxed));
        let timed_out = POOL.work.wait_for(&mut state, keep_alive).timed_out();
        state.idle -= 1;
        if timed_out && state.queue.is_empty() {
            state.threads -= 1;
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_jobs_all_run_and_threads_are_reused() {
        let (tx, rx) = std::sync::mpsc::channel();
        for i in 0..64 {
            let tx = tx.clone();
            submit(Box::new(move || {
                let _ = tx.send(i);
            }))
            .expect("submit");
        }
        let mut got: Vec<i32> = (0..64).map(|_| rx.recv().expect("job ran")).collect();
        got.sort_unstable();
        assert_eq!(got, (0..64).collect::<Vec<_>>());
        assert!(blocking_threads() <= DEFAULT_BLOCKING_THREAD_CAP);
    }
}
