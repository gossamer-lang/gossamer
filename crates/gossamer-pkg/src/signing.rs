//! Ed25519 signing primitives used by `gos publish`.
//!
//! Keys live at `~/.gossamer/keys/<id>.ed25519` (32 bytes of raw
//! secret material) or in `$GOS_PUBLISH_KEY` (hex-encoded). The
//! published artifact's sha256 is the message signed; the signature
//! and the matching public key are uploaded alongside the tarball so
//! the registry can verify the publish against the project's known
//! owner set.

#![forbid(unsafe_code)]
#![allow(clippy::must_use_candidate, clippy::manual_is_multiple_of)]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey as DalekSigningKey, Verifier};

/// Errors raised by the signing helpers.
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    /// I/O error reading or writing a key file.
    #[error("io: {0}")]
    Io(String),
    /// Key material had the wrong length (expected 32 bytes raw or
    /// 64 hex characters).
    #[error("malformed key: {0}")]
    Malformed(String),
    /// Verification rejected the signature.
    #[error("signature verification failed")]
    BadSignature,
    /// `$GOS_PUBLISH_KEY` and `~/.gossamer/keys/<id>.ed25519` are
    /// both missing.
    #[error("no signing key available for {0}: run `gos keygen {0}`, or set $GOS_PUBLISH_KEY")]
    Missing(String),
    /// `gos keygen` found a key already in place, which it never replaces:
    /// every consumer that trusts the old public key would reject packages
    /// signed by a new one.
    #[error("a signing key for {0} already exists at {1}")]
    Exists(String, String),
}

/// Owned signing key handle.
#[derive(Debug, Clone)]
pub struct SigningKey {
    inner: DalekSigningKey,
}

/// Owned verifying (public) key handle.
#[derive(Debug, Clone)]
pub struct VerifyingKey {
    inner: ed25519_dalek::VerifyingKey,
}

impl SigningKey {
    /// A fresh key from the operating system's random source.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            inner: DalekSigningKey::generate(&mut rand_core::OsRng),
        }
    }

    /// Builds a signing key from 32 bytes of raw secret material.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self {
            inner: DalekSigningKey::from_bytes(&bytes),
        }
    }

    /// Builds a signing key from a hex string (64 lowercase or
    /// uppercase hex chars).
    pub fn from_hex(text: &str) -> Result<Self, SigningError> {
        let bytes = hex_decode(text.trim())?;
        if bytes.len() != 32 {
            return Err(SigningError::Malformed(format!(
                "expected 32-byte ed25519 secret key, got {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self::from_bytes(arr))
    }

    /// Reads a signing key from `path`. The file may hold either a
    /// raw 32-byte payload or a hex string.
    pub fn from_path(path: &Path) -> Result<Self, SigningError> {
        let bytes = fs::read(path).map_err(|e| SigningError::Io(e.to_string()))?;
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            return Ok(Self::from_bytes(arr));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| SigningError::Malformed(format!("key file not utf-8: {e}")))?;
        Self::from_hex(text)
    }

    /// Returns the raw 32-byte secret.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 32] {
        self.inner.to_bytes()
    }

    /// Returns the matching verifying key.
    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey {
            inner: self.inner.verifying_key(),
        }
    }

    /// Signs `message` with this key.
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        let sig: Signature = self.inner.sign(message);
        sig.to_bytes()
    }
}

impl VerifyingKey {
    /// Builds a verifying key from 32 bytes of public material.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, SigningError> {
        ed25519_dalek::VerifyingKey::from_bytes(&bytes)
            .map(|inner| Self { inner })
            .map_err(|e| SigningError::Malformed(format!("verifying key: {e}")))
    }

    /// Builds a verifying key from a hex string.
    pub fn from_hex(text: &str) -> Result<Self, SigningError> {
        let bytes = hex_decode(text.trim())?;
        if bytes.len() != 32 {
            return Err(SigningError::Malformed(format!(
                "expected 32-byte ed25519 public key, got {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Self::from_bytes(arr)
    }

    /// Returns the raw 32-byte public key.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 32] {
        self.inner.to_bytes()
    }

    /// Returns the hex form (64 lowercase chars).
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex_encode(&self.to_bytes())
    }

    /// Verifies `signature_bytes` against `message`.
    pub fn verify(&self, message: &[u8], signature_bytes: &[u8]) -> Result<(), SigningError> {
        if signature_bytes.len() != 64 {
            return Err(SigningError::Malformed(format!(
                "expected 64-byte ed25519 signature, got {}",
                signature_bytes.len()
            )));
        }
        let mut arr = [0u8; 64];
        arr.copy_from_slice(signature_bytes);
        let sig = Signature::from_bytes(&arr);
        self.inner
            .verify(message, &sig)
            .map_err(|_| SigningError::BadSignature)
    }

    /// Verifies a normal Ed25519 signature while reading `reader` in bounded
    /// chunks. This is intentionally not Ed25519ph: registry signatures keep
    /// their existing wire format and verify the exact tarball bytes.
    pub fn verify_reader<R: Read>(
        &self,
        reader: &mut R,
        signature_bytes: &[u8],
    ) -> Result<(), SigningError> {
        if signature_bytes.len() != 64 {
            return Err(SigningError::Malformed(format!(
                "expected 64-byte ed25519 signature, got {}",
                signature_bytes.len()
            )));
        }
        let mut arr = [0u8; 64];
        arr.copy_from_slice(signature_bytes);
        let signature = Signature::from_bytes(&arr);
        let mut verifier = self
            .inner
            .verify_stream(&signature)
            .map_err(|_| SigningError::BadSignature)?;
        let mut buffer = [0u8; 8192];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| SigningError::Io(error.to_string()))?;
            if read == 0 {
                break;
            }
            verifier.update(&buffer[..read]);
        }
        verifier
            .finalize_and_verify()
            .map_err(|_| SigningError::BadSignature)
    }
}

/// Locates the project's signing key, in order:
/// 1. `$GOS_PUBLISH_KEY` (hex-encoded).
/// 2. `~/.gossamer/keys/<id>.ed25519` (raw bytes or hex).
pub fn load_publish_key(project_id: &str) -> Result<SigningKey, SigningError> {
    if let Ok(hex) = std::env::var("GOS_PUBLISH_KEY") {
        return SigningKey::from_hex(&hex);
    }
    let path = key_path(project_id)?;
    if path.is_file() {
        return SigningKey::from_path(&path);
    }
    Err(SigningError::Missing(project_id.to_string()))
}

/// Writes `key` as `project_id`'s publish key, readable by the current user
/// only, and answers where. An existing key is never replaced.
pub fn save_publish_key(project_id: &str, key: &SigningKey) -> Result<PathBuf, SigningError> {
    let path = key_path(project_id)?;
    if path.exists() {
        return Err(SigningError::Exists(
            project_id.to_string(),
            path.display().to_string(),
        ));
    }
    crate::credentials::write_private_file(&path, hex_encode(&key.to_bytes()).as_bytes())
        .map_err(|e| SigningError::Io(e.to_string()))?;
    Ok(path)
}

/// Returns the canonical key path for `project_id` (replaces `/`
/// with `__` so a single directory can hold per-id keys).
pub fn key_path(project_id: &str) -> Result<PathBuf, SigningError> {
    let home = std::env::var("HOME")
        .map_err(|_| SigningError::Io("HOME not set; cannot locate key store".to_string()))?;
    Ok(PathBuf::from(home)
        .join(".gossamer")
        .join("keys")
        .join(format!("{}.ed25519", project_id.replace('/', "__"))))
}

/// Signs `bytes` with `key`. Convenience wrapper.
#[must_use]
pub fn sign_bytes(key: &SigningKey, bytes: &[u8]) -> [u8; 64] {
    key.sign(bytes)
}

/// Verifies `signature_bytes` against `bytes` using `key`. Convenience
/// wrapper.
pub fn verify_bytes(
    key: &VerifyingKey,
    bytes: &[u8],
    signature_bytes: &[u8],
) -> Result<(), SigningError> {
    key.verify(bytes, signature_bytes)
}

/// Verifies a hex-encoded ed25519 signature over `message` against a
/// hex-encoded public key. Used by the package fetcher to authenticate
/// a registry tarball before it is unpacked.
pub fn verify_signature_hex(
    public_key_hex: &str,
    message: &[u8],
    signature_hex: &str,
) -> Result<(), SigningError> {
    let key = VerifyingKey::from_hex(public_key_hex)?;
    let signature = hex_decode(signature_hex.trim())?;
    key.verify(message, &signature)
}

/// Reader-based counterpart to [`verify_signature_hex`].
pub fn verify_signature_hex_reader<R: Read>(
    public_key_hex: &str,
    reader: &mut R,
    signature_hex: &str,
) -> Result<(), SigningError> {
    let key = VerifyingKey::from_hex(public_key_hex)?;
    let signature = hex_decode(signature_hex.trim())?;
    key.verify_reader(reader, &signature)
}

fn hex_decode(text: &str) -> Result<Vec<u8>, SigningError> {
    let bytes = text.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(SigningError::Malformed("hex length must be even".into()));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8, SigningError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(SigningError::Malformed(format!(
            "invalid hex char {:?}",
            char::from(c)
        ))),
    }
}

/// Returns the lowercase-hex form of `bytes`. Used by [`VerifyingKey::to_hex`]
/// and the publish flow's wire format.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    const CHARS: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(CHARS[(b >> 4) as usize]));
        out.push(char::from(CHARS[(b & 0xf) as usize]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    #[test]
    fn sign_and_verify_roundtrip() {
        let mut secret = [0u8; 32];
        rand_core::RngCore::fill_bytes(&mut OsRng, &mut secret);
        let key = SigningKey::from_bytes(secret);
        let vk = key.verifying_key();
        let msg = b"the rain in spain";
        let sig = sign_bytes(&key, msg);
        verify_bytes(&vk, msg, &sig).expect("verify");
        let tampered = b"the rain in fred";
        assert!(verify_bytes(&vk, tampered, &sig).is_err());
    }

    #[test]
    fn stream_verifier_accepts_chunked_standard_ed25519_message() {
        let mut secret = [0u8; 32];
        rand_core::RngCore::fill_bytes(&mut OsRng, &mut secret);
        let key = SigningKey::from_bytes(secret);
        let verifying = key.verifying_key();
        let message = vec![0x5au8; 32 * 1024 + 17];
        let signature = sign_bytes(&key, &message);
        let mut reader = std::io::Cursor::new(message.clone());
        verifying
            .verify_reader(&mut reader, &signature)
            .expect("stream verifier must accept the ordinary Ed25519 signature");

        let mut tampered = message;
        tampered[8192] ^= 1;
        let mut reader = std::io::Cursor::new(tampered);
        assert!(verifying.verify_reader(&mut reader, &signature).is_err());
    }

    #[test]
    fn hex_roundtrip() {
        let raw = [1u8, 2, 0xab, 0xcd];
        let h = hex_encode(&raw);
        assert_eq!(h, "0102abcd");
        let back = hex_decode(&h).unwrap();
        assert_eq!(back, raw);
    }
}
