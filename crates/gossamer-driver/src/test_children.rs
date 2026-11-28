//! Keeps this crate's tests that spawn child processes apart from the ones
//! that release a lock file and expect it free.
//!
//! A child holds a copy of every descriptor open in the process from its
//! spawn until it execs, and with it the lock on each lock file a test had
//! open. A test that drops its lock and takes it straight back can find it
//! still held by a sibling test's child.

#[cfg(unix)]
use parking_lot::RwLockWriteGuard;
use parking_lot::{RwLock, RwLockReadGuard};

static CHILDREN: RwLock<()> = RwLock::new(());

/// Held while a test spawns a child process; only the Unix tests spawn one.
#[cfg(unix)]
pub(crate) fn spawning() -> RwLockWriteGuard<'static, ()> {
    CHILDREN.write()
}

/// Held while a test releases a lock file and expects no one to hold it.
pub(crate) fn releasing_locks() -> RwLockReadGuard<'static, ()> {
    CHILDREN.read()
}
