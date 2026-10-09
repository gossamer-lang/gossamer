//! JSON string escaping shared by every JSON writer, HTML-safe by default.

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

/// Whether ASCII byte `b` is written as an escape inside a JSON string: the
/// quote, the backslash, every control byte below `0x20`, and the three bytes
/// that would let a string end an enclosing HTML `<script>` block.
#[inline]
const fn json_escaped_byte(b: u8) -> bool {
    b < 0x20 || matches!(b, b'"' | b'\\' | b'<' | b'>' | b'&')
}

/// The high bit of each byte lane of `w` that a JSON string escapes or that is
/// not ASCII. Lanes above the lowest flagged one may be flagged spuriously; the
/// word is clean exactly when the answer is zero.
#[inline]
const fn json_word_flags(w: u64) -> u64 {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    const fn has_zero(w: u64) -> u64 {
        w.wrapping_sub(ONES) & !w & HIGHS
    }
    (w | w.wrapping_sub(ONES * 0x20)) & HIGHS
        | has_zero(w ^ (ONES * b'"' as u64))
        | has_zero(w ^ (ONES * b'\\' as u64))
        | has_zero(w ^ (ONES * b'<' as u64))
        | has_zero(w ^ (ONES * b'>' as u64))
        | has_zero(w ^ (ONES * b'&' as u64))
}

/// Whether any byte of `w` is one a JSON string escapes or is not ASCII.
#[inline]
const fn json_word_has_special(w: u64) -> bool {
    json_word_flags(w) != 0
}

/// Offset of the first byte of `bytes` that a JSON string escapes or that is
/// not ASCII, or `bytes.len()` when there is none. Eight bytes are tested at a
/// time.
fn first_json_special(bytes: &[u8]) -> usize {
    // A borrow only ever carries into a higher byte, so the lowest flag marks
    // the first special byte exactly even where flags above it are spurious.
    let first_flag = |w: u64| {
        let flagged = json_word_flags(w);
        (flagged != 0).then(|| flagged.trailing_zeros() as usize / 8)
    };
    let mut chunks = bytes.chunks_exact(8);
    let mut at = 0;
    for chunk in &mut chunks {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        if let Some(offset) = first_flag(u64::from_le_bytes(word)) {
            return at + offset;
        }
        at += 8;
    }
    let len = bytes.len();
    if at == len {
        return len;
    }
    if len < 8 {
        return bytes
            .iter()
            .position(|&b| b >= 0x80 || json_escaped_byte(b))
            .unwrap_or(len);
    }
    // The last whole word overlaps bytes already found clean, so any flag in
    // it lies in the tail.
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[len - 8..]);
    first_flag(u64::from_le_bytes(word)).map_or(len, |offset| len - 8 + offset)
}

/// Appends `text` to `out` as the body of a JSON string, escaped as the
/// language's JSON encoder escapes one: `\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`,
/// `\u00XX` in lowercase hex for every other byte below `0x20`, and `<`, `>`,
/// `&`, U+2028, and U+2029 as `\u` escapes, so the text cannot end an
/// enclosing HTML `<script>` block or read as a JavaScript line break. Every
/// other byte, UTF-8 included, is copied as it is.
pub fn json_escape_into(text: &[u8], out: &mut Vec<u8>) {
    json_escape_with(text, |bytes| out.extend_from_slice(bytes));
}

/// Hands `text`, escaped as [`json_escape_into`] escapes it, to `emit` as a
/// sequence of byte runs: each unescaped span whole, then each escape.
pub fn json_escape_with(text: &[u8], mut emit: impl FnMut(&[u8])) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut run = 0;
    let mut i = 0;
    while i < text.len() {
        let b = text[i];
        let line_break = b == 0xe2
            && text.get(i + 1) == Some(&0x80)
            && matches!(text.get(i + 2), Some(0xa8 | 0xa9));
        if !json_escaped_byte(b) && !line_break {
            i += 1;
            continue;
        }
        if run < i {
            emit(&text[run..i]);
        }
        let consumed = if line_break {
            emit(if text[i + 2] == 0xa8 {
                b"\\u2028"
            } else {
                b"\\u2029"
            });
            3
        } else {
            match b {
                b'"' => emit(b"\\\""),
                b'\\' => emit(b"\\\\"),
                0x08 => emit(b"\\b"),
                0x0c => emit(b"\\f"),
                b'\n' => emit(b"\\n"),
                b'\r' => emit(b"\\r"),
                b'\t' => emit(b"\\t"),
                _ => emit(&[
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[usize::from(b >> 4)],
                    HEX[usize::from(b & 0xf)],
                ]),
            }
            1
        };
        i += consumed;
        run = i;
    }
    if run < text.len() {
        emit(&text[run..]);
    }
}

/// `s.push_json_quoted(buf, start, end) -> bool` - appends the `[start, end)`
/// byte window of `buf` to `s` as a quoted, escaped JSON string, when that
/// window is valid UTF-8.
///
/// The window is scanned once; plain ASCII text is copied between its quotes
/// in one reservation, and only text holding a byte at or above `0x80` is
/// validated as UTF-8. An out-of-range or non-UTF-8 window appends nothing.
/// Answers the carrier [`gos_rt_str_push_utf8`] answers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_push_json_quoted(
    s: *const c_char,
    buf: *const crate::c_abi::vec::GosVec,
    start: i64,
    end: i64,
) -> i128 {
    // Plain ASCII onto an exclusively held ASCII builder with room is the
    // common shape: the window is checked and copied in one pass. Nothing on
    // this path unwinds, so it stays out of the frame the general path needs.
    // SAFETY: `buf` is this shim's byte vec argument, null or live (C-ABI contract).
    if let Some(window) = unsafe { packed_byte_window(buf, start, end) }
        // SAFETY: `s` is this shim's accumulator argument, null or live (C-ABI contract).
        && unsafe { json_quote_ascii_in_place(s, window) }
    {
        return crate::c_abi::result::gos_rt_result_new(0, s as i64);
    }
    // SAFETY: `s` and `buf` are this shim's arguments, each null or live (C-ABI contract).
    unsafe { push_json_quoted_general(s, buf, start, end) }
}

/// Appends `window` to `acc` as a quoted JSON string in place, when `acc` is
/// an exclusively held ASCII builder with room and `window` holds no byte
/// JSON escapes and nothing wider than ASCII. Answers whether it appended; the
/// length is published only on success, so bytes written past it before a
/// special byte turned up are not part of the string.
///
///
/// # Safety
///
/// `acc` is null or a Gossamer string body.
#[inline]
unsafe fn json_quote_ascii_in_place(acc: *const c_char, window: &[u8]) -> bool {
    // SAFETY: this `unsafe fn`'s caller passes `acc` null or live, which the probe accepts.
    let Some((cap, len)) = (unsafe { unique_builder_cap_len(acc) }) else {
        return false;
    };
    let n = window.len();
    if len + n + 2 > cap {
        return false;
    }
    // SAFETY: `acc` is a uniquely held builder with room for the quoted window (checked above).
    unsafe {
        let footer = acc.cast::<u8>().add(cap + 1).cast::<u32>();
        if footer.read_unaligned() != STR_INDEX_ASCII {
            return false;
        }
        let dst = acc.cast_mut().cast::<u8>().add(len);
        *dst = b'"';
        let body = dst.add(1);
        let mut at = 0;
        while at + 8 <= n {
            let word = window.as_ptr().add(at).cast::<u64>().read_unaligned();
            if json_word_has_special(word) {
                return false;
            }
            body.add(at).cast::<u64>().write_unaligned(word);
            at += 8;
        }
        if at < n && n >= 8 {
            // The last whole word ends at the window's end and overlaps bytes
            // already found clean, so it covers the tail in one test and store.
            let word = window.as_ptr().add(n - 8).cast::<u64>().read_unaligned();
            if json_word_has_special(word) {
                return false;
            }
            body.add(n - 8).cast::<u64>().write_unaligned(word);
            at = n;
        }
        while at < n {
            let b = *window.get_unchecked(at);
            if json_escaped_byte(b) || b >= 0x80 {
                return false;
            }
            *body.add(at) = b;
            at += 1;
        }
        *body.add(n) = b'"';
        *body.add(n + 1) = 0;
        let hdr = acc.cast_mut().cast::<u8>().sub(13);
        std::ptr::copy_nonoverlapping(((len + n + 2) as u32).to_le_bytes().as_ptr(), hdr.add(8), 4);
    }
    true
}

/// The general `push_json_quoted`: a wide buffer, text that needs escaping or
/// is not ASCII, or a builder that is shared, full, or not ASCII.
///
///
/// # Safety
///
/// As [`gos_rt_str_push_json_quoted`].
#[cold]
#[inline(never)]
unsafe fn push_json_quoted_general(
    s: *const c_char,
    buf: *const crate::c_abi::vec::GosVec,
    start: i64,
    end: i64,
) -> i128 {
    ffi_entry!({
        let unchanged =
            |ok: bool| crate::c_abi::result::gos_rt_result_new(i64::from(!ok), s as i64);
        if buf.is_null() || start < 0 || end < start {
            return unchanged(false);
        }
        let (lo, hi) = (start as usize, end as usize);
        // SAFETY: `buf` is non-null (checked above) and this shim's live byte vec argument.
        let Some(bytes) = (unsafe { crate::c_abi::vec::vec_bytes_window(buf, lo, hi) }) else {
            return unchanged(false);
        };
        let window = &bytes[..];
        let first = first_json_special(window);
        let appended = if first == window.len() {
            // SAFETY: this `unsafe fn`'s caller passes `s` null or a share it hands on.
            unsafe { str_append_parts(s, &[b"\"", window, b"\""], true) }
        } else {
            let rest = &window[first..];
            if !rest.is_ascii() && std::str::from_utf8(rest).is_err() {
                return unchanged(false);
            }
            let mut quoted = Vec::with_capacity(window.len() + 16);
            quoted.push(b'"');
            quoted.extend_from_slice(&window[..first]);
            json_escape_into(rest, &mut quoted);
            quoted.push(b'"');
            // SAFETY: this `unsafe fn`'s caller passes `s` null or a share it hands on.
            unsafe { str_append_parts(s, &[&quoted], false) }
        };
        crate::c_abi::result::gos_rt_result_new(0, appended as i64)
    })
}

#[cfg(test)]
mod json_quote_tests {
    use super::{first_json_special, json_escape_into, json_escaped_byte};

    #[test]
    fn push_json_quoted_matches_the_escaper_for_every_length_and_special_position() {
        for len in 0..40 {
            for at in 0..=len {
                for special in [b'"', b'<', 0x1f, 0xc3] {
                    let mut bytes = vec![b'x'; len];
                    if at < len {
                        bytes[at] = special;
                        if special == 0xc3 && at + 1 < len {
                            bytes[at + 1] = 0xa9;
                        } else if special == 0xc3 {
                            bytes[at] = b'y';
                        }
                    }
                    let mut want = b"ab\"".to_vec();
                    json_escape_into(&bytes, &mut want);
                    want.push(b'"');
                    // SAFETY: every pointer argument is a value this test built above and still
                    // holds live; a null one is accepted by the callee.
                    unsafe {
                        let buf = crate::c_abi::encoding::bytes_to_gosvec(&bytes);
                        let acc = super::gos_rt_str_with_capacity(128);
                        let acc = super::gos_rt_str_append_bytes(acc, b"ab".as_ptr(), 2);
                        let answer = super::gos_rt_str_push_json_quoted(acc, buf, 0, len as i64);
                        assert_eq!(crate::c_abi::result::gos_rt_result_disc(answer), 0);
                        let out = crate::c_abi::result::gos_rt_result_payload(answer) as usize
                            as *mut std::ffi::c_char;
                        assert_eq!(
                            super::typed_str_bytes(out),
                            &want[..],
                            "len {len} special {special:#x} at {at}"
                        );
                        super::gos_rt_str_free(out);
                        crate::c_abi::gos_rt_vec_free(buf);
                    }
                }
            }
        }
    }

    fn quoted(text: &str) -> String {
        let mut out = vec![b'"'];
        json_escape_into(text.as_bytes(), &mut out);
        out.push(b'"');
        String::from_utf8(out).unwrap()
    }

    /// The escape the language's JSON encoder writes for one character.
    fn expected_escape(c: char) -> String {
        match c {
            '"' => "\\\"".to_string(),
            '\\' => "\\\\".to_string(),
            '\n' => "\\n".to_string(),
            '\t' => "\\t".to_string(),
            '\r' => "\\r".to_string(),
            '\u{0008}' => "\\b".to_string(),
            '\u{000c}' => "\\f".to_string(),
            '<' => "\\u003c".to_string(),
            '>' => "\\u003e".to_string(),
            '&' => "\\u0026".to_string(),
            '\u{2028}' => "\\u2028".to_string(),
            '\u{2029}' => "\\u2029".to_string(),
            c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32),
            c => c.to_string(),
        }
    }

    #[test]
    fn json_escape_matches_the_encoder_for_every_ascii_byte_and_line_break() {
        let mut chars: Vec<char> = (0u8..0x80).map(char::from).collect();
        chars.extend(['\u{2028}', '\u{2029}', '\u{e9}', '\u{2027}', '\u{1f600}']);
        for c in chars {
            let text = format!("a{c}b");
            let want = format!("\"a{}b\"", expected_escape(c));
            assert_eq!(quoted(&text), want, "char {:#x}", c as u32);
        }
    }

    #[test]
    fn first_json_special_finds_the_first_escaped_or_wide_byte_at_every_offset() {
        for len in 0..40 {
            for at in 0..=len {
                for special in [0x00u8, 0x1f, b'"', b'\\', b'<', b'>', b'&', 0x80, 0xff] {
                    let mut bytes = vec![b'x'; len];
                    if at < len {
                        bytes[at] = special;
                    }
                    let want = if at < len { at } else { len };
                    assert_eq!(first_json_special(&bytes), want, "len {len} at {at}");
                }
            }
        }
        assert_eq!(first_json_special(b" !#[]~\x7f"), 7);
    }

    #[test]
    fn first_json_special_agrees_with_a_byte_scan_on_dense_inputs() {
        const ALPHABET: [u8; 12] = [
            0x00, 0x01, 0x1f, 0x20, b'!', b'"', b'#', b'\\', b'&', 0x7f, 0x80, 0xff,
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let rounds = if cfg!(miri) { 4 } else { 200 };
        for len in 0..40 {
            for _ in 0..rounds {
                let bytes: Vec<u8> = (0..len)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        ALPHABET[(state % ALPHABET.len() as u64) as usize]
                    })
                    .collect();
                let want = bytes
                    .iter()
                    .position(|&b| b >= 0x80 || json_escaped_byte(b))
                    .unwrap_or(len);
                assert_eq!(first_json_special(&bytes), want, "{bytes:?}");
            }
        }
    }
}
