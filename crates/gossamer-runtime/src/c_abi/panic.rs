#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// Panic
// ---------------------------------------------------------------

thread_local! {
    /// The message of the most recent goroutine panic on this worker
    /// thread. Set by `gos_rt_panic` just before it raises the Rust
    /// panic, so a spawned goroutine's Drop-guard (in `gos_rt_spawn`)
    /// can read it during unwinding and deliver `Err(message)` to the
    /// join handle. The runtime catches the panic itself, so the
    /// payload string is otherwise unreachable from the spawn body.
    static LAST_GOROUTINE_PANIC: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Records the current goroutine's panic message for `gos_rt_spawn`'s
/// join-handle delivery.
pub(crate) fn set_last_goroutine_panic(msg: &str) {
    LAST_GOROUTINE_PANIC.with(|c| *c.borrow_mut() = Some(msg.to_string()));
}

/// Takes (and clears) the last goroutine panic message recorded on
/// this thread, if any.
pub(crate) fn take_last_goroutine_panic() -> Option<String> {
    LAST_GOROUTINE_PANIC.with(|c| c.borrow_mut().take())
}

/// Reads the last goroutine panic message without clearing it, for a
/// second observer on the same unwind: a cohort records the child's
/// failure and the join handle still delivers the message.
pub(crate) fn peek_last_goroutine_panic() -> Option<String> {
    LAST_GOROUTINE_PANIC.with(|c| c.borrow().clone())
}

// `C-unwind`, not `C`: on the goroutine path this raises a Rust panic
// that must unwind back through its Gossamer caller to the coroutine
// wrapper (and to a `spawn` join handle's Drop-guard). A plain
// `extern "C"` declares the function nounwind, so that unwind would
// trip the nounwind contract and abort whenever a cleanup frame sits
// between the panic and its catch.
use gossamer_coro::GosPanic;

/// User panic hook installed via `runtime::set_panic_hook`: a bare
/// `fn(String)` code pointer called with the rendered message instead
/// of the default `error[GX0005]` report. Zero = unset.
static USER_PANIC_HOOK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Registers a non-capturing `fn(String)` as the process panic hook.
/// Null clears it. The hook replaces the default stderr report for
/// both main-goroutine and isolated-goroutine panics; fatality and
/// isolation semantics are unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_panic_hook(f: *const u8) {
    USER_PANIC_HOOK.store(f as usize, std::sync::atomic::Ordering::Release);
}

/// Invoke the user hook with `text`. Returns false when no hook is set.
pub(crate) fn call_user_panic_hook(text: &str) -> bool {
    let f = USER_PANIC_HOOK.load(std::sync::atomic::Ordering::Acquire);
    if f == 0 {
        return false;
    }
    let c = std::ffi::CString::new(text).unwrap_or_default();
    // SAFETY: the pointer was registered by compiled code as a
    // non-capturing `fn(String)`; the ABI is one c-string argument.
    let hook: extern "C" fn(*const c_char) = unsafe { std::mem::transmute(f as *const u8) };
    hook(c.as_ptr());
    true
}

/// Install the process Rust panic hook that silences
/// Gossamer-originated panics (their report is printed by
/// `gos_rt_panic` before the unwind starts). Idempotent.
pub(crate) fn install_silent_gos_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if info.payload().downcast_ref::<GosPanic>().is_some() {
                return;
            }
            prev(info);
        }));
    });
}

/// Renders the host's own call stack for a fault raised in compiled code.
/// The bytecode VM installs one so a JIT-compiled body's panic still names
/// the interpreted frames that reached it; a standalone native binary has no
/// host and leaves it unset.
pub type TraceHookFn = extern "C" fn() -> *mut c_char;

static TRACE_HOOK: std::sync::atomic::AtomicPtr<()> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Installs the host's call-stack renderer. Idempotent; the last wins.
pub unsafe fn install_trace_hook(hook: TraceHookFn) {
    TRACE_HOOK.store(hook as *mut (), std::sync::atomic::Ordering::Release);
}

/// The host's call stack, or the empty string when no host installed a
/// renderer or it had nothing to report.
fn host_trace() -> String {
    let raw = TRACE_HOOK.load(std::sync::atomic::Ordering::Acquire);
    if raw.is_null() {
        return String::new();
    }
    // SAFETY: `raw` was stored from a `TraceHookFn` in
    // `install_trace_hook` and is read back at the same type.
    let hook: TraceHookFn = unsafe { std::mem::transmute::<*mut (), TraceHookFn>(raw) };
    let text = hook();
    if text.is_null() {
        return String::new();
    }
    // SAFETY: the hook hands back an owned runtime string; copy and free it.
    let out = unsafe { crate::c_abi::gos_str_arg_string(text) };
    unsafe { crate::c_abi::string::gos_rt_str_free(text) };
    out
}

#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_panic(msg: *const c_char) {
    let text = if msg.is_null() {
        "panic".to_string()
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(msg) }
    };
    raise("GX0005", "panic: ", text);
}

/// Raises the same fault as [`gos_rt_panic`] for a caller inside the runtime.
///
/// The C entry point measures its argument through the string ABI, which reads
/// the header a Gossamer `String` carries behind its body. Runtime callers
/// already hold Rust text, so they hand it over directly rather than shaping a
/// C string the ABI would have to treat as foreign.
pub(crate) fn panic_text(text: &str) {
    raise("GX0005", "panic: ", text.to_string());
}

/// [`gos_rt_panic_oob`] for a caller inside the runtime. See [`panic_text`].
pub(crate) fn panic_oob_text(what: &str, idx: i64, len: i64) -> ! {
    raise(
        "GX0005",
        "panic: ",
        format!("{what} out of bounds: the len is {len} but the index is {idx}"),
    );
}

thread_local! {
    /// Whether a fault on this thread ends only the work it is serving.
    ///
    /// A goroutine is its own fault domain because the scheduler owns it.
    /// A thread the runtime spawned to serve one unit of work - an HTTP
    /// connection - is one too: the unit fails, the process keeps running.
    /// Only the main goroutine's fault is the program's.
    static ISOLATED_FAULTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks the calling thread as its own fault domain for the guard's life.
pub struct IsolatedFaults(bool);

impl IsolatedFaults {
    /// Enters an isolated fault domain on this thread.
    #[must_use]
    pub fn enter() -> Self {
        Self(ISOLATED_FAULTS.replace(true))
    }
}

impl Drop for IsolatedFaults {
    fn drop(&mut self) {
        ISOLATED_FAULTS.set(self.0);
    }
}

/// Whether a fault raised on this thread ends only the work it is serving.
fn faults_are_isolated() -> bool {
    gossamer_coro::in_goroutine() || ISOLATED_FAULTS.with(std::cell::Cell::get)
}

/// Raises `text` as a fault carrying diagnostic `code`, rendered as
/// `error[<code>]: <prefix><text>`.
///
/// Every fault a compiled body can raise shares this path: the user
/// hook, the per-goroutine isolation, the stdout flush, and the pinned
/// exit code are properties of the fault, not of which one it is.
fn raise(code: &str, prefix: &str, text: String) -> ! {
    // A static panic message from codegen carries its own line terminator;
    // the report adds one, and two would leave a blank line between the
    // message and the frames below it.
    let text = text.trim_end_matches('\n').to_string();
    install_silent_gos_hook();
    let hooked = call_user_panic_hook(&text);
    // per-goroutine panic isolation. If the panic originates inside a spawned
    // goroutine, raise a Rust panic the coroutine wrapper catches - the
    // scheduler continues running other goroutines. If we're on the main thread
    // (no active coroutine), a panic in `fn main()` is fatal, just like in Rust.
    if faults_are_isolated() {
        // Stash the message so a `spawn`-created join handle can deliver
        // `Err(message)` from its unwinding Drop-guard before the coroutine
        // wrapper catches and isolates this panic.
        set_last_goroutine_panic(&text);
        // A panic in a JOINABLE (`spawn`) body is observed through `join()`,
        // which delivers it as `Err`. Suppress the eager report so stderr stays
        // clean, matching the VM's silent `spawn`+`join` path. A fire-and-forget
        // `go` panic is unobserved, so it still reports - eagerly, so the report
        // is reliable even when `main` exits right after.
        // A connection thread reports through the server's own request-fault
        // record, which carries the method and path this bare line cannot.
        if !hooked
            && !gossamer_coro::in_joinable_spawn()
            && !ISOLATED_FAULTS.with(std::cell::Cell::get)
        {
            unsafe {
                gos_rt_flush_stdout();
            }
            eprintln!("error[{code}]: {prefix}{text}");
        }
        std::panic::panic_any(GosPanic(text));
    }
    // Fatal main-goroutine fault: report (with the active call stack), flush
    // buffered stdout (a plain `abort` would drop it), and exit with the pinned
    // panic code 101 - matching Rust; no core is dumped for an ordinary panic.
    if !hooked {
        // Everything the program printed before the fault belongs ahead of
        // the report; buffered stdout would otherwise land after it and read
        // as though the fault came first.
        unsafe {
            gos_rt_flush_stdout();
        }
        // Match the unified diagnostic-code prefix the VM uses so both
        // execution modes tag a fault with the same code.
        eprintln!("error[{code}]: {prefix}{text}");
        let trace = crate::sigquit::render_active_panic_trace();
        if trace.is_empty() {
            let host = host_trace();
            if host.is_empty() {
                let native = crate::sigquit::render_native_panic_trace();
                if !native.is_empty() {
                    eprint!("{native}");
                }
            } else {
                eprint!("{host}");
            }
        } else {
            eprint!("{trace}");
        }
    }
    unsafe {
        gos_rt_flush_stdout();
    }
    std::process::exit(101);
}

/// Pushes a call-stack frame on entry to a Gossamer function.
/// Codegen prologues emit one call per function entry; the
/// interpreter calls this directly. `function`, `file`, and `line`
/// identify the frame for panic dumps and SIGQUIT renders.
///
/// `function` and `file` must be string constants in the program
/// image - the frame borrows them for the process lifetime rather
/// than copying, which is what keeps this off the allocator on a
/// path that runs once per call. NULL is rendered as an empty
/// string. The shim is reentrant-safe and takes no lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_stack_push(
    function: *const c_char,
    file: *const c_char,
    line: u32,
) {
    // SAFETY: the parameters are program-image string constants, per this
    // shim's contract above.
    let function = unsafe { crate::sigquit::ImageStr::new(function) };
    // SAFETY: as above.
    let file = unsafe { crate::sigquit::ImageStr::new(file) };
    crate::sigquit::stack_push(function, file, line);
}

/// Pops the topmost call-stack frame on return from a Gossamer
/// function. Tolerates over-pop (no-op when the stack is empty).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_stack_pop() {
    crate::sigquit::stack_pop();
}

/// Updates the line number of the topmost call-stack frame.
/// Emitted by codegen at MIR-statement granularity so panic
/// dumps show the line of the most recent statement, not the
/// function entry. The frame's file path stays as it was set by
/// the matching `gos_rt_stack_push`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_stack_set_line(line: u32) {
    crate::sigquit::set_active_line(line);
}

/// Returns 1 if any spawned goroutine has panicked since process
/// start, 0 otherwise. Sticky once set. Test helpers and
/// long-running services call this to assert clean execution
/// after a wait-group join.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_goroutine_panicked() -> i32 {
    i32::from(gossamer_coro::any_goroutine_panicked())
}

/// Panic helper for the dynamic array-index bounds check emitted
/// by the Cranelift and LLVM back-ends. Prints a diagnostic naming
/// the operation, the offending index, and the array length, then
/// routes through `gos_rt_panic` so the unified `error[GX0005]`
/// prefix and the panic-on-abort semantics stay consistent.
///
/// `what` is a static C string (e.g. `"array index"`) identifying
/// the failing access. NULL is tolerated and rendered as
/// `"array index"`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_panic_oob(what: *const c_char, idx: i64, len: i64) -> ! {
    let label = if what.is_null() {
        "array index".to_string()
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(what) }
    };
    panic_oob_text(&label, idx, len);
}

/// Panic helper for a failed `Vec` bounds check: names the index and the
/// vector's length, read here on the failing path so a passing check carries
/// no length for it. A null vector is the empty one.
///
/// # Safety
/// `v` must be null or point to a live `GosVec`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_panic_vec_index(
    v: *const crate::c_abi::vec::GosVec,
    idx: i64,
) -> ! {
    let len = if v.is_null() { 0 } else { unsafe { (*v).len } };
    panic_oob_text("vec index", idx, len);
}

// ---------------------------------------------------------------
// Exit
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_exit(code: i32) -> ! {
    // signal the netpoller thread to drain its
    // current `poll()` cycle before `std::process::exit` kills it.
    // Without this, in-flight TCP send buffers were terminated by
    // RST (process death) instead of FIN (graceful close). The
    // poller checks the flag at the top of each tick (1 ms ceiling).
    crate::sched_global::request_shutdown();
    // Drain the runtime's line-buffered stdout cache before
    // process exit. Without the flush, `println!("...")` followed
    // by `os::exit(N)` produces no output - `std::process::exit`
    // skips the C++/atexit handlers that would normally drain
    // stdio.
    unsafe {
        gos_rt_flush_stdout();
    }
    std::process::exit(code);
}

/// Returns the current process ID. Wraps `std::process::id`. The
/// LLVM and cranelift backends call this for `process::id()` in
/// Gossamer source.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_process_id() -> u32 {
    ffi_entry!(0, { crate::platform::process_id() })
}

/// Aborts the current process without unwinding. Wraps
/// `std::process::abort`. Used by `process::abort()` in Gossamer
/// source. Doesn't flush stdout - abort semantics.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_process_abort() -> ! {
    std::process::abort();
}
