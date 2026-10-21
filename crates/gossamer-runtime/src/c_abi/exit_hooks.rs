//! `runtime::at_exit(f)`: closures run, last registered first, when the
//! program ends by returning from `main`, by `process::exit`, or by an
//! uncaught panic.
//!
//! Compiled code registers a closure environment; the bytecode VM registers
//! an id it resolves itself through the host runner it installs. Both share
//! one list, so the order is registration order whichever tier registered.

#![allow(unsafe_code)]

use parking_lot::Mutex;

/// One registered hook.
enum Hook {
    /// A compiled closure environment holding a share of it; its first word
    /// is the code, called as `extern "C" fn(env)`.
    Native(usize),
    /// A hook the host (the bytecode VM) keeps under this id.
    Host(u64),
}

static HOOKS: Mutex<Vec<Hook>> = Mutex::new(Vec::new());

/// Runs a host hook by id. Installed once by the bytecode VM.
pub type HostRunner = fn(u64);

static HOST_RUNNER: Mutex<Option<HostRunner>> = Mutex::new(None);

type StaticDrop = unsafe extern "C" fn();
static STATIC_DROPS: Mutex<Vec<(usize, StaticDrop)>> = Mutex::new(Vec::new());

/// Registers the finalizer of one native static cell, once per cell. The
/// finalizer reads its current value, so reassignments do not retain old values.
///
/// # Safety
/// `slot` identifies live program storage and `dropper` releases and clears
/// that storage. Both remain valid until program or library shutdown.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_register_static_drop(
    slot: *const u8,
    dropper: unsafe extern "C" fn(),
) {
    let mut drops = STATIC_DROPS.lock();
    if !drops.iter().any(|(address, _)| *address == slot as usize) {
        drops.push((slot as usize, dropper));
    }
}

/// Releases native static owners after user exit hooks have finished reading
/// them. Take the registry before calling into generated code to avoid holding
/// a lock across finalizers and to release its own allocation as well.
pub(crate) fn run_static_drops() {
    let drops = std::mem::take(&mut *STATIC_DROPS.lock());
    for (_, dropper) in drops.into_iter().rev() {
        // SAFETY: registration keeps the cell and generated callback valid
        // until this shutdown, and each registered cell is visited once.
        unsafe { dropper() };
    }
}

/// Normal native termination has no user frames left. A bounded exit drain
/// may still leave background services alive, in which case process teardown
/// must keep their storage valid until the OS stops them.
pub(crate) fn release_program_storage() {
    if crate::sched_global::program_storage_is_idle() && crate::sched_global::finish_idle_runtime()
    {
        run_static_drops();
        super::args::release_program_args();
        // Images are decoded only off wasm32, which has no image module.
        #[cfg(not(target_arch = "wasm32"))]
        super::image::release_program_images();
        super::rc::release_program_pins();
        super::string::trim_heap_string_registry();
        super::cohort::trim_idle_registries();
    }
}

/// Installs the runner for `Hook::Host` entries. The last call wins.
pub fn install_host_runner(runner: HostRunner) {
    *HOST_RUNNER.lock() = Some(runner);
}

/// Registers a host hook under `id`.
pub fn push_host_hook(id: u64) {
    HOOKS.lock().push(Hook::Host(id));
}

/// `runtime::at_exit(f)` on the compiled tiers: keeps a share of the closure
/// environment `env` until the program ends.
///
/// # Safety
///
/// `env` is a live closure environment of a `Fn()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_at_exit(env: *mut u8) {
    if env.is_null() {
        return;
    }
    // SAFETY: `env` is a live counted environment (contract).
    unsafe { super::rc::gos_rt_rc_retain(env) };
    HOOKS.lock().push(Hook::Native(env as usize));
}

/// Runs every registered hook, last registered first, each at most once. A
/// hook is taken off the list before it runs, so one that exits or panics
/// leaves the rest to the exit path it starts.
pub fn run_exit_hooks() {
    loop {
        let Some(hook) = HOOKS.lock().pop() else {
            return;
        };
        match hook {
            Hook::Native(env) => {
                let env = env as *const u8;
                // SAFETY: the environment's first word is its closure's code,
                // compiled as `extern "C" fn(env)` for a `Fn()`; the share
                // taken at registration keeps it live.
                unsafe {
                    let code = env.cast::<*const ()>().read();
                    let call: unsafe extern "C" fn(*const u8) = std::mem::transmute(code);
                    call(env);
                    super::rc::gos_rt_rc_release(env.cast_mut());
                }
            }
            Hook::Host(id) => {
                let runner = *HOST_RUNNER.lock();
                if let Some(runner) = runner {
                    runner(id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn static_finalizers_are_deduplicated_and_consumed() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn release() {
            CALLS.fetch_add(1, Ordering::SeqCst);
        }
        let slot = (&raw const CALLS).cast::<u8>();
        // SAFETY: the test's static cell and callback remain valid forever.
        unsafe {
            gos_rt_register_static_drop(slot, release);
            gos_rt_register_static_drop(slot, release);
        }
        run_static_drops();
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        run_static_drops();
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exit_hook_releases_its_environment_share() {
        unsafe extern "C" fn hook(_: *const u8) {}
        // SAFETY: this test owns a counted environment whose code word names
        // the no-op hook; its own share stays live across hook execution.
        unsafe {
            let env = super::super::rc::gos_rt_rc_alloc(8, std::ptr::null());
            assert!(!env.is_null());
            env.cast::<*const ()>().write(hook as *const ());
            gos_rt_at_exit(env);
            assert_eq!(super::super::rc::rc_strong_count(env), 2);
            run_exit_hooks();
            assert_eq!(super::super::rc::rc_strong_count(env), 1);
            super::super::rc::gos_rt_rc_release(env);
        }
    }
}
