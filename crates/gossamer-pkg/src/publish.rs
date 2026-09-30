//! Publish flow: pack a project into a deterministic tar archive,
//! compute its sha256, optionally sign with ed25519, and upload to
//! the registry's `v1/upload` endpoint.
//!
//! Determinism rules for the tar buffer (sourced from the `tar`
//! module's `pack` helper):
//!
//! - File entries are emitted in lexicographic order.
//! - mtime / uid / gid are zeroed.
//! - Mode is normalised to `0o644`.
//! - End-of-archive marker is two 512-byte zero blocks.
//!
//! Two runs of `pack_crate` on the same source tree produce bytewise
//! identical archives, so the sha256 is stable across machines.

#![allow(
    clippy::map_unwrap_or,
    clippy::doc_markdown,
    clippy::elidable_lifetime_names,
    clippy::type_complexity,
    clippy::must_use_candidate
)]
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use thiserror::Error;

use crate::sha256;
use crate::tar;
use crate::transport::{Transport, TransportError};

/// Errors raised by [`pack_crate`].
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PackError {
    /// Underlying filesystem error.
    #[error("io reading {path}: {reason}")]
    Io {
        /// Path being read.
        path: String,
        /// Stringified io::Error.
        reason: String,
    },
    /// `project.toml` is missing from the project root.
    #[error("project root {0} has no project.toml")]
    MissingManifest(String),
    /// USTAR pack error (path too long / file too big).
    #[error("pack: {0}")]
    TarPack(String),
    /// A symlink cannot be packed safely because it can resolve outside the
    /// project root or make archive contents platform-dependent.
    #[error("refusing symlink in publish tree: {0}")]
    Symlink(String),
    /// The project has more regular files than the configured package budget.
    #[error("publish tree contains more than {0} files")]
    TooManyFiles(usize),
    /// A project file exceeded the configured package budget before it was
    /// read into the in-memory deterministic archive.
    #[error("publish file {path} has {size} bytes; limit is {limit}")]
    FileTooLarge {
        /// File path.
        path: String,
        /// Observed file size.
        size: u64,
        /// Configured per-file ceiling.
        limit: usize,
    },
    /// Aggregate project payload exceeded the configured package budget.
    #[error("publish tree expands beyond {limit} bytes")]
    TotalTooLarge {
        /// Configured aggregate payload ceiling.
        limit: usize,
    },
}

/// Errors raised by the publish flow above and beyond [`PackError`].
#[derive(Debug, Error)]
pub enum PublishError {
    /// Pack-time failure.
    #[error("pack: {0}")]
    Pack(#[from] PackError),
    /// Transport-level failure (DNS / TCP / TLS / status).
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    /// Caller-side validation / config failure.
    #[error("publish: {0}")]
    Config(String),
    /// The caller supplied a public `PublishedArtifact` whose recorded digest
    /// does not match its payload, so it must not be sent to a registry.
    #[error("publish artifact digest mismatch: expected {expected}, found {found}")]
    ArtifactDigestMismatch {
        /// Digest supplied by the caller.
        expected: String,
        /// Digest recomputed from the bytes.
        found: String,
    },
    /// The caller supplied an artifact that exceeds the built-in transport
    /// response/package limit.
    #[error("publish artifact exceeds {limit}-byte limit")]
    ArtifactTooLarge {
        /// Maximum archive byte count.
        limit: usize,
    },
}

/// One packed crate ready to upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifact {
    /// Deterministic tar buffer.
    pub bytes: Vec<u8>,
    /// SHA-256 (hex, lowercase) of `bytes`.
    pub sha256: String,
}

/// A deterministic package archive backed by a private temporary file.
///
/// Unlike [`PublishedArtifact`], this never holds the encoded tarball in a
/// `Vec`. The archive is removed when this value is dropped. Its SHA-256 is
/// calculated while the tar writer copies project files, so publication can
/// pass a fresh file reader directly to the transport.
#[derive(Debug)]
pub struct StreamingArtifact {
    path: PathBuf,
    /// Encoded USTAR byte length.
    pub bytes: usize,
    /// SHA-256 (hex, lowercase) of the encoded archive.
    pub sha256: String,
}

impl StreamingArtifact {
    /// Opens a new reader positioned at the beginning of the archive.
    pub fn open(&self) -> Result<File, PackError> {
        File::open(&self.path).map_err(|error| PackError::Io {
            path: self.path.display().to_string(),
            reason: error.to_string(),
        })
    }
}

impl Drop for StreamingArtifact {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Walks `root` for every file (excluding `target/`, `.gos-cache/`,
/// `.git/`, `vendor/`, `.gos-bindings/`, hidden dotfiles), packs them
/// into a deterministic tar buffer, and returns the archive plus its
/// sha256.
///
/// The tar is *uncompressed* USTAR - the publish protocol leaves
/// compression to the registry's storage backend. This keeps the
/// digest verifiable from the on-the-wire bytes without a decompress
/// step, and matches the existing fetcher's straight-tar contract.
pub fn pack_crate(root: &Path) -> Result<PublishedArtifact, PackError> {
    pack_crate_with_limits(root, tar::PackLimits::default())
}

/// Bounded variant of [`pack_crate`]. File sizes are checked from metadata
/// before they enter the file map, preventing a local accidental or hostile
/// multi-gigabyte file from being read merely to discover that publication is
/// not permitted.
pub fn pack_crate_with_limits(
    root: &Path,
    limits: tar::PackLimits,
) -> Result<PublishedArtifact, PackError> {
    let root_metadata = std::fs::symlink_metadata(root).map_err(|e| PackError::Io {
        path: root.display().to_string(),
        reason: e.to_string(),
    })?;
    if root_metadata.file_type().is_symlink() {
        return Err(PackError::Symlink(root.display().to_string()));
    }
    let manifest_path = root.join("project.toml");
    if !manifest_path.is_file() {
        return Err(PackError::MissingManifest(root.display().to_string()));
    }
    let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut budget = PackBudget::default();
    walk(root, root, &mut entries, limits, &mut budget)?;
    let bytes =
        tar::pack_with_limits(&entries, limits).map_err(|e| PackError::TarPack(e.to_string()))?;
    let sha = sha256::hex(&bytes);
    Ok(PublishedArtifact { bytes, sha256: sha })
}

/// Streams a deterministic USTAR archive into a private spool file. This is
/// the archive constructor for network publication: only the current input
/// file and an 8 KiB copy buffer are retained while the final archive is
/// hashed. [`pack_crate`] remains for callers that explicitly need bytes.
pub fn pack_crate_streaming(root: &Path) -> Result<StreamingArtifact, PackError> {
    pack_crate_streaming_with_limits(root, tar::PackLimits::default())
}

/// Bounded variant of [`pack_crate_streaming`]. Every file length is planned
/// and validated before the spool is created, then rechecked while copying so
/// a concurrently changed project file cannot produce an inconsistent tar.
pub fn pack_crate_streaming_with_limits(
    root: &Path,
    limits: tar::PackLimits,
) -> Result<StreamingArtifact, PackError> {
    let root_metadata = std::fs::symlink_metadata(root).map_err(|error| PackError::Io {
        path: root.display().to_string(),
        reason: error.to_string(),
    })?;
    if root_metadata.file_type().is_symlink() {
        return Err(PackError::Symlink(root.display().to_string()));
    }
    let manifest_path = root.join("project.toml");
    if !manifest_path.is_file() {
        return Err(PackError::MissingManifest(root.display().to_string()));
    }

    let mut plan = Vec::new();
    collect_streaming_plan(root, root, &mut plan)?;
    let sizes: Vec<(String, usize)> = plan
        .iter()
        .map(|entry: &StreamingFile| (entry.relative.clone(), entry.size))
        .collect();
    let expected_bytes = tar::checked_pack_file_sizes(&sizes, limits)
        .map_err(|error| PackError::TarPack(error.to_string()))?;
    let (path, file) = create_archive_spool()?;
    let result = (|| {
        let mut output = HashingArchiveFile::new(file);
        for entry in &plan {
            let mut input = File::open(&entry.path).map_err(|error| PackError::Io {
                path: entry.path.display().to_string(),
                reason: error.to_string(),
            })?;
            tar::write_file_entry(&mut output, &entry.relative, entry.size, &mut input)
                .map_err(|error| PackError::TarPack(error.to_string()))?;
        }
        tar::write_end_marker(&mut output)
            .map_err(|error| PackError::TarPack(error.to_string()))?;
        let (bytes, sha256) = output.finish()?;
        if bytes != expected_bytes {
            return Err(PackError::TarPack(format!(
                "archive length changed during packing: planned {expected_bytes}, wrote {bytes}"
            )));
        }
        Ok(StreamingArtifact {
            path: path.clone(),
            bytes,
            sha256,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    result
}

#[derive(Debug)]
struct StreamingFile {
    path: PathBuf,
    relative: String,
    size: usize,
}

fn collect_streaming_plan(
    base: &Path,
    current: &Path,
    out: &mut Vec<StreamingFile>,
) -> Result<(), PackError> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(current)
        .map_err(|error| PackError::Io {
            path: current.display().to_string(),
            reason: error.to_string(),
        })?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if should_skip(name) {
            continue;
        }
        let file_type = std::fs::symlink_metadata(&path)
            .map_err(|error| PackError::Io {
                path: path.display().to_string(),
                reason: error.to_string(),
            })?
            .file_type();
        if file_type.is_symlink() {
            return Err(PackError::Symlink(path.display().to_string()));
        }
        if file_type.is_dir() {
            collect_streaming_plan(base, &path, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let size = std::fs::metadata(&path)
            .map_err(|error| PackError::Io {
                path: path.display().to_string(),
                reason: error.to_string(),
            })?
            .len();
        let size = usize::try_from(size).map_err(|_| PackError::FileTooLarge {
            path: path.display().to_string(),
            size,
            limit: usize::MAX,
        })?;
        let relative = path.strip_prefix(base).map_or_else(
            |_| path.display().to_string(),
            |path| path.to_string_lossy().replace('\\', "/"),
        );
        out.push(StreamingFile {
            path,
            relative,
            size,
        });
    }
    Ok(())
}

static ARCHIVE_SPOOL_ID: AtomicU64 = AtomicU64::new(0);

fn create_archive_spool() -> Result<(PathBuf, File), PackError> {
    let root = std::env::temp_dir();
    for _ in 0..32 {
        let path = root.join(format!(
            "gos-publish-{}-{}.tar",
            std::process::id(),
            ARCHIVE_SPOOL_ID.fetch_add(1, Ordering::Relaxed),
        ));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(PackError::Io {
                    path: path.display().to_string(),
                    reason: error.to_string(),
                });
            }
        }
    }
    Err(PackError::Io {
        path: root.display().to_string(),
        reason: "could not allocate a unique publish archive spool".to_string(),
    })
}

struct HashingArchiveFile {
    file: File,
    hasher: sha256::Hasher,
    bytes: usize,
}

impl HashingArchiveFile {
    fn new(file: File) -> Self {
        Self {
            file,
            hasher: sha256::Hasher::new(),
            bytes: 0,
        }
    }

    fn finish(mut self) -> Result<(usize, String), PackError> {
        self.file.flush().map_err(|error| PackError::Io {
            path: "publish archive spool".to_string(),
            reason: error.to_string(),
        })?;
        self.file.sync_all().map_err(|error| PackError::Io {
            path: "publish archive spool".to_string(),
            reason: error.to_string(),
        })?;
        Ok((self.bytes, self.hasher.finalize_hex()))
    }
}

impl Write for HashingArchiveFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write_all(bytes)?;
        self.hasher.update(bytes);
        self.bytes = self.bytes.checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "publish archive length overflow",
            )
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[derive(Default)]
struct PackBudget {
    files: usize,
    total_bytes: usize,
}

fn walk(
    base: &Path,
    current: &Path,
    out: &mut BTreeMap<String, Vec<u8>>,
    limits: tar::PackLimits,
    budget: &mut PackBudget,
) -> Result<(), PackError> {
    if !current.is_dir() {
        return Ok(());
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(current)
        .map_err(|e| PackError::Io {
            path: current.display().to_string(),
            reason: e.to_string(),
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    paths.sort();
    for path in paths {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if should_skip(name) {
            continue;
        }
        let file_type = std::fs::symlink_metadata(&path)
            .map_err(|e| PackError::Io {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?
            .file_type();
        if file_type.is_symlink() {
            return Err(PackError::Symlink(path.display().to_string()));
        }
        if file_type.is_dir() {
            walk(base, &path, out, limits, budget)?;
        } else if file_type.is_file() {
            budget.files = budget.files.saturating_add(1);
            if budget.files > limits.max_entries {
                return Err(PackError::TooManyFiles(limits.max_entries));
            }
            let size = std::fs::metadata(&path)
                .map_err(|e| PackError::Io {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?
                .len();
            let size_usize = usize::try_from(size).map_err(|_| PackError::FileTooLarge {
                path: path.display().to_string(),
                size,
                limit: limits.max_file_bytes,
            })?;
            if size_usize > limits.max_file_bytes {
                return Err(PackError::FileTooLarge {
                    path: path.display().to_string(),
                    size,
                    limit: limits.max_file_bytes,
                });
            }
            budget.total_bytes =
                budget
                    .total_bytes
                    .checked_add(size_usize)
                    .ok_or(PackError::TotalTooLarge {
                        limit: limits.max_total_bytes,
                    })?;
            if budget.total_bytes > limits.max_total_bytes {
                return Err(PackError::TotalTooLarge {
                    limit: limits.max_total_bytes,
                });
            }
            let key = path.strip_prefix(base).map_or_else(
                |_| path.display().to_string(),
                |p| p.to_string_lossy().replace('\\', "/"),
            );
            let bytes = std::fs::read(&path).map_err(|e| PackError::Io {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
            out.insert(key, bytes);
        }
    }
    Ok(())
}

fn should_skip(name: &str) -> bool {
    matches!(
        name,
        "target"
            | "vendor"
            | ".git"
            | ".gos-cache"
            | ".gos-bindings"
            | "node_modules"
            | ".DS_Store"
    ) || name.ends_with(".rs.bk")
}

/// A publish request whose body is the archive itself.
///
/// The deterministic tar spool is copied directly to the transport by
/// [`upload_streaming_with`] and its small metadata travels in protocol
/// headers.
/// When present, `signature` is over the lowercase ASCII SHA-256 digest named
/// by `X-Gossamer-Signature-Input`, not the archive body. That deliberately
/// makes signing possible without materialising the archive after it has been
/// streamed to its private spool.
#[derive(Debug, Clone)]
pub struct StreamingPublishRequest<'a> {
    /// Project id being published (e.g. `example.com/widget`).
    pub project_id: &'a str,
    /// Version being published (`MAJOR.MINOR.PATCH`).
    pub version: &'a str,
    /// Deterministic archive spool and its incrementally calculated digest.
    pub artifact: &'a StreamingArtifact,
    /// Optional ed25519 signature over `artifact.sha256.as_bytes()`.
    pub signature: Option<[u8; 64]>,
    /// Optional ed25519 public key for the signature.
    pub public_key: Option<[u8; 32]>,
    /// Bearer token authenticating the upload.
    pub auth_token: Option<&'a str>,
}

/// Uploads a version-2 raw archive request without allocating a JSON/base16
/// copy of the package. The protocol is deliberately explicit:
///
/// - the body is `application/vnd.gossamer.package+tar`;
/// - `X-Gossamer-Publish-Protocol: 2` selects this representation;
/// - the archive hash and optional signature/key are carried in named headers.
///
pub fn upload_streaming_with(
    transport: &dyn UploadTransport,
    registry_url: &str,
    request: &StreamingPublishRequest<'_>,
) -> Result<(), PublishError> {
    let (project_id, version) = validated_publish_location(request.project_id, request.version)?;
    validate_streaming_artifact(request.artifact)?;
    let url = format!(
        "{base}/v1/upload/{id}/{version}",
        base = registry_url.trim_end_matches('/'),
        id = project_id,
        version = version,
    );
    let signature = request
        .signature
        .map(|value| crate::signing::hex_encode(&value));
    let public_key = request
        .public_key
        .map(|value| crate::signing::hex_encode(&value));
    let mut headers = vec![
        ("X-Gossamer-Publish-Protocol", "2".to_string()),
        (
            "X-Gossamer-Artifact-Sha256",
            request.artifact.sha256.clone(),
        ),
    ];
    if let Some(signature) = signature.as_deref() {
        headers.push(("X-Gossamer-Signature-Input", "sha256".to_string()));
        headers.push(("X-Gossamer-Signature", signature.to_string()));
    }
    if let Some(public_key) = public_key.as_deref() {
        headers.push(("X-Gossamer-Public-Key", public_key.to_string()));
    }
    let header_refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let mut archive = request.artifact.open()?;
    transport
        .post_reader_with_headers(
            &url,
            &mut archive,
            request.artifact.bytes,
            "application/vnd.gossamer.package+tar",
            request.auth_token,
            &header_refs,
        )
        .map_err(PublishError::Transport)
}

fn validate_streaming_artifact(artifact: &StreamingArtifact) -> Result<(), PublishError> {
    let limit = tar::PackLimits::default().max_archive_bytes;
    if artifact.bytes > limit {
        return Err(PublishError::ArtifactTooLarge { limit });
    }
    let mut input = artifact.open()?;
    let mut hasher = sha256::Hasher::new();
    let mut total = 0usize;
    let mut buffer = [0u8; 8192];
    loop {
        let read = input.read(&mut buffer).map_err(|error| PackError::Io {
            path: "publish archive spool".to_string(),
            reason: error.to_string(),
        })?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read)
            .ok_or(PublishError::ArtifactTooLarge { limit })?;
        if total > limit {
            return Err(PublishError::ArtifactTooLarge { limit });
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hasher.finalize_hex();
    if total != artifact.bytes || actual != artifact.sha256 {
        return Err(PublishError::ArtifactDigestMismatch {
            expected: artifact.sha256.clone(),
            found: actual,
        });
    }
    Ok(())
}

fn validated_publish_location(
    project_id: &str,
    version: &str,
) -> Result<(String, String), PublishError> {
    let project_id = crate::id::ProjectId::parse(project_id)
        .map_err(|e| PublishError::Config(format!("invalid project id: {e}")))?;
    let version = crate::version::Version::parse(version)
        .map_err(|e| PublishError::Config(format!("invalid version: {e}")))?;
    Ok((project_id.to_string(), version.to_string()))
}

/// Transport extension that supports POSTing a body with an optional
/// auth header. Implemented for both [`crate::transport::HttpsTransport`] and a test
/// double; kept as a separate trait so the registry-side mock can
/// inspect the body without going through the `Transport::get` API.
pub trait UploadTransport: Send + Sync {
    /// Sends `body` to `url` with an optional `Authorization: Bearer`
    /// header. Returns `Ok(())` on a 2xx response.
    fn post(&self, url: &str, body: &[u8], auth_token: Option<&str>) -> Result<(), TransportError>;

    /// Reader-oriented upload API. The default retains compatibility with
    /// existing test doubles while HTTP-backed uploaders stream to the socket.
    fn post_reader(
        &self,
        url: &str,
        body: &mut dyn Read,
        body_len: usize,
        auth_token: Option<&str>,
    ) -> Result<(), TransportError> {
        let mut bytes = Vec::with_capacity(body_len);
        body.take((body_len as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| TransportError::Io(format!("read upload body: {e}")))?;
        if bytes.len() != body_len {
            return Err(TransportError::Io(
                "upload body length does not match declared length".to_string(),
            ));
        }
        self.post(url, &bytes, auth_token)
    }

    /// Streams a request body with validated, publish-protocol headers. The
    /// default intentionally rejects extra headers so older upload transports
    /// cannot accidentally receive a version-2 raw archive they do not know
    /// how to represent.
    fn post_reader_with_headers(
        &self,
        url: &str,
        body: &mut dyn Read,
        body_len: usize,
        _content_type: &str,
        auth_token: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Result<(), TransportError> {
        if !headers.is_empty() {
            return Err(TransportError::Io(
                "upload transport does not support protocol request headers".to_string(),
            ));
        }
        self.post_reader(url, body, body_len, auth_token)
    }
}

/// Generic upload transport backed by any `Transport` + plain HTTP/1.1.
/// Sends a `POST` with `Content-Type: application/json`. Used by the
/// CLI's publish path when the registry speaks the minimal upload
/// protocol.
///
/// The transport handles TLS / DNS / status-line parsing itself.
pub struct HttpUploader<'a> {
    /// Underlying transport - used as a TLS connector.
    pub transport: &'a dyn Transport,
}

impl UploadTransport for HttpUploader<'_> {
    fn post(&self, url: &str, body: &[u8], auth_token: Option<&str>) -> Result<(), TransportError> {
        // The `Transport::post` default impl rejects unsupported
        // transports; `HttpTransport` / `HttpsTransport` override
        // it with the rustls-backed POST shape (`Content-Type:
        // application/json`, optional `Authorization: Bearer`).
        self.transport
            .post(url, body, "application/json", auth_token)
            .map(|_response_body| ())
    }

    fn post_reader(
        &self,
        url: &str,
        body: &mut dyn Read,
        body_len: usize,
        auth_token: Option<&str>,
    ) -> Result<(), TransportError> {
        self.transport
            .post_reader(url, body, body_len, "application/json", auth_token)
            .map(|_response_body| ())
    }

    fn post_reader_with_headers(
        &self,
        url: &str,
        body: &mut dyn Read,
        body_len: usize,
        content_type: &str,
        auth_token: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Result<(), TransportError> {
        self.transport
            .post_reader_with_headers(url, body, body_len, content_type, auth_token, headers)
            .map(|_response_body| ())
    }
}

/// In-memory upload transport for tests - records every POST in
/// order so assertions can inspect what would have been sent.
#[derive(Debug, Default)]
pub struct RecordingUploader {
    /// Captured POSTs: `(url, body, auth_token)`.
    pub posts: parking_lot::Mutex<Vec<(String, Vec<u8>, Option<String>)>>,
    /// Captured version-2 stream uploads: URL, raw body, auth token, and
    /// protocol headers. Recording naturally buffers only because the test
    /// double must expose the transmitted payload for assertions.
    pub stream_posts:
        parking_lot::Mutex<Vec<(String, Vec<u8>, Option<String>, Vec<(String, String)>)>>,
}

impl RecordingUploader {
    /// Returns a fresh recording uploader.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of every recorded post.
    pub fn take_posts(&self) -> Vec<(String, Vec<u8>, Option<String>)> {
        std::mem::take(&mut *self.posts.lock())
    }

    /// Snapshot of raw streaming upload requests.
    pub fn take_stream_posts(
        &self,
    ) -> Vec<(String, Vec<u8>, Option<String>, Vec<(String, String)>)> {
        std::mem::take(&mut *self.stream_posts.lock())
    }
}

impl UploadTransport for RecordingUploader {
    fn post(&self, url: &str, body: &[u8], auth_token: Option<&str>) -> Result<(), TransportError> {
        self.posts.lock().push((
            url.to_string(),
            body.to_vec(),
            auth_token.map(str::to_string),
        ));
        Ok(())
    }

    fn post_reader_with_headers(
        &self,
        url: &str,
        body: &mut dyn Read,
        body_len: usize,
        _content_type: &str,
        auth_token: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Result<(), TransportError> {
        let mut bytes = Vec::with_capacity(body_len);
        body.take((body_len as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| TransportError::Io(format!("read upload body: {error}")))?;
        if bytes.len() != body_len {
            return Err(TransportError::Io(
                "upload body length does not match declared length".to_string(),
            ));
        }
        self.stream_posts.lock().push((
            url.to_string(),
            bytes,
            auth_token.map(str::to_string),
            headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        ));
        Ok(())
    }
}

/// Yank a published version by POSTing to `<registry>/v1/yank/<id>/<ver>`.
/// `reason` is the optional human-readable explanation.
pub fn yank_with(
    transport: &dyn UploadTransport,
    registry_url: &str,
    project_id: &str,
    version: &str,
    reason: Option<&str>,
    auth_token: Option<&str>,
) -> Result<(), PublishError> {
    let (project_id, version) = validated_publish_location(project_id, version)?;
    let url = format!(
        "{base}/v1/yank/{id}/{version}",
        base = registry_url.trim_end_matches('/'),
        id = project_id,
        version = version,
    );
    let body = json!({ "reason": reason.unwrap_or("") }).to_string();
    transport.post(&url, body.as_bytes(), auth_token)?;
    Ok(())
}

/// Owner management: add / remove / list users authorised to publish
/// `project_id`. The registry endpoint is `v1/owners/<id>` with the
/// HTTP verb encoded in the body (POST + `{"op":"add"|"remove"}`).
pub fn owner_op_with(
    transport: &dyn UploadTransport,
    registry_url: &str,
    project_id: &str,
    op: &str,
    user: Option<&str>,
    auth_token: Option<&str>,
) -> Result<(), PublishError> {
    let (project_id, _) = validated_publish_location(project_id, "0.0.0")?;
    if !matches!(op, "add" | "remove" | "list") {
        return Err(PublishError::Config(format!(
            "invalid owner operation {op:?}"
        )));
    }
    let url = format!(
        "{base}/v1/owners/{id}",
        base = registry_url.trim_end_matches('/'),
        id = project_id,
    );
    let body = json!({ "op": op, "user": user.unwrap_or("") }).to_string();
    transport.post(&url, body.as_bytes(), auth_token)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_crate_returns_deterministic_bytes() {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("gossamer-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("project.toml"), b"[project]\nid = \"a.b/c\"\n").unwrap();
        std::fs::write(tmp.join("src/main.gos"), b"fn main() {}\n").unwrap();
        let a = pack_crate(&tmp).unwrap();
        let b = pack_crate(&tmp).unwrap();
        assert_eq!(a.bytes, b.bytes, "pack must be byte-stable");
        assert_eq!(a.sha256, b.sha256);
        assert_eq!(a.sha256.len(), 64);
        // Unpacking the produced tar yields back the original tree.
        let back = crate::tar::unpack(&a.bytes).unwrap();
        assert!(back.contains_key("project.toml"));
        assert!(back.contains_key("src/main.gos"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn streaming_pack_matches_buffered_archive_without_retaining_tar_bytes() {
        let tmp = std::env::temp_dir().join(format!(
            "gossamer-stream-pack-{}-{}",
            std::process::id(),
            ARCHIVE_SPOOL_ID.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("project.toml"), b"[project]\nid = \"a.b/c\"\n").unwrap();
        std::fs::write(tmp.join("src/main.gos"), b"fn main() {}\n").unwrap();

        let buffered = pack_crate(&tmp).unwrap();
        let streamed = pack_crate_streaming(&tmp).unwrap();
        assert_eq!(streamed.bytes, buffered.bytes.len());
        assert_eq!(streamed.sha256, buffered.sha256);
        let mut actual = Vec::new();
        streamed.open().unwrap().read_to_end(&mut actual).unwrap();
        assert_eq!(actual, buffered.bytes);

        drop(streamed);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn streaming_upload_uses_raw_tar_and_versioned_metadata_headers() {
        let tmp = std::env::temp_dir().join(format!(
            "gossamer-stream-upload-{}-{}",
            std::process::id(),
            ARCHIVE_SPOOL_ID.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("project.toml"), b"[project]\nid = \"a.b/c\"\n").unwrap();
        std::fs::write(tmp.join("src/main.gos"), b"fn main() {}\n").unwrap();

        let artifact = pack_crate_streaming(&tmp).unwrap();
        let key = crate::signing::SigningKey::from_bytes([9; 32]);
        let signature = key.sign(artifact.sha256.as_bytes());
        let uploader = RecordingUploader::new();
        let request = StreamingPublishRequest {
            project_id: "a.b/c",
            version: "0.1.0",
            artifact: &artifact,
            signature: Some(signature),
            public_key: Some(key.verifying_key().to_bytes()),
            auth_token: Some("token"),
        };
        upload_streaming_with(&uploader, "https://registry.example", &request).unwrap();

        let posts = uploader.take_stream_posts();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].0, "https://registry.example/v1/upload/a.b/c/0.1.0");
        assert_eq!(posts[0].2.as_deref(), Some("token"));
        assert_eq!(posts[0].1.len(), artifact.bytes);
        assert_eq!(sha256::hex(&posts[0].1), artifact.sha256);
        let headers: std::collections::BTreeMap<_, _> = posts[0].3.iter().cloned().collect();
        assert_eq!(
            headers.get("X-Gossamer-Publish-Protocol"),
            Some(&"2".to_string())
        );
        assert_eq!(
            headers.get("X-Gossamer-Artifact-Sha256"),
            Some(&artifact.sha256)
        );
        assert_eq!(
            headers.get("X-Gossamer-Signature-Input"),
            Some(&"sha256".to_string())
        );
        assert_eq!(
            headers.get("X-Gossamer-Signature"),
            Some(&crate::signing::hex_encode(&signature))
        );
        assert!(uploader.take_posts().is_empty());

        drop(artifact);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pack_crate_rejects_missing_manifest() {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("gossamer-pack-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let err = pack_crate(&tmp).unwrap_err();
        assert!(matches!(err, PackError::MissingManifest(_)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn pack_crate_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let mut tmp = std::env::temp_dir();
        tmp.push(format!("gossamer-pack-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("project.toml"), b"[project]\nid = \"a.b/c\"\n").unwrap();
        symlink("/etc/passwd", tmp.join("outside")).unwrap();

        let err = pack_crate(&tmp).unwrap_err();
        assert!(matches!(err, PackError::Symlink(_)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A packed archive of a one-file project under a fresh temporary root.
    fn streaming_fixture(tag: &str) -> (PathBuf, StreamingArtifact) {
        let tmp = std::env::temp_dir().join(format!(
            "gossamer-{tag}-{}-{}",
            std::process::id(),
            ARCHIVE_SPOOL_ID.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("project.toml"), b"[project]\nid = \"a.b/c\"\n").unwrap();
        let artifact = pack_crate_streaming(&tmp).unwrap();
        (tmp, artifact)
    }

    #[test]
    fn owner_payloads_are_structured_json() {
        let uploader = RecordingUploader::new();
        owner_op_with(
            &uploader,
            "https://registry.example",
            "example.com/widget",
            "add",
            Some("a\"quoted\nowner"),
            None,
        )
        .unwrap();
        let posts = uploader.take_posts();
        let owner: serde_json::Value = serde_json::from_slice(&posts[0].1).unwrap();
        assert_eq!(owner["op"], "add");
        assert_eq!(owner["user"], "a\"quoted\nowner");
    }

    #[test]
    fn upload_rejects_unvalidated_path_components_before_posting() {
        let (tmp, artifact) = streaming_fixture("pub-path");
        let request = StreamingPublishRequest {
            project_id: "example.com/widget/../escape",
            version: "1.2.3",
            artifact: &artifact,
            signature: None,
            public_key: None,
            auth_token: None,
        };
        let uploader = RecordingUploader::new();
        assert!(matches!(
            upload_streaming_with(&uploader, "https://registry.example", &request),
            Err(PublishError::Config(_))
        ));
        assert!(uploader.take_stream_posts().is_empty());
        drop(artifact);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn upload_rejects_a_forged_artifact_digest_before_posting() {
        let (tmp, mut artifact) = streaming_fixture("pub-digest");
        artifact.sha256 = "00".repeat(32);
        let request = StreamingPublishRequest {
            project_id: "example.com/widget",
            version: "1.2.3",
            artifact: &artifact,
            signature: None,
            public_key: None,
            auth_token: None,
        };
        let uploader = RecordingUploader::new();
        assert!(matches!(
            upload_streaming_with(&uploader, "https://registry.example", &request),
            Err(PublishError::ArtifactDigestMismatch { .. })
        ));
        assert!(uploader.take_stream_posts().is_empty());
        drop(artifact);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pack_crate_checks_file_limits_before_reading_contents() {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("gossamer-pack-limits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("project.toml"), b"[project]\nid = \"a.b/c\"\n").unwrap();
        std::fs::write(tmp.join("large.gos"), b"0123456789").unwrap();
        let limits = tar::PackLimits {
            max_entries: 8,
            max_file_bytes: 8,
            max_total_bytes: 128,
            max_archive_bytes: 128 * 1024,
        };
        assert!(matches!(
            pack_crate_with_limits(&tmp, limits),
            Err(PackError::FileTooLarge { .. })
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
