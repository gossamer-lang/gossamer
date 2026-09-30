#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// gzip module - encode / decode using `flate2`.
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_gzip_encode(data: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if data.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `data` is a String argument from compiled code, null or a live string body for the whole call.
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(data) };
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        if enc.write_all(bytes).is_err() {
            return alloc_cstring(b"");
        }
        let buf = enc.finish().unwrap_or_default();
        alloc_cstring(&buf)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_gzip_decode(data: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if data.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `data` is a String argument from compiled code, null or a live string body for the whole call.
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(data) };
        use std::io::Read;
        let mut dec = flate2::read::GzDecoder::new(bytes);
        let mut out = Vec::new();
        if dec.read_to_end(&mut out).is_err() {
            return alloc_cstring(b"");
        }
        alloc_cstring(&out)
    })
}

// ---------------------------------------------------------------
// flate (raw DEFLATE) module - `std::compress::flate::{compress,
// decompress}`. Input is a `Vec<u8>` (GosVec), output is a
// `Result<Vec<u8>, errors::Error>` (GosResult: disc 0 Ok with a
// GosVec payload, disc 1 Err with a gos error handle).
// ---------------------------------------------------------------

/// Wraps `bytes` in an `Ok(Vec<u8>)` `GosResult`.
fn ok_bytes_result(bytes: &[u8]) -> i128 {
    let v = super::encoding::bytes_to_gosvec(bytes);
    super::vec::gos_rt_result_new(0, v as i64)
}

fn err_bytes_result(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    super::vec::gos_rt_result_new(1, err as i64)
}

/// `compress::gzip::encode(data, level) -> Result<[u8], Error>`.
/// The byte-accurate counterpart to `gos_rt_gzip_encode` (which
/// returns a c-string and truncates binary output at the first NUL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_gzip_encode(
    data: *const super::vec::GosVec,
    level: i64,
) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let input = unsafe { crate::c_abi::vec::vec_bytes(data) };
        let lvl = level.clamp(0, 9) as u32;
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(lvl));
        if enc.write_all(&input).is_err() {
            return err_bytes_result("gzip: compress write failed");
        }
        match enc.finish() {
            Ok(out) => ok_bytes_result(&out),
            Err(e) => err_bytes_result(&format!("gzip: {e}")),
        }
    })
}

/// `compress::gzip::decode(data) -> Result<[u8], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_gzip_decode(data: *const super::vec::GosVec) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let input = unsafe { crate::c_abi::vec::vec_bytes(data) };
        use std::io::Read;
        let mut dec = flate2::read::GzDecoder::new(&input[..]);
        let mut out = Vec::new();
        match dec.read_to_end(&mut out) {
            Ok(_) => ok_bytes_result(&out),
            Err(e) => err_bytes_result(&format!("gzip: {e}")),
        }
    })
}

/// `compress::flate::compress(data, level) -> Result<[u8], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_flate_compress(
    data: *const super::vec::GosVec,
    level: i64,
) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let input = unsafe { crate::c_abi::vec::vec_bytes(data) };
        let lvl = level.clamp(0, 9) as u32;
        use std::io::Write;
        let mut enc = flate2::write::DeflateEncoder::new(
            Vec::with_capacity(input.len()),
            flate2::Compression::new(lvl),
        );
        if enc.write_all(&input).is_err() {
            return err_bytes_result("flate: compress write failed");
        }
        match enc.finish() {
            Ok(out) => ok_bytes_result(&out),
            Err(e) => err_bytes_result(&format!("flate: {e}")),
        }
    })
}

/// `compress::flate::decompress(data) -> Result<[u8], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_flate_decompress(data: *const super::vec::GosVec) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let input = unsafe { crate::c_abi::vec::vec_bytes(data) };
        use std::io::Read;
        let mut dec = flate2::read::DeflateDecoder::new(&input[..]);
        let mut out = Vec::new();
        match dec.read_to_end(&mut out) {
            Ok(_) => ok_bytes_result(&out),
            Err(e) => err_bytes_result(&format!("flate: {e}")),
        }
    })
}

/// `compress::zlib::compress(data, level) -> Result<[u8], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zlib_compress(
    data: *const super::vec::GosVec,
    level: i64,
) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let input = unsafe { crate::c_abi::vec::vec_bytes(data) };
        let lvl = level.clamp(0, 9) as u32;
        use std::io::Write;
        let mut enc = flate2::write::ZlibEncoder::new(
            Vec::with_capacity(input.len()),
            flate2::Compression::new(lvl),
        );
        if enc.write_all(&input).is_err() {
            return err_bytes_result("zlib: compress write failed");
        }
        match enc.finish() {
            Ok(out) => ok_bytes_result(&out),
            Err(e) => err_bytes_result(&format!("zlib: {e}")),
        }
    })
}

/// `compress::zlib::decompress(data) -> Result<[u8], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zlib_decompress(data: *const super::vec::GosVec) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let input = unsafe { crate::c_abi::vec::vec_bytes(data) };
        use std::io::Read;
        let mut dec = flate2::read::ZlibDecoder::new(&input[..]);
        let mut out = Vec::new();
        match dec.read_to_end(&mut out) {
            Ok(_) => ok_bytes_result(&out),
            Err(e) => err_bytes_result(&format!("zlib: {e}")),
        }
    })
}

/// `compress::zstd::encode(data) -> Result<[u8], Error>` - Zstandard at
/// the default level (3).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zstd_encode(data: *const super::vec::GosVec) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let input = unsafe { crate::c_abi::vec::vec_bytes(data) };
        match zstd::stream::encode_all(&input[..], 3) {
            Ok(out) => ok_bytes_result(&out),
            Err(e) => err_bytes_result(&format!("zstd: {e}")),
        }
    })
}

/// `compress::zstd::encode_level(data, level) -> Result<[u8], Error>` -
/// `level` clamped to 1..=22.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zstd_encode_level(
    data: *const super::vec::GosVec,
    level: i64,
) -> i128 {
    ffi_entry!(0i128, {
        if !(1..=22).contains(&level) {
            return err_bytes_result(&format!(
                "zstd level out of range (expected 1..=22): {level}"
            ));
        }
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let input = unsafe { crate::c_abi::vec::vec_bytes(data) };
        match zstd::stream::encode_all(&input[..], level as i32) {
            Ok(out) => ok_bytes_result(&out),
            Err(e) => err_bytes_result(&format!("zstd: {e}")),
        }
    })
}

/// `compress::zstd::decode(data) -> Result<[u8], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_compress_zstd_decode(data: *const super::vec::GosVec) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let input = unsafe { crate::c_abi::vec::vec_bytes(data) };
        match zstd::stream::decode_all(&input[..]) {
            Ok(out) => ok_bytes_result(&out),
            Err(e) => err_bytes_result(&format!("zstd: {e}")),
        }
    })
}
