//! Hexadecimal text, two digits per byte.

/// Lowercase hex text of `data`.
#[must_use]
pub fn encode(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for &byte in data {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0xF)]));
    }
    out
}

/// Decodes hex text of either case, skipping ASCII whitespace between
/// digits.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 2);
    let mut high: Option<u8> = None;
    for ch in text.chars() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        let Some(value) = ch.to_digit(16) else {
            return Err(format!("hex: invalid character '{ch}'"));
        };
        let value = value as u8;
        match high.take() {
            Some(h) => out.push((h << 4) | value),
            None => high = Some(value),
        }
    }
    if high.is_some() {
        return Err("hex: odd number of digits".to_string());
    }
    Ok(out)
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
