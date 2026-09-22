//! Socket readiness and sleep timers for goroutines, delivered without a lock
//! shared with the goroutines that wait.
//!
//! A socket is registered once, edge-triggered, for both directions, and owns a
//! [`PollDesc`] holding one waiter word per direction. A goroutine whose read or
//! write would block parks with its gid in that word; the poll thread swaps the
//! word to `READY` when the kernel reports readiness and unparks whoever was
//! there. Waiting and waking touch that word and the scheduler and nothing else,
//! so connections do not queue behind one another on a registry lock.
//!
//! The same thread keeps a heap of sleep deadlines and sleeps in the kernel
//! until the earliest, and sweeps I/O deadlines on a short fixed cadence: an
//! I/O deadline bounds a stalled peer, where seconds matter and a few
//! milliseconds of slack do not.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

/// Token reserved for the poll thread's own waker.
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

struct NetPoll {
    registry: mio::Registry,
    waker: mio::Waker,
    descs: Mutex<Slab>,
    timers: Mutex<BinaryHeap<Reverse<Timer>>>,
    /// Deadline, in nanoseconds past [`epoch`], the poll thread sleeps until.
    sleeping_until: AtomicU64,
    /// Waits currently carrying an I/O deadline; the sweep is skipped at 0.
    armed_deadlines: AtomicUsize,
}

/// One sleeping goroutine. `claimed` is taken by whichever of the poll thread
/// (the deadline came) and the sleeper (it woke for another reason, a
/// cancellation) gets there first, so a sleep that ended early never unparks
/// its goroutine later, in the middle of some other wait.
struct Timer {
    at: u64,
    gid: u32,
    claimed: Arc<std::sync::atomic::AtomicBool>,
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

fn netpoll() -> io::Result<&'static NetPoll> {
    static NETPOLL: OnceLock<Result<NetPoll, String>> = OnceLock::new();
    let state = NETPOLL.get_or_init(|| {
        let start = || -> io::Result<(NetPoll, mio::Poll)> {
            let poll = mio::Poll::new()?;
            let registry = poll.registry().try_clone()?;
            let waker = mio::Waker::new(poll.registry(), WAKER_TOKEN)?;
            Ok((
                NetPoll {
                    registry,
                    waker,
                    descs: Mutex::new(Slab::default()),
                    timers: Mutex::new(BinaryHeap::new()),
                    sleeping_until: AtomicU64::new(u64::MAX),
                    armed_deadlines: AtomicUsize::new(0),
                },
                poll,
            ))
        };
        match start() {
            Ok((netpoll, poll)) => {
                let _ = epoch();
                std::thread::Builder::new()
                    .name("gos-netpoll".to_string())
                    .spawn(move || poll_loop(poll))
                    .map_err(|e| e.to_string())?;
                Ok(netpoll)
            }
            Err(e) => Err(e.to_string()),
        }
    });
    state.as_ref().map_err(|e| io::Error::other(e.clone()))
}

fn poll_loop(mut poll: mio::Poll) {
    let mut events = mio::Events::with_capacity(1024);
    let mut woken: Vec<u32> = Vec::with_capacity(64);
    let mut next_sweep = now_nanos() + SWEEP.as_nanos() as u64;
    // Blocks until the thread that started this loop has published the
    // state it is reached through.
    let Ok(np) = netpoll() else {
        return;
    };
    loop {
        let now = now_nanos();
        fire_timers(np, now, &mut woken);
        if now >= next_sweep {
            if np.armed_deadlines.load(Ordering::Acquire) > 0 {
                sweep_deadlines(np, now, &mut woken);
            }
            next_sweep = now + SWEEP.as_nanos() as u64;
        }
        for gid in woken.drain(..) {
            crate::sched_global::scheduler().unpark(Gid(gid));
        }
        let next_timer = np.timers.lock().peek().map_or(u64::MAX, |Reverse(t)| t.at);
        let wake_at = next_timer.min(if np.armed_deadlines.load(Ordering::Acquire) > 0 {
            next_sweep
        } else {
            u64::MAX
        });
        np.sleeping_until.store(wake_at, Ordering::Release);
        // A timer added between the peek and the store above saw the old,
        // later deadline and did not wake the thread; look once more.
        let next_timer = np.timers.lock().peek().map_or(u64::MAX, |Reverse(t)| t.at);
        let wake_at = wake_at.min(next_timer);
        let timeout = (wake_at != u64::MAX)
            .then(|| Duration::from_nanos(wake_at.saturating_sub(now_nanos())));
        if poll.poll(&mut events, timeout).is_err() {
            // Interrupted by a signal, or a transient kernel refusal: the
            // next pass polls again.
            continue;
        }
        np.sleeping_until.store(0, Ordering::Release);
        let descs = np.descs.lock();
        for event in &events {
            if event.token() == WAKER_TOKEN {
                continue;
            }
            let Some(Some(desc)) = descs.entries.get(event.token().0) else {
                continue;
            };
            let failed = event.is_error();
            if failed || event.is_readable() || event.is_read_closed() {
                notify(&desc.read, &mut woken);
            }
            if failed || event.is_writable() || event.is_write_closed() {
                notify(&desc.write, &mut woken);
            }
        }
        drop(descs);
        for gid in woken.drain(..) {
            crate::sched_global::scheduler().unpark(Gid(gid));
        }
    }
}

fn notify(slot: &AtomicU64, woken: &mut Vec<u32>) {
    let previous = slot.swap(READY, Ordering::AcqRel);
    if previous >= WAITER {
        woken.push(u32::try_from(previous - WAITER).unwrap_or(u32::MAX));
    }
}

fn fire_timers(np: &NetPoll, now: u64, woken: &mut Vec<u32>) {
    let mut timers = np.timers.lock();
    while timers.peek().is_some_and(|Reverse(t)| t.at <= now) {
        let Some(Reverse(timer)) = timers.pop() else {
            break;
        };
        if !timer.claimed.swap(true, Ordering::AcqRel) {
            woken.push(timer.gid);
        }
    }
}

fn sweep_deadlines(np: &NetPoll, now: u64, woken: &mut Vec<u32>) {
    let descs = np.descs.lock();
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

/// A socket registered with the poll thread for as long as this lives.
pub struct Registration {
    desc: Arc<PollDesc>,
    token: usize,
}

impl Registration {
    /// Registers `source` for readiness in both directions.
    pub fn new(source: &mut impl mio::event::Source) -> io::Result<Self> {
        let np = netpoll()?;
        let desc = Arc::new(PollDesc::default());
        let token = np.descs.lock().insert(Arc::clone(&desc));
        if let Err(e) = np.registry.register(
            source,
            mio::Token(token),
            mio::Interest::READABLE | mio::Interest::WRITABLE,
        ) {
            np.descs.lock().remove(token);
            return Err(e);
        }
        Ok(Self { desc, token })
    }

    /// Removes `source`, which must be the one this was made for.
    pub fn deregister(self, source: &mut impl mio::event::Source) {
        if let Ok(np) = netpoll() {
            let _ = np.registry.deregister(source);
            np.descs.lock().remove(self.token);
        }
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
            if let Ok(np) = netpoll() {
                np.armed_deadlines.fetch_add(1, Ordering::AcqRel);
            }
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
            if let Ok(np) = netpoll() {
                np.armed_deadlines.fetch_sub(1, Ordering::AcqRel);
            }
        }
        deadline.is_none_or(|at| Instant::now() < at)
    }
}

/// Parks the calling goroutine until `deadline`, or until something else
/// unparks it first (a cancellation).
///
/// Answers `false`, having done nothing, when the poll thread cannot run.
#[must_use]
pub fn sleep_until(deadline: Instant) -> bool {
    let Ok(np) = netpoll() else {
        return false;
    };
    let at = nanos_at(deadline);
    let claimed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    crate::sched_global::park(ParkReason::Timer, |parker| {
        np.timers.lock().push(Reverse(Timer {
            at,
            gid: parker.gid.as_u32(),
            claimed: Arc::clone(&claimed),
        }));
        // The thread is woken only when this deadline comes before the one it
        // is already sleeping until.
        if at < np.sleeping_until.load(Ordering::Acquire) {
            let _ = np.waker.wake();
        }
    });
    // Woken early, the entry still in the heap must not wake this goroutine
    // later; woken by the deadline, this finds it already taken.
    claimed.store(true, Ordering::Release);
    true
}
