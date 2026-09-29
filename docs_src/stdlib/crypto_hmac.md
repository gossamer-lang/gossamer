# `std::crypto::hmac`

Status: experimental

HMAC-SHA-256 keyed MACs.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`sha256_hex`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn sha256_hex(key: String, message: String) -> String` | HMAC-SHA-256 over a message, hex-encoded. |
| [`sha256_mac`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn sha256_mac(key: Vec<u8>, message: Vec<u8>) -> Vec<u8>` | HMAC-SHA-256 over a message. |
