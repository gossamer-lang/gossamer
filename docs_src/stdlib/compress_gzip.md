# `std::compress::gzip`

Status: experimental

gzip encoder / decoder (RFC 1952; flate2-backed).

## Items

| Item | Signature | Description |
|---|---|---|
| `encode` | `fn encode(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>` | Compresses bytes at `level`, `0` (store only) to `9` (best); gzip(1) uses `6`. |
| `decode` | `fn decode(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | Decompresses a gzip-formatted payload. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
