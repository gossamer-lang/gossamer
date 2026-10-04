//! Per-source frontend cache.
//!
//! `gos`, `gos check`, `gos test`, and `gos build` all reach the compiler
//! through [`crate::frontend::check_frontend`]. This module
//! stores that gate's complete output - the parsed [`SourceFile`], the
//! [`Resolutions`] side table, the [`TypeTable`], and the [`TyCtxt`]
//! interner - as one postcard blob addressed by a key covering every input
//! that can change the result. A second invocation over unchanged inputs
//! deserializes the blob and skips parse, resolve, typecheck,
//! exhaustiveness, and arena-escape analysis outright.
//!
//! Only a run that produced zero diagnostics publishes a blob, so a hit is
//! also proof that the program was accepted.
//!
//! Blobs live under `<project>/.gos-cache/frontend` when the working
//! directory sits inside a project, and otherwise under
//! `$XDG_CACHE_HOME/gossamer/frontend` (`$HOME/.cache/gossamer/frontend`,
//! `%LOCALAPPDATA%\gossamer\frontend` on Windows). `GOSSAMER_CACHE_DIR`
//! overrides the choice; `GOS_NO_CACHE` disables the cache entirely.
//!
//! Writes go to a uniquely-named temporary file that is renamed into place,
//! so concurrent `gos` processes never observe a partial blob. Every read is
//! length-capped and magic-checked, and a decode failure is a miss rather
//! than an error: cache contents are advisory and disposable.
//!
//! `docs_src/design/incremental.md` documents the key recipe and the
//! retention story in full.

#![forbid(unsafe_code)]

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gossamer_ast::SourceFile;
use gossamer_lex::FileId;
use gossamer_pkg::sha256;
use gossamer_resolve::Resolutions;
use gossamer_types::{TyCtxt, TypeTable};

/// Bumped whenever the payload layout or the key recipe changes, so blobs
/// written by an older schema are rejected instead of mis-decoded.
const BLOB_MAGIC: &[u8; 8] = b"GOSFC002";
const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;
static CACHE_WRITE_ID: AtomicU64 = AtomicU64::new(0);

/// The complete output of one accepted front-end pass, in the form the
/// cache persists.
#[derive(Debug, serde::Deserialize)]
pub struct CachedFrontend {
    /// Parsed and augmented AST.
    pub sf: SourceFile,
    /// Name-resolution side table.
    pub resolutions: Resolutions,
    /// Node-to-type side table.
    pub table: TypeTable,
    /// Type interner backing every [`gossamer_types::Ty`] in `table`.
    pub tcx: TyCtxt,
}

/// Borrowed mirror of [`CachedFrontend`] used on the write path so
/// publishing a blob does not deep-copy the whole front-end result.
/// postcard encodes struct fields positionally, so the two must keep the
/// same field order.
#[derive(serde::Serialize)]
struct FrontendView<'a> {
    sf: &'a SourceFile,
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
}

/// Publishes one accepted front-end result under `key`.
pub fn store_frontend(
    key: &FrontendCacheKey,
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) {
    store_frontend_in(&cache_dir(), key, sf, resolutions, table, tcx);
}

/// Variant of [`store_frontend`] that writes into `root`.
pub fn store_frontend_in(
    root: &Path,
    key: &FrontendCacheKey,
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) {
    store_blob_in(
        root,
        key,
        &FrontendView {
            sf,
            resolutions,
            table,
            tcx,
        },
    );
}

/// Content-addressed identifier for one frontend compile. The key
/// combines the source bytes with the toolchain version so a
/// compiler upgrade invalidates every cached entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FrontendCacheKey {
    hash: String,
}

impl FrontendCacheKey {
    /// Computes a cache key from `source` text and the driver
    /// toolchain identifier (typically `env!("CARGO_PKG_VERSION")`).
    #[must_use]
    pub fn new(source: &str, toolchain: &str) -> Self {
        Self::new_with_context(source, toolchain, "edition=2026")
    }

    /// Computes a cache key with an explicit semantic context. An edition
    /// changes source typing and lowering, so it must never share a cached AST
    /// with a different edition.
    #[must_use]
    pub fn new_with_context(source: &str, toolchain: &str, context: &str) -> Self {
        // The build stamp changes when the frontend crates recompile, so a
        // development rebuild with an unchanged version string cannot
        // serve ASTs parsed by older frontend code.
        let stamp = env!("GOS_DRIVER_BUILD_STAMP");
        // The stamp alone rotates only when a frontend crate's sources
        // change (its build script watches those directories). A rebuild
        // that touches other compiler crates relinks the executable
        // without re-running that script, so the running binary's own
        // identity is mixed in too - any rebuilt `gos` starts from a
        // cold frontend cache instead of consuming blobs written by a
        // different build of the compiler. Mirrors the object cache's
        // `compiler_fingerprint`.
        let exe = exe_fingerprint();
        let mut buf = Vec::with_capacity(
            source.len() + toolchain.len() + context.len() + stamp.len() + exe.len() + 10,
        );
        buf.extend_from_slice(toolchain.as_bytes());
        buf.push(0);
        buf.extend_from_slice(stamp.as_bytes());
        buf.push(0);
        buf.extend_from_slice(exe.as_bytes());
        buf.push(0);
        buf.extend_from_slice(context.as_bytes());
        buf.push(0);
        buf.extend_from_slice(source.as_bytes());
        Self {
            hash: sha256::hex(&buf),
        }
    }

    /// Returns the hex SHA-256 identifying this key.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.hash
    }
}

/// Identity of the running compiler binary (size and mtime), computed once.
/// Folded into every frontend cache key so a rebuilt `gos` - even one whose
/// frontend crates did not recompile - never reads blobs a different build
/// of the compiler wrote.
fn exe_fingerprint() -> &'static str {
    static FP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FP.get_or_init(|| {
        let mut s = String::new();
        if let Ok(exe) = std::env::current_exe()
            && let Ok(meta) = fs::metadata(&exe)
        {
            s.push_str(&format!("len={}", meta.len()));
            if let Ok(mtime) = meta.modified()
                && let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH)
            {
                s.push_str(&format!("|mtime={}", dur.as_nanos()));
            }
        }
        s
    })
}

/// Builds the cache key for one front-end pass over `source`.
///
/// Beyond the source bytes and the compiler's own identity, the key covers
/// every other input the gate reads: the language edition, the [`FileId`]
/// that anchors spans in the cached AST, the compile target, whether
/// `#[cfg(test)]` items are visible, and the registered Rust-binding
/// signatures. Imports need no separate term because the CLI hands the gate
/// a single bundled source containing every sibling module.
#[must_use]
pub fn frontend_key(source: &str, file_id: FileId) -> FrontendCacheKey {
    let bindings = gossamer_resolve::all_external_modules();
    let bindings_digest = if bindings.is_empty() {
        String::new()
    } else {
        sha256::hex(format!("{bindings:?}").as_bytes())
    };
    let context = format!(
        "file={}|target={}|cfg={}|ffi={}|cfg_test={}|bindings={bindings_digest}",
        file_id.as_u32(),
        gossamer_codegen_llvm::active_target_triple(),
        gossamer_resolve::cfg_target_key(),
        crate::foreign_policy::foreign_policy().cache_term(),
        gossamer_resolve::test_cfg_enabled(),
    );
    FrontendCacheKey::new_with_context(source, env!("CARGO_PKG_VERSION"), &context)
}

/// Reports whether the frontend cache is enabled. `GOS_NO_CACHE` turns it
/// off, matching the LLVM object cache's opt-out.
#[must_use]
pub fn cache_enabled() -> bool {
    std::env::var_os("GOS_NO_CACHE").is_none()
}

/// Resolves the directory blobs are read from and written to.
///
/// Order of precedence: the `GOSSAMER_CACHE_DIR` override, the nearest
/// ancestor project's `.gos-cache/frontend`, then the user cache root.
/// Anchoring to the project keeps a checkout's warm cache with the
/// checkout, mirroring where `gos build` puts its object and link caches.
#[must_use]
pub fn cache_dir() -> PathBuf {
    if let Some(explicit) = std::env::var_os("GOSSAMER_CACHE_DIR") {
        return PathBuf::from(explicit);
    }
    if let Some(root) = project_root() {
        return root.join(".gos-cache").join("frontend");
    }
    user_cache_root().join("frontend")
}

/// Root of the per-user toolchain cache shared by every cache class:
/// `$XDG_CACHE_HOME/gossamer`, `$HOME/.cache/gossamer`, or
/// `%LOCALAPPDATA%\gossamer`, falling back to a workspace-relative
/// `target/gossamer-cache` when no home directory is discoverable.
#[must_use]
pub fn user_cache_root() -> PathBuf {
    if cfg!(windows)
        && let Some(local) = std::env::var_os("LOCALAPPDATA")
    {
        return PathBuf::from(local).join("gossamer");
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("gossamer");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache").join("gossamer");
    }
    PathBuf::from("target").join("gossamer-cache")
}

/// Nearest ancestor of the current directory containing a `project.toml`.
fn project_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    cwd.ancestors()
        .find(|dir| dir.join("project.toml").is_file())
        .map(Path::to_path_buf)
}

/// Serializes `value` as a postcard blob keyed by `key`. Errors
/// silently - cache writes are advisory.
pub fn store_blob<T: serde::Serialize>(key: &FrontendCacheKey, value: &T) {
    store_blob_in(&cache_dir(), key, value);
}

/// Variant of [`store_blob`] that writes into `root` instead of the
/// shared cache directory.
pub fn store_blob_in<T: serde::Serialize>(root: &Path, key: &FrontendCacheKey, value: &T) {
    let _ = fs::create_dir_all(root);
    let Ok(encoded) = postcard::to_allocvec(value) else {
        return;
    };
    if encoded.len() > MAX_BLOB_BYTES {
        return;
    }
    // Do not build a second `magic + encoded` allocation. Frontend trees can
    // approach the blob ceiling, and the atomic writer already owns the file
    // handle needed to write a small prefix followed by the postcard bytes.
    let _ = write_atomic_parts(&blob_path(root, key), BLOB_MAGIC, &encoded);
}

/// Attempts to load a previously-cached blob for `key`, returning
/// `None` on any failure (absent, corrupt, wrong schema).
#[must_use]
pub fn load_blob<T: serde::de::DeserializeOwned>(key: &FrontendCacheKey) -> Option<T> {
    load_blob_in(&cache_dir(), key)
}

/// Variant of [`load_blob`] that reads from `root` instead of the
/// shared cache directory.
#[must_use]
pub fn load_blob_in<T: serde::de::DeserializeOwned>(
    root: &Path,
    key: &FrontendCacheKey,
) -> Option<T> {
    let bytes = read_capped(&blob_path(root, key), BLOB_MAGIC.len() + MAX_BLOB_BYTES)?;
    let payload = bytes.strip_prefix(BLOB_MAGIC)?;
    postcard::from_bytes(payload).ok()
}

/// Writes `bytes` directly to the cache file for `key` without any
/// envelope encoding. Use this for raw binary blobs (object files,
/// large buffers) where a serde `Vec<u8>` round-trip would force
/// an extra full-buffer clone on every cache miss.
pub fn store_raw(key: &FrontendCacheKey, bytes: &[u8]) {
    store_raw_in(&cache_dir(), key, bytes);
}

/// Variant of [`store_raw`] that writes into `root`.
pub fn store_raw_in(root: &Path, key: &FrontendCacheKey, bytes: &[u8]) {
    let _ = fs::create_dir_all(root);
    let _ = write_atomic(&blob_path(root, key), bytes);
}

/// Returns the on-disk path the cache uses for `key` if a prior
/// `store_raw` (or `store_blob`) populated it. Used by callers that
/// want to copy the cache file directly to a destination path
/// instead of round-tripping through a `Vec<u8>` in memory.
#[must_use]
pub fn raw_blob_path(key: &FrontendCacheKey) -> Option<PathBuf> {
    raw_blob_path_in(&cache_dir(), key)
}

/// Variant of [`raw_blob_path`] that consults `root`.
#[must_use]
pub fn raw_blob_path_in(root: &Path, key: &FrontendCacheKey) -> Option<PathBuf> {
    let p = blob_path(root, key);
    if p.is_file() { Some(p) } else { None }
}

fn blob_path(dir: &Path, key: &FrontendCacheKey) -> PathBuf {
    dir.join(format!("{}.bin", key.as_hex()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_with(path, |file| file.write_all(bytes))
}

fn write_atomic_parts(path: &Path, first: &[u8], second: &[u8]) -> std::io::Result<()> {
    write_atomic_with(path, |file| {
        file.write_all(first)?;
        file.write_all(second)
    })
}

fn write_atomic_with<F>(path: &Path, write: F) -> std::io::Result<()>
where
    F: FnOnce(&mut fs::File) -> std::io::Result<()>,
{
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "cache path has no parent")
    })?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("cache"),
        std::process::id(),
        CACHE_WRITE_ID.fetch_add(1, Ordering::Relaxed),
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        write(&mut file)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Reads one cache blob with a sentinel byte. A prior `metadata` size check is
/// insufficient because another process can replace the file before the read;
/// taking at most one extra byte keeps advisory cache corruption from becoming
/// an unbounded allocation.
fn read_capped(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(limit.saturating_add(1)).ok()?)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_deterministic_for_the_same_source() {
        let a = FrontendCacheKey::new("fn main() {}\n", "0.0.0");
        let b = FrontendCacheKey::new("fn main() {}\n", "0.0.0");
        assert_eq!(a, b);
    }

    #[test]
    fn cache_key_changes_when_toolchain_version_changes() {
        let a = FrontendCacheKey::new("fn main() {}\n", "0.0.0");
        let b = FrontendCacheKey::new("fn main() {}\n", "0.0.1");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_changes_when_the_context_changes() {
        let host = FrontendCacheKey::new_with_context("fn main() {}\n", "0.0.0", "target=host");
        let cross = FrontendCacheKey::new_with_context("fn main() {}\n", "0.0.0", "target=cross");
        assert_ne!(host, cross);
    }

    #[test]
    fn frontend_key_separates_sources_and_file_ids() {
        let mut map = gossamer_lex::SourceMap::new();
        let first = map.add_file("a.gos", "fn main() {}\n".to_string());
        let second = map.add_file("b.gos", "fn main() {}\n".to_string());
        let base = frontend_key("fn main() {}\n", first);
        assert_eq!(base, frontend_key("fn main() {}\n", first));
        assert_ne!(base, frontend_key("fn main() {}\n", second));
        assert_ne!(base, frontend_key("fn main() { }\n", first));
    }

    #[test]
    fn cache_key_changes_with_any_source_byte_change() {
        let a = FrontendCacheKey::new("fn main() {}\n", "0.0.0");
        let b = FrontendCacheKey::new("fn main() { }\n", "0.0.0");
        assert_ne!(a, b);
    }

    #[test]
    fn blob_round_trip_preserves_value() {
        let tmp = tempdir();
        let key = FrontendCacheKey::new("fn a() {}\n", "test");
        let payload = vec!["alpha".to_string(), "beta".to_string()];
        assert!(load_blob_in::<Vec<String>>(&tmp, &key).is_none());
        store_blob_in(&tmp, &key, &payload);
        let round_trip: Vec<String> = load_blob_in(&tmp, &key).expect("blob not found");
        assert_eq!(round_trip, payload);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn blob_loader_rejects_legacy_and_oversized_files_before_decode() {
        let tmp = tempdir();
        let key = FrontendCacheKey::new("fn a() {}\n", "test");
        let path = blob_path(&tmp, &key);
        fs::write(&path, postcard::to_allocvec(&vec!["legacy"]).unwrap()).unwrap();
        assert!(load_blob_in::<Vec<String>>(&tmp, &key).is_none());

        let file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(u64::try_from(BLOB_MAGIC.len() + MAX_BLOB_BYTES + 1).unwrap())
            .unwrap();
        assert!(load_blob_in::<Vec<String>>(&tmp, &key).is_none());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn capped_reader_rejects_the_sentinel_byte() {
        let tmp = tempdir();
        let path = tmp.join("blob");
        fs::write(&path, b"four").unwrap();
        assert!(read_capped(&path, 3).is_none());
        assert_eq!(read_capped(&path, 4), Some(b"four".to_vec()));
        let _ = fs::remove_dir_all(tmp);
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let path = base.join(format!("gossamer-cache-test-{pid}-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
