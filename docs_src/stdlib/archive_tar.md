# `std::archive::tar`

Status: experimental

Unix tar reader and writer (USTAR / PAX-aware decode).

## Items

| Item | Signature | Description |
|---|---|---|
| `TarEntry` | `type TarEntry` | name + data + is_dir flag. |
| `read` | `fn read(data: Vec<u8>) -> Result<Vec<tar::TarEntry>, errors::Error>` | Reads all entries from a tar archive. |
| `write` | `fn write(entries: Vec<(String, Vec<u8>)>) -> Result<Vec<u8>, errors::Error>` | Builds a tar archive from (name, data) pairs. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
