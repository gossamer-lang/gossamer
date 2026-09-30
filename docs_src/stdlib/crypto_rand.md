# `std::crypto::rand`

Status: experimental

Secure random bytes from the host CSPRNG.

## Items

| Item | Signature | Description |
|---|---|---|
| `bytes` | `fn bytes(n: i64) -> Result<Vec<u8>, errors::Error>` | Returns a fresh random byte vector. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
