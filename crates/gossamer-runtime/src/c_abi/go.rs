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

// ---------------------------------------------------------------
// Scheduler - every `go fn(args)` lands on the M:N pool
// ---------------------------------------------------------------

fn spawn_task(task: Box<dyn FnOnce() + Send + 'static>) {
    crate::sched_global::spawn(task);
}

// ---------------------------------------------------------------
// Join handles - `spawn(f)` captures the goroutine's outcome
// ---------------------------------------------------------------

/// Heap-boxed result of a spawned goroutine, carried over the
/// one-shot handle channel as a single 8-byte pointer.
/// `ret_words` code for a callable answering a float: one slot, delivered in a
/// floating-point register rather than an integer one.
const RET_WORDS_F64: i64 = 3;

/// `disc` 0 = Ok (`payload` is the returned i64); `disc` 1 = Err
/// (`payload` is a c-string pointer to the panic message).
#[repr(C)]
struct SpawnOutcome {
    disc: i64,
    payload: i64,
}

/// Sends a boxed `SpawnOutcome` over the one-shot handle channel.
fn deliver_outcome(ch_addr: usize, disc: i64, payload: i64) {
    let boxed = Box::new(SpawnOutcome { disc, payload });
    let outcome_ptr = Box::into_raw(boxed) as i64;
    let bytes = outcome_ptr.to_ne_bytes();
    let ch = ch_addr as *mut super::chan::GosChan;
    // SAFETY: `ch` is the live one-shot channel; `bytes` is an 8-byte
    // buffer matching the channel's element width.
    unsafe {
        super::chan::gos_rt_chan_send(ch, bytes.as_ptr());
    }
}

/// The child's share of the one-shot handle channel. Declared first in
/// the spawned body so it drops last: every other guard has already
/// delivered its outcome by the time this releases, whichever path the
/// Releases the child's share of a spawned callable's environment when the
/// goroutine leaves, by any edge. The spawn site takes that share, so the
/// spawning frame's own release and this one are the environment's two
/// owners; a non-capturing callable's environment is reference-counted the
/// same way, and a null one releases as a no-op.
struct ChildEnvRef {
    env: usize,
}

impl Drop for ChildEnvRef {
    fn drop(&mut self) {
        if self.env == 0 {
            return;
        }
        let payload: *mut u8 = std::ptr::with_exposed_provenance_mut(self.env);
        // SAFETY: the spawn site retained this environment for the child, so
        // the share released here is the child's own.
        unsafe { crate::c_abi::rc::gos_rt_rc_release(payload) };
    }
}

/// body left by.
struct ChildChanRef {
    ch_addr: usize,
}

impl Drop for ChildChanRef {
    fn drop(&mut self) {
        // SAFETY: the child held this reference for the whole body, and
        // nothing on the child touches the channel after this point.
        unsafe { super::chan::chan_release(self.ch_addr as *mut super::chan::GosChan) };
    }
}

/// Delivers `Err(message)` to the join handle if the spawned body is
/// unwinding (a panic). On the normal path it is disarmed after the
/// `Ok` outcome is sent.
struct SpawnOutcomeGuard {
    ch_addr: usize,
    armed: bool,
}

impl Drop for SpawnOutcomeGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Unwinding: the goroutine panicked. `gos_rt_panic` stashed the
        // message before raising; read it (falling back to a generic note
        // for a non-`gos_rt_panic` unwind) and hand the joiner an Err. The
        // message is read without clearing it, because the cohort guard
        // drops after this one and reports the same failure. The panic
        // itself continues up to the coroutine wrapper, which isolates the
        // goroutine.
        let msg = super::panic::peek_last_goroutine_panic()
            .unwrap_or_else(|| "spawned goroutine panicked".to_string());
        let cstr = super::string::alloc_cstring(msg.as_bytes());
        deliver_outcome(self.ch_addr, 1, cstr as i64);
    }
}

/// Reports a child's completion to the cohort that owns it, whichever
/// path the body left by. A panicking body reaches the cohort through
/// this guard's `Drop`, which is the only report the unwind path gets.
struct CohortChildGuard {
    cohort: i64,
    index: i64,
    failure: Option<String>,
    /// Set once the body finished without unwinding, so the report can
    /// tell a completed child from a panicking one.
    completed: bool,
    /// Set once the cohort has been told, so the normal path's explicit
    /// report and this guard's `Drop` cannot report twice.
    reported: bool,
}

impl CohortChildGuard {
    /// Reports the child's outcome to its cohort, once.
    fn report(&mut self) {
        if self.cohort == 0 || self.reported {
            return;
        }
        self.reported = true;
        let failure = if self.completed {
            self.failure.take()
        } else {
            // Last observer on this unwind: the outcome guard has already
            // delivered the same message, so this one clears it rather than
            // leaving it to be read by the next goroutine on this worker.
            Some(super::cohort::panic_failure_message(
                super::panic::take_last_goroutine_panic(),
            ))
        };
        super::cohort::leave_child(self.cohort, self.index, failure);
    }
}

impl Drop for CohortChildGuard {
    fn drop(&mut self) {
        self.report();
    }
}

/// How many 8-byte words the spawned callable returns. A `Result` or an
/// `Option` comes back in two registers, so reading it as one word keeps
/// the discriminant and drops the payload.
const SPAWN_RET_ONE_WORD: i64 = 1;

/// Whether the callable returns a `Result`, and what its `Err` payload
/// is. `NONE` means the return is not a `Result` at all, which is what
/// keeps `Option`'s `None` - discriminant 1, exactly like `Err` - from
/// reading as a failed child.
pub const SPAWN_ERR_KIND_NONE: i64 = 0;
pub const SPAWN_ERR_KIND_ERROR: i64 = 1;
pub const SPAWN_ERR_KIND_STRING: i64 = 2;
/// A `Result` whose `Err` payload is neither an `errors::Error` nor a
/// `String`: the failure counts, and its message is the generic one.
pub const SPAWN_ERR_KIND_OTHER: i64 = 3;

/// Copies a two-word `Result` / `Option` into an RC cell and answers the
/// cell pointer, which is how a value wider than one slot is carried in
/// a slot everywhere else in the ABI.
fn box_two_word_value(disc: i64, payload: i64) -> i64 {
    let words = [disc, payload];
    // SAFETY: `words` is a live 16-byte buffer for the length of the
    // call, and a null meta blob names a leaf with no RC children.
    let cell = unsafe {
        super::rc::gos_rt_rc_alloc_copy(16, std::ptr::null(), words.as_ptr().cast::<u8>())
    };
    cell as i64
}

/// Renders a failed child's `Err` payload for the cohort's report.
fn child_error_message(payload: i64, err_kind: i64) -> String {
    if payload == 0 {
        return "cohort child failed".to_string();
    }
    match err_kind {
        SPAWN_ERR_KIND_ERROR => {
            // SAFETY: the callable's static return type named
            // `errors::Error` as its Err payload, so the word is a live
            // `GosError` pointer.
            let rendered = unsafe {
                super::errors::gos_rt_error_display(payload as *const super::errors::GosError)
            };
            if rendered.is_null() {
                return "cohort child failed".to_string();
            }
            // SAFETY: `gos_rt_error_display` answers a runtime-owned
            // Gossamer string. Read through the length header so a
            // message carrying an interior NUL is not truncated.
            let text = unsafe { super::gos_str_arg_string(rendered) };
            unsafe { super::string::gos_rt_str_free(rendered) };
            text
        }
        SPAWN_ERR_KIND_STRING => {
            // SAFETY: the Err payload is a runtime string pointer.
            let bytes = unsafe { super::gos_str_arg_bytes(payload as *const std::os::raw::c_char) };
            String::from_utf8_lossy(bytes).into_owned()
        }
        _ => "cohort child failed".to_string(),
    }
}

/// `spawn(f) -> handle` - runs the callable `code`/`env` pair on the
/// goroutine pool and returns a one-shot channel handle. The outcome
/// (returned value, or panic message) is delivered to `gos_rt_join`.
/// `code` is the per-shape thunk / lifted-closure address; `env` is
/// the closure environment blob (passed as the implicit first arg).
///
/// A panic is NOT caught here: catching across the runtime-call
/// boundary trips the nounwind contract on the `gos_rt_panic` frame
/// and aborts. Instead the panic propagates to the coroutine wrapper
/// (the same path `go` uses to isolate goroutine panics), and a
/// Drop-guard delivers `Err(message)` to the handle as the stack
/// unwinds past it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_spawn(code: usize, env: usize) -> *mut super::chan::GosChan {
    // The one-word form: a callable whose return fits a single slot.
    unsafe {
        gos_rt_spawn_ex(
            code,
            env,
            SPAWN_RET_ONE_WORD,
            SPAWN_ERR_KIND_NONE,
            std::ptr::null(),
        )
    }
}

/// `spawn(f) -> handle`, told how wide the callable's return is.
///
/// `ret_words` is 1 for a value that fits one slot and 2 for a `Result`
/// or `Option`, which comes back in two registers and is copied into an
/// RC cell here so the handle carries it the way every other slot does.
/// `err_kind` names the `Err` payload's shape so a cohort can render a
/// failed child's message. `reason` is the spawn's own `reason:` label, or
/// null when it carried none; the cohort's reports name a labelled child by
/// it rather than only by its spawn index.
///
/// A panic is NOT caught here: catching across the runtime-call
/// boundary trips the nounwind contract on the `gos_rt_panic` frame
/// and aborts. Instead the panic propagates to the coroutine wrapper
/// (the same path `go` uses to isolate goroutine panics), and the
/// Drop-guards deliver `Err(message)` to the handle, and the failure to
/// the cohort, as the stack unwinds past them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_spawn_ex(
    code: usize,
    env: usize,
    ret_words: i64,
    err_kind: i64,
    reason: *const std::os::raw::c_char,
) -> *mut super::chan::GosChan {
    ffi_entry!(std::ptr::null_mut(), {
        if code == 0 {
            return std::ptr::null_mut();
        }
        // One-shot, capacity-1 channel carrying a single SpawnOutcome
        // pointer. Capacity 1 lets the worker deposit the outcome
        // without waiting for the joiner to arrive.
        let ch = unsafe { super::chan::gos_rt_chan_new(8, 1) };
        if ch.is_null() {
            return std::ptr::null_mut();
        }
        // Two parties reach the handle: the value this returns, released
        // by the drop the codegen emits at its last use, and the child,
        // which releases as it leaves.
        // SAFETY: `ch` is the channel just constructed above.
        unsafe { super::chan::chan_retain(&*ch) };
        let ch_addr = ch as usize;
        // The cohort is read on the spawning goroutine, before the task
        // is queued, so the child's slot is reserved in call order and a
        // cohort cannot finish joining while a child of it is still in
        // flight to a worker.
        let cohort = super::cohort::current_cohort();
        let index = if cohort == 0 {
            -1
        } else {
            // The label is read on the spawning goroutine: the caller's
            // string is live here, and the child never reaches it.
            let label = if reason.is_null() {
                String::new()
            } else {
                unsafe { super::gos_str_arg_string(reason) }
            };
            super::cohort::register_child(cohort, label)
        };
        super::cohort::note_child_handle(ch as usize, cohort, index);
        let body = move || {
            // This body is joinable: a panic here is observed through `join()`,
            // so `gos_rt_panic` suppresses its eager report (the guard delivers
            // `Err` instead). The scope restores the flag on the unwind too.
            let _joinable = gossamer_coro::JoinableScope::enter(true);
            super::cohort::enter_child(cohort);
            let _chan_ref = ChildChanRef { ch_addr };
            // The child holds its own share of the environment, taken at the
            // spawn site, and gives it back however it leaves - including on
            // an unwind, which is why the guard drops it rather than a call
            // after the body.
            let _env_ref = ChildEnvRef { env };
            let mut cohort_guard = CohortChildGuard {
                cohort,
                index,
                failure: None,
                completed: false,
                reported: false,
            };
            // Declared after the cohort guard so it drops first on an
            // unwind: the handle carries the outcome before the cohort is
            // told, and telling the cohort is what cancels a fail-fast
            // block - which would otherwise wake a joiner parked on this
            // very handle with a cancellation in place of the outcome.
            let mut guard = SpawnOutcomeGuard {
                ch_addr,
                armed: true,
            };
            // SAFETY: `code` is the callable's entry address; the
            // closure ABI calls it as `fn(env) -> T` with the
            // environment blob as the implicit argument, and `ret_words`
            // reports T's register shape. The `C-unwind` ABI lets a
            // goroutine panic propagate across this call into the
            // Drop-guards above.
            let value = if ret_words == RET_WORDS_F64 {
                // A float answer arrives in a floating-point register, so the
                // call has to be made through a float-returning signature; the
                // bits are what the joiner's carrier holds, symmetric with
                // `gos_rt_result_new_f64`.
                type Fn1F64 = unsafe extern "C-unwind" fn(usize) -> f64;
                let f: Fn1F64 = unsafe { std::mem::transmute(code) };
                unsafe { f(env) }.to_bits() as i64
            } else if ret_words >= 2 {
                type Fn1Wide = unsafe extern "C-unwind" fn(usize) -> i128;
                let f: Fn1Wide = unsafe { std::mem::transmute(code) };
                let wide = unsafe { f(env) };
                let disc = super::vec::result_disc_of(wide);
                let payload = super::vec::result_payload_of(wide);
                if disc == 1 && err_kind != SPAWN_ERR_KIND_NONE {
                    cohort_guard.failure = Some(child_error_message(payload, err_kind));
                }
                box_two_word_value(disc, payload)
            } else {
                type Fn1 = unsafe extern "C-unwind" fn(usize) -> i64;
                let f: Fn1 = unsafe { std::mem::transmute(code) };
                unsafe { f(env) }
            };
            // Normal completion. The outcome reaches the handle before the
            // cohort is told, because a child answering `Err` cancels a
            // fail-fast cohort exactly as a panic does, and that
            // cancellation would reach a joiner parked on this handle. A
            // joiner that wakes first marks the child observed, which
            // `leave_child` picks up when it records the failure.
            guard.armed = false;
            cohort_guard.completed = true;
            deliver_outcome(ch_addr, 0, value);
            cohort_guard.report();
        };
        if cohort != 0 && super::cohort::current_isolation() == super::cohort::ISOLATION_THREAD {
            // An isolated child owns an OS thread for its whole life, so
            // it may block or call into synchronous Rust without
            // stalling anything else. Channels already work from a
            // thread that is not a scheduler goroutine.
            super::cohort::spawn_isolated(Box::new(body));
        } else {
            spawn_task(Box::new(body));
        }
        ch
    })
}

/// `handle.join() -> Result<T, String>` - blocks (parking the caller
/// cooperatively in a goroutine, or condvar-blocking on an OS thread)
/// until the spawned goroutine deposits its outcome, then unpacks it
/// into the 2-word Result aggregate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_join(ch: *mut super::chan::GosChan) -> i128 {
    ffi_entry!(super::vec::pack_result(1, 0), {
        if ch.is_null() {
            return super::vec::pack_result(1, 0);
        }
        let mut buf = [0u8; 8];
        // SAFETY: `buf` is an 8-byte sink matching the channel width.
        let ok = unsafe { super::chan::gos_rt_chan_recv(ch, buf.as_mut_ptr()) };
        if ok == 0 {
            return super::vec::pack_result(1, 0);
        }
        // Joining is how a child's outcome reaches the program, so a
        // failure read here is not one the root cohort reports as
        // unobserved at exit. Marked after the outcome arrives, by which
        // point the child has already recorded the failure.
        super::cohort::mark_handle_observed(ch as usize);
        let outcome_ptr = i64::from_ne_bytes(buf) as *mut SpawnOutcome;
        if outcome_ptr.is_null() {
            return super::vec::pack_result(1, 0);
        }
        // SAFETY: the pointer was produced by `Box::into_raw` in the
        // spawn body; reclaim ownership and free it here.
        let outcome = unsafe { Box::from_raw(outcome_ptr) };
        super::vec::pack_result(outcome.disc, outcome.payload)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_go_yield() {
    ffi_entry!((), {
        // Real coroutine yield - suspend this goroutine and let the
        // worker M run another. The scheduler immediately re-enqueues
        // the suspended goroutine because we don't set the
        // pending-park flag, so this is a "give up the slice"
        // primitive (Go's `runtime.Gosched`). Falls back to an OS
        // yield if called outside a goroutine context.
        if gossamer_coro::in_goroutine() {
            gossamer_coro::suspend();
        } else {
            std::thread::yield_now();
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sleep_ns(ns: i64) {
    ffi_entry!((), {
        if ns <= 0 {
            return;
        }
        let deadline = crate::platform::Instant::now() + std::time::Duration::from_nanos(ns as u64);
        // Park on the netpoller's timer wheel so a sleeping goroutine
        // does not consume a worker slot for the full duration. The
        // worker thread is still parked on a Condvar, but the
        // scheduler's pool grows transparently if multiple goroutines
        // sleep concurrently.
        //
        // Sleeping is also a cancellation point. Registering with the
        // cohort means cancelling unparks the sleeper, which then leaves
        // through the check below instead of finishing its nap; without a
        // cohort the loop parks once and returns at the deadline.
        loop {
            if super::cohort::current_is_cancelled() {
                return;
            }
            if crate::platform::Instant::now() >= deadline {
                return;
            }
            let registered = crate::sched_global::current_gid()
                .map(|gid| (gid, super::cohort::register_waiter(gid)));
            crate::sched_global::sleep_until(deadline);
            if let Some((gid, cohort)) = registered {
                super::cohort::deregister_waiter(cohort, gid);
            }
            if !super::cohort::any_cohort_live() {
                return;
            }
        }
    });
}

/// `time::sleep_ctx(ctx, ms)` - sleeps for `ms` milliseconds unless the
/// context fires first. Returns `1` when the full duration elapsed and `0`
/// when the context cancelled the wait. A cancelled context returns `0`
/// without sleeping.
///
/// # Safety
/// `ctx_handle` is an opaque context handle, or null for an uncancellable
/// sleep.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sleep_ms_ctx(ctx_handle: *const u8, ms: i64) -> i64 {
    ffi_entry!(0, {
        if ms < 0 {
            unsafe {
                crate::c_abi::panic::panic_text(
                    "time::sleep_ctx: duration_ms must be non-negative",
                );
            };
        }
        let addr = ctx_handle as usize;
        let cancelled = || super::context::addr_is_cancelled(addr);
        if cancelled() {
            return 0;
        }
        if addr == 0 {
            unsafe { gos_rt_sleep_ms(ms) };
            return 1;
        }
        let deadline =
            crate::platform::Instant::now() + std::time::Duration::from_millis(ms.max(0) as u64);
        // Cancelling unparks every goroutine registered on the node, so the
        // sleep resumes on whichever of the two arrives first and re-parks
        // for the remainder when it was neither.
        while crate::platform::Instant::now() < deadline {
            if cancelled() {
                return 0;
            }
            if let Some(gid) = crate::sched_global::current_gid() {
                super::context::register_waiter(addr, gid);
            }
            crate::sched_global::sleep_until(deadline);
            if let Some(gid) = crate::sched_global::current_gid() {
                super::context::deregister_waiter(addr, gid);
            }
        }
        i64::from(!cancelled())
    })
}

/// `time::sleep(ms: i64)` - the millisecond-units variant
/// surfaced to Gossamer code. The bytecode VM uses
/// `Duration::from_millis(ms)`; this helper gives the cranelift
/// / LLVM dispatch the same units so `time::sleep(1000)` waits
/// one second across all three tiers. Without it the compiled
/// tier called `gos_rt_sleep_ns(ms)` directly and slept for
/// nanoseconds, busy-spinning every poll loop under
/// `gos build` / `gos build --release` builds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sleep_ms(ms: i64) {
    ffi_entry!((), {
        if ms < 0 {
            unsafe {
                crate::c_abi::panic::panic_text("time::sleep: duration_ms must be non-negative");
            };
        }
        let ns = ms.saturating_mul(1_000_000);
        unsafe { gos_rt_sleep_ns(ns) }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_now_ns() -> i64 {
    ffi_entry!(-1, {
        use std::time::UNIX_EPOCH;
        crate::platform::system_time_now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i64)
    })
}
