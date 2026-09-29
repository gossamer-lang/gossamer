# `std::archive::zip`

Status: experimental

ZIP archive reader and writer.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/archive/zip.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`ZipEntry`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/archive/zip.rs) | `type ZipEntry` | name + decompressed data + is_dir flag. |
| [`read`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/archive/zip.rs) | `fn read(data: Vec<u8>) -> Result<Vec<zip::ZipEntry>, errors::Error>` | Reads all file entries from a zip stored in `data`. |
| [`write`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/archive/zip.rs) | `fn write(entries: Vec<(String, Vec<u8>)>) -> Result<Vec<u8>, errors::Error>` | Builds an in-memory zip from (name, data) pairs. |
