//! Work-stealing M:N scheduler.
//!
//! Each worker thread owns a [`crossbeam_deque::Worker`] local deque
//! plus a [`Stealer`] handle published into the shared `MultiState`.
//! When a worker drains its local deque it first tries the global
//! [`Injector`], then steals from a peer chosen round-robin. Spawning
//! from outside the scheduler pushes onto the injector so any worker
//! can pick the new task up.
//!
//! The model follows Go's P/M split:
//!
//! - A `Worker<SendTask>` is the "P" - the run-queue half a worker
//!   thread owns exclusively.
//! - The OS thread driving a `Worker` is the "M".
//! - Goroutines (`SendTask`) are the "G".
//!
//! When a goroutine parks (e.g. blocked on I/O or a mutex), the M
//! removes it from the local deque, hands it to the side `parked` map
//! keyed by [`Gid`], and continues running other tasks. An external
//! agent (poller, mutex-release, channel-send) calls
//! [`MultiScheduler::unpark`] to resurrect the parked goroutine onto a
//! ready queue. Workers waiting on an empty deque park themselves on a
//! per-worker [`Condvar`] until either new work lands or another
//! worker shouts via the `wake_one` helper.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_deque::{Injector, Steal, Stealer, Worker as Deque};
use parking_lot::{Condvar, Mutex};

use super::task::{Gid, Step, Task};
use crate::platform::Instant;

/// Monotonic process-start anchor. Yield timestamps are encoded as
/// `Instant::now().duration_since(*PROCESS_START).as_micros() as u64`
/// so a single `AtomicU64::store` per Yield replaces the prior
/// `Mutex<Vec<Instant>>` write.
fn process_start() -> &'static Instant {
    static PROCESS_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    PROCESS_START.get_or_init(Instant::now)
}

#[inline]
fn now_micros_since_start() -> u64 {
    Instant::now()
        .duration_since(*process_start())
        .as_micros()
        .min(u128::from(u64::MAX)) as u64
}

/// Task stored in the multi-M scheduler. Requires `Send` so workers
/// on different threads can pull from a shared queue.
pub trait SchedTask: Task + Send {}
impl<T: Task + Send> SchedTask for T {}

/// Boxed schedulable task moved through the deques and injector.
pub type SendTask = Box<dyn SchedTask + Send>;

/// Reason a goroutine has been parked. Carried alongside the task in
/// the `parked` table so introspection / debugging tools can attribute
/// the wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkReason {
    /// Generic park - the runtime did not specify a more specific
    /// reason.
    Other,
    /// Waiting on a channel send / receive.
    Chan,
    /// Waiting on a mutex / rwlock / once / wait-group.
    Sync,
    /// Waiting on the netpoller for a socket to become readable /
    /// writable.
    Io,
    /// Waiting on a timer to expire.
    Timer,
}

/// Snapshot of goroutines currently parked by wait category.
///
/// These are instantaneous counts, not accumulated contention or wait-time
/// metrics. They are intended for diagnostics such as the pprof mutex and
/// block endpoints.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ParkedReasonCounts {
    /// Parks for which the runtime did not supply a more specific reason.
    pub other: usize,
    /// Goroutines waiting on a channel send or receive.
    pub chan: usize,
    /// Goroutines waiting on synchronization primitives.
    pub sync: usize,
    /// Goroutines waiting for socket readiness.
    pub io: usize,
    /// Goroutines waiting for a timer.
    pub timer: usize,
}

/// Accumulated time goroutines spent parked, grouped by wait category.
///
/// Values are monotonic microseconds since the scheduler was created. They
/// are deliberately cumulative, so callers can take two snapshots and
/// compute a delta without coordinating with parked goroutines.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ParkWaitStats {
    /// Time spent in generic runtime waits.
    pub other_micros: u64,
    /// Time spent waiting on channel operations.
    pub chan_micros: u64,
    /// Time spent waiting on synchronization primitives.
    pub sync_micros: u64,
    /// Time spent waiting for I/O readiness.
    pub io_micros: u64,
    /// Time spent waiting for timers.
    pub timer_micros: u64,
}

/// One scheduler event captured by [`MultiScheduler::start_execution_trace`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionTraceEvent {
    /// Monotonic timestamp in microseconds since scheduler process start.
    pub timestamp_micros: u64,
    /// Event name (`spawn`, `park`, or `unpark`).
    pub name: &'static str,
    /// Goroutine the event concerns.
    pub gid: u32,
    /// Wait category for a park event, otherwise `None`.
    pub reason: Option<ParkReason>,
}

/// Statistics produced by [`MultiScheduler`].
#[derive(Debug, Default, Clone, Copy)]
pub struct MultiStats {
    /// Total tasks spawned.
    pub spawned: u64,
    /// Total tasks completed.
    pub finished: u64,
    /// Total `Task::step` calls issued across all workers.
    pub steps: u64,
    /// Total [`Step::Yield`] observations across all workers.
    pub yields: u64,
    /// Total successful steals from peer workers.
    pub steals: u64,
    /// Total successful pulls from the global injector.
    pub injects: u64,
    /// Total goroutines parked at least once.
    pub parks: u64,
    /// Total `unpark` calls that successfully resurrected a parked
    /// goroutine.
    pub unparks: u64,
}

#[derive(Default, Debug)]
struct AtomicStats {
    spawned: AtomicU64,
    finished: AtomicU64,
    steps: AtomicU64,
    yields: AtomicU64,
    steals: AtomicU64,
    injects: AtomicU64,
    parks: AtomicU64,
    unparks: AtomicU64,
}

#[allow(
    clippy::struct_field_names,
    reason = "unit-bearing atomics mirror the public snapshot"
)]
#[derive(Default, Debug)]
struct AtomicParkWaitStats {
    other_micros: AtomicU64,
    chan_micros: AtomicU64,
    sync_micros: AtomicU64,
    io_micros: AtomicU64,
    timer_micros: AtomicU64,
}

impl AtomicParkWaitStats {
    fn add(&self, reason: ParkReason, elapsed: Duration) {
        let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        let target = match reason {
            ParkReason::Other => &self.other_micros,
            ParkReason::Chan => &self.chan_micros,
            ParkReason::Sync => &self.sync_micros,
            ParkReason::Io => &self.io_micros,
            ParkReason::Timer => &self.timer_micros,
        };
        target.fetch_add(micros, Ordering::Relaxed);
    }

    fn snapshot(&self) -> ParkWaitStats {
        ParkWaitStats {
            other_micros: self.other_micros.load(Ordering::Relaxed),
            chan_micros: self.chan_micros.load(Ordering::Relaxed),
            sync_micros: self.sync_micros.load(Ordering::Relaxed),
            io_micros: self.io_micros.load(Ordering::Relaxed),
            timer_micros: self.timer_micros.load(Ordering::Relaxed),
        }
    }
}

impl AtomicStats {
    fn snapshot(&self) -> MultiStats {
        MultiStats {
            spawned: self.spawned.load(Ordering::Relaxed),
            finished: self.finished.load(Ordering::Relaxed),
            steps: self.steps.load(Ordering::Relaxed),
            yields: self.yields.load(Ordering::Relaxed),
            steals: self.steals.load(Ordering::Relaxed),
            injects: self.injects.load(Ordering::Relaxed),
            parks: self.parks.load(Ordering::Relaxed),
            unparks: self.unparks.load(Ordering::Relaxed),
        }
    }
}

/// Per-worker shared handles published into [`Shared`] so peers can
/// steal from this worker and so the scheduler can wake it.
struct WorkerSlot {
    /// Steal half of this worker's deque. Used by other workers when
    /// their local deque is empty.
    stealer: Stealer<SendTask>,
    /// Per-worker incoming queue. `unpark(gid)` pushes the
    /// resurrected task onto the home worker's `inbox` so the
    /// goroutine resumes on the same OS thread it parked on.
    /// Required because stackful coroutines from `gossamer-coro`
    /// (corosensei) are not safe to migrate across OS threads
    /// while suspended.
    inbox: Injector<SendTask>,
    /// `true` while the OS thread for this worker is parked on the
    /// `cv` waiting for new work.
    parked: AtomicBool,
    /// Mutex/condvar pair - workers park here when their deque is
    /// empty; spawn / unpark calls notify this condvar.
    cv: Condvar,
    cv_mu: Mutex<()>,
    /// `true` when this slot has been retired (e.g. because
    /// `set_max_procs` shrank the worker count). Workers consult this
    /// before parking and exit.
    retired: AtomicBool,
    /// Opaque OS-thread handle (Unix: `pthread_t` cast to `u64`,
    /// other platforms: 0). Captured by `worker_loop` on entry; the
    /// watchdog uses it to send a targeted SIGURG via
    /// [`crate::preempt::signal_thread_sigurg`] when this
    /// worker's task overstays its budget.
    thread_handle: AtomicU64,
    /// Monotonic micros-since-process-start of this worker's most
    /// recent `Yield`/`Done` boundary. The watchdog reads it lock-
    /// free; the worker writes it lock-free at each safepoint.
    /// Replaces the previous `Mutex<Vec<Instant>>` on `Shared`,
    /// which serialised every Yield on a single global lock.
    last_yield_micros: AtomicU64,
    /// Monotonic micros-since-process-start of the last targeted SIGURG the
    /// watchdog sent this worker, or zero if it has sent none.
    ///
    /// A signal is a request to reach a safepoint, and a worker that cannot
    /// reach one - a compiled numeric loop calling nothing - does not answer
    /// it. Without this the watchdog re-sends on every pass for as long as
    /// the loop runs, which is a kernel round trip per worker per pass on
    /// both sides and interrupts the very work it is waiting for.
    last_signal_micros: AtomicU64,
}

impl WorkerSlot {
    fn wake(&self) {
        if self.parked.swap(false, Ordering::AcqRel) {
            // Notify; the lock is held briefly only as the condvar
            // contract requires.
            let _g = self.cv_mu.lock();
            self.cv.notify_one();
        }
    }
}

/// State shared across every worker thread plus user-facing handles.
struct Shared {
    injector: Injector<SendTask>,
    workers: Mutex<Vec<Arc<WorkerSlot>>>,
    parked: Mutex<HashMap<Gid, ParkedEntry>>,
    /// Gids whose `unpark(gid)` arrived *before* the suspending
    /// worker had a chance to insert them into `parked`. The
    /// worker's Yield→park path checks this set and, if the gid
    /// is present, immediately re-ejects the task to the
    /// injector. Closes the wake-before-park race window.
    pre_unpark: Mutex<std::collections::HashSet<Gid>>,
    /// Live (spawned but not yet finished) goroutine count. The
    /// scheduler refuses new spawns above `max_live`.
    live_goroutines: AtomicUsize,
    /// Maximum live goroutines this scheduler will admit. Honours
    /// `runtime::set_max_procs` and `GOSSAMER_MAX_PROCS`. Default is
    /// `1_000_000`.
    max_live: AtomicUsize,
    stats: AtomicStats,
    park_wait: AtomicParkWaitStats,
    trace: Mutex<Option<Vec<ExecutionTraceEvent>>>,
    /// Set to `true` when [`MultiScheduler::shutdown`] is called.
    /// Workers exit once their local deque is drained.
    stopping: AtomicBool,
    /// Number of running worker threads. Used to coordinate dynamic
    /// resize.
    live_workers: AtomicUsize,
    /// Most recent target P count. The active worker pool is grown to
    /// match.
    target_workers: AtomicUsize,
    /// `true` once the watchdog thread has been spawned.
    #[cfg_attr(miri, allow(dead_code))]
    watchdog_started: AtomicBool,
    /// Set when the scheduler should request that all goroutines
    /// reach a safepoint (used by the GC).
    request_safepoint: AtomicBool,
    // `last_yield` was a `Mutex<Vec<Instant>>` that every worker
    // re-acquired on every Yield/Done. The per-worker
    // `last_yield_micros: AtomicU64` on each `WorkerSlot` is the
    // replacement: lock-free read in the watchdog, lock-free write
    // in the worker.
    /// Idle signal - workers notify when they reach a quiescent
    /// state (deque empty + no peer work + no parked tasks). The
    /// orchestrator's `wait_until_idle` parks on this Condvar
    /// instead of polling, so an empty `gos` does not consume
    /// CPU on the main thread while the workers are idle.
    idle_mu: Mutex<()>,
    idle_cv: Condvar,
}

struct ParkedEntry {
    task: SendTask,
    #[allow(
        dead_code,
        reason = "captured for introspection / scheduler debugging; readers will land alongside diagnostic surface"
    )]
    reason: ParkReason,
    /// Hint indicating which worker this task previously ran on, used
    /// to maintain locality on resume.
    home: usize,
    parked_at: Instant,
}

impl fmt::Debug for Shared {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("Shared")
            .field("injector_len", &self.injector.len())
            .field("live_workers", &self.live_workers.load(Ordering::Relaxed))
            .field(
                "target_workers",
                &self.target_workers.load(Ordering::Relaxed),
            )
            .field("parked", &self.parked.lock().len())
            .field("stats", &self.stats.snapshot())
            .finish_non_exhaustive()
    }
}

/// Multi-threaded work-stealing scheduler.
#[derive(Clone)]
pub struct MultiScheduler {
    inner: Arc<Shared>,
}

impl fmt::Debug for MultiScheduler {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("MultiScheduler")
            .field("shared", &self.inner)
            .finish()
    }
}

impl MultiScheduler {
    /// Returns a scheduler sized for `worker_count` workers (clamped
    /// to at least 1). The workers are not spawned until [`Self::run`]
    /// or [`Self::start`] is called.
    #[must_use]
    pub fn new(worker_count: usize) -> Self {
        let n = worker_count.max(1);
        let shared = Arc::new(Shared {
            injector: Injector::new(),
            workers: Mutex::new(Vec::new()),
            parked: Mutex::new(HashMap::new()),
            pre_unpark: Mutex::new(std::collections::HashSet::new()),
            live_goroutines: AtomicUsize::new(0),
            max_live: AtomicUsize::new(default_max_live()),
            stats: AtomicStats::default(),
            park_wait: AtomicParkWaitStats::default(),
            trace: Mutex::new(None),
            stopping: AtomicBool::new(false),
            live_workers: AtomicUsize::new(0),
            target_workers: AtomicUsize::new(n),
            watchdog_started: AtomicBool::new(false),
            request_safepoint: AtomicBool::new(false),
            idle_mu: Mutex::new(()),
            idle_cv: Condvar::new(),
        });
        Self { inner: shared }
    }

    /// Pushes a task onto the global injector. Workers that have an
    /// empty local deque will pick it up.
    ///
    /// Returns `None` when the live-goroutine cap (set via
    /// `runtime::set_max_procs` or `GOSSAMER_MAX_PROCS`) would be
    /// exceeded - surface the refusal to user code instead of
    /// silently overcommitting kernel resources.
    pub fn try_spawn<T: SchedTask + 'static>(&self, task: T) -> Option<Gid> {
        let max = self.inner.max_live.load(Ordering::Relaxed);
        let prev = self.inner.live_goroutines.fetch_add(1, Ordering::AcqRel);
        if prev >= max {
            self.inner.live_goroutines.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        // Ids come from the diagnostic registry's own counter so every entry
        // in that process-wide table has exactly one allocator. A second
        // sequence handing out ids into the same table would let one
        // goroutine's completion unregister another's live entry.
        let gid = Gid(crate::sigquit::next_id());
        crate::sigquit::register(gid.as_u32(), std::any::type_name::<T>());
        self.trace_event("spawn", gid, None);
        // Wrap the task with a `GidStamped` adapter so the worker
        // publishes the goroutine's `gid` into the race-detector
        // thread-local before each `step` and clears it after. This
        // is a no-op when the race detector is disabled (the only
        // cost is one TLS write per step).
        let stamped = GidStamped { gid, inner: task };
        self.inner.injector.push(Box::new(stamped));
        self.inner.stats.spawned.fetch_add(1, Ordering::Relaxed);
        self.wake_any();
        Some(gid)
    }

    /// Backwards-compatible wrapper: refusal panics. Callers that
    /// need graceful refusal should use `try_spawn`.
    pub fn spawn<T: SchedTask + 'static>(&self, task: T) -> Gid {
        self.try_spawn(task)
            .expect("MultiScheduler::spawn refused: live-goroutine cap reached")
    }

    /// Sets the maximum live-goroutine count. Returns the previous
    /// value. A value of zero disables the cap (interpreted as
    /// `usize::MAX`).
    #[must_use]
    pub fn set_max_goroutines(&self, n: usize) -> usize {
        let new = if n == 0 { usize::MAX } else { n };
        self.inner.max_live.swap(new, Ordering::AcqRel)
    }

    /// Current live-goroutine count.
    #[must_use]
    pub fn live_goroutines(&self) -> usize {
        self.inner.live_goroutines.load(Ordering::Relaxed)
    }

    /// Resizes the worker pool to `n`. Honoured asynchronously: extra
    /// workers are spawned immediately; surplus workers retire after
    /// finishing their current task. A value of `0` is clamped to one.
    /// Values larger than the worker-count cap (see
    /// [`Self::worker_count_cap`]) are clamped down so a runaway
    /// caller cannot exhaust kernel-thread budget by asking for tens
    /// of thousands of OS threads.
    pub fn set_worker_count(&self, n: usize) {
        let cap = Self::worker_count_cap();
        let target = n.clamp(1, cap);
        self.inner.target_workers.store(target, Ordering::Relaxed);
        self.reconcile_pool();
    }

    /// Hard upper bound on the worker pool. The default is
    /// `min(num_cpus * 4, 256)`; the `GOSSAMER_MAX_WORKERS`
    /// environment variable overrides it (clamped to `[1, 4096]`).
    /// Exposed so tests and tooling can read the bound the same way
    /// `set_worker_count` enforces it.
    #[must_use]
    pub fn worker_count_cap() -> usize {
        if let Ok(v) = std::env::var("GOSSAMER_MAX_WORKERS") {
            if let Ok(n) = v.parse::<usize>() {
                return n.clamp(1, 4096);
            }
        }
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        cores.saturating_mul(4).clamp(1, 256)
    }

    /// Returns the configured target worker count.
    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.inner.target_workers.load(Ordering::Relaxed)
    }

    /// Snapshot of scheduler statistics.
    #[must_use]
    pub fn stats(&self) -> MultiStats {
        self.inner.stats.snapshot()
    }

    /// Drives the scheduler until every spawned task (and every task
    /// any of those spawn) has finished. Returns the final statistics.
    /// Callers may push additional work before the final completion.
    #[must_use]
    pub fn run(&self) -> MultiStats {
        self.inner.stopping.store(false, Ordering::Release);
        crate::stack_guard::install_stack_guard();
        crate::preempt::init();
        crate::sigquit::install_handler();
        self.start_watchdog();
        self.reconcile_pool();
        self.wait_until_idle();
        self.shutdown();
        self.inner.stats.snapshot()
    }

    /// Starts the worker pool without blocking. Tasks pushed via
    /// [`Self::spawn`] run in the background; call [`Self::shutdown`]
    /// to drain workers when done.
    pub fn start(&self) {
        self.inner.stopping.store(false, Ordering::Release);
        crate::stack_guard::install_stack_guard();
        crate::preempt::init();
        crate::sigquit::install_handler();
        self.start_watchdog();
        self.reconcile_pool();
    }

    fn start_watchdog(&self) {
        #[cfg(miri)]
        {
            // Miri has no OS-signal model and no benefit from a background
            // wall-clock watchdog; safepoint behaviour stays testable through
            // explicit cooperative yield requests.
            return;
        }
        #[cfg(not(miri))]
        {
            if self.inner.watchdog_started.swap(true, Ordering::AcqRel) {
                return;
            }
            let inner = Arc::clone(&self.inner);
            thread::Builder::new()
                .name("gos-preempt-watchdog".to_string())
                .spawn(move || watchdog_loop(inner))
                .expect("spawn watchdog");
        }
    }

    /// Signals every worker to exit once their deques drain, then
    /// joins them.
    pub fn shutdown(&self) {
        self.inner.stopping.store(true, Ordering::Release);
        let workers = self.inner.workers.lock().clone();
        for slot in &workers {
            slot.retired.store(true, Ordering::Release);
            slot.wake();
        }
        // Joining the threads themselves happens inside
        // `reconcile_pool` / the ParkedJoinHandles store; there is no
        // join handle stored here because workers self-detach when
        // they retire. A future iteration could move handles into
        // `Shared` for a deterministic join.
    }

    /// Parks the goroutine identified by `gid`. The supplied `task`
    /// is held until [`Self::unpark`] resurrects it. The `home` hint
    /// indicates which worker should pick the task back up; values
    /// outside the worker count fall through to the injector.
    pub fn park(&self, gid: Gid, reason: ParkReason, home: usize, task: SendTask) {
        self.inner.parked.lock().insert(
            gid,
            ParkedEntry {
                task,
                reason,
                home,
                parked_at: Instant::now(),
            },
        );
        self.inner.stats.parks.fetch_add(1, Ordering::Relaxed);
        self.trace_event("park", gid, Some(reason));
    }

    /// Resurrects a previously parked goroutine. Returns `true` when a
    /// parked entry was found and re-enqueued.
    ///
    /// If the gid is not yet in `parked` - because the goroutine has
    /// armed its wakeup source but hasn't suspended yet - the gid is
    /// recorded in `pre_unpark`. The worker that's about to park the
    /// task checks this set and, if the gid is present, re-ejects
    /// the task to the injector instead of leaving it parked.
    #[allow(
        clippy::must_use_candidate,
        reason = "called for its side effect; the bool (found-parked vs pre-unpark) is informational and most call sites are fire-and-forget"
    )]
    pub fn unpark(&self, gid: Gid) -> bool {
        // Hold the `parked` guard across the `pre_unpark.insert()`
        // below so the worker's symmetric "insert into parked, then
        // check pre_unpark" sequence (in `worker_loop`) cannot
        // complete between our miss and our pre_unpark write. If we
        // released `parked` first, a worker could insert + check +
        // proceed before our pre_unpark.insert landed - leaving the
        // gid parked indefinitely. (Windows surfaces the race more
        // often because of the coarser timer-wake granularity that
        // widens the netpoller's deliver→worker park interleaving.)
        let mut parked = self.inner.parked.lock();
        let entry = parked.remove(&gid);
        let Some(entry) = entry else {
            self.inner.pre_unpark.lock().insert(gid);
            drop(parked);
            return false;
        };
        drop(parked);
        self.inner
            .park_wait
            .add(entry.reason, entry.parked_at.elapsed());
        self.trace_event("unpark", gid, None);
        let home = entry.home;
        // INVARIANT (retired-inbox handoff): an inbox push for a
        // worker slot is legal only while holding the `workers`
        // lock AND having observed `slot.retired == false` under
        // that lock. A retiring worker drains its inbox to the
        // global injector under the same lock after `retired` was
        // set, so every push either happens before the drain (and
        // is moved by it) or observes `retired == true` and routes
        // to the injector directly. Violating this strands the
        // task in a dead slot's inbox - a permanently lost wake.
        let workers = self.inner.workers.lock();
        match workers.get(home) {
            Some(slot) if !slot.retired.load(Ordering::Acquire) => {
                // Pin the resumed goroutine to the home worker -
                // stackful coroutines are not safe to migrate across
                // OS threads while suspended. Push onto the worker's
                // private inbox; the worker drains it before its main
                // deque on the next iteration.
                slot.inbox.push(entry.task);
                let slot = Arc::clone(slot);
                drop(workers);
                slot.wake();
            }
            _ => {
                // Home worker retired or gone (pool shrink, slot
                // replacement, shutdown). Hand the task to the
                // global injector so any live worker picks it up.
                drop(workers);
                self.inner.injector.push(entry.task);
                self.wake_any();
            }
        }
        self.inner.stats.unparks.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Returns the number of currently parked goroutines. Exposed for
    /// tests and introspection.
    #[must_use]
    pub fn parked_count(&self) -> usize {
        self.inner.parked.lock().len()
    }

    /// Returns a snapshot of currently parked goroutines grouped by reason.
    #[must_use]
    pub fn parked_reason_counts(&self) -> ParkedReasonCounts {
        let mut counts = ParkedReasonCounts::default();
        for entry in self.inner.parked.lock().values() {
            match entry.reason {
                ParkReason::Other => counts.other += 1,
                ParkReason::Chan => counts.chan += 1,
                ParkReason::Sync => counts.sync += 1,
                ParkReason::Io => counts.io += 1,
                ParkReason::Timer => counts.timer += 1,
            }
        }
        counts
    }

    /// Returns accumulated goroutine park time by wait category.
    #[must_use]
    pub fn park_wait_stats(&self) -> ParkWaitStats {
        self.inner.park_wait.snapshot()
    }

    /// Starts a fresh scheduler execution trace. Starting a new capture drops
    /// any prior unfinished capture.
    pub fn start_execution_trace(&self) {
        *self.inner.trace.lock() = Some(Vec::new());
    }

    /// Stops capture and returns scheduler spawn, park, and unpark events.
    #[must_use]
    pub fn finish_execution_trace(&self) -> Vec<ExecutionTraceEvent> {
        self.inner.trace.lock().take().unwrap_or_default()
    }

    fn trace_event(&self, name: &'static str, gid: Gid, reason: Option<ParkReason>) {
        if let Some(events) = self.inner.trace.lock().as_mut() {
            events.push(ExecutionTraceEvent {
                timestamp_micros: now_micros_since_start(),
                name,
                gid: gid.as_u32(),
                reason,
            });
        }
    }

    /// Asks every running goroutine to reach a safepoint at its next
    /// poll. Used by the GC before the concurrent mark phase.
    pub fn request_safepoint(&self) {
        self.inner.request_safepoint.store(true, Ordering::Release);
        crate::preempt::request_yield_all();
    }

    /// Clears the safepoint request once the caller is done.
    pub fn clear_safepoint(&self) {
        self.inner.request_safepoint.store(false, Ordering::Release);
    }

    fn reconcile_pool(&self) {
        let target = self.inner.target_workers.load(Ordering::Relaxed);
        let current = self.inner.live_workers.load(Ordering::Relaxed);
        if current >= target {
            // Mark surplus slots retired; they exit on next park.
            let workers = self.inner.workers.lock();
            for slot in workers.iter().skip(target) {
                slot.retired.store(true, Ordering::Release);
                slot.wake();
            }
            return;
        }
        for index in current..target {
            self.spawn_worker(index);
        }
    }

    fn spawn_worker(&self, index: usize) {
        let deque: Deque<SendTask> = Deque::new_fifo();
        let stealer = deque.stealer();
        let slot = Arc::new(WorkerSlot {
            stealer,
            inbox: Injector::new(),
            parked: AtomicBool::new(false),
            cv: Condvar::new(),
            cv_mu: Mutex::new(()),
            retired: AtomicBool::new(false),
            thread_handle: AtomicU64::new(0),
            last_yield_micros: AtomicU64::new(now_micros_since_start()),
            last_signal_micros: AtomicU64::new(0),
        });
        {
            let mut workers = self.inner.workers.lock();
            // Slot vector grows monotonically; we may overwrite a
            // retired slot if `index < workers.len()`.
            if index < workers.len() {
                workers[index] = Arc::clone(&slot);
            } else {
                while workers.len() < index {
                    // Pad: if for some reason we're spawning out of
                    // order, fill with retired placeholders.
                    let placeholder = Arc::new(WorkerSlot {
                        stealer: Deque::<SendTask>::new_fifo().stealer(),
                        inbox: Injector::new(),
                        parked: AtomicBool::new(false),
                        cv: Condvar::new(),
                        cv_mu: Mutex::new(()),
                        retired: AtomicBool::new(true),
                        thread_handle: AtomicU64::new(0),
                        last_yield_micros: AtomicU64::new(now_micros_since_start()),
                        last_signal_micros: AtomicU64::new(0),
                    });
                    workers.push(placeholder);
                }
                workers.push(Arc::clone(&slot));
            }
        }
        self.inner.live_workers.fetch_add(1, Ordering::AcqRel);
        let inner = Arc::clone(&self.inner);
        let _: JoinHandle<()> = thread::Builder::new()
            .name(format!("gos-sched-{index}"))
            .spawn(move || worker_loop(index, deque, slot, inner))
            .expect("scheduler worker thread spawn failed");
    }

    fn wake_any(&self) {
        let workers = self.inner.workers.lock();
        for slot in workers.iter() {
            if slot.parked.load(Ordering::Acquire) {
                slot.wake();
                return;
            }
        }
    }

    /// Blocks until every spawned task has finished (`live == 0` and
    /// `spawned == finished`), or `timeout` passes. Returns whether the
    /// pool quiesced. The bound is a liveness guarantee for process
    /// exit: a goroutine blocked forever (say, on a channel nobody
    /// sends to) must not wedge the process. Waits on `idle_cv` - the
    /// workers notify it on every transition that could make the pool
    /// idle, and the 200 ms re-check cap covers the same missed-wake
    /// races `wait_until_idle` documents.
    #[must_use]
    pub fn wait_quiescent(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.idle_mu.lock();
        loop {
            let stats = self.inner.stats.snapshot();
            if self.live_goroutines() == 0 && stats.spawned == stats.finished {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let wait = (deadline - now).min(Duration::from_millis(200));
            self.inner.idle_cv.wait_for(&mut g, wait);
        }
    }

    fn wait_until_idle(&self) {
        let mut g = self.inner.idle_mu.lock();
        loop {
            if self.is_idle_snapshot() {
                return;
            }
            // Bounded wait so a missed wake-up never strands the
            // orchestrator. The bound is loose (200 ms) because
            // workers actively notify on every transition that
            // could make us idle - the timeout is only a safety
            // net for races during scheduler resize / shutdown.
            self.inner
                .idle_cv
                .wait_for(&mut g, Duration::from_millis(200));
        }
    }

    fn is_idle_snapshot(&self) -> bool {
        let injector_empty = self.inner.injector.is_empty();
        let parked_empty = self.inner.parked.lock().is_empty();
        let workers = self.inner.workers.lock();
        let all_parked = !workers.is_empty()
            && workers
                .iter()
                .all(|s| s.parked.load(Ordering::Acquire) || s.retired.load(Ordering::Acquire));
        let no_local_work = workers.iter().all(|s| s.stealer.is_empty());
        drop(workers);
        injector_empty && parked_empty && all_parked && no_local_work
    }
}

/// RAII guard around the per-worker thread handle. On drop (panic
/// unwind or normal return) it zeroes the `WorkerSlot::thread_handle`
/// and hands the OS handle back to [`crate::preempt::release_thread_handle`].
///
/// Without this guard, a panicking goroutine on Windows leaks the
/// `DuplicateHandle`-allocated thread handle: `preempt::current_thread_handle`
/// acquires it, the worker hits a panic, the unwinder destroys the
/// frame without anyone running release. Long-running services hit
/// the per-process handle limit and start failing thread creation.
struct WorkerHandleGuard {
    slot: Arc<WorkerSlot>,
}

impl Drop for WorkerHandleGuard {
    fn drop(&mut self) {
        let prev = self.slot.thread_handle.swap(0, Ordering::AcqRel);
        if prev != 0 {
            crate::preempt::release_thread_handle(prev);
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "scheduler worker loop is intentionally linear"
)]
fn worker_loop(index: usize, deque: Deque<SendTask>, slot: Arc<WorkerSlot>, shared: Arc<Shared>) {
    crate::stack_guard::install_stack_guard();
    // Round-robin steal cursor - biases away from always poking the
    // same peer first, which would imbalance work.
    let mut steal_cursor = index.wrapping_add(1);
    // Publish this thread's pthread_t so the watchdog can pthread_kill
    // a stuck worker. Released so the watchdog observes
    // the value before it tries to use it.
    slot.thread_handle
        .store(crate::preempt::current_thread_handle(), Ordering::Release);
    // RAII guard: even if the worker panics, the handle is released
    // and the slot zeroed before this frame unwinds.
    let _handle_guard = WorkerHandleGuard {
        slot: Arc::clone(&slot),
    };
    slot.last_yield_micros
        .store(now_micros_since_start(), Ordering::Release);
    loop {
        if slot.retired.load(Ordering::Acquire) {
            // Hand off every task still queued for this worker
            // before the thread exits - anything left behind would
            // never run again (a permanently lost wake). The inbox
            // drain holds the `workers` lock to pair with the
            // retired-inbox handoff invariant in
            // [`MultiScheduler::unpark`]: pushes happen under that
            // lock only after observing `retired == false`, so this
            // drain (which runs after `retired` was set, under the
            // same lock) sees every push that did not already
            // divert to the injector. The local deque only ever
            // receives pushes from this thread, so draining it here
            // is race-free by construction.
            let mut handed_off = false;
            {
                let _workers = shared.workers.lock();
                loop {
                    match slot.inbox.steal() {
                        Steal::Success(task) => {
                            shared.injector.push(task);
                            handed_off = true;
                        }
                        Steal::Empty => break,
                        Steal::Retry => {}
                    }
                }
            }
            while let Some(task) = deque.pop() {
                shared.injector.push(task);
                handed_off = true;
            }
            if handed_off {
                let workers = shared.workers.lock();
                for peer in workers.iter() {
                    if peer.parked.load(Ordering::Acquire) {
                        peer.wake();
                        break;
                    }
                }
            }
            // Zero the handle before exiting so the watchdog cannot
            // call pthread_kill on a thread that has already exited.
            slot.thread_handle.store(0, Ordering::Release);
            shared.live_workers.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        let task = next_task(index, &deque, &slot, &shared, &mut steal_cursor);
        let Some(mut task) = task else {
            if shared.stopping.load(Ordering::Acquire) {
                // Same: zero the handle before this thread exits.
                slot.thread_handle.store(0, Ordering::Release);
                shared.live_workers.fetch_sub(1, Ordering::AcqRel);
                // The exiting worker may have been the last one
                // holding wait_until_idle awake. Notify in case
                // shutdown is in progress.
                let _g = shared.idle_mu.lock();
                shared.idle_cv.notify_all();
                return;
            }
            // About to park: tell wait_until_idle to re-snapshot.
            // The Condvar is signalled inside park_worker after the
            // parked flag is set so the snapshot sees a consistent
            // view.
            park_worker(&slot, &shared);
            continue;
        };
        let step = task.step();
        shared.stats.steps.fetch_add(1, Ordering::Relaxed);
        match step {
            Step::Yield => {
                shared.stats.yields.fetch_add(1, Ordering::Relaxed);
                slot.last_yield_micros
                    .store(now_micros_since_start(), Ordering::Release);
                // The goroutine may have requested a park via
                // `sched_global::park`. The park helper writes
                // `(gid, reason)` into a thread-local slot before
                // suspending; we honour it here so the suspended
                // goroutine sits in the parked map until its
                // wakeup source unparks it, instead of busy-
                // looping back through the run queue.
                if let Some((gid, reason)) = crate::sched_global::take_pending_park() {
                    let mut parked = shared.parked.lock();
                    parked.insert(
                        gid,
                        ParkedEntry {
                            task,
                            reason,
                            home: index,
                            parked_at: Instant::now(),
                        },
                    );
                    shared.stats.parks.fetch_add(1, Ordering::Relaxed);
                    if let Some(events) = shared.trace.lock().as_mut() {
                        events.push(ExecutionTraceEvent {
                            timestamp_micros: now_micros_since_start(),
                            name: "park",
                            gid: gid.as_u32(),
                            reason: Some(reason),
                        });
                    }
                    // Race-window protection: if `unpark(gid)`
                    // already fired (poller delivery between
                    // `arm()` and the park insertion), the gid is
                    // queued in `pre_unpark`. Drain that and, if
                    // our gid is in it, immediately re-eject the
                    // task.
                    let mut pre = shared.pre_unpark.lock();
                    if pre.remove(&gid) {
                        if let Some(entry) = parked.remove(&gid) {
                            drop(pre);
                            drop(parked);
                            shared
                                .park_wait
                                .add(entry.reason, entry.parked_at.elapsed());
                            if let Some(events) = shared.trace.lock().as_mut() {
                                events.push(ExecutionTraceEvent {
                                    timestamp_micros: now_micros_since_start(),
                                    name: "unpark",
                                    gid: gid.as_u32(),
                                    reason: None,
                                });
                            }
                            // Back onto this worker's own deque, never the
                            // global injector. This goroutine parked on this
                            // OS thread, and a suspended stackful coroutine
                            // stays on the thread it suspended on - the same
                            // reason `unpark` pins to `home` and peer
                            // stealing is off. A thread-local read taken
                            // before the suspend and used after it, such as
                            // the current-goroutine id, resolves against
                            // whichever thread resumed the stack.
                            deque.push(entry.task);
                            shared.stats.unparks.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                } else {
                    deque.push(task);
                }
            }
            Step::Done => {
                shared.stats.finished.fetch_add(1, Ordering::Relaxed);
                shared.live_goroutines.fetch_sub(1, Ordering::AcqRel);
                slot.last_yield_micros
                    .store(now_micros_since_start(), Ordering::Release);
            }
        }
    }
}

/// Default `MultiScheduler::max_live` - 1M live goroutines, or
/// `GOSSAMER_MAX_GOROUTINES` if set. Surfaces a
/// `for _ in 0.. { go work() }` runaway as a refused spawn rather
/// than a kernel-thread OOM.
///
/// `GOSSAMER_MAX_PROCS` controls the worker (P) count, not the
/// live-goroutine cap; the two were previously conflated, which
/// surprised callers that wanted "use 4 cores" but inadvertently
/// limited themselves to 4 live goroutines. Use
/// `GOSSAMER_MAX_GOROUTINES` to cap the goroutine count.
fn default_max_live() -> usize {
    if let Ok(s) = std::env::var("GOSSAMER_MAX_GOROUTINES") {
        if let Ok(n) = s.parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    1_000_000
}

/// Watchdog loop: every ~5 ms, checks per-worker `last_yield`
/// timestamps and bumps the global preempt phase for any worker
/// that has been running without yielding for more than 10 ms.
/// Compiled / interpreter code is expected to call into
/// [`crate::preempt::should_yield`] at safepoints and
/// honour the request.
///
/// When a worker has been running for more than
/// `kill_threshold`, the watchdog also sends SIGURG to that worker's
/// OS thread. The cooperative bump alone is silent if the worker is
/// inside a tight C-side loop or a blocking syscall; the kernel
/// signal interrupts both.
#[cfg_attr(miri, allow(dead_code))]
fn watchdog_loop(shared: Arc<Shared>) {
    let preempt_threshold = Duration::from_millis(10);
    let kill_threshold = Duration::from_millis(100);
    loop {
        if shared.stopping.load(Ordering::Acquire) {
            // One last bump so any spinning thread observes the
            // shutdown and reaches a safepoint.
            crate::preempt::request_yield_all();
            return;
        }
        crate::platform::sleep(Duration::from_millis(5));
        let now_micros = now_micros_since_start();
        let preempt_micros = u64::try_from(preempt_threshold.as_micros()).unwrap_or(u64::MAX);
        let kill_micros = u64::try_from(kill_threshold.as_micros()).unwrap_or(u64::MAX);
        let mut needs_preempt = false;
        let mut kill_indices: Vec<usize> = Vec::new();
        // Lock-free read of every worker's last-yield timestamp.
        // Acquire the workers list briefly to walk the slot vector;
        // each slot's `last_yield_micros` is then read with an
        // atomic load, with no mutex held across the comparison.
        let snapshot: Vec<u64> = {
            let workers = shared.workers.lock();
            workers
                .iter()
                .map(|s| s.last_yield_micros.load(Ordering::Acquire))
                .collect()
        };
        for (i, ts) in snapshot.iter().enumerate() {
            let elapsed = now_micros.saturating_sub(*ts);
            if elapsed > preempt_micros {
                needs_preempt = true;
            }
            if elapsed > kill_micros {
                kill_indices.push(i);
            }
        }
        if needs_preempt || shared.request_safepoint.load(Ordering::Acquire) {
            crate::preempt::request_yield_all();
            crate::preempt::bump_pressure();
        }
        if !kill_indices.is_empty() {
            let workers = shared.workers.lock();
            for i in kill_indices {
                if let Some(slot) = workers.get(i) {
                    // Skip retired slots - the thread may have already
                    // exited and the pthread_t would be dangling.
                    if slot.retired.load(Ordering::Acquire) {
                        continue;
                    }
                    // One signal per overstay window. The worker answers by
                    // reaching a safepoint, which moves its yield timestamp
                    // and takes it out of this list; until then a second
                    // signal tells it nothing the first did not.
                    let last_signal = slot.last_signal_micros.load(Ordering::Acquire);
                    if last_signal != 0 && now_micros.saturating_sub(last_signal) < kill_micros {
                        continue;
                    }
                    slot.last_signal_micros.store(now_micros, Ordering::Release);
                    let handle = slot.thread_handle.load(Ordering::Acquire);
                    let _ = crate::preempt::signal_thread_sigurg(handle);
                }
            }
        }
    }
}

fn next_task(
    _index: usize,
    deque: &Deque<SendTask>,
    self_slot: &Arc<WorkerSlot>,
    shared: &Arc<Shared>,
    steal_cursor: &mut usize,
) -> Option<SendTask> {
    // 1) own inbox - unparked goroutines pinned to this worker.
    loop {
        match self_slot.inbox.steal_batch_and_pop(deque) {
            Steal::Success(task) => {
                shared.stats.unparks.fetch_add(1, Ordering::Relaxed);
                return Some(task);
            }
            Steal::Empty => break,
            Steal::Retry => {}
        }
    }
    // 2) Global injector. Check it before the local deque so a task
    // that was cooperatively preempted cannot immediately select
    // itself forever while newly spawned work waits in the injector.
    // This is essential for fairness when the pool has one worker.
    loop {
        match shared.injector.steal_batch_and_pop(deque) {
            Steal::Success(task) => {
                shared.stats.injects.fetch_add(1, Ordering::Relaxed);
                return Some(task);
            }
            Steal::Empty => break,
            Steal::Retry => {}
        }
    }
    // 3) Tasks that previously yielded on this worker.
    if let Some(task) = deque.pop() {
        return Some(task);
    }
    // Peer-stealing is disabled: stackful coroutines from
    // [`gossamer_coro::Goroutine`] (built on `corosensei`) are not
    // safe to migrate across OS worker threads while suspended.
    // Once a goroutine lands on a worker (via the global injector
    // on first spawn, or on its `home` worker after unpark), it
    // stays there for its lifetime. The trade-off: load imbalance
    // under non-uniform per-goroutine work; for the dominant
    // HTTP keep-alive shape (each connection = one goroutine,
    // uniform per-request work), all workers stay busy because
    // the injector hands out new connections one by one.
    let _ = self_slot;
    let _ = steal_cursor;
    let _ = shared;
    None
}

fn park_worker(slot: &Arc<WorkerSlot>, shared: &Arc<Shared>) {
    slot.parked.store(true, Ordering::Release);
    // Wake any orchestrator waiting in `wait_until_idle`: now that
    // this worker is parked, the snapshot may show all-idle.
    {
        let _g = shared.idle_mu.lock();
        shared.idle_cv.notify_all();
    }
    let mut g = slot.cv_mu.lock();
    // Brief timeout so a missed wake doesn't strand the worker forever.
    let _ = slot.cv.wait_for(&mut g, Duration::from_millis(50));
    slot.parked.store(false, Ordering::Release);
}

/// Task adapter that publishes the goroutine's `gid` into the
/// race-detector thread-local for the duration of each `step`,
/// so `crate::race::current_gid` returns the right
/// value when the task touches a sync primitive. Cleared after
/// the step so the host thread's gid does not pollute work the
/// scheduler queue runs between tasks.
struct GidStamped<T> {
    gid: Gid,
    inner: T,
}

impl<T: Task> Task for GidStamped<T> {
    fn step(&mut self) -> Step {
        crate::race::set_current_gid(self.gid.as_u32());
        crate::sched_global::set_current_gid(self.gid);
        // Bind the active goroutine to this OS thread so the
        // call-stack-push helpers attribute frames to the right
        // entry. Cleared after the step so unrelated work the
        // worker picks up between tasks doesn't reuse the gid.
        crate::sigquit::set_active_gid(self.gid.as_u32());
        let result = self.inner.step();
        crate::sigquit::set_active_gid(u32::MAX);
        crate::sched_global::clear_current_gid();
        crate::race::set_current_gid(0);
        if matches!(result, Step::Done) {
            // The registry entry exists to describe a live goroutine; a
            // finished one is never dumped again, and retaining it would grow
            // the table for the lifetime of the process.
            crate::sigquit::unregister(self.gid.as_u32());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct CountTask {
        counter: Arc<AtomicUsize>,
        budget: usize,
    }

    impl Task for CountTask {
        fn step(&mut self) -> Step {
            if self.budget == 0 {
                return Step::Done;
            }
            self.budget -= 1;
            self.counter.fetch_add(1, Ordering::Relaxed);
            if self.budget == 0 {
                Step::Done
            } else {
                Step::Yield
            }
        }
    }

    struct CoroTask(gossamer_coro::Goroutine);

    impl Task for CoroTask {
        fn step(&mut self) -> Step {
            let yielder = self.0.yielder_ptr();
            if !yielder.is_null() {
                gossamer_coro::set_current_yielder(yielder);
            }
            let done = self.0.resume();
            gossamer_coro::clear_current_yielder();
            if done { Step::Done } else { Step::Yield }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn watchdog_preempts_tight_loop_on_one_worker() {
        let sched = MultiScheduler::new(1);
        let peer_ran = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&peer_ran);
        sched.spawn(CoroTask(gossamer_coro::Goroutine::new(Box::new(
            move || {
                while !stop.load(Ordering::Acquire) {
                    let _ = crate::preempt::gos_rt_preempt_check_and_yield();
                    std::hint::spin_loop();
                }
            },
        ))));
        let peer = Arc::clone(&peer_ran);
        sched.spawn(CoroTask(gossamer_coro::Goroutine::new(Box::new(
            move || peer.store(true, Ordering::Release),
        ))));

        let stats = sched.run();
        assert!(peer_ran.load(Ordering::Acquire));
        assert!(
            stats.yields > 0,
            "tight loop should reach a preemption yield"
        );
        assert_eq!(stats.finished, 2);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // starts worker threads that sigaltstack; Miri has no signals
    fn drains_all_tasks_across_workers() {
        let sched = MultiScheduler::new(4);
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..256 {
            sched.spawn(CountTask {
                counter: Arc::clone(&counter),
                budget: 8,
            });
        }
        let stats = sched.run();
        assert_eq!(counter.load(Ordering::Relaxed), 256 * 8);
        assert_eq!(stats.finished, 256);
        assert!(stats.steps >= 256 * 8);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // starts worker threads that sigaltstack; Miri has no signals
    fn park_unpark_round_trip() {
        let sched = MultiScheduler::new(2);
        sched.start();
        // Push a parked task directly.
        let task: SendTask = Box::new(CountTask {
            counter: Arc::new(AtomicUsize::new(0)),
            budget: 1,
        });
        let gid = Gid(99);
        sched.park(gid, ParkReason::Other, 0, task);
        assert_eq!(sched.parked_count(), 1);
        assert_eq!(
            sched.parked_reason_counts(),
            ParkedReasonCounts {
                other: 1,
                ..ParkedReasonCounts::default()
            }
        );
        assert!(sched.unpark(gid));
        assert_eq!(sched.parked_count(), 0);
        assert_eq!(sched.parked_reason_counts(), ParkedReasonCounts::default());
        let _ = sched.run();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // starts worker threads that sigaltstack; Miri has no signals
    fn park_wait_stats_accumulate_by_reason() {
        let sched = MultiScheduler::new(1);
        let task: SendTask = Box::new(CountTask {
            counter: Arc::new(AtomicUsize::new(0)),
            budget: 1,
        });
        let gid = Gid(100);
        sched.park(gid, ParkReason::Sync, 0, task);
        crate::platform::sleep(Duration::from_millis(2));
        assert!(sched.unpark(gid));
        assert!(
            sched.park_wait_stats().sync_micros >= 1_000,
            "parked synchronization wait was not accumulated"
        );
        sched.shutdown();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // starts worker threads that sigaltstack; Miri has no signals
    fn execution_trace_records_park_and_unpark() {
        let sched = MultiScheduler::new(1);
        let task: SendTask = Box::new(CountTask {
            counter: Arc::new(AtomicUsize::new(0)),
            budget: 1,
        });
        let gid = Gid(101);
        sched.start_execution_trace();
        sched.park(gid, ParkReason::Chan, 0, task);
        assert!(sched.unpark(gid));
        let trace = sched.finish_execution_trace();
        assert!(
            trace
                .iter()
                .any(|event| event.name == "park" && event.reason == Some(ParkReason::Chan))
        );
        assert!(
            trace
                .iter()
                .any(|event| event.name == "unpark" && event.gid == gid.as_u32())
        );
        sched.shutdown();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // starts worker threads that sigaltstack; Miri has no signals
    fn unpark_after_home_worker_retired_still_runs_task() {
        let sched = MultiScheduler::new(4);
        sched.start();
        let counter = Arc::new(AtomicUsize::new(0));
        let task: SendTask = Box::new(CountTask {
            counter: Arc::clone(&counter),
            budget: 1,
        });
        let gid = Gid(7);
        // Park with home = 3, then retire that worker before the
        // wake arrives - the exact interleaving `set_max_procs`
        // round-trips produce while a goroutine is parked.
        sched.park(gid, ParkReason::Io, 3, task);
        sched.set_worker_count(1);
        assert!(sched.unpark(gid));
        // The resurrected task must still run even though its home
        // worker is retired. Bounded condition-poll, same idiom as
        // the sched_global spawn tests.
        for _ in 0..400 {
            if counter.load(Ordering::Relaxed) == 1 {
                sched.shutdown();
                return;
            }
            crate::platform::sleep(Duration::from_millis(5));
        }
        panic!("unparked task stranded after its home worker retired");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // starts worker threads that sigaltstack; Miri has no signals
    fn retiring_worker_does_not_strand_inbox_tasks() {
        let sched = MultiScheduler::new(2);
        sched.start();
        let counter = Arc::new(AtomicUsize::new(0));
        let task: SendTask = Box::new(CountTask {
            counter: Arc::clone(&counter),
            budget: 1,
        });
        // Push directly into worker 1's inbox, then retire it before
        // it necessarily noticed the push - the task must migrate to
        // a surviving worker instead of dying with the slot.
        {
            let workers = sched.inner.workers.lock();
            workers[1].inbox.push(task);
        }
        sched.set_worker_count(1);
        for _ in 0..400 {
            if counter.load(Ordering::Relaxed) == 1 {
                sched.shutdown();
                return;
            }
            crate::platform::sleep(Duration::from_millis(5));
        }
        panic!("inbox task stranded on retired worker");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // starts worker threads that sigaltstack; Miri has no signals
    fn set_worker_count_grows_pool() {
        let sched = MultiScheduler::new(1);
        sched.start();
        sched.set_worker_count(4);
        assert_eq!(sched.worker_count(), 4);
        let _ = sched.run();
    }
}
