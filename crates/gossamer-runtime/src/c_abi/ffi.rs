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
    CALL_GUARDS.with(|guards| drop(guards.borrow_mut().pop()));
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

/// Ends the program because native code called back on a thread that is not
/// inside one of the program's foreign calls: a thread the library started,
/// or a callback kept past the call that registered it on another thread.
pub fn foreign_thread_callback(callback: &str) -> ! {
    super::panic::fatal_program_fault(
        "GX0015",
        "",
        &format!(
            "native code called back into `{callback}` on a thread that is not running one of \
             the program's foreign calls; a callback runs only on the thread whose foreign \
             call invokes it"
        ),
        false,
    )
}

/// The C-ABI entry of a compiled callback: runs the adapter `code` over the
/// argument words at `words` and answers its result word. A fault the
/// adapter raises is held and raised again when the foreign call that ran
/// the callback returns; until then the callback answers zero.
///
/// # Safety
///
/// `code` is a compiled adapter of the shape `fn(i64) -> i64`, `words`
/// holds the words it reads, and `name` is a NUL-terminated name.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ffi_callback_run(
    code: *const u8,
    words: *const u64,
    name: *const u8,
) -> u64 {
    if !in_foreign_call() {
        // HOST-CSTRING: `name` is a NUL-terminated label the backend emits
        // as constant data beside the callback's entry, not a `String`.
        // SAFETY: `name` is NUL-terminated (contract).
        let name = unsafe { std::ffi::CStr::from_ptr(name.cast()) };
        foreign_thread_callback(&name.to_string_lossy());
    }
    type Adapter = unsafe extern "C-unwind" fn(i64) -> i64;
    // SAFETY: `code` is an adapter of this shape (contract).
    let adapter: Adapter = unsafe { std::mem::transmute::<*const u8, Adapter>(code) };
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
