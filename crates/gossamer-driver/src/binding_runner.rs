//! Per-project Rust-binding runner.
//!
//! When a `project.toml` declares a non-empty `[rust-bindings]`
//! section, `gos` / `gos build` re-execs into a *runner*
//! binary that statically links every binding's Cargo crate. The
//! runner is built on demand by Cargo and cached under
//! `$XDG_CACHE_HOME/gossamer/runners/<fp>` keyed by the manifest's
//! [`Manifest::rust_binding_fingerprint`].
//!
//! Three artefacts can be materialised under the same workdir:
//!
//! - `runner/` - the executable runner used by `gos`.
//! - `staticlib/` - `libgos_static_bindings.a` (or
//!   `gos_static_bindings.lib` on Windows MSVC) used by the
//!   compiled-mode link step.
//! - `sigs/signatures.json` - JSON dump of every binding's module
//!   + item signature, fed to the resolver / typechecker.

// `deny` rather than `forbid` so `pid_alive` can opt into its FFI liveness
// probe via a scoped `#[allow(unsafe_code)]`; nothing else here uses unsafe.
#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use parking_lot::Mutex;

use gossamer_pkg::{GitRef, Manifest, RustBindingSpec};
use gossamer_runner_template::{
    BindingEntry, GossamerPatchSource, Profile as TmplProfile, RenderInput, render_cargo_toml,
    render_main_rs, render_sigs_dump_rs, render_staticlib_cargo_toml, render_staticlib_lib_rs,
};
use thiserror::Error;

/// Cache subdirectory of the runner executable.
/// Cargo's resolved dependency graph. A generated project without one
/// resolves every dependency afresh against the registry on each build.
const CARGO_LOCK: &str = "Cargo.lock";

/// Copy of the toolchain lockfile a generated project was last seeded from,
/// so a toolchain whose pins have moved re-seeds instead of building on the
/// graph an older one resolved.
const LOCK_SEED_STAMP: &str = ".gos-lock-seed";

/// Name of the lock file guarding one runner workdir.
const BUILD_LOCK: &str = ".gos-build.lock";

const SUBDIR_RUNNER: &str = "runner";
/// Cache subdirectory of the staticlib build.
const SUBDIR_STATICLIB: &str = "staticlib";
/// Cache subdirectory of the signatures dump.
const SUBDIR_SIGS: &str = "sigs";

/// Filename Cargo lands the bindings staticlib at, matching the
/// `[lib] name = "gos_static_bindings"` entry of the generated
/// `Cargo.toml`.
fn staticlib_archive_filename() -> &'static str {
    if cfg!(all(windows, target_env = "msvc")) {
        "gos_static_bindings.lib"
    } else {
        "libgos_static_bindings.a"
    }
}

/// Errors raised by [`BindingRunner`] / [`StaticBindingsLib`].
#[derive(Debug, Error)]
pub enum BindingRunnerError {
    /// `cargo` was not found on PATH.
    #[error("this project declares `[rust-bindings]`; install Rust + cargo from https://rustup.rs")]
    CargoMissing,
    /// `cargo build` failed for the runner / staticlib.
    #[error("cargo build failed for binding `{crate_name}`:\n{stderr}")]
    CargoFailed {
        /// Crate that failed (or `<runner>` / `<staticlib>` when the
        /// failure can't be attributed to one binding).
        crate_name: String,
        /// Captured cargo stderr, verbatim.
        stderr: String,
    },
    /// I/O error while preparing the cache.
    #[error("cache i/o error: {0}")]
    Io(#[from] io::Error),
    /// The project manifest is malformed (e.g. a bare `[project] id`).
    /// A present-but-invalid manifest is a hard error, never a silent
    /// "no bindings".
    #[error("manifest error: {0}")]
    Manifest(String),
    /// Template rendering failed (unexpected - rendering is total).
    #[error("template render failed: {0}")]
    Render(String),
    /// Signatures dump produced unparseable JSON.
    #[error("signature dump produced invalid json: {0}")]
    BadSignatureJson(String),
}

/// Profile (debug / release) for the runner build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// `cargo build` (no `--release`).
    Debug,
    /// `cargo build --release`.
    Release,
}

impl Profile {
    fn template_profile(self) -> TmplProfile {
        match self {
            Self::Debug => TmplProfile::Debug,
            Self::Release => TmplProfile::Release,
        }
    }

    /// Cargo profile dirname.
    #[must_use]
    pub fn dir(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

/// Materialised binding metadata used by all three artefacts.
#[derive(Debug, Clone)]
pub struct RenderedBinding {
    /// Cargo crate name (matches the `[rust-bindings]` key).
    pub crate_name: String,
    /// Cargo dep line, e.g. `foo = { path = "/abs/path" }`.
    pub cargo_dep_line: String,
    /// Cargo features requested for this binding.
    pub features: Vec<String>,
    /// For path-deps, the resolved absolute crate root. Used for
    /// the source-tree mtime walk.
    pub local_root: Option<PathBuf>,
}

/// A per-project runner build.
#[derive(Debug)]
pub struct BindingRunner {
    /// Full SHA-256 of the manifest's binding set.
    pub fingerprint: [u8; 32],
    /// 12-char hex prefix of [`Self::fingerprint`].
    pub fingerprint_hex: String,
    /// Workdir under the cache (`<cache>/runners/<fp>/`).
    pub workdir: PathBuf,
    /// Bindings to link into the runner.
    pub bindings: Vec<RenderedBinding>,
    /// Absolute path to the gossamer source tree (for path deps in
    /// the rendered Cargo.toml).
    pub gossamer_root: PathBuf,
    /// Cargo profile to build with.
    pub profile: Profile,
    /// Project id for cosmetic comments in the rendered files.
    pub project_id: String,
    /// `[patch]` sources whose `gossamer-*` crates must be redirected
    /// to [`Self::gossamer_root`] so the runner links one gossamer-runtime.
    pub patch_sources: Vec<GossamerPatchSource>,
}

impl BindingRunner {
    /// Constructs a runner from the manifest. Returns
    /// `Ok(None)` if `[rust-bindings]` is empty.
    ///
    /// `manifest_dir` is the directory containing `project.toml`;
    /// path-deps in the manifest resolve against it.
    /// `gossamer_root` is the absolute path of this checkout (the
    /// directory containing the workspace `Cargo.toml`).
    pub fn from_manifest(
        manifest: &Manifest,
        manifest_dir: &Path,
        gossamer_root: &Path,
        profile: Profile,
    ) -> io::Result<Option<Self>> {
        let cache = cache_root()?;
        // Prune after the workdir is resolved, so this run's own artifacts
        // are the one thing the reclamation is told to keep.
        let resolved =
            Self::from_manifest_in(manifest, manifest_dir, gossamer_root, profile, &cache)?;
        prune_runner_cache_once(&cache, resolved.as_ref().map(|r| r.workdir.as_path()));
        Ok(resolved)
    }

    /// Same as [`Self::from_manifest`] but uses an explicit cache
    /// root instead of reading `GOSSAMER_CACHE` / `XDG_CACHE_HOME`.
    pub fn from_manifest_in(
        manifest: &Manifest,
        manifest_dir: &Path,
        gossamer_root: &Path,
        profile: Profile,
        cache_root: &Path,
    ) -> io::Result<Option<Self>> {
        if manifest.rust_bindings.is_empty() {
            return Ok(None);
        }
        let fingerprint = manifest.rust_binding_fingerprint(manifest_dir);
        let fingerprint_hex = hex_prefix(&fingerprint, 6);
        let workdir = cache_root.join("runners").join(&fingerprint_hex);
        fs::create_dir_all(&workdir)?;
        let bindings = render_bindings(&manifest.rust_bindings, manifest_dir);
        let patch_sources = detect_patch_sources(&manifest.rust_bindings, manifest_dir);
        Ok(Some(Self {
            fingerprint,
            fingerprint_hex,
            workdir,
            bindings,
            gossamer_root: gossamer_root.to_path_buf(),
            profile,
            project_id: manifest.project.id.as_str().to_string(),
            patch_sources,
        }))
    }

    /// Returns the path where the runner binary will live after
    /// `ensure_built`.
    #[must_use]
    pub fn runner_binary_path(&self) -> PathBuf {
        self.workdir
            .join(SUBDIR_RUNNER)
            .join("target")
            .join(self.profile.dir())
            .join(if cfg!(windows) {
                "gos-runner.exe"
            } else {
                "gos-runner"
            })
    }

    /// Idempotently builds the runner. Returns the produced binary under a
    /// lease that keeps reclamation off the workdir until it is dropped.
    pub fn ensure_built(&self) -> Result<WorkdirLease, BindingRunnerError> {
        // The lock comes before the directory: reclamation removes this
        // workdir under the same lock, so taking it first is what keeps a
        // build's files out of a removal already in flight.
        let _lock = AdvisoryLock::acquire(&build_lock_path(&self.workdir))?;
        let dir = self.workdir.join(SUBDIR_RUNNER);
        fs::create_dir_all(&dir)?;

        let cargo_toml = dir.join("Cargo.toml");
        let main_rs = dir.join("main.rs");
        let sigs_rs = dir.join("sigs_dump.rs");

        let input = self.render_input(self.profile.template_profile());
        write_if_different(&cargo_toml, &render_cargo_toml(&input))?;
        write_if_different(&main_rs, &render_main_rs(&input))?;
        write_if_different(&sigs_rs, &render_sigs_dump_rs(&input))?;
        seed_lockfile(&dir, &self.gossamer_root)?;

        let bin_path = self.runner_binary_path();
        let stamp = dir.join("stamp.json");
        if self.is_fresh(&bin_path, &stamp, "runner")? {
            return Ok(WorkdirLease::take(&self.workdir, bin_path)?);
        }
        run_cargo_build(
            &cargo_toml,
            &dir.join("target"),
            self.profile,
            None,
            "--bin",
            "gos-runner",
            "<runner>",
        )?;
        write_stamp(&stamp, &self.fingerprint_hex, self.profile, "runner")?;
        // A single Cargo invocation can cross the runner budget after the
        // startup prune. Keep this workdir locked while reclaiming older
        // artifacts so concurrent and just-produced outputs are protected.
        if let Some(root) = self.workdir.parent() {
            let _ = crate::cache_maintenance::prune_runner_root(
                root,
                crate::cache_maintenance::CachePolicy::default(),
                false,
                Some(&self.workdir),
            );
        }
        Ok(WorkdirLease::take(&self.workdir, bin_path)?)
    }

    /// Idempotently builds the signatures bin and runs it, returning
    /// `signatures.json` under a lease that keeps reclamation off the workdir
    /// until it is dropped.
    pub fn ensure_signatures(&self) -> Result<WorkdirLease, BindingRunnerError> {
        // Reuse the runner's Cargo.toml - the sigs-dump bin lives
        // alongside the runner bin in the same crate.
        let _lock = AdvisoryLock::acquire(&build_lock_path(&self.workdir))?;
        let dir = self.workdir.join(SUBDIR_RUNNER);
        fs::create_dir_all(&dir)?;

        let cargo_toml = dir.join("Cargo.toml");
        let main_rs = dir.join("main.rs");
        let sigs_rs = dir.join("sigs_dump.rs");

        let input = self.render_input(self.profile.template_profile());
        write_if_different(&cargo_toml, &render_cargo_toml(&input))?;
        write_if_different(&main_rs, &render_main_rs(&input))?;
        write_if_different(&sigs_rs, &render_sigs_dump_rs(&input))?;
        seed_lockfile(&dir, &self.gossamer_root)?;

        let bin_path = dir
            .join("target")
            .join(self.profile.dir())
            .join(if cfg!(windows) {
                "gos-sigs-dump.exe"
            } else {
                "gos-sigs-dump"
            });
        let sigs_dir = self.workdir.join(SUBDIR_SIGS);
        fs::create_dir_all(&sigs_dir)?;
        let json_path = sigs_dir.join("signatures.json");
        let stamp = sigs_dir.join("stamp.json");
        if self.is_fresh(&json_path, &stamp, "sigs")? && bin_path.exists() {
            return Ok(WorkdirLease::take(&self.workdir, json_path)?);
        }
        run_cargo_build(
            &cargo_toml,
            &dir.join("target"),
            self.profile,
            None,
            "--bin",
            "gos-sigs-dump",
            "<sigs>",
        )?;
        if let Some(root) = self.workdir.parent() {
            let _ = crate::cache_maintenance::prune_runner_root(
                root,
                crate::cache_maintenance::CachePolicy::default(),
                false,
                Some(&self.workdir),
            );
        }
        let mut out = Command::new(&bin_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut buf = String::new();
        if let Some(mut s) = out.stdout.take() {
            s.read_to_string(&mut buf)?;
        }
        let status = out.wait()?;
        if !status.success() {
            let mut err = String::new();
            if let Some(mut s) = out.stderr.take() {
                let _ = s.read_to_string(&mut err);
            }
            return Err(BindingRunnerError::CargoFailed {
                crate_name: "<sigs-dump>".to_string(),
                stderr: err,
            });
        }
        // Atomic write.
        let tmp = sigs_dir.join("signatures.json.tmp");
        fs::write(&tmp, buf.as_bytes())?;
        fs::rename(&tmp, &json_path)?;
        write_stamp(&stamp, &self.fingerprint_hex, self.profile, "sigs")?;
        Ok(WorkdirLease::take(&self.workdir, json_path)?)
    }

    /// Runs the runner with this process's arguments and answers its exit
    /// code, so the caller can give back what it holds before exiting with
    /// it.
    ///
    /// # Errors
    /// Returns the spawn failure when the runner cannot be started.
    pub fn exec(runner: &Path, argv: &[OsString]) -> Result<i32, BindingRunnerError> {
        // A child-process wait keeps the workspace free of `unsafe`
        // `execvp`, with the same exit status for the caller.
        let mut cmd = Command::new(runner);
        cmd.args(&argv[1..]);
        cmd.env("GOSSAMER_IN_RUNNER", "1");
        let status = cmd.status()?;
        Ok(status.code().unwrap_or(127))
    }

    fn render_input(&self, profile: TmplProfile) -> RenderInput<'_> {
        // `BindingEntry` lives in the template crate, but our
        // `RenderedBinding` mirrors it. We have to materialise a
        // matching `Vec<BindingEntry>` and stash it on `self`'s
        // lifetime via a thread-local - but that's fragile. The
        // simpler approach: build the `Vec<BindingEntry>` here and
        // own it via a leaking helper. We side-step that by calling
        // through small adapter helpers on `RenderInput` so we just
        // hand a freshly-built slice.
        RenderInput {
            project_id: &self.project_id,
            fingerprint_hex: &self.fingerprint_hex,
            gossamer_root: &self.gossamer_root,
            bindings: leaked_entries(&self.bindings),
            profile,
            patch_sources: &self.patch_sources,
        }
    }

    fn is_fresh(
        &self,
        artifact: &Path,
        stamp: &Path,
        kind: &str,
    ) -> Result<bool, BindingRunnerError> {
        if !artifact.exists() || !stamp.exists() {
            return Ok(false);
        }
        let Ok(stamp_text) = fs::read_to_string(stamp) else {
            return Ok(false);
        };
        if !stamp_text.contains(&self.fingerprint_hex)
            || !stamp_text.contains(self.profile.dir())
            || !stamp_text.contains(kind)
        {
            return Ok(false);
        }
        let Ok(artifact_meta) = artifact.metadata() else {
            return Ok(false);
        };
        let artifact_mtime = artifact_meta.modified()?;
        let max_dep_mtime = max_path_dep_mtime(&self.bindings, &self.gossamer_root)?;
        if let Some(dep_mtime) = max_dep_mtime
            && dep_mtime > artifact_mtime
        {
            return Ok(false);
        }
        Ok(true)
    }
}

/// Compiled-mode static-link companion to [`BindingRunner`].
#[derive(Debug)]
pub struct StaticBindingsLib {
    /// SHA-256 of the manifest's binding set.
    pub fingerprint: [u8; 32],
    /// 12-char hex prefix of [`Self::fingerprint`].
    pub fingerprint_hex: String,
    /// `<cache>/runners/<fp>/staticlib/`.
    pub workdir: PathBuf,
    /// Bindings to link into the staticlib.
    pub bindings: Vec<RenderedBinding>,
    /// Absolute path to the gossamer source tree.
    pub gossamer_root: PathBuf,
    /// Cargo profile.
    pub profile: Profile,
    /// Cross-compilation target triple passed to cargo (`--target`),
    /// e.g. `x86_64-unknown-linux-musl` for the static-musl release
    /// link. `None` builds for the host.
    pub cargo_target: Option<String>,
    /// Project id for cosmetic comments.
    pub project_id: String,
    /// `[patch]` sources whose `gossamer-*` crates must be redirected
    /// to [`Self::gossamer_root`] so the staticlib links one gossamer-runtime.
    pub patch_sources: Vec<GossamerPatchSource>,
}

impl StaticBindingsLib {
    /// Constructs a staticlib build from the manifest. Returns
    /// `Ok(None)` if `[rust-bindings]` is empty.
    pub fn from_manifest(
        manifest: &Manifest,
        manifest_dir: &Path,
        gossamer_root: &Path,
        profile: Profile,
    ) -> io::Result<Option<Self>> {
        let cache = cache_root()?;
        // Prune after the workdir is resolved, so this run's own artifacts
        // are the one thing the reclamation is told to keep.
        let resolved =
            Self::from_manifest_in(manifest, manifest_dir, gossamer_root, profile, &cache)?;
        prune_runner_cache_once(&cache, resolved.as_ref().map(|r| r.workdir.as_path()));
        Ok(resolved)
    }

    /// Same as [`Self::from_manifest`] but uses an explicit cache
    /// root instead of reading `GOSSAMER_CACHE` / `XDG_CACHE_HOME`.
    pub fn from_manifest_in(
        manifest: &Manifest,
        manifest_dir: &Path,
        gossamer_root: &Path,
        profile: Profile,
        cache_root: &Path,
    ) -> io::Result<Option<Self>> {
        if manifest.rust_bindings.is_empty() {
            return Ok(None);
        }
        let fingerprint = manifest.rust_binding_fingerprint(manifest_dir);
        let fingerprint_hex = hex_prefix(&fingerprint, 6);
        let workdir = cache_root
            .join("runners")
            .join(&fingerprint_hex)
            .join(SUBDIR_STATICLIB);
        fs::create_dir_all(&workdir)?;
        let bindings = render_bindings(&manifest.rust_bindings, manifest_dir);
        let patch_sources = detect_patch_sources(&manifest.rust_bindings, manifest_dir);
        Ok(Some(Self {
            fingerprint,
            fingerprint_hex,
            workdir,
            bindings,
            gossamer_root: gossamer_root.to_path_buf(),
            profile,
            cargo_target: None,
            project_id: manifest.project.id.as_str().to_string(),
            patch_sources,
        }))
    }

    /// Path the staticlib lands at after `ensure_built`.
    ///
    /// Cargo names a `crate-type = ["staticlib"]` artifact
    /// `lib<name>.a` on every platform *except* Windows MSVC, where
    /// it lands as `<name>.lib`. The lib name in the staticlib
    /// `Cargo.toml` is `gos_static_bindings`.
    /// The runner workdir this staticlib lives inside - the unit the cache
    /// reclaims and the build lock covers.
    fn unit_dir(&self) -> &Path {
        self.workdir.parent().unwrap_or(&self.workdir)
    }

    /// Sets the cargo `--target` triple for the staticlib build.
    #[must_use]
    pub fn with_cargo_target(mut self, target: Option<String>) -> Self {
        self.cargo_target = target;
        self
    }

    /// Path the staticlib lands at after `ensure_built`, accounting
    /// for the optional cargo `--target` subdirectory.
    #[must_use]
    pub fn archive_path(&self) -> PathBuf {
        let mut dir = self.workdir.join("target");
        if let Some(t) = &self.cargo_target {
            dir = dir.join(t);
        }
        dir.join(self.profile.dir())
            .join(staticlib_archive_filename())
    }

    /// Idempotently builds the staticlib. Returns the path to the
    /// produced archive (`.a` on Unix / Windows-GNU, `.lib` on
    /// Windows MSVC).
    pub fn ensure_built(&self) -> Result<PathBuf, BindingRunnerError> {
        // The staticlib sits inside the runner workdir, which is reclaimed
        // as one piece, so it takes that workdir's lock rather than one of
        // its own.
        let _lock = AdvisoryLock::acquire(&build_lock_path(self.unit_dir()))?;
        fs::create_dir_all(&self.workdir)?;

        let cargo_toml = self.workdir.join("Cargo.toml");
        let lib_rs = self.workdir.join("lib.rs");
        let input = RenderInput {
            project_id: &self.project_id,
            fingerprint_hex: &self.fingerprint_hex,
            gossamer_root: &self.gossamer_root,
            bindings: leaked_entries(&self.bindings),
            profile: self.profile.template_profile(),
            patch_sources: &self.patch_sources,
        };
        write_if_different(&cargo_toml, &render_staticlib_cargo_toml(&input))?;
        write_if_different(&lib_rs, &render_staticlib_lib_rs(&input))?;
        seed_lockfile(&self.workdir, &self.gossamer_root)?;

        let archive = self.archive_path();
        let stamp = self.workdir.join("stamp.json");
        if self.is_fresh(&archive, &stamp)? {
            return Ok(archive);
        }
        run_cargo_build(
            &cargo_toml,
            &self.workdir.join("target"),
            self.profile,
            self.cargo_target.as_deref(),
            "--lib",
            "",
            "<staticlib>",
        )?;
        let kind = match &self.cargo_target {
            Some(t) => format!("staticlib:{t}"),
            None => "staticlib".to_string(),
        };
        write_stamp(&stamp, &self.fingerprint_hex, self.profile, &kind)?;
        if let Some(root) = self.workdir.parent().and_then(Path::parent) {
            let _ = crate::cache_maintenance::prune_runner_root(
                root,
                crate::cache_maintenance::CachePolicy::default(),
                false,
                Some(&self.workdir),
            );
        }
        Ok(archive)
    }

    fn is_fresh(&self, artifact: &Path, stamp: &Path) -> Result<bool, BindingRunnerError> {
        if !artifact.exists() || !stamp.exists() {
            return Ok(false);
        }
        let Ok(stamp_text) = fs::read_to_string(stamp) else {
            return Ok(false);
        };
        let kind = match &self.cargo_target {
            Some(t) => format!("staticlib:{t}"),
            None => "staticlib".to_string(),
        };
        if !stamp_text.contains(&self.fingerprint_hex)
            || !stamp_text.contains(self.profile.dir())
            || !stamp_text.contains(&kind)
        {
            return Ok(false);
        }
        let Ok(artifact_meta) = artifact.metadata() else {
            return Ok(false);
        };
        let artifact_mtime = artifact_meta.modified()?;
        let max_dep_mtime = max_path_dep_mtime(&self.bindings, &self.gossamer_root)?;
        if let Some(dep_mtime) = max_dep_mtime
            && dep_mtime > artifact_mtime
        {
            return Ok(false);
        }
        Ok(true)
    }
}

fn hex_prefix(bytes: &[u8], n: usize) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(n * 2);
    for b in bytes.iter().take(n) {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// JSON model of `signatures.json` produced by the sigs-dump bin.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SignatureDump {
    /// All modules registered via `register_module!`.
    pub modules: Vec<DumpedModule>,
}

/// One module entry in the sigs-dump JSON.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DumpedModule {
    /// `module::path` declared by the binding.
    pub path: String,
    /// Module-level doc string (may be empty).
    pub doc: String,
    /// Items in declaration order.
    pub items: Vec<DumpedItem>,
}

/// One item entry in the sigs-dump JSON.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DumpedItem {
    /// Item name.
    pub name: String,
    /// Item-level doc string.
    pub doc: String,
    /// Parameter types.
    pub params: Vec<DumpedType>,
    /// Return type.
    pub ret: DumpedType,
}

/// Type description recorded in the sigs-dump JSON.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind")]
pub enum DumpedType {
    /// `()`.
    #[serde(rename = "unit")]
    Unit,
    /// `bool`.
    #[serde(rename = "bool")]
    Bool,
    /// `i64`.
    #[serde(rename = "i64")]
    I64,
    /// `f64`.
    #[serde(rename = "f64")]
    F64,
    /// `char`.
    #[serde(rename = "char")]
    Char,
    /// `String` / `&str`.
    #[serde(rename = "string")]
    String,
    /// `Bytes` (ABI 0.4+).
    #[serde(rename = "bytes")]
    Bytes,
    /// `(T1, T2, ...)`.
    #[serde(rename = "tuple")]
    Tuple {
        /// Element types.
        items: Vec<DumpedType>,
    },
    /// `Vec<T>`.
    #[serde(rename = "vec")]
    Vec {
        /// Element type.
        of: Box<DumpedType>,
    },
    /// `Option<T>`.
    #[serde(rename = "option")]
    Option {
        /// Inner type.
        of: Box<DumpedType>,
    },
    /// `Result<T, E>`.
    #[serde(rename = "result")]
    Result {
        /// `Ok` payload type.
        ok: Box<DumpedType>,
        /// `Err` payload type.
        err: Box<DumpedType>,
    },
    /// `Map<K, V>` (ABI 0.4+).
    #[serde(rename = "map")]
    Map {
        /// Key type.
        key: Box<DumpedType>,
        /// Value type.
        value: Box<DumpedType>,
    },
    /// Tagged-union return (ABI 0.4+).
    #[serde(rename = "variant")]
    Variant {
        /// Variant arms.
        arms: Vec<DumpedVariantArm>,
    },
    /// `Fn(args...) -> ret` callback (ABI 0.4+).
    #[serde(rename = "callback")]
    Callback {
        /// Positional argument types.
        args: Vec<DumpedType>,
        /// Return type.
        ret: Box<DumpedType>,
    },
    /// Opaque handle.
    #[serde(rename = "opaque")]
    Opaque {
        /// Opaque type name.
        name: String,
    },
    /// Untyped (`Value::Native` passthrough).
    #[serde(rename = "any")]
    Any,
}

/// One arm in a [`DumpedType::Variant`].
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DumpedVariantArm {
    /// Arm name.
    pub name: String,
    /// Positional payload types.
    pub payload: Vec<DumpedType>,
}

/// Parses the sigs-dump JSON.
pub fn parse_signature_dump(text: &str) -> Result<SignatureDump, BindingRunnerError> {
    serde_json::from_str(text).map_err(|e| BindingRunnerError::BadSignatureJson(e.to_string()))
}

/// Gossamer crates whose source a binding must share with the
/// toolchain (gossamer-runtime owns the process `#[global_allocator]`,
/// so a second copy is a link error).
const GOSSAMER_CRATES: [&str; 3] = ["gossamer-runtime", "gossamer-std", "gossamer-binding"];

/// Determines the `[patch]` sources needed so every binding's
/// `gossamer-*` crates resolve to the toolchain checkout rather than a
/// second copy from crates.io / git.
///
/// For path / src bindings the binding crate is on disk, so its
/// `Cargo.toml` is read and each gossamer dep's source is detected
/// precisely (crates.io vs a git URL). Crates.io / git bindings aren't
/// materialised at generation time; per the version contract (bindings
/// declare a crates.io `gossamer-* = "<req>"` requirement - any req the
/// toolchain version satisfies, e.g. `=X.Y.Z` or `>=X.Y.Z`) they resolve
/// gossamer-* from crates.io, so `[patch.crates-io]` is emitted. The
/// `[patch]` supplies the toolchain checkout, so the requirement need
/// only be satisfiable by `gos --version`, not exact.
/// Sources are de-duplicated by their patch-table key.
fn detect_patch_sources(
    rust_bindings: &BTreeMap<String, RustBindingSpec>,
    manifest_dir: &Path,
) -> Vec<GossamerPatchSource> {
    let mut sources: Vec<GossamerPatchSource> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for spec in rust_bindings.values() {
        for src in gossamer_sources_for_binding(spec, manifest_dir) {
            if seen.insert(src.table_key()) {
                sources.push(src);
            }
        }
    }
    sources
}

fn gossamer_sources_for_binding(
    spec: &RustBindingSpec,
    manifest_dir: &Path,
) -> Vec<GossamerPatchSource> {
    match spec {
        RustBindingSpec::Path { path, .. } => {
            let abs = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                manifest_dir.join(path)
            };
            gossamer_sources_from_manifest_path(&abs.join("Cargo.toml"))
        }
        RustBindingSpec::Src { deps, .. } => {
            gossamer_sources_from_manifest_text(&format!("[dependencies]\n{deps}\n"))
        }
        // A crates.io binding cannot carry path / git deps (crates.io
        // forbids them), so it pulls gossamer-* from crates.io. A git
        // binding most commonly does the same; the version contract
        // makes crates-io the correct patch source either way.
        RustBindingSpec::Crates { .. } | RustBindingSpec::Git { .. } => {
            vec![GossamerPatchSource::CratesIo]
        }
        // Prebuilt archives carry no Cargo dep graph - nothing to patch.
        RustBindingSpec::Prebuilt { .. } => Vec::new(),
    }
}

fn gossamer_sources_from_manifest_path(cargo_toml: &Path) -> Vec<GossamerPatchSource> {
    match fs::read_to_string(cargo_toml) {
        Ok(text) => gossamer_sources_from_manifest_text(&text),
        Err(_) => Vec::new(),
    }
}

/// Parses a Cargo manifest and returns the distinct non-path sources
/// its `gossamer-*` dependencies resolve through. Path-sourced
/// gossamer deps are skipped: cargo already unifies them by canonical
/// path, and `[patch]` cannot rewrite a path dependency.
fn gossamer_sources_from_manifest_text(text: &str) -> Vec<GossamerPatchSource> {
    let Ok(doc) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    let mut out: Vec<GossamerPatchSource> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut scan = |table: Option<&toml::Value>| {
        let Some(table) = table.and_then(toml::Value::as_table) else {
            return;
        };
        for crate_name in GOSSAMER_CRATES {
            if let Some(src) = table.get(crate_name).and_then(source_of_dep)
                && seen.insert(src.table_key())
            {
                out.push(src);
            }
        }
    };
    scan(doc.get("dependencies"));
    scan(doc.get("build-dependencies"));
    if let Some(target) = doc.get("target").and_then(toml::Value::as_table) {
        for cfg in target.values() {
            scan(cfg.get("dependencies"));
        }
    }
    out
}

fn source_of_dep(val: &toml::Value) -> Option<GossamerPatchSource> {
    match val {
        // `gossamer-runtime = "=0.16.0"` - crates.io.
        toml::Value::String(_) => Some(GossamerPatchSource::CratesIo),
        toml::Value::Table(t) => {
            if let Some(git) = t.get("git").and_then(toml::Value::as_str) {
                Some(GossamerPatchSource::Git(git.to_string()))
            } else if t.contains_key("path") {
                None
            } else if t.contains_key("version") {
                Some(GossamerPatchSource::CratesIo)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn render_bindings(
    rust_bindings: &BTreeMap<String, RustBindingSpec>,
    manifest_dir: &Path,
) -> Vec<RenderedBinding> {
    rust_bindings
        .iter()
        .map(|(name, spec)| {
            let (cargo_dep_line, features, local_root) = render_one(name, spec, manifest_dir);
            RenderedBinding {
                crate_name: name.clone(),
                cargo_dep_line,
                features,
                local_root,
            }
        })
        .collect()
}

fn render_one(
    name: &str,
    spec: &RustBindingSpec,
    manifest_dir: &Path,
) -> (String, Vec<String>, Option<PathBuf>) {
    match spec {
        RustBindingSpec::Path {
            version,
            path,
            features,
            default_features,
        } => {
            let abs = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                manifest_dir.join(path)
            };
            let mut parts: Vec<String> = Vec::new();
            if let Some(v) = version {
                parts.push(format!("version = \"{}\"", cargo_version_requirement(v)));
            }
            parts.push(toml_path_kv("path", &abs));
            push_cargo_features(&mut parts, features, *default_features);
            (
                format!("{name} = {{ {} }}", parts.join(", ")),
                features.clone(),
                Some(abs),
            )
        }
        RustBindingSpec::Git {
            version,
            url,
            reference,
            features,
            default_features,
        } => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(v) = version {
                parts.push(format!("version = \"{}\"", cargo_version_requirement(v)));
            }
            parts.push(format!("git = \"{url}\""));
            if let Some(r) = reference {
                match r {
                    GitRef::Branch(b) => parts.push(format!("branch = \"{b}\"")),
                    GitRef::Tag(t) => parts.push(format!("tag = \"{t}\"")),
                    GitRef::Rev(r) => parts.push(format!("rev = \"{r}\"")),
                }
            }
            push_cargo_features(&mut parts, features, *default_features);
            (
                format!("{name} = {{ {} }}", parts.join(", ")),
                features.clone(),
                None,
            )
        }
        RustBindingSpec::Crates {
            version,
            features,
            default_features,
        } => {
            let mut parts: Vec<String> = Vec::new();
            parts.push(format!(
                "version = \"{}\"",
                cargo_version_requirement(version)
            ));
            push_cargo_features(&mut parts, features, *default_features);
            (
                format!("{name} = {{ {} }}", parts.join(", ")),
                features.clone(),
                None,
            )
        }
        RustBindingSpec::Src { src, deps } => {
            let abs_src = if Path::new(src).is_absolute() {
                PathBuf::from(src)
            } else {
                manifest_dir.join(src)
            };
            let wrapper_dir = manifest_dir
                .join(".gos-bindings")
                .join(format!("__srcwrap-{name}"));
            let _ = materialise_src_binding(name, &wrapper_dir, &abs_src, deps);
            (
                format!("{name} = {{ {} }}", toml_path_kv("path", &wrapper_dir)),
                Vec::new(),
                Some(wrapper_dir),
            )
        }
        RustBindingSpec::Prebuilt { archive, abi: _ } => {
            // Prebuilt-archive binding: the staticlib is supplied
            // directly. There is no Cargo dep - the link step in
            // `gos build` consumes the archive path. The emitted
            // line is a TOML-friendly comment; the manifest-side
            // record keeps the archive path reachable through the
            // resolved-binding metadata.
            let abs = if Path::new(archive).is_absolute() {
                PathBuf::from(archive)
            } else {
                manifest_dir.join(archive)
            };
            (
                format!("# prebuilt: {name} archive = '{}'", abs.display()),
                Vec::new(),
                None,
            )
        }
    }
}

/// Scaffold a wrapper crate around a single-file binding source.
/// Idempotent - re-rendering is byte-stable, and `write_if_different`
/// skips the rewrite when the contents match.
fn materialise_src_binding(
    name: &str,
    wrapper_dir: &Path,
    src_file: &Path,
    deps: &str,
) -> io::Result<()> {
    fs::create_dir_all(wrapper_dir.join("src"))?;
    let gossamer_root = std::env::var_os("GOSSAMER_ROOT")
        .map(PathBuf::from)
        .or_else(|| {
            wrapper_dir
                .ancestors()
                .find(|p| p.join("crates").join("gossamer-binding").is_dir())
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(|| PathBuf::from("."));
    let cargo_toml = format!(
        "[package]\nname = \"gos-srcwrap-{name}\"\nversion = \"0.0.1\"\nedition = \"2024\"\npublish = false\n\n[workspace]\n\n[lib]\ncrate-type = [\"rlib\"]\n\n[dependencies]\n{deps}\ngossamer-binding = {{ {} }}\n",
        toml_path_kv("path", &gossamer_root.join("crates/gossamer-binding")),
    );
    let lib_rs = format!(
        "//! Generated wrapper around `{}` for the `{name}` binding.\n\n#[path = {:?}]\nmod __user;\n\npub use __user::*;\n\n/// Linker-hook anchoring `linkme` registry entries across LTO.\npub fn __bindings_force_link() {{\n    let _ = ::gossamer_binding::modules();\n}}\n",
        src_file.display(),
        src_file.display(),
    );
    let cargo_path = wrapper_dir.join("Cargo.toml");
    let lib_path = wrapper_dir.join("src").join("lib.rs");
    let _ = write_if_different(&cargo_path, &cargo_toml);
    let _ = write_if_different(&lib_path, &lib_rs);
    Ok(())
}

/// Renders a `key = '...'` TOML pair using a single-quoted literal
/// string so backslashes (Windows `D:\a\...`), quotes, and other
/// escape-prone bytes round-trip unchanged. TOML literal strings
/// disallow `'` and ASCII control chars; if the path contains
/// either we fall back to a basic string with `\\` doubling, which
/// covers every realistic filesystem path on the platforms we
/// support without sacrificing correctness.
/// Renders a `key = <path>` TOML pair that a strict parser accepts on every
/// platform: a literal string keeps a Windows separator verbatim, and a path
/// containing an apostrophe falls back to an escaped basic string.
pub fn toml_path_kv(key: &str, path: &Path) -> String {
    let display = path.display().to_string();
    if !display.contains('\'') && !display.chars().any(char::is_control) {
        format!("{key} = '{display}'")
    } else {
        let escaped = display.replace('\\', "\\\\").replace('"', "\\\"");
        format!("{key} = \"{escaped}\"")
    }
}

/// Renders a Gossamer version requirement in Cargo's grammar.
///
/// The two grammars disagree about the default: a bare literal pins in
/// `project.toml` and means a caret range in `Cargo.toml`, so writing
/// one through unchanged would silently widen a pin. Each bound is
/// spelled explicitly instead.
fn cargo_version_requirement(requirement: &gossamer_pkg::VersionReq) -> String {
    match requirement.bound {
        gossamer_pkg::VersionBound::Exact => format!("={}", requirement.version),
        gossamer_pkg::VersionBound::AtLeast => format!(">={}", requirement.version),
    }
}

fn push_cargo_features(parts: &mut Vec<String>, features: &[String], default_features: bool) {
    if !features.is_empty() {
        let listed: Vec<String> = features.iter().map(|f| format!("\"{f}\"")).collect();
        parts.push(format!("features = [{}]", listed.join(", ")));
    }
    if !default_features {
        parts.push("default-features = false".to_string());
    }
}

fn cache_root() -> io::Result<PathBuf> {
    if let Some(s) = std::env::var_os("GOSSAMER_CACHE") {
        return Ok(PathBuf::from(s).join("gossamer"));
    }
    if let Some(s) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(s).join("gossamer"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".cache").join("gossamer"));
    }
    // Windows fallback: %LOCALAPPDATA% is the per-user cache root,
    // %USERPROFILE%\AppData\Local is its long form.
    if let Some(s) = std::env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(s).join("gossamer"));
    }
    if let Some(s) = std::env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(s)
            .join("AppData")
            .join("Local")
            .join("gossamer"));
    }
    Err(io::Error::other(
        "cannot determine cache directory: set GOSSAMER_CACHE, XDG_CACHE_HOME, HOME, LOCALAPPDATA, or USERPROFILE",
    ))
}

fn prune_runner_cache_once(cache_root: &Path, in_use: Option<&Path>) {
    use std::sync::OnceLock;
    static PRUNED: OnceLock<()> = OnceLock::new();
    PRUNED.get_or_init(|| {
        let _ = crate::cache_maintenance::prune_runner_root(
            &cache_root.join("runners"),
            crate::cache_maintenance::CachePolicy::default(),
            false,
            in_use,
        );
    });
}

/// The lock guarding one runner workdir.
///
/// The runner project, the staticlib, and the signatures beside them are
/// reclaimed together, so one lock at the workdir root covers all three.
pub(crate) fn build_lock_path(workdir: &Path) -> PathBuf {
    workdir.join(BUILD_LOCK)
}

/// Seeds a generated project's lockfile from the toolchain's own resolved
/// dependency graph.
///
/// Cargo runs a dependency's build script as part of compiling it, so the
/// version chosen for every transitive crate decides what code executes. A
/// generated project carrying no lockfile resolves the whole graph afresh
/// against the registry and takes the newest semver-compatible release of
/// each crate, which makes any newly published version of any dependency
/// run here - regardless of what the toolchain itself pins.
///
/// Seeding makes the toolchain's audited graph the resolver's starting
/// point. Cargo's resolution is minimal-change, so a package the seed
/// already pins keeps that version and only what the bindings add on top is
/// resolved. Bindings still reach their own dependencies; those are the
/// user's tree to pin, and a binding that names none adds nothing to
/// resolve.
fn seed_lockfile(project_dir: &Path, gossamer_root: &Path) -> io::Result<()> {
    // A toolchain installed without its source tree has no lockfile to seed
    // from. Its generated projects also have no path dependencies to reach,
    // so a binding build there fails on the manifest well before resolution.
    let Ok(seed) = fs::read(gossamer_root.join(CARGO_LOCK)) else {
        return Ok(());
    };
    let lock = project_dir.join(CARGO_LOCK);
    let stamp = project_dir.join(LOCK_SEED_STAMP);
    if lock.exists() && fs::read(&stamp).is_ok_and(|previous| previous == seed) {
        return Ok(());
    }
    fs::write(&lock, &seed)?;
    fs::write(&stamp, &seed)?;
    Ok(())
}

fn write_if_different(path: &Path, contents: &str) -> io::Result<()> {
    if let Ok(existing) = fs::read_to_string(path)
        && existing == contents
    {
        return Ok(());
    }
    let tmp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("dat")
    ));
    fs::write(&tmp_path, contents.as_bytes())?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn write_stamp(path: &Path, fingerprint_hex: &str, profile: Profile, kind: &str) -> io::Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let body = format!(
        "{{\"fingerprint\":\"{fingerprint_hex}\",\"built_at\":{now},\"profile\":\"{}\",\"kind\":\"{kind}\"}}",
        profile.dir()
    );
    write_if_different(path, &body)
}

fn run_cargo_build(
    manifest_path: &Path,
    target_dir: &Path,
    profile: Profile,
    cargo_target: Option<&str>,
    kind_flag: &str,
    kind_value: &str,
    crate_label: &str,
) -> Result<(), BindingRunnerError> {
    let cargo = which::which("cargo").map_err(|_| BindingRunnerError::CargoMissing)?;
    let mut cmd = Command::new(cargo);
    cmd.arg("build");
    if matches!(profile, Profile::Release) {
        cmd.arg("--release");
    }
    if let Some(t) = cargo_target {
        cmd.arg("--target").arg(t);
    }
    configure_macos_cargo_build(&mut cmd, cargo_target);
    cmd.arg("--manifest-path").arg(manifest_path);
    if kind_value.is_empty() {
        cmd.arg(kind_flag);
    } else {
        cmd.arg(kind_flag).arg(kind_value);
    }
    cmd.env("CARGO_TARGET_DIR", target_dir);
    // Capture both streams so cargo's progress and diagnostics are shown
    // only when the build fails, keeping the user's terminal clean on
    // success while still surfacing errors.
    cmd.stderr(Stdio::piped());
    cmd.stdout(Stdio::piped());
    let mut child = cmd.spawn().map_err(BindingRunnerError::Io)?;
    // Each pipe is drained by its own reader started before `wait`, so a
    // pipe that fills to its OS buffer limit cannot back-pressure cargo
    // into blocking on `write` while we block on `wait`. cargo routinely
    // emits more than a pipe buffer's worth of warnings on stderr.
    let stdout_reader = spawn_stream_reader(child.stdout.take());
    let stderr_reader = spawn_stream_reader(child.stderr.take());
    let status = child.wait()?;
    let stdout_text = stdout_reader.join().unwrap_or_default();
    let stderr_text = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        // Forward both streams so the user sees what went wrong.
        let _ = writeln!(io::stderr(), "{stdout_text}");
        let _ = writeln!(io::stderr(), "{stderr_text}");
        return Err(BindingRunnerError::CargoFailed {
            crate_name: crate_label.to_string(),
            stderr: stderr_text,
        });
    }
    Ok(())
}

fn configure_macos_cargo_build(command: &mut Command, cargo_target: Option<&str>) {
    if crate::macos_deployment::is_macos_target(cargo_target, cfg!(target_os = "macos")) {
        let deployment_target = crate::macos_deployment::effective_deployment_target();
        crate::macos_deployment::set_command_deployment_target(command, &deployment_target);
    }
}

/// Reads `stream` to EOF on its own thread, returning the join handle so
/// callers can start a reader for every child pipe before waiting on the
/// child. Concurrent readers keep any one pipe from filling and blocking
/// the child mid-write.
fn spawn_stream_reader<R: Read + Send + 'static>(
    stream: Option<R>,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut s) = stream {
            let _ = s.read_to_string(&mut buf);
        }
        buf
    })
}

fn max_path_dep_mtime(
    bindings: &[RenderedBinding],
    gossamer_root: &Path,
) -> io::Result<Option<SystemTime>> {
    let mut best: Option<SystemTime> = None;
    for b in bindings {
        let Some(root) = &b.local_root else {
            continue;
        };
        if !root.exists() {
            continue;
        }
        walk_max_mtime(root, &mut best)?;
    }
    // Track the gossamer source tree alongside the binding crates:
    // changes to gossamer-binding / gossamer-codegen-cranelift /
    // the runtime affect what the runner needs to do at startup
    // (e.g. registering binding C-ABI thunks with the JIT). Without
    // this, an upgrade of `gos` reuses a stale runner compiled
    // against the previous gossamer-binding ABI.
    let crates_dir = gossamer_root.join("crates");
    if crates_dir.exists() {
        walk_max_mtime(&crates_dir, &mut best)?;
    }
    Ok(best)
}

fn walk_max_mtime(path: &Path, best: &mut Option<SystemTime>) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_dir() {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if name == "target" || name == ".git" || name.starts_with('.') {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            walk_max_mtime(&entry.path(), best)?;
        }
        return Ok(());
    }
    if let Ok(mtime) = meta.modified()
        && best.is_none_or(|b| mtime > b)
    {
        *best = Some(mtime);
    }
    Ok(())
}

fn leaked_entries(rendered: &[RenderedBinding]) -> &'static [BindingEntry] {
    // We pre-construct a slice of `BindingEntry` matching the
    // `RenderedBinding` layout. The template renderer only reads
    // it; no leak required since it lives on the heap inside a
    // `Box::leak` keyed by the rendered set's identity.
    use std::sync::OnceLock;
    static TABLE: OnceLock<Mutex<std::collections::HashMap<usize, &'static [BindingEntry]>>> =
        OnceLock::new();
    let table = TABLE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let key = rendered.as_ptr() as usize;
    let mut guard = table.lock();
    if let Some(v) = guard.get(&key) {
        return v;
    }
    let entries: Vec<BindingEntry> = rendered
        .iter()
        .map(|r| BindingEntry {
            crate_name: r.crate_name.clone(),
            cargo_dep_line: r.cargo_dep_line.clone(),
            features: r.features.clone(),
        })
        .collect();
    let leaked: &'static [BindingEntry] = Box::leak(entries.into_boxed_slice());
    guard.insert(key, leaked);
    leaked
}

/// Cross-process advisory lock.
///
/// We avoid pulling in `fs2` / `fd-lock` for one-shot use: a
/// best-effort exclusive create-on-open is sufficient for the
/// intended "two `gos` processes started seconds apart" case, and
/// it doesn't require the workspace to permit unsafe.
pub(crate) struct AdvisoryLock {
    path: PathBuf,
}

impl AdvisoryLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_mins(5);
        loop {
            match Self::take(path) {
                Ok(Some(lock)) => return Ok(lock),
                Ok(None) => {
                    if std::time::Instant::now() > deadline {
                        return Err(io::Error::other(format!(
                            "another `gos` process holds {} for >5 min",
                            path.display()
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Takes the lock if it is free, and answers `None` if another process
    /// holds it. Reclamation uses this so a workdir is only ever removed
    /// while nothing is building in it.
    pub(crate) fn try_acquire(path: &Path) -> Option<Self> {
        Self::take(path).ok().flatten()
    }

    /// Path of the lock file this guard owns.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// One attempt: `Ok(None)` means a live process holds the lock.
    fn take(path: &Path) -> io::Result<Option<Self>> {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                Ok(Some(Self {
                    path: path.to_path_buf(),
                }))
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                // Best-effort: stale lock detection - if the PID
                // in the file no longer exists, take it.
                if let Ok(text) = fs::read_to_string(path)
                    && let Ok(pid) = text.trim().parse::<u32>()
                    && !pid_alive(pid)
                {
                    let _ = fs::remove_file(path);
                    return Self::take(path);
                }
                Ok(None)
            }
            // The workdir is reclaimed as a whole, so the directory holding
            // the lock can be gone between one build and the next.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let Some(parent) = path.parent() else {
                    return Err(err);
                };
                fs::create_dir_all(parent)?;
                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                {
                    Ok(mut f) => {
                        let _ = writeln!(f, "{}", std::process::id());
                        Ok(Some(Self {
                            path: path.to_path_buf(),
                        }))
                    }
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(None),
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(err),
        }
    }
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Prefix of the marker a process leaves in a runner workdir while it uses
/// what the workdir holds.
const LEASE_PREFIX: &str = ".gos-lease-";

/// A process's claim on a built artifact in a runner workdir.
///
/// A `gos` that re-executes into the runner, or reads its signatures, uses
/// the workdir after the build lock is released; reclamation removes a
/// workdir only while no live process holds a lease on it. The lease is
/// taken under the build lock, so a reclaim either sees it or has already
/// removed what the lease would have named.
#[derive(Debug)]
pub struct WorkdirLease {
    artifact: PathBuf,
    marker: PathBuf,
}

impl WorkdirLease {
    /// Leases `workdir` for `artifact`. The caller holds the workdir's build
    /// lock.
    fn take(workdir: &Path, artifact: PathBuf) -> io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let marker = workdir.join(format!(
            "{LEASE_PREFIX}{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&marker, b"")?;
        Ok(Self { artifact, marker })
    }

    /// The artifact the lease keeps in place.
    #[must_use]
    pub fn artifact(&self) -> &Path {
        &self.artifact
    }
}

impl Drop for WorkdirLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.marker);
    }
}

/// Whether a live process holds a lease on `workdir`. A lease whose process
/// has exited is removed as it is found.
pub(crate) fn has_live_lease(workdir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(workdir) else {
        return false;
    };
    let mut live = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(LEASE_PREFIX)) else {
            continue;
        };
        let pid = rest.split('-').next().and_then(|p| p.parse::<u32>().ok());
        if pid.is_some_and(pid_alive) {
            live = true;
        } else {
            let _ = fs::remove_file(entry.path());
        }
    }
    live
}

// The liveness probe is an unavoidable FFI call (`libc::kill` on unix,
// `OpenProcess`/`GetExitCodeProcess` on Windows); there is no safe std API
// for "is this pid alive". Unsafe is contained to this single function.
#[allow(unsafe_code)]
fn pid_alive(pid: u32) -> bool {
    // 0 names the caller's own process group on unix and the idle process on
    // Windows; neither is a lock holder, and both would read as alive.
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // `kill` reads a negative argument as a process group or a broadcast,
        // which answers "alive" for a value that never named a process, so
        // only one that fits a pid reaches it.
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // POSIX signal 0 error-checks without delivering a signal. 0 (alive)
        // or EPERM (alive but owned by another user) => alive; ESRCH => dead.
        // Portable across Linux and macOS, unlike a /proc check.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, STILL_ACTIVE,
        };
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // Nonexistent pid => dead; any other open failure (e.g. access denied)
        // => conservatively alive so a live process's lock is never stolen.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return GetLastError() != ERROR_INVALID_PARAMETER;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(h, &raw mut code);
            CloseHandle(h);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gossamer_pkg::Manifest;

    fn write_manifest(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("project.toml");
        fs::write(&path, body).unwrap();
        path
    }

    // A child that fills its stderr pipe before writing stdout must be
    // drained by concurrent readers; draining the pipes one after the
    // other lets the unread pipe fill and wedges the child mid-write,
    // matching the deadlock `run_cargo_build` avoids for cargo's stderr.
    #[cfg(unix)]
    #[test]
    fn stream_readers_drain_concurrently_without_deadlock() {
        const STDERR_BYTES: usize = 256 * 1024;
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("yes X | head -c {STDERR_BYTES} 1>&2; printf DONE"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let stdout_reader = spawn_stream_reader(child.stdout.take());
        let stderr_reader = spawn_stream_reader(child.stderr.take());
        let status = child.wait().expect("wait");
        let stdout_text = stdout_reader.join().unwrap();
        let stderr_text = stderr_reader.join().unwrap();
        assert!(status.success());
        assert_eq!(stdout_text, "DONE");
        assert_eq!(stderr_text.len(), STDERR_BYTES);
    }

    #[test]
    fn rust_binding_cargo_build_inherits_macos_deployment_target() {
        let mut command = Command::new("cargo");
        configure_macos_cargo_build(&mut command, Some("aarch64-apple-darwin"));
        let configured = command
            .get_envs()
            .find(|(name, _)| *name == crate::macos_deployment::MACOSX_DEPLOYMENT_TARGET_ENV)
            .and_then(|(_, value)| value)
            .expect("binding Cargo deployment target environment");
        assert_eq!(
            configured,
            crate::macos_deployment::effective_deployment_target().as_str()
        );
    }

    #[test]
    fn from_manifest_returns_none_on_empty_section() {
        let src = "[project]\nid = \"example.com/p\"\nversion = \"0.1.0\"\n";
        let m = Manifest::parse(src).unwrap();
        let out = BindingRunner::from_manifest(
            &m,
            std::env::temp_dir().as_path(),
            std::env::temp_dir().as_path(),
            Profile::Debug,
        )
        .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn from_manifest_in_yields_runner_for_path_binding() {
        let cache = tempdir();
        let manifest_dir = tempdir();
        let echo_dir = manifest_dir.join("echo");
        fs::create_dir_all(&echo_dir).unwrap();
        let body = "[project]\nid = \"example.com/p\"\nversion = \"0.1.0\"\n\n[rust-bindings]\necho = { path = \"./echo\" }\n".to_string();
        write_manifest(&manifest_dir, &body);
        let m = Manifest::parse(&body).unwrap();
        let runner = BindingRunner::from_manifest_in(
            &m,
            &manifest_dir,
            Path::new("/fake"),
            Profile::Debug,
            &cache,
        )
        .unwrap()
        .expect("runner");
        assert_eq!(runner.bindings.len(), 1);
        assert_eq!(runner.bindings[0].crate_name, "echo");
        assert!(
            runner.bindings[0]
                .local_root
                .as_ref()
                .unwrap()
                .ends_with("echo")
        );
        assert!(runner.workdir.starts_with(&cache));
    }

    /// Writes a minimal binding crate at `dir/<name>` whose Cargo.toml
    /// declares the supplied `gossamer-*` dependency lines, returning
    /// the crate directory.
    fn write_binding_crate(dir: &Path, name: &str, gossamer_deps: &str) -> PathBuf {
        let crate_dir = dir.join(name);
        fs::create_dir_all(crate_dir.join("src")).unwrap();
        let cargo = format!(
            "[package]\nname = \"{name}\"\nversion = \"0.0.1\"\nedition = \"2024\"\n\n[dependencies]\n{gossamer_deps}\n"
        );
        fs::write(crate_dir.join("Cargo.toml"), cargo).unwrap();
        fs::write(crate_dir.join("src").join("lib.rs"), "").unwrap();
        crate_dir
    }

    fn path_binding_manifest(name: &str) -> Manifest {
        let body = format!(
            "[project]\nid = \"example.com/p\"\nversion = \"0.1.0\"\n\n[rust-bindings]\n{name} = {{ path = \"./{name}\" }}\n"
        );
        Manifest::parse(&body).unwrap()
    }

    #[test]
    fn detect_patch_sources_reads_crates_io_gossamer_dep() {
        let dir = tempdir();
        write_binding_crate(&dir, "sqlite", "gossamer-runtime = \"=0.16.0\"");
        let m = path_binding_manifest("sqlite");
        let sources = detect_patch_sources(&m.rust_bindings, &dir);
        assert_eq!(sources, vec![GossamerPatchSource::CratesIo]);
    }

    #[test]
    fn detect_patch_sources_reads_git_gossamer_dep() {
        let dir = tempdir();
        write_binding_crate(
            &dir,
            "sqlite",
            "gossamer-runtime = { git = \"https://github.com/dpup/gossamer\", tag = \"v0.16.0\" }",
        );
        let m = path_binding_manifest("sqlite");
        let sources = detect_patch_sources(&m.rust_bindings, &dir);
        assert_eq!(
            sources,
            vec![GossamerPatchSource::Git(
                "https://github.com/dpup/gossamer".to_string()
            )]
        );
    }

    #[test]
    fn detect_patch_sources_skips_path_gossamer_dep() {
        let dir = tempdir();
        write_binding_crate(
            &dir,
            "sqlite",
            "gossamer-runtime = { path = \"../../gossamer/crates/gossamer-runtime\" }",
        );
        let m = path_binding_manifest("sqlite");
        let sources = detect_patch_sources(&m.rust_bindings, &dir);
        assert!(sources.is_empty(), "path deps need no patch: {sources:?}");
    }

    #[test]
    fn detect_patch_sources_defaults_crates_io_for_non_path_binding() {
        let body = "[project]\nid = \"example.com/p\"\nversion = \"0.1.0\"\n\n[rust-bindings]\nsqlite = { version = \"0.16.0\" }\n";
        let m = Manifest::parse(body).unwrap();
        let sources = detect_patch_sources(&m.rust_bindings, &std::env::temp_dir());
        assert_eq!(sources, vec![GossamerPatchSource::CratesIo]);
    }

    #[test]
    fn detect_patch_sources_defaults_crates_io_for_git_binding() {
        let body = "[project]\nid = \"example.com/p\"\nversion = \"0.1.0\"\n\n[rust-bindings]\nsqlite = { git = \"https://example.com/sqlite-binding\" }\n";
        let m = Manifest::parse(body).unwrap();
        let sources = detect_patch_sources(&m.rust_bindings, &std::env::temp_dir());
        assert_eq!(sources, vec![GossamerPatchSource::CratesIo]);
    }

    #[test]
    fn gossamer_dep_with_range_requirement_is_crates_io() {
        // A binding may use any crates.io version requirement the toolchain
        // version satisfies - `>=X.Y.Z` is preferred over an exact pin so it
        // survives `gos` upgrades. It still resolves through `[patch.crates-io]`.
        let sources = gossamer_sources_from_manifest_text(
            "[dependencies]\ngossamer-runtime = \">=0.16.0\"\n",
        );
        assert_eq!(sources, vec![GossamerPatchSource::CratesIo]);
        assert_eq!(
            source_of_dep(&toml::Value::String(">=0.16.0".to_string())),
            Some(GossamerPatchSource::CratesIo)
        );
    }

    #[test]
    fn runner_manifest_contains_patch_block_for_crates_io_binding() {
        let cache = tempdir();
        let manifest_dir = tempdir();
        write_binding_crate(&manifest_dir, "sqlite", "gossamer-runtime = \"=0.16.0\"");
        let m = path_binding_manifest("sqlite");
        let root = tempdir();
        let runner =
            BindingRunner::from_manifest_in(&m, &manifest_dir, &root, Profile::Debug, &cache)
                .unwrap()
                .expect("runner");
        let rendered = render_cargo_toml(&runner.render_input(TmplProfile::Debug));
        assert!(
            rendered.contains("[patch.crates-io]"),
            "runner manifest missing patch table:\n{rendered}"
        );
        let runtime_line = format!(
            "gossamer-runtime = {{ path = '{}/crates/gossamer-runtime' }}",
            root.display()
        );
        assert!(
            rendered.contains(&runtime_line),
            "runner manifest missing runtime redirect:\n{rendered}"
        );
        let _: toml::Value = toml::from_str(&rendered).expect("rendered runner Cargo.toml parses");
    }

    #[test]
    fn toml_path_kv_uses_literal_string_for_backslash_paths() {
        // Mimics a Windows GitHub runner path. TOML basic strings
        // would interpret `\a`/`\g` as escape sequences and fail
        // to parse - single-quoted literal strings preserve the
        // bytes verbatim. This is the regression gate for the
        // Windows CI failure observed 2026-04-30.
        let p = PathBuf::from("D:\\a\\gossamer\\gossamer/crates/gossamer-binding");
        let kv = toml_path_kv("path", &p);
        assert!(
            kv.starts_with("path = '"),
            "expected literal string, got: {kv}"
        );
        // The whole expression must round-trip through cargo's
        // strict TOML parser inside a `{ ... }` inline table.
        let snippet = format!("[deps]\nfoo = {{ {kv} }}\n");
        let _: toml::Value = toml::from_str(&snippet)
            .expect("toml_path_kv output round-trips through strict TOML parser");
    }

    #[test]
    fn toml_path_kv_falls_back_to_basic_string_when_path_has_apostrophe() {
        // Single-quoted TOML literal strings disallow `'`; the
        // helper must fall back to a basic string with `\\` doubling.
        // PathBuf accepts the literal regardless of platform; the
        // test exercises the quoter, not the filesystem.
        let p = PathBuf::from("tmp/it's a path/echo");
        let kv = toml_path_kv("path", &p);
        assert!(
            kv.starts_with("path = \""),
            "expected basic string, got: {kv}"
        );
        let snippet = format!("[deps]\nfoo = {{ {kv} }}\n");
        let _: toml::Value =
            toml::from_str(&snippet).expect("apostrophe-path renders as escaped basic string");
    }

    #[test]
    fn write_if_different_is_idempotent() {
        let dir = tempdir();
        let p = dir.join("file.txt");
        write_if_different(&p, "abc").unwrap();
        let mtime1 = fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_if_different(&p, "abc").unwrap();
        let mtime2 = fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2, "no rewrite when content unchanged");
        write_if_different(&p, "def").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "def");
    }

    #[test]
    fn parse_signature_dump_round_trips_minimal_input() {
        let json = r#"{"modules":[{"path":"echo","doc":"d","items":[{"name":"shout","doc":"","params":[{"kind":"string"}],"ret":{"kind":"string"}}]}]}"#;
        let parsed = parse_signature_dump(json).unwrap();
        assert_eq!(parsed.modules.len(), 1);
        assert_eq!(parsed.modules[0].path, "echo");
        assert_eq!(parsed.modules[0].items[0].name, "shout");
        assert!(matches!(parsed.modules[0].items[0].ret, DumpedType::String));
    }

    #[test]
    fn parse_signature_dump_handles_nested_types() {
        let json = r#"{"modules":[{"path":"m","doc":"","items":[{"name":"f","doc":"","params":[{"kind":"vec","of":{"kind":"i64"}}],"ret":{"kind":"result","ok":{"kind":"i64"},"err":{"kind":"string"}}}]}]}"#;
        let parsed = parse_signature_dump(json).unwrap();
        let item = &parsed.modules[0].items[0];
        assert!(
            matches!(&item.params[0], DumpedType::Vec { of } if matches!(**of, DumpedType::I64))
        );
        assert!(matches!(&item.ret, DumpedType::Result { .. }));
    }

    /// A lock file names the process that holds it. A value that is not a pid
    /// must read as dead, or a workdir the cache should reclaim stays pinned
    /// and the lock nothing owns is never taken.
    #[test]
    fn a_value_that_is_not_a_pid_is_not_alive() {
        assert!(!pid_alive(0), "pid 0 names a process group, not a process");
        assert!(!pid_alive(u32::MAX), "no platform hands out this pid");
        assert!(pid_alive(std::process::id()), "this process is alive");
    }

    /// A held lock is what keeps reclamation and a build apart, so a second
    /// taker must be told the lock is busy instead of getting one of its own.
    #[test]
    fn a_held_build_lock_is_not_taken_twice() {
        let dir = tempdir();
        let path = build_lock_path(&dir);
        let held = AdvisoryLock::try_acquire(&path).expect("a free lock is takeable");
        assert!(AdvisoryLock::try_acquire(&path).is_none());
        drop(held);
        assert!(AdvisoryLock::try_acquire(&path).is_some());
    }

    /// The pins a generated project starts from decide which code its build
    /// scripts run, so an absent lockfile is the whole exposure: cargo would
    /// resolve every transitive crate to whatever the registry currently
    /// serves.
    #[test]
    fn generated_project_starts_from_the_toolchain_pins() {
        let root = tempdir();
        let project = tempdir();
        let pins = "version = 4\n\n[[package]]\nname = \"arrayref\"\nversion = \"0.3.9\"\n";
        fs::write(root.join(CARGO_LOCK), pins).unwrap();

        seed_lockfile(&project, &root).unwrap();

        assert_eq!(
            fs::read_to_string(project.join(CARGO_LOCK)).unwrap(),
            pins,
            "a generated project must resolve from the toolchain's graph, not the registry's newest"
        );
    }

    #[test]
    fn seeding_keeps_a_resolution_the_project_already_has() {
        let root = tempdir();
        let project = tempdir();
        fs::write(root.join(CARGO_LOCK), "version = 4\n").unwrap();
        seed_lockfile(&project, &root).unwrap();

        // What cargo resolved on top of the seed - the bindings' own tree.
        let resolved = "version = 4\n\n[[package]]\nname = \"user-binding\"\n";
        fs::write(project.join(CARGO_LOCK), resolved).unwrap();
        seed_lockfile(&project, &root).unwrap();

        assert_eq!(
            fs::read_to_string(project.join(CARGO_LOCK)).unwrap(),
            resolved,
            "an unchanged toolchain must not discard the bindings' resolved versions"
        );
    }

    #[test]
    fn moved_toolchain_pins_replace_an_older_seed() {
        let root = tempdir();
        let project = tempdir();
        fs::write(
            root.join(CARGO_LOCK),
            "version = 4\n\n[[package]]\nname = \"a\"\n",
        )
        .unwrap();
        seed_lockfile(&project, &root).unwrap();
        fs::write(
            project.join(CARGO_LOCK),
            "version = 4\n\n[[package]]\nname = \"stale\"\n",
        )
        .unwrap();

        let moved = "version = 4\n\n[[package]]\nname = \"a\"\nversion = \"2\"\n";
        fs::write(root.join(CARGO_LOCK), moved).unwrap();
        seed_lockfile(&project, &root).unwrap();

        assert_eq!(
            fs::read_to_string(project.join(CARGO_LOCK)).unwrap(),
            moved,
            "an upgraded toolchain's pins must reach projects an older one seeded"
        );
    }

    #[test]
    fn a_toolchain_without_a_lockfile_leaves_the_project_alone() {
        let root = tempdir();
        let project = tempdir();
        seed_lockfile(&project, &root).unwrap();
        assert!(!project.join(CARGO_LOCK).exists());
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "gos-binding-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn rand_suffix() -> String {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{now:x}")
    }
}
