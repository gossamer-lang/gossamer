//! Wake-ups for threads waiting on more than one source at once.
//!
//! A goroutine parks by id and anything can unpark it, but an OS thread
//! waits on one condition variable, and a wait that must end on either of
//! two sources (a channel or a context, a wait group or a context, any arm
//! of a `select`) cannot sit on both. Each source keeps a [`WakerSet`]; the
//! waiting thread registers one [`ThreadSignal`] waker with every source it
//! depends on and sleeps on the signal, so whichever source changes first
//! wakes it directly.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Wake, Waker};

/// Wakers to run when the owning object changes.
#[derive(Debug, Default)]
pub struct WakerSet {
    wakers: parking_lot::Mutex<Vec<Waker>>,
    /// How many wakers are registered, read without the lock so an object
    /// nobody watches pays one atomic load per change.
    len: AtomicUsize,
}

impl WakerSet {
    /// An empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            wakers: parking_lot::Mutex::new(Vec::new()),
            len: AtomicUsize::new(0),
        }
    }

    /// Adds `waker`, which [`WakerSet::wake_all`] runs until it is removed.
    pub fn register(&self, waker: &Waker) {
        let mut wakers = self.wakers.lock();
        wakers.push(waker.clone());
        self.len.store(wakers.len(), Ordering::Release);
    }

    /// Removes one registration of `waker`.
    pub fn deregister(&self, waker: &Waker) {
        let mut wakers = self.wakers.lock();
        if let Some(at) = wakers.iter().position(|w| w.will_wake(waker)) {
            wakers.swap_remove(at);
        }
        self.len.store(wakers.len(), Ordering::Release);
    }

    /// Runs every registered waker. Registrations stay in place: a waiter
    /// re-checks its condition and removes its own waker when it leaves.
    pub fn wake_all(&self) {
        if self.len.load(Ordering::Acquire) == 0 {
            return;
        }
        let wakers = self.wakers.lock().clone();
        for waker in wakers {
            waker.wake();
        }
    }
}

/// A flag one OS thread sleeps on until any of its sources raises it.
#[derive(Debug, Default)]
pub struct ThreadSignal {
    raised: parking_lot::Mutex<bool>,
    cv: parking_lot::Condvar,
}

impl ThreadSignal {
    /// A lowered signal.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The waker that raises this signal.
    #[must_use]
    pub fn waker(self: &Arc<Self>) -> Waker {
        Waker::from(Arc::clone(self))
    }

    /// Sleeps until the signal is raised, then lowers it. A raise that came
    /// before the call ends the wait at once, so a source that changed
    /// between the caller's check and this sleep is never missed.
    pub fn wait(&self) {
        let mut raised = self.raised.lock();
        while !*raised {
            self.cv.wait(&mut raised);
        }
        *raised = false;
    }

    fn raise(&self) {
        *self.raised.lock() = true;
        self.cv.notify_one();
    }
}

impl Wake for ThreadSignal {
    fn wake(self: Arc<Self>) {
        self.raise();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.raise();
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn a_raise_before_the_wait_ends_it_at_once() {
        let signal = ThreadSignal::new();
        let set = WakerSet::new();
        set.register(&signal.waker());
        set.wake_all();
        signal.wait();
    }

    #[test]
    fn a_raise_from_another_thread_wakes_the_sleeper() {
        let signal = ThreadSignal::new();
        let set = Arc::new(WakerSet::new());
        set.register(&signal.waker());
        let raiser = {
            let set = Arc::clone(&set);
            std::thread::spawn(move || set.wake_all())
        };
        signal.wait();
        raiser.join().expect("raiser thread");
    }

    #[test]
    fn a_removed_waker_is_not_run() {
        let signal = ThreadSignal::new();
        let set = WakerSet::new();
        let waker = signal.waker();
        set.register(&waker);
        set.deregister(&waker);
        set.wake_all();
        assert!(!*signal.raised.lock());
    }
}
