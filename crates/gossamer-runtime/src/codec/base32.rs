//! RFC 4648 Base32 with the standard (A-Z 2-7) and extended-hex (0-9 A-V)
//! alphabets.

const ALPHA_STD: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const ALPHA_HEX: &[u8; 32] = b"0123456789ABCDEFGHIJKLMNOPQRSTUV";
const PAD: u8 = b'=';

/// Which RFC 4648 Base32 alphabet a text uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alphabet {
    /// `A-Z 2-7`, section 6.
    Standard,
    /// `0-9 A-V`, section 7; sorts in the same order as the bytes it encodes.
    Hex,
}

impl Alphabet {
    const fn symbols(self) -> &'static [u8; 32] {
        match self {
            Self::Standard => ALPHA_STD,
            Self::Hex => ALPHA_HEX,
        }
    }

    /// The five-bit value of an ASCII symbol, either case.
    fn value(self, byte: u8) -> Option<u8> {
        let up = byte.to_ascii_uppercase();
        let value = match self {
            Self::Standard => match up {
                b'A'..=b'Z' => up - b'A',
                b'2'..=b'7' => up - b'2' + 26,
                _ => return None,
            },
            Self::Hex => match up {
                b'0'..=b'9' => up - b'0',
                b'A'..=b'V' => up - b'A' + 10,
                _ => return None,
            },
        };
        Some(value)
    }
}

/// Base32 text of `data` in `alphabet`, padded with `=` to a multiple of
/// eight characters.
#[must_use]
pub fn encode(data: &[u8], alphabet: Alphabet) -> String {
    let symbols = alphabet.symbols();
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    for chunk in data.chunks(5) {
        let mut buf = [0u8; 8];
        buf[3..3 + chunk.len()].copy_from_slice(chunk);
        let n = u64::from_be_bytes(buf);
        let keep = symbols_for(chunk.len());
        for i in 0..8 {
            if i < keep {
                out.push(char::from(symbols[((n >> (35 - 5 * i)) & 0x1F) as usize]));
            } else {
                out.push(char::from(PAD));
            }
        }
    }
    out
}

/// Symbols an encoder writes for a final group of `bytes` bytes.
const fn symbols_for(bytes: usize) -> usize {
    match bytes {
        1 => 2,
        2 => 4,
        3 => 5,
        4 => 7,
        _ => 8,
    }
}

/// Decodes Base32 text in `alphabet`. Lowercase symbols and omitted padding
/// are accepted; when padding is present it completes the last group of
/// eight exactly and nothing follows it. Symbols an encoder never writes,
/// such as nonzero bits after the last byte, are refused.
pub fn decode(text: &str, alphabet: Alphabet) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    let data_len = bytes.iter().position(|&b| b == PAD).unwrap_or(bytes.len());
    let (data, padding) = bytes.split_at(data_len);
    if padding.iter().any(|&b| b != PAD) {
        return Err("base32: data after padding".to_string());
    }
    let mut out = Vec::with_capacity(data.len() * 5 / 8);
    let mut buf = 0u16;
    let mut bits = 0u32;
    for (i, &b) in data.iter().enumerate() {
        let Some(value) = alphabet.value(b) else {
            return Err(invalid_character(text, i, b));
        };
        buf = (buf << 5) | u16::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    let tail = data.len() % 8;
    if !matches!(tail, 0 | 2 | 4 | 5 | 7) {
        return Err("base32: input length is not a whole number of bytes".to_string());
    }
    if !padding.is_empty() && (tail == 0 || tail + padding.len() != 8) {
        return Err("base32: padding does not complete the final group".to_string());
    }
    if buf != 0 {
        return Err("base32: nonzero bits after the final byte".to_string());
    }
    Ok(out)
}

/// The refusal for byte `byte` at offset `at`, naming the whole character
/// it begins.
fn invalid_character(text: &str, at: usize, byte: u8) -> String {
    let ch = text
        .get(at..)
        .and_then(|rest| rest.chars().next())
        .unwrap_or(char::from(byte));
    format!("base32: invalid character '{ch}'")
}

#[cfg(test)]
mod tests {
    use super::{Alphabet, decode, encode};

    const VECTORS: [(&str, &str); 7] = [
        ("", ""),
        ("f", "MY======"),
        ("fo", "MZXQ===="),
        ("foo", "MZXW6==="),
        ("foob", "MZXW6YQ="),
        ("fooba", "MZXW6YTB"),
        ("foobar", "MZXW6YTBOI======"),
    ];

    #[test]
    fn rfc4648_vectors_round_trip() {
        for (plain, coded) in VECTORS {
            assert_eq!(encode(plain.as_bytes(), Alphabet::Standard), coded);
            assert_eq!(
                decode(coded, Alphabet::Standard).as_deref(),
                Ok(plain.as_bytes())
            );
            let unpadded = coded.trim_end_matches('=');
            assert_eq!(
                decode(unpadded, Alphabet::Standard).as_deref(),
                Ok(plain.as_bytes())
            );
            let lower = coded.to_ascii_lowercase();
            assert_eq!(
                decode(&lower, Alphabet::Standard).as_deref(),
                Ok(plain.as_bytes())
            );
        }
        assert_eq!(encode(b"foobar", Alphabet::Hex), "CPNMUOJ1E8======");
        assert_eq!(
            decode("CPNMUOJ1E8======", Alphabet::Hex).as_deref(),
            Ok(b"foobar".as_slice())
        );
    }

    #[test]
    fn every_length_round_trips() {
        let data: Vec<u8> = (0..=255u8).collect();
        for len in 0..=64 {
            for alphabet in [Alphabet::Standard, Alphabet::Hex] {
                let slice = &data[len..len * 2];
                assert_eq!(
                    decode(&encode(slice, alphabet), alphabet).as_deref(),
                    Ok(slice)
                );
            }
        }
    }

    #[test]
    fn data_after_padding_is_refused() {
        assert!(decode("MY======!", Alphabet::Standard).is_err());
        assert!(decode("MY======MY======", Alphabet::Standard).is_err());
        assert!(decode("MY=A====", Alphabet::Standard).is_err());
    }

    #[test]
    fn impossible_lengths_are_refused() {
        for text in ["A", "AAA", "AAAAAA", "A=======", "AAA=====", "AAAAAA=="] {
            assert!(decode(text, Alphabet::Standard).is_err(), "{text}");
        }
    }

    #[test]
    fn wrong_padding_count_is_refused() {
        assert!(decode("MY=", Alphabet::Standard).is_err());
        assert!(decode("MY=======", Alphabet::Standard).is_err());
        assert!(decode("MZXW6YTB========", Alphabet::Standard).is_err());
    }

    #[test]
    fn nonzero_unused_bits_are_refused() {
        assert!(decode("MZ======", Alphabet::Standard).is_err());
        assert!(decode("MZXW6YR=", Alphabet::Standard).is_err());
    }

    #[test]
    fn non_ascii_symbols_are_refused() {
        assert!(decode("\u{141}A======", Alphabet::Standard).is_err());
        assert_eq!(
            decode("M\u{e9}======", Alphabet::Standard),
            Err("base32: invalid character '\u{e9}'".to_string())
        );
        assert!(decode("MY======", Alphabet::Hex).is_err());
    }
}
