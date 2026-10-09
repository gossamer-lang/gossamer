//! ZIP and tar archives: reading with optional bounds, writing, and
//! extraction confined to a destination directory.

use std::io::{Cursor, Read, Write};
use std::path::Path;

/// What an archive entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file, whose bytes the entry carries.
    File,
    /// A directory.
    Dir,
    /// A symbolic or hard link, device, or other special entry, which carries
    /// no bytes and is never extracted.
    Other,
}

/// One archive entry, read into memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The name as the archive stores it.
    pub name: String,
    /// The file's bytes; empty for anything but a regular file.
    pub data: Vec<u8>,
    /// What the entry is.
    pub kind: EntryKind,
}

/// Bounds on what reading an archive may produce; `None` is unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    /// Most entries.
    pub entries: Option<u64>,
    /// Most bytes one entry may hold.
    pub entry_bytes: Option<u64>,
    /// Most bytes all entries may hold together.
    pub total_bytes: Option<u64>,
}

impl Limits {
    /// Limits from three counts where a negative count means no bound.
    #[must_use]
    pub fn from_counts(entries: i64, entry_bytes: i64, total_bytes: i64) -> Self {
        Self {
            entries: u64::try_from(entries).ok(),
            entry_bytes: u64::try_from(entry_bytes).ok(),
            total_bytes: u64::try_from(total_bytes).ok(),
        }
    }
}

/// Reads one entry's bytes, holding them to the limits and, when the archive
/// declares it, to the size the entry's header states.
fn read_entry(
    format: &str,
    name: &str,
    mut reader: impl Read,
    declared: Option<u64>,
    limits: Limits,
    total: &mut u64,
) -> Result<Vec<u8>, String> {
    let remaining = limits.total_bytes.map(|t| t.saturating_sub(*total));
    let cap = match (limits.entry_bytes, remaining) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let mut data = Vec::new();
    let read = if let Some(cap) = cap {
        reader
            .by_ref()
            .take(cap.saturating_add(1))
            .read_to_end(&mut data)
    } else {
        reader.read_to_end(&mut data)
    };
    read.map_err(|e| format!("{format} read entry {name}: {e}"))?;
    let len = data.len() as u64;
    if let Some(max) = limits.entry_bytes
        && len > max
    {
        return Err(format!(
            "{format} entry {name}: more than the limit of {max} bytes per entry"
        ));
    }
    if let Some(max) = limits.total_bytes
        && total.saturating_add(len) > max
    {
        return Err(format!(
            "{format}: entries hold more than the limit of {max} bytes in total"
        ));
    }
    if let Some(declared) = declared
        && declared != len
    {
        return Err(format!(
            "{format} entry {name}: holds {len} bytes, its header says {declared}"
        ));
    }
    *total += len;
    Ok(data)
}

fn check_entry_count(format: &str, count: u64, limits: Limits) -> Result<(), String> {
    match limits.entries {
        Some(max) if count > max => Err(format!(
            "{format}: archive holds more than the limit of {max} entries"
        )),
        _ => Ok(()),
    }
}

/// Every entry of the ZIP archive in `data`, held to `limits`.
pub fn zip_read(data: &[u8], limits: Limits) -> Result<Vec<Entry>, String> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(data)).map_err(|e| format!("zip read: {e}"))?;
    check_entry_count("zip", archive.len() as u64, limits)?;
    let mut entries = Vec::with_capacity(archive.len());
    let mut total = 0u64;
    for i in 0..archive.len() {
        let file = archive
            .by_index(i)
            .map_err(|e| format!("zip entry {i}: {e}"))?;
        let name = file.name().to_owned();
        let kind = if file.is_dir() {
            EntryKind::Dir
        } else if file.is_symlink() {
            EntryKind::Other
        } else {
            EntryKind::File
        };
        let declared = file.size();
        let data = if kind == EntryKind::Dir {
            Vec::new()
        } else {
            read_entry("zip", &name, file, Some(declared), limits, &mut total)?
        };
        entries.push(Entry { name, data, kind });
    }
    Ok(entries)
}

/// Every entry of the tar archive in `data`, held to `limits`.
pub fn tar_read(data: &[u8], limits: Limits) -> Result<Vec<Entry>, String> {
    let mut archive = tar::Archive::new(Cursor::new(data));
    let mut entries = Vec::new();
    let mut total = 0u64;
    for entry in archive.entries().map_err(|e| format!("tar entries: {e}"))? {
        let entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        check_entry_count("tar", entries.len() as u64 + 1, limits)?;
        let name = entry
            .path()
            .map_err(|e| format!("tar entry path: {e}"))?
            .to_string_lossy()
            .into_owned();
        let header_kind = entry.header().entry_type();
        let kind = if header_kind.is_dir() {
            EntryKind::Dir
        } else if header_kind.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        let data = if kind == EntryKind::File {
            read_entry("tar", &name, entry, None, limits, &mut total)?
        } else {
            Vec::new()
        };
        entries.push(Entry { name, data, kind });
    }
    Ok(entries)
}

/// A ZIP archive of `files`, `(name, data)` pairs, deflate-compressed.
pub fn zip_write(files: &[(impl AsRef<str>, impl AsRef<[u8]>)]) -> Result<Vec<u8>, String> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in files {
        let name = name.as_ref();
        writer
            .start_file(name, options)
            .map_err(|e| format!("zip start {name}: {e}"))?;
        writer
            .write_all(data.as_ref())
            .map_err(|e| format!("zip write {name}: {e}"))?;
    }
    writer
        .finish()
        .map(Cursor::into_inner)
        .map_err(|e| format!("zip finish: {e}"))
}

/// A tar archive of `files`, `(name, data)` pairs.
pub fn tar_write(files: &[(impl AsRef<str>, impl AsRef<[u8]>)]) -> Result<Vec<u8>, String> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in files {
        let (name, data) = (name.as_ref(), data.as_ref());
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, data)
            .map_err(|e| format!("tar append {name}: {e}"))?;
    }
    builder.into_inner().map_err(|e| format!("tar finish: {e}"))
}

/// `name` as a relative path that stays inside the directory it is extracted
/// to, with `.` steps and `..` steps that stay inside resolved, or `None` for
/// an absolute path, a drive or UNC prefix, a NUL byte, a path that leaves the
/// directory, or one that names the directory itself.
#[must_use]
pub fn enclosed_path(name: &str) -> Option<String> {
    if name.contains('\0') || name.starts_with(['/', '\\']) {
        return None;
    }
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Writes every file and directory of `entries` under `dir`, refusing the
/// whole extraction before writing anything when a name would land outside
/// it. Link and special entries are skipped. Answers how many entries it
/// wrote.
pub fn extract(entries: &[Entry], dir: &Path) -> Result<u64, String> {
    let mut targets = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.kind == EntryKind::Other {
            continue;
        }
        let Some(relative) = enclosed_path(&entry.name) else {
            return Err(format!(
                "archive entry {:?} would be written outside {}",
                entry.name,
                dir.display()
            ));
        };
        targets.push((dir.join(relative), entry));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    for (path, entry) in &targets {
        if entry.kind == EntryKind::Dir {
            std::fs::create_dir_all(path).map_err(|e| format!("create {}: {e}", path.display()))?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        std::fs::write(path, &entry.data).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(targets.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::{
        EntryKind, Limits, enclosed_path, extract, tar_read, tar_write, zip_read, zip_write,
    };

    type Reader = fn(&[u8], Limits) -> Result<Vec<super::Entry>, String>;
    type Writer = fn(&[(String, Vec<u8>)]) -> Result<Vec<u8>, String>;

    fn writers() -> [(Reader, Writer); 2] {
        [(zip_read, |f| zip_write(f)), (tar_read, |f| tar_write(f))]
    }

    fn files() -> Vec<(String, Vec<u8>)> {
        vec![
            ("a.txt".to_string(), b"alpha".to_vec()),
            ("dir/b.bin".to_string(), vec![7u8; 1000]),
        ]
    }

    #[test]
    fn archives_round_trip() {
        for (read, write) in writers() {
            let bytes = write(&files()).expect("writes");
            let entries = read(&bytes, Limits::default()).expect("reads");
            let got: Vec<(String, Vec<u8>)> = entries
                .into_iter()
                .filter(|e| e.kind == EntryKind::File)
                .map(|e| (e.name, e.data))
                .collect();
            assert_eq!(got, files());
        }
    }

    #[test]
    fn limits_are_exact() {
        for (read, write) in writers() {
            let bytes = write(&files()).expect("writes");
            let fits = Limits::from_counts(2, 1000, 1005);
            assert!(read(&bytes, fits).is_ok());
            for over in [
                Limits::from_counts(1, -1, -1),
                Limits::from_counts(-1, 999, -1),
                Limits::from_counts(-1, -1, 1004),
            ] {
                assert!(read(&bytes, over).is_err(), "{over:?}");
            }
        }
    }

    #[test]
    fn a_zip_entry_larger_than_its_header_is_refused() {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("x", options).expect("start");
        std::io::Write::write_all(&mut writer, b"0123456789").expect("write");
        let mut bytes = writer.finish().expect("finish").into_inner();
        // The central directory's uncompressed size (offset 24 of the record)
        // now understates the entry.
        let central = bytes
            .windows(4)
            .rposition(|w| w == [0x50, 0x4b, 0x01, 0x02])
            .expect("central directory");
        bytes[central + 24..central + 28].copy_from_slice(&4u32.to_le_bytes());
        assert!(zip_read(&bytes, Limits::default()).is_err());
    }

    #[test]
    fn paths_stay_inside() {
        assert_eq!(enclosed_path("a/b.txt").as_deref(), Some("a/b.txt"));
        assert_eq!(enclosed_path("./a//b/../c").as_deref(), Some("a/c"));
        assert_eq!(enclosed_path("a\\b").as_deref(), Some("a/b"));
        for bad in [
            "../x",
            "/etc/x",
            "C:\\x",
            "c:x",
            "a/../../x",
            "",
            ".",
            "a/..",
            "\\\\host\\s",
            "a\0b",
        ] {
            assert_eq!(enclosed_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn extraction_refuses_an_escaping_name_before_writing() {
        let dir = std::env::temp_dir().join(format!("gos-extract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let entries = vec![
            super::Entry {
                name: "ok.txt".into(),
                data: b"ok".to_vec(),
                kind: EntryKind::File,
            },
            super::Entry {
                name: "../evil".into(),
                data: b"x".to_vec(),
                kind: EntryKind::File,
            },
        ];
        assert!(extract(&entries, &dir).is_err());
        assert!(!dir.join("ok.txt").exists());
        let good = &entries[..1];
        assert_eq!(extract(good, &dir), Ok(1));
        assert_eq!(
            std::fs::read(dir.join("ok.txt")).ok().as_deref(),
            Some(b"ok".as_slice())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
