//! RFC 4648 Base64 with the standard alphabet and `=` padding.

const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Sextet value per input byte; `INVALID` for anything outside the alphabet.
const INVALID: u8 = 0xFF;
static VALUES: [u8; 256] = {
    let mut table = [INVALID; 256];
    let mut i = 0;
    while i < 64 {
        table[ALPHA[i] as usize] = i as u8;
        i += 1;
    }
    table
};

/// Base64 text of `data`, padded with `=` to a multiple of four characters.
#[must_use]
pub fn encode(data: &[u8]) -> String {
    // The output is pure ASCII from the alphabet, so it is built as bytes at
    // its final length rather than pushed a `char` at a time.
    let mut out = vec![0u8; data.len().div_ceil(3) * 4];
    let mut written = 0usize;
    let mut chunks = data.chunks_exact(3);
    for chunk in &mut chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out[written] = ALPHA[((n >> 18) & 0x3f) as usize];
        out[written + 1] = ALPHA[((n >> 12) & 0x3f) as usize];
        out[written + 2] = ALPHA[((n >> 6) & 0x3f) as usize];
        out[written + 3] = ALPHA[(n & 0x3f) as usize];
        written += 4;
    }
    let tail = chunks.remainder();
    if !tail.is_empty() {
        let b1 = tail.get(1).copied().unwrap_or(0);
        let n = (u32::from(tail[0]) << 16) | (u32::from(b1) << 8);
        out[written] = ALPHA[((n >> 18) & 0x3f) as usize];
        out[written + 1] = ALPHA[((n >> 12) & 0x3f) as usize];
        out[written + 2] = if tail.len() > 1 {
            ALPHA[((n >> 6) & 0x3f) as usize]
        } else {
            b'='
        };
        out[written + 3] = b'=';
    }
    // SAFETY: every byte written comes from the base64 alphabet or `=`, all
    // of which are ASCII.
    unsafe { String::from_utf8_unchecked(out) }
}

/// Decodes Base64 text, skipping ASCII whitespace anywhere in it.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    decode_bytes(text.as_bytes())
}

/// The decoded bytes of Base64 text.
///
/// The alphabet, the padding and the whitespace it skips are all ASCII, so
/// the encoding of the surrounding text decides nothing here: reading the
/// bytes answers the same result without validating them as UTF-8 first.
pub fn decode_bytes(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; decoded_bound(bytes.len())];
    let written = decode_into(bytes, &mut out)?;
    out.truncate(written);
    Ok(out)
}

/// Bytes [`decode_into`] may write for an input of this length.
///
/// Every character carries six bits, so the output is at most three quarters
/// of the input; the spare rounds the last partial group up.
#[must_use]
pub const fn decoded_bound(input_len: usize) -> usize {
    input_len / 4 * 3 + 3
}

/// Decodes Base64 `bytes` into `out`, answering how many bytes it wrote.
///
/// The input is whole groups of four once whitespace is skipped; `=` pads
/// only the last group, as `xx==` or `xxx=`, and the bits the padding leaves
/// over must be zero, as an encoder writes them.
///
/// Four characters carry exactly three bytes, so a group whose characters are
/// all in the alphabet is one 24-bit assemble and three stores. A group
/// holding padding or whitespace - and the tail - goes through the
/// character-at-a-time accumulator beside it, which is also what keeps the
/// group path entered only on a byte boundary.
///
/// # Panics
/// If `out` is shorter than [`decoded_bound`] of the input length.
pub fn decode_into(bytes: &[u8], out: &mut [u8]) -> Result<usize, String> {
    assert!(
        out.len() >= decoded_bound(bytes.len()),
        "base64 output buffer is shorter than the decoded bound"
    );
    let mut bits: u32 = 0;
    let mut nbits = 0u32;
    let mut i = 0usize;
    let mut written = 0usize;
    // Alphabet characters and `=` padding read so far; whitespace is skipped.
    let mut data = 0usize;
    let mut pad = 0usize;
    while i < bytes.len() {
        if nbits == 0
            && pad == 0
            && i + 4 <= bytes.len()
            && let a = u32::from(VALUES[bytes[i] as usize])
            && let b = u32::from(VALUES[bytes[i + 1] as usize])
            && let c = u32::from(VALUES[bytes[i + 2] as usize])
            && let d = u32::from(VALUES[bytes[i + 3] as usize])
            // Every alphabet value is below 64 and the refusal marker is
            // 0xFF, so one compare of the union rejects the whole group.
            && (a | b | c | d) < 64
        {
            let n = (a << 18) | (b << 12) | (c << 6) | d;
            out[written] = (n >> 16) as u8;
            out[written + 1] = (n >> 8) as u8;
            out[written + 2] = n as u8;
            written += 3;
            data += 4;
            i += 4;
            continue;
        }
        let ch = bytes[i];
        i += 1;
        if ch.is_ascii_whitespace() {
            continue;
        }
        if ch == b'=' {
            pad += 1;
            continue;
        }
        let val = VALUES[ch as usize];
        if val == INVALID {
            return Err(invalid_character(bytes, i - 1));
        }
        if pad > 0 {
            return Err("base64: data after padding".to_string());
        }
        data += 1;
        bits = (bits << 6) | u32::from(val);
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out[written] = (bits >> nbits) as u8;
            written += 1;
            bits &= (1 << nbits) - 1;
        }
    }
    check_shape(data, pad)?;
    if bits != 0 {
        return Err("base64: nonzero bits after the final byte".to_string());
    }
    Ok(written)
}

/// Whether `data` alphabet characters followed by `pad` padding characters
/// spell whole groups of four, padded only where the last group is short:
/// `xx==` and `xxx=` end an encoding, and nothing else may.
fn check_shape(data: usize, pad: usize) -> Result<(), String> {
    if !(data + pad).is_multiple_of(4) {
        return Err("base64: input length must be a multiple of 4".to_string());
    }
    let valid_padding = match pad {
        0 => true,
        1 => data % 4 == 3,
        2 => data % 4 == 2,
        _ => false,
    };
    if !valid_padding {
        return Err("base64: padding does not end a group".to_string());
    }
    Ok(())
}

/// The refusal for the byte at `at`, naming the whole character it begins
/// when the text is UTF-8 there.
fn invalid_character(bytes: &[u8], at: usize) -> String {
    let rest = &bytes[at..bytes.len().min(at + 4)];
    let ch = match std::str::from_utf8(rest) {
        Ok(s) => s.chars().next(),
        Err(e) => std::str::from_utf8(&rest[..e.valid_up_to()])
            .ok()
            .and_then(|s| s.chars().next()),
    }
    .unwrap_or(char::REPLACEMENT_CHARACTER);
    format!("base64: invalid character '{ch}'")
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn round_trips_every_byte_and_padding_length() {
        let all: Vec<u8> = (0..=255u8).collect();
        for len in 0..=all.len() {
            let data = &all[..len];
            let text = encode(data);
            assert_eq!(text.len(), len.div_ceil(3) * 4, "length for {len} bytes");
            assert_eq!(
                decode(&text).as_deref(),
                Ok(data),
                "round trip for {len} bytes"
            );
        }
    }

    #[test]
    fn pads_the_tail_it_has() {
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn reports_the_character_it_refuses() {
        assert_eq!(decode(" Zm9v \n").as_deref(), Ok(b"foo".as_slice()));
        assert_eq!(
            decode("Zm9v*"),
            Err("base64: invalid character '*'".to_string())
        );
        assert_eq!(
            decode("Zm9\u{e9}"),
            Err("base64: invalid character '\u{e9}'".to_string())
        );
    }

    #[test]
    fn same_wherever_whitespace_falls() {
        let all: Vec<u8> = (0..=255u8).collect();
        for len in 0..=64usize {
            let data = &all[..len];
            let text = encode(data);
            for cut in 0..=text.len() {
                let spaced = format!("{} \n{}", &text[..cut], &text[cut..]);
                assert_eq!(
                    decode(&spaced).as_deref(),
                    Ok(data),
                    "{len} bytes, space at {cut}"
                );
            }
        }
    }

    #[test]
    fn malformed_shapes_are_refused() {
        for text in [
            "Z", "Zg", "Zg=", "Zg===", "Z===", "Zg==Zg==", "Zm9=v", "====",
        ] {
            assert!(decode(text).is_err(), "{text}");
        }
    }

    #[test]
    fn nonzero_unused_bits_are_refused() {
        assert_eq!(decode("Zg==").as_deref(), Ok(b"f".as_slice()));
        assert!(decode("Zh==").is_err());
        assert!(decode("Zm9=").is_err());
        assert!(decode("Zm8=").is_ok());
    }
}
