//! Filesystem operations past the portable basics: links, a recursive
//! directory copy, a move that crosses devices, ownership, and free space.
//! The bytecode VM and compiled code both call these, so every tier reads the
//! filesystem the same way.

use std::io;
use std::path::{Path, PathBuf};

/// Creates `link`, a symbolic link to `target`. A relative `target` is read
/// from the link's own directory when the link is followed.
///
/// # Errors
/// The link exists, its directory does not, or the platform refuses (on
/// Windows, creating a link needs Developer Mode or privilege).
pub fn symlink(target: &str, link: &str) -> io::Result<()> {
    platform_symlink(Path::new(target), Path::new(link))
}

// Platform: a Unix link names its target whatever kind of file it is.
#[cfg(unix)]
fn platform_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

// Platform: a Windows link is made for a file or for a directory, decided by
// what the target is when the link is made.
#[cfg(windows)]
fn platform_symlink(target: &Path, link: &Path) -> io::Result<()> {
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        link.parent().unwrap_or_else(|| Path::new("")).join(target)
    };
    if resolved.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

// Platform: a target with neither Unix nor Windows links has none to make.
#[cfg(not(any(unix, windows)))]
fn platform_symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// Creates `link`, a second name for the file `original`.
///
/// # Errors
/// `link` exists, the two are on different filesystems, or `original` is a
/// directory or missing.
pub fn hard_link(original: &str, link: &str) -> io::Result<()> {
    std::fs::hard_link(original, link)
}

/// The target a symbolic link names, as it was written.
///
/// # Errors
/// `path` is missing or is not a symbolic link.
pub fn read_link(path: &str) -> io::Result<String> {
    std::fs::read_link(path).map(|target| crate::canonical_path::display(&target))
}

/// Copies the directory `src` and everything below it to `dst`, which must
/// not exist yet, answering the bytes of file content copied. Symbolic links
/// are copied as links, never followed, so a link cycle cannot recurse, and
/// every file and directory keeps its permissions.
///
/// # Errors
/// `src` is not a directory, `dst` exists or lies inside `src`, or a read or
/// write fails.
pub fn copy_dir(src: &str, dst: &str) -> io::Result<u64> {
    let (src, dst) = (Path::new(src), Path::new(dst));
    let meta = std::fs::symlink_metadata(src)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", src.display()),
        ));
    }
    if std::fs::symlink_metadata(dst).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", dst.display()),
        ));
    }
    if within(&std::fs::canonicalize(src)?, &absolute_target(dst)?) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} lies inside {}", dst.display(), src.display()),
        ));
    }
    copy_tree(src, dst)
}

/// `dst` made absolute through its existing parent, for a path that does not
/// exist yet.
fn absolute_target(dst: &Path) -> io::Result<PathBuf> {
    let parent = match dst.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => std::fs::canonicalize(parent)?,
        _ => std::env::current_dir()?,
    };
    Ok(match dst.file_name() {
        Some(name) => parent.join(name),
        None => parent,
    })
}

/// Whether `inner` is `outer` or lies below it.
fn within(outer: &Path, inner: &Path) -> bool {
    inner.starts_with(outer)
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<u64> {
    std::fs::create_dir(dst)?;
    let mut copied = 0u64;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if kind.is_symlink() {
            platform_symlink(&std::fs::read_link(entry.path())?, &to)?;
        } else if kind.is_dir() {
            copied += copy_tree(&entry.path(), &to)?;
        } else {
            copied += std::fs::copy(entry.path(), &to)?;
        }
    }
    std::fs::set_permissions(dst, std::fs::metadata(src)?.permissions())?;
    Ok(copied)
}

/// Moves `src` to `dst`: a rename where both are on one filesystem, else a
/// copy of the file, link, or directory tree followed by removing `src`.
///
/// # Errors
/// `src` is missing, `dst` cannot be written, or the copy or the removal
/// fails; a failed copy leaves `src` in place.
pub fn move_path(src: &str, dst: &str) -> io::Result<()> {
    match std::fs::rename(src, dst) {
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            let meta = std::fs::symlink_metadata(src)?;
            if meta.file_type().is_symlink() {
                platform_symlink(&std::fs::read_link(src)?, Path::new(dst))?;
                std::fs::remove_file(src)
            } else if meta.is_dir() {
                copy_dir(src, dst)?;
                std::fs::remove_dir_all(src)
            } else {
                std::fs::copy(src, dst)?;
                std::fs::remove_file(src)
            }
        }
        other => other,
    }
}

/// Sets the owner and group of `path`, leaving each one `None` names alone.
/// A symbolic link is followed.
///
/// # Errors
/// `path` is missing, the caller may not change its owner, or the platform
/// has no Unix ownership.
pub fn chown(path: &str, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
    platform_chown(Path::new(path), uid, gid)
}

// Platform: Unix files carry an owner and a group.
#[cfg(unix)]
fn platform_chown(path: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
    std::os::unix::fs::chown(path, uid, gid)
}

// Platform: Windows ownership is a security descriptor, not a uid and gid.
#[cfg(not(unix))]
fn platform_chown(_path: &Path, _uid: Option<u32>, _gid: Option<u32>) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "chown: this platform has no Unix owner and group",
    ))
}

/// The space of the filesystem holding `path`, in bytes: its total size, the
/// space free, and the space free to an unprivileged caller.
///
/// # Errors
/// `path` is missing or the platform reports no filesystem space.
pub fn disk_space(path: &str) -> io::Result<[u64; 3]> {
    platform_disk_space(path)
}

#[cfg(unix)]
fn platform_disk_space(path: &str) -> io::Result<[u64; 3]> {
    let c_path = std::ffi::CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path holds a NUL byte"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c_path` is a NUL-terminated path and `stat` is writable
    // storage the size of the `statvfs` the call fills.
    let status = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `statvfs` answered 0, so it filled `stat`.
    let stat = unsafe { stat.assume_init() };
    #[allow(
        clippy::useless_conversion,
        reason = "the field widths differ by platform; the conversion widens where they are narrow"
    )]
    let block = u64::from(stat.f_frsize);
    #[allow(clippy::useless_conversion, reason = "as above")]
    let count = |blocks: libc::fsblkcnt_t| u64::from(blocks).saturating_mul(block);
    Ok([
        count(stat.f_blocks),
        count(stat.f_bfree),
        count(stat.f_bavail),
    ])
}

#[cfg(windows)]
fn platform_disk_space(path: &str) -> io::Result<[u64; 3]> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is a NUL-terminated UTF-16 path and the three out
    // pointers are live `u64`s the call writes.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut available,
            &raw mut total,
            &raw mut free,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok([total, free, available])
}

// Platform: a target with neither `statvfs` nor `GetDiskFreeSpaceExW` reports
// no filesystem space.
#[cfg(not(any(unix, windows)))]
fn platform_disk_space(_path: &str) -> io::Result<[u64; 3]> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}
