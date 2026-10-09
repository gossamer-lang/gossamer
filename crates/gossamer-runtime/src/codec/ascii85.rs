//! Adobe ASCII85: four bytes as five digits from `!` (0x21) through `u`
//! (0x75), `z` for an all-zero group, wrapped in `<~` ... `~>`.

const DIGIT_BASE: u64 = 85;
const FIRST_DIGIT: u8 = b'!';
const LAST_DIGIT: u8 = b'u';

/// ASCII85 text of `data`, wrapped in `<~` ... `~>`, with `z` for each
/// all-zero four-byte group.
#[must_use]
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(4) * 5 + 4);
    out.push_str("<~");
    for chunk in data.chunks(4) {
        let mut group = [0u8; 4];
        group[..chunk.len()].copy_from_slice(chunk);
        let value = u32::from_be_bytes(group);
        if chunk.len() == 4 && value == 0 {
            out.push('z');
            continue;
        }
        let digits = group_digits(value);
        for &d in &digits[..=chunk.len()] {
            out.push(char::from(d));
        }
    }
    out.push_str("~>");
    out
}

/// The five digits spelling `value`, most significant first.
fn group_digits(value: u32) -> [u8; 5] {
    let mut digits = [0u8; 5];
    let mut v = u64::from(value);
    for d in digits.iter_mut().rev() {
        *d = (v % DIGIT_BASE) as u8 + FIRST_DIGIT;
        v /= DIGIT_BASE;
    }
    digits
}

/// The group value of five digit values, or an error when it exceeds the
/// four-byte range.
fn group_value(digits: [u8; 5]) -> Result<u32, String> {
    let value = digits
        .iter()
        .fold(0u64, |acc, &d| acc * DIGIT_BASE + u64::from(d));
    u32::try_from(value).map_err(|_| "ascii85: group value exceeds four bytes".to_string())
}

/// Decodes ASCII85 text. The `<~` / `~>` delimiters are optional but must
/// appear together, and whitespace between digits is skipped.
///
/// A final partial group must be the one an encoder writes for its bytes:
/// any other digits there would decode to the same bytes as the canonical
/// spelling.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim();
    let body = match (text.strip_prefix("<~"), text.ends_with("~>")) {
        (Some(inner), true) if inner.len() >= 2 => &inner[..inner.len() - 2],
        (None, false) => text,
        (Some(_), _) => return Err("ascii85: '<~' without a closing '~>'".to_string()),
        (None, true) => return Err("ascii85: '~>' without an opening '<~'".to_string()),
    };
    let mut out = Vec::with_capacity(body.len() / 5 * 4 + 4);
    let mut group = [0u8; 5];
    let mut count = 0usize;
    for ch in body.chars() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        if ch == 'z' {
            if count != 0 {
                return Err("ascii85: z inside group".to_string());
            }
            out.extend_from_slice(&[0u8; 4]);
            continue;
        }
        let byte = u8::try_from(ch)
            .ok()
            .filter(|b| (FIRST_DIGIT..=LAST_DIGIT).contains(b));
        let Some(byte) = byte else {
            return Err(format!("ascii85: invalid character '{ch}'"));
        };
        group[count] = byte - FIRST_DIGIT;
        count += 1;
        if count == 5 {
            out.extend_from_slice(&group_value(group)?.to_be_bytes());
            count = 0;
        }
    }
    if count == 1 {
        return Err("ascii85: trailing single digit".to_string());
    }
    if count > 1 {
        let tail = count - 1;
        for slot in group.iter_mut().skip(count) {
            *slot = LAST_DIGIT - FIRST_DIGIT;
        }
        let bytes = group_value(group)?.to_be_bytes();
        let mut canonical = [0u8; 4];
        canonical[..tail].copy_from_slice(&bytes[..tail]);
        let spelled = group_digits(u32::from_be_bytes(canonical));
        if spelled[..count]
            .iter()
            .zip(&group[..count])
            .any(|(s, g)| s - FIRST_DIGIT != *g)
        {
            return Err("ascii85: final group is not canonical".to_string());
        }
        out.extend_from_slice(&bytes[..tail]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn every_length_round_trips() {
        let data: Vec<u8> = (0..=255u8).cycle().take(300).collect();
        for len in 0..=64 {
            for start in [0usize, 7, 200] {
                let slice = &data[start..start + len];
                assert_eq!(decode(&encode(slice)).as_deref(), Ok(slice), "len {len}");
            }
        }
        for tail in [[0xFFu8; 1].as_slice(), &[0xFF; 2], &[0xFF; 3], &[0xFF; 4]] {
            assert_eq!(decode(&encode(tail)).as_deref(), Ok(tail));
        }
    }

    #[test]
    fn known_vector_decodes() {
        assert_eq!(decode("<~9jqo^~>").as_deref(), Ok(b"Man ".as_slice()));
        assert_eq!(decode("9jqo^").as_deref(), Ok(b"Man ".as_slice()));
        assert_eq!(decode("<~z~>").as_deref(), Ok([0u8; 4].as_slice()));
        assert_eq!(decode("<~!!!!!~>").as_deref(), Ok([0u8; 4].as_slice()));
        assert_eq!(decode("<~s8W-!~>").as_deref(), Ok([0xFF; 4].as_slice()));
    }

    #[test]
    fn non_ascii_digits_are_refused() {
        assert!(decode("<~\u{121}\u{121}\u{121}\u{121}\u{121}~>").is_err());
        assert!(decode("9jqo\u{15e}").is_err());
    }

    #[test]
    fn group_above_four_bytes_is_refused() {
        assert!(decode("<~uuuuu~>").is_err());
        assert!(decode("<~s8W-\"~>").is_err());
        assert!(decode("<~uu~>").is_err());
    }

    #[test]
    fn unpaired_delimiters_are_refused() {
        assert!(decode("<~9jqo^").is_err());
        assert!(decode("9jqo^~>").is_err());
        assert!(decode("<~").is_err());
        assert_eq!(decode("<~~>").as_deref(), Ok([].as_slice()));
    }

    #[test]
    fn malformed_groups_are_refused() {
        assert!(decode("<~9z~>").is_err());
        assert!(decode("<~9~>").is_err());
        assert!(decode("<~9jqo^v~>").is_err());
    }

    #[test]
    fn non_canonical_final_group_is_refused() {
        let canonical = encode(b"M");
        assert_eq!(decode(&canonical).as_deref(), Ok(b"M".as_slice()));
        let digits: Vec<char> = canonical[2..canonical.len() - 2].chars().collect();
        let bumped = char::from(digits[1] as u8 + 1);
        let other = format!("<~{}{bumped}~>", digits[0]);
        assert!(decode(&other).is_err(), "{other}");
    }
}
