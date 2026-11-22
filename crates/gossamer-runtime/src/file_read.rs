//! Whole-file reads in as few system calls as the file allows, shared by the
//! interpreter and the compiled tiers.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Bytes the first read takes before the file's size is asked for. A file
/// that fits, as all of procfs and sysfs and most configuration does, is
/// opened, read, and closed with no size probe at all.
const FIRST_READ: usize = 16 * 1024;

/// The whole of the file at `path`, as `std::fs::read` answers it.
pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = open_for_read(path)?;
    let mut bytes = Vec::with_capacity(FIRST_READ);
    (&mut file)
        .take(FIRST_READ as u64)
        .read_to_end(&mut bytes)?;
    // A file that filled the first read sizes the rest of its buffer from its
    // own length, as `std::fs::read` does from the start.
    if bytes.len() == FIRST_READ {
        file.read_to_end(&mut bytes)?;
    }
    Ok(bytes)
}

/// The whole of the file at `path` as UTF-8 text, as `std::fs::read_to_string`
/// answers it.
pub fn read_to_string(path: &Path) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )
    })
}

/// Opens `path` for reading with close-on-exec set.
#[cfg(all(target_os = "linux", not(miri)))]
pub(crate) fn open_for_read(path: &Path) -> io::Result<File> {
    Ok(File::from(open_cloexec(path, libc::O_RDONLY)?))
}

// Miri models `File::open` but does not interpret the raw `openat` syscall
// used by the Linux close-on-exec fast path.
#[cfg(all(target_os = "linux", miri))]
pub(crate) fn open_for_read(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Opens `path` with `flags` and close-on-exec set.
///
/// musl's `open` follows an `O_CLOEXEC` open with an `fcntl(F_SETFD)` for
/// kernels older than 2.6.23, which ignored the flag; every kernel this runtime
/// supports honors it, so the descriptor comes from the system call directly.
#[cfg(all(target_os = "linux", not(miri)))]
pub(crate) fn open_cloexec(path: &Path, flags: libc::c_int) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "file name contained an unexpected NUL byte",
        )
    })?;
    loop {
        // SAFETY: `path` is a NUL-terminated string that outlives the call, and
        // `openat` reads nothing else.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat,
                libc::AT_FDCWD,
                path.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_LARGEFILE,
            )
        };
        if fd >= 0 {
            let fd = i32::try_from(fd).map_err(|_| io::Error::other("descriptor out of range"))?;
            // SAFETY: `fd` is a descriptor `openat` just answered, owned by
            // nothing else.
            return Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

// Elsewhere the C library's `open` makes no extra call for the flag.
#[cfg(not(target_os = "linux"))]
pub(crate) fn open_for_read(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gos-file-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("scratch file");
        path
    }

    #[test]
    fn reads_answer_what_std_reads_answer() {
        for (name, len) in [
            ("empty", 0),
            ("small", 300),
            ("exact", FIRST_READ),
            ("one-past", FIRST_READ + 1),
            ("large", FIRST_READ * 5 + 17),
        ] {
            let bytes: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
            let path = scratch(name, &bytes);
            assert_eq!(
                read(&path).expect("read"),
                std::fs::read(&path).expect("std read")
            );
            assert_eq!(
                read_to_string(&path).expect("read_to_string"),
                std::fs::read_to_string(&path).expect("std read_to_string")
            );
        }
    }

    #[test]
    fn errors_match_std_errors() {
        let missing = std::env::temp_dir().join("gos-file-read-no-such-file");
        assert_eq!(
            read(&missing).expect_err("missing").kind(),
            std::fs::read(&missing).expect_err("std missing").kind()
        );
        let invalid = scratch("invalid-utf8", &[0xff, 0xfe]);
        assert_eq!(
            read_to_string(&invalid).expect_err("invalid").to_string(),
            std::fs::read_to_string(&invalid)
                .expect_err("std invalid")
                .to_string()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_reads_like_std() {
        let path = Path::new("/proc/self/stat");
        assert!(read_to_string(path).expect("procfs").contains(')'));
    }
}
