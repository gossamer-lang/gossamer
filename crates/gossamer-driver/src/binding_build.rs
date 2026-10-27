//! Building `[rust-bindings]` crates with Cargo: the release `cdylib`
//! `gos run` loads (see [`crate::binding_library`]) and the
//! `libgos_static_bindings.a` (or `gos_static_bindings.lib` on Windows
//! MSVC) a native build links. Both live in the user cache under
//! `bindings/`, each build in a directory of its own that its build lock
//! guards.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use gossamer_pkg::{GitRef, Manifest, RustBindingSpec};
use thiserror::Error;

/// The name of the file whose lock guards one build directory.
const BUILD_LOCK: &str = ".gos-build.lock";

/// Cargo's resolved dependency graph.
const CARGO_LOCK: &str = "Cargo.lock";

/// The seed a generated project's lockfile was last written from.
const LOCK_SEED_STAMP: &str = ".gos-lock-seed";

/// The dependency graph the toolchain itself resolved, which seeds each
/// generated project's lockfile so the versions its dependencies resolve to
/// are the audited ones wherever both graphs meet.
pub(crate) const TOOLCHAIN_LOCK: &str = include_str!("../../../Cargo.lock");

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

/// Errors raised building a binding.
#[derive(Debug, Error)]
pub enum BindingBuildError {
    /// `cargo` was not found on PATH.
    #[error("this project declares `[rust-bindings]`; install Rust + cargo from https://rustup.rs")]
    CargoMissing,
    /// `cargo build` exited unsuccessfully. Its diagnostics were streamed to
    /// the terminal as the build ran, so the message does not repeat them.
    #[error("cargo build failed for binding `{crate_name}` ({status}); its output is above")]
    CargoBuildFailed {
        /// Crate that failed, or `<staticlib>` when the failure can't be
        /// attributed to one binding.
        crate_name: String,
        /// How cargo exited.
        status: std::process::ExitStatus,
        /// The last part of what cargo wrote, for a caller that reports the
        /// failure somewhere other than the terminal.
        output_tail: String,
    },
    /// I/O error while preparing the cache.
    #[error("cache i/o error: {0}")]
    Io(#[from] io::Error),
    /// The project manifest is malformed (e.g. a bare `[project] id`).
    /// A present-but-invalid manifest is a hard error, never a silent
    /// "no bindings".
    #[error("manifest error: {0}")]
    Manifest(String),
    /// rustc built the staticlib without reporting the native libraries it
    /// needs, so a link that takes the archive cannot name them.
    #[error("rustc did not report the native libraries `{0}` needs")]
    MissingNativeLibs(String),
}

/// A built `[rust-bindings]` staticlib and what linking it requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticBindingsArchive {
    /// The archive itself.
    pub archive: PathBuf,
    /// Linker driver arguments (`-lxcb`, `-framework AppKit`, `kernel32.lib`)
    /// naming the system libraries the archive's foreign code calls, in the
    /// order rustc reports them. They follow the archive on the link line.
    pub native_libs: Vec<String>,
    /// Directories the archive's build scripts name for native libraries,
    /// such as the import libraries of the `windows-targets` crates, which
    /// the link searches for `native_libs`.
    pub search: Vec<PathBuf>,
}

/// Profile (debug / release) of a binding build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// `cargo build` (no `--release`).
    Debug,
    /// `cargo build --release`.
    Release,
}

impl Profile {
    /// Cargo profile dirname.
    #[must_use]
    pub fn dir(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

/// A binding as its generated Cargo projects depend on it.
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

/// The staticlib a native build of a project with `[rust-bindings]` links.
#[derive(Debug)]
pub struct StaticBindingsLib {
    /// 12-char hex prefix of the manifest's binding fingerprint.
    pub fingerprint_hex: String,
    /// `<cache>/bindings/static-<fp>/`.
    pub workdir: PathBuf,
    /// Bindings to link into the staticlib.
    pub bindings: Vec<RenderedBinding>,
    /// Cargo profile.
    pub profile: Profile,
    /// Cross-compilation target triple passed to cargo (`--target`),
    /// e.g. `x86_64-unknown-linux-musl` for the static-musl release
    /// link. `None` builds for the host.
    pub cargo_target: Option<String>,
    /// Project id for cosmetic comments.
    pub project_id: String,
}

impl StaticBindingsLib {
    /// Constructs a staticlib build from the manifest. Returns
    /// `Ok(None)` if `[rust-bindings]` is empty.
    pub fn from_manifest(
        manifest: &Manifest,
        manifest_dir: &Path,
        profile: Profile,
    ) -> io::Result<Option<Self>> {
        Self::from_manifest_in(manifest, manifest_dir, profile, &cache_root()?)
    }

    /// Same as [`Self::from_manifest`] but uses an explicit cache
    /// root instead of reading `GOSSAMER_CACHE` / `XDG_CACHE_HOME`.
    pub fn from_manifest_in(
        manifest: &Manifest,
        manifest_dir: &Path,
        profile: Profile,
        cache_root: &Path,
    ) -> io::Result<Option<Self>> {
        if manifest.rust_bindings.is_empty() {
            return Ok(None);
        }
        let fingerprint = manifest.rust_binding_fingerprint(manifest_dir);
        let fingerprint_hex = hex_prefix(&fingerprint, 6);
        let workdir = cache_root
            .join("bindings")
            .join(format!("static-{fingerprint_hex}"));
        Ok(Some(Self {
            fingerprint_hex,
            workdir,
            bindings: render_bindings(&manifest.rust_bindings, manifest_dir),
            profile,
            cargo_target: None,
            project_id: manifest.project.id.as_str().to_string(),
        }))
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

    /// Where the native libraries rustc reported for the archive are recorded,
    /// beside the archive of the same target and profile.
    fn native_libs_path(&self) -> PathBuf {
        self.archive_path().with_file_name("native-static-libs.txt")
    }

    fn native_search_path(&self) -> PathBuf {
        self.archive_path().with_file_name("native-link-search.txt")
    }

    /// The generated crate's manifest: one dependency per binding, built as
    /// one staticlib so the bindings share one copy of their dependencies.
    fn cargo_toml(&self) -> String {
        let mut deps: Vec<String> = self
            .bindings
            .iter()
            .map(|b| b.cargo_dep_line.clone())
            .collect();
        // `lib.rs` names the runtime allocator `gossamer-binding` defines.
        deps.push(gossamer_binding_dependency());
        format!(
            r#"# generated by gos build for {project} -- fingerprint {fp}
[package]
name = "gos-static-{fp}"
version = "0.0.0"
edition = "2024"
publish = false

# A workspace of its own, so a cache under some other workspace's tree is
# not absorbed into it.
[workspace]

[lib]
name = "gos_static_bindings"
crate-type = ["staticlib"]
path = "lib.rs"

[dependencies]
{deps}

[profile.release]
opt-level = 3
lto = "thin"
incremental = false

[profile.dev]
opt-level = 0
debug = 0
incremental = false

# Build scripts and proc-macros run only on the build machine, and rustc loads
# a proc-macro through the crate metadata in its dylib. They stay unstripped:
# the Xcode 27 linker lays out a stripped macOS dylib built for a deployment
# target the SDK supplies in a way rustc cannot read that metadata from.
[profile.release.build-override]
strip = false

[profile.dev.build-override]
strip = false

{patch}"#,
            project = self.project_id,
            fp = self.fingerprint_hex,
            deps = deps.join("\n"),
            patch =
                binding_crates_patch(self.bindings.iter().filter_map(|b| b.local_root.as_deref())),
        )
    }

    /// The generated crate's source: it names each binding, so the archive
    /// holds every binding's thunks and the `gos_binding_init` the program
    /// calls before `main`.
    fn lib_rs(&self) -> String {
        let mut out = format!(
            "// generated by gos build -- fingerprint {}\n",
            self.fingerprint_hex
        );
        for binding in &self.bindings {
            out.push_str(&format!(
                "extern crate {};\n",
                binding.crate_name.replace('-', "_")
            ));
        }
        // The archive links beside the runtime's, each with its own `std`;
        // one allocator for both keeps every free with the allocator that
        // made the memory.
        out.push_str(
            "\n#[global_allocator]\nstatic ALLOCATOR: gossamer_binding::RuntimeAllocator = \
             gossamer_binding::RuntimeAllocator;\n",
        );
        out
    }

    /// Idempotently builds the staticlib. Returns the produced archive
    /// (`.a` on Unix / Windows-GNU, `.lib` on Windows MSVC) with the native
    /// libraries a link that takes it must also name.
    pub fn ensure_built(&self) -> Result<StaticBindingsArchive, BindingBuildError> {
        fs::create_dir_all(&self.workdir)?;
        let crates: Vec<&str> = self
            .bindings
            .iter()
            .map(|b| b.crate_name.as_str())
            .collect();
        let _lock = AdvisoryLock::acquire(
            &build_lock_path(&self.workdir),
            &format!("the static library of Rust bindings {}", crates.join(", ")),
        )?;
        let cargo_toml = self.workdir.join("Cargo.toml");
        write_if_different(&cargo_toml, &self.cargo_toml())?;
        write_if_different(&self.workdir.join("lib.rs"), &self.lib_rs())?;
        seed_lockfile(&self.workdir, TOOLCHAIN_LOCK)?;

        let archive = self.archive_path();
        let native_libs_path = self.native_libs_path();
        let search_path = self.native_search_path();
        let stamp = self
            .workdir
            .join(format!("stamp-{}.json", self.profile.dir()));
        if self.is_fresh(&archive, &stamp)?
            && let Ok(recorded) = fs::read_to_string(&native_libs_path)
            && let Ok(searched) = fs::read_to_string(&search_path)
        {
            return Ok(StaticBindingsArchive {
                archive,
                native_libs: recorded.split_whitespace().map(str::to_string).collect(),
                search: searched.lines().map(PathBuf::from).collect(),
            });
        }
        let mut messages = Vec::new();
        let output = run_cargo_build(
            &CargoBuild {
                manifest_path: &cargo_toml,
                target_dir: &self.workdir.join("target"),
                profile: self.profile,
                cargo_target: self.cargo_target.as_deref(),
                target_args: &["--lib"],
                crate_label: "<staticlib>",
                rustc_args: &["--print", "native-static-libs"],
            },
            Some(&mut messages),
        )?;
        let native_libs = parse_native_static_libs(&output)
            .ok_or_else(|| BindingBuildError::MissingNativeLibs(archive.display().to_string()))?;
        let search = native_search_dirs(&String::from_utf8_lossy(&messages));
        fs::write(&native_libs_path, native_libs.join("\n"))?;
        let search_lines: Vec<String> = search.iter().map(|d| d.display().to_string()).collect();
        fs::write(&search_path, search_lines.join("\n"))?;
        link_archive_exception_tables(&archive)?;
        write_stamp(
            &stamp,
            &self.fingerprint_hex,
            self.profile,
            &self.stamp_kind(),
        )?;
        if let Some(root) = self.workdir.parent() {
            let _ = crate::cache_maintenance::prune_binding_root(
                root,
                crate::cache_maintenance::CachePolicy::default(),
                false,
                Some(&self.workdir),
            );
        }
        Ok(StaticBindingsArchive {
            archive,
            native_libs,
            search,
        })
    }

    fn stamp_kind(&self) -> String {
        match &self.cargo_target {
            Some(t) => format!("staticlib:{t}"),
            None => "staticlib".to_string(),
        }
    }

    fn is_fresh(&self, artifact: &Path, stamp: &Path) -> Result<bool, BindingBuildError> {
        let Ok(stamp_text) = fs::read_to_string(stamp) else {
            return Ok(false);
        };
        if !stamp_text.contains(&self.fingerprint_hex) || !stamp_text.contains(&self.stamp_kind()) {
            return Ok(false);
        }
        let Ok(artifact_meta) = artifact.metadata() else {
            return Ok(false);
        };
        let artifact_mtime = artifact_meta.modified()?;
        let newest = newest_source(&self.bindings)?;
        Ok(newest.is_none_or(|source| source <= artifact_mtime))
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

pub(crate) fn render_one(
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
    let cargo_toml = format!(
        "[package]\nname = \"gos-srcwrap-{name}\"\nversion = \"0.0.1\"\nedition = \"2024\"\npublish = false\n\n[workspace]\n\n[lib]\ncrate-type = [\"rlib\"]\n\n[dependencies]\n{deps}\n{}\n",
        gossamer_binding_dependency(),
    );
    let lib_rs = format!(
        "//! Generated wrapper around `{}` for the `{name}` binding.\n\n#[path = {:?}]\nmod __user;\n\npub use __user::*;\n",
        src_file.display(),
        src_file.display(),
    );
    write_if_different(&wrapper_dir.join("Cargo.toml"), &cargo_toml)?;
    write_if_different(&wrapper_dir.join("src").join("lib.rs"), &lib_rs)?;
    Ok(())
}

/// The crates a binding builds against that are not published to
/// crates.io; a generated project patches each to this toolchain's copy.
const BINDING_CRATES: [&str; 3] = [
    "gossamer-binding",
    "gossamer-binding-abi",
    "gossamer-binding-macros",
];

/// The Gossamer source checkout this toolchain was built from, when it is
/// reachable: `GOSSAMER_ROOT`, the tree this crate was compiled in, or a
/// directory above the running binary holding the binding crates.
#[must_use]
pub fn toolchain_source_root() -> Option<PathBuf> {
    let holds_bindings = |root: &Path| root.join("crates").join("gossamer-binding").is_dir();
    if let Some(root) = std::env::var_os("GOSSAMER_ROOT").map(PathBuf::from)
        && holds_bindings(&root)
    {
        return Some(root);
    }
    let compiled_in = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    if let Some(root) = compiled_in
        && holds_bindings(&root)
    {
        return Some(root);
    }
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .skip(1)
        .find(|dir| holds_bindings(dir))
        .map(Path::to_path_buf)
}

/// Where a generated project takes `krate` from: the checkout's copy when
/// one is reachable, otherwise this toolchain's release tag of the repository.
fn binding_crate_source(krate: &str) -> String {
    match toolchain_source_root() {
        Some(root) => toml_path_kv("path", &root.join("crates").join(krate)),
        None => format!(
            "git = \"https://github.com/gossamer-lang/gossamer\", tag = \"v{}\"",
            env!("CARGO_PKG_VERSION")
        ),
    }
}

/// Where generated projects take the binding SDK from: this checkout's
/// `crates/gossamer-binding` when one is reachable, else the repository at
/// this toolchain's release tag, as `URL@TAG`.
#[must_use]
pub fn binding_sdk_location() -> String {
    match toolchain_source_root() {
        Some(root) => root
            .join("crates")
            .join("gossamer-binding")
            .display()
            .to_string(),
        None => format!(
            "https://github.com/gossamer-lang/gossamer@v{}",
            env!("CARGO_PKG_VERSION")
        ),
    }
}

/// The `gossamer-binding` dependency a generated binding crate names.
#[must_use]
pub fn gossamer_binding_dependency() -> String {
    format!(
        "gossamer-binding = {{ {} }}",
        binding_crate_source("gossamer-binding")
    )
}

/// The `[patch.crates-io]` table a generated root project carries.
///
/// A binding that depends on `gossamer-binding = "<version>"`, as `gos new
/// --template binding` writes it, resolves to this toolchain's copy. A crate
/// the set carries on disk - one of `local_roots`, the binding crates whose
/// sources are local, or a crate they reach by path - also stands in for
/// that crate wherever a crate of the set asks for it by version. A
/// library's Rust core, shared by the bindings of the packages built on it,
/// then links once, whichever package's tree each binding was fetched into.
#[must_use]
pub fn binding_crates_patch<'a>(local_roots: impl IntoIterator<Item = &'a Path>) -> String {
    let mut out = String::from("[patch.crates-io]\n");
    for krate in BINDING_CRATES {
        out.push_str(&format!(
            "{krate} = {{ {} }}\n",
            binding_crate_source(krate)
        ));
    }
    for (name, dir) in shared_crates(local_roots) {
        out.push_str(&format!("{name} = {{ {} }}\n", toml_path_kv("path", &dir)));
    }
    out
}

/// Each crate the set carries on disk that a crate of the set asks for by
/// version, by name.
fn shared_crates<'a>(local_roots: impl IntoIterator<Item = &'a Path>) -> BTreeMap<String, PathBuf> {
    let mut provided: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut requested: Vec<String> = Vec::new();
    let mut pending: Vec<PathBuf> = local_roots.into_iter().map(Path::to_path_buf).collect();
    let mut seen: Vec<PathBuf> = Vec::new();
    while let Some(dir) = pending.pop() {
        let identity = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if seen.contains(&identity) {
            continue;
        }
        seen.push(identity);
        let Some(doc) = cargo_manifest(&dir.join("Cargo.toml")) else {
            continue;
        };
        if let Some(name) = doc
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
        {
            provided.entry(name.to_string()).or_insert(dir.clone());
        }
        for (key, dep) in dependency_entries(&doc) {
            match dep.get("path").and_then(toml::Value::as_str) {
                Some(path) => pending.push(dir.join(path)),
                None if dep.get("git").is_none() => requested.push(
                    dep.get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(key)
                        .to_string(),
                ),
                None => {}
            }
        }
    }
    provided.retain(|name, _| requested.contains(name) && !BINDING_CRATES.contains(&name.as_str()));
    provided
}

fn cargo_manifest(path: &Path) -> Option<toml::Table> {
    fs::read_to_string(path).ok()?.parse::<toml::Table>().ok()
}

/// Every `(key, spec)` of a `Cargo.toml`'s dependency tables, the
/// per-target ones included.
fn dependency_entries(doc: &toml::Table) -> Vec<(&str, &toml::Value)> {
    let mut tables: Vec<&toml::Table> = Vec::new();
    for key in ["dependencies", "build-dependencies"] {
        if let Some(table) = doc.get(key).and_then(toml::Value::as_table) {
            tables.push(table);
        }
    }
    if let Some(targets) = doc.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            for key in ["dependencies", "build-dependencies"] {
                if let Some(table) = target.get(key).and_then(toml::Value::as_table) {
                    tables.push(table);
                }
            }
        }
    }
    tables
        .into_iter()
        .flat_map(|table| table.iter().map(|(key, dep)| (key.as_str(), dep)))
        .collect()
}

/// The directories a `Cargo.toml` names as path dependencies, in any of its
/// dependency tables.
pub(crate) fn path_dependencies(manifest: &Path) -> Vec<PathBuf> {
    let Some(doc) = cargo_manifest(manifest) else {
        return Vec::new();
    };
    let base = manifest.parent().unwrap_or(Path::new("."));
    dependency_entries(&doc)
        .into_iter()
        .filter_map(|(_, dep)| dep.get("path").and_then(toml::Value::as_str))
        .map(|path| base.join(path))
        .collect()
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

pub(crate) fn cache_root() -> io::Result<PathBuf> {
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

/// The lock guarding one build directory: a build holds it while it writes
/// there, and reclamation removes the directory only while holding it.
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
pub(crate) fn seed_lockfile(project_dir: &Path, seed: &str) -> io::Result<()> {
    let lock = project_dir.join(CARGO_LOCK);
    let stamp = project_dir.join(LOCK_SEED_STAMP);
    if lock.exists() && fs::read(&stamp).is_ok_and(|previous| previous == seed.as_bytes()) {
        return Ok(());
    }
    fs::write(&lock, seed)?;
    fs::write(&stamp, seed)?;
    Ok(())
}

pub(crate) fn write_if_different(path: &Path, contents: &str) -> io::Result<()> {
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

/// One `cargo` build of a generated project.
pub(crate) struct CargoBuild<'a> {
    pub(crate) manifest_path: &'a Path,
    pub(crate) target_dir: &'a Path,
    pub(crate) profile: Profile,
    pub(crate) cargo_target: Option<&'a str>,
    /// Which of the project's targets to build (`--lib`, `--bin NAME`).
    pub(crate) target_args: &'a [&'a str],
    /// How a failure names the crate.
    pub(crate) crate_label: &'a str,
    /// Arguments for the final crate's rustc alone.
    pub(crate) rustc_args: &'a [&'a str],
}

pub(crate) fn run_cargo_build(
    build: &CargoBuild<'_>,
    messages: Option<&mut Vec<u8>>,
) -> Result<String, BindingBuildError> {
    let CargoBuild {
        manifest_path,
        target_dir,
        profile,
        cargo_target,
        target_args,
        crate_label,
        rustc_args,
    } = *build;
    let cargo = which::which("cargo").map_err(|_| BindingBuildError::CargoMissing)?;
    let mut cmd = Command::new(cargo);
    // `cargo rustc` hands its trailing arguments to the final crate's rustc alone.
    cmd.arg(if rustc_args.is_empty() {
        "build"
    } else {
        "rustc"
    });
    if matches!(profile, Profile::Release) {
        cmd.arg("--release");
    }
    if let Some(t) = cargo_target {
        cmd.arg("--target").arg(t);
    }
    configure_macos_cargo_build(&mut cmd, cargo_target);
    cmd.arg("--manifest-path").arg(manifest_path);
    cmd.args(target_args);
    cmd.env("CARGO_TARGET_DIR", target_dir);
    // Both streams pass through to the terminal while cargo runs, so a long
    // build shows its progress and a failing one its diagnostics as they
    // happen. Cargo colours them only when told to, its stderr being a pipe.
    if io::stderr().is_terminal() {
        cmd.arg("--color").arg("always");
    }
    // Cargo still renders diagnostics on stderr; its JSON messages go to
    // stdout, which `run_streamed` hands to `messages` instead of the terminal.
    if messages.is_some() {
        cmd.arg("--message-format=json-render-diagnostics");
    }
    if !rustc_args.is_empty() {
        cmd.arg("--").args(rustc_args);
    }
    let (status, output_tail) = run_streamed(cmd, &mut io::stderr(), messages)?;
    if !status.success() {
        return Err(BindingBuildError::CargoBuildFailed {
            crate_name: crate_label.to_string(),
            status,
            output_tail,
        });
    }
    Ok(output_tail)
}

/// The native library directories the build scripts in cargo's JSON
/// `messages` add to the link (`cargo:rustc-link-search`), in the order
/// cargo reports them.
fn native_search_dirs(messages: &str) -> Vec<PathBuf> {
    #[derive(serde::Deserialize)]
    struct Message {
        reason: String,
        #[serde(default)]
        linked_paths: Vec<String>,
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    for line in messages.lines() {
        // Every line is a message; one of a kind this reader does not model
        // carries nothing it needs.
        let Ok(message) = serde_json::from_str::<Message>(line) else {
            continue;
        };
        if message.reason != "build-script-executed" {
            continue;
        }
        for path in &message.linked_paths {
            // `KIND=PATH`, where a crate or framework kind names no native
            // library directory.
            let dir = match path.split_once('=') {
                Some(("native" | "all", dir)) => dir,
                Some(_) => continue,
                None => path,
            };
            let dir = PathBuf::from(dir);
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// The linker arguments rustc reports in its `native-static-libs` note: the
/// system libraries and frameworks a staticlib's foreign code needs, which a
/// link that takes the archive must name itself.
///
/// Cargo replays a fresh unit's cached diagnostics, so the note is present
/// whether or not the final crate was recompiled.
fn parse_native_static_libs(output: &str) -> Option<Vec<String>> {
    const MARKER: &str = "native-static-libs:";
    let plain = strip_ansi(output);
    let line = plain.lines().rev().find(|l| l.contains(MARKER))?;
    let (_, libs) = line.split_once(MARKER)?;
    Some(libs.split_whitespace().map(str::to_string).collect())
}

/// `text` with its ANSI escape sequences removed.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // A CSI sequence (`ESC [ params final`) ends at its first byte in `@..=~`.
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// How much of a streamed child's output [`run_streamed`] keeps for a
/// failure report.
const STREAMED_TAIL_BYTES: usize = 16 * 1024;

/// Runs `command` with both of its output streams copied to `sink` as they
/// arrive, and answers how it exited with the last [`STREAMED_TAIL_BYTES`] of
/// its output. With `messages`, the child's stdout is collected there in full
/// instead of reaching `sink` or the tail.
///
/// Each pipe is drained on its own thread, so a child that fills one pipe
/// while this side reads the other never blocks on its write. The threads
/// hand their chunks to this one, which alone writes `sink`, so a chunk
/// reaches it whole.
fn run_streamed(
    mut command: Command,
    sink: &mut impl Write,
    mut messages: Option<&mut Vec<u8>>,
) -> io::Result<(std::process::ExitStatus, String)> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let (sender, chunks) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();
    let readers = [
        child
            .stdout
            .take()
            .map(|s| forward_chunks(s, true, sender.clone())),
        child
            .stderr
            .take()
            .map(|s| forward_chunks(s, false, sender.clone())),
    ];
    drop(sender);
    let mut tail: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
    for (from_stdout, chunk) in chunks {
        if from_stdout && let Some(collected) = messages.as_deref_mut() {
            collected.extend_from_slice(&chunk);
            continue;
        }
        // A closed terminal must not stop the drain, or the child blocks.
        let _ = sink.write_all(&chunk).and_then(|()| sink.flush());
        tail.extend(chunk);
        let excess = tail.len().saturating_sub(STREAMED_TAIL_BYTES);
        tail.drain(..excess);
    }
    for reader in readers.into_iter().flatten() {
        reader
            .join()
            .map_err(|_| io::Error::other("cargo output reader panicked"))??;
    }
    let status = child.wait()?;
    let tail: Vec<u8> = tail.into();
    Ok((status, String::from_utf8_lossy(&tail).into_owned()))
}

/// Reads `stream` to its end on a thread of its own, sending each chunk as
/// it arrives, marked with whether it is the child's stdout.
fn forward_chunks<R: Read + Send + 'static>(
    mut stream: R,
    is_stdout: bool,
    sender: std::sync::mpsc::Sender<(bool, Vec<u8>)>,
) -> std::thread::JoinHandle<io::Result<()>> {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    // The receiver outlives every reader, so a send only
                    // fails if it panicked, and then nobody is listening.
                    let _ = sender.send((is_stdout, buf[..n].to_vec()));
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
    })
}

fn configure_macos_cargo_build(command: &mut Command, cargo_target: Option<&str>) {
    if crate::macos_deployment::is_macos_target(cargo_target, cfg!(target_os = "macos")) {
        let deployment_target = crate::macos_deployment::effective_deployment_target();
        crate::macos_deployment::set_command_deployment_target(command, &deployment_target);
    }
}

/// The newest modification time of any source file of the path bindings
/// and the crates they name by path, or `None` when no binding is a path.
fn newest_source(bindings: &[RenderedBinding]) -> io::Result<Option<SystemTime>> {
    let mut newest = None;
    let mut seen = Vec::new();
    for binding in bindings {
        if let Some(root) = &binding.local_root
            && root.is_dir()
        {
            crate::binding_library::newest_in_tree(root, &mut newest, &mut seen)?;
        }
    }
    Ok(newest)
}

/// Cross-process advisory lock on a file in a workdir.
///
/// The operating system holds the lock (`File::lock`) and releases it when
/// the holder's process ends, so a build that dies leaves nothing to reclaim.
/// The file itself is removed when the guard drops, and reclamation removes
/// it with the rest of its workdir. A process that waited on a file that has
/// since been removed holds a lock on nothing anyone else can see, so a new
/// holder writes a token of its own into the file and checks the path still
/// reads it back before it counts the lock as taken.
pub(crate) struct AdvisoryLock {
    path: PathBuf,
    /// The locked file; closing it releases the lock.
    file: fs::File,
}

impl AdvisoryLock {
    /// Takes the lock, waiting while another process holds it. `build` names
    /// what the holder is compiling, for the message a wait prints.
    pub(crate) fn acquire(path: &Path, build: &str) -> io::Result<Self> {
        let mut announced = false;
        loop {
            let file = open_lock_file(path)?;
            match file.try_lock() {
                Ok(()) => {}
                Err(fs::TryLockError::WouldBlock) => {
                    if !announced {
                        let _ = writeln!(
                            io::stderr(),
                            "gos: waiting for another `gos` process to finish compiling {build} \
                             in {}; this process then uses the library it built",
                            path.parent().unwrap_or(path).display()
                        );
                        announced = true;
                    }
                    file.lock()?;
                }
                Err(fs::TryLockError::Error(err)) => return Err(err),
            }
            if let Some(lock) = Self::claim(path, file)? {
                return Ok(lock);
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

    /// Takes the lock without waiting: `Ok(None)` means another holder has
    /// it. A file removed from the path between the open and the lock is
    /// opened afresh, and each retry follows such a removal.
    fn take(path: &Path) -> io::Result<Option<Self>> {
        loop {
            let file = open_lock_file(path)?;
            match file.try_lock() {
                Ok(()) => {}
                Err(fs::TryLockError::WouldBlock) => return Ok(None),
                Err(fs::TryLockError::Error(err)) => return Err(err),
            }
            if let Some(lock) = Self::claim(path, file)? {
                return Ok(Some(lock));
            }
        }
    }

    /// With `file` locked, answers the guard if `path` still names that file.
    /// `None` when the file was removed from the path while it was being
    /// locked: nothing else can reach it, so the lock on it guards nothing.
    fn claim(path: &Path, file: fs::File) -> io::Result<Option<Self>> {
        match names_same_file(&file, path) {
            Ok(true) => Ok(Some(Self {
                path: path.to_path_buf(),
                file,
            })),
            Ok(false) => Ok(None),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            // Windows refuses to open a file whose removal waits on handles
            // still open, and the handle this holder keeps is one of them.
            #[cfg(windows)]
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => Ok(None),
            Err(err) => Err(err),
        }
    }
}

/// Whether `path` names the file `file` has open.
#[cfg(unix)]
fn names_same_file(file: &fs::File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = file.metadata()?;
    let named = fs::metadata(path)?;
    Ok(held.dev() == named.dev() && held.ino() == named.ino())
}

/// Whether `path` names the file `file` has open.
// A file's identity on Windows is only reachable through
// `GetFileInformationByHandle`; the unsafe is contained to this function.
#[cfg(windows)]
#[allow(unsafe_code)]
fn names_same_file(file: &fs::File, path: &Path) -> io::Result<bool> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle,
    };

    fn identity(file: &fs::File) -> io::Result<(u32, u32, u32)> {
        // SAFETY: `BY_HANDLE_FILE_INFORMATION` is plain integers, for which all zeroes is valid.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the handle belongs to `file`, open for the call, and `info` is the out-parameter
        // the call fills.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }
    // A byte-range lock refuses reads through every other handle, the locking
    // process's own included, but not an open that asks for no data access.
    let named = fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)?;
    Ok(identity(file)? == identity(&named)?)
}

/// Opens the lock file at `path`, creating it and the workdir around it as
/// needed: the workdir is reclaimed as a whole, so it can be gone between one
/// build and the next.
fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    let open = || {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    };
    match open() {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let Some(parent) = path.parent() else {
                return Err(err);
            };
            fs::create_dir_all(parent)?;
            open()
        }
        opened => opened,
    }
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        // The path may name a newer holder's file once this one was removed
        // from under it; that file is the newer holder's to remove.
        if names_same_file(&self.file, &self.path).unwrap_or(false) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Ties each function's exception table in `archive` to its code, so a link
/// that drops the function drops the table (see
/// [`crate::lsda_link_order`]). The archive is rewritten through a temporary
/// file, so a reader never sees it half written.
fn link_archive_exception_tables(archive: &Path) -> io::Result<()> {
    let mut bytes = fs::read(archive)?;
    if crate::lsda_link_order::link_exception_tables_in_archive(&mut bytes) == 0 {
        return Ok(());
    }
    let tmp = archive.with_extension(format!("a.tmp-{}", std::process::id()));
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, archive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_static_libs_parse_from_coloured_cargo_output() {
        let output = "   Compiling gos-static v0.0.0\n\
            \u{1b}[0m\u{1b}[1m\u{1b}[38;5;14mnote\u{1b}[0m\u{1b}[0m\u{1b}[1m: \
            native-static-libs: -lxcb -lxkbcommon -lgcc_s -lc\u{1b}[0m\n\
            \u{1b}[1m\u{1b}[32m    Finished\u{1b}[0m `dev` profile\n";
        assert_eq!(
            parse_native_static_libs(output),
            Some(vec![
                "-lxcb".into(),
                "-lxkbcommon".into(),
                "-lgcc_s".into(),
                "-lc".into()
            ]),
        );
    }

    #[test]
    fn native_search_dirs_come_from_build_script_messages() {
        let messages = concat!(
            r#"{"reason":"compiler-artifact","package_id":"a","filenames":[]}"#,
            "\n",
            r#"{"reason":"build-script-executed","linked_libs":["windows.0.53.0"],"#,
            r#""linked_paths":["native=C:\\w\\lib","dependency=C:\\deps"],"cfgs":[]}"#,
            "\n",
            r#"{"reason":"build-script-executed","linked_paths":["/ring/out","all=/x","native=C:\\w\\lib"]}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
        );
        assert_eq!(
            native_search_dirs(messages),
            vec![
                PathBuf::from(r"C:\w\lib"),
                PathBuf::from("/ring/out"),
                PathBuf::from("/x")
            ],
        );
    }

    #[test]
    fn native_static_libs_keep_framework_pairs_in_order() {
        let output = "note: native-static-libs: -framework AppKit -liconv -lSystem\n";
        assert_eq!(
            parse_native_static_libs(output),
            Some(vec![
                "-framework".into(),
                "AppKit".into(),
                "-liconv".into(),
                "-lSystem".into()
            ]),
        );
    }

    #[test]
    fn native_static_libs_absent_without_the_note() {
        assert_eq!(
            parse_native_static_libs("    Finished `dev` profile\n"),
            None
        );
    }
    use gossamer_pkg::Manifest;

    fn lock_scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gos-lock-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // A lock file left by a dead process on a location this process cannot
    // write is an error the caller reports, not a lock taken or a retry.
    #[cfg(unix)]
    #[test]
    fn a_lock_left_on_a_read_only_location_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = lock_scratch("read-only");
        let path = build_lock_path(&dir);
        fs::write(&path, format!("{}\n", u32::MAX)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        // A process that may write anything regardless (root) cannot observe
        // the refusal this test is about.
        let writable = fs::OpenOptions::new().write(true).open(&path).is_ok();
        if !writable {
            assert!(AdvisoryLock::take(&path).is_err());
            assert!(AdvisoryLock::acquire(&path, "test bindings").is_err());
            assert!(AdvisoryLock::try_acquire(&path).is_none());
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    // A dead process's lock file is no lock: the operating system released
    // the lock with the process, so the next build takes it.
    #[test]
    fn a_lock_file_left_by_a_dead_process_is_taken() {
        let dir = lock_scratch("dead-holder");
        let path = build_lock_path(&dir);
        fs::write(&path, format!("{}\n", u32::MAX)).unwrap();
        let lock = AdvisoryLock::take(&path)
            .unwrap()
            .expect("a released lock is free");
        assert!(AdvisoryLock::take(&path).unwrap().is_none());
        drop(lock);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    // Competing acquirers each hold the lock alone, however their attempts
    // interleave with the removal of the file by the holder before them.
    #[test]
    fn concurrent_acquirers_never_hold_the_lock_together() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = lock_scratch("concurrent");
        let path = build_lock_path(&dir);
        let holders = Arc::new(AtomicUsize::new(0));
        let taken = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let holders = Arc::clone(&holders);
                let taken = Arc::clone(&taken);
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        let lock = AdvisoryLock::acquire(&path, "test bindings").expect("acquire");
                        assert_eq!(holders.fetch_add(1, Ordering::SeqCst), 0);
                        std::thread::yield_now();
                        assert_eq!(holders.fetch_sub(1, Ordering::SeqCst), 1);
                        taken.fetch_add(1, Ordering::SeqCst);
                        drop(lock);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("acquirer");
        }
        assert_eq!(taken.load(Ordering::SeqCst), 8 * 25);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    // A waiter whose lock file is removed with its workdir while it waits
    // holds the lock on the file at the path once it gets it, and nobody
    // else can take that one.
    #[test]
    fn a_waiter_whose_lock_file_is_removed_takes_the_new_one() {
        let dir = lock_scratch("removed-while-waiting");
        let path = build_lock_path(&dir);
        let first = AdvisoryLock::take(&path).unwrap().expect("free");
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || {
                AdvisoryLock::acquire(&path, "test bindings").expect("acquire")
            })
        };
        std::thread::yield_now();
        // The file goes, then the directory if nothing has been created in
        // it since, as reclamation removes a workdir: a lock file the waiter
        // creates afresh is never deleted from under it.
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
        drop(first);
        let second = waiter.join().expect("waiter");
        assert!(
            path.exists(),
            "the waiter's lock is on the file at the path"
        );
        assert!(AdvisoryLock::take(&path).unwrap().is_none());
        drop(second);
        let _ = fs::remove_dir_all(&dir);
    }

    // A child that fills its stderr pipe before writing stdout must be
    // drained by concurrent readers; draining the pipes one after the
    // other lets the unread pipe fill and wedges the child mid-write.
    #[cfg(unix)]
    #[test]
    fn streamed_output_drains_both_pipes_without_deadlock() {
        const STDERR_BYTES: usize = 256 * 1024;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("yes X | head -c {STDERR_BYTES} 1>&2; printf DONE"));
        let mut sink = Vec::new();
        let (status, tail) = run_streamed(command, &mut sink, None).expect("run sh");
        assert!(status.success());
        // Each pipe has a reader of its own, so the two streams interleave in
        // whatever order their chunks arrive; what is fixed is that both are
        // read to the end.
        let done = sink
            .windows(4)
            .position(|w| w == b"DONE")
            .expect("stdout is drained");
        let mut stderr = sink.clone();
        stderr.drain(done..done + 4);
        assert_eq!(stderr, b"X\n".repeat(STDERR_BYTES / 2));
        assert_eq!(tail.len(), STREAMED_TAIL_BYTES);
    }

    /// A sink that releases the child once the child's first line reaches it.
    #[cfg(unix)]
    struct ReleasingSink {
        seen: Vec<u8>,
        release: PathBuf,
    }

    #[cfg(unix)]
    impl Write for ReleasingSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.seen.extend_from_slice(buf);
            if self.seen.windows(5).any(|w| w == b"first") {
                fs::write(&self.release, b"")?;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // Output reaches the terminal while the child still runs: the child
    // waits for a file the sink creates only after seeing its first line,
    // and gives up with status 3 if that never happens.
    #[cfg(unix)]
    #[test]
    fn streamed_output_arrives_before_the_child_exits() {
        let dir = std::env::temp_dir().join(format!("gos-stream-live-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let release = dir.join("released");
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "echo first 1>&2; i=0; while [ ! -f '{}' ]; do i=$((i+1)); [ $i -gt 200 ] && exit 3; sleep 0.05; done; echo second",
            release.display()
        ));
        let mut sink = ReleasingSink {
            seen: Vec::new(),
            release,
        };
        let (status, tail) = run_streamed(command, &mut sink, None).expect("run sh");
        assert_eq!(
            status.code(),
            Some(0),
            "the first line reached the sink only at exit"
        );
        assert_eq!(String::from_utf8_lossy(&sink.seen), "first\nsecond\n");
        assert_eq!(tail, "first\nsecond\n");
        let _ = fs::remove_dir_all(&dir);
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
        let project = tempdir();
        let pins = "version = 4\n\n[[package]]\nname = \"arrayref\"\nversion = \"0.3.9\"\n";

        seed_lockfile(&project, pins).unwrap();

        assert_eq!(
            fs::read_to_string(project.join(CARGO_LOCK)).unwrap(),
            pins,
            "a generated project must resolve from the toolchain's graph, not the registry's newest"
        );
    }

    #[test]
    fn seeding_keeps_a_resolution_the_project_already_has() {
        let project = tempdir();
        seed_lockfile(&project, "version = 4\n").unwrap();

        // What cargo resolved on top of the seed - the bindings' own tree.
        let resolved = "version = 4\n\n[[package]]\nname = \"user-binding\"\n";
        fs::write(project.join(CARGO_LOCK), resolved).unwrap();
        seed_lockfile(&project, "version = 4\n").unwrap();

        assert_eq!(
            fs::read_to_string(project.join(CARGO_LOCK)).unwrap(),
            resolved,
            "an unchanged toolchain must not discard the bindings' resolved versions"
        );
    }

    #[test]
    fn moved_toolchain_pins_replace_an_older_seed() {
        let project = tempdir();
        seed_lockfile(&project, "version = 4\n\n[[package]]\nname = \"a\"\n").unwrap();
        fs::write(
            project.join(CARGO_LOCK),
            "version = 4\n\n[[package]]\nname = \"stale\"\n",
        )
        .unwrap();

        let moved = "version = 4\n\n[[package]]\nname = \"a\"\nversion = \"2\"\n";
        seed_lockfile(&project, moved).unwrap();

        assert_eq!(
            fs::read_to_string(project.join(CARGO_LOCK)).unwrap(),
            moved,
            "an upgraded toolchain's pins must reach projects an older one seeded"
        );
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

    #[test]
    fn a_manifest_without_bindings_builds_no_staticlib() {
        let src = "[project]\nid = \"example.com/p\"\nversion = \"0.1.0\"\n";
        let m = Manifest::parse(src).unwrap();
        let out = StaticBindingsLib::from_manifest_in(
            &m,
            std::env::temp_dir().as_path(),
            Profile::Debug,
            &tempdir(),
        )
        .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn the_staticlib_names_every_binding_and_nothing_of_the_toolchain() {
        let cache = tempdir();
        let manifest_dir = tempdir();
        let body = "[project]\nid = \"example.com/p\"\nversion = \"0.1.0\"\n\n[rust-bindings]\necho-lib = { path = \"./echo\" }\nother = { path = \"./other\" }\n";
        let m = Manifest::parse(body).unwrap();
        let lib = StaticBindingsLib::from_manifest_in(&m, &manifest_dir, Profile::Release, &cache)
            .unwrap()
            .expect("a staticlib");
        assert!(lib.workdir.starts_with(cache.join("bindings")));
        let lib_rs = lib.lib_rs();
        assert!(lib_rs.contains("extern crate echo_lib;"), "{lib_rs}");
        assert!(lib_rs.contains("extern crate other;"), "{lib_rs}");
        let cargo = lib.cargo_toml();
        assert!(cargo.contains("echo-lib = {"), "{cargo}");
        assert!(
            lib_rs.contains("gossamer_binding::RuntimeAllocator"),
            "{lib_rs}"
        );
        let toolchain: Vec<&str> = cargo
            .lines()
            .filter(|line| line.starts_with("gossamer-"))
            .filter(|line| {
                !BINDING_CRATES
                    .iter()
                    .any(|k| line.starts_with(&format!("{k} = ")))
            })
            .collect();
        assert!(toolchain.is_empty(), "{cargo}");
    }

    #[test]
    fn a_crate_one_binding_carries_stands_in_where_another_asks_for_it_by_version() {
        let dir = std::env::temp_dir().join(format!("gos-shared-crates-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let write = |path: &str, text: &str| {
            let file = dir.join(path).join("Cargo.toml");
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        };
        // One package's binding carries the core crate beside it.
        write("lower/core", "[package]\nname = \"lower-core\"\n");
        write(
            "lower/binding",
            "[package]\nname = \"lower-binding\"\n[dependencies]\nlower-core = { path = \"../core\" }\n",
        );
        // Another package's engine asks for it by version, under its own key.
        write(
            "upper/engine",
            "[package]\nname = \"upper-engine\"\n[dependencies]\nlower = { package = \"lower-core\", version = \"0.1.0\" }\nserde = \"1\"\n",
        );
        write(
            "upper/binding",
            "[package]\nname = \"upper-binding\"\n[dependencies]\nupper-engine = { path = \"../engine\" }\n",
        );
        let roots = [dir.join("lower/binding"), dir.join("upper/binding")];
        let shared = shared_crates(roots.iter().map(PathBuf::as_path));
        assert_eq!(
            shared.keys().collect::<Vec<_>>(),
            vec!["lower-core"],
            "only a crate the set carries and also asks for by version is patched"
        );
        let patch = binding_crates_patch(roots.iter().map(PathBuf::as_path));
        assert!(patch.contains("lower-core = { path = "), "{patch}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn generated_projects_patch_every_unpublished_binding_crate() {
        let patch = binding_crates_patch([]);
        assert!(patch.starts_with("[patch.crates-io]\n"), "{patch}");
        for krate in BINDING_CRATES {
            assert!(patch.contains(&format!("\n{krate} = {{ ")), "{patch}");
        }
        // This test runs from the checkout, so the patch names its copies.
        let root = toolchain_source_root().expect("the checkout is reachable");
        assert!(
            patch.contains(
                &root
                    .join("crates")
                    .join("gossamer-binding")
                    .display()
                    .to_string()
            )
        );
    }
}
