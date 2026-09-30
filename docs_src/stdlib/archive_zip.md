# `std::archive::zip`

Status: experimental

ZIP archive reader and writer.

## Items

| Item | Signature | Description |
|---|---|---|
| `ZipEntry` | `type ZipEntry` | name + decompressed data + is_dir flag. |
| `read` | `fn read(data: Vec<u8>) -> Result<Vec<zip::ZipEntry>, errors::Error>` | Reads all file entries from a zip stored in `data`. |
| `write` | `fn write(entries: Vec<(String, Vec<u8>)>) -> Result<Vec<u8>, errors::Error>` | Builds an in-memory zip from (name, data) pairs. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
