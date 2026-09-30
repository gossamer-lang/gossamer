# `std::hash::fnv`

Status: experimental

FNV-1a non-cryptographic hash (32-bit, 64-bit).

## Items

| Item | Signature | Description |
|---|---|---|
| `hash32` | `fn hash32(data: Vec<u8>) -> u32` | 32-bit FNV-1a of a byte slice. |
| `hash64` | `fn hash64(data: Vec<u8>) -> u64` | 64-bit FNV-1a of a byte slice. |
| `hash_string` | `fn hash_string(text: String) -> u64` | 64-bit FNV-1a of a String. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
