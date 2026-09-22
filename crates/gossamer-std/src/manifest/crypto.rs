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
            name: "parse_pem",
            kind: StdItemKind::Function,
            doc: "Parses one PEM-encoded certificate.",
        },
        StdItem {
            name: "verify_server_certificate_with_crls",
            kind: StdItemKind::Function,
            doc: "Fail-closed TLS server-chain, hostname, and CRL verification.",
        },
    ],
};

pub const CRYPTO_KDF: StdModule = StdModule {
    path: "std::crypto::kdf",
    summary: "Password-based key-derivation functions.",
    items: &[
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
