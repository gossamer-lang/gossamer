use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use gossamer_runtime::platform::Instant;
use parking_lot::{Condvar, Mutex};

/// Deadlines for one kind of VM handle, fired by one thread in deadline order.
///
/// Insertion and removal are logarithmic in the number pending, and a handle
/// retired before its deadline takes its entry with it, so what is pending
/// tracks the live handles rather than every deadline ever set.
pub(crate) struct DeadlineQueue {
    thread_name: &'static str,
    fire: fn(i64),
    entries: Mutex<BTreeSet<(Instant, i64)>>,
    wake: Condvar,
    started: OnceLock<Result<(), String>>,
}

impl DeadlineQueue {
    /// A queue whose thread, once started, calls `fire(id)` as each deadline
    /// arrives.
    pub(crate) const fn new(thread_name: &'static str, fire: fn(i64)) -> Self {
        Self {
            thread_name,
            fire,
            entries: Mutex::new(BTreeSet::new()),
            wake: Condvar::new(),
            started: OnceLock::new(),
        }
    }

    /// Starts the queue's thread on first use, answering why it could not be.
    pub(crate) fn start(&'static self) -> Result<(), String> {
        self.started
            .get_or_init(|| {
                // A daemon thread: nothing joins it, and it exits with the
                // process, the same lifecycle the goroutine workers have.
                std::thread::Builder::new()
                    .name(self.thread_name.to_string())
                    .spawn(move || self.run())
                    .map(drop)
                    .map_err(|e| e.to_string())
            })
            .clone()
    }

    /// Queues `id` to fire at `deadline` unless `retired` is already set.
    ///
    /// Whoever retires the handle sets `retired` before calling
    /// [`Self::remove`], so a retirement racing this insert is either seen
    /// here or removes what this inserts.
    pub(crate) fn insert(&self, deadline: Instant, id: i64, retired: &AtomicBool) {
        let nearest = {
            let mut entries = self.entries.lock();
            if retired.load(Ordering::Acquire) {
                return;
            }
            entries.insert((deadline, id));
            entries.first() == Some(&(deadline, id))
        };
        if nearest {
            self.wake.notify_all();
        }
    }

    /// Drops `id`'s pending deadline, if it has one.
    pub(crate) fn remove(&self, deadline: Instant, id: i64) {
        self.entries.lock().remove(&(deadline, id));
    }

    /// Whether any deadline is still waiting to fire.
    pub(crate) fn pending(&self) -> bool {
        !self.entries.lock().is_empty()
    }

    fn run(&self) {
        loop {
            let mut entries = self.entries.lock();
            let Some(&(earliest, id)) = entries.first() else {
                self.wake.wait(&mut entries);
                continue;
            };
            let now = Instant::now();
            if earliest > now {
                self.wake.wait_for(&mut entries, earliest - now);
                continue;
            }
            entries.pop_first();
            drop(entries);
            (self.fire)(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    static FIRED: Mutex<Vec<i64>> = Mutex::new(Vec::new());
    static FIRED_CHANGED: Condvar = Condvar::new();
    static QUEUE: DeadlineQueue = DeadlineQueue::new("deadline-queue-test", |id| {
        FIRED.lock().push(id);
        FIRED_CHANGED.notify_all();
    });

    #[test]
    fn removed_deadline_never_fires_and_retired_insert_is_skipped() {
        QUEUE.start().expect("start the deadline thread");
        let live = AtomicBool::new(false);
        let far = Instant::now() + Duration::from_hours(1);
        for id in 0..10_000 {
            QUEUE.insert(far, id, &live);
            QUEUE.remove(far, id);
        }
        assert!(!QUEUE.pending(), "removed deadlines must not stay queued");

        let retired = AtomicBool::new(true);
        QUEUE.insert(far, 1, &retired);
        assert!(
            !QUEUE.pending(),
            "a retired handle must not queue a deadline"
        );

        QUEUE.insert(Instant::now(), 42, &live);
        let mut fired = FIRED.lock();
        while fired.is_empty() {
            FIRED_CHANGED.wait(&mut fired);
        }
        assert_eq!(*fired, vec![42]);
        drop(fired);
        assert!(!QUEUE.pending());
    }
}
