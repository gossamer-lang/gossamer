#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(clippy::wildcard_imports)]

use parking_lot::{Condvar, Mutex, Once};
use std::sync::atomic::{AtomicI64, Ordering};

// ---------------------------------------------------------------
// sync::Barrier - fixed-participant rendezvous
// ---------------------------------------------------------------
//
// Mirrors `gossamer_std::sync::Barrier` (the VM/interp backing
// type) bit-for-bit: a `Mutex<BarrierState>` plus a `Condvar`,
// generation-counted so a barrier reused across rounds wakes only
// the waiters of the current generation. Every participant calls
// `wait()`; the first `n - 1` block on the condvar, the `n`th
// flips the generation and notifies all. No spinning, no sleeps -
// identical observable semantics to the interpreter so tier-parity
// output matches.
//
// Like the compiled `GosWaitGroup`, a waiter blocks its OS worker
// thread on the condvar. A barrier needs every participant alive
// at once, so a program must not enqueue more simultaneous
// participants than the scheduler has worker threads (the main
// goroutine runs on its own thread and never consumes a pool
// worker).

struct BarrierState {
    expected: usize,
    arrived: usize,
    generation: u64,
}

pub struct GosBarrier {
    state: Mutex<BarrierState>,
    cv: Condvar,
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_barrier_new(n: i64) -> *mut GosBarrier {
    ffi_entry!(std::ptr::null_mut(), {
        // A zero or negative count would never release; clamp to a
        // single participant so `wait()` returns immediately rather
        // than deadlocking, matching `Barrier::new(1)`.
        let expected = if n < 1 { 1 } else { n as usize };
        Box::into_raw(Box::new(GosBarrier {
            state: Mutex::new(BarrierState {
                expected,
                arrived: 0,
                generation: 0,
            }),
            cv: Condvar::new(),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_barrier_wait(b: *mut GosBarrier) {
    ffi_entry_passthrough!((), {
        if b.is_null() {
            return;
        }
        // SAFETY: `b` is a handle from compiled code, checked non-null above and live for the whole call.
        let b = unsafe { &*b };
        let mut state = b.state.lock();
        let captured_gen = state.generation;
        state.arrived += 1;
        if state.arrived >= state.expected {
            state.arrived = 0;
            state.generation = state.generation.wrapping_add(1);
            b.cv.notify_all();
            return;
        }
        while state.generation == captured_gen {
            b.cv.wait(&mut state);
        }
    });
}

// ---------------------------------------------------------------
// sync::Once - run-exactly-once guard with a closure callback
// ---------------------------------------------------------------
//
// Wraps `parking_lot::Once`. `call(env)` runs the supplied closure
// the first time it is reached and never again, returning `1` on
// the run that executed the body and `0` on every subsequent call.
//
// The closure crosses the C-ABI through the shared callable
// convention used by the `iter::*` / `option::*` combinators: `env`
// is a heap blob whose first word is the callable address; the body
// is invoked as `f(env)`. A nullary closure returning `()` lowers to
// the same `fn(env) -> i64` value-thunk shape as
// `option::default_with`'s callback.

type ThunkValFn = unsafe extern "C-unwind" fn(env: *const u8) -> i64;

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

pub struct GosOnce {
    inner: Once,
    /// Goroutine that completed the once body. Every caller returning from
    /// `call_once` acquires this publication for race-detector purposes.
    completed_by: AtomicI64,
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_once_new() -> *mut GosOnce {
    ffi_entry!(std::ptr::null_mut(), {
        Box::into_raw(Box::new(GosOnce {
            inner: Once::new(),
            completed_by: AtomicI64::new(-1),
        }))
    })
}

/// Runs `env`'s closure exactly once across all callers of this
/// handle. Returns `1` on the call that executed the body, `0`
/// otherwise. Mirrors the interp's `native_once_call`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_once_call(o: *mut GosOnce, env: *const u8) -> i64 {
    ffi_entry_passthrough!(0, {
        if o.is_null() {
            return 0;
        }
        // SAFETY: `o` is a handle from compiled code, checked non-null above and live for the whole call.
        let o = unsafe { &*o };
        let mut ran = 0i64;
        o.inner.call_once(|| {
            ran = 1;
            // SAFETY: `env` is this shim's argument, as `env_fn_addr` requires (C-ABI contract).
            if let Some(addr) = unsafe { env_fn_addr(env) } {
                // SAFETY: addr is the callable stored by the closure
                // lowering; the nullary-unit closure lowers to the
                // `fn(env) -> i64` value-thunk shape.
                let f: ThunkValFn = unsafe { std::mem::transmute(addr) };
                // SAFETY: `f` is the callable `addr` names, whose environment `env` is live for
                // the call (C-ABI contract).
                unsafe {
                    f(env);
                }
            }
            o.completed_by
                .store(i64::from(crate::race::current_gid()), Ordering::Release);
        });
        let from = o.completed_by.load(Ordering::Acquire);
        if from >= 0 {
            crate::race::record_sync(u32::try_from(from).unwrap_or(0), crate::race::current_gid());
        }
        ran
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_records_the_goroutine_that_published_its_body() {
        crate::race::set_current_gid(703);
        let once = gos_rt_once_new();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert_eq!(unsafe { gos_rt_once_call(once, std::ptr::null()) }, 1);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert_eq!(unsafe { &*once }.completed_by.load(Ordering::Acquire), 703);
        crate::race::set_current_gid(704);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert_eq!(unsafe { gos_rt_once_call(once, std::ptr::null()) }, 0);
        // SAFETY: the pointer is a box this test's constructor call answered, reclaimed once here
        // and not used again.
        unsafe { drop(Box::from_raw(once)) };
    }
}
