//! C-ABI dispatch shims for `std::net::ip`. Mirrors the bytecode-VM
//! builtins in `gossamer-interp/src/stdlib_builtins/net_ip.rs` so the
//! compiled (Cranelift / LLVM) tier resolves the same calls natively
//! instead of failing to link.
//!
//! `std::net::ip` is a thin, stateless wrapper over Rust's
//! `std::net::{IpAddr, Ipv4Addr, Ipv6Addr}` (see
//! `gossamer-std/src/net/ip.rs`). Every classification predicate
//! (`is_valid` / `is_v4` / `is_v6` / `is_loopback` / `is_private` /
//! `is_multicast` / `is_unspecified`) takes a c-string IP and returns
//! an i64 truthiness, identical in shape and result to the already-wired
//! `gos_rt_netip_*` family - so those reuse the `netip` shims directly
//! (see the stdlib_free dispatch). The only genuinely new shapes are:
//!
//! - `parse(s) -> Result<Ip, Error>`: on the compiled tier the `Ip`
//!   payload is represented as its canonical string form (the fixture
//!   surface only observes the Ok/Err discriminant), so this returns a
//!   packed `Result<String, errors::Error>`.
//! - `octets(ip) -> [u8]`: the 4 (v4) or 16 (v6) raw address bytes as a
//!   `Vec<u8>`; empty on parse failure (matches the VM builtin).

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::wildcard_imports)]

use std::net::IpAddr;
use std::os::raw::c_char;

/// # Safety
///
/// `p` is null or a live string body.
unsafe fn cstr_to_str(p: *const c_char) -> String {
    // SAFETY: this function's contract is the one the reader states for `p`.
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

/// Packs an `Err(errors::Error)` for the Result-returning `net::ip`
/// shims, matching the `gos_rt_result_new(1, error_ptr)` convention.
fn ip_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    super::vec::gos_rt_result_new(1, err as i64)
}

/// `net::ip::parse(s) -> Result<Ip, errors::Error>` - the compiled-tier
/// `Ip` payload is its canonical string form. `Err` mirrors the VM's
/// `net::ip: <reason>` message.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_net_ip_parse(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `s` is this shim's argument, as `cstr_to_str` requires (C-ABI contract).
        let text = unsafe { cstr_to_str(s) };
        match text.parse::<IpAddr>() {
            Ok(ip) => {
                let canon = super::string::alloc_cstring(ip.to_string().as_bytes());
                super::vec::gos_rt_result_new(0, canon as i64)
            }
            Err(e) => ip_err(&format!("net::ip: {e}")),
        }
    })
}

/// `net::ip::octets(ip) -> [u8]` - raw address bytes (4 for v4, 16 for
/// v6). Empty vector on parse failure. The argument is the canonical
/// string produced by `parse` (or any valid IP literal).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_net_ip_octets(s: *const c_char) -> *mut super::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `s` is this shim's argument, as `cstr_to_str` requires (C-ABI contract).
        let text = unsafe { cstr_to_str(s) };
        let bytes: Vec<u8> = match text.parse::<IpAddr>() {
            Ok(IpAddr::V4(v4)) => v4.octets().to_vec(),
            Ok(IpAddr::V6(v6)) => v6.octets().to_vec(),
            Err(_) => Vec::new(),
        };
        super::encoding::bytes_to_gosvec(&bytes)
    })
}
