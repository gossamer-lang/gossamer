//! Content-addressable cache backing the fetcher.
//!
//! Every cached source tree lives at
//! `~/.gossamer/cache/pkg/<sha256>/source/`. The cache key is the
//! sha256 of the canonical `path\0bytes\0...` serialisation of the
//! file map. The on-disk layout pairs each digest directory with an
//! `id.txt` sidecar so the runtime can re-hash on read and reject
//! silent corruption.
//!
//! The in-memory [`Cache`] is a transient mirror of the disk
//! layer; tests drive it directly when no `GOS_CACHE_DIR` is set.
//! Production builds construct a [`Cache`] anchored on the resolved
//! cache root via [`Cache::with_disk_root`] so a second fetch hits
//! disk and never touches the network.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::id::ProjectId;
use crate::resolver::Resolved;
use crate::sha256;

/// Maximum number of regular files admitted to one materialized package
/// source tree. The public `CachedSource` API is map-based, so keeping this
/// finite is necessary to bound both cache loads and path-dependency walks.
pub const MAX_CACHED_SOURCE_FILES: usize = 4096;
/// Maximum bytes in one file admitted to a materialized package source tree.
pub const MAX_CACHED_SOURCE_FILE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum aggregate payload admitted to one materialized package source tree.
pub const MAX_CACHED_SOURCE_BYTES: usize = 64 * 1024 * 1024;

/// One cached source tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedSource {
    /// Project the source belongs to.
    pub id: ProjectId,
    /// Mapping of file path to file contents.
    pub files: BTreeMap<String, Vec<u8>>,
    /// SHA-256 of the canonical serialisation of `files`.
    pub digest: String,
}

impl CachedSource {
    /// Builds a cached source from a directory-shaped file map.
    /// The digest is a SHA-256 of the concatenation
    /// `path\0bytes\0path\0bytes\0...` in path-sorted order, so equal
    /// inputs produce equal digests across runs and platforms.
    #[must_use]
    pub fn build(id: ProjectId, files: BTreeMap<String, Vec<u8>>) -> Self {
        let digest = compute_digest(&files);
        Self { id, files, digest }
    }
}

/// Incremental canonical source-tree hasher.
///
/// Call [`Self::update_file`] in sorted path order. The public
/// [`CachedSource`] result still stores a `BTreeMap`, but callers that already
/// stream entries in canonical order can compute the same digest without a
/// second map walk.
pub struct SourceTreeDigest {
    hasher: sha256::Hasher,
}

impl Default for SourceTreeDigest {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceTreeDigest {
    /// Starts a new source-tree digest.
    #[must_use]
    pub fn new() -> Self {
        Self {
            hasher: sha256::Hasher::new(),
        }
    }

    /// Adds one canonical package path and its bytes.
    pub fn update_file(&mut self, path: &str, bytes: &[u8]) {
        self.hasher.update(path.as_bytes());
        self.hasher.update(&[0]);
        self.hasher.update(bytes);
        self.hasher.update(&[0]);
    }

    /// Finishes and returns a lowercase SHA-256 hex digest.
    #[must_use]
    pub fn finalize_hex(self) -> String {
        self.hasher.finalize_hex()
    }
}

fn compute_digest(files: &BTreeMap<String, Vec<u8>>) -> String {
    compute_digest_from_iter(
        files
            .iter()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice())),
    )
}

pub(crate) fn compute_digest_from_iter<'a>(
    files: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> String {
    let mut hasher = SourceTreeDigest::new();
    for (path, bytes) in files {
        hasher.update_file(path, bytes);
    }
    hasher.finalize_hex()
}

/// Returns the cache directory, honouring `$GOS_CACHE_DIR` first,
/// then `$HOME/.gossamer/cache`. Returns `None` when neither is
/// available (e.g. sandboxed CI without a `HOME`).
#[must_use]
pub fn default_cache_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GOS_CACHE_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(PathBuf::from(home).join(".gossamer").join("cache"));
    }
    None
}

/// In-memory cache used by tests and as the on-disk cache's
/// transient layer.
#[derive(Debug, Default)]
pub struct Cache {
    entries: BTreeMap<String, CachedSource>,
    disk_root: Option<PathBuf>,
}

impl Cache {
    /// Returns an empty cache with no disk-backed layer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a cache anchored on `root`. Disk reads/writes go under
    /// `root/pkg/<digest>/source/`.
    #[must_use]
    pub fn with_disk_root(root: PathBuf) -> Self {
        Self {
            entries: BTreeMap::new(),
            disk_root: Some(root),
        }
    }

    /// Returns the configured disk root, if any.
    #[must_use]
    pub fn disk_root(&self) -> Option<&Path> {
        self.disk_root.as_deref()
    }

    /// Stores `source` keyed on its digest. Returns `true` when the
    /// entry was new. When a disk root is set, the source is also
    /// written under `root/pkg/<digest>/source/`.
    pub fn insert(&mut self, source: CachedSource) -> bool {
        if let Err(e) = validate_cached_source(&source) {
            eprintln!("cache: warning: rejected {}: {e}", source.digest);
            return false;
        }
        if let Some(root) = self.disk_root.clone()
            && let Err(e) = write_to_disk(&root, &source)
        {
            // Keep the documented in-memory fallback for legacy callers, but
            // only after the source itself passed path and digest validation.
            eprintln!(
                "cache: warning: failed to persist {} to {}: {e}",
                source.digest,
                root.display()
            );
        }
        let key = source.digest.clone();
        self.entries.insert(key, source).is_none()
    }

    /// Validates and stores `source`, returning a persistence error to callers
    /// that need cache admission to be atomic with a successful fetch. The
    /// legacy [`Self::insert`] keeps its best-effort behaviour for existing
    /// in-memory callers; fetches use this checked form.
    pub fn insert_checked(&mut self, source: CachedSource) -> Result<bool, CacheError> {
        validate_cached_source(&source)?;
        if let Some(root) = self.disk_root.clone() {
            write_to_disk(&root, &source).map_err(|e| CacheError::CacheIo {
                path: root.display().to_string(),
                reason: e.to_string(),
            })?;
        }
        let key = source.digest.clone();
        Ok(self.entries.insert(key, source).is_none())
    }

    /// Validates and persists a source without retaining a second in-memory
    /// copy when this cache has a disk root. Fetchers use this because their
    /// returned [`Fetched`] value already owns the source tree; keeping an
    /// additional transient mirror doubles peak package memory for no gain.
    ///
    /// Caches without a disk root preserve the legacy in-memory behavior by
    /// cloning into their map, since that map is their only storage layer.
    pub fn insert_checked_ref(&mut self, source: &CachedSource) -> Result<bool, CacheError> {
        validate_cached_source(source)?;
        if let Some(root) = self.disk_root.clone() {
            write_to_disk(&root, source).map_err(|e| CacheError::CacheIo {
                path: root.display().to_string(),
                reason: e.to_string(),
            })?;
            return Ok(!self.entries.contains_key(&source.digest));
        }
        let key = source.digest.clone();
        Ok(self.entries.insert(key, source.clone()).is_none())
    }

    /// Looks up a cached entry by digest. Consults the in-memory
    /// map first; on miss with a disk root configured, attempts to
    /// load the digest from disk (with re-hash verification).
    pub fn get(&mut self, digest: &str) -> Option<&CachedSource> {
        if !is_canonical_digest(digest) {
            return None;
        }
        if !self.entries.contains_key(digest)
            && let Some(root) = self.disk_root.clone()
            && let Some(source) = load_from_disk(&root, digest)
        {
            self.entries.insert(digest.to_string(), source);
        }
        self.entries.get(digest)
    }

    /// Whether the cache currently contains the given digest. Consults
    /// the disk layer (existence-only) when the in-memory map misses.
    #[must_use]
    pub fn contains(&self, digest: &str) -> bool {
        if !is_canonical_digest(digest) {
            return false;
        }
        if self.entries.contains_key(digest) {
            return true;
        }
        if let Some(root) = self.disk_root.as_deref() {
            disk_has_digest(root, digest)
        } else {
            false
        }
    }

    /// Returns every (digest, source) pair currently cached in memory.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &CachedSource)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of in-memory cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the in-memory cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn digest_dir(root: &Path, digest: &str) -> PathBuf {
    root.join("pkg").join(digest)
}

/// The directory holding the unpacked source a fetch cached under `digest`
/// below `root`, when one is there.
#[must_use]
pub fn cached_source_dir(root: &Path, digest: &str) -> Option<PathBuf> {
    if !is_canonical_digest(digest) {
        return None;
    }
    let dir = digest_dir(root, digest).join("source");
    dir.is_dir().then_some(dir)
}

fn is_canonical_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// A path that can be joined beneath a package directory without escaping it.
/// This is shared by cache persistence and vendoring because `CachedSource` is
/// intentionally public and can be constructed outside the tar parser.
pub(crate) fn is_safe_package_path(path: &str) -> bool {
    use std::path::{Component, Path};
    if path.is_empty()
        || path.contains('\\')
        || path.contains('\0')
        || path.as_bytes().get(1) == Some(&b':')
    {
        return false;
    }
    let parsed = Path::new(path);
    if parsed.is_absolute() {
        return false;
    }
    let mut normalized = Vec::new();
    for component in parsed.components() {
        let Component::Normal(component) = component else {
            return false;
        };
        let Some(component) = component.to_str() else {
            return false;
        };
        normalized.push(component);
    }
    normalized.join("/") == path
}

fn validate_cached_source(source: &CachedSource) -> Result<(), CacheError> {
    if !is_canonical_digest(&source.digest) || compute_digest(&source.files) != source.digest {
        return Err(CacheError::InvalidCacheSource(
            "digest is not the canonical hash of the source tree".to_string(),
        ));
    }
    if source.files.len() > MAX_CACHED_SOURCE_FILES {
        return Err(CacheError::InvalidCacheSource(format!(
            "source tree has {} files; limit is {MAX_CACHED_SOURCE_FILES}",
            source.files.len()
        )));
    }
    let mut total_bytes = 0usize;
    for (path, bytes) in &source.files {
        if bytes.len() > MAX_CACHED_SOURCE_FILE_BYTES {
            return Err(CacheError::InvalidCacheSource(format!(
                "source file {path:?} has {} bytes; limit is {MAX_CACHED_SOURCE_FILE_BYTES}",
                bytes.len()
            )));
        }
        total_bytes = total_bytes.checked_add(bytes.len()).ok_or_else(|| {
            CacheError::InvalidCacheSource(format!(
                "source tree exceeds {MAX_CACHED_SOURCE_BYTES}-byte limit"
            ))
        })?;
        if total_bytes > MAX_CACHED_SOURCE_BYTES {
            return Err(CacheError::InvalidCacheSource(format!(
                "source tree has {total_bytes} bytes; limit is {MAX_CACHED_SOURCE_BYTES}"
            )));
        }
        if !is_safe_package_path(path) {
            return Err(CacheError::InvalidCacheSource(format!(
                "unsafe package path {path:?}"
            )));
        }
        let mut slash = path.find('/');
        while let Some(index) = slash {
            if source.files.contains_key(&path[..index]) {
                return Err(CacheError::InvalidCacheSource(format!(
                    "file {path:?} is nested under another file"
                )));
            }
            slash = path[index + 1..].find('/').map(|next| index + 1 + next);
        }
    }
    Ok(())
}

static CACHE_STAGING_ID: AtomicU64 = AtomicU64::new(0);

fn write_to_disk(root: &Path, source: &CachedSource) -> io::Result<()> {
    let dir = digest_dir(root, &source.digest);
    if dir.is_dir() {
        return Ok(());
    }
    let pkg_root = root.join("pkg");
    fs::create_dir_all(&pkg_root)?;
    let staging = pkg_root.join(format!(
        ".{}.tmp-{}-{}",
        source.digest,
        std::process::id(),
        CACHE_STAGING_ID.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir(&staging)?;
    let result = (|| {
        let source_dir = staging.join("source");
        fs::create_dir_all(&source_dir)?;
        write_file_synced(&staging.join("id.txt"), source.id.as_str().as_bytes())?;
        for (rel, bytes) in &source.files {
            let target = source_dir.join(rel);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            write_file_synced(&target, bytes)?;
        }
        // Publish only a complete, hashable tree. If another process wins this
        // digest race, its equivalent content is already the canonical entry.
        if let Err(err) = fs::rename(&staging, &dir) {
            if dir.is_dir() {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn write_file_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn disk_has_digest(root: &Path, digest: &str) -> bool {
    digest_dir(root, digest).join("id.txt").is_file()
}

fn load_from_disk(root: &Path, digest: &str) -> Option<CachedSource> {
    if !is_canonical_digest(digest) {
        return None;
    }
    let dir = digest_dir(root, digest);
    let id_text = String::from_utf8(read_file_bounded(&dir.join("id.txt"), 4096).ok()?).ok()?;
    let id = ProjectId::parse(id_text.trim()).ok()?;
    let source_dir = dir.join("source");
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut total_bytes = 0usize;
    walk_cached(&source_dir, &source_dir, &mut files, &mut total_bytes).ok()?;
    let actual = compute_digest(&files);
    if actual != digest {
        eprintln!(
            "cache: warning: integrity check failed for {digest}; recomputed {actual} - ignoring on-disk copy"
        );
        return None;
    }
    let source = CachedSource {
        id,
        files,
        digest: digest.to_string(),
    };
    validate_cached_source(&source).ok()?;
    Some(source)
}

fn walk_cached(
    base: &Path,
    current: &Path,
    out: &mut BTreeMap<String, Vec<u8>>,
    total_bytes: &mut usize,
) -> io::Result<()> {
    let file_type = fs::symlink_metadata(current)?.file_type();
    if file_type.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("symlink in cache entry: {}", current.display()),
        ));
    }
    if file_type.is_file() {
        if out.len() >= MAX_CACHED_SOURCE_FILES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cache entry has more than {MAX_CACHED_SOURCE_FILES} files"),
            ));
        }
        let key = current.strip_prefix(base).map_or_else(
            |_| current.display().to_string(),
            |p| p.to_string_lossy().replace('\\', "/"),
        );
        let remaining = MAX_CACHED_SOURCE_BYTES.saturating_sub(*total_bytes);
        let bytes = read_file_bounded(current, MAX_CACHED_SOURCE_FILE_BYTES.min(remaining))?;
        *total_bytes = total_bytes.checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "cached source byte count overflow",
            )
        })?;
        out.insert(key, bytes);
        return Ok(());
    }
    if !file_type.is_dir() {
        return Ok(());
    }
    // `BTreeMap` canonicalizes the final file order for hashing, so walking
    // directory entries does not need a temporary sorted `Vec<PathBuf>`.
    for entry in fs::read_dir(current)? {
        walk_cached(base, &entry?.path(), out, total_bytes)?;
    }
    Ok(())
}

/// Reads at most `limit` bytes plus one sentinel byte. Metadata checks alone
/// are not sufficient because a path can change size between `metadata` and
/// `read`; the bounded reader keeps that race from allocating unbounded RAM.
pub(crate) fn read_file_bounded(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds {limit}-byte limit: {}", path.display()),
        ));
    }
    Ok(bytes)
}

/// Errors raised by the cache layer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CacheError {
    /// A source tree could not be safely persisted in the cache.
    #[error("invalid cache source: {0}")]
    InvalidCacheSource(String),
    /// A checked cache admission failed on disk. Fetching fails rather than
    /// reporting a dependency as cached when it was never durably stored.
    #[error("cache I/O at {path}: {reason}")]
    CacheIo {
        /// Cache-root path whose persistence operation failed.
        path: String,
        /// Stringified operating-system error.
        reason: String,
    },
    /// The fetcher could not produce a source tree for this entry.
    /// Wraps the underlying transport / git / parser message.
    #[error("unsupported source for {0}")]
    Unsupported(String),
    /// The on-disk path source could not be read.
    #[error("path source for {id} unreadable at {path}")]
    PathUnreadable {
        /// Project id.
        id: String,
        /// Filesystem path that failed.
        path: String,
    },
    /// Digest mismatch - the cached payload differs from the recorded
    /// `sha256` in the manifest/lockfile.
    #[error("digest mismatch for {id}: expected {expected}, found {found}")]
    DigestMismatch {
        /// Project id.
        id: String,
        /// Expected digest.
        expected: String,
        /// Actually-computed digest.
        found: String,
    },
    /// The fetched package was marked yanked by the registry. Returned
    /// only when `--allow-yanked` was not passed.
    #[error("{id} version {version} is yanked: {reason}")]
    Yanked {
        /// Project id.
        id: String,
        /// Version that was yanked.
        version: String,
        /// Server-supplied reason or `"(no reason given)"`.
        reason: String,
    },
    /// A registry source arrived without the ed25519 signature +
    /// public key the registry is required to advertise. The tarball
    /// is rejected before it is unpacked.
    #[error("{0}: registry source is missing a publisher signature")]
    Unsigned(String),
    /// A registry dependency with no registry configured to fetch it from.
    #[error(
        "{0} comes from a package registry, and none is configured: set `[registries] default` \
         in project.toml or GOS_REGISTRY_URL"
    )]
    NoRegistry(String),
    /// The publisher signature did not verify against the tarball
    /// bytes. The tarball is rejected before it is unpacked.
    #[error("{0}: publisher signature does not verify")]
    SignatureInvalid(String),
    /// The registry advertised a publisher key that differs from the
    /// one pinned in the lockfile - a key rotation or substitution.
    #[error("{id}: publisher key changed (pinned {pinned}, registry offered {offered})")]
    KeyMismatch {
        /// Project id.
        id: String,
        /// Key pinned in the lockfile.
        pinned: String,
        /// Key the registry index now advertises.
        offered: String,
    },
    /// A signed registry source named a publisher key that was not
    /// already trusted by the lockfile or project manifest. An index
    /// cannot establish publisher identity by itself.
    #[error(
        "{id}: publisher key {offered} is not trusted; pin it in project.lock or [trusted-publishers]"
    )]
    UntrustedPublisher {
        /// Project id.
        id: String,
        /// Key advertised by the registry index.
        offered: String,
    },
    /// A git dependency's URL or ref was rejected before any git
    /// process ran: a disallowed transport (only `https://` and `ssh://`
    /// are permitted, so plaintext `git://`, `ext::`/`file://`/remote-helper
    /// prefixes that can execute arbitrary commands are refused) or a
    /// leading `-` that git would parse as an option rather than a
    /// positional argument.
    #[error("{0}")]
    RejectedGitSource(String),
}

/// Resolved source tree fetched into the cache.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// Resolved entry that produced this fetch.
    pub resolved: Resolved,
    /// Cached source contents.
    pub source: CachedSource,
    /// Hex-encoded publisher ed25519 public key that signed this
    /// source, when it came from a registry. Recorded into the
    /// lockfile so later fetches detect a key change. `None` for
    /// path/git/inline-tarball sources.
    pub owner_pubkey: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_write_publishes_a_complete_tree_without_staging_residue() {
        let root = std::env::temp_dir().join(format!(
            "gossamer-cache-atomic-{}-{}",
            std::process::id(),
            CACHE_STAGING_ID.fetch_add(1, Ordering::Relaxed),
        ));
        let mut files = BTreeMap::new();
        files.insert("src/main.gos".to_string(), b"fn main() {}\n".to_vec());
        let source = CachedSource::build(ProjectId::parse("example.test/app").unwrap(), files);

        write_to_disk(&root, &source).unwrap();
        assert_eq!(load_from_disk(&root, &source.digest), Some(source.clone()));
        let pkg_root = root.join("pkg");
        let entries: Vec<_> = fs::read_dir(&pkg_root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            entries
                .iter()
                .all(|name| !name.to_string_lossy().starts_with('.'))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn checked_cache_admission_rejects_path_traversal_and_bad_digests() {
        let root = std::env::temp_dir().join(format!(
            "gossamer-cache-path-{}-{}",
            std::process::id(),
            CACHE_STAGING_ID.fetch_add(1, Ordering::Relaxed),
        ));
        let mut files = BTreeMap::new();
        files.insert("../outside.gos".to_string(), b"no".to_vec());
        let source = CachedSource::build(ProjectId::parse("example.test/app").unwrap(), files);
        let mut cache = Cache::with_disk_root(root.clone());
        assert!(matches!(
            cache.insert_checked(source),
            Err(CacheError::InvalidCacheSource(_))
        ));
        assert!(!root.exists());

        let mut files = BTreeMap::new();
        files.insert("src/main.gos".to_string(), b"ok".to_vec());
        let mut source = CachedSource::build(ProjectId::parse("example.test/app").unwrap(), files);
        source.digest = "not-a-digest".to_string();
        assert!(matches!(
            cache.insert_checked(source),
            Err(CacheError::InvalidCacheSource(_))
        ));
    }

    #[test]
    fn checked_cache_admission_rejects_trees_over_the_file_budget() {
        let mut files = BTreeMap::new();
        for n in 0..=MAX_CACHED_SOURCE_FILES {
            files.insert(format!("src/{n}.gos"), Vec::new());
        }
        let source = CachedSource::build(ProjectId::parse("example.test/app").unwrap(), files);
        let mut cache = Cache::new();
        assert!(matches!(
            cache.insert_checked(source),
            Err(CacheError::InvalidCacheSource(message))
                if message.contains("files") && message.contains("limit")
        ));
    }

    #[test]
    fn bounded_file_read_rejects_the_sentinel_byte() {
        let path = std::env::temp_dir().join(format!(
            "gossamer-cache-bounded-read-{}-{}",
            std::process::id(),
            CACHE_STAGING_ID.fetch_add(1, Ordering::Relaxed),
        ));
        fs::write(&path, b"four").unwrap();
        assert!(read_file_bounded(&path, 3).is_err());
        assert_eq!(read_file_bounded(&path, 4).unwrap(), b"four");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn disk_backed_reference_admission_does_not_duplicate_the_source_tree() {
        let root = std::env::temp_dir().join(format!(
            "gossamer-cache-reference-admission-{}-{}",
            std::process::id(),
            CACHE_STAGING_ID.fetch_add(1, Ordering::Relaxed),
        ));
        let mut files = BTreeMap::new();
        files.insert("src/main.gos".to_string(), b"fn main() {}".to_vec());
        let source = CachedSource::build(ProjectId::parse("example.test/app").unwrap(), files);
        let mut cache = Cache::with_disk_root(root.clone());

        assert!(cache.insert_checked_ref(&source).unwrap());
        assert!(cache.is_empty());
        assert_eq!(cache.get(&source.digest), Some(&source));
        let _ = fs::remove_dir_all(root);
    }
}
