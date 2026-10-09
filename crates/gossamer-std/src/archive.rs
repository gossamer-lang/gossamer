//! Runtime support for `std::archive` - archive format readers and writers.

#![forbid(unsafe_code)]

/// Tar archive reader and writer.
pub mod tar;
/// ZIP archive reader and writer.
pub mod zip;

pub use gossamer_runtime::codec::archive::Limits;

/// `name` as a relative path that stays inside the directory an archive is
/// extracted to, or `None` for an absolute path, a drive prefix, or a path
/// whose `..` steps leave it.
#[must_use]
pub fn enclosed_path(name: &str) -> Option<String> {
    gossamer_runtime::codec::archive::enclosed_path(name)
}
