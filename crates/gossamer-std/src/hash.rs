//! Runtime support for `std::hash` - non-cryptographic hash functions.

#![forbid(unsafe_code)]

/// Adler-32 checksum (used in zlib).
pub mod adler32;
/// CRC-32 checksum (IEEE polynomial, used in gzip/zip/Ethernet).
pub mod crc32;
/// CRC-32C checksum (Castagnoli polynomial, used by storage formats).
pub mod crc32c;
/// FNV-1a and FNV-1 hash functions.
pub mod fnv;
