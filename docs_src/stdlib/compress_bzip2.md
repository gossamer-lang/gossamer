# `std::compress::bzip2`

Status: experimental

bzip2 encoder / decoder (BZh format).

## Items

| Item | Signature | Description |
|---|---|---|
| `compress` | `fn compress(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>` | One-shot bzip2 compress. |
| `decompress` | `fn decompress(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | One-shot bzip2 decompress. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
