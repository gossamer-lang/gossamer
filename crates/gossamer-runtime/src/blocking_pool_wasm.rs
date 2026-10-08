//! The browser build's blocking pool: there is none. A wasm build has no
//! threads, so a blocking operation runs inline where it is called, and the
//! pool's load always reads as empty.

/// A snapshot of the pool's load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockingStats {
    /// Threads alive, busy or idle.
    pub threads: usize,
    /// Jobs waiting for a thread.
    pub queued: usize,
    /// Submitters waiting for room in the queue.
    pub admission_waiters: usize,
    /// How long the oldest waiting job has waited, in milliseconds.
    pub oldest_queued_ms: u64,
}

/// The pool's load now: empty.
#[must_use]
pub fn stats() -> BlockingStats {
    BlockingStats::default()
}
