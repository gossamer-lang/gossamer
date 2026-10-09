//! `std::compress::bzip2` C-ABI shims over `crate::codec::compress`.

use crate::codec::compress::Format;

/// `compress::bzip2::compress(data, level) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_bzip2_compress(
    data: *const super::vec::GosVec,
    level: i64,
) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { super::gzip::compress_shim(Format::Bzip2, data, level) } })
}

/// `compress::bzip2::decompress(data) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_bzip2_decompress(data: *const super::vec::GosVec) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { super::gzip::decompress_shim(Format::Bzip2, data) } })
}

/// `compress::bzip2::decompress_limited(data, max_bytes) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_bzip2_decompress_limited(
    data: *const super::vec::GosVec,
    max_bytes: i64,
) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { super::gzip::decompress_limited_shim(Format::Bzip2, data, max_bytes) } })
}
