//! C-ABI dispatch shims for `std::jwt`. These mirror the bytecode-VM
//! builtins in `gossamer-interp/src/stdlib_builtins/jwt.rs` so the
//! compiled (Cranelift / LLVM) tier resolves the same calls natively
//! instead of failing to link.
//!
//! The JWT surface signs / verifies a `Claims` *struct* in
//! `gossamer-std`, and a struct does not cross the C-ABI by value.
//! These shims therefore expose the struct-marshalling-free
//! **JSON-string** entry API: claims travel as a JSON object string,
//! and `verify_*` returns the canonical claims as a JSON object
//! string. The interp builtins call `gossamer_std::jwt::*_json`; the
//! runtime cannot (a `runtime -> std` dependency would cycle, since
//! `gossamer-std` already depends on `gossamer-runtime`), so the
//! algorithm is reimplemented here over the same crypto crates
//! (`sha2`, `p256`, `ed25519-dalek`) and the same claims
//! canonicalization, producing byte-identical tokens.
//!
//! Parity contract with `gossamer-std/src/jwt.rs`:
//! - Claims canonicalization (`normalize_claims`) mirrors
//!   `Claims::from_json` + `Claims::to_json`: registered claims are
//!   type-validated, a single-element `aud` collapses to a bare
//!   string, `exp`/`nbf`/`iat` coerce to integer seconds, and every
//!   key sorts (`serde_json::Map` is a `BTreeMap` - no `preserve_order`
//!   in the workspace `serde_json`).
//! - The JOSE header is `{"alg":<alg>,"typ":"JWT"}`, serialized
//!   through `serde_json` (keys sort to `alg`, `typ`).
//! - base64url is RFC 4648 §5, no padding.
//! - The same security invariants are enforced on verify: `alg:"none"`
//!   refused, `alg` header must equal the expected algorithm, HMAC
//!   verifiers reject asymmetric tokens and vice versa, `crit` is
//!   fatal, `typ` must be `JWT` or absent, HMAC compare is
//!   constant-time.
//!
//! Returns are packed as the runtime's `i128` Result:
//! `Result<String, errors::Error>` for every entry - `Ok` (disc 0)
//! carries a runtime c-string pointer, `Err` (disc 1) a fresh
//! `errors::Error`.

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::too_many_lines)]

use std::os::raw::c_char;
use std::time::UNIX_EPOCH;

use sha2::Digest;

use super::result::gos_rt_result_new;
use super::vec::GosVec;

/// Packs an `Ok(String)` result (disc 0, payload = runtime c-string).
fn ok_string(s: &str) -> i128 {
    let ptr = super::string::alloc_cstring(s.as_bytes());
    gos_rt_result_new(0, ptr as i64)
}

/// Packs an `Err(errors::Error)` result (disc 1, payload = a fresh
/// `errors::Error`). The message is fixed up to be NUL-safe.
fn jwt_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    gos_rt_result_new(1, err as i64)
}

unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    // SAFETY: this `unsafe fn`'s caller passes `p` live or null, which `gos_str_arg_text`
    // accepts.
    unsafe { crate::c_abi::gos_str_arg_text(p) }
}

// -- algorithm tag ---------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Alg {
    Hs256,
    Hs384,
    Hs512,
    Es256,
    EdDsa,
    Rs256,
    Rs384,
    Rs512,
}

impl Alg {
    fn as_str(self) -> &'static str {
        match self {
            Alg::Hs256 => "HS256",
            Alg::Hs384 => "HS384",
            Alg::Hs512 => "HS512",
            Alg::Es256 => "ES256",
            Alg::EdDsa => "EdDSA",
            Alg::Rs256 => "RS256",
            Alg::Rs384 => "RS384",
            Alg::Rs512 => "RS512",
        }
    }

    /// Mirrors `gossamer_std::jwt::Alg::from_str`. The RS family is
    /// verify-only on every tier: a service accepts a provider's token
    /// with it and mints none of its own.
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "HS256" => Ok(Alg::Hs256),
            "HS384" => Ok(Alg::Hs384),
            "HS512" => Ok(Alg::Hs512),
            "ES256" => Ok(Alg::Es256),
            "EdDSA" => Ok(Alg::EdDsa),
            "RS256" => Ok(Alg::Rs256),
            "RS384" => Ok(Alg::Rs384),
            "RS512" => Ok(Alg::Rs512),
            "none" => {
                Err("jwt: alg \"none\" is refused on principle (RFC 7515 §4.1.1)".to_string())
            }
            other => Err(format!("jwt: unsupported alg {other:?}")),
        }
    }

    fn is_hmac(self) -> bool {
        matches!(self, Alg::Hs256 | Alg::Hs384 | Alg::Hs512)
    }

    fn is_rsa(self) -> bool {
        matches!(self, Alg::Rs256 | Alg::Rs384 | Alg::Rs512)
    }
}

// -- claims canonicalization (mirrors Claims::from_json + to_json) ----------

fn parse_aud(v: &serde_json::Value) -> Result<Vec<String>, String> {
    match v {
        serde_json::Value::String(s) => Ok(vec![s.clone()]),
        serde_json::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for entry in arr {
                let s = entry
                    .as_str()
                    .ok_or_else(|| "jwt: aud array contains non-string".to_string())?;
                out.push(s.to_string());
            }
            Ok(out)
        }
        _ => Err("jwt: aud must be a string or array of strings".to_string()),
    }
}

fn numeric_date(v: &serde_json::Value, name: &str) -> Result<i64, String> {
    if let Some(n) = v.as_i64() {
        return Ok(n);
    }
    if let Some(f) = v.as_f64() {
        if f.is_finite() {
            return Ok(f as i64);
        }
    }
    Err(format!("jwt: {name} is not a numeric date"))
}

/// Folds a raw claims JSON string into the canonical claims object,
/// exactly as `Claims::from_json` then `Claims::to_json` would.
fn normalize_claims(raw: &str) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("jwt: claims json: {e}"))?;
    let Value::Object(obj) = v else {
        return Err("jwt: claims payload is not a JSON object".to_string());
    };
    let mut out = serde_json::Map::new();
    for (k, val) in obj {
        match k.as_str() {
            "iss" => {
                let s = val
                    .as_str()
                    .ok_or_else(|| "jwt: iss is not a string".to_string())?;
                out.insert(k, Value::String(s.to_string()));
            }
            "sub" => {
                let s = val
                    .as_str()
                    .ok_or_else(|| "jwt: sub is not a string".to_string())?;
                out.insert(k, Value::String(s.to_string()));
            }
            "jti" => {
                let s = val
                    .as_str()
                    .ok_or_else(|| "jwt: jti is not a string".to_string())?;
                out.insert(k, Value::String(s.to_string()));
            }
            "aud" => {
                let auds = parse_aud(&val)?;
                // RFC 7519 §4.1.3: a single audience MAY be a bare string.
                if auds.len() == 1 {
                    out.insert(k, Value::String(auds[0].clone()));
                } else {
                    out.insert(
                        k,
                        Value::Array(auds.into_iter().map(Value::String).collect()),
                    );
                }
            }
            "exp" => {
                out.insert(k, Value::from(numeric_date(&val, "exp")?));
            }
            "nbf" => {
                out.insert(k, Value::from(numeric_date(&val, "nbf")?));
            }
            "iat" => {
                out.insert(k, Value::from(numeric_date(&val, "iat")?));
            }
            _ => {
                out.insert(k, val);
            }
        }
    }
    Ok(Value::Object(out))
}

fn header_json(alg: Alg) -> Vec<u8> {
    let mut obj = serde_json::Map::new();
    obj.insert("alg".to_string(), alg.as_str().into());
    obj.insert("typ".to_string(), "JWT".into());
    // Infallible: a small Map of string values always serializes.
    serde_json::to_vec(&serde_json::Value::Object(obj)).unwrap_or_default()
}

fn build_signing_input(alg: Alg, claims_json: &str) -> Result<String, String> {
    let header = header_json(alg);
    let claims = normalize_claims(claims_json)?;
    let claims_bytes =
        serde_json::to_vec(&claims).map_err(|e| format!("jwt: encode claims: {e}"))?;
    Ok(format!(
        "{}.{}",
        b64url_encode(&header),
        b64url_encode(&claims_bytes)
    ))
}

// -- HMAC (RFC 2104) -------------------------------------------------------

const SHA256_BLOCK: usize = 64;
const SHA512_BLOCK: usize = 128;

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut block = [0u8; SHA256_BLOCK];
    if key.len() > SHA256_BLOCK {
        let d: [u8; 32] = sha2::Sha256::digest(key).into();
        block[..32].copy_from_slice(&d);
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut ikey = [0u8; SHA256_BLOCK];
    let mut okey = [0u8; SHA256_BLOCK];
    for i in 0..SHA256_BLOCK {
        ikey[i] = block[i] ^ 0x36;
        okey[i] = block[i] ^ 0x5c;
    }
    let mut inner = sha2::Sha256::new();
    inner.update(ikey);
    inner.update(message);
    let ih = inner.finalize();
    let mut outer = sha2::Sha256::new();
    outer.update(okey);
    outer.update(ih);
    outer.finalize().to_vec()
}

fn hmac_sha384(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut block = [0u8; SHA512_BLOCK];
    if key.len() > SHA512_BLOCK {
        let d: [u8; 48] = sha2::Sha384::digest(key).into();
        block[..48].copy_from_slice(&d);
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut ikey = [0u8; SHA512_BLOCK];
    let mut okey = [0u8; SHA512_BLOCK];
    for i in 0..SHA512_BLOCK {
        ikey[i] = block[i] ^ 0x36;
        okey[i] = block[i] ^ 0x5c;
    }
    let mut inner = sha2::Sha384::new();
    inner.update(ikey);
    inner.update(message);
    let ih = inner.finalize();
    let mut outer = sha2::Sha384::new();
    outer.update(okey);
    outer.update(ih);
    outer.finalize().to_vec()
}

fn hmac_sha512(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut block = [0u8; SHA512_BLOCK];
    if key.len() > SHA512_BLOCK {
        let d: [u8; 64] = sha2::Sha512::digest(key).into();
        block[..64].copy_from_slice(&d);
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut ikey = [0u8; SHA512_BLOCK];
    let mut okey = [0u8; SHA512_BLOCK];
    for i in 0..SHA512_BLOCK {
        ikey[i] = block[i] ^ 0x36;
        okey[i] = block[i] ^ 0x5c;
    }
    let mut inner = sha2::Sha512::new();
    inner.update(ikey);
    inner.update(message);
    let ih = inner.finalize();
    let mut outer = sha2::Sha512::new();
    outer.update(okey);
    outer.update(ih);
    outer.finalize().to_vec()
}

fn hmac_sign(alg: Alg, key: &[u8], message: &[u8]) -> Vec<u8> {
    match alg {
        Alg::Hs256 => hmac_sha256(key, message),
        Alg::Hs384 => hmac_sha384(key, message),
        Alg::Hs512 => hmac_sha512(key, message),
        _ => Vec::new(),
    }
}

/// Constant-time byte-slice comparison. Mirrors
/// `gossamer_std::crypto::subtle::constant_time_eq`.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// -- token splitting + header / claim validation ---------------------------

struct Decoded {
    alg: Alg,
    signing_input: String,
    claims: serde_json::Value,
    sig: Vec<u8>,
}

fn decode_token(token: &str) -> Result<Decoded, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(format!("jwt: expected 3 segments, got {}", parts.len()));
    }
    let header_b = b64url_decode(parts[0])?;
    let payload_b = b64url_decode(parts[1])?;
    let sig = b64url_decode(parts[2])?;

    let header_v: serde_json::Value =
        serde_json::from_slice(&header_b).map_err(|e| format!("jwt: header json: {e}"))?;
    let obj = header_v
        .as_object()
        .ok_or_else(|| "jwt: header is not a JSON object".to_string())?;
    if obj.contains_key("crit") {
        return Err(
            "jwt: header carries crit; refusing (RFC 7515 §4.1.11 - unknown critical extensions)"
                .to_string(),
        );
    }
    let alg_str = obj
        .get("alg")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "jwt: header missing alg".to_string())?;
    let alg = Alg::parse(alg_str)?;
    if let Some(t) = obj.get("typ") {
        let s = t
            .as_str()
            .ok_or_else(|| "jwt: header typ is not a string".to_string())?;
        if !s.eq_ignore_ascii_case("JWT") {
            return Err(format!(
                "jwt: unsupported header typ {s:?} (expected \"JWT\")"
            ));
        }
    }

    let payload_str = String::from_utf8_lossy(&payload_b).into_owned();
    let claims = normalize_claims(&payload_str)?;
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    Ok(Decoded {
        alg,
        signing_input,
        claims,
        sig,
    })
}

fn now_unix_secs() -> i64 {
    crate::platform::system_time_now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Mirrors `validate_claims` with `VerifyOpts::new().leeway(leeway)` -
/// only `exp` / `nbf` are checked (required iss/aud/sub are left to
/// the caller, who inspects the returned claims JSON).
fn validate_claims(claims: &serde_json::Value, leeway: i64) -> Result<(), String> {
    let now = now_unix_secs();
    if let Some(exp) = claims.get("exp").and_then(serde_json::Value::as_i64) {
        if now > exp.saturating_add(leeway) {
            return Err("jwt: token expired".to_string());
        }
    }
    if let Some(nbf) = claims.get("nbf").and_then(serde_json::Value::as_i64) {
        if now.saturating_add(leeway) < nbf {
            return Err("jwt: token not yet valid (nbf)".to_string());
        }
    }
    Ok(())
}

fn claims_string(claims: &serde_json::Value) -> Result<String, String> {
    serde_json::to_string(claims).map_err(|e| format!("jwt: encode claims: {e}"))
}

/// Parses an RSA public key from PEM. Accepts both SPKI
/// (`BEGIN PUBLIC KEY`, the JOSE / OIDC convention) and bare
/// PKCS#1 (`BEGIN RSA PUBLIC KEY`). Returns `(modulus_be, exponent_be)`
/// with leading zero bytes stripped - the shape ring expects.
fn parse_rsa_public_key_pem(pem: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    use std::io::Cursor;
    use x509_parser::pem::Pem;
    use x509_parser::prelude::FromDer;
    use x509_parser::x509::SubjectPublicKeyInfo;

    let bytes = pem.as_bytes();
    let (parsed, _read) =
        Pem::read(Cursor::new(bytes)).map_err(|e| format!("jwt: RSA public pem: {e}"))?;

    let (n_bytes, e_bytes): (Vec<u8>, Vec<u8>) = match parsed.label.as_str() {
        "PUBLIC KEY" => {
            // SPKI: AlgorithmIdentifier + BIT STRING(RSAPublicKey).
            let (_, spki) = SubjectPublicKeyInfo::from_der(&parsed.contents)
                .map_err(|e| format!("jwt: RSA SPKI parse: {e}"))?;
            let pk = spki
                .parsed()
                .map_err(|e| format!("jwt: RSA SPKI parsed: {e}"))?;
            match pk {
                x509_parser::public_key::PublicKey::RSA(rsa) => {
                    (rsa.modulus.to_vec(), rsa.exponent.to_vec())
                }
                _ => {
                    return Err(String::from(
                        "jwt: RSA verifier got non-RSA key inside PUBLIC KEY pem",
                    ));
                }
            }
        }
        "RSA PUBLIC KEY" => {
            // Bare PKCS#1: SEQUENCE { modulus, exponent }.
            let (_, rsa) = x509_parser::public_key::RSAPublicKey::from_der(&parsed.contents)
                .map_err(|e| format!("jwt: RSA PKCS#1 parse: {e}"))?;
            (rsa.modulus.to_vec(), rsa.exponent.to_vec())
        }
        other => {
            return Err(format!(
                "jwt: RSA verifier got unsupported pem label {other:?} \
                 (expected PUBLIC KEY or RSA PUBLIC KEY)"
            ));
        }
    };

    Ok((strip_leading_zeros(&n_bytes), strip_leading_zeros(&e_bytes)))
}

/// Strips leading 0x00 bytes from a big-endian integer encoding -
/// ASN.1 INTEGER prepends a zero byte to keep the high bit clear,
/// but ring wants the unpadded magnitude.
fn strip_leading_zeros(bytes: &[u8]) -> Vec<u8> {
    let first_nonzero = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    bytes[first_nonzero..].to_vec()
}

/// Encodes an `RSAPublicKey` (PKCS#1) DER SEQUENCE from raw
/// big-endian n and e bytes. The output is what ring's
/// `RSA_PKCS1_*` verification algorithms accept as the public key.
fn encode_rsa_public_key_der(n: &[u8], e: &[u8]) -> Vec<u8> {
    let n_int = der_encode_integer(n);
    let e_int = der_encode_integer(e);
    let mut body = Vec::with_capacity(n_int.len() + e_int.len());
    body.extend_from_slice(&n_int);
    body.extend_from_slice(&e_int);
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(0x30); // SEQUENCE
    der_encode_length(&mut out, body.len());
    out.extend_from_slice(&body);
    out
}

/// Encodes a single DER INTEGER from a big-endian magnitude. A
/// leading 0x00 is prepended when the high bit is set so the
/// integer reads as non-negative.
fn der_encode_integer(magnitude: &[u8]) -> Vec<u8> {
    let needs_sign_pad = magnitude.first().is_some_and(|b| b & 0x80 != 0);
    let content_len = magnitude.len() + usize::from(needs_sign_pad);
    let mut out = Vec::with_capacity(content_len + 4);
    out.push(0x02); // INTEGER
    der_encode_length(&mut out, content_len);
    if needs_sign_pad {
        out.push(0x00);
    }
    out.extend_from_slice(magnitude);
    out
}

/// Appends an ASN.1 DER definite-length encoding. Short-form for
/// lengths < 128, long-form otherwise.
fn der_encode_length(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
        return;
    }
    let mut tmp = [0u8; 8];
    let mut n = len;
    let mut i = tmp.len();
    while n > 0 {
        i -= 1;
        tmp[i] = (n & 0xff) as u8;
        n >>= 8;
    }
    let count = tmp.len() - i;
    out.push(0x80 | count as u8);
    out.extend_from_slice(&tmp[i..]);
}

/// Verifies an ES256-signed token's signature over `signing_input`.
fn es256_verify(pem: &str, signing_input: &str, sig: &[u8]) -> Result<(), String> {
    use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
    use p256::pkcs8::DecodePublicKey;

    if sig.len() != 64 {
        return Err(format!(
            "jwt: ES256 signature must be 64 bytes (r||s), got {}",
            sig.len()
        ));
    }
    let key = VerifyingKey::from_public_key_pem(pem)
        .map_err(|e| format!("jwt: ES256 public pem: {e}"))?;
    let signature = Signature::from_slice(sig).map_err(|e| format!("jwt: ES256 signature: {e}"))?;
    key.verify(signing_input.as_bytes(), &signature)
        .map_err(|_| "jwt: signature invalid".to_string())
}

/// Verifies an EdDSA-signed token's signature over `signing_input`.
fn eddsa_verify(pem: &str, signing_input: &str, sig: &[u8]) -> Result<(), String> {
    use ed25519_dalek::pkcs8::DecodePublicKey;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let sig_arr: [u8; 64] = sig
        .try_into()
        .map_err(|_| format!("jwt: EdDSA signature must be 64 bytes, got {}", sig.len()))?;
    let key = VerifyingKey::from_public_key_pem(pem)
        .map_err(|e| format!("jwt: EdDSA public pem: {e}"))?;
    key.verify(signing_input.as_bytes(), &Signature::from_bytes(&sig_arr))
        .map_err(|_| "jwt: signature invalid".to_string())
}

/// Verifies an RSA-signed token's signature over `signing_input`.
fn rsa_verify(alg: Alg, pem: &str, signing_input: &str, sig: &[u8]) -> Result<(), String> {
    let params: &'static dyn ring::signature::VerificationAlgorithm = match alg {
        Alg::Rs256 => &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        Alg::Rs384 => &ring::signature::RSA_PKCS1_2048_8192_SHA384,
        Alg::Rs512 => &ring::signature::RSA_PKCS1_2048_8192_SHA512,
        _ => return Err("jwt: internal RSA alg dispatch error".to_string()),
    };
    let (n, e) = parse_rsa_public_key_pem(pem)?;
    // Ring wants the key as DER-encoded PKCS#1 `RSAPublicKey`
    // (SEQUENCE { modulus INTEGER, exponent INTEGER }); x509-parser
    // hands back the raw n/e, so they are re-encoded as that SEQUENCE.
    let der = encode_rsa_public_key_der(&n, &e);
    ring::signature::UnparsedPublicKey::new(params, der)
        .verify(signing_input.as_bytes(), sig)
        .map_err(|_| "jwt: signature invalid".to_string())
}

/// Enforces the registered claims a caller demanded.
///
/// A verifier that checks only the signature accepts a token minted for
/// another service by whoever shares the key, and a token from any issuer
/// that signed with it. `issuer` and `audience` are enforced when
/// non-empty; leaving them empty is the caller's decision to accept both.
fn enforce_claims(
    claims: &serde_json::Value,
    leeway: i64,
    issuer: &str,
    audience: &str,
) -> Result<(), String> {
    validate_claims(claims, leeway)?;
    if !issuer.is_empty() && claims.get("iss").and_then(serde_json::Value::as_str) != Some(issuer) {
        return Err(format!("jwt: iss mismatch (expected {issuer:?})"));
    }
    if !audience.is_empty() {
        let present = match claims.get("aud") {
            Some(serde_json::Value::String(s)) => s == audience,
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s == audience)),
            _ => false,
        };
        if !present {
            return Err(format!(
                "jwt: aud does not contain required value {audience:?}"
            ));
        }
    }
    Ok(())
}

/// `jwt::verify(token, alg, key, leeway, issuer, audience)
/// -> Result<String, errors::Error>` - the verifier a service protecting
/// an endpoint uses. `key` is the shared secret for the HS family and the
/// PEM-encoded public key for every other algorithm.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_jwt_verify(
    token: *const c_char,
    alg: *const c_char,
    key: *const c_char,
    leeway_secs: i64,
    issuer: *const c_char,
    audience: *const c_char,
) -> i128 {
    ffi_entry!({
        // SAFETY: `alg` is this shim's argument, null or a live string body (C-ABI contract),
        // which `cstr` accepts.
        let expected = match Alg::parse(unsafe { cstr(alg) }) {
            Ok(a) => a,
            Err(e) => return jwt_err(&e),
        };
        // SAFETY: `key` is this shim's argument, null or a live string body (C-ABI contract),
        // which `cstr` accepts.
        let key = unsafe { cstr(key) };
        // SAFETY: `issuer` is this shim's argument, null or a live string body (C-ABI contract),
        // which `cstr` accepts.
        let issuer = unsafe { cstr(issuer) };
        // SAFETY: `audience` is this shim's argument, null or a live string body (C-ABI
        // contract), which `cstr` accepts.
        let audience = unsafe { cstr(audience) };
        // SAFETY: `token` is this shim's argument, null or a live string body (C-ABI contract),
        // which `cstr` accepts.
        let dec = match decode_token(unsafe { cstr(token) }) {
            Ok(d) => d,
            Err(e) => return jwt_err(&e),
        };
        // The token's own `alg` never selects the verifier: a caller that
        // let it would accept an HMAC token signed with a public key it
        // published (RFC 8725 2.1).
        if !expected.is_hmac() && dec.alg.is_hmac() {
            return jwt_err(&format!(
                "jwt: {} verifier refusing HMAC token alg {}",
                if expected.is_rsa() {
                    "RSA"
                } else {
                    expected.as_str()
                },
                dec.alg.as_str()
            ));
        }
        if dec.alg != expected {
            return jwt_err(&format!(
                "jwt: alg mismatch - token says {} but verifier expected {}",
                dec.alg.as_str(),
                expected.as_str()
            ));
        }
        let verified: Result<(), String> = if expected.is_hmac() {
            let want = hmac_sign(expected, key.as_bytes(), dec.signing_input.as_bytes());
            if ct_eq(&want, &dec.sig) {
                Ok(())
            } else {
                Err("jwt: signature invalid".to_string())
            }
        } else if expected.is_rsa() {
            rsa_verify(expected, key, &dec.signing_input, &dec.sig)
        } else if expected == Alg::Es256 {
            es256_verify(key, &dec.signing_input, &dec.sig)
        } else {
            eddsa_verify(key, &dec.signing_input, &dec.sig)
        };
        if let Err(e) = verified {
            return jwt_err(&e);
        }
        if let Err(e) = enforce_claims(&dec.claims, leeway_secs, issuer, audience) {
            return jwt_err(&e);
        }
        match claims_string(&dec.claims) {
            Ok(s) => ok_string(&s),
            Err(e) => jwt_err(&e),
        }
    })
}

/// `jwt::header(token) -> Result<String, errors::Error>` - the token's
/// JOSE header as JSON, read WITHOUT verifying the signature.
///
/// A service holding a key set reads `kid` and `alg` to choose which key
/// to verify with. Nothing in the header is authenticated until that
/// verification succeeds, which is why `verify` takes the expected
/// algorithm as its own argument rather than reading it here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_jwt_header(token: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `token` is this shim's argument, null or a live string body (C-ABI contract),
        // which `cstr` accepts.
        let token = unsafe { cstr(token) };
        let Some(header_b64) = token.split('.').next() else {
            return jwt_err("jwt: token has no header segment");
        };
        let raw = match b64url_decode(header_b64) {
            Ok(r) => r,
            Err(e) => return jwt_err(&e),
        };
        let value: serde_json::Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(e) => return jwt_err(&format!("jwt: header is not JSON: {e}")),
        };
        match serde_json::to_string(&value) {
            Ok(s) => ok_string(&s),
            Err(e) => jwt_err(&format!("jwt: encode header: {e}")),
        }
    })
}

// -- HMAC entry points -----------------------------------------------------

/// `jwt::sign_hs(alg, claims_json, key) -> Result<String, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_jwt_sign_hs(
    alg: *const c_char,
    claims_json: *const c_char,
    key: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `alg` is this shim's argument, null or a live string body (C-ABI contract),
        // which `cstr` accepts.
        let alg = match Alg::parse(unsafe { cstr(alg) }) {
            Ok(a) => a,
            Err(e) => return jwt_err(&e),
        };
        if !alg.is_hmac() {
            return jwt_err(&format!(
                "jwt: sign_hs called with non-HMAC alg {}",
                alg.as_str()
            ));
        }
        // SAFETY: `claims_json` is this shim's argument, null or a live string body (C-ABI
        // contract), which `cstr` accepts.
        let claims_json = unsafe { cstr(claims_json) };
        // SAFETY: `key` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let key = unsafe { crate::c_abi::vec::vec_bytes(key) };
        let signing_input = match build_signing_input(alg, claims_json) {
            Ok(s) => s,
            Err(e) => return jwt_err(&e),
        };
        let sig = hmac_sign(alg, &key, signing_input.as_bytes());
        ok_string(&format!("{signing_input}.{}", b64url_encode(&sig)))
    })
}

/// `jwt::verify_hs(token, alg, key, leeway_secs)
/// -> Result<String, errors::Error>` - returns the canonical claims
/// JSON on success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_jwt_verify_hs(
    token: *const c_char,
    alg: *const c_char,
    key: *const GosVec,
    leeway_secs: i64,
) -> i128 {
    ffi_entry!({
        // SAFETY: `alg` is this shim's argument, null or a live string body (C-ABI contract),
        // which `cstr` accepts.
        let expected = match Alg::parse(unsafe { cstr(alg) }) {
            Ok(a) => a,
            Err(e) => return jwt_err(&e),
        };
        if !expected.is_hmac() {
            return jwt_err(&format!(
                "jwt: verify_hs called with non-HMAC alg {}",
                expected.as_str()
            ));
        }
        // SAFETY: `key` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let key = unsafe { crate::c_abi::vec::vec_bytes(key) };
        // SAFETY: `token` is this shim's argument, null or a live string body (C-ABI contract),
        // which `cstr` accepts.
        let dec = match decode_token(unsafe { cstr(token) }) {
            Ok(d) => d,
            Err(e) => return jwt_err(&e),
        };
        if !dec.alg.is_hmac() {
            return jwt_err(&format!(
                "jwt: HMAC verifier refusing asymmetric token alg {}",
                dec.alg.as_str()
            ));
        }
        if dec.alg != expected {
            return jwt_err(&format!(
                "jwt: alg mismatch - token says {} but verifier expected {}",
                dec.alg.as_str(),
                expected.as_str()
            ));
        }
        let want = hmac_sign(expected, &key, dec.signing_input.as_bytes());
        if !ct_eq(&want, &dec.sig) {
            return jwt_err("jwt: signature invalid");
        }
        if let Err(e) = validate_claims(&dec.claims, leeway_secs) {
            return jwt_err(&e);
        }
        match claims_string(&dec.claims) {
            Ok(s) => ok_string(&s),
            Err(e) => jwt_err(&e),
        }
    })
}

// -- ES256 (ECDSA P-256 + SHA-256) -----------------------------------------

/// `jwt::sign_es256(claims_json, signing_key_pem)
/// -> Result<String, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_jwt_sign_es256(
    claims_json: *const c_char,
    signing_key_pem: *const c_char,
) -> i128 {
    ffi_entry!({
        use p256::ecdsa::{Signature, SigningKey, signature::Signer};
        use p256::pkcs8::DecodePrivateKey;

        // SAFETY: `claims_json` is this shim's argument, null or a live string body (C-ABI
        // contract), which `cstr` accepts.
        let signing_input = match build_signing_input(Alg::Es256, unsafe { cstr(claims_json) }) {
            Ok(s) => s,
            Err(e) => return jwt_err(&e),
        };
        // SAFETY: `signing_key_pem` is this shim's argument, null or a live string body (C-ABI
        // contract), which `cstr` accepts.
        let signing = match SigningKey::from_pkcs8_pem(unsafe { cstr(signing_key_pem) }) {
            Ok(k) => k,
            Err(e) => return jwt_err(&format!("jwt: ES256 secret pem: {e}")),
        };
        let sig: Signature = signing.sign(signing_input.as_bytes());
        // RFC 7518 §3.4: raw r||s (64 bytes), not the ASN.1 DER envelope.
        let raw = sig.to_bytes();
        ok_string(&format!("{signing_input}.{}", b64url_encode(&raw[..])))
    })
}

/// `jwt::verify_es256(token, verifying_key_pem, leeway_secs)
/// -> Result<String, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_jwt_verify_es256(
    token: *const c_char,
    verifying_key_pem: *const c_char,
    leeway_secs: i64,
) -> i128 {
    ffi_entry!({
        use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
        use p256::pkcs8::DecodePublicKey;

        // SAFETY: `token` is this shim's argument, null or a live string body (C-ABI contract),
        // which `cstr` accepts.
        let dec = match decode_token(unsafe { cstr(token) }) {
            Ok(d) => d,
            Err(e) => return jwt_err(&e),
        };
        if dec.alg.is_hmac() {
            return jwt_err(&format!(
                "jwt: ES256 verifier refusing HMAC token alg {}",
                dec.alg.as_str()
            ));
        }
        if dec.alg != Alg::Es256 {
            return jwt_err(&format!(
                "jwt: alg mismatch - token says {} but verifier expected ES256",
                dec.alg.as_str()
            ));
        }
        if dec.sig.len() != 64 {
            return jwt_err(&format!(
                "jwt: ES256 signature must be 64 bytes (r||s), got {}",
                dec.sig.len()
            ));
        }
        // SAFETY: `verifying_key_pem` is this shim's argument, null or a live string body (C-ABI
        // contract), which `cstr` accepts.
        let key = match VerifyingKey::from_public_key_pem(unsafe { cstr(verifying_key_pem) }) {
            Ok(k) => k,
            Err(e) => return jwt_err(&format!("jwt: ES256 public pem: {e}")),
        };
        let signature = match Signature::from_slice(&dec.sig) {
            Ok(s) => s,
            Err(e) => return jwt_err(&format!("jwt: ES256 signature: {e}")),
        };
        if key
            .verify(dec.signing_input.as_bytes(), &signature)
            .is_err()
        {
            return jwt_err("jwt: signature invalid");
        }
        if let Err(e) = validate_claims(&dec.claims, leeway_secs) {
            return jwt_err(&e);
        }
        match claims_string(&dec.claims) {
            Ok(s) => ok_string(&s),
            Err(e) => jwt_err(&e),
        }
    })
}

// -- EdDSA (Ed25519) -------------------------------------------------------

/// `jwt::sign_eddsa(claims_json, signing_key_pem)
/// -> Result<String, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_jwt_sign_eddsa(
    claims_json: *const c_char,
    signing_key_pem: *const c_char,
) -> i128 {
    ffi_entry!({
        use ed25519_dalek::pkcs8::DecodePrivateKey;
        use ed25519_dalek::{Signer, SigningKey};

        // SAFETY: `claims_json` is this shim's argument, null or a live string body (C-ABI
        // contract), which `cstr` accepts.
        let signing_input = match build_signing_input(Alg::EdDsa, unsafe { cstr(claims_json) }) {
            Ok(s) => s,
            Err(e) => return jwt_err(&e),
        };
        // SAFETY: `signing_key_pem` is this shim's argument, null or a live string body (C-ABI
        // contract), which `cstr` accepts.
        let signing = match SigningKey::from_pkcs8_pem(unsafe { cstr(signing_key_pem) }) {
            Ok(k) => k,
            Err(e) => return jwt_err(&format!("jwt: EdDSA secret pem: {e}")),
        };
        let sig = signing.sign(signing_input.as_bytes());
        ok_string(&format!(
            "{signing_input}.{}",
            b64url_encode(&sig.to_bytes())
        ))
    })
}

/// `jwt::verify_eddsa(token, verifying_key_pem, leeway_secs)
/// -> Result<String, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_jwt_verify_eddsa(
    token: *const c_char,
    verifying_key_pem: *const c_char,
    leeway_secs: i64,
) -> i128 {
    ffi_entry!({
        use ed25519_dalek::pkcs8::DecodePublicKey;
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};

        // SAFETY: `token` is this shim's argument, null or a live string body (C-ABI contract),
        // which `cstr` accepts.
        let dec = match decode_token(unsafe { cstr(token) }) {
            Ok(d) => d,
            Err(e) => return jwt_err(&e),
        };
        if dec.alg.is_hmac() {
            return jwt_err(&format!(
                "jwt: EdDSA verifier refusing HMAC token alg {}",
                dec.alg.as_str()
            ));
        }
        if dec.alg != Alg::EdDsa {
            return jwt_err(&format!(
                "jwt: alg mismatch - token says {} but verifier expected EdDSA",
                dec.alg.as_str()
            ));
        }
        if dec.sig.len() != 64 {
            return jwt_err(&format!(
                "jwt: EdDSA signature must be 64 bytes, got {}",
                dec.sig.len()
            ));
        }
        // SAFETY: `verifying_key_pem` is this shim's argument, null or a live string body (C-ABI
        // contract), which `cstr` accepts.
        let key = match VerifyingKey::from_public_key_pem(unsafe { cstr(verifying_key_pem) }) {
            Ok(k) => k,
            Err(e) => return jwt_err(&format!("jwt: EdDSA public pem: {e}")),
        };
        let sig_arr: [u8; 64] = match dec.sig.as_slice().try_into() {
            Ok(a) => a,
            Err(_) => return jwt_err("jwt: EdDSA signature: bad length"),
        };
        if key
            .verify(
                dec.signing_input.as_bytes(),
                &Signature::from_bytes(&sig_arr),
            )
            .is_err()
        {
            return jwt_err("jwt: signature invalid");
        }
        if let Err(e) = validate_claims(&dec.claims, leeway_secs) {
            return jwt_err(&e);
        }
        match claims_string(&dec.claims) {
            Ok(s) => ok_string(&s),
            Err(e) => jwt_err(&e),
        }
    })
}

// -- base64url (RFC 4648 §5, no padding) -----------------------------------

const B64URL_ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n =
            (u32::from(input[i]) << 16) | (u32::from(input[i + 1]) << 8) | u32::from(input[i + 2]);
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let remaining = input.len() - i;
    if remaining == 1 {
        let n = u32::from(input[i]) << 16;
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
    } else if remaining == 2 {
        let n = (u32::from(input[i]) << 16) | (u32::from(input[i + 1]) << 8);
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 6) & 0x3f) as usize] as char);
    }
    out
}

fn b64url_decode(input: &str) -> Result<Vec<u8>, String> {
    let trimmed = input.trim_end_matches('=');
    let bytes = trimmed.as_bytes();
    let n = bytes.len();
    let out_len = match n % 4 {
        0 => n / 4 * 3,
        2 => n / 4 * 3 + 1,
        3 => n / 4 * 3 + 2,
        _ => return Err("jwt: base64url: invalid length".to_string()),
    };
    let mut out = Vec::with_capacity(out_len);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        let v = decode_b64url_char(b)?;
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if bits > 0 && (acc & ((1u32 << bits) - 1)) != 0 {
        return Err("jwt: base64url: non-zero padding bits".to_string());
    }
    Ok(out)
}

fn decode_b64url_char(c: u8) -> Result<u8, String> {
    match c {
        b'A'..=b'Z' => Ok(c - b'A'),
        b'a'..=b'z' => Ok(c - b'a' + 26),
        b'0'..=b'9' => Ok(c - b'0' + 52),
        b'-' => Ok(62),
        b'_' => Ok(63),
        _ => Err(format!("jwt: base64url: invalid character 0x{c:02x}")),
    }
}
