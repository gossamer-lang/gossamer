# `std::encoding::pem`

Status: experimental

PEM block encoder and decoder.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`Block`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding.rs) | `type Block` | A decoded PEM block with type label and DER bytes. |
| [`decode`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding.rs) | `fn decode(data: String) -> Result<pem::Block, errors::Error>` | Decodes the first PEM block from a string. |
| [`decode_all`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding.rs) | `fn decode_all(data: String) -> Result<Vec<pem::Block>, errors::Error>` | Decodes all PEM blocks from a string. |
| [`encode`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding.rs) | `fn encode(block: pem::Block) -> String` | Encodes a Block as a PEM string. |
