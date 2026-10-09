#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
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
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// Panic
// ---------------------------------------------------------------

/// The fault state of one goroutine, or of a thread outside any goroutine.
/// It follows the goroutine across worker threads, since a deferred
/// expression may block while its frame unwinds.
#[derive(Default)]
struct FaultState {
    /// The message of the goroutine's most recent panic, with the notes its
    /// landing pads recorded. Set when the panic is raised, so a spawned
    /// goroutine's Drop-guard (in `gos_rt_spawn`) can deliver
    /// `Err(message)` to the join handle as the panic unwinds; the runtime
    /// catches the panic itself, so the payload is otherwise unreachable.
    last_panic: Option<String>,
    /// How many landing pads are running their deferred expressions.
    pads: usize,
    /// A panic a deferred expression raised inside a landing pad, until the
    /// pad's note handler records it.
    nested: Option<String>,
    /// The panics deferred expressions raised while the current one
    /// unwound, oldest first.
    notes: Vec<String>,
    /// The faults [`gos_rt_unwind_try_call`] caught for the landing pads
    /// running now, innermost last, each resumed when its pad has run.
    caught: Vec<Box<dyn std::any::Any + Send>>,
}

/// Runs `f` on the running goroutine's [`FaultState`], creating it first.
fn with_fault_state<R>(f: impl FnOnce(&mut FaultState) -> R) -> R {
    let mut word = gossamer_coro::local_word();
    if word == 0 {
        gossamer_coro::set_local_word_drop(drop_fault_state);
        word = Box::into_raw(Box::new(FaultState::default())) as usize;
        gossamer_coro::set_local_word(word);
    }
    // SAFETY: a nonzero local word is a `Box<FaultState>` this function
    // allocated for the running goroutine or thread, freed only by
    // `drop_fault_state` once that goroutine is gone, and no other borrow of
    // it is live during `f`, which runs no Gossamer code.
    let state = unsafe { &mut *(word as *mut FaultState) };
    f(state)
}

/// Frees a finished goroutine's [`FaultState`].
fn drop_fault_state(word: usize) {
    // SAFETY: `word` is the `Box<FaultState>` `with_fault_state` leaked into
    // the goroutine's local word, and its goroutine is being dropped.
    drop(unsafe { Box::from_raw(word as *mut FaultState) });
}

/// Records the current goroutine's panic message for `gos_rt_spawn`'s
/// join-handle delivery.
pub(crate) fn set_last_goroutine_panic(msg: &str) {
    with_fault_state(|state| state.last_panic = Some(msg.to_string()));
}

/// Takes (and clears) the last goroutine panic message recorded on
/// this goroutine, if any.
pub(crate) fn take_last_goroutine_panic() -> Option<String> {
    with_fault_state(|state| state.last_panic.take())
}

/// Reads the last goroutine panic message without clearing it, for a
/// second observer on the same unwind: a cohort records the child's
/// failure and the join handle still delivers the message.
pub(crate) fn peek_last_goroutine_panic() -> Option<String> {
    with_fault_state(|state| state.last_panic.clone())
}

/// The report line a panic a deferred expression raised while another
/// unwound adds to that panic's report.
fn unwind_note(nested: &str) -> String {
    format!("\nnote: a deferred expression panicked while unwinding: {nested}")
}

/// `text` with the notes this goroutine's landing pads recorded while its
/// panic unwound.
fn with_unwind_notes(text: &str) -> String {
    with_fault_state(|state| {
        let mut out = text.to_string();
        for note in &state.notes {
            out.push_str(&unwind_note(note));
        }
        out
    })
}

/// Records `text` as raised: the panic a frame starts unwinding with, or,
/// inside a landing pad, one a deferred expression raised.
fn record_raise(text: &str) -> bool {
    with_fault_state(|state| {
        if state.pads > 0 {
            state.nested = Some(text.to_string());
            true
        } else {
            state.notes.clear();
            false
        }
    })
}

/// A landing pad starts running its frame's pending deferred expressions.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unwind_begin() {
    with_fault_state(|state| state.pads += 1);
}

/// A landing pad has run its deferred expressions and resumes unwinding.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unwind_end() {
    with_fault_state(|state| state.pads = state.pads.saturating_sub(1));
}

/// A deferred expression panicked inside a landing pad: the panic is kept
/// as a note on the one the frame unwinds with, whose report names it.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unwind_note() {
    with_fault_state(|state| {
        if let Some(nested) = state.nested.take() {
            if let Some(last) = state.last_panic.as_mut() {
                last.push_str(&unwind_note(&nested));
            }
            state.notes.push(nested);
        }
    });
}

/// The body a catching call runs: a compiled thunk that reads its callee
/// and arguments from `ctx` and writes the callee's results back there.
type CatchingThunk = extern "C-unwind" fn(*mut u8);

/// Runs `thunk(ctx)`, answering the payload of a fault that unwound out of
/// it.
///
/// # Safety
///
/// `thunk` is the address of a compiled function of the [`CatchingThunk`]
/// shape, and `ctx` the buffer it was compiled to read.
unsafe fn catching_call(
    thunk: *const u8,
    ctx: *mut u8,
) -> Result<(), Box<dyn std::any::Any + Send>> {
    // SAFETY: the caller passes a thunk compiled with this signature.
    let thunk: CatchingThunk = unsafe { std::mem::transmute(thunk) };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| thunk(ctx)))
}

/// Makes a call from a body with landing pads, catching a fault that
/// unwinds out of it so the caller branches to its cleanup pad. The fault
/// is held until the pad's [`gos_rt_unwind_resume`].
///
/// # Safety
///
/// `thunk` is the address of a compiled catching thunk and `ctx` the
/// buffer holding the callee and arguments it reads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unwind_try_call(thunk: *const u8, ctx: *mut u8) -> i32 {
    // SAFETY: forwarded from this function's own contract.
    match unsafe { catching_call(thunk, ctx) } {
        Ok(()) => 0,
        Err(payload) => {
            with_fault_state(|state| state.caught.push(payload));
            1
        }
    }
}

/// Makes a call from a landing pad's code, catching a panic a deferred
/// expression raises so the pad notes it on the fault it is running for and
/// goes on with the next one.
///
/// # Safety
///
/// As [`gos_rt_unwind_try_call`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unwind_try_call_in_pad(thunk: *const u8, ctx: *mut u8) -> i32 {
    // SAFETY: forwarded from this function's own contract.
    match unsafe { catching_call(thunk, ctx) } {
        Ok(()) => 0,
        Err(_nested) => 1,
    }
}

/// Continues the fault the innermost running landing pad caught, once its
/// deferred expressions have run.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_unwind_resume() -> ! {
    match with_fault_state(|state| state.caught.pop()) {
        Some(payload) => std::panic::resume_unwind(payload),
        // A pad is entered only from a catching call that held its fault.
        None => std::process::abort(),
    }
}

thread_local! {
    /// Whether a fault on `main`'s thread unwinds to `gos_rt_call_main`, so
    /// the deferred expressions pending in its frames run before the report.
    static MAIN_UNWINDS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs a native program's `main` through `entry`, which writes its result
/// to `out`. A fault raised on `main`'s thread unwinds to here, running the
/// deferred expressions pending in every frame it leaves, and is reported
/// then, as the bytecode VM reports it after its frames unwind.
///
/// # Safety
///
/// `entry` is the address of the program's main shim, a function taking the
/// result slot `out`, which it writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_call_main(entry: *const u8, out: *mut u8) {
    // SAFETY: the native `main` passes the address of `gos_main_shim`, an
    // unwinding function that takes the result slot's address.
    let entry: extern "C-unwind" fn(*mut u8) = unsafe { std::mem::transmute(entry) };
    let outer = MAIN_UNWINDS.with(|flag| flag.replace(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry(out)));
    MAIN_UNWINDS.with(|flag| flag.set(outer));
    if let Err(payload) = result {
        if let Some(fault) = take_deferred_fault(&*payload) {
            reraise_deferred_fault(&fault);
        }
        std::panic::resume_unwind(payload);
    }
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
    // SAFETY: `text` is the owned string the hook answered, its text copied, and not read again.
    unsafe { crate::c_abi::string::gos_rt_str_free(text) };
    out
}

#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_panic(msg: *const c_char) {
    let text = if msg.is_null() {
        "panic".to_string()
    } else {
        // SAFETY: `msg` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `gos_str_arg_string` accepts.
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

/// Installs `isolated` as this thread's fault domain and answers the one it
/// replaces. A worker swaps a goroutine's own value in around each step, so
/// goroutines sharing the worker never see one another's domain.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn swap_isolated_faults(isolated: bool) -> bool {
    ISOLATED_FAULTS.replace(isolated)
}

thread_local! {
    /// Whether a fault on this thread is held for a caller to re-raise.
    ///
    /// A parallel adapter runs leaves on several workers, and the fault it
    /// reports must be the lowest-indexed leaf's whatever order they finish
    /// in. So a leaf's fault neither reports, nor calls the user hook, nor
    /// ends the process: it unwinds to the adapter, which re-raises exactly
    /// one of them once every leaf below it has run.
    static DEFERRED_FAULTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

thread_local! {
    /// The call stack a deferred fault was raised with, rendered where it
    /// happened so the re-raise can report the frames that faulted rather
    /// than the adapter's.
    static DEFERRED_TRACE: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    /// The diagnostic code and report prefix of the deferred fault, so the
    /// re-raise reports it as what it was.
    static DEFERRED_KIND: std::cell::RefCell<(String, String)> =
        const { std::cell::RefCell::new((String::new(), String::new())) };
}

/// A fault a deferred domain held: its message and the call stack it was
/// raised with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredFault {
    /// The fault's message.
    pub text: String,
    /// The call stack rendered where the fault was raised.
    pub trace: String,
    /// The fault's diagnostic code (`GX0005` for a panic).
    pub code: String,
    /// The report's prefix (`panic: ` for a panic).
    pub prefix: String,
}

/// Holds this thread's faults for a caller to re-raise, for the guard's life.
pub struct DeferredFaults(bool);

impl DeferredFaults {
    /// Enters a deferred fault domain on this thread.
    #[must_use]
    pub fn enter() -> Self {
        Self(DEFERRED_FAULTS.replace(true))
    }
}

impl Drop for DeferredFaults {
    fn drop(&mut self) {
        DEFERRED_FAULTS.set(self.0);
    }
}

/// The fault a deferred domain held, when `payload` is one, taking the call
/// stack this thread recorded for it.
#[must_use]
pub fn take_deferred_fault(payload: &(dyn std::any::Any + Send)) -> Option<DeferredFault> {
    let text = with_unwind_notes(&payload.downcast_ref::<GosPanic>()?.0);
    let trace = DEFERRED_TRACE.with(|t| std::mem::take(&mut *t.borrow_mut()));
    let (code, prefix) = DEFERRED_KIND.with(|k| std::mem::take(&mut *k.borrow_mut()));
    let (code, prefix) = if code.is_empty() {
        ("GX0005".to_string(), "panic: ".to_string())
    } else {
        (code, prefix)
    };
    Some(DeferredFault {
        text,
        trace,
        code,
        prefix,
    })
}

/// Raises a fault a deferred domain held, as though it had been raised here,
/// reporting the call stack it was first raised with.
pub fn reraise_deferred_fault(fault: &DeferredFault) -> ! {
    raise_with_trace(
        &fault.code,
        &fault.prefix,
        fault.text.clone(),
        Some(fault.trace.clone()),
    )
}

/// Raises `text` as a fault with the foreign-boundary diagnostic `code`
/// (`GX0013`, `GX0014`), through the same report and isolation a panic takes.
pub(crate) fn raise_foreign_fault(code: &str, text: String) -> ! {
    raise(code, "", text)
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
    raise_with_trace(code, prefix, text, None)
}

/// The call stack a fault report shows: the interpreter's shadow stack, the
/// host's, or the machine stack, whichever this thread has.
fn fault_trace() -> String {
    let trace = crate::sigquit::render_active_panic_trace();
    if !trace.is_empty() {
        return trace;
    }
    let host = host_trace();
    if !host.is_empty() {
        return host;
    }
    crate::sigquit::render_native_panic_trace()
}

/// [`raise`] reporting `trace` in place of this thread's own call stack.
fn raise_with_trace(code: &str, prefix: &str, text: String, trace: Option<String>) -> ! {
    // A static panic message from codegen carries its own line terminator;
    // the report adds one, and two would leave a blank line between the
    // message and the frames below it.
    let text = text.trim_end_matches('\n').to_string();
    install_silent_gos_hook();
    let nested = record_raise(&text);
    // A frame's deferred expressions run before its main-thread fault is
    // reported, so the fault unwinds to `gos_rt_call_main` like a deferred
    // one; the report, the hook, and the exit happen there.
    let main_unwinds = MAIN_UNWINDS.with(std::cell::Cell::get) && !faults_are_isolated();
    if DEFERRED_FAULTS.with(std::cell::Cell::get) || main_unwinds {
        // A deferred expression's panic inside a landing pad is caught there
        // and noted; the held fault keeps the frames and kind it was raised
        // with.
        if !nested {
            let trace = trace.unwrap_or_else(fault_trace);
            DEFERRED_TRACE.with(|t| *t.borrow_mut() = trace);
            DEFERRED_KIND.with(|k| *k.borrow_mut() = (code.to_string(), prefix.to_string()));
        }
        std::panic::panic_any(GosPanic(text));
    }
    if nested {
        std::panic::panic_any(GosPanic(text));
    }
    let hooked = call_user_panic_hook(&text);
    // per-goroutine panic isolation. If the panic originates inside a spawned
    // goroutine, raise a Rust panic the coroutine wrapper catches - the
    // scheduler continues running other goroutines. If we're on the main thread
    // (no active coroutine), a panic in `fn main()` is fatal, just like in Rust.
    if faults_are_isolated() {
        // Stash the message so a `spawn`-created join handle can deliver
        // `Err(message)` from its unwinding Drop-guard before the coroutine
        // wrapper catches and isolates this panic. A deferred expression's
        // panic inside a landing pad becomes a note on that message instead.
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
            gos_rt_flush_stdout();

            eprintln!("error[{code}]: {prefix}{text}");
        }
        std::panic::panic_any(GosPanic(text));
    }
    // Fatal main-goroutine fault: report (with the active call stack), flush
    // buffered stdout (a plain `abort` would drop it), and exit with the pinned
    // panic code 101 - matching Rust; no core is dumped for an ordinary panic.
    // The stack is read before an exit hook runs on it. Everything the program
    // printed belongs ahead of the report, and the exit hooks run before it,
    // so a hook that restores the terminal does so before the report prints.
    let trace = if hooked {
        String::new()
    } else {
        trace.unwrap_or_else(fault_trace)
    };
    gos_rt_flush_stdout();
    crate::c_abi::exit_hooks::run_exit_hooks();
    gos_rt_flush_stdout();
    if !hooked {
        // Match the unified diagnostic-code prefix the VM uses so both
        // execution modes tag a fault with the same code.
        eprintln!("error[{code}]: {prefix}{text}");
        if !trace.is_empty() {
            eprint!("{trace}");
        }
    }
    gos_rt_flush_stdout();

    std::process::exit(101);
}

/// Ends the program with a fault whatever thread reports it, rendered as a
/// fault is on `main`'s thread: `error[<code>]: <prefix><text>`, the call
/// stack when `with_trace` asks for this thread's, and exit code 101.
///
/// For a fault that belongs to the whole program rather than to the work
/// the reporting thread serves, such as a deadlock a scheduler worker
/// notices, or a callback native code runs on a thread outside the
/// program's foreign calls.
pub(crate) fn fatal_program_fault(code: &str, prefix: &str, text: &str, with_trace: bool) -> ! {
    install_silent_gos_hook();
    if call_user_panic_hook(text) {
        crate::c_abi::exit_hooks::run_exit_hooks();
    } else {
        let trace = if with_trace {
            fault_trace()
        } else {
            String::new()
        };
        gos_rt_flush_stdout();
        crate::c_abi::exit_hooks::run_exit_hooks();
        gos_rt_flush_stdout();
        eprintln!("error[{code}]: {prefix}{text}");
        if !trace.is_empty() {
            eprint!("{trace}");
        }
    }
    gos_rt_flush_stdout();

    std::process::exit(101);
}

/// Pushes a call-stack frame on entry to a Gossamer function.
/// Codegen prologues emit one call per function entry; the
/// `function`, `file`, `line`, and `column` identify the frame for panic
/// dumps and SIGQUIT renders.
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
    column: u32,
) {
    // SAFETY: the parameters are program-image string constants, per this
    // shim's contract above.
    let function = unsafe { crate::sigquit::ImageStr::new(function) };
    // SAFETY: as above.
    let file = unsafe { crate::sigquit::ImageStr::new(file) };
    crate::sigquit::stack_push(function, file, line, column);
}

/// Pops the topmost call-stack frame on return from a Gossamer
/// function. Tolerates over-pop (no-op when the stack is empty).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_stack_pop() {
    crate::sigquit::stack_pop();
}

/// Updates the line and column of the topmost call-stack frame.
/// Emitted by codegen at MIR-statement granularity so panic
/// dumps show the position of the most recent statement, not the
/// function entry. The frame's file path stays as it was set by
/// the matching `gos_rt_stack_push`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_stack_set_line(line: u32, column: u32) {
    crate::sigquit::set_active_line(line, column);
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
pub unsafe extern "C-unwind" fn gos_rt_panic_oob(what: *const c_char, idx: i64, len: i64) -> ! {
    let label = if what.is_null() {
        "array index".to_string()
    } else {
        // SAFETY: `what` is a String argument from compiled code, null or a live string body for the whole call.
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
pub unsafe extern "C-unwind" fn gos_rt_panic_vec_index(
    v: *const crate::c_abi::vec::GosVec,
    idx: i64,
) -> ! {
    // SAFETY: `v` is null or a live `GosVec` (this shim's contract).
    let len = unsafe { v.as_ref() }.map_or(0, |vec| vec.len);
    panic_oob_text("vec index", idx, len);
}

/// Panic helper for the one failure block a release body's checks share:
/// `kind` is a [`gossamer_abi::check_fail`] value naming the check, and `v` and
/// `idx` are the vector and index of a failed bounds check. Each kind raises
/// the report its own check raises where it is not shared.
///
/// # Safety
/// `v` must be null or point to a live `GosVec`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_panic_check(
    kind: i64,
    v: *const crate::c_abi::vec::GosVec,
    idx: i64,
) -> ! {
    use gossamer_abi::check_fail;
    let operation = match kind {
        check_fail::ADD => "add",
        check_fail::SUBTRACT => "subtract",
        check_fail::MULTIPLY => "multiply",
        _ => {
            // SAFETY: `v` is null or a live `GosVec` (this shim's contract).
            let len = unsafe { v.as_ref() }.map_or(0, |vec| vec.len);
            panic_oob_text("vec index", idx, len);
        }
    };
    raise(
        "GX0005",
        "panic: ",
        format!("attempt to {operation} with overflow\n"),
    );
}

// ---------------------------------------------------------------
// Exit
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_exit(code: i32) -> ! {
    // signal the netpoller thread to drain its
    // current `poll()` cycle before `std::process::exit` kills it.
    // Without this, in-flight TCP send buffers were terminated by
    // RST (process death) instead of FIN (graceful close). The
    // poller checks the flag at the top of each tick (1 ms ceiling).
    crate::c_abi::exit_hooks::run_exit_hooks();
    crate::sched_global::request_shutdown();
    // Drain the runtime's line-buffered stdout cache before
    // process exit. Without the flush, `println!("...")` followed
    // by `os::exit(N)` produces no output - `std::process::exit`
    // skips the C++/atexit handlers that would normally drain
    // stdio.
    gos_rt_flush_stdout();

    std::process::exit(code);
}

/// Returns the current process ID. Wraps `std::process::id`. The
/// LLVM and cranelift backends call this for `process::id()` in
/// Gossamer source.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_process_id() -> u32 {
    ffi_entry!({ crate::platform::process_id() })
}

/// Aborts the current process without unwinding. Wraps
/// `std::process::abort`. Used by `process::abort()` in Gossamer
/// source. Doesn't flush stdout - abort semantics.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_process_abort() -> ! {
    std::process::abort();
}
