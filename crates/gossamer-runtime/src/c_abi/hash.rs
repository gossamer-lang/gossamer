#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::wildcard_imports)]

//! `std::hash::{crc32, adler32, fnv}` C-ABI shims.
//!
//! Mirrors the `gossamer_std::hash::*` implementations exactly so
//! `gos` and `gos build` produce identical checksums. The
//! runtime crate cannot depend on `gossamer-std` (that crate
//! depends on the runtime), so the algorithms are reimplemented
//! inline - they are tiny and table-free except for CRC-32.

use std::os::raw::c_char;

unsafe fn cstr_bytes<'a>(s: *const c_char) -> &'a [u8] {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `gos_str_arg_bytes`
    // accepts.
    unsafe { crate::c_abi::gos_str_arg_bytes(s) }
}

// ---------------------------------------------------------------
// CRC-32 (IEEE 802.3 polynomial)
// ---------------------------------------------------------------

const fn crc32_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
            j += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut i = 0usize;
    while i < 256 {
        let mut crc = tables[0][i];
        let mut k = 1;
        while k < 8 {
            crc = tables[0][(crc & 0xFF) as usize] ^ (crc >> 8);
            tables[k][i] = crc;
            k += 1;
        }
        i += 1;
    }
    tables
}

// Slicing-by-eight: one table lookup per byte with eight independent lookups
// per word, so the loop is bound by table loads rather than by the dependency
// chain of the byte-at-a-time form.
static CRC32_TABLES: [[u32; 256]; 8] = crc32_tables();

fn crc32_slice_by_eight(mut state: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let lo = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) ^ state;
        let hi = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        state = CRC32_TABLES[7][(lo & 0xFF) as usize]
            ^ CRC32_TABLES[6][((lo >> 8) & 0xFF) as usize]
            ^ CRC32_TABLES[5][((lo >> 16) & 0xFF) as usize]
            ^ CRC32_TABLES[4][(lo >> 24) as usize]
            ^ CRC32_TABLES[3][(hi & 0xFF) as usize]
            ^ CRC32_TABLES[2][((hi >> 8) & 0xFF) as usize]
            ^ CRC32_TABLES[1][((hi >> 16) & 0xFF) as usize]
            ^ CRC32_TABLES[0][(hi >> 24) as usize];
    }
    for &byte in chunks.remainder() {
        state = CRC32_TABLES[0][((state ^ u32::from(byte)) & 0xFF) as usize] ^ (state >> 8);
    }
    state
}

fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    !crc32_slice_by_eight(!crc, data)
}

/// `hash::crc32::checksum(data) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_crc32_checksum(data: *const super::vec::GosVec) -> i64 {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_cow` accepts.
        i64::from(crc32_update(0, &unsafe {
            crate::c_abi::vec::vec_bytes_cow(data)
        }))
    })
}

/// `hash::crc32::checksum_string(s) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_crc32_checksum_string(s: *const c_char) -> i64 {
    // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
    // `cstr_bytes` accepts.
    ffi_entry!({ i64::from(crc32_update(0, unsafe { cstr_bytes(s) })) })
}

/// `hash::crc32::update(crc, data) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_crc32_update(
    crc: i64,
    data: *const super::vec::GosVec,
) -> i64 {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_cow` accepts.
        i64::from(crc32_update(crc as u32, &unsafe {
            crate::c_abi::vec::vec_bytes_cow(data)
        }))
    })
}

/// `hash::crc32::update_window(crc, data, start, end) -> i64`.
///
/// Continues a checksum over `data[start..end]`. A caller holding a record
/// inside a larger buffer checks it where it lies, rather than copying the
/// window out to have something to hand `update`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_crc32_update_window(
    crc: i64,
    data: *const super::vec::GosVec,
    start: i64,
    end: i64,
) -> i64 {
    ffi_entry!({
        if start < 0 || end < start {
            return i64::from(crc as u32);
        }
        let (lo, hi) = (start as usize, end as usize);
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_window` accepts.
        let Some(bytes) = (unsafe { crate::c_abi::vec::vec_bytes_window(data, lo, hi) }) else {
            return i64::from(crc as u32);
        };
        i64::from(crc32_update(crc as u32, &bytes))
    })
}

/// `hash::crc32c::checksum(data) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_crc32c_checksum(data: *const super::vec::GosVec) -> i64 {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_cow` accepts.
        i64::from(crate::crc32c::update(0, &unsafe {
            crate::c_abi::vec::vec_bytes_cow(data)
        }))
    })
}

/// `hash::crc32c::checksum_string(s) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_crc32c_checksum_string(s: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_bytes` accepts.
        i64::from(crate::crc32c::update(0, unsafe { cstr_bytes(s) }))
    })
}

/// `hash::crc32c::update(crc, data) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_crc32c_update(
    crc: i64,
    data: *const super::vec::GosVec,
) -> i64 {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_cow` accepts.
        i64::from(crate::crc32c::update(crc as u32, &unsafe {
            crate::c_abi::vec::vec_bytes_cow(data)
        }))
    })
}

/// `hash::crc32c::update_window(crc, data, start, end) -> i64`: the CRC-32C
/// counterpart of [`gos_rt_hash_crc32_update_window`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_crc32c_update_window(
    crc: i64,
    data: *const super::vec::GosVec,
    start: i64,
    end: i64,
) -> i64 {
    ffi_entry!({
        if start < 0 || end < start {
            return i64::from(crc as u32);
        }
        let (lo, hi) = (start as usize, end as usize);
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_window` accepts.
        let Some(bytes) = (unsafe { crate::c_abi::vec::vec_bytes_window(data, lo, hi) }) else {
            return i64::from(crc as u32);
        };
        i64::from(crate::crc32c::update(crc as u32, &bytes))
    })
}

// ---------------------------------------------------------------
// Adler-32 (zlib / RFC 1950)
// ---------------------------------------------------------------

const MOD_ADLER: u32 = 65521;

fn adler32_update(adler: u32, data: &[u8]) -> u32 {
    let mut a = adler & 0xFFFF;
    let mut b = (adler >> 16) & 0xFFFF;
    for &byte in data {
        a = (a + u32::from(byte)) % MOD_ADLER;
        b = (b + a) % MOD_ADLER;
    }
    (b << 16) | a
}

/// `hash::adler32::checksum(data) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_adler32_checksum(data: *const super::vec::GosVec) -> i64 {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_cow` accepts.
        i64::from(adler32_update(1, &unsafe {
            crate::c_abi::vec::vec_bytes_cow(data)
        }))
    })
}

/// `hash::adler32::checksum_string(s) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_adler32_checksum_string(s: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
        // `cstr_bytes` accepts.
        i64::from(adler32_update(1, unsafe { cstr_bytes(s) }))
    })
}

/// `hash::adler32::update(adler, data) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_adler32_update(
    adler: i64,
    data: *const super::vec::GosVec,
) -> i64 {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_cow` accepts.
        i64::from(adler32_update(adler as u32, &unsafe {
            crate::c_abi::vec::vec_bytes_cow(data)
        }))
    })
}

// ---------------------------------------------------------------
// FNV-1a
// ---------------------------------------------------------------

const FNV1_64_INIT: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_64_PRIME: u64 = 0x0000_0100_0000_01b3;
const FNV1_32_INIT: u32 = 0x811c_9dc5;
const FNV_32_PRIME: u32 = 0x0100_0193;

fn fnv64(data: &[u8]) -> u64 {
    let mut hash = FNV1_64_INIT;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_64_PRIME);
    }
    hash
}

fn fnv32(data: &[u8]) -> u32 {
    let mut hash = FNV1_32_INIT;
    for &byte in data {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(FNV_32_PRIME);
    }
    hash
}

/// `hash::fnv::hash32(data) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_fnv32(data: *const super::vec::GosVec) -> i64 {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_cow` accepts.
        i64::from(fnv32(&unsafe { crate::c_abi::vec::vec_bytes_cow(data) }))
    })
}

/// `hash::fnv::hash64(data) -> u64`, carried as its bits in an `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_fnv64(data: *const super::vec::GosVec) -> i64 {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_cow` accepts.
        fnv64(&unsafe { crate::c_abi::vec::vec_bytes_cow(data) }) as i64
    })
}

/// `hash::fnv::hash_string(s) -> u64`, carried as its bits in an `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_hash_fnv_string(s: *const c_char) -> i64 {
    // SAFETY: `s` is this shim's argument, null or a live string body (C-ABI contract), which
    // `cstr_bytes` accepts.
    ffi_entry!({ fnv64(unsafe { cstr_bytes(s) }) as i64 })
}

/// `crypto::subtle::constant_time_eq(a, b) -> bool`. Length-aware
/// constant-time comparison: unequal lengths return false; equal
/// lengths are compared without short-circuiting.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_subtle_ct_eq(
    a: *const super::vec::GosVec,
    b: *const super::vec::GosVec,
) -> i32 {
    ffi_entry!({
        // SAFETY: `a` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_cow` accepts.
        let a = unsafe { crate::c_abi::vec::vec_bytes_cow(a) };
        // SAFETY: `b` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes_cow` accepts.
        let b = unsafe { crate::c_abi::vec::vec_bytes_cow(b) };
        if a.len() != b.len() {
            return 0;
        }
        let mut diff: u8 = 0;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        i32::from(diff == 0)
    })
}
