//! Runtime support for `std::os`.
//! Wraps the host's `std::env` and `std::fs` interfaces in the error
//! shape Gossamer programs see. Tests run against the host filesystem
//! by way of `std::env::temp_dir()`.

#![forbid(unsafe_code)]

use std::path::Path;

use crate::io::IoError;

/// Returns the program's command-line arguments. The 0th element is
/// the executable path, mirroring `std::env::args`.
#[must_use]
pub fn args() -> Vec<String> {
    std::env::args().collect()
}

/// Returns the value of the named environment variable, or `None` if
/// it is unset or contains invalid Unicode.
#[must_use]
pub fn env(name: &str) -> Option<String> {
    gossamer_runtime::safe_env::env_var(name)
}

/// Sets an environment variable in the current process.
///
/// Routes through `gossamer_runtime::safe_env::set_env`, which
/// contains the Rust-2024 `unsafe std::env::set_var` call in a
/// single audited site so this crate stays
/// `#![forbid(unsafe_code)]`. **Call before spawning any
/// goroutine / thread**; concurrent env reads from other threads
/// or external libraries can otherwise observe a torn value
/// (POSIX `setenv` is not thread-safe by spec).
pub fn set_env(name: &str, value: &str) -> Result<(), IoError> {
    gossamer_runtime::safe_env::set_env(name, value);
    Ok(())
}

/// Removes an environment variable from the current process.
/// Same threading contract as [`set_env`].
pub fn unset_env(name: &str) {
    gossamer_runtime::safe_env::unset_env(name);
}

/// Reads the entire contents of a file into memory.
pub fn read_file(path: &str) -> Result<Vec<u8>, IoError> {
    std::fs::read(path)
        .map(|mut bytes| {
            bytes.shrink_to_fit();
            bytes
        })
        .map_err(|e| IoError::from_std(e, path))
}

/// Reads the entire contents of a file as UTF-8 text.
pub fn read_file_to_string(path: &str) -> Result<String, IoError> {
    std::fs::read_to_string(path)
        .map(|mut text| {
            text.shrink_to_fit();
            text
        })
        .map_err(|e| IoError::from_std(e, path))
}

/// Writes `bytes` to `path`, creating or truncating the file.
pub fn write_file(path: &str, bytes: &[u8]) -> Result<(), IoError> {
    std::fs::write(path, bytes).map_err(|e| IoError::from_std(e, path))
}

/// Removes the file at `path`.
pub fn remove_file(path: &str) -> Result<(), IoError> {
    std::fs::remove_file(path).map_err(|e| IoError::from_std(e, path))
}

/// Renames a file or directory.
pub fn rename(from: &str, to: &str) -> Result<(), IoError> {
    std::fs::rename(from, to).map_err(|e| IoError::from_std(e, &format!("{from} -> {to}")))
}

/// Returns whether `path` exists.
#[must_use]
pub fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// Creates the directory at `path`. Fails if a parent is missing; use
/// [`mkdir_all`] for the recursive version.
pub fn mkdir(path: &str) -> Result<(), IoError> {
    std::fs::create_dir(path).map_err(|e| IoError::from_std(e, path))
}

/// Creates `path` along with any missing parents.
pub fn mkdir_all(path: &str) -> Result<(), IoError> {
    std::fs::create_dir_all(path).map_err(|e| IoError::from_std(e, path))
}

/// Iterates the entries of a directory, returning their names.
pub fn read_dir(path: &str) -> Result<Vec<String>, IoError> {
    let entries = std::fs::read_dir(path).map_err(|e| IoError::from_std(e, path))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| IoError::from_std(e, path))?;
        out.push(entry.file_name().to_string_lossy().into_owned());
    }
    out.sort();
    Ok(out)
}

/// Exits the process with the given status code.
///
/// Wrapped behind a panic in tests by inspecting the documented
/// behaviour rather than calling this directly.
pub fn exit(code: i32) -> ! {
    std::process::exit(code);
}

/// Returns the current working directory.
pub fn cwd() -> Result<String, IoError> {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| IoError::from_std(e, "cwd"))
}

/// Sets the current working directory.
pub fn set_cwd(path: &str) -> Result<(), IoError> {
    std::env::set_current_dir(path).map_err(|e| IoError::from_std(e, path))
}

/// Returns `true` when `path` exists and is a regular file.
#[must_use]
pub fn is_file(path: &str) -> bool {
    Path::new(path).is_file()
}

/// Returns `true` when `path` exists and is a directory.
#[must_use]
pub fn is_dir(path: &str) -> bool {
    Path::new(path).is_dir()
}

/// Returns `true` when `path` is a symbolic link.
#[must_use]
pub fn is_symlink(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// File-size in bytes, or `0` when the path is missing.
#[must_use]
pub fn file_size(path: &str) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// Removes an empty directory at `path`.
pub fn remove_dir(path: &str) -> Result<(), IoError> {
    std::fs::remove_dir(path).map_err(|e| IoError::from_std(e, path))
}

/// Removes a directory and every file inside it recursively.
pub fn remove_dir_all(path: &str) -> Result<(), IoError> {
    std::fs::remove_dir_all(path).map_err(|e| IoError::from_std(e, path))
}

/// Copies `src` to `dst`, returning the number of bytes copied.
pub fn copy(src: &str, dst: &str) -> Result<u64, IoError> {
    std::fs::copy(src, dst).map_err(|e| IoError::from_std(e, &format!("{src} -> {dst}")))
}

/// Returns the canonical absolute form of `path`.
pub fn canonicalize(path: &str) -> Result<String, IoError> {
    std::fs::canonicalize(path)
        .map(|p| gossamer_runtime::canonical_path::display(&p))
        .map_err(|e| IoError::from_std(e, path))
}

/// Returns the value of the `HOME` (or platform equivalent) directory,
/// when set.
#[must_use]
pub fn home() -> Option<String> {
    if let Some(home) = std::env::var_os("HOME") {
        return Some(home.to_string_lossy().into_owned());
    }
    if cfg!(windows) {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return Some(profile.to_string_lossy().into_owned());
        }
    }
    None
}

/// Returns the platform-canonical temporary directory path.
#[must_use]
pub fn temp_dir() -> String {
    gossamer_runtime::platform::temp_dir()
        .to_string_lossy()
        .into_owned()
}

/// Returns the OS family - `"unix"` or `"windows"`. Mirrors
/// Go's `runtime.GOOS` and the family component of Rust's
/// `std::env::consts::FAMILY`.
#[must_use]
pub fn family() -> &'static str {
    std::env::consts::FAMILY
}

/// Returns the target CPU architecture, e.g. `"x86_64"` or
/// `"aarch64"`. Mirrors Go's `runtime.GOARCH`.
#[must_use]
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}
