//! The `strings::*` free functions on the compiled tiers, matching `gossamer_std::strings`.

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

// Each function mirrors the `gossamer_std::strings` helper of the same
// name, so `gos` and `gos build` produce identical output.

unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    // SAFETY: this `unsafe fn`'s caller passes `p` live or null, which `typed_str_text` accepts.
    unsafe { typed_str_text(p) }
}

/// Builds a `*mut GosVec` of c-string pointers from owned strings.
/// Builds the `[String]` a split answers, one allocation per piece, taken
/// straight from the run of the input each piece names.
///
/// STRING-typed: the vec owns the pieces, so `gos_rt_vec_free` reclaims them
/// even when a consumer loop breaks early.
fn alloc_str_vec<'a>(parts: impl Iterator<Item = &'a str>) -> *mut GosVec {
    let parts: Vec<i64> = parts
        .map(|p| alloc_cstring(p.as_bytes()) as usize as i64)
        .collect();
    str_vec_from_words(&parts)
}

/// Wraps already-allocated string pointers in a STRING-typed vec that owns
/// them, writing the slots in one copy.
fn str_vec_from_words(parts: &[i64]) -> *mut GosVec {
    let vec = {
        crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
            8,
            parts.len() as i64,
            crate::c_abi::vec::vec_elem_kind::STRING,
        )
    };
    if vec.is_null() || parts.is_empty() {
        return vec;
    }
    // SAFETY: the vec was created with room for `parts.len()` 8-byte slots,
    // and a STRING vec takes ownership of the pointer each slot holds.
    unsafe {
        let v = &mut *vec;
        std::ptr::copy_nonoverlapping(parts.as_ptr().cast::<u8>(), v.ptr.as_ptr(), parts.len() * 8);
        v.len = parts.len() as i64;
    }
    vec
}

/// The whitespace-separated pieces of `text` as a `[String]`, split exactly
/// as `str::split_whitespace` splits. An ASCII input is split on bytes: the
/// only ASCII characters Unicode counts as whitespace are `\t` through `\r`
/// and the space, and every piece of an ASCII input is itself ASCII.
fn split_whitespace_vec(text: &str) -> *mut GosVec {
    let bytes = text.as_bytes();
    if !bytes.is_ascii() {
        return alloc_str_vec(text.split_whitespace());
    }
    let is_space = |b: u8| matches!(b, b' ' | b'\t'..=b'\r');
    let mut parts: Vec<i64> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && is_space(bytes[i]) {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && !is_space(bytes[i]) {
            i += 1;
        }
        if i > start {
            parts.push(alloc_ascii_cstring(&bytes[start..i]) as usize as i64);
        }
    }
    str_vec_from_words(&parts)
}

/// `strings::splitn(s, n, sep) -> [String]`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_str_splitn(
    s: *const c_char,
    n: i64,
    sep: *const c_char,
) -> *mut GosVec {
    ffi_entry_passthrough!({
        if n < 0 {
            crate::c_abi::panic::panic_text("strings::splitn: count must be non-negative");
        }
        let n = usize::try_from(n).unwrap_or(0);
        // SAFETY: `s` and `sep` are this shim's string arguments, each null or live (C-ABI
        // contract), which `cstr` accepts.
        alloc_str_vec(unsafe { cstr(s) }.splitn(n, unsafe { cstr(sep) }))
    })
}

/// `strings::split_whitespace(s) -> [String]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_split_whitespace(s: *const c_char) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        split_whitespace_vec(unsafe { cstr(s) })
    })
}

/// `strings::fields(s) -> [String]`. Same semantics as
/// `split_whitespace` (Go's `strings.Fields`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_fields(s: *const c_char) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        split_whitespace_vec(unsafe { cstr(s) })
    })
}

/// `strings::replacen(s, from, to, n) -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_str_replacen(
    s: *const c_char,
    from: *const c_char,
    to: *const c_char,
    n: i64,
) -> *mut c_char {
    ffi_entry_passthrough!({
        if n < 0 {
            crate::c_abi::panic::panic_text("strings::replacen: count must be non-negative");
        }
        let n = usize::try_from(n).unwrap_or(0);
        // SAFETY: `s`, `from`, and `to` are this shim's string arguments, each null or live
        // (C-ABI contract), which `cstr` accepts.
        let out = unsafe { cstr(s) }.replacen(unsafe { cstr(from) }, unsafe { cstr(to) }, n);
        alloc_cstring(out.as_bytes())
    })
}

/// `strings::to_title(s) -> String` - capitalises the first
/// character of each whitespace-separated word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_to_title(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        let text = unsafe { cstr(s) };
        let mut result = String::with_capacity(text.len());
        let mut capitalize_next = true;
        for c in text.chars() {
            if c.is_whitespace() {
                capitalize_next = true;
                result.push(c);
            } else if capitalize_next {
                result.extend(c.to_uppercase());
                capitalize_next = false;
            } else {
                result.push(c);
            }
        }
        alloc_cstring(result.as_bytes())
    })
}

/// `strings::trim_matches(s, cutset) -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_trim_matches(
    s: *const c_char,
    cutset: *const c_char,
) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `cutset` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        let cutset = unsafe { cstr(cutset) };
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        let out = unsafe { cstr(s) }.trim_matches(|c| cutset.contains(c));
        alloc_cstring(out.as_bytes())
    })
}

/// First Unicode scalar of `s`, or 32 (space) when `s` is empty or
/// null. Backs the `strings::pad_left/pad_right` lowering, whose
/// pad-char parameter is an `i64` codepoint but whose language-level
/// argument is a String pad glyph.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_first_codepoint(s: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        unsafe { cstr(s) }.chars().next().map_or(32, |c| c as i64)
    })
}

/// `strings::pad_left(s, width, pad_char) -> String`. `pad_char` is
/// the Unicode scalar value; invalid scalars fall back to a space.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_str_pad_left(
    s: *const c_char,
    width: i64,
    pad_char: i64,
) -> *mut c_char {
    ffi_entry_passthrough!({
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        let text = unsafe { cstr(s) };
        if width < 0 {
            crate::c_abi::panic::panic_text("strings::pad_left: width must be non-negative");
        }
        let width = usize::try_from(width).unwrap_or(0);
        let pc = u32::try_from(pad_char)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or(' ');
        let count = text.chars().count();
        let out = if count >= width {
            text.to_string()
        } else {
            let mut out = String::new();
            for _ in 0..(width - count) {
                out.push(pc);
            }
            out.push_str(text);
            out
        };
        alloc_cstring(out.as_bytes())
    })
}

/// `strings::pad_right(s, width, pad_char) -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_str_pad_right(
    s: *const c_char,
    width: i64,
    pad_char: i64,
) -> *mut c_char {
    ffi_entry_passthrough!({
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        let text = unsafe { cstr(s) };
        if width < 0 {
            crate::c_abi::panic::panic_text("strings::pad_right: width must be non-negative");
        }
        let width = usize::try_from(width).unwrap_or(0);
        let pc = u32::try_from(pad_char)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or(' ');
        let count = text.chars().count();
        let out = if count >= width {
            text.to_string()
        } else {
            let mut out = String::with_capacity(text.len() + width - count);
            out.push_str(text);
            for _ in 0..(width - count) {
                out.push(pc);
            }
            out
        };
        alloc_cstring(out.as_bytes())
    })
}

/// `__fmt_pad(s, width, fill, align)` - pads the already-rendered string `s`
/// to `width` characters with the `fill` codepoint. `align`: 0 = right
/// (pad on the left), 1 = left (pad on the right), 2 = center, 3 = zeros
/// between the number's sign (and radix prefix) and its digits. Backs the
/// `{:>N}` / `{:<N}` / `{:^N}` / `{:0N}` format specs.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_fmt_pad(
    s: *const c_char,
    width: i64,
    fill: i64,
    align: i64,
) -> *mut c_char {
    ffi_entry_passthrough!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        if width < 0 {
            crate::c_abi::panic::panic_text("__fmt_pad: width must be non-negative");
        }
        let width = usize::try_from(width).unwrap_or(0);
        let pad_char = u32::try_from(fill)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or(' ');
        let count = text.chars().count();
        if count >= width {
            return alloc_cstring(text.as_bytes());
        }
        let total = width - count;
        if align == gossamer_abi::format_pad::PAD_ALIGN_SIGN_AWARE_ZERO {
            let split = gossamer_abi::format_pad::sign_aware_prefix_len(text);
            let mut out = String::with_capacity(text.len() + total);
            out.push_str(&text[..split]);
            for _ in 0..total {
                out.push('0');
            }
            out.push_str(&text[split..]);
            return alloc_cstring(out.as_bytes());
        }
        let (left, right) = match align {
            1 => (0, total),                     // left-align: pad on the right
            2 => (total / 2, total - total / 2), // center
            _ => (total, 0),                     // right-align / default
        };
        let mut out = String::with_capacity(text.len() + total);
        for _ in 0..left {
            out.push(pad_char);
        }
        out.push_str(text);
        for _ in 0..right {
            out.push(pad_char);
        }
        alloc_cstring(out.as_bytes())
    })
}

/// Integer-specialized width formatting. This fuses the integer rendering and
/// padding stages so `{:08}` produces one runtime string instead of rendering
/// an intermediate decimal string and copying it into a second allocation.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_fmt_pad_i64(
    value: i64,
    width: i64,
    fill: i64,
    align: i64,
) -> *mut c_char {
    ffi_entry_passthrough!({
        if width < 0 {
            crate::c_abi::panic::panic_text("__fmt_pad: width must be non-negative");
        }
        let mut number = itoa::Buffer::new();
        let rendered = number.format(value);
        let width = usize::try_from(width).unwrap_or(0);
        let pad_char = u32::try_from(fill)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or(' ');
        let count = rendered.len();
        if count >= width {
            return alloc_cstring(rendered.as_bytes());
        }
        let total = width - count;
        if align == gossamer_abi::format_pad::PAD_ALIGN_SIGN_AWARE_ZERO {
            let split = gossamer_abi::format_pad::sign_aware_prefix_len(rendered);
            let output_len = rendered.len().saturating_add(total);
            return alloc_growable_with_fill(output_len, output_len, false, |out| {
                // SAFETY: `out` addresses the `output_len` bytes the allocation handed this
                // closure, and every write stays below that length.
                unsafe { copy_small_bytes(rendered.as_ptr(), out, split) };
                for index in 0..total {
                    // SAFETY: `out` addresses the `output_len` bytes the allocation handed this
                    // closure, and every write stays below that length.
                    unsafe { out.add(split + index).write(b'0') };
                }
                // SAFETY: `out` addresses the `output_len` bytes the allocation handed this
                // closure, and every write stays below that length.
                unsafe {
                    copy_small_bytes(
                        rendered.as_ptr().add(split),
                        out.add(split + total),
                        rendered.len() - split,
                    );
                }
            });
        }
        let (left, right) = match align {
            1 => (0, total),
            2 => (total / 2, total - total / 2),
            _ => (total, 0),
        };
        let mut encoded_fill = [0u8; 4];
        let fill_bytes = pad_char.encode_utf8(&mut encoded_fill).as_bytes();
        let output_len = rendered
            .len()
            .saturating_add(total.saturating_mul(fill_bytes.len()));
        alloc_growable_with_fill(output_len, output_len, false, |out| {
            let mut offset = 0;
            for _ in 0..left {
                // SAFETY: `out` addresses the `output_len` bytes the allocation handed this
                // closure, and every write stays below that length.
                unsafe { copy_small_bytes(fill_bytes.as_ptr(), out.add(offset), fill_bytes.len()) };
                offset += fill_bytes.len();
            }
            // SAFETY: `out` addresses the `output_len` bytes the allocation handed this closure,
            // and every write stays below that length.
            unsafe { copy_small_bytes(rendered.as_ptr(), out.add(offset), rendered.len()) };
            offset += rendered.len();
            for _ in 0..right {
                // SAFETY: `out` addresses the `output_len` bytes the allocation handed this
                // closure, and every write stays below that length.
                unsafe { copy_small_bytes(fill_bytes.as_ptr(), out.add(offset), fill_bytes.len()) };
                offset += fill_bytes.len();
            }
        })
    })
}

/// Concatenate a string prefix and a width-formatted integer in one allocation.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_concat_pad_i64(
    prefix: *const c_char,
    value: i64,
    width: i64,
    fill: i64,
    align: i64,
) -> *mut c_char {
    ffi_entry_passthrough!({
        if width < 0 {
            crate::c_abi::panic::panic_text("__fmt_pad: width must be non-negative");
        }
        // SAFETY: `prefix` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        let prefix = unsafe { cstr(prefix) }.as_bytes();
        let mut number = itoa::Buffer::new();
        let rendered = number.format(value);
        let width = usize::try_from(width).unwrap_or(0);
        let pad_char = u32::try_from(fill)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or(' ');
        let total = width.saturating_sub(rendered.len());
        if align == gossamer_abi::format_pad::PAD_ALIGN_SIGN_AWARE_ZERO {
            let split = gossamer_abi::format_pad::sign_aware_prefix_len(rendered);
            let output_len = prefix
                .len()
                .saturating_add(rendered.len())
                .saturating_add(total);
            return alloc_growable_with_fill(output_len, output_len, false, |out| {
                // SAFETY: `out` addresses the `output_len` bytes the allocation handed this
                // closure, and every write stays below that length.
                unsafe { copy_small_bytes(prefix.as_ptr(), out, prefix.len()) };
                let base = prefix.len();
                // SAFETY: `out` addresses the `output_len` bytes the allocation handed this
                // closure, and every write stays below that length.
                unsafe { copy_small_bytes(rendered.as_ptr(), out.add(base), split) };
                for index in 0..total {
                    // SAFETY: `out` addresses the `output_len` bytes the allocation handed this
                    // closure, and every write stays below that length.
                    unsafe { out.add(base + split + index).write(b'0') };
                }
                // SAFETY: `out` addresses the `output_len` bytes the allocation handed this
                // closure, and every write stays below that length.
                unsafe {
                    copy_small_bytes(
                        rendered.as_ptr().add(split),
                        out.add(base + split + total),
                        rendered.len() - split,
                    );
                }
            });
        }
        let (left, right) = match align {
            1 => (0, total),
            2 => (total / 2, total - total / 2),
            _ => (total, 0),
        };
        let mut encoded_fill = [0u8; 4];
        let fill_bytes = pad_char.encode_utf8(&mut encoded_fill).as_bytes();
        let padding_len = total.saturating_mul(fill_bytes.len());
        let output_len = prefix
            .len()
            .saturating_add(rendered.len())
            .saturating_add(padding_len);
        alloc_growable_with_fill(output_len, output_len, false, |out| {
            // SAFETY: `out` addresses the `output_len` bytes the allocation handed this closure,
            // and every write stays below that length.
            unsafe { copy_small_bytes(prefix.as_ptr(), out, prefix.len()) };
            let mut offset = prefix.len();
            for _ in 0..left {
                // SAFETY: `out` addresses the `output_len` bytes the allocation handed this
                // closure, and every write stays below that length.
                unsafe { copy_small_bytes(fill_bytes.as_ptr(), out.add(offset), fill_bytes.len()) };
                offset += fill_bytes.len();
            }
            // SAFETY: `out` addresses the `output_len` bytes the allocation handed this closure,
            // and every write stays below that length.
            unsafe { copy_small_bytes(rendered.as_ptr(), out.add(offset), rendered.len()) };
            offset += rendered.len();
            for _ in 0..right {
                // SAFETY: `out` addresses the `output_len` bytes the allocation handed this
                // closure, and every write stays below that length.
                unsafe { copy_small_bytes(fill_bytes.as_ptr(), out.add(offset), fill_bytes.len()) };
                offset += fill_bytes.len();
            }
        })
    })
}

/// `strings::contains_rune(s, r) -> bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_contains_rune(s: *const c_char, r: i64) -> i32 {
    ffi_entry!({
        let Some(rc) = u32::try_from(r).ok().and_then(char::from_u32) else {
            return 0;
        };
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        i32::from(unsafe { cstr(s) }.contains(rc))
    })
}

/// `strings::contains_any(s, chars) -> bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_contains_any(s: *const c_char, chars: *const c_char) -> i32 {
    ffi_entry!({
        // SAFETY: `chars` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        let chars = unsafe { cstr(chars) };
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        i32::from(unsafe { cstr(s) }.chars().any(|c| chars.contains(c)))
    })
}

/// `strings::equal_fold(a, b) -> bool` - case-insensitive compare.
/// Mirrors `gossamer_std::strings::equal_fold`: compares scalar by
/// scalar and requires both sequences to end together, so a string
/// is never equal to a fold-prefix of itself even when their byte
/// lengths coincide (e.g. KELVIN SIGN U+212A vs "kab").
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_equal_fold(a: *const c_char, b: *const c_char) -> i32 {
    ffi_entry!({
        // SAFETY: `a` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        let mut ac = unsafe { cstr(a) }.chars();
        // SAFETY: `b` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        let mut bc = unsafe { cstr(b) }.chars();
        loop {
            match (ac.next(), bc.next()) {
                (Some(x), Some(y)) if x.to_lowercase().eq(y.to_lowercase()) => {}
                (None, None) => return 1,
                _ => return 0,
            }
        }
    })
}

/// `strings::index_rune(s, r) -> Option<i64>` byte index, packed as
/// a `*mut GosResult` (`disc 0 = Some(idx)`, `disc 1 = None`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_index_rune(s: *const c_char, r: i64) -> i128 {
    ffi_entry!({
        let rc = u32::try_from(r).ok().and_then(char::from_u32);
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        match rc.and_then(|rc| unsafe { cstr(s) }.find(rc)) {
            Some(i) => gos_rt_result_new(0, i as i64),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// `strings::index_any(s, chars) -> Option<i64>` byte index of the
/// first character that appears in `chars`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_index_any(s: *const c_char, chars: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `chars` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        let chars = unsafe { cstr(chars) };
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        match unsafe { cstr(s) }
            .char_indices()
            .find(|(_, c)| chars.contains(*c))
            .map(|(i, _)| i)
        {
            Some(i) => gos_rt_result_new(0, i as i64),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// `strings::last_index_any(s, chars) -> Option<i64>` byte index of
/// the last character that appears in `chars`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_last_index_any(s: *const c_char, chars: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `chars` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        let chars = unsafe { cstr(chars) };
        // SAFETY: `s` is this shim's `String` argument, null or live (C-ABI contract), which
        // `cstr` accepts.
        match unsafe { cstr(s) }
            .char_indices()
            .rev()
            .find(|(_, c)| chars.contains(*c))
            .map(|(i, _)| i)
        {
            Some(i) => gos_rt_result_new(0, i as i64),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// `strings::strip_prefix(s, prefix) -> Option<String>` packed as a
/// `*mut GosResult` (`disc 0 = Some(string-ptr)`, `disc 1 = None`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_strip_prefix(s: *const c_char, prefix: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `s` and `prefix` are this shim's string arguments, each null or live (C-ABI
        // contract), which `cstr` accepts.
        match unsafe { cstr(s) }.strip_prefix(unsafe { cstr(prefix) }) {
            Some(stripped) => {
                let p = alloc_cstring(stripped.as_bytes()) as i64;
                gos_rt_result_new(0, p)
            }
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// `strings::strip_suffix(s, suffix) -> Option<String>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_strip_suffix(s: *const c_char, suffix: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `s` and `suffix` are this shim's string arguments, each null or live (C-ABI
        // contract), which `cstr` accepts.
        match unsafe { cstr(s) }.strip_suffix(unsafe { cstr(suffix) }) {
            Some(stripped) => {
                let p = alloc_cstring(stripped.as_bytes()) as i64;
                gos_rt_result_new(0, p)
            }
            None => gos_rt_result_new(1, 0),
        }
    })
}
