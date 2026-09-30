# `std::compress::zlib`

Status: experimental

zlib (RFC 1950) encoder / decoder.

## Items

| Item | Signature | Description |
|---|---|---|
| `compress` | `fn compress(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>` | One-shot zlib compress. |
| `decompress` | `fn decompress(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | One-shot zlib decompress. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
