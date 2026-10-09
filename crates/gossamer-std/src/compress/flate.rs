// Runtime support for `std::compress::flate` - raw DEFLATE encoding/decoding.
//
// Uses flate2 (pure-Rust miniz_oxide backend). Go's `compress/flate` package
// is mirrored here with Gossamer's error shape. Raw DEFLATE is the building
// block for gzip, zlib, and zip.

#![forbid(unsafe_code)]

use gossamer_runtime::codec::compress::{self, Format};

use crate::io::IoError;

/// Compresses `input` as raw DEFLATE at `level`, which must lie in 0..=9 (`0` is
/// store-only, `9` maximum).
pub fn compress(input: &[u8], level: u32) -> Result<Vec<u8>, IoError> {
    compress::compress(Format::Deflate, input, i64::from(level)).map_err(IoError::Other)
}

/// Decompresses raw DEFLATE `input`.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>, IoError> {
    compress::decompress(Format::Deflate, input, None).map_err(IoError::Other)
}

/// [`decompress`], refusing output past `max_bytes`.
pub fn decompress_limited(input: &[u8], max_bytes: u64) -> Result<Vec<u8>, IoError> {
    compress::decompress(Format::Deflate, input, Some(max_bytes)).map_err(IoError::Other)
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
}
