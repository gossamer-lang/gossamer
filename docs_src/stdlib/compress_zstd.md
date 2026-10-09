# `std::compress::zstd`

Status: experimental

Zstandard encoder / decoder (RFC 8478; libzstd-vendored).

## Items

| Item | Signature | Description |
|---|---|---|
| `encode` | `fn encode(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | One-shot Zstandard compress at the default level (3). |
| `encode_level` | `fn encode_level(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>` | One-shot Zstandard compress at the supplied level (1 fastest -- 22 best). |
| `decode` | `fn decode(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | One-shot Zstandard decompress. |
| `decode_limited` | `fn decode_limited(data: Vec<u8>, max_bytes: i64) -> Result<Vec<u8>, errors::Error>` | `decode(data)`, refusing output past `max_bytes`. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
