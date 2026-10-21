//! Hexadecimal text, two digits per byte.

/// Each byte's two lowercase digits, indexed by the byte.
const PAIRS: [[u8; 2]; 256] = {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut table = [[0u8; 2]; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = [DIGITS[i >> 4], DIGITS[i & 0xF]];
        i += 1;
    }
    table
};

/// Each ASCII byte's digit value, `INVALID` for a byte that is not a hex
/// digit and `SPACE` for ASCII whitespace.
const VALUES: [u8; 256] = {
    let mut table = [INVALID; 256];
    let mut i = 0;
    while i < 10 {
        table[b'0' as usize + i] = i as u8;
        i += 1;
    }
    let mut i = 0;
    while i < 6 {
        table[b'a' as usize + i] = 10 + i as u8;
        table[b'A' as usize + i] = 10 + i as u8;
        i += 1;
    }
    table[b' ' as usize] = SPACE;
    table[b'\t' as usize] = SPACE;
    table[b'\n' as usize] = SPACE;
    table[0x0C] = SPACE;
    table[b'\r' as usize] = SPACE;
    table
};
const INVALID: u8 = 0xFF;
const SPACE: u8 = 0xFE;

/// Lowercase hex text of `data`.
#[must_use]
pub fn encode(data: &[u8]) -> String {
    let mut out = vec![0u8; data.len() * 2];
    encode_into(data, &mut out);
    // SAFETY: every byte written is an ASCII hex digit.
    unsafe { String::from_utf8_unchecked(out) }
}

/// Writes the lowercase hex of `data` into `out`, which holds exactly
/// `2 * data.len()` bytes.
pub fn encode_into(data: &[u8], out: &mut [u8]) {
    for (pair, &byte) in out.chunks_exact_mut(2).zip(data) {
        pair.copy_from_slice(&PAIRS[usize::from(byte)]);
    }
}

/// Decodes hex text of either case, skipping ASCII whitespace between
/// digits.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; decoded_bound(text.len())];
    let written = decode_into(text.as_bytes(), &mut out)?;
    out.truncate(written);
    Ok(out)
}

/// The most bytes `text_len` bytes of hex text decode to.
#[must_use]
pub fn decoded_bound(text_len: usize) -> usize {
    text_len / 2
}

/// Decodes the UTF-8 hex text `bytes` into `out`, which holds at least
/// `decoded_bound(bytes.len())` bytes, answering how many it wrote.
pub fn decode_into(bytes: &[u8], out: &mut [u8]) -> Result<usize, String> {
    // Unbroken digit pairs decode with one test each; the first pair holding
    // whitespace or an invalid byte hands the rest to the general walk.
    let mut written = 0;
    for (dst, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        let (h, l) = (VALUES[usize::from(pair[0])], VALUES[usize::from(pair[1])]);
        // `INVALID` and `SPACE` both exceed every digit value.
        if (h | l) >= 16 {
            break;
        }
        *dst = (h << 4) | l;
        written += 1;
    }
    let mut high: Option<u8> = None;
    let mut i = written * 2;
    while i < bytes.len() {
        match VALUES[usize::from(bytes[i])] {
            SPACE => {}
            INVALID => {
                // Every earlier byte was ASCII, so `i` starts a character.
                let end = bytes.len().min(i + 4);
                let ch = String::from_utf8_lossy(&bytes[i..end])
                    .chars()
                    .next()
                    .unwrap_or('\u{FFFD}');
                return Err(format!("hex: invalid character '{ch}'"));
            }
            value => match high.take() {
                Some(h) => {
                    out[written] = (h << 4) | value;
                    written += 1;
                }
                None => high = Some(value),
            },
        }
        i += 1;
    }
    if high.is_some() {
        return Err("hex: odd number of digits".to_string());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn round_trips_every_byte() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&all)).as_deref(), Ok(all.as_slice()));
        assert_eq!(
            decode("DEADbeef").as_deref(),
            Ok([0xDE, 0xAD, 0xBE, 0xEF].as_slice())
        );
        assert_eq!(
            decode("de ad\nbe ef").as_deref(),
            Ok([0xDE, 0xAD, 0xBE, 0xEF].as_slice())
        );
    }

    #[test]
    fn malformed_text_is_refused() {
        assert_eq!(decode("abc"), Err("hex: odd number of digits".to_string()));
        assert_eq!(decode("zz"), Err("hex: invalid character 'z'".to_string()));
        assert_eq!(
            decode("\u{663}0"),
            Err("hex: invalid character '\u{663}'".to_string())
        );
    }
}
