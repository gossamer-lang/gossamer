# `std::hash::crc32`

Status: experimental

CRC-32 (IEEE) checksums.

## Items

| Item | Signature | Description |
|---|---|---|
| `checksum` | `fn checksum(data: Vec<u8>) -> u32` | CRC-32 checksum of a byte slice. |
| `checksum_string` | `fn checksum_string(text: String) -> u32` | CRC-32 checksum of a String. |
| `update` | `fn update(seed: u32, data: Vec<u8>) -> u32` | Continues a CRC-32 from a running value over more bytes. |
| `update_window` | `fn update_window(seed: u32, data: Vec<u8>, start: i64, end: i64) -> u32` | Continues a CRC-32 over `data[start..end]`, checking a record where it lies rather than copying the window out first. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
