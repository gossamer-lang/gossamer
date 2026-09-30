# `std::crypto::sha256`

Status: experimental

SHA-256 hashing.

## Items

| Item | Signature | Description |
|---|---|---|
| `digest` | `fn digest(data: Vec<u8>) -> Vec<u8>` | Returns the 32-byte digest of an input. |
| `hex` | `fn hex(data: Vec<u8>) -> String` | Returns the digest as lowercase hex. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
