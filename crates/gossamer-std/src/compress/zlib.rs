// Runtime support for `std::compress::zlib` - zlib (RFC 1950) encoding/decoding.
//
// Uses flate2 (pure-Rust miniz_oxide backend). The zlib format wraps raw DEFLATE
// with a two-byte header and an Adler-32 checksum trailer.

#![forbid(unsafe_code)]

use gossamer_runtime::codec::compress::{self, Format};

use crate::io::IoError;

/// Compresses `input` as zlib at `level`, which must lie in 0..=9 (`0` is
/// store-only, `9` maximum).
pub fn compress(input: &[u8], level: u32) -> Result<Vec<u8>, IoError> {
    compress::compress(Format::Zlib, input, i64::from(level)).map_err(IoError::Other)
}

/// Decompresses zlib `input`.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>, IoError> {
    compress::decompress(Format::Zlib, input, None).map_err(IoError::Other)
}

/// [`decompress`], refusing output past `max_bytes`.
pub fn decompress_limited(input: &[u8], max_bytes: u64) -> Result<Vec<u8>, IoError> {
    compress::decompress(Format::Zlib, input, Some(max_bytes)).map_err(IoError::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_short_text() {
        let src = b"hello, world!";
        let compressed = compress(src, 6).unwrap();
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, src);
    }

    #[test]
    fn level_none_is_lossless() {
        let src = b"data data data";
        let compressed = compress(src, 0).unwrap();
        assert_eq!(decompress(&compressed).unwrap(), src);
    }

    #[test]
    fn level_9_is_lossless() {
        let src = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let compressed = compress(src, 9).unwrap();
        assert!(compressed.len() < src.len());
        assert_eq!(decompress(&compressed).unwrap(), src);
    }

    #[test]
    fn zlib_header_magic() {
        // zlib streams start with 0x78 (deflate, window bits=15)
        let compressed = compress(b"hello", 6).unwrap();
        assert_eq!(compressed[0], 0x78, "expected zlib magic byte");
    }
}
