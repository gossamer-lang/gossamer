//! Interpreter entries for `gossamer_runtime::crypto_keys`, the code the
//! compiled tiers call through `gos_rt_crypto_*`.

use std::sync::Arc;

use gossamer_runtime::crypto_interop as CI;
use gossamer_runtime::crypto_keys as CK;

use super::crypto::{bytes_to_value_array, value_to_bytes};
use crate::builtins::{BuiltinFnPub, as_str, err_variant, ok_variant, value_to_int};
use crate::value::{RuntimeResult, Value};

fn hex(bytes: &[u8]) -> String {
    gossamer_runtime::codec::hex::encode(bytes)
}

fn bytes(args: &[Value], n: usize) -> Vec<u8> {
    value_to_bytes(args.get(n).unwrap_or(&Value::Unit))
}

fn text(args: &[Value], n: usize) -> String {
    args.get(n).and_then(as_str).unwrap_or("").to_string()
}

fn int(args: &[Value], n: usize) -> i64 {
    args.get(n).and_then(value_to_int).unwrap_or(0)
}

pub(crate) fn install_crypto_keys(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&'static str, BuiltinFnPub)] = &[
        ("crypto::sha384::digest", vm_sha384_digest),
        ("crypto::sha384::hex", vm_sha384_hex),
        ("crypto::hmac::sha384_mac", vm_hmac_sha384_mac),
        ("crypto::hmac::sha384_hex", vm_hmac_sha384_hex),
        ("crypto::hmac::sha512_mac", vm_hmac_sha512_mac),
        ("crypto::hmac::sha512_hex", vm_hmac_sha512_hex),
        ("crypto::hmac::sha3_256_mac", vm_hmac_sha3_256_mac),
        ("crypto::hmac::sha3_256_hex", vm_hmac_sha3_256_hex),
        ("crypto::hmac::sha3_512_mac", vm_hmac_sha3_512_mac),
        ("crypto::hmac::sha3_512_hex", vm_hmac_sha3_512_hex),
        ("crypto::hmac::verify_sha256", vm_hmac_verify_sha256),
        ("crypto::hmac::verify_sha384", vm_hmac_verify_sha384),
        ("crypto::hmac::verify_sha512", vm_hmac_verify_sha512),
        ("crypto::hmac::verify_sha3_256", vm_hmac_verify_sha3_256),
        ("crypto::hmac::verify_sha3_512", vm_hmac_verify_sha3_512),
        ("crypto::insecure::hmac_sha1", vm_insecure_hmac_sha1),
        ("crypto::insecure::hmac_sha1_hex", vm_insecure_hmac_sha1_hex),
        (
            "crypto::insecure::verify_hmac_sha1",
            vm_insecure_verify_hmac_sha1,
        ),
        ("crypto::ecdsa::sign_fixed_pem", vm_ecdsa_sign_fixed_pem),
        ("crypto::ecdsa::verify_fixed_pem", vm_ecdsa_verify_fixed_pem),
        ("crypto::ecdsa::der_to_fixed", vm_ecdsa_der_to_fixed),
        ("crypto::ecdsa::fixed_to_der", vm_ecdsa_fixed_to_der),
        ("crypto::ecdsa::p384_keypair_pem", vm_ecdsa_p384_keypair_pem),
        ("crypto::ecdsa::p384_sign_pem", vm_ecdsa_p384_sign_pem),
        ("crypto::ecdsa::p384_verify_pem", vm_ecdsa_p384_verify_pem),
        (
            "crypto::ecdsa::p384_sign_fixed_pem",
            vm_ecdsa_p384_sign_fixed_pem,
        ),
        (
            "crypto::ecdsa::p384_verify_fixed_pem",
            vm_ecdsa_p384_verify_fixed_pem,
        ),
        (
            "crypto::ecdsa::p384_der_to_fixed",
            vm_ecdsa_p384_der_to_fixed,
        ),
        (
            "crypto::ecdsa::p384_fixed_to_der",
            vm_ecdsa_p384_fixed_to_der,
        ),
        ("crypto::rsa::generate", vm_rsa_generate),
        ("crypto::kdf::hkdf_sha384", vm_hkdf_sha384),
        ("crypto::x509::private_key_pkcs8", vm_x509_private_key_pkcs8),
        ("crypto::x509::private_key_der", vm_x509_private_key_der),
        (
            "crypto::x509::decrypt_private_key",
            vm_x509_decrypt_private_key,
        ),
        ("crypto::x509::public_key_spki", vm_x509_public_key_spki),
        ("crypto::x509::public_key_der", vm_x509_public_key_der),
        ("__gos_ecdh_keypair_raw", vm_ecdh_keypair_raw),
        ("__gos_ecdh_agree_raw", vm_ecdh_agree_raw),
        ("__gos_mlkem_keypair_raw", vm_mlkem_keypair_raw),
        ("__gos_mlkem_encapsulate_raw", vm_mlkem_encapsulate_raw),
        ("__gos_mlkem_decapsulate_raw", vm_mlkem_decapsulate_raw),
    ];
    for (name, call) in entries {
        globals.push((name, crate::builtins::builtin_pub(name, *call)));
    }
}

/// `crypto::sha384::digest`.
fn vm_sha384_digest(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes(args, 0);
    Ok(bytes_to_value_array(&CK::sha384(&data)))
}

/// `crypto::sha384::hex`.
fn vm_sha384_hex(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes(args, 0);
    Ok(Value::String(hex(&CK::sha384(&data)).into()))
}

/// `crypto::hmac::sha384_mac`.
fn vm_hmac_sha384_mac(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    Ok(bytes_to_value_array(&CK::hmac(
        CK::MacHash::Sha384,
        &key,
        &message,
    )))
}

/// `crypto::hmac::sha384_hex`.
fn vm_hmac_sha384_hex(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    Ok(Value::String(
        hex(&CK::hmac(CK::MacHash::Sha384, &key, &message)).into(),
    ))
}

/// `crypto::hmac::sha512_mac`.
fn vm_hmac_sha512_mac(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    Ok(bytes_to_value_array(&CK::hmac(
        CK::MacHash::Sha512,
        &key,
        &message,
    )))
}

/// `crypto::hmac::sha512_hex`.
fn vm_hmac_sha512_hex(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    Ok(Value::String(
        hex(&CK::hmac(CK::MacHash::Sha512, &key, &message)).into(),
    ))
}

/// `crypto::hmac::sha3_256_mac`.
fn vm_hmac_sha3_256_mac(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    Ok(bytes_to_value_array(&CK::hmac(
        CK::MacHash::Sha3_256,
        &key,
        &message,
    )))
}

/// `crypto::hmac::sha3_256_hex`.
fn vm_hmac_sha3_256_hex(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    Ok(Value::String(
        hex(&CK::hmac(CK::MacHash::Sha3_256, &key, &message)).into(),
    ))
}

/// `crypto::hmac::sha3_512_mac`.
fn vm_hmac_sha3_512_mac(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    Ok(bytes_to_value_array(&CK::hmac(
        CK::MacHash::Sha3_512,
        &key,
        &message,
    )))
}

/// `crypto::hmac::sha3_512_hex`.
fn vm_hmac_sha3_512_hex(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    Ok(Value::String(
        hex(&CK::hmac(CK::MacHash::Sha3_512, &key, &message)).into(),
    ))
}

/// `crypto::hmac::verify_sha256`.
fn vm_hmac_verify_sha256(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    let tag = bytes(args, 2);
    Ok(Value::Bool(CK::hmac_verify(
        CK::MacHash::Sha256,
        &key,
        &message,
        &tag,
    )))
}

/// `crypto::hmac::verify_sha384`.
fn vm_hmac_verify_sha384(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    let tag = bytes(args, 2);
    Ok(Value::Bool(CK::hmac_verify(
        CK::MacHash::Sha384,
        &key,
        &message,
        &tag,
    )))
}

/// `crypto::hmac::verify_sha512`.
fn vm_hmac_verify_sha512(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    let tag = bytes(args, 2);
    Ok(Value::Bool(CK::hmac_verify(
        CK::MacHash::Sha512,
        &key,
        &message,
        &tag,
    )))
}

/// `crypto::hmac::verify_sha3_256`.
fn vm_hmac_verify_sha3_256(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    let tag = bytes(args, 2);
    Ok(Value::Bool(CK::hmac_verify(
        CK::MacHash::Sha3_256,
        &key,
        &message,
        &tag,
    )))
}

/// `crypto::hmac::verify_sha3_512`.
fn vm_hmac_verify_sha3_512(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    let tag = bytes(args, 2);
    Ok(Value::Bool(CK::hmac_verify(
        CK::MacHash::Sha3_512,
        &key,
        &message,
        &tag,
    )))
}

/// `crypto::insecure::hmac_sha1`.
fn vm_insecure_hmac_sha1(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    Ok(bytes_to_value_array(&CK::hmac(
        CK::MacHash::Sha1,
        &key,
        &message,
    )))
}

/// `crypto::insecure::hmac_sha1_hex`.
fn vm_insecure_hmac_sha1_hex(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    Ok(Value::String(
        hex(&CK::hmac(CK::MacHash::Sha1, &key, &message)).into(),
    ))
}

/// `crypto::insecure::verify_hmac_sha1`.
fn vm_insecure_verify_hmac_sha1(args: &[Value]) -> RuntimeResult<Value> {
    let key = bytes(args, 0);
    let message = bytes(args, 1);
    let tag = bytes(args, 2);
    Ok(Value::Bool(CK::hmac_verify(
        CK::MacHash::Sha1,
        &key,
        &message,
        &tag,
    )))
}

/// `crypto::ecdsa::sign_fixed_pem`.
fn vm_ecdsa_sign_fixed_pem(args: &[Value]) -> RuntimeResult<Value> {
    let secret_pem = text(args, 0);
    let message = bytes(args, 1);
    Ok(
        match CK::ecdsa_sign(CK::Curve::P256, &secret_pem, &message, true) {
            Ok(v) => ok_variant(bytes_to_value_array(&v)),
            Err(e) => err_variant(e),
        },
    )
}

/// `crypto::ecdsa::verify_fixed_pem`.
fn vm_ecdsa_verify_fixed_pem(args: &[Value]) -> RuntimeResult<Value> {
    let public_pem = text(args, 0);
    let message = bytes(args, 1);
    let signature = bytes(args, 2);
    Ok(
        match CK::ecdsa_verify(CK::Curve::P256, &public_pem, &message, &signature, true) {
            Ok(()) => ok_variant(Value::Unit),
            Err(e) => err_variant(e),
        },
    )
}

/// `crypto::ecdsa::der_to_fixed`.
fn vm_ecdsa_der_to_fixed(args: &[Value]) -> RuntimeResult<Value> {
    let signature = bytes(args, 0);
    Ok(match CK::ecdsa_convert(CK::Curve::P256, &signature, true) {
        Ok(v) => ok_variant(bytes_to_value_array(&v)),
        Err(e) => err_variant(e),
    })
}

/// `crypto::ecdsa::fixed_to_der`.
fn vm_ecdsa_fixed_to_der(args: &[Value]) -> RuntimeResult<Value> {
    let signature = bytes(args, 0);
    Ok(
        match CK::ecdsa_convert(CK::Curve::P256, &signature, false) {
            Ok(v) => ok_variant(bytes_to_value_array(&v)),
            Err(e) => err_variant(e),
        },
    )
}

/// `crypto::ecdsa::p384_keypair_pem`.
fn vm_ecdsa_p384_keypair_pem(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(match CK::ecdsa_keypair_pem(CK::Curve::P384) {
        Ok((a, b)) => ok_variant(Value::Tuple(Arc::from(vec![
            Value::String(a.into()),
            Value::String(b.into()),
        ]))),
        Err(e) => err_variant(e),
    })
}

/// `crypto::ecdsa::p384_sign_pem`.
fn vm_ecdsa_p384_sign_pem(args: &[Value]) -> RuntimeResult<Value> {
    let secret_pem = text(args, 0);
    let message = bytes(args, 1);
    Ok(
        match CK::ecdsa_sign(CK::Curve::P384, &secret_pem, &message, false) {
            Ok(v) => ok_variant(bytes_to_value_array(&v)),
            Err(e) => err_variant(e),
        },
    )
}

/// `crypto::ecdsa::p384_verify_pem`.
fn vm_ecdsa_p384_verify_pem(args: &[Value]) -> RuntimeResult<Value> {
    let public_pem = text(args, 0);
    let message = bytes(args, 1);
    let signature = bytes(args, 2);
    Ok(
        match CK::ecdsa_verify(CK::Curve::P384, &public_pem, &message, &signature, false) {
            Ok(()) => ok_variant(Value::Unit),
            Err(e) => err_variant(e),
        },
    )
}

/// `crypto::ecdsa::p384_sign_fixed_pem`.
fn vm_ecdsa_p384_sign_fixed_pem(args: &[Value]) -> RuntimeResult<Value> {
    let secret_pem = text(args, 0);
    let message = bytes(args, 1);
    Ok(
        match CK::ecdsa_sign(CK::Curve::P384, &secret_pem, &message, true) {
            Ok(v) => ok_variant(bytes_to_value_array(&v)),
            Err(e) => err_variant(e),
        },
    )
}

/// `crypto::ecdsa::p384_verify_fixed_pem`.
fn vm_ecdsa_p384_verify_fixed_pem(args: &[Value]) -> RuntimeResult<Value> {
    let public_pem = text(args, 0);
    let message = bytes(args, 1);
    let signature = bytes(args, 2);
    Ok(
        match CK::ecdsa_verify(CK::Curve::P384, &public_pem, &message, &signature, true) {
            Ok(()) => ok_variant(Value::Unit),
            Err(e) => err_variant(e),
        },
    )
}

/// `crypto::ecdsa::p384_der_to_fixed`.
fn vm_ecdsa_p384_der_to_fixed(args: &[Value]) -> RuntimeResult<Value> {
    let signature = bytes(args, 0);
    Ok(match CK::ecdsa_convert(CK::Curve::P384, &signature, true) {
        Ok(v) => ok_variant(bytes_to_value_array(&v)),
        Err(e) => err_variant(e),
    })
}

/// `crypto::ecdsa::p384_fixed_to_der`.
fn vm_ecdsa_p384_fixed_to_der(args: &[Value]) -> RuntimeResult<Value> {
    let signature = bytes(args, 0);
    Ok(
        match CK::ecdsa_convert(CK::Curve::P384, &signature, false) {
            Ok(v) => ok_variant(bytes_to_value_array(&v)),
            Err(e) => err_variant(e),
        },
    )
}

/// `crypto::rsa::generate`.
fn vm_rsa_generate(args: &[Value]) -> RuntimeResult<Value> {
    let bits = int(args, 0);
    Ok(match CK::rsa_generate(bits) {
        Ok((a, b)) => ok_variant(Value::Tuple(Arc::from(vec![
            Value::String(a.into()),
            Value::String(b.into()),
        ]))),
        Err(e) => err_variant(e),
    })
}

/// `crypto::kdf::hkdf_sha384`.
fn vm_hkdf_sha384(args: &[Value]) -> RuntimeResult<Value> {
    let ikm = bytes(args, 0);
    let salt = bytes(args, 1);
    let info = bytes(args, 2);
    let length = int(args, 3);
    Ok(
        match CI::hkdf(CI::HkdfHash::Sha384, &ikm, &salt, &info, length) {
            Ok(v) => ok_variant(bytes_to_value_array(&v)),
            Err(e) => err_variant(e),
        },
    )
}

fn vec_pair(answer: Result<(Vec<u8>, Vec<u8>), String>) -> Value {
    match answer {
        Ok((a, b)) => ok_variant(Value::Tuple(Arc::from(vec![
            bytes_to_value_array(&a),
            bytes_to_value_array(&b),
        ]))),
        Err(e) => err_variant(e),
    }
}

fn vec_result(answer: Result<Vec<u8>, String>) -> Value {
    match answer {
        Ok(v) => ok_variant(bytes_to_value_array(&v)),
        Err(e) => err_variant(e),
    }
}

/// `__gos_ecdh_keypair_raw(scheme)`.
fn vm_ecdh_keypair_raw(args: &[Value]) -> RuntimeResult<Value> {
    Ok(vec_pair(
        CK::Agreement::from_selector(int(args, 0)).and_then(CK::agreement_keypair),
    ))
}

/// `__gos_ecdh_agree_raw(scheme, secret, peer_public)`.
fn vm_ecdh_agree_raw(args: &[Value]) -> RuntimeResult<Value> {
    let (secret, peer) = (bytes(args, 1), bytes(args, 2));
    Ok(vec_result(
        CK::Agreement::from_selector(int(args, 0)).and_then(|s| CK::agree(s, &secret, &peer)),
    ))
}

/// `__gos_mlkem_keypair_raw(set)`.
fn vm_mlkem_keypair_raw(args: &[Value]) -> RuntimeResult<Value> {
    Ok(vec_pair(
        CK::KemSet::from_selector(int(args, 0)).and_then(CK::kem_keypair),
    ))
}

/// `__gos_mlkem_encapsulate_raw(set, ek)`.
fn vm_mlkem_encapsulate_raw(args: &[Value]) -> RuntimeResult<Value> {
    let ek = bytes(args, 1);
    Ok(vec_pair(
        CK::KemSet::from_selector(int(args, 0)).and_then(|set| CK::kem_encapsulate(set, &ek)),
    ))
}

/// `__gos_mlkem_decapsulate_raw(set, dk, ciphertext)`.
fn vm_mlkem_decapsulate_raw(args: &[Value]) -> RuntimeResult<Value> {
    let (dk, ct) = (bytes(args, 1), bytes(args, 2));
    Ok(vec_result(
        CK::KemSet::from_selector(int(args, 0)).and_then(|set| CK::kem_decapsulate(set, &dk, &ct)),
    ))
}

/// `crypto::x509::private_key_pkcs8`.
fn vm_x509_private_key_pkcs8(args: &[Value]) -> RuntimeResult<Value> {
    let pem = text(args, 0);
    Ok(match CK::private_key_pem(&pem) {
        Ok(text) => ok_variant(Value::String(text.into())),
        Err(e) => err_variant(e),
    })
}

/// `crypto::x509::private_key_der`.
fn vm_x509_private_key_der(args: &[Value]) -> RuntimeResult<Value> {
    let pem = text(args, 0);
    Ok(vec_result(CK::private_key_der(&pem)))
}

/// `crypto::x509::decrypt_private_key`.
fn vm_x509_decrypt_private_key(args: &[Value]) -> RuntimeResult<Value> {
    let pem = text(args, 0);
    let password = text(args, 1);
    Ok(match CK::decrypt_private_key(&pem, &password) {
        Ok(text) => ok_variant(Value::String(text.into())),
        Err(e) => err_variant(e),
    })
}

/// `crypto::x509::public_key_spki`.
fn vm_x509_public_key_spki(args: &[Value]) -> RuntimeResult<Value> {
    let pem = text(args, 0);
    Ok(match CK::public_key_pem(&pem) {
        Ok(text) => ok_variant(Value::String(text.into())),
        Err(e) => err_variant(e),
    })
}

/// `crypto::x509::public_key_der`.
fn vm_x509_public_key_der(args: &[Value]) -> RuntimeResult<Value> {
    let pem = text(args, 0);
    Ok(vec_result(CK::public_key_der(&pem)))
}
