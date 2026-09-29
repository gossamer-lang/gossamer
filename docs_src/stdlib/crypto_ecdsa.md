# `std::crypto::ecdsa`

Status: experimental

ECDSA over the NIST P-256 curve.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`keypair_pem`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn keypair_pem() -> Result<(String, String), errors::Error>` | Generates (secret_pem, public_pem) for a fresh P-256 keypair. |
| [`sign_pem`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn sign_pem(secret_pem: String, message: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | Signs a message with a PKCS#8-PEM-encoded P-256 secret key. |
| [`verify_pem`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn verify_pem(public_pem: String, message: Vec<u8>, signature: Vec<u8>) -> Result<(), errors::Error>` | Verifies a DER-encoded signature against an SPKI-PEM public key. |
