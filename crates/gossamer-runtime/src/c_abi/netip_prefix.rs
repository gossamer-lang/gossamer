//! C-ABI leaves under the `netip::Prefix` wrapper source, over
//! `gossamer_runtime::netip_prefix`, the code the VM calls.

use std::os::raw::c_char;

use super::result::gos_rt_result_new;
use super::string::alloc_cstring;

fn err(message: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
    gos_rt_result_new(1, err as i64)
}

/// The text of argument `s`.
///
/// # Safety
/// `s` is null or a live string body for the call.
unsafe fn text<'a>(s: *const c_char) -> &'a str {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null for the call.
    unsafe { crate::c_abi::gos_str_arg_text(s) }
}

/// Layout of the `(String, i64)` parse answer: the address string is owned
/// by the blob.
static ADDR_LEN_META: [i64; 5] = [
    gossamer_abi::rc::RC_KIND_STRUCT,
    1,
    0,
    1,
    gossamer_abi::rc::RC_CHILD_RC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT,
];

/// `__gos_netip_prefix_raw(text) -> Result<(String, i64), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_prefix_raw(text_arg: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `text_arg` is this shim's String argument, live for the call or null.
        match crate::netip_prefix::parse(unsafe { text(text_arg) }) {
            Ok((addr, len)) => {
                let words = [alloc_cstring(addr.as_bytes()) as i64, len];
                let blob = crate::c_abi::rc::counted_words(&words, &ADDR_LEN_META);
                gos_rt_result_new(0, blob as i64)
            }
            Err(message) => err(&message),
        }
    })
}

/// `__gos_netip_prefix_op_raw(op, addr, len) -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_prefix_op_raw(
    op: i64,
    addr: *const c_char,
    len: i64,
) -> i128 {
    ffi_entry!({
        // SAFETY: `addr` is this shim's String argument, live for the call or null.
        match crate::netip_prefix::op(op, unsafe { text(addr) }, len) {
            Ok(answer) => gos_rt_result_new(0, alloc_cstring(answer.as_bytes()) as i64),
            Err(message) => err(&message),
        }
    })
}

/// `__gos_netip_prefix_contains_raw(addr, len, candidate) -> Result<bool, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_prefix_contains_raw(
    addr: *const c_char,
    len: i64,
    candidate: *const c_char,
) -> i128 {
    ffi_entry!({
        // SAFETY: `addr` and `candidate` are this shim's String arguments, live for the call or null.
        let inside =
            crate::netip_prefix::contains(unsafe { text(addr) }, len, unsafe { text(candidate) });
        gos_rt_result_new(0, i64::from(inside))
    })
}

/// `__gos_netip_prefix_overlaps_raw(a, a_len, b, b_len) -> Result<bool, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_netip_prefix_overlaps_raw(
    a: *const c_char,
    a_len: i64,
    b: *const c_char,
    b_len: i64,
) -> i128 {
    ffi_entry!({
        // SAFETY: `a` and `b` are this shim's String arguments, live for the call or null.
        let shared =
            crate::netip_prefix::overlaps(unsafe { text(a) }, a_len, unsafe { text(b) }, b_len);
        gos_rt_result_new(0, i64::from(shared))
    })
}
