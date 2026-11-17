//! Directory listing with each entry's own metadata, shared by the
//! interpreter and the compiled tiers so `fs::read_dir` and `fs::walk_dir`
//! report the same entries, order, and sizes everywhere.

use std::ffi::OsString;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

/// One directory entry, described by its own metadata rather than a
/// symlink target's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedEntry {
    /// File name within the parent directory.
    pub name: OsString,
    /// Parent directory joined with `name`.
    pub path: PathBuf,
    /// `true` for a regular file.
    pub is_file: bool,
    /// `true` for a directory (a symlink to one is not).
    pub is_dir: bool,
    /// `true` for a symlink, dangling or not.
    pub is_symlink: bool,
    /// Size in bytes; `0` when the entry could not be stat-ed.
    pub size: i64,
    /// Modification time as unix milliseconds; `0` when unknown or before
    /// the epoch.
    pub modified_ms: i64,
}

/// Entries of `dir`, sorted by name.
pub fn scan_dir(dir: &Path) -> io::Result<Vec<ScannedEntry>> {
    let mut entries = scan_unsorted(dir, Stat::Every)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// Entries of `dir`, sorted by name, each with its kind but no `size` or
/// `modified_ms` (both `0`). The kind comes from the directory record, so an
/// entry is stat-ed only when its record does not state one.
pub fn scan_dir_kinds(dir: &Path) -> io::Result<Vec<ScannedEntry>> {
    let mut entries = scan_unsorted(dir, Stat::KindUnknown)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// Which entries a scan stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stat {
    /// Every entry, for its size and modification time.
    Every,
    /// Only an entry whose directory record states no kind.
    KindUnknown,
}

/// Depth-first walk below `root`: each directory's entries in name order,
/// every entry handed to `visit` before the walk descends into it. Symlinks
/// are reported but never followed. The walk ends early with the first
/// `ControlFlow::Break` `visit` answers.
pub fn walk<B>(
    root: &Path,
    mut visit: impl FnMut(&ScannedEntry) -> ControlFlow<B>,
) -> io::Result<ControlFlow<B>> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in scan_dir(&dir)? {
            if let ControlFlow::Break(b) = visit(&entry) {
                return Ok(ControlFlow::Break(b));
            }
            if entry.is_dir {
                stack.push(entry.path);
            }
        }
    }
    Ok(ControlFlow::Continue(()))
}

fn modified_ms(secs: i64, nanos: i64) -> i64 {
    if secs < 0 {
        return 0;
    }
    secs.checked_mul(1000)
        .and_then(|ms| ms.checked_add(nanos / 1_000_000))
        .unwrap_or(i64::MAX)
}

/// Stats each entry relative to the open directory, so the kernel resolves
/// one name per entry instead of walking the full path again.
#[cfg(all(unix, not(miri)))]
fn scan_unsorted(dir: &Path, stat: Stat) -> io::Result<Vec<ScannedEntry>> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    use nix::dir::Type;
    use nix::fcntl::AtFlags;
    use nix::sys::stat::fstatat;

    // `libc::stat` field widths differ across Unix targets.
    fn wide(v: impl Into<i64>) -> i64 {
        v.into()
    }

    let mut handle = open_dir(dir)?;
    let names = entry_names(&mut handle)?;
    Ok(names
        .into_iter()
        .map(|(name, d_type)| {
            let os_name = OsStr::from_bytes(name.to_bytes()).to_os_string();
            let path = dir.join(&os_name);
            if stat == Stat::KindUnknown && d_type.is_some() {
                return ScannedEntry {
                    name: os_name,
                    path,
                    is_file: d_type == Some(Type::File),
                    is_dir: d_type == Some(Type::Directory),
                    is_symlink: d_type == Some(Type::Symlink),
                    size: 0,
                    modified_ms: 0,
                };
            }
            let (kind, size, modified) =
                match fstatat(&handle, name.as_c_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                    Ok(st) => {
                        let kind = match st.st_mode & libc::S_IFMT {
                            libc::S_IFREG => Some(Type::File),
                            libc::S_IFDIR => Some(Type::Directory),
                            libc::S_IFLNK => Some(Type::Symlink),
                            _ => None,
                        };
                        (
                            kind,
                            wide(st.st_size),
                            modified_ms(wide(st.st_mtime), wide(st.st_mtime_nsec)),
                        )
                    }
                    Err(_) => (d_type, 0, 0),
                };
            let (size, modified) = match stat {
                Stat::Every => (size, modified),
                Stat::KindUnknown => (0, 0),
            };
            ScannedEntry {
                name: os_name,
                path,
                is_file: kind == Some(Type::File),
                is_dir: kind == Some(Type::Directory),
                is_symlink: kind == Some(Type::Symlink),
                size,
                modified_ms: modified,
            }
        })
        .collect())
}

/// Opens `dir` for listing, through [`crate::file_read::open_cloexec`].
#[cfg(all(target_os = "linux", not(miri)))]
fn open_dir(dir: &Path) -> io::Result<nix::dir::Dir> {
    let fd = crate::file_read::open_cloexec(dir, libc::O_RDONLY | libc::O_DIRECTORY)?;
    Ok(nix::dir::Dir::from_fd(fd)?)
}

/// Opens `dir` for listing.
#[cfg(all(unix, not(target_os = "linux"), not(miri)))]
fn open_dir(dir: &Path) -> io::Result<nix::dir::Dir> {
    use nix::fcntl::OFlag;
    use nix::sys::stat::Mode;

    Ok(nix::dir::Dir::open(
        dir,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?)
}

/// The names in the open directory `handle` and the kind each entry's
/// directory record states, without `.` and `..`.
///
/// The C library's `readdir` refills through a buffer of a couple of
/// kilobytes, so a directory such as `/proc` takes several `getdents64` calls
/// per scan; reading the records into a buffer of our own takes one or two.
#[cfg(all(target_os = "linux", not(miri)))]
fn entry_names(
    handle: &mut nix::dir::Dir,
) -> io::Result<Vec<(std::ffi::CString, Option<nix::dir::Type>)>> {
    use std::os::fd::AsRawFd;

    use nix::dir::Type;

    /// Bytes of directory records each `getdents64` call may fill.
    const RECORDS: usize = 32 * 1024;
    // `struct linux_dirent64`: d_ino (8), d_off (8), d_reclen (2), d_type (1),
    // then the NUL-terminated name.
    const RECLEN_AT: usize = 16;
    const TYPE_AT: usize = 18;
    const NAME_AT: usize = 19;

    let fd = handle.as_raw_fd();
    let mut records = vec![0u8; RECORDS];
    let mut names = Vec::new();
    loop {
        // SAFETY: `records` is a writable buffer of `RECORDS` bytes that
        // outlives the call, and `fd` is the directory `handle` keeps open.
        let filled =
            unsafe { libc::syscall(libc::SYS_getdents64, fd, records.as_mut_ptr(), RECORDS) };
        if filled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let filled = usize::try_from(filled).unwrap_or(0);
        if filled == 0 {
            return Ok(names);
        }
        let mut at = 0;
        while at + NAME_AT <= filled {
            let reclen = usize::from(u16::from_ne_bytes([
                records[at + RECLEN_AT],
                records[at + RECLEN_AT + 1],
            ]));
            if reclen == 0 || at + reclen > filled {
                break;
            }
            let record = &records[at..at + reclen];
            at += reclen;
            let Ok(name) = std::ffi::CStr::from_bytes_until_nul(&record[NAME_AT..]) else {
                continue;
            };
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let kind = match record[TYPE_AT] {
                libc::DT_REG => Some(Type::File),
                libc::DT_DIR => Some(Type::Directory),
                libc::DT_LNK => Some(Type::Symlink),
                libc::DT_FIFO => Some(Type::Fifo),
                libc::DT_CHR => Some(Type::CharacterDevice),
                libc::DT_BLK => Some(Type::BlockDevice),
                libc::DT_SOCK => Some(Type::Socket),
                _ => None,
            };
            names.push((name.to_owned(), kind));
        }
    }
}

/// [`entry_names`] through the C library's `readdir`, on the Unix systems
/// whose directory record layout differs from Linux's.
#[cfg(all(unix, not(target_os = "linux"), not(miri)))]
fn entry_names(
    handle: &mut nix::dir::Dir,
) -> io::Result<Vec<(std::ffi::CString, Option<nix::dir::Type>)>> {
    let mut names = Vec::new();
    for entry in handle.iter() {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        names.push((name.to_owned(), entry.file_type()));
    }
    Ok(names)
}

// Miri does not support the directory-relative Unix APIs above. Keep the
// directory and C-ABI tests running through the portable implementation.
#[cfg(any(not(unix), miri))]
fn scan_unsorted(dir: &Path, stat: Stat) -> io::Result<Vec<ScannedEntry>> {
    std::fs::read_dir(dir)?
        .map(|entry| {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if stat == Stat::KindUnknown {
                return Ok(ScannedEntry {
                    name: entry.file_name(),
                    path,
                    is_file: ft.is_file(),
                    is_dir: ft.is_dir(),
                    is_symlink: ft.is_symlink(),
                    size: 0,
                    modified_ms: 0,
                });
            }
            // Windows: WIN32_FIND_DATA (behind entry.metadata()) stores
            // nFileSize=0 for directories, so stat the path to get the NTFS
            // directory-index size.
            let meta = if cfg!(windows) && !ft.is_symlink() {
                std::fs::metadata(&path)
            } else {
                entry.metadata()
            };
            let (size, modified) = meta.map_or((0, 0), |m| {
                let since_epoch = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                (
                    i64::try_from(m.len()).unwrap_or(i64::MAX),
                    since_epoch.map_or(0, |d| {
                        modified_ms(
                            i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                            i64::from(d.subsec_nanos()),
                        )
                    }),
                )
            });
            Ok(ScannedEntry {
                name: entry.file_name(),
                path,
                is_file: ft.is_file(),
                is_dir: ft.is_dir(),
                is_symlink: ft.is_symlink(),
                size,
                modified_ms: modified,
            })
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn scan_dir_kinds_answers_scan_dir_without_metadata() {
        let root = std::env::temp_dir().join(format!("gos-dir-kinds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).expect("create fixture dir");
        std::fs::write(root.join("big"), vec![0u8; 5000]).expect("write fixture file");
        std::os::unix::fs::symlink("big", root.join("link")).expect("symlink");
        std::os::unix::fs::symlink("missing-target", root.join("dangling")).expect("symlink");

        let full = scan_dir(&root).expect("scan fixture");
        let kinds = scan_dir_kinds(&root).expect("scan fixture kinds");
        std::fs::remove_dir_all(&root).expect("remove fixture");

        let without_metadata: Vec<ScannedEntry> = full
            .into_iter()
            .map(|e| ScannedEntry {
                size: 0,
                modified_ms: 0,
                ..e
            })
            .collect();
        assert_eq!(kinds, without_metadata);
        assert_eq!(kinds.len(), 4);
    }

    #[test]
    fn scan_dir_reports_symlinks_by_their_own_metadata_in_name_order() {
        let root = std::env::temp_dir().join(format!("gos-dir-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).expect("create fixture dir");
        std::fs::write(root.join("big"), vec![0u8; 5000]).expect("write fixture file");
        std::os::unix::fs::symlink("big", root.join("link")).expect("symlink");
        std::os::unix::fs::symlink("missing-target", root.join("dangling")).expect("symlink");

        let entries = scan_dir(&root).expect("scan fixture");
        let summary: Vec<(String, bool, bool, bool, i64)> = entries
            .iter()
            .map(|e| {
                let size = if e.is_dir { -1 } else { e.size };
                (
                    e.name.to_string_lossy().into_owned(),
                    e.is_file,
                    e.is_dir,
                    e.is_symlink,
                    size,
                )
            })
            .collect();
        std::fs::remove_dir_all(&root).expect("remove fixture");
        assert_eq!(
            summary,
            vec![
                ("big".to_string(), true, false, false, 5000),
                ("dangling".to_string(), false, false, true, 14),
                ("link".to_string(), false, false, true, 3),
                ("sub".to_string(), false, true, false, -1),
            ]
        );
        assert!(entries.iter().all(|e| e.path == root.join(&e.name)));
    }

    #[test]
    fn walk_descends_directories_but_not_symlinks() {
        let root = std::env::temp_dir().join(format!("gos-dir-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b")).expect("create fixture dirs");
        std::fs::write(root.join("a/b/f"), b"x").expect("write fixture file");
        std::os::unix::fs::symlink("a", root.join("loop")).expect("symlink");

        let mut seen = Vec::new();
        let flow = walk(&root, |e| {
            seen.push(
                e.path
                    .strip_prefix(&root)
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
            );
            ControlFlow::<()>::Continue(())
        });
        std::fs::remove_dir_all(&root).expect("remove fixture");
        assert_eq!(flow.expect("walk fixture"), ControlFlow::Continue(()));
        assert_eq!(
            seen,
            ["a", "loop", "a/b", "a/b/f"].map(PathBuf::from).to_vec()
        );
    }
}
