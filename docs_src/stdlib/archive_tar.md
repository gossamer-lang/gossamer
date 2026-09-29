# `std::archive::tar`

Status: experimental

Unix tar reader and writer (USTAR / PAX-aware decode).

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/archive/tar.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`TarEntry`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/archive/tar.rs) | `type TarEntry` | name + data + is_dir flag. |
| [`read`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/archive/tar.rs) | `fn read(data: Vec<u8>) -> Result<Vec<tar::TarEntry>, errors::Error>` | Reads all entries from a tar archive. |
| [`write`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/archive/tar.rs) | `fn write(entries: Vec<(String, Vec<u8>)>) -> Result<Vec<u8>, errors::Error>` | Builds a tar archive from (name, data) pairs. |
