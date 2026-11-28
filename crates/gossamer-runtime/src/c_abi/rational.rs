//! C-ABI leaves under the `math::Rational` wrapper source, over
//! `gossamer_runtime::rational`, the code the VM calls.

use std::os::raw::c_char;

use super::result::gos_rt_result_new;
use super::string::alloc_cstring;

fn err(message: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
    gos_rt_result_new(1, err as i64)
}

/// `__gos_rational_from_f64_raw(x) -> Result<(String, String), Error>`: the
/// exact value of `x` as numerator and denominator text.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_rational_from_f64_raw(x: f64) -> i128 {
    ffi_entry!({
        match crate::rational::from_f64(x) {
            Ok(pair) => super::result::ok_string_pair(pair),
            Err(message) => err(&message),
        }
    })
}

/// `__gos_rational_to_f64_raw(numerator, denominator) -> Result<String, Error>`:
/// the nearest `f64`, in the shortest text that reads back as it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rational_to_f64_raw(
    numerator: *const c_char,
    denominator: *const c_char,
) -> i128 {
    ffi_entry!({
        // SAFETY: both are this shim's String arguments, live for the call or null.
        let (numerator, denominator) = unsafe {
            (
                crate::c_abi::gos_str_arg_text(numerator),
                crate::c_abi::gos_str_arg_text(denominator),
            )
        };
        match crate::rational::to_f64(numerator, denominator) {
            Ok(value) => {
                gos_rt_result_new(0, alloc_cstring(format!("{value:?}").as_bytes()) as i64)
            }
            Err(message) => err(&message),
        }
    })
}
