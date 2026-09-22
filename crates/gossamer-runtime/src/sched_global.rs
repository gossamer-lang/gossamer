//! Process-wide M:N scheduler + netpoller singleton owned by the
//! runtime crate.
//!
//! Every compiled Gossamer binary links `gossamer-runtime` as a
//! `staticlib`, so the singleton ships with the program automatically
//! - no extra registration step is required from user code.
//!
//! Boot ordering:
//!
//! 1. The first call to [`scheduler`] / [`with_poller`] / [`add_timer`]
//!    constructs the [`MultiScheduler`] sized at `runtime::max_procs()`
//!    and an `OsPoller` (mio epoll/kqueue/IOCP).
//! 2. A dedicated `gos-netpoller` OS thread starts and, in a tight
//!    loop, blocks on `OsPoller::poll`, then dispatches every
//!    delivered `Readiness` by `unpark`-ing the goroutine that
//!    registered for that event.
//! 3. Compiled `go fn(args)` lands here through [`spawn`], which
//!    constructs a real [`gossamer_coro::Goroutine`] (stackful
//!    coroutine) and pushes it onto the work-stealing pool.
//!
//! Goroutines are stackful coroutines. When user code blocks on a
//! channel, mutex, sleep, or socket, [`park`] suspends the
//! coroutine - the worker thread immediately picks up the next
//! runnable goroutine instead of being held hostage by the OS-level
//! block. The wakeup source (poller, channel queue, mutex release)
//! calls [`MultiScheduler::unpark`] when ready.

use std::io;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;

use crate::platform::Instant;
use crate::sched::{Gid, MultiScheduler, OsPoller, ParkReason, Poller, Readiness, Step};

/// Wakers registered with the global poller. Keyed by [`Gid`], the
/// closure invoked when the poller delivers a readiness for that gid.
type WakerMap = std::collections::HashMap<Gid, Box<dyn Fn() + Send + Sync>>;

struct Globals {
    scheduler: MultiScheduler,
    poller: Mutex<OsPoller>,
    /// Shared interrupt handle for the netpoller thread. Held
    /// outside the `poller` mutex so registration paths can wake
    /// the running `poll()` without contending for the lock. Set
    /// once during `globals()` init; never replaced.
    poller_interrupt: std::sync::Arc<mio::Waker>,
    wakers: Mutex<WakerMap>,
    /// Monotonic gid allocator handed out for park/unpark purposes
    /// outside of `MultiScheduler::spawn` (timers, signal handlers,
    /// blocking thread pool callbacks).
    gid_alloc: AtomicU64,
    /// Set after the poller thread has been started.
    poller_started: AtomicBool,
    /// Set by [`request_shutdown`] to ask the poller loop to exit
    /// cleanly so the runtime can flush in-flight I/O before
    /// `gos_rt_exit`. Without this signal the poller thread was
    /// killed mid-`poll()` by `std::process::exit`, sending RST
    /// (not FIN) on any TCP connections in the OS kernel's send
    /// buffer.
    poller_shutdown: AtomicBool,
}

static GLOBALS: OnceLock<Globals> = OnceLock::new();

fn globals() -> &'static Globals {
    GLOBALS.get_or_init(|| {
        let workers = default_workers();
        let scheduler = MultiScheduler::new(workers);
        let os_poller = OsPoller::new().expect("OsPoller::new");
        let poller_interrupt = os_poller.interrupt_handle();
        let poller = Mutex::new(os_poller);
        scheduler.start();
        Globals {
            scheduler,
            poller,
            poller_interrupt,
            wakers: Mutex::new(WakerMap::new()),
            gid_alloc: AtomicU64::new(1_000_000),
            poller_started: AtomicBool::new(false),
            poller_shutdown: AtomicBool::new(false),
        }
    })
}

fn default_workers() -> usize {
    if let Ok(s) = std::env::var("GOSSAMER_MAX_PROCS") {
        if let Ok(n) = s.parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// Returns a handle to the process-wide scheduler, booting it on first use.
#[must_use]
pub fn scheduler() -> &'static MultiScheduler {
    &globals().scheduler
}

fn ensure_poller_thread(g: &'static Globals) {
    if g.poller_started.swap(true, Ordering::AcqRel) {
        return;
    }
    thread::Builder::new()
        .name("gos-netpoller".to_string())
        .spawn(poller_loop)
        .expect("spawn netpoller thread");
}

fn poller_loop() {
    let g = globals();
    let driver = g.poller.lock().driver_handle();
    loop {
        if g.poller_shutdown.load(Ordering::Acquire) {
            break;
        }
        // The wait holds only the kernel handle, never the bookkeeping, so a
        // registrar is never queued behind it. A registration made after the
        // timeout is read wakes the handle, and that wake stays pending until
        // the wait below consumes it, so none is lost.
        let timeout = g.poller.lock().timer_timeout();
        // An interrupted or refused wait reports nothing; timers still settle.
        let events = driver.lock().wait(timeout).unwrap_or_default();
        let ready = g.poller.lock().settle(&events);
        for ev in ready {
            deliver_event(ev);
        }
    }
}

/// Signals the netpoller thread to exit on its next tick. Called
/// from `gos_rt_exit` so in-flight I/O drains cleanly instead of
/// being interrupted mid-syscall by `std::process::exit`. Idempotent.
pub fn request_shutdown() {
    // The observable flag is a bare process global: setting it must
    // not depend on (or boot) the runtime - `gos_rt_exit` runs on the
    // exit path of every program and booting a worker pool there just
    // to flag it down cost ~150 ms per short-lived process.
    SHUTDOWN_REQUESTED.store(true, Ordering::Release);
    // Only an already-booted runtime has a poller to wake.
    let Some(g) = GLOBALS.get() else { return };
    g.poller_shutdown.store(true, Ordering::Release);
    // Wake the poller so it observes the flag instead of sleeping on.
    let _ = g.poller_interrupt.wake();
}

/// Process-wide shutdown flag, independent of runtime boot state so
/// `request_shutdown` / `is_shutdown_requested` keep their contract
/// for programs that never started the scheduler.
static SHUTDOWN_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Blocks until every goroutine spawned via `go` has finished, bounded
/// at five seconds so a permanently blocked goroutine cannot wedge
/// process exit. No-op - and crucially, does not boot the runtime -
/// when nothing ever started the scheduler: a program with no
/// concurrency has nothing to drain, and booting a worker pool on the
/// exit path just to observe it idle cost ~150 ms per process.
pub fn drain_goroutines_for_exit() {
    if let Some(g) = GLOBALS.get() {
        let _ = g
            .scheduler
            .wait_quiescent(std::time::Duration::from_secs(5));
    }
}

/// True if [`request_shutdown`] has been called. Long-running
/// runtime loops (HTTP accept, M:N worker idle, …) poll this
/// flag at safepoints so `gos_rt_exit` doesn't interrupt them
/// mid-iteration.
#[must_use]
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Acquire)
}

/// Wakes the netpoller thread early so it re-enters `poll()` with
/// up-to-date registrations and timer entries. Called by paths
/// that just inserted into the poller (registration, timer add)
/// to avoid waiting up to one poll cycle for the change to take
/// effect. Cheap and idempotent - safe to call from any thread.
pub fn wake_poller() {
    let _ = globals().poller_interrupt.wake();
}

fn deliver_event(ev: Readiness) {
    let waker = globals().wakers.lock().remove(&ev.gid);
    if let Some(w) = waker {
        w();
    }
}

/// Allocates a fresh [`Gid`] for use as a runtime-internal wait
/// handle (timers, blocking pool callbacks, I/O readiness). These ids
/// do not correspond to user-spawned goroutines.
#[must_use]
pub fn alloc_runtime_gid() -> Gid {
    let raw = globals().gid_alloc.fetch_add(1, Ordering::Relaxed);
    Gid(u32::try_from(raw & 0xFFFF_FFFF).unwrap_or(u32::MAX))
}

/// Registers `waker` to be invoked when the poller delivers the
/// next event tagged with `gid`.
pub fn register_waker(gid: Gid, waker: Box<dyn Fn() + Send + Sync>) {
    globals().wakers.lock().insert(gid, waker);
}

/// Removes any waker associated with `gid`.
pub fn forget_waker(gid: Gid) {
    globals().wakers.lock().remove(&gid);
}

/// Adds a one-shot timer firing at `deadline`. Returns the [`Gid`]
/// the caller passes to [`register_waker`].
#[must_use]
pub fn add_timer(deadline: Instant) -> Gid {
    let gid = alloc_runtime_gid();
    with_poller(|poller| {
        poller.add_timer(deadline, gid);
    });
    gid
}

/// Borrows the netpoller for a closure. Used by the I/O bridge code
/// in `gossamer-std::net` and the runtime's own HTTP plumbing.
pub fn with_poller<R>(f: impl FnOnce(&mut OsPoller) -> R) -> R {
    let g = globals();
    // The thread exists only once something registers with the poller, so a
    // program that never does carries no thread polling for it.
    ensure_poller_thread(g);
    let result = {
        let mut poller = g.poller.lock();
        f(&mut poller)
    };
    // The closure may have registered a new I/O source or
    // dropped a timer in the wheel; wake the netpoller thread so
    // it re-polls with the up-to-date state.
    let _ = g.poller_interrupt.wake();
    result
}

/// Convenience wrapper around `Poller::poll(0)` that returns any
/// already-ready events without blocking.
///
/// # Errors
///
/// Returns `io::Error` when the poller's underlying `epoll`/`kqueue`
/// rejects the call.
pub fn drain_ready() -> io::Result<Vec<Readiness>> {
    with_poller(|poller| poller.poll(Some(Duration::ZERO)))
}

// ---------------------------------------------------------------
// Goroutine plumbing
// ---------------------------------------------------------------

thread_local! {
    /// Gid of the goroutine currently running on this OS worker
    /// thread, biased by `+1` so that `0` reliably means
    /// "no goroutine on this thread" (a goroutine with raw gid 0
    /// is a real value the scheduler hands out and must be
    /// distinguishable from the unset sentinel).
    /// Set by the worker loop immediately before each resume,
    /// cleared after.
    static CURRENT_GID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Returns the gid of the goroutine currently running on this OS
/// thread, or `None` if the calling thread is not a scheduler worker
/// driving a goroutine.
#[must_use]
pub fn current_gid() -> Option<Gid> {
    let raw = CURRENT_GID.with(std::cell::Cell::get);
    if raw == 0 { None } else { Some(Gid(raw - 1)) }
}

pub(crate) fn set_current_gid(gid: Gid) {
    CURRENT_GID.with(|c| c.set(gid.as_u32().wrapping_add(1)));
}

pub(crate) fn clear_current_gid() {
    CURRENT_GID.with(|c| c.set(0));
}

/// Parker handle handed to the closure in [`park`]. Carries the gid
/// of the goroutine that is about to suspend; the closure is expected
/// to register the gid with whatever wakeup source it wants to wait
/// on (a poller waker, a channel parked-receivers list, a mutex
/// queue) so that source can later call
/// [`MultiScheduler::unpark`].
#[derive(Debug, Clone, Copy)]
pub struct Parker {
    /// Identifier of the goroutine being parked. Pass to
    /// [`MultiScheduler::unpark`] when the wait is satisfied.
    pub gid: Gid,
    /// Reason the goroutine is parking. Carried alongside for
    /// diagnostics; the scheduler uses it to populate goroutine
    /// state in SIGQUIT dumps.
    pub reason: ParkReason,
}

/// Suspends the calling goroutine after invoking `arm` to register
/// the wakeup source. The `arm` callback runs *before* the suspend
/// so the wakeup source already knows the gid when this function
/// transfers control back to the scheduler.
///
/// Wakeup race window: if the wake fires *between* `arm()`
/// returning and the worker loop moving this task into the parked
/// map, the scheduler's `unpark(gid)` can't find the gid in
/// `parked` yet. The scheduler handles this with a side
/// `pre_unpark` set - the worker checks it just after parking
/// and immediately re-ejects the task if its gid is in `pre_unpark`.
///
/// # Panics
///
/// Panics if the calling thread is not currently driving a
/// goroutine. Stdlib code that may be invoked from non-goroutine
/// contexts must check [`gossamer_coro::in_goroutine`] and fall
/// back to OS-thread blocking when off.
pub fn park(reason: ParkReason, arm: impl FnOnce(&Parker)) {
    let gid = current_gid().expect("park called outside a goroutine");
    let parker = Parker { gid, reason };
    arm(&parker);
    // Publish the park request to the worker M, which reads it
    // after the coroutine suspends.
    PENDING_PARK.with(|cell| cell.set(Some((gid, reason))));
    gossamer_coro::suspend();
}

thread_local! {
    /// Set by [`park`] just before suspending; read-and-cleared by
    /// the worker M's `Step::Yield` handler. When `Some`, the
    /// scheduler moves the task into the parked map keyed by gid
    /// instead of re-enqueueing onto the local deque.
    static PENDING_PARK: std::cell::Cell<Option<(Gid, ParkReason)>> =
        const { std::cell::Cell::new(None) };
}

/// Returns the most recent `(gid, reason)` published by [`park`],
/// reads-and-clears.
pub(crate) fn take_pending_park() -> Option<(Gid, ParkReason)> {
    PENDING_PARK.with(std::cell::Cell::take)
}

/// Suspends the current goroutine on `io`'s readiness for the given
/// `interest`. Wires the netpoller registration, the waker, the
/// park, and the cleanup into one call.
///
/// How long a thread-blocking wait sleeps before returning so its caller
/// can re-check a condition the descriptor cannot report. Two orders of
/// magnitude longer than the 1 ms poll this replaced, and well inside the
/// deadlines cancellation tests allow.
const RECHECK_INTERVAL: Duration = Duration::from_millis(50);

/// Blocks the calling OS thread until `io` is ready, for callers with no
/// goroutine to park. Uses a poll of its own rather than the shared
/// scheduler poller, whose readiness is delivered by unparking a
/// goroutine that does not exist here.
fn block_thread_on_io<S: mio::event::Source + ?Sized>(
    io: &mut S,
    interest: crate::sched::Interest,
) -> io::Result<()> {
    block_thread_on_io_for(io, interest, RECHECK_INTERVAL)
}

/// Blocks the calling OS thread until `io` is ready or `slice` passes.
fn block_thread_on_io_for<S: mio::event::Source + ?Sized>(
    io: &mut S,
    interest: crate::sched::Interest,
    slice: Duration,
) -> io::Result<()> {
    let mio_interest = match interest {
        crate::sched::Interest::Readable => mio::Interest::READABLE,
        crate::sched::Interest::Writable => mio::Interest::WRITABLE,
        crate::sched::Interest::Timer => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Interest::Timer is reserved for add_timer",
            ));
        }
    };
    let mut poll = mio::Poll::new()?;
    let mut events = mio::Events::with_capacity(1);
    poll.registry().register(io, mio::Token(0), mio_interest)?;
    // Bounded, because callers loop on a condition the descriptor cannot
    // signal: `accept_ctx` and `read_ctx` re-check their context between
    // waits, and a wait that only ends on readiness would never let them
    // observe a cancellation. The bound is the re-check interval, not a
    // guess at how long readiness takes - readiness still wakes the poll
    // immediately.
    let waited = match poll.poll(&mut events, Some(slice)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(()),
        Err(e) => Err(e),
    };
    let _ = poll.registry().deregister(io);
    waited
}

/// Outside a goroutine there is no coroutine to suspend, so the calling
/// OS thread blocks on the source itself. `fn main() { listener.accept() }`
/// takes that path - the first shape a program is written in - and it has
/// to cost nothing while it waits.
///
/// # Errors
///
/// Returns the underlying `io::Error` if mio refuses the
/// registration (e.g. file descriptor closed).
pub fn wait_io<S: mio::event::Source + ?Sized>(
    io: &mut S,
    interest: crate::sched::Interest,
) -> io::Result<()> {
    if !gossamer_coro::in_goroutine() {
        return block_thread_on_io(io, interest);
    }
    let mut result: io::Result<()> = Ok(());
    let mut source = None;
    let mut parked_as = None;
    park(ParkReason::Io, |parker| {
        parked_as = Some(parker.gid);
        let gid = parker.gid;
        register_waker(
            gid,
            Box::new(move || {
                scheduler().unpark(gid);
            }),
        );
        match with_poller(|p| p.register_io(io, interest, gid)) {
            Ok(registered) => source = Some(registered),
            Err(e) => {
                result = Err(e);
                // `park` still runs after its arming closure. Publish an
                // immediate wake so the coroutine can observe the error
                // instead of remaining parked forever.
                scheduler().unpark(gid);
            }
        }
    });
    if let Some(gid) = parked_as {
        forget_waker(gid);
    }
    if let Some(source) = source {
        let _ = with_poller(|p| p.deregister_io(io, source, interest));
    }
    result
}

/// Waits for an I/O readiness event until `deadline`.
///
/// Returns `Ok(true)` when the source woke before the deadline and
/// `Ok(false)` when the deadline elapsed.  Both registrations are removed on
/// resume, so a former read deadline cannot wake a later keep-alive request.
/// This is the deadline-aware primitive used by the compiled HTTP/1 server.
pub fn wait_io_until<S: mio::event::Source + ?Sized>(
    io: &mut S,
    interest: crate::sched::Interest,
    deadline: Instant,
) -> io::Result<bool> {
    if deadline <= Instant::now() {
        return Ok(false);
    }
    if !gossamer_coro::in_goroutine() {
        // Readiness still ends the wait: the poll is bounded by the
        // deadline, and the caller retries its transfer while the
        // deadline holds.
        let slice = RECHECK_INTERVAL.min(deadline.saturating_duration_since(Instant::now()));
        block_thread_on_io_for(io, interest, slice)?;
        return Ok(Instant::now() < deadline);
    }

    let gid = current_gid().expect("goroutine lost its gid while waiting for I/O");
    let mut result = Ok(());
    let mut io_source = None;
    let mut timer_source = None;
    park(ParkReason::Io, |parker| {
        register_waker(
            parker.gid,
            Box::new(move || {
                scheduler().unpark(gid);
            }),
        );
        match with_poller(|p| {
            let io_source = p.register_io(io, interest, gid)?;
            let timer_source = p.add_timer(deadline, gid);
            Ok::<_, io::Error>((io_source, timer_source))
        }) {
            Ok((registered_io, registered_timer)) => {
                io_source = Some(registered_io);
                timer_source = Some(registered_timer);
            }
            Err(e) => {
                result = Err(e);
                scheduler().unpark(gid);
            }
        }
    });
    forget_waker(gid);
    if let Some(source) = io_source {
        let _ = with_poller(|p| p.deregister_io(io, source, interest));
    }
    if let Some(source) = timer_source {
        with_poller(|p| p.cancel_timer(source));
    }
    result.map(|()| Instant::now() < deadline)
}

/// Suspends the current goroutine until `deadline` by registering a
/// one-shot timer with the netpoller. Falls back to
/// [`thread::sleep`] when called outside a goroutine context (e.g.
/// from synchronous tooling code).
pub fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline <= now {
        return;
    }
    if !gossamer_coro::in_goroutine() {
        // No goroutine to park - fall back to OS-thread sleep.
        crate::platform::sleep(deadline - now);
        return;
    }
    if crate::netpoll::sleep_until(deadline) {
        return;
    }
    park(ParkReason::Timer, |parker| {
        let gid = parker.gid;
        register_waker(
            gid,
            Box::new(move || {
                scheduler().unpark(gid);
            }),
        );
        with_poller(|p| p.add_timer(deadline, gid));
    });
    // Cleanup on resume - the waker entry was consumed by
    // `deliver_event`, but if the wait timed out before delivery
    // (poll loop's 50 ms tick), the waker may still be registered.
    if let Some(gid) = current_gid() {
        forget_waker(gid);
    }
}

/// Channels holding a waiter whose operation would complete if it woke now.
/// A non-zero count means the program can still move even with every
/// goroutine inside a channel wait.
static PENDING_HANDOFFS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Adds or removes one channel from the ready count.
pub fn adjust_pending_handoffs(ready: bool) {
    if ready {
        PENDING_HANDOFFS.fetch_add(1, Ordering::AcqRel);
    } else {
        PENDING_HANDOFFS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Stops the program with the deadlock report when the caller is about to
/// block on another goroutine and nothing is left that could wake it.
///
/// Call immediately before suspending on a channel, with the caller already
/// registered among that channel's waiters so a handoff it would complete is
/// visible. At that point every value that could satisfy any waiter is
/// already queued, so a runtime with no ready waiter and nothing runnable is
/// in a terminal state - waiting longer cannot change the answer.
///
/// A deadlock is a property of the whole program rather than of the
/// goroutine that notices it, so the report ends the program. `op` names the
/// operation that could not complete.
pub fn report_deadlock_if_stuck(op: &str) {
    if !PROGRAM_ENTERED.load(Ordering::Acquire) {
        return;
    }
    if PENDING_HANDOFFS.load(Ordering::Acquire) > 0 {
        return;
    }
    // A live non-goroutine actor can send, receive, or close, whether or not a
    // scheduler was ever built: an HTTP server holds one per connection in
    // flight and one for its accept loop, and none of them is a goroutine.
    if EXTERNAL_ACTORS.load(Ordering::Acquire) > 0 {
        return;
    }
    // No scheduler has ever been built, so no goroutine exists to send,
    // receive, or release: an OS thread blocking here waits on nobody.
    let Some(globals) = GLOBALS.get() else {
        report_fatal_deadlock(op);
    };
    // Reported only with no goroutine left to run: the caller is then the
    // whole program, and a channel that can hand nothing over will never be
    // able to. With a goroutine still live the counts are read while it is
    // free to change them, and a deadlock claimed over a state still being
    // written would end a working program - so that case waits, exactly as
    // it did before this check existed.
    //
    // The read is an atomic. The caller holds a channel lock here, and the
    // scheduler's own locks are taken by workers that then reach for channel
    // locks, so consulting them from this side would invert the order.
    // The caller must also be the only thread inside a channel operation.
    // Goroutines are not the only actors: an embedder's thread, a blocking
    // worker, or a test can hold the other end, and none of them is a live
    // goroutine. A second waiter means someone else is positioned to act.
    // A registered waker is an actor outside the goroutine set that is
    // still positioned to act: a context deadline in the timer wheel, an
    // I/O readiness registration, a blocking-pool completion. Any of them
    // can close or fill the channel this caller waits on.
    if globals.scheduler.live_goroutines() == 0
        && CHANNEL_WAITERS.load(Ordering::Acquire) == 1
        && globals.wakers.lock().is_empty()
    {
        report_fatal_deadlock(op);
    }
}

/// Threads and goroutines currently suspended inside a channel wait.
static CHANNEL_WAITERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Threads outside the goroutine set that can still run Gossamer code, and so
/// can still send on, receive from, or close a channel.
///
/// The runtime serves an HTTP connection on an OS thread of its own rather
/// than on a goroutine, and its accept loop waits for the next connection on
/// another. Both run user code and reach channels, so a channel wait is not
/// terminal while either is live, however few goroutines the scheduler holds.
static EXTERNAL_ACTORS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Adds or removes the caller from the count of non-goroutine actors.
///
/// Prefer [`ExternalActor::enter`], which pairs the two halves.
pub fn adjust_external_actors(entering: bool) {
    if entering {
        EXTERNAL_ACTORS.fetch_add(1, Ordering::AcqRel);
    } else {
        EXTERNAL_ACTORS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Counts the caller as an actor that can reach a channel for as long as it
/// is held.
pub struct ExternalActor;

impl ExternalActor {
    /// Registers the caller among the non-goroutine actors.
    #[must_use]
    pub fn enter() -> Self {
        adjust_external_actors(true);
        Self
    }
}

impl Drop for ExternalActor {
    fn drop(&mut self) {
        adjust_external_actors(false);
    }
}

/// Set once a compiled Gossamer program has entered `main`.
///
/// Deadlock reporting needs to know every party that could act on a channel.
/// That holds for a Gossamer program - its actors are `main` and the
/// goroutines - and not for this library linked into something else, where an
/// embedder's or a test's own thread can hold the other end without being a
/// goroutine. Reporting stays off until a program says the process is one.
static PROGRAM_ENTERED: AtomicBool = AtomicBool::new(false);

/// Marks the process as a running Gossamer program. Called from the entry
/// shim a compiled binary emits, and from nowhere else.
pub fn mark_program_entered() {
    PROGRAM_ENTERED.store(true, Ordering::Release);
}

/// Adds or removes the caller from the channel-waiter count.
pub fn adjust_channel_waiters(entering: bool) {
    if entering {
        CHANNEL_WAITERS.fetch_add(1, Ordering::AcqRel);
    } else {
        CHANNEL_WAITERS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn report_fatal_deadlock(op: &str) -> ! {
    use std::io::Write as _;
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "error: runtime error: error[GX0005]: panic: all goroutines are \
         asleep - deadlock! ({op} can never complete)"
    );
    let _ = err.flush();
    std::process::exit(101);
}

pub use crate::sched::multi::{SYSCALL_HANDOFF_THRESHOLD, SyscallGuard, syscall_enter};

/// Runs a potentially blocking OS operation without pinning a scheduler
/// worker. When called from a goroutine, the operation runs on the blocking
/// pool ([`crate::blocking_pool`]) and the goroutine parks until completion;
/// outside the scheduler the closure runs inline to preserve ordinary
/// blocking semantics.
pub fn run_blocking<T, F>(label: &'static str, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = panic.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = panic.downcast_ref::<String>() {
            s.clone()
        } else {
            "panic".to_string()
        }
    }

    let Some(gid) = current_gid() else {
        return std::panic::catch_unwind(AssertUnwindSafe(f)).map_err(|panic| {
            format!(
                "{label}: blocking operation panicked: {}",
                panic_message(panic)
            )
        });
    };

    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    crate::blocking_pool::submit(Box::new(move || {
        let result = std::panic::catch_unwind(AssertUnwindSafe(f)).map_err(|panic| {
            format!(
                "{label}: blocking operation panicked: {}",
                panic_message(panic)
            )
        });
        let _ = tx.send(result);
        scheduler().unpark(gid);
    }))
    .map_err(|e| format!("{label}: {e}"))?;

    park(ParkReason::Other, |_parker| {});

    rx.recv()
        .map_err(|_| format!("{label}: blocking worker ended without a result"))?
}

/// Spawns `task` on the M:N pool. Returns `None` when the
/// scheduler's live-goroutine cap would be exceeded, or when the host
/// refuses the goroutine's stack mapping; the caller should surface the
/// refusal to user code instead of silently overcommitting kernel
/// resources.
#[must_use]
pub fn try_spawn(task: Box<dyn FnOnce() + Send + 'static>) -> Option<Gid> {
    // A stack costs two mappings, so a process reaches `vm.max_map_count`
    // before it reaches any limit the scheduler tracks. Refusing here keeps
    // that a reportable condition rather than an abort mid-spawn.
    let coro = gossamer_coro::Goroutine::try_new(task).ok()?;
    scheduler().try_spawn(GoroutineTask {
        coro,
        arena: crate::c_abi::rc::ArenaState::empty(),
        isolated_faults: false,
        joinable: false,
    })
}

/// [`try_spawn`] for a goroutine the runtime runs on the program's behalf,
/// such as one serving an accepted connection, which process exit does not
/// wait for.
#[must_use]
pub fn try_spawn_service(task: Box<dyn FnOnce() + Send + 'static>) -> Option<Gid> {
    let coro = gossamer_coro::Goroutine::try_new(task).ok()?;
    scheduler().try_spawn_service(GoroutineTask {
        coro,
        arena: crate::c_abi::rc::ArenaState::empty(),
        isolated_faults: false,
        joinable: false,
    })
}

/// Spawns `task` on the M:N pool. Panics if the live-goroutine cap
/// would be exceeded. Use [`try_spawn`] for graceful refusal.
///
/// The returned [`Gid`] is informational; fire-and-forget is the
/// common shape, so the result is intentionally not `#[must_use]`.
#[allow(
    clippy::must_use_candidate,
    reason = "fire-and-forget spawn is the common shape; Gid is informational"
)]
pub fn spawn(task: Box<dyn FnOnce() + Send + 'static>) -> Gid {
    let coro = gossamer_coro::Goroutine::new(task);
    scheduler().spawn(GoroutineTask {
        coro,
        arena: crate::c_abi::rc::ArenaState::empty(),
        isolated_faults: false,
        joinable: false,
    })
}

/// Adapts a [`gossamer_coro::Goroutine`] into the scheduler's
/// [`crate::sched::Task`] trait. Each `step()` call resumes the
/// coroutine; if the coroutine completes, returns [`Step::Done`].
/// If the coroutine called [`gossamer_coro::suspend`], returns
/// [`Step::Yield`] - the worker loop further consults
/// [`take_pending_park`] to decide whether to re-enqueue the task
/// or move it to the parked map.
struct GoroutineTask {
    coro: gossamer_coro::Goroutine,
    /// Active arena regions travel with the coroutine, not the scheduler
    /// worker. See `c_abi::rc::ArenaState` for why a worker-local arena is
    /// unsound when a parked task is resumed after worker retirement.
    arena: crate::c_abi::rc::ArenaState,
    /// The goroutine's own fault domain and joinable-body flag. Each lives in
    /// a worker thread-local while the goroutine runs and here while it is
    /// parked, since several goroutines take turns on one worker.
    isolated_faults: bool,
    joinable: bool,
}

impl crate::sched::Task for GoroutineTask {
    fn step(&mut self) -> Step {
        crate::c_abi::rc::install_arena_state(std::mem::replace(
            &mut self.arena,
            crate::c_abi::rc::ArenaState::empty(),
        ));
        // The closure inside the coroutine's first `resume()` sets
        // the worker's TLS yielder. Subsequent steps need the
        // worker to re-set it from the slot the closure published.
        let yielder_ptr = self.coro.yielder_ptr();
        if !yielder_ptr.is_null() {
            gossamer_coro::set_current_yielder(yielder_ptr);
        }
        let worker_faults = crate::c_abi::panic::swap_isolated_faults(self.isolated_faults);
        let worker_joinable = gossamer_coro::swap_joinable_spawn(self.joinable);
        let done = self.coro.resume();
        self.isolated_faults = crate::c_abi::panic::swap_isolated_faults(worker_faults);
        self.joinable = gossamer_coro::swap_joinable_spawn(worker_joinable);
        self.arena = crate::c_abi::rc::take_arena_state();
        gossamer_coro::clear_current_yielder();
        if done { Step::Done } else { Step::Yield }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::fd::{FromRawFd, RawFd};
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicBool, Ordering};

    #[cfg(unix)]
    const BLOCKING_FS_CHILD_ENV: &str = "GOSSAMER_BLOCKING_FS_CHILD";
    #[cfg(unix)]
    const BLOCKING_CHILD_PIPE_ENV: &str = "GOSSAMER_BLOCKING_CHILD_PIPE";
    #[cfg(unix)]
    const BLOCKING_TERMINAL_ENV: &str = "GOSSAMER_BLOCKING_TERMINAL";

    #[test]
    #[cfg_attr(miri, ignore)] // spawns goroutines on the mmap-stack scheduler; Miri can't
    fn sleep_until_honors_its_deadline() {
        // The contract `sleep_until` must keep is that it sleeps until at
        // least the requested instant (no early wake) and then returns -
        // the test completing proves it returns. There is deliberately no
        // upper bound on the wake latency: under the parallel load of
        // `cargo test --workspace` the OS can delay the wake by hundreds of
        // milliseconds, which is scheduling jitter, not a scheduler defect,
        // and any fixed ceiling here is flaky rather than a real check.
        let start = Instant::now();
        sleep_until(start + Duration::from_millis(20));
        assert!(start.elapsed() >= Duration::from_millis(15));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns goroutines on the mmap-stack scheduler; Miri can't
    fn spawn_runs_on_pool() {
        use std::sync::atomic::AtomicUsize;
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let c2 = std::sync::Arc::clone(&counter);
        let _ = spawn(Box::new(move || {
            c2.fetch_add(7, Ordering::Relaxed);
        }));
        for _ in 0..200 {
            if counter.load(Ordering::Relaxed) == 7 {
                return;
            }
            crate::platform::sleep(Duration::from_millis(10));
        }
        panic!("spawned closure did not run within deadline");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns goroutines on the mmap-stack scheduler; Miri can't
    fn goroutine_can_sleep_via_park() {
        use std::sync::atomic::AtomicUsize;
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let c2 = std::sync::Arc::clone(&counter);
        let start = Instant::now();
        let _ = spawn(Box::new(move || {
            sleep_until(Instant::now() + Duration::from_millis(20));
            c2.fetch_add(1, Ordering::Relaxed);
        }));
        for _ in 0..200 {
            if counter.load(Ordering::Relaxed) == 1 {
                let elapsed = start.elapsed();
                assert!(elapsed >= Duration::from_millis(15));
                return;
            }
            crate::platform::sleep(Duration::from_millis(10));
        }
        panic!("goroutine sleep did not return within deadline");
    }

    #[cfg(unix)]
    #[test]
    #[cfg_attr(miri, ignore)] // starts a one-worker subprocess with mmap-stack coroutines
    fn blocking_filesystem_read_allows_peer_progress_on_one_worker() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("sched_global::tests::blocking_filesystem_read_child")
            .arg("--nocapture")
            .env(BLOCKING_FS_CHILD_ENV, "1")
            .env("GOSSAMER_MAX_PROCS", "1")
            .status()
            .expect("start one-worker child");
        assert!(status.success(), "one-worker child failed: {status}");
    }

    #[cfg(unix)]
    #[test]
    #[cfg_attr(miri, ignore)] // exercises a real FIFO and scheduler worker thread
    fn blocking_filesystem_read_child() {
        if std::env::var_os(BLOCKING_FS_CHILD_ENV).is_none() {
            return;
        }

        let fifo = crate::platform::temp_dir().join(format!(
            "gos-blocking-fs-{}-{}",
            crate::platform::process_id(),
            crate::platform::system_time_now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after epoch")
                .as_nanos()
        ));
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path has no NUL");
        // SAFETY: `fifo_c` is a NUL-terminated path that remains live for the
        // call; 0600 avoids exposing the test FIFO to other local users.
        assert_eq!(
            unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) },
            0,
            "create FIFO"
        );

        let peer_ran = std::sync::Arc::new(AtomicBool::new(false));
        let read_done = std::sync::Arc::new(AtomicBool::new(false));
        let reader_path = fifo_c.clone();
        let reader_done = std::sync::Arc::clone(&read_done);
        let _ = spawn(Box::new(move || {
            // SAFETY: `reader_path` is an owned, NUL-terminated FIFO path and
            // the C-ABI helper accepts it for the duration of this call.
            let raw = unsafe { crate::c_abi::fs::gos_rt_fs_read_to_string(reader_path.as_ptr()) };
            if !raw.is_null() {
                // SAFETY: the C-ABI helper returns either null or an owned
                // CString allocation, so this consumes exactly that allocation.
                drop(unsafe { CString::from_raw(raw) });
            }
            reader_done.store(true, Ordering::Release);
        }));
        let peer = std::sync::Arc::clone(&peer_ran);
        let _ = spawn(Box::new(move || peer.store(true, Ordering::Release)));

        for _ in 0..200 {
            if peer_ran.load(Ordering::Acquire) {
                break;
            }
            crate::platform::sleep(Duration::from_millis(5));
        }
        assert!(
            peer_ran.load(Ordering::Acquire),
            "a FIFO read pinned the only scheduler worker"
        );

        std::fs::write(&fifo, b"unblock").expect("unblock FIFO reader");
        for _ in 0..200 {
            if read_done.load(Ordering::Acquire) {
                let _ = std::fs::remove_file(&fifo);
                return;
            }
            crate::platform::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_file(&fifo);
        panic!("FIFO read did not resume after writer closed");
    }

    #[cfg(unix)]
    #[test]
    #[cfg_attr(miri, ignore)] // starts a one-worker subprocess with mmap-stack coroutines
    fn blocking_child_pipe_allows_peer_progress_on_one_worker() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("sched_global::tests::blocking_child_pipe_child")
            .arg("--nocapture")
            .env(BLOCKING_CHILD_PIPE_ENV, "1")
            .env("GOSSAMER_MAX_PROCS", "1")
            .status()
            .expect("start one-worker child");
        assert!(status.success(), "one-worker child failed: {status}");
    }

    #[cfg(unix)]
    #[test]
    #[cfg_attr(miri, ignore)] // waits on a real child stdout pipe
    fn blocking_child_pipe_child() {
        if std::env::var_os(BLOCKING_CHILD_PIPE_ENV).is_none() {
            return;
        }

        let read_started = std::sync::Arc::new(AtomicBool::new(false));
        let peer_ran = std::sync::Arc::new(AtomicBool::new(false));
        let read_done = std::sync::Arc::new(AtomicBool::new(false));
        let started = std::sync::Arc::clone(&read_started);
        let done = std::sync::Arc::clone(&read_done);
        let _ = spawn(Box::new(move || {
            let args = vec!["-c".to_string(), "sleep 0.2; printf done".to_string()];
            let handle = crate::c_abi::piped_child_spawn("sh", &args).expect("spawn shell");
            started.store(true, Ordering::Release);
            assert_eq!(
                crate::c_abi::piped_child_read_stdout(handle).as_deref(),
                Some("done")
            );
            assert_eq!(
                crate::c_abi::piped_child_wait(handle).expect("wait shell"),
                0
            );
            done.store(true, Ordering::Release);
        }));

        for _ in 0..200 {
            if read_started.load(Ordering::Acquire) {
                break;
            }
            crate::platform::sleep(Duration::from_millis(5));
        }
        assert!(
            read_started.load(Ordering::Acquire),
            "child pipe did not start"
        );

        let peer = std::sync::Arc::clone(&peer_ran);
        let _ = spawn(Box::new(move || peer.store(true, Ordering::Release)));
        for _ in 0..200 {
            if peer_ran.load(Ordering::Acquire) {
                break;
            }
            crate::platform::sleep(Duration::from_millis(5));
        }
        assert!(
            peer_ran.load(Ordering::Acquire),
            "a child stdout read pinned the only scheduler worker"
        );

        for _ in 0..200 {
            if read_done.load(Ordering::Acquire) {
                return;
            }
            crate::platform::sleep(Duration::from_millis(5));
        }
        panic!("child stdout read did not resume after the child exited");
    }

    #[cfg(unix)]
    #[test]
    #[cfg_attr(miri, ignore)] // starts a one-worker subprocess with a real full pipe
    fn blocking_terminal_write_allows_peer_progress_on_one_worker() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("sched_global::tests::blocking_terminal_write_child")
            .arg("--nocapture")
            .env(BLOCKING_TERMINAL_ENV, "1")
            .env("GOSSAMER_MAX_PROCS", "1")
            .status()
            .expect("start one-worker child");
        assert!(status.success(), "one-worker child failed: {status}");
    }

    #[cfg(unix)]
    #[test]
    #[cfg_attr(miri, ignore)] // redirects stdout to a real pipe and drains it after the assertion
    fn blocking_terminal_write_child() {
        if std::env::var_os(BLOCKING_TERMINAL_ENV).is_none() {
            return;
        }

        let mut pipe: [RawFd; 2] = [-1; 2];
        // SAFETY: `pipe` points to two valid fd slots.
        assert_eq!(
            unsafe { libc::pipe(pipe.as_mut_ptr()) },
            0,
            "create stdout pipe"
        );
        // SAFETY: stdout is an open descriptor in the test subprocess.
        let saved_stdout = unsafe { libc::dup(libc::STDOUT_FILENO) };
        assert!(saved_stdout >= 0, "save stdout");
        // SAFETY: both descriptors are open. stdout now shares the pipe's write
        // end; closing the original write descriptor leaves stdout valid.
        assert_eq!(
            unsafe { libc::dup2(pipe[1], libc::STDOUT_FILENO) },
            libc::STDOUT_FILENO
        );
        assert_eq!(
            unsafe { libc::close(pipe[1]) },
            0,
            "close original pipe writer"
        );

        let writer_started = std::sync::Arc::new(AtomicBool::new(false));
        let (writer_done, write_finished) = std::sync::mpsc::channel();
        let peer_ran = std::sync::Arc::new(AtomicBool::new(false));
        let started = std::sync::Arc::clone(&writer_started);
        let _ = spawn(Box::new(move || {
            started.store(true, Ordering::Release);
            crate::c_abi::write_terminal(1, &vec![b'x'; 1024 * 1024]);
            let _ = writer_done.send(());
        }));

        for _ in 0..200 {
            if writer_started.load(Ordering::Acquire) {
                break;
            }
            crate::platform::sleep(Duration::from_millis(5));
        }
        assert!(
            writer_started.load(Ordering::Acquire),
            "terminal writer did not start"
        );
        // Let the blocking worker fill the pipe before scheduling the peer.
        crate::platform::sleep(Duration::from_millis(20));

        let peer = std::sync::Arc::clone(&peer_ran);
        let _ = spawn(Box::new(move || peer.store(true, Ordering::Release)));
        for _ in 0..200 {
            if peer_ran.load(Ordering::Acquire) {
                break;
            }
            crate::platform::sleep(Duration::from_millis(5));
        }
        assert!(
            peer_ran.load(Ordering::Acquire),
            "a terminal write pinned the only scheduler worker"
        );

        // Drain the pipe on a plain OS thread while fd 1 still names it, and
        // restore stdout only once the write has returned: on macOS, replacing
        // a descriptor that a thread is blocked writing to fails that write
        // with EPIPE rather than letting it finish.
        let reader = std::thread::spawn(move || {
            // SAFETY: this thread uniquely owns the pipe's read descriptor.
            let mut pipe = unsafe { std::fs::File::from_raw_fd(pipe[0]) };
            let mut buf = [0_u8; 8192];
            while std::io::Read::read(&mut pipe, &mut buf).expect("read terminal pipe") > 0 {}
        });
        write_finished
            .recv()
            .expect("terminal write did not resume after pipe drain");

        // SAFETY: `saved_stdout` is valid and becomes fd 1; it is then closed.
        // That drops the last descriptor on the pipe's write end, so the
        // reader sees end of file.
        assert_eq!(
            unsafe { libc::dup2(saved_stdout, libc::STDOUT_FILENO) },
            libc::STDOUT_FILENO
        );
        assert_eq!(
            unsafe { libc::close(saved_stdout) },
            0,
            "close saved stdout"
        );
        reader.join().expect("terminal pipe reader");
    }
}
