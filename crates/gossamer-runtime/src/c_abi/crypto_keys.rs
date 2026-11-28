//! C-ABI shims for `gossamer_runtime::crypto_keys`: SHA-384, the HMAC family,
//! P-256 / P-384 ECDSA in both signature forms, RSA key generation, and
//! HKDF-SHA384, the same code the VM calls.

use std::os::raw::c_char;

use super::result::gos_rt_result_new;
use super::string::alloc_cstring;
use super::vec::GosVec;
use crate::crypto_interop as CI;
use crate::crypto_keys as CK;

fn err(message: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
    gos_rt_result_new(1, err as i64)
}

/// Layout of a `(Vec<u8>, Vec<u8>)` pair: both vectors are owned by the blob.
pub(crate) static VEC_PAIR_META: [i64; 6] = [
    gossamer_abi::rc::RC_KIND_STRUCT,
    1,
    0,
    2,
    gossamer_abi::rc::RC_CHILD_VEC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT,
    (gossamer_abi::rc::RC_CHILD_VEC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT) | 1,
];

/// The bytes of argument `v`.
///
/// # Safety
/// `v` is null or a live vector for the call.
unsafe fn bytes<'a>(v: *const GosVec) -> std::borrow::Cow<'a, [u8]> {
    // SAFETY: this `unsafe fn`'s caller passes `v` live or null for the call.
    unsafe { super::vec::vec_bytes_cow(v) }
}

/// The text of argument `s`.
///
/// # Safety
/// `s` is null or a live string body for the call.
unsafe fn text<'a>(s: *const c_char) -> std::borrow::Cow<'a, str> {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null for the call.
    std::borrow::Cow::Borrowed(unsafe { crate::c_abi::gos_str_arg_text(s) })
}

fn hex(bytes: &[u8]) -> String {
    crate::codec::hex::encode(bytes)
}

pub(crate) fn vec_pair(answer: Result<(Vec<u8>, Vec<u8>), String>) -> i128 {
    match answer {
        Ok((a, b)) => {
            let words = [
                super::encoding::bytes_to_gosvec(&a) as i64,
                super::encoding::bytes_to_gosvec(&b) as i64,
            ];
            let blob = crate::c_abi::rc::counted_words(&words, &VEC_PAIR_META);
            gos_rt_result_new(0, blob as i64)
        }
        Err(message) => err(&message),
    }
}

pub(crate) fn vec_result(answer: Result<Vec<u8>, String>) -> i128 {
    match answer {
        Ok(v) => gos_rt_result_new(0, super::encoding::bytes_to_gosvec(&v) as i64),
        Err(message) => err(&message),
    }
}

/// `crypto::sha384::digest`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_sha384_digest(data: *const GosVec) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, live for the call or null.
        let data = unsafe { bytes(data) };
        super::encoding::bytes_to_gosvec(&CK::sha384(&data))
    })
}

/// `crypto::sha384::hex`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_sha384_hex(data: *const GosVec) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, live for the call or null.
        let data = unsafe { bytes(data) };
        alloc_cstring(hex(&CK::sha384(&data)).as_bytes())
    })
}

/// `crypto::hmac::sha384_mac`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_sha384_mac(
    key: *const GosVec,
    message: *const GosVec,
) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        super::encoding::bytes_to_gosvec(&CK::hmac(CK::MacHash::Sha384, &key, &message))
    })
}

/// `crypto::hmac::sha384_hex`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_sha384_hex(
    key: *const GosVec,
    message: *const GosVec,
) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        alloc_cstring(hex(&CK::hmac(CK::MacHash::Sha384, &key, &message)).as_bytes())
    })
}

/// `crypto::hmac::sha512_mac`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_sha512_mac(
    key: *const GosVec,
    message: *const GosVec,
) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        super::encoding::bytes_to_gosvec(&CK::hmac(CK::MacHash::Sha512, &key, &message))
    })
}

/// `crypto::hmac::sha512_hex`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_sha512_hex(
    key: *const GosVec,
    message: *const GosVec,
) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        alloc_cstring(hex(&CK::hmac(CK::MacHash::Sha512, &key, &message)).as_bytes())
    })
}

/// `crypto::hmac::sha3_256_mac`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_sha3_256_mac(
    key: *const GosVec,
    message: *const GosVec,
) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        super::encoding::bytes_to_gosvec(&CK::hmac(CK::MacHash::Sha3_256, &key, &message))
    })
}

/// `crypto::hmac::sha3_256_hex`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_sha3_256_hex(
    key: *const GosVec,
    message: *const GosVec,
) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        alloc_cstring(hex(&CK::hmac(CK::MacHash::Sha3_256, &key, &message)).as_bytes())
    })
}

/// `crypto::hmac::sha3_512_mac`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_sha3_512_mac(
    key: *const GosVec,
    message: *const GosVec,
) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        super::encoding::bytes_to_gosvec(&CK::hmac(CK::MacHash::Sha3_512, &key, &message))
    })
}

/// `crypto::hmac::sha3_512_hex`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_sha3_512_hex(
    key: *const GosVec,
    message: *const GosVec,
) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        alloc_cstring(hex(&CK::hmac(CK::MacHash::Sha3_512, &key, &message)).as_bytes())
    })
}

/// `crypto::hmac::verify_sha256`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_verify_sha256(
    key: *const GosVec,
    message: *const GosVec,
    tag: *const GosVec,
) -> i32 {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        // SAFETY: `tag` is this shim's argument, live for the call or null.
        let tag = unsafe { bytes(tag) };
        i32::from(CK::hmac_verify(CK::MacHash::Sha256, &key, &message, &tag))
    })
}

/// `crypto::hmac::verify_sha384`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_verify_sha384(
    key: *const GosVec,
    message: *const GosVec,
    tag: *const GosVec,
) -> i32 {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        // SAFETY: `tag` is this shim's argument, live for the call or null.
        let tag = unsafe { bytes(tag) };
        i32::from(CK::hmac_verify(CK::MacHash::Sha384, &key, &message, &tag))
    })
}

/// `crypto::hmac::verify_sha512`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_verify_sha512(
    key: *const GosVec,
    message: *const GosVec,
    tag: *const GosVec,
) -> i32 {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        // SAFETY: `tag` is this shim's argument, live for the call or null.
        let tag = unsafe { bytes(tag) };
        i32::from(CK::hmac_verify(CK::MacHash::Sha512, &key, &message, &tag))
    })
}

/// `crypto::hmac::verify_sha3_256`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_verify_sha3_256(
    key: *const GosVec,
    message: *const GosVec,
    tag: *const GosVec,
) -> i32 {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        // SAFETY: `tag` is this shim's argument, live for the call or null.
        let tag = unsafe { bytes(tag) };
        i32::from(CK::hmac_verify(CK::MacHash::Sha3_256, &key, &message, &tag))
    })
}

/// `crypto::hmac::verify_sha3_512`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hmac_verify_sha3_512(
    key: *const GosVec,
    message: *const GosVec,
    tag: *const GosVec,
) -> i32 {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        // SAFETY: `tag` is this shim's argument, live for the call or null.
        let tag = unsafe { bytes(tag) };
        i32::from(CK::hmac_verify(CK::MacHash::Sha3_512, &key, &message, &tag))
    })
}

/// `crypto::insecure::hmac_sha1`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_insecure_hmac_sha1(
    key: *const GosVec,
    message: *const GosVec,
) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        super::encoding::bytes_to_gosvec(&CK::hmac(CK::MacHash::Sha1, &key, &message))
    })
}

/// `crypto::insecure::hmac_sha1_hex`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_insecure_hmac_sha1_hex(
    key: *const GosVec,
    message: *const GosVec,
) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        alloc_cstring(hex(&CK::hmac(CK::MacHash::Sha1, &key, &message)).as_bytes())
    })
}

/// `crypto::insecure::verify_hmac_sha1`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_insecure_verify_hmac_sha1(
    key: *const GosVec,
    message: *const GosVec,
    tag: *const GosVec,
) -> i32 {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call or null.
        let key = unsafe { bytes(key) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        // SAFETY: `tag` is this shim's argument, live for the call or null.
        let tag = unsafe { bytes(tag) };
        i32::from(CK::hmac_verify(CK::MacHash::Sha1, &key, &message, &tag))
    })
}

/// `crypto::ecdsa::sign_fixed_pem`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_sign_fixed_pem(
    secret_pem: *const c_char,
    message: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `secret_pem` is this shim's argument, live for the call or null.
        let secret_pem = unsafe { text(secret_pem) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        vec_result(CK::ecdsa_sign(CK::Curve::P256, &secret_pem, &message, true))
    })
}

/// `crypto::ecdsa::verify_fixed_pem`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_verify_fixed_pem(
    public_pem: *const c_char,
    message: *const GosVec,
    signature: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `public_pem` is this shim's argument, live for the call or null.
        let public_pem = unsafe { text(public_pem) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        // SAFETY: `signature` is this shim's argument, live for the call or null.
        let signature = unsafe { bytes(signature) };
        match CK::ecdsa_verify(CK::Curve::P256, &public_pem, &message, &signature, true) {
            Ok(()) => gos_rt_result_new(0, 0),
            Err(message) => err(&message),
        }
    })
}

/// `crypto::ecdsa::der_to_fixed`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_der_to_fixed(signature: *const GosVec) -> i128 {
    ffi_entry!({
        // SAFETY: `signature` is this shim's argument, live for the call or null.
        let signature = unsafe { bytes(signature) };
        vec_result(CK::ecdsa_convert(CK::Curve::P256, &signature, true))
    })
}

/// `crypto::ecdsa::fixed_to_der`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_fixed_to_der(signature: *const GosVec) -> i128 {
    ffi_entry!({
        // SAFETY: `signature` is this shim's argument, live for the call or null.
        let signature = unsafe { bytes(signature) };
        vec_result(CK::ecdsa_convert(CK::Curve::P256, &signature, false))
    })
}

/// `crypto::ecdsa::p384_keypair_pem`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_crypto_ecdsa_p384_keypair_pem() -> i128 {
    ffi_entry!({
        match CK::ecdsa_keypair_pem(CK::Curve::P384) {
            Ok(pair) => super::result::ok_string_pair(pair),
            Err(message) => err(&message),
        }
    })
}

/// `crypto::ecdsa::p384_sign_pem`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_p384_sign_pem(
    secret_pem: *const c_char,
    message: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `secret_pem` is this shim's argument, live for the call or null.
        let secret_pem = unsafe { text(secret_pem) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        vec_result(CK::ecdsa_sign(
            CK::Curve::P384,
            &secret_pem,
            &message,
            false,
        ))
    })
}

/// `crypto::ecdsa::p384_verify_pem`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_p384_verify_pem(
    public_pem: *const c_char,
    message: *const GosVec,
    signature: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `public_pem` is this shim's argument, live for the call or null.
        let public_pem = unsafe { text(public_pem) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        // SAFETY: `signature` is this shim's argument, live for the call or null.
        let signature = unsafe { bytes(signature) };
        match CK::ecdsa_verify(CK::Curve::P384, &public_pem, &message, &signature, false) {
            Ok(()) => gos_rt_result_new(0, 0),
            Err(message) => err(&message),
        }
    })
}

/// `crypto::ecdsa::p384_sign_fixed_pem`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_p384_sign_fixed_pem(
    secret_pem: *const c_char,
    message: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `secret_pem` is this shim's argument, live for the call or null.
        let secret_pem = unsafe { text(secret_pem) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        vec_result(CK::ecdsa_sign(CK::Curve::P384, &secret_pem, &message, true))
    })
}

/// `crypto::ecdsa::p384_verify_fixed_pem`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_p384_verify_fixed_pem(
    public_pem: *const c_char,
    message: *const GosVec,
    signature: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `public_pem` is this shim's argument, live for the call or null.
        let public_pem = unsafe { text(public_pem) };
        // SAFETY: `message` is this shim's argument, live for the call or null.
        let message = unsafe { bytes(message) };
        // SAFETY: `signature` is this shim's argument, live for the call or null.
        let signature = unsafe { bytes(signature) };
        match CK::ecdsa_verify(CK::Curve::P384, &public_pem, &message, &signature, true) {
            Ok(()) => gos_rt_result_new(0, 0),
            Err(message) => err(&message),
        }
    })
}

/// `crypto::ecdsa::p384_der_to_fixed`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_p384_der_to_fixed(signature: *const GosVec) -> i128 {
    ffi_entry!({
        // SAFETY: `signature` is this shim's argument, live for the call or null.
        let signature = unsafe { bytes(signature) };
        vec_result(CK::ecdsa_convert(CK::Curve::P384, &signature, true))
    })
}

/// `crypto::ecdsa::p384_fixed_to_der`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_p384_fixed_to_der(signature: *const GosVec) -> i128 {
    ffi_entry!({
        // SAFETY: `signature` is this shim's argument, live for the call or null.
        let signature = unsafe { bytes(signature) };
        vec_result(CK::ecdsa_convert(CK::Curve::P384, &signature, false))
    })
}

/// `crypto::rsa::generate`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_crypto_rsa_generate(bits: i64) -> i128 {
    ffi_entry!({
        match CK::rsa_generate(bits) {
            Ok(pair) => super::result::ok_string_pair(pair),
            Err(message) => err(&message),
        }
    })
}

/// `crypto::kdf::hkdf_sha384`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hkdf_sha384(
    ikm: *const GosVec,
    salt: *const GosVec,
    info: *const GosVec,
    length: i64,
) -> i128 {
    ffi_entry!({
        // SAFETY: `ikm` is this shim's argument, live for the call or null.
        let ikm = unsafe { bytes(ikm) };
        // SAFETY: `salt` is this shim's argument, live for the call or null.
        let salt = unsafe { bytes(salt) };
        // SAFETY: `info` is this shim's argument, live for the call or null.
        let info = unsafe { bytes(info) };
        vec_result(CI::hkdf(CI::HkdfHash::Sha384, &ikm, &salt, &info, length))
    })
}

/// `__gos_ecdh_keypair_raw(scheme) -> Result<(Vec<u8>, Vec<u8>), Error>`:
/// `(secret, public)` for scheme 0 X25519, 1 P-256, 2 P-384.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_ecdh_keypair_raw(scheme: i64) -> i128 {
    ffi_entry!({ vec_pair(CK::Agreement::from_selector(scheme).and_then(CK::agreement_keypair)) })
}

/// `__gos_ecdh_agree_raw(scheme, secret, peer_public) -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ecdh_agree_raw(
    scheme: i64,
    secret: *const GosVec,
    peer_public: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `secret` and `peer_public` are this shim's arguments, live for
        // the call or null.
        let (secret, peer_public) = unsafe { (bytes(secret), bytes(peer_public)) };
        vec_result(
            CK::Agreement::from_selector(scheme)
                .and_then(|scheme| CK::agree(scheme, &secret, &peer_public)),
        )
    })
}

/// `__gos_mlkem_keypair_raw(set) -> Result<(Vec<u8>, Vec<u8>), Error>`:
/// `(decapsulation seed, encapsulation key)` for ML-KEM-768 or ML-KEM-1024.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_mlkem_keypair_raw(set: i64) -> i128 {
    ffi_entry!({ vec_pair(CK::KemSet::from_selector(set).and_then(CK::kem_keypair)) })
}

/// `__gos_mlkem_encapsulate_raw(set, ek) -> Result<(Vec<u8>, Vec<u8>), Error>`:
/// `(ciphertext, shared secret)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_mlkem_encapsulate_raw(set: i64, ek: *const GosVec) -> i128 {
    ffi_entry!({
        // SAFETY: `ek` is this shim's argument, live for the call or null.
        let ek = unsafe { bytes(ek) };
        vec_pair(CK::KemSet::from_selector(set).and_then(|set| CK::kem_encapsulate(set, &ek)))
    })
}

/// `__gos_mlkem_decapsulate_raw(set, dk, ciphertext) -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_mlkem_decapsulate_raw(
    set: i64,
    dk: *const GosVec,
    ciphertext: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `dk` and `ciphertext` are this shim's arguments, live for the
        // call or null.
        let (dk, ciphertext) = unsafe { (bytes(dk), bytes(ciphertext)) };
        vec_result(
            CK::KemSet::from_selector(set)
                .and_then(|set| CK::kem_decapsulate(set, &dk, &ciphertext)),
        )
    })
}

/// `crypto::x509::private_key_pkcs8`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_x509_private_key_pkcs8(pem: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `pem` is this shim's String argument, live for the call or null.
        let pem = unsafe { text(pem) };
        match CK::private_key_pem(&pem) {
            Ok(text) => gos_rt_result_new(0, alloc_cstring(text.as_bytes()) as i64),
            Err(message) => err(&message),
        }
    })
}

/// `crypto::x509::private_key_der`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_x509_private_key_der(pem: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `pem` is this shim's String argument, live for the call or null.
        let pem = unsafe { text(pem) };
        vec_result(CK::private_key_der(&pem))
    })
}

/// `crypto::x509::decrypt_private_key`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_x509_decrypt_private_key(
    pem: *const c_char,
    password: *const c_char,
) -> i128 {
    ffi_entry!({
        // SAFETY: `pem` is this shim's String argument, live for the call or null.
        let pem = unsafe { text(pem) };
        // SAFETY: `password` is this shim's String argument, live for the call or null.
        let password = unsafe { text(password) };
        match CK::decrypt_private_key(&pem, &password) {
            Ok(text) => gos_rt_result_new(0, alloc_cstring(text.as_bytes()) as i64),
            Err(message) => err(&message),
        }
    })
}

/// `crypto::x509::public_key_spki`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_x509_public_key_spki(pem: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `pem` is this shim's String argument, live for the call or null.
        let pem = unsafe { text(pem) };
        match CK::public_key_pem(&pem) {
            Ok(text) => gos_rt_result_new(0, alloc_cstring(text.as_bytes()) as i64),
            Err(message) => err(&message),
        }
    })
}

/// `crypto::x509::public_key_der`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_x509_public_key_der(pem: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `pem` is this shim's String argument, live for the call or null.
        let pem = unsafe { text(pem) };
        vec_result(CK::public_key_der(&pem))
    })
}
