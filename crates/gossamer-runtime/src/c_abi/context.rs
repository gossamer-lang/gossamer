#![allow(clippy::missing_safety_doc)]
#![allow(clippy::cast_sign_loss)]

//! Runtime support for `std::context` - request-scoped cancellation
//! and deadlines, modeled after Go's `context.Context`.
//!
//! This is the standalone all-tier handle surface:
//! `background` / `with_cancel` / `with_timeout` constructors plus the
//! `cancel` / `is_cancelled` / `done` methods. Cancellation is eager
//! down the tree (a `cancel` flips every descendant's flag) and
//! `is_cancelled` also walks up the parent chain and honours an
//! optional deadline. Deadlines use `std::time::Instant` plus a small
//! timer thread that drives the same cancellation path as explicit
//! cancel, so `done_chan()` is selectable on timeout.
//!
//! The closure-returning `with_cancel -> (ctx, cancel)` shape from the
//! library `gossamer_std::context` is intentionally out of scope here:
//! this handle exposes `cancel` as a direct method on the context, and
//! `done` is a non-blocking cancellation check. `done_chan`
//! (`gos_rt_ctx_cancelled`) returns a channel the cancel walk closes,
//! so cancellation is observable from a `select` arm: a parked select
//! is unparked when the channel closes, and a closed channel's recv
//! arm is always ready. A deadline (`with_timeout`) actively closes the
//! done channel through the normal cancel walk.
//!
//! Node state lives in a process-global registry keyed by the node's
//! address, and the handle a compiled tier carries as an `i64` is that
//! address. Every operation resolves the address through the registry
//! and holds an `Arc` for the length of the call, so a node stays alive
//! while it is in use and is reclaimed once cancellation removes it and
//! the last in-flight operation returns. Parent / child links are the
//! same addresses, which keeps the struct `Send + Sync` without an
//! `unsafe impl` and makes a link to a reclaimed node resolve as
//! cancelled rather than dangle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;

use super::chan::GosChan;
use crate::platform::Instant;

/// Opaque heap handle for a context node.
pub struct GosCtx {
    cancelled: AtomicBool,
    deadline: Option<Instant>,
    /// Parent node address as `usize`, or `0` for a root context.
    parent: usize,
    /// Child node addresses; `cancel` walks these depth-first.
    children: Mutex<Vec<usize>>,
    /// Address of this node's "done" channel as `usize`, or `0` until
    /// one is asked for. A channel outlives every node that could hold
    /// it, so it is minted only for a context that actually selects on
    /// cancellation. Stored as `usize` (not a raw pointer) so `GosCtx`
    /// stays `Send + Sync` without an `unsafe impl`.
    chan: Mutex<usize>,
    /// Goroutines parked in a cancellation-aware wait on this context.
    /// Cancelling unparks them so each re-checks its own condition.
    parked_waiters: Mutex<Vec<crate::sched::Gid>>,
}

/// Records `gid` as parked on `addr`, so cancelling that context wakes it.
pub(crate) fn register_waiter(addr: usize, gid: crate::sched::Gid) {
    if let Some(node) = ctx_at(addr) {
        node.parked_waiters.lock().push(gid);
    }
}

/// Drops `gid` from `addr`'s parked set.
pub(crate) fn deregister_waiter(addr: usize, gid: crate::sched::Gid) {
    if let Some(node) = ctx_at(addr) {
        node.parked_waiters.lock().retain(|x| *x != gid);
    }
}

/// Whether the context at `addr` is cancelled or past its deadline. The
/// cancellation-aware runtime entry points consult this for handles minted
/// by a compiled program, which reach the runtime's own node registry.
pub(crate) fn addr_is_cancelled(addr: usize) -> bool {
    addr != 0 && node_is_cancelled(addr)
}

/// The channel handed to `done_chan()` for a context whose cancellation
/// already ran. It is born closed, so a recv arm on it is ready at once -
/// the only thing a cancelled context's done channel ever reports.
static RETIRED_CHAN: LazyLock<usize> = LazyLock::new(|| {
    let chan = super::chan::gos_rt_chan_new(8, 0);
    if !chan.is_null() {
        // SAFETY: `chan` was just allocated and is non-null here.
        super::chan::chan_close_idempotent(unsafe { &*chan });
    }
    chan as usize
});

/// Live nodes by address. An address absent here names a context whose
/// cancellation already ran.
static NODES: LazyLock<Mutex<HashMap<usize, Arc<GosCtx>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn ctx_at(addr: usize) -> Option<Arc<GosCtx>> {
    if addr == 0 {
        return None;
    }
    NODES.lock().get(&addr).cloned()
}

fn alloc_ctx(deadline: Option<Instant>, parent: usize) -> *mut GosCtx {
    let node = Arc::new(GosCtx {
        cancelled: AtomicBool::new(false),
        deadline,
        parent,
        children: Mutex::new(Vec::new()),
        chan: Mutex::new(0),
        parked_waiters: Mutex::new(Vec::new()),
    });
    let child = Arc::as_ptr(&node).cast_mut();
    let addr = child as usize;
    NODES.lock().insert(addr, node);
    if let Some(p) = ctx_at(parent) {
        p.children.lock().push(addr);
    }
    // A parent that finished cancelling before this link was made never
    // reaches the child through its own walk, so the child takes the
    // ancestry's state at birth.
    if parent != 0 && node_is_cancelled(parent) {
        cancel_node(addr);
    }
    if let Some(deadline) = deadline {
        // The deadline rides the scheduler's timer wheel: the netpoller already
        // wakes on the earliest one, so a context costs an entry there rather
        // than an OS thread parked on a sleep.
        let gid = crate::sched_global::add_timer(deadline);
        crate::sched_global::register_waker(gid, Box::new(move || cancel_node(addr)));
    }
    child
}

/// Closes the node's done channel idempotently, if one was ever minted.
/// A node cancelled before anything asked for its channel records the
/// cancellation in `cancelled`, and `done_chan_of` mints the channel
/// closed when it is finally asked for.
fn close_done_chan(node: &GosCtx) {
    let chan = *node.chan.lock();
    if chan == 0 {
        return;
    }
    // SAFETY: the address came from `gos_rt_chan_new`; channels are
    // never freed, so the pointer is valid for the process lifetime.
    let chan = unsafe { &*(chan as *const GosChan) };
    super::chan::chan_close_idempotent(chan);
}

/// The node's done channel, minting one on first use. A channel for a
/// context already cancelled is born closed, so a `select` recv arm on
/// it is ready immediately.
fn done_chan_of(node: &GosCtx) -> *mut GosChan {
    let mut slot = node.chan.lock();
    if *slot == 0 {
        let fresh = super::chan::gos_rt_chan_new(8, 0);
        *slot = fresh as usize;
        if !fresh.is_null() && node.cancelled.load(Ordering::Acquire) {
            // SAFETY: `fresh` was just allocated and is non-null here.
            super::chan::chan_close_idempotent(unsafe { &*fresh });
        }
    }
    *slot as *mut GosChan
}

/// Contexts belonging to requests currently in flight.
///
/// Shutdown cancels every one, so a handler that watches its context
/// learns the process is going down at the same moment the accept loop
/// stops taking new work.
static LIVE_REQUESTS: LazyLock<Mutex<Vec<usize>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Opens a request-scoped context, optionally with a deadline, and
/// records it as in flight.
#[must_use]
pub fn open_request_context(timeout_ms: u64) -> usize {
    let deadline = (timeout_ms != 0).then(|| Instant::now() + Duration::from_millis(timeout_ms));
    open_request_context_at(deadline)
}

/// Opens a request context that expires at an already-computed instant.
///
/// The deadline a request is served under starts when the request does, so a
/// context created later in that request - the first time its handler asks
/// for one - still expires when the request's own deadline says.
#[must_use]
pub fn open_request_context_at(deadline: Option<Instant>) -> usize {
    let addr = alloc_ctx(deadline, 0) as usize;
    LIVE_REQUESTS.lock().push(addr);
    addr
}

/// Cancels a request's context and stops tracking it.
pub fn close_request_context(addr: usize) {
    LIVE_REQUESTS.lock().retain(|live| *live != addr);
    cancel_node(addr);
}

/// Cancels every in-flight request's context. Called when shutdown
/// begins, so a handler watching its context can stop early rather than
/// hold the drain to its deadline.
pub fn cancel_live_requests() {
    for addr in std::mem::take(&mut *LIVE_REQUESTS.lock()) {
        cancel_node(addr);
    }
}

/// `context::Context::background()` - a root context, never cancelled,
/// no deadline.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_ctx_background() -> *mut GosCtx {
    ffi_entry!(std::ptr::null_mut(), { alloc_ctx(None, 0) })
}

/// `context::Context::with_cancel(parent)` - a child whose
/// cancellation can be triggered via `cancel`; cancelling an ancestor
/// also cancels it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_with_cancel(parent: *mut GosCtx) -> *mut GosCtx {
    ffi_entry!(std::ptr::null_mut(), { alloc_ctx(None, parent as usize) })
}

/// `context::Context::with_timeout(parent, millis)` - a child whose
/// `is_cancelled` flips `true` once `millis` have elapsed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_with_timeout(parent: *mut GosCtx, millis: i64) -> *mut GosCtx {
    ffi_entry!(std::ptr::null_mut(), {
        let deadline = Instant::now() + Duration::from_millis(millis.max(0) as u64);
        alloc_ctx(Some(deadline), parent as usize)
    })
}

/// Cancels `addr` and every descendant, then retires each from the
/// registry. The walk carries its own stack so a deep context chain
/// costs heap rather than call frames, and each child list is copied
/// out before its node is cancelled so the tree lock is never held
/// across the cancel of a child. Retiring the root of the walk from its
/// parent's child list keeps a long-lived parent's list proportional to
/// the children still live under it.
fn cancel_node(addr: usize) {
    if let Some(node) = ctx_at(addr)
        && let Some(parent) = ctx_at(node.parent)
    {
        parent.children.lock().retain(|child| *child != addr);
    }
    let mut pending = vec![addr];
    while let Some(current) = pending.pop() {
        let Some(node) = ctx_at(current) else {
            continue;
        };
        node.cancelled.store(true, Ordering::Release);
        close_done_chan(&node);
        for gid in std::mem::take(&mut *node.parked_waiters.lock()) {
            crate::sched_global::scheduler().unpark(gid);
        }
        let kids: Vec<usize> = node.children.lock().clone();
        pending.extend(kids);
        NODES.lock().remove(&current);
    }
}

/// `ctx.cancel()` - cancel this context and every descendant.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_cancel(ctx: *mut GosCtx) {
    ffi_entry!((), {
        if !ctx.is_null() {
            cancel_node(ctx as usize);
        }
    });
}

/// Whether `addr` or any ancestor is cancelled or past its deadline. The
/// chain is walked iteratively so ancestry depth costs heap rather than
/// call frames, and an address the registry no longer holds names a
/// context whose cancellation already ran.
fn node_is_cancelled(addr: usize) -> bool {
    let mut current = addr;
    while current != 0 {
        let Some(node) = ctx_at(current) else {
            return true;
        };
        if node.cancelled.load(Ordering::Acquire) {
            return true;
        }
        if let Some(deadline) = node.deadline
            && Instant::now() >= deadline
        {
            return true;
        }
        current = node.parent;
    }
    false
}

/// `ctx.is_cancelled()` - `1` when this context (or an ancestor) is
/// cancelled or its deadline has passed, else `0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_is_cancelled(ctx: *mut GosCtx) -> i64 {
    ffi_entry!(0, {
        if ctx.is_null() {
            return 0;
        }
        i64::from(node_is_cancelled(ctx as usize))
    })
}

/// `ctx.done()` - non-blocking cancellation check; identical to
/// `is_cancelled`. For the `select`-arm channel form, see
/// `gos_rt_ctx_cancelled` / `ctx.done_chan()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_done(ctx: *mut GosCtx) -> i64 {
    ffi_entry!(0, {
        if ctx.is_null() {
            return 0;
        }
        i64::from(node_is_cancelled(ctx as usize))
    })
}

/// `ctx.done_chan()` - returns the context's "done" channel as a
/// receive endpoint. `cancel` (this context or any ancestor) closes
/// the channel, so a `select { _ = ctx.done_chan().recv() => … }` arm
/// fires on cancellation via closed-channel select readiness. Returns
/// the same channel on every call for a given context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_cancelled(ctx: *mut GosCtx) -> *mut GosChan {
    ffi_entry!(std::ptr::null_mut(), {
        let Some(node) = ctx_at(ctx as usize) else {
            return *RETIRED_CHAN as *mut GosChan;
        };
        done_chan_of(&node)
    })
}
