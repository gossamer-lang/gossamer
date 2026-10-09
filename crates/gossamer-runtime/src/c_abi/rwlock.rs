#![allow(clippy::missing_safety_doc)]

//! Runtime support for `std::sync::RwLock` - a reader-writer lock
//! guarding a single `i64` value. The handle is the payload of a counted
//! heap node with no counted children: compiled tiers carry the pointer as
//! an `i64`, retain it for each holder (a binding, a closure environment, a
//! field, a goroutine), and release it when the holder ends, so the lock is
//! freed with its last share. The MIR receiver-kind dispatch tags
//! constructor results `sync::RwLock` so method calls route to the helpers
//! below.
//!
//! `with_read` / `with_write` cross the C-ABI through the shared
//! callable convention used by the `iter::*` / `option::*` / `Once`
//! combinators: `env` is a heap blob whose first word is the callable
//! address; the body is invoked as `f(env, value)`. `with_read` reads the
//! guarded value under the shared lock, releases it, and returns the
//! callback's result for that value; `with_write` runs the callback under
//! the exclusive lock, stores its return value back, and returns it.

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

/// The guarded `i64`, stored as the payload of a counted node.
///
/// The lock is a goroutine-level reader-writer lock: a goroutine that has to
/// wait parks, giving its worker back to the scheduler, so a callback that
/// blocks while holding the lock never stalls the goroutines that would let
/// it finish. A thread that is not a goroutine waits on a condvar.
pub struct GosRwLock {
    state: parking_lot::Mutex<RwState>,
    /// Signalled on release for a waiting thread that is not a goroutine.
    released: parking_lot::Condvar,
}

struct RwState {
    value: i64,
    readers: usize,
    writer: bool,
    /// Goroutines parked waiting for the lock, in arrival order.
    parked: std::collections::VecDeque<crate::sched::Gid>,
}

impl RwState {
    fn can_read(&self) -> bool {
        !self.writer
    }

    fn can_write(&self) -> bool {
        !self.writer && self.readers == 0
    }
}

super::rc::managed_handle!(GosRwLock);

/// The access a holder of the lock has, given back when it is dropped -
/// including when a callback run under the lock panics.
struct Held<'a> {
    lock: &'a GosRwLock,
    exclusive: bool,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let mut state = self.lock.state.lock();
        if self.exclusive {
            state.writer = false;
        } else {
            state.readers -= 1;
        }
        // Every parked goroutine re-checks the lock, so a writer that cannot
        // proceed yet parks again rather than being skipped for good.
        let waiters: Vec<crate::sched::Gid> = state.parked.drain(..).collect();
        drop(state);
        for gid in waiters {
            crate::sched_global::scheduler().unpark(gid);
        }
        self.lock.released.notify_all();
    }
}

/// Takes `lock` for shared or exclusive access, waiting until it is free.
fn acquire(lock: &GosRwLock, exclusive: bool) -> Held<'_> {
    let free = |state: &RwState| {
        if exclusive {
            state.can_write()
        } else {
            state.can_read()
        }
    };
    let take = |state: &mut RwState| {
        if exclusive {
            state.writer = true;
        } else {
            state.readers += 1;
        }
    };
    if gossamer_coro::in_goroutine() {
        loop {
            let mut state = lock.state.lock();
            if free(&state) {
                take(&mut state);
                return Held { lock, exclusive };
            }
            let mut guard = Some(state);
            crate::sched_global::park(crate::sched::ParkReason::Sync, |parker| {
                if let Some(state) = guard.as_mut() {
                    state.parked.push_back(parker.gid);
                }
                drop(guard.take());
            });
        }
    }
    let mut state = lock.state.lock();
    if free(&state) {
        take(&mut state);
        return Held { lock, exclusive };
    }
    drop(state);
    // `main` waiting on a lock is waiting on the program's goroutines, so a
    // lock no goroutine left can release is a deadlock rather than a hang.
    let addr = std::ptr::from_ref(lock) as usize;
    crate::sched_global::main_waits_on(
        "RwLock",
        std::sync::Arc::new(move || {
            // SAFETY: the lock outlives this wait, which `main` ends with
            // `end_main_wait` before `acquire` returns.
            let lock = unsafe { &*(addr as *const GosRwLock) };
            let state = lock.state.lock();
            if exclusive {
                !state.can_write()
            } else {
                !state.can_read()
            }
        }),
    );
    let mut state = lock.state.lock();
    while !free(&state) {
        lock.released.wait(&mut state);
    }
    take(&mut state);
    drop(state);
    crate::sched_global::end_main_wait();
    Held { lock, exclusive }
}

/// Runs the callable `env` names on `value`, or answers `value` when there
/// is none.
///
/// # Safety
///
/// `env` is null or a live closure environment of the one-argument
/// value-thunk shape.
unsafe fn call_guard(env: *const u8, value: i64) -> i64 {
    // SAFETY: `env` is this function's argument, as `env_fn_addr` requires.
    match unsafe { env_fn_addr(env) } {
        Some(addr) => {
            // SAFETY: `addr` is the callable the closure lowering stored, and a one-argument
            // closure lowers to the `fn(env, i64) -> i64` value-thunk shape.
            let f: GuardFn = unsafe { std::mem::transmute(addr) };
            // SAFETY: `f` is that callable, whose environment `env` is live for the call.
            unsafe { f(env, value) }
        }
        None => value,
    }
}

/// Allocate a `sync::RwLock` guarding `value`, as a counted node holding one
/// share for the caller.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_rwlock_new(value: i64) -> *mut GosRwLock {
    ffi_entry!({
        super::rc::alloc_managed(GosRwLock {
            state: parking_lot::Mutex::new(RwState {
                value,
                readers: 0,
                writer: false,
                parked: std::collections::VecDeque::new(),
            }),
            released: parking_lot::Condvar::new(),
        })
    })
}

/// `lock.get()` - read the guarded value under a shared lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rwlock_get(lock: *mut GosRwLock) -> i64 {
    ffi_entry!({
        if lock.is_null() {
            return 0;
        }
        // SAFETY: `lock` is a handle from compiled code, checked non-null above and live for the whole call.
        let lock = unsafe { &*lock };
        let _held = acquire(lock, false);
        lock.state.lock().value
    })
}

/// `lock.set(value)` - overwrite the guarded value under an
/// exclusive lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rwlock_set(lock: *mut GosRwLock, value: i64) {
    ffi_entry!({
        if lock.is_null() {
            return;
        }
        // SAFETY: `lock` is a handle from compiled code, checked non-null above and live for the whole call.
        let lock = unsafe { &*lock };
        let _held = acquire(lock, true);
        lock.state.lock().value = value;
    });
}

/// `sync::RwLock::with_read(lock, f)` - read the value under the shared lock,
/// then return `f(value)` with the lock released; the guarded value is
/// unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_rwlock_with_read(
    lock: *mut GosRwLock,
    env: *const u8,
) -> i64 {
    ffi_entry_passthrough!({
        if lock.is_null() {
            return 0;
        }
        // SAFETY: `lock` is a handle from compiled code, checked non-null above and live for the whole call.
        let lock = unsafe { &*lock };
        let value = {
            let _held = acquire(lock, false);
            lock.state.lock().value
        };
        // SAFETY: `env` is this shim's argument, a live closure environment or null (C-ABI
        // contract).
        unsafe { call_guard(env, value) }
    })
}

/// `sync::RwLock::with_write(lock, f)` - run `f(value)` under an
/// exclusive lock, store the returned value back, and return it.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_rwlock_with_write(
    lock: *mut GosRwLock,
    env: *const u8,
) -> i64 {
    ffi_entry_passthrough!({
        if lock.is_null() {
            return 0;
        }
        // SAFETY: `lock` is a handle from compiled code, checked non-null above and live for the whole call.
        let lock = unsafe { &*lock };
        let _held = acquire(lock, true);
        let current = lock.state.lock().value;
        // SAFETY: `env` is this shim's argument, a live closure environment or null (C-ABI
        // contract).
        let next = unsafe { call_guard(env, current) };
        lock.state.lock().value = next;
        next
    })
}
