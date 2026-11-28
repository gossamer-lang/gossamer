//! Cryptography for talking to other systems: SHA-3 digests, HKDF key
//! derivation, RSA signatures (PKCS#1 v1.5 and PSS), and X.509 certificate
//! and signing-request generation. The bytecode VM and compiled code both
//! call these, so every tier answers the same bytes.

use sha3::Digest;

/// The SHA3-256 digest of `data`.
#[must_use]
pub fn sha3_256(data: &[u8]) -> Vec<u8> {
    sha3::Sha3_256::digest(data).to_vec()
}

/// The SHA3-512 digest of `data`.
#[must_use]
pub fn sha3_512(data: &[u8]) -> Vec<u8> {
    sha3::Sha3_512::digest(data).to_vec()
}

/// Which hash an HKDF derivation runs over.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HkdfHash {
    /// HKDF-SHA256.
    Sha256,
    /// HKDF-SHA384.
    Sha384,
    /// HKDF-SHA512.
    Sha512,
}

/// `length` bytes of key material derived from `ikm` with `salt` and `info`
/// (RFC 5869).
///
/// # Errors
/// `length` is negative or longer than 255 times the hash's output.
#[cfg(not(target_arch = "wasm32"))]
pub fn hkdf(
    hash: HkdfHash,
    ikm: &[u8],
    salt: &[u8],
    info: &[u8],
    length: i64,
) -> Result<Vec<u8>, String> {
    struct Len(usize);
    impl ring::hkdf::KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }
    let algorithm = match hash {
        HkdfHash::Sha256 => ring::hkdf::HKDF_SHA256,
        HkdfHash::Sha384 => ring::hkdf::HKDF_SHA384,
        HkdfHash::Sha512 => ring::hkdf::HKDF_SHA512,
    };
    let length = usize::try_from(length).map_err(|_| "hkdf: length is negative".to_string())?;
    let limit = 255 * ring::hkdf::KeyType::len(&algorithm);
    if length > limit {
        return Err(format!(
            "hkdf: length {length} is past the limit of {limit} bytes"
        ));
    }
    let prk = ring::hkdf::Salt::new(algorithm, salt).extract(ikm);
    let info = [info];
    let okm = prk
        .expand(&info, Len(length))
        .map_err(|_| "hkdf: expansion failed".to_string())?;
    let mut out = vec![0u8; length];
    okm.fill(&mut out)
        .map_err(|_| "hkdf: expansion failed".to_string())?;
    Ok(out)
}

/// An RSA signature scheme over SHA-256.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsaScheme {
    /// RSASSA-PKCS1-v1_5 with SHA-256 (`RS256`).
    Pkcs1Sha256,
    /// RSASSA-PSS with SHA-256 and MGF1-SHA-256 (`PS256`).
    PssSha256,
}

/// The key pair a PKCS#8 (`PRIVATE KEY`) or PKCS#1 (`RSA PRIVATE KEY`) PEM
/// block holds.
#[cfg(not(target_arch = "wasm32"))]
fn rsa_key_pair(secret_pem: &str) -> Result<ring::rsa::KeyPair, String> {
    let (block, _) = crate::codec::pem::decode(secret_pem).map_err(|e| format!("rsa: {e}"))?;
    let parsed = match block.label.as_str() {
        "PRIVATE KEY" => ring::rsa::KeyPair::from_pkcs8(&block.bytes),
        "RSA PRIVATE KEY" => ring::rsa::KeyPair::from_der(&block.bytes),
        other => {
            return Err(format!(
                "rsa: a `{other}` PEM block is not an RSA private key"
            ));
        }
    };
    parsed.map_err(|e| format!("rsa: private key: {e}"))
}

/// The PKCS#1 `RSAPublicKey` a `PUBLIC KEY` (SPKI) or `RSA PUBLIC KEY` PEM
/// block holds.
#[cfg(not(target_arch = "wasm32"))]
fn rsa_public_key(public_pem: &str) -> Result<Vec<u8>, String> {
    use x509_parser::prelude::FromDer;
    let (block, _) = crate::codec::pem::decode(public_pem).map_err(|e| format!("rsa: {e}"))?;
    match block.label.as_str() {
        "RSA PUBLIC KEY" => Ok(block.bytes),
        "PUBLIC KEY" => {
            let (_, spki) = x509_parser::x509::SubjectPublicKeyInfo::from_der(&block.bytes)
                .map_err(|e| format!("rsa: public key: {e}"))?;
            if spki.algorithm.algorithm.to_id_string() != RSA_ENCRYPTION_OID {
                return Err("rsa: the public key is not an RSA key".to_string());
            }
            Ok(spki.subject_public_key.data.to_vec())
        }
        other => Err(format!(
            "rsa: a `{other}` PEM block is not an RSA public key"
        )),
    }
}

#[cfg(not(target_arch = "wasm32"))]
const RSA_ENCRYPTION_OID: &str = "1.2.840.113549.1.1.1";

/// The signature of `message` under the RSA key `secret_pem`.
///
/// # Errors
/// A PEM block that is not an RSA private key, or a key ring refuses.
#[cfg(not(target_arch = "wasm32"))]
pub fn rsa_sign(scheme: RsaScheme, secret_pem: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    let key = rsa_key_pair(secret_pem)?;
    let padding: &dyn ring::signature::RsaEncoding = match scheme {
        RsaScheme::Pkcs1Sha256 => &ring::signature::RSA_PKCS1_SHA256,
        RsaScheme::PssSha256 => &ring::signature::RSA_PSS_SHA256,
    };
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(
        padding,
        &ring::rand::SystemRandom::new(),
        message,
        &mut signature,
    )
    .map_err(|_| "rsa: signing failed".to_string())?;
    Ok(signature)
}

/// Whether `signature` is a valid signature of `message` under the RSA
/// public key `public_pem` (2048 to 8192 bits).
///
/// # Errors
/// A PEM block that is not an RSA public key, or a signature that does not
/// verify.
#[cfg(not(target_arch = "wasm32"))]
pub fn rsa_verify(
    scheme: RsaScheme,
    public_pem: &str,
    message: &[u8],
    signature: &[u8],
) -> Result<(), String> {
    let key = rsa_public_key(public_pem)?;
    let algorithm: &dyn ring::signature::VerificationAlgorithm = match scheme {
        RsaScheme::Pkcs1Sha256 => &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        RsaScheme::PssSha256 => &ring::signature::RSA_PSS_2048_8192_SHA256,
    };
    ring::signature::UnparsedPublicKey::new(algorithm, key)
        .verify(message, signature)
        .map_err(|_| "rsa: signature does not verify".to_string())
}

/// `plaintext` encrypted with RSAES-OAEP under the RSA public key
/// `public_pem`, with SHA-256 for the hash and for MGF1 and an empty label.
/// The plaintext may be at most the modulus length less 66 bytes.
///
/// # Errors
/// A PEM block that is not an RSA public key, or a plaintext too long for it.
#[cfg(not(target_arch = "wasm32"))]
pub fn rsa_encrypt_oaep_sha256(public_pem: &str, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    use rsa::pkcs1::DecodeRsaPublicKey;
    let key = rsa::PublicKey::from_pkcs1_der(&rsa_public_key(public_pem)?)
        .map_err(|e| format!("rsa: public key: {e}"))?;
    key.oaep_encrypt::<rsa::sha2::Sha256>(plaintext)
        .map_err(|e| format!("rsa: encryption: {e}"))
}

/// The plaintext of the RSAES-OAEP (SHA-256, MGF1-SHA-256, empty label)
/// ciphertext `ciphertext` under the RSA private key `secret_pem`. The
/// decryption is blinded, so its timing does not depend on the key.
///
/// # Errors
/// A PEM block that is not an RSA private key, or a ciphertext that does not
/// decrypt under it.
#[cfg(not(target_arch = "wasm32"))]
pub fn rsa_decrypt_oaep_sha256(secret_pem: &str, ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;
    let (block, _) = crate::codec::pem::decode(secret_pem).map_err(|e| format!("rsa: {e}"))?;
    let key = match block.label.as_str() {
        "PRIVATE KEY" => rsa::PrivateKey::from_pkcs8_der(&block.bytes)
            .map_err(|e| format!("rsa: private key: {e}"))?,
        "RSA PRIVATE KEY" => rsa::PrivateKey::from_pkcs1_der(&block.bytes)
            .map_err(|e| format!("rsa: private key: {e}"))?,
        other => {
            return Err(format!(
                "rsa: a `{other}` PEM block is not an RSA private key"
            ));
        }
    };
    key.oaep_decrypt::<rsa::sha2::Sha256>(ciphertext)
        .map_err(|_| "rsa: the ciphertext does not decrypt under this key".to_string())
}

/// The `PUBLIC KEY` (SPKI) PEM of the RSA private key `secret_pem`.
///
/// # Errors
/// A PEM block that is not an RSA private key.
#[cfg(not(target_arch = "wasm32"))]
pub fn rsa_public_key_pem(secret_pem: &str) -> Result<String, String> {
    use ring::signature::KeyPair;
    // SubjectPublicKeyInfo: the rsaEncryption algorithm with NULL parameters,
    // then the PKCS#1 key as a bit string with no unused bits.
    const ALGORITHM: [u8; 15] = [
        0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
    ];
    let key = rsa_key_pair(secret_pem)?;
    let pkcs1 = key.public_key().as_ref();
    let mut bit_string = vec![0u8];
    bit_string.extend_from_slice(pkcs1);
    let mut body = ALGORITHM.to_vec();
    body.extend(der_element(0x03, &bit_string));
    Ok(crate::codec::pem::encode(
        "PUBLIC KEY",
        &der_element(0x30, &body),
    ))
}

/// A DER element: `tag`, its definite length, and `content`.
#[cfg(not(target_arch = "wasm32"))]
fn der_element(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(u8::try_from(len).unwrap_or(0));
    } else {
        let bytes: Vec<u8> = len
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        out.push(0x80 | u8::try_from(bytes.len()).unwrap_or(0));
        out.extend(bytes);
    }
    out.extend_from_slice(content);
    out
}

/// What a generated certificate is for.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CertUse {
    /// The certificate may sign other certificates.
    pub is_ca: bool,
    /// The certificate authenticates a TLS client rather than a server.
    pub client: bool,
}

/// Certificate parameters for `common_name`, `dns_names`, a validity of
/// `days` from now, and `cert_use`.
#[cfg(not(target_arch = "wasm32"))]
fn cert_params(
    common_name: &str,
    dns_names: &[String],
    days: i64,
    cert_use: CertUse,
    extensions: &[rcgen::CustomExtension],
) -> Result<rcgen::CertificateParams, String> {
    use rcgen::{BasicConstraints, DnType, ExtendedKeyUsagePurpose, IsCa, KeyUsagePurpose};
    if days <= 0 {
        return Err("x509: a certificate's validity must be at least one day".to_string());
    }
    let mut params =
        rcgen::CertificateParams::new(dns_names.to_vec()).map_err(|e| format!("x509: {e}"))?;
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now
        .checked_add(time::Duration::days(days))
        .ok_or_else(|| "x509: validity runs past the representable dates".to_string())?;
    if cert_use.is_ca {
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
    } else {
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![if cert_use.client {
            ExtendedKeyUsagePurpose::ClientAuth
        } else {
            ExtendedKeyUsagePurpose::ServerAuth
        }];
    }
    params.custom_extensions = extensions.to_vec();
    Ok(params)
}

/// The custom extensions `json` describes, a JSON array of
/// `{"oid": "1.2.3", "critical": bool, "value": "<hex>"}`, each checked: the
/// OID dotted decimal with a first arc of 0 to 2 (and a second below 40 under
/// 0 or 1), the value exactly one well-formed DER element, and no OID twice.
///
/// # Errors
/// The JSON does not have that shape, or an extension fails a check.
#[cfg(not(target_arch = "wasm32"))]
pub fn parse_extensions(json: &str) -> Result<Vec<rcgen::CustomExtension>, String> {
    use der::Decode;
    let items: Vec<serde_json::Value> = if json.trim().is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(json).map_err(|e| format!("x509: extensions: {e}"))?
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let oid = item["oid"].as_str().unwrap_or_default();
        let arcs = oid_arcs(oid)?;
        if !seen.insert(arcs.clone()) {
            return Err(format!("x509: the extension {oid} appears twice"));
        }
        let value = crate::codec::hex::decode(item["value"].as_str().unwrap_or_default())
            .map_err(|e| format!("x509: extension {oid}: {e}"))?;
        der::AnyRef::from_der(&value)
            .map_err(|e| format!("x509: extension {oid}: the value is not one DER element: {e}"))?;
        let mut ext = rcgen::CustomExtension::from_oid_content(&arcs, value);
        ext.set_criticality(item["critical"].as_bool().unwrap_or(false));
        out.push(ext);
    }
    Ok(out)
}

/// The arcs of the dotted-decimal OID `oid`.
#[cfg(not(target_arch = "wasm32"))]
fn oid_arcs(oid: &str) -> Result<Vec<u64>, String> {
    let arcs = oid
        .split('.')
        .map(|arc| {
            let well_formed = !arc.is_empty()
                && arc.bytes().all(|b| b.is_ascii_digit())
                && (arc == "0" || !arc.starts_with('0'));
            well_formed.then(|| arc.parse::<u64>().ok()).flatten()
        })
        .collect::<Option<Vec<u64>>>()
        .filter(|arcs| arcs.len() >= 2 && arcs[0] <= 2 && (arcs[0] == 2 || arcs[1] < 40));
    arcs.ok_or_else(|| format!("x509: `{oid}` is not a well-formed OID"))
}

/// The extensions of the certificate `cert_pem` as the JSON array
/// [`parse_extensions`] reads, in the order the certificate holds them.
///
/// # Errors
/// The PEM is not a certificate.
#[cfg(not(target_arch = "wasm32"))]
pub fn extensions_json(cert_pem: &str) -> Result<String, String> {
    use x509_parser::prelude::FromDer;
    let der = certificate_der(cert_pem)?;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(&der)
        .map_err(|e| format!("x509: certificate: {e}"))?;
    let items: Vec<serde_json::Value> = cert
        .extensions()
        .iter()
        .map(|ext| {
            serde_json::json!({
                "oid": ext.oid.to_id_string(),
                "critical": ext.critical,
                "value": crate::codec::hex::encode(ext.value),
            })
        })
        .collect();
    Ok(serde_json::Value::Array(items).to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn certificate_der(cert_pem: &str) -> Result<Vec<u8>, String> {
    let (block, _) = crate::codec::pem::decode(cert_pem).map_err(|e| format!("x509: {e}"))?;
    if block.label != "CERTIFICATE" {
        return Err(format!(
            "x509: a `{}` PEM block is not a certificate",
            block.label
        ));
    }
    Ok(block.bytes)
}

/// Refuses a certificate that names one extension twice, which a custom
/// extension repeating one the generator writes (the subject alternative
/// names, the key usages) would make.
#[cfg(not(target_arch = "wasm32"))]
fn refuse_repeated_extensions(cert: &rcgen::Certificate) -> Result<(), String> {
    use x509_parser::prelude::FromDer;
    let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(cert.der())
        .map_err(|e| format!("x509: certificate: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    for ext in parsed.extensions() {
        let oid = ext.oid.to_id_string();
        if !seen.insert(oid.clone()) {
            return Err(format!("x509: the extension {oid} appears twice"));
        }
    }
    Ok(())
}

/// A self-signed certificate and its fresh P-256 key, as `(cert_pem,
/// key_pem)`.
///
/// # Errors
/// A DNS name that is not valid, or a validity under one day.
#[cfg(not(target_arch = "wasm32"))]
pub fn self_signed(
    common_name: &str,
    dns_names: &[String],
    days: i64,
    cert_use: CertUse,
    extensions: &[rcgen::CustomExtension],
) -> Result<(String, String), String> {
    let key = rcgen::KeyPair::generate().map_err(|e| format!("x509: key: {e}"))?;
    let cert = cert_params(common_name, dns_names, days, cert_use, extensions)?
        .self_signed(&key)
        .map_err(|e| format!("x509: {e}"))?;
    refuse_repeated_extensions(&cert)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// The certificate issuer `ca_cert_pem` / `ca_key_pem` names.
#[cfg(not(target_arch = "wasm32"))]
fn issuer(
    ca_cert_pem: &str,
    ca_key_pem: &str,
) -> Result<rcgen::Issuer<'static, rcgen::KeyPair>, String> {
    let key = rcgen::KeyPair::from_pem(ca_key_pem).map_err(|e| format!("x509: CA key: {e}"))?;
    rcgen::Issuer::from_ca_cert_pem(ca_cert_pem, key)
        .map_err(|e| format!("x509: CA certificate: {e}"))
}

/// A certificate the CA `ca_cert_pem` / `ca_key_pem` signs, with its fresh
/// P-256 key, as `(cert_pem, key_pem)`.
///
/// # Errors
/// A CA certificate or key that does not parse, a DNS name that is not
/// valid, or a validity under one day.
#[cfg(not(target_arch = "wasm32"))]
pub fn issue(
    ca_cert_pem: &str,
    ca_key_pem: &str,
    common_name: &str,
    dns_names: &[String],
    days: i64,
    cert_use: CertUse,
    extensions: &[rcgen::CustomExtension],
) -> Result<(String, String), String> {
    let issuer = issuer(ca_cert_pem, ca_key_pem)?;
    let key = rcgen::KeyPair::generate().map_err(|e| format!("x509: key: {e}"))?;
    let cert = cert_params(common_name, dns_names, days, cert_use, extensions)?
        .signed_by(&key, &issuer)
        .map_err(|e| format!("x509: {e}"))?;
    refuse_repeated_extensions(&cert)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// A certificate signing request for `common_name` and `dns_names` with its
/// fresh P-256 key, as `(csr_pem, key_pem)`.
///
/// # Errors
/// A DNS name that is not valid.
#[cfg(not(target_arch = "wasm32"))]
pub fn csr(common_name: &str, dns_names: &[String]) -> Result<(String, String), String> {
    let key = rcgen::KeyPair::generate().map_err(|e| format!("x509: key: {e}"))?;
    let use_server = CertUse {
        is_ca: false,
        client: false,
    };
    let request = cert_params(common_name, dns_names, 1, use_server, &[])?
        .serialize_request(&key)
        .map_err(|e| format!("x509: {e}"))?;
    let pem = request.pem().map_err(|e| format!("x509: {e}"))?;
    Ok((pem, key.serialize_pem()))
}

/// The certificate the CA signs for the request `csr_pem`, valid for `days`.
/// The request's names and key are kept; its signature is checked first.
///
/// # Errors
/// A request whose signature does not verify, a CA certificate or key that
/// does not parse, or a validity under one day.
#[cfg(not(target_arch = "wasm32"))]
pub fn sign_csr(
    csr_pem: &str,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    days: i64,
    client: bool,
) -> Result<String, String> {
    use rcgen::{ExtendedKeyUsagePurpose, IsCa, KeyUsagePurpose};
    if days <= 0 {
        return Err("x509: a certificate's validity must be at least one day".to_string());
    }
    let mut request = rcgen::CertificateSigningRequestParams::from_pem(csr_pem)
        .map_err(|e| format!("x509: signing request: {e}"))?;
    let now = time::OffsetDateTime::now_utc();
    request.params.not_before = now;
    request.params.not_after = now
        .checked_add(time::Duration::days(days))
        .ok_or_else(|| "x509: validity runs past the representable dates".to_string())?;
    request.params.is_ca = IsCa::ExplicitNoCa;
    request.params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    request.params.extended_key_usages = vec![if client {
        ExtendedKeyUsagePurpose::ClientAuth
    } else {
        ExtendedKeyUsagePurpose::ServerAuth
    }];
    let issuer = issuer(ca_cert_pem, ca_key_pem)?;
    let cert = request
        .signed_by(&issuer)
        .map_err(|e| format!("x509: {e}"))?;
    Ok(cert.pem())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn sha3_matches_the_fips_202_vectors() {
        let hex = |bytes: Vec<u8>| {
            bytes.iter().fold(String::new(), |mut out, b| {
                use std::fmt::Write as _;
                let _ = write!(out, "{b:02x}");
                out
            })
        };
        assert_eq!(
            hex(sha3_256(b"abc")),
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
        );
        assert!(hex(sha3_512(b"")).starts_with("a69f73cca23a9ac5c8b567dc185a756e97c98216"));
    }

    /// A 2048-bit RSA key made for these tests only.
    const RSA_TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCJhixYImDP8Nh6\nzRUWsj5wpWwm5xiMBzwe+zI/5gGHFkJL9Slav2V58njLZbdAiCl66pzfjKsHjzM9\nUITQjLGughDnaK6nsUCH3sJewySTkysyxCBkStUE08c+QWcDfUtJW20lJpKPdWSa\n/uLQc74A7aWzsgeDXWgyGfOiQNwWgONlAHipP/n+P+Yv1O0P1JY8fTQH+JbGhpfp\nv/Uv/ajmxbzpRlLlSoO18J27ew4ZLDl9Fsi1kfCoNffQu++0EmEocx16ppTsafg0\n1FW0XgMkJCx3KCoUKXncfEmJHnLVdcOmO045SKPjXhPDRievXuoworMpXZ/764gp\nFZKUk/NzAgMBAAECggEAF7YKBEIdJg0VDgs5BRpDWMG0xXpXyaDftUnDLrA1+DgW\nwyjoceyJDv8G467MazRuznIQDR0gSePfRkWROLHhWsCzMGrkniYaBbMbGSKVNlqL\n+qtFGHWsh7IJ6sp0++2I4wN/7IVbhJ+Nconaym0CR/+xb6uUzwn2UpkV4BZFvB1J\nHwEO4xledzmWDnnHOEh6TsdmrGs+gIZlAAmY/weJfakIVGxTYIPHMHpJhS4vCQ+f\n2w7jAkFh0N9HAw71qGEzck1Tn/3msZejI9k9RnhnHDGmaISpL0vSzAjZNc/0g8zy\n6MKiVwChKyrRHrPUMht09KWe6DU24Y4wpejAIKwaGQKBgQDAOwEQQxXKIzVqlvhE\nJacBKSP7BTR4jl6yqDsuaQDMSTs3zDP3/0EN76FxwI01yE+AXlj9GK4VAHHBoPmO\nlm2TtIV0XYtjZe5iZNtfJP5zXjByNiXXnMZvtSG5ya1JgMrus0+hUdvMhq0yUY2K\nTVLBV1PtX//AseTuCygrcS7myQKBgQC3JUf9BJ04fEJKBlDyrk4xgjAJEFDv+P5d\n436dAiFQin+Q6xP32cm2kIwjh8DbW4eM3nj3HsdVKhHPhnPfMwi67n2Mt/stL9bW\nePoJVkbGNo6iftXa+vMG5P3NhtgLsOlYkD3yq27gXG5EEJXUORUM8iAfeUaDEttt\nONhGg/uaWwKBgFdLNCtvW3jEzmAOGMXRl2pWhWlbrr1EYcqdzOpBcsRSSRsIl1z6\nRE+Qz265syaGRVwU/DJLP9p3rjDw6gMPWsllg+kZBdBonH2AMYKeaRJ45mUnI74r\nCo314k/dPt0PKJiHQxUN+FODoSklkbIJdRgpkQpNuvWFk2FqHZ7EsjjJAoGAYHhh\n0BQTIMLLRCTJlNtd0tRJqyyOBam5iwkfE2Mg20To5xP8o6xzhRfkyX5jhuhSoVA6\nlaj/xItxcEfZVzORoK2pMmtjAi1V1AlLY4mvpA7u2QdIbJ2NUA9k5VLnEFlywLTR\n/LZyo/nsFyxJT811mikb5JhEHRzcuk6cFaDF8ksCgYBO1Irod6Y7U2lvX+jxvkDG\n6chnVhCI/UwzZHADu3i/OO2po0vD9rhvMkMNWfwBRTG/URJNzz0PA83Ttn6V2N1W\nTsLgNc89CTSbCXlZ1EutSfCRRQ7Ub/sBwmRWN7u3yGl3G4xdeMOLtELv3zSXBcxI\nFFrxzZfeDTW7095brPmPoA==\n-----END PRIVATE KEY-----\n";

    #[test]
    fn oaep_ciphertext_decrypts_to_its_plaintext_and_differs_each_time() {
        let secret = RSA_TEST_KEY;
        let public = rsa_public_key_pem(secret).unwrap();
        let first = rsa_encrypt_oaep_sha256(&public, b"wrap this key").unwrap();
        let second = rsa_encrypt_oaep_sha256(&public, b"wrap this key").unwrap();
        assert_ne!(first, second);
        assert_eq!(
            rsa_decrypt_oaep_sha256(secret, &first).unwrap(),
            b"wrap this key"
        );
        let mut tampered = first;
        tampered[10] ^= 1;
        assert!(rsa_decrypt_oaep_sha256(secret, &tampered).is_err());
        assert!(rsa_encrypt_oaep_sha256(&public, &[0u8; 300]).is_err());
    }

    #[test]
    fn hkdf_sha256_matches_rfc_5869_case_1() {
        let ikm = [0x0b; 22];
        let salt: Vec<u8> = (0x00..=0x0c).collect();
        let info: Vec<u8> = (0xf0..=0xf9).collect();
        let okm = hkdf(HkdfHash::Sha256, &ikm, &salt, &info, 42).unwrap();
        assert_eq!(okm[..4], [0x3c, 0xb2, 0x5f, 0x25]);
        assert_eq!(okm.len(), 42);
        assert!(hkdf(HkdfHash::Sha256, &ikm, &salt, &info, 255 * 32 + 1).is_err());
    }

    #[test]
    fn an_issued_certificate_chains_to_its_ca() {
        let ca_use = CertUse {
            is_ca: true,
            client: false,
        };
        let leaf_use = CertUse {
            is_ca: false,
            client: false,
        };
        let (ca_cert, ca_key) = self_signed("Test CA", &[], 30, ca_use, &[]).unwrap();
        let (leaf, _) = issue(
            &ca_cert,
            &ca_key,
            "leaf",
            &["localhost".into()],
            30,
            leaf_use,
            &[],
        )
        .unwrap();
        assert!(leaf.starts_with("-----BEGIN CERTIFICATE-----"));
        let (request, _) = csr("svc", &["svc.test".into()]).unwrap();
        let signed = sign_csr(&request, &ca_cert, &ca_key, 30, false).unwrap();
        assert!(signed.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(self_signed("x", &[], 0, leaf_use, &[]).is_err());
    }
}
