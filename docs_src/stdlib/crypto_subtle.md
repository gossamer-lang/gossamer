# `std::crypto::subtle`

Status: experimental

Constant-time comparison helpers.

## Items

| Item | Signature | Description |
|---|---|---|
| `constant_time_eq` | `fn constant_time_eq(a: Vec<u8>, b: Vec<u8>) -> bool` | Compares two byte slices without data-dependent branches. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
