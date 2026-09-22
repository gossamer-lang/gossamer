//! Socket readiness and sleep timers for goroutines, delivered on the worker
//! thread that runs the goroutine waiting for them.
//!
//! A goroutine stays on the worker it first ran on, so each worker index owns
//! a [`Poller`]: the sockets its goroutines registered, and the timers they
//! sleep on. A worker with nothing to run blocks in its own poller instead of
//! on a condition variable, and readiness it reads there makes its own
//! goroutines runnable with no other thread involved. A busy worker takes a
//! non-blocking pass every few dozen steps, the scheduler's watchdog drives
//! the poller of an index no worker occupies, and code outside the scheduler
//! gets a poller driven by a thread of its own.
//!
//! A socket is registered once, edge-triggered, for both directions, and owns
//! a [`PollDesc`] holding one waiter word per direction. A goroutine whose read
//! or write would block parks with its gid in that word; the pass that sees the
//! readiness swaps the word to `READY` and unparks whoever was there.
//!
//! I/O deadlines are swept on a short fixed cadence: a deadline bounds a
//! stalled peer, where seconds matter and a few milliseconds of slack do not.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;

use crate::platform::Instant;
use crate::sched::{Gid, ParkReason};

/// No goroutine waits and no readiness is pending.
const EMPTY: u64 = 0;
/// Readiness arrived with nobody waiting; the next wait returns at once.
const READY: u64 = 1;
/// A waiting goroutine is stored as its gid plus this offset.
const WAITER: u64 = 2;

/// How often I/O deadlines are checked.
const SWEEP: Duration = Duration::from_millis(10);

/// Token reserved for a poller's own waker.
const WAKER_TOKEN: mio::Token = mio::Token(usize::MAX);

/// A direction a goroutine waits on a socket for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// The socket has bytes to read, or its peer closed it.
    Read,
    /// The socket has room to write, or it failed.
    Write,
}

/// Readiness state of one registered socket.
#[derive(Debug, Default)]
pub struct PollDesc {
    read: AtomicU64,
    write: AtomicU64,
    /// Nanoseconds past [`epoch`] at which a parked reader is woken, or 0.
    read_deadline: AtomicU64,
    /// Nanoseconds past [`epoch`] at which a parked writer is woken, or 0.
    write_deadline: AtomicU64,
}

impl PollDesc {
    fn slots(&self, direction: Direction) -> (&AtomicU64, &AtomicU64) {
        match direction {
            Direction::Read => (&self.read, &self.read_deadline),
            Direction::Write => (&self.write, &self.write_deadline),
        }
    }
}

/// The kernel poll handle and its event buffer, held by whichever thread is
/// taking a pass.
struct Driver {
    poll: mio::Poll,
    events: mio::Events,
}

/// Registered sockets and sleep timers for one worker index, or for the
/// threads outside the scheduler.
pub(crate) struct Poller {
    driver: Mutex<Driver>,
    registry: mio::Registry,
    waker: mio::Waker,
    descs: Mutex<Slab>,
    timers: Mutex<BinaryHeap<Reverse<Timer>>>,
    /// Deadline, in nanoseconds past [`epoch`], a blocked pass sleeps until,
    /// or 0 while no pass is blocked.
    sleeping_until: AtomicU64,
    /// Waits currently carrying an I/O deadline; the sweep is skipped at 0.
    armed_deadlines: AtomicUsize,
    /// When the next deadline sweep is due, in nanoseconds past [`epoch`].
    next_sweep: AtomicU64,
    /// The worker index this poller belongs to, or `None` for the one the
    /// threads outside the scheduler share.
    index: Option<usize>,
}

/// One sleeping goroutine. `claimed` is taken by whichever of the poller (the
/// deadline came) and the sleeper (it woke for another reason, a cancellation)
/// gets there first, so a sleep that ended early never unparks its goroutine
/// later, in the middle of some other wait.
struct Timer {
    at: u64,
    gid: u32,
    claimed: Arc<AtomicBool>,
}

impl PartialEq for Timer {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.gid) == (other.at, other.gid)
    }
}

impl Eq for Timer {}

impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.at, self.gid).cmp(&(other.at, other.gid))
    }
}

#[derive(Default)]
struct Slab {
    entries: Vec<Option<Arc<PollDesc>>>,
    free: Vec<usize>,
}

impl Slab {
    fn insert(&mut self, desc: Arc<PollDesc>) -> usize {
        if let Some(index) = self.free.pop() {
            self.entries[index] = Some(desc);
            index
        } else {
            self.entries.push(Some(desc));
            self.entries.len() - 1
        }
    }

    fn remove(&mut self, index: usize) {
        if let Some(entry) = self.entries.get_mut(index)
            && entry.take().is_some()
        {
            self.free.push(index);
        }
    }
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn nanos_at(at: Instant) -> u64 {
    // Zero means "no deadline", so a deadline at the epoch itself reads as 1.
    u64::try_from(at.saturating_duration_since(epoch()).as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn now_nanos() -> u64 {
    nanos_at(Instant::now())
}

impl Poller {
    fn new(index: Option<usize>) -> io::Result<Self> {
        let poll = mio::Poll::new()?;
        let registry = poll.registry().try_clone()?;
        let waker = mio::Waker::new(poll.registry(), WAKER_TOKEN)?;
        let _ = epoch();
        Ok(Self {
            driver: Mutex::new(Driver {
                poll,
                events: mio::Events::with_capacity(256),
            }),
            registry,
            waker,
            descs: Mutex::new(Slab::default()),
            timers: Mutex::new(BinaryHeap::new()),
            sleeping_until: AtomicU64::new(0),
            armed_deadlines: AtomicUsize::new(0),
            next_sweep: AtomicU64::new(0),
            index,
        })
    }

    /// Ends a blocked pass early, or makes the next one return at once.
    pub(crate) fn wake(&self) {
        let _ = self.waker.wake();
    }

    /// Takes one pass: fires due timers and deadlines, then waits for socket
    /// readiness up to `max_wait` (`None` for as long as nothing is due),
    /// pushing the gids that became runnable onto `woken`.
    ///
    /// Answers `false`, having done nothing, while another thread holds the
    /// pass.
    pub(crate) fn turn(&self, max_wait: Option<Duration>, woken: &mut Vec<u32>) -> bool {
        let Some(mut driver) = self.driver.try_lock() else {
            return false;
        };
        let now = now_nanos();
        self.fire_due(now, woken);
        let timeout = if woken.is_empty() && max_wait != Some(Duration::ZERO) {
            let wake_at = self.next_due();
            self.sleeping_until.store(wake_at, Ordering::Release);
            // A timer added between reading the heap and publishing the store
            // above saw no sleeping pass and did not wake this one; look once
            // more now that it is published.
            let wake_at = wake_at.min(self.next_due());
            let until_due = (wake_at != u64::MAX)
                .then(|| Duration::from_nanos(wake_at.saturating_sub(now_nanos())));
            match (max_wait, until_due) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        } else {
            Some(Duration::ZERO)
        };
        let Driver { poll, events } = &mut *driver;
        // An interrupted or refused poll delivers nothing; the next pass
        // polls again.
        let polled = poll.poll(events, timeout).is_ok();
        self.sleeping_until.store(0, Ordering::Release);
        if polled {
            let descs = self.descs.lock();
            for event in events.iter() {
                if event.token() == WAKER_TOKEN {
                    continue;
                }
                let Some(Some(desc)) = descs.entries.get(event.token().0) else {
                    continue;
                };
                let failed = event.is_error();
                if failed || event.is_readable() || event.is_read_closed() {
                    notify(&desc.read, woken);
                }
                if failed || event.is_writable() || event.is_write_closed() {
                    notify(&desc.write, woken);
                }
            }
        }
        drop(driver);
        self.fire_due(now_nanos(), woken);
        true
    }

    /// The earliest moment a timer or a deadline sweep is due.
    fn next_due(&self) -> u64 {
        let next_timer = self
            .timers
            .lock()
            .peek()
            .map_or(u64::MAX, |Reverse(t)| t.at);
        if self.armed_deadlines.load(Ordering::Acquire) > 0 {
            next_timer.min(self.next_sweep.load(Ordering::Acquire))
        } else {
            next_timer
        }
    }

    fn fire_due(&self, now: u64, woken: &mut Vec<u32>) {
        {
            let mut timers = self.timers.lock();
            while timers.peek().is_some_and(|Reverse(t)| t.at <= now) {
                let Some(Reverse(timer)) = timers.pop() else {
                    break;
                };
                if !timer.claimed.swap(true, Ordering::AcqRel) {
                    woken.push(timer.gid);
                }
            }
        }
        if self.armed_deadlines.load(Ordering::Acquire) == 0
            || now < self.next_sweep.load(Ordering::Acquire)
        {
            return;
        }
        self.next_sweep
            .store(now + SWEEP.as_nanos() as u64, Ordering::Release);
        let descs = self.descs.lock();
        for desc in descs.entries.iter().flatten() {
            for (slot, deadline) in [
                (&desc.read, &desc.read_deadline),
                (&desc.write, &desc.write_deadline),
            ] {
                let at = deadline.load(Ordering::Acquire);
                if at == 0 || at > now {
                    continue;
                }
                let waiter = slot.load(Ordering::Acquire);
                if waiter >= WAITER
                    && slot
                        .compare_exchange(waiter, EMPTY, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    woken.push(u32::try_from(waiter - WAITER).unwrap_or(u32::MAX));
                }
            }
        }
    }

    fn arm_deadline(&self) {
        if self.armed_deadlines.fetch_add(1, Ordering::AcqRel) == 0 {
            self.next_sweep
                .store(now_nanos() + SWEEP.as_nanos() as u64, Ordering::Release);
        }
    }
}

fn notify(slot: &AtomicU64, woken: &mut Vec<u32>) {
    let previous = slot.swap(READY, Ordering::AcqRel);
    if previous >= WAITER {
        woken.push(u32::try_from(previous - WAITER).unwrap_or(u32::MAX));
    }
}

/// Unparks every gid in `woken`, leaving it empty.
pub(crate) fn unpark_all(woken: &mut Vec<u32>) {
    for gid in woken.drain(..) {
        crate::sched_global::scheduler().unpark(Gid(gid));
    }
}

/// A worker index's poller, created the first time it is needed, or why it
/// could not be.
type PollerSlot = OnceLock<Result<Arc<Poller>, String>>;

/// One slot per worker index the scheduler can reach, each filled the first
/// time a goroutine on that worker registers a socket or sleeps.
fn worker_pollers() -> &'static [PollerSlot] {
    static POLLERS: OnceLock<Box<[PollerSlot]>> = OnceLock::new();
    POLLERS.get_or_init(|| {
        (0..crate::sched::MultiScheduler::worker_count_cap())
            .map(|_| OnceLock::new())
            .collect()
    })
}

/// The poller of worker `index`, if one of its goroutines has needed it.
pub(crate) fn worker_poller(index: usize) -> Option<&'static Arc<Poller>> {
    worker_pollers().get(index)?.get()?.as_ref().ok()
}

/// Every worker index that has a poller, for the watchdog's pass over the
/// ones no worker occupies.
pub(crate) fn worker_poller_indices() -> impl Iterator<Item = (usize, &'static Arc<Poller>)> {
    worker_pollers()
        .iter()
        .enumerate()
        .filter_map(|(index, cell)| Some((index, cell.get()?.as_ref().ok()?)))
}

/// The poller shared by threads outside the scheduler, driven by a thread of
/// its own that it starts the first time it is needed.
fn thread_poller() -> io::Result<&'static Arc<Poller>> {
    static POLLER: OnceLock<Result<Arc<Poller>, String>> = OnceLock::new();
    let state = POLLER.get_or_init(|| {
        let poller = Arc::new(Poller::new(None).map_err(|e| e.to_string())?);
        let driven = Arc::clone(&poller);
        std::thread::Builder::new()
            .name("gos-netpoll".to_string())
            .spawn(move || {
                let mut woken = Vec::with_capacity(64);
                loop {
                    driven.turn(None, &mut woken);
                    unpark_all(&mut woken);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(poller)
    });
    state.as_ref().map_err(|e| io::Error::other(e.clone()))
}

/// The poller for the calling thread: its worker's, or the shared one.
fn current_poller() -> io::Result<&'static Arc<Poller>> {
    let Some(index) = crate::sched::multi::current_worker_index() else {
        return thread_poller();
    };
    let Some(cell) = worker_pollers().get(index) else {
        return thread_poller();
    };
    cell.get_or_init(|| {
        Poller::new(Some(index))
            .map(Arc::new)
            .map_err(|e| e.to_string())
    })
    .as_ref()
    .map_err(|e| io::Error::other(e.clone()))
}

/// A socket registered for readiness for as long as this lives.
pub struct Registration {
    desc: Arc<PollDesc>,
    token: usize,
    poller: &'static Arc<Poller>,
}

impl Registration {
    /// Registers `source` for readiness in both directions with the calling
    /// worker's poller.
    pub fn new(source: &mut impl mio::event::Source) -> io::Result<Self> {
        let poller = current_poller()?;
        let desc = Arc::new(PollDesc::default());
        let token = Self::register(poller, source, &desc)?;
        Ok(Self {
            desc,
            token,
            poller,
        })
    }

    fn register(
        poller: &Poller,
        source: &mut impl mio::event::Source,
        desc: &Arc<PollDesc>,
    ) -> io::Result<usize> {
        let token = poller.descs.lock().insert(Arc::clone(desc));
        if let Err(e) = poller.registry.register(
            source,
            mio::Token(token),
            mio::Interest::READABLE | mio::Interest::WRITABLE,
        ) {
            poller.descs.lock().remove(token);
            return Err(e);
        }
        Ok(token)
    }

    /// Moves `source` to the calling worker's poller when its goroutine now
    /// runs on another worker than the one that registered it, so its
    /// readiness is read where the goroutine waits for it.
    pub fn follow(&mut self, source: &mut impl mio::event::Source) {
        let Some(index) = crate::sched::multi::current_worker_index() else {
            return;
        };
        if self.poller.index == Some(index) {
            return;
        }
        let Ok(poller) = current_poller() else {
            return;
        };
        let _ = self.poller.registry.deregister(source);
        self.poller.descs.lock().remove(self.token);
        // The descriptor moves whole, so a readiness recorded before the move
        // is still there for the next wait; registering reports a socket that
        // is already ready again.
        if let Ok(token) = Self::register(poller, source, &self.desc) {
            self.token = token;
            self.poller = poller;
        }
    }

    /// Removes `source`, which must be the one this was made for.
    pub fn deregister(self, source: &mut impl mio::event::Source) {
        let _ = self.poller.registry.deregister(source);
        self.poller.descs.lock().remove(self.token);
    }

    /// Parks the calling goroutine until the socket is ready in `direction`
    /// or `deadline` passes. Answers `false` only when the deadline passed.
    ///
    /// A readiness that arrived since the last wait is taken at once. The
    /// caller retries its transfer after every return: an edge that lands
    /// between that retry and the next wait is kept in the waiter word, so
    /// none is lost.
    #[must_use]
    pub fn wait(&self, direction: Direction, deadline: Option<Instant>) -> bool {
        let (slot, deadline_slot) = self.desc.slots(direction);
        if slot
            .compare_exchange(READY, EMPTY, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return true;
        }
        if let Some(at) = deadline {
            if Instant::now() >= at {
                return false;
            }
            deadline_slot.store(nanos_at(at), Ordering::Release);
            self.poller.arm_deadline();
        }
        crate::sched_global::park(ParkReason::Io, |parker| {
            let me = u64::from(parker.gid.as_u32()) + WAITER;
            if slot
                .compare_exchange(EMPTY, me, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                // Readiness arrived since the check above: stay runnable.
                slot.store(EMPTY, Ordering::Release);
                crate::sched_global::scheduler().unpark(parker.gid);
            }
        });
        slot.store(EMPTY, Ordering::Release);
        if deadline.is_some() {
            deadline_slot.store(0, Ordering::Release);
            self.poller.armed_deadlines.fetch_sub(1, Ordering::AcqRel);
        }
        deadline.is_none_or(|at| Instant::now() < at)
    }
}

/// Parks the calling goroutine until `deadline`, or until something else
/// unparks it first (a cancellation).
///
/// Answers `false`, having done nothing, when no poller can run.
#[must_use]
pub fn sleep_until(deadline: Instant) -> bool {
    let Ok(poller) = current_poller() else {
        return false;
    };
    let at = nanos_at(deadline);
    let claimed = Arc::new(AtomicBool::new(false));
    crate::sched_global::park(ParkReason::Timer, |parker| {
        poller.timers.lock().push(Reverse(Timer {
            at,
            gid: parker.gid.as_u32(),
            claimed: Arc::clone(&claimed),
        }));
        // A pass blocked past this deadline is ended early; one not blocked
        // reads the heap before it next blocks.
        let sleeping_until = poller.sleeping_until.load(Ordering::Acquire);
        if sleeping_until != 0 && at < sleeping_until {
            poller.wake();
        }
    });
    // Woken early, the entry still in the heap must not wake this goroutine
    // later; woken by the deadline, this finds it already taken.
    claimed.store(true, Ordering::Release);
    true
}
