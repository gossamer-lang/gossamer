// Runtime support for `std::hash::crc32c` - CRC-32C (Castagnoli) checksums,
// computed with the CPU's CRC instruction where there is one.

#![forbid(unsafe_code)]

/// Computes the CRC-32C checksum of `data`.
#[must_use]
pub fn checksum(data: &[u8]) -> u32 {
    update(0, data)
}

/// Continues an incremental CRC-32C computation.
/// Pass the previous checksum as `crc`; pass `0` to start fresh.
#[must_use]
pub fn update(crc: u32, data: &[u8]) -> u32 {
    gossamer_runtime::crc32c::update(crc, data)
}

/// Computes the CRC-32C checksum of a UTF-8 string.
#[must_use]
pub fn checksum_string(s: &str) -> u32 {
    checksum(s.as_bytes())
}
