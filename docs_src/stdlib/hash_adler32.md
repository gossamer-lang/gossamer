# `std::hash::adler32`

Status: experimental

Adler-32 checksums.

## Items

| Item | Signature | Description |
|---|---|---|
| `checksum` | `fn checksum(data: Vec<u8>) -> u32` | Adler-32 checksum of a byte slice. |
| `checksum_string` | `fn checksum_string(text: String) -> u32` | Adler-32 checksum of a String. |
| `update` | `fn update(seed: u32, data: Vec<u8>) -> u32` | Continues an Adler-32 from a running value over more bytes. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
