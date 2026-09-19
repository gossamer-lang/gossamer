//! Cooperative single-threaded process-global scheduler for
//! wasm32-unknown-unknown.
//!
//! The native [`sched_global`](super::sched_global) wires the
//! work-stealing M:N scheduler to a mio netpoller and a dedicated
//! poller thread. The browser playground has neither threads nor a
//! netpoller, so this module provides the same public surface with
//! cooperative single-threaded semantics:
//!
//! - [`spawn`] / [`try_spawn`] run the goroutine body to completion
//!   immediately (the eager `gossamer_coro::Goroutine` shim).
//! - [`park`] diverges through `gossamer_coro::suspend`: a goroutine
//!   that genuinely needs to block cannot make progress with no other
//!   thread to run, which is the documented v1 limit.
//! - the poller / timer / waker entry points are inert no-ops so that
//!   non-blocking programs run unchanged.

#![forbid(unsafe_code)]

use std::cell::Cell;
use std::io;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::platform::Instant;
use crate::sched::{Gid, Interest, MultiScheduler, OsPoller, ParkReason, Readiness, Step, Task};

static SCHEDULER: OnceLock<MultiScheduler> = OnceLock::new();
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static GID_ALLOC: AtomicU64 = AtomicU64::new(1_000_000);

/// Returns the process-wide cooperative scheduler.
#[must_use]
pub fn scheduler() -> &'static MultiScheduler {
    SCHEDULER.get_or_init(|| MultiScheduler::new(1))
}

/// Flags the runtime as shutting down. No poller thread to wake.
pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::Release);
}

/// No-op: every goroutine has already run to completion eagerly, so
/// there is nothing outstanding to drain at exit.
pub fn drain_goroutines_for_exit() {}

/// Returns `true` once [`request_shutdown`] has been called.
#[must_use]
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Acquire)
}

/// No-op: there is no poller thread to interrupt.
pub fn wake_poller() {}

/// Allocates a fresh gid for park/unpark bookkeeping outside of a
/// spawn (timers, callbacks). Monotonic, process-global.
#[must_use]
pub fn alloc_runtime_gid() -> Gid {
    Gid(GID_ALLOC.fetch_add(1, Ordering::Relaxed) as u32)
}

/// No-op: nothing is ever parked, so no waker is ever delivered.
pub fn register_waker(_gid: Gid, _waker: Box<dyn Fn() + Send + Sync>) {}

/// No-op counterpart to [`register_waker`].
pub fn forget_waker(_gid: Gid) {}

/// Returns a fresh timer gid. The timer never fires (no poller), which
/// is consistent with [`sleep_until`] returning immediately.
#[must_use]
pub fn add_timer(_deadline: Instant) -> Gid {
    alloc_runtime_gid()
}

/// Runs `f` against a stub poller. Reachable only from network std
/// modules, which are gated out of the wasm build; present so the
/// signature stays source-compatible with native.
pub fn with_poller<R>(f: impl FnOnce(&mut OsPoller) -> R) -> R {
    let mut poller = OsPoller::new().expect("stub OsPoller::new is infallible");
    f(&mut poller)
}

/// No readiness ever arrives on the single-threaded runtime.
pub fn drain_ready() -> io::Result<Vec<Readiness>> {
    Ok(Vec::new())
}

thread_local! {
    /// Gid of the goroutine currently running, biased by `+1` so `0`
    /// means "no goroutine" (matching the native convention).
    static CURRENT_GID: Cell<u32> = const { Cell::new(0) };
}

/// Returns the gid of the goroutine currently running, or `None` when
/// the caller is the main goroutine.
#[must_use]
pub fn current_gid() -> Option<Gid> {
    let raw = CURRENT_GID.with(Cell::get);
    if raw == 0 { None } else { Some(Gid(raw - 1)) }
}

pub(crate) fn current_gid_raw() -> u32 {
    CURRENT_GID.with(Cell::get)
}

pub(crate) fn set_current_gid_raw(raw: u32) {
    CURRENT_GID.with(|c| c.set(raw));
}

pub(crate) fn set_current_gid(gid: Gid) {
    set_current_gid_raw(gid.as_u32().wrapping_add(1));
}

/// Parker handle handed to the closure in [`park`].
#[derive(Debug, Clone, Copy)]
pub struct Parker {
    /// Identifier of the goroutine being parked.
    pub gid: Gid,
    /// Reason the goroutine is parking.
    pub reason: ParkReason,
}

/// Counts a channel becoming able to hand a value over. Kept for surface
/// parity with the native scheduler; nothing on wasm reads it, because the
/// state it guards against - a goroutine waiting for one that never runs -
/// cannot arise where every goroutine runs to completion at its spawn.
pub fn adjust_pending_handoffs(_ready: bool) {}

/// Counts the caller entering or leaving a channel wait. Inert here for the
/// same reason as [`adjust_pending_handoffs`].
pub fn adjust_channel_waiters(_entering: bool) {}

/// Marks the process as a running Gossamer program. Native uses this to keep
/// deadlock reporting off inside an embedder; the playground is only ever a
/// program, and reports nothing, so it records nothing.
pub fn mark_program_entered() {}

/// Reports a deadlock before blocking. Inert on wasm: a channel operation
/// that cannot complete immediately goes on to [`park`], which diverges with
/// the documented "blocking not supported" message. That message names the
/// real limit of this target, so a second report would say less, not more.
pub fn report_deadlock_if_stuck(_op: &str) {}

/// Suspends the calling goroutine. With no other thread to make
/// progress, a real block cannot be satisfied, so after running `arm`
/// (which would register the wakeup source on native) this diverges
/// through `gossamer_coro::suspend`, panicking with the documented
/// "blocking not supported in the wasm playground" message.
pub fn park(reason: ParkReason, arm: impl FnOnce(&Parker)) {
    let gid = current_gid().unwrap_or(Gid(0));
    let parker = Parker { gid, reason };
    arm(&parker);
    gossamer_coro::suspend();
}

/// Would suspend on `io` readiness; the network std modules that call
/// this are gated out of the wasm build, so this is unreachable in
/// practice. Diverges if ever reached.
pub fn wait_io<S: ?Sized>(_io: &mut S, _interest: Interest) -> io::Result<()> {
    park(ParkReason::Io, |_parker| {});
    Ok(())
}

/// Wasm has no OS netpoller.  This follows [`wait_io`] and diverges through
/// the documented unsupported-blocking path when reached.
pub fn wait_io_until<S: ?Sized>(
    _io: &mut S,
    _interest: Interest,
    _deadline: Instant,
) -> io::Result<bool> {
    park(ParkReason::Io, |_parker| {});
    Ok(false)
}

/// Returns immediately. Cooperative single-threaded time cannot block
/// the only thread; programs that depend on real sleeping should run
/// with `gos` locally.
pub fn sleep_until(_deadline: Instant) {}

/// Wasm has no worker threads. Run the closure inline; callers that need true
/// blocking already reach the documented unsupported-blocking path elsewhere.
pub fn run_blocking<T, F>(label: &'static str, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    std::panic::catch_unwind(AssertUnwindSafe(f)).map_err(|panic| {
        if let Some(s) = panic.downcast_ref::<&str>() {
            format!("{label}: blocking operation panicked: {s}")
        } else if let Some(s) = panic.downcast_ref::<String>() {
            format!("{label}: blocking operation panicked: {s}")
        } else {
            format!("{label}: blocking operation panicked: panic")
        }
    })
}

/// Runs `task` to completion immediately, returning `Some(gid)`.
#[must_use]
pub fn try_spawn(task: Box<dyn FnOnce() + Send + 'static>) -> Option<Gid> {
    let coro = gossamer_coro::Goroutine::new(task);
    scheduler().try_spawn(GoroutineTask { coro })
}

/// Runs `task` to completion immediately, returning its gid.
#[allow(
    clippy::must_use_candidate,
    reason = "fire-and-forget spawn is the common shape; Gid is informational"
)]
pub fn spawn(task: Box<dyn FnOnce() + Send + 'static>) -> Gid {
    let coro = gossamer_coro::Goroutine::new(task);
    scheduler().spawn(GoroutineTask { coro })
}

/// Adapts a [`gossamer_coro::Goroutine`] into the scheduler's [`Task`].
/// On wasm the coro resumes the body to completion (or catches its
/// panic) on the first step, so one `step` always settles it.
struct GoroutineTask {
    coro: gossamer_coro::Goroutine,
}

impl Task for GoroutineTask {
    fn step(&mut self) -> Step {
        if self.coro.resume() {
            Step::Done
        } else {
            Step::Yield
        }
    }
}

/// Marks the caller as inside a blocking system call. A wasm build runs one
/// thread with no workers to hand off to, so there is nothing to mark.
#[must_use]
pub fn syscall_enter() {}
