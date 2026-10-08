//! Handles that own an entry of a runtime registry.
//!
//! An open file or socket lives in a registry keyed by id, which every call
//! on it looks up. The handle a program holds is a counted node owning that
//! id, so the entry - and the descriptor it holds - is removed when the last
//! copy of the handle is gone, as well as by an explicit `close`.

/// One registry entry, removed with the last share of the handle naming it.
pub struct RegistryKey {
    id: i64,
    retire: fn(i64),
}

super::rc::managed_handle!(RegistryKey);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        (self.retire)(self.id);
    }
}

/// A handle owning registry entry `id`, which `retire` removes when the
/// handle's last share is released. Answers `0`, having retired the entry,
/// when the handle cannot be allocated.
#[must_use]
pub fn key_handle(id: i64, retire: fn(i64)) -> i64 {
    let node = super::rc::alloc_managed(RegistryKey { id, retire });
    if node.is_null() {
        retire(id);
        return 0;
    }
    node.expose_provenance() as i64
}

/// The registry id the handle `h` owns; `0`, which names no entry, for a
/// null handle.
#[must_use]
pub fn key_id(h: i64) -> i64 {
    if h == 0 {
        return 0;
    }
    let node = std::ptr::with_exposed_provenance::<RegistryKey>(h as usize);
    // SAFETY: a non-null handle from compiled code is a `RegistryKey` node the
    // caller holds a share of, live for this read.
    unsafe { (*node).id }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    static RETIRED: AtomicI64 = AtomicI64::new(0);

    fn retire(id: i64) {
        RETIRED.store(id, Ordering::SeqCst);
    }

    #[test]
    fn the_entry_is_retired_with_the_last_share() {
        let h = key_handle(41, retire);
        assert_eq!(key_id(h), 41);
        let node = std::ptr::with_exposed_provenance_mut::<u8>(h as usize);
        // SAFETY: `node` is the live handle made above.
        unsafe { crate::c_abi::rc::gos_rt_rc_retain(node) };
        // SAFETY: each release gives back a share taken above.
        unsafe { crate::c_abi::rc::gos_rt_rc_release(node) };
        assert_eq!(RETIRED.load(Ordering::SeqCst), 0, "a share is still held");
        // SAFETY: the last share, taken by `key_handle`.
        unsafe { crate::c_abi::rc::gos_rt_rc_release(node) };
        assert_eq!(RETIRED.load(Ordering::SeqCst), 41);
    }
}
