# `std::crypto::kdf`

Status: experimental

Password-based key-derivation functions.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`argon2id_hash`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn argon2id_hash(password: Vec<u8>) -> Result<String, errors::Error>` | Argon2id PHC-format password hash. |
| [`argon2id_verify`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn argon2id_verify(password: Vec<u8>, phc: String) -> Result<bool, errors::Error>` | Verifies a password against an Argon2id PHC string. |
| [`pbkdf2_sha256`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn pbkdf2_sha256(password: Vec<u8>, salt: Vec<u8>, iterations: i64, length: i64) -> Vec<u8>` | PBKDF2-HMAC-SHA256 KDF. |
| [`scrypt_interactive`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn scrypt_interactive(password: Vec<u8>, salt: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | scrypt with the standard interactive parameters. |
