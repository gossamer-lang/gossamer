# `std::compress::gzip`

Status: experimental

gzip encoder / decoder (RFC 1952; flate2-backed).

## Items

| Item | Signature | Description |
|---|---|---|
| `encode` | `fn encode(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>` | Compresses bytes at `level`, `0` (store only) to `9` (best); gzip(1) uses `6`. |
| `decode` | `fn decode(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | Decompresses a gzip payload; concatenated members decode as one stream. Output is unbounded - use `decode_limited` for untrusted input. |
| `decode_limited` | `fn decode_limited(data: Vec<u8>, max_bytes: i64) -> Result<Vec<u8>, errors::Error>` | `decode(data)`, refusing output past `max_bytes`: an archive bomb answers `Err` instead of exhausting memory. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
