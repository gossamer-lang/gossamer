# `std::compress::flate`

Status: experimental

Raw DEFLATE (RFC 1951) encoder / decoder.

## Items

| Item | Signature | Description |
|---|---|---|
| `compress` | `fn compress(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>` | One-shot DEFLATE compress. |
| `decompress` | `fn decompress(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | One-shot DEFLATE decompress. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
