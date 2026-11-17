//! Clone, free, and share-marking of the container kinds a Vec slot, a struct
//! field, or a map value can hold, for the walks that handle every kind.
//!
//! Each kind registers its operations when its first value is made (in the
//! one constructor every value of the kind goes through), so a program that
//! never makes a map links no map code through those walks.

/// The operations of one container kind, on its handle.
#[derive(Clone, Copy)]
pub(crate) struct KindOps {
    /// An independent copy of the value behind the handle.
    pub(crate) clone: unsafe fn(*mut u8) -> *mut u8,
    /// Releases the handle the caller owns.
    pub(crate) free: unsafe fn(*mut u8),
    /// Marks the value as reachable from more than one thread; `None` for a
    /// kind with no share state of its own.
    pub(crate) mark_shared: Option<unsafe fn(*mut u8)>,
}

/// The container kinds with registered operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Map,
    Set,
    Deque,
    Json,
}

static OPS: [std::sync::OnceLock<KindOps>; 4] = [const { std::sync::OnceLock::new() }; 4];

/// Records `ops` as the operations of `kind`. The kind's constructor calls
/// this for every value it makes, before handing the value out.
#[inline]
pub(crate) fn register(kind: Kind, ops: KindOps) {
    OPS[kind as usize].get_or_init(|| ops);
}

/// The operations of `kind`, or `None` when no value of it has been made, so
/// none can be in hand.
#[inline]
fn ops(kind: Kind) -> Option<KindOps> {
    OPS[kind as usize].get().copied()
}

/// An independent copy of the `kind` value behind `handle`.
///
/// # Safety
///
/// `handle` is a live handle of `kind`, or null.
pub(crate) unsafe fn clone(kind: Kind, handle: *mut u8) -> *mut u8 {
    match ops(kind) {
        // SAFETY: `handle` is a live handle of `kind` (contract).
        Some(ops) => unsafe { (ops.clone)(handle) },
        None => handle,
    }
}

/// Releases the `kind` handle the caller owns.
///
/// # Safety
///
/// `handle` is a live handle of `kind` the caller owns, or null.
pub(crate) unsafe fn free(kind: Kind, handle: *mut u8) {
    if let Some(ops) = ops(kind) {
        // SAFETY: `handle` is a live handle of `kind` the caller owns
        // (contract).
        unsafe { (ops.free)(handle) };
    }
}

/// Marks the `kind` value behind `handle` as reachable from more than one
/// thread.
///
/// # Safety
///
/// `handle` is a live handle of `kind`, or null.
pub(crate) unsafe fn mark_shared(kind: Kind, handle: *mut u8) {
    if let Some(mark) = ops(kind).and_then(|ops| ops.mark_shared) {
        // SAFETY: `handle` is a live handle of `kind` (contract).
        unsafe { mark(handle) };
    }
}
