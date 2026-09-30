//! Channels: buffered and rendezvous queues between goroutines, and the waiter registry.

use super::{RuntimeError, Value};

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

/// What a send did.
///
/// A send into a closed channel is a program error, matching Go: the
/// receiver has been told no more values are coming. A deadlocked send is
/// one no runnable goroutine can ever complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The value is on the channel.
    Sent,
    /// Nothing left running can take the value.
    Deadlocked,
    /// The channel was closed.
    Closed,
}

/// Shared channel backing a `(Sender<T>, Receiver<T>)` pair.
///
/// Capacity semantics mirror modern Go where `0` is an unbuffered
/// rendezvous channel, positive values are bounded buffers, and
/// `Channel::unbounded()` is the explicit queue form retained for
/// Gossamer code that wants non-blocking producer growth.
#[derive(Clone)]
pub struct Channel {
    inner: Arc<ChannelInner>,
}

impl Channel {
    /// Stable identity of this channel, shared by every clone of it.
    /// A join handle is a channel, so this is what keys a cohort child
    /// to the handle its joiner holds.
    #[must_use]
    pub fn identity(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }
}

struct ChannelInner {
    state: Mutex<ChannelState>,
    cv: parking_lot::Condvar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelCapacity {
    Unbuffered,
    Unbounded,
    Bounded(usize),
}

struct ChannelMessage {
    id: u64,
    value: Value,
}

struct ChannelState {
    buf: VecDeque<ChannelMessage>,
    capacity: ChannelCapacity,
    closed: bool,
    next_send_id: u64,
    waiting_receivers: usize,
    waiting_senders: usize,
    select_waiters: Vec<Arc<SelectWaiter>>,
    /// Whether this channel currently contributes to the pending-handoff count
    /// maintained through [`crate::vm::goroutine::adjust_pending_handoffs`],
    /// kept in step with [`ChannelState::has_ready_waiter`] so the global count
    /// is a plain sum.
    counted_ready: bool,
}

impl ChannelState {
    /// True when a thread already waiting on this channel would complete if
    /// it woke right now: a queued value with a receiver to take it, or room
    /// (or a receiver) for a blocked sender's value.
    ///
    /// A closed channel releases every waiter, so it is always ready.
    fn has_ready_waiter(&self) -> bool {
        if self.closed && (self.waiting_receivers > 0 || self.waiting_senders > 0) {
            return true;
        }
        if self.waiting_receivers > 0 && !self.buf.is_empty() {
            return true;
        }
        if self.waiting_senders == 0 {
            return false;
        }
        match self.capacity {
            // An unbuffered sender's value is queued until a receiver takes
            // it. A receiver present means the handoff is imminent; an empty
            // buffer means it already happened and the sender has simply not
            // observed it yet. Either way the sender is not stuck.
            ChannelCapacity::Unbuffered => self.waiting_receivers > 0 || self.buf.is_empty(),
            ChannelCapacity::Unbounded => true,
            ChannelCapacity::Bounded(capacity) => self.buf.len() < capacity,
        }
    }
}

/// Wait handle used by the bytecode VM to park one `select` expression
/// across several channels and wake when any arm may be ready.
pub struct SelectWaiter {
    ready: Mutex<bool>,
    cv: parking_lot::Condvar,
}

impl SelectWaiter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ready: Mutex::new(false),
            cv: parking_lot::Condvar::new(),
        })
    }

    fn wake(&self) {
        let mut ready = self.ready.lock();
        *ready = true;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut ready = self.ready.lock();
        while !*ready {
            self.cv.wait(&mut ready);
        }
    }
}

/// Every channel still reachable, so a deadlock report can recompute
/// readiness from the channels themselves rather than trust a counter that
/// any interleaving could have left behind.
static LIVE_CHANNELS: Mutex<Vec<std::sync::Weak<ChannelInner>>> = Mutex::new(Vec::new());

fn register_live_channel(inner: &Arc<ChannelInner>) {
    let mut live = LIVE_CHANNELS.lock();
    live.retain(|weak| weak.strong_count() > 0);
    live.push(Arc::downgrade(inner));
}

/// True when any channel other than `holding` has a waiter that would
/// complete, or is being changed right now by another thread.
///
/// Called only once the waiter counts already say every participant is
/// blocked, so the walk is rare. A channel whose lock is held elsewhere is
/// mid-update and counts as progress: reporting a deadlock over a state
/// still being written would turn a working program into a failure, while
/// missing one only means the program waits as it did before.
fn any_channel_can_progress(holding: &ChannelInner) -> bool {
    let live = LIVE_CHANNELS.lock();
    for weak in live.iter() {
        let Some(inner) = weak.upgrade() else {
            continue;
        };
        if std::ptr::eq(Arc::as_ptr(&inner), std::ptr::from_ref(holding)) {
            continue;
        }
        let Some(state) = inner.state.try_lock() else {
            return true;
        };
        if state.has_ready_waiter() {
            return true;
        }
    }
    false
}

/// Wakes every goroutine parked on a channel so each re-evaluates whether
/// the program can still make progress. Called once when `main` returns:
/// the set of participants shrinks at that moment, and a waiter parked
/// before it can only learn so by being given a chance to look again.
pub fn wake_all_channel_waiters() {
    // The registry is released before any channel lock is taken. A waiter
    // deciding whether to park holds its own channel's lock and reads the
    // registry from under it, so a walk that held the registry and then
    // waited on a channel's lock would take the two in the opposite order.
    let channels: Vec<Arc<ChannelInner>> = {
        let live = LIVE_CHANNELS.lock();
        live.iter().filter_map(std::sync::Weak::upgrade).collect()
    };
    for inner in channels {
        // Taken and released around the notify: a waiter decides
        // whether to sleep while holding this lock, so notifying
        // without it can land in the gap between that decision and
        // the wait, waking nobody and leaving the waiter asleep on a
        // condition that has already changed.
        drop(inner.state.lock());
        inner.cv.notify_all();
    }
}

/// The deadlock report: every participant is waiting on a channel, so the
/// value the operation waits for can never arrive.
#[must_use]
pub fn deadlock_error(op: &'static str) -> RuntimeError {
    if gossamer_runtime::platform::CAN_BLOCK {
        RuntimeError::Panic(format!(
            "all goroutines are asleep - deadlock! ({op} can never complete)"
        ))
    } else {
        RuntimeError::WouldNeverWake(op)
    }
}

/// Result of a blocking receive.
#[derive(Debug)]
pub enum RecvOutcome {
    /// A value was taken from the channel.
    Value(Value),
    /// Every sender is gone and the channel is drained.
    Closed,
    /// Nothing in the program can send: every participant is waiting on a
    /// channel, so no value will ever arrive.
    Deadlocked,
}

impl Channel {
    /// Constructs a new unbuffered channel.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// Constructs an explicit unbounded queue channel.
    #[must_use]
    pub fn unbounded() -> Self {
        Self::with_mode(ChannelCapacity::Unbounded)
    }

    /// Constructs a channel with the given buffered capacity. A
    /// `capacity` of `0` is unbuffered; a positive value bounds the
    /// buffer so a send parks once the buffer reaches capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        if capacity == 0 {
            Self::with_mode(ChannelCapacity::Unbuffered)
        } else {
            Self::with_mode(ChannelCapacity::Bounded(capacity))
        }
    }

    fn with_mode(capacity: ChannelCapacity) -> Self {
        let channel = Self {
            inner: Arc::new(ChannelInner {
                state: Mutex::new(ChannelState {
                    buf: VecDeque::new(),
                    capacity,
                    closed: false,
                    next_send_id: 1,
                    waiting_receivers: 0,
                    waiting_senders: 0,
                    select_waiters: Vec::new(),
                    counted_ready: false,
                }),
                cv: parking_lot::Condvar::new(),
            }),
        };
        register_live_channel(&channel.inner);
        channel
    }

    /// Pushes `value` onto the channel and notifies any parked
    /// receiver so it can re-check. On a bounded channel (positive
    /// capacity) the caller parks on the condvar while the buffer is at
    /// capacity, so a producer outrunning its consumer applies
    /// backpressure exactly as the compiled tier's `gos_rt_chan_send`.
    #[must_use]
    pub fn send(&self, value: Value) -> SendOutcome {
        let mut guard = self.inner.state.lock();
        // A receiver has been told no more values are coming, so a value
        // handed over after the close would be dropped where the program
        // expects it delivered. Go panics here and so does this.
        if guard.closed {
            return SendOutcome::Closed;
        }
        match guard.capacity {
            ChannelCapacity::Unbuffered => {
                let id = guard.next_send_id;
                guard.next_send_id = guard.next_send_id.wrapping_add(1).max(1);
                guard.buf.push_back(ChannelMessage { id, value });
                // Register before announcing: a receiver that wakes on the
                // notify must find this sender already on record, or the
                // channel reads as holding a value nobody is waiting to hand
                // over. The registration is released only once the value has
                // been taken, for the same reason.
                guard.waiting_senders += 1;
                self.notify_channel_changed(&mut guard);
                let mut deadlocked = false;
                while guard.buf.iter().any(|msg| msg.id == id)
                    && !guard.closed
                    // A cancelled cohort has no reader left to take this
                    // value, so the send stops waiting for a handoff that
                    // is not coming.
                    && !crate::stdlib_builtins::cohort::current_is_cancelled()
                {
                    let Some(_waiting) = crate::vm::goroutine::ChannelWait::enter("send", || {
                        guard.has_ready_waiter()
                            || any_channel_can_progress(&self.inner)
                            || crate::stdlib_builtins::cohort::deadline_pending()
                    }) else {
                        deadlocked = true;
                        break;
                    };
                    self.inner.cv.wait(&mut guard);
                }
                guard.waiting_senders = guard.waiting_senders.saturating_sub(1);
                Self::sync_ready_count(&mut guard);
                if deadlocked {
                    return SendOutcome::Deadlocked;
                }
            }
            ChannelCapacity::Unbounded => {
                guard.buf.push_back(ChannelMessage { id: 0, value });
                self.notify_channel_changed(&mut guard);
            }
            ChannelCapacity::Bounded(capacity) => {
                if guard.buf.len() >= capacity {
                    guard.waiting_senders += 1;
                    Self::sync_ready_count(&mut guard);
                    let mut deadlocked = false;
                    while guard.buf.len() >= capacity {
                        let Some(_waiting) =
                            crate::vm::goroutine::ChannelWait::enter("send", || {
                                guard.has_ready_waiter() || any_channel_can_progress(&self.inner)
                            })
                        else {
                            deadlocked = true;
                            break;
                        };
                        self.inner.cv.wait(&mut guard);
                    }
                    guard.waiting_senders = guard.waiting_senders.saturating_sub(1);
                    Self::sync_ready_count(&mut guard);
                    if deadlocked {
                        return SendOutcome::Deadlocked;
                    }
                }
                guard.buf.push_back(ChannelMessage { id: 0, value });
                self.notify_channel_changed(&mut guard);
            }
        }
        // A close that landed while this send was parked leaves no reader
        // expecting the value, which is the same program error.
        if guard.closed {
            return SendOutcome::Closed;
        }
        SendOutcome::Sent
    }

    /// Non-blocking send. Enqueues `value` and returns `true` when the
    /// operation can complete immediately; returns
    /// `false` without enqueueing when a bounded buffer is at capacity.
    /// Used by `select` so a full send arm reads as not-ready instead
    /// of blocking inside the readiness probe.
    #[must_use]
    pub fn try_send(&self, value: Value) -> SendOutcome {
        let mut guard = self.inner.state.lock();
        if guard.closed {
            return SendOutcome::Closed;
        }
        match guard.capacity {
            ChannelCapacity::Unbuffered => {
                if guard.waiting_receivers == 0 {
                    return SendOutcome::Deadlocked;
                }
                guard.buf.push_back(ChannelMessage { id: 0, value });
                self.notify_channel_changed(&mut guard);
                SendOutcome::Sent
            }
            ChannelCapacity::Unbounded => {
                guard.buf.push_back(ChannelMessage { id: 0, value });
                self.notify_channel_changed(&mut guard);
                SendOutcome::Sent
            }
            ChannelCapacity::Bounded(capacity) => {
                if guard.buf.len() >= capacity {
                    return SendOutcome::Deadlocked;
                }
                guard.buf.push_back(ChannelMessage { id: 0, value });
                self.notify_channel_changed(&mut guard);
                SendOutcome::Sent
            }
        }
    }

    /// Marks the channel as closed and wakes every parked receiver
    /// so they observe the closed state and exit their wait. Returns
    /// `true` when this call performed the close and `false` when the
    /// channel was already closed - the caller turns the latter into
    /// a `close of closed channel` panic, matching Go. That panic is
    /// goroutine-scoped, so it ends only the offending goroutine
    /// (fatal on `main`) and never aborts the whole process.
    #[must_use]
    pub fn close(&self) -> bool {
        let mut guard = self.inner.state.lock();
        if guard.closed {
            return false;
        }
        guard.closed = true;
        self.notify_channel_changed(&mut guard);
        true
    }

    /// Non-blocking receive. Returns `None` when the channel is
    /// empty (regardless of close state - callers that need
    /// drain-aware semantics should use [`Channel::recv`]).
    #[must_use]
    pub fn try_recv(&self) -> Option<Value> {
        let mut guard = self.inner.state.lock();
        let value = guard.buf.pop_front().map(|msg| msg.value);
        if value.is_some() {
            self.notify_channel_changed(&mut guard);
        }
        value
    }

    /// Blocking receive. Parks until a value is available or the
    /// channel is closed AND drained. Returns `None` only after
    /// observing `closed = true && buf.is_empty()`. Mirrors Go's
    /// `v, ok := <-ch` shape so `while let Some(v) = rx.recv()`
    /// drains and exits cleanly when the producer closes.
    #[must_use]
    pub fn recv(&self) -> RecvOutcome {
        let mut guard = self.inner.state.lock();
        // The registration is held across the wait *and* the take that ends
        // it. Dropping it the moment the wait returns would leave a queued
        // value with no receiver on record, and a sender checking in that
        // window would read a channel that is about to hand off as stuck.
        let mut registered = false;
        let outcome = loop {
            // A cancelled cohort answers its children the way a closed
            // channel does: nothing more is coming. What already arrived is
            // still handed over, because a closed channel drains before it
            // answers `None` and cancellation says no more will be sent, not
            // that a delivered value is dropped. A join handle carries its
            // child's outcome on this path, and the failure that cancelled
            // the cohort is often the very value waiting in the buffer.
            if crate::stdlib_builtins::cohort::current_is_cancelled() {
                if let Some(msg) = guard.buf.pop_front() {
                    break RecvOutcome::Value(msg.value);
                }
                break RecvOutcome::Closed;
            }
            if let Some(msg) = guard.buf.pop_front() {
                break RecvOutcome::Value(msg.value);
            }
            if guard.closed {
                break RecvOutcome::Closed;
            }
            if !registered {
                registered = true;
                guard.waiting_receivers += 1;
                Self::sync_ready_count(&mut guard);
                self.wake_select_waiters(&guard);
            }
            let Some(_waiting) = crate::vm::goroutine::ChannelWait::enter("receive", || {
                guard.has_ready_waiter()
                    || any_channel_can_progress(&self.inner)
                    || crate::stdlib_builtins::context::deadline_pending()
                    || crate::stdlib_builtins::cohort::deadline_pending()
            }) else {
                break RecvOutcome::Deadlocked;
            };
            self.inner.cv.wait(&mut guard);
        };
        if registered {
            guard.waiting_receivers = guard.waiting_receivers.saturating_sub(1);
        }
        self.notify_channel_changed(&mut guard);
        outcome
    }

    /// Blocking receive which also observes a caller-provided cancellation
    /// predicate. A context that has already fired short-circuits without
    /// consuming a queued value, matching the native runtime's receive
    /// ordering, so the answer does not depend on whether a sender reached
    /// the channel first. The bounded wait makes a cancellation raised during
    /// the wait visible even though a Context does not share this channel's
    /// condvar.
    #[must_use]
    pub fn recv_with_cancel(&self, is_cancelled: impl Fn() -> bool) -> Option<Value> {
        if is_cancelled() {
            return None;
        }
        let mut guard = self.inner.state.lock();
        loop {
            if let Some(msg) = guard.buf.pop_front() {
                self.notify_channel_changed(&mut guard);
                return Some(msg.value);
            }
            if guard.closed || is_cancelled() {
                return None;
            }
            // Nothing can send, and no cancellation can be raised, while this
            // waits on the browser build, so waiting cannot change the answer
            // a drained open channel gives.
            if !gossamer_runtime::platform::CAN_BLOCK {
                return None;
            }
            guard.waiting_receivers += 1;
            self.wake_select_waiters(&guard);
            self.inner
                .cv
                .wait_for(&mut guard, Duration::from_millis(50));
            guard.waiting_receivers = guard.waiting_receivers.saturating_sub(1);
        }
    }

    /// Returns `true` when the channel currently has at least one
    /// pending value. Used by `select` to pick a ready arm.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        !self.inner.state.lock().buf.is_empty()
    }

    /// `true` when both buffer drained and channel closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        let guard = self.inner.state.lock();
        guard.closed && guard.buf.is_empty()
    }

    /// Registers a `select` waiter that will be woken when this
    /// channel's readiness may have changed.
    pub fn register_select_waiter(&self, waiter: &Arc<SelectWaiter>) {
        let mut guard = self.inner.state.lock();
        if !guard.select_waiters.iter().any(|w| Arc::ptr_eq(w, waiter)) {
            guard.select_waiters.push(Arc::clone(waiter));
        }
    }

    /// Removes a previously registered `select` waiter.
    pub fn unregister_select_waiter(&self, waiter: &Arc<SelectWaiter>) {
        let mut guard = self.inner.state.lock();
        guard.select_waiters.retain(|w| !Arc::ptr_eq(w, waiter));
    }

    /// Constructs a waiter for a blocking `select`.
    #[must_use]
    pub fn select_waiter() -> Arc<SelectWaiter> {
        SelectWaiter::new()
    }

    /// Blocks until any registered channel wakes the waiter.
    pub fn wait_select(waiter: &SelectWaiter) {
        waiter.wait();
    }

    fn notify_channel_changed(&self, guard: &mut ChannelState) {
        Self::sync_ready_count(guard);
        self.inner.cv.notify_all();
        self.wake_select_waiters(guard);
    }

    /// Re-reads this channel's readiness and applies the difference to the
    /// process-wide count. Called under the channel lock on every state
    /// change and on every waiter arrival or departure, so the count is
    /// always the number of channels with a waiter that could proceed.
    fn sync_ready_count(guard: &mut ChannelState) {
        let ready = guard.has_ready_waiter();
        if ready == guard.counted_ready {
            return;
        }
        guard.counted_ready = ready;
        crate::vm::goroutine::adjust_pending_handoffs(ready);
    }

    fn wake_select_waiters(&self, guard: &ChannelState) {
        let waiters = guard.select_waiters.clone();
        for waiter in waiters {
            waiter.wake();
        }
    }
}

impl Default for Channel {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Channel {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "<channel len={}>", self.inner.state.lock().buf.len())
    }
}
