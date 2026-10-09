// Runtime support for `std::archive::zip` - ZIP archive reading and writing.
//
// Wraps the `zip` crate. The read API extracts files into memory; the write API
// builds an in-memory ZIP archive from name/content pairs. Both return IoError on
// failure so callers can use `?` without a separate error conversion.

#![forbid(unsafe_code)]

use gossamer_runtime::codec::archive::{self, Limits};

use crate::io::IoError;

/// A single entry extracted from a ZIP archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    /// Path inside the archive.
    pub name: String,
    /// Decompressed file content. Empty for directory entries.
    pub data: Vec<u8>,
    /// `true` for directory entries (no data).
    pub is_dir: bool,
}

/// Reads all file entries from a ZIP archive stored in `data`.
///
/// Directory entries are included with an empty `data` field and
/// `is_dir = true`. Returns an error if the bytes are not a valid ZIP
/// archive or an entry does not hold the size its header states.
pub fn read(data: &[u8]) -> Result<Vec<ZipEntry>, IoError> {
    read_limited(data, Limits::default())
}

/// [`read`], refusing an archive that holds more than `limits` allows.
pub fn read_limited(data: &[u8], limits: Limits) -> Result<Vec<ZipEntry>, IoError> {
    let entries = archive::zip_read(data, limits).map_err(IoError::Other)?;
    Ok(entries
        .into_iter()
        .map(|e| ZipEntry {
            is_dir: e.kind == archive::EntryKind::Dir,
            name: e.name,
            data: e.data,
        })
        .collect())
}

/// Builds an in-memory ZIP archive from `files` - a list of `(name, data)` pairs.
///
/// Files are stored with deflate compression at the default level. Returns the
/// raw ZIP bytes on success.
pub fn write(files: &[(&str, &[u8])]) -> Result<Vec<u8>, IoError> {
    archive::zip_write(files).map_err(IoError::Other)
}

/// Writes every file and directory of the ZIP archive in `data` under `dir`,
/// refusing the archive before writing anything when an entry's name would
/// land outside `dir`. Answers how many entries it wrote.
pub fn extract(data: &[u8], dir: &std::path::Path) -> Result<u64, IoError> {
    let entries = archive::zip_read(data, Limits::default()).map_err(IoError::Other)?;
    archive::extract(&entries, dir).map_err(IoError::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_single_file() {
        let content = b"hello from zip";
        let zip_bytes = write(&[("hello.txt", content)]).unwrap();
        let entries = read(&zip_bytes).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "hello.txt");
        assert_eq!(entries[0].data, content);
        assert!(!entries[0].is_dir);
    }

    #[test]
    fn roundtrip_multiple_files() {
        let zip_bytes = write(&[("a.txt", b"aaa"), ("b.txt", b"bbb")]).unwrap();
        let entries = read(&zip_bytes).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
    }

    #[test]
    fn invalid_bytes_return_error() {
        let result = read(b"not a zip");
        assert!(result.is_err());
    }

    #[test]
    fn empty_archive() {
        let zip_bytes = write(&[]).unwrap();
        let entries = read(&zip_bytes).unwrap();
        assert!(entries.is_empty());
    }
}
