//! Positions in UTF-8 text, in its two units: byte offsets (what `substring`,
//! `slice`, and `byte_at` take) and Unicode scalar indices (what `find`,
//! `len`, and indexing answer).

/// Byte offset of the first occurrence of `needle` in `text`.
#[must_use]
pub fn byte_find(text: &str, needle: &str) -> Option<usize> {
    text.find(needle)
}

/// Byte offset of the last occurrence of `needle` in `text`.
#[must_use]
pub fn byte_rfind(text: &str, needle: &str) -> Option<usize> {
    text.rfind(needle)
}

/// Byte offset where the scalar at `char_index` starts; the index one past
/// the last scalar answers the byte length, and any later one `None`.
#[must_use]
pub fn byte_offset(text: &str, char_index: usize) -> Option<usize> {
    text.char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .nth(char_index)
}

/// Scalar index of the scalar starting at byte `offset`; the byte length
/// answers the scalar count, and an offset inside a scalar or past the end
/// `None`.
#[must_use]
pub fn char_index(text: &str, offset: usize) -> Option<usize> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    Some(text[..offset].chars().count())
}

#[cfg(test)]
mod tests {
    use super::{byte_find, byte_offset, byte_rfind, char_index};

    #[test]
    fn units_compose() {
        let text = "\u{e9}ab\u{1F600}cab";
        let at = byte_find(text, "a").expect("found");
        assert_eq!(&text[at..=at], "a");
        assert_eq!(char_index(text, at), Some(1));
        assert_eq!(byte_offset(text, 1), Some(at));
        assert_eq!(byte_rfind(text, "ab"), Some(text.len() - 2));
        assert_eq!(char_index(text, 4), Some(3));
        assert_eq!(char_index(text, 5), None);
        assert_eq!(byte_offset(text, 7), Some(text.len()));
        assert_eq!(byte_offset(text, 8), None);
        assert_eq!(char_index(text, text.len()), Some(7));
        assert_eq!(byte_find("a\0b", "b"), Some(2));
        assert_eq!(byte_find("abc", "z"), None);
    }
}
