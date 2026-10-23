//! Opening a shared library and finding a symbol in it. A library stays open
//! for the life of the process: its items are called until the process ends.

#![allow(unsafe_code, reason = "the platform's dynamic loader")]

use std::ffi::c_void;
use std::path::Path;

/// An open shared library.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Handle(*mut c_void);

// SAFETY: a loader handle is a process-wide token the loader's functions
// accept from any thread.
unsafe impl Send for Handle {}
// SAFETY: as above.
unsafe impl Sync for Handle {}

/// Opens the library at `path`, answering the loader's message on failure.
#[cfg(unix)]
pub(crate) fn open(path: &Path) -> Result<Handle, String> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    // SAFETY: `bytes` is a NUL-terminated path. `RTLD_LOCAL` keeps the
    // library's symbols out of the process's global namespace, so two
    // libraries never resolve each other's.
    let handle = unsafe { libc::dlopen(bytes.as_ptr().cast(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(last_error());
    }
    Ok(Handle(handle))
}

/// The address of `symbol` in `handle`, if it exports one.
#[cfg(unix)]
pub(crate) fn symbol(handle: Handle, symbol: &str) -> Option<*const c_void> {
    let mut name = symbol.as_bytes().to_vec();
    name.push(0);
    // SAFETY: `handle` is open and `name` is NUL-terminated.
    let addr = unsafe { libc::dlsym(handle.0, name.as_ptr().cast()) };
    (!addr.is_null()).then_some(addr.cast_const())
}

#[cfg(unix)]
fn last_error() -> String {
    // SAFETY: `dlerror` answers null or a NUL-terminated message owned by the
    // loader, read before any other loader call.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        return "the dynamic loader gave no reason".to_string();
    }
    // SAFETY: as above.
    unsafe { std::ffi::CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned()
}

/// Opens the library at `path`, answering the loader's message on failure.
#[cfg(windows)]
pub(crate) fn open(path: &Path) -> Result<Handle, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path.
    let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(Handle(handle))
}

/// The address of `symbol` in `handle`, if it exports one.
#[cfg(windows)]
pub(crate) fn symbol(handle: Handle, symbol: &str) -> Option<*const c_void> {
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    let mut name = symbol.as_bytes().to_vec();
    name.push(0);
    // SAFETY: `handle` is a loaded module and `name` is NUL-terminated.
    let addr = unsafe { GetProcAddress(handle.0, name.as_ptr()) }?;
    Some(addr as *const c_void)
}

/// Other targets cannot load a library.
#[cfg(not(any(unix, windows)))]
pub(crate) fn open(_path: &Path) -> Result<Handle, String> {
    Err("this target cannot load a shared library".to_string())
}

/// Other targets cannot load a library.
#[cfg(not(any(unix, windows)))]
pub(crate) fn symbol(_handle: Handle, _symbol: &str) -> Option<*const c_void> {
    None
}
