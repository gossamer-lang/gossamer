//! C-ABI shims over `crate::crypto_interop`: SHA-3, HKDF, RSA signatures,
//! and X.509 generation for compiled code.

use std::os::raw::c_char;

use super::encoding::bytes_to_gosvec;
use super::result::{gos_rt_result_new, ok_string_pair};
use super::vec::GosVec;

fn interop_err(message: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
    gos_rt_result_new(1, err as i64)
}

/// A String argument as owned text, or the empty string for null.
///
/// # Safety
///
/// `ptr` is null or a live string body for the call.
unsafe fn text(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        // SAFETY: the caller's contract.
        unsafe { crate::c_abi::gos_str_arg_string(ptr) }
    }
}

/// `crypto::sha3_256::digest(data) -> Vec<u8>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_sha3_256_digest(data: *const GosVec) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let data = unsafe { crate::c_abi::vec::vec_bytes(data) };
        bytes_to_gosvec(&crate::crypto_interop::sha3_256(&data))
    })
}

/// `crypto::sha3_256::hex(data) -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_sha3_256_hex(data: *const GosVec) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let data = unsafe { crate::c_abi::vec::vec_bytes(data) };
        super::alloc_cstring(
            crate::codec::hex::encode(&crate::crypto_interop::sha3_256(&data)).as_bytes(),
        )
    })
}

/// `crypto::sha3_512::digest(data) -> Vec<u8>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_sha3_512_digest(data: *const GosVec) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let data = unsafe { crate::c_abi::vec::vec_bytes(data) };
        bytes_to_gosvec(&crate::crypto_interop::sha3_512(&data))
    })
}

/// `crypto::sha3_512::hex(data) -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_sha3_512_hex(data: *const GosVec) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let data = unsafe { crate::c_abi::vec::vec_bytes(data) };
        super::alloc_cstring(
            crate::codec::hex::encode(&crate::crypto_interop::sha3_512(&data)).as_bytes(),
        )
    })
}

/// `crypto::kdf::hkdf_sha256(ikm, salt, info, length) -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hkdf_sha256(
    ikm: *const GosVec,
    salt: *const GosVec,
    info: *const GosVec,
    length: i64,
) -> i128 {
    ffi_entry!({
        // SAFETY: `ikm` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let ikm = unsafe { crate::c_abi::vec::vec_bytes(ikm) };
        // SAFETY: `salt` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let salt = unsafe { crate::c_abi::vec::vec_bytes(salt) };
        // SAFETY: `info` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let info = unsafe { crate::c_abi::vec::vec_bytes(info) };
        match crate::crypto_interop::hkdf(
            crate::crypto_interop::HkdfHash::Sha256,
            &ikm,
            &salt,
            &info,
            length,
        ) {
            Ok(bytes) => gos_rt_result_new(0, bytes_to_gosvec(&bytes) as i64),
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::kdf::hkdf_sha512(ikm, salt, info, length) -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_hkdf_sha512(
    ikm: *const GosVec,
    salt: *const GosVec,
    info: *const GosVec,
    length: i64,
) -> i128 {
    ffi_entry!({
        // SAFETY: `ikm` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let ikm = unsafe { crate::c_abi::vec::vec_bytes(ikm) };
        // SAFETY: `salt` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let salt = unsafe { crate::c_abi::vec::vec_bytes(salt) };
        // SAFETY: `info` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let info = unsafe { crate::c_abi::vec::vec_bytes(info) };
        match crate::crypto_interop::hkdf(
            crate::crypto_interop::HkdfHash::Sha512,
            &ikm,
            &salt,
            &info,
            length,
        ) {
            Ok(bytes) => gos_rt_result_new(0, bytes_to_gosvec(&bytes) as i64),
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::rsa::sign_pkcs1_sha256(secret_pem, message) -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_rsa_sign_pkcs1_sha256(
    secret_pem: *const c_char,
    message: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `secret_pem` is this shim's String argument (C-ABI contract).
        let secret_pem = unsafe { text(secret_pem) };
        // SAFETY: `message` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let message = unsafe { crate::c_abi::vec::vec_bytes(message) };
        match crate::crypto_interop::rsa_sign(
            crate::crypto_interop::RsaScheme::Pkcs1Sha256,
            &secret_pem,
            &message,
        ) {
            Ok(bytes) => gos_rt_result_new(0, bytes_to_gosvec(&bytes) as i64),
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::rsa::sign_pss_sha256(secret_pem, message) -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_rsa_sign_pss_sha256(
    secret_pem: *const c_char,
    message: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `secret_pem` is this shim's String argument (C-ABI contract).
        let secret_pem = unsafe { text(secret_pem) };
        // SAFETY: `message` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let message = unsafe { crate::c_abi::vec::vec_bytes(message) };
        match crate::crypto_interop::rsa_sign(
            crate::crypto_interop::RsaScheme::PssSha256,
            &secret_pem,
            &message,
        ) {
            Ok(bytes) => gos_rt_result_new(0, bytes_to_gosvec(&bytes) as i64),
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::rsa::encrypt_oaep_sha256(public_pem, plaintext) -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_rsa_encrypt_oaep_sha256(
    public_pem: *const c_char,
    plaintext: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `public_pem` is this shim's String argument (C-ABI contract).
        let public_pem = unsafe { text(public_pem) };
        // SAFETY: `plaintext` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let plaintext = unsafe { crate::c_abi::vec::vec_bytes(plaintext) };
        match crate::crypto_interop::rsa_encrypt_oaep_sha256(&public_pem, &plaintext) {
            Ok(bytes) => gos_rt_result_new(0, bytes_to_gosvec(&bytes) as i64),
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::rsa::decrypt_oaep_sha256(secret_pem, ciphertext) -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_rsa_decrypt_oaep_sha256(
    secret_pem: *const c_char,
    ciphertext: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `secret_pem` is this shim's String argument (C-ABI contract).
        let secret_pem = unsafe { text(secret_pem) };
        // SAFETY: `ciphertext` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let ciphertext = unsafe { crate::c_abi::vec::vec_bytes(ciphertext) };
        match crate::crypto_interop::rsa_decrypt_oaep_sha256(&secret_pem, &ciphertext) {
            Ok(bytes) => gos_rt_result_new(0, bytes_to_gosvec(&bytes) as i64),
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::rsa::verify_pkcs1_sha256(public_pem, message, signature) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_rsa_verify_pkcs1_sha256(
    public_pem: *const c_char,
    message: *const GosVec,
    signature: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `public_pem` is this shim's String argument (C-ABI contract).
        let public_pem = unsafe { text(public_pem) };
        // SAFETY: `message` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let message = unsafe { crate::c_abi::vec::vec_bytes(message) };
        // SAFETY: `signature` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let signature = unsafe { crate::c_abi::vec::vec_bytes(signature) };
        match crate::crypto_interop::rsa_verify(
            crate::crypto_interop::RsaScheme::Pkcs1Sha256,
            &public_pem,
            &message,
            &signature,
        ) {
            Ok(()) => gos_rt_result_new(0, 0),
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::rsa::verify_pss_sha256(public_pem, message, signature) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_rsa_verify_pss_sha256(
    public_pem: *const c_char,
    message: *const GosVec,
    signature: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `public_pem` is this shim's String argument (C-ABI contract).
        let public_pem = unsafe { text(public_pem) };
        // SAFETY: `message` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let message = unsafe { crate::c_abi::vec::vec_bytes(message) };
        // SAFETY: `signature` is this shim's argument, live for the call or null (C-ABI
        // contract), which `vec_bytes` accepts.
        let signature = unsafe { crate::c_abi::vec::vec_bytes(signature) };
        match crate::crypto_interop::rsa_verify(
            crate::crypto_interop::RsaScheme::PssSha256,
            &public_pem,
            &message,
            &signature,
        ) {
            Ok(()) => gos_rt_result_new(0, 0),
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::rsa::public_key_pem(secret_pem) -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_rsa_public_key_pem(secret_pem: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `secret_pem` is this shim's String argument (C-ABI contract).
        let secret_pem = unsafe { text(secret_pem) };
        match crate::crypto_interop::rsa_public_key_pem(&secret_pem) {
            Ok(text) => gos_rt_result_new(0, super::alloc_cstring(text.as_bytes()) as i64),
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::x509::self_signed(common_name, dns_names, days, is_ca, client) -> Result<(String, String), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_x509_self_signed(
    common_name: *const c_char,
    dns_names: *mut GosVec,
    days: i64,
    is_ca: i8,
    client: i8,
    extensions: *const c_char,
) -> i128 {
    ffi_entry!({
        // SAFETY: `common_name` is this shim's String argument (C-ABI contract).
        let common_name = unsafe { text(common_name) };
        // SAFETY: `dns_names` is this shim's `Vec<String>` argument, live or null (C-ABI contract).
        let dns_names = unsafe { crate::c_abi::exec::argv_strings(dns_names) };
        // SAFETY: `extensions` is this shim's String argument (C-ABI contract).
        let extensions = match crate::crypto_interop::parse_extensions(&unsafe { text(extensions) })
        {
            Ok(extensions) => extensions,
            Err(e) => return interop_err(&e),
        };
        let is_ca = is_ca != 0;
        let client = client != 0;
        match crate::crypto_interop::self_signed(
            &common_name,
            &dns_names,
            days,
            crate::crypto_interop::CertUse { is_ca, client },
            &extensions,
        ) {
            Ok(pair) => ok_string_pair(pair),
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::x509::issue(ca_cert_pem, ca_key_pem, common_name, dns_names, days, is_ca, client) -> Result<(String, String), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_x509_issue(
    ca_cert_pem: *const c_char,
    ca_key_pem: *const c_char,
    common_name: *const c_char,
    dns_names: *mut GosVec,
    days: i64,
    is_ca: i8,
    client: i8,
    extensions: *const c_char,
) -> i128 {
    ffi_entry!({
        // SAFETY: `extensions` is this shim's String argument (C-ABI contract).
        let extensions = match crate::crypto_interop::parse_extensions(&unsafe { text(extensions) })
        {
            Ok(extensions) => extensions,
            Err(e) => return interop_err(&e),
        };
        // SAFETY: `ca_cert_pem` is this shim's String argument (C-ABI contract).
        let ca_cert_pem = unsafe { text(ca_cert_pem) };
        // SAFETY: `ca_key_pem` is this shim's String argument (C-ABI contract).
        let ca_key_pem = unsafe { text(ca_key_pem) };
        // SAFETY: `common_name` is this shim's String argument (C-ABI contract).
        let common_name = unsafe { text(common_name) };
        // SAFETY: `dns_names` is this shim's `Vec<String>` argument, live or null (C-ABI contract).
        let dns_names = unsafe { crate::c_abi::exec::argv_strings(dns_names) };
        let is_ca = is_ca != 0;
        let client = client != 0;
        match crate::crypto_interop::issue(
            &ca_cert_pem,
            &ca_key_pem,
            &common_name,
            &dns_names,
            days,
            crate::crypto_interop::CertUse { is_ca, client },
            &extensions,
        ) {
            Ok(pair) => ok_string_pair(pair),
            Err(e) => interop_err(&e),
        }
    })
}

/// `__gos_x509_extensions_raw(cert_pem) -> Result<String, Error>`: the
/// certificate's extensions as JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_x509_extensions_raw(cert_pem: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `cert_pem` is this shim's String argument (C-ABI contract).
        let cert_pem = unsafe { text(cert_pem) };
        match crate::crypto_interop::extensions_json(&cert_pem) {
            Ok(json) => {
                let p = super::string::alloc_cstring(json.as_bytes());
                gos_rt_result_new(0, p as i64)
            }
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::x509::csr(common_name, dns_names) -> Result<(String, String), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_x509_csr(
    common_name: *const c_char,
    dns_names: *mut GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `common_name` is this shim's String argument (C-ABI contract).
        let common_name = unsafe { text(common_name) };
        // SAFETY: `dns_names` is this shim's `Vec<String>` argument, live or null (C-ABI contract).
        let dns_names = unsafe { crate::c_abi::exec::argv_strings(dns_names) };
        match crate::crypto_interop::csr(&common_name, &dns_names) {
            Ok(pair) => ok_string_pair(pair),
            Err(e) => interop_err(&e),
        }
    })
}

/// `crypto::x509::sign_csr(csr_pem, ca_cert_pem, ca_key_pem, days, client) -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_x509_sign_csr(
    csr_pem: *const c_char,
    ca_cert_pem: *const c_char,
    ca_key_pem: *const c_char,
    days: i64,
    client: i8,
) -> i128 {
    ffi_entry!({
        // SAFETY: `csr_pem` is this shim's String argument (C-ABI contract).
        let csr_pem = unsafe { text(csr_pem) };
        // SAFETY: `ca_cert_pem` is this shim's String argument (C-ABI contract).
        let ca_cert_pem = unsafe { text(ca_cert_pem) };
        // SAFETY: `ca_key_pem` is this shim's String argument (C-ABI contract).
        let ca_key_pem = unsafe { text(ca_key_pem) };
        let client = client != 0;
        match crate::crypto_interop::sign_csr(&csr_pem, &ca_cert_pem, &ca_key_pem, days, client) {
            Ok(text) => gos_rt_result_new(0, super::alloc_cstring(text.as_bytes()) as i64),
            Err(e) => interop_err(&e),
        }
    })
}
