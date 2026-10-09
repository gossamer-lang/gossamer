# `std::compress::bzip2`

Status: experimental

bzip2 encoder / decoder (BZh format).

## Items

| Item | Signature | Description |
|---|---|---|
| `compress` | `fn compress(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>` | One-shot bzip2 compress at `level`, `1` (fastest) to `9` (best). |
| `decompress` | `fn decompress(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | One-shot bzip2 decompress. |
| `decompress_limited` | `fn decompress_limited(data: Vec<u8>, max_bytes: i64) -> Result<Vec<u8>, errors::Error>` | `decompress(data)`, refusing output past `max_bytes`. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
