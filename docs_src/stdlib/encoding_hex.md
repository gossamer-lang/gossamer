# `std::encoding::hex`

Status: experimental

Lowercase hex encode/decode.

## Items

| Item | Signature | Description |
|---|---|---|
| `encode` | `fn encode(data: Vec<u8>) -> String` | Encodes bytes to hex. |
| `decode` | `fn decode(text: String) -> Result<Vec<u8>, errors::Error>` | Decodes a hex string. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
