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
//! optional deadline. A deadline rides the scheduler's timer wheel and
//! drives the same cancellation path as an explicit cancel, so
//! `done_chan()` is selectable on timeout.
//!
//! `done` is a non-blocking cancellation check. `done_chan`
//! (`gos_rt_ctx_cancelled`) returns a channel the cancel walk closes,
//! so cancellation is observable from a `select` arm: a parked select
//! is unparked when the channel closes, and a closed channel's recv
//! arm is always ready.
//!
//! A handle is a counted node holding one share of its context, and
//! compiled code gives each holder a share of the handle, so a context
//! lives exactly as long as something holds it: a handle, or a child,
//! which holds its parent so the ancestry it reads stays alive. A parent
//! holds its children weakly. A held handle therefore always names the
//! context it was minted for.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;

use parking_lot::Mutex;

use super::chan::GosChan;
use crate::platform::Instant;

/// A context handle as the runtime stores it: the address of a [`GosCtx`]
/// node, or `0` for none. Compiled code passes the same handle as a
/// `*mut GosCtx`.
pub type CtxHandle = usize;

/// `handle` as the pointer compiled code carries.
fn as_ptr(handle: CtxHandle) -> *mut GosCtx {
    std::ptr::with_exposed_provenance_mut(handle)
}

/// One context of the tree.
struct CtxNode {
    /// Key of this node in its parent's child set and the live-request set.
    id: u64,
    cancelled: AtomicBool,
    deadline: Option<Instant>,
    /// The deadline's armed timer, disarmed when the node is cancelled or
    /// dropped first.
    timer: Mutex<Option<crate::sched_global::TimerHandle>>,
    parent: Option<Arc<CtxNode>>,
    /// Live children by id; `cancel` walks these depth-first.
    children: Mutex<HashMap<u64, Weak<CtxNode>>>,
    /// Address of this node's "done" channel as `usize`, or `0` until
    /// one is asked for. A channel outlives every node that could hold
    /// it, so it is minted only for a context that actually selects on
    /// cancellation.
    chan: Mutex<usize>,
    /// Goroutines parked in a cancellation-aware wait on this context.
    /// Cancelling unparks them so each re-checks its own condition.
    parked_waiters: Mutex<Vec<crate::sched::Gid>>,
    /// OS threads waiting on this context among other sources. Cancelling
    /// raises them, as it unparks the goroutines above.
    wakers: crate::wake::WakerSet,
}

impl Drop for CtxNode {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.get_mut().take() {
            crate::sched_global::cancel_timer(timer);
        }
        if let Some(parent) = &self.parent {
            parent.children.lock().remove(&self.id);
        }
        let chan = *self.chan.get_mut();
        // SAFETY: a minted done channel is a counted node this context holds
        // one share of, given back here.
        unsafe { super::chan::chan_release(chan as *mut GosChan) };
    }
}

/// The payload of a context handle: one share of its context.
pub struct GosCtx {
    node: Arc<CtxNode>,
}

super::rc::managed_handle!(GosCtx);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The done channel of the absent context: born closed, so a recv arm on it
/// is ready at once, as on a context already cancelled.
static NO_CONTEXT_CHAN: LazyLock<usize> = LazyLock::new(|| {
    let chan = super::chan::gos_rt_chan_new(8, 0);
    if !chan.is_null() {
        // SAFETY: `chan` was just allocated and is non-null here.
        super::chan::chan_close_idempotent(unsafe { &*chan });
    }
    chan as usize
});

/// The context `handle` names, or `None` for `0`.
///
/// # Safety
///
/// `handle` is `0` or a live handle the caller holds a share of.
unsafe fn node_of(handle: CtxHandle) -> Option<Arc<CtxNode>> {
    if handle == 0 {
        return None;
    }
    let ctx = std::ptr::with_exposed_provenance::<GosCtx>(handle);
    // SAFETY: the caller holds a share of the live handle, so its payload
    // is a `GosCtx` that stays alive for this read.
    Some(Arc::clone(unsafe { &(*ctx).node }))
}

/// Records `gid` as parked on `handle`, so cancelling that context wakes it.
pub(crate) fn register_waiter(handle: CtxHandle, gid: crate::sched::Gid) {
    // SAFETY: the cancellation-aware waits pass the live handle their caller holds.
    if let Some(node) = unsafe { node_of(handle) } {
        node.parked_waiters.lock().push(gid);
    }
}

/// Drops `gid` from `handle`'s parked set.
pub(crate) fn deregister_waiter(handle: CtxHandle, gid: crate::sched::Gid) {
    // SAFETY: the cancellation-aware waits pass the live handle their caller holds.
    if let Some(node) = unsafe { node_of(handle) } {
        node.parked_waiters.lock().retain(|x| *x != gid);
    }
}

/// Runs `waker` when the context `handle` names is cancelled.
pub(crate) fn watch(handle: CtxHandle, waker: &std::task::Waker) {
    // SAFETY: the cancellation-aware waits pass the live handle their caller holds.
    if let Some(node) = unsafe { node_of(handle) } {
        node.wakers.register(waker);
    }
}

/// A check answering whether the context `handle` names is still live. It
/// holds the context, so it stays sound after the caller's share is gone.
pub(crate) fn not_cancelled(handle: CtxHandle) -> Arc<dyn Fn() -> bool + Send + Sync> {
    // SAFETY: the cancellation-aware waits pass the live handle their caller holds.
    let node = unsafe { node_of(handle) };
    Arc::new(move || node.as_ref().is_none_or(|node| !node_is_cancelled(node)))
}

/// Withdraws a registration made by [`watch`].
pub(crate) fn unwatch(handle: CtxHandle, waker: &std::task::Waker) {
    // SAFETY: the cancellation-aware waits pass the live handle their caller holds.
    if let Some(node) = unsafe { node_of(handle) } {
        node.wakers.deregister(waker);
    }
}

/// Whether the context `handle` names is cancelled or past its deadline.
/// The cancellation-aware runtime entry points consult this for handles
/// minted by a compiled program.
pub(crate) fn handle_is_cancelled(handle: CtxHandle) -> bool {
    // SAFETY: the cancellation-aware waits pass the live handle their caller holds.
    unsafe { node_of(handle) }.is_some_and(|node| node_is_cancelled(&node))
}

/// Builds a context under `parent` and answers a handle holding one share.
fn alloc_ctx(deadline: Option<Instant>, parent: Option<Arc<CtxNode>>) -> CtxHandle {
    let node = Arc::new(CtxNode {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        cancelled: AtomicBool::new(false),
        deadline,
        timer: Mutex::new(None),
        parent,
        children: Mutex::new(HashMap::new()),
        chan: Mutex::new(0),
        parked_waiters: Mutex::new(Vec::new()),
        wakers: crate::wake::WakerSet::new(),
    });
    if let Some(parent) = &node.parent {
        parent
            .children
            .lock()
            .insert(node.id, Arc::downgrade(&node));
        // A parent that finished cancelling before this link was made never
        // reaches the child through its own walk, so the child takes the
        // ancestry's state at birth.
        if node_is_cancelled(parent) {
            cancel_node(&node);
        }
    }
    if let Some(deadline) = deadline {
        // The deadline rides the scheduler's timer wheel: the netpoller already
        // wakes on the earliest one, so a context costs an entry there rather
        // than an OS thread parked on a sleep.
        let mut slot = node.timer.lock();
        // A node cancelled before its timer is armed needs none; one cancelled
        // after waits on this lock and then disarms what was stored.
        if !node.cancelled.load(Ordering::Acquire) {
            let weak = Arc::downgrade(&node);
            *slot = Some(crate::sched_global::add_timer(
                deadline,
                Box::new(move || {
                    if let Some(node) = weak.upgrade() {
                        cancel_node(&node);
                    }
                }),
            ));
        }
    }
    super::rc::alloc_managed(GosCtx { node }).expose_provenance()
}

/// Closes the node's done channel idempotently, if one was ever minted.
/// A node cancelled before anything asked for its channel records the
/// cancellation in `cancelled`, and `done_chan_of` mints the channel
/// closed when it is finally asked for.
fn close_done_chan(node: &CtxNode) {
    let chan = *node.chan.lock();
    if chan == 0 {
        return;
    }
    // SAFETY: the node holds a share of its done channel for as long as it
    // lives, and the caller reaches the node.
    let chan = unsafe { &*(chan as *const GosChan) };
    super::chan::chan_close_idempotent(chan);
}

/// The node's done channel, minting one on first use; the node holds one
/// share of it. A channel for a context already cancelled is born closed,
/// so a `select` recv arm on it is ready immediately.
fn done_chan_of(node: &CtxNode) -> *mut GosChan {
    let mut slot = node.chan.lock();
    if *slot == 0 {
        let fresh = super::chan::gos_rt_chan_new(8, 0);
        *slot = fresh as usize;
        if !fresh.is_null() && node_is_cancelled(node) {
            // SAFETY: `fresh` was just allocated and is non-null here.
            super::chan::chan_close_idempotent(unsafe { &*fresh });
        }
    }
    *slot as *mut GosChan
}

/// Contexts belonging to requests currently in flight, by id.
///
/// Shutdown cancels every one, so a handler that watches its context
/// learns the process is going down at the same moment the accept loop
/// stops taking new work.
static LIVE_REQUESTS: LazyLock<Mutex<HashMap<u64, Weak<CtxNode>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Opens a request-scoped context, optionally with a deadline, and
/// records it as in flight. The handle holds one share, which
/// [`close_request_context`] gives back.
#[must_use]
pub fn open_request_context(timeout_ms: u64) -> CtxHandle {
    let deadline = (timeout_ms != 0).then(|| Instant::now() + Duration::from_millis(timeout_ms));
    open_request_context_at(deadline)
}

/// Opens a request context that expires at an already-computed instant.
///
/// The deadline a request is served under starts when the request does, so a
/// context created later in that request - the first time its handler asks
/// for one - still expires when the request's own deadline says.
#[must_use]
pub fn open_request_context_at(deadline: Option<Instant>) -> CtxHandle {
    let handle = alloc_ctx(deadline, None);
    // SAFETY: `handle` was just minted and this function holds its share.
    if let Some(node) = unsafe { node_of(handle) } {
        LIVE_REQUESTS.lock().insert(node.id, Arc::downgrade(&node));
    }
    handle
}

/// Cancels a request's context, stops tracking it, and gives back the share
/// [`open_request_context`] handed out.
pub fn close_request_context(handle: CtxHandle) {
    // SAFETY: the caller passes the handle it opened and still holds.
    if let Some(node) = unsafe { node_of(handle) } {
        LIVE_REQUESTS.lock().remove(&node.id);
        cancel_node(&node);
    }
    release_handle(handle);
}

/// Cancels the context `handle` names. The caller keeps its share.
pub fn cancel_handle(handle: CtxHandle) {
    // SAFETY: the caller passes a live handle it holds a share of.
    if let Some(node) = unsafe { node_of(handle) } {
        cancel_node(&node);
    }
}

/// Takes one more share of `handle`, for a second holder.
pub fn retain_handle(handle: CtxHandle) {
    if handle != 0 {
        // SAFETY: the caller holds a share of the live handle.
        unsafe { super::rc::gos_rt_rc_retain(std::ptr::with_exposed_provenance_mut(handle)) };
    }
}

/// Gives back one share of `handle`.
pub fn release_handle(handle: CtxHandle) {
    if handle != 0 {
        // SAFETY: the caller holds the share it gives back here.
        unsafe { super::rc::gos_rt_rc_release(std::ptr::with_exposed_provenance_mut(handle)) };
    }
}

/// Cancels every in-flight request's context. Called when shutdown
/// begins, so a handler watching its context can stop early rather than
/// hold the drain to its deadline.
pub fn cancel_live_requests() {
    for node in std::mem::take(&mut *LIVE_REQUESTS.lock()).into_values() {
        if let Some(node) = node.upgrade() {
            cancel_node(&node);
        }
    }
}

/// A root context nothing cancels, shared by every request that has no
/// context of its own. It holds its own share for the life of the process,
/// so it is handed out without one.
// Requests are served only where the HTTP modules build, which wasm does not.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn shared_background() -> CtxHandle {
    static BACKGROUND: LazyLock<CtxHandle> = LazyLock::new(|| alloc_ctx(None, None));
    *BACKGROUND
}

/// `context::Context::background()` - a root context, never cancelled,
/// no deadline.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_ctx_background() -> *mut GosCtx {
    ffi_entry!(std::ptr::null_mut(), { as_ptr(alloc_ctx(None, None)) })
}

/// `context::Context::with_cancel(parent)` - a child whose
/// cancellation can be triggered via `cancel`; cancelling an ancestor
/// also cancels it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_with_cancel(parent: *mut GosCtx) -> *mut GosCtx {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `parent` is null or a handle the caller holds.
        as_ptr(alloc_ctx(None, unsafe {
            node_of(parent.expose_provenance())
        }))
    })
}

/// `context::Context::with_timeout(parent, millis)` - a child whose
/// `is_cancelled` flips `true` once `millis` have elapsed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_with_timeout(parent: *mut GosCtx, millis: i64) -> *mut GosCtx {
    ffi_entry!(std::ptr::null_mut(), {
        let deadline = Instant::now() + Duration::from_millis(millis.max(0) as u64);
        // SAFETY: `parent` is null or a handle the caller holds.
        as_ptr(alloc_ctx(Some(deadline), unsafe {
            node_of(parent.expose_provenance())
        }))
    })
}

/// Cancels `root` and every descendant, disarming each pending deadline.
/// The walk carries its own stack so a deep context chain costs heap
/// rather than call frames, and each child set is copied out before its
/// node is cancelled so no tree lock is held across the cancel of a child.
fn cancel_node(root: &Arc<CtxNode>) {
    let mut pending = vec![Arc::clone(root)];
    while let Some(node) = pending.pop() {
        if node.cancelled.swap(true, Ordering::AcqRel) {
            continue;
        }
        if let Some(timer) = node.timer.lock().take() {
            crate::sched_global::cancel_timer(timer);
        }
        close_done_chan(&node);
        for gid in std::mem::take(&mut *node.parked_waiters.lock()) {
            crate::sched_global::scheduler().unpark(gid);
        }
        node.wakers.wake_all();
        let kids: Vec<Weak<CtxNode>> = node.children.lock().values().cloned().collect();
        pending.extend(kids.iter().filter_map(Weak::upgrade));
    }
}

/// `ctx.cancel()` - cancel this context and every descendant.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_cancel(ctx: *mut GosCtx) {
    ffi_entry!((), { cancel_handle(ctx.expose_provenance()) });
}

/// Whether `node` or any ancestor is cancelled or past its deadline. The
/// chain is walked iteratively so ancestry depth costs no call frames.
fn node_is_cancelled(node: &CtxNode) -> bool {
    let mut current = Some(node);
    while let Some(node) = current {
        if node.cancelled.load(Ordering::Acquire) {
            return true;
        }
        if let Some(deadline) = node.deadline
            && Instant::now() >= deadline
        {
            return true;
        }
        current = node.parent.as_deref();
    }
    false
}

/// `ctx.is_cancelled()` - `1` when this context (or an ancestor) is
/// cancelled or its deadline has passed, else `0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_is_cancelled(ctx: *mut GosCtx) -> i64 {
    ffi_entry!(0, {
        i64::from(handle_is_cancelled(ctx.expose_provenance()))
    })
}

/// `ctx.done()` - non-blocking cancellation check; identical to
/// `is_cancelled`. For the `select`-arm channel form, see
/// `gos_rt_ctx_cancelled` / `ctx.done_chan()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_done(ctx: *mut GosCtx) -> i64 {
    ffi_entry!(0, {
        i64::from(handle_is_cancelled(ctx.expose_provenance()))
    })
}

/// `ctx.done_chan()` - returns the context's "done" channel as a
/// receive endpoint. `cancel` (this context or any ancestor) closes
/// the channel, so a `select { _ = ctx.done_chan().recv() => ... }` arm
/// fires on cancellation via closed-channel select readiness. Returns
/// the same channel on every call for a given context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ctx_cancelled(ctx: *mut GosCtx) -> *mut GosChan {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `ctx` is null or a handle the caller holds.
        let chan = match unsafe { node_of(ctx.expose_provenance()) } {
            Some(node) => done_chan_of(&node),
            None => *NO_CONTEXT_CHAN as *mut GosChan,
        };
        // The caller gets a share of its own; the context, or the static for
        // the absent one, keeps its share.
        if !chan.is_null() {
            // SAFETY: `chan` is a live channel the context or the static holds.
            super::chan::chan_retain(unsafe { &*chan });
        }
        chan
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(ptr: *mut GosCtx) -> CtxHandle {
        ptr.expose_provenance()
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arms netpoller timers; Miri can't
    fn a_cancelled_handle_stays_cancelled_and_never_reaches_a_later_context() {
        let root = gos_rt_ctx_background();
        // SAFETY: every handle here is one this test minted and still holds.
        unsafe {
            let old = gos_rt_ctx_with_cancel(root);
            gos_rt_ctx_cancel(old);
            for _ in 0..10_000 {
                let fresh = gos_rt_ctx_with_cancel(root);
                assert_ne!(fresh, old);
                assert_eq!(gos_rt_ctx_is_cancelled(old), 1);
                gos_rt_ctx_cancel(old);
                assert_eq!(gos_rt_ctx_is_cancelled(fresh), 0);
                release_handle(h(fresh));
            }
            release_handle(h(old));
            release_handle(h(root));
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arms netpoller timers; Miri can't
    fn a_context_and_its_deadline_go_with_its_last_share() {
        let root = gos_rt_ctx_background();
        // SAFETY: every handle here is one this test minted and still holds.
        unsafe {
            let timed = gos_rt_ctx_with_timeout(root, 3_600_000);
            let node = node_of(h(timed)).expect("a live handle");
            let weak = Arc::downgrade(&node);
            let timer = (*node.timer.lock()).expect("a timed context arms a timer");
            drop(node);
            assert_eq!(node_of(h(root)).expect("root").children.lock().len(), 1);
            release_handle(h(timed));
            assert!(
                weak.upgrade().is_none(),
                "the context outlived its last handle"
            );
            assert!(!crate::sched_global::timer_is_armed(timer));
            assert!(node_of(h(root)).expect("root").children.lock().is_empty());
            release_handle(h(root));
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arms netpoller timers; Miri can't
    fn a_child_keeps_its_ancestry_alive_and_cancellable() {
        let root = gos_rt_ctx_background();
        // SAFETY: every handle here is one this test minted and still holds.
        unsafe {
            let parent = gos_rt_ctx_with_cancel(root);
            let child = gos_rt_ctx_with_cancel(parent);
            let parent_node = Arc::downgrade(&node_of(h(parent)).expect("parent"));
            release_handle(h(parent));
            assert!(
                parent_node.upgrade().is_some(),
                "the child holds its parent"
            );
            gos_rt_ctx_cancel(root);
            assert_eq!(gos_rt_ctx_is_cancelled(child), 1);
            release_handle(h(child));
            assert!(parent_node.upgrade().is_none());
            release_handle(h(root));
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arms netpoller timers; Miri can't
    fn an_expired_deadline_closes_the_done_channel() {
        // SAFETY: the handle is one this test minted and holds; the channel is live for the
        // process.
        unsafe {
            let ctx = gos_rt_ctx_with_timeout(std::ptr::null_mut(), 0);
            let chan = gos_rt_ctx_cancelled(ctx);
            let packed = super::super::chan::gos_rt_chan_recv_option(chan);
            assert_eq!(
                packed & 0xFFFF_FFFF_FFFF_FFFF,
                1,
                "a closed done channel answers None"
            );
            assert_eq!(gos_rt_ctx_is_cancelled(ctx), 1);
            release_handle(h(ctx));
        }
    }
}
