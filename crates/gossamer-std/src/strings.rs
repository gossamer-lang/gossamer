//! Runtime support for `std::strings`.

#![forbid(unsafe_code)]

/// Splits `text` on `delimiter`, returning every segment (including
/// trailing empties), mirroring Rust's `str::split`.
#[must_use]
pub fn split(text: &str, delimiter: &str) -> Vec<String> {
    text.split(delimiter).map(str::to_string).collect()
}

/// Splits `text` into at most `n` parts on `delimiter`.
#[must_use]
pub fn splitn(text: &str, n: usize, delimiter: &str) -> Vec<String> {
    text.splitn(n, delimiter).map(str::to_string).collect()
}

/// Splits on ASCII whitespace, dropping empty segments.
#[must_use]
pub fn split_whitespace(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// Returns the UTF-8 byte length of `text`.
#[must_use]
pub fn byte_len(text: &str) -> usize {
    text.len()
}

/// Returns the byte at `index`, or `0` when out of bounds.
#[must_use]
pub fn byte_at(text: &str, index: usize) -> u8 {
    text.as_bytes().get(index).copied().unwrap_or(0)
}

/// Returns a UTF-8 substring using byte offsets.
///
/// Offsets are clamped into the source byte range. If either offset lands
/// inside a multibyte scalar, it is advanced to the next valid boundary.
#[must_use]
pub fn substring(text: &str, start: usize, end: usize) -> String {
    let len = text.len();
    let lo = next_char_boundary(text, start.min(len));
    let hi = next_char_boundary(text, end.min(len)).max(lo);
    text[lo..hi].to_string()
}

fn next_char_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Trims leading and trailing whitespace (Unicode-aware).
#[must_use]
pub fn trim(text: &str) -> String {
    text.trim().to_string()
}

/// Returns whether `text` contains `needle`.
#[must_use]
pub fn contains(text: &str, needle: &str) -> bool {
    text.contains(needle)
}

/// Unicode scalar offset of the first occurrence of `needle` in `text`, or
/// `None` if absent.
#[must_use]
pub fn find(text: &str, needle: &str) -> Option<usize> {
    text.find(needle)
        .map(|byte_index| text[..byte_index].chars().count())
}

/// Byte offset of the first occurrence of `needle` in `text`, in the unit
/// `substring`, `slice`, and `byte_at` take.
#[must_use]
pub fn byte_find(text: &str, needle: &str) -> Option<usize> {
    gossamer_runtime::codec::text::byte_find(text, needle)
}

/// Byte offset of the last occurrence of `needle` in `text`.
#[must_use]
pub fn byte_rfind(text: &str, needle: &str) -> Option<usize> {
    gossamer_runtime::codec::text::byte_rfind(text, needle)
}

/// Byte offset where the scalar at `char_index` starts, converting a `find`
/// answer for `substring`; the index one past the last scalar answers the byte
/// length.
#[must_use]
pub fn byte_offset(text: &str, char_index: usize) -> Option<usize> {
    gossamer_runtime::codec::text::byte_offset(text, char_index)
}

/// Scalar index of the scalar starting at byte `offset`, or `None` inside a
/// scalar or past the end.
#[must_use]
pub fn char_index(text: &str, offset: usize) -> Option<usize> {
    gossamer_runtime::codec::text::char_index(text, offset)
}

/// Replaces every occurrence of `from` with `to`.
#[must_use]
pub fn replace(text: &str, from: &str, to: &str) -> String {
    text.replace(from, to)
}

/// Lowercases every character using Unicode scalar semantics.
#[must_use]
pub fn to_lowercase(text: &str) -> String {
    text.to_lowercase()
}

/// Uppercases every character using Unicode scalar semantics.
#[must_use]
pub fn to_uppercase(text: &str) -> String {
    text.to_uppercase()
}

/// Returns whether `text` starts with `prefix`.
#[must_use]
pub fn starts_with(text: &str, prefix: &str) -> bool {
    text.starts_with(prefix)
}

/// Returns whether `text` ends with `suffix`.
#[must_use]
pub fn ends_with(text: &str, suffix: &str) -> bool {
    text.ends_with(suffix)
}

/// Repeats `text` `count` times.
#[must_use]
pub fn repeat(text: &str, count: usize) -> String {
    text.repeat(count)
}

/// Returns an iterator-style `Vec<String>` of lines (no trailing
/// line-terminators).
#[must_use]
pub fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

/// Joins every element of `parts` with `sep` between them.
#[must_use]
pub fn join(parts: &[String], sep: &str) -> String {
    parts.join(sep)
}

/// Trims leading whitespace only.
#[must_use]
pub fn trim_start(text: &str) -> String {
    text.trim_start().to_string()
}

/// Trims trailing whitespace only.
#[must_use]
pub fn trim_end(text: &str) -> String {
    text.trim_end().to_string()
}

/// Strips `prefix` from the start of `text` if present.
#[must_use]
pub fn strip_prefix(text: &str, prefix: &str) -> Option<String> {
    text.strip_prefix(prefix).map(str::to_string)
}

/// Strips `suffix` from the end of `text` if present.
#[must_use]
pub fn strip_suffix(text: &str, suffix: &str) -> Option<String> {
    text.strip_suffix(suffix).map(str::to_string)
}

/// Pads `text` on the left with `pad_char` until it reaches `width`.
#[must_use]
pub fn pad_left(text: &str, width: usize, pad_char: char) -> String {
    let count = text.chars().count();
    if count >= width {
        return text.to_string();
    }
    let mut out = String::new();
    for _ in 0..(width - count) {
        out.push(pad_char);
    }
    out.push_str(text);
    out
}

/// Pads `text` on the right with `pad_char` until it reaches `width`.
#[must_use]
pub fn pad_right(text: &str, width: usize, pad_char: char) -> String {
    let count = text.chars().count();
    if count >= width {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + width - count);
    out.push_str(text);
    for _ in 0..(width - count) {
        out.push(pad_char);
    }
    out
}

/// Returns the Unicode scalar offset of the last occurrence of `needle` in
/// `text`, or `None` if absent.
#[must_use]
pub fn rfind(text: &str, needle: &str) -> Option<usize> {
    text.rfind(needle)
        .map(|byte_index| text[..byte_index].chars().count())
}

/// Replaces at most `n` occurrences of `from` with `to`.
#[must_use]
pub fn replacen(text: &str, from: &str, to: &str, n: usize) -> String {
    text.replacen(from, to, n)
}

/// Returns `true` if `text` contains the Unicode scalar `r`.
#[must_use]
pub fn contains_rune(text: &str, r: char) -> bool {
    text.contains(r)
}

/// Returns `true` if `text` contains any character found in `chars`.
#[must_use]
pub fn contains_any(text: &str, chars: &str) -> bool {
    text.chars().any(|c| chars.contains(c))
}

/// Byte offset of the first occurrence of `r` in `text`, or `None`.
#[must_use]
pub fn index_rune(text: &str, r: char) -> Option<usize> {
    text.find(r)
}

/// Byte offset of the first occurrence of any character in `chars`
/// within `text`, or `None`.
#[must_use]
pub fn index_any(text: &str, chars: &str) -> Option<usize> {
    text.char_indices()
        .find(|(_, c)| chars.contains(*c))
        .map(|(i, _)| i)
}

/// Byte offset of the last occurrence of any character in `chars`
/// within `text`, or `None`.
#[must_use]
pub fn last_index_any(text: &str, chars: &str) -> Option<usize> {
    text.char_indices()
        .rev()
        .find(|(_, c)| chars.contains(*c))
        .map(|(i, _)| i)
}

/// Returns `true` if `a` and `b` are equal under Unicode simple case folding.
#[must_use]
pub fn equal_fold(a: &str, b: &str) -> bool {
    // Compare char-by-char and require both iterators to end together.
    // A `zip().all(..)` truncates to the shorter side, so it would
    // report a string equal to its own prefix; byte-length differs
    // legitimately under folding (e.g. `K` U+212A vs `k`), so it is
    // not a reliable early-reject either.
    let mut ac = a.chars();
    let mut bc = b.chars();
    loop {
        match (ac.next(), bc.next()) {
            (Some(x), Some(y)) if x.to_lowercase().eq(y.to_lowercase()) => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Applies `f` to each Unicode scalar in `text`, replacing it with the
/// return value. If `f` returns `None` the character is dropped.
#[must_use]
pub fn map_chars(text: &str, f: impl Fn(char) -> Option<char>) -> String {
    text.chars().filter_map(f).collect()
}

/// Trims characters in `cutset` from both ends of `text`.
#[must_use]
pub fn trim_matches(text: &str, cutset: &str) -> String {
    text.trim_matches(|c| cutset.contains(c)).to_string()
}

/// Trims characters in `cutset` from the start of `text`.
#[must_use]
pub fn trim_start_matches(text: &str, cutset: &str) -> String {
    text.trim_start_matches(|c| cutset.contains(c)).to_string()
}

/// Trims characters in `cutset` from the end of `text`.
#[must_use]
pub fn trim_end_matches(text: &str, cutset: &str) -> String {
    text.trim_end_matches(|c| cutset.contains(c)).to_string()
}

/// Returns `text` with non-UTF-8 bytes replaced by `replacement`.
#[must_use]
pub fn to_valid_utf8(text: &[u8], replacement: &str) -> String {
    String::from_utf8_lossy(text).replace('\u{FFFD}', replacement)
}

/// Converts `text` to Unicode title case (first letter of each word uppercased).
#[must_use]
pub fn to_title(text: &str) -> String {
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
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_inserts_separator_between_parts() {
        let parts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(join(&parts, ","), "a,b,c");
        assert_eq!(join(&parts, ""), "abc");
        let empty: Vec<String> = Vec::new();
        assert_eq!(join(&empty, ","), "");
    }

    #[test]
    fn equal_fold_matches_case_insensitively_and_respects_length() {
        assert!(equal_fold("Hello", "hello"));
        assert!(equal_fold("GoSSAMER", "gossamer"));
        assert!(equal_fold("", ""));
        // Different lengths must not compare equal even when one is a
        // prefix of the other - the old `zip().all()` truncated and
        // wrongly returned true here.
        assert!(!equal_fold("hello", "hell"));
        assert!(!equal_fold("hell", "hello"));
        assert!(!equal_fold("abc", "abcd"));
    }

    #[test]
    fn pad_helpers_round_to_width() {
        assert_eq!(pad_left("42", 5, '0'), "00042");
        assert_eq!(pad_right("hi", 4, '_'), "hi__");
        assert_eq!(pad_left("toolong", 3, ' '), "toolong");
    }

    #[test]
    fn strip_helpers_only_match_at_anchor() {
        assert_eq!(strip_prefix("abcdef", "abc").as_deref(), Some("def"));
        assert_eq!(strip_prefix("abcdef", "xyz"), None);
        assert_eq!(strip_suffix("file.txt", ".txt").as_deref(), Some("file"));
    }

    #[test]
    fn replacen_caps_replacement_count() {
        assert_eq!(replacen("aaa", "a", "b", 2), "bba");
        assert_eq!(replacen("aaa", "a", "b", 0), "aaa");
    }
}
