//! Stackful coroutines for Gossamer goroutines.
//!
//! Wraps [`corosensei`] in just enough surface area to make every
//! Gossamer `go fn(args)` a real stackful coroutine: an OS-thread
//! worker (the M) resumes a coroutine; the coroutine runs user code;
//! when user code calls [`suspend`], control returns to the worker,
//! which can pick up another goroutine. The coroutine's stack is
//! preserved between resumes so the function can pick up exactly
//! where it left off.
//!
//! The crate is deliberately thin - it does not own scheduling,
//! parking semantics, or wakeup wiring. Those live in
//! `gossamer-runtime::sched` / `sched_global`. This crate only
//! exposes:
//!
//! - [`Goroutine`] - owns a `corosensei::Coroutine` plus a stable
//!   pointer to its [`corosensei::Yielder`].
//! - [`suspend`] - yields the currently running goroutine via a
//!   thread-local pointer to its yielder. The scheduler's worker
//!   loop sets this pointer before each resume.
//!
//! ## Send / Sync
//!
//! `corosensei::Coroutine` is `!Send` by default to guard against
//! TLS-binding accidents. Gossamer's M:N scheduler explicitly
//! migrates goroutines across worker threads, so [`Goroutine`]
//! provides an `unsafe impl Send`. The contract is:
//!
//! - User code inside a goroutine **must not assume** any TLS slot
//!   is preserved across [`suspend`] calls. If user code stashes
//!   thread-local state, suspend, and the goroutine resumes on a
//!   different worker, the TLS read returns the new worker's slot.
//!   This is the same constraint Go imposes on goroutines.
//! - The coroutine's saved register state and stack are
//!   pure-data and trivially `Send`.

#![forbid(unsafe_op_in_unsafe_fn)]

use std::cell::Cell;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicPtr, AtomicUsize};

/// Payload type for Gossamer-originated panics. Raised by the
/// runtime's `gos_rt_panic` on the goroutine path and recognised by
/// the process panic hook (which stays silent for it - the Gossamer
/// report has already been printed) and by the catch in
/// [`Goroutine::new`].
pub struct GosPanic(pub String);

/// Set to `true` whenever any spawned goroutine has panicked.
/// Tests / runtime status helpers read this flag via
/// [`any_goroutine_panicked`] to assert that long-running services
/// finished without hitting the M9 isolation path.
static GOROUTINE_PANICKED: AtomicBool = AtomicBool::new(false);

/// Returns `true` if any goroutine has panicked since this
/// process started. The flag is sticky - once set it stays set,
/// so a test that spawns multiple goroutines can check it after
/// the full batch joins.
#[must_use]
pub fn any_goroutine_panicked() -> bool {
    GOROUTINE_PANICKED.load(Ordering::Acquire)
}

thread_local! {
    /// `true` while the worker thread is running the body of a `spawn`ed
    /// (joinable) goroutine, `false` for a fire-and-forget `go`. A panic in a
    /// joinable body is OBSERVED through its `join()` handle, so `gos_rt_panic`
    /// suppresses its eager `error[GX0005]` report and lets the outcome guard
    /// deliver `Err` instead - matching the VM, whose `spawn`+`join` path is
    /// silent. Set on body entry and restored on exit (including the panic
    /// unwind) via [`JoinableScope`], so a synchronous panic - the dominant
    /// shape - reads the correct value.
    static IN_JOINABLE_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Returns `true` when the current worker is executing a joinable (`spawn`)
/// goroutine body, so a panic here is observed through `join()`.
#[must_use]
pub fn in_joinable_spawn() -> bool {
    IN_JOINABLE_SPAWN.with(std::cell::Cell::get)
}

/// Scopes `IN_JOINABLE_SPAWN` to a goroutine body, restoring the previous
/// value on drop (so the flag is reset even when the body unwinds, and nested
/// `go`-inside-`spawn` bodies see their own value).
pub struct JoinableScope(bool);

impl JoinableScope {
    /// Enters a goroutine body, marking it joinable (`spawn`) or not (`go`).
    #[must_use]
    pub fn enter(joinable: bool) -> Self {
        let prev = IN_JOINABLE_SPAWN.with(|c| c.replace(joinable));
        Self(prev)
    }
}

impl Drop for JoinableScope {
    fn drop(&mut self) {
        IN_JOINABLE_SPAWN.with(|c| c.set(self.0));
    }
}

/// Installs `joinable` as the running body's flag and answers the one it
/// replaces. A worker swaps a goroutine's own value in around each resume, so
/// goroutines sharing the worker never see one another's.
#[must_use]
pub fn swap_joinable_spawn(joinable: bool) -> bool {
    IN_JOINABLE_SPAWN.with(|c| c.replace(joinable))
}

#[cfg(not(target_arch = "wasm32"))]
use corosensei::stack::DefaultStack;
#[cfg(not(target_arch = "wasm32"))]
use corosensei::{Coroutine, CoroutineResult, Yielder};

/// Default goroutine stack size in bytes (1 MiB). Override via the
/// `GOSSAMER_GOROUTINE_STACK` environment variable, parsed at the
/// first [`Goroutine::new`] call after process start.
///
/// 1 MiB is generous compared to Go's 8 KiB starting size, but Go
/// grows stacks on demand via segmented + relocating allocation;
/// our stacks are fixed. On 64-bit hosts the cost is virtual address
/// space (cheap) - `mmap`'s on-demand committing keeps RSS
/// proportional to *actual* depth used. 10 000 goroutines eat
/// ~10 GiB of address space and typically tens of MiB of committed
/// RAM. Compiled-tier code (HTTP handlers, JSON parsing, regex
/// captures, format!) routinely uses tens of KiB of stack frames,
/// so the previous 16 KiB default overflowed under real workloads
/// and corrupted adjacent heap mappings.
pub const DEFAULT_STACK_BYTES: usize = 1024 * 1024;

/// Minimum allowed stack size in bytes (32 KiB). Overrides smaller
/// than this are clamped up - anything less risks overflowing into
/// the guard page from a single function prologue.
pub const MIN_STACK_BYTES: usize = 32 * 1024;

/// Reads the configured goroutine stack size. Honours
/// `GOSSAMER_GOROUTINE_STACK` (parsed once and cached). Values
/// below [`MIN_STACK_BYTES`] are clamped up so a stray override
/// can't reintroduce the heap-corruption bug.
#[must_use]
pub fn stack_size() -> usize {
    use std::sync::OnceLock;
    static SIZE: OnceLock<usize> = OnceLock::new();
    *SIZE.get_or_init(|| {
        std::env::var("GOSSAMER_GOROUTINE_STACK")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .map_or(DEFAULT_STACK_BYTES, |n| n.max(MIN_STACK_BYTES))
    })
}

/// Headroom reserved below the stack limit by the byte-budget
/// recursion guard, in bytes. Must comfortably exceed one deep
/// interpreter frame plus the error-construction / unwind path that
/// runs *after* the guard fires. 256 KiB leaves room for several fat
/// frames before the real guard page.
pub const STACK_GUARD_MARGIN: usize = 256 * 1024;

thread_local! {
    /// Stack address captured at a shallow point (goroutine body
    /// entry, or program start for the main thread) as the baseline
    /// for the byte-budget recursion guard. `0` means unarmed -
    /// callers fall back to a frame count.
    static STACK_ORIGIN: Cell<usize> = const { Cell::new(0) };
    /// Bytes of stack growth allowed past [`STACK_ORIGIN`] before the
    /// guard trips. Set alongside the origin.
    static STACK_BUDGET: Cell<usize> = const { Cell::new(0) };
}

/// Current stack pointer, approximated by the address of a stack
/// local. Stacks grow downward on every platform Gossamer targets, so
/// a larger value is a shallower stack.
#[inline(never)]
#[must_use]
pub fn current_stack_ptr() -> usize {
    let probe = 0u8;
    std::ptr::from_ref(&probe) as usize
}

/// Arms the byte-budget recursion guard on the current thread:
/// records the current (shallow) stack pointer as the origin and the
/// bytes of growth allowed past it before [`stack_guard_tripped`]
/// fires. Call at a shallow point - goroutine body entry (`budget =
/// stack_size() - STACK_GUARD_MARGIN`) or program start.
pub fn arm_stack_guard(budget: usize) {
    STACK_ORIGIN.with(|o| o.set(current_stack_ptr()));
    STACK_BUDGET.with(|b| b.set(budget));
}

/// Overwrites the guard origin/budget and returns the previous pair,
/// so [`Goroutine::resume`] can re-arm a migrated goroutine on its
/// new worker and restore the worker's prior state afterward.
#[cfg(not(target_arch = "wasm32"))]
fn set_stack_guard(origin: usize, budget: usize) -> (usize, usize) {
    let prev = (STACK_ORIGIN.with(Cell::get), STACK_BUDGET.with(Cell::get));
    STACK_ORIGIN.with(|o| o.set(origin));
    STACK_BUDGET.with(|b| b.set(budget));
    prev
}

/// Whether the byte-budget guard is armed on this thread. When it is,
/// recursion-depth checks should consult [`stack_guard_tripped`]
/// (precise, frame-fatness-aware) rather than a frame count.
#[must_use]
pub fn stack_guard_armed() -> bool {
    STACK_ORIGIN.with(Cell::get) != 0
}

/// Returns `true` when the armed guard's stack has grown past its
/// budget - the caller must stop recursing and raise a clean
/// stack-overflow error rather than let the native stack overflow
/// (which aborts the whole process, fatal on a 1 MiB goroutine
/// stack). Always `false` when unarmed.
#[must_use]
pub fn stack_guard_tripped() -> bool {
    let origin = STACK_ORIGIN.with(Cell::get);
    if origin == 0 {
        return false;
    }
    let budget = STACK_BUDGET.with(Cell::get);
    origin.saturating_sub(current_stack_ptr()) > budget
}

/// `(lo, hi)` byte bounds of the stack the calling code is running on
/// when that stack is a goroutine's, or `None` on a bare OS thread.
///
/// A goroutine runs on its own allocated stack, so the OS thread's
/// bounds do not describe it. The guard already records the shallow
/// origin and the growth budget, which is exactly that window.
#[must_use]
pub fn goroutine_stack_bounds() -> Option<(usize, usize)> {
    let origin = STACK_ORIGIN.with(Cell::get);
    if origin == 0 {
        return None;
    }
    let budget = STACK_BUDGET.with(Cell::get);
    Some((origin.saturating_sub(budget), origin))
}

/// Unarms the guard on this thread, so a caller that has just tripped
/// it can run its reporting and unwind path with the guard's own margin
/// to work in rather than tripping again on every frame.
pub fn disarm_stack_guard() {
    STACK_ORIGIN.with(|o| o.set(0));
    STACK_BUDGET.with(|b| b.set(0));
}

/// Stackful coroutine that runs a single Gossamer goroutine to
/// completion across one or more `resume()` calls.
#[cfg(not(target_arch = "wasm32"))]
pub struct Goroutine {
    coro: Coroutine<(), (), (), DefaultStack>,
    yielder_slot: Arc<AtomicPtr<()>>,
    /// Stack origin captured at body entry, published so
    /// [`Self::resume`] can re-arm the byte-budget guard when this
    /// goroutine migrates to a different worker thread. `0` until the
    /// first resume runs the body.
    stack_origin_slot: Arc<AtomicUsize>,
}

// SAFETY: the coroutine's stack and saved register state are
// plain data. Migration across OS threads is deliberate: the
// Gossamer M:N scheduler's worker pool steals goroutines off
// peer deques. User code is documented to not rely on
// TLS-stable-across-yield semantics.
#[cfg(not(target_arch = "wasm32"))]
unsafe impl Send for Goroutine {}

#[cfg(not(target_arch = "wasm32"))]
impl Goroutine {
    /// Constructs a new goroutine whose entry point is `main`. The
    /// goroutine does not start running until [`Self::resume`] is
    /// called.
    ///
    /// # Panics
    ///
    /// Panics if the coroutine stack cannot be allocated. Callers that can
    /// report the failure instead of aborting the process should use
    /// [`Self::try_new`]: a host at its mapping limit fails here for every
    /// subsequent goroutine, and a panic gives the scheduler nothing to say
    /// about why.
    #[must_use]
    pub fn new(main: Box<dyn FnOnce() + Send + 'static>) -> Self {
        Self::try_new(main).expect("alloc goroutine stack")
    }

    /// Constructs a new goroutine, returning the mapping error instead of
    /// panicking when its stack cannot be allocated.
    ///
    /// # Errors
    ///
    /// Returns the underlying `mmap` failure - typically `ENOMEM` from an
    /// exhausted address space or from the per-process mapping limit
    /// (`vm.max_map_count`), which a stack's guard page makes a program hit at
    /// roughly half that many live goroutines.
    pub fn try_new(main: Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<Self> {
        let yielder_slot: Arc<AtomicPtr<()>> = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
        let yielder_slot_clone = Arc::clone(&yielder_slot);
        let stack_origin_slot: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let stack_origin_slot_clone = Arc::clone(&stack_origin_slot);
        let stack = DefaultStack::new(stack_size())?;
        let coro = Coroutine::with_stack(stack, move |yielder: &Yielder<(), ()>, ()| {
            // The yielder is a stack value with an address that is
            // stable for the lifetime of the coroutine. Two writes
            // happen on first entry:
            //
            // 1. `yielder_slot` - published so subsequent resumes
            //    (which the scheduler initiates from a worker M
            //    that may differ from this one) can read the
            //    pointer and re-arm the worker's TLS_YIELDER.
            // 2. `set_current_yielder` - bootstrap value for *this*
            //    first resume, so `suspend()` can find the yielder
            //    before the worker had a chance to set TLS itself.
            let ptr = std::ptr::from_ref::<Yielder<(), ()>>(yielder)
                .cast::<()>()
                .cast_mut();
            yielder_slot_clone.store(ptr, Ordering::Release);
            set_current_yielder(ptr);
            // Arm the byte-budget recursion guard at the shallowest
            // point of this goroutine's stack, and publish the origin
            // so a post-migration `resume` can re-arm the new worker.
            let origin = current_stack_ptr();
            let budget = stack_size().saturating_sub(STACK_GUARD_MARGIN);
            stack_origin_slot_clone.store(origin, Ordering::Release);
            let _ = set_stack_guard(origin, budget);
            // contain panics inside the
            // goroutine body so they don't propagate to the
            // scheduler's `resume()` and abort the worker (and
            // the process). The goroutine state machine on the
            // outside observes the goroutine as `done` and the
            // global panicked-flag flips so test programs can
            // assert clean execution.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(main));
            if let Err(payload) = result {
                // The panic's `error[GX0005]` report was already printed
                // eagerly by `gos_rt_panic` for an unobserved goroutine (and
                // suppressed for a joinable one, whose `join()` handle delivers
                // the error instead). Nothing to print here - just record that
                // a goroutine panicked and isolate it from the scheduler.
                let _ = &payload;
                GOROUTINE_PANICKED.store(true, Ordering::Release);
            }
        });
        Ok(Self {
            coro,
            yielder_slot,
            stack_origin_slot,
        })
    }

    /// Returns the yielder pointer for this goroutine, or null if
    /// the coroutine has not yet been resumed for the first time.
    /// The scheduler's worker loop reads this and pushes it into
    /// thread-local state before calling [`Self::resume`].
    #[must_use]
    pub fn yielder_ptr(&self) -> *mut () {
        self.yielder_slot.load(Ordering::Acquire)
    }

    /// Resumes execution of the goroutine. Returns `true` when the
    /// goroutine has completed (its entry function returned).
    /// Returns `false` when the goroutine called [`suspend`] -
    /// the caller should re-resume later when the wakeup event
    /// fires.
    ///
    /// # Panics
    ///
    /// Panics if the goroutine itself panics; the panic is propagated
    /// to the caller. (Suspended-then-resumed coroutines do not panic
    /// from the resume site itself.)
    pub fn resume(&mut self) -> bool {
        // Re-arm the byte-budget guard for this goroutine before
        // switching to its stack: it may resume on a different worker
        // than the one that first ran (and armed) it. The body arms
        // the guard itself on the very first resume (origin still 0
        // here). Restore the worker's prior state afterward.
        let origin = self.stack_origin_slot.load(Ordering::Acquire);
        let restore = (origin != 0)
            .then(|| set_stack_guard(origin, stack_size().saturating_sub(STACK_GUARD_MARGIN)));
        let done = match self.coro.resume(()) {
            CoroutineResult::Yield(()) => false,
            CoroutineResult::Return(()) => true,
        };
        if let Some((o, b)) = restore {
            let _ = set_stack_guard(o, b);
        }
        done
    }

    /// Returns whether the goroutine has finished.
    #[must_use]
    pub fn done(&self) -> bool {
        self.coro.done()
    }
}

/// Cooperative eager goroutine shim for single-threaded wasm32, where
/// stackful coroutines are unavailable (no native stack switching).
/// The body runs to completion on the first [`Self::resume`]; a body
/// that reaches a blocking point calls [`suspend`], which is
/// unsupported here and raises a clean error. This is the wasm
/// playground's documented concurrency limit: `go`/`spawn` bodies that
/// run to completion without blocking on another goroutine work; true
/// interleaving needs the native runtime.
#[cfg(target_arch = "wasm32")]
pub struct Goroutine {
    body: Option<Box<dyn FnOnce() + Send + 'static>>,
    done: bool,
}

#[cfg(target_arch = "wasm32")]
impl Goroutine {
    /// Constructs a goroutine whose entry point is `main`. It does not
    /// run until [`Self::resume`].
    #[must_use]
    pub fn new(main: Box<dyn FnOnce() + Send + 'static>) -> Self {
        Self {
            body: Some(main),
            done: false,
        }
    }

    /// Yielder pointer is meaningless without stackful coroutines.
    #[must_use]
    pub fn yielder_ptr(&self) -> *mut () {
        std::ptr::null_mut()
    }

    /// Runs the body to completion and returns `true` (done). A panic
    /// inside the body is contained and recorded, matching the native
    /// goroutine isolation contract.
    pub fn resume(&mut self) -> bool {
        if let Some(body) = self.body.take() {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            if result.is_err() {
                GOROUTINE_PANICKED.store(true, Ordering::Release);
            }
            self.done = true;
        }
        true
    }

    /// Returns whether the goroutine has finished.
    #[must_use]
    pub fn done(&self) -> bool {
        self.done
    }
}

thread_local! {
    /// Pointer to the [`Yielder`] of the goroutine currently
    /// running on this OS thread. Set by the scheduler's worker
    /// loop immediately before each `resume()` call; cleared after.
    /// Code paths that want to suspend the calling goroutine read
    /// this and call `suspend()` on the yielder.
    static CURRENT_YIELDER: Cell<*mut ()> = const { Cell::new(std::ptr::null_mut()) };
}

/// Sets the thread-local current-yielder pointer. Called by the
/// scheduler's worker loop before resuming a goroutine.
pub fn set_current_yielder(ptr: *mut ()) {
    CURRENT_YIELDER.with(|c| c.set(ptr));
}

/// Clears the thread-local current-yielder pointer. Called by the
/// scheduler's worker loop after a resume returns.
pub fn clear_current_yielder() {
    CURRENT_YIELDER.with(|c| c.set(std::ptr::null_mut()));
}

/// Returns whether the calling thread is currently executing inside
/// a goroutine. Equivalent to `current_yielder().is_some()`.
#[must_use]
pub fn in_goroutine() -> bool {
    CURRENT_YIELDER.with(|c| !c.get().is_null())
}

/// Suspends the goroutine currently running on this OS thread.
/// Control returns to the scheduler at the resume site; the
/// goroutine becomes runnable again only when the scheduler's
/// `unpark(gid)` is called by whatever the goroutine was waiting
/// on (a channel, a poller readiness, a mutex release, ...).
///
/// # Panics
///
/// Panics if called from outside a goroutine (i.e. when the
/// thread-local current-yielder pointer is null). This is a
/// programming error: stdlib code that may suspend the calling
/// goroutine must check [`in_goroutine`] first if it can be
/// invoked from a non-goroutine thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn suspend() {
    let ptr = CURRENT_YIELDER.with(Cell::get);
    assert!(
        !ptr.is_null(),
        "gossamer_coro::suspend() called outside a goroutine context",
    );
    // SAFETY: the scheduler's worker loop sets this pointer to the
    // yielder of the goroutine currently executing on this OS
    // thread, and clears it after the resume returns. `suspend()`
    // is therefore only ever called between matching set/clear
    // calls, while the pointed-to yielder's coroutine is alive on
    // the worker's stack.
    let yielder: &Yielder<(), ()> = unsafe { &*ptr.cast::<Yielder<(), ()>>() };
    yielder.suspend(());
}

/// Suspension is unavailable on single-threaded wasm32 (no stackful
/// coroutines). A goroutine that reaches a blocking point raises this
/// rather than deadlocking the browser tab. The wasm runtime maps it
/// to a Gossamer panic surfaced in the playground output.
#[cfg(target_arch = "wasm32")]
pub fn suspend() {
    panic!(
        "goroutine blocking/suspension is not supported in the Gossamer wasm playground; \
         run concurrent programs with `gos` locally",
    );
}

// `corosensei` allocates each goroutine's stack via `mmap(PROT_NONE)` for
// the guard page, then `mprotect` to make it readable. Miri doesn't
// simulate guard-page protection toggles, so the entire goroutine test
// suite is gated behind `not(miri)`. Runtime-level coroutine coverage
// stays exercised by the host-CPU tests; Miri continues to check the
// non-coroutine paths (channels, schedulers, GC, FFI argument coercion).
#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn coroutine_runs_to_completion() {
        let trace: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let trace_for_main = Arc::clone(&trace);
        let mut g = Goroutine::new(Box::new(move || {
            trace_for_main.lock().unwrap().push("a");
        }));
        // Worker shim: stash yielder, resume, clear.
        set_current_yielder(g.yielder_ptr());
        let done = g.resume();
        clear_current_yielder();
        assert!(done);
        assert_eq!(*trace.lock().unwrap(), vec!["a"]);
    }

    #[test]
    fn coroutine_suspends_and_resumes() {
        let trace: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let trace_for_main = Arc::clone(&trace);
        let mut g = Goroutine::new(Box::new(move || {
            trace_for_main.lock().unwrap().push("a");
            suspend();
            trace_for_main.lock().unwrap().push("b");
            suspend();
            trace_for_main.lock().unwrap().push("c");
        }));
        // First resume: runs until first suspend.
        set_current_yielder(g.yielder_ptr());
        assert!(!g.resume());
        clear_current_yielder();
        assert_eq!(*trace.lock().unwrap(), vec!["a"]);
        // Second resume: runs until second suspend.
        set_current_yielder(g.yielder_ptr());
        assert!(!g.resume());
        clear_current_yielder();
        assert_eq!(*trace.lock().unwrap(), vec!["a", "b"]);
        // Third resume: runs to completion.
        set_current_yielder(g.yielder_ptr());
        assert!(g.resume());
        clear_current_yielder();
        assert_eq!(*trace.lock().unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn in_goroutine_returns_true_inside_running_coroutine() {
        let observation: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let observation_for_main = Arc::clone(&observation);
        let mut g = Goroutine::new(Box::new(move || {
            *observation_for_main.lock().unwrap() = Some(in_goroutine());
        }));
        assert!(!in_goroutine());
        set_current_yielder(g.yielder_ptr());
        let _ = g.resume();
        clear_current_yielder();
        assert_eq!(*observation.lock().unwrap(), Some(true));
        assert!(!in_goroutine());
    }
}
