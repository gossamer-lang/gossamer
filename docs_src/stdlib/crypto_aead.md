# `std::crypto::aead`

Status: experimental

Authenticated encryption with associated data.

## Items

| Item | Signature | Description |
|---|---|---|
| `aes_256_gcm_seal` | `fn aes_256_gcm_seal(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | AES-256-GCM seal: encrypts plaintext with key, nonce, and AAD. |
| `aes_256_gcm_open` | `fn aes_256_gcm_open(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | AES-256-GCM open: decrypts and authenticates ciphertext. |
| `chacha20_poly1305_seal` | `fn chacha20_poly1305_seal(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | ChaCha20-Poly1305 seal. |
| `chacha20_poly1305_open` | `fn chacha20_poly1305_open(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | ChaCha20-Poly1305 open. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
