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

use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// net::netip - typed IP address parsing / classification. All
// inputs are c-string IPs ("127.0.0.1", "::1", "addr:port"); outputs
// are scalars (i64 truthiness or i64 component) or canonical-form
// c-strings. Backed by std::net::IpAddr / SocketAddr.
// ---------------------------------------------------------------

/// # Safety
///
/// `s` is null or a live string body.
unsafe fn netip_parse_ip(s: *const c_char) -> Option<std::net::IpAddr> {
    if s.is_null() {
        return None;
    }
    // SAFETY: this `unsafe fn`'s caller passes `s` live; non-null, checked above.
    std::str::from_utf8(unsafe { crate::c_abi::gos_str_arg_bytes(s) })
        .ok()?
        .parse()
        .ok()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_is_valid(s: *const c_char) -> i64 {
    // SAFETY: `s` is this shim's argument, as `netip_parse_ip` requires (C-ABI contract).
    ffi_entry!(0, { i64::from(unsafe { netip_parse_ip(s) }.is_some()) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_is_v4(s: *const c_char) -> i64 {
    ffi_entry!(0, {
        // SAFETY: `s` is this shim's argument, as `netip_parse_ip` requires (C-ABI contract).
        i64::from(matches!(
            unsafe { netip_parse_ip(s) },
            Some(std::net::IpAddr::V4(_))
        ))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_is_v6(s: *const c_char) -> i64 {
    ffi_entry!(0, {
        // SAFETY: `s` is this shim's argument, as `netip_parse_ip` requires (C-ABI contract).
        i64::from(matches!(
            unsafe { netip_parse_ip(s) },
            Some(std::net::IpAddr::V6(_))
        ))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_is_loopback(s: *const c_char) -> i64 {
    ffi_entry!(0, {
        // SAFETY: `s` is this shim's argument, as `netip_parse_ip` requires (C-ABI contract).
        i64::from(unsafe { netip_parse_ip(s) }.is_some_and(|ip| ip.is_loopback()))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_is_unspecified(s: *const c_char) -> i64 {
    ffi_entry!(0, {
        // SAFETY: `s` is this shim's argument, as `netip_parse_ip` requires (C-ABI contract).
        i64::from(unsafe { netip_parse_ip(s) }.is_some_and(|ip| ip.is_unspecified()))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_is_multicast(s: *const c_char) -> i64 {
    ffi_entry!(0, {
        // SAFETY: `s` is this shim's argument, as `netip_parse_ip` requires (C-ABI contract).
        i64::from(unsafe { netip_parse_ip(s) }.is_some_and(|ip| ip.is_multicast()))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_is_private(s: *const c_char) -> i64 {
    ffi_entry!(0, {
        // SAFETY: `s` is this shim's argument, as `netip_parse_ip` requires (C-ABI contract).
        let priv_ = match unsafe { netip_parse_ip(s) } {
            Some(std::net::IpAddr::V4(v4)) => v4.is_private(),
            Some(std::net::IpAddr::V6(v6)) => v6.segments()[0] & 0xfe00 == 0xfc00,
            None => false,
        };
        i64::from(priv_)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_normalize(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `s` is this shim's argument, as `netip_parse_ip` requires (C-ABI contract).
        let out = match unsafe { netip_parse_ip(s) } {
            Some(ip) => ip.to_string(),
            None => String::new(),
        };
        alloc_cstring(out.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_host_of(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s_str = unsafe { crate::c_abi::gos_str_arg_text(s) };
        let out = match s_str.parse::<std::net::SocketAddr>() {
            Ok(a) => a.ip().to_string(),
            Err(_) => String::new(),
        };
        alloc_cstring(out.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_port_of(s: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if s.is_null() {
            return -1;
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s_str = unsafe { crate::c_abi::gos_str_arg_text(s) };
        match s_str.parse::<std::net::SocketAddr>() {
            Ok(a) => i64::from(a.port()),
            Err(_) => -1,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_join_addr_port(
    host: *const c_char,
    port: i64,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if host.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `host` is a String argument from compiled code, null or a live string body for the whole call.
        let h = unsafe { crate::c_abi::gos_str_arg_text(host) };
        let out = match (h.parse::<std::net::IpAddr>(), u16::try_from(port)) {
            (Ok(ip), Ok(p)) => std::net::SocketAddr::new(ip, p).to_string(),
            _ => String::new(),
        };
        alloc_cstring(out.as_bytes())
    })
}
