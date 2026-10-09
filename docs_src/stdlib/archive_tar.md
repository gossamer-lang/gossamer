# `std::archive::tar`

Status: experimental

Unix tar reader and writer (USTAR / PAX-aware decode).

## Items

| Item | Signature | Description |
|---|---|---|
| `TarEntry` | `type TarEntry` | name + data + is_dir flag. |
| `read` | `fn read(data: Vec<u8>) -> Result<Vec<tar::TarEntry>, errors::Error>` | Reads all entries from a tar archive. |
| `write` | `fn write(entries: Vec<(String, Vec<u8>)>) -> Result<Vec<u8>, errors::Error>` | Builds a tar archive from (name, data) pairs. |
| `read_limited` | `fn read_limited(data: Vec<u8>, max_entries: i64, max_entry_bytes: i64, max_total_bytes: i64) -> Result<Vec<tar::TarEntry>, errors::Error>` | `read(data)` refusing an archive with more than `max_entries` entries, an entry over `max_entry_bytes`, or more than `max_total_bytes` in all, counted as the bytes are read. |
| `extract` | `fn extract(data: Vec<u8>, dir: String) -> Result<i64, errors::Error>` | `extract(data, dir) -> Result<i64, Error>`: writes every file and directory under `dir`, refusing the archive before writing anything when a name would land outside it (`archive::enclosed_path`); answers how many entries it wrote. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
