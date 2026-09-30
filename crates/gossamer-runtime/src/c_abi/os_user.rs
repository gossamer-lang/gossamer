#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
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
#![allow(clippy::wildcard_imports)]

// The C-ABI signatures below return `*mut c_char` on every platform; only the
// lookup they wrap is unix-specific.
use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// os::user - POSIX user / group lookup. Unix uses `nix`; on
// Windows everything falls back to USERNAME / USERPROFILE env vars.
// ---------------------------------------------------------------

/// Login name of the current process user, or empty string.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_os_user_current_name() -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        #[cfg(unix)]
        let name = {
            use nix::unistd::{Uid, User};
            match User::from_uid(Uid::current()) {
                Ok(Some(u)) => u.name,
                _ => std::env::var("USER").unwrap_or_default(),
            }
        };
        #[cfg(not(unix))]
        let name = std::env::var("USERNAME").unwrap_or_default();
        alloc_cstring(name.as_bytes())
    })
}

/// uid of the current process user, or -1 on non-unix.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_os_user_current_uid() -> i64 {
    ffi_entry!(-1, {
        #[cfg(unix)]
        {
            use nix::unistd::Uid;
            i64::from(Uid::current().as_raw())
        }
        #[cfg(not(unix))]
        {
            -1_i64
        }
    })
}

/// gid of the current process user, or -1 on non-unix.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_os_user_current_gid() -> i64 {
    ffi_entry!(-1, {
        #[cfg(unix)]
        {
            use nix::unistd::Gid;
            i64::from(Gid::current().as_raw())
        }
        #[cfg(not(unix))]
        {
            -1_i64
        }
    })
}

/// Home directory of the current process user.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_os_user_current_home() -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        #[cfg(unix)]
        let home = {
            use nix::unistd::{Uid, User};
            match User::from_uid(Uid::current()) {
                Ok(Some(u)) => u.dir.to_string_lossy().into_owned(),
                _ => std::env::var("HOME").unwrap_or_default(),
            }
        };
        #[cfg(not(unix))]
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        alloc_cstring(home.as_bytes())
    })
}

/// Login name for `uid`, or empty string. Unix-only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_user_lookup_uid(uid: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        #[cfg(unix)]
        let name = {
            use nix::unistd::{Uid, User};
            match u32::try_from(uid)
                .ok()
                .and_then(|raw| User::from_uid(Uid::from_raw(raw)).ok().flatten())
            {
                Some(u) => u.name,
                None => String::new(),
            }
        };
        #[cfg(not(unix))]
        let name = {
            let _ = uid;
            String::new()
        };
        alloc_cstring(name.as_bytes())
    })
}

/// uid for user `name`, or -1 if not found.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_user_lookup_name(name: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if name.is_null() {
            return -1;
        }
        #[cfg(unix)]
        {
            // SAFETY: `name` is a String argument from compiled code, null or a live string body for the whole call.
            let n = unsafe { crate::c_abi::gos_str_arg_text(name) };
            use nix::unistd::User;
            match User::from_name(n) {
                Ok(Some(u)) => i64::from(u.uid.as_raw()),
                _ => -1,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            -1_i64
        }
    })
}
