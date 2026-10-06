#![allow(clippy::missing_safety_doc)]

//! Runtime support for `std::sync::RwLock` - a reader-writer lock
//! guarding a single `i64` value. The handle is an opaque heap
//! `Box<GosRwLock>`; compiled tiers carry the pointer as an `i64` and
//! the MIR receiver-kind dispatch tags constructor results
//! `sync::RwLock` so method calls route to the helpers below. The
//! handle is never freed (it leaks at process exit), matching the
//! other long-lived synchronisation handles (`sync::Map`,
//! `sync::Once`).
//!
//! `with_read` / `with_write` cross the C-ABI through the shared
//! callable convention used by the `iter::*` / `option::*` / `Once`
//! combinators: `env` is a heap blob whose first word is the callable
//! address; the body is invoked as `f(env, value)`. `with_read`
//! passes the guarded value and returns the callback's result without
//! mutating the lock; `with_write` stores the callback's return value
//! back into the lock and returns it. The guarded value is an `i64`
//! for this first cut - a String-guarded variant is a documented
//! follow-up (it needs a pointer-shaped thunk and a String slot).

use parking_lot::RwLock as PRwLock;

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
        // sound to call under strict provenance; a bare integer
        // transmute at the call site would carry none.
        Some(std::ptr::with_exposed_provenance::<()>(addr))
    }
}

/// Opaque heap handle wrapping the guarded `i64`.
pub struct GosRwLock {
    inner: PRwLock<i64>,
}

/// Allocate a `sync::RwLock` guarding `value`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_rwlock_new(value: i64) -> *mut GosRwLock {
    ffi_entry!(std::ptr::null_mut(), {
        Box::into_raw(Box::new(GosRwLock {
            inner: PRwLock::new(value),
        }))
    })
}

/// `lock.get()` - read the guarded value under a shared lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rwlock_get(lock: *mut GosRwLock) -> i64 {
    ffi_entry!(0, {
        if lock.is_null() {
            return 0;
        }
        // SAFETY: `lock` is a handle from compiled code, checked non-null above and live for the whole call.
        *unsafe { &*lock }.inner.read()
    })
}

/// `lock.set(value)` - overwrite the guarded value under an
/// exclusive lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rwlock_set(lock: *mut GosRwLock, value: i64) {
    ffi_entry!((), {
        if lock.is_null() {
            return;
        }
        // SAFETY: `lock` is a handle from compiled code, checked non-null above and live for the whole call.
        *unsafe { &*lock }.inner.write() = value;
    });
}

/// `sync::RwLock::with_read(lock, f)` - run `f(value)` under a shared
/// lock and return its result; the guarded value is unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_rwlock_with_read(
    lock: *mut GosRwLock,
    env: *const u8,
) -> i64 {
    ffi_entry_passthrough!(0, {
        if lock.is_null() {
            return 0;
        }
        // SAFETY: `lock` is a handle from compiled code, checked non-null above and live for the whole call.
        let value = *unsafe { &*lock }.inner.read();
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

/// `sync::RwLock::with_write(lock, f)` - run `f(value)` under an
/// exclusive lock, store the returned value back, and return it.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_rwlock_with_write(
    lock: *mut GosRwLock,
    env: *const u8,
) -> i64 {
    ffi_entry_passthrough!(0, {
        if lock.is_null() {
            return 0;
        }
        // SAFETY: `lock` is a handle from compiled code, checked non-null above and live for the whole call.
        let mut guard = unsafe { &*lock }.inner.write();
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
        next
    })
}
