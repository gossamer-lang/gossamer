//! The threads `sched_global::run_blocking` runs blocking operations on.
//!
//! A job waits in one queue. A thread is started only while every existing one
//! is busy and the pool is below its cap; at the cap a job waits for a thread
//! to come free rather than starting another, so the blocking-thread count is
//! bounded. A thread idle for the keep-alive period exits.
//!
//! The queue is bounded too, by the number of jobs waiting and by how long
//! the oldest has waited. A job holds whatever its operation carries - a
//! buffer to write, a path - so an unbounded queue retains memory in step
//! with how far submitters have run ahead of the OS. Past either limit a
//! submitter waits for a thread to take a job: a goroutine parks, an OS
//! thread blocks. A job never waits on admission for another job queued
//! behind it, because a blocking operation started on a blocking thread runs
//! there inline rather than being submitted.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::platform::Instant;

/// Default ceiling on blocking threads. Every job holds its thread for the
/// whole operation, so the cap bounds how many operations wait on the OS at
/// once; it is tokio's `max_blocking_threads` default, and far below the
/// process thread and memory-map limits an unbounded count runs into.
pub const DEFAULT_BLOCKING_THREAD_CAP: usize = 512;

/// Default time an idle blocking thread waits for work before it exits. Long
/// enough that a steady stream of operations reuses its threads, short enough
/// that a burst's threads do not linger; tokio's `thread_keep_alive` default.
pub const DEFAULT_BLOCKING_KEEP_ALIVE: Duration = Duration::from_secs(10);

/// Default ceiling on jobs waiting for a thread: two for every thread the
/// pool may start, so a full set of threads always has its next job queued
/// while the backlog, and what it retains, stays bounded.
pub const DEFAULT_BLOCKING_QUEUE_CAP: usize = 2 * DEFAULT_BLOCKING_THREAD_CAP;

/// Default age past which the oldest waiting job holds back new ones. A job
/// that has waited this long says every thread is held by the OS, and work
/// queued behind it would only wait longer.
pub const DEFAULT_BLOCKING_QUEUE_AGE_LIMIT: Duration = Duration::from_secs(1);

type Job = Box<dyn FnOnce() + Send + 'static>;

struct PoolState {
    /// Waiting jobs, each with the instant it was queued.
    queue: VecDeque<(Instant, Job)>,
    /// Threads alive, busy or idle.
    threads: usize,
    /// Threads waiting for a job.
    idle: usize,
    /// Goroutines waiting for room in the queue, longest-waiting first.
    parked_submitters: VecDeque<crate::sched::Gid>,
    /// OS threads waiting for room in the queue.
    blocked_submitters: usize,
}

/// A set of blocking threads and the queue that feeds them.
struct Pool {
    state: Mutex<PoolState>,
    work: Condvar,
    /// Signalled when a thread takes a job, for OS-thread submitters.
    room: Condvar,
    thread_cap: AtomicUsize,
    keep_alive_ms: AtomicU64,
    queue_cap: AtomicUsize,
    queue_age_limit_ms: AtomicU64,
}

/// A duration in whole milliseconds, saturating.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Pool {
    const fn new() -> Self {
        Self {
            state: Mutex::new(PoolState {
                queue: VecDeque::new(),
                threads: 0,
                idle: 0,
                parked_submitters: VecDeque::new(),
                blocked_submitters: 0,
            }),
            work: Condvar::new(),
            room: Condvar::new(),
            thread_cap: AtomicUsize::new(DEFAULT_BLOCKING_THREAD_CAP),
            keep_alive_ms: AtomicU64::new(DEFAULT_BLOCKING_KEEP_ALIVE.as_millis() as u64),
            queue_cap: AtomicUsize::new(DEFAULT_BLOCKING_QUEUE_CAP),
            queue_age_limit_ms: AtomicU64::new(DEFAULT_BLOCKING_QUEUE_AGE_LIMIT.as_millis() as u64),
        }
    }

    /// Whether the queue can take another job: below its depth cap, with no
    /// waiting job past the age limit.
    fn has_room(&self, state: &PoolState) -> bool {
        if state.queue.len() >= self.queue_cap.load(Ordering::Relaxed) {
            return false;
        }
        let limit = Duration::from_millis(self.queue_age_limit_ms.load(Ordering::Relaxed));
        state
            .queue
            .front()
            .is_none_or(|(queued, _)| queued.elapsed() < limit)
    }

    fn submit(&'static self, job: Job) -> Result<(), String> {
        let mut state = self.state.lock();
        // A thread taking a job is what makes room, so with no thread alive
        // the job is admitted: it is what starts one.
        while state.threads > 0 && !self.has_room(&state) {
            if gossamer_coro::in_goroutine() {
                let mut guard = Some(state);
                crate::sched_global::park(crate::sched::ParkReason::Other, |parker| {
                    if let Some(state) = guard.as_mut() {
                        state.parked_submitters.push_back(parker.gid);
                    }
                    drop(guard.take());
                });
                state = self.state.lock();
            } else {
                state.blocked_submitters += 1;
                self.room.wait(&mut state);
                state.blocked_submitters -= 1;
            }
        }
        state.queue.push_back((Instant::now(), job));
        if state.idle > 0 {
            self.work.notify_one();
        }
        // A woken thread counts as idle until it runs, so compare waiting
        // jobs with waiting threads: a job beyond them needs a thread of its
        // own, or it queues behind one that may wait on the OS indefinitely.
        if state.queue.len() <= state.idle
            || state.threads >= self.thread_cap.load(Ordering::Relaxed)
        {
            return Ok(());
        }
        state.threads += 1;
        drop(state);
        let spawned = std::thread::Builder::new()
            .name("gos-blocking".to_string())
            .spawn(move || self.worker());
        if let Err(e) = spawned {
            let mut state = self.state.lock();
            state.threads -= 1;
            if state.threads == 0 {
                // Nothing alive would ever take the job just queued.
                state.queue.pop_back();
                return Err(format!("spawn blocking worker: {e}"));
            }
        }
        Ok(())
    }

    fn worker(&self) {
        let mut state = self.state.lock();
        loop {
            if let Some((_, job)) = state.queue.pop_front() {
                self.admit_next(&mut state);
                drop(state);
                job();
                state = self.state.lock();
                continue;
            }
            state.idle += 1;
            let keep_alive = Duration::from_millis(self.keep_alive_ms.load(Ordering::Relaxed));
            let timed_out = self.work.wait_for(&mut state, keep_alive).timed_out();
            state.idle -= 1;
            if timed_out && state.queue.is_empty() {
                state.threads -= 1;
                return;
            }
        }
    }

    /// Lets the longest-waiting submitter retry, now that a job has left the
    /// queue. Each pop releases one, and one that finds the queue still at a
    /// limit waits again.
    fn admit_next(&self, state: &mut PoolState) {
        if let Some(gid) = state.parked_submitters.pop_front() {
            crate::sched_global::scheduler().unpark(gid);
        } else if state.blocked_submitters > 0 {
            self.room.notify_one();
        }
    }

    fn stats(&self) -> BlockingStats {
        let state = self.state.lock();
        BlockingStats {
            threads: state.threads,
            queued: state.queue.len(),
            admission_waiters: state.parked_submitters.len() + state.blocked_submitters,
            oldest_queued_ms: state
                .queue
                .front()
                .map_or(0, |(queued, _)| millis(queued.elapsed())),
        }
    }
}

static POOL: Pool = Pool::new();

/// Sets the most blocking threads alive at once; at least one.
pub fn set_blocking_thread_cap(cap: usize) {
    POOL.thread_cap.store(cap.max(1), Ordering::Relaxed);
}

/// Sets how long an idle blocking thread waits for work before exiting.
pub fn set_blocking_keep_alive(keep_alive: Duration) {
    POOL.keep_alive_ms
        .store(millis(keep_alive), Ordering::Relaxed);
}

/// Sets the most jobs that may wait for a thread; at least one.
pub fn set_blocking_queue_cap(cap: usize) {
    POOL.queue_cap.store(cap.max(1), Ordering::Relaxed);
}

/// Sets how long the oldest waiting job may wait before new jobs are held
/// back until a thread takes it.
pub fn set_blocking_queue_age_limit(limit: Duration) {
    POOL.queue_age_limit_ms
        .store(millis(limit), Ordering::Relaxed);
}

/// Blocking threads alive now, busy or idle.
#[must_use]
pub fn blocking_threads() -> usize {
    POOL.state.lock().threads
}

/// A snapshot of the pool's load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockingStats {
    /// Threads alive, busy or idle.
    pub threads: usize,
    /// Jobs waiting for a thread.
    pub queued: usize,
    /// Submitters waiting for room in the queue.
    pub admission_waiters: usize,
    /// How long the oldest waiting job has waited, in milliseconds.
    pub oldest_queued_ms: u64,
}

/// The pool's load now.
#[must_use]
pub fn stats() -> BlockingStats {
    POOL.stats()
}

/// Queues `job` to run on a blocking thread, first waiting for room when
/// the queue is at its limits. Errs only when a needed thread could not be
/// started and none is alive to take the job.
pub fn submit(job: Job) -> Result<(), String> {
    POOL.submit(job)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

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

    /// A pool of its own, so its limits leave the process pool alone.
    fn private_pool(threads: usize, queue: usize) -> &'static Pool {
        let pool: &'static Pool = Box::leak(Box::new(Pool::new()));
        pool.thread_cap.store(threads, Ordering::Relaxed);
        pool.queue_cap.store(queue, Ordering::Relaxed);
        pool
    }

    #[test]
    fn a_full_queue_holds_its_submitter_until_a_thread_takes_a_job() {
        let pool = private_pool(1, 1);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        // The one thread blocks until released, and one more job fills the
        // queue behind it.
        pool.submit(Box::new(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        }))
        .expect("first job");
        started_rx.recv().expect("first job started");
        pool.submit(Box::new(|| {})).expect("queued job");
        let admitted = Arc::new(AtomicBool::new(false));
        let submitter = {
            let admitted = Arc::clone(&admitted);
            std::thread::spawn(move || {
                pool.submit(Box::new(|| {})).expect("third job");
                admitted.store(true, Ordering::Release);
            })
        };
        while pool.stats().admission_waiters == 0 {
            std::thread::yield_now();
        }
        assert!(
            !admitted.load(Ordering::Acquire),
            "admitted past a full queue"
        );
        assert_eq!(pool.stats().queued, 1);
        release_tx.send(()).expect("release the thread");
        submitter.join().expect("submitter");
        assert!(admitted.load(Ordering::Acquire));
    }

    #[test]
    fn an_old_waiting_job_holds_back_new_ones() {
        let pool = private_pool(1, 64);
        pool.queue_age_limit_ms.store(0, Ordering::Relaxed);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        pool.submit(Box::new(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        }))
        .expect("first job");
        started_rx.recv().expect("first job started");
        pool.submit(Box::new(|| {})).expect("queued job");
        let submitter = std::thread::spawn(move || pool.submit(Box::new(|| {})));
        while pool.stats().admission_waiters == 0 {
            std::thread::yield_now();
        }
        assert_eq!(pool.stats().queued, 1, "held back below the depth cap");
        release_tx.send(()).expect("release the thread");
        submitter.join().expect("submitter").expect("third job");
    }
}
