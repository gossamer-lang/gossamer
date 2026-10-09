#![allow(clippy::missing_safety_doc)]

//! Runtime support for `std::sync::Shared` - a value one goroutine
//! publishes and any number reach under a lock.
//!
//! The handle is an opaque heap `Box<GosShared>`; compiled tiers carry
//! the pointer as an `i64`, and the MIR receiver-kind dispatch tags a
//! constructor result `sync::Shared` so method calls route here. The
//! handle outlives every goroutine that captured it and is never freed,
//! matching the other long-lived synchronisation handles (`sync::Map`,
//! `sync::Once`, `sync::RwLock`).
//!
//! The guarded payload is one word: a scalar, or the handle a `String`,
//! `Vec`, `Map`, or `Set` is carried by. Every read and every write goes
//! through the same `parking_lot::Mutex`, so two goroutines never observe
//! a half-written update, and `update` holds the lock across the callback
//! so a read-modify-write cannot interleave with another one.
//!
//! `with` / `update` cross the C-ABI through the callable convention the
//! `iter::*` and `RwLock` combinators use: `env` is a heap blob whose
//! first word is the callable address, invoked as `f(env, value)`.

use std::sync::atomic::{AtomicI64, Ordering};

/// `fn(env, value) -> result` - the one-argument value-thunk shape
/// shared with the `MapFn` callbacks in `combinator.rs`.
type GuardFn = unsafe extern "C-unwind" fn(env: *const u8, value: i64) -> i64;

/// Callable address stored at `env[0]`, or `None` for a null/zero env.
///
/// # Safety
///
/// `env` is null or a live closure environment, whose first word is the
/// closure's entry address.
unsafe fn env_fn_addr(env: *const u8) -> Option<*const ()> {
    if env.is_null() {
        return None;
    }
    // SAFETY: `env` is a live closure blob whose first word is the
    // callable address (codegen invariant shared with the combinator
    // and `iter::*` families).
    let addr = unsafe { (env.cast::<usize>()).read() };
    if addr == 0 {
        None
    } else {
        // Recover the address's exposed provenance so the pointer is
        // sound to call under strict provenance.
        Some(std::ptr::with_exposed_provenance::<()>(addr))
    }
}

/// Opaque heap handle wrapping the guarded payload word.
pub struct GosShared {
    inner: parking_lot::Mutex<i64>,
    /// Goroutine that most recently published an update, so the race
    /// detector records a happens-before edge into the next reader.
    last_release_gid: AtomicI64,
}

super::rc::managed_handle!(GosShared);

impl GosShared {
    fn record_acquire(&self) {
        let from = self.last_release_gid.load(Ordering::Acquire);
        if from >= 0 {
            crate::race::record_sync(u32::try_from(from).unwrap_or(0), crate::race::current_gid());
        }
    }

    fn record_release(&self) {
        self.last_release_gid
            .store(i64::from(crate::race::current_gid()), Ordering::Release);
    }
}

/// Allocate a `sync::Shared` holding `value`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_shared_new(value: i64) -> *mut GosShared {
    ffi_entry!({
        super::rc::alloc_managed(GosShared {
            inner: parking_lot::Mutex::new(value),
            last_release_gid: AtomicI64::new(-1),
        })
    })
}

/// `shared.get()` - the guarded value, read under the lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_shared_get(shared: *mut GosShared) -> i64 {
    ffi_entry!({
        if shared.is_null() {
            return 0;
        }
        // SAFETY: `shared` is a handle from compiled code, checked non-null above and live for the whole call.
        let shared = unsafe { &*shared };
        let value = *shared.inner.lock();
        shared.record_acquire();
        value
    })
}

/// `shared.set(value)` - replace the guarded value under the lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_shared_set(shared: *mut GosShared, value: i64) {
    ffi_entry!({
        if shared.is_null() {
            return;
        }
        // SAFETY: `shared` is a handle from compiled code, checked non-null above and live for the whole call.
        let shared = unsafe { &*shared };
        *shared.inner.lock() = value;
        shared.record_release();
    });
}

/// `shared.with(f)` - run `f(value)` under the lock and answer its
/// result. The guarded value is unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_shared_with(shared: *mut GosShared, env: *const u8) -> i64 {
    ffi_entry_passthrough!({
        if shared.is_null() {
            return 0;
        }
        // SAFETY: `shared` is a handle from compiled code, checked non-null above and live for the whole call.
        let shared = unsafe { &*shared };
        // The guard is held across the callback: a reader must see one
        // whole value, not a state another goroutine is midway through.
        let guard = shared.inner.lock();
        shared.record_acquire();
        let value = *guard;
        // SAFETY: `env` is this shim's argument, as `env_fn_addr` requires (C-ABI contract).
        match unsafe { env_fn_addr(env) } {
            Some(addr) => {
                // SAFETY: `addr` is the callable the closure lowering stored, and a one-argument
                // closure lowers to the `fn(env, i64) -> i64` value-thunk shape.
                let f: GuardFn = unsafe { std::mem::transmute(addr) };
                // SAFETY: `f` is that callable, whose environment `env` is live for the call (C-ABI
                // contract).
                unsafe { f(env, value) }
            }
            None => value,
        }
    })
}

/// `shared.update(f)` - run `f(value)` under the lock, store what it
/// answers, and return that. The lock spans the read and the write, so
/// two goroutines updating at once cannot lose one another's work.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_shared_update(
    shared: *mut GosShared,
    env: *const u8,
) -> i64 {
    ffi_entry_passthrough!({
        if shared.is_null() {
            return 0;
        }
        // SAFETY: `shared` is a handle from compiled code, checked non-null above and live for the whole call.
        let shared = unsafe { &*shared };
        let mut guard = shared.inner.lock();
        shared.record_acquire();
        let current = *guard;
        // SAFETY: `env` is this shim's argument, as `env_fn_addr` requires (C-ABI contract).
        let next = match unsafe { env_fn_addr(env) } {
            Some(addr) => {
                // SAFETY: `addr` is the callable the closure lowering stored, and a one-argument
                // closure lowers to the `fn(env, i64) -> i64` value-thunk shape.
                let f: GuardFn = unsafe { std::mem::transmute(addr) };
                // SAFETY: `f` is that callable, whose environment `env` is live for the call (C-ABI
                // contract).
                unsafe { f(env, current) }
            }
            None => current,
        };
        *guard = next;
        drop(guard);
        shared.record_release();
        next
    })
}
