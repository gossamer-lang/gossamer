# `std::encoding::ascii85`

Status: experimental

ASCII85 / base85 encode / decode.

## Items

| Item | Signature | Description |
|---|---|---|
| `encode` | `fn encode(data: Vec<u8>) -> String` | Bytes -> ASCII85 string. |
| `decode` | `fn decode(text: String) -> Result<Vec<u8>, errors::Error>` | ASCII85 string -> bytes. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
