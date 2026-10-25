//! Runtime support for `std::crypto`.
//! Phase-1 primitives plus AEAD (AES-GCM, ChaCha20-Poly1305), digital
//! signatures (ECDSA P-256, Ed25519), X.509 certificate parsing,
//! key-derivation functions (PBKDF2, scrypt, Argon2), and the wider
//! hash family (SHA-256, SHA-512, BLAKE3). All FFI is contained inside
//! the upstream `RustCrypto` crates so this crate keeps
//! `#![forbid(unsafe_code)]`.
//!
//! RSA-PKCS#1 v1.5 was previously offered through `crypto::rsa`. It
//! was removed because the only viable pure-Rust implementation
//! (`rsa 0.9.x`) is affected by RUSTSEC-2023-0071 (Marvin Attack
//! timing sidechannel) with no upstream fix available. Use
//! `crypto::ed25519` or `crypto::ecdsa` for new code; the surface
//! returns when a constant-time RSA replacement lands.

#![forbid(unsafe_code)]

// The pure hash families (sha256/sha512/blake3/hmac/subtle) are
// computation-only and compile to wasm32; the rest pull an OS RNG
// (`getrandom`) or heavier crates and are gated to native so the
// in-browser playground still has hashing.
#[cfg(not(target_arch = "wasm32"))]
pub mod rand {
    //! OS-backed secure random bytes.
    //!
    //! Two surfaces:
    //!
    //! - [`fill`] / [`bytes()`] / [`nonce_12`] - recoverable. They
    //!   return `Result<_, Error>`; callers that can refuse the
    //!   operation surface the failure.
    //! - [`fill_or_abort`] - non-recoverable. Used by infallible
    //!   helpers like `OsRng`'s `RngCore` impl, where the
    //!   alternative was a `.expect(...)` panic. The function
    //!   `process::abort()`s on CSPRNG failure rather than
    //!   panicking, because a panic could be caught by an outer
    //!   [`std::panic::catch_unwind`] in a binding-ABI thunk
    //!   wrapper, an HTTP handler middleware, or a goroutine
    //!   isolation boundary. Aborting guarantees that no code path
    //!   produces all-zero entropy.

    use crate::errors::Error;

    /// Fills `buf` with cryptographically-secure random bytes from
    /// the host's CSPRNG.
    ///
    /// Uses the `getrandom` crate, which routes to the kernel CSPRNG
    /// on every supported target: `getrandom(2)` on Linux, `arc4random`
    /// on macOS / *BSD, and `BCryptGenRandom` on Windows. Preserves
    /// the workspace's no-`unsafe` rule because all platform FFI is
    /// contained inside the `getrandom` crate.
    ///
    /// On any host where the kernel CSPRNG is unavailable this returns
    /// an `Err` rather than zero-filling the buffer; callers must
    /// surface the failure to refuse the operation. Fault injection
    /// for tests is available via `set_fault_for_tests`.
    pub fn fill(buf: &mut [u8]) -> Result<(), Error> {
        if test_support::fault_injected() {
            return Err(Error::new(
                "rand: injected fault (set_fault_for_tests is true)",
            ));
        }
        getrandom::fill(buf).map_err(|e| Error::new(format!("rand: {e}")))
    }

    /// Fills `buf` with cryptographically-secure random bytes or
    /// aborts the process on failure.
    ///
    /// Use only at call sites whose API contract is infallible
    /// (`RngCore::fill_bytes`, internal key-material generation
    /// inside `OsRng`). The abort is deliberate: it cannot be
    /// caught by [`std::panic::catch_unwind`], so an outer
    /// fault-isolation boundary cannot silently swallow a
    /// catastrophic entropy failure and continue with zero bytes.
    pub fn fill_or_abort(buf: &mut [u8]) {
        match fill(buf) {
            Ok(()) => {}
            Err(err) => {
                // Log via stderr (best-effort; process is about to
                // die) so the operator sees the cause. Use `print!`-
                // free writeln to avoid re-entering panic-prone
                // formatting code in degraded conditions.
                let _ = std::io::Write::write_all(
                    &mut std::io::stderr(),
                    format!("crypto::rand: fatal: CSPRNG unavailable; aborting process: {err}\n")
                        .as_bytes(),
                );
                std::process::abort();
            }
        }
    }

    /// Convenience: allocates a fresh buffer and returns `n` random
    /// bytes.
    pub fn bytes(n: usize) -> Result<Vec<u8>, Error> {
        let mut out = vec![0u8; n];
        fill(&mut out)?;
        Ok(out)
    }

    /// Returns a freshly-generated 12-byte random nonce. Suitable for
    /// AES-GCM and ChaCha20-Poly1305 IVs (both use 96-bit nonces).
    pub fn nonce_12() -> Result<[u8; 12], Error> {
        let mut out = [0u8; 12];
        fill(&mut out)?;
        Ok(out)
    }

    /// Test-only hooks. Production callers never reach this module.
    pub mod test_support {
        use std::sync::atomic::{AtomicBool, Ordering};

        static FAULT: AtomicBool = AtomicBool::new(false);

        /// Enables or disables CSPRNG fault injection process-wide.
        /// When enabled, every call to [`super::fill`] returns
        /// `Err` without consulting `getrandom`. Tests must reset
        /// the flag to `false` before returning; the suite is
        /// process-global, not thread-scoped.
        pub fn set_fault_for_tests(on: bool) {
            FAULT.store(on, Ordering::SeqCst);
        }

        pub(super) fn fault_injected() -> bool {
            FAULT.load(Ordering::SeqCst)
        }
    }

    pub(crate) struct OsRng;

    impl rand_core::CryptoRng for OsRng {}

    // P-256 uses rand_core 0.10; Ed25519 still uses 0.6. Keep both on
    // the same entropy source and preserve the abort-on-failure contract.
    impl p256::elliptic_curve::rand_core::TryCryptoRng for OsRng {}

    impl p256::elliptic_curve::rand_core::TryRng for OsRng {
        type Error = core::convert::Infallible;

        fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
            Ok(rand_core::RngCore::next_u32(self))
        }

        fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
            Ok(rand_core::RngCore::next_u64(self))
        }

        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
            fill_or_abort(dest);
            Ok(())
        }
    }

    impl rand_core::RngCore for OsRng {
        fn next_u32(&mut self) -> u32 {
            let mut buf = [0u8; 4];
            fill_or_abort(&mut buf);
            u32::from_le_bytes(buf)
        }

        fn next_u64(&mut self) -> u64 {
            let mut buf = [0u8; 8];
            fill_or_abort(&mut buf);
            u64::from_le_bytes(buf)
        }

        fn fill_bytes(&mut self, dest: &mut [u8]) {
            fill_or_abort(dest);
        }

        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
            fill(dest).map_err(|_| {
                let nz = core::num::NonZeroU32::new(rand_core::Error::CUSTOM_START).unwrap();
                rand_core::Error::from(nz)
            })
        }
    }
}

pub mod sha256 {
    //! SHA-256 hashing - thin wrapper over the in-repo `gossamer-pkg`
    //! implementation so the crypto module has a stable, pure-Rust
    //! digest without pulling in a crypto crate.

    /// Returns the 32-byte SHA-256 digest of `input`.
    #[must_use]
    pub fn digest(input: &[u8]) -> [u8; 32] {
        gossamer_pkg::sha256::digest(input)
    }

    /// Returns the SHA-256 digest as lowercase hex.
    #[must_use]
    pub fn hex(input: &[u8]) -> String {
        gossamer_pkg::sha256::hex(input)
    }
}

pub mod sha512 {
    //! SHA-512 hashing.
    use sha2::Digest;

    /// Returns the 64-byte SHA-512 digest of `input`.
    #[must_use]
    pub fn digest(input: &[u8]) -> [u8; 64] {
        let mut h = sha2::Sha512::new();
        h.update(input);
        h.finalize().into()
    }

    /// Lowercase hex of [`digest`].
    #[must_use]
    pub fn hex(input: &[u8]) -> String {
        let bytes = digest(input);
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(super::nibble_char(b >> 4));
            out.push(super::nibble_char(b & 0x0f));
        }
        out
    }
}

pub mod blake3 {
    //! BLAKE3 cryptographic hash.
    /// Returns the 32-byte BLAKE3 digest of `input`.
    #[must_use]
    pub fn digest(input: &[u8]) -> [u8; 32] {
        let mut hasher = ::blake3::Hasher::new();
        hasher.update(input);
        *hasher.finalize().as_bytes()
    }

    /// Lowercase hex of [`digest`].
    #[must_use]
    pub fn hex(input: &[u8]) -> String {
        let bytes = digest(input);
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(super::nibble_char(b >> 4));
            out.push(super::nibble_char(b & 0x0f));
        }
        out
    }
}

pub mod hmac {
    //! HMAC-SHA-256.

    use super::sha256;

    const BLOCK_SIZE: usize = 64;

    /// HMAC-SHA-256 keyed MAC over `message`.
    #[must_use]
    pub fn sha256_mac(key: &[u8], message: &[u8]) -> [u8; 32] {
        let mut block = [0u8; BLOCK_SIZE];
        if key.len() > BLOCK_SIZE {
            block[..32].copy_from_slice(&sha256::digest(key));
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let mut inner_key = [0u8; BLOCK_SIZE];
        let mut outer_key = [0u8; BLOCK_SIZE];
        for i in 0..BLOCK_SIZE {
            inner_key[i] = block[i] ^ 0x36;
            outer_key[i] = block[i] ^ 0x5c;
        }
        let mut inner_input = Vec::with_capacity(BLOCK_SIZE + message.len());
        inner_input.extend_from_slice(&inner_key);
        inner_input.extend_from_slice(message);
        let inner_hash = sha256::digest(&inner_input);
        let mut outer_input = Vec::with_capacity(BLOCK_SIZE + 32);
        outer_input.extend_from_slice(&outer_key);
        outer_input.extend_from_slice(&inner_hash);
        sha256::digest(&outer_input)
    }
}

pub mod subtle {
    //! Constant-time compare helpers. Hides operand-dependent
    //! branches so attackers cannot time-side-channel on
    //! byte-by-byte secret comparison.

    /// Returns `true` iff `a == b`, with running time independent of
    /// where the first byte difference occurs.
    #[must_use]
    pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for i in 0..a.len() {
            diff |= a[i] ^ b[i];
        }
        diff == 0
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub mod aead {
    //! Authenticated encryption with associated data.
    //!
    //! Two algorithms ship: AES-256-GCM and ChaCha20-Poly1305. Both
    //! take a 32-byte key and a 12-byte nonce; both return a single
    //! ciphertext-with-tag blob to keep callers from forgetting to
    //! ship the auth tag separately. AAD is optional.

    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Nonce as AesNonce};
    use chacha20poly1305::{ChaCha20Poly1305, Nonce as ChaNonce};

    use crate::errors::Error;

    /// AES-256-GCM key length in bytes.
    pub const AES_KEY_LEN: usize = 32;
    /// AES-GCM nonce length in bytes.
    pub const AES_NONCE_LEN: usize = 12;

    /// ChaCha20-Poly1305 key length in bytes.
    pub const CHACHA_KEY_LEN: usize = 32;
    /// ChaCha20-Poly1305 nonce length in bytes.
    pub const CHACHA_NONCE_LEN: usize = 12;

    /// AES-256-GCM seal (encrypt + authenticate). Produces ciphertext
    /// followed by the 16-byte auth tag.
    pub fn aes_256_gcm_seal(
        key: &[u8],
        nonce: &[u8],
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, Error> {
        if key.len() != AES_KEY_LEN {
            return Err(Error::new(format!(
                "aes-256-gcm: key must be {AES_KEY_LEN} bytes"
            )));
        }
        if nonce.len() != AES_NONCE_LEN {
            return Err(Error::new(format!(
                "aes-256-gcm: nonce must be {AES_NONCE_LEN} bytes"
            )));
        }
        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|e| Error::new(format!("aes-256-gcm: key: {e}")))?;
        cipher
            .encrypt(
                AesNonce::from_slice(nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|e| Error::new(format!("aes-256-gcm: seal: {e}")))
    }

    /// AES-256-GCM open (decrypt + verify).
    pub fn aes_256_gcm_open(
        key: &[u8],
        nonce: &[u8],
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, Error> {
        if key.len() != AES_KEY_LEN {
            return Err(Error::new(format!(
                "aes-256-gcm: key must be {AES_KEY_LEN} bytes"
            )));
        }
        if nonce.len() != AES_NONCE_LEN {
            return Err(Error::new(format!(
                "aes-256-gcm: nonce must be {AES_NONCE_LEN} bytes"
            )));
        }
        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|e| Error::new(format!("aes-256-gcm: key: {e}")))?;
        cipher
            .decrypt(
                AesNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|e| Error::new(format!("aes-256-gcm: open: {e}")))
    }

    /// ChaCha20-Poly1305 seal.
    pub fn chacha20_poly1305_seal(
        key: &[u8],
        nonce: &[u8],
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, Error> {
        if key.len() != CHACHA_KEY_LEN {
            return Err(Error::new(format!(
                "chacha20-poly1305: key must be {CHACHA_KEY_LEN} bytes"
            )));
        }
        if nonce.len() != CHACHA_NONCE_LEN {
            return Err(Error::new(format!(
                "chacha20-poly1305: nonce must be {CHACHA_NONCE_LEN} bytes"
            )));
        }
        let cipher = ChaCha20Poly1305::new_from_slice(key)
            .map_err(|e| Error::new(format!("chacha20-poly1305: key: {e}")))?;
        cipher
            .encrypt(
                ChaNonce::from_slice(nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|e| Error::new(format!("chacha20-poly1305: seal: {e}")))
    }

    /// ChaCha20-Poly1305 open.
    pub fn chacha20_poly1305_open(
        key: &[u8],
        nonce: &[u8],
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, Error> {
        if key.len() != CHACHA_KEY_LEN {
            return Err(Error::new(format!(
                "chacha20-poly1305: key must be {CHACHA_KEY_LEN} bytes"
            )));
        }
        if nonce.len() != CHACHA_NONCE_LEN {
            return Err(Error::new(format!(
                "chacha20-poly1305: nonce must be {CHACHA_NONCE_LEN} bytes"
            )));
        }
        let cipher = ChaCha20Poly1305::new_from_slice(key)
            .map_err(|e| Error::new(format!("chacha20-poly1305: key: {e}")))?;
        cipher
            .decrypt(
                ChaNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|e| Error::new(format!("chacha20-poly1305: open: {e}")))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub mod ed25519 {
    //! Ed25519 signatures.

    use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

    use crate::errors::Error;

    /// Generates a fresh Ed25519 signing keypair from the host CSPRNG.
    /// Returns `(secret, public)` byte arrays.
    pub fn keypair() -> Result<([u8; 32], [u8; 32]), Error> {
        let mut rng = super::rand::OsRng;
        let signing = SigningKey::generate(&mut rng);
        let public = signing.verifying_key();
        Ok((signing.to_bytes(), public.to_bytes()))
    }

    /// Signs `message` with a 32-byte secret key.
    pub fn sign(secret: &[u8], message: &[u8]) -> Result<[u8; 64], Error> {
        let secret: [u8; 32] = secret
            .try_into()
            .map_err(|_| Error::new("ed25519: secret must be 32 bytes"))?;
        let signing = SigningKey::from_bytes(&secret);
        Ok(signing.sign(message).to_bytes())
    }

    /// Verifies a 64-byte signature on `message` against a 32-byte public key.
    pub fn verify(public: &[u8], message: &[u8], signature: &[u8]) -> Result<(), Error> {
        let public: [u8; 32] = public
            .try_into()
            .map_err(|_| Error::new("ed25519: public key must be 32 bytes"))?;
        let signature: [u8; 64] = signature
            .try_into()
            .map_err(|_| Error::new("ed25519: signature must be 64 bytes"))?;
        let key = VerifyingKey::from_bytes(&public)
            .map_err(|e| Error::new(format!("ed25519: public key: {e}")))?;
        key.verify(message, &Signature::from_bytes(&signature))
            .map_err(|e| Error::new(format!("ed25519: verify: {e}")))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub mod ecdsa {
    //! ECDSA over the NIST P-256 curve.

    use p256::ecdsa::signature::{Signer, Verifier};
    use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
    use p256::elliptic_curve::Generate;
    use p256::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};

    use crate::errors::Error;

    /// Generates a fresh P-256 keypair. Returns
    /// `(secret_pkcs8_pem, public_spki_pem)`.
    pub fn keypair_pem() -> Result<(String, String), Error> {
        let mut rng = super::rand::OsRng;
        let signing = SigningKey::generate_from_rng(&mut rng);
        let secret_pem = signing
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .map_err(|e| Error::new(format!("ecdsa: encode secret: {e}")))?
            .to_string();
        let verifying = signing.verifying_key();
        let public_pem = verifying
            .to_public_key_pem(p256::pkcs8::LineEnding::LF)
            .map_err(|e| Error::new(format!("ecdsa: encode public: {e}")))?;
        Ok((secret_pem, public_pem))
    }

    /// Signs `message` with a PKCS#8-PEM-encoded P-256 secret key.
    /// Returns the DER-encoded signature.
    pub fn sign_pem(secret_pem: &str, message: &[u8]) -> Result<Vec<u8>, Error> {
        let signing = SigningKey::from_pkcs8_pem(secret_pem)
            .map_err(|e| Error::new(format!("ecdsa: secret pem: {e}")))?;
        let sig: Signature = signing.sign(message);
        Ok(sig.to_der().as_bytes().to_vec())
    }

    /// Verifies a DER-encoded signature against an SPKI-PEM-encoded
    /// P-256 public key.
    pub fn verify_pem(public_pem: &str, message: &[u8], signature: &[u8]) -> Result<(), Error> {
        let key = VerifyingKey::from_public_key_pem(public_pem)
            .map_err(|e| Error::new(format!("ecdsa: public pem: {e}")))?;
        let sig = Signature::from_der(signature)
            .map_err(|e| Error::new(format!("ecdsa: signature: {e}")))?;
        key.verify(message, &sig)
            .map_err(|e| Error::new(format!("ecdsa: verify: {e}")))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub mod x509 {
    //! X.509 v3 certificate inspection and TLS server-certificate
    //! verification.

    use crate::errors::Error;
    use rustls::RootCertStore;
    use rustls::client::WebPkiServerVerifier;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use std::sync::Arc;

    /// Inspected certificate fields. Only the load-bearing pieces are
    /// surfaced; the raw DER is preserved for callers needing the rest.
    #[derive(Debug, Clone)]
    pub struct CertInfo {
        /// PEM-encoded subject common name (best effort).
        pub subject: String,
        /// PEM-encoded issuer common name (best effort).
        pub issuer: String,
        /// Serial number as a big-endian byte string.
        pub serial: Vec<u8>,
        /// Validity start (`Not Before`) as a Unix timestamp.
        pub not_before_unix: i64,
        /// Validity end (`Not After`) as a Unix timestamp.
        pub not_after_unix: i64,
        /// `SubjectAltName` DNS names (empty when the SAN extension
        /// is missing).
        pub san_dns: Vec<String>,
        /// SHA-256 of the DER-encoded certificate.
        pub sha256: [u8; 32],
    }

    /// Parses one PEM-encoded certificate.
    pub fn parse_pem(pem: &[u8]) -> Result<CertInfo, Error> {
        let (_, der) = x509_parser::pem::parse_x509_pem(pem)
            .map_err(|e| Error::new(format!("x509: pem: {e}")))?;
        parse_der(&der.contents)
    }

    /// Parses one DER-encoded certificate.
    pub fn parse_der(der: &[u8]) -> Result<CertInfo, Error> {
        let (_, cert) = x509_parser::parse_x509_certificate(der)
            .map_err(|e| Error::new(format!("x509: der: {e}")))?;
        let subject = cert.subject().to_string();
        let issuer = cert.issuer().to_string();
        let serial = cert.serial.to_bytes_be();
        let not_before_unix = cert.validity().not_before.timestamp();
        let not_after_unix = cert.validity().not_after.timestamp();
        let mut san_dns = Vec::new();
        if let Ok(Some(san)) = cert.subject_alternative_name() {
            for name in &san.value.general_names {
                if let x509_parser::extensions::GeneralName::DNSName(s) = name {
                    san_dns.push((*s).to_string());
                }
            }
        }
        let sha256 = super::sha256::digest(der);
        Ok(CertInfo {
            subject,
            issuer,
            serial,
            not_before_unix,
            not_after_unix,
            san_dns,
            sha256,
        })
    }

    /// Verifies a PEM-encoded TLS server certificate chain against a
    /// PEM-encoded private root store, hostname, and PEM CRLs.
    ///
    /// This is the Gossamer-callable form of the same fail-closed policy used
    /// by [`crate::tls::client_config_with_crls`]: the full chain and hostname
    /// are checked by rustls/webpki, unknown revocation status is rejected, and
    /// CRLs past `nextUpdate` are rejected. The leaf certificate must be first
    /// in `chain_pem`; remaining certificates are passed as intermediates.
    pub fn verify_server_certificate_with_crls(
        chain_pem: &[u8],
        roots_pem: &[u8],
        hostname: &str,
        crl_pem: &[u8],
    ) -> Result<(), Error> {
        let chain = CertificateDer::pem_slice_iter(chain_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::new(format!("x509: certificate chain: {e}")))?;
        let (leaf, intermediates) = chain
            .split_first()
            .ok_or_else(|| Error::new("x509: certificate chain contains no certificates"))?;

        let mut roots = RootCertStore::empty();
        let root_certs = CertificateDer::pem_slice_iter(roots_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::new(format!("x509: roots: {e}")))?;
        if root_certs.is_empty() {
            return Err(Error::new("x509: roots contain no certificates"));
        }
        for root in root_certs {
            roots
                .add(root)
                .map_err(|e| Error::new(format!("x509: root: {e}")))?;
        }

        let crls = rustls::pki_types::CertificateRevocationListDer::pem_slice_iter(crl_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::new(format!("x509: CRL: {e}")))?;
        if crls.is_empty() {
            return Err(Error::new("x509: CRL input contains no X509 CRLs"));
        }
        let verifier = WebPkiServerVerifier::builder(Arc::new(roots))
            .with_crls(crls)
            .enforce_revocation_expiration()
            .build()
            .map_err(|e| Error::new(format!("x509: verifier: {e}")))?;
        let server_name = ServerName::try_from(hostname.to_owned())
            .map_err(|e| Error::new(format!("x509: hostname: {e}")))?;
        verifier
            .verify_server_cert(leaf, intermediates, &server_name, &[], UnixTime::now())
            .map(|_| ())
            .map_err(|e| Error::new(format!("x509: verify server certificate: {e}")))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub mod kdf {
    //! Password-based key derivation: PBKDF2, scrypt, Argon2id.

    use argon2::password_hash::{PasswordHasher, PasswordVerifier, SaltString};
    use argon2::{Algorithm, Argon2, Params, Version};
    use pbkdf2::pbkdf2_hmac;
    use scrypt::{Params as ScryptParams, scrypt};
    use sha2::Sha256;

    use crate::errors::Error;

    /// PBKDF2-HMAC-SHA256. `output` defines the derived-key length.
    #[must_use]
    pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32, output: usize) -> Vec<u8> {
        let mut out = vec![0u8; output];
        pbkdf2_hmac::<Sha256>(password, salt, iterations, &mut out);
        out
    }

    /// scrypt with the standard interactive cost (`log_n=15, r=8, p=1`).
    pub fn scrypt_interactive(
        password: &[u8],
        salt: &[u8],
        output: usize,
    ) -> Result<Vec<u8>, Error> {
        let params = ScryptParams::new(15, 8, 1, output)
            .map_err(|e| Error::new(format!("scrypt: params: {e}")))?;
        let mut out = vec![0u8; output];
        scrypt(password, salt, &params, &mut out)
            .map_err(|e| Error::new(format!("scrypt: derive: {e}")))?;
        Ok(out)
    }

    /// Hashes `password` for storage with Argon2id and the default
    /// interactive parameters. Returns the PHC-format string. Pair
    /// with [`argon2id_verify`] for verification.
    pub fn argon2id_hash(password: &[u8]) -> Result<String, Error> {
        let mut salt_bytes = [0u8; 16];
        super::rand::fill(&mut salt_bytes)?;
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|e| Error::new(format!("argon2: salt: {e}")))?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default());
        let hash = argon
            .hash_password(password, &salt)
            .map_err(|e| Error::new(format!("argon2: hash: {e}")))?;
        Ok(hash.to_string())
    }

    /// Verifies `password` against a PHC-format hash produced by
    /// [`argon2id_hash`].
    pub fn argon2id_verify(password: &[u8], phc: &str) -> Result<bool, Error> {
        let parsed = argon2::password_hash::PasswordHash::new(phc)
            .map_err(|e| Error::new(format!("argon2: parse phc: {e}")))?;
        Ok(Argon2::default().verify_password(password, &parsed).is_ok())
    }
}

/// Legacy/insecure hashes (MD5, SHA-1). Feature-gated; must not be used for security.
pub mod insecure;

/// High-level password storage: opinionated argon2id defaults with
/// PHC-string round-trip and a `needs_rehash` advisory.
///
/// Thin facade over [`kdf::argon2id_hash`] / [`kdf::argon2id_verify`]
/// so application code stays readable:
///
/// ```text
/// let phc = password::hash(b"correct horse battery staple")?;
/// if password::verify(b"correct horse battery staple", &phc)? {
///     // login succeeded; rehash if parameters have moved
///     if password::needs_rehash(&phc) {
///         let new_phc = password::hash(b"correct horse battery staple")?;
///         // store new_phc
///     }
/// }
/// ```
#[cfg(not(target_arch = "wasm32"))]
pub mod password {
    use argon2::password_hash::PasswordHash;
    use argon2::{Algorithm, Params, Version};

    use crate::errors::Error;

    /// Hashes `plaintext` with Argon2id at the current default
    /// parameters. Returns a PHC-format string suitable for storage.
    pub fn hash(plaintext: &[u8]) -> Result<String, Error> {
        super::kdf::argon2id_hash(plaintext)
    }

    /// Verifies `plaintext` against a stored PHC-format hash. Uses
    /// the parameters embedded in the PHC string, so old hashes
    /// still verify after the defaults move.
    pub fn verify(plaintext: &[u8], phc: &str) -> Result<bool, Error> {
        super::kdf::argon2id_verify(plaintext, phc)
    }

    /// Returns `true` if `phc` was hashed with parameters weaker
    /// than the current default. Call after a successful
    /// [`verify`] and, when this returns true, re-hash the
    /// plaintext via [`hash`] and store the new PHC string.
    ///
    /// Returns `false` (the conservative default) when the PHC
    /// cannot be parsed - re-hashing an unparseable record is the
    /// caller's choice, not ours.
    #[must_use]
    pub fn needs_rehash(phc: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(phc) else {
            return false;
        };
        if !matches!(parsed.algorithm.as_str(), "argon2id") {
            // Any non-argon2id hash should be re-encoded to current default.
            return true;
        }
        let target = Params::default();
        let Ok(current) = Params::try_from(&parsed) else {
            return true;
        };
        current.m_cost() < target.m_cost()
            || current.t_cost() < target.t_cost()
            || current.p_cost() < target.p_cost()
    }

    /// Bare reference to the active algorithm and parameter set;
    /// useful for diagnostic logs.
    #[must_use]
    pub fn current_algorithm() -> &'static str {
        "argon2id"
    }

    /// Returns `(m_cost_kib, t_cost_iters, p_cost_parallelism)` -
    /// the active argon2id parameters.
    #[must_use]
    pub fn current_params() -> (u32, u32, u32) {
        let p = Params::default();
        (p.m_cost(), p.t_cost(), p.p_cost())
    }

    // Reserved: future addition of bcrypt for legacy compatibility.
    // Excluded today because the `bcrypt` crate is not a workspace
    // dep yet; add it behind a clearly-named function so callers
    // can detect legacy hashes via PHC prefix and reach for the
    // legacy path without touching the default `verify` flow.
    #[doc(hidden)]
    #[must_use]
    pub fn _algorithm_marker() -> Algorithm {
        Algorithm::Argon2id
    }

    #[doc(hidden)]
    #[must_use]
    pub fn _version_marker() -> Version {
        Version::V0x13
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn hash_verify_round_trip() {
            let pw = b"correct horse battery staple";
            let phc = hash(pw).expect("hash");
            assert!(phc.starts_with("$argon2id$"));
            assert!(verify(pw, &phc).expect("verify"));
            assert!(!verify(b"wrong", &phc).expect("verify wrong"));
        }

        #[test]
        fn needs_rehash_false_for_current_defaults() {
            let phc = hash(b"pw").expect("hash");
            assert!(!needs_rehash(&phc));
        }

        #[test]
        fn needs_rehash_true_for_unparseable_input() {
            // Unparseable returns false (conservative) - caller decides.
            assert!(!needs_rehash("not-a-phc-string"));
        }

        #[test]
        fn current_params_match_argon2_defaults() {
            let (m, t, p) = current_params();
            assert!(m > 0);
            assert!(t > 0);
            assert!(p > 0);
        }

        #[test]
        fn empty_password_hashes_cleanly() {
            let phc = hash(b"").expect("hash empty");
            assert!(verify(b"", &phc).expect("verify"));
        }
    }
}

#[inline]
fn nibble_char(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + n - 10) as char,
        _ => '?',
    }
}

/// Block-cipher modes: AES-CTR, AES-CBC. Mirrors Go's
/// `crypto/cipher` surface - `Stream` / `Block` primitives so
/// downstream code can compose modes that Gossamer doesn't
/// ship directly (e.g. AES-OFB layered on the same Block).
#[cfg(not(target_arch = "wasm32"))]
pub mod cipher {
    use crate::errors::Error;
    use cipher::{
        BlockDecryptMut, BlockEncryptMut, BlockSizeUser, KeyIvInit, StreamCipher,
        block_padding::Pkcs7,
    };

    /// AES key sizes accepted by [`aes_block_size`] and the
    /// stream-mode constructors below.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AesKeySize {
        /// 128-bit key.
        K128,
        /// 192-bit key.
        K192,
        /// 256-bit key.
        K256,
    }

    /// Returns the AES block size in bytes (always 16).
    #[must_use]
    pub fn aes_block_size() -> usize {
        16
    }

    /// Detects the AES key size from a byte slice. Returns
    /// `Err` for any length other than 16 / 24 / 32.
    pub fn aes_key_size(key: &[u8]) -> Result<AesKeySize, Error> {
        match key.len() {
            16 => Ok(AesKeySize::K128),
            24 => Ok(AesKeySize::K192),
            32 => Ok(AesKeySize::K256),
            n => Err(Error::new(format!(
                "AES key must be 16/24/32 bytes, got {n}"
            ))),
        }
    }

    /// In-place AES-CTR. The same call both encrypts and
    /// decrypts (CTR is symmetric). `iv` must be exactly
    /// [`aes_block_size`] bytes; reusing a `(key, iv)` pair on
    /// distinct plaintexts is catastrophic - pick a fresh `iv`
    /// per message.
    pub fn aes_ctr_xor(key: &[u8], iv: &[u8], buf: &mut [u8]) -> Result<(), Error> {
        if iv.len() != aes_block_size() {
            return Err(Error::new(format!(
                "AES-CTR iv must be {} bytes, got {}",
                aes_block_size(),
                iv.len()
            )));
        }
        match aes_key_size(key)? {
            AesKeySize::K128 => {
                let mut c = ctr::Ctr128BE::<aes::Aes128>::new(key.into(), iv.into());
                c.apply_keystream(buf);
            }
            AesKeySize::K192 => {
                let mut c = ctr::Ctr128BE::<aes::Aes192>::new(key.into(), iv.into());
                c.apply_keystream(buf);
            }
            AesKeySize::K256 => {
                let mut c = ctr::Ctr128BE::<aes::Aes256>::new(key.into(), iv.into());
                c.apply_keystream(buf);
            }
        }
        Ok(())
    }

    /// Encrypts `plaintext` with AES-CBC + PKCS#7 padding.
    /// Returns the ciphertext; the IV is NOT prepended - pass
    /// it alongside ciphertext on the wire.
    pub fn aes_cbc_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        if iv.len() != aes_block_size() {
            return Err(Error::new(format!(
                "AES-CBC iv must be {} bytes",
                aes_block_size()
            )));
        }
        let block_size = aes_block_size();
        let mut buf = vec![0u8; plaintext.len() + block_size];
        let ct_len = match aes_key_size(key)? {
            AesKeySize::K128 => cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into())
                .encrypt_padded_b2b_mut::<Pkcs7>(plaintext, &mut buf)
                .map(<[u8]>::len)
                .map_err(|e| Error::new(format!("AES-CBC encrypt: {e}")))?,
            AesKeySize::K192 => cbc::Encryptor::<aes::Aes192>::new(key.into(), iv.into())
                .encrypt_padded_b2b_mut::<Pkcs7>(plaintext, &mut buf)
                .map(<[u8]>::len)
                .map_err(|e| Error::new(format!("AES-CBC encrypt: {e}")))?,
            AesKeySize::K256 => cbc::Encryptor::<aes::Aes256>::new(key.into(), iv.into())
                .encrypt_padded_b2b_mut::<Pkcs7>(plaintext, &mut buf)
                .map(<[u8]>::len)
                .map_err(|e| Error::new(format!("AES-CBC encrypt: {e}")))?,
        };
        buf.truncate(ct_len);
        Ok(buf)
    }

    /// Inverts [`aes_cbc_encrypt`]. Strips PKCS#7 padding.
    pub fn aes_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        if iv.len() != aes_block_size() {
            return Err(Error::new(format!(
                "AES-CBC iv must be {} bytes",
                aes_block_size()
            )));
        }
        let _ = <aes::Aes128 as BlockSizeUser>::block_size();
        let mut buf = ciphertext.to_vec();
        let pt: &[u8] = match aes_key_size(key)? {
            AesKeySize::K128 => cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
                .decrypt_padded_mut::<Pkcs7>(&mut buf)
                .map_err(|e| Error::new(format!("AES-CBC decrypt: {e}")))?,
            AesKeySize::K192 => cbc::Decryptor::<aes::Aes192>::new(key.into(), iv.into())
                .decrypt_padded_mut::<Pkcs7>(&mut buf)
                .map_err(|e| Error::new(format!("AES-CBC decrypt: {e}")))?,
            AesKeySize::K256 => cbc::Decryptor::<aes::Aes256>::new(key.into(), iv.into())
                .decrypt_padded_mut::<Pkcs7>(&mut buf)
                .map_err(|e| Error::new(format!("AES-CBC decrypt: {e}")))?,
        };
        Ok(pt.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256::hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha512_matches_known_vector() {
        assert_eq!(
            sha512::hex(b"abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn blake3_matches_known_vector() {
        assert_eq!(
            blake3::hex(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc_vector() {
        let key = [0x0bu8; 20];
        let mac = hmac::sha256_mac(&key, b"Hi There");
        assert_eq!(
            to_hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn constant_time_eq_matches_std_eq() {
        assert!(subtle::constant_time_eq(b"same", b"same"));
        assert!(!subtle::constant_time_eq(b"same", b"samd"));
        assert!(!subtle::constant_time_eq(b"same", b"longer"));
    }

    #[test]
    fn rand_fill_returns_non_constant_bytes() {
        let a = rand::bytes(32).unwrap();
        let b = rand::bytes(32).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn aes_gcm_round_trips() {
        let key = [0x42u8; 32];
        let nonce = [0x01u8; 12];
        let pt = b"hello, world";
        let aad = b"meta";
        let ct = aead::aes_256_gcm_seal(&key, &nonce, pt, aad).unwrap();
        let back = aead::aes_256_gcm_open(&key, &nonce, &ct, aad).unwrap();
        assert_eq!(back, pt);
        // Tampered ciphertext fails.
        let mut bad = ct.clone();
        bad[0] ^= 0x01;
        assert!(aead::aes_256_gcm_open(&key, &nonce, &bad, aad).is_err());
    }

    #[test]
    fn chacha20_poly1305_round_trips() {
        let key = [0x33u8; 32];
        let nonce = [0x07u8; 12];
        let ct = aead::chacha20_poly1305_seal(&key, &nonce, b"chacha", b"").unwrap();
        let back = aead::chacha20_poly1305_open(&key, &nonce, &ct, b"").unwrap();
        assert_eq!(back, b"chacha");
    }

    #[test]
    fn ed25519_round_trips() {
        let (secret, public) = ed25519::keypair().unwrap();
        let msg = b"vote count: 42";
        let sig = ed25519::sign(&secret, msg).unwrap();
        ed25519::verify(&public, msg, &sig).unwrap();
        assert!(ed25519::verify(&public, b"forged", &sig).is_err());
    }

    #[test]
    fn ecdsa_p256_round_trips() {
        let (secret_pem, public_pem) = ecdsa::keypair_pem().unwrap();
        let msg = b"merge after green ci";
        let sig = ecdsa::sign_pem(&secret_pem, msg).unwrap();
        ecdsa::verify_pem(&public_pem, msg, &sig).unwrap();
    }

    #[test]
    fn pbkdf2_sha256_known_vector() {
        let out = kdf::pbkdf2_sha256(b"password", b"salt", 1, 32);
        assert_eq!(
            to_hex(&out),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
    }

    #[test]
    fn argon2id_round_trips() {
        let phc = kdf::argon2id_hash(b"correct horse").unwrap();
        assert!(kdf::argon2id_verify(b"correct horse", &phc).unwrap());
        assert!(!kdf::argon2id_verify(b"wrong", &phc).unwrap());
    }

    #[test]
    fn aes_ctr_round_trip_128() {
        let key = [0x42u8; 16];
        let iv = [0x11u8; 16];
        let pt = b"the quick brown fox jumps over the lazy dog";
        let mut buf = pt.to_vec();
        cipher::aes_ctr_xor(&key, &iv, &mut buf).unwrap();
        assert_ne!(buf, pt);
        cipher::aes_ctr_xor(&key, &iv, &mut buf).unwrap();
        assert_eq!(buf, pt);
    }

    #[test]
    fn aes_ctr_round_trip_192_256() {
        for key_len in [24, 32] {
            let key = vec![0x42u8; key_len];
            let iv = [0x99u8; 16];
            let pt = b"another payload to test 192/256-bit AES-CTR".to_vec();
            let mut buf = pt.clone();
            cipher::aes_ctr_xor(&key, &iv, &mut buf).unwrap();
            cipher::aes_ctr_xor(&key, &iv, &mut buf).unwrap();
            assert_eq!(buf, pt);
        }
    }

    #[test]
    fn aes_cbc_round_trip() {
        let key = [0x33u8; 16];
        let iv = [0x55u8; 16];
        let pt = b"AES-CBC needs PKCS#7 padding";
        let ct = cipher::aes_cbc_encrypt(&key, &iv, pt).unwrap();
        assert_ne!(ct, pt);
        let back = cipher::aes_cbc_decrypt(&key, &iv, &ct).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn aes_cbc_block_aligned_payload() {
        let key = [0u8; 32];
        let iv = [1u8; 16];
        let pt = b"exactly sixteen!"; // 16 bytes - full block
        assert_eq!(pt.len(), 16);
        let ct = cipher::aes_cbc_encrypt(&key, &iv, pt).unwrap();
        // PKCS#7 adds a full padding block on aligned inputs.
        assert_eq!(ct.len(), 32);
        let back = cipher::aes_cbc_decrypt(&key, &iv, &ct).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn aes_ctr_rejects_bad_key_size() {
        let bad_key = [0u8; 17];
        let iv = [0u8; 16];
        let mut buf = [0u8; 8];
        assert!(cipher::aes_ctr_xor(&bad_key, &iv, &mut buf).is_err());
    }

    #[test]
    fn aes_ctr_rejects_bad_iv() {
        let key = [0u8; 16];
        let bad_iv = [0u8; 12];
        let mut buf = [0u8; 8];
        assert!(cipher::aes_ctr_xor(&key, &bad_iv, &mut buf).is_err());
    }

    #[test]
    fn aes_key_size_detection() {
        assert_eq!(
            cipher::aes_key_size(&[0u8; 16]).unwrap(),
            cipher::AesKeySize::K128
        );
        assert_eq!(
            cipher::aes_key_size(&[0u8; 24]).unwrap(),
            cipher::AesKeySize::K192
        );
        assert_eq!(
            cipher::aes_key_size(&[0u8; 32]).unwrap(),
            cipher::AesKeySize::K256
        );
        assert!(cipher::aes_key_size(&[0u8; 31]).is_err());
    }

    fn to_hex(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            out.push(nibble_char(byte >> 4));
            out.push(nibble_char(byte & 0xf));
        }
        out
    }
}
