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

/// Installs the runner for [`Hook::Host`] entries. The last call wins.
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
