//! Calling binding items from a binding crate's own tests.

#![allow(unsafe_code, reason = "a host table of C functions")]

use std::alloc::Layout;
use std::ffi::c_void;
use std::os::raw::c_char;

use gossamer_binding_abi::{ABI_VERSION, HostTable, StrRef};

use crate::value::{RuntimeError, RuntimeResult, Value};
unsafe extern "C" fn alloc(size: usize, align: usize) -> *mut u8 {
    let Ok(layout) = Layout::from_size_align(size.max(1), align) else {
        return std::ptr::null_mut();
    };
    // SAFETY: the layout has a non-zero size.
    unsafe { std::alloc::alloc_zeroed(layout) }
}

unsafe extern "C" fn dealloc(ptr: *mut u8, size: usize, align: usize) {
    if let Ok(layout) = Layout::from_size_align(size.max(1), align) {
        // SAFETY: `alloc` made `ptr` with this layout.
        unsafe { std::alloc::dealloc(ptr, layout) }
    }
}

unsafe extern "C" fn vec_with_capacity(elem_bytes: u32, cap: i64, _elem_kind: u8) -> *mut c_void {
    let bytes = usize::try_from(cap.max(1)).unwrap_or(1) * elem_bytes.max(1) as usize;
    // SAFETY: any size and an 8-byte alignment are valid.
    let ptr = unsafe { alloc(bytes, 8) };
    let header = Box::new(crate::native::GosVec {
        len: 0,
        cap,
        elem_bytes,
        ptr,
    });
    Box::into_raw(header).cast()
}

unsafe extern "C" fn str_new(text: StrRef) -> *mut c_char {
    // SAFETY: the caller's text lives for this call; the copy is leaked, as
    // a test host frees nothing.
    let bytes = unsafe { text.as_bytes() };
    std::ffi::CString::new(
        bytes
            .iter()
            .copied()
            .filter(|b| *b != 0)
            .collect::<Vec<u8>>(),
    )
    .map_or(std::ptr::null_mut(), std::ffi::CString::into_raw)
}

unsafe extern "C" fn str_free(_s: *mut c_char) {}

unsafe extern "C" fn callback_invoke(
    _handle: u64,
    _args: *const c_void,
    _arg_count: u32,
    _out: *mut c_void,
) -> i32 {
    -1
}

unsafe extern "C" fn callback_share(_handle: u64) {}

unsafe extern "C" fn raise_panic(_message: StrRef) {}

unsafe extern "C" fn blocking_enter() -> u64 {
    0
}

unsafe extern "C" fn blocking_leave(_token: u64) {}

static TABLE: HostTable = HostTable {
    abi_version: ABI_VERSION,
    size: std::mem::size_of::<HostTable>() as u32,
    alloc,
    dealloc,
    vec_with_capacity,
    str_new,
    str_free,
    callback_invoke,
    callback_retain: callback_share,
    callback_release: callback_share,
    raise_panic,
    blocking_enter,
    blocking_leave,
    vm_invoke: None,
    vm_free_out: None,
    vm_callback_retain: None,
    vm_callback_release: None,
};

/// Installs a host table over the global allocator, so a binding crate's
/// own tests can call its items without `gos`. Compiled callbacks and VM
/// callbacks are unavailable under it.
pub fn install_host() {
    // SAFETY: the table is a static.
    let installed = unsafe { crate::host::install(&raw const TABLE) };
    assert!(installed.is_ok(), "the testing host table was refused");
}

/// Calls the item `qualified` (`module::item`) through its VM entry with
/// `args`, as `gos run` does, after installing the testing host.
pub fn call(qualified: &str, args: &[Value]) -> RuntimeResult<Value> {
    install_host();
    let Some((_, item)) = crate::item(qualified) else {
        return Err(RuntimeError::Type(format!("no binding item {qualified}")));
    };
    // A position that crosses as text takes and answers the text the
    // program's walk writes; a test passes and reads the values themselves.
    let Some(text) = item.wire_text else {
        return crate::abi::call_vm_item(item.vm, args);
    };
    let mut wired = Vec::with_capacity(args.len());
    for (arg, to_text) in args.iter().zip(text.args) {
        wired.push(to_text(arg)?);
    }
    (text.ret)(&crate::abi::call_vm_item(item.vm, &wired)?)
}
