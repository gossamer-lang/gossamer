//! Cooperative preemption + GC safepoint plumbing.
//!
//! A global atomic *preempt phase* counter lets the scheduler and the
//! collector ask a CPU-bound goroutine to reach a safepoint. Application code
//! polls [`should_yield`] at safepoints (function entry, loop back-edges,
//! allocation sites). If it returns `true`, the caller jumps to its yield
//! handler - interpreter calls into the scheduler, compiled code calls
//! [`gos_rt_preempt_check`].
//!
//! Preemption is cooperative only. A loop nest that is counted all the way
//! down and calls none of the program's functions has no poll and runs to
//! completion, as the language documents; no OS signal is sent, since a
//! signal reaching such a loop finds nothing that reads it.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Global counter incremented every time the scheduler asks all
/// goroutines to reach a safepoint (start of a GC cycle, set-max-procs
/// reduction, etc.). Application code compares its own
/// thread-local copy and yields if the global moved.
static GLOBAL_PHASE: AtomicU64 = AtomicU64::new(0);

/// Nonzero while a yield request has not yet reached a compiled loop's poll.
/// A compiled loop tests this byte on every pass, and only a set byte sends
/// it to [`gos_rt_preempt_check_and_yield`], which clears it and decides from
/// `GLOBAL_PHASE` whether this worker yields. A worker that misses a
/// request because another cleared the byte first sees the next one: the
/// watchdog keeps asking while a worker overruns its slice.
#[unsafe(export_name = "gos_rt_preempt_requested")]
pub static PREEMPT_REQUESTED: AtomicU8 = AtomicU8::new(0);

// Per-thread "yield requested" flag set by `request_yield_self`.
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

/// Arms the preemption statistics `GOS_PREEMPT_STATS` asks for. Idempotent.
pub fn init() {
    init_preempt_stats();
}

/// Signals every active worker to reach a safepoint. The actual
/// per-thread flag is consulted by [`should_yield`] from the
/// generated code. Increments the global phase counter so threads
/// without per-thread state can also notice.
pub fn request_yield_all() {
    GLOBAL_PHASE.fetch_add(1, Ordering::AcqRel);
    PREEMPT_REQUESTED.store(1, Ordering::Release);
}

/// Whether each worker times its own CPU use (Linux, FreeBSD): a per-thread
/// timer fires after a slice of CPU and asks only that worker to yield, so the
/// watchdog has no running goroutine to time. Elsewhere the watchdog times
/// runs on the wall clock.
pub const CPU_TIME_SLICES: bool = cfg!(all(
    any(target_os = "linux", target_os = "freebsd"),
    not(miri)
));

/// The CPU a goroutine may run before its worker's slice timer asks it to
/// yield, matching the watchdog's wall-clock threshold elsewhere.
#[cfg(all(any(target_os = "linux", target_os = "freebsd"), not(miri)))]
const SLICE_NANOS: libc::c_long = 10_000_000;

/// `sigev_notify` for a signal to one thread: 4 in the Linux and FreeBSD
/// kernel ABIs, which some `libc` releases leave out for musl.
#[cfg(all(any(target_os = "linux", target_os = "freebsd"), not(miri)))]
const SIGEV_THREAD_ID: libc::c_int = 4;

/// A timer on the calling thread's CPU clock that delivers `SIGURG` to that
/// thread at every slice it runs, armed while the worker runs goroutines.
#[cfg(all(any(target_os = "linux", target_os = "freebsd"), not(miri)))]
pub struct SliceTimer {
    id: libc::timer_t,
}

#[cfg(all(any(target_os = "linux", target_os = "freebsd"), not(miri)))]
impl SliceTimer {
    /// A disarmed timer for the calling thread, or `None` when the kernel
    /// refuses one, which leaves the thread to cooperative yields alone.
    #[must_use]
    pub fn for_this_thread() -> Option<Self> {
        install_slice_handler();
        // SAFETY: a zeroed `sigevent` is a valid starting value; the fields
        // this call reads are set below.
        let mut event: libc::sigevent = unsafe { std::mem::zeroed() };
        event.sigev_notify = SIGEV_THREAD_ID;
        event.sigev_signo = libc::SIGURG;
        event.sigev_notify_thread_id = this_thread_id();
        let mut id: libc::timer_t = std::ptr::null_mut();
        // SAFETY: `event` and `id` are live locals the call reads and writes.
        let rc = unsafe {
            libc::timer_create(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut event, &raw mut id)
        };
        (rc == 0).then_some(Self { id })
    }

    /// Fires at every slice of CPU the thread runs from now on.
    pub fn arm(&self) {
        let slice = libc::timespec {
            tv_sec: 0,
            tv_nsec: SLICE_NANOS,
        };
        self.set(slice);
    }

    /// Stops the timer while the thread parks.
    pub fn disarm(&self) {
        self.set(libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        });
    }

    fn set(&self, every: libc::timespec) {
        let spec = libc::itimerspec {
            it_interval: every,
            it_value: every,
        };
        // SAFETY: `self.id` is a timer this thread created and has not
        // deleted, and `spec` outlives the call.
        unsafe { libc::timer_settime(self.id, 0, &raw const spec, std::ptr::null_mut()) };
    }
}

#[cfg(all(any(target_os = "linux", target_os = "freebsd"), not(miri)))]
impl Drop for SliceTimer {
    fn drop(&mut self) {
        // SAFETY: `self.id` came from `timer_create` and is deleted once.
        unsafe { libc::timer_delete(self.id) };
    }
}

/// The kernel id of the calling thread, which `SIGEV_THREAD_ID` names.
#[cfg(all(target_os = "linux", not(miri)))]
fn this_thread_id() -> libc::c_int {
    // SAFETY: `gettid` takes nothing and cannot fail.
    unsafe { libc::gettid() }
}

/// The kernel id of the calling thread, which `SIGEV_THREAD_ID` names.
#[cfg(all(target_os = "freebsd", not(miri)))]
fn this_thread_id() -> libc::lwpid_t {
    // SAFETY: `pthread_getthreadid_np` takes nothing and cannot fail.
    unsafe { libc::pthread_getthreadid_np() }
}

/// Installs, once, the `SIGURG` handler a slice timer delivers to.
#[cfg(all(any(target_os = "linux", target_os = "freebsd"), not(miri)))]
fn install_slice_handler() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        // SAFETY: a zeroed `sigaction` is a valid starting value; the
        // handler, flags, and mask are set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_slice_expired as extern "C" fn(libc::c_int) as usize;
        // Restarted calls keep a slice from surfacing as EINTR, and the
        // alternate stack keeps a handler on a goroutine stack's guard
        // margin from faulting.
        action.sa_flags = libc::SA_RESTART | libc::SA_ONSTACK;
        // SAFETY: `action.sa_mask` is a live field of `action`.
        unsafe { libc::sigemptyset(&raw mut action.sa_mask) };
        // SAFETY: `action` is fully set, and the handler only stores atomics.
        unsafe { libc::sigaction(libc::SIGURG, &raw const action, std::ptr::null_mut()) };
    });
}

/// Asks the running goroutine to yield at its next safepoint. Runs in signal
/// context: it only stores atomics, and the thread-local it reaches is
/// `const`-initialised with no destructor, so no lazy setup runs here.
#[cfg(all(any(target_os = "linux", target_os = "freebsd"), not(miri)))]
extern "C" fn on_slice_expired(_signal: libc::c_int) {
    LOCAL_YIELD.with(|flag| flag.store(true, Ordering::Release));
    PREEMPT_REQUESTED.store(1, Ordering::Release);
}

/// Asks the calling thread to reach a safepoint at its next opportunity.
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

/// Bumps the pending-pressure counter; called by the scheduler when it asks
/// an overrunning worker to reach a safepoint.
pub fn bump_pressure() {
    PENDING_PRESSURE.fetch_add(1, Ordering::Relaxed);
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
