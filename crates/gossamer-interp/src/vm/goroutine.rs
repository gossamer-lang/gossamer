//! Goroutine worker pool backing the bytecode VM's `Op::Spawn`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Joins every outstanding goroutine and returns once they finish.
/// Called by the CLI entrypoint after `main` returns so spawned work
/// has a chance to land before the process exits.
pub fn join_outstanding_goroutines() {
    MAIN_RETURNED.store(true, Ordering::Release);
    crate::value::wake_all_channel_waiters();
    if let Some(pool) = POOL.get() {
        pool.drain_until(exit_drain_deadline);
    }
}

/// The instant every wait on the exit path is bounded by, starting the
/// clock on the first call.
///
/// The root cohort's drain and the pool drain that follows it are two
/// waits over the same goroutines, so they share one deadline: the process
/// leaves when a compiled binary would, rather than at the sum of two
/// independent bounds. A goroutine still running at that instant is one
/// the root cohort has already reported and abandoned, and waiting past it
/// would contradict that report.
///
/// Both drains take this as a function rather than a value: a monotonic
/// reading belongs to a wait that actually happens. An exit with nothing
/// outstanding never asks for one, which is all a target with no monotonic
/// clock can answer - wasm32 runs a goroutine to completion at its spawn
/// site, so its pool reaches exit already drained.
pub(crate) fn exit_drain_deadline() -> gossamer_runtime::platform::Instant {
    *EXIT_DRAIN_UNTIL.get_or_init(|| {
        gossamer_runtime::platform::Instant::now()
            + crate::stdlib_builtins::cohort::ROOT_DRAIN_DEADLINE
    })
}

static EXIT_DRAIN_UNTIL: OnceLock<gossamer_runtime::platform::Instant> = OnceLock::new();

/// Set once `main` has returned and the process is draining spawned work.
/// From that point the program's participants are the outstanding
/// goroutines alone, so a program is stuck as soon as all of them are
/// waiting on channels that can hand nothing over.
static MAIN_RETURNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Goroutines parked in a channel wait that nothing left in the program can
/// satisfy. Draining stops once every outstanding goroutine is counted here:
/// waiting longer cannot change the answer, and the process leaves them
/// parked exactly as a compiled binary does.
static STUCK_WAITERS: AtomicU64 = AtomicU64::new(0);

/// Goroutine task: a closure to run on a pool worker.
type GoroutineTask = Box<dyn FnOnce() + Send + 'static>;

/// Spawns an OS thread that runs Gossamer goroutine code. Sizes its
/// stack to [`crate::vm::VM_THREAD_STACK_BYTES`] - the same reserve the
/// main VM thread uses - because a goroutine body runs the bytecode
/// interpreter and JIT. The byte-budget recursion guard is armed at the
/// thread's shallowest point so native recursion reports a clean overflow
/// before reaching the OS guard page.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_goroutine_thread<F: FnOnce() + Send + 'static>(
    name: &str,
    body: F,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(crate::vm::VM_THREAD_STACK_BYTES)
        .spawn(move || {
            gossamer_coro::arm_stack_guard(
                crate::vm::VM_THREAD_STACK_BYTES - gossamer_coro::STACK_GUARD_MARGIN,
            );
            body();
        })
}

/// Elastic worker pool that runs goroutines spawned via the bytecode
/// `Op::Spawn`. Workers share one task queue so a goroutine costs a queue
/// push rather than a fresh OS thread and a cold-started `Vm`.
///
/// The pool starts small: each worker owns a large VM stack, and a test or
/// embedding can run many interpreter processes concurrently, so claiming a
/// thread per CPU up front multiplies that footprint across processes.
/// `GOSSAMER_VM_GOROUTINE_WORKERS` sets that starting size.
///
/// A background watchdog then grows it, up to [`MAX_WORKERS`], but only when
/// the pool has made zero progress for a full [`STARVATION_CHECK_INTERVAL`]
/// while saturated - see [`GoroutinePool::start_starvation_watchdog`] for
/// why growth is reactive rather than triggered by `spawn` itself.
///
/// `outstanding` tracks queued + in-flight tasks so
/// [`join_outstanding_goroutines`] can wait for completion.
pub(crate) struct GoroutinePool {
    inner: parking_lot::Mutex<PoolInner>,
    cv: parking_lot::Condvar,
    /// Wake-up condition for `drain()` to learn that the
    /// counter has reached zero.
    drain_cv: parking_lot::Condvar,
    /// Total tasks that have not yet completed (queued +
    /// running). Used by `drain()` for completion wait.
    outstanding: AtomicU64,
    /// Worker threads created so far, counted at creation rather than on
    /// thread entry so a growth decision cannot race a starting worker.
    workers: AtomicUsize,
    /// Workers parked waiting for a task. Read and written under `inner`,
    /// so the watchdog sees an accurate count when deciding whether to grow.
    idle: AtomicUsize,
    /// Tasks that have finished, counted monotonically. The watchdog
    /// compares two snapshots across an interval: unequal means the pool
    /// is progressing (however slowly) and needs no help; equal, with the
    /// queue non-empty and no worker idle, means nothing can ever free a
    /// worker on its own.
    completions: AtomicU64,
    /// Tasks ever enqueued, counted monotonically. `outstanding` falls as
    /// tasks finish, so it cannot answer how many goroutines a program has
    /// started; this reports that for `runtime::scheduler_stats_json`.
    spawned: AtomicU64,
    /// Tasks ever pulled off the shared queue by a worker. The pool has one
    /// queue and no per-worker deques, so every task a worker runs was taken
    /// from it - the counterpart to `MultiStats::injects`.
    injects: AtomicU64,
}

struct PoolInner {
    queue: VecDeque<GoroutineTask>,
    /// `true` once the runtime is shutting down; workers exit
    /// once `queue` drains. Currently never set in practice
    /// (the process exits right after `drain()` returns), but
    /// kept for cleanliness.
    shutting_down: bool,
}

impl GoroutinePool {
    fn new(num_workers: usize) -> Arc<Self> {
        let pool = Arc::new(Self {
            inner: parking_lot::Mutex::new(PoolInner {
                queue: VecDeque::new(),
                shutting_down: false,
            }),
            cv: parking_lot::Condvar::new(),
            drain_cv: parking_lot::Condvar::new(),
            outstanding: AtomicU64::new(0),
            workers: AtomicUsize::new(0),
            idle: AtomicUsize::new(0),
            completions: AtomicU64::new(0),
            spawned: AtomicU64::new(0),
            injects: AtomicU64::new(0),
        });
        // wasm32 is single-threaded: there are no worker threads. `go` /
        // `spawn` run the goroutine body to completion immediately (see
        // the wasm `spawn` below), matching the eager coro shim.
        #[cfg(target_arch = "wasm32")]
        let _ = num_workers;
        #[cfg(not(target_arch = "wasm32"))]
        {
            for _ in 0..num_workers {
                Self::start_worker(&pool);
            }
            Self::start_starvation_watchdog(&pool);
        }
        pool
    }

    /// Adds one worker thread to `pool`, counting it before the thread
    /// starts so a concurrent growth decision cannot double-count it.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_worker(pool: &Arc<Self>) {
        pool.workers.fetch_add(1, Ordering::Relaxed);
        {
            let p = Arc::clone(pool);
            let spawned = spawn_goroutine_thread("gossamer-worker", move || {
                ON_GOROUTINE_WORKER.with(|flag| flag.set(true));
                loop {
                    let task = {
                        let mut inner = p.inner.lock();
                        loop {
                            if let Some(task) = inner.queue.pop_front() {
                                p.injects.fetch_add(1, Ordering::Relaxed);
                                break Some(task);
                            }
                            if inner.shutting_down {
                                break None;
                            }
                            p.idle.fetch_add(1, Ordering::Relaxed);
                            p.cv.wait(&mut inner);
                            p.idle.fetch_sub(1, Ordering::Relaxed);
                        }
                    };
                    match task {
                        Some(task) => {
                            // A worker task must settle the outstanding counter even
                            // when an implementation panic escapes its VM boundary.
                            // Otherwise program exit waits forever after main has
                            // already printed all of its output. User panics are
                            // converted to RuntimeError by `spawn_goroutine_native`;
                            // this catch is the final containment for host panics.
                            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)).is_err()
                            {
                                eprintln!("gossamer: goroutine worker panicked");
                            }
                            // A panic still counts as progress: the worker is
                            // free again either way, which is all the
                            // starvation watchdog needs to know.
                            p.completions.fetch_add(1, Ordering::AcqRel);
                            // `drain()` checks `outstanding` while holding
                            // `inner` and then waits on `drain_cv`.  Update
                            // the predicate under that same mutex: notifying
                            // without it admits the classic lost-wakeup race
                            // where a completed task signals between the
                            // check and the Condvar wait, stranding program
                            // shutdown after all user output is printed.
                            let _inner = p.inner.lock();
                            let prev = p.outstanding.fetch_sub(1, Ordering::AcqRel);
                            note_progress();
                            if prev == 1 {
                                // Last in-flight task settled -
                                // wake any drain() waiter.
                                p.drain_cv.notify_all();
                            }
                        }
                        None => break,
                    }
                }
            });
            if let Err(e) = spawned {
                // A pool below its requested size starves blocking
                // goroutines (e.g. `go http::serve`); say so instead
                // of failing silently.
                pool.workers.fetch_sub(1, Ordering::Relaxed);
                eprintln!("gossamer-worker spawn failed: {e}");
            }
        }
    }

    /// Enqueues a task on `pool` and wakes a parked worker, if one is
    /// waiting.
    ///
    /// Deliberately does not decide growth: a burst of `go` statements
    /// arriving faster than the existing workers can drain them looks
    /// identical, for one instant, to a pool that can never drain them at
    /// all - "queue longer than idle workers" is true many times a second
    /// in a healthy worker-pool program and stays true for the life of a
    /// genuinely stuck one. Only observing the pool *over time*, which
    /// [`start_starvation_watchdog`](Self::start_starvation_watchdog) does,
    /// tells those two apart.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn spawn(pool: &Arc<Self>, task: GoroutineTask) {
        let mut inner = pool.inner.lock();
        // Publish the counter and task under the same mutex used by
        // `drain()`, so a drain cannot observe an empty program while a
        // concurrent goroutine spawn is about to enqueue work.
        pool.outstanding.fetch_add(1, Ordering::AcqRel);
        pool.spawned.fetch_add(1, Ordering::Relaxed);
        note_progress();
        inner.queue.push_back(task);
        drop(inner);
        pool.cv.notify_one();
    }

    /// Single-threaded wasm: run the goroutine body to completion
    /// immediately. A body that tries to block reaches
    /// `gossamer_coro::suspend`, which panics with the documented
    /// "blocking not supported in the playground" message.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn spawn(pool: &Arc<Self>, task: GoroutineTask) {
        pool.spawned.fetch_add(1, Ordering::Relaxed);
        pool.injects.fetch_add(1, Ordering::Relaxed);
        task();
        pool.completions.fetch_add(1, Ordering::AcqRel);
    }

    /// Starts the one background thread that grows `pool`, and only when
    /// growth is the sole way anything could proceed.
    ///
    /// A goroutine blocked in a channel operation keeps its worker for the
    /// duration, so a fixed-size pool can starve: four workers parked as
    /// receivers, a fifth goroutine (the only sender that could unblock
    /// them) stuck in the queue because no worker is ever free to run it.
    /// Nothing about that state ever changes on its own, which is exactly
    /// what distinguishes it from an ordinary busy pool - a worker pool
    /// draining a large backlog also has zero idle workers and a non-empty
    /// queue at every instant, but it keeps completing tasks throughout.
    ///
    /// So growth is judged over an interval, not a snapshot: take two
    /// [`completions`](GoroutinePool::completions) readings
    /// [`STARVATION_CHECK_INTERVAL`] apart. Unequal means real progress
    /// happened somewhere - a healthy pool clears this every cycle, so it is
    /// never grown for a burst it is already draining. Equal, with the
    /// queue still non-empty and no worker idle, means nothing has
    /// completed *at all* in that whole window; only then does one more
    /// worker get started, and the check repeats, so a program that needs
    /// several more workers gains them one interval at a time rather than
    /// all at once.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_starvation_watchdog(pool: &Arc<Self>) {
        let p = Arc::clone(pool);
        // A daemon thread: nothing joins it, and it exits only when the
        // process does, the same lifecycle every worker thread already has.
        let spawned = std::thread::Builder::new()
            .name("gossamer-pool-watchdog".to_string())
            .spawn(move || {
                loop {
                    let before = p.completions.load(Ordering::Acquire);
                    gossamer_runtime::platform::sleep(STARVATION_CHECK_INTERVAL);
                    let after = p.completions.load(Ordering::Acquire);
                    if after != before {
                        continue;
                    }
                    let queued = p.inner.lock().queue.len();
                    if queued == 0 || p.idle.load(Ordering::Relaxed) > 0 {
                        continue;
                    }
                    if p.workers.load(Ordering::Relaxed) < MAX_WORKERS {
                        Self::start_worker(&p);
                    } else {
                        warn_worker_ceiling_once();
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("gossamer-pool-watchdog spawn failed: {e}");
        }
    }

    /// Blocks until every queued / in-flight task has finished, or until
    /// the instant `until` answers passes. Called by
    /// [`join_outstanding_goroutines`] at program exit.
    ///
    /// A task can be outstanding without ever reaching a channel wait - a
    /// goroutine spinning on a computation, or parked in a blocking call -
    /// so the stuck-waiter count alone cannot decide when to stop waiting.
    /// The deadline is what makes the wait answer for every such task, and
    /// it is shared with the root cohort's own drain. A pool that is
    /// already settled returns without asking for one.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn drain_until(&self, until: impl FnOnce() -> gossamer_runtime::platform::Instant) {
        let mut inner = self.inner.lock();
        if self.drain_settled() {
            return;
        }
        let until = until();
        while !self.drain_settled() {
            if self.drain_cv.wait_until(&mut inner, until).timed_out() {
                return;
            }
        }
    }

    /// Whether waiting longer could still change what the pool holds:
    /// nothing outstanding, or every outstanding task already counted as
    /// permanently parked.
    #[cfg(not(target_arch = "wasm32"))]
    fn drain_settled(&self) -> bool {
        let outstanding = self.outstanding.load(Ordering::Acquire);
        outstanding == 0 || STUCK_WAITERS.load(Ordering::Acquire) >= outstanding
    }

    /// Wakes a drain that is waiting on progress this pool can no longer make.
    fn notify_drain(&self) {
        let _inner = self.inner.lock();
        self.drain_cv.notify_all();
    }

    /// wasm runs goroutines eagerly to completion in `spawn`, so there
    /// is never anything outstanding to drain at exit.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn drain_until(&self, _until: impl FnOnce() -> gossamer_runtime::platform::Instant) {
    }
}

static POOL: OnceLock<Arc<GoroutinePool>> = OnceLock::new();

const DEFAULT_MAX_WORKERS: usize = 4;
/// Ceiling on worker threads. A goroutine blocked in a channel operation
/// holds its worker, so this bounds how many may block at once. Each worker
/// reserves a 16 MiB stack, but the reservation is virtual and only the
/// pages actually used are committed, so the ceiling costs address space
/// rather than memory. Reaching it means the program has more
/// simultaneously-blocked goroutines than the host can back with threads.
const MAX_WORKERS: usize = 1024;

/// How long the starvation watchdog waits between progress checks. Short
/// enough that resolving a genuine deadlock is not a noticeable pause even
/// stacked several times over (a program needing five workers grows one
/// interval at a time); generous enough that ordinary scheduler noise on a
/// loaded machine does not read as zero progress. Not a correctness
/// deadline - the watchdog runs for as long as the process does, so a
/// program that is merely slow just waits one more interval before the next
/// check.
#[cfg(not(target_arch = "wasm32"))]
const STARVATION_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

/// Workers a parallel adapter spreads its leaves over: the configured pool
/// size when one is pinned, otherwise the machine's cores. The pool grows to
/// meet queued work, and a helper it has not started yet costs nothing, since
/// the caller takes any leaf no helper has claimed.
pub(crate) fn parallel_width() -> usize {
    if let Ok(raw) = std::env::var("GOSSAMER_VM_GOROUTINE_WORKERS")
        && let Ok(requested) = raw.parse::<usize>()
        && requested > 0
    {
        return requested.min(MAX_WORKERS);
    }
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

fn default_worker_count() -> usize {
    if let Ok(raw) = std::env::var("GOSSAMER_VM_GOROUTINE_WORKERS")
        && let Ok(requested) = raw.parse::<usize>()
        && requested > 0
    {
        return requested.min(MAX_WORKERS);
    }
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(DEFAULT_MAX_WORKERS)
        .min(DEFAULT_MAX_WORKERS)
}

/// Says once that the pool cannot grow further. A goroutine queued in this
/// state waits for a running one to finish; if those are all blocked on it,
/// the program stops making progress, and silence would leave nothing to
/// explain why.
#[cfg(not(target_arch = "wasm32"))]
fn warn_worker_ceiling_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "gossamer: {MAX_WORKERS} goroutine workers in use and all are busy; \
             further goroutines wait for one to finish"
        );
    });
}

/// Lazily-initialised process-wide goroutine pool. First call
/// builds the pool with the bounded default worker count.
pub(crate) fn pool() -> &'static Arc<GoroutinePool> {
    POOL.get_or_init(|| GoroutinePool::new(default_worker_count()))
}

/// The goroutine pool's counters, in the shape
/// `runtime::scheduler_stats_json` reports.
pub(crate) struct PoolStats {
    pub(crate) spawned: u64,
    pub(crate) finished: u64,
    pub(crate) injects: u64,
    pub(crate) live: u64,
    pub(crate) worker_count: usize,
    pub(crate) worker_count_cap: usize,
}

/// Snapshot of the pool that runs VM goroutines. The compiled tiers run
/// theirs on `MultiScheduler` and report its counters; this is the same
/// reading for the tier the interpreter actually schedules on. A program
/// that has never spawned has no pool, and reads as all zeroes.
pub(crate) fn pool_stats() -> PoolStats {
    let Some(pool) = POOL.get() else {
        return PoolStats {
            spawned: 0,
            finished: 0,
            injects: 0,
            live: 0,
            worker_count: 0,
            worker_count_cap: MAX_WORKERS,
        };
    };
    PoolStats {
        spawned: pool.spawned.load(Ordering::Acquire),
        finished: pool.completions.load(Ordering::Acquire),
        injects: pool.injects.load(Ordering::Acquire),
        live: pool.outstanding.load(Ordering::Acquire),
        worker_count: pool.workers.load(Ordering::Relaxed),
        worker_count_cap: MAX_WORKERS,
    }
}

/// Goroutine tasks queued or running. A task that has not started yet is
/// counted, so it always reads as able to make progress.
fn outstanding_goroutines() -> u64 {
    POOL.get()
        .map_or(0, |p| p.outstanding.load(Ordering::Acquire))
}

thread_local! {
    /// Set on every goroutine worker thread, so the deadlock report can tell
    /// a goroutine's own wait from the program's main thread.
    static ON_GOROUTINE_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The operation `main`'s thread is waiting in, if any. A deadlock report
/// names it whichever participant notices, since `main`'s wait is the one the
/// program is stopped at.
static MAIN_WAIT_OP: parking_lot::Mutex<Option<&'static str>> = parking_lot::Mutex::new(None);

/// Records `op` as `main`'s current wait when called on `main`'s thread.
fn note_main_wait(op: Option<&'static str>) {
    if !ON_GOROUTINE_WORKER.with(std::cell::Cell::get) {
        *MAIN_WAIT_OP.lock() = op;
    }
}

/// Threads currently suspended inside a channel wait.
static CHANNEL_WAITERS: AtomicUsize = AtomicUsize::new(0);

/// Threads currently suspended joining a cohort's children. A joiner waits
/// on other participants just as a channel waiter does, so it counts toward
/// the waits a deadlock needs.
static JOIN_WAITERS: AtomicUsize = AtomicUsize::new(0);

/// Channels holding a waiter whose operation would complete if it woke now -
/// a queued value with a receiver for it, or room for a blocked sender's.
/// A non-zero count means the program can still move even with every thread
/// inside a channel wait.
static PENDING_HANDOFFS: AtomicUsize = AtomicUsize::new(0);

/// Adds or removes one channel from the ready count.
pub fn adjust_pending_handoffs(ready: bool) {
    if ready {
        PENDING_HANDOFFS.fetch_add(1, Ordering::AcqRel);
    } else {
        PENDING_HANDOFFS.fetch_sub(1, Ordering::AcqRel);
    }
    note_progress();
}

/// Advances every time one of the deadlock inputs changes: a waiter leaves
/// its wait, a channel's readiness moves, or the outstanding set changes.
static PROGRESS_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Records that a deadlock input has moved.
fn note_progress() {
    PROGRESS_EPOCH.fetch_add(1, Ordering::AcqRel);
}

/// Whether the sampled counts describe a state nothing left in the program
/// can move: every participant inside a channel wait, no channel holding a
/// handoff, and no channel able to complete one.
///
/// `epoch` is the [`PROGRESS_EPOCH`] reading taken before the counts. A
/// different reading here means a waiter left, a readiness moved, or the
/// outstanding set changed while the counts were being read, so they describe
/// separate states and the caller waits rather than reporting.
fn reads_as_terminal(
    waiting: u64,
    participants: u64,
    epoch: u64,
    can_progress: impl FnOnce() -> bool,
) -> bool {
    waiting >= participants
        && PENDING_HANDOFFS.load(Ordering::Acquire) == 0
        && !can_progress()
        && PROGRESS_EPOCH.load(Ordering::Acquire) == epoch
}

/// Marks its thread as suspended in a channel wait for as long as it lives.
pub(crate) struct ChannelWait {
    /// Whether this wait was counted among the waits nothing can satisfy.
    stuck: bool,
}

impl ChannelWait {
    /// Enters a channel wait, reporting a deadlock when doing so leaves
    /// nothing in the program able to run.
    ///
    /// Participants are the main thread plus every queued or running
    /// goroutine task. A task waiting on a timer, on I/O, or on a blocking
    /// call is outstanding without being a channel waiter, so it keeps the
    /// count short and the program reads as able to progress - which it is.
    ///
    /// Returns `None` when every participant is waiting on a channel and no
    /// channel holds a waiter that could proceed. `can_progress` recomputes
    /// that second condition from the channels themselves, so a counter left
    /// behind by an interleaving cannot turn a working program into a
    /// failure. Nothing can deliver a
    /// value in that state, so waiting longer cannot change the answer and
    /// the caller reports a deadlock.
    ///
    /// Call with the channel's own lock held and the caller already counted
    /// among that channel's waiters, so a handoff this caller completes is
    /// visible to the readiness count.
    pub(crate) fn enter(op: &'static str, can_progress: impl FnOnce() -> bool) -> Option<Self> {
        // The browser settles every goroutine at its spawn, so by the time a
        // wait is entered there every sender that will ever run has run.
        if !gossamer_runtime::platform::CAN_BLOCK {
            return None;
        }
        // Recorded before this wait is counted, so a participant that sees
        // the count already sees which operation `main` is stopped at.
        note_main_wait(Some(op));
        // Sampled before the counts: a waiter drops its count one step ahead
        // of retiring the readiness that woke it, so counts read across such a
        // step belong to two different states. `reads_as_terminal` compares
        // this reading again at the end and stands only on a window nothing
        // moved in.
        let epoch = PROGRESS_EPOCH.load(Ordering::Acquire);
        let waiting = CHANNEL_WAITERS.fetch_add(1, Ordering::AcqRel)
            + 1
            + JOIN_WAITERS.load(Ordering::Acquire);
        // The program's participants are every outstanding goroutine plus
        // `main` while it is still running. All of them waiting on channels
        // that can hand nothing over means nothing is left to deliver a
        // value, so waiting longer cannot change the answer.
        let main_returned = MAIN_RETURNED.load(Ordering::Acquire);
        let participants = outstanding_goroutines() + u64::from(!main_returned);
        let stuck = reads_as_terminal(waiting as u64, participants, epoch, can_progress);
        // Past `main`, the remaining goroutines are abandoned rather than
        // reported: a compiled binary exits the same way, with the same
        // status and the same output.
        if stuck && main_returned {
            STUCK_WAITERS.fetch_add(1, Ordering::AcqRel);
            if let Some(pool) = POOL.get() {
                pool.notify_drain();
            }
            return Some(Self { stuck: true });
        }
        if stuck {
            note_main_wait(None);
            CHANNEL_WAITERS.fetch_sub(1, Ordering::AcqRel);
            // A deadlock is a property of the whole program, not of the
            // goroutine that happens to notice. Ending only this goroutine
            // would leave the others asleep with nothing left to wake them,
            // so a worker reports for the program and stops it; the main
            // thread returns instead, and its error carries a call stack.
            if ON_GOROUTINE_WORKER.with(std::cell::Cell::get) {
                report_fatal_deadlock(op);
            }
            return None;
        }
        Some(Self { stuck: false })
    }
}

/// Prints the deadlock report and stops the program, as a panic on `main`'s
/// thread does: the same line and the same exit code.
fn report_fatal_deadlock(op: &str) -> ! {
    use std::io::Write as _;
    let op = MAIN_WAIT_OP.lock().unwrap_or(op);
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "error[GX0005]: panic: all goroutines are asleep - deadlock! ({op} can never complete)"
    );
    let _ = err.flush();
    std::process::exit(101);
}

/// Marks its thread as suspended joining a cohort for as long as it lives.
pub(crate) struct JoinWait;

impl JoinWait {
    /// Enters a cohort join, reporting a deadlock when doing so leaves every
    /// participant waiting with no channel able to hand anything over: the
    /// children the join waits for are then waiting too, and nothing is left
    /// to finish them.
    ///
    /// Returns `None` on `main`'s thread in that state, for the caller to
    /// report with its call stack; a goroutine worker stops the program.
    pub(crate) fn enter() -> Option<Self> {
        if !gossamer_runtime::platform::CAN_BLOCK {
            return Some(Self);
        }
        note_main_wait(Some("cohort join"));
        let epoch = PROGRESS_EPOCH.load(Ordering::Acquire);
        let waiting = JOIN_WAITERS.fetch_add(1, Ordering::AcqRel)
            + 1
            + CHANNEL_WAITERS.load(Ordering::Acquire);
        let main_returned = MAIN_RETURNED.load(Ordering::Acquire);
        let participants = outstanding_goroutines() + u64::from(!main_returned);
        // Past `main`, leftover goroutines are abandoned rather than reported,
        // as a compiled binary leaves them.
        if !main_returned && reads_as_terminal(waiting as u64, participants, epoch, || false) {
            note_main_wait(None);
            JOIN_WAITERS.fetch_sub(1, Ordering::AcqRel);
            note_progress();
            if ON_GOROUTINE_WORKER.with(std::cell::Cell::get) {
                report_fatal_deadlock("cohort join");
            }
            return None;
        }
        Some(Self)
    }
}

impl Drop for JoinWait {
    fn drop(&mut self) {
        note_main_wait(None);
        JOIN_WAITERS.fetch_sub(1, Ordering::AcqRel);
        note_progress();
    }
}

impl Drop for ChannelWait {
    fn drop(&mut self) {
        note_main_wait(None);
        if self.stuck {
            STUCK_WAITERS.fetch_sub(1, Ordering::AcqRel);
        }
        CHANNEL_WAITERS.fetch_sub(1, Ordering::AcqRel);
        note_progress();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gossamer_runtime::platform::Instant;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A deadline past every timeout these tests set, so a drain that
    /// returns did so because the work finished.
    fn far_future() -> Instant {
        Instant::now() + Duration::from_mins(10)
    }

    /// The counts a report rests on are read one at a time, so a program that
    /// moved between two of them was never in the state they add up to.
    #[test]
    fn counts_read_across_a_change_do_not_read_as_terminal() {
        let stale = PROGRESS_EPOCH.load(Ordering::Acquire);
        note_progress();
        assert!(
            !reads_as_terminal(u64::MAX, 0, stale, || false),
            "a wait whose counts span a change is not a program with nothing left to run"
        );
    }

    /// The reading is what makes a quiet window quiet: with it unchanged, the
    /// counts are one state and a program with every participant parked and
    /// nothing to hand over is reported.
    #[test]
    fn quiet_counts_read_as_terminal() {
        let epoch = PROGRESS_EPOCH.load(Ordering::Acquire);
        let quiet_handoffs = PENDING_HANDOFFS.load(Ordering::Acquire) == 0;
        let terminal = reads_as_terminal(u64::MAX, 0, epoch, || false);
        assert_eq!(
            terminal,
            quiet_handoffs && PROGRESS_EPOCH.load(Ordering::Acquire) == epoch,
            "a window nothing moved in decides on its counts alone"
        );
    }

    /// A task that blocks holds its worker, so a pool that could not grow
    /// would never start the task whose completion releases it.
    #[test]
    fn blocked_tasks_do_not_starve_a_queued_task() {
        let pool = GoroutinePool::new(1);
        let (blocked_tx, blocked_rx) = mpsc::channel::<()>();
        // Occupy every initial worker, and then some, with tasks that cannot
        // finish until released below.
        let mut releases = Vec::new();
        for _ in 0..4 {
            let blocked = blocked_tx.clone();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            releases.push(release_tx);
            GoroutinePool::spawn(
                &pool,
                Box::new(move || {
                    blocked.send(()).ok();
                    release_rx.recv().ok();
                }),
            );
        }
        for _ in 0..4 {
            blocked_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("every blocking task started");
        }
        let (done_tx, done_rx) = mpsc::channel::<()>();
        GoroutinePool::spawn(
            &pool,
            Box::new(move || {
                done_tx.send(()).ok();
            }),
        );
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("queued task ran while the others were blocked");
        for release in releases {
            release.send(()).ok();
        }
        pool.drain_until(far_future);
    }

    #[test]
    fn panicking_task_does_not_strand_drain() {
        let pool = GoroutinePool::new(1);
        let ran = Arc::new(AtomicBool::new(false));
        GoroutinePool::spawn(&pool, Box::new(|| panic!("intentional worker panic")));
        let ran_after_panic = Arc::clone(&ran);
        GoroutinePool::spawn(
            &pool,
            Box::new(move || {
                ran_after_panic.store(true, Ordering::Release);
            }),
        );

        // Use `drain` itself to observe completion. Polling the atomic can
        // spuriously time out when another test temporarily starves this
        // private worker; the condition-variable handoff is the liveness
        // guarantee this regression is meant to exercise. A bounded helper
        // prevents a future lost wakeup from hanging the entire test process.
        let drain_pool = Arc::clone(&pool);
        let (drained_tx, drained_rx) = mpsc::channel();
        let drainer = std::thread::spawn(move || {
            drain_pool.drain_until(far_future);
            drained_tx.send(()).expect("test receiver remains live");
        });
        drained_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("drain must settle panicking and queued tasks");
        drainer.join().expect("drain helper must not panic");
        assert_eq!(pool.outstanding.load(Ordering::Acquire), 0);
        assert!(ran.load(Ordering::Acquire));
    }

    #[test]
    fn default_worker_count_is_bounded() {
        assert!((1..=DEFAULT_MAX_WORKERS).contains(&default_worker_count()));
    }

    #[test]
    fn drain_cannot_miss_a_last_task_completion() {
        let pool = GoroutinePool::new(1);
        // Repeat the check because the old implementation's lost wakeup
        // depended on a narrow scheduling window.  Each task waits until the
        // drainer is armed, then becomes the last outstanding task.
        for _ in 0..128 {
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            GoroutinePool::spawn(
                &pool,
                Box::new(move || {
                    started_tx.send(()).expect("test receiver remains live");
                    release_rx.recv().expect("test release remains live");
                }),
            );
            started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("worker starts the task");

            let drain_pool = Arc::clone(&pool);
            let (drained_tx, drained_rx) = mpsc::channel();
            std::thread::spawn(move || {
                drain_pool.drain_until(far_future);
                drained_tx.send(()).expect("test receiver remains live");
            });
            release_tx.send(()).expect("worker remains live");
            drained_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("drain must observe the final completion");
        }
    }
}
