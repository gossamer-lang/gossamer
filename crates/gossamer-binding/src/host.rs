//! The host's [`HostTable`], which `gos_binding_init` installs before any
//! item runs, and the runtime functions this crate reaches through it.

#![allow(unsafe_code, reason = "calls through the host's C function table")]

use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicPtr, Ordering};

use gossamer_binding_abi::{ABI_VERSION, AbiValue, CallCtx, CallOut, HostTable, StrRef, abi_major};

static HOST: AtomicPtr<HostTable> = AtomicPtr::new(std::ptr::null_mut());

/// Why a host table was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallError {
    /// The table pointer was null.
    Missing,
    /// The host speaks another major version of the ABI.
    Version(u32),
    /// The table is smaller than the fields this crate reads.
    Truncated,
}

/// Keeps `host` for the life of the process.
///
/// # Safety
/// `host` is null or points at a table that lives as long as the process.
pub(crate) unsafe fn install(host: *const HostTable) -> Result<(), InstallError> {
    if host.is_null() {
        return Err(InstallError::Missing);
    }
    // SAFETY: the caller's contract; the version and size are read before
    // any other field.
    let (version, size) = unsafe { ((*host).abi_version, (*host).size) };
    if abi_major(version) != abi_major(ABI_VERSION) {
        return Err(InstallError::Version(version));
    }
    if (size as usize) < std::mem::size_of::<HostTable>() {
        return Err(InstallError::Truncated);
    }
    HOST.store(host.cast_mut(), Ordering::Release);
    // SAFETY: as above; the field exists, as the size check showed.
    if unsafe { (*host).vm_invoke }.is_some() {
        quiet_panics_inside_calls();
    }
    Ok(())
}

thread_local! {
    static IN_CALL: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Marks this thread as running a binding item until dropped.
pub(crate) struct InCall;

impl InCall {
    pub(crate) fn enter() -> Self {
        IN_CALL.with(|depth| depth.set(depth.get() + 1));
        Self
    }
}

impl Drop for InCall {
    fn drop(&mut self) {
        IN_CALL.with(|depth| depth.set(depth.get() - 1));
    }
}

/// A library `gos` loads has a standard library of its own, whose panic
/// hook only its own code reaches. A panic inside a binding item becomes a
/// Gossamer panic the host reports with its message, so the hook prints only
/// for a panic outside any call, as on a thread the binding started.
fn quiet_panics_inside_calls() {
    static QUIETED: std::sync::Once = std::sync::Once::new();
    QUIETED.call_once(|| {
        let report = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if IN_CALL.with(std::cell::Cell::get) == 0 {
                report(info);
            }
        }));
    });
}

/// The installed table.
///
/// # Panics
/// Panics when no host installed one, which only a call made before
/// `gos_binding_init` can see; the item's thunk reports the panic.
fn table() -> &'static HostTable {
    let host = HOST.load(Ordering::Acquire);
    assert!(
        !host.is_null(),
        "a binding item ran before its host called gos_binding_init"
    );
    // SAFETY: `install` stored a table that lives as long as the process.
    unsafe { &*host }
}

/// Allocates `size` bytes the runtime frees with the matching layout.
pub(crate) fn alloc(size: usize, align: usize) -> *mut u8 {
    // SAFETY: the host's allocator takes any size and a power-of-two align.
    unsafe { (table().alloc)(size, align) }
}

/// An empty runtime vector with room for `cap` elements of `elem_bytes`
/// each, whose elements are of `elem_kind`.
pub(crate) fn vec_with_capacity(elem_bytes: u32, cap: i64, elem_kind: u8) -> *mut c_void {
    // SAFETY: the runtime builds and owns the header it answers.
    unsafe { (table().vec_with_capacity)(elem_bytes, cap, elem_kind) }
}

/// A runtime string holding `text`, which the compiled caller owns.
pub(crate) fn str_new(text: &str) -> *mut c_char {
    // SAFETY: the runtime copies `text` before this returns.
    unsafe { (table().str_new)(StrRef::new(text)) }
}

/// Frees a runtime string a compiled callback answered.
///
/// # Safety
/// `text` is a string the runtime allocated and nothing else frees.
pub(crate) unsafe fn str_free(text: *mut c_char) {
    // SAFETY: the caller's contract.
    unsafe { (table().str_free)(text) }
}

/// Calls a compiled-tier callback handle; answers 0 on success.
///
/// # Safety
/// `args` points at `arg_count` wire values and `out` at a writable wire value.
pub(crate) unsafe fn callback_invoke(
    handle: u64,
    args: *const c_void,
    arg_count: u32,
    out: *mut c_void,
) -> i32 {
    // SAFETY: the caller's contract.
    unsafe { (table().callback_invoke)(handle, args, arg_count, out) }
}

/// Takes a share of a compiled-tier callback handle.
pub(crate) fn callback_retain(handle: u64) {
    // SAFETY: the runtime ignores a handle it does not know.
    unsafe { (table().callback_retain)(handle) }
}

/// Gives back a share of a compiled-tier callback handle.
pub(crate) fn callback_release(handle: u64) {
    // SAFETY: as for `callback_retain`.
    unsafe { (table().callback_release)(handle) }
}

/// Records a wire thunk's panic for its compiled caller to raise.
pub(crate) fn raise_panic(message: &str) {
    // SAFETY: the host copies the message before this returns.
    unsafe { (table().raise_panic)(StrRef::new(message)) }
}

/// Runs `f` as a blocking native call the scheduler works around.
pub(crate) fn blocking<R>(f: impl FnOnce() -> R) -> R {
    struct Leave(u64);
    impl Drop for Leave {
        fn drop(&mut self) {
            // SAFETY: the token is the one `blocking_enter` answered.
            unsafe { (table().blocking_leave)(self.0) }
        }
    }
    // SAFETY: entering a blocking region has no precondition.
    let _leave = Leave(unsafe { (table().blocking_enter)() });
    f()
}

/// Calls a VM callback handle through the running VM call `ctx`, answering
/// the status and the host-owned answer.
///
/// # Safety
/// `ctx` is the context of the VM call running on this thread, and `args`
/// points at `arg_count` values that stay alive for the call.
pub(crate) unsafe fn vm_invoke(
    ctx: *mut CallCtx,
    handle: u64,
    args: *const AbiValue,
    arg_count: usize,
    out: &mut CallOut,
) -> Option<u32> {
    let invoke = table().vm_invoke?;
    // SAFETY: the caller's contract.
    Some(unsafe { invoke(ctx, handle, args, arg_count, out) })
}

/// Hands an answer of [`vm_invoke`] back to the host.
pub(crate) fn vm_free_out(out: &CallOut) {
    if out.owner.is_null() {
        return;
    }
    if let Some(free) = table().vm_free_out {
        // SAFETY: `owner` is what the host's `vm_invoke` wrote.
        unsafe { free(out.owner) }
    }
}

/// Takes a share of a VM callback handle.
pub(crate) fn vm_callback_retain(handle: u64) {
    if let Some(retain) = table().vm_callback_retain {
        // SAFETY: the host ignores a handle it does not know.
        unsafe { retain(handle) }
    }
}

/// Gives back a share of a VM callback handle.
pub(crate) fn vm_callback_release(handle: u64) {
    if let Some(release) = table().vm_callback_release {
        // SAFETY: as for `vm_callback_retain`.
        unsafe { release(handle) }
    }
}
