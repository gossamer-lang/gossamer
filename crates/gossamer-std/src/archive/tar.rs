// Runtime support for `std::archive::tar` - tar archive reading and writing.
//
// Wraps the `tar` crate. The read API extracts regular files into memory; the
// write API builds an in-memory tar archive from name/content pairs.

#![forbid(unsafe_code)]

use gossamer_runtime::codec::archive::{self, Limits};

use crate::io::IoError;

/// A single file entry extracted from a tar archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarEntry {
    /// Path inside the archive.
    pub name: String,
    /// File content (empty for non-regular entries).
    pub data: Vec<u8>,
    /// `true` for directory entries.
    pub is_dir: bool,
}

/// Reads every entry of the tar archive in `data`. A link or special entry
/// appears with no data and `is_dir = false`.
pub fn read(data: &[u8]) -> Result<Vec<TarEntry>, IoError> {
    read_limited(data, Limits::default())
}

/// [`read`], refusing an archive that holds more than `limits` allows.
pub fn read_limited(data: &[u8], limits: Limits) -> Result<Vec<TarEntry>, IoError> {
    let entries = archive::tar_read(data, limits).map_err(IoError::Other)?;
    Ok(entries
        .into_iter()
        .map(|e| TarEntry {
            is_dir: e.kind == archive::EntryKind::Dir,
            name: e.name,
            data: e.data,
        })
        .collect())
}

/// Builds an in-memory (ustar) tar archive from `files` - `(name, data)` pairs.
pub fn write(files: &[(&str, &[u8])]) -> Result<Vec<u8>, IoError> {
    archive::tar_write(files).map_err(IoError::Other)
}

/// Writes every file and directory of the tar archive in `data` under `dir`,
/// refusing the archive before writing anything when an entry's name would
/// land outside `dir`; link and special entries are skipped. Answers how many
/// entries it wrote.
pub fn extract(data: &[u8], dir: &std::path::Path) -> Result<u64, IoError> {
    let entries = archive::tar_read(data, Limits::default()).map_err(IoError::Other)?;
    archive::extract(&entries, dir).map_err(IoError::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_single_file() {
        let content = b"hello from tar";
        let tar_bytes = write(&[("hello.txt", content)]).unwrap();
        let entries = read(&tar_bytes).unwrap();
        assert_eq!(entries.len(), 1);
        // tar appends a trailing slash for dirs; files keep the path
        assert!(entries[0].name.contains("hello.txt"));
        assert_eq!(entries[0].data, content);
        assert!(!entries[0].is_dir);
    }

    #[test]
    fn roundtrip_multiple_files() {
        let tar_bytes = write(&[("a.txt", b"aaa"), ("b.txt", b"bbb")]).unwrap();
        let entries = read(&tar_bytes).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn empty_archive() {
        let tar_bytes = write(&[]).unwrap();
        let entries = read(&tar_bytes).unwrap();
        assert!(entries.is_empty());
    }
}
