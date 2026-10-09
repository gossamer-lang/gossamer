// Runtime support for `std::compress::bzip2` - bzip2 compress/decompress.

#![forbid(unsafe_code)]

use gossamer_runtime::codec::compress::{self, Format};

use crate::io::IoError;

/// Compresses `data` as bzip2 at `level`, which must lie in 1..=9.
pub fn compress(data: &[u8], level: u32) -> Result<Vec<u8>, IoError> {
    compress::compress(Format::Bzip2, data, i64::from(level)).map_err(IoError::Other)
}

/// Decompresses bzip2 `data`; concatenated streams decode as one.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, IoError> {
    compress::decompress(Format::Bzip2, data, None).map_err(IoError::Other)
}

/// [`decompress`], refusing output past `max_bytes`.
pub fn decompress_limited(data: &[u8], max_bytes: u64) -> Result<Vec<u8>, IoError> {
    compress::decompress(Format::Bzip2, data, Some(max_bytes)).map_err(IoError::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_compresses_and_restores() {
        let data = b"hello, gossamer lang! hello, gossamer lang!";
        let compressed = compress(data, 6).unwrap();
        assert!(compressed.len() < data.len() + 20);
        let restored = decompress(&compressed).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn empty_input_round_trips() {
        let compressed = compress(b"", 6).unwrap();
        let restored = decompress(&compressed).unwrap();
        assert_eq!(restored, b"");
    }
}
