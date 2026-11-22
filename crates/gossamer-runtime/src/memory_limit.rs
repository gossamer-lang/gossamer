//! The most memory one buffer can be given, and the panic for a buffer past
//! it.
//!
//! A buffer the machine cannot back is refused when it is asked for, with a
//! panic every tier words the same way, rather than mapped lazily and killed
//! by the system once it is filled. The limit is what the system can commit:
//! physical memory plus swap on Linux, the commit limit on Windows. Elsewhere
//! no limit is read and only the allocator's own refusal applies.

use std::sync::OnceLock;

/// Bytes the system can commit to this process at most, or `None` where the
/// platform does not say.
#[must_use]
pub fn commit_limit() -> Option<u64> {
    static LIMIT: OnceLock<Option<u64>> = OnceLock::new();
    *LIMIT.get_or_init(read_commit_limit)
}

/// Whether a buffer of `bytes` is past what the system can ever back.
#[must_use]
pub fn exceeds_commit_limit(bytes: u128) -> bool {
    commit_limit().is_some_and(|limit| bytes > u128::from(limit))
}

/// The panic message for a buffer of `bytes` the system cannot back.
#[must_use]
pub fn out_of_memory_message(bytes: u128) -> String {
    format!("out of memory: cannot allocate {bytes} bytes")
}

// Linux reports physical memory and swap in units of `mem_unit` bytes.
#[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
fn read_commit_limit() -> Option<u64> {
    // SAFETY: `sysinfo` fills the zeroed struct it is handed and reads nothing else.
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid, writable `sysinfo` struct.
    if unsafe { libc::sysinfo(&raw mut info) } != 0 {
        return None;
    }
    // The fields are as wide as a C `unsigned long`, so the sum is taken in a
    // type wider than either width.
    let total =
        (u128::from(info.totalram) + u128::from(info.totalswap)) * u128::from(info.mem_unit.max(1));
    u64::try_from(total).ok()
}

// Windows reports the commit limit, physical memory plus the page file, as
// the total page file size.
#[cfg(windows)]
fn read_commit_limit() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: an all-zero `MEMORYSTATUSEX` is a valid value; its length is set below.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).ok()?;
    // SAFETY: `status` is a valid, writable struct whose length field is set.
    if unsafe { GlobalMemoryStatusEx(&raw mut status) } == 0 {
        return None;
    }
    Some(status.ullTotalPageFile)
}

// Other platforms grow swap on demand or report no total; only the
// allocator's refusal bounds a buffer there.
#[cfg(any(miri, not(any(target_os = "linux", target_os = "android", windows))))]
fn read_commit_limit() -> Option<u64> {
    None
}
