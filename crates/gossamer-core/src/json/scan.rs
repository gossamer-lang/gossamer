//! Skipping runs of bytes a scan does not act on, eight at a time.
//!
//! Each test flags the high bit of every lane of a little-endian word that
//! matches. A borrow only travels toward higher lanes, so a lane above a
//! match may be flagged too, but the lowest flagged lane of each test is
//! exact, and so is the lowest of several tests or-ed together.

const ONES: u64 = u64::from_ne_bytes([0x01; 8]);
const HIGHS: u64 = u64::from_ne_bytes([0x80; 8]);

/// Lanes of `word` equal to `byte`.
#[inline]
pub(super) fn equal(word: u64, byte: u8) -> u64 {
    let x = word ^ (ONES * u64::from(byte));
    x.wrapping_sub(ONES) & !x & HIGHS
}

/// Lanes of `word` below `bound`, which is at most `0x80`.
#[inline]
pub(super) fn below(word: u64, bound: u8) -> u64 {
    word.wrapping_sub(ONES * u64::from(bound)) & !word & HIGHS
}

/// The index of the first byte at or after `from` that `hits` flags in its
/// word, or that `stops` accepts in the last few bytes, or `bytes.len()`.
#[inline]
pub(super) fn find(
    bytes: &[u8],
    from: usize,
    hits: impl Fn(u64) -> u64,
    stops: impl Fn(u8) -> bool,
) -> usize {
    let mut i = from;
    while let Some(chunk) = bytes.get(i..).and_then(<[u8]>::first_chunk::<8>) {
        let flagged = hits(u64::from_le_bytes(*chunk));
        if flagged != 0 {
            return i + (flagged.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < bytes.len() && !stops(bytes[i]) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::{below, equal, find};

    #[test]
    fn the_first_match_is_found_at_every_offset() {
        for len in 0..40 {
            for at in 0..=len {
                let mut bytes = vec![b'a'; len];
                if at < len {
                    bytes[at] = b'"';
                }
                // A byte one below the match exercises the borrow between lanes.
                if at + 1 < len {
                    bytes[at + 1] = b'"' - 1;
                }
                let found = find(&bytes, 0, |w| equal(w, b'"'), |b| b == b'"');
                assert_eq!(found, at, "{bytes:?}");
            }
        }
    }

    #[test]
    fn control_bytes_are_found_and_others_are_not() {
        let text = "plain text \u{e9}\u{1f600} then\ttab";
        let at = find(text.as_bytes(), 0, |w| below(w, 0x20), |b| b < 0x20);
        assert_eq!(&text[at..], "\ttab");
    }
}
