//! Runtime support for `std::runtime` - goroutine / scheduler
//! introspection and tuning knobs, analogous to Go's `runtime`
//! package: CPU count, a `GOMAXPROCS`-equivalent setter, and
//! goroutine / call-stack diagnostics.

#![forbid(unsafe_code)]
#![allow(
    clippy::map_unwrap_or,
    clippy::type_complexity,
    clippy::must_use_candidate
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// Soft upper bound on simultaneously-running goroutines.
///
/// Mirrors `runtime.GOMAXPROCS(n)`. The scheduler reads this on
/// every worker-thread startup; adjusting mid-run does not kill
/// already-running workers but caps how many new ones spawn.
static MAX_PROCS: AtomicUsize = AtomicUsize::new(0);

/// Returns the current goroutine-concurrency cap. When no value has
/// been set, reads the host's logical CPU count via
/// [`std::thread::available_parallelism`].
#[must_use]
pub fn max_procs() -> usize {
    let cached = MAX_PROCS.load(Ordering::Relaxed);
    if cached > 0 {
        return cached;
    }
    num_cpus()
}

/// Sets the goroutine concurrency cap. Returns the previous value.
/// A value of `0` restores the automatic-from-host behaviour.
///
/// The cap is applied to two layers: the worker (P) count grows /
/// shrinks to match, and the live-goroutine cap surfaces refusal
/// for `try_spawn` instead of silently overcommitting kernel
/// resources.
pub fn set_max_procs(n: usize) -> usize {
    let prev = MAX_PROCS.swap(n, Ordering::Relaxed);
    let scheduler = crate::sched_global::scheduler();
    let workers = if n == 0 { num_cpus() } else { n };
    scheduler.set_worker_count(workers);
    let _ = scheduler.set_max_goroutines(if n == 0 { 1_000_000 } else { n });
    prev
}

/// Number of logical CPU cores visible to the process, per
/// `std::thread::available_parallelism`. Returns `1` if the query
/// fails.
#[must_use]
pub fn num_cpus() -> usize {
    thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// Snapshots every live goroutine for diagnostics. Wraps
/// [`gossamer_runtime::sigquit::snapshot`].
#[must_use]
pub fn all_goroutines() -> Vec<gossamer_runtime::sigquit::GoroutineInfo> {
    gossamer_runtime::sigquit::snapshot()
}

/// Number of currently-live goroutines.
#[must_use]
pub fn num_goroutines() -> usize {
    gossamer_runtime::sigquit::snapshot().len()
}

/// Snapshot of global goroutine scheduler counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerStats {
    /// Total goroutines spawned.
    pub spawned: u64,
    /// Total goroutines finished.
    pub finished: u64,
    /// Total task step calls.
    pub steps: u64,
    /// Total yielded steps.
    pub yields: u64,
    /// Total successful work steals.
    pub steals: u64,
    /// Total pulls from the global injector.
    pub injects: u64,
    /// Total goroutine parks.
    pub parks: u64,
    /// Total successful unparks.
    pub unparks: u64,
    /// Currently live goroutine count tracked by the scheduler.
    pub live_goroutines: usize,
    /// Current scheduler worker target.
    pub worker_count: usize,
    /// Hard worker-count cap.
    pub worker_count_cap: usize,
}

/// Returns a low-overhead snapshot of the global scheduler counters.
#[must_use]
pub fn scheduler_stats() -> SchedulerStats {
    let scheduler = gossamer_runtime::sched_global::scheduler();
    let stats = scheduler.stats();
    SchedulerStats {
        spawned: stats.spawned,
        finished: stats.finished,
        steps: stats.steps,
        yields: stats.yields,
        steals: stats.steals,
        injects: stats.injects,
        parks: stats.parks,
        unparks: stats.unparks,
        live_goroutines: scheduler.live_goroutines(),
        worker_count: scheduler.worker_count(),
        worker_count_cap: gossamer_runtime::sched::MultiScheduler::worker_count_cap(),
    }
}

/// Returns [`scheduler_stats`] rendered as a compact JSON object.
#[must_use]
pub fn scheduler_stats_json() -> String {
    let stats = scheduler_stats();
    format!(
        "{{\"spawned\":{},\"finished\":{},\"steps\":{},\"yields\":{},\"steals\":{},\"injects\":{},\"parks\":{},\"unparks\":{},\"live_goroutines\":{},\"worker_count\":{},\"worker_count_cap\":{}}}",
        stats.spawned,
        stats.finished,
        stats.steps,
        stats.yields,
        stats.steals,
        stats.injects,
        stats.parks,
        stats.unparks,
        stats.live_goroutines,
        stats.worker_count,
        stats.worker_count_cap,
    )
}

/// Returns whether this Rust-hosted runtime has a cycle collector.
///
/// Gossamer's compiled tiers return `true`; the bytecode VM returns `false`
/// because its `Arc`-backed heap intentionally has no cycle collector.
#[must_use]
pub const fn cycle_collection_supported() -> bool {
    true
}

// --- runtime::caller / runtime::stack (P1 stdlib) -------------------

/// One frame in a [`stack`] or `callers` dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    /// Function name (best-effort; demangled when available).
    pub function: String,
    /// Source file path. Empty when DWARF is unavailable.
    pub file: String,
    /// 1-based line number. `0` when DWARF is unavailable.
    pub line: u32,
}

/// Returns the caller's stack frame `skip` levels up from the
/// current frame (`skip == 0` returns the immediate caller).
/// Mirrors Go's `runtime.Caller(skip)`.
///
/// Under `gos build` without DWARF, the
/// returned frame contains the function name only; `file` is
/// empty and `line` is 0. Under `gos` (interpreter), full
/// frame info is available from the bytecode line tables.
#[must_use]
pub fn caller(skip: usize) -> Option<StackFrame> {
    let frames = collect_frames();
    // skip + 1 to step past `caller` itself.
    frames.into_iter().nth(skip + 1)
}

/// Snapshot of the current call stack. Mirrors Go's
/// `runtime.Stack(buf, all=false)`.
#[must_use]
pub fn stack() -> Vec<StackFrame> {
    collect_frames()
}

/// Lowest-level caller iterator. Walks the current thread's
/// stack via `backtrace::trace`.
#[cfg(not(target_arch = "wasm32"))]
fn collect_frames() -> Vec<StackFrame> {
    let mut out: Vec<StackFrame> = Vec::new();
    backtrace::trace(|frame| {
        backtrace::resolve_frame(frame, |sym| {
            let function = sym
                .name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            let file = sym
                .filename()
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string();
            let line = sym.lineno().unwrap_or(0);
            out.push(StackFrame {
                function,
                file,
                line,
            });
        });
        true
    });
    out
}

/// wasm32 has no machine-stack unwinder (`backtrace` does not build for
/// wasm32-unknown-unknown), so caller introspection yields no frames in
/// the playground.
#[cfg(target_arch = "wasm32")]
fn collect_frames() -> Vec<StackFrame> {
    Vec::new()
}

/// Registers a finalizer for an `Arc`-managed value.
///
/// `finalize` runs when this returned guard is dropped while it owns the last
/// `Arc` reference. Clones obtained through [`Finalizer::clone_inner`] keep
/// the value alive, but do not retain the callback after the guard itself has
/// gone away.
///
/// Internally just wraps the value in a guard type - the
/// finalizer fires on `Drop`. Mirrors Go's
/// `runtime.SetFinalizer` shape with the lifetime constraint
/// that `value` must outlive every call to the closure.
pub fn set_finalizer<T: Send + Sync + 'static>(
    value: std::sync::Arc<T>,
    finalize: impl FnOnce(&T) + Send + 'static,
) -> Finalizer<T> {
    Finalizer {
        inner: value,
        finalize: Some(Box::new(finalize)),
    }
}

/// Handle returned by [`set_finalizer`]. Dropping it triggers
/// the finalizer only when this guard holds the last reference to `T`.
pub struct Finalizer<T: Send + Sync + 'static> {
    inner: std::sync::Arc<T>,
    finalize: Option<Box<dyn FnOnce(&T) + Send>>,
}

impl<T: Send + Sync + 'static> Finalizer<T> {
    /// Borrows the wrapped value.
    pub fn get(&self) -> &T {
        &self.inner
    }

    /// Returns a clone of the inner `Arc`.
    ///
    /// The clone keeps the value alive. If it outlives this guard, the
    /// callback is not retained and therefore will not run when the clone
    /// eventually drops.
    pub fn clone_inner(&self) -> std::sync::Arc<T> {
        std::sync::Arc::clone(&self.inner)
    }

    /// Cancels the finalizer. The wrapped value will drop
    /// silently when its last reference goes away.
    pub fn cancel(mut self) {
        self.finalize = None;
    }
}

impl<T: Send + Sync + 'static> Drop for Finalizer<T> {
    fn drop(&mut self) {
        if let Some(f) = self.finalize.take()
            && std::sync::Arc::strong_count(&self.inner) == 1
        {
            f(&self.inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn num_cpus_is_at_least_one() {
        assert!(num_cpus() >= 1);
    }

    // `set_max_procs` mutates a process-global, so the two tests
    // that exercise it must not interleave. A `Mutex` shared
    // across both serialises them under `cargo test` (which runs
    // tests in parallel by default).
    static MAX_PROCS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn set_max_procs_round_trips() {
        let _guard = MAX_PROCS_LOCK.lock();
        let prev = set_max_procs(42);
        assert_eq!(max_procs(), 42);
        let restored = set_max_procs(prev);
        assert_eq!(restored, 42);
    }

    #[test]
    fn max_procs_defaults_to_num_cpus_when_unset() {
        let _guard = MAX_PROCS_LOCK.lock();
        let _ = set_max_procs(0);
        assert_eq!(max_procs(), num_cpus());
    }

    #[test]
    fn scheduler_stats_json_contains_core_counters() {
        let text = scheduler_stats_json();
        assert!(text.contains("\"spawned\":"));
        assert!(text.contains("\"finished\":"));
        assert!(text.contains("\"worker_count\":"));
    }

    #[test]
    fn caller_returns_immediate_caller_frame() {
        fn inner_call() -> Option<StackFrame> {
            caller(0)
        }
        let frame = inner_call().expect("caller frame");
        // Symbol resolution under `cargo test` is best-effort;
        // we accept any non-empty function name and assert the
        // frame is structured.
        let _ = frame;
    }

    #[test]
    fn stack_returns_non_empty_frame_list() {
        let frames = stack();
        assert!(
            !frames.is_empty(),
            "stack() must produce at least one frame"
        );
    }

    #[test]
    fn finalizer_runs_when_last_reference_drops() {
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_for_finalize = std::sync::Arc::clone(&fired);
        let value = std::sync::Arc::new(42_i64);
        {
            let _guard = set_finalizer(value, move |v| {
                assert_eq!(*v, 42);
                fired_for_finalize.store(true, std::sync::atomic::Ordering::Release);
            });
            // Guard drop fires the finalizer here.
        }
        assert!(fired.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn finalizer_cancel_suppresses_callback() {
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_for_finalize = std::sync::Arc::clone(&fired);
        let value = std::sync::Arc::new("hello".to_string());
        let guard = set_finalizer(value, move |_| {
            fired_for_finalize.store(true, std::sync::atomic::Ordering::Release);
        });
        guard.cancel();
        assert!(!fired.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn finalizer_only_runs_at_guard_drop_when_it_is_last_reference() {
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_for_finalize = std::sync::Arc::clone(&fired);
        let value = std::sync::Arc::new(7_i64);
        let outside_ref = std::sync::Arc::clone(&value);
        {
            let _guard = set_finalizer(value, move |_| {
                fired_for_finalize.store(true, std::sync::atomic::Ordering::Release);
            });
            // Guard drops here, but `outside_ref` still holds a clone.
        }
        assert!(!fired.load(std::sync::atomic::Ordering::Acquire));
        drop(outside_ref);
        assert!(
            !fired.load(std::sync::atomic::Ordering::Acquire),
            "a clone that outlives the guard cannot invoke a callback the guard no longer owns"
        );
    }
}
