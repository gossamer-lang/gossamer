//! A foreign C string is measured without reading the byte before it.
//!
//! Gossamer strings carry their length in a header behind the body, and a
//! shim reaching that header selects it by the body's low-bit shape. A
//! pointer a host API produced has no such header, and the bytes before it
//! belong to whoever allocated it: when the string begins an OS mapping,
//! reading one byte earlier faults. The string here is placed at the start of
//! a mapping whose preceding page is unreadable, so any backwards probe is a
//! hard fault rather than a silently wrong length.

#![allow(missing_docs)]

use std::ffi::c_char;

use gossamer_runtime::c_abi::{gos_rt_str_byte_len, gos_rt_str_is_empty, gos_rt_str_len};

const TEXT: &[u8] = b"# pprof text format\0";

/// A readable page whose preceding page is mapped without access.
struct GuardedPage {
    base: *mut u8,
    len: usize,
}

impl GuardedPage {
    /// The first readable byte, which is also the first byte of its mapping.
    fn text_start(&self) -> *const c_char {
        // SAFETY: `base` maps `len` bytes and the readable half starts at
        // `len / 2`.
        unsafe { self.base.add(self.len / 2).cast::<c_char>() }
    }
}

impl Drop for GuardedPage {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: releases exactly the mapping this value owns.
        unsafe {
            libc::munmap(self.base.cast(), self.len);
        }
        #[cfg(windows)]
        // SAFETY: releases exactly the reservation this value owns; a
        // `MEM_RELEASE` free takes the base address and a zero size.
        unsafe {
            windows_sys::Win32::System::Memory::VirtualFree(
                self.base.cast(),
                0,
                windows_sys::Win32::System::Memory::MEM_RELEASE,
            );
        }
    }
}

#[cfg(unix)]
fn guarded_page() -> GuardedPage {
    // SAFETY: a fresh anonymous mapping is requested and its first page is
    // then dropped to no access; both calls are checked below.
    unsafe {
        let page = libc::sysconf(libc::_SC_PAGESIZE) as usize;
        let base = libc::mmap(
            std::ptr::null_mut(),
            page * 2,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        assert!(base != libc::MAP_FAILED, "mmap two pages");
        assert_eq!(
            libc::mprotect(base, page, libc::PROT_NONE),
            0,
            "revoke access to the leading page"
        );
        let guarded = GuardedPage {
            base: base.cast::<u8>(),
            len: page * 2,
        };
        std::ptr::copy_nonoverlapping(
            TEXT.as_ptr(),
            guarded.text_start().cast_mut().cast::<u8>(),
            TEXT.len(),
        );
        guarded
    }
}

#[cfg(windows)]
fn guarded_page() -> GuardedPage {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };

    const PAGE: usize = 4096;
    // SAFETY: two pages are reserved and only the trailing one is committed,
    // so the leading page has no backing to read; both calls are checked.
    unsafe {
        let base = VirtualAlloc(std::ptr::null(), PAGE * 2, MEM_RESERVE, PAGE_READWRITE);
        assert!(!base.is_null(), "reserve two pages");
        let readable = VirtualAlloc(
            base.cast::<u8>().add(PAGE).cast(),
            PAGE,
            MEM_COMMIT,
            PAGE_READWRITE,
        );
        assert!(!readable.is_null(), "commit the trailing page");
        let guarded = GuardedPage {
            base: base.cast::<u8>(),
            len: PAGE * 2,
        };
        std::ptr::copy_nonoverlapping(
            TEXT.as_ptr(),
            guarded.text_start().cast_mut().cast::<u8>(),
            TEXT.len(),
        );
        guarded
    }
}

#[test]
#[cfg_attr(
    miri,
    ignore = "the guard page is made with `mprotect`, which Miri cannot model"
)]
fn measuring_a_foreign_string_never_reads_behind_it() {
    let guarded = guarded_page();
    let s = guarded.text_start();
    let content = TEXT.len() - 1;
    // SAFETY: `s` addresses a NUL-terminated buffer inside the readable page.
    unsafe {
        assert_eq!(gos_rt_str_byte_len(s), content as i64);
        assert_eq!(gos_rt_str_len(s), content as i64);
        assert!(!gos_rt_str_is_empty(s));
    }
}
