#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_lossless)]

//! `std::encoding::utf16` C-ABI shims. Mirrors
//! `gossamer_std::encoding::utf16`; UTF-16 code-unit vectors cross
//! the ABI as i64-per-element `GosVec`s (the compiled-tier narrow
//! integer Vec representation).

use std::os::raw::c_char;

const HIGH_MIN: u16 = 0xD800;
const HIGH_MAX: u16 = 0xDBFF;
const LOW_MIN: u16 = 0xDC00;
const LOW_MAX: u16 = 0xDFFF;

/// `encoding::utf16::is_surrogate(r) -> bool`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_utf16_is_surrogate(r: i64) -> i32 {
    let r = r as u16;
    i32::from((0xD800..=0xDFFF).contains(&r))
}

/// `encoding::utf16::rune_len(r) -> i64` (1 for BMP, 2 for astral).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_utf16_rune_len(r: i32) -> i64 {
    if (r as u32) < 0x10000 { 1 } else { 2 }
}

/// `encoding::utf16::decode_surrogate_pair(high, low) -> Option<char>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_utf16_decode_surrogate_pair(high: i64, low: i64) -> i128 {
    ffi_entry!(0i128, {
        let high = high as u16;
        let low = low as u16;
        let pair_ok = (HIGH_MIN..=HIGH_MAX).contains(&high) && (LOW_MIN..=LOW_MAX).contains(&low);
        let ch = if pair_ok {
            let cp = 0x10000 + u32::from(high - HIGH_MIN) * 0x400 + u32::from(low - LOW_MIN);
            char::from_u32(cp)
        } else {
            None
        };
        match ch {
            Some(c) => super::vec::gos_rt_result_new(0, i64::from(c as u32)),
            None => super::vec::gos_rt_result_new(1, 0),
        }
    })
}

/// `encoding::utf16::encode_string(s) -> [u16]` (i64-per-element).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf16_encode_string(s: *const c_char) -> *mut super::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        let units: Vec<u16> = text.encode_utf16().collect();
        let out = super::vec::gos_rt_vec_with_capacity(8, units.len() as i64);
        // SAFETY: `gos_rt_vec_with_capacity` answers a live header for a non-negative capacity,
        // owned here alone.
        let vref = unsafe { &mut *out };
        if !vref.ptr.is_null() {
            let dst = vref.ptr.as_ptr().cast::<i64>();
            for (i, u) in units.iter().enumerate() {
                // SAFETY: `i` is below `units.len()`, the capacity the vec was made with.
                unsafe { *dst.add(i) = i64::from(*u) };
            }
            vref.len = units.len() as i64;
        }
        out
    })
}

/// `encoding::utf16::decode_to_string(units) -> String` (lossy).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf16_decode_to_string(
    v: *const super::vec::GosVec,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `v` is this shim's argument, null or a live `Vec` (C-ABI contract). Each unit
        // reads at the width its header declares.
        let units: Vec<u16> = unsafe { super::vec::VecView::of(v) }
            .map_or_else(Vec::new, |vec| vec.words().map(|w| w as u16).collect());
        super::string::alloc_cstring(String::from_utf16_lossy(&units).as_bytes())
    })
}
