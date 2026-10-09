//! Compression codecs: gzip, zlib, and raw deflate everywhere; bzip2 and
//! zstd where their C libraries link.

use std::io::{Read, Write};

/// A compressed stream format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// RFC 1952 gzip; concatenated members decode as one stream.
    Gzip,
    /// RFC 1950 zlib.
    Zlib,
    /// RFC 1951 raw deflate.
    Deflate,
    /// bzip2.
    #[cfg(not(target_arch = "wasm32"))]
    Bzip2,
    /// Zstandard.
    #[cfg(not(target_arch = "wasm32"))]
    Zstd,
}

impl Format {
    /// The name error messages lead with.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Zlib => "zlib",
            Self::Deflate => "flate",
            #[cfg(not(target_arch = "wasm32"))]
            Self::Bzip2 => "bzip2",
            #[cfg(not(target_arch = "wasm32"))]
            Self::Zstd => "zstd",
        }
    }
}

/// The zstd level `compress::zstd::encode` uses.
pub const ZSTD_DEFAULT_LEVEL: i64 = 3;

/// The levels `format` accepts, inclusive.
#[must_use]
pub const fn level_range(format: Format) -> (i64, i64) {
    match format {
        Format::Gzip | Format::Zlib | Format::Deflate => (0, 9),
        #[cfg(not(target_arch = "wasm32"))]
        Format::Bzip2 => (1, 9),
        #[cfg(not(target_arch = "wasm32"))]
        Format::Zstd => (1, 22),
    }
}

/// `data` compressed in `format` at `level`, which must lie in the format's
/// [`level_range`].
pub fn compress(format: Format, data: &[u8], level: i64) -> Result<Vec<u8>, String> {
    let name = format.name();
    let (lo, hi) = level_range(format);
    if !(lo..=hi).contains(&level) {
        let op = match format {
            Format::Gzip => "encode",
            #[cfg(not(target_arch = "wasm32"))]
            Format::Zstd => "encode_level",
            _ => "compress",
        };
        return Err(format!(
            "compress::{name}::{op}: level must be between {lo} and {hi}, got {level}"
        ));
    }
    let fail = |e: std::io::Error| format!("{name}: {e}");
    // In range for every format, checked above.
    let small = u32::try_from(level).unwrap_or(0);
    match format {
        Format::Gzip => {
            let mut enc =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(small));
            enc.write_all(data).map_err(fail)?;
            enc.finish().map_err(fail)
        }
        Format::Zlib => {
            let mut enc =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(small));
            enc.write_all(data).map_err(fail)?;
            enc.finish().map_err(fail)
        }
        Format::Deflate => {
            let mut enc =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(small));
            enc.write_all(data).map_err(fail)?;
            enc.finish().map_err(fail)
        }
        #[cfg(not(target_arch = "wasm32"))]
        Format::Bzip2 => {
            let mut out = Vec::new();
            bzip2::read::BzEncoder::new(data, bzip2::Compression::new(small))
                .read_to_end(&mut out)
                .map_err(fail)?;
            Ok(out)
        }
        #[cfg(not(target_arch = "wasm32"))]
        Format::Zstd => {
            zstd::stream::encode_all(data, i32::try_from(level).unwrap_or(3)).map_err(fail)
        }
    }
}

/// `data` decompressed from `format`, refusing output past `limit` bytes when
/// one is given. The bytes actually produced are counted, never a size the
/// stream claims.
pub fn decompress(format: Format, data: &[u8], limit: Option<u64>) -> Result<Vec<u8>, String> {
    match format {
        Format::Gzip => read_limited(format, flate2::read::MultiGzDecoder::new(data), limit),
        Format::Zlib => read_limited(format, flate2::read::ZlibDecoder::new(data), limit),
        Format::Deflate => read_limited(format, flate2::read::DeflateDecoder::new(data), limit),
        #[cfg(not(target_arch = "wasm32"))]
        Format::Bzip2 => read_limited(format, bzip2::read::MultiBzDecoder::new(data), limit),
        #[cfg(not(target_arch = "wasm32"))]
        Format::Zstd => {
            let decoder = zstd::stream::read::Decoder::new(data)
                .map_err(|e| format!("{}: {e}", format.name()))?;
            read_limited(format, decoder, limit)
        }
    }
}

/// Reads `reader` to its end, refusing more than `limit` bytes.
fn read_limited(format: Format, reader: impl Read, limit: Option<u64>) -> Result<Vec<u8>, String> {
    let name = format.name();
    let mut out = Vec::new();
    match limit {
        None => {
            let mut reader = reader;
            reader
                .read_to_end(&mut out)
                .map_err(|e| format!("{name}: {e}"))?;
        }
        Some(limit) => {
            reader
                .take(limit.saturating_add(1))
                .read_to_end(&mut out)
                .map_err(|e| format!("{name}: {e}"))?;
            if out.len() as u64 > limit {
                return Err(format!(
                    "{name}: decompressed size exceeds the limit of {limit} bytes"
                ));
            }
        }
    }
    Ok(out)
}

/// The limit a language-level `max_bytes` argument names: a negative value
/// is a programming error the caller reports, so only non-negative ones map.
pub fn limit_of(max_bytes: i64) -> Result<u64, String> {
    u64::try_from(max_bytes).map_err(|_| format!("max_bytes must not be negative, got {max_bytes}"))
}

#[cfg(test)]
mod tests {
    use super::{Format, compress, decompress};

    fn formats() -> Vec<Format> {
        [Format::Gzip, Format::Zlib, Format::Deflate]
            .into_iter()
            .chain(native_formats())
            .collect()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn native_formats() -> Vec<Format> {
        vec![Format::Bzip2, Format::Zstd]
    }

    #[cfg(target_arch = "wasm32")]
    fn native_formats() -> Vec<Format> {
        Vec::new()
    }

    #[test]
    fn every_format_round_trips() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
        for format in formats() {
            let packed = compress(format, &data, 6).expect("compresses");
            assert_eq!(
                decompress(format, &packed, None).as_deref(),
                Ok(data.as_slice())
            );
        }
    }

    #[test]
    fn levels_outside_the_range_are_refused() {
        for format in formats() {
            let (lo, hi) = super::level_range(format);
            assert!(compress(format, b"x", lo).is_ok());
            assert!(compress(format, b"x", hi).is_ok());
            assert!(compress(format, b"x", lo - 1).is_err());
            assert!(compress(format, b"x", hi + 1).is_err());
        }
    }

    #[test]
    fn the_limit_is_exact() {
        let data = vec![b'a'; 100_000];
        for format in formats() {
            let packed = compress(format, &data, 6).expect("compresses");
            assert_eq!(
                decompress(format, &packed, Some(100_000)).map(|d| d.len()),
                Ok(100_000)
            );
            let refused = decompress(format, &packed, Some(99_999)).expect_err("over the limit");
            assert!(
                refused.contains("exceeds the limit of 99999 bytes"),
                "{refused}"
            );
        }
    }

    #[test]
    fn concatenated_gzip_members_decode_whole() {
        let mut both = compress(Format::Gzip, b"first ", 6).expect("compresses");
        both.extend(compress(Format::Gzip, b"second", 6).expect("compresses"));
        assert_eq!(
            decompress(Format::Gzip, &both, None).as_deref(),
            Ok(b"first second".as_slice())
        );
    }

    #[test]
    fn corrupt_and_truncated_streams_are_refused() {
        let packed = compress(Format::Gzip, b"hello hello hello", 6).expect("compresses");
        assert!(decompress(Format::Gzip, &packed[..packed.len() - 4], None).is_err());
        let mut flipped = packed.clone();
        let last = flipped.len() - 5;
        flipped[last] ^= 0xFF;
        assert!(decompress(Format::Gzip, &flipped, None).is_err());
        assert!(decompress(Format::Zlib, b"not zlib", None).is_err());
    }
}
