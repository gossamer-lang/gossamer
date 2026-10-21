//! Goroutine worker pool backing the bytecode VM's `Op::Spawn`.

use std::collections::BTreeMap;
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

/// A goroutine waiting for a worker, with the identity of the join handle
/// its outcome reaches (0 when nothing can join it).
struct QueuedTask {
    handle: usize,
    task: GoroutineTask,
}

/// Runs one goroutine task to its end on the calling thread.
fn run_task(task: GoroutineTask) {
    // A task must settle the outstanding counter even when an implementation
    // panic escapes its VM boundary, or program exit would wait forever after
    // main has printed all of its output. User panics are converted to
    // RuntimeError by `spawn_goroutine_native`; this catch is the final
    // containment for host panics.
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)).is_err() {
        eprintln!("gossamer: goroutine worker panicked");
    }
}

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
/// At most `target` goroutines run at once. A worker that parks in a
/// channel, lock, or join wait gives up its slot, and a fresh worker is
/// started when no idle one can take it, as Go hands a blocked thread's
/// processor to another. A background watchdog raises `target`, up to
/// [`MAX_WORKERS`], only when the pool has made zero progress for a full
/// [`STARVATION_CHECK_INTERVAL`] while saturated - see
/// [`GoroutinePool::start_starvation_watchdog`] for why that growth is
/// reactive rather than triggered by `spawn` itself.
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
    /// Queued goroutines keyed by spawn order, so workers take the oldest
    /// first and a joiner can take its own child from anywhere in line.
    queue: BTreeMap<u64, QueuedTask>,
    /// The key the next queued goroutine takes.
    next_seq: u64,
    /// The queue key of each queued goroutine that has a join handle.
    by_handle: rustc_hash::FxHashMap<usize, u64>,
    /// `true` once the runtime is shutting down; workers exit
    /// once `queue` drains. Currently never set in practice
    /// (the process exits right after `drain()` returns), but
    /// kept for cleanliness.
    shutting_down: bool,
    /// Workers executing a goroutine and not parked in a wait. A worker takes
    /// a queued task only while this is below `target`, so goroutines share
    /// `target` threads' worth of the machine however many workers exist.
    running: usize,
    /// Workers parked in a channel, lock, or join wait. A parked worker holds
    /// its thread but runs nothing, so its slot passes to another worker.
    parked: usize,
    /// How many goroutines run at once: the starting worker count, raised by
    /// the starvation watchdog when goroutines block outside a counted wait.
    target: usize,
}

impl PoolInner {
    fn push(&mut self, handle: usize, task: GoroutineTask) {
        let seq = self.next_seq;
        self.next_seq += 1;
        if handle != 0 {
            self.by_handle.insert(handle, seq);
        }
        self.queue.insert(seq, QueuedTask { handle, task });
    }

    /// Takes the longest-queued goroutine.
    fn pop_oldest(&mut self) -> Option<GoroutineTask> {
        let (_, queued) = self.queue.pop_first()?;
        if queued.handle != 0 {
            self.by_handle.remove(&queued.handle);
        }
        Some(queued.task)
    }

    /// Takes the goroutine behind join handle `handle` while no worker has.
    fn take_joinable(&mut self, handle: usize) -> Option<GoroutineTask> {
        let seq = self.by_handle.remove(&handle)?;
        self.queue.remove(&seq).map(|queued| queued.task)
    }
}

impl GoroutinePool {
    fn new(num_workers: usize) -> Arc<Self> {
        let pool = Arc::new(Self {
            inner: parking_lot::Mutex::new(PoolInner {
                queue: BTreeMap::new(),
                next_seq: 0,
                by_handle: rustc_hash::FxHashMap::default(),
                shutting_down: false,
                running: 0,
                parked: 0,
                target: num_workers,
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
                // Each task is a goroutine, so a fault native code raises here
                // ends that goroutine, as the bytecode's own panics do.
                let _faults = gossamer_runtime::c_abi::panic::IsolatedFaults::enter();
                loop {
                    let task = {
                        let mut inner = p.inner.lock();
                        loop {
                            if inner.running < inner.target
                                && let Some(task) = inner.pop_oldest()
                            {
                                inner.running += 1;
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
                            run_task(task);
                            p.settle_task(true);
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
        Self::spawn_joinable(pool, 0, task);
    }

    /// [`spawn`](Self::spawn) for a goroutine whose outcome reaches the join
    /// handle with identity `handle`, so [`run_queued_inline`] can find it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn spawn_joinable(pool: &Arc<Self>, handle: usize, task: GoroutineTask) {
        let mut inner = pool.inner.lock();
        // Publish the counter and task under the same mutex used by
        // `drain()`, so a drain cannot observe an empty program while a
        // concurrent goroutine spawn is about to enqueue work.
        pool.outstanding.fetch_add(1, Ordering::AcqRel);
        pool.spawned.fetch_add(1, Ordering::Relaxed);
        note_progress();
        inner.push(handle, task);
        Self::admit_queued(pool, &inner);
    }

    /// Settles one finished task: counts the completion, and frees the
    /// running slot when `held_slot` says the task took one of its own.
    fn settle_task(&self, held_slot: bool) {
        // A panic still counts as progress: the thread is free again either
        // way, which is all the starvation watchdog needs to know.
        self.completions.fetch_add(1, Ordering::AcqRel);
        // `drain()` checks `outstanding` while holding `inner` and then waits
        // on `drain_cv`. Update the predicate under that same mutex: notifying
        // without it admits the classic lost-wakeup race where a completed
        // task signals between the check and the Condvar wait, stranding
        // program shutdown after all user output is printed.
        let mut inner = self.inner.lock();
        if held_slot {
            inner.running -= 1;
        }
        let prev = self.outstanding.fetch_sub(1, Ordering::AcqRel);
        note_progress();
        if prev == 1 {
            // Last in-flight task settled - wake any drain() waiter.
            self.drain_cv.notify_all();
        }
        drop(inner);
        report_if_left_waiting();
    }

    /// Hands a queued goroutine to a worker when fewer than `target` run:
    /// wakes an idle worker, or starts one when every worker that is not
    /// parked is already taken. A goroutine that joins its children parks
    /// its worker, so without this handoff a fork-join program waits on
    /// children queued behind their own parents. Call with `inner` locked,
    /// so the queue, the counts, and `idle` are one reading.
    #[cfg(not(target_arch = "wasm32"))]
    fn admit_queued(pool: &Arc<Self>, inner: &PoolInner) {
        if inner.queue.is_empty() || inner.running >= inner.target {
            return;
        }
        let workers = pool.workers.load(Ordering::Relaxed);
        if pool.idle.load(Ordering::Relaxed) > 0 {
            pool.cv.notify_one();
        } else if workers - inner.parked < inner.target && workers < MAX_WORKERS {
            Self::start_worker(pool);
        }
    }

    /// wasm runs every goroutine at its spawn, so no worker ever parks.
    #[cfg(target_arch = "wasm32")]
    fn admit_queued(_pool: &Arc<Self>, _inner: &PoolInner) {}

    /// Single-threaded wasm: run the goroutine body to completion
    /// immediately. A body that tries to block reaches
    /// `gossamer_coro::suspend`, which panics with the documented
    /// "blocking not supported in the playground" message.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn spawn_joinable(pool: &Arc<Self>, _handle: usize, task: GoroutineTask) {
        Self::spawn(pool, task);
    }

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
    /// A goroutine blocked outside a counted wait (a sleep, a blocking call,
    /// a foreign lock) keeps its slot for the duration, so a fixed `target`
    /// can starve: every running goroutine waiting on one still queued, the
    /// only one that could release them, which no slot is ever free to run.
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
    /// queue still non-empty and every slot taken, means nothing has
    /// completed *at all* in that whole window; only then does one more
    /// goroutine get to run, and the check repeats, so a program that needs
    /// several more slots gains them one interval at a time rather than
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
                    let mut inner = p.inner.lock();
                    if inner.queue.is_empty() {
                        continue;
                    }
                    // Every running goroutine is blocked outside a counted
                    // wait, so one more may run beside them.
                    if inner.running >= inner.target {
                        if inner.target >= MAX_WORKERS {
                            warn_worker_ceiling_once();
                            continue;
                        }
                        inner.target += 1;
                    }
                    Self::admit_queued(&p, &inner);
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

/// The participants other than `main` that can still deliver a value: every
/// outstanding goroutine, plus each actor outside the goroutine set (a live
/// `ffi::Handle` whose holder may run a callback on a thread of its own).
fn participants_besides_main() -> u64 {
    outstanding_goroutines() + gossamer_runtime::sched_global::external_actors() as u64
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

/// The cohort of each thread inside a channel wait, one entry per waiter.
/// Cancelling a cohort wakes its waiters, and each leaves its wait with
/// `None`; until it runs it is still counted among the waiters, so a waiter
/// whose cohort is cancelled marks a program that can still move.
static WAITING_COHORTS: parking_lot::Mutex<Vec<i64>> = parking_lot::Mutex::new(Vec::new());

/// Whether a thread inside a channel wait belongs to a cancelled cohort, and
/// so is about to leave it.
fn a_waiter_is_cancelled() -> bool {
    any_cancelled(&WAITING_COHORTS.lock())
}

/// Whether any of `cohorts` is cancelled, directly or through an enclosing
/// cohort.
fn any_cancelled(cohorts: &[i64]) -> bool {
    cohorts
        .iter()
        .any(|&id| id != 0 && crate::stdlib_builtins::cohort::chain_is_cancelled(id))
}

/// Counts the calling thread as parked when it is a pool worker, handing its
/// slot to a fresh worker if queued goroutines need one. Answers whether the
/// thread was counted, for [`unpark_worker`].
fn park_worker() -> bool {
    if !ON_GOROUTINE_WORKER.with(std::cell::Cell::get) {
        return false;
    }
    let Some(pool) = POOL.get() else {
        return false;
    };
    let mut inner = pool.inner.lock();
    inner.running -= 1;
    inner.parked += 1;
    GoroutinePool::admit_queued(pool, &inner);
    true
}

/// Runs the goroutine behind join handle `handle` on the calling worker when
/// no worker has taken it yet, as a fork-join scheduler runs the child its
/// parent is about to wait on. A joiner that parked instead would hold its
/// thread while the child queued behind other parked parents, so a wide tree
/// would need a thread per directory of it. Answers whether the goroutine ran
/// here; `main` holds no worker, so it waits as before.
pub(crate) fn run_queued_inline(handle: usize) -> bool {
    if !ON_GOROUTINE_WORKER.with(std::cell::Cell::get) {
        return false;
    }
    let Some(pool) = POOL.get() else {
        return false;
    };
    let Some(task) = pool.inner.lock().take_joinable(handle) else {
        return false;
    };
    pool.injects.fetch_add(1, Ordering::Relaxed);
    crate::stdlib_builtins::cohort::nested(|| run_task(task));
    // The joining task already holds this thread's slot.
    pool.settle_task(false);
    true
}

/// Ends the parked count [`park_worker`] started, when it counted one.
fn unpark_worker(counted: bool) {
    if counted && let Some(pool) = POOL.get() {
        let mut inner = pool.inner.lock();
        inner.parked -= 1;
        inner.running += 1;
    }
}

fn forget_waiting_cohort(id: i64) {
    let mut waiting = WAITING_COHORTS.lock();
    if let Some(at) = waiting.iter().position(|&entry| entry == id) {
        waiting.swap_remove(at);
    }
}

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

/// Whether the program is in a state nothing left in it can move, read as one
/// consistent snapshot: `counts` answers `(waiting, participants)`.
///
/// The counts are read one at a time, so a reading that spans a change of
/// [`PROGRESS_EPOCH`] mixes two states. A reading that is not terminal needs
/// no second look, since whoever moves the program next checks again; a
/// terminal reading that spans a change is read again, because the change
/// may have been the one that completed it, and the participant that made it
/// may have checked before this caller was counted.
fn settles_terminal(counts: impl Fn() -> (u64, u64), can_progress: impl Fn() -> bool) -> bool {
    loop {
        let epoch = PROGRESS_EPOCH.load(Ordering::Acquire);
        let (waiting, participants) = counts();
        let terminal = waiting >= participants
            && PENDING_HANDOFFS.load(Ordering::Acquire) == 0
            && !can_progress();
        if !terminal {
            return false;
        }
        if PROGRESS_EPOCH.load(Ordering::Acquire) == epoch {
            return true;
        }
    }
}

/// Marks its thread as suspended in a channel wait for as long as it lives.
pub(crate) struct ChannelWait {
    /// Whether this wait was counted among the waits nothing can satisfy.
    stuck: bool,
    /// The waiting thread's cohort, as recorded in [`WAITING_COHORTS`].
    cohort: i64,
    /// Whether the waiting thread was counted as a parked pool worker.
    parked: bool,
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
    /// channel holds a waiter that could proceed. Readiness is the count each
    /// channel and lock keeps in step under its own lock, never a probe of
    /// another thread's lock, so every reading is one the next participant to
    /// move reads again; `can_progress` adds what the count does not carry,
    /// such as a pending deadline. Nothing can deliver a value in that state,
    /// so waiting longer cannot change the answer and the caller reports a
    /// deadlock.
    ///
    /// Call with the channel's own lock held and the caller already counted
    /// among that channel's waiters, so a handoff this caller completes is
    /// visible to the readiness count.
    pub(crate) fn enter(op: &'static str, can_progress: impl Fn() -> bool) -> Option<Self> {
        // The browser settles every goroutine at its spawn, so by the time a
        // wait is entered there every sender that will ever run has run.
        if !gossamer_runtime::platform::CAN_BLOCK {
            return None;
        }
        // Recorded before this wait is counted, so a participant that sees
        // the count already sees which operation `main` is stopped at.
        note_main_wait(Some(op));
        let cohort = crate::stdlib_builtins::cohort::current_cohort();
        WAITING_COHORTS.lock().push(cohort);
        CHANNEL_WAITERS.fetch_add(1, Ordering::AcqRel);
        // The program's participants are every outstanding goroutine plus
        // `main` while it is still running. All of them waiting on channels
        // that can hand nothing over means nothing is left to deliver a
        // value, so waiting longer cannot change the answer. A waiter drops
        // its count one step ahead of retiring the readiness that woke it, so
        // the counts are read as one snapshot.
        let main_returned = MAIN_RETURNED.load(Ordering::Acquire);
        let stuck = settles_terminal(
            || {
                let waiting =
                    CHANNEL_WAITERS.load(Ordering::Acquire) + JOIN_WAITERS.load(Ordering::Acquire);
                let participants = participants_besides_main() + u64::from(!main_returned);
                (waiting as u64, participants)
            },
            || a_waiter_is_cancelled() || can_progress(),
        );
        // Past `main`, the remaining goroutines are abandoned rather than
        // reported: a compiled binary exits the same way, with the same
        // status and the same output.
        if stuck && main_returned {
            STUCK_WAITERS.fetch_add(1, Ordering::AcqRel);
            if let Some(pool) = POOL.get() {
                pool.notify_drain();
            }
            return Some(Self {
                stuck: true,
                cohort,
                parked: park_worker(),
            });
        }
        if stuck {
            note_main_wait(None);
            forget_waiting_cohort(cohort);
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
        Some(Self {
            stuck: false,
            cohort,
            parked: park_worker(),
        })
    }
}

/// Reports a deadlock when a goroutine that just finished was the last
/// participant not waiting.
///
/// A wait checks for a deadlock as it is entered, so the program is judged
/// again at every change that could leave it with nothing able to run. A
/// goroutine that finishes shrinks the participants without entering a wait,
/// and the ones already waiting would never look again.
fn report_if_left_waiting() {
    if !gossamer_runtime::platform::CAN_BLOCK || MAIN_RETURNED.load(Ordering::Acquire) {
        return;
    }
    if CHANNEL_WAITERS.load(Ordering::Acquire) + JOIN_WAITERS.load(Ordering::Acquire) == 0 {
        return;
    }
    let terminal = settles_terminal(
        || {
            let waiting =
                CHANNEL_WAITERS.load(Ordering::Acquire) + JOIN_WAITERS.load(Ordering::Acquire);
            (waiting as u64, participants_besides_main() + 1)
        },
        || {
            a_waiter_is_cancelled()
                || crate::stdlib_builtins::context::deadline_pending()
                || crate::stdlib_builtins::cohort::deadline_pending()
        },
    );
    if terminal {
        report_fatal_deadlock("receive");
    }
}

/// Prints the deadlock report and stops the program, as a panic on `main`'s
/// thread does: the same line and the same exit code.
fn report_fatal_deadlock(op: &str) -> ! {
    use std::io::Write as _;
    let op = MAIN_WAIT_OP.lock().unwrap_or(op);
    let mut err = std::io::stderr();
    crate::run_exit_hooks();
    let _ = writeln!(
        err,
        "error[GX0005]: panic: all goroutines are asleep - deadlock! ({op} can never complete)"
    );
    let _ = err.flush();
    std::process::exit(101);
}

/// Marks its thread as suspended joining a cohort for as long as it lives.
pub(crate) struct JoinWait {
    /// Whether the joining thread was counted as a parked pool worker.
    parked: bool,
}

impl JoinWait {
    /// Enters a cohort join, reporting a deadlock when doing so leaves every
    /// participant waiting with no channel able to hand anything over: the
    /// children the join waits for are then waiting too, and nothing is left
    /// to finish them.
    ///
    /// Returns `None` on `main`'s thread in that state, for the caller to
    /// report with its call stack; a goroutine worker stops the program.
    ///
    /// `settled` answers whether the join has nothing left to wait for. It
    /// is read after the counts, so a last child that finished between the
    /// caller's own check and this one is never read as a deadlock.
    pub(crate) fn enter(settled: impl Fn() -> bool) -> Option<Self> {
        if !gossamer_runtime::platform::CAN_BLOCK {
            return Some(Self { parked: false });
        }
        note_main_wait(Some("cohort join"));
        JOIN_WAITERS.fetch_add(1, Ordering::AcqRel);
        let main_returned = MAIN_RETURNED.load(Ordering::Acquire);
        // Past `main`, leftover goroutines are abandoned rather than reported,
        // as a compiled binary leaves them.
        if !main_returned
            && settles_terminal(
                || {
                    let waiting = JOIN_WAITERS.load(Ordering::Acquire)
                        + CHANNEL_WAITERS.load(Ordering::Acquire);
                    (waiting as u64, participants_besides_main() + 1)
                },
                || a_waiter_is_cancelled() || settled(),
            )
        {
            note_main_wait(None);
            JOIN_WAITERS.fetch_sub(1, Ordering::AcqRel);
            note_progress();
            if ON_GOROUTINE_WORKER.with(std::cell::Cell::get) {
                report_fatal_deadlock("cohort join");
            }
            return None;
        }
        Some(Self {
            parked: park_worker(),
        })
    }
}

impl Drop for JoinWait {
    fn drop(&mut self) {
        unpark_worker(self.parked);
        note_main_wait(None);
        JOIN_WAITERS.fetch_sub(1, Ordering::AcqRel);
        note_progress();
    }
}

impl Drop for ChannelWait {
    fn drop(&mut self) {
        unpark_worker(self.parked);
        note_main_wait(None);
        forget_waiting_cohort(self.cohort);
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

    #[test]
    fn a_waiter_in_a_cancelled_cohort_marks_a_program_that_can_move() {
        let open = crate::stdlib_builtins::cohort::cohort_for_test(false);
        let cancelled = crate::stdlib_builtins::cohort::cohort_for_test(true);
        assert!(!any_cancelled(&[0, open]));
        assert!(any_cancelled(&[open, cancelled]));
    }

    /// A deadline past every timeout these tests set, so a drain that
    /// returns did so because the work finished.
    fn far_future() -> Instant {
        Instant::now() + Duration::from_mins(10)
    }

    /// A terminal reading that spans a change is read again rather than
    /// given up on: the change may be the one that completed the state, made
    /// by a participant that checked before this one was counted.
    #[test]
    fn a_terminal_reading_across_a_change_is_read_again() {
        let reads = std::cell::Cell::new(0);
        let quiet_handoffs = PENDING_HANDOFFS.load(Ordering::Acquire) == 0;
        let terminal = settles_terminal(
            || {
                reads.set(reads.get() + 1);
                if reads.get() == 1 {
                    note_progress();
                }
                (u64::MAX, 0)
            },
            || false,
        );
        assert_eq!(terminal, quiet_handoffs);
        assert!(
            reads.get() >= 2,
            "the reading that spanned a change was read again"
        );
    }

    /// A reading that is not terminal is final: whoever moves the program
    /// next checks it again.
    #[test]
    fn a_reading_with_a_participant_running_is_not_terminal() {
        assert!(!settles_terminal(|| (0, 1), || false));
    }

    /// A last child can finish between a join's own check and the counts it
    /// reads, leaving the joiner as the only participant; the join has then
    /// nothing to wait for, which is not a deadlock.
    #[test]
    fn a_join_whose_children_have_settled_is_not_a_deadlock() {
        assert!(JoinWait::enter(|| true).is_some());
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
