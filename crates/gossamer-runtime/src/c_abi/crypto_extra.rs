//! C-ABI dispatch shims for `std::crypto::kdf` and
//! `std::crypto::insecure`. These mirror the bytecode-VM builtins in
//! `gossamer-interp/src/stdlib_builtins/{crypto_breadth,crypto_insecure}.rs`
//! so the compiled (Cranelift / LLVM) tier resolves the same calls
//! natively instead of failing to link.
//!
//! Shapes match the interp's value model:
//! - Digests (`insecure::md5_hex` / `sha1_hex`) read the input as a byte
//!   vector (as the `sha256::hex` family does) and return a
//!   freshly-allocated lowercase-hex c-string.
//! - `kdf::pbkdf2_sha256` takes two `[u8]` byte vectors plus the
//!   iteration count and derived-key length, and returns a `Vec<u8>`.
//! - `kdf::scrypt_interactive` returns `Result<Vec<u8>, errors::Error>`
//!   packed as the runtime's `i128`.
//! - `kdf::argon2id_hash` / `argon2id_verify` return
//!   `Result<String, _>` / `Result<bool, _>` packed as `i128`.
//!
//! The algorithm parameters are copied verbatim from
//! `gossamer-std/src/crypto.rs` (Argon2id / V0x13 / `Params::default`;
//! PBKDF2 via the `pbkdf2` crate; scrypt `log_n=15, r=8, p=1`,
//! 32-byte output) so a value derived on one tier matches another
//! bit-for-bit.

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]

use std::os::raw::c_char;

use super::encoding::bytes_to_gosvec;
use super::vec::GosVec;

/// scrypt interactive cost: `log_n = 15` (N = 2^15), `r = 8`, `p = 1`.
/// Matches `gossamer-std`'s `kdf::scrypt_interactive`.
const SCRYPT_LOG_N: u8 = 15;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
/// Derived-key length for the 2-arg `scrypt_interactive` surface;
/// equals the bytecode VM's default `output` of 32.
const SCRYPT_OUTPUT: usize = 32;

fn nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => '?',
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(nibble(b >> 4));
        out.push(nibble(b & 0x0f));
    }
    out
}

/// Packs an `Err(errors::Error)` for the Result-returning `kdf` shims.
fn kdf_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    super::result::gos_rt_result_new(1, err as i64)
}

/// `crypto::insecure::md5_hex(data) -> String` - lowercase-hex MD5
/// digest of the input bytes's bytes. MD5 is cryptographically
/// broken; provided for legacy interop only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_md5_hex(input: *const GosVec) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `input` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let bytes = unsafe { crate::c_abi::vec::vec_bytes(input) };
        let bytes = bytes.as_slice();
        let digest: [u8; 16] = <md5::Md5 as md5::Digest>::digest(bytes).into();
        super::string::alloc_cstring(hex_encode(&digest).as_bytes())
    })
}

/// `crypto::insecure::sha1_hex(data) -> String` - lowercase-hex SHA-1
/// digest of the input bytes's bytes. SHA-1 is cryptographically
/// broken for collision resistance; provided for legacy interop only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_sha1_hex(input: *const GosVec) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `input` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let bytes = unsafe { crate::c_abi::vec::vec_bytes(input) };
        let bytes = bytes.as_slice();
        let digest: [u8; 20] = <sha1::Sha1 as sha1::Digest>::digest(bytes).into();
        super::string::alloc_cstring(hex_encode(&digest).as_bytes())
    })
}

/// `crypto::insecure::md5(data) -> [u8; 16]` - the raw 16-byte MD5
/// digest (not the hex string). The argument is a Gossamer `[u8]`
/// (`GosVec`), so the digest covers arbitrary binary input.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_md5(input: *const GosVec) -> *mut GosVec {
    // SAFETY: `input` is this shim's argument, live for the call (C-ABI contract) or null, which
    // `vec_bytes` accepts.
    let bytes = unsafe { crate::c_abi::vec::vec_bytes(input) };
    let digest: [u8; 16] = <md5::Md5 as md5::Digest>::digest(&bytes).into();
    bytes_to_gosvec(&digest)
}

/// `crypto::insecure::sha1(data) -> [u8; 20]` - the raw 20-byte SHA-1
/// digest (not the hex string). The argument is a Gossamer `[u8]`
/// (`GosVec`), so the digest covers arbitrary binary input.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_sha1(input: *const GosVec) -> *mut GosVec {
    // SAFETY: `input` is this shim's argument, live for the call (C-ABI contract) or null, which
    // `vec_bytes` accepts.
    let bytes = unsafe { crate::c_abi::vec::vec_bytes(input) };
    let digest: [u8; 20] = <sha1::Sha1 as sha1::Digest>::digest(&bytes).into();
    bytes_to_gosvec(&digest)
}

/// `crypto::kdf::pbkdf2_sha256(password, salt, iters, dklen) -> [u8]`
/// - PBKDF2-HMAC-SHA256. Returns a fresh `Vec<u8>` of length `dklen`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_pbkdf2_sha256(
    password: *const GosVec,
    salt: *const GosVec,
    iters: i64,
    dklen: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        use pbkdf2::pbkdf2_hmac;
        use sha2::Sha256;
        // SAFETY: `password` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let pw = unsafe { crate::c_abi::vec::vec_bytes(password) };
        // SAFETY: `salt` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let salt = unsafe { crate::c_abi::vec::vec_bytes(salt) };
        let rounds = iters.max(0) as u32;
        let out_len = dklen.max(0) as usize;
        let mut out = vec![0u8; out_len];
        pbkdf2_hmac::<Sha256>(&pw, &salt, rounds, &mut out);
        bytes_to_gosvec(&out)
    })
}

/// `crypto::kdf::scrypt_interactive(password, salt) -> Result<[u8], _>`
/// - scrypt at the standard interactive cost, 32-byte derived key.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_scrypt_interactive(
    password: *const GosVec,
    salt: *const GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        use scrypt::{Params as ScryptParams, scrypt};
        // SAFETY: `password` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let pw = unsafe { crate::c_abi::vec::vec_bytes(password) };
        // SAFETY: `salt` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let salt = unsafe { crate::c_abi::vec::vec_bytes(salt) };
        let params = match ScryptParams::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P, SCRYPT_OUTPUT) {
            Ok(p) => p,
            Err(e) => return kdf_err(&format!("scrypt: params: {e}")),
        };
        let mut out = vec![0u8; SCRYPT_OUTPUT];
        if let Err(e) = scrypt(&pw, &salt, &params, &mut out) {
            return kdf_err(&format!("scrypt: derive: {e}"));
        }
        super::result::gos_rt_result_new(0, bytes_to_gosvec(&out) as i64)
    })
}

/// `crypto::kdf::argon2id_hash(password) -> Result<String, _>` -
/// Argon2id PHC hash with default interactive parameters.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_argon2id_hash(password: *const GosVec) -> i128 {
    ffi_entry!(0i128, {
        use argon2::password_hash::{PasswordHasher, SaltString};
        use argon2::{Algorithm, Argon2, Params, Version};
        // SAFETY: `password` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let pw = unsafe { crate::c_abi::vec::vec_bytes(password) };
        let mut salt_bytes = [0u8; 16];
        if getrandom::fill(&mut salt_bytes).is_err() {
            return kdf_err("argon2: rng failure");
        }
        let salt = match SaltString::encode_b64(&salt_bytes) {
            Ok(s) => s,
            Err(e) => return kdf_err(&format!("argon2: salt: {e}")),
        };
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default());
        match argon.hash_password(&pw, &salt) {
            Ok(h) => super::result::gos_rt_result_new(
                0,
                super::string::alloc_cstring(h.to_string().as_bytes()) as i64,
            ),
            Err(e) => kdf_err(&format!("argon2: hash: {e}")),
        }
    })
}

/// `crypto::kdf::argon2id_verify(password, phc) -> Result<bool, _>` -
/// verifies `password` against a PHC string. `Ok(false)` on mismatch,
/// `Err` only when the PHC string cannot be parsed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_argon2id_verify(
    password: *const GosVec,
    phc: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        use argon2::Argon2;
        use argon2::password_hash::{PasswordHash, PasswordVerifier};
        // SAFETY: `password` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let pw = unsafe { crate::c_abi::vec::vec_bytes(password) };
        let phc_s = if phc.is_null() {
            String::new()
        } else {
            // SAFETY: `phc` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(phc) }
        };
        let parsed = match PasswordHash::new(&phc_s) {
            Ok(p) => p,
            Err(e) => return kdf_err(&format!("argon2: parse phc: {e}")),
        };
        let ok = Argon2::default().verify_password(&pw, &parsed).is_ok();
        super::result::gos_rt_result_new(0, i64::from(ok))
    })
}
