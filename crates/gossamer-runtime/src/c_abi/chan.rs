#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};

// ---------------------------------------------------------------
// Channel runtime - bounded MPMC via parking_lot Mutex<VecDeque>
// ---------------------------------------------------------------

use std::collections::VecDeque;

use parking_lot::{Condvar as PlCondvar, Mutex as PlMutex};

/// Channel payload storage. The 8-byte specialisation matches the
/// most common shape (every i64-class scalar plus pointer-sized
/// values fit) and avoids the per-message `Vec<u8>` allocation that
/// the byte-erased path needs. The codegen always knows
/// `elem_bytes` at the `gos_rt_chan_new` site, so the dispatch is
/// a one-time check at construction.
enum ChanStorage {
    /// 8-byte inline payloads. A 1M-message run with cap=100
    /// holds at most 100 * 8 = 800 B of payload here, vs ~3.2 MB
    /// of `Vec<u8>` headers + 8 B allocations under `Bytes`.
    I64(VecDeque<(u64, i64)>),
    /// Erased byte storage for any other element size.
    Bytes(VecDeque<(u64, Vec<u8>)>),
}

pub struct GosChan {
    pub elem_bytes: u32,
    pub cap: i64, // 0 = unbuffered, >0 = bounded, <0 = unbounded
    pub closed: PlMutex<bool>,
    buf: PlMutex<ChanStorage>,
    pub not_empty: PlCondvar,
    pub not_full: PlCondvar,
    /// Gids of goroutines parked on a recv (channel was empty). The
    /// next sender pops one and unparks it. Empty when no
    /// goroutines are waiting, in which case the OS-thread
    /// `not_empty` Condvar is the only waker path.
    parked_recv: parking_lot::Mutex<std::collections::VecDeque<crate::sched::Gid>>,
    /// Goroutines parked on a send, each tagged with the value it is
    /// waiting to hand off. See [`SendWaiter`] for why the tag is
    /// needed to route a wake.
    parked_send: parking_lot::Mutex<std::collections::VecDeque<SendWaiter>>,
    /// Goroutine id of the most recent sender. Read by recv to
    /// record a happens-before edge into the race detector. `-1`
    /// means "no sender yet observed".
    pub last_sender: AtomicI64,
    next_send_id: AtomicU64,
    recv_waiters: AtomicUsize,
    /// Threads and goroutines blocked in a send on this channel.
    send_waiters: AtomicUsize,
    /// Whether this channel is currently counted among those holding a
    /// waiter that could proceed. Kept in step by [`GosChan::sync_ready`].
    counted_ready: std::sync::atomic::AtomicBool,
    /// What the element word owns, so a value the channel still holds at
    /// teardown can be given back: 0 nothing, 1 a `String`, 2 a `Vec`, 3 a
    /// reference-counted node. Recorded by the send site, which is where the
    /// element's type is known.
    elem_kind: AtomicI64,
    /// One character per 8-byte slot of an aggregate element, saying what that
    /// slot owns: `s` nothing, `S` a `String`, `V` a `Vec`, `R` a counted node.
    /// This is the ownership descriptor a value nobody received is given back
    /// through. Empty until a send records it.
    elem_desc: PlMutex<Vec<u8>>,
    /// How many parties still reach this channel. A channel a single
    /// binding owns starts at one and is reclaimed by the drop the
    /// codegen emits at its last use; a spawn handle is shared with the
    /// child that delivers the outcome, so it starts at two and the
    /// party that leaves last reclaims it.
    refs: AtomicUsize,
}

/// A goroutine suspended inside a send, and the value it is waiting to
/// hand off.
///
/// An unbuffered sender queues its value and then waits for *that*
/// value to be taken, so its wake condition is per-waiter and a wake
/// delivered to any other sender does not satisfy it. `send_id` carries
/// the queued value's id so a consuming receiver can route the wake to
/// the one sender it released. Waiters with no value of their own - a
/// sender blocked on a full buffer, a `select` send arm - use
/// [`ANY_SEND`], because any consumption is what they are waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SendWaiter {
    gid: crate::sched::Gid,
    send_id: u64,
}

/// `send_id` of a waiter that is released by any consumption rather
/// than by one specific value. Never collides with a real send id:
/// [`GosChan::next_send_id`] starts at 1.
const ANY_SEND: u64 = 0;

impl GosChan {
    /// True when a waiter already on this channel would complete if it woke
    /// now: a queued value with a receiver to take it, or room (or a
    /// receiver) for a blocked sender's value. A closed channel releases
    /// every waiter, so it is always ready.
    fn has_ready_waiter(&self, queued: usize) -> bool {
        let recvs = self.recv_waiters.load(Ordering::Acquire);
        let sends = self.send_waiters.load(Ordering::Acquire);
        if *self.closed.lock() && (recvs > 0 || sends > 0) {
            return true;
        }
        if recvs > 0 && queued > 0 {
            return true;
        }
        if sends == 0 {
            return false;
        }
        if self.cap == 0 {
            // An unbuffered sender's value is queued until a receiver takes
            // it. A receiver present means the handoff is imminent; an empty
            // buffer means it already happened and the sender has simply not
            // observed it yet. Either way the sender is not stuck.
            return recvs > 0 || queued == 0;
        }
        self.cap < 0 || (queued as i64) < self.cap
    }

    /// Re-reads this channel's readiness and applies the difference to the
    /// process-wide count. Call under the `buf` lock after any change to the
    /// buffer, the waiter counts, or the closed flag.
    fn sync_ready(&self, queued: usize) {
        let ready = self.has_ready_waiter(queued);
        if ready == self.counted_ready.swap(ready, Ordering::AcqRel) {
            return;
        }
        crate::sched_global::adjust_pending_handoffs(ready);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chan_new(elem_bytes: u32, cap: i64) -> *mut GosChan {
    ffi_entry!(std::ptr::null_mut(), {
        let buf = if elem_bytes == 8 {
            ChanStorage::I64(VecDeque::new())
        } else {
            ChanStorage::Bytes(VecDeque::new())
        };
        Box::into_raw(Box::new(GosChan {
            elem_bytes,
            cap,
            closed: PlMutex::new(false),
            buf: PlMutex::new(buf),
            not_empty: PlCondvar::new(),
            not_full: PlCondvar::new(),
            parked_recv: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            parked_send: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            last_sender: AtomicI64::new(-1),
            next_send_id: AtomicU64::new(1),
            recv_waiters: AtomicUsize::new(0),
            send_waiters: AtomicUsize::new(0),
            counted_ready: std::sync::atomic::AtomicBool::new(false),
            elem_kind: AtomicI64::new(0),
            elem_desc: PlMutex::new(Vec::new()),
            refs: AtomicUsize::new(1),
        }))
    })
}

/// Sends a value, blocking until the channel can take it.
///
/// Sending into a closed channel panics with `send on closed channel`,
/// matching Go and the double-close panic below: a receiver has been told
/// no more values are coming, so a value handed over after that would be
/// dropped where the program expects it delivered. The panic is
/// goroutine-scoped and is raised outside the FFI catch-guard so the unwind
/// reaches the coroutine boundary rather than being converted to a sentinel.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_chan_send(c: *mut GosChan, val: *const u8) {
    let sent_on_closed = ffi_entry!(false, {
        if c.is_null() || val.is_null() {
            return false;
        }
        let chan = unsafe { &*c };
        if *chan.closed.lock() {
            return true;
        }
        let bytes_len = chan.elem_bytes as usize;
        let mut queued_unbuffered = false;
        let send_id = if chan.cap == 0 {
            chan.next_send_id.fetch_add(1, Ordering::Relaxed).max(1)
        } else {
            0
        };
        loop {
            let mut guard = chan.buf.lock();
            // A queued value that is no longer in the queue was taken by a
            // receiver, which completes this send. The check comes before the
            // closed check because a close that lands after the handoff says
            // nothing about a send that is already done.
            if queued_unbuffered && !storage_contains_id(&guard, send_id) {
                return false;
            }
            // A channel closed while this send was parked has no reader
            // left expecting the value, which is the same program error as
            // sending into one already closed.
            if *chan.closed.lock() {
                return true;
            }
            chan.sync_ready(storage_len(&guard));
            if chan.cap == 0 && !queued_unbuffered {
                push_back(&mut guard, send_id, val, bytes_len);
                queued_unbuffered = true;
                chan.sync_ready(storage_len(&guard));
                drop(guard);
                chan.last_sender
                    .store(i64::from(crate::race::current_gid()), Ordering::Release);
                wake_one_recv(chan);
                continue;
            } else if chan.cap != 0 && (chan.cap < 0 || (storage_len(&guard) as i64) < chan.cap) {
                push_back(&mut guard, 0, val, bytes_len);
                chan.sync_ready(storage_len(&guard));
                drop(guard);
                chan.last_sender
                    .store(i64::from(crate::race::current_gid()), Ordering::Release);
                wake_one_recv(chan);
                return false;
            }
            // A cancelled cohort has no reader left to wait for, so the send
            // stops here rather than pinning its child in a block the cohort
            // is winding down. The check sits after the paths that complete
            // without waiting: cancellation ends the wait, not a delivery the
            // channel could already take.
            if super::cohort::current_is_cancelled() {
                return false;
            }
            // Buffer full. Goroutines park; OS threads block. Either way,
            // a runtime with nothing left able to run cannot deliver the
            // value this send is waiting to hand off.
            chan.send_waiters.fetch_add(1, Ordering::AcqRel);
            crate::sched_global::adjust_channel_waiters(true);
            let _sending = SendWaiterGuard { chan };
            chan.sync_ready(storage_len(&guard));
            crate::sched_global::report_deadlock_if_stuck("send");
            if gossamer_coro::in_goroutine() {
                // Publish our waiter entry before releasing `buf`. A receiver
                // can otherwise consume the last queued value between the
                // full-buffer check above and registration below, observe no
                // parked sender, and leave this goroutine asleep forever.
                // `park` records a pre-unpark wake that arrives after the
                // queue registration but before suspension.
                let mut parked_as = None;
                let mut guard = Some(guard);
                let mut cohort_wait = 0i64;
                crate::sched_global::park(crate::sched::ParkReason::Chan, |parker| {
                    parked_as = Some(parker.gid);
                    chan.parked_send.lock().push_back(SendWaiter {
                        gid: parker.gid,
                        send_id,
                    });
                    cohort_wait = super::cohort::register_waiter(parker.gid);
                    drop(guard.take());
                });
                // Cleanup: remove our gid from parked_send if still
                // present (e.g. a parallel close fired with pre_unpark
                // before any matching receive). The gid comes from the
                // parker rather than from the thread-local, which names
                // whichever goroutine the resuming thread is running.
                if let Some(gid) = parked_as {
                    chan.parked_send.lock().retain(|w| w.gid != gid);
                    super::cohort::deregister_waiter(cohort_wait, gid);
                }
            } else {
                // Non-goroutine fallback: condvar-block the OS thread.
                // parking_lot's `wait` takes &mut guard and re-acquires
                // on wakeup; drop the guard explicitly after so clippy
                // doesn't flag a let-underscore-lock pattern.
                chan.not_full.wait(&mut guard);
                drop(guard);
            }
        }
    });
    if sent_on_closed {
        super::panic::panic_text("send on closed channel");
    }
}

/// Releases a send registration when the wait ends, by any path. The
/// channel's readiness is re-synced by whichever thread next holds its lock;
/// taking the lock here would re-enter one the wait sites already hold.
struct SendWaiterGuard<'a> {
    chan: &'a GosChan,
}

impl Drop for SendWaiterGuard<'_> {
    fn drop(&mut self) {
        crate::sched_global::adjust_channel_waiters(false);
        self.chan.send_waiters.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Releases a receive registration. As with the send guard, the lock stays
/// untouched here.
struct RecvWaiterGuard<'a> {
    chan: &'a GosChan,
}

impl Drop for RecvWaiterGuard<'_> {
    fn drop(&mut self) {
        crate::sched_global::adjust_channel_waiters(false);
        self.chan.recv_waiters.fetch_sub(1, Ordering::AcqRel);
    }
}

fn wake_one_recv(chan: &GosChan) {
    if let Some(gid) = chan.parked_recv.lock().pop_front() {
        crate::sched_global::scheduler().unpark(gid);
    }
    chan.not_empty.notify_one();
}

/// Removes the send waiters released by consuming the value tagged
/// `consumed_id`, longest-waiting first.
///
/// The waiter holding `consumed_id` is released because its own value
/// was taken. One [`ANY_SEND`] waiter is released too, because room -
/// or a receiver - is what it is waiting for and this consumption
/// provided it.
fn take_released_senders(
    queue: &mut std::collections::VecDeque<SendWaiter>,
    consumed_id: u64,
) -> Vec<crate::sched::Gid> {
    let mut released = Vec::new();
    if consumed_id != ANY_SEND
        && let Some(i) = queue.iter().position(|w| w.send_id == consumed_id)
        && let Some(w) = queue.remove(i)
    {
        released.push(w.gid);
    }
    if let Some(i) = queue.iter().position(|w| w.send_id == ANY_SEND)
        && let Some(w) = queue.remove(i)
    {
        released.push(w.gid);
    }
    released
}

/// Wakes the senders released by a receiver taking the value tagged
/// `consumed_id` (`ANY_SEND` when the value carries no tag, which is
/// every buffered send).
fn wake_send_after_consume(chan: &GosChan, consumed_id: u64) {
    let released = take_released_senders(&mut chan.parked_send.lock(), consumed_id);
    let sched = crate::sched_global::scheduler();
    for gid in released {
        sched.unpark(gid);
    }
    if chan.cap == 0 {
        // An OS-thread sender on an unbuffered channel waits on the same
        // per-sender condition as a goroutine one, and a Condvar cannot
        // name the waiter to release, so every waiter re-tests instead.
        chan.not_full.notify_all();
    } else {
        chan.not_full.notify_one();
    }
}

fn wake_all(chan: &GosChan) {
    let recvs: Vec<_> = chan.parked_recv.lock().drain(..).collect();
    let sends: Vec<_> = chan.parked_send.lock().drain(..).collect();
    let sched = crate::sched_global::scheduler();
    for gid in recvs.into_iter().chain(sends.into_iter().map(|w| w.gid)) {
        sched.unpark(gid);
    }
    chan.not_empty.notify_all();
    chan.not_full.notify_all();
}

/// Sends without blocking, answering `1` when the value was taken and `0`
/// when it was not. Sending into a closed channel panics exactly as the
/// blocking [`gos_rt_chan_send`] does.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_chan_try_send(c: *mut GosChan, val: *const u8) -> i32 {
    let outcome = ffi_entry!(-1, {
        if c.is_null() || val.is_null() {
            return 0;
        }
        let chan = unsafe { &*c };
        if *chan.closed.lock() {
            return CLOSED_SEND;
        }
        let bytes_len = chan.elem_bytes as usize;
        let mut guard = chan.buf.lock();
        if chan.cap == 0 {
            let has_receiver = chan.recv_waiters.load(Ordering::Acquire) > 0
                || !chan.parked_recv.lock().is_empty();
            if !has_receiver {
                return 0;
            }
            push_back(&mut guard, 0, val, bytes_len);
        } else {
            if chan.cap > 0 && storage_len(&guard) as i64 >= chan.cap {
                return 0;
            }
            push_back(&mut guard, 0, val, bytes_len);
        }
        drop(guard);
        chan.last_sender
            .store(i64::from(crate::race::current_gid()), Ordering::Release);
        wake_one_recv(chan);
        1
    });
    if outcome == CLOSED_SEND {
        super::panic::panic_text("send on closed channel");
        return 0;
    }
    outcome
}

/// [`gos_rt_chan_try_send`]'s internal answer for a send into a closed
/// channel, distinguished from a refused send (`0`) and a null channel
/// (`-1`) so the panic is raised outside the FFI catch-guard.
const CLOSED_SEND: i32 = -2;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chan_recv(c: *mut GosChan, out: *mut u8) -> i32 {
    ffi_entry!(-1, {
        if c.is_null() || out.is_null() {
            return 0;
        }
        let chan = unsafe { &*c };
        let bytes_len = chan.elem_bytes as usize;
        loop {
            // A cancelled cohort answers its children the way a closed
            // channel does: nothing more is coming. What already arrived is
            // still handed over, because a closed channel drains before it
            // answers, and cancellation says no more will be sent rather
            // than that a delivered value is dropped. A join handle carries
            // its child's outcome on this path, and the failure that
            // cancelled the cohort is often the very value in the buffer.
            if super::cohort::current_is_cancelled() {
                let mut guard = chan.buf.lock();
                chan.sync_ready(storage_len(&guard));
                if let Some(consumed_id) = pop_front(&mut guard, out, bytes_len) {
                    chan.sync_ready(storage_len(&guard));
                    drop(guard);
                    record_chan_handoff(chan);
                    wake_send_after_consume(chan, consumed_id);
                    return 1;
                }
                return 0;
            }
            let mut guard = chan.buf.lock();
            chan.sync_ready(storage_len(&guard));
            if let Some(consumed_id) = pop_front(&mut guard, out, bytes_len) {
                chan.sync_ready(storage_len(&guard));
                drop(guard);
                record_chan_handoff(chan);
                wake_send_after_consume(chan, consumed_id);
                return 1;
            }
            if *chan.closed.lock() {
                return 0;
            }
            // Empty channel. Goroutines park; OS threads block. An empty
            // channel that no runnable goroutine can fill stays empty.
            chan.recv_waiters.fetch_add(1, Ordering::AcqRel);
            crate::sched_global::adjust_channel_waiters(true);
            let _receiving = RecvWaiterGuard { chan };
            chan.sync_ready(storage_len(&guard));
            crate::sched_global::report_deadlock_if_stuck("receive");
            if gossamer_coro::in_goroutine() {
                // Register while still holding `buf`, pairing this empty
                // check with waiter publication. Without that pairing a
                // sender can queue a value in the gap, find no waiter to
                // wake, and strand both goroutines indefinitely.
                let mut parked_as = None;
                let mut guard = Some(guard);
                let mut cohort_wait = 0i64;
                crate::sched_global::park(crate::sched::ParkReason::Chan, |parker| {
                    parked_as = Some(parker.gid);
                    chan.recv_waiters.fetch_add(1, Ordering::AcqRel);
                    chan.parked_recv.lock().push_back(parker.gid);
                    // Cancelling the cohort unparks this goroutine, which
                    // then re-reads the check at the top of the loop.
                    cohort_wait = super::cohort::register_waiter(parker.gid);
                    drop(guard.take());
                });
                chan.recv_waiters.fetch_sub(1, Ordering::AcqRel);
                if let Some(gid) = parked_as {
                    chan.parked_recv.lock().retain(|g| *g != gid);
                    super::cohort::deregister_waiter(cohort_wait, gid);
                }
            } else {
                chan.recv_waiters.fetch_add(1, Ordering::AcqRel);
                chan.not_empty.wait(&mut guard);
                chan.recv_waiters.fetch_sub(1, Ordering::AcqRel);
                drop(guard);
            }
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chan_try_recv(c: *mut GosChan, out: *mut u8) -> i32 {
    ffi_entry!(-1, {
        if c.is_null() || out.is_null() {
            return 0;
        }
        let chan = unsafe { &*c };
        let bytes_len = chan.elem_bytes as usize;
        let mut guard = chan.buf.lock();
        if let Some(consumed_id) = pop_front(&mut guard, out, bytes_len) {
            drop(guard);
            record_chan_handoff(chan);
            wake_send_after_consume(chan, consumed_id);
            return 1;
        }
        0
    })
}

/// Single-argument wrapper for LLVM: calls `gos_rt_chan_recv` and
/// boxes the status + value into a `*mut GosResult` (disc=0 → Some,
/// disc=1 → None) so callers don't need to manage an out-pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chan_recv_option(c: *mut GosChan) -> i128 {
    ffi_entry!(0i128, {
        let mut out = 0i64;
        let status = unsafe { gos_rt_chan_recv(c, std::ptr::addr_of_mut!(out).cast::<u8>()) };
        let disc = 1 - i64::from(status);
        let payload = if status == 1 { out } else { 0 };
        crate::c_abi::vec::pack_result(disc, payload)
    })
}

/// Single-argument wrapper for LLVM: like `gos_rt_chan_recv_option`
/// but non-blocking (returns None immediately when the buffer is empty).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chan_try_recv_option(c: *mut GosChan) -> i128 {
    ffi_entry!(0i128, {
        let mut out = 0i64;
        let status = unsafe { gos_rt_chan_try_recv(c, std::ptr::addr_of_mut!(out).cast::<u8>()) };
        let disc = 1 - i64::from(status);
        let payload = if status == 1 { out } else { 0 };
        crate::c_abi::vec::pack_result(disc, payload)
    })
}

/// Cross-crate hooks installed by `gossamer-std` so the runtime
/// can observe a `Context` without depending on `gossamer-std`
/// itself. `ctx_handle` is the opaque pointer the caller passes
/// to `gos_rt_chan_recv_ctx_option` etc.; the installed callbacks
/// downcast it on their side. All three hooks must be installed
/// together via [`gos_rt_install_ctx_hooks`] before any
/// context-aware runtime entry point is called.
type CtxRegisterFn = unsafe extern "C" fn(ctx_handle: *const u8, gid: u32);
type CtxDeregisterFn = unsafe extern "C" fn(ctx_handle: *const u8, gid: u32);
type CtxIsCancelledFn = unsafe extern "C" fn(ctx_handle: *const u8) -> i32;

static CTX_REGISTER_HOOK: std::sync::atomic::AtomicPtr<()> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static CTX_DEREGISTER_HOOK: std::sync::atomic::AtomicPtr<()> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static CTX_IS_CANCELLED_HOOK: std::sync::atomic::AtomicPtr<()> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Installs the cross-crate context hooks. Idempotent; calling
/// twice with the same fn pointers is a no-op. Calling with a
/// different fn pointer (an actual rebind) is undefined behaviour -
/// the caller (gossamer-std) installs exactly once at first
/// use of a context-aware runtime entry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_install_ctx_hooks(
    register: CtxRegisterFn,
    deregister: CtxDeregisterFn,
    is_cancelled: CtxIsCancelledFn,
) {
    ffi_entry!((), {
        use std::sync::atomic::Ordering;
        CTX_REGISTER_HOOK.store(register as *mut (), Ordering::Release);
        CTX_DEREGISTER_HOOK.store(deregister as *mut (), Ordering::Release);
        CTX_IS_CANCELLED_HOOK.store(is_cancelled as *mut (), Ordering::Release);
    });
}

fn ctx_register_hook() -> Option<CtxRegisterFn> {
    let p = CTX_REGISTER_HOOK.load(std::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        None
    } else {
        // SAFETY: `p` was stored via `CtxRegisterFn as *mut ()` in
        // `gos_rt_install_ctx_hooks` and is read back with the
        // same function-pointer type. The pointer itself is
        // immutable for the program's lifetime after install.
        Some(unsafe { std::mem::transmute::<*mut (), CtxRegisterFn>(p) })
    }
}

fn ctx_deregister_hook() -> Option<CtxDeregisterFn> {
    let p = CTX_DEREGISTER_HOOK.load(std::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut (), CtxDeregisterFn>(p) })
    }
}

fn ctx_is_cancelled_hook() -> Option<CtxIsCancelledFn> {
    let p = CTX_IS_CANCELLED_HOOK.load(std::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut (), CtxIsCancelledFn>(p) })
    }
}

/// Cancellation-aware variant of [`gos_rt_chan_recv_option`].
///
/// Behaves identically to `chan_recv_option` when the context
/// is uncancelled. If the context fires while the goroutine is
/// parked on the channel's `parked_recv` queue, the registered
/// `is_cancelled` hook's cancellation will be observed on the
/// next unpark cycle and the function returns `None` (disc=1).
///
/// `ctx_handle` is the opaque pointer the caller's
/// `gos_rt_install_ctx_hooks` callbacks know how to interpret;
/// the runtime never derefs it directly. Passing `null` falls
/// back to the unconditional [`gos_rt_chan_recv_option`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chan_recv_ctx_option(
    c: *mut GosChan,
    ctx_handle: *const u8,
) -> i128 {
    ffi_entry!(0i128, {
        let (disc, payload) = unsafe { chan_recv_ctx_core(c, ctx_handle) };
        crate::c_abi::vec::pack_result(disc, payload)
    })
}

/// The same cancellation-aware receive as [`gos_rt_chan_recv_ctx_option`],
/// reporting through the status-plus-out-pointer convention of
/// [`gos_rt_chan_recv`]: `1` with the value stored through `out`, or `0`
/// for a closed channel or a cancelled context. A backend whose calling
/// convention for a two-word return is not under its own control reaches
/// the same receive through this shape.
///
/// # Safety
/// `out` must be writable for 8 bytes when non-null; `c` and `ctx_handle`
/// carry the same requirements as [`gos_rt_chan_recv_ctx_option`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chan_recv_ctx(
    c: *mut GosChan,
    ctx_handle: *const u8,
    out: *mut i64,
) -> i32 {
    ffi_entry!(0, {
        let (disc, payload) = unsafe { chan_recv_ctx_core(c, ctx_handle) };
        if disc == 0 {
            if !out.is_null() {
                unsafe { out.write(payload) };
            }
            1
        } else {
            0
        }
    })
}

/// Receives with cancellation, answering `(disc, payload)` where `disc` is
/// `0` for a value and `1` for a closed channel or a cancelled context.
unsafe fn chan_recv_ctx_core(c: *mut GosChan, ctx_handle: *const u8) -> (i64, i64) {
    {
        if ctx_handle.is_null() {
            let packed = unsafe { gos_rt_chan_recv_option(c) };
            return (
                (packed & 0xFFFF_FFFF_FFFF_FFFF) as u64 as i64,
                ((packed >> 64) as u64) as i64,
            );
        }
        // Hooks carry contexts owned by a Rust caller. A handle a compiled
        // program minted is a node in this runtime's own registry, which the
        // fallbacks consult directly, so both kinds of context cancel a
        // receive through the same loop below.
        let addr = ctx_handle as usize;
        let register: Box<dyn Fn(*const u8, u32)> = match ctx_register_hook() {
            Some(hook) => Box::new(move |h, g| unsafe { hook(h, g) }),
            None => Box::new(move |_, g| {
                super::context::register_waiter(addr, crate::sched::Gid(g));
            }),
        };
        let deregister: Box<dyn Fn(*const u8, u32)> = match ctx_deregister_hook() {
            Some(hook) => Box::new(move |h, g| unsafe { hook(h, g) }),
            None => Box::new(move |_, g| {
                super::context::deregister_waiter(addr, crate::sched::Gid(g));
            }),
        };
        let is_cancelled: Box<dyn Fn(*const u8) -> i32> = match ctx_is_cancelled_hook() {
            Some(hook) => Box::new(move |h| unsafe { hook(h) }),
            None => Box::new(move |_| i32::from(super::context::addr_is_cancelled(addr))),
        };
        // Check before parking: an already-cancelled context
        // short-circuits without touching the channel.
        if is_cancelled(ctx_handle) != 0 {
            return (1, 0);
        }
        if c.is_null() {
            return (1, 0);
        }
        let chan = unsafe { &*c };
        let bytes_len = chan.elem_bytes as usize;
        let gid = crate::sched_global::current_gid();
        if let Some(g) = gid {
            register(ctx_handle, g.as_u32());
        }
        // Inline the recv loop with cancel polling on both the
        // goroutine park path and the OS-thread condvar path. The
        // 50 ms condvar timeout is the cancel-observation latency
        // for non-goroutine callers: short enough to feel responsive,
        // long enough not to hot-loop while idle.
        let mut out_val = 0i64;
        let out_ptr = std::ptr::addr_of_mut!(out_val).cast::<u8>();
        let (result_disc, result_payload) = loop {
            let mut guard = chan.buf.lock();
            chan.sync_ready(storage_len(&guard));
            if let Some(consumed_id) = pop_front(&mut guard, out_ptr, bytes_len) {
                chan.sync_ready(storage_len(&guard));
                drop(guard);
                record_chan_handoff(chan);
                wake_send_after_consume(chan, consumed_id);
                break (0i64, out_val);
            }
            if *chan.closed.lock() {
                break (1i64, 0i64);
            }
            chan.recv_waiters.fetch_add(1, Ordering::AcqRel);
            crate::sched_global::adjust_channel_waiters(true);
            let _receiving = RecvWaiterGuard { chan };
            chan.sync_ready(storage_len(&guard));
            crate::sched_global::report_deadlock_if_stuck("receive");
            if gossamer_coro::in_goroutine() {
                // Keep the empty-buffer check and receiver registration in
                // one critical section, just like the unconditional recv
                // path. Cancellation does not change the channel wakeup
                // contract.
                let mut parked_as = None;
                let mut guard = Some(guard);
                crate::sched_global::park(crate::sched::ParkReason::Chan, |parker| {
                    parked_as = Some(parker.gid);
                    chan.recv_waiters.fetch_add(1, Ordering::AcqRel);
                    chan.parked_recv.lock().push_back(parker.gid);
                    drop(guard.take());
                });
                chan.recv_waiters.fetch_sub(1, Ordering::AcqRel);
                if let Some(g) = parked_as {
                    chan.parked_recv.lock().retain(|x| *x != g);
                }
                if is_cancelled(ctx_handle) != 0 {
                    break (1i64, 0i64);
                }
            } else {
                chan.recv_waiters.fetch_add(1, Ordering::AcqRel);
                chan.not_empty
                    .wait_for(&mut guard, std::time::Duration::from_millis(50));
                chan.recv_waiters.fetch_sub(1, Ordering::AcqRel);
                drop(guard);
                if is_cancelled(ctx_handle) != 0 {
                    break (1i64, 0i64);
                }
            }
        };
        if let Some(g) = gid {
            deregister(ctx_handle, g.as_u32());
        }
        (result_disc, result_payload)
    }
}

fn storage_len(storage: &ChanStorage) -> usize {
    match storage {
        ChanStorage::I64(d) => d.len(),
        ChanStorage::Bytes(d) => d.len(),
    }
}

fn storage_contains_id(storage: &ChanStorage, id: u64) -> bool {
    match storage {
        ChanStorage::I64(d) => d.iter().any(|(msg_id, _)| *msg_id == id),
        ChanStorage::Bytes(d) => d.iter().any(|(msg_id, _)| *msg_id == id),
    }
}

fn push_back(storage: &mut ChanStorage, id: u64, val: *const u8, bytes_len: usize) {
    match storage {
        ChanStorage::I64(deque) => {
            // Read 8 bytes from `val` into an i64 in a way that
            // doesn't assume natural alignment of the source.
            let mut tmp = [0u8; 8];
            unsafe {
                std::ptr::copy_nonoverlapping(val, tmp.as_mut_ptr(), 8);
            }
            deque.push_back((id, i64::from_ne_bytes(tmp)));
        }
        ChanStorage::Bytes(deque) => {
            let mut data = vec![0u8; bytes_len];
            unsafe {
                std::ptr::copy_nonoverlapping(val, data.as_mut_ptr(), bytes_len);
            }
            deque.push_back((id, data));
        }
    }
}

/// Moves the front value into `out`, returning its send id so the
/// caller can wake the sender that queued it. `None` when empty.
fn pop_front(storage: &mut ChanStorage, out: *mut u8, bytes_len: usize) -> Option<u64> {
    match storage {
        ChanStorage::I64(deque) => deque.pop_front().map(|(id, n)| {
            let bytes = n.to_ne_bytes();
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, 8);
            }
            id
        }),
        ChanStorage::Bytes(deque) => deque.pop_front().map(|(id, item)| {
            unsafe {
                std::ptr::copy_nonoverlapping(item.as_ptr(), out, bytes_len);
            }
            id
        }),
    }
}

/// Records the sender->receiver synchronisation edge into the race
/// detector. Called immediately after a successful recv. No-op
/// when the race detector is disabled.
fn record_chan_handoff(chan: &GosChan) {
    let from = chan.last_sender.load(Ordering::Acquire);
    if from < 0 {
        return;
    }
    let to = crate::race::current_gid();
    crate::race::record_sync(u32::try_from(from).unwrap_or(0), to);
}

/// Closes `chan` if it is not already closed, returning `true` when
/// this call performed the close and `false` when it was already
/// closed. Never panics: the reclamation path (`gos_rt_chan_drop`)
/// closes through here so a channel the user already closed
/// explicitly is reclaimed without a spurious double-close panic.
pub(crate) fn chan_close_idempotent(chan: &GosChan) -> bool {
    {
        let mut guard = chan.closed.lock();
        if *guard {
            return false;
        }
        *guard = true;
    }
    // A close releases every waiter, so the channel becomes one that can
    // make progress. Recording that before the wake keeps a waiter elsewhere
    // from reading the program as stuck while these waiters are on their way
    // to being resumed.
    {
        let queued = storage_len(&chan.buf.lock());
        chan.sync_ready(queued);
    }
    wake_all(chan);
    true
}

/// Closes a channel. Returns `0` on success and `-1` if `c` is null.
/// Closing an already-closed channel panics with
/// `close of closed channel`, matching Go. The panic is
/// goroutine-scoped (via `gos_rt_panic`): it unwinds to the
/// coroutine boundary inside a spawned goroutine (isolating only
/// that goroutine) and exits 101 on the main goroutine - it never
/// aborts the whole process. Callers may ignore the return value.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_chan_close(c: *mut GosChan) -> i32 {
    // `None` = null channel, `Some(false)` = already closed. The close
    // itself runs under the FFI catch-guard; the double-close panic is
    // raised below, *outside* the guard, so a goroutine-scoped unwind
    // reaches the coroutine boundary instead of being swallowed and
    // converted to an FFI-entry sentinel.
    let outcome = ffi_entry!(None, {
        if c.is_null() {
            None
        } else {
            Some(chan_close_idempotent(unsafe { &*c }))
        }
    });
    match outcome {
        None => -1,
        Some(true) => 0,
        Some(false) => {
            super::panic::panic_text("close of closed channel");
            0
        }
    }
}

/// Drops this caller's reference to a channel created with
/// `gos_rt_chan_new`. The last reference closes the channel, so any
/// thread parked on `not_empty` / `not_full` wakes with
/// `RecvResult::Closed` / `SendResult::Closed` before the underlying
/// storage is reclaimed. The codegen emits the call at the channel's
/// last live use.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chan_drop(c: *mut GosChan) {
    ffi_entry!((), {
        if c.is_null() {
            return;
        }
        // SAFETY: `c` is a live channel this caller holds a reference to.
        unsafe { chan_release(c) };
    });
}

/// Records what a queued element word owns, so the channel can give the share
/// back for a value nobody receives. Emitted at each send, where the element's
/// type is known; writing the same kind again is the common case.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chan_set_elem_kind(c: *mut GosChan, kind: i64) {
    ffi_entry!((), {
        if c.is_null() {
            return;
        }
        unsafe { &*c }.elem_kind.store(kind, Ordering::Relaxed);
    });
}

/// Records the ownership descriptor of an aggregate element's slots.
///
/// The channel carries a heap copy of the aggregate, so a value nobody
/// receives holds a share of every heap field in it. The descriptor is what
/// the teardown walks to give those back.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chan_set_elem_desc(c: *mut GosChan, desc: *const std::ffi::c_char) {
    ffi_entry!((), {
        if c.is_null() || desc.is_null() {
            return;
        }
        // SAFETY: `desc` is the descriptor string the compiler emitted for this
        // element type; read through the length header like any other string
        // argument.
        let bytes = unsafe { crate::c_abi::string::gos_str_arg_bytes(desc) }.to_vec();
        *unsafe { &*c }.elem_desc.lock() = bytes;
    });
}

/// Gives back the shares an aggregate element's slots hold, then the copy the
/// channel carries.
fn release_aggregate(base: i64, desc: &[u8]) {
    if base == 0 {
        return;
    }
    for (slot, kind) in desc.iter().enumerate() {
        let word_at = (base as usize).wrapping_add(slot * 8) as *const i64;
        // SAFETY: the descriptor has one character per slot of the heap copy
        // the send made, so every offset it names is inside that allocation.
        let word = unsafe { std::ptr::read_unaligned(word_at) };
        match kind {
            b'S' => release_elem(1, word),
            b'V' => release_elem(2, word),
            b'R' => release_elem(3, word),
            _ => {}
        }
    }
    release_elem(3, base);
}

/// Gives back the share the send minted for one queued element word.
fn release_elem(kind: i64, word: i64) {
    if word == 0 {
        return;
    }
    match kind {
        // SAFETY: the kind is the static type of what the send put in the
        // queue, so the word is a body of exactly that shape.
        1 => unsafe {
            crate::c_abi::string::gos_rt_str_free_typed(word as usize as *mut std::ffi::c_char);
        },
        2 => unsafe {
            crate::c_abi::gos_rt_vec_free(word as usize as *mut crate::c_abi::vec::GosVec);
        },
        3 => unsafe {
            crate::c_abi::rc::gos_rt_rc_release(word as usize as *mut u8);
        },
        _ => {}
    }
}

/// Records one more party reaching `chan`.
pub(crate) fn chan_retain(chan: &GosChan) {
    chan.refs.fetch_add(1, Ordering::Relaxed);
}

/// Drops one party's reference to `chan`, reclaiming the storage once
/// the last one is gone.
///
/// # Safety
/// `chan` must be a live channel the caller holds a reference to, and
/// the caller must not touch it again.
pub(crate) unsafe fn chan_release(chan: *mut GosChan) {
    if chan.is_null() {
        return;
    }
    // SAFETY: the caller's reference keeps the channel alive for this read.
    let remaining = unsafe { (*chan).refs.fetch_sub(1, Ordering::AcqRel) };
    if remaining != 1 {
        return;
    }
    // Close + notify before reclamation so parked threads observe the
    // closed flag rather than racing the Box drop. The Drop impl on
    // `GosChan` repeats the close+notify, harmlessly, because callers
    // may also drop a `Box<GosChan>` directly in tests without going
    // through this entry point.
    unsafe {
        // Idempotent close for reclamation - must not panic if the user
        // already closed this channel explicitly (the user-facing
        // `gos_rt_chan_close` panics on double-close).
        chan_close_idempotent(&*chan);
        // A value nobody received still holds the share its send minted. The
        // last party out is who gives it back, so an abandoned producer costs
        // the queue's storage and nothing more.
        let kind = (*chan).elem_kind.load(Ordering::Relaxed);
        if kind != 0 {
            let queued: Vec<i64> = match &*(*chan).buf.lock() {
                ChanStorage::I64(deque) => deque.iter().map(|(_, word)| *word).collect(),
                ChanStorage::Bytes(deque) => deque
                    .iter()
                    .filter(|(_, bytes)| bytes.len() >= 8)
                    .map(|(_, bytes)| {
                        let mut tmp = [0u8; 8];
                        tmp.copy_from_slice(&bytes[..8]);
                        i64::from_ne_bytes(tmp)
                    })
                    .collect(),
            };
            let desc = (*chan).elem_desc.lock().clone();
            for word in queued {
                if kind == 3 && !desc.is_empty() {
                    release_aggregate(word, &desc);
                } else {
                    release_elem(kind, word);
                }
            }
        }
        drop(Box::from_raw(chan));
    }
}

impl Drop for GosChan {
    fn drop(&mut self) {
        *self.closed.lock() = true;
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }
}

// ---------------------------------------------------------------
// select { } multiplexing
// ---------------------------------------------------------------
//
// The compiled tiers lower a `select` to a builder: one `gos_rt_select_new`,
// one `gos_rt_select_arm_*` per arm in source order, then `gos_rt_select_wait`
// (poll + park), `gos_rt_select_value` (the popped recv payload), and
// `gos_rt_select_free`. This sequence-of-scalar-calls shape keeps the MIR
// lowering free of array construction while the transfer stays atomic inside
// `wait` (no recheck-after-return TOCTOU). Semantics match the VM walker:
// ready arms are polled in pseudo-random order, a closed+drained recv arm is
// always ready (yielding the element-type zero value, matching Go), and the
// default arm fires only when nothing else is.

enum SelectArmRt {
    Recv(*mut GosChan),
    Send(*mut GosChan, i64),
    Default,
}

fn select_shuffle_indices(n: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    if n <= 1 {
        return order;
    }
    #[cfg(not(target_arch = "wasm32"))]
    let mut x = {
        let nanos = crate::platform::system_time_now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0_u64, |d| d.as_nanos() as u64);
        nanos ^ 0xA076_1D64_78BD_642F
    };
    #[cfg(target_arch = "wasm32")]
    let mut x = {
        static SELECT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        SELECT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) ^ 0xA076_1D64_78BD_642F
    };
    for i in (1..n).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let j = (x as usize) % (i + 1);
        order.swap(i, j);
    }
    order
}

pub struct SelectBuilder {
    arms: Vec<SelectArmRt>,
    last_value: i64,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_select_new(n: i64) -> *mut SelectBuilder {
    ffi_entry!(std::ptr::null_mut(), {
        let cap = usize::try_from(n).unwrap_or(0);
        Box::into_raw(Box::new(SelectBuilder {
            arms: Vec::with_capacity(cap),
            last_value: 0,
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_select_arm_recv(b: *mut SelectBuilder, c: *mut GosChan) {
    ffi_entry!((), {
        if b.is_null() {
            return;
        }
        unsafe { &mut *b }.arms.push(SelectArmRt::Recv(c));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_select_arm_send(b: *mut SelectBuilder, c: *mut GosChan, val: i64) {
    ffi_entry!((), {
        if b.is_null() {
            return;
        }
        unsafe { &mut *b }.arms.push(SelectArmRt::Send(c, val));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_select_arm_default(b: *mut SelectBuilder) {
    ffi_entry!((), {
        if b.is_null() {
            return;
        }
        unsafe { &mut *b }.arms.push(SelectArmRt::Default);
    });
}

/// Checks whether a select arm became ready while the caller was publishing
/// its waiter registrations. The check happens after every arm is registered,
/// so a readiness transition cannot be lost between the main select poll and
/// suspension.
fn select_arm_is_ready(kind: u8, chan: &GosChan) -> bool {
    let guard = chan.buf.lock();
    match kind {
        // Receive is ready for a queued value or a closed-and-drained channel.
        0 => storage_len(&guard) != 0 || *chan.closed.lock(),
        // Send is ready when buffer space exists. An unbuffered send instead
        // needs a receiver that has already published its interest.
        1 if chan.cap == 0 => {
            chan.recv_waiters.load(Ordering::Acquire) > 0 || !chan.parked_recv.lock().is_empty()
        }
        1 => chan.cap < 0 || (storage_len(&guard) as i64) < chan.cap,
        _ => false,
    }
}

/// Polls the registered arms in pseudo-random order, parking the goroutine until one
/// is ready when there is no default arm. Returns the chosen arm's source
/// index (the default arm's index when nothing else is ready), or -1 on a null
/// builder. The popped value of a chosen recv arm is stored for retrieval via
/// `gos_rt_select_value`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_select_wait(b: *mut SelectBuilder) -> i64 {
    ffi_entry!(-1, {
        if b.is_null() {
            return -1;
        }
        let builder = unsafe { &mut *b };
        // Snapshot (kind, chan, send_val) so the poll/park loops don't hold a
        // borrow of `builder` across the `last_value` write. 0=recv, 1=send,
        // 2=default. Raw pointers are Copy; the snapshot is cheap.
        let arms: Vec<(u8, *mut GosChan, i64)> = builder
            .arms
            .iter()
            .map(|a| match a {
                SelectArmRt::Recv(c) => (0u8, *c, 0i64),
                SelectArmRt::Send(c, v) => (1u8, *c, *v),
                SelectArmRt::Default => (2u8, std::ptr::null_mut(), 0i64),
            })
            .collect();
        let default_index = arms.iter().position(|(k, _, _)| *k == 2);
        loop {
            for i in select_shuffle_indices(arms.len()) {
                let (kind, c, v) = arms[i];
                if kind == 0 {
                    if c.is_null() {
                        continue;
                    }
                    let chan = unsafe { &*c };
                    let mut tmp = 0i64;
                    let mut guard = chan.buf.lock();
                    if let Some(consumed_id) = pop_front(
                        &mut guard,
                        std::ptr::addr_of_mut!(tmp).cast::<u8>(),
                        chan.elem_bytes as usize,
                    ) {
                        drop(guard);
                        record_chan_handoff(chan);
                        wake_send_after_consume(chan, consumed_id);
                        builder.last_value = tmp;
                        return i as i64;
                    }
                    // Go semantics: a closed+drained recv arm is always
                    // ready, yielding the element-type zero value. Hold the
                    // `buf` lock while reading `closed` (same lock order as
                    // `gos_rt_chan_recv`) so an empty-then-closed transition
                    // is observed atomically.
                    if *chan.closed.lock() {
                        drop(guard);
                        builder.last_value = 0;
                        return i as i64;
                    }
                } else if kind == 1 {
                    if c.is_null() {
                        continue;
                    }
                    let send_val = v;
                    if unsafe { gos_rt_chan_try_send(c, std::ptr::addr_of!(send_val).cast::<u8>()) }
                        == 1
                    {
                        return i as i64;
                    }
                }
            }
            if let Some(idx) = default_index {
                return idx as i64;
            }
            // A cancelled cohort makes every blocking arm behave the way a
            // closed channel does, so a select under cancellation resolves
            // instead of waiting for a partner that will never come. The
            // check sits after the poll above: an arm that is already ready
            // is taken, because cancellation ends the wait rather than
            // discarding a value the channel is holding.
            if super::cohort::current_is_cancelled() {
                builder.last_value = 0;
                return default_index.map_or(0, |index| index as i64);
            }
            // Nothing ready, no default: block until a channel changes, then
            // re-poll. Mirrors the single-channel recv/send park discipline,
            // registering on every arm's queue so any sender/receiver wakes us.
            // No arm can become ready if nothing is left to run.
            crate::sched_global::report_deadlock_if_stuck("select");
            if gossamer_coro::in_goroutine() {
                let mut parked_as = None;
                let mut cohort_wait = 0i64;
                crate::sched_global::park(crate::sched::ParkReason::Chan, |parker| {
                    parked_as = Some(parker.gid);
                    cohort_wait = super::cohort::register_waiter(parker.gid);
                    for (kind, c, _) in &arms {
                        if c.is_null() {
                            continue;
                        }
                        let chan = unsafe { &**c };
                        if *kind == 0 {
                            chan.recv_waiters.fetch_add(1, Ordering::AcqRel);
                            chan.parked_recv.lock().push_back(parker.gid);
                        } else if *kind == 1 {
                            // A select send arm holds no queued value; it is
                            // waiting for the channel to become sendable.
                            chan.parked_send.lock().push_back(SendWaiter {
                                gid: parker.gid,
                                send_id: ANY_SEND,
                            });
                        }
                    }
                    // An arm may have become ready after the initial poll but
                    // before its waiter entry was published. Recheck after
                    // all entries exist; `unpark` records a pre-unpark wake
                    // until this coroutine has actually suspended.
                    if arms.iter().any(|(kind, c, _)| {
                        !c.is_null() && select_arm_is_ready(*kind, unsafe { &**c })
                    }) {
                        crate::sched_global::scheduler().unpark(parker.gid);
                    }
                });
                if let Some(gid) = parked_as {
                    super::cohort::deregister_waiter(cohort_wait, gid);
                    for (kind, c, _) in &arms {
                        if c.is_null() {
                            continue;
                        }
                        let chan = unsafe { &**c };
                        if *kind == 0 {
                            chan.recv_waiters.fetch_sub(1, Ordering::AcqRel);
                            chan.parked_recv.lock().retain(|g| *g != gid);
                        } else if *kind == 1 {
                            chan.parked_send.lock().retain(|w| w.gid != gid);
                        }
                    }
                }
            } else {
                // OS-thread fallback: a select waits on several channels, but a
                // single condvar wait tracks only one. Bounded-wait on the first
                // operable channel and re-poll; the 50 ms bound is the
                // missed-notify backstop, matching the walker's park cadence.
                let first = arms
                    .iter()
                    .find(|(kind, c, _)| *kind != 2 && !c.is_null())
                    .map(|(_, c, _)| *c);
                if let Some(c) = first {
                    let chan = unsafe { &*c };
                    let mut guard = chan.buf.lock();
                    chan.not_empty
                        .wait_for(&mut guard, std::time::Duration::from_millis(50));
                    drop(guard);
                } else {
                    return -1;
                }
            }
        }
    })
}

/// Returns the value popped by the most recent `gos_rt_select_wait` recv
/// outcome on this builder (0 for a send/default outcome or a null builder).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_select_value(b: *mut SelectBuilder) -> i64 {
    ffi_entry!(0, {
        if b.is_null() {
            return 0;
        }
        unsafe { &*b }.last_value
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_select_free(b: *mut SelectBuilder) {
    ffi_entry!((), {
        if b.is_null() {
            return;
        }
        unsafe {
            drop(Box::from_raw(b));
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn cap_zero_channel_send_waits_for_receiver() {
        let chan = unsafe { gos_rt_chan_new(8, 0) };
        assert!(!chan.is_null());
        let done = Arc::new(AtomicBool::new(false));
        let done_tx = Arc::clone(&done);
        let addr = chan as usize;
        let sender = std::thread::spawn(move || {
            let value = 77_i64;
            unsafe {
                gos_rt_chan_send(addr as *mut GosChan, std::ptr::addr_of!(value).cast());
            }
            done_tx.store(true, Ordering::Release);
        });
        crate::platform::sleep(Duration::from_millis(20));
        assert!(
            !done.load(Ordering::Acquire),
            "unbuffered send returned before recv"
        );
        let mut out = 0_i64;
        let ok = unsafe { gos_rt_chan_recv(chan, std::ptr::addr_of_mut!(out).cast()) };
        assert_eq!(ok, 1);
        assert_eq!(out, 77);
        sender.join().expect("sender");
        assert!(done.load(Ordering::Acquire));
        unsafe { gos_rt_chan_drop(chan) };
    }

    #[test]
    fn consuming_a_value_releases_the_sender_that_queued_it() {
        let mut queue = std::collections::VecDeque::from(vec![
            SendWaiter {
                gid: crate::sched::Gid(7),
                send_id: 2,
            },
            SendWaiter {
                gid: crate::sched::Gid(3),
                send_id: 1,
            },
        ]);
        // Taking value 1 must release gid 3, which queued it - not the
        // longest-waiting sender, whose own value is still in the buffer.
        assert_eq!(
            take_released_senders(&mut queue, 1),
            vec![crate::sched::Gid(3)]
        );
        assert_eq!(
            queue.iter().map(|w| w.gid).collect::<Vec<_>>(),
            vec![crate::sched::Gid(7)]
        );
    }

    #[test]
    fn untagged_consumption_releases_the_longest_waiting_sender() {
        let mut queue = std::collections::VecDeque::from(vec![
            SendWaiter {
                gid: crate::sched::Gid(4),
                send_id: ANY_SEND,
            },
            SendWaiter {
                gid: crate::sched::Gid(5),
                send_id: ANY_SEND,
            },
        ]);
        assert_eq!(
            take_released_senders(&mut queue, ANY_SEND),
            vec![crate::sched::Gid(4)]
        );
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn a_tagged_consumption_also_releases_one_untagged_waiter() {
        let mut queue = std::collections::VecDeque::from(vec![
            SendWaiter {
                gid: crate::sched::Gid(9),
                send_id: ANY_SEND,
            },
            SendWaiter {
                gid: crate::sched::Gid(1),
                send_id: 4,
            },
        ]);
        assert_eq!(
            take_released_senders(&mut queue, 4),
            vec![crate::sched::Gid(1), crate::sched::Gid(9)]
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn a_consumption_no_sender_is_waiting_for_releases_nobody() {
        let mut queue = std::collections::VecDeque::from(vec![SendWaiter {
            gid: crate::sched::Gid(2),
            send_id: 8,
        }]);
        assert!(take_released_senders(&mut queue, 3).is_empty());
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn every_unbuffered_sender_completes_under_a_single_receiver() {
        const SENDERS: i64 = 8;
        const PER_SENDER: i64 = 20;

        let chan = unsafe { gos_rt_chan_new(8, 0) };
        assert!(!chan.is_null());
        let addr = chan as usize;
        let senders: Vec<_> = (0..SENDERS)
            .map(|id| {
                std::thread::spawn(move || {
                    for i in 0..PER_SENDER {
                        let value = id * 100 + i;
                        unsafe {
                            gos_rt_chan_send(
                                addr as *mut GosChan,
                                std::ptr::addr_of!(value).cast(),
                            );
                        }
                    }
                })
            })
            .collect();

        let mut sum = 0i64;
        for _ in 0..SENDERS * PER_SENDER {
            let mut out = 0i64;
            assert_eq!(
                unsafe { gos_rt_chan_recv(chan, std::ptr::addr_of_mut!(out).cast()) },
                1
            );
            sum += out;
        }
        // Every send must have returned. A misrouted wake leaves one
        // sender blocked on a value that was already taken, and this join
        // never completes.
        for sender in senders {
            sender.join().expect("sender");
        }

        let expected: i64 = (0..SENDERS)
            .flat_map(|id| (0..PER_SENDER).map(move |i| id * 100 + i))
            .sum();
        assert_eq!(sum, expected);
        unsafe { gos_rt_chan_drop(chan) };
    }

    #[test]
    fn cap_negative_channel_is_explicitly_unbounded() {
        let chan = unsafe { gos_rt_chan_new(8, -1) };
        assert!(!chan.is_null());
        let value = 11_i64;
        let sent = unsafe { gos_rt_chan_try_send(chan, std::ptr::addr_of!(value).cast()) };
        assert_eq!(sent, 1);
        let mut out = 0_i64;
        let got = unsafe { gos_rt_chan_try_recv(chan, std::ptr::addr_of_mut!(out).cast()) };
        assert_eq!(got, 1);
        assert_eq!(out, 11);
        unsafe { gos_rt_chan_drop(chan) };
    }

    #[test]
    fn a_retained_channel_outlives_the_first_drop() {
        let chan = unsafe { gos_rt_chan_new(8, 1) };
        assert!(!chan.is_null());
        // SAFETY: `chan` is the channel constructed above, still live.
        unsafe { chan_retain(&*chan) };
        unsafe { gos_rt_chan_drop(chan) };
        // The second party still reaches the channel: sending and
        // receiving here would fault if the first drop had reclaimed it.
        let value = 5_i64;
        let sent = unsafe { gos_rt_chan_try_send(chan, std::ptr::addr_of!(value).cast()) };
        assert_eq!(sent, 1);
        let mut out = 0_i64;
        let got = unsafe { gos_rt_chan_try_recv(chan, std::ptr::addr_of_mut!(out).cast()) };
        assert_eq!(got, 1);
        assert_eq!(out, 5);
        unsafe { gos_rt_chan_drop(chan) };
    }
}
