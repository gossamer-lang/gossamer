# `std::crypto::insecure`

Status: experimental

Legacy / broken hashes (MD5, SHA-1). Compat only - never use for new code.

## Items

| Item | Signature | Description |
|---|---|---|
| `md5` | `fn md5(data: Vec<u8>) -> Vec<u8>` | One-shot MD5. |
| `sha1` | `fn sha1(data: Vec<u8>) -> Vec<u8>` | One-shot SHA-1. |
| `md5_hex` | `fn md5_hex(data: Vec<u8>) -> String` | One-shot MD5, hex-encoded. |
| `sha1_hex` | `fn sha1_hex(data: Vec<u8>) -> String` | One-shot SHA-1, hex-encoded. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
