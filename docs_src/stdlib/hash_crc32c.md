# `std::hash::crc32c`

Status: experimental

CRC-32C (Castagnoli) checksums, computed with the CPU's CRC instruction where there is one.

## Items

| Item | Signature | Description |
|---|---|---|
| `checksum` | `fn checksum(data: Vec<u8>) -> u32` | CRC-32C checksum of a byte slice. |
| `checksum_string` | `fn checksum_string(text: String) -> u32` | CRC-32C checksum of a String. |
| `update` | `fn update(seed: u32, data: Vec<u8>) -> u32` | Continues a CRC-32C from a running value over more bytes. |
| `update_window` | `fn update_window(seed: u32, data: Vec<u8>, start: i64, end: i64) -> u32` | Continues a CRC-32C over `data[start..end]`, checking a record where it lies rather than copying the window out first. |
