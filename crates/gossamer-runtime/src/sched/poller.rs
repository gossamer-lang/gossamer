//! Network poller abstraction.
//!
//! The poller is the bridge between a parked goroutine waiting on
//! I/O readiness and the OS-level mechanism that delivers it. Two
//! concrete implementations live here:
//!
//! - [`MockPoller`] - deterministic, in-memory; readiness events are
//!   synthesised by the caller. Used by unit tests.
//! - [`OsPoller`] - `mio`-backed, wraps `epoll` (Linux), `kqueue`
//!   (macOS/BSD), or IOCP (Windows). Production runtime path.
//!
//! Both implementations satisfy the [`Poller`] trait. The scheduler
//! integrates with the trait, never with the underlying OS handle,
//! so swapping implementations during tests is mechanical.
//!
//! Timers piggy-back on the same event loop: callers register a
//! deadline through [`OsPoller::add_timer`]; the next call to
//! [`Poller::poll`] returns timer firings as ordinary [`Readiness`]
//! events with `interest == Interest::Timer`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::{BinaryHeap, HashSet};
use std::io;
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use crate::platform::Instant;

use super::task::Gid;

/// Opaque identifier for a registered I/O source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PollSource(pub u32);

/// Direction (or kind) a goroutine is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Interest {
    /// Wait for the source to become readable.
    Readable,
    /// Wait for the source to become writable.
    Writable,
    /// Synthetic kind used for timer firings.
    Timer,
}

/// Ready event returned from the poller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Readiness {
    /// Source that is now ready (or the timer id for timer events).
    pub source: PollSource,
    /// Direction that fired.
    pub interest: Interest,
    /// Goroutine to resume.
    pub gid: Gid,
}

/// Minimal interface a poller must satisfy.
pub trait Poller: Send {
    /// Registers a goroutine's interest in `source` firing.
    fn register(&mut self, source: PollSource, interest: Interest, gid: Gid);

    /// Removes any outstanding registration matching `source` +
    /// `interest`.
    fn deregister(&mut self, source: PollSource, interest: Interest);

    /// Drains every readiness event accumulated since the last call,
    /// without blocking.
    fn drain(&mut self) -> Vec<Readiness>;

    /// Blocks until at least one event is ready or `timeout` elapses,
    /// then drains the readiness queue. Implementations are free to
    /// return early if `timeout` is `Some(Duration::ZERO)`.
    fn poll(&mut self, timeout: Option<Duration>) -> io::Result<Vec<Readiness>> {
        // Default fallback for pollers that do not have a real
        // blocking primitive - sleeps and then drains.
        if let Some(t) = timeout {
            crate::platform::sleep(t);
        }
        Ok(self.drain())
    }
}

/// Deterministic in-memory poller used by tests and by platforms that
/// do not yet have an OS backend. Callers fire readiness events
/// explicitly via [`MockPoller::fire`].
#[derive(Debug, Default)]
pub struct MockPoller {
    registrations: HashMap<(PollSource, Interest), Gid>,
    pending: Vec<Readiness>,
}

impl MockPoller {
    /// Returns a fresh poller with no registered sources.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Synthesises a readiness event for `source` + `interest`. The
    /// corresponding goroutine, if registered, will be delivered on
    /// the next call to [`Poller::drain`].
    pub fn fire(&mut self, source: PollSource, interest: Interest) {
        if let Some(gid) = self.registrations.remove(&(source, interest)) {
            self.pending.push(Readiness {
                source,
                interest,
                gid,
            });
        }
    }
}

impl Poller for MockPoller {
    fn register(&mut self, source: PollSource, interest: Interest, gid: Gid) {
        self.registrations.insert((source, interest), gid);
    }

    fn deregister(&mut self, source: PollSource, interest: Interest) {
        self.registrations.remove(&(source, interest));
    }

    fn drain(&mut self) -> Vec<Readiness> {
        std::mem::take(&mut self.pending)
    }
}

/// Min-heap entry for the timer wheel. Sorted by expiry; the entry
/// with the soonest deadline pops first.
#[derive(Debug, Clone, Copy)]
#[cfg(not(target_arch = "wasm32"))]
struct TimerEntry {
    deadline: Instant,
    source: PollSource,
    gid: Gid,
}

#[cfg(not(target_arch = "wasm32"))]
impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.source == other.source
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Eq for TimerEntry {}

#[cfg(not(target_arch = "wasm32"))]
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse: BinaryHeap is max-heap by default and we want the
        // *earliest* deadline at the top.
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.source.0.cmp(&self.source.0))
    }
}

/// Production poller backed by `mio` (epoll / kqueue / IOCP).
///
/// The poller owns a `mio::Poll` plus a `mio::Registry`, and tracks
/// every registered source in a side map so deregister calls can
/// look up the underlying [`mio::event::Source`] handle. Sources are
/// registered by the network code via [`OsPoller::register_io`],
/// which wraps the registration with the [`Interest`] -> mio
/// translation.
/// Reserved mio token for the internal `mio::Waker` that lets us
/// interrupt a long-blocking `poll()` from another thread (e.g.
/// when a goroutine registers a new I/O source while the poller
/// thread is mid-syscall). Numerically distinct from the
/// `next_token` allocator, which starts at 1.
#[cfg(not(target_arch = "wasm32"))]
const INTERRUPT_TOKEN: mio::Token = mio::Token(0);

/// `mio`-backed OS poller. Holds the `mio::Poll` handle plus
/// bookkeeping for outstanding registrations (one entry per
/// `(PollSource, Interest)` pair), the timer wheel, and the
/// pending readiness buffer drained between polls.
#[cfg(not(target_arch = "wasm32"))]
pub struct OsPoller {
    /// The kernel poll handle, locked apart from the bookkeeping below so a
    /// thread blocked in it never holds up a registration.
    driver: std::sync::Arc<parking_lot::Mutex<PollDriver>>,
    /// Registers and deregisters sources while another thread polls.
    registry: mio::Registry,
    /// `mio::Waker` bound to this poll. Calling `.wake()` from any
    /// thread unblocks an in-flight `poll()` call immediately -
    /// used by `register_io` to let a freshly registered source
    /// take effect without waiting for the current poll cycle's
    /// timeout to expire.
    interrupt: std::sync::Arc<mio::Waker>,
    /// Map registered `PollSource` -> `(mio::Token, Gid)` so
    /// `deregister` can find the entry to remove without scanning
    /// [`Self::by_token`], which a busy server walks once per completed read.
    by_source: HashMap<(PollSource, Interest), (Option<mio::Token>, Gid)>,
    /// Pending readiness events accumulated between `poll` and
    /// `drain` calls.
    pending: Vec<Readiness>,
    /// Outstanding timer wheel.
    timers: BinaryHeap<TimerEntry>,
    /// Timers cannot be removed cheaply from `BinaryHeap`.  Remember
    /// cancelled sources and discard them when they reach the heap top.
    /// This is essential for I/O deadlines: a readiness wake must not leave
    /// a timer behind that can later unpark the goroutine during an unrelated
    /// wait.
    cancelled_timers: HashSet<PollSource>,
    /// Next free token id used when registering with mio.
    next_token: usize,
    /// Map from mio Token to `(PollSource, Gid, Interest)`.
    by_token: HashMap<mio::Token, (PollSource, Interest, Gid)>,
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for OsPoller {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("OsPoller")
            .field("registered", &self.by_source.len())
            .field("pending", &self.pending.len())
            .field("timers", &self.timers.len())
            .finish_non_exhaustive()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl OsPoller {
    /// Builds a fresh OS-backed poller. Returns an error if the
    /// kernel rejects the underlying `epoll_create1` / `kqueue` /
    /// `CreateIoCompletionPort` syscall.
    pub fn new() -> io::Result<Self> {
        let poll = mio::Poll::new()?;
        let interrupt = std::sync::Arc::new(mio::Waker::new(poll.registry(), INTERRUPT_TOKEN)?);
        let registry = poll.registry().try_clone()?;
        Ok(Self {
            driver: std::sync::Arc::new(parking_lot::Mutex::new(PollDriver {
                poll,
                events: mio::Events::with_capacity(1024),
            })),
            registry,
            interrupt,
            by_source: HashMap::new(),
            pending: Vec::new(),
            timers: BinaryHeap::new(),
            cancelled_timers: HashSet::new(),
            next_token: 1,
            by_token: HashMap::new(),
        })
    }

    /// Returns a clone of the interrupt-waker Arc. Callers fire
    /// `.wake()` on it before waiting for the poller lock and after
    /// changing poller state. The first wake releases an in-flight
    /// `poll()`; the second makes the poller observe the fresh state.
    #[must_use]
    pub fn interrupt_handle(&self) -> std::sync::Arc<mio::Waker> {
        std::sync::Arc::clone(&self.interrupt)
    }

    /// Registers a goroutine `gid` to wake when `io` reports the
    /// requested `interest`. Returns the [`PollSource`] handle the
    /// caller should later use to deregister.
    ///
    /// Falls back to `reregister` when the source is already known to
    /// the OS poller (epoll/kqueue reject re-`register` with
    /// `AlreadyExists`). Without this fallback, the second `wait_io`
    /// call on a long-lived connection silently parks the goroutine
    /// with no netpoller subscription - every keep-alive connection
    /// would stall on its second request until the client timeout
    /// fired, surfacing as ~200+ "deadline exceeded" failures per
    /// 30-second bench run.
    pub fn register_io<S: mio::event::Source + ?Sized>(
        &mut self,
        io: &mut S,
        interest: Interest,
        gid: Gid,
    ) -> io::Result<PollSource> {
        // Skip token 0 (reserved for the interrupt-waker).
        if self.next_token == INTERRUPT_TOKEN.0 {
            self.next_token = 1;
        }
        let token = mio::Token(self.next_token);
        self.next_token = self.next_token.wrapping_add(1).max(1);
        let source = PollSource(u32::try_from(token.0 & 0xFFFF_FFFF).unwrap_or(0));
        let mio_int = match interest {
            Interest::Readable => mio::Interest::READABLE,
            Interest::Writable => mio::Interest::WRITABLE,
            Interest::Timer => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Interest::Timer is reserved for add_timer",
                ));
            }
        };
        let registered = match self.registry.register(io, token, mio_int) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.registry.reregister(io, token, mio_int)?;
                false
            }
            Err(e) => return Err(e),
        };
        let _ = registered;
        self.by_source
            .insert((source, interest), (Some(token), gid));
        self.by_token.insert(token, (source, interest, gid));
        Ok(source)
    }

    /// Removes a previously registered source.
    pub fn deregister_io<S: mio::event::Source + ?Sized>(
        &mut self,
        io: &mut S,
        source: PollSource,
        interest: Interest,
    ) -> io::Result<()> {
        if let Some((Some(token), _)) = self.by_source.remove(&(source, interest)) {
            self.by_token.remove(&token);
        }
        self.registry.deregister(io)
    }

    /// Adds a one-shot timer that fires at `deadline`. Returns the
    /// [`PollSource`] handle that identifies this timer.
    pub fn add_timer(&mut self, deadline: Instant, gid: Gid) -> PollSource {
        let token = mio::Token(self.next_token);
        self.next_token = self.next_token.wrapping_add(1).max(1);
        let source = PollSource(u32::try_from(token.0 & 0xFFFF_FFFF).unwrap_or(0));
        self.timers.push(TimerEntry {
            deadline,
            source,
            gid,
        });
        source
    }

    /// Cancels a timer previously returned by [`Self::add_timer`].
    ///
    /// Removal is lazy because `BinaryHeap` has no indexed removal.  The
    /// source is discarded from the heap and pending queue before it can be
    /// delivered, so cancellation cannot produce a stale scheduler wake.
    pub fn cancel_timer(&mut self, source: PollSource) {
        self.cancelled_timers.insert(source);
        self.pending
            .retain(|event| !(event.source == source && event.interest == Interest::Timer));
    }

    /// Returns the duration until the next timer fires, or `None` if
    /// no timer is pending.
    fn next_timeout(&self, base: Option<Duration>) -> Option<Duration> {
        let now = Instant::now();
        let timer_dur = self.timers.peek().map(|entry| {
            if entry.deadline <= now {
                Duration::ZERO
            } else {
                entry.deadline - now
            }
        });
        match (base, timer_dur) {
            (None, t) => t,
            (Some(b), None) => Some(b),
            (Some(b), Some(t)) => Some(b.min(t)),
        }
    }

    fn drain_expired_timers(&mut self) {
        let now = Instant::now();
        while let Some(top) = self.timers.peek() {
            if top.deadline > now {
                break;
            }
            let entry = self.timers.pop().expect("peeked timer disappeared");
            if self.cancelled_timers.remove(&entry.source) {
                continue;
            }
            self.pending.push(Readiness {
                source: entry.source,
                interest: Interest::Timer,
                gid: entry.gid,
            });
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Poller for OsPoller {
    fn register(&mut self, source: PollSource, interest: Interest, gid: Gid) {
        // Type-erased entry point; the OsPoller's authoritative entry
        // is `register_io`. Recording in `by_source` is enough to
        // satisfy the trait contract for code paths that only use
        // synthetic sources (e.g. timers via `add_timer`).
        self.by_source.insert((source, interest), (None, gid));
    }

    fn deregister(&mut self, source: PollSource, interest: Interest) {
        self.by_source.remove(&(source, interest));
    }

    fn drain(&mut self) -> Vec<Readiness> {
        std::mem::take(&mut self.pending)
    }

    fn poll(&mut self, timeout: Option<Duration>) -> io::Result<Vec<Readiness>> {
        // mio's `poll` can return early without events on every
        // platform - spurious wakeups, signal interruption, or
        // simply rounding the remaining timeout down to zero.
        // Loop until we have an event, an interrupt asks the caller to
        // re-read the registrations, or the caller-supplied deadline passes.
        let driver = self.driver_handle();
        let user_deadline = timeout.map(|t| Instant::now() + t);
        loop {
            self.drain_expired_timers();
            if !self.pending.is_empty() {
                return Ok(self.drain());
            }
            let user_remaining = user_deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if matches!(user_remaining, Some(d) if d.is_zero()) && self.timers.peek().is_none() {
                return Ok(self.drain());
            }
            let combined = self.next_timeout(user_remaining);
            let events = driver.lock().wait(combined)?;
            let interrupted = self.absorb(&events);
            self.drain_expired_timers();
            if !self.pending.is_empty() {
                return Ok(self.drain());
            }
            if interrupted {
                return Ok(Vec::new());
            }
            // No events. If the user gave a timeout and it has
            // elapsed, return empty. Otherwise loop and re-poll.
            if let Some(d) = user_deadline {
                if Instant::now() >= d {
                    return Ok(self.drain());
                }
            } else if self.timers.peek().is_none() {
                // No deadline and no timer pending - re-polling
                // would block forever. Return empty.
                return Ok(self.drain());
            }
        }
    }
}

/// The kernel poll handle and its event buffer.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct PollDriver {
    poll: mio::Poll,
    events: mio::Events,
}

/// One readiness report from the kernel: the source's token and the
/// directions it reported.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawEvent {
    token: mio::Token,
    readable: bool,
    writable: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl PollDriver {
    /// Blocks until a source is ready, the waker fires, or `timeout` passes.
    pub(crate) fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Vec<RawEvent>> {
        self.poll.poll(&mut self.events, timeout)?;
        Ok(self
            .events
            .iter()
            .map(|event| RawEvent {
                token: event.token(),
                readable: event.is_readable(),
                writable: event.is_writable(),
            })
            .collect())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl OsPoller {
    /// The kernel poll handle, for a thread that waits on it without holding
    /// this poller's bookkeeping.
    pub(crate) fn driver_handle(&self) -> std::sync::Arc<parking_lot::Mutex<PollDriver>> {
        std::sync::Arc::clone(&self.driver)
    }

    /// How long a wait may block before the earliest timer is due, or `None`
    /// when no timer is pending.
    pub(crate) fn timer_timeout(&self) -> Option<Duration> {
        self.next_timeout(None)
    }

    /// Records the readiness a wait reported and every timer now due, and
    /// answers what became ready. The waker's own event only ends a wait.
    pub(crate) fn settle(&mut self, events: &[RawEvent]) -> Vec<Readiness> {
        self.absorb(events);
        self.drain_expired_timers();
        self.drain()
    }

    /// Queues the readiness `events` report; answers whether the waker fired.
    fn absorb(&mut self, events: &[RawEvent]) -> bool {
        let mut interrupted = false;
        for event in events {
            if event.token == INTERRUPT_TOKEN {
                interrupted = true;
                continue;
            }
            if let Some(&(source, interest, gid)) = self.by_token.get(&event.token) {
                let fired = match interest {
                    Interest::Readable => event.readable,
                    Interest::Writable => event.writable,
                    Interest::Timer => false,
                };
                if fired {
                    self.pending.push(Readiness {
                        source,
                        interest,
                        gid,
                    });
                }
            }
        }
        interrupted
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn os_poller_round_trips_a_timer() {
        let mut poller = OsPoller::new().expect("OsPoller::new");
        let when = Instant::now() + Duration::from_millis(5);
        let _ = poller.add_timer(when, Gid(7));
        let events = poller
            .poll(Some(Duration::from_millis(50)))
            .expect("OsPoller::poll");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].gid, Gid(7));
        assert!(matches!(events[0].interest, Interest::Timer));
    }

    #[test]
    fn cancelled_timer_is_never_delivered() {
        let mut poller = OsPoller::new().expect("OsPoller::new");
        let source = poller.add_timer(Instant::now(), Gid(9));
        poller.cancel_timer(source);
        let events = poller
            .poll(Some(Duration::from_millis(5)))
            .expect("OsPoller::poll");
        assert!(
            events.is_empty(),
            "cancelled deadline must not wake later work"
        );
    }

    #[test]
    fn os_poller_returns_empty_with_no_work() {
        let mut poller = OsPoller::new().expect("OsPoller::new");
        let events = poller
            .poll(Some(Duration::from_millis(1)))
            .expect("OsPoller::poll");
        assert!(events.is_empty());
    }

    #[test]
    fn interrupt_wakes_an_in_flight_poll() {
        let mut poller = OsPoller::new().expect("OsPoller::new");
        let interrupt = poller.interrupt_handle();
        let wake = std::thread::spawn(move || {
            crate::platform::sleep(Duration::from_millis(20));
            interrupt.wake().expect("wake poller");
        });
        let start = Instant::now();
        let events = poller
            .poll(Some(Duration::from_secs(5)))
            .expect("OsPoller::poll");
        wake.join().expect("poller interrupt thread panicked");
        assert!(events.is_empty());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "interrupt did not release poll promptly"
        );
    }

    #[test]
    fn next_timeout_picks_earlier_value() {
        let mut poller = OsPoller::new().expect("OsPoller::new");
        let when = Instant::now() + Duration::from_mins(1);
        let _ = poller.add_timer(when, Gid(1));
        let dt = poller.next_timeout(Some(Duration::from_millis(10)));
        // Caller's 10 ms is the earlier deadline.
        assert!(dt.unwrap() <= Duration::from_millis(10));
    }
}

/// Stub OS poller for wasm32-unknown-unknown, where there is no mio
/// netpoller. The single-threaded playground never performs real
/// blocking I/O (the network std modules are gated out), so this
/// poller registers nothing and delivers no readiness events. It
/// exists so `gossamer_sched` can re-export `OsPoller` unchanged.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Default)]
pub struct OsPoller;

#[cfg(target_arch = "wasm32")]
impl OsPoller {
    /// Constructs the stub poller. Never fails.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self)
    }
}

#[cfg(target_arch = "wasm32")]
impl Poller for OsPoller {
    fn register(&mut self, _source: PollSource, _interest: Interest, _gid: Gid) {}

    fn deregister(&mut self, _source: PollSource, _interest: Interest) {}

    fn drain(&mut self) -> Vec<Readiness> {
        Vec::new()
    }
}
