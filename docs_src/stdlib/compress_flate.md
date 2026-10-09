# `std::compress::flate`

Status: experimental

Raw DEFLATE (RFC 1951) encoder / decoder.

## Items

| Item | Signature | Description |
|---|---|---|
| `compress` | `fn compress(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>` | One-shot DEFLATE compress at `level`, `0` (store only) to `9` (best). |
| `decompress` | `fn decompress(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | One-shot DEFLATE decompress. |
| `decompress_limited` | `fn decompress_limited(data: Vec<u8>, max_bytes: i64) -> Result<Vec<u8>, errors::Error>` | `decompress(data)`, refusing output past `max_bytes`. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
