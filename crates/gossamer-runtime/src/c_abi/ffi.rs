//! Support for calls to functions declared in `unsafe extern "C"` blocks.
//!
//! Every tier brackets a foreign call the same way: [`gos_rt_ffi_enter`]
//! marks the worker as inside a call that may block, so the scheduler's
//! watchdog runs waiting goroutines on another worker if it does, and clears
//! `errno` (and on Windows the thread's last error) as the last step before
//! the call; [`gos_rt_ffi_leave`] captures both before any other code runs,
//! then gives the worker back. A slice argument crosses as a
//! pointer to its C elements from [`gos_rt_ffi_buf_begin`], which
//! [`gos_rt_ffi_buf_end`] retires after the call; a struct or fixed array is
//! packed into a [`gos_rt_ffi_struct_alloc`] buffer with the `put` helpers and
//! read back with the `get` helpers.

#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use super::vec::GosVec;
use crate::sched_global::{SyscallGuard, syscall_enter};

/// The error codes a foreign call left behind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForeignErrors {
    /// The C library's `errno`.
    pub errno: i64,
    /// The operating system's error: `GetLastError` on Windows, `errno`
    /// elsewhere.
    pub os: i64,
}

thread_local! {
    /// The error codes the most recent foreign call left, for the goroutine
    /// running on this worker. The scheduler saves and restores them around
    /// every goroutine switch, so they follow the goroutine across workers.
    static FFI_ERRNO: Cell<ForeignErrors> = const {
        Cell::new(ForeignErrors { errno: 0, os: 0 })
    };
    /// The system-call marks of the foreign calls in progress on this worker,
    /// innermost last: a callback may make a foreign call of its own.
    static CALL_GUARDS: RefCell<Vec<SyscallGuard>> = const { RefCell::new(Vec::new()) };
    /// A fault a callback raised, held until the foreign call that ran the
    /// callback returns, so it never unwinds through native frames.
    static PENDING_CALLBACK_FAULT: RefCell<Option<super::panic::DeferredFault>> =
        const { RefCell::new(None) };
    /// Packed copies made for byte arguments whose `Vec` stores a byte per
    /// word, keyed by the copy's address, retired by [`gos_rt_ffi_buf_end`].
    static PACKED_COPIES: RefCell<HashMap<usize, Vec<u8>>> = RefCell::new(HashMap::new());
    /// Live struct buffers by address, with their size in words.
    static STRUCT_BUFFERS: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());
}

/// Swaps the worker's foreign-call error codes for `value`, answering the
/// old ones. The scheduler calls this around each goroutine step.
pub fn swap_ffi_errno(value: ForeignErrors) -> ForeignErrors {
    FFI_ERRNO.with(|cell| cell.replace(value))
}

/// The error codes the current goroutine's most recent foreign call left.
#[must_use]
pub fn ffi_errno() -> ForeignErrors {
    FFI_ERRNO.with(Cell::get)
}

/// Marks the worker as inside a foreign call that may block, then clears the
/// thread's error codes, so a call that succeeds without touching them reads
/// back zero. Every tier calls this immediately before the native function.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_ffi_enter() {
    let guard = syscall_enter();
    CALL_GUARDS.with(|guards| guards.borrow_mut().push(guard));
    native_errors::clear();
}

/// Captures the foreign call's `errno` and leaves the call mark, then raises
/// the fault a callback held during the call, if one did.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_ffi_leave() {
    ffi_leave_capture();
    if let Some(fault) = PENDING_CALLBACK_FAULT.with(|slot| slot.borrow_mut().take()) {
        super::panic::reraise_deferred_fault(&fault);
    }
}

/// [`gos_rt_ffi_leave`] without raising a held fault: the bytecode tier
/// reports a callback's fault through its own error value.
pub fn ffi_leave_capture() {
    // First, before anything that could make a system call of its own.
    let errors = native_errors::capture();
    FFI_ERRNO.with(|cell| cell.set(errors));
    CALL_GUARDS.with(|guards| {
        guards.borrow_mut().pop();
    });
}

/// The calling thread's C `errno` and operating-system error code.
mod native_errors {
    use super::ForeignErrors;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn errno_location() -> *mut libc::c_int {
        // SAFETY: answers the calling thread's `errno` slot; no preconditions.
        unsafe { libc::__errno_location() }
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    fn errno_location() -> *mut libc::c_int {
        // SAFETY: answers the calling thread's `errno` slot; no preconditions.
        unsafe { libc::__error() }
    }

    #[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
    fn errno_location() -> *mut libc::c_int {
        // SAFETY: answers the calling thread's `errno` slot; no preconditions.
        unsafe { libc::__errno() }
    }

    #[cfg(unix)]
    pub(super) fn clear() {
        // SAFETY: the slot is the calling thread's own and always writable.
        unsafe { *errno_location() = 0 }
    }

    // On Unix the system reports through `errno`, so the two codes agree.
    #[cfg(unix)]
    pub(super) fn capture() -> ForeignErrors {
        // SAFETY: the slot is the calling thread's own and always readable.
        let errno = i64::from(unsafe { *errno_location() });
        ForeignErrors { errno, os: errno }
    }

    // Windows keeps two codes: the C runtime's `errno`, which CRT functions
    // set, and the thread's last error, which Win32 functions set.
    #[cfg(windows)]
    unsafe extern "C" {
        fn _errno() -> *mut core::ffi::c_int;
    }

    #[cfg(windows)]
    pub(super) fn clear() {
        // SAFETY: `_errno` answers the calling thread's CRT `errno` slot,
        // always writable; `SetLastError` has no preconditions.
        unsafe {
            *_errno() = 0;
            windows_sys::Win32::Foundation::SetLastError(0);
        }
    }

    #[cfg(windows)]
    pub(super) fn capture() -> ForeignErrors {
        // The last error first: reaching the CRT's per-thread data may make
        // Win32 calls of its own.
        // SAFETY: `GetLastError` has no preconditions.
        let os = i64::from(unsafe { windows_sys::Win32::Foundation::GetLastError() });
        // SAFETY: `_errno` answers the calling thread's CRT `errno` slot.
        let errno = i64::from(unsafe { *_errno() });
        ForeignErrors { errno, os }
    }

    // wasm32 has no foreign functions, so there is nothing to clear or read.
    #[cfg(not(any(unix, windows)))]
    pub(super) fn clear() {}

    #[cfg(not(any(unix, windows)))]
    pub(super) fn capture() -> ForeignErrors {
        ForeignErrors::default()
    }
}

/// Whether this thread is inside a foreign call, the only time native code
/// may call back into the program on it.
#[must_use]
pub fn in_foreign_call() -> bool {
    CALL_GUARDS.with(|guards| !guards.borrow().is_empty())
}

/// Readies a thread the program did not start to run Gossamer code: the
/// native fault handler and the recursion guard, once per thread.
pub fn attach_foreign_thread() {
    thread_local! {
        static ATTACHED: Cell<bool> = const { Cell::new(false) };
    }
    if ATTACHED.with(|attached| attached.replace(true)) {
        return;
    }
    crate::stack_guard::install_stack_guard();
    let remaining =
        crate::stack_guard::remaining_stack_bytes().unwrap_or(gossamer_coro::DEFAULT_STACK_BYTES);
    gossamer_coro::arm_stack_guard(remaining.saturating_sub(gossamer_coro::STACK_GUARD_MARGIN));
}

/// Ends the program with the fault a callback raised on a thread outside
/// the program's foreign calls, where no Gossamer frame can take it: the
/// report it would have printed, and exit code 101.
pub fn foreign_thread_fault(code: &str, prefix: &str, text: &str, trace: &str) -> ! {
    if !trace.is_empty() {
        eprint!("{trace}");
    }
    super::panic::fatal_program_fault(code, prefix, text, false)
}

/// The C-ABI entry of a compiled callback: runs the adapter `code` over the
/// argument words at `words` and answers its result word. A fault the
/// adapter raises is held and raised again when the foreign call that ran
/// the callback returns; until then the callback answers zero. On a thread
/// outside the program's foreign calls the callback runs on that thread,
/// and a fault ends the program.
///
/// # Safety
///
/// `code` is a compiled adapter of the shape `fn(i64) -> i64`, `words`
/// holds the words it reads, and `_name` is the callback's NUL-terminated
/// name, which the entry carries for diagnostics.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_callback_run(
    code: *const u8,
    words: *const u64,
    _name: *const u8,
) -> u64 {
    type Adapter = unsafe extern "C-unwind" fn(i64) -> i64;
    // SAFETY: `code` is an adapter of this shape (contract).
    let adapter: Adapter = unsafe { std::mem::transmute::<*const u8, Adapter>(code) };
    // A thread the library started (or one running a callback after the
    // call that registered it returned) runs the callback itself, blocking
    // as an `Isolation::Thread` child does; a fault there has no foreign call
    // to resume in, so it ends the program.
    if !in_foreign_call() {
        attach_foreign_thread();
        // SAFETY: the adapter reads only the words the shim stored.
        return match super::par::run_deferred(|| unsafe { adapter(words as i64) }) {
            Ok(result) => result as u64,
            Err(fault) => {
                foreign_thread_fault(&fault.code, &fault.prefix, &fault.text, &fault.trace)
            }
        };
    }
    // SAFETY: the adapter reads only the words the shim stored.
    match super::par::run_deferred(|| unsafe { adapter(words as i64) }) {
        Ok(result) => result as u64,
        Err(fault) => {
            PENDING_CALLBACK_FAULT.with(|slot| {
                let mut slot = slot.borrow_mut();
                if slot.is_none() {
                    *slot = Some(fault);
                }
            });
            0
        }
    }
}

/// Readies the runtime inside a library whose host owns `main`, once per
/// process: the arguments `os::args` answers and the heap statics `init`
/// builds. A program with its own `main` has done both, so its exported
/// entries skip this. The host's threads stay outside the program's
/// deadlock accounting, which only a program with `main` enters.
fn start_library(init: *const u8) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    if crate::sched_global::program_entered() {
        return;
    }
    STARTED.call_once(|| {
        attach_foreign_thread();
        // The argument strings live for the process, as a C `argv` does.
        let argv: Vec<*const std::ffi::c_char> = std::env::args_os()
            .filter_map(|arg| std::ffi::CString::new(arg.as_encoded_bytes()).ok())
            .map(|arg| arg.into_raw().cast_const())
            .collect();
        let argc = std::ffi::c_int::try_from(argv.len()).unwrap_or(std::ffi::c_int::MAX);
        let argv = Box::leak(argv.into_boxed_slice());
        // SAFETY: `argv` holds `argc` NUL-terminated strings leaked above.
        unsafe { super::args::gos_rt_set_args(argc, argv.as_ptr()) };
        if init.is_null() {
            return;
        }
        type Init = unsafe extern "C-unwind" fn();
        // SAFETY: a non-null `init` is the program's compiled static-init
        // function, which takes nothing and answers nothing (contract).
        let init: Init = unsafe { std::mem::transmute::<*const u8, Init>(init) };
        // SAFETY: as above.
        if let Err(fault) = super::par::run_deferred(|| unsafe { init() }) {
            foreign_thread_fault(&fault.code, &fault.prefix, &fault.text, &fault.trace);
        }
    });
}

/// `<library>_shutdown()`: what a library's host calls before it exits, to
/// run the hooks `runtime::at_exit` registered and write out what was
/// printed. A process's own exit cannot run them, since the thread state
/// Gossamer code needs is gone by then. A hook that panics is reported and
/// the rest still run.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_library_shutdown() {
    super::print::gos_rt_flush_stdout();
    attach_foreign_thread();
    loop {
        match super::par::run_deferred(super::exit_hooks::run_exit_hooks) {
            Ok(()) => break,
            Err(fault) => {
                if !fault.trace.is_empty() {
                    eprint!("{}", fault.trace);
                }
                eprintln!("{}{}", fault.prefix, fault.text);
            }
        }
    }
    super::print::gos_rt_flush_stdout();
}

/// The C-ABI entry of an `#[export]` function: readies the runtime the
/// first time a library is entered, runs the adapter `code` over the
/// argument words at `words` as [`gos_rt_ffi_callback_run`] does, and
/// flushes what it printed, since the host may exit without the runtime's
/// shutdown.
///
/// # Safety
///
/// As for [`gos_rt_ffi_callback_run`]; `init` is null or the program's
/// compiled static-init function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_export_run(
    code: *const u8,
    words: *const u64,
    name: *const u8,
    init: *const u8,
) -> u64 {
    start_library(init);
    // SAFETY: forwarded under the same contract.
    let result = unsafe { gos_rt_ffi_callback_run(code, words, name) };
    if !in_foreign_call() {
        super::print::gos_rt_flush_stdout();
    }
    result
}

/// The word at index `index` of the callback argument words at `base`.
///
/// # Safety
///
/// `base` addresses at least `index + 1` words.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_word(base: u64, index: u64) -> u64 {
    // SAFETY: in bounds (contract).
    unsafe { (base as *const u64).add(index as usize).read_unaligned() }
}

/// The word at index `index` of the callback argument words at `base`, as
/// the `double` whose bits it holds: a reverse entry stores every float
/// argument widened to one.
///
/// # Safety
///
/// `base` addresses at least `index + 1` words.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_word_f64(base: u64, index: u64) -> f64 {
    // SAFETY: in bounds (contract).
    f64::from_bits(unsafe { gos_rt_ffi_word(base, index) })
}

/// The bits of `value`, the word a callback's float result crosses in.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_ffi_f64_bits(value: f64) -> u64 {
    value.to_bits()
}

/// Copies `len` bytes from the foreign address `src` into `dst`.
///
/// # Safety
///
/// `src` addresses `len` readable bytes and `dst` has room for them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_read(dst: *mut u8, src: u64, len: u64) {
    if len == 0 {
        return;
    }
    // SAFETY: both ranges are valid for `len` bytes (contract).
    unsafe { std::ptr::copy(src as *const u8, dst, len as usize) };
}

/// The integer of C class `class` at the foreign address `addr`, sign- or
/// zero-extended by its class (`B` reads as 0 or 1).
///
/// # Safety
///
/// `addr` addresses a readable value of the class.
#[must_use]
pub unsafe fn load_int(addr: u64, class: i64) -> i64 {
    let (width, signed, _) = class_shape(class);
    let mut bytes = [0u8; 8];
    // SAFETY: `addr` holds `width` readable bytes (contract).
    unsafe { std::ptr::copy_nonoverlapping(addr as *const u8, bytes.as_mut_ptr(), width) };
    if signed && bytes[width - 1] & 0x80 != 0 {
        bytes[width..].fill(0xff);
    }
    let value = i64::from_le_bytes(bytes);
    if u8::try_from(class).ok().map(char::from) == Some('B') {
        i64::from(value & 0xff != 0)
    } else {
        value
    }
}

/// The float of C class `class` (`f` or `d`) at `addr`, as a double.
///
/// # Safety
///
/// `addr` addresses a readable value of the class.
#[must_use]
pub unsafe fn load_float(addr: u64, class: i64) -> f64 {
    let (width, _, _) = class_shape(class);
    if width == 4 {
        // SAFETY: four readable bytes (contract).
        f64::from(unsafe { (addr as *const f32).read_unaligned() })
    } else {
        // SAFETY: eight readable bytes (contract).
        unsafe { (addr as *const f64).read_unaligned() }
    }
}

/// Stores `value` at `addr` as a C integer of class `class`.
///
/// # Safety
///
/// `addr` addresses writable room for a value of the class.
pub unsafe fn store_int(addr: u64, class: i64, value: i64) {
    let (width, _, _) = class_shape(class);
    let bytes = value.to_le_bytes();
    // SAFETY: `addr` holds `width` writable bytes (contract).
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, width) };
}

/// Stores `value` at `addr` as a C float of class `class` (`f` or `d`).
///
/// # Safety
///
/// `addr` addresses writable room for a value of the class.
pub unsafe fn store_float(addr: u64, class: i64, value: f64) {
    let (width, _, _) = class_shape(class);
    if width == 4 {
        // SAFETY: four writable bytes (contract).
        unsafe { (addr as *mut f32).write_unaligned(value as f32) };
    } else {
        // SAFETY: eight writable bytes (contract).
        unsafe { (addr as *mut f64).write_unaligned(value) };
    }
}

/// [`load_int`] for compiled code: `ffi::read` of an integer and
/// `ffi::View::get`.
///
/// # Safety
///
/// As [`load_int`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_load_int(addr: u64, class: i64) -> i64 {
    // SAFETY: forwarded contract.
    unsafe { load_int(addr, class) }
}

/// [`load_float`] for compiled code.
///
/// # Safety
///
/// As [`load_float`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_load_float(addr: u64, class: i64) -> f64 {
    // SAFETY: forwarded contract.
    unsafe { load_float(addr, class) }
}

/// [`store_int`] for compiled code.
///
/// # Safety
///
/// As [`store_int`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_store_int(addr: u64, class: i64, value: i64) {
    // SAFETY: forwarded contract.
    unsafe { store_int(addr, class, value) }
}

/// [`store_float`] for compiled code.
///
/// # Safety
///
/// As [`store_float`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_store_float(addr: u64, class: i64, value: f64) {
    // SAFETY: forwarded contract.
    unsafe { store_float(addr, class, value) }
}

/// The panic message for element `index` of an `ffi::View` of `len`.
#[must_use]
pub fn view_index_message(index: i64, len: i64) -> String {
    format!("ffi::View index out of bounds: the len is {len} but the index is {index}")
}

/// Panics unless `0 <= index < len`: an `ffi::View` element access.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_ffi_view_check(index: i64, len: i64) {
    if index < 0 || index >= len {
        super::panic::panic_text(&view_index_message(index, len));
    }
}

/// The panic message for `lo..hi` outside an `ffi::View` of `len`.
#[must_use]
pub fn view_range_message(lo: i64, hi: i64, len: i64) -> String {
    format!("ffi::View range {lo}..{hi} is out of bounds for a view of {len}")
}

/// Panics unless `0 <= lo <= hi <= len`: an `ffi::View` sub-view.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_ffi_view_range_check(lo: i64, hi: i64, len: i64) {
    if lo < 0 || lo > hi || hi > len {
        super::panic::panic_text(&view_range_message(lo, hi, len));
    }
}

/// The panic message for copying `found` values into a view of `len`.
#[must_use]
pub fn view_len_message(len: i64, found: i64) -> String {
    format!("ffi::View::copy_from: the view holds {len} values but the source holds {found}")
}

/// Panics unless `found == len`: `ffi::View::copy_from`.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_ffi_view_len_check(len: i64, found: i64) {
    if found != len {
        super::panic::panic_text(&view_len_message(len, found));
    }
}

/// The atomic operation `op` (0 load, 1 store, 2 swap, 3 add, 4 sub, 5 and,
/// 6 or, 7 xor) on the `width`-byte integer at `addr`, sequentially
/// consistent, answering the value before it (the value read, for a load).
/// An address not aligned to `width` is an error message.
///
/// # Safety
///
/// `addr` addresses a live, writable integer of `width` bytes that every
/// concurrent access reaches atomically.
pub unsafe fn atomic_rmw(addr: u64, op: i64, width: i64, value: i64) -> Result<i64, String> {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
    let width = u64::try_from(width).unwrap_or(0);
    if !matches!(width, 4 | 8) || !addr.is_multiple_of(width) {
        return Err(format!(
            "ffi atomic: the address 0x{addr:x} is not aligned to {width} bytes"
        ));
    }
    if width == 4 {
        // SAFETY: aligned and live (checked above, contract).
        let cell = unsafe { AtomicU32::from_ptr(addr as *mut u32) };
        let v = value as u32;
        let old = match op {
            0 => cell.load(SeqCst),
            1 => {
                cell.store(v, SeqCst);
                0
            }
            2 => cell.swap(v, SeqCst),
            3 => cell.fetch_add(v, SeqCst),
            4 => cell.fetch_sub(v, SeqCst),
            5 => cell.fetch_and(v, SeqCst),
            6 => cell.fetch_or(v, SeqCst),
            _ => cell.fetch_xor(v, SeqCst),
        };
        Ok(i64::from(old))
    } else {
        // SAFETY: aligned and live (checked above, contract).
        let cell = unsafe { AtomicU64::from_ptr(addr as *mut u64) };
        let v = value as u64;
        let old = match op {
            0 => cell.load(SeqCst),
            1 => {
                cell.store(v, SeqCst);
                0
            }
            2 => cell.swap(v, SeqCst),
            3 => cell.fetch_add(v, SeqCst),
            4 => cell.fetch_sub(v, SeqCst),
            5 => cell.fetch_and(v, SeqCst),
            6 => cell.fetch_or(v, SeqCst),
            _ => cell.fetch_xor(v, SeqCst),
        };
        Ok(old as i64)
    }
}

/// Atomically replaces the `width`-byte integer at `addr` with `new` when it
/// holds `expected`, sequentially consistent, answering the value it held.
///
/// # Safety
///
/// As [`atomic_rmw`].
pub unsafe fn atomic_cas(addr: u64, width: i64, expected: i64, new: i64) -> Result<i64, String> {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
    let width = u64::try_from(width).unwrap_or(0);
    if !matches!(width, 4 | 8) || !addr.is_multiple_of(width) {
        return Err(format!(
            "ffi atomic: the address 0x{addr:x} is not aligned to {width} bytes"
        ));
    }
    Ok(if width == 4 {
        // SAFETY: aligned and live (checked above, contract).
        let cell = unsafe { AtomicU32::from_ptr(addr as *mut u32) };
        let old = match cell.compare_exchange(expected as u32, new as u32, SeqCst, SeqCst) {
            Ok(old) | Err(old) => old,
        };
        i64::from(old)
    } else {
        // SAFETY: aligned and live (checked above, contract).
        let cell = unsafe { AtomicU64::from_ptr(addr as *mut u64) };
        let old = match cell.compare_exchange(expected as u64, new as u64, SeqCst, SeqCst) {
            Ok(old) | Err(old) => old,
        };
        old as i64
    })
}

/// [`atomic_rmw`] for compiled code; a misaligned address panics.
///
/// # Safety
///
/// As [`atomic_rmw`].
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_ffi_atomic_rmw(
    addr: u64,
    op: i64,
    width: i64,
    value: i64,
) -> i64 {
    // SAFETY: forwarded contract.
    match unsafe { atomic_rmw(addr, op, width, value) } {
        Ok(old) => old,
        Err(message) => {
            super::panic::panic_text(&message);
            0
        }
    }
}

/// [`atomic_cas`] for compiled code; a misaligned address panics.
///
/// # Safety
///
/// As [`atomic_cas`].
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_ffi_atomic_cas(
    addr: u64,
    width: i64,
    expected: i64,
    new: i64,
) -> i64 {
    // SAFETY: forwarded contract.
    match unsafe { atomic_cas(addr, width, expected, new) } {
        Ok(old) => old,
        Err(message) => {
            super::panic::panic_text(&message);
            0
        }
    }
}

/// Copies the leading `len` bytes of the C buffer `src` over those of `dst`:
/// how a value is reinterpreted as a union member and back.
///
/// # Safety
///
/// `dst` and `src` each hold at least `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_copy(dst: *mut u8, src: *const u8, len: u64) {
    if len == 0 {
        return;
    }
    // SAFETY: both ranges are valid for `len` bytes (contract).
    unsafe { std::ptr::copy(src, dst, len as usize) };
}

/// Copies `len` bytes from `src` to the foreign address `dst`.
///
/// # Safety
///
/// `dst` addresses `len` writable bytes and `src` holds them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_write(dst: u64, src: *const u8, len: u64) {
    if len == 0 {
        return;
    }
    // SAFETY: both ranges are valid for `len` bytes (contract).
    unsafe { std::ptr::copy(src, dst as *mut u8, len as usize) };
}

/// The length of the NUL-terminated string at the foreign address `src`.
///
/// # Safety
///
/// `src` addresses a NUL-terminated byte string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_strlen(src: u64) -> u64 {
    // HOST-CSTRING: `src` is foreign memory a C library owns.
    // SAFETY: NUL-terminated (contract).
    unsafe { std::ffi::CStr::from_ptr(src as *const std::ffi::c_char) }
        .to_bytes()
        .len() as u64
}

// wasm32 has no C allocator and no foreign code to hand memory to.
#[cfg(not(target_arch = "wasm32"))]
unsafe extern "C" {
    #[link_name = "calloc"]
    fn c_calloc(count: usize, size: usize) -> *mut u8;
    #[link_name = "free"]
    fn c_free(ptr: *mut u8);
}

/// `len` zeroed bytes from the platform C allocator, so a library's own
/// `free` accepts them; `0` when the allocation fails.
#[cfg(not(target_arch = "wasm32"))]
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_ffi_alloc(len: u64) -> u64 {
    // SAFETY: `calloc` accepts any size and answers null on failure.
    unsafe { c_calloc(1, usize::try_from(len).unwrap_or(usize::MAX).max(1)) as u64 }
}

/// Frees memory the platform C allocator handed out.
///
/// # Safety
///
/// `addr` came from `malloc`, `calloc`, `realloc`, or
/// [`gos_rt_ffi_alloc`], and is not used again.
#[cfg(not(target_arch = "wasm32"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_free(addr: u64) {
    // SAFETY: an allocation of the platform C allocator (contract).
    unsafe { c_free(addr as *mut u8) };
}

/// Raises `GX0013` for a foreign function that answered null where its
/// declaration promised a non-null `Ptr`.
///
/// # Safety
///
/// `symbol` is a live Gossamer string.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_ffi_null_result(symbol: *const std::ffi::c_char) {
    // SAFETY: a live string (contract).
    let name = unsafe { super::gos_str_arg_string(symbol) };
    super::panic::raise_foreign_fault("GX0013", null_result_message(&name))
}

/// The `GX0013` report for `symbol`, shared with the bytecode tier.
#[must_use]
pub fn null_result_message(symbol: &str) -> String {
    format!(
        "the foreign function `{symbol}` answered a null pointer where its declaration promises \
         `Ptr`; declare the result `Option<Ptr<..>>` if it can be null"
    )
}

/// Handles: values a program hands to native code as an integer and reads
/// back by it, each held in a one-element `Vec` the registry keeps a share of.
mod handles {
    use std::collections::HashMap;

    use parking_lot::Mutex;

    use super::GosVec;

    struct Registry {
        next: u64,
        cells: HashMap<u64, usize>,
    }

    static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

    fn with<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
        let mut guard = REGISTRY.lock();
        f(guard.get_or_insert_with(|| Registry {
            next: 1,
            cells: HashMap::new(),
        }))
    }

    pub(super) fn missing(id: u64) -> ! {
        super::super::panic::raise_foreign_fault("GX0014", super::missing_handle_message(id))
    }

    /// Registers `cell`, taking a share of it, and answers its id.
    ///
    /// # Safety
    ///
    /// `cell` is a live vector.
    pub(super) unsafe fn pin(cell: *mut GosVec) -> u64 {
        // A handle may be read on any worker, so the cell's counts become
        // atomic before it is published.
        // SAFETY: a live vector (contract).
        unsafe {
            super::super::vec::gos_rt_vec_mark_shared(cell);
            super::super::vec::gos_rt_vec_retain(cell);
        }
        with(|registry| {
            let id = registry.next;
            registry.next += 1;
            registry.cells.insert(id, cell as usize);
            id
        })
    }

    /// The cell of `id`, with a share for the caller.
    pub(super) fn cell(id: u64) -> *mut GosVec {
        let Some(cell) = with(|registry| registry.cells.get(&id).copied()) else {
            missing(id)
        };
        let cell = cell as *mut GosVec;
        // SAFETY: the registry holds a share, so the cell is live.
        unsafe { super::super::vec::gos_rt_vec_retain(cell) };
        cell
    }

    /// Replaces the cell of `id` with `cell`, taking a share of it.
    ///
    /// # Safety
    ///
    /// `cell` is a live vector.
    pub(super) unsafe fn store(id: u64, cell: *mut GosVec) {
        // SAFETY: a live vector (contract).
        unsafe {
            super::super::vec::gos_rt_vec_mark_shared(cell);
            super::super::vec::gos_rt_vec_retain(cell);
        }
        let Some(old) = with(|registry| registry.cells.insert(id, cell as usize)) else {
            with(|registry| registry.cells.remove(&id));
            // SAFETY: the share taken above.
            unsafe { super::super::map::gos_rt_vec_free(cell) };
            missing(id)
        };
        // SAFETY: the registry's share of the replaced cell.
        unsafe { super::super::map::gos_rt_vec_free(old as *mut GosVec) };
    }

    /// Drops `id`, giving back the registry's share.
    pub(super) fn release(id: u64) {
        let Some(old) = with(|registry| registry.cells.remove(&id)) else {
            missing(id)
        };
        // SAFETY: the registry's share.
        unsafe { super::super::map::gos_rt_vec_free(old as *mut GosVec) };
    }
}

/// The `GX0014` report for an id no live handle has, shared with the
/// bytecode tier.
#[must_use]
pub fn missing_handle_message(id: u64) -> String {
    format!(
        "`ffi::Handle` {id} is not live: it was released, or the pointer native code handed \
         back is not a handle"
    )
}

/// `ffi::Handle::new`: registers the one-element cell holding the value and
/// answers the handle's id.
///
/// # Safety
///
/// `cell` is a live vector.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_handle_pin(cell: *mut GosVec) -> u64 {
    // SAFETY: a live vector (contract).
    unsafe { handles::pin(cell) }
}

/// The cell of the handle `id`, with a share for the caller; `GX0014` when
/// no live handle has that id.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_ffi_handle_cell(id: u64) -> *mut GosVec {
    handles::cell(id)
}

/// Replaces the cell of the handle `id`; `GX0014` when it is not live.
///
/// # Safety
///
/// `cell` is a live vector.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_ffi_handle_store(id: u64, cell: *mut GosVec) {
    // SAFETY: a live vector (contract).
    unsafe { handles::store(id, cell) }
}

/// Releases the handle `id`; `GX0014` when it is not live.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_ffi_handle_release(id: u64) {
    handles::release(id);
}

/// `std::ffi::last_errno()`: the C `errno` the current goroutine's most
/// recent foreign call left.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_ffi_last_errno() -> i64 {
    ffi_errno().errno
}

/// `std::ffi::last_os_error()`: the operating-system error code
/// (`GetLastError` on Windows, `errno` elsewhere) the current goroutine's
/// most recent foreign call left.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_ffi_last_os_error() -> i64 {
    ffi_errno().os
}

/// Bytes, signedness, and float-ness of the C element class `class`.
fn class_shape(class: i64) -> (usize, bool, bool) {
    match u8::try_from(class).map(char::from) {
        Ok('c') => (1, true, false),
        Ok('h') => (2, true, false),
        Ok('H') => (2, false, false),
        Ok('i') => (4, true, false),
        Ok('I') => (4, false, false),
        Ok('l') => (8, true, false),
        Ok('L') => (8, false, false),
        Ok('f') => (4, false, true),
        Ok('d') => (8, false, true),
        _ => (1, false, false),
    }
}

/// A pointer to the elements of the vector `v`, as C elements of class
/// `class`, for a foreign call. A vector that already stores them at their C
/// width answers its own buffer, so the call reads and writes it in place;
/// one storing each in a wider slot answers a packed copy.
///
/// # Safety
///
/// `v` is null or a live vector of `class` elements that nothing resizes
/// until the matching [`gos_rt_ffi_buf_end`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_buf_begin(v: *mut GosVec, class: i64) -> *mut u8 {
    if v.is_null() {
        return std::ptr::NonNull::<u8>::dangling().as_ptr();
    }
    // SAFETY: `v` is a live vector (contract).
    let vec = unsafe { &*v };
    let len = usize::try_from(vec.len).unwrap_or(0);
    let (width, _, float) = class_shape(class);
    let stride = vec.elem_bytes as usize;
    if stride == width {
        if vec.ptr.is_null() || len == 0 {
            return std::ptr::NonNull::<u8>::dangling().as_ptr();
        }
        return vec.ptr.as_ptr();
    }
    let mut packed: Vec<u8> = Vec::with_capacity(len * width);
    for i in 0..len {
        // SAFETY: `i` is below the vector's length and each slot is `stride`
        // bytes; the runtime's targets are little-endian, so a slot's low
        // bytes are its value at a narrower width.
        let slot = unsafe { vec.ptr.as_ptr().add(i * stride) };
        if float && width == 4 {
            // SAFETY: a wider float slot holds the element as a double.
            let value = unsafe { slot.cast::<f64>().read_unaligned() } as f32;
            packed.extend_from_slice(&value.to_le_bytes());
        } else {
            // SAFETY: the slot has at least `width` bytes.
            packed.extend_from_slice(unsafe { std::slice::from_raw_parts(slot, width) });
        }
    }
    let ptr = packed.as_mut_ptr();
    PACKED_COPIES.with(|copies| copies.borrow_mut().insert(ptr as usize, packed));
    ptr
}

/// Retires the pointer [`gos_rt_ffi_buf_begin`] answered for `v`. A packed
/// copy is written back to `v` when `writable` is non-zero, widening each
/// element to its slot, then freed.
///
/// # Safety
///
/// `v`, `ptr`, and `class` are what a [`gos_rt_ffi_buf_begin`] call took and
/// answered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_buf_end(
    v: *mut GosVec,
    ptr: *mut u8,
    writable: i64,
    class: i64,
) {
    let Some(packed) = PACKED_COPIES.with(|copies| copies.borrow_mut().remove(&(ptr as usize)))
    else {
        return;
    };
    if writable == 0 || v.is_null() {
        return;
    }
    // SAFETY: `v` is the live vector the copy was made from (contract).
    let vec = unsafe { &*v };
    let (width, signed, float) = class_shape(class);
    let stride = vec.elem_bytes as usize;
    let len = usize::try_from(vec.len).unwrap_or(0);
    for (i, element) in packed.chunks_exact(width).take(len).enumerate() {
        // SAFETY: `i` is below the vector's length and the slot is `stride`
        // bytes.
        let slot = unsafe { vec.ptr.as_ptr().add(i * stride) };
        if float && width == 4 {
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(element);
            let value = f64::from(f32::from_le_bytes(bytes));
            // SAFETY: a wider float slot holds the element as a double.
            unsafe { slot.cast::<f64>().write_unaligned(value) };
            continue;
        }
        let mut bytes = [0u8; 8];
        bytes[..width].copy_from_slice(element);
        let negative = signed && element[width - 1] & 0x80 != 0;
        if negative {
            bytes[width..].fill(0xff);
        }
        // SAFETY: the slot holds `stride` bytes, at most eight.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), slot, stride.min(8)) };
    }
}

/// A zeroed C buffer of `size` bytes that a struct or array argument is
/// packed into, freed by [`gos_rt_ffi_struct_free`].
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_ffi_struct_alloc(size: i64) -> *mut u8 {
    let size = usize::try_from(size).unwrap_or(0).max(1);
    let buffer = vec![0u64; size.div_ceil(8)].into_boxed_slice();
    let ptr = Box::into_raw(buffer).cast::<u8>();
    STRUCT_BUFFERS.with(|sizes| sizes.borrow_mut().insert(ptr as usize, size.div_ceil(8)));
    ptr
}

/// Frees a buffer [`gos_rt_ffi_struct_alloc`] answered.
///
/// # Safety
///
/// `ptr` came from [`gos_rt_ffi_struct_alloc`] on this thread and is not
/// used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_struct_free(ptr: *mut u8) {
    let Some(words) = STRUCT_BUFFERS.with(|sizes| sizes.borrow_mut().remove(&(ptr as usize)))
    else {
        return;
    };
    // SAFETY: `ptr` is the start of a boxed `[u64]` of `words` words.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr.cast::<u64>(), words)) });
}

/// Writes the low `width` bytes of `bits` at `offset` in a struct buffer.
///
/// # Safety
///
/// `buffer` has at least `offset + width` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_put(buffer: *mut u8, offset: i64, width: i64, bits: i64) {
    let width = usize::try_from(width).unwrap_or(0).min(8);
    let bytes = bits.to_le_bytes();
    // SAFETY: in bounds (contract).
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.add(offset as usize), width);
    }
}

/// Writes `value` as a C `double` at `offset` in a struct buffer.
///
/// # Safety
///
/// `buffer` has at least `offset + 8` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_put_f64(buffer: *mut u8, offset: i64, value: f64) {
    // SAFETY: in bounds (contract).
    unsafe {
        buffer
            .add(offset as usize)
            .cast::<f64>()
            .write_unaligned(value);
    }
}

/// Writes `value` as a C `float` at `offset` in a struct buffer.
///
/// # Safety
///
/// `buffer` has at least `offset + 4` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_put_f32(buffer: *mut u8, offset: i64, value: f64) {
    // SAFETY: in bounds (contract).
    unsafe {
        buffer
            .add(offset as usize)
            .cast::<f32>()
            .write_unaligned(value as f32);
    }
}

/// Reads `width` bytes at `offset` in a struct buffer, sign-extended when
/// `signed` is non-zero.
///
/// # Safety
///
/// `buffer` has at least `offset + width` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_get(
    buffer: *const u8,
    offset: i64,
    width: i64,
    signed: i64,
) -> i64 {
    let width = usize::try_from(width).unwrap_or(0).clamp(1, 8);
    let mut bytes = [0u8; 8];
    // SAFETY: in bounds (contract).
    unsafe {
        std::ptr::copy_nonoverlapping(buffer.add(offset as usize), bytes.as_mut_ptr(), width);
    }
    if signed != 0 && bytes[width - 1] & 0x80 != 0 {
        bytes[width..].fill(0xff);
    }
    i64::from_le_bytes(bytes)
}

/// Reads a C `double` at `offset` in a struct buffer.
///
/// # Safety
///
/// `buffer` has at least `offset + 8` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_get_f64(buffer: *const u8, offset: i64) -> f64 {
    // SAFETY: in bounds (contract).
    unsafe { buffer.add(offset as usize).cast::<f64>().read_unaligned() }
}

/// Reads a C `float` at `offset` in a struct buffer, as a double.
///
/// # Safety
///
/// `buffer` has at least `offset + 4` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_get_f32(buffer: *const u8, offset: i64) -> f64 {
    // SAFETY: in bounds (contract).
    f64::from(unsafe { buffer.add(offset as usize).cast::<f32>().read_unaligned() })
}

/// `ioctl(fd, request, arg)` with a pointer argument, for a declaration that
/// cannot name the variadic C function itself.
///
/// # Safety
///
/// `arg` points at memory the request reads or writes, as `ioctl` requires.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_ioctl(fd: i32, request: u64, arg: *mut u8) -> i32 {
    // SAFETY: forwarded unchanged under the caller's contract. The request
    // parameter's C type differs by platform.
    unsafe { libc::ioctl(fd, request as _, arg) }
}

/// `fcntl(fd, cmd, arg)` with an integer argument.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_ffi_fcntl(fd: i32, cmd: i32, arg: i64) -> i32 {
    // SAFETY: integer-argument `fcntl` commands read no memory through `arg`.
    unsafe { libc::fcntl(fd, cmd, arg as libc::c_long) }
}

/// `open(path, flags, mode)` with the mode always passed.
///
/// # Safety
///
/// `path` points at a NUL-terminated byte string.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_open(path: *const u8, flags: i32, mode: u32) -> i32 {
    // SAFETY: `path` is NUL-terminated (contract).
    unsafe { libc::open(path.cast(), flags, mode as libc::c_uint) }
}

/// Directories `#[link(search = "..")]` names, searched for a library
/// before the platform's own search path.
static LIBRARY_DIRS: parking_lot::RwLock<Vec<std::path::PathBuf>> =
    parking_lot::RwLock::new(Vec::new());

/// Sets the directories the bytecode tier and the JIT search for the
/// libraries foreign declarations name.
pub fn set_library_search_dirs(dirs: Vec<std::path::PathBuf>) {
    *LIBRARY_DIRS.write() = dirs;
}

// Only targets that load shared libraries search for one.
#[cfg(any(unix, windows))]
fn library_search_dirs() -> Vec<std::path::PathBuf> {
    LIBRARY_DIRS.read().clone()
}

/// The shared libraries compiled from the program's `[native]` sources,
/// which a declaration with no `#[link]` resolves against before the
/// process's own modules.
static NATIVE_LIBRARIES: parking_lot::RwLock<Vec<std::path::PathBuf>> =
    parking_lot::RwLock::new(Vec::new());

/// Sets the `[native]` shared libraries the bytecode tier and the JIT load.
pub fn set_native_libraries(paths: Vec<std::path::PathBuf>) {
    *NATIVE_LIBRARIES.write() = paths;
}

#[cfg(any(unix, windows))]
fn native_libraries() -> Vec<std::path::PathBuf> {
    NATIVE_LIBRARIES.read().clone()
}

/// The address of the native function `name`, looked up the way the
/// compiled tiers link it: the runtime's own exports first, then the
/// libraries named in `libraries`, then the process's loaded modules (the
/// platform C library, and on Windows `kernel32`).
#[must_use]
pub fn resolve_symbol(name: &str, libraries: &[String]) -> Option<*const u8> {
    if let Some(addr) = crate::symbols::address(name) {
        return Some(addr.cast());
    }
    if let Some(addr) = linked_math::lookup(name) {
        return Some(addr);
    }
    platform::resolve(name, libraries)
}

/// On Linux the runtime links its own definitions of the C math functions
/// (from `compiler_builtins`), and a compiled program's call to one binds to
/// those rather than the C library's, which round differently in the last
/// place. The bytecode VM and the JIT take the same definitions from here, so
/// a foreign call answers the same bits on every tier.
#[cfg(target_os = "linux")]
mod linked_math {
    macro_rules! linked {
        ($($name:ident($($arg:ty),*) -> $ret:ty;)*) => {
            unsafe extern "C" {
                $(fn $name($(_: $arg),*) -> $ret;)*
            }

            pub(super) fn lookup(name: &str) -> Option<*const u8> {
                Some(match name {
                    $(stringify!($name) => $name as *const u8,)*
                    _ => return None,
                })
            }
        };
    }

    linked! {
        acos(f64) -> f64; acosf(f32) -> f32; acosh(f64) -> f64; acoshf(f32) -> f32;
        asin(f64) -> f64; asinf(f32) -> f32; asinh(f64) -> f64; asinhf(f32) -> f32;
        atan(f64) -> f64; atanf(f32) -> f32; atan2(f64, f64) -> f64; atan2f(f32, f32) -> f32;
        atanh(f64) -> f64; atanhf(f32) -> f32; cbrt(f64) -> f64; cbrtf(f32) -> f32;
        ceil(f64) -> f64; ceilf(f32) -> f32; copysign(f64, f64) -> f64;
        copysignf(f32, f32) -> f32; cos(f64) -> f64; cosf(f32) -> f32; cosh(f64) -> f64;
        coshf(f32) -> f32; erf(f64) -> f64; erff(f32) -> f32; erfc(f64) -> f64;
        erfcf(f32) -> f32; exp(f64) -> f64; expf(f32) -> f32; exp2(f64) -> f64;
        exp2f(f32) -> f32; expm1(f64) -> f64; expm1f(f32) -> f32; fabs(f64) -> f64;
        fabsf(f32) -> f32; fdim(f64, f64) -> f64; fdimf(f32, f32) -> f32; floor(f64) -> f64;
        floorf(f32) -> f32; fma(f64, f64, f64) -> f64; fmaf(f32, f32, f32) -> f32;
        fmax(f64, f64) -> f64; fmaxf(f32, f32) -> f32; fmin(f64, f64) -> f64;
        fminf(f32, f32) -> f32; fmod(f64, f64) -> f64; fmodf(f32, f32) -> f32;
        hypot(f64, f64) -> f64; hypotf(f32, f32) -> f32; ldexp(f64, i32) -> f64;
        ldexpf(f32, i32) -> f32; log(f64) -> f64; logf(f32) -> f32; log10(f64) -> f64;
        log10f(f32) -> f32; log1p(f64) -> f64; log1pf(f32) -> f32; log2(f64) -> f64;
        log2f(f32) -> f32; nextafter(f64, f64) -> f64; nextafterf(f32, f32) -> f32;
        pow(f64, f64) -> f64; powf(f32, f32) -> f32; remainder(f64, f64) -> f64;
        remainderf(f32, f32) -> f32; rint(f64) -> f64; rintf(f32) -> f32; round(f64) -> f64;
        roundf(f32) -> f32; sin(f64) -> f64; sinf(f32) -> f32; sinh(f64) -> f64;
        sinhf(f32) -> f32; sqrt(f64) -> f64; sqrtf(f32) -> f32; tan(f64) -> f64;
        tanf(f32) -> f32; tanh(f64) -> f64; tanhf(f32) -> f32; tgamma(f64) -> f64;
        tgammaf(f32) -> f32; trunc(f64) -> f64; truncf(f32) -> f32;
    }
}

#[cfg(not(target_os = "linux"))]
mod linked_math {
    // Elsewhere the runtime links the platform's own math library, which is
    // what the dynamic lookup finds too.
    pub(super) fn lookup(_name: &str) -> Option<*const u8> {
        None
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::CString;

    pub(super) fn resolve(name: &str, libraries: &[String]) -> Option<*const u8> {
        let symbol = CString::new(name).ok()?;
        let dirs = super::library_search_dirs();
        for library in libraries {
            let bare = library_file_names(library);
            let located = dirs.iter().flat_map(|dir| {
                bare.iter()
                    .map(move |file| dir.join(file).to_string_lossy().into_owned())
            });
            for file in located.chain(bare.iter().cloned()) {
                let Ok(file) = CString::new(file) else {
                    continue;
                };
                // SAFETY: `file` is NUL-terminated; a failed open answers null.
                let handle =
                    unsafe { libc::dlopen(file.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
                if handle.is_null() {
                    continue;
                }
                // SAFETY: `handle` is a live library handle and `symbol` is
                // NUL-terminated.
                let addr = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
                if !addr.is_null() {
                    return Some(addr.cast_const().cast());
                }
            }
        }
        for path in super::native_libraries() {
            let Ok(file) = CString::new(path.to_string_lossy().into_owned()) else {
                continue;
            };
            // SAFETY: `file` is NUL-terminated; a failed open answers null.
            let handle = unsafe { libc::dlopen(file.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
            if handle.is_null() {
                continue;
            }
            // SAFETY: `handle` is a live library handle and `symbol` is
            // NUL-terminated.
            let addr = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
            if !addr.is_null() {
                return Some(addr.cast_const().cast());
            }
        }
        // SAFETY: `RTLD_DEFAULT` searches every loaded object; `symbol` is
        // NUL-terminated.
        let addr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr()) };
        (!addr.is_null()).then(|| addr.cast_const().cast())
    }

    fn library_file_names(library: &str) -> Vec<String> {
        if cfg!(target_os = "macos") {
            vec![
                format!("lib{library}.dylib"),
                format!("{library}.framework/{library}"),
            ]
        } else {
            vec![format!("lib{library}.so"), format!("lib{library}.so.6")]
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleHandleA, GetProcAddress, LoadLibraryA,
    };

    pub(super) fn resolve(name: &str, libraries: &[String]) -> Option<*const u8> {
        let symbol = std::ffi::CString::new(name).ok()?;
        let dirs = super::library_search_dirs();
        let mut modules: Vec<String> = libraries
            .iter()
            .flat_map(|l| {
                dirs.iter()
                    .map(move |dir| dir.join(format!("{l}.dll")).to_string_lossy().into_owned())
            })
            .collect();
        modules.extend(libraries.iter().map(|l| format!("{l}.dll")));
        modules.extend(
            super::native_libraries()
                .iter()
                .map(|path| path.to_string_lossy().into_owned()),
        );
        modules.extend(
            ["kernel32.dll", "user32.dll", "ucrtbase.dll", "msvcrt.dll"].map(str::to_string),
        );
        for module in modules {
            let Ok(file) = std::ffi::CString::new(module) else {
                continue;
            };
            // SAFETY: `file` is NUL-terminated; a module not loaded answers null.
            let mut handle = unsafe { GetModuleHandleA(file.as_ptr().cast()) };
            if handle.is_null() {
                // SAFETY: as above; a missing library answers null.
                handle = unsafe { LoadLibraryA(file.as_ptr().cast()) };
            }
            if handle.is_null() {
                continue;
            }
            // SAFETY: `handle` is a loaded module and `symbol` is NUL-terminated.
            if let Some(addr) = unsafe { GetProcAddress(handle, symbol.as_ptr().cast()) } {
                return Some(addr as *const u8);
            }
        }
        None
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    pub(super) fn resolve(_name: &str, _libraries: &[String]) -> Option<*const u8> {
        None
    }
}
