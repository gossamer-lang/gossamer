# `std::encoding::ascii85`

Status: experimental

ASCII85 / base85 encode / decode.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding/ascii85.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`decode`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding/ascii85.rs) | `fn decode(text: String) -> Result<Vec<u8>, errors::Error>` | ASCII85 string -> bytes. |
| [`encode`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding/ascii85.rs) | `fn encode(data: Vec<u8>) -> String` | Bytes -> ASCII85 string. |
