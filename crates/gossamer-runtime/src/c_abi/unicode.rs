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

//! C-ABI surface for `std::unicode` - general-category predicates,
//! casing helpers, normalization forms, and grapheme / word /
//! sentence segmentation. Compiled-tier programs call these through
//! the MIR `stdlib_free` dispatcher.
//!
//! Convention:
//! - `char` arguments arrive as `u32` (UTF-32 code points).
//! - `bool` results return `i64` (0/1) so the LLVM lowerer can
//!   truncate the i64 to i1 without an explicit cast in the IR.
//! - `String` arguments arrive as `*const c_char` (null-terminated
//!   UTF-8); `String` results return a fresh `*mut c_char` from
//!   [`alloc_cstring`].
//! - `Vec<String>` results return a `*mut GosVec` with
//!   `elem_kind = STRING`.

use std::os::raw::c_char;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;
use unicode_properties::{GeneralCategory, GeneralCategoryGroup, UnicodeGeneralCategory};
use unicode_segmentation::UnicodeSegmentation;

use crate::c_abi::string::alloc_cstring;
use crate::c_abi::vec::{GosVec, gos_rt_vec_new_typed, gos_rt_vec_push, vec_elem_kind};

#[inline]
fn char_from(u: u32) -> char {
    char::from_u32(u).unwrap_or('\u{FFFD}')
}

#[inline]
unsafe fn cstr_to_str<'a>(s: *const c_char) -> &'a str {
    if s.is_null() {
        return "";
    }
    // SAFETY: this `unsafe fn`'s caller passes `s` live; non-null, checked above.
    unsafe { crate::c_abi::gos_str_arg_text(s) }
}

fn alloc_string_vec(strings: Vec<String>) -> *mut GosVec {
    let v = gos_rt_vec_new_typed(8, vec_elem_kind::STRING);
    if v.is_null() {
        return v;
    }
    for s in strings {
        let cs = alloc_cstring(s.as_bytes());
        let cs_i64 = cs as i64;
        // SAFETY: `v` is the fresh non-null vec made above, and `cs_i64` is one 8-byte element.
        unsafe {
            gos_rt_vec_push(v, std::ptr::addr_of!(cs_i64).cast::<u8>());
        }
    }
    v
}

// ---------------------------------------------------------------
// General-category predicates
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_letter(c: u32) -> i64 {
    ffi_entry!({
        i64::from(matches!(
            char_from(c).general_category_group(),
            GeneralCategoryGroup::Letter
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_digit(c: u32) -> i64 {
    ffi_entry!({
        i64::from(matches!(
            char_from(c).general_category(),
            GeneralCategory::DecimalNumber
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_number(c: u32) -> i64 {
    ffi_entry!({
        i64::from(matches!(
            char_from(c).general_category_group(),
            GeneralCategoryGroup::Number
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_space(c: u32) -> i64 {
    ffi_entry!({
        let ch = char_from(c);
        if matches!(ch.general_category_group(), GeneralCategoryGroup::Separator) {
            return 1;
        }
        i64::from(matches!(
            ch,
            '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | '\u{0085}'
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_upper(c: u32) -> i64 {
    ffi_entry!({
        i64::from(matches!(
            char_from(c).general_category(),
            GeneralCategory::UppercaseLetter
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_lower(c: u32) -> i64 {
    ffi_entry!({
        i64::from(matches!(
            char_from(c).general_category(),
            GeneralCategory::LowercaseLetter
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_title(c: u32) -> i64 {
    ffi_entry!({
        i64::from(matches!(
            char_from(c).general_category(),
            GeneralCategory::TitlecaseLetter
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_punct(c: u32) -> i64 {
    ffi_entry!({
        i64::from(matches!(
            char_from(c).general_category_group(),
            GeneralCategoryGroup::Punctuation
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_symbol(c: u32) -> i64 {
    ffi_entry!({
        i64::from(matches!(
            char_from(c).general_category_group(),
            GeneralCategoryGroup::Symbol
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_mark(c: u32) -> i64 {
    ffi_entry!({
        i64::from(matches!(
            char_from(c).general_category_group(),
            GeneralCategoryGroup::Mark
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_print(c: u32) -> i64 {
    ffi_entry!({
        i64::from(!matches!(
            char_from(c).general_category_group(),
            GeneralCategoryGroup::Other
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_graphic(c: u32) -> i64 {
    ffi_entry!({
        let ch = char_from(c);
        let printable = !matches!(ch.general_category_group(), GeneralCategoryGroup::Other);
        let space = matches!(ch.general_category_group(), GeneralCategoryGroup::Separator)
            || matches!(
                ch,
                '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | '\u{0085}'
            );
        i64::from(printable && !space)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_control(c: u32) -> i64 {
    ffi_entry!({
        i64::from(matches!(
            char_from(c).general_category(),
            GeneralCategory::Control
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_is_assigned(c: u32) -> i64 {
    ffi_entry!({
        i64::from(!matches!(
            char_from(c).general_category(),
            GeneralCategory::Unassigned
        ))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_combining_class(c: u32) -> i64 {
    ffi_entry!({ i64::from(canonical_combining_class(char_from(c))) })
}

// ---------------------------------------------------------------
// Casing (single rune)
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_to_upper(c: u32) -> u32 {
    ffi_entry!({
        let ch = char_from(c);
        let up = ch.to_uppercase().next().unwrap_or(ch);
        up as u32
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_to_lower(c: u32) -> u32 {
    ffi_entry!({
        let ch = char_from(c);
        let lo = ch.to_lowercase().next().unwrap_or(ch);
        lo as u32
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_to_title(c: u32) -> u32 {
    ffi_entry!({
        let ch = char_from(c);
        ch.to_uppercase().next().unwrap_or(ch) as u32
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_simple_fold(c: u32) -> u32 {
    ffi_entry!({
        let ch = char_from(c);
        if ch.is_lowercase() {
            let up = ch.to_uppercase().next().unwrap_or(ch);
            if up != ch {
                return up as u32;
            }
        } else if ch.is_uppercase() {
            let lo = ch.to_lowercase().next().unwrap_or(ch);
            if lo != ch {
                return lo as u32;
            }
        }
        ch as u32
    })
}

// ---------------------------------------------------------------
// Casing (whole string - handles full mappings like ß -> SS)
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_to_upper_str(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        let out: String = text.chars().flat_map(char::to_uppercase).collect();
        alloc_cstring(out.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_to_lower_str(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        let out: String = text.chars().flat_map(char::to_lowercase).collect();
        alloc_cstring(out.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_fold_case(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        let out: String = text.chars().flat_map(char::to_lowercase).collect();
        alloc_cstring(out.as_bytes())
    })
}

// ---------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_nfc(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        let out: String = text.nfc().collect();
        alloc_cstring(out.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_nfd(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        let out: String = text.nfd().collect();
        alloc_cstring(out.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_nfkc(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        let out: String = text.nfkc().collect();
        alloc_cstring(out.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_nfkd(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        let out: String = text.nfkd().collect();
        alloc_cstring(out.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_is_nfc(s: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        i64::from(text.chars().eq(text.nfc()))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_is_nfd(s: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        i64::from(text.chars().eq(text.nfd()))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_is_nfkc(s: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        i64::from(text.chars().eq(text.nfkc()))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_is_nfkd(s: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        i64::from(text.chars().eq(text.nfkd()))
    })
}

// ---------------------------------------------------------------
// Segmentation (graphemes / words / sentences)
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_graphemes(s: *const c_char) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        let parts: Vec<String> = UnicodeSegmentation::graphemes(text, true)
            .map(String::from)
            .collect();
        alloc_string_vec(parts)
    })
}

/// `unicode::char_width(c)`: terminal columns the scalar occupies.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_unicode_char_width(c: u32) -> i64 {
    ffi_entry!({ unicode_width::UnicodeWidthChar::width(char_from(c)).map_or(0, |w| w as i64) })
}

/// `unicode::str_width(s)`: terminal columns the string occupies.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_str_width(s: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        unicode_width::UnicodeWidthStr::width(text) as i64
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_grapheme_count(s: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        UnicodeSegmentation::graphemes(text, true).count() as i64
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_words(s: *const c_char) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        let parts: Vec<String> = text.unicode_words().map(String::from).collect();
        alloc_string_vec(parts)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_word_bounds(s: *const c_char) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        let parts: Vec<String> = text.split_word_bounds().map(String::from).collect();
        alloc_string_vec(parts)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_word_count(s: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        text.unicode_words().count() as i64
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_sentences(s: *const c_char) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        let parts: Vec<String> = text.unicode_sentences().map(String::from).collect();
        alloc_string_vec(parts)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unicode_sentence_count(s: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_to_str` accepts.
        let text = unsafe { cstr_to_str(s) };
        text.unicode_sentences().count() as i64
    })
}
