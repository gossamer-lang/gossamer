//! `std::runtime::Guard` for the bytecode VM: a value that runs a closure
//! once, when the last reference to it goes.
//!
//! The handle shares one [`VmGuard`] between every copy, so the release of
//! the last copy drops it. A release can happen while the interpreter's own
//! state is borrowed (a frame being torn down), so the guard's `Drop` queues
//! its cleanup on the releasing thread, which runs one goroutine. The queue
//! runs at the cleanup points the compiler places (the end of a statement
//! that released a guard, a scope-bound binding's release) and when the
//! goroutine or the program ends, as on the compiled tiers. A panic in a
//! cleanup ends the program with its report.

use std::sync::Arc;

use crate::value::{RuntimeError, RuntimeResult, Value};

thread_local! {
    /// Cleanups released on this thread, oldest first, not yet run.
    static PENDING: std::cell::RefCell<Vec<Value>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Whether [`PENDING`] holds any, read on the interpreter's hot path.
    static ANY_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the program has made a guard, so a call return in a program
/// that never does pays one relaxed load for its cleanup point.
static GUARDS_MADE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The state every copy of one guard shares: its cleanup, until it runs.
struct VmGuard {
    cleanup: parking_lot::Mutex<Option<Value>>,
}

impl Drop for VmGuard {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.get_mut().take() {
            PENDING.with(|queue| queue.borrow_mut().push(cleanup));
            ANY_PENDING.with(|flag| flag.set(true));
        }
    }
}

/// Runs the cleanups released on this thread, oldest first, including any a
/// cleanup itself releases. A cleanup's fault ends the program.
#[inline]
pub fn run_pending_cleanups(vm: &crate::vm::Vm) {
    if !GUARDS_MADE.load(std::sync::atomic::Ordering::Relaxed)
        || !ANY_PENDING.with(std::cell::Cell::get)
    {
        return;
    }
    while let Some(cleanup) = take_pending() {
        if let Err(err) = vm.apply_value(cleanup, Vec::new()) {
            end_for_cleanup_fault(err);
            return;
        }
    }
}

thread_local! {
    /// The fault a cleanup raised where the process cannot end, which ends
    /// the program's run instead.
    static CLEANUP_FAULT: std::cell::RefCell<Option<RuntimeError>> =
        const { std::cell::RefCell::new(None) };
}

/// The fault a cleanup raised where the process could not end, taken once.
pub fn take_cleanup_fault() -> Option<RuntimeError> {
    CLEANUP_FAULT.with(|fault| fault.borrow_mut().take())
}

/// Drops the cleanups waiting on this thread without running them: those a
/// finished program's last releases queued, which no program is left to run.
pub fn discard_pending_cleanups() {
    // Taken out before it drops: a cleanup's environment may hold a guard
    // whose release queues another.
    let stale = PENDING.with(|queue| std::mem::take(&mut *queue.borrow_mut()));
    drop(stale);
    PENDING.with(|queue| queue.borrow_mut().clear());
    ANY_PENDING.with(|flag| flag.set(false));
}

/// Ends the program for a fault a cleanup raised: an exit ends the process
/// with its code, and any other fault is reported as a fault in `main` is.
/// Where the process cannot end (a wasm module), the fault is kept for the
/// run to report and the cleanups still waiting are dropped.
fn end_for_cleanup_fault(err: RuntimeError) {
    if !gossamer_runtime::platform::CAN_END_PROCESS {
        CLEANUP_FAULT.with(|fault| *fault.borrow_mut() = Some(err));
        discard_pending_cleanups();
        return;
    }
    crate::flush_runtime_stdout();
    if let Some(code) = crate::exit_status(&err) {
        crate::run_exit_hooks();
        std::process::exit(code);
    }
    crate::run_exit_hooks();
    crate::flush_runtime_stdout();
    eprintln!("{err}");
    std::process::exit(101);
}

pub(crate) fn install_guard(globals: &mut Vec<(&'static str, Value)>) {
    for name in ["Guard::new", "runtime::Guard::new"] {
        globals.push((name, crate::builtins::builtin_pub(name, builtin_guard_new)));
    }
    for name in [
        "__gos_keep",
        "__gos_release_binding",
        "__gos_release_scrutinee",
    ] {
        globals.push((name, crate::builtins::builtin_pub(name, builtin_keep)));
    }
    globals.push((
        "__gos_run_cleanups",
        Value::native("__gos_run_cleanups", native_run_cleanups),
    ));
}

/// A read of a binding that keeps it to that point. The compiler gives a
/// local binding's `__gos_release_binding` its own op, which empties the
/// binding's register.
fn builtin_keep(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Unit)
}

/// Runs the cleanups the binding releases just before it released, at the
/// point in the block the compiled tiers run them.
fn native_run_cleanups(
    dispatch: &mut dyn crate::value::NativeDispatch,
    _args: &[Value],
) -> RuntimeResult<Value> {
    while let Some(cleanup) = take_pending() {
        if let Err(err) = dispatch.call_value(&cleanup, Vec::new()) {
            end_for_cleanup_fault(err);
            break;
        }
    }
    Ok(Value::Unit)
}

/// The oldest cleanup waiting on this thread, if any.
fn take_pending() -> Option<Value> {
    PENDING.with(|queue| {
        let mut queue = queue.borrow_mut();
        if queue.is_empty() {
            ANY_PENDING.with(|flag| flag.set(false));
            None
        } else {
            Some(queue.remove(0))
        }
    })
}

/// `runtime::Guard::new(f)`: a guard that calls `f` once, when the last
/// reference to it goes.
pub(crate) fn builtin_guard_new(args: &[Value]) -> RuntimeResult<Value> {
    let cleanup = args
        .first()
        .filter(|f| matches!(f, Value::Closure(_) | Value::Builtin(_) | Value::Native(_)))
        .cloned();
    Ok(guard_value(cleanup))
}

fn guard_value(cleanup: Option<Value>) -> Value {
    GUARDS_MADE.store(true, std::sync::atomic::Ordering::Relaxed);
    crate::value::state_handle(
        "runtime::Guard",
        "__guard",
        Arc::new(VmGuard {
            cleanup: parking_lot::Mutex::new(cleanup),
        }),
    )
}
