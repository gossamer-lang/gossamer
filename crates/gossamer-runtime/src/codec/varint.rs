//! LEB128 variable-length integers, compatible with Go's `encoding/binary`.

/// Most bytes a `u64` takes as a varint.
pub const MAX_LEN: usize = 10;

/// Unsigned varint bytes of `value`.
#[must_use]
pub fn encode_unsigned(mut value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAX_LEN);
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
    out
}

/// Zigzag varint bytes of `value`, so small magnitudes of either sign stay
/// short.
#[must_use]
pub fn encode_signed(value: i64) -> Vec<u8> {
    encode_unsigned(((value << 1) ^ (value >> 63)) as u64)
}

/// The unsigned varint at the start of `buf` and the bytes it took.
pub fn decode_unsigned(buf: &[u8]) -> Result<(u64, usize), String> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (i, &b) in buf.iter().enumerate() {
        if i == MAX_LEN {
            return Err("varint overflows u64".to_string());
        }
        if b < 0x80 {
            if i == MAX_LEN - 1 && b > 1 {
                return Err("varint overflows u64".to_string());
            }
            return Ok((value | (u64::from(b) << shift), i + 1));
        }
        value |= u64::from(b & 0x7f) << shift;
        shift += 7;
    }
    Err("varint: buffer too small".to_string())
}

/// The zigzag varint at the start of `buf` and the bytes it took.
pub fn decode_signed(buf: &[u8]) -> Result<(i64, usize), String> {
    let (ux, n) = decode_unsigned(buf)?;
    let x = if ux & 1 == 0 {
        (ux >> 1) as i64
    } else {
        !((ux >> 1) as i64)
    };
    Ok((x, n))
}

#[cfg(test)]
mod tests {
    use super::{decode_signed, decode_unsigned, encode_signed, encode_unsigned};

    #[test]
    fn boundaries_round_trip() {
        for v in [
            0u64,
            1,
            127,
            128,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            let bytes = encode_unsigned(v);
            assert_eq!(decode_unsigned(&bytes), Ok((v, bytes.len())));
        }
        for v in [0i64, -1, 1, -64, 64, i64::MIN, i64::MAX] {
            let bytes = encode_signed(v);
            assert_eq!(decode_signed(&bytes), Ok((v, bytes.len())));
        }
    }

    #[test]
    fn malformed_buffers_are_refused() {
        assert!(decode_unsigned(&[]).is_err());
        assert!(decode_unsigned(&[0x80, 0x80]).is_err());
        assert!(
            decode_unsigned(&[0xFF; 9].iter().copied().chain([2]).collect::<Vec<_>>()).is_err()
        );
        assert!(decode_unsigned(&[0x80; 11]).is_err());
    }
}
