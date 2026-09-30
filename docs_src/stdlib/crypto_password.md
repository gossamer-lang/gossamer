# `std::crypto::password`

Status: experimental

Argon2id password hashing facade: PHC-string hash / verify / re-hash policy.

## Items

| Item | Signature | Description |
|---|---|---|
| `hash` | `fn hash(password: String) -> Result<String, errors::Error>` | Argon2id hash of plaintext; returns a PHC-format string for storage. |
| `verify` | `fn verify(password: String, hash: String) -> Result<bool, errors::Error>` | Constant-time verify of plaintext against a stored PHC string. |
| `needs_rehash` | `fn needs_rehash(hash: String) -> bool` | True iff the stored PHC's parameters are below the current defaults. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
