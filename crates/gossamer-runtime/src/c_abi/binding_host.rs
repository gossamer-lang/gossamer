//! The [`HostTable`] a Rust binding library reaches this runtime through.
//!
//! A native build hands the runtime's table to the linked bindings'
//! `gos_binding_init` before `main` runs; `gos run` builds its own table from
//! [`compiled_host_table`] and adds the VM's callback entries.

use std::alloc::Layout;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::os::raw::c_char;

use gossamer_binding_abi::{ABI_VERSION, HostTable, StrRef};

unsafe extern "C" fn alloc(size: usize, align: usize) -> *mut u8 {
    let Ok(layout) = Layout::from_size_align(size, align) else {
        return std::ptr::null_mut();
    };
    if layout.size() == 0 {
        return std::ptr::without_provenance_mut(layout.align());
    }
    // SAFETY: the layout has a non-zero size.
    unsafe { std::alloc::alloc(layout) }
}

unsafe extern "C" fn dealloc(ptr: *mut u8, size: usize, align: usize) {
    let Ok(layout) = Layout::from_size_align(size, align) else {
        return;
    };
    if layout.size() != 0 && !ptr.is_null() {
        // SAFETY: `alloc` made `ptr` with this layout.
        unsafe { std::alloc::dealloc(ptr, layout) }
    }
}

/// Allocates `layout` from this runtime's allocator. A native build's binding
/// archive carries its own copy of `std`, and its global allocator
/// (`gossamer_binding::RuntimeAllocator`) calls these four entries so every
/// Rust allocation in the program has the one owner every free reaches.
///
/// # Safety
/// As for [`std::alloc::GlobalAlloc::alloc`], with `size` and `align` a valid
/// non-zero-size layout.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_heap_alloc(size: usize, align: usize) -> *mut u8 {
    // SAFETY: the caller's contract.
    unsafe { std::alloc::alloc(Layout::from_size_align_unchecked(size, align)) }
}

/// [`gos_rt_binding_heap_alloc`], zeroed.
///
/// # Safety
/// As for [`gos_rt_binding_heap_alloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_heap_alloc_zeroed(size: usize, align: usize) -> *mut u8 {
    // SAFETY: the caller's contract.
    unsafe { std::alloc::alloc_zeroed(Layout::from_size_align_unchecked(size, align)) }
}

/// Frees what [`gos_rt_binding_heap_alloc`] answered.
///
/// # Safety
/// As for [`std::alloc::GlobalAlloc::dealloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_heap_dealloc(ptr: *mut u8, size: usize, align: usize) {
    // SAFETY: the caller's contract.
    unsafe { std::alloc::dealloc(ptr, Layout::from_size_align_unchecked(size, align)) }
}

/// Resizes what [`gos_rt_binding_heap_alloc`] answered.
///
/// # Safety
/// As for [`std::alloc::GlobalAlloc::realloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_heap_realloc(
    ptr: *mut u8,
    size: usize,
    align: usize,
    new_size: usize,
) -> *mut u8 {
    // SAFETY: the caller's contract.
    unsafe {
        std::alloc::realloc(
            ptr,
            Layout::from_size_align_unchecked(size, align),
            new_size,
        )
    }
}

unsafe extern "C" fn vec_with_capacity(elem_bytes: u32, cap: i64, elem_kind: u8) -> *mut c_void {
    super::vec::gos_rt_vec_with_capacity_typed(elem_bytes, cap, elem_kind).cast()
}

unsafe extern "C" fn str_new(text: StrRef) -> *mut c_char {
    // SAFETY: the binding's text lives for this call, which copies it.
    super::string::alloc_cstring(unsafe { text.as_bytes() })
}

unsafe extern "C" fn str_free(s: *mut c_char) {
    // SAFETY: the binding hands back a string this runtime allocated.
    unsafe { super::string::gos_rt_str_free(s) }
}

unsafe extern "C" fn callback_invoke(
    handle: u64,
    args: *const c_void,
    arg_count: u32,
    out: *mut c_void,
) -> i32 {
    // SAFETY: the binding passes `arg_count` wire values and a writable result.
    unsafe { super::signal::gos_rt_callback_invoke(handle, args.cast(), arg_count, out.cast()) }
}

unsafe extern "C" fn callback_retain(handle: u64) {
    super::binding_callback::gos_rt_binding_callback_retain(handle);
}

unsafe extern "C" fn callback_release(handle: u64) {
    super::binding_callback::gos_rt_binding_callback_release(handle);
}

thread_local! {
    /// The panic a wire thunk on this thread caught, until its caller raises it.
    static PENDING_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
    // Whether `PENDING_PANIC` holds a message. Every binding call's return
    // reads it, and a flag without a destructor is one thread-local load.
    static PANIC_RECORDED: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "C" fn raise_panic(message: StrRef) {
    // SAFETY: the binding's message lives for this call, which copies it.
    let text = unsafe { message.to_string_lossy() };
    PENDING_PANIC.with(|pending| *pending.borrow_mut() = Some(text));
    PANIC_RECORDED.set(true);
}

unsafe extern "C" fn blocking_enter() -> u64 {
    crate::sched_global::syscall_mark();
    0
}

unsafe extern "C" fn blocking_leave(_token: u64) {
    crate::sched_global::syscall_unmark();
}

/// The table every tier shares; the VM entries are absent.
#[must_use]
pub const fn compiled_host_table() -> HostTable {
    HostTable {
        abi_version: ABI_VERSION,
        size: std::mem::size_of::<HostTable>() as u32,
        alloc,
        dealloc,
        vec_with_capacity,
        str_new,
        str_free,
        callback_invoke,
        callback_retain,
        callback_release,
        raise_panic,
        blocking_enter,
        blocking_leave,
        vm_invoke: None,
        vm_free_out: None,
        vm_callback_retain: None,
        vm_callback_release: None,
    }
}

static HOST_TABLE: HostTable = compiled_host_table();

/// Hands the runtime's table to `init`, the `gos_binding_init` of the
/// bindings a native build links, before the program's `main` runs. A table
/// the bindings refuse leaves every binding call to report it.
///
/// # Safety
/// `init` is a binding library's `gos_binding_init`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_init(
    init: unsafe extern "C" fn(*const HostTable) -> *const c_void,
) {
    // SAFETY: the table is a static, as `init` requires.
    let _ = unsafe { init(&raw const HOST_TABLE) };
}

/// Raises the panic a binding call on this thread recorded, if it recorded
/// one. A compiled caller calls this right after each binding call returns.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_binding_raise_pending() {
    if PANIC_RECORDED.get() {
        raise_recorded();
    }
}

#[cold]
#[inline(never)]
fn raise_recorded() {
    PANIC_RECORDED.set(false);
    if let Some(message) = PENDING_PANIC.with(|pending| pending.borrow_mut().take()) {
        super::panic::panic_text(&message);
    }
}

/// Raises `message` as a panic of the running goroutine, for a binding call
/// that cannot be made.
pub fn raise_now(message: &str) {
    super::panic::panic_text(message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_panic_is_taken_once() {
        // SAFETY: the message is a live local.
        unsafe { raise_panic(StrRef::new("no such window")) };
        assert!(PANIC_RECORDED.get(), "a recorded panic raises on return");
        let taken = PENDING_PANIC.with(|pending| pending.borrow_mut().take());
        assert_eq!(taken.as_deref(), Some("no such window"));
        assert!(PENDING_PANIC.with(|pending| pending.borrow().is_none()));
    }

    #[test]
    fn host_memory_matches_the_layout_the_runtime_frees() {
        // SAFETY: a fresh allocation freed by the matching `Box`.
        unsafe {
            let p = alloc(std::mem::size_of::<u64>(), std::mem::align_of::<u64>()).cast::<u64>();
            p.write(7);
            assert_eq!(*Box::from_raw(p), 7);
        }
    }
}
