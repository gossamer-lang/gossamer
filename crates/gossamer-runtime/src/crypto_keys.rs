//! Hashes, MACs, signatures, key generation, key agreement, and key
//! encapsulation for `std::crypto`, shared by every tier.
//!
//! Each operation goes through an audited `RustCrypto` or `ring` primitive; this
//! module only chooses the algorithm and the encoding. Keys and signatures
//! cross as PEM text or bytes, never as handles.

use hmac::Mac;

/// SHA-384 of `data`.
#[must_use]
pub fn sha384(data: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha384::digest(data).to_vec()
}

/// The hash an HMAC is built over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacHash {
    /// SHA-1, for TOTP and older APIs (`crypto::insecure`).
    Sha1,
    /// SHA-256.
    Sha256,
    /// SHA-384.
    Sha384,
    /// SHA-512.
    Sha512,
    /// SHA3-256.
    Sha3_256,
    /// SHA3-512.
    Sha3_512,
}

/// HMAC of `message` keyed by `key` (RFC 2104).
#[must_use]
pub fn hmac(hash: MacHash, key: &[u8], message: &[u8]) -> Vec<u8> {
    macro_rules! mac {
        ($digest:ty) => {{
            let mut m = keyed::<hmac::Hmac<$digest>>(key);
            m.update(message);
            m.finalize().into_bytes().to_vec()
        }};
    }
    match hash {
        MacHash::Sha1 => mac!(sha1::Sha1),
        MacHash::Sha256 => mac!(sha2::Sha256),
        MacHash::Sha384 => mac!(sha2::Sha384),
        MacHash::Sha512 => mac!(sha2::Sha512),
        MacHash::Sha3_256 => mac!(sha3::Sha3_256),
        MacHash::Sha3_512 => mac!(sha3::Sha3_512),
    }
}

/// Whether `tag` is the HMAC of `message` under `key`, compared in constant
/// time (`Mac::verify_slice`), so the comparison reveals nothing about where a
/// forged tag differs.
#[must_use]
pub fn hmac_verify(hash: MacHash, key: &[u8], message: &[u8], tag: &[u8]) -> bool {
    macro_rules! verify {
        ($digest:ty) => {{
            let mut m = keyed::<hmac::Hmac<$digest>>(key);
            m.update(message);
            m.verify_slice(tag).is_ok()
        }};
    }
    match hash {
        MacHash::Sha1 => verify!(sha1::Sha1),
        MacHash::Sha256 => verify!(sha2::Sha256),
        MacHash::Sha384 => verify!(sha2::Sha384),
        MacHash::Sha512 => verify!(sha2::Sha512),
        MacHash::Sha3_256 => verify!(sha3::Sha3_256),
        MacHash::Sha3_512 => verify!(sha3::Sha3_512),
    }
}

/// An HMAC instance keyed by `key`. HMAC hashes a key longer than its block
/// and pads a shorter one, so every key length is valid.
fn keyed<M: Mac + hmac::digest::KeyInit>(key: &[u8]) -> M {
    match <M as hmac::digest::KeyInit>::new_from_slice(key) {
        Ok(m) => m,
        Err(_) => unreachable!("HMAC accepts every key length"),
    }
}

/// A NIST curve for ECDSA and key agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    /// P-256 with SHA-256.
    P256,
    /// P-384 with SHA-384.
    P384,
}

impl Curve {
    fn name(self) -> &'static str {
        match self {
            Self::P256 => "P-256",
            Self::P384 => "P-384",
        }
    }

    /// Bytes in one scalar or coordinate.
    fn field_len(self) -> usize {
        match self {
            Self::P256 => 32,
            Self::P384 => 48,
        }
    }
}

/// A fresh ECDSA keypair as `(secret PKCS#8 PEM, public SPKI PEM)`.
///
/// # Errors
/// The OS random source failed.
pub fn ecdsa_keypair_pem(curve: Curve) -> Result<(String, String), String> {
    use p256::elliptic_curve::Generate;
    use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    macro_rules! keypair {
        ($signing:ty) => {{
            let key = <$signing>::try_generate().map_err(|_| "ecdsa: OS random source failed")?;
            let secret = key
                .to_pkcs8_pem(LineEnding::LF)
                .map_err(|e| format!("ecdsa: encode secret: {e}"))?;
            let public = key
                .verifying_key()
                .to_public_key_pem(LineEnding::LF)
                .map_err(|e| format!("ecdsa: encode public key: {e}"))?;
            Ok((secret.to_string(), public))
        }};
    }
    match curve {
        Curve::P256 => keypair!(p256::ecdsa::SigningKey),
        Curve::P384 => keypair!(p384::ecdsa::SigningKey),
    }
}

/// An ECDSA signature of `message` under the PKCS#8 key `secret_pem`, with
/// deterministic RFC 6979 nonces: DER (X.509, TLS) or the fixed `r || s`
/// form (JWT, `WebAuthn`) when `fixed`.
///
/// # Errors
/// The PEM is not a PKCS#8 key on `curve`.
pub fn ecdsa_sign(
    curve: Curve,
    secret_pem: &str,
    message: &[u8],
    fixed: bool,
) -> Result<Vec<u8>, String> {
    use p256::ecdsa::signature::Signer;
    use p256::pkcs8::DecodePrivateKey;
    macro_rules! sign {
        ($signing:ty, $sig:ty) => {{
            let key = <$signing>::from_pkcs8_pem(secret_pem)
                .map_err(|e| format!("ecdsa: {} secret key: {e}", curve.name()))?;
            let signature: $sig = key.sign(message);
            Ok(if fixed {
                signature.to_bytes().to_vec()
            } else {
                signature.to_der().as_bytes().to_vec()
            })
        }};
    }
    match curve {
        Curve::P256 => sign!(p256::ecdsa::SigningKey, p256::ecdsa::Signature),
        Curve::P384 => sign!(p384::ecdsa::SigningKey, p384::ecdsa::Signature),
    }
}

/// Whether `signature` (DER, or fixed `r || s` when `fixed`) is a valid
/// ECDSA signature of `message` under the SPKI key `public_pem`.
///
/// # Errors
/// The PEM is not an SPKI key on `curve`, the signature is malformed, or it
/// does not verify.
pub fn ecdsa_verify(
    curve: Curve,
    public_pem: &str,
    message: &[u8],
    signature: &[u8],
    fixed: bool,
) -> Result<(), String> {
    use p256::ecdsa::signature::Verifier;
    use p256::pkcs8::DecodePublicKey;
    macro_rules! verify {
        ($verifying:ty, $sig:ty) => {{
            let key = <$verifying>::from_public_key_pem(public_pem)
                .map_err(|e| format!("ecdsa: {} public key: {e}", curve.name()))?;
            let signature = if fixed {
                <$sig>::from_slice(signature)
            } else {
                <$sig>::from_der(signature)
            }
            .map_err(|_| format!("ecdsa: not a {} signature", curve.name()))?;
            key.verify(message, &signature)
                .map_err(|_| "ecdsa: signature does not verify".to_string())
        }};
    }
    match curve {
        Curve::P256 => verify!(p256::ecdsa::VerifyingKey, p256::ecdsa::Signature),
        Curve::P384 => verify!(p384::ecdsa::VerifyingKey, p384::ecdsa::Signature),
    }
}

/// `signature` converted between DER and the fixed `r || s` form: to the
/// fixed form when `to_fixed`, to DER otherwise.
///
/// # Errors
/// `signature` is not a signature on `curve` in the form converted from.
pub fn ecdsa_convert(curve: Curve, signature: &[u8], to_fixed: bool) -> Result<Vec<u8>, String> {
    macro_rules! convert {
        ($sig:ty) => {{
            if to_fixed {
                let sig = <$sig>::from_der(signature)
                    .map_err(|_| format!("ecdsa: not a DER {} signature", curve.name()))?;
                Ok(sig.to_bytes().to_vec())
            } else {
                let sig = <$sig>::from_slice(signature).map_err(|_| {
                    format!(
                        "ecdsa: a fixed {} signature is {} bytes, not {}",
                        curve.name(),
                        2 * curve.field_len(),
                        signature.len()
                    )
                })?;
                Ok(sig.to_der().as_bytes().to_vec())
            }
        }};
    }
    match curve {
        Curve::P256 => convert!(p256::ecdsa::Signature),
        Curve::P384 => convert!(p384::ecdsa::Signature),
    }
}

/// A fresh RSA keypair of `bits` bits (2048, 3072, or 4096; e = 65537) as
/// `(secret PKCS#8 PEM, public SPKI PEM)`.
///
/// # Errors
/// Another size, or the OS random source failed.
pub fn rsa_generate(bits: i64) -> Result<(String, String), String> {
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    if !matches!(bits, 2048 | 3072 | 4096) {
        return Err(format!(
            "rsa: a key is 2048, 3072, or 4096 bits, not {bits}"
        ));
    }
    let bits = usize::try_from(bits).map_err(|_| "rsa: key size".to_string())?;
    let key = rsa::PrivateKey::new(&mut getrandom04::SysRng, bits)
        .map_err(|e| format!("rsa: key generation: {e}"))?;
    let secret = key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| format!("rsa: encode secret: {e}"))?;
    let public = key
        .to_public_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| format!("rsa: encode public key: {e}"))?;
    Ok((secret.to_string(), public))
}

/// A key-agreement scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agreement {
    /// X25519 (RFC 7748): 32-byte secret and public keys.
    X25519,
    /// ECDH on a NIST curve: the secret is the scalar, the public key its
    /// uncompressed SEC1 point.
    Nist(Curve),
}

impl Agreement {
    /// The scheme a raw-leaf selector names: 0 X25519, 1 P-256, 2 P-384.
    ///
    /// # Errors
    /// Any other selector.
    pub fn from_selector(selector: i64) -> Result<Self, String> {
        match selector {
            0 => Ok(Self::X25519),
            1 => Ok(Self::Nist(Curve::P256)),
            2 => Ok(Self::Nist(Curve::P384)),
            other => Err(format!("ecdh: no key-agreement scheme {other}")),
        }
    }
}

/// A fresh key-agreement keypair as `(secret, public)` bytes. Either side may
/// keep it (a static key) or use it once (an ephemeral one).
///
/// # Errors
/// The OS random source failed.
pub fn agreement_keypair(scheme: Agreement) -> Result<(Vec<u8>, Vec<u8>), String> {
    use p256::elliptic_curve::Generate;
    use p256::elliptic_curve::sec1::ToSec1Point;
    match scheme {
        Agreement::X25519 => {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).map_err(|e| format!("ecdh: OS random source: {e}"))?;
            let secret = x25519_dalek::StaticSecret::from(bytes);
            let public = x25519_dalek::PublicKey::from(&secret);
            Ok((secret.to_bytes().to_vec(), public.as_bytes().to_vec()))
        }
        Agreement::Nist(Curve::P256) => {
            let secret = p256::SecretKey::try_generate()
                .map_err(|_| "ecdh: OS random source failed".to_string())?;
            let public = secret.public_key().to_sec1_point(false);
            Ok((secret.to_bytes().to_vec(), public.as_bytes().to_vec()))
        }
        Agreement::Nist(Curve::P384) => {
            let secret = p384::SecretKey::try_generate()
                .map_err(|_| "ecdh: OS random source failed".to_string())?;
            let public = secret.public_key().to_sec1_point(false);
            Ok((secret.to_bytes().to_vec(), public.as_bytes().to_vec()))
        }
    }
}

/// The shared secret of `secret` and the peer's `peer_public`.
///
/// # Errors
/// A key of the wrong length, a public key that is not a point on the curve,
/// or an X25519 result of all zeros (a small-order peer key, RFC 7748 §6.1).
pub fn agree(scheme: Agreement, secret: &[u8], peer_public: &[u8]) -> Result<Vec<u8>, String> {
    match scheme {
        Agreement::X25519 => {
            let secret: [u8; 32] = secret.try_into().map_err(|_| {
                format!(
                    "ecdh: an X25519 secret key is 32 bytes, not {}",
                    secret.len()
                )
            })?;
            let public: [u8; 32] = peer_public.try_into().map_err(|_| {
                format!(
                    "ecdh: an X25519 public key is 32 bytes, not {}",
                    peer_public.len()
                )
            })?;
            let shared = x25519_dalek::StaticSecret::from(secret)
                .diffie_hellman(&x25519_dalek::PublicKey::from(public));
            if !shared.was_contributory() {
                return Err("ecdh: the peer's X25519 key gives an all-zero secret".to_string());
            }
            Ok(shared.as_bytes().to_vec())
        }
        Agreement::Nist(curve) => {
            macro_rules! nist {
                ($secret:ty, $public:ty) => {{
                    let secret = <$secret>::from_slice(secret)
                        .map_err(|_| format!("ecdh: not a {} secret key", curve.name()))?;
                    let public = <$public>::from_sec1_bytes(peer_public).map_err(|_| {
                        format!("ecdh: the peer's key is not a {} point", curve.name())
                    })?;
                    let shared = p256::elliptic_curve::ecdh::diffie_hellman(
                        secret.to_nonzero_scalar(),
                        public.as_affine(),
                    );
                    Ok(shared.raw_secret_bytes().to_vec())
                }};
            }
            match curve {
                Curve::P256 => nist!(p256::SecretKey, p256::PublicKey),
                Curve::P384 => nist!(p384::SecretKey, p384::PublicKey),
            }
        }
    }
}

/// An ML-KEM parameter set (FIPS 203).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KemSet {
    /// ML-KEM-768.
    MlKem768,
    /// ML-KEM-1024.
    MlKem1024,
}

impl KemSet {
    /// The set a raw-leaf selector names: 768 or 1024.
    ///
    /// # Errors
    /// Any other selector.
    pub fn from_selector(selector: i64) -> Result<Self, String> {
        match selector {
            768 => Ok(Self::MlKem768),
            1024 => Ok(Self::MlKem1024),
            other => Err(format!("mlkem: no parameter set ML-KEM-{other}")),
        }
    }
}

/// A fresh ML-KEM keypair as `(decapsulation key, encapsulation key)`. The
/// decapsulation key is its 64-byte seed `d || z`, the form FIPS 203 §7.1
/// recommends keeping.
///
/// # Errors
/// The OS random source failed.
pub fn kem_keypair(set: KemSet) -> Result<(Vec<u8>, Vec<u8>), String> {
    use ml_kem::KeyExport;
    let mut seed = ml_kem::Seed::default();
    getrandom::fill(&mut seed).map_err(|e| format!("mlkem: OS random source: {e}"))?;
    let ek = match set {
        KemSet::MlKem768 => ml_kem::DecapsulationKey768::from_seed(seed)
            .encapsulation_key()
            .to_bytes()
            .to_vec(),
        KemSet::MlKem1024 => ml_kem::DecapsulationKey1024::from_seed(seed)
            .encapsulation_key()
            .to_bytes()
            .to_vec(),
    };
    Ok((seed.to_vec(), ek))
}

/// A shared secret encapsulated to the holder of `encapsulation_key`, as
/// `(ciphertext, shared secret)`.
///
/// # Errors
/// The key is not an encapsulation key of `set`.
pub fn kem_encapsulate(
    set: KemSet,
    encapsulation_key: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), String> {
    use ml_kem::Encapsulate;
    macro_rules! encapsulate {
        ($key:ty, $len:expr) => {{
            let bytes = ml_kem::array::Array::try_from(encapsulation_key).map_err(|_| {
                format!(
                    "mlkem: an {} encapsulation key is {} bytes, not {}",
                    set.name(),
                    $len,
                    encapsulation_key.len()
                )
            })?;
            let key = <$key>::new(&bytes)
                .map_err(|_| format!("mlkem: not a valid {} encapsulation key", set.name()))?;
            let (ciphertext, shared) = key.encapsulate();
            Ok((ciphertext.to_vec(), shared.to_vec()))
        }};
    }
    match set {
        KemSet::MlKem768 => encapsulate!(ml_kem::EncapsulationKey768, 1184),
        KemSet::MlKem1024 => encapsulate!(ml_kem::EncapsulationKey1024, 1568),
    }
}

/// The shared secret `ciphertext` carries for the holder of the seed
/// `decapsulation_key`. A ciphertext that was tampered with answers an
/// unrelated secret rather than an error, as FIPS 203's implicit rejection
/// requires, so a peer learns nothing from the failure.
///
/// # Errors
/// A seed that is not 64 bytes, or a ciphertext of the wrong length.
pub fn kem_decapsulate(
    set: KemSet,
    decapsulation_key: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, String> {
    use ml_kem::Decapsulate;
    let seed = ml_kem::Seed::try_from(decapsulation_key).map_err(|_| {
        format!(
            "mlkem: a decapsulation key is the 64-byte seed, not {} bytes",
            decapsulation_key.len()
        )
    })?;
    macro_rules! decapsulate {
        ($key:ty, $ct:ty, $len:expr) => {{
            let ct = <$ct>::try_from(ciphertext).map_err(|_| {
                format!(
                    "mlkem: an {} ciphertext is {} bytes, not {}",
                    set.name(),
                    $len,
                    ciphertext.len()
                )
            })?;
            Ok(<$key>::from_seed(seed).decapsulate(&ct).to_vec())
        }};
    }
    match set {
        KemSet::MlKem768 => {
            decapsulate!(
                ml_kem::DecapsulationKey768,
                ml_kem::ml_kem_768::Ciphertext,
                1088
            )
        }
        KemSet::MlKem1024 => {
            decapsulate!(
                ml_kem::DecapsulationKey1024,
                ml_kem::ml_kem_1024::Ciphertext,
                1568
            )
        }
    }
}

impl KemSet {
    fn name(self) -> &'static str {
        match self {
            Self::MlKem768 => "ML-KEM-768",
            Self::MlKem1024 => "ML-KEM-1024",
        }
    }
}

/// The PKCS#8 DER of the private key in `pem`: a PKCS#8 `PRIVATE KEY`, or
/// one of the legacy forms OpenSSL writes, a PKCS#1 `RSA PRIVATE KEY` or a
/// SEC1 `EC PRIVATE KEY` on P-256 or P-384.
///
/// # Errors
/// An encrypted key (which [`decrypt_private_key`] opens), another PEM
/// block, or a key that does not parse.
pub fn private_key_der(pem: &str) -> Result<Vec<u8>, String> {
    use p256::pkcs8::EncodePrivateKey;
    let (block, _) = crate::codec::pem::decode(pem).map_err(|e| format!("x509: {e}"))?;
    match block.label.as_str() {
        "PRIVATE KEY" => {
            pkcs8::PrivateKeyInfoRef::try_from(block.bytes.as_slice())
                .map_err(|e| format!("x509: PKCS#8 private key: {e}"))?;
            Ok(block.bytes)
        }
        "RSA PRIVATE KEY" => {
            use rsa::pkcs1::DecodeRsaPrivateKey;
            let key = rsa::PrivateKey::from_pkcs1_der(&block.bytes)
                .map_err(|e| format!("x509: PKCS#1 private key: {e}"))?;
            let der = rsa::pkcs8::EncodePrivateKey::to_pkcs8_der(&key)
                .map_err(|e| format!("x509: encode PKCS#8: {e}"))?;
            Ok(der.as_bytes().to_vec())
        }
        "EC PRIVATE KEY" => {
            let ec = sec1::EcPrivateKey::try_from(block.bytes.as_slice())
                .map_err(|e| format!("x509: SEC1 private key: {e}"))?;
            let curve = ec.parameters.and_then(sec1::EcParameters::named_curve);
            let der = if curve == Some(P256_OID) {
                p256::SecretKey::from_sec1_der(&block.bytes)
                    .map_err(|e| format!("x509: P-256 key: {e}"))?
                    .to_pkcs8_der()
            } else if curve == Some(P384_OID) {
                p384::SecretKey::from_sec1_der(&block.bytes)
                    .map_err(|e| format!("x509: P-384 key: {e}"))?
                    .to_pkcs8_der()
            } else {
                return Err(match curve {
                    Some(oid) => format!("x509: the curve {oid} is neither P-256 nor P-384"),
                    None => "x509: the SEC1 key does not name its curve".to_string(),
                });
            }
            .map_err(|e| format!("x509: encode PKCS#8: {e}"))?;
            Ok(der.as_bytes().to_vec())
        }
        "ENCRYPTED PRIVATE KEY" => Err(
            "x509: the key is encrypted; open it with x509::decrypt_private_key(pem, password)"
                .to_string(),
        ),
        other => Err(format!("x509: a `{other}` PEM block is not a private key")),
    }
}

const P256_OID: pkcs8::ObjectIdentifier =
    pkcs8::ObjectIdentifier::new_unwrap("1.2.840.10045.3.1.7");
const P384_OID: pkcs8::ObjectIdentifier = pkcs8::ObjectIdentifier::new_unwrap("1.3.132.0.34");

/// [`private_key_der`] as a PKCS#8 `PRIVATE KEY` PEM.
///
/// # Errors
/// As [`private_key_der`].
pub fn private_key_pem(pem: &str) -> Result<String, String> {
    Ok(crate::codec::pem::encode(
        "PRIVATE KEY",
        &private_key_der(pem)?,
    ))
}

/// The PKCS#8 `PRIVATE KEY` PEM inside the `ENCRYPTED PRIVATE KEY` `pem`.
/// Only PBES2 opens, with PBKDF2-HMAC-SHA256 or scrypt deriving the key and
/// AES-128-CBC or AES-256-CBC inside: the schemes OpenSSL 3 writes.
///
/// # Errors
/// Another PEM block, a PBES1 scheme (DES, RC2, MD5, SHA-1), another PBES2
/// choice, or a wrong password.
pub fn decrypt_private_key(pem: &str, password: &str) -> Result<String, String> {
    use pkcs8::pkcs5::pbes2::{EncryptionScheme as Cipher, Kdf, Pbkdf2Prf};
    let (block, _) = crate::codec::pem::decode(pem).map_err(|e| format!("x509: {e}"))?;
    if block.label != "ENCRYPTED PRIVATE KEY" {
        return Err(format!(
            "x509: a `{}` PEM block is not an encrypted private key",
            block.label
        ));
    }
    if let Some((oid, name)) = legacy_scheme(&block.bytes) {
        return Err(format!(
            "x509: the key uses {name} ({oid}), a PBES1 scheme, which is refused; re-encrypt it \
             with `openssl pkcs8 -topk8 -v2 aes-256-cbc`"
        ));
    }
    let info = pkcs8::EncryptedPrivateKeyInfoRef::try_from(block.bytes.as_slice())
        .map_err(|e| format!("x509: encrypted private key: {e}"))?;
    match &info.encryption_algorithm {
        pkcs8::pkcs5::EncryptionScheme::Pbes1(algorithm) => {
            return Err(format!(
                "x509: the key uses PBES1 ({}), which is refused; re-encrypt it with \
                 `openssl pkcs8 -topk8 -v2 aes-256-cbc`",
                algorithm.oid()
            ));
        }
        pkcs8::pkcs5::EncryptionScheme::Pbes2(params) => {
            let kdf_ok = match &params.kdf {
                Kdf::Pbkdf2(p) => p.prf == Pbkdf2Prf::HmacWithSha256,
                Kdf::Scrypt(_) => true,
                _ => false,
            };
            if !kdf_ok {
                return Err(
                    "x509: the key's PBES2 key derivation is neither PBKDF2-HMAC-SHA256 nor scrypt"
                        .to_string(),
                );
            }
            if !matches!(
                params.encryption,
                Cipher::Aes128Cbc { .. } | Cipher::Aes256Cbc { .. }
            ) {
                return Err(
                    "x509: the key's PBES2 cipher is neither AES-128-CBC nor AES-256-CBC"
                        .to_string(),
                );
            }
        }
        _ => return Err("x509: the key's encryption scheme is not PBES2".to_string()),
    }
    let document = info
        .decrypt(password)
        .map_err(|_| "x509: the password does not open the key".to_string())?;
    Ok(crate::codec::pem::encode(
        "PRIVATE KEY",
        document.as_bytes(),
    ))
}

/// The OID and name of the PBES1 or PKCS#12 scheme an `EncryptedPrivateKeyInfo`
/// names, or `None` for any other scheme. These derive keys with MD5 or SHA-1
/// and encrypt with DES, RC2, RC4, or 3DES, so none of them opens here.
fn legacy_scheme(der: &[u8]) -> Option<(String, &'static str)> {
    use x509_parser::der_parser::der::parse_der_sequence;
    let (_, info) = parse_der_sequence(der).ok()?;
    let algorithm = info
        .as_sequence()
        .ok()?
        .first()?
        .as_sequence()
        .ok()?
        .first()?;
    let oid = algorithm.as_oid().ok()?.to_id_string();
    let name = match oid.as_str() {
        "1.2.840.113549.1.5.1" => "pbeWithMD2AndDES-CBC",
        "1.2.840.113549.1.5.3" => "pbeWithMD5AndDES-CBC",
        "1.2.840.113549.1.5.4" => "pbeWithMD2AndRC2-CBC",
        "1.2.840.113549.1.5.6" => "pbeWithMD5AndRC2-CBC",
        "1.2.840.113549.1.5.10" => "pbeWithSHA1AndDES-CBC",
        "1.2.840.113549.1.5.11" => "pbeWithSHA1AndRC2-CBC",
        "1.2.840.113549.1.12.1.1" => "pbeWithSHAAnd128BitRC4",
        "1.2.840.113549.1.12.1.2" => "pbeWithSHAAnd40BitRC4",
        "1.2.840.113549.1.12.1.3" => "pbeWithSHAAnd3-KeyTripleDES-CBC",
        "1.2.840.113549.1.12.1.4" => "pbeWithSHAAnd2-KeyTripleDES-CBC",
        "1.2.840.113549.1.12.1.5" => "pbeWithSHAAnd128BitRC2-CBC",
        "1.2.840.113549.1.12.1.6" => "pbewithSHAAnd40BitRC2-CBC",
        _ => return None,
    };
    Some((oid, name))
}

/// The SPKI DER of the public key in `pem`: an SPKI `PUBLIC KEY`, a PKCS#1
/// `RSA PUBLIC KEY`, or the key a `CERTIFICATE` carries.
///
/// # Errors
/// Another PEM block, or a key that does not parse.
pub fn public_key_der(pem: &str) -> Result<Vec<u8>, String> {
    use x509_parser::prelude::FromDer;
    let (block, _) = crate::codec::pem::decode(pem).map_err(|e| format!("x509: {e}"))?;
    match block.label.as_str() {
        "PUBLIC KEY" => {
            x509_parser::x509::SubjectPublicKeyInfo::from_der(&block.bytes)
                .map_err(|e| format!("x509: SPKI public key: {e}"))?;
            Ok(block.bytes)
        }
        "RSA PUBLIC KEY" => {
            use rsa::pkcs1::DecodeRsaPublicKey;
            use rsa::pkcs8::EncodePublicKey;
            let key = rsa::PublicKey::from_pkcs1_der(&block.bytes)
                .map_err(|e| format!("x509: PKCS#1 public key: {e}"))?;
            let der = key
                .to_public_key_der()
                .map_err(|e| format!("x509: encode SPKI: {e}"))?;
            Ok(der.as_bytes().to_vec())
        }
        "CERTIFICATE" => {
            let (_, cert) = x509_parser::certificate::X509Certificate::from_der(&block.bytes)
                .map_err(|e| format!("x509: certificate: {e}"))?;
            Ok(cert.public_key().raw.to_vec())
        }
        other => Err(format!("x509: a `{other}` PEM block is not a public key")),
    }
}

/// [`public_key_der`] as an SPKI `PUBLIC KEY` PEM.
///
/// # Errors
/// As [`public_key_der`].
pub fn public_key_pem(pem: &str) -> Result<String, String> {
    Ok(crate::codec::pem::encode(
        "PUBLIC KEY",
        &public_key_der(pem)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        crate::codec::hex::encode(bytes)
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    fn key_fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/testdata/keys/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn legacy_private_keys_import_as_pkcs8_that_signs() {
        let rsa = private_key_pem(&key_fixture("rsa_pkcs1.pem")).unwrap();
        assert!(rsa.starts_with("-----BEGIN PRIVATE KEY-----"));
        let sig = crate::crypto_interop::rsa_sign(
            crate::crypto_interop::RsaScheme::PssSha256,
            &rsa,
            b"m",
        )
        .unwrap();
        let public = public_key_pem(&key_fixture("rsa_pkcs1_public.pem")).unwrap();
        crate::crypto_interop::rsa_verify(
            crate::crypto_interop::RsaScheme::PssSha256,
            &public,
            b"m",
            &sig,
        )
        .unwrap();
        for (file, curve) in [
            ("ec_p256_sec1.pem", Curve::P256),
            ("ec_p384_sec1.pem", Curve::P384),
        ] {
            let pkcs8 = private_key_pem(&key_fixture(file)).unwrap();
            assert!(ecdsa_sign(curve, &pkcs8, b"m", true).is_ok(), "{file}");
            assert_eq!(private_key_pem(&pkcs8).unwrap(), pkcs8);
        }
        assert!(
            private_key_pem(&key_fixture("p256_pbes2_pbkdf2.pem"))
                .unwrap_err()
                .contains("decrypt_private_key")
        );
    }

    #[test]
    fn pbes2_keys_open_with_their_password_and_pbes1_is_refused_by_name() {
        let expected = private_key_pem(&key_fixture("ec_p256_sec1.pem")).unwrap();
        for file in ["p256_pbes2_pbkdf2.pem", "p256_pbes2_scrypt.pem"] {
            let opened = decrypt_private_key(&key_fixture(file), "correct-horse").unwrap();
            assert_eq!(
                ecdsa_sign(Curve::P256, &opened, b"m", true).unwrap(),
                ecdsa_sign(Curve::P256, &expected, b"m", true).unwrap(),
                "{file}"
            );
            assert!(
                decrypt_private_key(&key_fixture(file), "wrong")
                    .unwrap_err()
                    .contains("password")
            );
        }
        for (file, scheme) in [
            (
                "p256_pbes1_md5_des.pem",
                "pbeWithMD5AndDES-CBC (1.2.840.113549.1.5.3)",
            ),
            (
                "p256_pkcs12_sha1_3des.pem",
                "pbeWithSHAAnd3-KeyTripleDES-CBC (1.2.840.113549.1.12.1.3)",
            ),
        ] {
            let refused = decrypt_private_key(&key_fixture(file), "correct-horse").unwrap_err();
            assert!(
                refused.contains(scheme) && refused.contains("PBES1"),
                "{refused}"
            );
        }
    }

    #[test]
    fn a_certificates_public_key_and_custom_extensions_read_back() {
        let cert = key_fixture("cert_with_ext.pem");
        assert!(
            public_key_pem(&cert)
                .unwrap()
                .starts_with("-----BEGIN PUBLIC KEY-----")
        );
        let json = crate::crypto_interop::extensions_json(&cert).unwrap();
        assert!(
            json.contains(r#""oid":"1.3.6.1.4.1.11129.2.4.3""#),
            "{json}"
        );
        assert!(json.contains(r#""critical":true"#), "{json}");
        assert!(json.contains(r#""value":"0500""#), "{json}");
    }

    #[test]
    fn custom_extensions_are_checked_and_written() {
        use crate::crypto_interop::{CertUse, parse_extensions, self_signed};
        let ok =
            parse_extensions(r#"[{"oid": "1.2.3.4", "critical": false, "value": "0c0268690"}]"#);
        assert!(ok.is_err(), "odd hex");
        assert!(
            parse_extensions(r#"[{"oid": "1.2.3.4", "critical": false, "value": "0c02"}]"#)
                .unwrap_err()
                .contains("one DER element")
        );
        assert!(
            parse_extensions(r#"[{"oid": "1.2.3.4", "critical": false, "value": "05000500"}]"#)
                .unwrap_err()
                .contains("one DER element")
        );
        assert!(
            parse_extensions(r#"[{"oid": "1.02.3", "critical": false, "value": "0500"}]"#)
                .unwrap_err()
                .contains("not a well-formed OID")
        );
        assert!(
            parse_extensions(r#"[{"oid": "3.1", "critical": false, "value": "0500"}]"#).is_err()
        );
        let twice = r#"[{"oid": "1.2.3.4", "critical": false, "value": "0500"}, {"oid": "1.2.3.4", "critical": true, "value": "0500"}]"#;
        assert!(
            parse_extensions(twice)
                .unwrap_err()
                .contains("appears twice")
        );
        let use_server = CertUse {
            is_ca: false,
            client: false,
        };
        let exts =
            parse_extensions(r#"[{"oid": "1.2.3.4", "critical": true, "value": "0c026869"}]"#)
                .unwrap();
        let (cert, _) = self_signed("x", &["x.test".into()], 1, use_server, &exts).unwrap();
        let json = crate::crypto_interop::extensions_json(&cert).unwrap();
        assert!(
            json.contains(r#"{"critical":true,"oid":"1.2.3.4","value":"0c026869"}"#),
            "{json}"
        );
        let san = parse_extensions(r#"[{"oid": "2.5.29.17", "critical": false, "value": "3000"}]"#)
            .unwrap();
        assert!(
            self_signed("x", &["x.test".into()], 1, use_server, &san)
                .unwrap_err()
                .contains("appears twice")
        );
    }

    #[test]
    fn sha384_matches_fips_180_4() {
        assert_eq!(
            hex(&sha384(b"abc")),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
        );
    }

    #[test]
    fn hmac_matches_rfc_4231_and_verifies_in_constant_time() {
        let key = [0x0b; 20];
        assert_eq!(
            hex(&hmac(MacHash::Sha384, &key, b"Hi There")),
            "afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59cfaea9ea9076ede7f4af152e8b2fa9cb6"
        );
        assert_eq!(
            hex(&hmac(MacHash::Sha512, &key, b"Hi There"))[..32],
            *"87aa7cdea5ef619d4ff0b4241a1d6cb0"
        );
        let tag = hmac(MacHash::Sha256, b"k", b"m");
        assert!(hmac_verify(MacHash::Sha256, b"k", b"m", &tag));
        assert!(!hmac_verify(MacHash::Sha256, b"k", b"n", &tag));
        assert!(!hmac_verify(MacHash::Sha256, b"k", b"m", &tag[..31]));
        // RFC 2202 test case 2 for HMAC-SHA1.
        assert_eq!(
            hex(&hmac(
                MacHash::Sha1,
                b"Jefe",
                b"what do ya want for nothing?"
            )),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
    }

    #[test]
    fn ecdsa_signs_deterministically_in_both_forms_on_both_curves() {
        for curve in [Curve::P256, Curve::P384] {
            let (secret, public) = ecdsa_keypair_pem(curve).unwrap();
            let der = ecdsa_sign(curve, &secret, b"msg", false).unwrap();
            assert_eq!(der, ecdsa_sign(curve, &secret, b"msg", false).unwrap());
            let fixed = ecdsa_sign(curve, &secret, b"msg", true).unwrap();
            assert_eq!(fixed.len(), 2 * curve.field_len());
            ecdsa_verify(curve, &public, b"msg", &der, false).unwrap();
            ecdsa_verify(curve, &public, b"msg", &fixed, true).unwrap();
            assert!(ecdsa_verify(curve, &public, b"other", &der, false).is_err());
            assert_eq!(ecdsa_convert(curve, &der, true).unwrap(), fixed);
            assert_eq!(ecdsa_convert(curve, &fixed, false).unwrap(), der);
        }
    }

    #[test]
    fn p384_nonces_follow_rfc_6979() {
        use p256::ecdsa::signature::Signer;
        use p256::pkcs8::{EncodePrivateKey, LineEnding};
        // RFC 6979 A.2.6: P-384, SHA-384, message "sample".
        let x = unhex(
            "6b9d3dad2e1b8c1c05b19875b6659f4de23c3b667bf297ba9aa47740787137d896d5724e4c70a825f872c9ea60d2edf5",
        );
        let key = p384::ecdsa::SigningKey::from_slice(&x).unwrap();
        let expected = "94edbb92a5ecb8aad4736e56c691916b3f88140666ce9fa73d64c4ea95ad133c81a648152e44acf96e36dd1e80fabe46\
                        99ef4aeb15f178cea1fe40db2603138f130e740a19624526203b6351d0a3a94fa329c145786e679e7b82c71a38628ac8";
        let signature: p384::ecdsa::Signature = key.sign(b"sample");
        assert_eq!(hex(&signature.to_bytes()), expected);
        let pem = key.to_pkcs8_pem(LineEnding::LF).unwrap();
        assert_eq!(
            hex(&ecdsa_sign(Curve::P384, &pem, b"sample", true).unwrap()),
            expected
        );
    }

    #[test]
    fn a_tampered_ml_kem_ciphertext_answers_an_unrelated_secret() {
        let (dk, ek) = kem_keypair(KemSet::MlKem768).unwrap();
        let (mut ct, sent) = kem_encapsulate(KemSet::MlKem768, &ek).unwrap();
        ct[0] ^= 1;
        let received = kem_decapsulate(KemSet::MlKem768, &dk, &ct).unwrap();
        assert_ne!(received, sent);
        assert!(kem_decapsulate(KemSet::MlKem768, &dk[..63], &ct).is_err());
        assert!(kem_encapsulate(KemSet::MlKem1024, &ek).is_err());
    }

    #[test]
    fn rsa_generates_only_the_supported_sizes() {
        let (secret, public) = rsa_generate(2048).unwrap();
        assert!(secret.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(public.starts_with("-----BEGIN PUBLIC KEY-----"));
        assert!(rsa_generate(1024).unwrap_err().contains("not 1024"));
        assert!(rsa_generate(2049).is_err());
    }

    #[test]
    fn both_sides_agree_on_every_scheme_and_bad_keys_are_refused() {
        for scheme in [
            Agreement::X25519,
            Agreement::Nist(Curve::P256),
            Agreement::Nist(Curve::P384),
        ] {
            let (a_secret, a_public) = agreement_keypair(scheme).unwrap();
            let (b_secret, b_public) = agreement_keypair(scheme).unwrap();
            assert_eq!(
                agree(scheme, &a_secret, &b_public).unwrap(),
                agree(scheme, &b_secret, &a_public).unwrap()
            );
        }
        let (secret, _) = agreement_keypair(Agreement::X25519).unwrap();
        assert!(
            agree(Agreement::X25519, &secret, &[0u8; 32])
                .unwrap_err()
                .contains("all-zero")
        );
        let (secret, public) = agreement_keypair(Agreement::Nist(Curve::P256)).unwrap();
        let mut off_curve = public.clone();
        off_curve[64] ^= 1;
        assert!(agree(Agreement::Nist(Curve::P256), &secret, &off_curve).is_err());
    }

    #[test]
    fn ml_kem_round_trips_and_matches_the_nist_acvp_vectors() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/mlkem_acvp.json")).unwrap();
        for (name, set) in [
            ("ML-KEM-768", KemSet::MlKem768),
            ("ML-KEM-1024", KemSet::MlKem1024),
        ] {
            let (dk, ek) = kem_keypair(set).unwrap();
            let (ct, sent) = kem_encapsulate(set, &ek).unwrap();
            assert_eq!(kem_decapsulate(set, &dk, &ct).unwrap(), sent);

            let v = &vectors["sets"][name];
            let mut seed = unhex(v["keygen"]["d"].as_str().unwrap());
            seed.extend(unhex(v["keygen"]["z"].as_str().unwrap()));
            let derived_ek = match set {
                KemSet::MlKem768 => {
                    use ml_kem::KeyExport;
                    ml_kem::DecapsulationKey768::from_seed(
                        ml_kem::Seed::try_from(&seed[..]).unwrap(),
                    )
                    .encapsulation_key()
                    .to_bytes()
                    .to_vec()
                }
                KemSet::MlKem1024 => {
                    use ml_kem::KeyExport;
                    ml_kem::DecapsulationKey1024::from_seed(
                        ml_kem::Seed::try_from(&seed[..]).unwrap(),
                    )
                    .encapsulation_key()
                    .to_bytes()
                    .to_vec()
                }
            };
            assert_eq!(
                hex(&sha256(&derived_ek)),
                v["keygen"]["ek_sha256"].as_str().unwrap()
            );

            let enc = &v["encapsulation"];
            let ek = unhex(enc["ek"].as_str().unwrap());
            let m = ml_kem::B32::try_from(&unhex(enc["m"].as_str().unwrap())[..]).unwrap();
            let (c, k) = match set {
                KemSet::MlKem768 => {
                    let key = ml_kem::EncapsulationKey768::new(
                        &ml_kem::array::Array::try_from(&ek[..]).unwrap(),
                    )
                    .unwrap();
                    let (c, k) = key.encapsulate_deterministic(&m);
                    (c.to_vec(), k.to_vec())
                }
                KemSet::MlKem1024 => {
                    let key = ml_kem::EncapsulationKey1024::new(
                        &ml_kem::array::Array::try_from(&ek[..]).unwrap(),
                    )
                    .unwrap();
                    let (c, k) = key.encapsulate_deterministic(&m);
                    (c.to_vec(), k.to_vec())
                }
            };
            assert_eq!(hex(&k), enc["k"].as_str().unwrap().to_lowercase());
            assert_eq!(hex(&sha256(&c)), enc["c_sha256"].as_str().unwrap());
        }
    }

    fn sha256(data: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        sha2::Sha256::digest(data).to_vec()
    }
}
