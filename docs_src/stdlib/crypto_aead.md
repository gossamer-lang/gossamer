# `std::crypto::aead`

Status: experimental

Authenticated encryption with associated data.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`aes_256_gcm_open`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn aes_256_gcm_open(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | AES-256-GCM open: decrypts and authenticates ciphertext. |
| [`aes_256_gcm_seal`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn aes_256_gcm_seal(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | AES-256-GCM seal: encrypts plaintext with key, nonce, and AAD. |
| [`chacha20_poly1305_open`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn chacha20_poly1305_open(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | ChaCha20-Poly1305 open. |
| [`chacha20_poly1305_seal`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn chacha20_poly1305_seal(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | ChaCha20-Poly1305 seal. |
