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
use std::sync::atomic::Ordering;

use super::*;

// ---------------------------------------------------------------
// Signal notifier table - `os::signal::on` / `Notifier::wait`
// ---------------------------------------------------------------

struct SignalNotifier {
    // Read only by the Windows console-control bridge; on unix
    // delivery is owned by the per-signal relay thread.
    #[cfg_attr(not(windows), allow(dead_code))]
    sig: i32,
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    waiter: std::sync::Arc<SignalWaiter>,
}

#[derive(Default)]
struct SignalWaiter {
    mu: parking_lot::Mutex<()>,
    cv: parking_lot::Condvar,
}

struct SignalRegistry {
    notifiers: parking_lot::Mutex<Vec<Option<SignalNotifier>>>,
}

fn signal_registry() -> &'static SignalRegistry {
    static REGISTRY: std::sync::OnceLock<SignalRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| SignalRegistry {
        notifiers: parking_lot::Mutex::new(Vec::new()),
    })
}

// One relay thread per watched signal: blocks in signal-hook,
// then flips the flag and wakes the condvar for that notifier.
#[cfg(unix)]
fn install_signal_relay(
    sig_raw: i32,
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    waiter: std::sync::Arc<SignalWaiter>,
) {
    use signal_hook::iterator::Signals;
    let Ok(mut signals) = Signals::new([sig_raw]) else {
        return;
    };
    std::thread::Builder::new()
        .name(format!("gos-sig-{sig_raw}"))
        .spawn(move || {
            for _ in signals.forever() {
                flag.store(true, Ordering::Release);
                let _g = waiter.mu.lock();
                waiter.cv.notify_all();
            }
        })
        .ok();
}

// On Windows there is no POSIX signal delivery; the console
// control handler is the closest equivalent. Ctrl-C raises
// CTRL_C_EVENT and Ctrl-Break raises CTRL_BREAK_EVENT - both are
// mapped onto notifiers subscribed to SIGINT (2), SIGTERM (15),
// or SIGBREAK (21) so graceful-shutdown code is portable. The
// handler is installed once on the first `signal::on` call.
#[cfg(windows)]
fn install_console_ctrl_handler() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // SAFETY: `console_ctrl_handler` is a valid `extern "system"`
        // routine and SetConsoleCtrlHandler only stores the pointer.
        unsafe {
            SetConsoleCtrlHandler(Some(console_ctrl_handler), 1);
        }
    });
}

// Fired by the OS on a console control event. Wakes every notifier
// whose subscribed signal is the Windows analogue of the event,
// then returns TRUE so the default terminate-the-process behaviour
// is suppressed (graceful shutdown owns the exit).
#[cfg(windows)]
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    // SIGINT for Ctrl-C; SIGBREAK + SIGTERM for Ctrl-Break.
    let want: &[i32] = match ctrl_type {
        CTRL_C_EVENT => &[2, 15],
        CTRL_BREAK_EVENT => &[21, 15],
        _ => return 0,
    };
    let notifiers = signal_registry().notifiers.lock();
    let mut handled = false;
    for slot in notifiers.iter() {
        if let Some(n) = slot
            && want.contains(&n.sig)
        {
            n.flag.store(true, Ordering::Release);
            let _g = n.waiter.mu.lock();
            n.waiter.cv.notify_all();
            handled = true;
        }
    }
    i32::from(handled)
}

/// `signal::on(sig_raw) -> i64` - registers a notifier for the
/// given raw signal number and returns an opaque handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_signal_on(sig_raw: i32) -> i64 {
    ffi_entry!(-1, {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = std::sync::Arc::new(SignalWaiter::default());
        #[cfg(unix)]
        install_signal_relay(
            sig_raw,
            std::sync::Arc::clone(&flag),
            std::sync::Arc::clone(&waiter),
        );
        // On Windows the console-control bridge maps Ctrl-C /
        // Ctrl-Break onto SIGINT / SIGTERM / SIGBREAK notifiers.
        #[cfg(windows)]
        install_console_ctrl_handler();
        let notifier = SignalNotifier {
            sig: sig_raw,
            flag,
            waiter,
        };
        let mut notifiers = signal_registry().notifiers.lock();
        notifiers.push(Some(notifier));
        i64::try_from(notifiers.len() - 1).unwrap_or(-1)
    })
}

/// `signal::wait(handle)` - blocks until the registered signal fires.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_signal_wait(handle: i64) {
    ffi_entry!((), {
        let notifiers = signal_registry().notifiers.lock();
        let Some(Some(n)) = notifiers.get(handle as usize) else {
            return;
        };
        let flag = std::sync::Arc::clone(&n.flag);
        let waiter = std::sync::Arc::clone(&n.waiter);
        drop(notifiers);
        let mut g = waiter.mu.lock();
        loop {
            if flag.swap(false, Ordering::AcqRel) {
                return;
            }
            waiter.cv.wait(&mut g);
        }
    });
}

/// `signal::try_wait(handle) -> i32` - returns 1 if the signal
/// fired since the last check, 0 otherwise. Non-blocking.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_signal_try_wait(handle: i64) -> i32 {
    ffi_entry!(-1, {
        let notifiers = signal_registry().notifiers.lock();
        let Some(Some(n)) = notifiers.get(handle as usize) else {
            return 0;
        };
        let flag = std::sync::Arc::clone(&n.flag);
        drop(notifiers);
        i32::from(flag.swap(false, Ordering::AcqRel))
    })
}

/// Sorts a flat `[i64; len]` buffer in place using the closure
/// callback at `env`. The env's first word is the closure body
/// address; the body has signature `(env, i64, i64) -> i64`
/// (negative if a < b, positive if a > b, zero if equal),
/// matching `slice::sort_by`'s comparator contract.
///
/// Used by the MIR-side `xs.sort_by(closure)` lowering for fixed-
/// size arrays. The `Vec<T>` case routes through
/// `gos_rt_vec_sort_by_i64` instead. We pass the elements by
/// value (not pointer) because the typechecker today leaves the
/// closure params as plain `i64` rather than `&i64`, so the
/// closure body reads them as direct register values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_sort_by_i64(p: *mut i64, len: i64, env: *const u8) {
    ffi_entry!((), {
        if p.is_null() || len <= 0 || env.is_null() {
            return;
        }
        let len_usize = len.max(0) as usize;
        let buf = unsafe { std::slice::from_raw_parts_mut(p, len_usize) };
        // Closure body sig: (env, i64, i64) -> i64.
        type CmpFn = unsafe extern "C" fn(env: *const u8, a: i64, b: i64) -> i64;
        // env[0] holds the body address (cranelift / LLVM both use
        // this layout for Fn(...)-shaped values).
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return;
        }
        super::fn_registry::verify(fn_addr_raw, super::fn_registry::FnKind::SortCmp);
        let cmp: CmpFn = unsafe { std::mem::transmute(fn_addr_raw) };
        buf.sort_by(|a, b| {
            let r = unsafe { cmp(env, *a, *b) };
            r.cmp(&0)
        });
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_sort_i64(p: *mut i64, len: i64) {
    ffi_entry!((), {
        if p.is_null() || len <= 1 {
            return;
        }
        unsafe { std::slice::from_raw_parts_mut(p, len as usize) }.sort_unstable();
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_sort_str(p: *mut usize, len: i64) {
    ffi_entry!((), {
        if p.is_null() || len <= 1 {
            return;
        }
        let slots = unsafe { std::slice::from_raw_parts_mut(p, len as usize) };
        slots.sort_by(|&a, &b| {
            let a = unsafe { crate::c_abi::gos_str_arg_bytes(a as *const c_char) };
            let b = unsafe { crate::c_abi::gos_str_arg_bytes(b as *const c_char) };
            a.cmp(b)
        });
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_reverse(p: *mut u8, len: i64, elem_bytes: i64) {
    ffi_entry!((), {
        if p.is_null() || len <= 1 || elem_bytes <= 0 {
            return;
        }
        let width = elem_bytes as usize;
        for i in 0..(len as usize / 2) {
            unsafe {
                std::ptr::swap_nonoverlapping(
                    p.add(i * width),
                    p.add((len as usize - 1 - i) * width),
                    width,
                );
            }
        }
    });
}

/// Sorts a word-element `Vec` (heap `GosVec`) in place using the closure
/// callback at `env`. Mirrors [`gos_rt_arr_sort_by_i64`] for the
/// growable-vec receiver shape. Elements move through the header's own
/// `elem_bytes`, so a byte-strided `bool` or `u8` sequence sorts within its
/// own storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_sort_by_i64(v: *mut GosVec, env: *const u8) {
    ffi_entry!((), {
        let Some(mut elems) = (unsafe { sortable_elems(v, env) }) else {
            return;
        };
        let Some(cmp_addr) = (unsafe { sort_cmp_addr(env) }) else {
            return;
        };
        // SAFETY: the address is the closure body the sort lowering stored,
        // whose comparator shape `fn_registry::verify` has just confirmed.
        let cmp: WordCmpFn = unsafe { std::mem::transmute(cmp_addr) };
        elems.sort_by(|a, b| unsafe { cmp(env, *a, *b) }.cmp(&0));
        unsafe { store_elems(v, &elems) };
    });
}

/// Sorts an `f64`-element `Vec` in place. The comparator takes its two
/// elements in SSE registers, which an integer-shaped signature never fills.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_sort_by_f64(v: *mut GosVec, env: *const u8) {
    ffi_entry!((), {
        let Some(mut elems) = (unsafe { sortable_elems(v, env) }) else {
            return;
        };
        let Some(cmp_addr) = (unsafe { sort_cmp_addr(env) }) else {
            return;
        };
        // SAFETY: as in `gos_rt_vec_sort_by_i64`, for the float comparator.
        let cmp: FloatCmpFn = unsafe { std::mem::transmute(cmp_addr) };
        elems.sort_by(|a, b| {
            let (a, b) = (f64::from_bits(*a as u64), f64::from_bits(*b as u64));
            unsafe { cmp(env, a, b) }.cmp(&0)
        });
        unsafe { store_elems(v, &elems) };
    });
}

/// Sorts a fixed `f64` array in place, the array counterpart of
/// [`gos_rt_vec_sort_by_f64`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_sort_by_f64(p: *mut f64, len: i64, env: *const u8) {
    ffi_entry!((), {
        if p.is_null() || len <= 0 {
            return;
        }
        let Some(cmp_addr) = (unsafe { sort_cmp_addr(env) }) else {
            return;
        };
        let buf = unsafe { std::slice::from_raw_parts_mut(p, len as usize) };
        // SAFETY: as in `gos_rt_vec_sort_by_f64`.
        let cmp: FloatCmpFn = unsafe { std::mem::transmute(cmp_addr) };
        buf.sort_by(|a, b| unsafe { cmp(env, *a, *b) }.cmp(&0));
    });
}

type WordCmpFn = unsafe extern "C" fn(env: *const u8, a: i64, b: i64) -> i64;
type FloatCmpFn = unsafe extern "C" fn(env: *const u8, a: f64, b: f64) -> i64;

/// The comparator body address stored at `env[0]`, or `None` when there is
/// no callable to run.
unsafe fn sort_cmp_addr(env: *const u8) -> Option<*const ()> {
    if env.is_null() {
        return None;
    }
    // SAFETY: `env` is a live closure blob whose first word is the callable
    // address (the layout Cranelift and LLVM both build).
    let raw = unsafe { (env as *const usize).read() };
    if raw == 0 {
        return None;
    }
    super::fn_registry::verify(raw, super::fn_registry::FnKind::SortCmp);
    Some(std::ptr::with_exposed_provenance(raw))
}

/// Every element of `v` as a slot word, or `None` when there is nothing to
/// sort.
unsafe fn sortable_elems(v: *const GosVec, env: *const u8) -> Option<Vec<i64>> {
    if v.is_null() || env.is_null() {
        return None;
    }
    // SAFETY: caller supplies a live header.
    let vec = unsafe { &*v };
    if vec.len <= 0 || vec.ptr.is_null() {
        return None;
    }
    Some(
        (0..vec.len)
            .map(|i| unsafe { crate::c_abi::vec::vec_elem_load_i64(vec, i) })
            .collect(),
    )
}

/// Writes `elems` back over `v`'s storage at the header's own stride.
unsafe fn store_elems(v: *mut GosVec, elems: &[i64]) {
    // SAFETY: caller supplies a live header whose length `elems` came from.
    let vec = unsafe { &mut *v };
    for (i, &value) in elems.iter().enumerate() {
        unsafe { crate::c_abi::vec::vec_elem_store_i64(vec, i as i64, value) };
    }
}

/// Sorts a `Vec<i64>` (heap `GosVec`) in ascending order in place.
/// Used by `xs.sort()` on integer vecs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_sort_i64(v: *mut GosVec) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        let vec = unsafe { &mut *v };
        if vec.len <= 0 || vec.ptr.is_null() {
            return;
        }
        let len_usize = vec.len.max(0) as usize;
        // A `Vec<u8>` packs its elements one byte wide; reading those slots
        // as `i64` would order eight elements at a time and write the
        // combined words back over the buffer.
        if vec.elem_bytes == 1 {
            let buf = unsafe { std::slice::from_raw_parts_mut(vec.ptr.as_ptr(), len_usize) };
            buf.sort_unstable();
            return;
        }
        let buf = unsafe { std::slice::from_raw_parts_mut(vec.ptr.cast::<i64>(), len_usize) };
        buf.sort_unstable();
    });
}

/// Sorts a `Vec<String>` (heap `GosVec` whose 8-byte slots hold
/// `*const c_char` element pointers) lexicographically in place by
/// UTF-8 byte order, matching the VM's `xs.sort()`. `xs.sort()` on a
/// string vec routes here instead of the i64 sort, which would order
/// the elements by pointer address rather than by value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_sort_str(v: *mut GosVec) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        let vec = unsafe { &mut *v };
        if vec.len <= 1 || vec.ptr.is_null() {
            return;
        }
        let len_usize = vec.len.max(0) as usize;
        let slots = unsafe { std::slice::from_raw_parts_mut(vec.ptr.cast::<usize>(), len_usize) };
        slots.sort_by(|&a, &b| {
            let sa = unsafe { crate::c_abi::gos_str_arg_bytes(a as *const c_char) };
            let sb = unsafe { crate::c_abi::gos_str_arg_bytes(b as *const c_char) };
            sa.cmp(sb)
        });
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_reverse(v: *mut GosVec) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        let vec = unsafe { &mut *v };
        if vec.ptr.is_null() || vec.len <= 1 || vec.elem_bytes == 0 {
            return;
        }
        unsafe {
            gos_rt_arr_reverse(vec.ptr.as_ptr(), vec.len, i64::from(vec.elem_bytes));
        }
    });
}

/// Sorts a flat `[T; len]` buffer of `elem_bytes`-wide elements in
/// place using the closure callback at `env`. The closure body sig
/// is `(env, *const T, *const T) -> i64` - multi-slot aggregates
/// (Tuple / struct) are passed as pointers because the cranelift /
/// LLVM ABI already routes by-value aggregates that way. Used by
/// `xs.sort_by(closure)` for fixed-size arrays whose element type
/// is not single-slot scalar.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_sort_by_aggr(
    p: *mut u8,
    len: i64,
    elem_bytes: i64,
    env: *const u8,
) {
    ffi_entry!((), {
        if p.is_null() || len <= 0 || elem_bytes <= 0 || env.is_null() {
            return;
        }
        let len_usize = len.max(0) as usize;
        let stride = elem_bytes.max(0) as usize;
        type CmpFn = unsafe extern "C" fn(env: *const u8, a: *const u8, b: *const u8) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return;
        }
        super::fn_registry::verify(fn_addr_raw, super::fn_registry::FnKind::SortCmpAggr);
        let cmp: CmpFn = unsafe { std::mem::transmute(fn_addr_raw) };
        // Indirect sort: rank the indices, then permute the buffer.
        // Sorting indices keeps the comparator pointer-stable across
        // swaps and avoids `unsafe` slice juggling for variable
        // strides that `slice::sort_by` doesn't support natively.
        let mut indices: Vec<usize> = (0..len_usize).collect();
        indices.sort_by(|&ai, &bi| {
            let pa = unsafe { p.add(ai * stride) };
            let pb = unsafe { p.add(bi * stride) };
            let r = unsafe { cmp(env, pa, pb) };
            r.cmp(&0)
        });
        // Permute via a temp buffer rather than in-place cycle
        // following - simpler, still O(n * stride) bytes and one
        // memcpy per element on the way back. Cycle-following would
        // halve peak memory but adds index bookkeeping that doesn't
        // earn its complexity at the sizes the comparator surface
        // sees in practice.
        let total = len_usize.checked_mul(stride).unwrap_or(0);
        let mut tmp: Vec<u8> = vec![0u8; total];
        for (new_idx, &old_idx) in indices.iter().enumerate() {
            unsafe {
                let src = p.add(old_idx * stride);
                let dst = tmp.as_mut_ptr().add(new_idx * stride);
                std::ptr::copy_nonoverlapping(src, dst, stride);
            }
        }
        unsafe {
            std::ptr::copy_nonoverlapping(tmp.as_ptr(), p, total);
        }
    });
}

/// Sorts a `Vec<T>` (heap `GosVec`) of multi-slot aggregate
/// elements in place. Stride comes from `vec.elem_bytes`, so the
/// MIR side doesn't have to thread it through separately.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_sort_by_aggr(v: *mut GosVec, env: *const u8) {
    ffi_entry!((), {
        if v.is_null() || env.is_null() {
            return;
        }
        let vec = unsafe { &mut *v };
        if vec.len <= 0 || vec.ptr.is_null() {
            return;
        }
        unsafe {
            gos_rt_arr_sort_by_aggr(vec.ptr.as_ptr(), vec.len, i64::from(vec.elem_bytes), env);
        }
    });
}

/// Handle table for the ABI 0.4 compiled-tier callback
/// dispatcher Each registration produces a `u64`
/// handle that compiled code can pass across an FFI boundary and
/// later invoke via [`gos_rt_callback_invoke`]. The table is
/// process-global; lookups acquire the mutex briefly to clone the
/// callback reference, then drop the lock before invocation so
/// the callback can register / unregister sibling handles
/// without deadlocking.
#[repr(C)]
struct CallbackEntry {
    /// Caller-supplied context pointer passed unchanged on every
    /// invocation. Typically a pointer to a heap-allocated
    /// closure environment owned by the binding crate.
    ctx: SyncRawPtr<u8>,
    /// C-ABI entry point - receives `(ctx, args, args_len,
    /// result_out)` and returns a status code (0 = ok, non-zero
    /// = caller-defined error).
    invoke: extern "C" fn(*const u8, *const u8, u32, *mut u8) -> i32,
    state: parking_lot::Mutex<CallbackState>,
    idle: parking_lot::Condvar,
}

#[derive(Default)]
struct CallbackState {
    closing: bool,
    active: usize,
}

static CALLBACK_TABLE: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<u64, std::sync::Arc<CallbackEntry>>>,
> = std::sync::OnceLock::new();
static NEXT_CALLBACK_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

thread_local! {
    // A callback may unregister itself to prevent future calls. It cannot wait
    // for its own in-flight count without deadlocking, so that case only
    // closes the handle; the binding must defer freeing `ctx` until return.
    static ACTIVE_CALLBACKS: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn callback_table()
-> &'static parking_lot::Mutex<std::collections::HashMap<u64, std::sync::Arc<CallbackEntry>>> {
    CALLBACK_TABLE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Registers a callback in the process-global handle table.
/// Returns the assigned handle (non-zero on success; 0 reserved
/// for "no callback"). The caller is responsible for
/// [`gos_rt_callback_unregister`]ing when the closure's lifetime
/// ends - `BindingCallback`'s `Drop` impl handles this for
/// bindings that use the ABI 0.4 surface.
#[allow(unsafe_code, reason = "no_mangle FFI entry; raw fn pointer + ctx")]
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_callback_register(
    ctx: *const u8,
    invoke: extern "C" fn(*const u8, *const u8, u32, *mut u8) -> i32,
) -> u64 {
    ffi_entry!(0, {
        let handle = NEXT_CALLBACK_HANDLE.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        callback_table().lock().insert(
            handle,
            std::sync::Arc::new(CallbackEntry {
                ctx: SyncRawPtr::new(ctx.cast_mut()),
                invoke,
                state: parking_lot::Mutex::new(CallbackState::default()),
                idle: parking_lot::Condvar::new(),
            }),
        );
        handle
    })
}

/// Removes a callback from the handle table. Idempotent on
/// unknown handles. After this call returns, no invocation that began before
/// removal is still executing, so the binding may release its context safely.
/// A callback may unregister itself to prevent future calls, but must defer
/// freeing its own context until the invocation returns.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_callback_unregister(handle: u64) {
    ffi_entry!((), {
        if handle == 0 {
            return;
        }
        let Some(entry) = callback_table().lock().remove(&handle) else {
            return;
        };
        let mut state = entry.state.lock();
        state.closing = true;
        if ACTIVE_CALLBACKS.with(|active| active.borrow().contains(&handle)) {
            return;
        }
        while state.active != 0 {
            entry.idle.wait(&mut state);
        }
    });
}

/// Invokes the callback registered under `handle`. Returns the
/// status code from the callback (0 = ok, non-zero = error), or
/// `-1` when the handle is unknown.
///
/// The handle table mutex is released before the callback runs,
/// so the callback can register / unregister sibling handles
/// without deadlocking. `result_out` is zero-filled before
/// invocation so a callback that returns an error sentinel
/// (without touching the slot) leaves the caller observing zero
/// bytes instead of garbage.
#[allow(unsafe_code, reason = "no_mangle FFI entry; invokes raw fn pointer")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_callback_invoke(
    handle: u64,
    args: *const u8,
    args_len: u32,
    result_out: *mut u8,
) -> i32 {
    ffi_entry!(-1, {
        if handle == 0 {
            return -1;
        }
        // Best-effort zero of the first 16 bytes of result_out so an
        // error-path return doesn't leave stack garbage observable.
        if !result_out.is_null() {
            // SAFETY: caller declares result_out as a write-only
            // slot per the ABI. 16 bytes is the documented minimum.
            unsafe { std::ptr::write_bytes(result_out, 0, 16) };
        }
        // Clone the Arc so table removal can happen without holding its mutex
        // during user code. Mark the invocation in-flight before releasing the
        // entry state lock; unregister waits for that count to reach zero
        // before it lets a binding free `ctx`.
        let entry = {
            let table = callback_table().lock();
            table.get(&handle).cloned()
        };
        let Some(entry) = entry else { return -1 };
        {
            let mut state = entry.state.lock();
            if state.closing {
                return -1;
            }
            state.active += 1;
        }
        ACTIVE_CALLBACKS.with(|active| active.borrow_mut().push(handle));
        let result = (entry.invoke)(entry.ctx.as_const_ptr(), args, args_len, result_out);
        ACTIVE_CALLBACKS.with(|active| {
            let popped = active.borrow_mut().pop();
            debug_assert_eq!(popped, Some(handle));
        });
        let mut state = entry.state.lock();
        state.active -= 1;
        if state.active == 0 {
            entry.idle.notify_all();
        }
        result
    })
}

/// A heap-allocated iterator over a `GosVec`. Created by
/// `gos_rt_arr_iter`; advanced one element at a time by
/// `gos_rt_arr_iter_next`.
#[repr(C)]
pub struct GosArrIter {
    /// Pointer to the vec being iterated. The caller must keep the
    /// vec alive for the iterator's lifetime.
    pub vec: SyncRawPtr<GosVec>,
    /// Next element index to yield.
    pub idx: i64,
}

/// Creates an iterator over `vec`, starting at index 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_iter(vec: *mut GosVec) -> *mut GosArrIter {
    ffi_entry!(std::ptr::null_mut(), {
        Box::into_raw(Box::new(GosArrIter {
            vec: SyncRawPtr::new(vec),
            idx: 0,
        }))
    })
}

/// Advances the iterator by one and returns `GosResult { disc=0,
/// payload=element }` (Some) or `GosResult { disc=1, payload=0 }`
/// (None) when exhausted. Reads 8-byte-wide element slots only;
/// callers with other element widths must use a lower-level helper.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_iter_next(iter: *mut GosArrIter) -> i128 {
    ffi_entry!(0i128, {
        if iter.is_null() {
            return gos_rt_result_new(1, 0);
        }
        let iter_ref = unsafe { &mut *iter };
        if iter_ref.vec.is_null() {
            return gos_rt_result_new(1, 0);
        }
        let vec_ref = unsafe { &*iter_ref.vec.as_ptr() };
        if iter_ref.idx >= vec_ref.len {
            return gos_rt_result_new(1, 0);
        }
        let value = unsafe { gos_rt_vec_get_i64(iter_ref.vec.as_ptr(), iter_ref.idx) };
        iter_ref.idx += 1;
        gos_rt_result_new(0, value)
    })
}

/// Frees a `GosArrIter` allocated by [`gos_rt_arr_iter`]. Does NOT
/// free the underlying vec - the vec is owned by the original local.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_iter_free(iter: *mut GosArrIter) {
    ffi_entry!((), {
        if iter.is_null() {
            return;
        }
        drop(unsafe { Box::from_raw(iter) });
    });
}

/// Reads an `i64`-shaped element from a `Vec` (or any
/// 8-byte-elem `GosVec`) by index. Invalid scalar indexing is a
/// bounds panic, matching fixed-array and aggregate Vec indexing on
/// every execution tier. Use an explicit `get`-style API where a
/// non-panicking probe is intended.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_get_i64(v: *const GosVec, idx: i64) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() {
            crate::c_abi::panic::panic_oob_text("vec index", idx, 0);
        }
        let vec = unsafe { &*v };
        if idx < 0 || idx >= vec.len {
            crate::c_abi::panic::panic_oob_text("vec index", idx, vec.len);
        }
        unsafe { crate::c_abi::vec::vec_elem_load_i64(vec, idx) }
    })
}

/// Reads an `i64`-shaped element from a `Vec` WITHOUT the null/bounds
/// guard of [`gos_rt_vec_get_i64`]. Emitted only by the counted-loop
/// element read, where the index is proven in `[0, len)` against this
/// same vec and the receiver is non-null. The LLVM tier inlines this
/// branch-free; the symbol exists so that AOT declare/link resolves.
///
/// # Safety
/// `v` must be a non-null `GosVec` and `idx` in `[0, v.len)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_get_i64_unchecked(v: *const GosVec, idx: i64) -> i64 {
    ffi_entry!(-1, {
        let vec = unsafe { &*v };
        unsafe { crate::c_abi::vec::vec_elem_load_i64(vec, idx) }
    })
}

/// Writes an `i64`-shaped element to a `Vec` at `idx`. Invalid scalar
/// indexing is a bounds panic; it is never silently ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_set_i64(v: *mut GosVec, idx: i64, value: i64) {
    ffi_entry!((), {
        if v.is_null() {
            crate::c_abi::panic::panic_oob_text("vec index", idx, 0);
        }
        let vec = unsafe { &mut *v };
        if idx < 0 || idx >= vec.len {
            crate::c_abi::panic::panic_oob_text("vec index", idx, vec.len);
        }
        unsafe { crate::c_abi::vec::vec_release_owned_elem(vec, idx, value) };
        unsafe { crate::c_abi::vec::vec_elem_store_i64(vec, idx, value) };
    });
}

/// Writes an `i64`-shaped element to a `Vec` at `idx` WITHOUT the
/// null/bounds guard of [`gos_rt_vec_set_i64`]. Emitted only by the
/// counted-loop bounds-check elision, where the index is proven in
/// `[0, len)` against this same vec and the receiver is non-null. The
/// LLVM tier inlines this branch-free; the symbol exists so AOT
/// declare/link resolves.
///
/// # Safety
/// `v` must be a non-null `GosVec` and `idx` in `[0, v.len)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_set_i64_unchecked(v: *mut GosVec, idx: i64, value: i64) {
    ffi_entry!((), {
        let vec = unsafe { &mut *v };
        unsafe { crate::c_abi::vec::vec_release_owned_elem(vec, idx, value) };
        unsafe { crate::c_abi::vec::vec_elem_store_i64(vec, idx, value) };
    });
}

/// Swaps two Vec elements. No-op for null receivers or out-of-range
/// indices, matching the old MIR expansion through get plus set calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_swap_i64(v: *mut GosVec, i: i64, j: i64) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        let vec = unsafe { &mut *v };
        if i < 0 || i >= vec.len || j < 0 || j >= vec.len || i == j {
            return;
        }
        let elem_bytes = vec.elem_bytes as usize;
        if elem_bytes == 0 {
            return;
        }
        crate::c_abi::vec::bump_vec_mutation_generation(vec);
        let i_addr = unsafe { vec.ptr.add((i as usize) * elem_bytes) };
        let j_addr = unsafe { vec.ptr.add((j as usize) * elem_bytes) };
        let mut tmp = vec![0u8; elem_bytes];
        unsafe {
            std::ptr::copy_nonoverlapping(i_addr, tmp.as_mut_ptr(), elem_bytes);
            std::ptr::copy_nonoverlapping(j_addr, i_addr, elem_bytes);
            std::ptr::copy_nonoverlapping(tmp.as_ptr(), j_addr, elem_bytes);
        }
    });
}

/// Swaps two `Vec` elements. `swap` is an indexed write, so an index
/// outside `[0, len)` is a bounds panic on every tier, matching `xs[i] = v`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_swap_safe(v: *mut GosVec, i: i64, j: i64) {
    ffi_entry!((), {
        if v.is_null() {
            crate::c_abi::panic::panic_oob_text("vec swap index", i, 0);
        }
        let len = unsafe { (*v).len };
        if i < 0 || i >= len {
            crate::c_abi::panic::panic_oob_text("vec swap index", i, len);
        }
        if j < 0 || j >= len {
            crate::c_abi::panic::panic_oob_text("vec swap index", j, len);
        }
        unsafe { gos_rt_vec_swap_i64(v, i, j) };
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_get_ptr(v: *const GosVec, idx: i64) -> *mut u8 {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return std::ptr::null_mut();
        }
        let len = unsafe { (*v).len };
        if idx < 0 || idx >= len {
            return std::ptr::null_mut();
        }
        let elem_kind = unsafe { (*v).elem_kind };
        if elem_kind == crate::c_abi::vec::vec_elem_kind::PACKED_ROWS {
            return unsafe { crate::c_abi::vec::packed_row_at(v, idx) };
        }
        if elem_kind == crate::c_abi::vec::vec_elem_kind::VEC
            && unsafe { crate::c_abi::vec::try_pack_primitive_rows(v.cast_mut()) }
        {
            return unsafe { crate::c_abi::vec::packed_row_at(v, idx) };
        }
        let ptr = unsafe { (*v).ptr };
        let elem_bytes = unsafe { (*v).elem_bytes };
        unsafe { ptr.add((idx as usize) * (elem_bytes as usize)) }
    })
}

/// Removes the last element of `v` and writes its bytes to
/// `out`. Returns 1 on success, 0 if the vec was empty. `out`
/// must be sized for `v.elem_bytes`.
/// `vec[lo..hi]` - copies the subrange `[lo, hi)` of `v`'s
/// elements into a fresh `GosVec` and returns a pointer to it.
/// Out-of-range bounds are clamped. Element bytes are copied
/// directly (the i64-erased ABI matches the rest of the Vec
/// surface).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_slice(v: *const GosVec, lo: i64, hi: i64) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let src = unsafe { &*v };
        let elem_bytes = src.elem_bytes;
        let len = src.len;
        let lo = lo.max(0).min(len);
        let hi = hi.max(lo).min(len);
        let count = hi - lo;
        let out = unsafe { gos_rt_vec_with_capacity(elem_bytes, count) };
        // Propagate the guarded-aggregate tag before the pushes so each
        // copied element retains its copy-blob children.
        if !out.is_null() && src.elem_kind == crate::c_abi::vec::vec_elem_kind::AGGR_GUARDED {
            let meta = crate::c_abi::vec::vec_elem_meta(v);
            if !meta.is_null() {
                unsafe { crate::c_abi::vec::gos_rt_vec_set_elem_meta(out, meta) };
            }
        }
        if !out.is_null() && count > 0 {
            for i in 0..count {
                unsafe {
                    let src_ptr = src.ptr.add(((lo + i) as usize) * (elem_bytes as usize));
                    gos_rt_vec_push(out, src_ptr);
                }
            }
        }
        // STRING / VEC / AGGR_OWNED elements: the slice now shares the
        // copied slots' heap children with the source; re-tag and retain
        // so both vecs' deep-frees are balanced.
        unsafe { crate::c_abi::vec::vec_share_owned_elements(v, out) };
        out
    })
}

// --- 0.7.0 Vec method surface ---

/// `xs.first() -> Option<T>` packed as `*mut GosResult`. For
/// `Vec<i64>` / `Vec<*c_char>` / `Vec<f64>` (any 8-byte element)
/// the payload is the raw 8 bytes of element 0 cast to i64.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_first(v: *const GosVec) -> i128 {
    ffi_entry!(0i128, {
        if v.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let vec = unsafe { &*v };
        if vec.len <= 0 {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let value = unsafe { crate::c_abi::vec::vec_elem_shared_payload_word(vec, 0) };
        unsafe { gos_rt_result_new(0, value) }
    })
}

/// `xs.last() -> Option<T>` - sibling of `first`. Out-of-range on
/// an empty Vec returns None.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_last(v: *const GosVec) -> i128 {
    ffi_entry!(0i128, {
        if v.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let vec = unsafe { &*v };
        if vec.len <= 0 {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let value = unsafe { crate::c_abi::vec::vec_elem_shared_payload_word(vec, vec.len - 1) };
        unsafe { gos_rt_result_new(0, value) }
    })
}

/// `xs.get(i) -> Option<T>`. Out-of-range and negative indices return None.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_get_opt(v: *const GosVec, idx: i64) -> i128 {
    ffi_entry!(0i128, {
        if v.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let vec = unsafe { &*v };
        if idx < 0 || idx >= vec.len {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let value = unsafe { crate::c_abi::vec::vec_elem_shared_payload_word(vec, idx) };
        unsafe { gos_rt_result_new(0, value) }
    })
}

/// `xs.rev() -> Vec<T>` - fresh Vec with the same elements in
/// reverse order. Element bytes are copied through the i64-erased
/// ABI matching the rest of the Vec surface.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_reversed(v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let src = unsafe { &*v };
        let elem_bytes = src.elem_bytes;
        let len = src.len;
        let out = unsafe { gos_rt_vec_with_capacity(elem_bytes, len) };
        if !out.is_null() && len > 0 {
            for i in 0..len {
                let from_idx = len - 1 - i;
                let src_ptr = unsafe { src.ptr.add((from_idx as usize) * (elem_bytes as usize)) };
                unsafe { gos_rt_vec_push(out, src_ptr) };
            }
        }
        // Same sharing contract as `gos_rt_vec_slice`: the rev copy
        // owns its own share of every element's heap children.
        unsafe { crate::c_abi::vec::vec_share_owned_elements(v, out) };
        out
    })
}

/// `xs.step_by(step) -> [T]`: every `step`-th element starting at
/// index 0, as a fresh Vec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_step_by(v: *const GosVec, step: i64) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if step <= 0 {
            crate::c_abi::panic::panic_text("Vec::step_by: count must be positive");
        }
        if v.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let src = unsafe { &*v };
        let out = unsafe { gos_rt_vec_with_capacity(src.elem_bytes, src.len / step + 1) };
        if !out.is_null() {
            let mut i = 0;
            while i < src.len {
                let src_ptr = unsafe { src.ptr.add((i as usize) * (src.elem_bytes as usize)) };
                unsafe { gos_rt_vec_push(out, src_ptr) };
                i += step;
            }
        }
        // Same sharing contract as `gos_rt_vec_slice`: the stepped copy
        // owns its own share of every element's heap children.
        unsafe { crate::c_abi::vec::vec_share_owned_elements(v, out) };
        out
    })
}

/// `xs.take(n) -> [T]`: the first `n` elements (clamped to the source
/// length) as a fresh Vec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_take(v: *const GosVec, n: i64) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if n < 0 {
            crate::c_abi::panic::panic_text("Vec::take: count must be non-negative");
        }
        if v.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let src = unsafe { &*v };
        let count = n.min(src.len);
        let out = unsafe { gos_rt_vec_with_capacity(src.elem_bytes, count.max(1)) };
        if !out.is_null() {
            for i in 0..count {
                let src_ptr = unsafe { src.ptr.add((i as usize) * (src.elem_bytes as usize)) };
                unsafe { gos_rt_vec_push(out, src_ptr) };
            }
        }
        // Same sharing contract as `gos_rt_vec_slice`: the taken copy
        // owns its own share of every element's heap children.
        unsafe { crate::c_abi::vec::vec_share_owned_elements(v, out) };
        out
    })
}

/// `xs.skip(n)` - a fresh Vec of the elements past the first `n`, clamped to
/// `[0, len]`. The copy keeps the source's element width and takes its own
/// share of every element's heap children, exactly as `take` does.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_skip(v: *const GosVec, n: i64) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if n < 0 {
            crate::c_abi::panic::panic_text("Vec::skip: count must be non-negative");
        }
        if v.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let src = unsafe { &*v };
        let start = n.min(src.len);
        let count = src.len - start;
        let out = unsafe { gos_rt_vec_with_capacity(src.elem_bytes, count.max(1)) };
        if !out.is_null() {
            for i in start..src.len {
                let src_ptr = unsafe { src.ptr.add((i as usize) * (src.elem_bytes as usize)) };
                unsafe { gos_rt_vec_push(out, src_ptr) };
            }
        }
        unsafe { crate::c_abi::vec::vec_share_owned_elements(v, out) };
        out
    })
}

/// `xs.index_of(&needle) -> Option<i64>` for an i64-shaped Vec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_index_of_i64(v: *const GosVec, needle: i64) -> i128 {
    ffi_entry!(0i128, {
        if v.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let vec = unsafe { &*v };
        for i in 0..vec.len {
            let elem = unsafe { crate::c_abi::vec::vec_elem_load_i64(vec, i) };
            if elem == needle {
                return unsafe { gos_rt_result_new(0, i) };
            }
        }
        unsafe { gos_rt_result_new(1, 0) }
    })
}

/// `xs.index_of(&needle) -> Option<i64>` for a Vec of c-string pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_index_of_str(v: *const GosVec, needle: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if v.is_null() || needle.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let vec = unsafe { &*v };
        for i in 0..vec.len {
            let p = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let elem = unsafe {
                std::ptr::with_exposed_provenance::<c_char>((p as *const usize).read_unaligned())
            };
            if !elem.is_null()
                && unsafe {
                    crate::c_abi::gos_str_arg_bytes(elem) == crate::c_abi::gos_str_arg_bytes(needle)
                }
            {
                return unsafe { gos_rt_result_new(0, i) };
            }
        }
        unsafe { gos_rt_result_new(1, 0) }
    })
}

/// `xs.count_of(&needle) -> i64` for an i64-shaped Vec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_count_of_i64(v: *const GosVec, needle: i64) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() {
            return 0;
        }
        let vec = unsafe { &*v };
        let mut count: i64 = 0;
        for i in 0..vec.len {
            let elem = unsafe { crate::c_abi::vec::vec_elem_load_i64(vec, i) };
            if elem == needle {
                count += 1;
            }
        }
        count
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_count_of_str(v: *const GosVec, needle: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() || needle.is_null() {
            return 0;
        }
        let vec = unsafe { &*v };
        let mut count: i64 = 0;
        for i in 0..vec.len {
            let p = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let elem = unsafe {
                std::ptr::with_exposed_provenance::<c_char>((p as *const usize).read_unaligned())
            };
            if !elem.is_null()
                && unsafe {
                    crate::c_abi::gos_str_arg_bytes(elem) == crate::c_abi::gos_str_arg_bytes(needle)
                }
            {
                count += 1;
            }
        }
        count
    })
}

/// `xs.contains(&needle) -> bool` for an i64-shaped Vec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_contains_i64(v: *const GosVec, needle: i64) -> i8 {
    ffi_entry!(0, {
        if v.is_null() {
            return 0;
        }
        let vec = unsafe { &*v };
        for i in 0..vec.len {
            let elem = unsafe { crate::c_abi::vec::vec_elem_load_i64(vec, i) };
            if elem == needle {
                return 1;
            }
        }
        0
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_contains_str(v: *const GosVec, needle: *const c_char) -> i8 {
    ffi_entry!(0, {
        if v.is_null() || needle.is_null() {
            return 0;
        }
        let vec = unsafe { &*v };
        for i in 0..vec.len {
            let p = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let elem = unsafe {
                std::ptr::with_exposed_provenance::<c_char>((p as *const usize).read_unaligned())
            };
            if !elem.is_null()
                && unsafe {
                    crate::c_abi::gos_str_arg_bytes(elem) == crate::c_abi::gos_str_arg_bytes(needle)
                }
            {
                return 1;
            }
        }
        0
    })
}

/// `xs.slice(start, end) -> Result<Vec<T>, errors::Error>`.
/// Safe sub-range slicer. Inverted or out-of-range bounds return
/// `Err(errors::Error)`; valid bounds return `Ok(Vec<T>)` with the
/// elements byte-copied into a fresh Vec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_slice_result(v: *const GosVec, start: i64, end: i64) -> i128 {
    ffi_entry!(0i128, {
        let len = if v.is_null() { 0 } else { unsafe { (*v).len } };
        if start < 0 || end < 0 || start > end || end > len {
            let msg = format!("slice: range [{start}, {end}) out of bounds for length {len}");
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let out = unsafe { gos_rt_vec_slice(v, start, end) };
        unsafe { gos_rt_result_new(0, out as i64) }
    })
}

/// `xs.slice(start, end) -> Result<Vec<i64>, errors::Error>` for
/// fixed-size i64 array receivers. The buffer is the raw inline
/// `[i64; N]` storage (no length prefix); MIR splices the
/// statically-known `len` from `TyKind::Array { len }` as the
/// second argument. Routed from MIR dispatch when the receiver
/// type is `[i64; N]` / `&[i64]`. Returns the same
/// `Result<Vec<i64>, errors::Error>` shape as
/// [`gos_rt_vec_slice_result`] so callers don't need a separate
/// match arm.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_intarr_slice_result(
    p: *const i64,
    len: i64,
    start: i64,
    end: i64,
) -> i128 {
    ffi_entry!(0i128, {
        if p.is_null() || start < 0 || end < 0 || start > end || end > len {
            let msg = format!("slice: range [{start}, {end}) out of bounds for length {len}");
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let count = end - start;
        let out = unsafe { gos_rt_vec_with_capacity(8, count) };
        if !out.is_null() && count > 0 {
            for i in 0..count {
                let element_index = (start + i) as usize;
                unsafe {
                    let src_ptr = p.add(element_index) as *const u8;
                    gos_rt_vec_push(out, src_ptr);
                }
            }
        }
        unsafe { gos_rt_result_new(0, out as i64) }
    })
}

/// `xs.slice(start, end) -> Result<Vec<u8>, errors::Error>` for fixed-size
/// `[u8; N]` array receivers. Identical to [`gos_rt_intarr_slice_result`]
/// except the result is byte-packed (stride 1) instead of one 8-byte word per
/// element: a kept `[u8]` slice costs one byte per byte like Go's `[]byte`,
/// not 8x. The inline `[u8; N]` source still stores each element in an 8-byte
/// slot, so each push copies the low byte (`src_ptr` points at the element's
/// first byte; little-endian targets). The result's header carries
/// `elem_bytes == 1`, and every `Vec<u8>` reader takes the header-driven byte
/// path, so the narrower result round-trips identically.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytearr_slice_result(
    p: *const i64,
    len: i64,
    start: i64,
    end: i64,
) -> i128 {
    ffi_entry!(0i128, {
        if p.is_null() || start < 0 || end < 0 || start > end || end > len {
            let msg = format!("slice: range [{start}, {end}) out of bounds for length {len}");
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let count = end - start;
        let out = unsafe { gos_rt_vec_with_capacity(1, count) };
        if !out.is_null() && count > 0 {
            let dst = unsafe { (*out).ptr.as_ptr() };
            for i in 0..(count as usize) {
                unsafe {
                    *dst.add(i) = *p.add(start as usize + i) as u8;
                }
            }
            unsafe { (*out).len = count };
        }
        unsafe { gos_rt_result_new(0, out as i64) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_packed_bytearr_slice_result(
    p: *const u8,
    len: i64,
    start: i64,
    end: i64,
) -> i128 {
    ffi_entry!(0i128, {
        if p.is_null() || start < 0 || end < 0 || start > end || end > len {
            let msg = format!("slice: range [{start}, {end}) out of bounds for length {len}");
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let count = end - start;
        let out = unsafe { gos_rt_vec_with_capacity(1, count) };
        if !out.is_null() && count > 0 {
            let dst = unsafe { (*out).ptr.as_ptr() };
            unsafe {
                std::ptr::copy_nonoverlapping(p.add(start as usize), dst, count as usize);
                (*out).len = count;
            }
        }
        unsafe { gos_rt_result_new(0, out as i64) }
    })
}

/// `xs.slice(start, end) -> Result<Vec<f64>, errors::Error>` for
/// fixed-size f64 array receivers. Same layout contract as
/// [`gos_rt_intarr_slice_result`] - raw inline buffer plus a
/// statically-known length spliced by the MIR dispatcher.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_floatarr_slice_result(
    p: *const i64,
    len: i64,
    start: i64,
    end: i64,
) -> i128 {
    ffi_entry!(0i128, {
        if p.is_null() || start < 0 || end < 0 || start > end || end > len {
            let msg = format!("slice: range [{start}, {end}) out of bounds for length {len}");
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let count = end - start;
        let out = unsafe { gos_rt_vec_with_capacity(8, count) };
        if !out.is_null() && count > 0 {
            for i in 0..count {
                let element_index = (start + i) as usize;
                unsafe {
                    let src_ptr = p.add(element_index) as *const u8;
                    gos_rt_vec_push(out, src_ptr);
                }
            }
        }
        unsafe { gos_rt_result_new(0, out as i64) }
    })
}

/// Bounds-checked in-place insert returning `Result<(), errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_insert_safe(v: *mut GosVec, idx: i64, value: i64) -> i128 {
    ffi_entry!(0i128, {
        let len = if v.is_null() { 0 } else { unsafe { (*v).len } };
        if idx < 0 || idx > len {
            let msg = format!("insert: index {idx} out of bounds for length {len}");
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        unsafe { gos_rt_vec_insert_at(v, idx, value) };
        unsafe { gos_rt_result_new(0, 0) }
    })
}

/// In-place insert at `idx`, shifting the tail up one slot and panicking on an
/// invalid index. `value` is the raw 8-byte payload used by the erased Vec ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_insert_at(v: *mut GosVec, idx: i64, value: i64) {
    ffi_entry!((), {
        let len = if v.is_null() { 0 } else { unsafe { (*v).len } };
        if v.is_null() || idx < 0 || idx > len {
            let msg = format!("insert: index {idx} out of bounds for length {len}");
            crate::c_abi::panic::panic_text(&msg);
            return;
        }
        // Grow by one (handling region/global reallocation) with the new
        // element parked at the tail, then rotate it down to `idx`.
        let val_ptr = std::ptr::addr_of!(value).cast::<u8>();
        unsafe { gos_rt_vec_push(v, val_ptr) };
        let vec = unsafe { &mut *v };
        let stride = vec.elem_bytes as usize;
        if !vec.ptr.is_null() && stride > 0 && idx < len {
            let base = vec.ptr.as_ptr();
            let span = ((len - idx + 1) as usize) * stride;
            let region =
                unsafe { std::slice::from_raw_parts_mut(base.add(idx as usize * stride), span) };
            region.rotate_right(stride);
        }
    });
}

/// Bounds-checked in-place insert of an element the caller addresses in
/// place, returning `Result<(), errors::Error>`.
///
/// Companion to [`gos_rt_vec_insert_safe`], which takes the element as the
/// word itself. A struct, tuple, or array element is a flat slot block the
/// caller holds in its own frame - exactly the shape `gos_rt_vec_push` takes
/// and `remove` hands back - so `slots` is that block's address, and reading
/// the parameter instead would store the pointer and whatever sits beside it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_insert_slots_safe(
    v: *mut GosVec,
    idx: i64,
    slots: *const u8,
) -> i128 {
    ffi_entry!(0i128, {
        let len = if v.is_null() { 0 } else { unsafe { (*v).len } };
        if idx < 0 || idx > len {
            let msg = format!("insert: index {idx} out of bounds for length {len}");
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        if !v.is_null() && !slots.is_null() {
            unsafe { vec_insert_payload_at(v, idx, slots) };
        }
        unsafe { gos_rt_result_new(0, 0) }
    })
}

/// Shared body of the two insert entry points: grow by one with the new
/// element parked at the tail, then rotate it down to `idx`.
unsafe fn vec_insert_payload_at(v: *mut GosVec, idx: i64, payload: *const u8) {
    let len = unsafe { (*v).len };
    unsafe { gos_rt_vec_push(v, payload) };
    let vec = unsafe { &mut *v };
    let stride = vec.elem_bytes as usize;
    if !vec.ptr.is_null() && stride > 0 && idx < len {
        let base = vec.ptr.as_ptr();
        let span = ((len - idx + 1) as usize) * stride;
        let region =
            unsafe { std::slice::from_raw_parts_mut(base.add(idx as usize * stride), span) };
        region.rotate_right(stride);
    }
}

/// The payload word for the element `remove` hands back.
///
/// A word-wide element is the value itself. A wider one is a flat slot block
/// the caller addresses in place, so it is copied out first: the shift that
/// closes the gap writes over the slot it was read from, and the payload has
/// to outlive that.
unsafe fn removed_elem_payload(vec: &GosVec, idx: i64) -> i64 {
    unsafe { crate::c_abi::vec::vec_elem_owned_payload_word(vec, idx) }
}

/// Bounds-checked in-place removal returning `Result<T, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_remove_safe(v: *mut GosVec, idx: i64) -> i128 {
    ffi_entry!(0i128, {
        let len = if v.is_null() { 0 } else { unsafe { (*v).len } };
        if v.is_null() || idx < 0 || idx >= len {
            let msg = format!("remove: index {idx} out of bounds for length {len}");
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let vec = unsafe { &mut *v };
        crate::c_abi::vec::bump_vec_mutation_generation(vec);
        let removed = unsafe { removed_elem_payload(vec, idx) };
        // Shift the tail [idx+1, len) down one element so the removal is
        // reflected in place (the caller owns the returned element, so its
        // pointer-bearing payload is not freed here).
        let stride = vec.elem_bytes as usize;
        if !vec.ptr.is_null() && idx + 1 < len {
            let base = vec.ptr.as_ptr();
            let dst = unsafe { base.add(idx as usize * stride) };
            let src = unsafe { base.add((idx as usize + 1) * stride) };
            let count = ((len - idx - 1) as usize) * stride;
            unsafe { std::ptr::copy(src, dst, count) };
        }
        vec.len = len - 1;
        unsafe { gos_rt_result_new(0, removed) }
    })
}

/// `xs.remove(i)` / `Vec::remove(&mut xs, i)` - removes and returns the
/// element. Invalid indices are invariant violations and panic.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_remove_at(v: *mut GosVec, idx: i64) -> i64 {
    ffi_entry!(0, {
        let len = if v.is_null() { 0 } else { unsafe { (*v).len } };
        if v.is_null() || idx < 0 || idx >= len {
            let msg = format!("remove: index {idx} out of bounds for length {len}");
            crate::c_abi::panic::panic_text(&msg);
            return 0;
        }
        let vec = unsafe { &mut *v };
        crate::c_abi::vec::bump_vec_mutation_generation(vec);
        let removed = unsafe { removed_elem_payload(vec, idx) };
        let stride = vec.elem_bytes as usize;
        if !vec.ptr.is_null() && idx + 1 < len {
            let base = vec.ptr.as_ptr();
            let dst = unsafe { base.add(idx as usize * stride) };
            let src = unsafe { base.add((idx as usize + 1) * stride) };
            let count = ((len - idx - 1) as usize) * stride;
            unsafe { std::ptr::copy(src, dst, count) };
        }
        vec.len = len - 1;
        removed
    })
}

/// `xs.pop() -> Option<T>` - removes the last element and returns it
/// packed as the 2-word Option (disc 0 = Some, 1 = None), honoring
/// the header's `elem_bytes` for the payload read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_pop_opt(v: *mut GosVec) -> i128 {
    ffi_entry!(0i128, {
        if v.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let vec = unsafe { &mut *v };
        if vec.len <= 0 {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        crate::c_abi::vec::bump_vec_mutation_generation(vec);
        vec.len -= 1;
        let value = unsafe { crate::c_abi::vec::vec_elem_owned_payload_word(vec, vec.len) };
        unsafe { gos_rt_result_new(0, value) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_pop(v: *mut GosVec, out: *mut u8) -> i32 {
    ffi_entry!(-1, {
        if v.is_null() || out.is_null() {
            return 0;
        }
        let vec = unsafe { &mut *v };
        if vec.len <= 0 {
            return 0;
        }
        crate::c_abi::vec::bump_vec_mutation_generation(vec);
        vec.len -= 1;
        let src = unsafe { vec.ptr.add((vec.len as usize) * (vec.elem_bytes as usize)) };
        unsafe {
            std::ptr::copy_nonoverlapping(src, out, vec.elem_bytes as usize);
        }
        1
    })
}
