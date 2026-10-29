//! Runtime support for `std::fs` - filesystem walking + mutation
//! helpers on top of `std::fs`.

#![allow(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self as stdfs, Metadata};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{Receiver, Sender, channel};

#[cfg(not(target_arch = "wasm32"))]
use notify::{
    Event as NotifyEvent, EventKind as NotifyEventKind, RecommendedWatcher, RecursiveMode,
    Watcher as NotifyWatcherTrait,
};
#[cfg(not(target_arch = "wasm32"))]
use parking_lot::Mutex;
use parking_lot::RwLock;

pub use std::fs::{File, OpenOptions};

#[cfg(any(unix, windows))]
const ENCODED_PATH_PREFIX: &str = "@gossamer-path:x";
const ESCAPED_PATH_PREFIX: &str = "@gossamer-path:u";

/// Encodes an operating-system path into a Gossamer `String` without losing
/// non-UTF-8 bytes. Ordinary UTF-8 paths are returned unchanged.
#[must_use]
pub fn encode_path(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        if let Ok(text) = std::str::from_utf8(bytes) {
            return escape_reserved_path_prefix(text);
        }
        let mut encoded = String::with_capacity(ENCODED_PATH_PREFIX.len() + bytes.len() * 2);
        encoded.push_str(ENCODED_PATH_PREFIX);
        for byte in bytes {
            use std::fmt::Write;
            write!(encoded, "{byte:02X}").expect("writing to String cannot fail");
        }
        encoded
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if let Some(text) = path.to_str() {
            return escape_reserved_path_prefix(text);
        }
        let mut encoded = String::from(ENCODED_PATH_PREFIX);
        for unit in path.as_os_str().encode_wide() {
            use std::fmt::Write;
            write!(encoded, "{unit:04X}").expect("writing to String cannot fail");
        }
        encoded
    }
    #[cfg(not(any(unix, windows)))]
    {
        escape_reserved_path_prefix(&path.to_string_lossy())
    }
}

/// Decodes a path produced by [`encode_path`].
#[must_use]
pub fn decode_path(path: &str) -> PathBuf {
    if let Some(text) = path.strip_prefix(ESCAPED_PATH_PREFIX) {
        return PathBuf::from(format!("@gossamer-path:{text}"));
    }
    #[cfg(not(any(unix, windows)))]
    {
        PathBuf::from(path)
    }
    #[cfg(any(unix, windows))]
    {
        let Some(hex) = path.strip_prefix(ENCODED_PATH_PREFIX) else {
            return PathBuf::from(path);
        };
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let bytes = decode_hex(hex, 2).unwrap_or_else(|| path.as_bytes().to_vec());
            PathBuf::from(std::ffi::OsString::from_vec(bytes))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let units = decode_hex_units(hex).unwrap_or_else(|| path.encode_utf16().collect());
            PathBuf::from(std::ffi::OsString::from_wide(&units))
        }
    }
}

fn escape_reserved_path_prefix(path: &str) -> String {
    path.strip_prefix("@gossamer-path:").map_or_else(
        || path.to_string(),
        |rest| format!("{ESCAPED_PATH_PREFIX}{rest}"),
    )
}

#[cfg(unix)]
fn decode_hex(hex: &str, width: usize) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(width) {
        return None;
    }
    (0..hex.len())
        .step_by(width)
        .map(|start| u8::from_str_radix(&hex[start..start + width], 16).ok())
        .collect()
}

#[cfg(windows)]
fn decode_hex_units(hex: &str) -> Option<Vec<u16>> {
    if !hex.len().is_multiple_of(4) {
        return None;
    }
    (0..hex.len())
        .step_by(4)
        .map(|start| u16::from_str_radix(&hex[start..start + 4], 16).ok())
        .collect()
}

static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Directory entry surfaced by [`read_dir`].
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Full path to the entry.
    pub path: PathBuf,
    /// File name within the parent directory.
    pub name: String,
    /// `true` when the entry is a directory.
    pub is_dir: bool,
    /// `true` when the entry is a regular file.
    pub is_file: bool,
    /// `true` when the entry is a symlink.
    pub is_symlink: bool,
}

/// Portable kind of a filesystem node.
///
/// Unlike [`Metadata`], this is available from every [`FileSystem`], including
/// the in-memory and embedded implementations below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A regular byte stream.
    File,
    /// A directory containing named entries.
    Directory,
    /// A symbolic link on an operating-system filesystem.
    Symlink,
    /// A filesystem-specific node that is neither a file nor a directory.
    Other,
}

/// Metadata that can be supplied by any [`FileSystem`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// Path used to obtain this information.
    pub path: PathBuf,
    /// Final path component, or an empty string for a filesystem root.
    pub name: String,
    /// Size in bytes. Directories report zero.
    pub len: u64,
    /// Node kind.
    pub kind: FileKind,
}

impl FileInfo {
    /// Returns whether this is a regular file.
    #[must_use]
    pub const fn is_file(&self) -> bool {
        matches!(self.kind, FileKind::File)
    }

    /// Returns whether this is a directory.
    #[must_use]
    pub const fn is_dir(&self) -> bool {
        matches!(self.kind, FileKind::Directory)
    }
}

/// An opened read-only file supplied by a [`FileSystem`].
///
/// This intentionally has only the portable read and stat contract. Callers
/// that need OS-specific handles can opt into `std::fs::File` separately.
pub trait FsFile: Read + Send {
    /// Metadata captured when this file was opened.
    fn info(&self) -> io::Result<FileInfo>;
}

/// A small, object-safe `io/fs`-style filesystem interface.
///
/// Paths are relative to the filesystem root. [`TestFileSystem`],
/// [`EmbeddedAssets`], and [`SubFileSystem`] reject absolute paths, `..`, and
/// backslashes so an untrusted path cannot escape its virtual root. The OS
/// implementation accepts normal platform paths for compatibility with the
/// existing `std::fs` helpers.
pub trait FileSystem: Send + Sync {
    /// Opens a regular file for reading.
    fn open(&self, path: &Path) -> io::Result<Box<dyn FsFile>>;

    /// Returns direct children of a directory in deterministic name order.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;

    /// Returns portable metadata for a path.
    fn metadata(&self, path: &Path) -> io::Result<FileInfo>;

    /// Reads a complete file using [`Self::open`].
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let mut file = self.open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Reads a complete UTF-8 file using [`Self::open`].
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let mut file = self.open(path)?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text)
    }
}

/// Filesystem implementation backed by the host operating system.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsFileSystem;

impl FileSystem for OsFileSystem {
    fn open(&self, path: &Path) -> io::Result<Box<dyn FsFile>> {
        let file = stdfs::File::open(path)?;
        let info = os_file_info(path, &file.metadata()?);
        Ok(Box::new(OsFsFile { file, info }))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for raw in stdfs::read_dir(path)? {
            let raw = raw?;
            let ty = raw.file_type()?;
            out.push(DirEntry {
                path: raw.path(),
                name: encode_path(Path::new(&raw.file_name())),
                is_dir: ty.is_dir(),
                is_file: ty.is_file(),
                is_symlink: ty.is_symlink(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn metadata(&self, path: &Path) -> io::Result<FileInfo> {
        Ok(os_file_info(path, &stdfs::symlink_metadata(path)?))
    }
}

struct OsFsFile {
    file: File,
    info: FileInfo,
}

impl Read for OsFsFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl FsFile for OsFsFile {
    fn info(&self) -> io::Result<FileInfo> {
        Ok(self.info.clone())
    }
}

fn os_file_info(path: &Path, metadata: &Metadata) -> FileInfo {
    let file_type = metadata.file_type();
    let kind = if file_type.is_file() {
        FileKind::File
    } else if file_type.is_dir() {
        FileKind::Directory
    } else if file_type.is_symlink() {
        FileKind::Symlink
    } else {
        FileKind::Other
    };
    FileInfo {
        path: path.to_path_buf(),
        name: path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        len: metadata.len(),
        kind,
    }
}

/// Deterministically walks every descendant of `root` in a portable
/// filesystem. Directories are yielded before their children.
pub fn walk<F>(fs: &dyn FileSystem, root: impl AsRef<Path>, mut visit: F) -> io::Result<()>
where
    F: FnMut(&DirEntry) -> io::Result<()>,
{
    fn descend<F>(fs: &dyn FileSystem, directory: &Path, visit: &mut F) -> io::Result<()>
    where
        F: FnMut(&DirEntry) -> io::Result<()>,
    {
        for entry in fs.read_dir(directory)? {
            visit(&entry)?;
            if entry.is_dir {
                descend(fs, &entry.path, visit)?;
            }
        }
        Ok(())
    }
    descend(fs, root.as_ref(), &mut visit)
}

/// Returns files below `root` whose root-relative paths match `pattern`.
///
/// `*` and `?` do not cross path separators; `**` does. Results are sorted.
pub fn glob_fs(
    fs: &dyn FileSystem,
    root: impl AsRef<Path>,
    pattern: &str,
) -> io::Result<Vec<PathBuf>> {
    let root = root.as_ref().to_path_buf();
    let pattern = normalize_virtual_pattern(pattern)?;
    let mut matches = Vec::new();
    walk(fs, &root, |entry| {
        if !entry.is_file {
            return Ok(());
        }
        let relative = entry.path.strip_prefix(&root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "filesystem returned a path outside root",
            )
        })?;
        if glob_matches(&pattern, &relative.to_string_lossy()) {
            matches.push(entry.path.clone());
        }
        Ok(())
    })?;
    matches.sort();
    Ok(matches)
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    fn matches_at(pattern: &[u8], path: &[u8]) -> bool {
        match pattern {
            [] => path.is_empty(),
            [b'*', b'*', b'/', rest @ ..] => {
                matches_at(rest, path) || (!path.is_empty() && matches_at(pattern, &path[1..]))
            }
            [b'*', b'*', rest @ ..] => {
                matches_at(rest, path) || (!path.is_empty() && matches_at(pattern, &path[1..]))
            }
            [b'*', rest @ ..] => {
                matches_at(rest, path)
                    || (!path.is_empty() && path[0] != b'/' && matches_at(pattern, &path[1..]))
            }
            [b'?', rest @ ..] => {
                !path.is_empty() && path[0] != b'/' && matches_at(rest, &path[1..])
            }
            [byte, rest @ ..] => {
                !path.is_empty() && *byte == path[0] && matches_at(rest, &path[1..])
            }
        }
    }
    matches_at(pattern.as_bytes(), path.as_bytes())
}

fn normalize_virtual_pattern(pattern: &str) -> io::Result<String> {
    if pattern.is_empty()
        || pattern.starts_with('/')
        || pattern.contains('\\')
        || pattern.split('/').any(|part| part == "..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "filesystem glob patterns must be non-empty relative paths without `..`",
        ));
    }
    Ok(pattern.trim_start_matches("./").to_string())
}

fn normalize_virtual_path(path: &Path) -> io::Result<PathBuf> {
    let raw = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "virtual filesystem paths must be valid UTF-8",
        )
    })?;
    if raw.starts_with('/') || raw.contains('\\') || raw.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "virtual filesystem paths must be relative and slash-separated",
        ));
    }
    let mut components = Vec::new();
    for component in raw.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "virtual filesystem paths cannot contain `..`",
                ));
            }
            name => components.push(name),
        }
    }
    // Keep virtual paths slash-separated even on Windows. `PathBuf::push`
    // would translate every separator to `\\`, which both leaks host syntax
    // through this portable API and makes an already-normalized path fail a
    // second validation pass.
    Ok(PathBuf::from(components.join("/")))
}

fn file_info(path: PathBuf, len: u64, kind: FileKind) -> FileInfo {
    FileInfo {
        name: path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        path,
        len,
        kind,
    }
}

struct MemoryFile {
    cursor: Cursor<Vec<u8>>,
    info: FileInfo,
}

impl MemoryFile {
    fn new(bytes: Vec<u8>, info: FileInfo) -> Self {
        Self {
            cursor: Cursor::new(bytes),
            info,
        }
    }
}

impl Read for MemoryFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.cursor.read(buf)
    }
}

impl FsFile for MemoryFile {
    fn info(&self) -> io::Result<FileInfo> {
        Ok(self.info.clone())
    }
}

#[derive(Default)]
struct TestFileState {
    files: BTreeMap<PathBuf, Vec<u8>>,
    directories: BTreeSet<PathBuf>,
}

/// Thread-safe in-memory filesystem for deterministic unit and integration
/// tests. It never reads or mutates host files.
#[derive(Clone)]
pub struct TestFileSystem {
    state: Arc<RwLock<TestFileState>>,
}

impl Default for TestFileSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl TestFileSystem {
    /// Creates an empty filesystem containing only its root directory.
    #[must_use]
    pub fn new() -> Self {
        let mut state = TestFileState::default();
        state.directories.insert(PathBuf::new());
        Self {
            state: Arc::new(RwLock::new(state)),
        }
    }

    /// Creates a filesystem from files, creating each parent directory.
    pub fn from_files<I, P, B>(files: I) -> io::Result<Self>
    where
        I: IntoIterator<Item = (P, B)>,
        P: AsRef<Path>,
        B: AsRef<[u8]>,
    {
        let fs = Self::new();
        for (path, bytes) in files {
            fs.write(path, bytes)?;
        }
        Ok(fs)
    }

    /// Creates `path` and all missing parent directories.
    pub fn create_dir_all(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = normalize_virtual_path(path.as_ref())?;
        let mut state = self.state.write();
        let mut components = Vec::new();
        state.directories.insert(PathBuf::new());
        for component in path.components() {
            components.push(component.as_os_str().to_string_lossy());
            let current = PathBuf::from(components.join("/"));
            if state.files.contains_key(&current) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a file already exists where a directory was requested",
                ));
            }
            state.directories.insert(current);
        }
        Ok(())
    }

    /// Replaces a file's bytes, creating missing parent directories.
    pub fn write(&self, path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
        let path = normalize_virtual_path(path.as_ref())?;
        if path.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot write the filesystem root",
            ));
        }
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        self.create_dir_all(parent)?;
        let mut state = self.state.write();
        if state.directories.contains(&path) {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                "cannot replace a directory with a file",
            ));
        }
        state.files.insert(path, bytes.as_ref().to_vec());
        Ok(())
    }
}

impl FileSystem for TestFileSystem {
    fn open(&self, path: &Path) -> io::Result<Box<dyn FsFile>> {
        let path = normalize_virtual_path(path)?;
        let state = self.state.read();
        let bytes = state.files.get(&path).cloned().ok_or_else(|| {
            let kind = if state.directories.contains(&path) {
                io::ErrorKind::IsADirectory
            } else {
                io::ErrorKind::NotFound
            };
            io::Error::new(kind, "file not found in test filesystem")
        })?;
        let info = file_info(path, bytes.len() as u64, FileKind::File);
        Ok(Box::new(MemoryFile::new(bytes, info)))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let path = normalize_virtual_path(path)?;
        let state = self.state.read();
        if !state.directories.contains(&path) {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "directory not found in test filesystem",
            ));
        }
        let mut entries = BTreeMap::<String, DirEntry>::new();
        for directory in &state.directories {
            if directory.parent() == Some(path.as_path()) && !directory.as_os_str().is_empty() {
                let name = directory
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                entries.insert(
                    name.clone(),
                    DirEntry {
                        path: directory.clone(),
                        name,
                        is_dir: true,
                        is_file: false,
                        is_symlink: false,
                    },
                );
            }
        }
        for file in state.files.keys() {
            if file.parent() == Some(path.as_path()) {
                let name = file.file_name().unwrap().to_string_lossy().into_owned();
                entries.insert(
                    name.clone(),
                    DirEntry {
                        path: file.clone(),
                        name,
                        is_dir: false,
                        is_file: true,
                        is_symlink: false,
                    },
                );
            }
        }
        Ok(entries.into_values().collect())
    }

    fn metadata(&self, path: &Path) -> io::Result<FileInfo> {
        let path = normalize_virtual_path(path)?;
        let state = self.state.read();
        if let Some(bytes) = state.files.get(&path) {
            return Ok(file_info(path, bytes.len() as u64, FileKind::File));
        }
        if state.directories.contains(&path) {
            return Ok(file_info(path, 0, FileKind::Directory));
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "path not found in test filesystem",
        ))
    }
}

/// A compile-time embedded asset.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedAsset {
    /// Slash-separated path inside the asset filesystem.
    pub path: &'static str,
    /// Bytes compiled into the executable.
    pub bytes: &'static [u8],
}

impl EmbeddedAsset {
    /// Creates one embedded asset. Prefer [`macro@crate::embed_assets`] so the bytes are
    /// included by the compiler and path dependencies are tracked.
    #[must_use]
    pub const fn new(path: &'static str, bytes: &'static [u8]) -> Self {
        Self { path, bytes }
    }
}

/// Read-only filesystem backed by bytes embedded in the executable.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedAssets {
    entries: &'static [EmbeddedAsset],
}

impl EmbeddedAssets {
    /// Creates an asset filesystem from statically embedded entries.
    #[must_use]
    pub const fn new(entries: &'static [EmbeddedAsset]) -> Self {
        Self { entries }
    }

    /// Validates portable asset paths and rejects duplicate entries.
    pub fn validate(&self) -> io::Result<()> {
        let mut paths = BTreeSet::<PathBuf>::new();
        for entry in self.entries {
            let path = normalize_virtual_path(Path::new(entry.path))?;
            let conflicts_with_file = paths.iter().any(|existing| {
                existing == &path || existing.starts_with(&path) || path.starts_with(existing)
            });
            if path.as_os_str().is_empty() || conflicts_with_file {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "embedded asset paths must be unique non-root paths without file-directory conflicts",
                ));
            }
            paths.insert(path);
        }
        Ok(())
    }

    /// Looks up an asset without copying its embedded bytes.
    pub fn get(&self, path: impl AsRef<Path>) -> Option<&'static [u8]> {
        let path = normalize_virtual_path(path.as_ref()).ok()?;
        self.entries
            .iter()
            .find(|entry| {
                normalize_virtual_path(Path::new(entry.path))
                    .ok()
                    .as_deref()
                    == Some(path.as_path())
            })
            .map(|entry| entry.bytes)
    }

    fn is_directory(&self, path: &Path) -> bool {
        path.as_os_str().is_empty()
            || self.entries.iter().any(|entry| {
                normalize_virtual_path(Path::new(entry.path))
                    .ok()
                    .and_then(|entry_path| entry_path.parent().map(Path::to_path_buf))
                    .is_some_and(|parent| parent == path || parent.starts_with(path))
            })
    }
}

impl FileSystem for EmbeddedAssets {
    fn open(&self, path: &Path) -> io::Result<Box<dyn FsFile>> {
        self.validate()?;
        let path = normalize_virtual_path(path)?;
        let bytes = self.get(&path).ok_or_else(|| {
            let kind = if self.is_directory(&path) {
                io::ErrorKind::IsADirectory
            } else {
                io::ErrorKind::NotFound
            };
            io::Error::new(kind, "asset not found")
        })?;
        let info = file_info(path, bytes.len() as u64, FileKind::File);
        Ok(Box::new(MemoryFile::new(bytes.to_vec(), info)))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.validate()?;
        let path = normalize_virtual_path(path)?;
        if !self.is_directory(&path) {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "asset directory not found",
            ));
        }
        let mut entries = BTreeMap::<String, DirEntry>::new();
        for asset in self.entries {
            let asset_path = Path::new(asset.path);
            let Ok(relative) = asset_path.strip_prefix(&path) else {
                continue;
            };
            let Some(first) = relative.components().next() else {
                continue;
            };
            let child = if path.as_os_str().is_empty() {
                PathBuf::from(first.as_os_str())
            } else {
                PathBuf::from(format!(
                    "{}/{}",
                    path.to_string_lossy(),
                    first.as_os_str().to_string_lossy()
                ))
            };
            let name = first.as_os_str().to_string_lossy().into_owned();
            let is_file = relative.components().count() == 1;
            entries.entry(name.clone()).or_insert(DirEntry {
                path: child,
                name,
                is_dir: !is_file,
                is_file,
                is_symlink: false,
            });
        }
        Ok(entries.into_values().collect())
    }

    fn metadata(&self, path: &Path) -> io::Result<FileInfo> {
        self.validate()?;
        let path = normalize_virtual_path(path)?;
        if let Some(bytes) = self.get(&path) {
            return Ok(file_info(path, bytes.len() as u64, FileKind::File));
        }
        if self.is_directory(&path) {
            return Ok(file_info(path, 0, FileKind::Directory));
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "asset not found"))
    }
}

/// A view rooted at a directory of another filesystem.
#[derive(Debug, Clone)]
pub struct SubFileSystem<F> {
    inner: F,
    root: PathBuf,
}

impl<F: FileSystem> SubFileSystem<F> {
    /// Creates a filesystem view rooted at `root`, which must be a directory.
    pub fn new(inner: F, root: impl AsRef<Path>) -> io::Result<Self> {
        let root = normalize_virtual_path(root.as_ref())?;
        if !inner.metadata(&root)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "sub-filesystem root is not a directory",
            ));
        }
        Ok(Self { inner, root })
    }

    fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
        let path = normalize_virtual_path(path)?;
        if self.root.as_os_str().is_empty() {
            Ok(path)
        } else if path.as_os_str().is_empty() {
            Ok(self.root.clone())
        } else {
            Ok(PathBuf::from(format!(
                "{}/{}",
                self.root.to_string_lossy(),
                path.to_string_lossy()
            )))
        }
    }

    /// Returns the backing filesystem.
    #[must_use]
    pub const fn inner(&self) -> &F {
        &self.inner
    }
}

impl<F: FileSystem> FileSystem for SubFileSystem<F> {
    fn open(&self, path: &Path) -> io::Result<Box<dyn FsFile>> {
        Ok(Box::new(SubFsFile {
            inner: self.inner.open(&self.resolve(path)?)?,
            root: self.root.clone(),
        }))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut entries = self.inner.read_dir(&self.resolve(path)?)?;
        for entry in &mut entries {
            entry.path = entry
                .path
                .strip_prefix(&self.root)
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "backing filesystem returned a path outside the sub-filesystem root",
                    )
                })?
                .to_path_buf();
        }
        Ok(entries)
    }

    fn metadata(&self, path: &Path) -> io::Result<FileInfo> {
        let mut info = self.inner.metadata(&self.resolve(path)?)?;
        info.path = info
            .path
            .strip_prefix(&self.root)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "backing filesystem returned metadata outside the sub-filesystem root",
                )
            })?
            .to_path_buf();
        info.name = info
            .path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        Ok(info)
    }
}

struct SubFsFile {
    inner: Box<dyn FsFile>,
    root: PathBuf,
}

impl Read for SubFsFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl FsFile for SubFsFile {
    fn info(&self) -> io::Result<FileInfo> {
        let mut info = self.inner.info()?;
        info.path = info
            .path
            .strip_prefix(&self.root)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "backing filesystem returned metadata outside the sub-filesystem root",
                )
            })?
            .to_path_buf();
        info.name = info
            .path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        Ok(info)
    }
}

/// Builds an [`EmbeddedAssets`] filesystem from files at compile time.
///
/// Paths on the left are reproducible virtual paths, while paths on the right
/// are resolved by Rust's `include_bytes!` relative to the macro invocation.
/// The asset bytes become part of the binary and never require host filesystem
/// access at runtime.
#[macro_export]
macro_rules! embed_assets {
    ($($name:literal => $source:literal),* $(,)?) => {
        {
            static EMBEDDED_ASSETS: &[$crate::fs::EmbeddedAsset] = &[
                $($crate::fs::EmbeddedAsset::new($name, include_bytes!($source))),*
            ];
            $crate::fs::EmbeddedAssets::new(EMBEDDED_ASSETS)
        }
    };
}

/// Lists the direct children of `path`. Does not recurse.
pub fn read_dir(path: impl AsRef<Path>) -> io::Result<Vec<DirEntry>> {
    let mut out = Vec::new();
    for raw in stdfs::read_dir(path)? {
        let raw = raw?;
        let ty = raw.file_type()?;
        out.push(DirEntry {
            path: raw.path(),
            name: encode_path(Path::new(&raw.file_name())),
            is_dir: ty.is_dir(),
            is_file: ty.is_file(),
            is_symlink: ty.is_symlink(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Recursively walks `root`, invoking `visit` for every entry.
/// Traversal is depth-first; directories are visited before their
/// children. Returns as soon as `visit` returns an `Err`.
pub fn walk_dir<F>(root: impl AsRef<Path>, mut visit: F) -> io::Result<()>
where
    F: FnMut(&DirEntry) -> io::Result<()>,
{
    let mut stack: Vec<PathBuf> = vec![root.as_ref().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in read_dir(&dir)? {
            visit(&entry)?;
            if entry.is_dir {
                stack.push(entry.path.clone());
            }
        }
    }
    Ok(())
}

/// Policy used when an entry cannot be read during a recursive operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorPolicy {
    #[default]
    Fail,
    Skip,
}

/// Deterministic recursive-walk options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkOptions {
    pub follow_symlinks: bool,
    pub max_depth: Option<usize>,
    pub on_error: ErrorPolicy,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            max_depth: None,
            on_error: ErrorPolicy::Fail,
        }
    }
}

/// Walks descendants in normalized lexical order with explicit symlink,
/// depth, and error behavior. Symlink cycles are detected when following.
pub fn walk_dir_with<F>(
    root: impl AsRef<Path>,
    options: WalkOptions,
    mut visit: F,
) -> io::Result<()>
where
    F: FnMut(&DirEntry) -> io::Result<()>,
{
    let root = root.as_ref().to_path_buf();
    let root_canonical = options
        .follow_symlinks
        .then(|| stdfs::canonicalize(&root))
        .transpose()?;
    let mut stack = vec![(root, 0usize)];
    let mut visited = BTreeSet::new();
    while let Some((dir, depth)) = stack.pop() {
        if options.follow_symlinks {
            match stdfs::canonicalize(&dir) {
                Ok(path)
                    if root_canonical
                        .as_ref()
                        .is_some_and(|root| !path.starts_with(root)) =>
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("walk would leave root through {}", dir.display()),
                    ));
                }
                Ok(path) if !visited.insert(path.clone()) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("symlink loop at {}", path.display()),
                    ));
                }
                Ok(_) => {}
                Err(_error) if options.on_error == ErrorPolicy::Skip => continue,
                Err(error) => return Err(error),
            }
        }
        let mut entries = match read_dir(&dir) {
            Ok(entries) => entries,
            Err(_error) if options.on_error == ErrorPolicy::Skip => continue,
            Err(error) => return Err(error),
        };
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let mut children = Vec::new();
        for entry in entries {
            let child_depth = depth + 1;
            if options.max_depth.is_some_and(|limit| child_depth > limit) {
                continue;
            }
            visit(&entry)?;
            let descend = entry.is_dir
                || (options.follow_symlinks && entry.is_symlink && entry.path.is_dir());
            if descend && options.max_depth.is_none_or(|limit| child_depth < limit) {
                children.push(entry.path);
            }
        }
        for child in children.into_iter().rev() {
            stack.push((child, depth + 1));
        }
    }
    Ok(())
}

/// Creates `path` and every missing ancestor, mirroring `mkdir -p`.
pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    stdfs::create_dir_all(path)
}

/// Removes `path` and everything underneath, if `path` is a
/// directory; or deletes a single file otherwise.
pub fn remove_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        stdfs::remove_dir_all(path)
    } else {
        stdfs::remove_file(path)
    }
}

/// Explicitly named recursive removal alias.
pub fn remove_tree(path: impl AsRef<Path>) -> io::Result<()> {
    remove_all(path)
}

/// Metadata and symlink policy for [`copy_tree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyTreeOptions {
    pub preserve_permissions: bool,
    pub follow_symlinks: bool,
}

impl Default for CopyTreeOptions {
    fn default() -> Self {
        Self {
            preserve_permissions: true,
            follow_symlinks: false,
        }
    }
}

/// Recursively copies one directory tree. Existing destination files are
/// replaced, symlinks are rejected unless following was explicitly requested,
/// and a failure reports the path at which the partial copy stopped.
pub fn copy_tree(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    options: CopyTreeOptions,
) -> io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    if !src.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copy_tree source is not a directory",
        ));
    }
    stdfs::create_dir_all(dst)?;
    walk_dir_with(
        src,
        WalkOptions {
            follow_symlinks: options.follow_symlinks,
            ..WalkOptions::default()
        },
        |entry| {
            let relative = entry.path.strip_prefix(src).map_err(io::Error::other)?;
            let target = dst.join(relative);
            if entry.is_symlink && !options.follow_symlinks {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("copy_tree refuses symlink {}", entry.path.display()),
                ));
            }
            let followed_dir = entry.is_symlink && options.follow_symlinks && entry.path.is_dir();
            if entry.is_dir || followed_dir {
                stdfs::create_dir_all(&target)?;
            } else if entry.is_file || (entry.is_symlink && options.follow_symlinks) {
                if let Some(parent) = target.parent() {
                    stdfs::create_dir_all(parent)?;
                }
                stdfs::copy(&entry.path, &target)?;
                if options.preserve_permissions {
                    stdfs::set_permissions(&target, stdfs::metadata(&entry.path)?.permissions())?;
                }
            }
            Ok(())
        },
    )
}

/// Copies `src` to `dst`, creating the destination's parent dirs if
/// needed. Returns the number of bytes copied.
pub fn copy(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<u64> {
    let dst = dst.as_ref();
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            stdfs::create_dir_all(parent)?;
        }
    }
    stdfs::copy(src, dst)
}

/// Renames `src` to `dst`.
pub fn rename(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    stdfs::rename(src, dst)
}

/// Returns the [`Metadata`] for `path`.
pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    stdfs::metadata(path)
}

/// Reads the entire contents of `path` into a string. Routes the
/// blocking read through the goroutine-aware blocking thread pool
/// so the calling worker P slot is freed for other goroutines.
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    let path = path.as_ref().to_path_buf();
    crate::blocking_pool::run(move || {
        let mut file = stdfs::File::open(&path)?;
        let mut out = String::new();
        file.read_to_string(&mut out)?;
        out.shrink_to_fit();
        Ok(out)
    })
}

/// Writes `contents` to `path`, truncating any existing file and
/// creating parent directories if needed. Same blocking-pool dispatch
/// as [`read_to_string`].
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    let bytes = contents.as_ref().to_vec();
    crate::blocking_pool::run(move || {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                stdfs::create_dir_all(parent)?;
            }
        }
        let mut file = stdfs::File::create(&path)?;
        file.write_all(&bytes)?;
        Ok(())
    })
}

/// Returns `true` iff `path` exists.
pub fn exists(path: impl AsRef<Path>) -> bool {
    path.as_ref().exists()
}

/// Reads the entire contents of `path` into a byte vector.
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let path = path.as_ref().to_path_buf();
    crate::blocking_pool::run(move || {
        let mut bytes = stdfs::read(&path)?;
        bytes.shrink_to_fit();
        Ok(bytes)
    })
}

/// `true` iff `path` exists and is a regular file.
#[must_use]
pub fn is_file(path: impl AsRef<Path>) -> bool {
    path.as_ref().is_file()
}

/// `true` iff `path` exists and is a directory.
#[must_use]
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    path.as_ref().is_dir()
}

/// `true` iff `path` exists and is a symbolic link.
#[must_use]
pub fn is_symlink(path: impl AsRef<Path>) -> bool {
    stdfs::symlink_metadata(path.as_ref()).is_ok_and(|m| m.file_type().is_symlink())
}

/// File size in bytes, or 0 if `path` cannot be stat'd.
#[must_use]
pub fn file_size(path: impl AsRef<Path>) -> u64 {
    stdfs::metadata(path).map_or(0, |m| m.len())
}

/// Resolves `path` to an absolute, symlink-free form.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<String> {
    stdfs::canonicalize(path).map(|p| gossamer_runtime::canonical_path::display(&p))
}

/// Creates a single directory at `path`. Fails if a parent is
/// missing - use [`create_dir_all`] for the recursive form.
pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    stdfs::create_dir(path)
}

/// Removes a single file.
pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    stdfs::remove_file(path)
}

/// Removes an empty directory.
pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    stdfs::remove_dir(path)
}

/// Recursively removes a directory and its contents.
pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    stdfs::remove_dir_all(path)
}

/// Returns paths matching the glob `pattern`. Supports `*`, `?`,
/// `[abc]`, and `**` (recursive directory match). The pattern is
/// rooted at the current working directory unless it begins
/// with `/`.
pub fn glob(pattern: &str) -> io::Result<Vec<String>> {
    crate::path::glob(pattern)
}

/// Resolves all symlinks along `path` and returns the canonical
/// absolute path. Same shape as [`canonicalize`] but mirrors Go's
/// `filepath.EvalSymlinks` name.
pub fn eval_symlinks(path: impl AsRef<Path>) -> io::Result<String> {
    crate::path::eval_symlinks(path)
}

/// Creates a hard link `dst` pointing at the same inode as `src`.
/// Wraps [`std::fs::hard_link`].
pub fn hard_link(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    stdfs::hard_link(src, dst)
}

/// Reads a symbolic link without resolving its target.
pub fn read_link(path: impl AsRef<Path>) -> io::Result<String> {
    stdfs::read_link(path).and_then(|path| {
        path.into_os_string().into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "symbolic-link target is not valid UTF-8",
            )
        })
    })
}

/// Creates a symbolic link to a file.
#[cfg(unix)]
pub fn symlink_file(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}
#[cfg(windows)]
pub fn symlink_file(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    std::os::windows::fs::symlink_file(src, dst)
}
#[cfg(not(any(unix, windows)))]
pub fn symlink_file(_src: impl AsRef<Path>, _dst: impl AsRef<Path>) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links are unsupported on this target",
    ))
}

/// Creates a symbolic link to a directory.
#[cfg(unix)]
pub fn symlink_dir(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}
#[cfg(windows)]
pub fn symlink_dir(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)
}
#[cfg(not(any(unix, windows)))]
pub fn symlink_dir(_src: impl AsRef<Path>, _dst: impl AsRef<Path>) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links are unsupported on this target",
    ))
}

/// Sets the permission bits on `path` from `mode`, in the chmod(2)
/// encoding (e.g. `0o755`).
///
/// Windows has no permission bits: only the owner write bit is
/// meaningful there, and it sets or clears the read-only attribute.
pub fn set_permissions_mode(path: impl AsRef<Path>, mode: u32) -> io::Result<()> {
    gossamer_runtime::fs_mode::apply(path.as_ref(), mode)
}

/// The permission bits of `path`, in the chmod(2) encoding.
///
/// On Windows the read-only attribute is widened into the bits an
/// equivalent Unix path would carry, so one value tests and re-applies
/// on both.
pub fn permissions_mode(path: impl AsRef<Path>) -> io::Result<u32> {
    gossamer_runtime::fs_mode::read(path.as_ref())
}

/// Creates the directory `path` and gives it exactly `mode`.
///
/// The mode is applied after the directory exists, so the process
/// umask cannot mask a bit out of it - `mkdir -m 0777` is the same two
/// steps, and a directory a tool requires to be world-writable is
/// world-writable however the umask is set.
pub fn create_dir_mode(path: impl AsRef<Path>, mode: u32) -> io::Result<()> {
    gossamer_runtime::fs_mode::create_dir(path.as_ref(), mode)
}

/// Creates `path` and every missing parent, giving `mode` to each
/// directory this call creates and leaving one that already existed as
/// it is.
pub fn create_dir_all_mode(path: impl AsRef<Path>, mode: u32) -> io::Result<()> {
    gossamer_runtime::fs_mode::create_dir_all(path.as_ref(), mode)
}

/// Writes `contents` to `path` and gives the file exactly `mode`.
///
/// The file is created with `mode` and then set to it, so it is never
/// more permissive than asked for at any point and the umask cannot
/// leave it less permissive than asked for at the end.
pub fn write_mode(path: impl AsRef<Path>, contents: impl AsRef<[u8]>, mode: u32) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    let bytes = contents.as_ref().to_vec();
    crate::blocking_pool::run(move || gossamer_runtime::fs_mode::write(&path, &bytes, mode))
}

/// Changes the owner and / or group of `path`. Pass `-1` for `uid`
/// or `gid` to leave that side unchanged. Returns
/// `ErrorKind::Unsupported` on non-Unix platforms.
#[cfg(unix)]
pub fn chown(path: impl AsRef<Path>, uid: i64, gid: i64) -> io::Result<()> {
    use nix::unistd::{Gid, Uid};
    let uid_arg = if uid < 0 {
        None
    } else {
        Some(Uid::from_raw(uid as u32))
    };
    let gid_arg = if gid < 0 {
        None
    } else {
        Some(Gid::from_raw(gid as u32))
    };
    nix::unistd::chown(path.as_ref(), uid_arg, gid_arg).map_err(io::Error::from)
}

/// Non-Unix stub for [`chown`]. Returns `ErrorKind::Unsupported`.
#[cfg(not(unix))]
pub fn chown(_path: impl AsRef<Path>, _uid: i64, _gid: i64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "chown is only supported on Unix targets",
    ))
}

/// Writes `bytes` to `path` atomically: the bytes are first written to a
/// sibling temp file, fsync'd, and renamed into place. Unix additionally syncs
/// the containing directory, making the rename durable across a power loss.
/// Other targets retain atomic replacement visibility but have their platform
/// filesystem's crash-durability semantics.
pub fn write_atomic(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    let path = path.as_ref();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "write_atomic: path has no file name",
            )
        })?
        .to_string_lossy()
        .into_owned();
    let nanos = gossamer_runtime::platform::system_time_now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = parent.join(format!(
        "{file_name}.tmp.{}.{nanos}",
        gossamer_runtime::platform::process_id()
    ));

    if !parent.as_os_str().is_empty() && !parent.exists() {
        stdfs::create_dir_all(parent)?;
    }

    let result = (|| -> io::Result<()> {
        let mut file = stdfs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg_attr(target_arch = "wasm32", allow(clippy::drop_non_drop))]
        drop(file);
        stdfs::rename(&tmp, path)?;
        sync_parent_dir(parent)
    })();

    if result.is_err() {
        let _ = stdfs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn sync_parent_dir(parent: &Path) -> io::Result<()> {
    stdfs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the Windows arm shares the fallible signature its Unix twin needs"
)]
fn sync_parent_dir(_parent: &Path) -> io::Result<()> {
    // Windows directory handles need platform-specific sharing flags. Rename
    // is still atomic here; this helper keeps the API portable until that
    // durable directory-sync implementation is available.
    Ok(())
}

/// Kind of filesystem change reported by [`Watcher`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(not(target_arch = "wasm32"))]
pub enum EventKind {
    /// A new file or directory appeared at the path.
    Created,
    /// An existing path's contents or metadata changed.
    Modified,
    /// A file or directory was deleted.
    Removed,
}

/// A single filesystem change observed by [`Watcher`].
#[derive(Debug, Clone)]
#[cfg(not(target_arch = "wasm32"))]
pub struct Event {
    /// Path that changed.
    pub path: String,
    /// Nature of the change.
    pub kind: EventKind,
}

#[cfg(not(target_arch = "wasm32"))]
fn translate_event_kind(kind: NotifyEventKind) -> Option<EventKind> {
    match kind {
        NotifyEventKind::Create(_) => Some(EventKind::Created),
        NotifyEventKind::Modify(_) => Some(EventKind::Modified),
        NotifyEventKind::Remove(_) => Some(EventKind::Removed),
        _ => None,
    }
}

/// Recursive filesystem watcher. Holds an underlying
/// [`notify`] recommended watcher and a channel of translated
/// [`Event`] values. Dropping the watcher stops further
/// notifications.
#[cfg(not(target_arch = "wasm32"))]
pub struct Watcher {
    inner: Mutex<RecommendedWatcher>,
    rx: Mutex<Option<Receiver<Event>>>,
    #[cfg_attr(not(test), allow(dead_code))]
    tx: Sender<Event>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Watcher {
    /// Constructs a new watcher wired to the platform's native
    /// notification backend. The receiver returned by
    /// [`Watcher::events`] yields one event per observed change.
    pub fn new() -> io::Result<Self> {
        let (tx, rx) = channel::<Event>();
        let event_tx = tx.clone();
        let inner = notify::recommended_watcher(move |res: Result<NotifyEvent, notify::Error>| {
            if let Ok(ev) = res {
                if let Some(kind) = translate_event_kind(ev.kind) {
                    for path in ev.paths {
                        let _ = event_tx.send(Event {
                            path: path.to_string_lossy().into_owned(),
                            kind,
                        });
                    }
                }
            }
        })
        .map_err(notify_to_io)?;
        Ok(Self {
            inner: Mutex::new(inner),
            rx: Mutex::new(Some(rx)),
            tx,
        })
    }

    /// Starts watching `path` recursively. The path must exist.
    pub fn add(&self, path: &str) -> io::Result<()> {
        let mut guard = self.inner.lock();
        guard
            .watch(Path::new(path), RecursiveMode::Recursive)
            .map_err(notify_to_io)
    }

    /// Takes ownership of the event receiver. Subsequent calls return
    /// `None` because a `Receiver` cannot be cloned.
    pub fn events(&self) -> Option<Receiver<Event>> {
        self.rx.lock().take()
    }

    /// Internal hook used by tests to inject a synthetic event.
    #[cfg(test)]
    fn inject_for_test(&self, event: Event) {
        let _ = self.tx.send(event);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn notify_to_io(err: notify::Error) -> io::Error {
    match err.kind {
        notify::ErrorKind::Io(e) => e,
        _ => io::Error::other(err.to_string()),
    }
}

/// Read-only memory map of a file. The map is valid for as long
/// as the `Mmap` is alive; dropping it unmaps the region via
/// the platform's `munmap` / `UnmapViewOfFile`.
#[derive(Debug)]
#[cfg(not(target_arch = "wasm32"))]
pub struct Mmap {
    inner: memmap2::Mmap,
}

#[cfg(not(target_arch = "wasm32"))]
impl Mmap {
    /// Returns the mapped bytes as a borrowed slice. The slice is
    /// valid for the lifetime of the `Mmap`.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.inner
    }

    /// Length of the mapped region in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns whether the map covers zero bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Memory-maps `path` for read-only access. Returns
/// `ErrorKind::InvalidInput` if the file is empty (most platforms
/// reject zero-length mappings).
#[cfg(not(target_arch = "wasm32"))]
pub fn mmap_read(path: &str) -> io::Result<Mmap> {
    let file = stdfs::File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "mmap_read: cannot map a zero-length file",
        ));
    }
    // SAFETY: caller guarantees the file's bytes are stable for
    // the duration of the returned `Mmap`. The wrapper is read-only
    // and never aliased mutably.
    #[allow(unsafe_code)]
    let inner = unsafe { memmap2::Mmap::map(&file)? };
    Ok(Mmap { inner })
}

/// Acquires an exclusive (writer) advisory lock on `file`. Blocks
/// until any conflicting lock is released. Locks are advisory -
/// only cooperating processes that also call the lock helpers
/// see them.
#[cfg(not(target_arch = "wasm32"))]
pub fn lock_exclusive(file: &File) -> io::Result<()> {
    file.lock()
}

/// Acquires a shared (reader) advisory lock on `file`. Multiple
/// shared locks may coexist; an exclusive lock blocks them.
#[cfg(not(target_arch = "wasm32"))]
pub fn lock_shared(file: &File) -> io::Result<()> {
    file.lock_shared()
}

/// Non-blocking variant of [`lock_exclusive`]. Returns
/// `ErrorKind::WouldBlock` immediately when a conflicting lock
/// is held.
#[cfg(not(target_arch = "wasm32"))]
pub fn try_lock_exclusive(file: &File) -> io::Result<()> {
    file.try_lock().map_err(try_lock_err_to_io)
}

/// Non-blocking variant of [`lock_shared`]. Returns
/// `ErrorKind::WouldBlock` immediately when a conflicting lock
/// is held.
#[cfg(not(target_arch = "wasm32"))]
pub fn try_lock_shared(file: &File) -> io::Result<()> {
    file.try_lock_shared().map_err(try_lock_err_to_io)
}

/// Maps the standard library's `TryLockError` onto the `io::Result` contract the
/// `try_lock_*` helpers expose. `WouldBlock` is surfaced as
/// `ErrorKind::WouldBlock` on every platform. The standard library
/// normalizes the Windows `ERROR_LOCK_VIOLATION` contention case
/// into this variant, so callers matching on `WouldBlock` get the
/// same shape everywhere.
#[cfg(not(target_arch = "wasm32"))]
fn try_lock_err_to_io(e: stdfs::TryLockError) -> io::Error {
    match e {
        stdfs::TryLockError::WouldBlock => io::ErrorKind::WouldBlock.into(),
        stdfs::TryLockError::Error(err) => err,
    }
}

/// Releases any advisory lock previously taken on `file`. Idempotent
/// - releasing an already-unlocked handle is not an error on POSIX.
#[cfg(not(target_arch = "wasm32"))]
pub fn unlock(file: &File) -> io::Result<()> {
    file.unlock()
}

/// RAII wrapper around a freshly-created temporary directory. The
/// directory and every entry inside it are removed when the handle
/// drops - even if the holder panicked. Mirrors Python's
/// `tempfile.TemporaryDirectory`.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates a new unique directory under the system temp root.
    /// The name uses the process id + a monotonic counter; never
    /// reuses a name within the lifetime of a process.
    pub fn new() -> io::Result<Self> {
        Self::with_prefix("tmp")
    }

    /// Like [`Self::new`] but the directory name carries the
    /// caller-supplied `prefix` for easier identification in
    /// long-lived working directories.
    pub fn with_prefix(prefix: &str) -> io::Result<Self> {
        validate_temp_prefix(prefix)?;
        let n = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = gossamer_runtime::platform::process_id();
        let nanos = gossamer_runtime::platform::system_time_now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut path = gossamer_runtime::platform::temp_dir();
        path.push(format!("gossamer-{prefix}-{pid}-{nanos:x}-{n}"));
        stdfs::create_dir(&path)?;
        Ok(Self { path })
    }

    /// Returns the absolute path to the temporary directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Consumes the wrapper without removing the directory. Used
    /// when ownership transfers (the new owner is responsible for
    /// cleanup).
    #[must_use]
    pub fn into_path(mut self) -> PathBuf {
        let out = std::mem::take(&mut self.path);
        // Prevent Drop from removing the directory.
        std::mem::forget(self);
        out
    }

    /// Removes the directory now and reports cleanup failure.
    pub fn close(mut self) -> io::Result<()> {
        let path = std::mem::take(&mut self.path);
        if path.as_os_str().is_empty() {
            Ok(())
        } else {
            stdfs::remove_dir_all(path)
        }
    }
}

/// Cleanup-owning temporary file. Dropping removes the directory entry;
/// [`TempFile::close`] makes any cleanup failure observable.
#[derive(Debug)]
pub struct TempFile {
    file: Option<File>,
    path: PathBuf,
}

impl TempFile {
    pub fn new() -> io::Result<Self> {
        Self::with_prefix("tmp")
    }

    pub fn with_prefix(prefix: &str) -> io::Result<Self> {
        let (file, path) = temp_file(prefix)?;
        Ok(Self {
            file: Some(file),
            path,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn file(&self) -> &File {
        self.file.as_ref().expect("temporary file is open")
    }

    pub fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("temporary file is open")
    }

    pub fn close(mut self) -> io::Result<()> {
        self.file.take();
        let path = std::mem::take(&mut self.path);
        if path.as_os_str().is_empty() {
            Ok(())
        } else {
            stdfs::remove_file(path)
        }
    }

    #[must_use]
    pub fn keep(mut self) -> (File, PathBuf) {
        let file = self.file.take().expect("temporary file is open");
        let path = std::mem::take(&mut self.path);
        (file, path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        self.file.take();
        if !self.path.as_os_str().is_empty() {
            let _ = stdfs::remove_file(&self.path);
        }
    }
}

/// Creates a unique temporary directory and transfers cleanup responsibility
/// to the caller. Pair the returned path with [`remove_dir_all`] after the
/// test or operation completes. [`TempDir`] remains the preferred Rust API
/// when RAII cleanup is available.
pub fn temp_dir(prefix: &str) -> io::Result<PathBuf> {
    TempDir::with_prefix(prefix).map(TempDir::into_path)
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = stdfs::remove_dir_all(&self.path);
        }
    }
}

/// Creates a freshly-named temporary file under the system temp
/// root and returns `(File, PathBuf)`. The caller is responsible
/// for removing the file when finished - pair with [`TempDir`] for
/// automatic cleanup. Mirrors Python's `tempfile.mkstemp` (sans
/// the file-descriptor return).
pub fn temp_file(prefix: &str) -> io::Result<(File, PathBuf)> {
    validate_temp_prefix(prefix)?;
    let n = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = gossamer_runtime::platform::process_id();
    let nanos = gossamer_runtime::platform::system_time_now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut path = gossamer_runtime::platform::temp_dir();
    path.push(format!("gossamer-{prefix}-{pid}-{nanos:x}-{n}"));
    let file = stdfs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .read(true)
        .open(&path)?;
    Ok((file, path))
}

/// Rejects a caller-controlled prefix that could turn a generated temporary
/// name into a path outside the system temporary root. This is deliberately
/// platform-independent: a prefix produced on one host must remain safe when
/// the same test runs on another host with different path separators.
fn validate_temp_prefix(prefix: &str) -> io::Result<()> {
    if prefix.contains(['/', '\\', '\0']) || matches!(prefix, "." | "..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "temporary-resource prefix must be a single path component",
        ));
    }
    Ok(())
}

#[cfg(test)]
fn drain_for(rx: &Receiver<Event>, deadline: std::time::Duration) -> Vec<Event> {
    let mut out = Vec::new();
    let start = gossamer_runtime::platform::Instant::now();
    while start.elapsed() < deadline {
        if let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(50)) {
            out.push(ev);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let mut dir = gossamer_runtime::platform::temp_dir();
        dir.push(format!(
            "gos-fs-{tag}-{}",
            gossamer_runtime::platform::process_id()
        ));
        let _ = stdfs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = scratch("wr");
        let path = dir.join("nested/file.txt");
        write(&path, "hello").unwrap();
        let text = read_to_string(&path).unwrap();
        assert_eq!(text, "hello");
        let _ = remove_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn encoded_non_utf8_directory_path_round_trips() {
        use std::os::unix::ffi::OsStringExt;

        let root = scratch("non-utf8-dir");
        stdfs::create_dir_all(&root).unwrap();
        let child = root.join(std::ffi::OsString::from_vec(b"x\xa0y".to_vec()));
        stdfs::create_dir(&child).unwrap();
        stdfs::write(child.join("payload"), b"x").unwrap();

        let entries = read_dir(&root).unwrap();
        assert_eq!(entries.len(), 1);
        let encoded = encode_path(&entries[0].path);
        assert!(encoded.starts_with(ENCODED_PATH_PREFIX));
        let nested = read_dir(decode_path(&encoded)).unwrap();
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].name, "payload");

        stdfs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_filesystem_is_a_deterministic_io_fs_implementation() {
        let fs = TestFileSystem::from_files([
            ("docs/readme.txt", b"read me".as_slice()),
            ("docs/guide.md", b"guide".as_slice()),
            ("static/site.css", b"body {}".as_slice()),
        ])
        .unwrap();
        let filesystem: &dyn FileSystem = &fs;

        assert_eq!(
            filesystem
                .read_to_string(Path::new("docs/readme.txt"))
                .unwrap(),
            "read me"
        );
        let info = filesystem.metadata(Path::new("docs/readme.txt")).unwrap();
        assert_eq!(info.kind, FileKind::File);
        assert_eq!(info.len, 7);

        let mut walked = Vec::new();
        walk(filesystem, "", |entry| {
            walked.push(entry.path.to_string_lossy().into_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            walked,
            [
                "docs",
                "docs/guide.md",
                "docs/readme.txt",
                "static",
                "static/site.css"
            ]
        );
        assert_eq!(
            glob_fs(filesystem, "", "**/*.txt").unwrap(),
            vec![PathBuf::from("docs/readme.txt")]
        );
    }

    #[test]
    fn virtual_filesystems_reject_escape_paths() {
        let fs = TestFileSystem::new();
        for path in ["../secret", "/etc/passwd", r"dir\\file"] {
            let err = fs.write(path, b"nope").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{path}");
        }
        let err = glob_fs(&fs, "", "../*").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn sub_filesystem_hides_its_backing_root() {
        let fs = TestFileSystem::from_files([
            ("public/index.html", b"ok".as_slice()),
            ("private/key.txt", b"secret".as_slice()),
        ])
        .unwrap();
        let sub = SubFileSystem::new(fs, "public").unwrap();
        assert_eq!(sub.read_to_string(Path::new("index.html")).unwrap(), "ok");
        assert_eq!(
            sub.metadata(Path::new("index.html")).unwrap().path,
            PathBuf::from("index.html")
        );
        assert_eq!(
            sub.read_dir(Path::new("")).unwrap()[0].path,
            PathBuf::from("index.html")
        );
        assert_eq!(
            glob_fs(&sub, "", "*.html").unwrap(),
            vec![PathBuf::from("index.html")]
        );
        assert_eq!(
            sub.read_to_string(Path::new("../private/key.txt"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn embedded_assets_are_read_only_filesystem_entries() {
        let assets = crate::embed_assets! {
            "tls/cert.pem" => "../tests/fixtures/test_cert.pem",
            "site/index.html" => "../tests/fixtures/test_key.pem",
        };
        assets.validate().unwrap();
        assert!(
            assets
                .get("tls/cert.pem")
                .unwrap()
                .starts_with(b"-----BEGIN CERTIFICATE-----")
        );
        assert_eq!(
            assets.metadata(Path::new("tls")).unwrap().kind,
            FileKind::Directory
        );
        let mut file = assets.open(Path::new("site/index.html")).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert!(bytes.starts_with(b"-----BEGIN PRIVATE KEY-----"));
        let Err(error) = assets.open(Path::new("missing.txt")) else {
            panic!("missing asset unexpectedly opened");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn embedded_assets_reject_duplicate_and_unsafe_paths() {
        static DUPLICATE: [EmbeddedAsset; 2] = [
            EmbeddedAsset::new("a.txt", b"a"),
            EmbeddedAsset::new("a.txt", b"b"),
        ];
        static UNSAFE_PATH: [EmbeddedAsset; 1] = [EmbeddedAsset::new("../secret", b"no")];
        static FILE_DIRECTORY_CONFLICT: [EmbeddedAsset; 2] = [
            EmbeddedAsset::new("site", b"file"),
            EmbeddedAsset::new("site/index.html", b"nested"),
        ];
        let duplicate = EmbeddedAssets::new(&DUPLICATE);
        assert_eq!(
            duplicate.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let unsafe_path = EmbeddedAssets::new(&UNSAFE_PATH);
        assert_eq!(
            unsafe_path.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let file_directory_conflict = EmbeddedAssets::new(&FILE_DIRECTORY_CONFLICT);
        assert_eq!(
            file_directory_conflict.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn temp_resources_are_unique_and_explicitly_cleanable() {
        let dir = temp_dir("gossamer-fs-test").unwrap();
        assert!(dir.is_dir());

        let (file, path) = temp_file("gossamer-fs-test").unwrap();
        assert!(path.is_file());
        assert_ne!(dir, path);
        drop(file);

        stdfs::remove_file(&path).unwrap();
        stdfs::remove_dir_all(&dir).unwrap();
        assert!(!path.exists());
        assert!(!dir.exists());
    }

    #[test]
    fn cleanup_owning_temp_resources_remove_on_drop_and_close() {
        let dir_path = {
            let dir = TempDir::with_prefix("owned-dir").unwrap();
            write(dir.path().join("nested/file.txt"), b"data").unwrap();
            dir.path().to_path_buf()
        };
        assert!(!dir_path.exists());
        let file = TempFile::with_prefix("owned-file").unwrap();
        let file_path = file.path().to_path_buf();
        file.close().unwrap();
        assert!(!file_path.exists());
    }

    #[test]
    fn copy_tree_preserves_layout_and_rejects_non_directory_source() {
        let root = TempDir::with_prefix("copy-tree").unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");
        write(src.join("a/b.txt"), b"payload").unwrap();
        copy_tree(&src, &dst, CopyTreeOptions::default()).unwrap();
        assert_eq!(read_to_string(dst.join("a/b.txt")).unwrap(), "payload");
        assert_eq!(
            copy_tree(
                src.join("a/b.txt"),
                root.path().join("bad"),
                CopyTreeOptions::default()
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn walk_options_bound_depth_deterministically() {
        let root = TempDir::with_prefix("walk-depth").unwrap();
        write(root.path().join("a/b/file.txt"), b"payload").unwrap();
        let mut paths = Vec::new();
        walk_dir_with(
            root.path(),
            WalkOptions {
                max_depth: Some(1),
                ..WalkOptions::default()
            },
            |entry| {
                paths.push(entry.path.strip_prefix(root.path()).unwrap().to_path_buf());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(paths, [PathBuf::from("a")]);
    }

    #[cfg(unix)]
    #[test]
    fn following_symlink_cannot_escape_walk_root() {
        let root = TempDir::with_prefix("walk-root").unwrap();
        let outside = TempDir::with_prefix("walk-outside").unwrap();
        write(outside.path().join("secret.txt"), b"secret").unwrap();
        symlink_dir(outside.path(), root.path().join("escape")).unwrap();
        let error = walk_dir_with(
            root.path(),
            WalkOptions {
                follow_symlinks: true,
                ..WalkOptions::default()
            },
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn temp_resource_prefix_rejects_path_components() {
        for prefix in ["../escape", "nested/path", r"nested\\path", ".", ".."] {
            let err = temp_dir(prefix).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{prefix:?}");
            let err = temp_file(prefix).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{prefix:?}");
        }
    }

    #[test]
    fn walk_dir_visits_every_descendant() {
        let dir = scratch("walk");
        write(dir.join("a/one.txt"), "1").unwrap();
        write(dir.join("a/two.txt"), "2").unwrap();
        write(dir.join("b/three.txt"), "3").unwrap();
        let mut names: Vec<String> = Vec::new();
        walk_dir(&dir, |entry| {
            if entry.is_file {
                names.push(entry.name.clone());
            }
            Ok(())
        })
        .unwrap();
        names.sort();
        assert_eq!(names, vec!["one.txt", "three.txt", "two.txt"]);
        let _ = remove_all(&dir);
    }

    #[test]
    fn copy_creates_missing_parents() {
        let dir = scratch("copy");
        write(dir.join("src.txt"), "hi").unwrap();
        copy(dir.join("src.txt"), dir.join("nested/out.txt")).unwrap();
        assert!(exists(dir.join("nested/out.txt")));
        let _ = remove_all(&dir);
    }

    #[test]
    fn remove_all_deletes_tree() {
        let dir = scratch("rm");
        write(dir.join("a/b/c.txt"), "x").unwrap();
        remove_all(&dir).unwrap();
        assert!(!exists(&dir));
    }

    #[test]
    fn hard_link_creates_second_name_for_same_bytes() {
        let dir = scratch("hardlink-ok");
        let src = dir.join("src.txt");
        let dst = dir.join("dst.txt");
        write(&src, "linked").unwrap();
        hard_link(&src, &dst).unwrap();
        assert!(exists(&dst));
        assert_eq!(read_to_string(&dst).unwrap(), "linked");
        let _ = remove_all(&dir);
    }

    #[test]
    fn hard_link_fails_for_missing_source() {
        let dir = scratch("hardlink-missing");
        stdfs::create_dir_all(&dir).unwrap();
        let err = hard_link(dir.join("nope.txt"), dir.join("dst.txt")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let _ = remove_all(&dir);
    }

    #[test]
    fn hard_link_fails_when_dst_exists() {
        let dir = scratch("hardlink-dup");
        let src = dir.join("src.txt");
        let dst = dir.join("dst.txt");
        write(&src, "a").unwrap();
        write(&dst, "b").unwrap();
        assert!(hard_link(&src, &dst).is_err());
        let _ = remove_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn set_permissions_mode_sets_unix_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perm-ok");
        let path = dir.join("file.sh");
        write(&path, "#!/bin/sh\n").unwrap();
        set_permissions_mode(&path, 0o755).unwrap();
        let mode = metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        let _ = remove_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn set_permissions_mode_changes_to_readonly() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perm-ro");
        let path = dir.join("file.txt");
        write(&path, "ro").unwrap();
        set_permissions_mode(&path, 0o400).unwrap();
        let mode = metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o400);
        // Restore write bit so cleanup can succeed.
        set_permissions_mode(&path, 0o644).unwrap();
        let _ = remove_all(&dir);
    }

    #[test]
    fn create_dir_mode_defeats_the_umask() {
        let dir = scratch("create-dir-mode");
        stdfs::create_dir_all(&dir).unwrap();
        let made = dir.join("shared");
        create_dir_mode(&made, 0o777).unwrap();
        assert!(made.is_dir());
        assert_eq!(permissions_mode(&made).unwrap(), 0o777);
        let _ = remove_all(&dir);
    }

    #[test]
    fn create_dir_all_mode_gives_every_directory_it_creates_the_mode() {
        let dir = scratch("create-dir-all-mode");
        stdfs::create_dir_all(&dir).unwrap();
        let leaf = dir.join("one").join("two").join("three");
        create_dir_all_mode(&leaf, 0o701).unwrap();
        assert!(leaf.is_dir());
        #[cfg(unix)]
        for made in [dir.join("one"), dir.join("one").join("two"), leaf.clone()] {
            assert_eq!(
                permissions_mode(&made).unwrap(),
                0o701,
                "{}",
                made.display()
            );
        }
        // What already exists is left alone, mode included: this call
        // creates directories, it does not re-mode a tree.
        create_dir_all_mode(&leaf, 0o700).unwrap();
        #[cfg(unix)]
        assert_eq!(permissions_mode(&leaf).unwrap(), 0o701);
        #[cfg(unix)]
        set_permissions_mode(dir.join("one"), 0o755).unwrap();
        let _ = remove_all(&dir);
    }

    #[test]
    fn write_mode_writes_the_bytes_and_the_bits() {
        let dir = scratch("write-mode");
        let path = dir.join("secret.txt");
        stdfs::create_dir_all(&dir).unwrap();
        write_mode(&path, b"hello", 0o600).unwrap();
        assert_eq!(read_to_string(&path).unwrap(), "hello");
        #[cfg(unix)]
        assert_eq!(permissions_mode(&path).unwrap(), 0o600);
        // A rewrite states the mode again rather than inheriting
        // whatever the file already carried.
        write_mode(&path, b"bye", 0o644).unwrap();
        assert_eq!(read_to_string(&path).unwrap(), "bye");
        #[cfg(unix)]
        assert_eq!(permissions_mode(&path).unwrap(), 0o644);
        let _ = remove_all(&dir);
    }

    #[test]
    fn permissions_mode_reads_back_what_was_written() {
        let dir = scratch("permissions-mode");
        let path = dir.join("file.txt");
        write(&path, "x").unwrap();
        set_permissions_mode(&path, 0o640).unwrap();
        let mode = permissions_mode(&path).unwrap();
        #[cfg(unix)]
        assert_eq!(mode, 0o640);
        // Windows carries one bit of this, and it is the one every
        // platform agrees on: the path is writable.
        assert_ne!(mode & 0o200, 0, "{mode:o}");
        set_permissions_mode(&path, 0o444).unwrap();
        assert_eq!(permissions_mode(&path).unwrap() & 0o200, 0);
        // Restored so the fixture can be removed.
        set_permissions_mode(&path, 0o644).unwrap();
        let _ = remove_all(&dir);
    }

    #[test]
    fn permissions_mode_fails_for_a_missing_path() {
        let dir = scratch("permissions-mode-missing");
        let err = permissions_mode(dir.join("nope.txt")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn set_permissions_mode_fails_for_missing_path() {
        let dir = scratch("perm-missing");
        stdfs::create_dir_all(&dir).unwrap();
        let err = set_permissions_mode(dir.join("nope.txt"), 0o644).unwrap_err();
        // Windows carries the read-only attribute, so a missing path
        // there fails the same way it does on Unix: there is nothing to
        // set it on. Only a target with neither mechanism reports the
        // operation itself as unsupported.
        #[cfg(any(unix, windows))]
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        #[cfg(not(any(unix, windows)))]
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        let _ = remove_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn chown_minus_one_leaves_owner_unchanged() {
        use std::os::unix::fs::MetadataExt;
        let dir = scratch("chown-noop");
        let path = dir.join("file.txt");
        write(&path, "x").unwrap();
        let before = metadata(&path).unwrap();
        chown(&path, -1, -1).unwrap();
        let after = metadata(&path).unwrap();
        assert_eq!(before.uid(), after.uid());
        assert_eq!(before.gid(), after.gid());
        let _ = remove_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn chown_to_current_uid_gid_round_trips() {
        use std::os::unix::fs::MetadataExt;
        let dir = scratch("chown-self");
        let path = dir.join("file.txt");
        write(&path, "x").unwrap();
        let md = metadata(&path).unwrap();
        let uid = i64::from(md.uid());
        let gid = i64::from(md.gid());
        chown(&path, uid, gid).unwrap();
        let after = metadata(&path).unwrap();
        assert_eq!(i64::from(after.uid()), uid);
        assert_eq!(i64::from(after.gid()), gid);
        let _ = remove_all(&dir);
    }

    #[test]
    fn chown_fails_for_missing_path() {
        let dir = scratch("chown-missing");
        stdfs::create_dir_all(&dir).unwrap();
        assert!(chown(dir.join("nope.txt"), -1, -1).is_err());
        let _ = remove_all(&dir);
    }

    #[test]
    fn write_atomic_writes_exact_bytes() {
        let dir = scratch("atomic-write");
        let path = dir.join("config.bin");
        let payload: Vec<u8> = (0u8..=255u8).collect();
        write_atomic(&path, &payload).unwrap();
        let read_back = read(&path).unwrap();
        assert_eq!(read_back, payload);
        let _ = remove_all(&dir);
    }

    #[test]
    fn write_atomic_leaves_no_temp_file_after_success() {
        let dir = scratch("atomic-cleanup");
        let path = dir.join("config.toml");
        write_atomic(&path, b"[section]\nkey = 1\n").unwrap();
        let mut found_temp = false;
        for entry in read_dir(&dir).unwrap() {
            if entry.name.contains(".tmp.") {
                found_temp = true;
            }
        }
        assert!(!found_temp, "leftover temp file in {dir:?}");
        let _ = remove_all(&dir);
    }

    #[test]
    fn write_atomic_overwrites_existing_atomically() {
        let dir = scratch("atomic-overwrite");
        let path = dir.join("file.txt");
        write(&path, "old contents").unwrap();
        write_atomic(&path, b"new contents").unwrap();
        assert_eq!(read_to_string(&path).unwrap(), "new contents");
        let _ = remove_all(&dir);
    }

    #[test]
    fn write_atomic_creates_missing_parents() {
        let dir = scratch("atomic-parents");
        let path = dir.join("nested/deeper/file.txt");
        write_atomic(&path, b"deep").unwrap();
        assert_eq!(read_to_string(&path).unwrap(), "deep");
        let _ = remove_all(&dir);
    }

    #[test]
    fn watch_injected_event_arrives_on_receiver() {
        let watcher = Watcher::new().unwrap();
        let rx = watcher.events().expect("first events() yields receiver");
        watcher.inject_for_test(Event {
            path: "/tmp/x".to_string(),
            kind: EventKind::Created,
        });
        let events = drain_for(&rx, std::time::Duration::from_millis(200));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "/tmp/x");
        assert_eq!(events[0].kind, EventKind::Created);
    }

    #[test]
    fn watch_events_taken_only_once() {
        let watcher = Watcher::new().unwrap();
        let _ = watcher.events().expect("first call returns Some");
        assert!(watcher.events().is_none());
    }

    #[test]
    fn watch_add_fails_for_missing_path() {
        let watcher = Watcher::new().unwrap();
        let err = watcher.add("/this/path/does/not/exist/zzz").unwrap_err();
        assert!(matches!(
            err.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::Other
        ));
    }

    #[test]
    fn watch_detects_file_creation() {
        let dir = scratch("watch-create");
        stdfs::create_dir_all(&dir).unwrap();
        let watcher = Watcher::new().unwrap();
        let rx = watcher.events().unwrap();
        watcher.add(&dir.to_string_lossy()).unwrap();
        // Give the platform backend a moment to register the watch.
        // There is no synchronous "watch active" ack from FSEvents /
        // ReadDirectoryChangesW / inotify, so a brief settle before the
        // triggering write is the documented platform constraint.
        gossamer_runtime::platform::sleep(std::time::Duration::from_millis(150));
        let file = dir.join("created.txt");
        stdfs::write(&file, b"hello").unwrap();
        // Wait until the matching event arrives, returning as soon as it
        // does. A generous deadline absorbs FSEvents coalescing latency
        // on a loaded CI runner without slowing the common (fast) case.
        let deadline =
            gossamer_runtime::platform::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw = false;
        while gossamer_runtime::platform::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(e) if e.path.ends_with("created.txt") && e.kind == EventKind::Created => {
                    saw = true;
                    break;
                }
                Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(saw, "expected Created event for created.txt within 10s");
        drop(watcher);
        let _ = remove_all(&dir);
    }

    #[test]
    fn watch_event_kind_translation_covers_each_arm() {
        let watcher = Watcher::new().unwrap();
        let rx = watcher.events().unwrap();
        watcher.inject_for_test(Event {
            path: "a".into(),
            kind: EventKind::Created,
        });
        watcher.inject_for_test(Event {
            path: "b".into(),
            kind: EventKind::Modified,
        });
        watcher.inject_for_test(Event {
            path: "c".into(),
            kind: EventKind::Removed,
        });
        let events = drain_for(&rx, std::time::Duration::from_millis(300));
        let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![EventKind::Created, EventKind::Modified, EventKind::Removed]
        );
    }

    #[test]
    fn mmap_read_round_trips_bytes() {
        let dir = scratch("mmap-rt");
        let path = dir.join("data.bin");
        let payload: Vec<u8> = (0u8..=255u8).collect();
        write(&path, &payload).unwrap();
        let mm = mmap_read(&path.to_string_lossy()).unwrap();
        assert_eq!(mm.len(), payload.len());
        assert_eq!(mm.as_slice(), payload.as_slice());
        drop(mm);
        let _ = remove_all(&dir);
    }

    #[test]
    fn mmap_read_rejects_empty_file() {
        let dir = scratch("mmap-empty");
        let path = dir.join("empty.bin");
        write(&path, b"").unwrap();
        let err = mmap_read(&path.to_string_lossy()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let _ = remove_all(&dir);
    }

    #[test]
    fn mmap_read_fails_for_missing_path() {
        let dir = scratch("mmap-missing");
        stdfs::create_dir_all(&dir).unwrap();
        let err = mmap_read(&dir.join("nope.bin").to_string_lossy()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let _ = remove_all(&dir);
    }

    #[test]
    fn mmap_read_is_empty_is_false_for_nonempty() {
        let dir = scratch("mmap-empty-false");
        let path = dir.join("x.bin");
        write(&path, b"abc").unwrap();
        let mm = mmap_read(&path.to_string_lossy()).unwrap();
        assert!(!mm.is_empty());
        assert_eq!(mm.len(), 3);
        drop(mm);
        let _ = remove_all(&dir);
    }

    #[test]
    fn mmap_read_large_file_maps_full_length() {
        let dir = scratch("mmap-large");
        let path = dir.join("big.bin");
        let payload = vec![0x42u8; 64 * 1024];
        write(&path, &payload).unwrap();
        let mm = mmap_read(&path.to_string_lossy()).unwrap();
        assert_eq!(mm.len(), payload.len());
        assert_eq!(mm.as_slice()[0], 0x42);
        assert_eq!(mm.as_slice()[payload.len() - 1], 0x42);
        drop(mm);
        let _ = remove_all(&dir);
    }

    #[test]
    fn lock_exclusive_then_unlock_round_trips() {
        let dir = scratch("lock-rt");
        let path = dir.join("a.txt");
        write(&path, "x").unwrap();
        let f = File::open(&path).unwrap();
        lock_exclusive(&f).unwrap();
        unlock(&f).unwrap();
        drop(f);
        let _ = remove_all(&dir);
    }

    #[test]
    fn try_lock_exclusive_second_holder_fails() {
        let dir = scratch("lock-conflict");
        let path = dir.join("a.txt");
        write(&path, "x").unwrap();
        let f1 = File::open(&path).unwrap();
        let f2 = File::open(&path).unwrap();
        lock_exclusive(&f1).unwrap();
        let err = try_lock_exclusive(&f2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        unlock(&f1).unwrap();
        drop(f1);
        drop(f2);
        let _ = remove_all(&dir);
    }

    #[test]
    fn try_lock_shared_two_holders_succeed() {
        let dir = scratch("lock-shared");
        let path = dir.join("a.txt");
        write(&path, "x").unwrap();
        let f1 = File::open(&path).unwrap();
        let f2 = File::open(&path).unwrap();
        try_lock_shared(&f1).unwrap();
        try_lock_shared(&f2).unwrap();
        unlock(&f1).unwrap();
        unlock(&f2).unwrap();
        drop(f1);
        drop(f2);
        let _ = remove_all(&dir);
    }

    #[test]
    fn try_lock_exclusive_then_shared_conflicts() {
        let dir = scratch("lock-x-then-s");
        let path = dir.join("a.txt");
        write(&path, "x").unwrap();
        let f1 = File::open(&path).unwrap();
        let f2 = File::open(&path).unwrap();
        lock_exclusive(&f1).unwrap();
        let err = try_lock_shared(&f2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        unlock(&f1).unwrap();
        drop(f1);
        drop(f2);
        let _ = remove_all(&dir);
    }

    #[test]
    fn lock_released_after_drop_or_unlock_can_be_reacquired() {
        let dir = scratch("lock-reacquire");
        let path = dir.join("a.txt");
        write(&path, "x").unwrap();
        {
            let f = File::open(&path).unwrap();
            lock_exclusive(&f).unwrap();
            unlock(&f).unwrap();
        }
        let f = File::open(&path).unwrap();
        try_lock_exclusive(&f).unwrap();
        unlock(&f).unwrap();
        drop(f);
        let _ = remove_all(&dir);
    }
}
