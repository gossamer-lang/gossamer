//! Runtime support for `runtime::Guard`: a value that runs a closure once,
//! when the last reference to it goes.
//!
//! The handle is a counted node of the finalized kind, so the release that
//! takes its strong count to zero runs [`GosGuard`]'s `Drop`, which queues
//! the closure on the releasing goroutine. The queue runs at the cleanup
//! points the compiler places (the end of a statement that released a guard,
//! a scope-bound binding's release) and when the goroutine or the program
//! ends, so a cleanup runs at the same point of the program on every tier,
//! whichever release inside a statement was the last. A panic in a cleanup
//! ends the program with its report.

use std::sync::atomic::{AtomicPtr, Ordering};

/// `fn(env)` - the shape a zero-argument closure lowers to.
type CleanupFn = unsafe extern "C-unwind" fn(env: *const u8);

/// The node a `runtime::Guard` carries: the cleanup closure's environment,
/// or null once the cleanup has run.
pub struct GosGuard {
    env: AtomicPtr<u8>,
}

super::rc::managed_handle!(GosGuard);

impl Drop for GosGuard {
    fn drop(&mut self) {
        let env = self.env.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !env.is_null() {
            super::panic::queue_cleanup(env as usize);
        }
    }
}

/// Runs the cleanups queued on the running goroutine, oldest first,
/// including any a cleanup itself queues.
pub(crate) fn run_cleanups() {
    while let Some(env) = super::panic::next_cleanup() {
        let env = env as *mut u8;
        // SAFETY: `env` is the live environment of a cleanup closure, whose
        // first word is the closure's entry address (closure lowering
        // invariant); the queue holds the guard's share of it.
        let addr = unsafe { env.cast::<usize>().read() };
        if addr != 0 {
            // SAFETY: `addr` is the entry a zero-argument closure lowers to, the
            // `fn(env)` shape, with its exposed provenance recovered.
            let cleanup: CleanupFn =
                unsafe { std::mem::transmute(std::ptr::with_exposed_provenance::<()>(addr)) };
            // SAFETY: `cleanup` is that entry and `env` its live environment.
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { cleanup(env) }));
            if let Err(payload) = outcome {
                super::panic::end_for_unreleasable_fault(payload.as_ref());
            }
        }
        // SAFETY: the guard's own share of the environment, taken in
        // `gos_rt_runtime_guard_new` and given back once.
        unsafe { super::rc::gos_rt_rc_release(env) };
    }
}

/// Runs the cleanups queued on the running goroutine when dropped, so a
/// goroutine's last releases run theirs before it ends, on an unwind too.
/// wasm32 runs no goroutines.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct CleanupsAtExit;

#[cfg(not(target_arch = "wasm32"))]
impl Drop for CleanupsAtExit {
    fn drop(&mut self) {
        run_cleanups();
    }
}

/// The point a program runs the cleanups of the guards released since the
/// last one: the end of a statement that released a guard, and a
/// scope-bound binding's release.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_runtime_run_cleanups() {
    run_cleanups();
}

/// `runtime::Guard::new(f)`: a guard that calls `f` once, when the last
/// reference to it goes.
///
/// # Safety
///
/// `env` is null or the live environment of a zero-argument closure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_runtime_guard_new(env: *mut u8) -> *mut GosGuard {
    ffi_entry!({
        if !env.is_null() {
            // SAFETY: `env` is a live counted closure environment (contract);
            // the guard keeps this share until its cleanup has run.
            unsafe { super::rc::gos_rt_rc_retain(env) };
        }
        super::rc::alloc_managed(GosGuard {
            env: AtomicPtr::new(env),
        })
    })
}
