#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Static manifest of every registered stdlib module.
//! Each stdlib milestone extends this table with
//! the modules it adds. Entries are listed in phase-introduction order
//! so a `gos doc` walk renders modules in the same sequence as the
//! implementation plan.

#![forbid(unsafe_code)]
use crate::registry::{StdItem, StdItemKind, StdModule};

use super::*;

pub const CRYPTO_RAND: StdModule = StdModule {
    path: "std::crypto::rand",
    summary: "Secure random bytes from the host CSPRNG.",
    items: &[StdItem {
        name: "bytes",
        kind: StdItemKind::Function,
        doc: "Returns a fresh random byte vector.",
    }],
};

pub const CRYPTO_SHA256: StdModule = StdModule {
    path: "std::crypto::sha256",
    summary: "SHA-256 hashing.",
    items: &[
        StdItem {
            name: "digest",
            kind: StdItemKind::Function,
            doc: "Returns the 32-byte digest of an input.",
        },
        StdItem {
            name: "hex",
            kind: StdItemKind::Function,
            doc: "Returns the digest as lowercase hex.",
        },
    ],
};

pub const CRYPTO_HMAC: StdModule = StdModule {
    path: "std::crypto::hmac",
    summary: "HMAC-SHA-256 keyed MACs.",
    items: &[
        StdItem {
            name: "sha256_mac",
            kind: StdItemKind::Function,
            doc: "HMAC-SHA-256 over a message.",
        },
        StdItem {
            name: "sha256_hex",
            kind: StdItemKind::Function,
            doc: "HMAC-SHA-256 over a message, hex-encoded.",
        },
        StdItem {
            name: "sha384_mac",
            kind: StdItemKind::Function,
            doc: "`sha384_mac(key, message) -> Vec<u8>`: the 48-byte HMAC-SHA-384 of a message. Check a tag with `verify_sha384`, which compares in constant time, not with `==`.",
        },
        StdItem {
            name: "sha384_hex",
            kind: StdItemKind::Function,
            doc: "`sha384_hex(key, message) -> String`: the HMAC-SHA-384 as lowercase hex.",
        },
        StdItem {
            name: "sha512_mac",
            kind: StdItemKind::Function,
            doc: "`sha512_mac(key, message) -> Vec<u8>`: the 64-byte HMAC-SHA-512 of a message. Check a tag with `verify_sha512`, which compares in constant time, not with `==`.",
        },
        StdItem {
            name: "sha512_hex",
            kind: StdItemKind::Function,
            doc: "`sha512_hex(key, message) -> String`: the HMAC-SHA-512 as lowercase hex.",
        },
        StdItem {
            name: "sha3_256_mac",
            kind: StdItemKind::Function,
            doc: "`sha3_256_mac(key, message) -> Vec<u8>`: the 32-byte HMAC-SHA3-256 of a message. Check a tag with `verify_sha3_256`, which compares in constant time, not with `==`.",
        },
        StdItem {
            name: "sha3_256_hex",
            kind: StdItemKind::Function,
            doc: "`sha3_256_hex(key, message) -> String`: the HMAC-SHA3-256 as lowercase hex.",
        },
        StdItem {
            name: "sha3_512_mac",
            kind: StdItemKind::Function,
            doc: "`sha3_512_mac(key, message) -> Vec<u8>`: the 64-byte HMAC-SHA3-512 of a message. Check a tag with `verify_sha3_512`, which compares in constant time, not with `==`.",
        },
        StdItem {
            name: "sha3_512_hex",
            kind: StdItemKind::Function,
            doc: "`sha3_512_hex(key, message) -> String`: the HMAC-SHA3-512 as lowercase hex.",
        },
        StdItem {
            name: "verify_sha256",
            kind: StdItemKind::Function,
            doc: "`verify_sha256(key, message, tag) -> bool`: whether `tag` is the HMAC of the message, compared in constant time so the comparison reveals nothing about a forged tag.",
        },
        StdItem {
            name: "verify_sha384",
            kind: StdItemKind::Function,
            doc: "`verify_sha384(key, message, tag) -> bool`: whether `tag` is the HMAC of the message, compared in constant time so the comparison reveals nothing about a forged tag.",
        },
        StdItem {
            name: "verify_sha512",
            kind: StdItemKind::Function,
            doc: "`verify_sha512(key, message, tag) -> bool`: whether `tag` is the HMAC of the message, compared in constant time so the comparison reveals nothing about a forged tag.",
        },
        StdItem {
            name: "verify_sha3_256",
            kind: StdItemKind::Function,
            doc: "`verify_sha3_256(key, message, tag) -> bool`: whether `tag` is the HMAC of the message, compared in constant time so the comparison reveals nothing about a forged tag.",
        },
        StdItem {
            name: "verify_sha3_512",
            kind: StdItemKind::Function,
            doc: "`verify_sha3_512(key, message, tag) -> bool`: whether `tag` is the HMAC of the message, compared in constant time so the comparison reveals nothing about a forged tag.",
        },
    ],
};

pub const CRYPTO_SUBTLE: StdModule = StdModule {
    path: "std::crypto::subtle",
    summary: "Constant-time comparison helpers.",
    items: &[StdItem {
        name: "constant_time_eq",
        kind: StdItemKind::Function,
        doc: "Compares two byte slices without data-dependent branches.",
    }],
};

pub const CRYPTO_SHA512: StdModule = StdModule {
    path: "std::crypto::sha512",
    summary: "SHA-512 hashing.",
    items: &[
        StdItem {
            name: "digest",
            kind: StdItemKind::Function,
            doc: "Returns the 64-byte digest of an input.",
        },
        StdItem {
            name: "hex",
            kind: StdItemKind::Function,
            doc: "Returns the digest as lowercase hex.",
        },
    ],
};

pub const CRYPTO_SHA384: StdModule = StdModule {
    path: "std::crypto::sha384",
    summary: "SHA-384 hashing (FIPS 180-4).",
    items: &[
        StdItem {
            name: "digest",
            kind: StdItemKind::Function,
            doc: "Returns the 48-byte SHA-384 digest of an input.",
        },
        StdItem {
            name: "hex",
            kind: StdItemKind::Function,
            doc: "Returns the SHA-384 digest as lowercase hex.",
        },
    ],
};

pub const CRYPTO_ECDH: StdModule = StdModule {
    path: "std::crypto::ecdh",
    summary: "Key agreement over X25519 (RFC 7748), P-256, and P-384 with static or ephemeral keys, answering a `SharedSecret` meant for HKDF.",
    items: &[
        StdItem {
            name: "x25519_keypair",
            kind: StdItemKind::Function,
            doc: "`x25519_keypair() -> Result<(Vec<u8>, Vec<u8>), errors::Error>`: a fresh X25519 keypair `(secret, public)`. Both are 32 bytes. Keep it for a static key or use it once for an ephemeral one.",
        },
        StdItem {
            name: "x25519",
            kind: StdItemKind::Function,
            doc: "`x25519(secret, peer_public) -> Result<SharedSecret, errors::Error>`: the X25519 shared secret with a peer. An all-zero result (a small-order peer key) is an `Err`. Derive keys from it with `hkdf_sha256` / `hkdf_sha384`.",
        },
        StdItem {
            name: "p256_keypair",
            kind: StdItemKind::Function,
            doc: "`p256_keypair() -> Result<(Vec<u8>, Vec<u8>), errors::Error>`: a fresh P-256 keypair `(secret, public)`. The secret is the scalar and the public key its uncompressed SEC1 point, the form a peer sends. Keep it for a static key or use it once for an ephemeral one.",
        },
        StdItem {
            name: "p256",
            kind: StdItemKind::Function,
            doc: "`p256(secret, peer_public) -> Result<SharedSecret, errors::Error>`: the P-256 shared secret with a peer. A peer key that is not a point on the curve is an `Err`. Derive keys from it with `hkdf_sha256` / `hkdf_sha384`.",
        },
        StdItem {
            name: "p384_keypair",
            kind: StdItemKind::Function,
            doc: "`p384_keypair() -> Result<(Vec<u8>, Vec<u8>), errors::Error>`: a fresh P-384 keypair `(secret, public)`. The secret is the scalar and the public key its uncompressed SEC1 point, the form a peer sends. Keep it for a static key or use it once for an ephemeral one.",
        },
        StdItem {
            name: "p384",
            kind: StdItemKind::Function,
            doc: "`p384(secret, peer_public) -> Result<SharedSecret, errors::Error>`: the P-384 shared secret with a peer. A peer key that is not a point on the curve is an `Err`. Derive keys from it with `hkdf_sha256` / `hkdf_sha384`.",
        },
        StdItem {
            name: "SharedSecret",
            kind: StdItemKind::Type,
            doc: "A secret a key agreement or an encapsulation produced: `hkdf_sha256(salt, info, length)?` and `hkdf_sha384(salt, info, length)?` derive keys from it, and `raw_bytes()` answers the bytes for a protocol that needs them. Printing one shows no bytes.",
        },
    ],
};

pub const CRYPTO_MLKEM: StdModule = StdModule {
    path: "std::crypto::mlkem",
    summary: "ML-KEM-768 and ML-KEM-1024 key encapsulation (FIPS 203), answering a `SharedSecret` meant for HKDF.",
    items: &[
        StdItem {
            name: "keypair_768",
            kind: StdItemKind::Function,
            doc: "`keypair_768() -> Result<(Vec<u8>, Vec<u8>), errors::Error>`: a fresh ML-KEM-768 keypair `(decapsulation key, encapsulation key)`; the decapsulation key is its 64-byte seed.",
        },
        StdItem {
            name: "encapsulate_768",
            kind: StdItemKind::Function,
            doc: "`encapsulate_768(encapsulation_key) -> Result<(Vec<u8>, SharedSecret), errors::Error>`: a shared secret for the key's holder and the ciphertext that carries it. Combine it with an X25519 secret through HKDF for a hybrid exchange.",
        },
        StdItem {
            name: "decapsulate_768",
            kind: StdItemKind::Function,
            doc: "`decapsulate_768(decapsulation_key, ciphertext) -> Result<SharedSecret, errors::Error>`: the secret the ciphertext carries. A tampered ciphertext answers an unrelated secret, not an error (FIPS 203 implicit rejection).",
        },
        StdItem {
            name: "keypair_1024",
            kind: StdItemKind::Function,
            doc: "`keypair_1024() -> Result<(Vec<u8>, Vec<u8>), errors::Error>`: a fresh ML-KEM-1024 keypair `(decapsulation key, encapsulation key)`; the decapsulation key is its 64-byte seed.",
        },
        StdItem {
            name: "encapsulate_1024",
            kind: StdItemKind::Function,
            doc: "`encapsulate_1024(encapsulation_key) -> Result<(Vec<u8>, SharedSecret), errors::Error>`: a shared secret for the key's holder and the ciphertext that carries it. Combine it with an X25519 secret through HKDF for a hybrid exchange.",
        },
        StdItem {
            name: "decapsulate_1024",
            kind: StdItemKind::Function,
            doc: "`decapsulate_1024(decapsulation_key, ciphertext) -> Result<SharedSecret, errors::Error>`: the secret the ciphertext carries. A tampered ciphertext answers an unrelated secret, not an error (FIPS 203 implicit rejection).",
        },
        StdItem {
            name: "SharedSecret",
            kind: StdItemKind::Type,
            doc: "A secret a key agreement or an encapsulation produced: `hkdf_sha256(salt, info, length)?` and `hkdf_sha384(salt, info, length)?` derive keys from it, and `raw_bytes()` answers the bytes for a protocol that needs them. Printing one shows no bytes.",
        },
    ],
};

pub const CRYPTO_SHA3_256: StdModule = StdModule {
    path: "std::crypto::sha3_256",
    summary: "SHA3-256 hashing (FIPS 202).",
    items: &[
        StdItem {
            name: "digest",
            kind: StdItemKind::Function,
            doc: "Returns the 32-byte SHA3-256 digest of an input.",
        },
        StdItem {
            name: "hex",
            kind: StdItemKind::Function,
            doc: "Returns the SHA3-256 digest as lowercase hex.",
        },
    ],
};

pub const CRYPTO_SHA3_512: StdModule = StdModule {
    path: "std::crypto::sha3_512",
    summary: "SHA3-512 hashing (FIPS 202).",
    items: &[
        StdItem {
            name: "digest",
            kind: StdItemKind::Function,
            doc: "Returns the 64-byte SHA3-512 digest of an input.",
        },
        StdItem {
            name: "hex",
            kind: StdItemKind::Function,
            doc: "Returns the SHA3-512 digest as lowercase hex.",
        },
    ],
};

pub const CRYPTO_RSA: StdModule = StdModule {
    path: "std::crypto::rsa",
    summary: "RSA signatures over SHA-256: PKCS#1 v1.5 (`RS256`) and PSS (`PS256`), with keys in PEM. Keys are imported, not generated.",
    items: &[
        StdItem {
            name: "sign_pkcs1_sha256",
            kind: StdItemKind::Function,
            doc: "Signs with RSASSA-PKCS1-v1_5 and SHA-256 (`RS256`) under a PKCS#8 or PKCS#1 PEM private key.",
        },
        StdItem {
            name: "encrypt_oaep_sha256",
            kind: StdItemKind::Function,
            doc: "Encrypts with RSAES-OAEP (SHA-256 and MGF1-SHA-256, empty label) under a `PUBLIC KEY` or `RSA PUBLIC KEY` PEM; the plaintext may be at most the modulus length less 66 bytes.",
        },
        StdItem {
            name: "decrypt_oaep_sha256",
            kind: StdItemKind::Function,
            doc: "Decrypts an RSAES-OAEP SHA-256 ciphertext under a PKCS#8 or PKCS#1 PEM private key, blinded so its timing does not depend on the key.",
        },
        StdItem {
            name: "sign_pss_sha256",
            kind: StdItemKind::Function,
            doc: "Signs with RSASSA-PSS, SHA-256, and MGF1-SHA-256 (`PS256`).",
        },
        StdItem {
            name: "verify_pkcs1_sha256",
            kind: StdItemKind::Function,
            doc: "Verifies an RSASSA-PKCS1-v1_5 SHA-256 signature under a `PUBLIC KEY` or `RSA PUBLIC KEY` PEM (2048 to 8192 bits).",
        },
        StdItem {
            name: "verify_pss_sha256",
            kind: StdItemKind::Function,
            doc: "Verifies an RSASSA-PSS SHA-256 signature (2048 to 8192 bits).",
        },
        StdItem {
            name: "public_key_pem",
            kind: StdItemKind::Function,
            doc: "The `PUBLIC KEY` (SubjectPublicKeyInfo) PEM of an RSA private key.",
        },
        StdItem {
            name: "generate",
            kind: StdItemKind::Function,
            doc: "`generate(bits: i64 = 3072) -> Result<(String, String), errors::Error>`: a fresh RSA keypair `(secret PKCS#8 PEM, public SPKI PEM)` with e = 65537. `bits` is 2048, 3072, or 4096. Sign with PSS (`sign_pss_sha256`) unless a peer needs PKCS#1 v1.5.",
        },
    ],
};

pub const CRYPTO_BLAKE3: StdModule = StdModule {
    path: "std::crypto::blake3",
    summary: "BLAKE3 hashing.",
    items: &[
        StdItem {
            name: "digest",
            kind: StdItemKind::Function,
            doc: "Returns the 32-byte BLAKE3 digest of an input.",
        },
        StdItem {
            name: "hex",
            kind: StdItemKind::Function,
            doc: "Returns the digest as lowercase hex.",
        },
    ],
};

pub const CRYPTO_AEAD: StdModule = StdModule {
    path: "std::crypto::aead",
    summary: "Authenticated encryption with associated data.",
    items: &[
        StdItem {
            name: "aes_256_gcm_seal",
            kind: StdItemKind::Function,
            doc: "AES-256-GCM seal: encrypts plaintext with key, nonce, and AAD.",
        },
        StdItem {
            name: "aes_256_gcm_open",
            kind: StdItemKind::Function,
            doc: "AES-256-GCM open: decrypts and authenticates ciphertext.",
        },
        StdItem {
            name: "chacha20_poly1305_seal",
            kind: StdItemKind::Function,
            doc: "ChaCha20-Poly1305 seal.",
        },
        StdItem {
            name: "chacha20_poly1305_open",
            kind: StdItemKind::Function,
            doc: "ChaCha20-Poly1305 open.",
        },
    ],
};

pub const CRYPTO_ED25519: StdModule = StdModule {
    path: "std::crypto::ed25519",
    summary: "Ed25519 digital signatures.",
    items: &[
        StdItem {
            name: "keypair",
            kind: StdItemKind::Function,
            doc: "Generates a fresh Ed25519 keypair from the host CSPRNG.",
        },
        StdItem {
            name: "sign",
            kind: StdItemKind::Function,
            doc: "Signs a message with a 32-byte secret key.",
        },
        StdItem {
            name: "verify",
            kind: StdItemKind::Function,
            doc: "Verifies a 64-byte signature against a 32-byte public key.",
        },
    ],
};

pub const CRYPTO_ECDSA: StdModule = StdModule {
    path: "std::crypto::ecdsa",
    summary: "ECDSA over the NIST P-256 curve.",
    items: &[
        StdItem {
            name: "keypair_pem",
            kind: StdItemKind::Function,
            doc: "Generates (secret_pem, public_pem) for a fresh P-256 keypair.",
        },
        StdItem {
            name: "sign_pem",
            kind: StdItemKind::Function,
            doc: "Signs a message with a PKCS#8-PEM-encoded P-256 secret key.",
        },
        StdItem {
            name: "verify_pem",
            kind: StdItemKind::Function,
            doc: "Verifies a DER-encoded signature against an SPKI-PEM public key.",
        },
        StdItem {
            name: "sign_fixed_pem",
            kind: StdItemKind::Function,
            doc: "`sign_fixed_pem(secret_pem, message) -> Result<Vec<u8>, errors::Error>`: a P-256 signature in the fixed `r || s` form JWT (ES256) and WebAuthn use.",
        },
        StdItem {
            name: "verify_fixed_pem",
            kind: StdItemKind::Function,
            doc: "`verify_fixed_pem(public_pem, message, signature) -> Result<(), errors::Error>`: `Ok` when the fixed-form P-256 signature verifies.",
        },
        StdItem {
            name: "der_to_fixed",
            kind: StdItemKind::Function,
            doc: "`der_to_fixed(signature) -> Result<Vec<u8>, errors::Error>`: a DER P-256 signature as `r || s`.",
        },
        StdItem {
            name: "fixed_to_der",
            kind: StdItemKind::Function,
            doc: "`fixed_to_der(signature) -> Result<Vec<u8>, errors::Error>`: an `r || s` P-256 signature as DER.",
        },
        StdItem {
            name: "p384_keypair_pem",
            kind: StdItemKind::Function,
            doc: "`p384_keypair_pem() -> Result<(String, String), errors::Error>`: a fresh P-384 keypair `(secret PKCS#8 PEM, public SPKI PEM)`.",
        },
        StdItem {
            name: "p384_sign_pem",
            kind: StdItemKind::Function,
            doc: "`p384_sign_pem(secret_pem, message) -> Result<Vec<u8>, errors::Error>`: a DER ECDSA P-384 / SHA-384 signature with RFC 6979 deterministic nonces.",
        },
        StdItem {
            name: "p384_verify_pem",
            kind: StdItemKind::Function,
            doc: "`p384_verify_pem(public_pem, message, signature) -> Result<(), errors::Error>`: `Ok` when the DER signature verifies.",
        },
        StdItem {
            name: "p384_sign_fixed_pem",
            kind: StdItemKind::Function,
            doc: "`p384_sign_fixed_pem(secret_pem, message) -> Result<Vec<u8>, errors::Error>`: a P-384 signature in the fixed `r || s` form JWT (ES384) and WebAuthn use.",
        },
        StdItem {
            name: "p384_verify_fixed_pem",
            kind: StdItemKind::Function,
            doc: "`p384_verify_fixed_pem(public_pem, message, signature) -> Result<(), errors::Error>`: `Ok` when the fixed-form P-384 signature verifies.",
        },
        StdItem {
            name: "p384_der_to_fixed",
            kind: StdItemKind::Function,
            doc: "`p384_der_to_fixed(signature) -> Result<Vec<u8>, errors::Error>`: a DER P-384 signature as `r || s`.",
        },
        StdItem {
            name: "p384_fixed_to_der",
            kind: StdItemKind::Function,
            doc: "`p384_fixed_to_der(signature) -> Result<Vec<u8>, errors::Error>`: an `r || s` P-384 signature as DER.",
        },
    ],
};

pub const CRYPTO_X509: StdModule = StdModule {
    path: "std::crypto::x509",
    summary: "X.509 certificate parsing.",
    items: &[
        StdItem {
            name: "CertInfo",
            kind: StdItemKind::Type,
            doc: "Inspected fields of an X.509 certificate.",
        },
        StdItem {
            name: "self_signed",
            kind: StdItemKind::Function,
            doc: "A self-signed certificate and its fresh P-256 key as `(cert_pem, key_pem)`: a CA with `is_ca: true`, else a TLS server (or with `client: true`, client) certificate for `dns_names`, valid for `days`.",
        },
        StdItem {
            name: "issue",
            kind: StdItemKind::Function,
            doc: "A certificate a CA signs, with its fresh P-256 key, as `(cert_pem, key_pem)`.",
        },
        StdItem {
            name: "csr",
            kind: StdItemKind::Function,
            doc: "A certificate signing request and its fresh P-256 key, as `(csr_pem, key_pem)`.",
        },
        StdItem {
            name: "sign_csr",
            kind: StdItemKind::Function,
            doc: "The certificate a CA signs for a signing request, after checking the request's signature; it keeps the request's names and key.",
        },
        StdItem {
            name: "parse_pem",
            kind: StdItemKind::Function,
            doc: "Parses one PEM-encoded certificate.",
        },
        StdItem {
            name: "verify_server_certificate_with_crls",
            kind: StdItemKind::Function,
            doc: "Fail-closed TLS server-chain, hostname, and CRL verification.",
        },
        StdItem {
            name: "Extension",
            kind: StdItemKind::Type,
            doc: "A certificate extension: `oid` (dotted decimal), `critical`, and `value`, its DER bytes. `x509::extensions(cert_pem)` reads them in certificate order; `self_signed` and `issue` take `extensions:` to write custom ones, each checked: a well-formed OID, a value of exactly one DER element, and no OID twice in a certificate (one the generator writes, such as the subject alternative names, included).",
        },
        StdItem {
            name: "extensions",
            kind: StdItemKind::Function,
            doc: "`extensions(cert_pem) -> Result<Vec<Extension>, errors::Error>`: the certificate's extensions, standard and custom, in order.",
        },
        StdItem {
            name: "private_key_pkcs8",
            kind: StdItemKind::Function,
            doc: "`private_key_pkcs8(pem) -> Result<String, errors::Error>`: the key as a PKCS#8 `PRIVATE KEY` PEM, read from PKCS#8 or from the legacy forms OpenSSL writes: PKCS#1 `RSA PRIVATE KEY` and SEC1 `EC PRIVATE KEY` (P-256, P-384).",
        },
        StdItem {
            name: "private_key_der",
            kind: StdItemKind::Function,
            doc: "`private_key_der(pem) -> Result<Vec<u8>, errors::Error>`: the same key as PKCS#8 DER.",
        },
        StdItem {
            name: "decrypt_private_key",
            kind: StdItemKind::Function,
            doc: "`decrypt_private_key(pem, password) -> Result<String, errors::Error>`: the PKCS#8 PEM inside an `ENCRYPTED PRIVATE KEY`. Only PBES2 opens (PBKDF2-HMAC-SHA256 or scrypt, with AES-128-CBC or AES-256-CBC); a PBES1 key (DES, RC2, MD5) is refused with an error naming its scheme.",
        },
        StdItem {
            name: "public_key_spki",
            kind: StdItemKind::Function,
            doc: "`public_key_spki(pem) -> Result<String, errors::Error>`: the key as an SPKI `PUBLIC KEY` PEM, read from SPKI, PKCS#1 `RSA PUBLIC KEY`, or a `CERTIFICATE`.",
        },
        StdItem {
            name: "public_key_der",
            kind: StdItemKind::Function,
            doc: "`public_key_der(pem) -> Result<Vec<u8>, errors::Error>`: the same key as SPKI DER.",
        },
    ],
};

pub const CRYPTO_KDF: StdModule = StdModule {
    path: "std::crypto::kdf",
    summary: "Password-based key-derivation functions.",
    items: &[
        StdItem {
            name: "hkdf_sha256",
            kind: StdItemKind::Function,
            doc: "HKDF-SHA256 (RFC 5869): `length` bytes derived from input key material, a salt, and context info; at most 8160 bytes.",
        },
        StdItem {
            name: "hkdf_sha512",
            kind: StdItemKind::Function,
            doc: "HKDF-SHA512 (RFC 5869); at most 16320 bytes.",
        },
        StdItem {
            name: "pbkdf2_sha256",
            kind: StdItemKind::Function,
            doc: "PBKDF2-HMAC-SHA256 KDF.",
        },
        StdItem {
            name: "scrypt_interactive",
            kind: StdItemKind::Function,
            doc: "scrypt with the standard interactive parameters.",
        },
        StdItem {
            name: "argon2id_hash",
            kind: StdItemKind::Function,
            doc: "Argon2id PHC-format password hash.",
        },
        StdItem {
            name: "argon2id_verify",
            kind: StdItemKind::Function,
            doc: "Verifies a password against an Argon2id PHC string.",
        },
        StdItem {
            name: "hkdf_sha384",
            kind: StdItemKind::Function,
            doc: "`hkdf_sha384(ikm, salt, info, length) -> Result<Vec<u8>, errors::Error>`: `length` bytes of key material derived with HKDF-SHA384 (RFC 5869).",
        },
    ],
};

pub const HASH_FNV: StdModule = StdModule {
    path: "std::hash::fnv",
    summary: "FNV-1a non-cryptographic hash (32-bit, 64-bit).",
    items: &[
        StdItem {
            name: "hash32",
            kind: StdItemKind::Function,
            doc: "32-bit FNV-1a of a byte slice.",
        },
        StdItem {
            name: "hash64",
            kind: StdItemKind::Function,
            doc: "64-bit FNV-1a of a byte slice.",
        },
        StdItem {
            name: "hash_string",
            kind: StdItemKind::Function,
            doc: "64-bit FNV-1a of a String.",
        },
    ],
};

pub const HASH_CRC32: StdModule = StdModule {
    path: "std::hash::crc32",
    summary: "CRC-32 (IEEE) checksums.",
    items: &[
        StdItem {
            name: "checksum",
            kind: StdItemKind::Function,
            doc: "CRC-32 checksum of a byte slice.",
        },
        StdItem {
            name: "checksum_string",
            kind: StdItemKind::Function,
            doc: "CRC-32 checksum of a String.",
        },
        StdItem {
            name: "update",
            kind: StdItemKind::Function,
            doc: "Continues a CRC-32 from a running value over more bytes.",
        },
        StdItem {
            name: "update_window",
            kind: StdItemKind::Function,
            doc: "Continues a CRC-32 over `data[start..end]`, checking a record \
                  where it lies rather than copying the window out first.",
        },
    ],
};

pub const HASH_CRC32C: StdModule = StdModule {
    path: "std::hash::crc32c",
    summary: "CRC-32C (Castagnoli) checksums, computed with the CPU's CRC instruction where there is one.",
    items: &[
        StdItem {
            name: "checksum",
            kind: StdItemKind::Function,
            doc: "CRC-32C checksum of a byte slice.",
        },
        StdItem {
            name: "checksum_string",
            kind: StdItemKind::Function,
            doc: "CRC-32C checksum of a String.",
        },
        StdItem {
            name: "update",
            kind: StdItemKind::Function,
            doc: "Continues a CRC-32C from a running value over more bytes.",
        },
        StdItem {
            name: "update_window",
            kind: StdItemKind::Function,
            doc: "Continues a CRC-32C over `data[start..end]`, checking a record \
                  where it lies rather than copying the window out first.",
        },
    ],
};

pub const HASH_ADLER32: StdModule = StdModule {
    path: "std::hash::adler32",
    summary: "Adler-32 checksums.",
    items: &[
        StdItem {
            name: "checksum",
            kind: StdItemKind::Function,
            doc: "Adler-32 checksum of a byte slice.",
        },
        StdItem {
            name: "checksum_string",
            kind: StdItemKind::Function,
            doc: "Adler-32 checksum of a String.",
        },
        StdItem {
            name: "update",
            kind: StdItemKind::Function,
            doc: "Continues an Adler-32 from a running value over more bytes.",
        },
    ],
};

pub const CRYPTO_INSECURE: StdModule = StdModule {
    path: "std::crypto::insecure",
    summary: "Legacy / broken hashes (MD5, SHA-1). Compat only - never use for new code.",
    items: &[
        StdItem {
            name: "md5",
            kind: StdItemKind::Function,
            doc: "One-shot MD5.",
        },
        StdItem {
            name: "sha1",
            kind: StdItemKind::Function,
            doc: "One-shot SHA-1.",
        },
        StdItem {
            name: "md5_hex",
            kind: StdItemKind::Function,
            doc: "One-shot MD5, hex-encoded.",
        },
        StdItem {
            name: "sha1_hex",
            kind: StdItemKind::Function,
            doc: "One-shot SHA-1, hex-encoded.",
        },
        StdItem {
            name: "hmac_sha1",
            kind: StdItemKind::Function,
            doc: "`hmac_sha1(key, message) -> Vec<u8>`: HMAC-SHA1, for TOTP (RFC 6238) and older APIs that require it. Check a tag with `verify_hmac_sha1`.",
        },
        StdItem {
            name: "hmac_sha1_hex",
            kind: StdItemKind::Function,
            doc: "`hmac_sha1_hex(key, message) -> String`: HMAC-SHA1 as lowercase hex.",
        },
        StdItem {
            name: "verify_hmac_sha1",
            kind: StdItemKind::Function,
            doc: "`verify_hmac_sha1(key, message, tag) -> bool`: whether `tag` is the HMAC-SHA1 of the message, compared in constant time.",
        },
    ],
};

pub const CRYPTO_PASSWORD: StdModule = StdModule {
    path: "std::crypto::password",
    summary: "Argon2id password hashing facade: PHC-string hash / verify / re-hash policy.",
    items: &[
        StdItem {
            name: "hash",
            kind: StdItemKind::Function,
            doc: "Argon2id hash of plaintext; returns a PHC-format string for storage.",
        },
        StdItem {
            name: "verify",
            kind: StdItemKind::Function,
            doc: "Constant-time verify of plaintext against a stored PHC string.",
        },
        StdItem {
            name: "needs_rehash",
            kind: StdItemKind::Function,
            doc: "True iff the stored PHC's parameters are below the current defaults.",
        },
    ],
};
