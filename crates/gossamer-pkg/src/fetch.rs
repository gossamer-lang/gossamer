//! Fetcher backing `gos fetch` / `gos vendor`.
//!
//! Materialises every source kind declared in the manifest:
//!
//! - `Path` - read the local filesystem.
//! - `Tarball` - HTTP(S) GET, sha256-verify, USTAR-unpack.
//! - `Git` - shell out to `git clone --bare` into the cache, then
//!   `git archive` the requested ref into the source tree.
//! - `Registry` - look the version up in the [`VersionCatalogue`]
//!   for a `(download_url, tarball_sha256)` pair, fetch that
//!   tarball, sha256-verify, USTAR-unpack.
//!
//! Every network fetch is content-addressed: the verified payload's
//! sha256 is the cache key. Cache hits skip the network entirely.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cache::{
    Cache, CacheError, CachedSource, Fetched, MAX_CACHED_SOURCE_BYTES,
    MAX_CACHED_SOURCE_FILE_BYTES, MAX_CACHED_SOURCE_FILES, is_safe_package_path, read_file_bounded,
};
use crate::resolver::{CatalogueEntry, Resolved, ResolvedSource, VersionCatalogue};
use crate::sha256;
use crate::tar;
use crate::transport::{StaticTransport, Transport, TransportError};

/// Fetcher configuration.
#[derive(Clone, Default)]
pub struct FetchOptions {
    /// When `true`, the fetcher refuses to populate cache entries it
    /// does not already have. Mirrors the SPEC §16.x `--offline` flag.
    pub offline: bool,
    /// When `true`, fetches a yanked registry version succeed.
    /// Default is to error with [`CacheError::Yanked`].
    pub allow_yanked: bool,
    /// Registry URL used for catalogue lookups (e.g. download URLs and
    /// sha256 pins). `None` when no registry is configured, which only a
    /// registry dependency needs.
    pub registry_url: Option<String>,
    /// Optional bearer token sent on registry requests. The CLI loads
    /// this from `~/.gossamer/credentials.toml`.
    pub auth_token: Option<String>,
}

impl std::fmt::Debug for FetchOptions {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("FetchOptions")
            .field("offline", &self.offline)
            .field("allow_yanked", &self.allow_yanked)
            .field("registry_url", &self.registry_url)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl FetchOptions {
    /// Builds a default `FetchOptions` with a registry URL set.
    #[must_use]
    pub fn with_registry(registry_url: impl Into<String>) -> Self {
        Self {
            registry_url: Some(registry_url.into()),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{FetchOptions, HashingFile, create_tarball_spool};

    #[test]
    fn debug_redacts_registry_token() {
        let options = FetchOptions {
            auth_token: Some("secret-token".to_string()),
            ..FetchOptions::default()
        };
        let rendered = format!("{options:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("secret-token"));
    }

    #[cfg(unix)]
    #[test]
    fn tarball_spool_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (path, _file) = create_tarball_spool().expect("spool");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_file(path).expect("remove spool");
    }

    #[test]
    fn hashing_file_enforces_archive_byte_cap_before_finish() {
        let (path, file) = create_tarball_spool().expect("spool");
        let mut writer = HashingFile::new(file, 3);
        writer.write_all(b"abc").expect("within cap");
        let err = writer
            .write_all(b"d")
            .expect_err("write beyond cap must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        std::fs::remove_file(path).expect("remove spool");
    }
}

/// Fetcher driver.
pub struct Fetcher {
    options: FetchOptions,
    transport: Arc<dyn Transport>,
    catalogue: VersionCatalogue,
    /// Publisher keys pinned from the lockfile (project id → hex
    /// public key). A registry fetch whose advertised key differs from
    /// a pin is rejected. These pins are sufficient trust roots for
    /// existing lockfile-backed projects.
    pinned_keys: BTreeMap<String, String>,
    /// Publisher keys declared by the root manifest. Unlike an index
    /// entry, this is source-controlled caller input and can establish
    /// first-fetch trust before a lockfile exists.
    trusted_keys: BTreeMap<String, String>,
}

impl std::fmt::Debug for Fetcher {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Fetcher")
            .field("options", &self.options)
            .field("transport", &"<dyn Transport>")
            .field("catalogue", &self.catalogue)
            .field("pinned_keys", &self.pinned_keys)
            .field("trusted_keys", &self.trusted_keys)
            .finish()
    }
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new(FetchOptions::default())
    }
}

impl Fetcher {
    /// Constructs a fetcher with the given options.
    #[must_use]
    pub fn new(options: FetchOptions) -> Self {
        Self {
            options,
            transport: Arc::new(StaticTransport::new()),
            catalogue: VersionCatalogue::new(),
            pinned_keys: BTreeMap::new(),
            trusted_keys: BTreeMap::new(),
        }
    }

    /// Constructs a fetcher that uses `transport` for every
    /// network-backed source kind.
    #[must_use]
    pub fn with_transport(options: FetchOptions, transport: Arc<dyn Transport>) -> Self {
        Self {
            options,
            transport,
            catalogue: VersionCatalogue::new(),
            pinned_keys: BTreeMap::new(),
            trusted_keys: BTreeMap::new(),
        }
    }

    /// Attaches a pre-populated catalogue so registry fetches can
    /// look up `(download_url, sha256)` without re-hitting the index.
    #[must_use]
    pub fn with_catalogue(mut self, catalogue: VersionCatalogue) -> Self {
        self.catalogue = catalogue;
        self
    }

    /// Pins publisher keys (project id → hex public key), normally
    /// loaded from the existing lockfile. A registry fetch whose
    /// advertised key differs from its pin is rejected.
    #[must_use]
    pub fn with_pinned_keys(mut self, pinned_keys: BTreeMap<String, String>) -> Self {
        self.pinned_keys = pinned_keys;
        self
    }

    /// Supplies publisher keys authorized by the root project manifest.
    /// A registry index can advertise signatures, but cannot make a new
    /// key trusted on its own; use this only for reviewed package-id/key
    /// bindings.
    #[must_use]
    pub fn with_trusted_publisher_keys(mut self, trusted_keys: BTreeMap<String, String>) -> Self {
        self.trusted_keys = trusted_keys
            .into_iter()
            .map(|(id, key)| (id, key.to_ascii_lowercase()))
            .collect();
        self
    }

    /// Returns the transport this fetcher uses.
    #[must_use]
    pub fn transport(&self) -> &Arc<dyn Transport> {
        &self.transport
    }

    /// Returns the configured options.
    #[must_use]
    pub fn options(&self) -> &FetchOptions {
        &self.options
    }

    /// Returns the catalogue this fetcher consults for registry lookups.
    #[must_use]
    pub fn catalogue(&self) -> &VersionCatalogue {
        &self.catalogue
    }

    /// Mutable handle to the catalogue.
    pub fn catalogue_mut(&mut self) -> &mut VersionCatalogue {
        &mut self.catalogue
    }

    /// Resolves every entry in `resolved` and inserts its source tree
    /// into `cache`. Returns one [`Fetched`] per entry in input order.
    pub fn fetch_all(
        &self,
        resolved: &[Resolved],
        cache: &mut Cache,
    ) -> Result<Vec<Fetched>, CacheError> {
        let mut out = Vec::with_capacity(resolved.len());
        for entry in resolved {
            out.push(self.fetch_one(entry, cache)?);
        }
        Ok(out)
    }

    fn fetch_one(&self, resolved: &Resolved, cache: &mut Cache) -> Result<Fetched, CacheError> {
        let (source, owner_pubkey) = match &resolved.pin {
            ResolvedSource::Path(path) => (fetch_path(resolved, Path::new(path))?, None),
            ResolvedSource::Git { url, reference } => {
                (self.fetch_git(resolved, url, reference)?, None)
            }
            ResolvedSource::Registry(version) => self.fetch_registry(resolved, version)?,
            ResolvedSource::Tarball { url, sha256: hash } => {
                (self.fetch_tarball(resolved, url, hash, None)?, None)
            }
        };
        if self.options.offline && !cache.contains(&source.digest) {
            return Err(CacheError::Unsupported(format!(
                "{}: offline mode and entry not in cache",
                resolved.id
            )));
        }
        // Do not report a fetched dependency as available when its
        // content-addressed cache entry could not be safely persisted.
        cache.insert_checked_ref(&source)?;
        Ok(Fetched {
            resolved: resolved.clone(),
            source,
            owner_pubkey,
        })
    }

    /// Downloads `url`, verifies its sha256, and - when `signature` is
    /// supplied (registry sources) - authenticates the publisher
    /// signature over that verified digest before unpacking. Both checks run
    /// before [`tar::unpack`], so a tampered or unsigned payload never
    /// reaches the filesystem.
    fn fetch_tarball(
        &self,
        resolved: &Resolved,
        url: &str,
        expected_sha256: &str,
        signature: Option<SignatureCheck<'_>>,
    ) -> Result<CachedSource, CacheError> {
        let (path, actual) = self.download_tarball(url, &resolved.id)?;
        let result = (|| {
            if actual != expected_sha256 {
                return Err(CacheError::DigestMismatch {
                    id: resolved.id.as_str().to_string(),
                    expected: expected_sha256.to_string(),
                    found: actual,
                });
            }
            if let Some(check) = signature {
                check.verify_digest(&resolved.id, &actual)?;
            }
            let file = File::open(&path).map_err(|e| CacheError::CacheIo {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
            let files = tar::unpack_reader(file).map_err(|e| {
                CacheError::Unsupported(format!("{}: tarball unpack failed: {e}", resolved.id))
            })?;
            Ok(CachedSource::build(resolved.id.clone(), files))
        })();
        let _ = fs::remove_file(path);
        result
    }

    /// Streams a tarball into a private temporary spool while computing its
    /// SHA-256. This bounds network-to-memory transfer to one 8 KiB buffer;
    /// the map-shaped cache API remains the only necessary materialisation.
    fn download_tarball(
        &self,
        url: &str,
        id: &crate::id::ProjectId,
    ) -> Result<(std::path::PathBuf, String), CacheError> {
        let (path, file) = create_tarball_spool().map_err(|e| CacheError::CacheIo {
            path: std::env::temp_dir().display().to_string(),
            reason: e.to_string(),
        })?;
        let mut writer = HashingFile::new(file, tar::MAX_PACKAGE_ARCHIVE_BYTES);
        let transfer = self.transport.get_to_writer(url, &mut writer);
        let finish = writer.finish();
        match (transfer, finish) {
            (Ok(()), Ok(actual)) => Ok((path, actual)),
            (Err(err), _) => {
                let _ = fs::remove_file(&path);
                Err(map_transport_error(id, err))
            }
            (_, Err(err)) => {
                let _ = fs::remove_file(&path);
                Err(CacheError::Unsupported(format!(
                    "{id}: tarball spool failed: {err}"
                )))
            }
        }
    }

    fn fetch_registry(
        &self,
        resolved: &Resolved,
        version: &crate::version::Version,
    ) -> Result<(CachedSource, Option<String>), CacheError> {
        let entry = self.catalogue.entry(&resolved.id, version).ok_or_else(|| {
            CacheError::Unsupported(format!(
                "{}: registry catalogue has no entry for {version}; \
                 did you run `gos fetch` to hydrate the index?",
                resolved.id
            ))
        })?;
        if entry.yanked && !self.options.allow_yanked {
            return Err(CacheError::Yanked {
                id: resolved.id.as_str().to_string(),
                version: version.to_string(),
                reason: entry
                    .yank_reason
                    .clone()
                    .unwrap_or_else(|| "(no reason given)".to_string()),
            });
        }
        let url = registry_download_url(self.options.registry_url.as_deref(), &resolved.id, entry)?;
        let expected = entry.tarball_sha256.clone().ok_or_else(|| {
            CacheError::Unsupported(format!(
                "{}: registry entry for {version} is missing a sha256 pin",
                resolved.id
            ))
        })?;
        // Registry sources must be signed by the publisher. The
        // signature is verified over the tarball bytes, and the
        // advertised key is checked against any lockfile pin so a
        // compromised registry cannot silently swap publishers.
        let (signature, public_key) = match (&entry.signature, &entry.public_key) {
            (Some(s), Some(k)) => (s.clone(), k.clone()),
            _ => return Err(CacheError::Unsigned(resolved.id.as_str().to_string())),
        };
        let trusted = self
            .pinned_keys
            .get(resolved.id.as_str())
            .or_else(|| self.trusted_keys.get(resolved.id.as_str()))
            .ok_or_else(|| CacheError::UntrustedPublisher {
                id: resolved.id.as_str().to_string(),
                offered: public_key.clone(),
            })?;
        let check = SignatureCheck {
            signature_hex: &signature,
            public_key_hex: &public_key,
            trusted_key: trusted,
        };
        let source = self.fetch_tarball(resolved, &url, &expected, Some(check))?;
        Ok((source, Some(public_key)))
    }

    fn fetch_git(
        &self,
        resolved: &Resolved,
        url: &str,
        reference: &str,
    ) -> Result<CachedSource, CacheError> {
        validate_git_source(url, reference)
            .map_err(|msg| CacheError::RejectedGitSource(format!("{}: {msg}", resolved.id)))?;
        let cache_root = crate::cache::default_cache_root().ok_or_else(|| {
            CacheError::Unsupported(format!(
                "{}: git fetch needs a writable cache (set GOS_CACHE_DIR or $HOME)",
                resolved.id
            ))
        })?;
        let bare_dir = cache_root.join("git").join(sha256::hex(url.as_bytes()));
        ensure_git_clone(url, &bare_dir).map_err(|e| {
            CacheError::Unsupported(format!("{}: git fetch failed: {e}", resolved.id))
        })?;
        // A branch or tag is resolved against the clone that was just
        // refreshed, so what the tree is read from - and what a lockfile
        // pins - is the immutable object the name pointed at.
        let object_id = resolve_git_ref(&bare_dir, reference)
            .map_err(|e| CacheError::RejectedGitSource(format!("{}: {e}", resolved.id)))?;
        let tarball = git_archive(&bare_dir, &object_id).map_err(|e| {
            CacheError::Unsupported(format!("{}: git archive failed: {e}", resolved.id))
        })?;
        let files = tar::unpack(&tarball).map_err(|e| {
            CacheError::Unsupported(format!("{}: tarball unpack failed: {e}", resolved.id))
        })?;
        Ok(CachedSource::build(resolved.id.clone(), files))
    }
}

static TARBALL_SPOOL_ID: AtomicU64 = AtomicU64::new(0);

fn create_tarball_spool() -> io::Result<(std::path::PathBuf, File)> {
    let root = std::env::temp_dir();
    for _ in 0..32 {
        let path = root.join(format!(
            "gos-pkg-{}-{}.tar",
            std::process::id(),
            TARBALL_SPOOL_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        // A package archive can contain private source before signature and
        // path validation. Do not rely on the caller's umask for the spool's
        // confidentiality; the mode is applied atomically with `create_new`.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique package tarball spool",
    ))
}

struct HashingFile {
    file: File,
    hasher: sha256::Hasher,
    written: usize,
    limit: usize,
}

impl HashingFile {
    fn new(file: File, limit: usize) -> Self {
        Self {
            file,
            hasher: sha256::Hasher::new(),
            written: 0,
            limit,
        }
    }

    fn finish(mut self) -> io::Result<String> {
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(self.hasher.finalize_hex())
    }
}

impl Write for HashingFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .written
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tarball size overflow"))?;
        if next > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tarball exceeds {}-byte limit", self.limit),
            ));
        }
        self.file.write_all(bytes)?;
        self.hasher.update(bytes);
        self.written = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Publisher-signature inputs for a registry tarball.
struct SignatureCheck<'a> {
    /// Hex ed25519 signature over the tarball's lowercase SHA-256 hex digest,
    /// which is what `gos publish` signs.
    signature_hex: &'a str,
    /// Hex ed25519 public key the registry advertises.
    public_key_hex: &'a str,
    /// Key trusted by the lockfile or root manifest.
    trusted_key: &'a str,
}

impl SignatureCheck<'_> {
    /// Rejects a key that disagrees with the lockfile pin, then verifies
    /// the signature over `digest`, the archive's verified SHA-256. Public
    /// keys are not secret, so the pin comparison need not be constant-time.
    fn verify_digest(&self, id: &crate::id::ProjectId, digest: &str) -> Result<(), CacheError> {
        if !self.trusted_key.eq_ignore_ascii_case(self.public_key_hex) {
            return Err(CacheError::KeyMismatch {
                id: id.as_str().to_string(),
                pinned: self.trusted_key.to_string(),
                offered: self.public_key_hex.to_string(),
            });
        }
        crate::signing::verify_signature_hex(
            self.public_key_hex,
            digest.to_ascii_lowercase().as_bytes(),
            self.signature_hex,
        )
        .map_err(|_| CacheError::SignatureInvalid(id.as_str().to_string()))
    }
}

fn registry_download_url(
    registry_url: Option<&str>,
    id: &crate::id::ProjectId,
    entry: &CatalogueEntry,
) -> Result<String, CacheError> {
    if let Some(url) = &entry.download_url {
        return Ok(url.clone());
    }
    let base = registry_url.ok_or_else(|| CacheError::NoRegistry(id.as_str().to_string()))?;
    Ok(format!(
        "{base}/v1/download/{id}/{version}.tar",
        base = base.trim_end_matches('/'),
        id = id.as_str(),
        version = entry.version,
    ))
}

fn map_transport_error(id: &crate::id::ProjectId, err: TransportError) -> CacheError {
    CacheError::Unsupported(format!("{id}: transport: {err}"))
}

fn fetch_path(resolved: &Resolved, base: &Path) -> Result<CachedSource, CacheError> {
    let mut files = BTreeMap::new();
    let mut total_bytes = 0usize;
    walk_path(base, base, &mut files, &mut total_bytes).map_err(|_| {
        CacheError::PathUnreadable {
            id: resolved.id.as_str().to_string(),
            path: base.display().to_string(),
        }
    })?;
    Ok(CachedSource::build(resolved.id.clone(), files))
}

fn walk_path(
    base: &Path,
    current: &Path,
    out: &mut BTreeMap<String, Vec<u8>>,
    total_bytes: &mut usize,
) -> std::io::Result<()> {
    let file_type = std::fs::symlink_metadata(current)?.file_type();
    if file_type.is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("symlink in path dependency: {}", current.display()),
        ));
    }
    if file_type.is_file() {
        if out.len() >= MAX_CACHED_SOURCE_FILES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("path dependency has more than {MAX_CACHED_SOURCE_FILES} files"),
            ));
        }
        let remaining = MAX_CACHED_SOURCE_BYTES.saturating_sub(*total_bytes);
        let bytes = read_file_bounded(current, MAX_CACHED_SOURCE_FILE_BYTES.min(remaining))?;
        *total_bytes = total_bytes.checked_add(bytes.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "source byte count overflow",
            )
        })?;
        let key = relative_key(base, current);
        out.insert(key, bytes);
        return Ok(());
    }
    if !file_type.is_dir() {
        return Ok(());
    }
    // The output map is ordered, so directory traversal need not materialize
    // and sort a potentially huge list of child paths just for hashing.
    for entry in std::fs::read_dir(current)? {
        walk_path(base, &entry?.path(), out, total_bytes)?;
    }
    Ok(())
}

fn relative_key(base: &Path, file: &Path) -> String {
    file.strip_prefix(base)
        .ok()
        .map_or_else(
            || file.display().to_string(),
            |p| p.to_string_lossy().into_owned(),
        )
        .replace('\\', "/")
}

/// Rejects a git dependency URL or ref that could turn `git` into an
/// arbitrary-command or argv-injection vector before any git process
/// runs.
///
/// A `<transport>::<address>` prefix (for example `ext::sh -c '...'`)
/// selects a git remote helper, and `ext::` runs a shell command, so
/// only the authenticated `https://` and `ssh://` transports are allowed;
/// plaintext `git://`, `file://`, and every remote-helper prefix are refused.
/// A URL or ref beginning with `-` would be parsed by git as an option rather
/// than a positional argument, so both are rejected here (and every callee
/// passes `--` before positional URL arguments as a second guard). A ref must
/// also be a full immutable git object ID; moving branches and tags cannot be
/// recorded as reproducible dependency pins.
fn validate_git_source(url: &str, reference: &str) -> Result<(), String> {
    // A `::` before the first `/` is a remote-helper transport prefix
    // (`ext::`, `fd::`, ...); reject it outright.
    if let Some(idx) = url.find("::")
        && !url[..idx].contains('/')
    {
        return Err(format!("git url uses a disallowed transport prefix: {url}"));
    }
    let scheme_ok = url.starts_with("https://") || url.starts_with("ssh://");
    if !scheme_ok {
        return Err(format!(
            "git url scheme not allowed (only https://, ssh://): {url}"
        ));
    }
    if url.starts_with('-') {
        return Err(format!("git url may not begin with '-': {url}"));
    }
    validate_git_ref(reference)
}

/// `true` when `reference` names an immutable object outright.
fn is_object_id(reference: &str) -> bool {
    matches!(reference.len(), 40 | 64) && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Accepts an object ID, or a branch / tag name safe to hand to `git`.
///
/// A moving name is resolved to the object it points at before anything is
/// read from it, and that object is what a lockfile records, so naming a
/// branch costs nothing in reproducibility. The characters refused here are
/// the ones that would let a name act as an option, a refspec, or a second
/// argument rather than as the ref it looks like.
fn validate_git_ref(reference: &str) -> Result<(), String> {
    // Checked before the object-id shortcut so no spelling of a ref can be
    // read as an option by the `git` invocations that take it positionally.
    if reference.starts_with('-') {
        return Err(format!("git ref may not begin with '-': {reference}"));
    }
    if is_object_id(reference) {
        return Ok(());
    }
    if reference.is_empty() {
        return Err("git ref may not be empty".to_string());
    }
    if reference.len() > 250 {
        return Err(format!("git ref is too long: {reference}"));
    }
    let rejected = |c: char| {
        c.is_whitespace()
            || c.is_control()
            || matches!(
                c,
                ':' | '?' | '[' | ']' | '\\' | '^' | '~' | '*' | '@' | '\'' | '"'
            )
    };
    if let Some(c) = reference.chars().find(|c| rejected(*c)) {
        return Err(format!(
            "git ref may not contain `{c}`, which git reads as a pattern or a refspec: {reference}"
        ));
    }
    if reference.contains("..")
        || reference.starts_with('/')
        || reference.ends_with('/')
        || reference.strip_suffix(".lock").is_some()
        || reference.starts_with('.')
    {
        return Err(format!(
            "git ref is not a well-formed branch or tag name: {reference}"
        ));
    }
    Ok(())
}

/// The object ID `reference` names in the clone at `bare_dir`.
///
/// A branch and a tag are looked up under their own namespaces before the
/// bare name is tried, so a repository carrying both cannot decide which one
/// a manifest meant. The answer is always a commit id, which is what the
/// tree is read from and what the lockfile pins.
fn resolve_git_ref(bare_dir: &Path, reference: &str) -> std::io::Result<String> {
    if is_object_id(reference) {
        return Ok(reference.to_string());
    }
    let candidates = [
        format!("refs/tags/{reference}"),
        format!("refs/heads/{reference}"),
        format!("refs/remotes/origin/{reference}"),
    ];
    for candidate in &candidates {
        let out = hardened_git()
            .arg("--git-dir")
            .arg(bare_dir)
            .arg("rev-parse")
            .arg("--verify")
            .arg("--end-of-options")
            .arg(format!("{candidate}^{{commit}}"))
            .output()?;
        if out.status.success() {
            let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if is_object_id(&id) {
                return Ok(id);
            }
        }
    }
    Err(std::io::Error::other(format!(
        "git ref `{reference}` names no tag or branch in this repository"
    )))
}

/// A `git` invocation with the remote-helper (`ext`) and local
/// (`file`) transports disabled and interactive protocol promotion
/// turned off, so a hostile URL or repository config cannot execute a
/// command. Every network-facing git call in this module starts here.
fn hardened_git() -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-c")
        .arg("protocol.ext.allow=never")
        .arg("-c")
        .arg("protocol.file.allow=never")
        .env("GIT_PROTOCOL_FROM_USER", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

/// Ensures `bare_dir` contains a bare clone of `url`, creating it
/// (or refreshing) as needed.
fn ensure_git_clone(url: &str, bare_dir: &Path) -> std::io::Result<()> {
    if bare_dir.join("HEAD").is_file() {
        let out = hardened_git()
            .arg("--git-dir")
            .arg(bare_dir)
            .arg("fetch")
            .arg("--all")
            .arg("--tags")
            .output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "git fetch in {}: {}",
                bare_dir.display(),
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        return Ok(());
    }
    if let Some(parent) = bare_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // `--` separates options from the positional repository and target
    // directory, so a URL that slipped a leading `-` past validation
    // still cannot be read as an option.
    let out = hardened_git()
        .arg("clone")
        .arg("--bare")
        .arg("--")
        .arg(url)
        .arg(bare_dir)
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git clone {url} -> {}: {}",
            bare_dir.display(),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Produces a tar buffer of the project tree at `reference` from the
/// bare clone at `bare_dir`.
fn git_archive(bare_dir: &Path, reference: &str) -> std::io::Result<Vec<u8>> {
    // `git archive` reads the tree-ish positionally and treats anything
    // after `--` as a pathspec, so the ref cannot be guarded with `--`;
    // `validate_git_source` has already rejected a ref beginning with
    // `-` (argv injection) before this runs.
    let out = hardened_git()
        .arg("--git-dir")
        .arg(bare_dir)
        .arg("archive")
        .arg("--format=tar")
        .arg(reference)
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git archive {} {reference}: {}",
            bare_dir.display(),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(out.stdout)
}

/// Implements `gos vendor` by writing every cached source tree under
/// `dest_dir/<id-with-slashes-replaced>/`. Returns the per-id list of
/// written files.
pub fn vendor(
    fetched: &[Fetched],
    dest_dir: &Path,
) -> Result<BTreeMap<String, Vec<String>>, std::io::Error> {
    std::fs::create_dir_all(dest_dir)?;
    let mut out = BTreeMap::new();
    for entry in fetched {
        let project_dir = dest_dir.join(entry.resolved.id.as_str().replace('/', "__"));
        std::fs::create_dir_all(&project_dir)?;
        let mut written = Vec::new();
        for (path, bytes) in &entry.source.files {
            if !is_safe_package_path(path) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unsafe package path while vendoring: {path:?}"),
                ));
            }
            let target = project_dir.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, bytes)?;
            written.push(path.clone());
        }
        out.insert(entry.resolved.id.as_str().to_string(), written);
    }
    Ok(out)
}

/// Deterministic test-only synthetic-source helper, retained behind
/// `cfg(any(test, feature = "synthetic-source"))` so production
/// fetches cannot accidentally invoke it.
#[cfg(any(test, feature = "synthetic-source"))]
#[must_use]
pub fn synthetic_source_for_test(resolved: &Resolved, seed: &str) -> CachedSource {
    let mut files = BTreeMap::new();
    let body = format!(
        "// stub source for {id} (seed {seed})\nfn __stub() {{}}\n",
        id = resolved.id
    );
    files.insert("src/main.gos".to_string(), body.into_bytes());
    let digest_seed = format!("{}\0{seed}", resolved.id);
    let digest = sha256::hex(digest_seed.as_bytes());
    CachedSource {
        id: resolved.id.clone(),
        files,
        digest,
    }
}

#[cfg(test)]
mod git_source_tests {
    #[cfg(unix)]
    use super::create_tarball_spool;
    use super::validate_git_source;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn allowed_schemes_pass() {
        for url in [
            "https://github.com/owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
        ] {
            assert!(validate_git_source(url, COMMIT).is_ok(), "rejected {url}");
        }
    }

    #[test]
    fn plaintext_git_transport_is_rejected() {
        assert!(validate_git_source("git://example.com/repo.git", COMMIT).is_err());
    }

    #[test]
    fn ext_transport_is_rejected() {
        let err = validate_git_source("ext::sh -c 'touch /tmp/pwned'", COMMIT).unwrap_err();
        assert!(err.contains("transport prefix"), "got: {err}");
    }

    #[test]
    fn file_and_unknown_schemes_are_rejected() {
        assert!(validate_git_source("file:///etc/passwd", COMMIT).is_err());
        assert!(validate_git_source("fd::17", COMMIT).is_err());
        assert!(validate_git_source("/local/path", COMMIT).is_err());
    }

    #[test]
    fn leading_dash_url_and_ref_are_rejected() {
        assert!(validate_git_source("--upload-pack=touch /tmp/pwned", COMMIT).is_err());
        let err =
            validate_git_source("https://example.com/repo.git", "--output=/etc/x").unwrap_err();
        assert!(err.contains("ref may not begin with '-'"), "got: {err}");
    }

    #[test]
    fn branch_and_tag_references_are_accepted() {
        // A moving name is resolved to the object it points at before the
        // tree is read, so naming one is allowed; the lockfile still pins
        // the resolved commit.
        for reference in ["main", "v1.0.0", "release/2.x", "0123456", "feature_a"] {
            assert!(
                validate_git_source("https://example.com/repo.git", reference).is_ok(),
                "rejected {reference}"
            );
        }
    }

    #[test]
    fn refspec_and_pattern_characters_are_rejected() {
        for reference in [
            "main:refs/heads/evil",
            "re*lease",
            "a b",
            "he^ad",
            "we~ird",
            "with[bracket]",
            "quo'te",
            "..",
            "/leading",
            "trailing/",
            "branch.lock",
            ".hidden",
            "",
        ] {
            assert!(
                validate_git_source("https://example.com/repo.git", reference).is_err(),
                "accepted {reference}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn tarball_spool_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (path, file) = create_tarball_spool().expect("spool");
        let mode = file.metadata().expect("metadata").permissions().mode() & 0o777;
        drop(file);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            mode, 0o600,
            "package spool must not be group/world-readable"
        );
    }
}
