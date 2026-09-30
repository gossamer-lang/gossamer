# `std::encoding::base32`

Status: experimental

RFC 4648 base32 (uppercase) encode / decode.

## Items

| Item | Signature | Description |
|---|---|---|
| `encode` | `fn encode(data: Vec<u8>) -> String` | Bytes -> base32 string. |
| `decode` | `fn decode(text: String) -> Result<Vec<u8>, errors::Error>` | Base32 string -> bytes. |
| `encode_string` | `fn encode_string(text: String) -> String` | Encodes a String as standard base32 text. |
| `decode_string` | `fn decode_string(text: String) -> Result<String, errors::Error>` | Decodes standard base32 text into a String. |
| `encode_hex` | `fn encode_hex(data: Vec<u8>) -> String` | Encodes a String as extended-hex base32 text. |
| `decode_hex` | `fn decode_hex(text: String) -> Result<Vec<u8>, errors::Error>` | Decodes extended-hex base32 text into a String. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
