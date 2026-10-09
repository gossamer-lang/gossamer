//! `std::compress` C-ABI shims over `crate::codec::compress`, the
//! implementation the bytecode VM shares. Input is a `Vec<u8>`; output is a
//! `Result<Vec<u8>, errors::Error>` carrier.

use crate::codec::compress::{self, Format};

/// The carrier a codec answer becomes.
fn bytes_result(answer: Result<Vec<u8>, String>) -> i128 {
    match answer {
        Ok(bytes) => {
            let v = super::encoding::bytes_to_gosvec(&bytes);
            super::result::gos_rt_result_new(0, v as i64)
        }
        Err(msg) => {
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            super::result::gos_rt_result_new(1, err as i64)
        }
    }
}

/// The bytes of a `Vec<u8>` shim argument.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
unsafe fn input<'a>(data: *const super::vec::GosVec) -> std::borrow::Cow<'a, [u8]> {
    // SAFETY: the caller passes `data` null or live, which `vec_bytes_cow` accepts.
    unsafe { crate::c_abi::vec::vec_bytes_cow(data) }
}

/// `data` compressed in `format` at `level`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
pub(crate) unsafe fn compress_shim(
    format: Format,
    data: *const super::vec::GosVec,
    level: i64,
) -> i128 {
    // SAFETY: the caller passes `data` null or live.
    bytes_result(compress::compress(format, &unsafe { input(data) }, level))
}

/// `data` decompressed from `format`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
pub(crate) unsafe fn decompress_shim(format: Format, data: *const super::vec::GosVec) -> i128 {
    // SAFETY: the caller passes `data` null or live.
    bytes_result(compress::decompress(format, &unsafe { input(data) }, None))
}

/// `data` decompressed from `format`, refusing output past `max_bytes`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
pub(crate) unsafe fn decompress_limited_shim(
    format: Format,
    data: *const super::vec::GosVec,
    max_bytes: i64,
) -> i128 {
    // SAFETY: the caller passes `data` null or live.
    let data = unsafe { input(data) };
    bytes_result(
        compress::limit_of(max_bytes)
            .and_then(|limit| compress::decompress(format, &data, Some(limit))),
    )
}

/// `compress::gzip::encode(data, level) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_gzip_encode(
    data: *const super::vec::GosVec,
    level: i64,
) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { compress_shim(Format::Gzip, data, level) } })
}

/// `compress::gzip::decode(data) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_gzip_decode(data: *const super::vec::GosVec) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { decompress_shim(Format::Gzip, data) } })
}

/// `compress::gzip::decode_limited(data, max_bytes) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_gzip_decode_limited(
    data: *const super::vec::GosVec,
    max_bytes: i64,
) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { decompress_limited_shim(Format::Gzip, data, max_bytes) } })
}

/// `compress::flate::compress(data, level) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_flate_compress(
    data: *const super::vec::GosVec,
    level: i64,
) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { compress_shim(Format::Deflate, data, level) } })
}

/// `compress::flate::decompress(data) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_flate_decompress(data: *const super::vec::GosVec) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { decompress_shim(Format::Deflate, data) } })
}

/// `compress::flate::decompress_limited(data, max_bytes) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_flate_decompress_limited(
    data: *const super::vec::GosVec,
    max_bytes: i64,
) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { decompress_limited_shim(Format::Deflate, data, max_bytes) } })
}

/// `compress::zlib::compress(data, level) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zlib_compress(
    data: *const super::vec::GosVec,
    level: i64,
) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { compress_shim(Format::Zlib, data, level) } })
}

/// `compress::zlib::decompress(data) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zlib_decompress(data: *const super::vec::GosVec) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { decompress_shim(Format::Zlib, data) } })
}

/// `compress::zlib::decompress_limited(data, max_bytes) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zlib_decompress_limited(
    data: *const super::vec::GosVec,
    max_bytes: i64,
) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { decompress_limited_shim(Format::Zlib, data, max_bytes) } })
}

/// `compress::zstd::encode(data) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zstd_encode(data: *const super::vec::GosVec) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { compress_shim(Format::Zstd, data, compress::ZSTD_DEFAULT_LEVEL) } })
}

/// `compress::zstd::encode_level(data, level) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zstd_encode_level(
    data: *const super::vec::GosVec,
    level: i64,
) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { compress_shim(Format::Zstd, data, level) } })
}

/// `compress::zstd::decode(data) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zstd_decode(data: *const super::vec::GosVec) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { decompress_shim(Format::Zstd, data) } })
}

/// `compress::zstd::decode_limited(data, max_bytes) -> Result<[u8], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zstd_decode_limited(
    data: *const super::vec::GosVec,
    max_bytes: i64,
) -> i128 {
    // SAFETY: this shim's caller passes `data` null or live (C-ABI contract).
    ffi_entry!({ unsafe { decompress_limited_shim(Format::Zstd, data, max_bytes) } })
}
