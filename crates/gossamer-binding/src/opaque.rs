//! Opaque handles for binding-owned Rust values.
//!
//! Bindings that want to expose a Rust struct (e.g.
//! `tuigoose::Terminal`) to Gossamer code can't pass it directly -
//! the interpreter's `Value` type does not have a `dyn Any`
//! variant. Instead, the binding stores the value in a
//! [`Registry<T>`] and gives Gossamer code an `i64` handle that
//! round-trips through the `Value::Int` variant.
//!
//! Each binding owns its own [`Registry`]. The registry is
//! `Send + Sync` and uses interior mutability so a binding fn
//! that takes `&[Value]` can still register a fresh value.
//!
//! This mirrors the pattern `gossamer-interp::builtins` already
//! uses for `I64Vec` / `WaitGroup` handles.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use crate::value::{RuntimeError, RuntimeResult};

/// Per-type registry mapping `i64` handles to owned Rust values.
#[derive(Debug)]
pub struct Registry<T: Send + Sync + 'static> {
    next: AtomicU64,
    entries: Mutex<FxHashMap<u64, Arc<T>>>,
}

impl<T: Send + Sync + 'static> Default for Registry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + Sync + 'static> Registry<T> {
    /// Builds an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            entries: Mutex::new(FxHashMap::with_hasher(rustc_hash::FxBuildHasher)),
        }
    }

    /// Inserts `value` and returns its handle.
    pub fn insert(&self, value: T) -> i64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.entries.lock().insert(id, Arc::new(value));
        i64::try_from(id).unwrap_or(i64::MAX)
    }

    /// Looks up the `Arc<T>` for `handle`, or returns a typed error.
    pub fn get(&self, handle: i64) -> RuntimeResult<Arc<T>> {
        let id = u64::try_from(handle)
            .map_err(|_| RuntimeError::Type(format!("invalid handle {handle}")))?;
        self.entries
            .lock()
            .get(&id)
            .cloned()
            .ok_or_else(|| RuntimeError::Type(format!("unknown handle {handle}")))
    }

    /// Removes `handle` and returns the inner value if it had a
    /// single owner; otherwise drops the registry's reference and
    /// returns `None`.
    pub fn remove(&self, handle: i64) -> Option<Arc<T>> {
        let id = u64::try_from(handle).ok()?;
        self.entries.lock().remove(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_round_trip() {
        let reg: Registry<String> = Registry::new();
        let h1 = reg.insert("alpha".to_string());
        let h2 = reg.insert("beta".to_string());
        assert!(h1 < h2);

        let a = reg.get(h1).unwrap();
        assert_eq!(*a, "alpha");
        let b = reg.get(h2).unwrap();
        assert_eq!(*b, "beta");
    }

    #[test]
    fn unknown_handle_returns_error() {
        let reg: Registry<String> = Registry::new();
        let err = reg.get(9999).unwrap_err();
        assert!(matches!(err, RuntimeError::Type(_)));
    }

    #[test]
    fn remove_drops_entry() {
        let reg: Registry<i64> = Registry::new();
        let h = reg.insert(42);
        assert!(reg.remove(h).is_some());
        assert!(reg.get(h).is_err());
    }
}
