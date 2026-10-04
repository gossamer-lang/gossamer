//! Goroutine task primitive used by the scheduler.

#![forbid(unsafe_code)]

/// Opaque identifier for a goroutine inside a [`crate::sched::Scheduler`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Gid(pub u32);

impl Gid {
    /// Returns the raw numeric index of this goroutine.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Result of advancing a [`Task`] by one unit of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The task yielded at a safepoint; it will be re-enqueued.
    Yield,
    /// The task finished. Its value is observable through the
    /// completion handle when the scheduler stores one.
    Done,
}

/// Cooperative task driven by [`crate::sched::Scheduler::run`]. Each call to
/// `step` advances the task's internal state machine by one quantum.
pub trait Task {
    /// Advances the task. Returning [`Step::Yield`] cedes control back
    /// to the scheduler; returning [`Step::Done`] removes the task
    /// from the run queue.
    fn step(&mut self) -> Step;

    /// Whether the task has run. One that has not holds no state bound to a
    /// worker's thread, so it may start on any worker.
    fn started(&self) -> bool {
        true
    }

    /// Gives up the worker placement the task was spawned with, before it
    /// moves to another worker.
    fn release_placement(&mut self) {}
}

impl<F> Task for F
where
    F: FnMut() -> Step,
{
    fn step(&mut self) -> Step {
        self()
    }
}
