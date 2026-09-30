# `std::encoding::pem`

Status: experimental

PEM block encoder and decoder.

## Items

| Item | Signature | Description |
|---|---|---|
| `Block` | `type Block` | A decoded PEM block with type label and DER bytes. |
| `encode` | `fn encode(block: pem::Block) -> String` | Encodes a Block as a PEM string. |
| `decode` | `fn decode(text: String) -> Result<pem::Block, errors::Error>` | Decodes the first PEM block from a string. |
| `decode_all` | `fn decode_all(text: String) -> Result<Vec<pem::Block>, errors::Error>` | Decodes all PEM blocks from a string. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
