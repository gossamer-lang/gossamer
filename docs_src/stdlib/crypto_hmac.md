# `std::crypto::hmac`

Status: experimental

HMAC-SHA-256 keyed MACs.

## Items

| Item | Signature | Description |
|---|---|---|
| `sha256_mac` | `fn sha256_mac(key: Vec<u8>, message: Vec<u8>) -> Vec<u8>` | HMAC-SHA-256 over a message. |
| `sha256_hex` | `fn sha256_hex(key: Vec<u8>, message: Vec<u8>) -> String` | HMAC-SHA-256 over a message, hex-encoded. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
