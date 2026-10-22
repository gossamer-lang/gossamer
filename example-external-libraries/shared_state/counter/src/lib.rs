//! Process-global state two binding crates depend on.

use std::sync::atomic::{AtomicI64, Ordering};

static COUNT: AtomicI64 = AtomicI64::new(0);

/// Adds one to the count and answers the new count.
pub fn bump() -> i64 {
    COUNT.fetch_add(1, Ordering::Relaxed) + 1
}

/// The current count.
pub fn count() -> i64 {
    COUNT.load(Ordering::Relaxed)
}
