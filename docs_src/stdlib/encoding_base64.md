# `std::encoding::base64`

Status: experimental

RFC 4648 base64 encode/decode.

## Items

| Item | Signature | Description |
|---|---|---|
| `encode` | `fn encode(data: Vec<u8>) -> String` | Encodes bytes to a base64 string. |
| `decode` | `fn decode(text: String) -> Result<Vec<u8>, errors::Error>` | Decodes a base64 string. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
