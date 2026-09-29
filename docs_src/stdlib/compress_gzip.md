# `std::compress::gzip`

Status: experimental

gzip encoder / decoder (RFC 1952; flate2-backed).

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/compress/gzip.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`Level`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/compress/gzip.rs) | `type Level` | Compression level (`0` store-only … `9` best); default is gzip(1)'s `6`. |
| [`decode`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/compress/gzip.rs) | `fn decode(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | Decompresses a gzip-formatted payload. |
| [`encode`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/compress/gzip.rs) | `fn encode(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>` | Compresses bytes at the supplied Level. |
