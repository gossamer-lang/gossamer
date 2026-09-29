# `std::crypto::insecure`

Status: experimental

Legacy / broken hashes (MD5, SHA-1). Compat only - never use for new code.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto/insecure.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`md5`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto/insecure.rs) | `fn md5(data: Vec<u8>) -> Vec<u8>` | One-shot MD5. |
| [`md5_hex`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto/insecure.rs) | `fn md5_hex(text: String) -> String` | One-shot MD5, hex-encoded. |
| [`sha1`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto/insecure.rs) | `fn sha1(data: Vec<u8>) -> Vec<u8>` | One-shot SHA-1. |
| [`sha1_hex`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto/insecure.rs) | `fn sha1_hex(text: String) -> String` | One-shot SHA-1, hex-encoded. |
