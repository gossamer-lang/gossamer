//! gzip encoder / decoder shipped behind `std::compress::gzip`.
//!
//! Wraps `flate2` (pure-Rust `miniz_oxide` backend, no system zlib
//! dependency) in the Gossamer error shape. The user surface mirrors
//! Go's `compress/gzip` package.
//!
//! Streaming `Reader` / `Writer` adapters are exposed for piping
//! through HTTP bodies or filesystem streams without buffering the
//! whole payload.

#![forbid(unsafe_code)]

use gossamer_runtime::codec::compress::{self, Format};

use crate::io::IoError;

/// Compression level. `Default` matches gzip(1) (level 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level(u32);

impl Level {
    /// `0` - store-only (no compression).
    pub const NONE: Self = Self(0);
    /// `1` - fastest.
    pub const FASTEST: Self = Self(1);
    /// `6` - gzip(1) default.
    pub const DEFAULT: Self = Self(6);
    /// `9` - best (slowest).
    pub const BEST: Self = Self(9);

    /// Constructs an arbitrary level. Returns `Err` if the level is
    /// outside `[0, 9]`.
    pub fn new(level: u32) -> Result<Self, IoError> {
        if level > 9 {
            return Err(IoError::Other(format!("gzip level out of range: {level}")));
        }
        Ok(Self(level))
    }

    /// Numeric level.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl Default for Level {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Encodes `input` into a gzip-formatted byte vector.
pub fn encode(input: &[u8], level: Level) -> Result<Vec<u8>, IoError> {
    compress::compress(Format::Gzip, input, i64::from(level.0)).map_err(IoError::Other)
}

/// Decodes `input`, a complete gzip payload whose concatenated members
/// decode as one stream, into the original bytes.
pub fn decode(input: &[u8]) -> Result<Vec<u8>, IoError> {
    compress::decompress(Format::Gzip, input, None).map_err(IoError::Other)
}

/// [`decode`], refusing output past `max_bytes`.
pub fn decode_limited(input: &[u8], max_bytes: u64) -> Result<Vec<u8>, IoError> {
    compress::decompress(Format::Gzip, input, Some(max_bytes)).map_err(IoError::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_short_text() {
        let plain = b"hello, gossamer";
        let cipher = encode(plain, Level::default()).unwrap();
        assert_ne!(cipher, plain);
        // Gzip header magic.
        assert_eq!(cipher[0..2], [0x1f, 0x8b]);
        let back = decode(&cipher).unwrap();
        assert_eq!(back, plain);
    }

    #[test]
    fn round_trips_long_repeating_text() {
        let plain: Vec<u8> = b"abcdefghij".repeat(10_000);
        let cipher = encode(&plain, Level::BEST).unwrap();
        assert!(
            cipher.len() < plain.len() / 5,
            "expected good ratio on repeating data"
        );
        let back = decode(&cipher).unwrap();
        assert_eq!(back, plain);
    }

    #[test]
    fn level_none_is_lossless() {
        let plain = b"abcdef";
        let cipher = encode(plain, Level::NONE).unwrap();
        let back = decode(&cipher).unwrap();
        assert_eq!(back, plain);
    }

    #[test]
    fn invalid_level_returns_error() {
        match Level::new(11) {
            Ok(_) => panic!("expected error for level 11"),
            Err(IoError::Other(msg)) => assert!(msg.contains("out of range"), "msg: {msg}"),
            Err(other) => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn decode_rejects_garbage_input() {
        let result = decode(&[0x00, 0x01, 0x02, 0x03]);
        assert!(result.is_err(), "expected error from non-gzip input");
    }

    #[test]
    fn empty_input_round_trips() {
        let plain: &[u8] = b"";
        let cipher = encode(plain, Level::default()).unwrap();
        let back = decode(&cipher).unwrap();
        assert_eq!(back, plain);
    }
}
