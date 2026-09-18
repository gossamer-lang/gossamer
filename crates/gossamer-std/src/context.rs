//! Runtime support for `std::context` - request-scoped cancellation
//! and deadlines, modeled after Go's `context.Context`.
//!
//! A `Context` is a cheap-to-clone handle carrying a shared
//! cancellation flag plus an optional deadline. Parent/child
//! contexts share storage so cancelling a parent cancels every
//! descendant; children can also add their own deadline narrower
//! than the parent's.
//!
//! ## Cancellation propagation
//!
//! `Cancel::cancel_with` is **eager**: it walks every descendant
//! of the cancelled context, flips each one's cancelled flag,
//! drains each one's wait-list, and unparks the registered
//! goroutines via `crate::sched_global::scheduler().unpark`.
//! Cancel-aware blocking primitives (`time::sleep_ctx`,
//! `Channel::recv_ctx`, etc.) register the current goroutine's
//! `Gid` with the context's wait list before parking; on resume
//! (whether via the underlying I/O or via cancel-driven unpark)
//! they deregister and check `is_cancelled`. The pre-0.5.0 design
//! only flipped the atomic on cancel without waking parked
//! goroutines; under 0.5.0's additive `_ctx` variants the unpark
//! is mandatory.
//!
//! ## Deadlines
//!
//! [`crate::context::with_deadline`] schedules a one-shot timer (via
//! [`crate::time::after_func`]) that calls `Cancel::cancel_with`
//! when the deadline elapses. Deadlines are therefore *active*:
//! a goroutine parked on a `with_timeout` context wakes up on
//! the deadline even if nothing else observes the context. The
//! pre-0.5.0 design evaluated the deadline lazily on every
//! `is_cancelled()` read, so a parked goroutine never woke up
//! from deadline expiry.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once, Weak};
use std::time::Duration;

use gossamer_runtime::platform::Instant;
use parking_lot::Mutex;

use crate::errors::Error;
use crate::sched_global::Gid;

/// Installs the cross-crate context hooks in `gossamer-runtime`
/// the first time any `Context` is constructed. Idempotent.
/// The runtime's context-aware blocking primitives
/// (`gos_rt_chan_recv_ctx_option`) consult these to wake parked
/// goroutines on cancel without `gossamer-runtime` needing to
/// link against this crate.
#[allow(unsafe_code)]
fn ensure_hooks_installed() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: hook fn pointers have C-ABI signatures matching
        // the runtime's declared `CtxRegisterFn` /
        // `CtxDeregisterFn` / `CtxIsCancelledFn`. The runtime
        // never dereferences `ctx_handle` directly - only these
        // hooks do, and they downcast back to `&Inner` knowing
        // the handle came from a live `Arc<Inner>`.
        unsafe {
            gossamer_runtime::c_abi::install_ctx_hooks(
                ctx_register_hook,
                ctx_deregister_hook,
                ctx_is_cancelled_hook,
            );
        }
    });
}

/// Hook called by the runtime to register `gid` on the context
/// pointed to by `ctx_handle`. `ctx_handle` is the raw pointer
/// from `Arc::as_ptr(&ctx.inner)`; the caller must hold the
/// `Arc` alive across the runtime call so the pointer stays
/// valid.
#[allow(unsafe_code)]
unsafe extern "C" fn ctx_register_hook(ctx_handle: *const u8, gid: u32) {
    if ctx_handle.is_null() {
        return;
    }
    // SAFETY: the caller (`Channel::recv_ctx` etc.) holds an
    // Arc clone of the context inner for the duration of the
    // runtime call, so the pointer is live.
    // The handle was produced by `Arc::as_ptr(&ctx.inner)` so
    // the alignment is correct for `Inner`. Cast through the
    // pointee's exposed-but-correctly-aligned form.
    #[allow(clippy::cast_ptr_alignment)]
    let inner = unsafe { &*ctx_handle.cast::<Inner>() };
    if inner.cancelled.load(Ordering::Acquire) {
        return;
    }
    let mut waiters = inner.waiters.lock();
    let g = Gid(gid);
    if !waiters.contains(&g) {
        waiters.push(g);
    }
}

#[allow(
    unsafe_code,
    reason = "a C-ABI hook the runtime calls through a function pointer"
)]
unsafe extern "C" fn ctx_deregister_hook(ctx_handle: *const u8, gid: u32) {
    if ctx_handle.is_null() {
        return;
    }
    // The handle was produced by `Arc::as_ptr(&ctx.inner)` so
    // the alignment is correct for `Inner`. Cast through the
    // pointee's exposed-but-correctly-aligned form.
    #[allow(clippy::cast_ptr_alignment)]
    let inner = unsafe { &*ctx_handle.cast::<Inner>() };
    let mut waiters = inner.waiters.lock();
    let g = Gid(gid);
    if let Some(pos) = waiters.iter().position(|&w| w == g) {
        waiters.swap_remove(pos);
    }
}

#[allow(
    unsafe_code,
    reason = "a C-ABI hook the runtime calls through a function pointer"
)]
unsafe extern "C" fn ctx_is_cancelled_hook(ctx_handle: *const u8) -> i32 {
    if ctx_handle.is_null() {
        return 0;
    }
    // The handle was produced by `Arc::as_ptr(&ctx.inner)` so
    // the alignment is correct for `Inner`. Cast through the
    // pointee's exposed-but-correctly-aligned form.
    #[allow(clippy::cast_ptr_alignment)]
    let inner = unsafe { &*ctx_handle.cast::<Inner>() };
    i32::from(inner.cancelled.load(Ordering::Acquire))
}

/// Cancellation-aware blocking receive on a runtime channel
/// pointer. Wraps `gos_rt_chan_recv_ctx_option` with hook
/// installation + Arc lifetime management. Returns `Some(value)`
/// on a successful recv, `None` on close or context cancel.
///
/// `chan` must be a live `*mut GosChan` returned from
/// `gos_rt_chan_new`; the caller owns its lifetime (typically
/// matched by a `gos_rt_chan_close` or `gos_rt_chan_drop`).
#[allow(unsafe_code)]
#[must_use]
pub fn chan_recv_ctx_i64(chan: *mut u8, ctx: &Context) -> Option<i64> {
    ensure_hooks_installed();
    // Hold an Arc clone for the duration of the call so the
    // inner pointer we pass remains valid.
    let inner_arc = Arc::clone(&ctx.inner);
    let handle = Arc::as_ptr(&inner_arc).cast::<u8>();
    // SAFETY: `chan` and `handle` are passed through the C-ABI
    // surface; runtime owns the channel state, we hold the Arc
    // here so `handle` is live throughout. Result pointer is a
    // heap `GosResult` we free below.
    let raw = unsafe { gossamer_runtime::c_abi::gos_rt_chan_recv_ctx_option(chan.cast(), handle) };
    // Keep `inner_arc` alive past the call to be sure.
    drop(inner_arc);
    // `raw` is the 2-word by-value `Option`/`Result` (disc + payload); no box.
    if gossamer_runtime::c_abi::gos_rt_result_disc(raw) == 0 {
        Some(gossamer_runtime::c_abi::gos_rt_result_payload(raw))
    } else {
        None
    }
}

#[derive(Debug)]
struct Inner {
    cancelled: AtomicBool,
    deadline: Mutex<Option<Instant>>,
    reason: Mutex<Option<String>>,
    parent: Option<Context>,
    /// Goroutines parked on a cancel-aware primitive that
    /// nominated this context as their cancellation source.
    /// `Cancel::cancel_with` drains this list and unparks each
    /// registered goroutine. Empty for `Context::background()`.
    waiters: Mutex<Vec<Gid>>,
    /// Children whose cancellation should fire when this context
    /// cancels. Held as weak pointers so a descendant that is
    /// otherwise unreferenced (no outstanding `Cancel` handle, no
    /// active goroutine) can be dropped - `cancel_with` skips
    /// upgrade failures rather than treating them as errors.
    children: Mutex<Vec<Weak<Inner>>>,
}

/// Shared, reference-counted context handle.
#[derive(Debug, Clone)]
pub struct Context {
    inner: Arc<Inner>,
}

impl Context {
    /// Background context - never cancelled, no deadline. Use as the
    /// root of every request pipeline.
    #[must_use]
    pub fn background() -> Self {
        Self {
            inner: Arc::new(Inner {
                cancelled: AtomicBool::new(false),
                deadline: Mutex::new(None),
                reason: Mutex::new(None),
                parent: None,
                waiters: Mutex::new(Vec::new()),
                children: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Registers `gid` with this context's wait list. Idempotent;
    /// duplicate registrations are dropped. Called from
    /// cancel-aware blocking primitives **before** parking so a
    /// subsequent [`Cancel::cancel_with`] knows which goroutines
    /// to unpark.
    ///
    /// If the context is already cancelled at registration time,
    /// the gid is not added - the caller should check
    /// `is_cancelled` before parking.
    pub fn register_waiter(&self, gid: Gid) {
        if self.is_cancelled() {
            return;
        }
        let mut waiters = self.inner.waiters.lock();
        if !waiters.contains(&gid) {
            waiters.push(gid);
        }
    }

    /// Removes `gid` from this context's wait list. Idempotent.
    /// Called on resume from a cancel-aware park, regardless of
    /// whether the resume was driven by the underlying I/O or by
    /// cancel-driven unpark.
    pub fn deregister_waiter(&self, gid: Gid) {
        let mut waiters = self.inner.waiters.lock();
        if let Some(pos) = waiters.iter().position(|&w| w == gid) {
            waiters.swap_remove(pos);
        }
    }

    /// Returns a [`Done`] handle - a receive-only,
    /// channel-shaped surface that exposes the context's
    /// cancellation state.
    ///
    /// [`Done::try_recv`] checks the cancellation flag without
    /// blocking; [`Done::recv`] parks the calling goroutine
    /// until cancellation fires (the same mechanism as
    /// [`Context::wait`]). The handle is cheap - it carries a
    /// cloned [`Arc`] reference to the context's inner state
    /// and adds no per-call allocation.
    ///
    /// `done()` is the recommended primitive for goroutines
    /// whose only blocking dependency is cancellation. A future
    /// release will extend this surface to be `select`-arm
    /// compatible at the language level; today, the
    /// channel-shaped methods (`try_recv` / `recv`) are the API
    /// contract.
    #[must_use]
    pub fn done(&self) -> Done {
        Done { ctx: self.clone() }
    }

    /// Blocks until the context is cancelled. The current
    /// goroutine parks via the scheduler's `park_self_io`
    /// primitive; cancel drives the unpark via the wait-list.
    /// Returns the cancellation [`Error`].
    ///
    /// Use this when a goroutine has no other I/O to wait on but
    /// must observe cancellation directly. For a channel-shaped
    /// surface (with both blocking and non-blocking variants),
    /// see [`Context::done`].
    #[must_use]
    pub fn wait(&self) -> Error {
        if let Some(err) = self.err() {
            return err;
        }
        let gid = crate::sched_global::current_gid()
            .expect("Context::wait must be called from a goroutine");
        self.register_waiter(gid);
        // Re-check after registration: cancel may have fired
        // between the initial `err()` check and the registration.
        if self.is_cancelled() {
            self.deregister_waiter(gid);
            return self
                .err()
                .unwrap_or_else(|| Error::new("context cancelled"));
        }
        // Park via the scheduler's generic Io park reason. The
        // unpark side is `cancel_with` → `scheduler().unpark(gid)`.
        crate::sched_global::park(
            crate::sched_global::ParkReason::Io,
            |_parker| { /* no extra arm needed; cancel_with does the unpark */ },
        );
        self.deregister_waiter(gid);
        self.err()
            .unwrap_or_else(|| Error::new("context cancelled"))
    }

    /// Placeholder context - semantically identical to
    /// [`Self::background`] today, but marks call sites that should
    /// eventually thread a real context through.
    #[must_use]
    pub fn todo() -> Self {
        Self::background()
    }

    /// Returns `true` when this context or any ancestor has been
    /// cancelled, or when the deadline has passed.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        if self.inner.cancelled.load(Ordering::Acquire) {
            return true;
        }
        if let Some(deadline) = *self.inner.deadline.lock() {
            if Instant::now() >= deadline {
                return true;
            }
        }
        self.inner
            .parent
            .as_ref()
            .is_some_and(Context::is_cancelled)
    }

    /// Returns the cancellation reason if any.
    #[must_use]
    pub fn err(&self) -> Option<Error> {
        if !self.is_cancelled() {
            return None;
        }
        if let Some(reason) = self.inner.reason.lock().clone() {
            return Some(Error::new(reason));
        }
        // The timer-driven cancel_with may not have fired yet even though
        // is_cancelled() returned true via the deadline time-check. Emit the
        // canonical deadline message here so callers don't fall through to
        // parent.err() and get None (background parent is never cancelled).
        if let Some(deadline) = *self.inner.deadline.lock() {
            if Instant::now() >= deadline {
                return Some(Error::new("context deadline exceeded"));
            }
        }
        if let Some(parent) = &self.inner.parent {
            return parent.err();
        }
        Some(Error::new("context cancelled"))
    }

    /// Deadline of this context, honouring parent deadlines.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        let local = *self.inner.deadline.lock();
        match (
            local,
            self.inner.parent.as_ref().and_then(Context::deadline),
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }
}

/// `with_cancel(parent)` returns `(child, cancel)` - invoking
/// `cancel` cancels the child and every descendant.
#[must_use]
pub fn with_cancel(parent: &Context) -> (Context, Cancel) {
    let child_inner = Arc::new(Inner {
        cancelled: AtomicBool::new(false),
        deadline: Mutex::new(None),
        reason: Mutex::new(None),
        parent: Some(parent.clone()),
        waiters: Mutex::new(Vec::new()),
        children: Mutex::new(Vec::new()),
    });
    // Register the child with the parent so an ancestor cancel
    // walks down to the descendants' wait-lists.
    parent
        .inner
        .children
        .lock()
        .push(Arc::downgrade(&child_inner));
    let child = Context {
        inner: Arc::clone(&child_inner),
    };
    let cancel = Cancel { inner: child_inner };
    (child, cancel)
}

/// `with_deadline(parent, deadline)` returns a child context whose
/// `is_cancelled` flips `true` when `deadline` elapses. The
/// flip is **active**: a one-shot timer is scheduled that runs
/// `cancel_with("context deadline exceeded")` at the supplied
/// instant, so any goroutine parked on the context wakes up
/// without needing to poll `is_cancelled()` itself.
#[must_use]
pub fn with_deadline(parent: &Context, deadline: Instant) -> Context {
    let (ctx, cancel) = with_cancel(parent);
    *ctx.inner.deadline.lock() = Some(deadline);
    // Schedule the active deadline timer. The TimerHandle is
    // returned by `after_func` but we deliberately do not store
    // it: cancellation of the timer is unnecessary because
    // (a) the worst case is the timer fires after the context is
    // already cancelled, and `cancel_with` on an
    // already-cancelled context is a documented no-op for
    // observable state, and (b) keeping the handle would force
    // every `with_deadline` caller to track a drop-on-completion
    // shape. The cancel-handle's strong reference inside the
    // closure keeps the context alive long enough to fire.
    let now = Instant::now();
    let delay = if deadline > now {
        deadline - now
    } else {
        Duration::from_nanos(0)
    };
    // `crate::time::Duration` is a thin newtype over
    // `std::time::Duration`; the closure receives our local
    // type. The internal field is `pub(crate)`-equivalent: the
    // `from_micros` factory is the public construction path.
    let delay_micros: u64 = delay.as_micros().try_into().unwrap_or(u64::MAX);
    let _ = crate::time::after_func(
        crate::time::Duration::from_micros(delay_micros),
        move || {
            cancel.cancel_with("context deadline exceeded");
        },
    );
    ctx
}

/// `with_timeout(parent, dur)` returns a child context whose
/// deadline is `now + dur`.
#[must_use]
pub fn with_timeout(parent: &Context, duration: Duration) -> Context {
    with_deadline(parent, Instant::now() + duration)
}

/// Receive-only channel-shaped handle on a context's
/// cancellation state. Returned by [`Context::done`].
///
/// The two methods mirror Go's `<-chan struct{}` idiom:
///
/// - [`try_recv`][Done::try_recv] is the non-blocking
///   `select { case <-ctx.Done(): ... default: ... }` shape.
///   Returns `true` once cancellation has fired and stays
///   `true` thereafter.
/// - [`recv`][Done::recv] is the blocking
///   `<-ctx.Done()` shape. Parks the current goroutine via
///   the same wait-list mechanism as [`Context::wait`].
///
/// `Done` is cheap to clone - it holds a single `Arc`
/// reference to the context's inner state.
#[derive(Clone)]
pub struct Done {
    ctx: Context,
}

impl Done {
    /// Non-blocking check. Returns `true` once cancellation has
    /// fired.
    #[must_use]
    pub fn try_recv(&self) -> bool {
        self.ctx.is_cancelled()
    }

    /// Blocks the current goroutine until cancellation fires.
    /// Returns the cancellation [`Error`] (`"context cancelled"`,
    /// `"context deadline exceeded"`, or the explicit reason
    /// supplied to [`Cancel::cancel_with`]).
    #[must_use]
    pub fn recv(&self) -> Error {
        self.ctx.wait()
    }
}

/// Cancel handle returned by [`with_cancel`]. Dropping the handle
/// does **not** cancel the context; call [`cancel`][Cancel::cancel]
/// explicitly (mirrors Go's idiom).
pub struct Cancel {
    inner: Arc<Inner>,
}

impl Cancel {
    /// Cancels the associated context with the supplied reason.
    ///
    /// Cancellation walks every descendant: each descendant's
    /// cancelled flag flips and its wait-list is drained. Each
    /// drained `Gid` is unparked via the scheduler so a goroutine
    /// parked on a cancel-aware primitive resumes promptly. The
    /// walk is depth-first; descendants are discovered through the
    /// per-context `children` list. Repeated calls on the same
    /// cancel handle are no-ops on the cancelled state but each
    /// call still publishes its reason - the first reason wins.
    pub fn cancel_with(&self, reason: impl Into<String>) {
        propagate_cancel(&self.inner, reason.into());
    }

    /// Cancels the associated context with a generic reason.
    pub fn cancel(&self) {
        self.cancel_with("context cancelled");
    }
}

/// Recursively cancels `inner` and every descendant, draining
/// each context's wait-list and unparking the registered
/// goroutines.
fn propagate_cancel(inner: &Arc<Inner>, reason: String) {
    // CAS-style flip: only set the cancelled flag if it was not
    // already cancelled. The "first reason wins" rule means the
    // first call to publish a reason is the one stored.
    let was_cancelled = inner.cancelled.swap(true, Ordering::AcqRel);
    if !was_cancelled {
        let mut slot = inner.reason.lock();
        if slot.is_none() {
            *slot = Some(reason.clone());
        }
    }
    // Drain the wait-list and unpark each registered goroutine.
    // Doing this on every call (not just the first) is harmless:
    // after the flag flipped, no new waiter can register (see
    // `register_waiter`); subsequent drains are empty.
    let parked: Vec<Gid> = std::mem::take(&mut *inner.waiters.lock());
    if !parked.is_empty() {
        let scheduler = crate::sched_global::scheduler();
        for gid in parked {
            scheduler.unpark(gid);
        }
    }
    // Walk descendants. Drop dead weak refs as we go.
    let children: Vec<Weak<Inner>> = std::mem::take(&mut *inner.children.lock());
    for weak in children {
        if let Some(child) = weak.upgrade() {
            propagate_cancel(&child, reason.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_is_never_cancelled() {
        let ctx = Context::background();
        assert!(!ctx.is_cancelled());
        assert!(ctx.err().is_none());
    }

    #[test]
    fn cancel_flags_child_and_descendants() {
        let root = Context::background();
        let (child, cancel) = with_cancel(&root);
        let (grandchild, _) = with_cancel(&child);
        cancel.cancel_with("done");
        assert!(child.is_cancelled());
        assert!(grandchild.is_cancelled());
        let err = grandchild.err().unwrap();
        assert!(err.message().contains("done"));
    }

    #[test]
    fn deadline_expires_context() {
        let root = Context::background();
        let ctx = with_timeout(&root, Duration::from_millis(10));
        gossamer_runtime::platform::sleep(Duration::from_millis(20));
        assert!(ctx.is_cancelled());
    }

    #[test]
    fn child_deadline_is_earliest_of_chain() {
        let root = Context::background();
        let parent_deadline = Instant::now() + Duration::from_mins(1);
        let parent = with_deadline(&root, parent_deadline);
        let child = with_timeout(&parent, Duration::from_millis(5));
        let deadline = child.deadline().unwrap();
        assert!(deadline <= parent_deadline);
    }
}
