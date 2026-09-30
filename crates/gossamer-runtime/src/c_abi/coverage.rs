//! C-ABI shim for coverage instrumentation. Codegen emits one
//! call to `gos_rt_cov_record(file, line, branch)` per basic
//! block entry when `gos test --coverage` is in effect. The
//! shim looks up (or lazily registers) the matching counter and
//! bumps it.

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]

use std::os::raw::c_char;

unsafe fn cstr_to_string(p: *const c_char) -> String {
    // SAFETY: this `unsafe fn`'s caller passes `p` live or null, which `gos_str_arg_string`
    // accepts.
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

/// Records one hit at `(file, line, branch)`. NULL `file` is
/// tolerated (renders as empty string) so codegen can elide the
/// argument when the source map has no entry. `branch` is `0`
/// for sequential statements and the arm index (1..N) for
/// match arms / if branches.
///
/// Cheap: a single load of the global enable flag plus an
/// `AtomicU64::fetch_add` when coverage is on. The codegen
/// pre-registers each slot at compile time, so the runtime path
/// avoids the registration hash lookup.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_cov_record(file: *const c_char, line: u32, branch: u32) {
    if !crate::coverage::enabled() {
        return;
    }
    // SAFETY: `file` is this shim's argument, null or a live string body for the call (C-ABI
    // contract), which `cstr_to_string` accepts.
    let file_str = unsafe { cstr_to_string(file) };
    let _ = crate::coverage::record(&file_str, line, branch);
}

/// Bumps a pre-registered counter slot. Codegen prefers this
/// over `gos_rt_cov_record` when it has cached the slot index
/// at compile time (one `fetch_add` per call).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_cov_bump(idx: u64) {
    if !crate::coverage::enabled() {
        return;
    }
    crate::coverage::bump(usize::try_from(idx).unwrap_or(usize::MAX));
}

/// Resets every counter back to zero. The test runner calls this
/// before invoking a test program so cumulative hit counts don't
/// bleed across runs.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_cov_reset() {
    crate::coverage::reset();
}

/// Enables or disables coverage instrumentation. `0 = disable`,
/// any other value enables.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_cov_set_enabled(flag: i32) {
    crate::coverage::set_enabled(flag != 0);
}
