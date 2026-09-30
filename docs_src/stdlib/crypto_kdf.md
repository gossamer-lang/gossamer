# `std::crypto::kdf`

Status: experimental

Password-based key-derivation functions.

## Items

| Item | Signature | Description |
|---|---|---|
| `pbkdf2_sha256` | `fn pbkdf2_sha256(password: Vec<u8>, salt: Vec<u8>, iterations: i64, length: i64) -> Vec<u8>` | PBKDF2-HMAC-SHA256 KDF. |
| `scrypt_interactive` | `fn scrypt_interactive(password: Vec<u8>, salt: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | scrypt with the standard interactive parameters. |
| `argon2id_hash` | `fn argon2id_hash(password: Vec<u8>) -> Result<String, errors::Error>` | Argon2id PHC-format password hash. |
| `argon2id_verify` | `fn argon2id_verify(password: Vec<u8>, phc: String) -> Result<bool, errors::Error>` | Verifies a password against an Argon2id PHC string. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
