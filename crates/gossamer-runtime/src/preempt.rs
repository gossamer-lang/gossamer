//! Async preemption + GC safepoint plumbing.
//!
//! This module owns the mechanism that lets the scheduler / GC
//! interrupt a CPU-bound goroutine so other goroutines can run and
//! the collector can mark the world. Two cooperating pieces:
//!
//! 1. A global atomic *preempt phase* counter. Application code
//!    polls [`should_yield`] at safepoints (function entry, loop
//!    back-edges, allocation sites). If it returns `true`, the
//!    caller jumps to its yield handler - interpreter calls into
//!    the scheduler, compiled code calls [`gos_rt_preempt_check`].
//!
//! 2. A real OS signal (`SIGURG` on Unix; a thread-targeted APC on
//!    Windows in a future iteration) installed by [`init`]. When the
//!    scheduler watchdog decides a worker has been running too long,
//!    it raises the signal at that worker's thread; the handler
//!    flips the atomic and the next safepoint poll observes it.
//!
//! The signal handler itself does only async-signal-safe work
//! (atomic store) - no allocations, no locks.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Global counter incremented every time the scheduler asks all
/// goroutines to reach a safepoint (start of a GC cycle, set-max-procs
/// reduction, etc.). Application code compares its own
/// thread-local copy and yields if the global moved.
static GLOBAL_PHASE: AtomicU64 = AtomicU64::new(0);

/// Nonzero while a yield request has not yet reached a compiled loop's poll.
/// A compiled loop tests this byte on every pass, and only a set byte sends
/// it to [`gos_rt_preempt_check_and_yield`], which clears it and decides from
/// [`GLOBAL_PHASE`] whether this worker yields. A worker that misses a
/// request because another cleared the byte first sees the next one: the
/// watchdog keeps asking while a worker overruns its slice.
#[unsafe(export_name = "gos_rt_preempt_requested")]
pub static PREEMPT_REQUESTED: AtomicU8 = AtomicU8::new(0);

// Per-thread "yield requested" flag set by the SIGURG handler.
// Stored thread-locally so the safepoint poll is a single relaxed
// load with no cache-line contention.
thread_local! {
    static LOCAL_YIELD: AtomicBool = const { AtomicBool::new(false) };
    static LOCAL_PHASE: AtomicU64 = const { AtomicU64::new(0) };
}

/// Number of cooperative yields recorded - exposed for tests.
static YIELD_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Slow compiled safepoint calls, enabled only for diagnostics.
static SLOW_POLL_COUNT: AtomicU64 = AtomicU64::new(0);
static PREEMPT_STATS_ENABLED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn report_preempt_stats() {
    eprintln!(
        "gos-preempt-stats: slow_polls={} yields={}",
        SLOW_POLL_COUNT.load(Ordering::Relaxed),
        YIELD_COUNT.load(Ordering::Relaxed)
    );
}

#[cfg(unix)]
fn init_preempt_stats() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("GOS_PREEMPT_STATS").is_some() {
            PREEMPT_STATS_ENABLED.store(true, Ordering::Relaxed);
            // SAFETY: `report_preempt_stats` has C ABI, takes no arguments,
            // and accesses only process-lifetime atomic statics at exit.
            unsafe { libc::atexit(report_preempt_stats) };
        }
    });
}

#[cfg(not(unix))]
fn init_preempt_stats() {}

/// Initialises the SIGURG handler. Idempotent.
pub fn init() {
    init_preempt_stats();
    // Miri cannot model sigaction or a signal-delivery thread. The scheduler
    // remains cooperative there: safepoint polls still observe explicit
    // `request_yield_*` calls, while OS-signal preemption is exercised by the
    // native and sanitizer suites.
    install_signal_handler();
}

#[cfg(all(unix, not(miri)))]
fn install_signal_handler() {
    use signal_hook::iterator::Signals;
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Ok(mut signals) = Signals::new([signal_hook::consts::SIGURG]) else {
            return;
        };
        std::thread::Builder::new()
            .name("gos-preempt".to_string())
            .spawn(move || {
                for _sig in signals.forever() {
                    request_yield_all();
                }
            })
            .ok();
    });
}

#[cfg(all(windows, not(miri)))]
fn install_signal_handler() {
    // Windows preemption via QueueUserAPC. No
    // signal-style dispatcher thread is needed because APCs deliver
    // directly to the targeted worker thread; the APC routine
    // simply calls `request_yield_all`, mirroring the Unix SIGURG
    // handler. Initialisation is a no-op - the work happens at
    // `signal_thread_sigurg`-equivalent time inside
    // [`signal_thread_sigurg`].
}

#[cfg(any(miri, not(any(unix, windows))))]
fn install_signal_handler() {
    // Other platforms: cooperative-only path still works; targeted
    // preemption is a no-op.
}

/// Signals every active worker to reach a safepoint. The actual
/// per-thread flag is consulted by [`should_yield`] from the
/// generated code. Increments the global phase counter so threads
/// without per-thread state can also notice.
pub fn request_yield_all() {
    GLOBAL_PHASE.fetch_add(1, Ordering::AcqRel);
    PREEMPT_REQUESTED.store(1, Ordering::Release);
}

/// Asks the calling thread to reach a safepoint at its next
/// opportunity. Hook used by the scheduler watchdog when it sends
/// SIGURG to a specific worker's thread.
pub fn request_yield_self() {
    LOCAL_YIELD.with(|f| f.store(true, Ordering::Release));
    GLOBAL_PHASE.fetch_add(1, Ordering::AcqRel);
    PREEMPT_REQUESTED.store(1, Ordering::Release);
}

/// Returns `true` when the calling thread should yield at the next
/// safepoint. Reads the global phase with `Acquire` ordering so a
/// `request_yield_all`'s `fetch_add(AcqRel)` on any architecture
/// (including weak-memory ARM/AArch64) is observed promptly - a
/// `Relaxed` load admits unbounded staleness on weak-memory CPUs
/// even though x86 happens to retire the update synchronously.
#[inline]
pub fn should_yield() -> bool {
    let global = GLOBAL_PHASE.load(Ordering::Acquire);
    if LOCAL_PHASE.with(|p| {
        if p.load(Ordering::Relaxed) == global {
            return false;
        }
        p.store(global, Ordering::Release);
        true
    }) {
        return true;
    }
    // The steady state is "nothing pending", so the flag is read before it is
    // cleared: a read-modify-write on every loop back-edge is a locked
    // operation even against thread-local memory. A set that lands after this
    // load is observed at the next safepoint, which is already how a polled
    // request reaches a running goroutine.
    LOCAL_YIELD.with(|f| {
        if f.load(Ordering::Relaxed) {
            f.swap(false, Ordering::Acquire)
        } else {
            false
        }
    })
}

/// Total cooperative yields recorded - for tests / diagnostics.
#[must_use]
pub fn yields_observed() -> usize {
    YIELD_COUNT.load(Ordering::Relaxed)
}

/// Records a successful cooperative yield. Called by code that
/// honours [`should_yield`] and actually returns control to the
/// scheduler.
pub fn note_yield() {
    YIELD_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Number of phase changes seen by [`should_yield`] - diagnostic.
#[must_use]
pub fn current_phase() -> u64 {
    GLOBAL_PHASE.load(Ordering::Relaxed)
}

/// C-ABI safepoint poll. Compiled code emits a call to this at each
/// loop back-edge / function entry. Returns `1` if the goroutine
/// should yield, `0` otherwise. Kept as a non-mangled `extern "C"`
/// so the LLVM lowerer can call it by name.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_preempt_check() -> i32 {
    if should_yield() {
        note_yield();
        1
    } else {
        0
    }
}

/// Combined safepoint: polls and, when a yield is requested,
/// honours it by handing the OS slice back to the kernel via
/// `sched_yield`. Cheaper at the call site than the
/// poll-then-conditional-yield pattern because compiled code
/// emits a single call per back-edge instead of poll + branch +
/// call. Returns `1` if a yield was performed, `0` otherwise.
///
/// On platforms without `sched_yield` (none on the supported
/// targets), the cooperative bump is the only behaviour and the
/// next safepoint will observe the same flag and try again.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_preempt_check_and_yield() -> i32 {
    PREEMPT_REQUESTED.store(0, Ordering::Relaxed);
    if PREEMPT_STATS_ENABLED.load(Ordering::Relaxed) {
        SLOW_POLL_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    if should_yield() {
        note_yield();
        if gossamer_coro::in_goroutine() {
            gossamer_coro::suspend();
        } else {
            std::thread::yield_now();
        }
        1
    } else {
        0
    }
}

/// Watchdog tick used by the scheduler. Returns the number of
/// outstanding `request_yield_*` calls a worker has not yet honoured.
/// Useful for diagnostics; the value is best-effort because workers
/// race the watchdog.
#[must_use]
pub fn pending_yield_pressure() -> u32 {
    PENDING_PRESSURE.load(Ordering::Relaxed)
}

static PENDING_PRESSURE: AtomicU32 = AtomicU32::new(0);

/// Bumps the pending-pressure counter; called by the scheduler when
/// it raises a SIGURG against a worker.
pub fn bump_pressure() {
    PENDING_PRESSURE.fetch_add(1, Ordering::Relaxed);
}

/// Handles [`current_thread_handle`] gave out and [`release_thread_handle`]
/// has not taken back. A thread releases its handle before it exits, and
/// [`signal_thread_sigurg`] signals only a handle listed here while holding
/// this lock, so no signal reaches a thread that has exited.
static LIVE_THREAD_HANDLES: parking_lot::Mutex<Vec<u64>> = parking_lot::Mutex::new(Vec::new());

/// Returns an opaque handle for the calling OS thread suitable for
/// later use with [`signal_thread_sigurg`], which the thread gives back
/// with [`release_thread_handle`] before it exits. On Unix this is the
/// `pthread_t` of the calling thread cast through `u64`. On other
/// platforms it returns `0`; the targeted preemption path becomes a
/// no-op and the cooperative phase counter does the work alone.
#[must_use]
pub fn current_thread_handle() -> u64 {
    let handle = os_thread_handle();
    if handle != 0 {
        LIVE_THREAD_HANDLES.lock().push(handle);
    }
    handle
}

fn os_thread_handle() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: `pthread_self` is async-signal-safe and has no
        // failure modes. Treating the opaque `pthread_t` as a u64
        // is the standard idiom for parking it in a non-FFI field.
        let raw = unsafe { libc::pthread_self() };
        raw as u64
    }
    #[cfg(windows)]
    {
        // use a duplicated Win32 thread handle
        // with THREAD_SET_CONTEXT access so the scheduler can later
        // queue an APC. The duplicated handle is opened against the
        // current thread (`GetCurrentThread` returns a pseudo-handle
        // that's only valid in-process; `DuplicateHandle` upgrades
        // it to a real handle with stable identity).
        use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
        // The duplicated handle is owned here; the caller eventually invokes
        // `release_thread_handle(h)` so the kernel object does not outlive
        // the thread, as the Unix path's `pthread_t` does after a join.
        let mut dup: HANDLE = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` takes nothing and answers a pseudo-handle.
        let proc_handle = unsafe { GetCurrentProcess() };
        // SAFETY: `GetCurrentThread` takes nothing and answers a pseudo-handle.
        let thread_handle = unsafe { GetCurrentThread() };
        // SAFETY: both pseudo-handles name this process and thread, and `dup` is a local the
        // call writes the real handle into.
        let ok = unsafe {
            DuplicateHandle(
                proc_handle,
                thread_handle,
                proc_handle,
                &raw mut dup,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 || dup.is_null() {
            return 0;
        }
        dup as u64
    }
    #[cfg(not(any(unix, windows)))]
    {
        0
    }
}

/// Takes back a handle [`current_thread_handle`] gave out, which its thread
/// does before it exits: afterwards [`signal_thread_sigurg`] ignores it. On
/// Windows this also closes the duplicated thread handle; the Unix
/// `pthread_t` is not refcounted. A handle that is not live is ignored.
pub fn release_thread_handle(handle: u64) {
    let mut live = LIVE_THREAD_HANDLES.lock();
    let Some(at) = live.iter().position(|&h| h == handle) else {
        return;
    };
    live.swap_remove(at);
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        // SAFETY: `handle` was a live `DuplicateHandle` output, and taking
        // it out of the list under its lock makes this its one close.
        let _ = unsafe { CloseHandle(handle as HANDLE) };
    }
}

/// Sends `SIGURG` to the OS thread identified by `handle`. Used by
/// the scheduler watchdog when cooperative preemption alone has not
/// landed (worker has not hit a safepoint inside its budget). The
/// SIGURG handler installed by [`init`] flips the global phase, and -
/// crucially - the kernel-level signal delivery interrupts any
/// blocking syscall the worker is currently inside.
///
/// Returns `true` if the signal was issued, `false` if the platform
/// has no targeted-preempt path or the handle is the null marker.
///
/// A handle that is not live (never given out, or already released) is not
/// signalled.
#[must_use]
pub fn signal_thread_sigurg(handle: u64) -> bool {
    if handle == 0 {
        return false;
    }
    // Held across the signal, so the thread cannot release its handle and
    // exit in between.
    let live = LIVE_THREAD_HANDLES.lock();
    if !live.contains(&handle) {
        return false;
    }
    #[cfg(miri)]
    {
        // Miri cannot model pthread_kill/APCs. Keep the cooperative path
        // observable to the scheduler tests without issuing an OS signal.
        let _ = handle;
        request_yield_all();
        return false;
    }
    #[cfg(all(unix, not(miri)))]
    {
        // SAFETY: `handle` is a live thread's `pthread_t`: it is listed in
        // `LIVE_THREAD_HANDLES`, whose lock is held, and a thread releases
        // its handle before exiting. The SIGURG handler only does atomic
        // stores.
        let rc = unsafe { libc::pthread_kill(handle as libc::pthread_t, libc::SIGURG) };
        rc == 0
    }
    #[cfg(all(windows, not(miri)))]
    {
        // queue a user-mode APC into the
        // targeted worker thread. The APC routine bumps the
        // global preempt phase, mirroring the Unix SIGURG handler.
        // APCs only fire at alertable wait points by default - for
        // tight CPU loops the cooperative `gos_rt_preempt_check`
        // emitted by codegen still handles preemption. The APC is
        // the mechanism that handles blocking syscalls; without
        // it, a worker stuck in `WaitForSingleObject` would never
        // observe a cooperative request.
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::Threading::QueueUserAPC;
        // SAFETY: `apc_callback` is `extern "system" fn(usize)` (the APC
        // ABI), and `handle` is a live duplicated thread handle with
        // QUEUE_USER_APC access: it is listed in `LIVE_THREAD_HANDLES`, whose
        // lock is held.
        let rc = unsafe { QueueUserAPC(Some(apc_callback), handle as HANDLE, 0) };
        rc != 0
    }
    #[cfg(all(not(miri), not(any(unix, windows))))]
    {
        false
    }
}

#[cfg(windows)]
unsafe extern "system" fn apc_callback(_arg: usize) {
    // Async-signal-safe-ish: only atomic stores happen here.
    request_yield_all();
}

/// Decrements pending pressure. Called by the safepoint handler when
/// the yield is honoured.
pub fn drop_pressure() {
    PENDING_PRESSURE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            if v > 0 { Some(v - 1) } else { None }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    // The phase + pressure counters this module exposes are
    // process-global. `cargo test` runs the unit tests in parallel
    // by default, so two preempt tests racing on `GLOBAL_PHASE` /
    // `PENDING_PRESSURE` would flake the second-`should_yield`
    // assertion below (a concurrent `request_yield_all` lifts the
    // global past the local phase again). Serialise every test
    // that touches the shared counters through this mutex.
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// This thread's copy of the preempt phase, for the assertions below.
    fn local_phase() -> u64 {
        LOCAL_PHASE.with(|p| p.load(Ordering::Relaxed))
    }

    #[test]
    fn yield_request_sticks_until_polled() {
        let _guard = TEST_LOCK.lock();
        // Drain any phase bumps left over from a prior test run in
        // this process so the local phase starts in sync with the
        // global. After the drain, `should_yield` is guaranteed
        // false until we ask it not to be.
        while should_yield() {}
        let baseline = current_phase();
        request_yield_all();
        assert!(current_phase() > baseline);
        // First should_yield observes the phase change.
        assert!(should_yield());
        // The second poll answers false for the phase this thread now
        // holds. Only a request this test made is serialised by the
        // mutex above - a scheduler another test started raises the
        // global from its own threads - so the poll is entitled to
        // answer true whenever it takes a phase past the held one, and
        // the phase it consumed is what says which happened.
        let held = local_phase();
        assert!(!should_yield() || local_phase() > held);
    }

    #[test]
    fn yield_self_flips_local_flag() {
        let _guard = TEST_LOCK.lock();
        let _ = should_yield();
        request_yield_self();
        assert!(should_yield());
    }

    #[test]
    #[cfg(unix)]
    #[cfg_attr(miri, ignore)] // installs a SIGURG handler via sigaction; Miri has no signals
    fn signal_thread_sigurg_round_trips() {
        let _guard = TEST_LOCK.lock();
        // Initialise the SIGURG dispatcher first so the kernel does
        // not raise the default action (which is to ignore SIGURG;
        // either way is fine, but the dispatcher path is what we
        // actually want to exercise).
        init();
        let handle = current_thread_handle();
        assert!(handle != 0);
        // Sending to ourselves should succeed; the dispatcher
        // thread observes the signal and bumps the phase.
        let baseline = current_phase();
        assert!(signal_thread_sigurg(handle));
        // Give the dispatcher a moment to wake.
        crate::platform::sleep(std::time::Duration::from_millis(50));
        // The phase should have moved at least once.
        assert!(current_phase() >= baseline);
        release_thread_handle(handle);
        assert!(
            !signal_thread_sigurg(handle),
            "a released handle is not signalled"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)] // installs a SIGURG handler via sigaction; Miri has no signals
    fn signal_thread_sigurg_null_handle_is_noop() {
        // No global state mutated; runs in parallel with the
        // serialised tests safely.
        assert!(!signal_thread_sigurg(0));
    }

    #[test]
    fn pressure_counter_round_trips() {
        let _guard = TEST_LOCK.lock();
        let baseline = pending_yield_pressure();
        bump_pressure();
        bump_pressure();
        assert_eq!(pending_yield_pressure(), baseline + 2);
        drop_pressure();
        drop_pressure();
        assert_eq!(pending_yield_pressure(), baseline);
    }
}
