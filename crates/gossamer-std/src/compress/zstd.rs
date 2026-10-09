// Runtime support for `std::compress::zstd` - Zstandard encoding/decoding.
//
// Wraps the `zstd` crate (libzstd C library, vendored) in the Gossamer
// error shape. The user surface mirrors the sibling gzip / flate / zlib
// modules: one-shot byte-in / byte-out entry points returning the
// standard `IoError`.

#![forbid(unsafe_code)]

use gossamer_runtime::codec::compress::{self, Format, ZSTD_DEFAULT_LEVEL};

use crate::io::IoError;

/// Encodes `input` as Zstandard at the default level.
pub fn encode(input: &[u8]) -> Result<Vec<u8>, IoError> {
    compress::compress(Format::Zstd, input, ZSTD_DEFAULT_LEVEL).map_err(IoError::Other)
}

/// Encodes `input` as Zstandard at `level`, which must lie in 1..=22.
pub fn encode_level(input: &[u8], level: i32) -> Result<Vec<u8>, IoError> {
    compress::compress(Format::Zstd, input, i64::from(level)).map_err(IoError::Other)
}

/// Decodes Zstandard `input`.
pub fn decode(input: &[u8]) -> Result<Vec<u8>, IoError> {
    compress::decompress(Format::Zstd, input, None).map_err(IoError::Other)
}

/// [`decode`], refusing output past `max_bytes`.
pub fn decode_limited(input: &[u8], max_bytes: u64) -> Result<Vec<u8>, IoError> {
    compress::decompress(Format::Zstd, input, Some(max_bytes)).map_err(IoError::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_non_trivial_payload() {
        let plain: Vec<u8> = (0..2048u32).flat_map(u32::to_le_bytes).collect();
        let cipher = encode(&plain).unwrap();
        assert_ne!(cipher, plain);
        // Zstandard frame magic: 0x28 0xB5 0x2F 0xFD (little-endian).
        assert_eq!(cipher[0..4], [0x28, 0xB5, 0x2F, 0xFD]);
        let back = decode(&cipher).unwrap();
        assert_eq!(back, plain);
    }

    #[test]
    fn decode_rejects_garbage_input() {
        let result = decode(&[0x00, 0x01, 0x02, 0x03, 0x04, 0x05]);
        assert!(result.is_err(), "expected error from non-zstd input");
    }

    #[test]
    fn compression_actually_compresses_repetitive_input() {
        let plain: Vec<u8> = b"abcdefghij".repeat(1024);
        assert_eq!(plain.len(), 10_240);
        let cipher = encode(&plain).unwrap();
        assert!(
            cipher.len() < plain.len(),
            "expected encoded length < input length (got {} >= {})",
            cipher.len(),
            plain.len()
        );
        let back = decode(&cipher).unwrap();
        assert_eq!(back, plain);
    }

    #[test]
    fn encode_level_respects_bounds() {
        let plain = b"hello, zstd";
        let (min, max) = compress::level_range(Format::Zstd);
        let (min, max) = (i32::try_from(min).unwrap(), i32::try_from(max).unwrap());
        assert!(encode_level(plain, min - 1).is_err());
        assert!(encode_level(plain, max + 1).is_err());
        let cipher = encode_level(plain, max).unwrap();
        assert_eq!(decode(&cipher).unwrap(), plain);
    }

    #[test]
    fn empty_input_round_trips() {
        let plain: &[u8] = b"";
        let cipher = encode(plain).unwrap();
        let back = decode(&cipher).unwrap();
        assert_eq!(back, plain);
    }
}
