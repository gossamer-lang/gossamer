//! `project.toml` parser.
//! Ships a deliberately small TOML reader covering exactly
//! the subset SPEC §6.4 / §16.1 specifies. Pulling in a full TOML
//! crate is overkill for the manifest grammar and would balloon the
//! workspace's dependency graph; the keys we accept are well-defined
//! enough that hand parsing stays manageable.

#![forbid(unsafe_code)]
#![allow(clippy::too_many_lines, clippy::implicit_clone)]

use std::collections::BTreeMap;

use thiserror::Error;

use crate::id::{ProjectId, ProjectIdError};
use crate::version::{Version, VersionError, VersionReq};

const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// Parsed `project.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// `[project]` table.
    pub project: ProjectTable,
    /// `[dependencies]` map keyed by project id.
    pub dependencies: BTreeMap<String, DependencySpec>,
    /// Module-name overrides from a dependency's `module = "..."` key,
    /// keyed by project id. A dependency without one is reached under the
    /// name derived from the final segment of its id, so two packages
    /// sharing that segment need one of these to coexist.
    pub dependency_modules: BTreeMap<String, String>,
    /// The features each dependency is built with and whether it is
    /// optional, keyed by project id; a dependency absent here is required
    /// and builds with its default features.
    pub dependency_features: BTreeMap<String, DependencyFeatures>,
    /// `[features]` - each feature this package offers, with the features,
    /// `dep:<id>` optional dependencies, and `<id>/<feature>` dependency
    /// features it turns on. `default` lists those a build gets unless it
    /// asks otherwise.
    pub features: BTreeMap<String, Vec<String>>,
    /// `[registries]` map keyed by DNS prefix.
    pub registries: BTreeMap<String, String>,
    /// `[trusted-publishers]` map from package id to the hex Ed25519
    /// key authorized to sign its registry tarballs. This is a trust
    /// root supplied by the project, not by the mutable registry index.
    pub trusted_publishers: BTreeMap<String, String>,
    /// `[rust-bindings]` map keyed by Cargo crate name.
    pub rust_bindings: BTreeMap<String, RustBindingSpec>,
    /// `[[bin]]` array-of-tables - explicit binary targets.
    /// When empty, the implicit `main.gos` / `src/main.gos`
    /// filesystem convention applies (with a deprecation
    /// warning planned for 0.5).
    pub bins: Vec<BinTarget>,
    /// `[lib]` table - explicit library target. `None` means
    /// no library; the implicit `lib.gos` / `src/lib.gos`
    /// convention only applies when no `[[bin]]` is declared
    /// either.
    pub lib: Option<LibTarget>,
    /// `[native]` table - C and assembly sources the package compiles
    /// with the target's C compiler and links like a `#[link]` library.
    pub native: Option<NativeSpec>,
}

/// How a dependency's features are chosen: `features = [..]`,
/// `default-features = false`, and `optional = true` in its inline table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyFeatures {
    /// Features turned on in the dependency.
    pub features: Vec<String>,
    /// Whether the dependency's `default` features are on.
    pub default_features: bool,
    /// Whether the dependency is built only when a `dep:<id>` member of an
    /// enabled feature names it.
    pub optional: bool,
}

impl Default for DependencyFeatures {
    fn default() -> Self {
        Self {
            features: Vec::new(),
            default_features: true,
            optional: false,
        }
    }
}

/// `[native]` table: the C and assembly sources a package carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeSpec {
    /// `native.sources` - `.c`, `.s`, and `.S` files, relative to the
    /// manifest directory.
    pub sources: Vec<String>,
    /// `native.include` - include directories, relative to the manifest
    /// directory.
    pub include: Vec<String>,
    /// `native.defines` - preprocessor definitions; an empty value defines
    /// the name with no value.
    pub defines: BTreeMap<String, String>,
    /// `native.flags` - further compiler flags, passed as written.
    pub flags: Vec<String>,
    /// `native.feature` - the package feature the sources belong to; the
    /// sources compile only when it is enabled.
    pub feature: Option<String>,
}

/// One `[[bin]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinTarget {
    /// `bin.name` - required, used as the artefact filename.
    pub name: String,
    /// `bin.path` - relative to the manifest directory.
    /// Defaults to `src/bin/<name>.gos` when omitted.
    pub path: Option<String>,
}

/// `[lib]` table - optional library target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibTarget {
    /// `lib.name` - defaults to the project id's leaf.
    pub name: Option<String>,
    /// `lib.path` - relative to manifest dir. Defaults to
    /// `src/lib.gos`.
    pub path: Option<String>,
    /// `lib.kind` - the native artifacts `gos build` writes for the
    /// library's `#[export]` functions. Empty for a library only other
    /// Gossamer packages use.
    pub kind: Vec<LibKind>,
}

/// A native artifact a `[lib]` target builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LibKind {
    /// A static archive a C build links: `lib<name>.a`, or `<name>.lib` on
    /// Windows.
    Staticlib,
    /// A shared library a C program links or loads: `lib<name>.so`,
    /// `lib<name>.dylib`, or `<name>.dll`.
    Cdylib,
}

impl LibKind {
    /// The manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staticlib => "staticlib",
            Self::Cdylib => "cdylib",
        }
    }
}

/// `[project]` table contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTable {
    /// `project.id`.
    pub id: ProjectId,
    /// `project.version`.
    pub version: Version,
    /// `project.gossamer-version` - which toolchain this project is
    /// written against. A bare version (or an explicit `=`) names that
    /// toolchain and no other; a `^` version names it as a floor.
    /// Absent when the manifest does not state one.
    pub gossamer_version: Option<VersionReq>,
    /// `project.authors`. Empty when omitted.
    pub authors: Vec<String>,
    /// `project.license`. Empty string when omitted.
    pub license: String,
    /// `project.output` - optional override for the binary `gos
    /// build` writes. Relative paths resolve against the manifest's
    /// directory; absent falls back to the source stem.
    pub output: Option<String>,
    /// `project.entry` - optional explicit entry source, relative to the
    /// manifest directory. Overrides convention-based entry resolution and
    /// designates the file that may carry top-level statements.
    pub entry: Option<String>,
    /// `project.enforce-format` - when true, `gos test` fails on any
    /// source that disagrees with `gos fmt`. Opt-in, so a project decides
    /// once that canonical formatting is part of passing rather than a
    /// separate step someone has to remember.
    pub enforce_format: bool,
    /// `project.comptime-io` - the capability posture compile-time
    /// evaluation runs under, spelled `none`, `confined`, or `full`.
    /// The toolchain resolves it against `--comptime-io` and takes the
    /// more restrictive of the two, so a manifest may tighten the
    /// posture and may never loosen it.
    pub comptime_io: Option<String>,
    /// `project.ffi` - whether the project may declare functions in
    /// `unsafe extern "C"` blocks, in its own sources or in a dependency's.
    /// Absent reads as `true`; `ffi = false` refuses native code the
    /// project does not see.
    pub ffi: bool,
}

/// One entry in `[dependencies]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencySpec {
    /// Bare version literal - registry source by default.
    Registry(VersionReq),
    /// Inline table form: `git`, `path`, or `tarball`.
    Inline(InlineDependency),
}

/// One entry in `[rust-bindings]` - a Rust crate to statically
/// link into the per-project runner / compiled binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustBindingSpec {
    /// `{ path = "..." }` - local Cargo path-dep.
    Path {
        /// Optional informational version range.
        version: Option<VersionReq>,
        /// Path as written in the manifest (relative to the
        /// manifest dir or absolute).
        path: String,
        /// Cargo features.
        features: Vec<String>,
        /// Whether `default-features` is enabled.
        default_features: bool,
    },
    /// `{ git = "..." }` - Cargo git-dep.
    Git {
        /// Optional informational version range.
        version: Option<VersionReq>,
        /// Repository URL.
        url: String,
        /// Optional reference (branch/tag/rev).
        reference: Option<GitRef>,
        /// Cargo features.
        features: Vec<String>,
        /// Whether `default-features` is enabled.
        default_features: bool,
    },
    /// `{ version = "..." }` - crates.io passthrough.
    Crates {
        /// Required version range.
        version: VersionReq,
        /// Cargo features.
        features: Vec<String>,
        /// Whether `default-features` is enabled.
        default_features: bool,
    },
    /// `{ src = "path/to/file.rs", deps = "..." }` - single-file
    /// binding (Phase 3 of rustergo.md). The CLI scaffolds a
    /// per-project Cargo crate around the source file with the
    /// supplied Cargo deps (free-form string of TOML key/value
    /// pairs) and links it like any other path-deps binding.
    Src {
        /// Path to the single Rust source file (relative to the
        /// manifest dir or absolute).
        src: String,
        /// Raw Cargo deps fragment, e.g.
        /// `unic-segment = "0.9"`. Appended verbatim under
        /// the scaffolded crate's `[dependencies]` table.
        deps: String,
    },
}

/// Reference for a `git` rust-binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRef {
    /// `branch = "..."`.
    Branch(String),
    /// `tag = "..."`.
    Tag(String),
    /// `rev = "..."`.
    Rev(String),
}

/// Inline-table dependency variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineDependency {
    /// `{ git = "...", tag = "..." }`.
    Git {
        /// Repository URL.
        url: String,
        /// Tag, branch, or commit reference.
        reference: String,
    },
    /// `{ path = "..." }`.
    Path {
        /// Local filesystem path relative to the manifest.
        path: String,
    },
    /// `{ tarball = "...", sha256 = "..." }`.
    Tarball {
        /// HTTP(S) URL of the archive.
        url: String,
        /// Mandatory sha256 of the archive contents.
        sha256: String,
    },
}

/// Errors returned by [`Manifest::parse`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManifestError {
    /// A required field was missing.
    #[error("missing required field {0}")]
    MissingField(&'static str),
    /// A field had the wrong type (e.g. expected string, found list).
    #[error("expected {expected} for {field}")]
    WrongType {
        /// Field name.
        field: String,
        /// Human-readable expected type.
        expected: &'static str,
    },
    /// A `[lib] kind` the toolchain does not build.
    #[error("unknown lib.kind {0:?}; write \"staticlib\", \"cdylib\", or both")]
    UnknownLibKind(String),
    /// A line could not be parsed.
    #[error("malformed line {line_no}: {line}")]
    Malformed {
        /// One-based line number.
        line_no: u32,
        /// Verbatim text of the offending line.
        line: String,
    },
    /// The project id failed validation.
    #[error("invalid project id: {0}")]
    BadId(#[from] ProjectIdError),
    /// The version literal failed validation.
    #[error("invalid version: {0}")]
    BadVersion(#[from] VersionError),
    /// A `[project]` key the manifest format has renamed.
    #[error("`{old}` is not a project key; write `{new}`")]
    RenamedProjectKey {
        /// Key as written.
        old: &'static str,
        /// Key that replaces it.
        new: &'static str,
    },
    /// The manifest's `gossamer-version` is not a toolchain version.
    #[error(
        "unsupported gossamer-version {0:?}; state a toolchain version matching \
         the release tag, such as \"v0.55.0\" for that toolchain exactly or \
         \"^v0.55.0\" for that one or later"
    )]
    UnsupportedGossamerVersion(String),
    /// The manifest pins a toolchain and this is not it.
    #[error(
        "this project is written against gossamer v{required}; this toolchain is \
         v{running}. Write \"^v{required}\" to accept v{required} or later"
    )]
    GossamerVersionMismatch {
        /// Version the manifest pins.
        required: String,
        /// Version of the running toolchain.
        running: String,
    },
    /// The manifest names a floor above the running toolchain.
    #[error("this project requires gossamer v{required} or later; this toolchain is v{running}")]
    GossamerVersionTooNew {
        /// Lowest version the manifest accepts.
        required: String,
        /// Version of the running toolchain.
        running: String,
    },
    /// An inline dependency table mixed incompatible keys.
    #[error("ambiguous dependency for {0}: pick at most one of git/path/tarball")]
    AmbiguousDependency(String),
    /// A `[rust-bindings]` key violates the Cargo package-name regex.
    #[error("invalid rust-binding name {0:?}: must match [A-Za-z_][A-Za-z0-9_-]*")]
    BadBindingName(String),
    /// `[rust-bindings]` entry named a pre-built archive. A binding is
    /// compiled from source against the toolchain building the program, so
    /// its wire layout is always the toolchain's own; an archive built by
    /// another toolchain has no layout anything could check it against.
    #[error(
        "rust-binding {0}: `prebuilt` archives are not supported; a binding is built from source \
         against the toolchain in use, so name its crate with `path`, `git`, `version`, or `src`"
    )]
    PrebuiltRustBinding(String),
    /// `[rust-bindings]` entry mixed `path`, `git`, and version-only.
    #[error("ambiguous rust-binding for {0}: pick exactly one of path/git/version")]
    AmbiguousRustBinding(String),
    /// A `[dependencies]` git entry carried a `version` range.
    #[error(
        "dependency {0}: a git source is versioned by `tag` / `branch` / `rev`, not by `version`"
    )]
    GitDependencyVersion(String),
    /// `[rust-bindings]` git source mixed branch/tag/rev.
    #[error("rust-binding {0} git source: pick at most one of branch/tag/rev")]
    AmbiguousGitRef(String),
    /// `[rust-bindings]` crates.io entry missing a `version` value.
    #[error("rust-binding {0} from crates.io requires a version")]
    MissingBindingVersion(String),
}

/// Walks parent directories of `start` looking for a `project.toml`.
/// Returns the first match, or `None` if the filesystem root is
/// reached. `start` may be either a directory or a file (in which
/// case its parent is walked).
#[must_use]
pub fn find_manifest(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut cursor: std::path::PathBuf = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        let candidate = cursor.join("project.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        cursor = cursor.parent()?.to_path_buf();
    }
}

impl Manifest {
    /// Parses a `project.toml` document.
    pub fn parse(source: &str) -> Result<Self, ManifestError> {
        if source.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::Malformed {
                line_no: 0,
                line: format!("manifest exceeds {MAX_MANIFEST_BYTES}-byte limit"),
            });
        }
        let document: toml::Value =
            toml::from_str(source).map_err(|e| ManifestError::Malformed {
                line_no: 0,
                line: format!("invalid TOML: {e}"),
            })?;
        let root = document
            .as_table()
            .ok_or_else(|| ManifestError::WrongType {
                field: "project.toml".to_string(),
                expected: "TOML table",
            })?;
        for section in root.keys() {
            if !matches!(
                section.as_str(),
                "project"
                    | "dependencies"
                    | "registries"
                    | "trusted-publishers"
                    | "rust-bindings"
                    | "bin"
                    | "lib"
                    | "native"
                    | "features"
            ) {
                return Err(ManifestError::Malformed {
                    line_no: 0,
                    line: format!("unknown section [{section}]"),
                });
            }
        }

        let project = required_toml_table(root, "project")?;
        let id = ProjectId::parse(required_toml_str(
            project,
            "id",
            "project.id",
            "project.id",
        )?)?;
        let version = Version::parse(required_toml_str(
            project,
            "version",
            "project.version",
            "project.version",
        )?)?;
        // `edition` is Rust's spelling; a Gossamer project states the
        // language version its source is written against instead. A manifest
        // still carrying the old key is named rather than silently ignored.
        if project.contains_key("edition") {
            return Err(ManifestError::RenamedProjectKey {
                old: "edition",
                new: "gossamer-version",
            });
        }
        // A toolchain the project cannot run on is named here rather
        // than failing later on a surface this build does not have.
        // A bare version pins - the project is written against that
        // toolchain and no other - and `^` states a floor.
        let gossamer_version =
            match optional_toml_str(project, "gossamer-version", "project.gossamer-version")? {
                Some(text) => {
                    let required = crate::parse_gossamer_version(&text)
                        .map_err(ManifestError::UnsupportedGossamerVersion)?;
                    let running = crate::toolchain_version();
                    if !required.matches(&running) {
                        return Err(if required.is_exact() {
                            ManifestError::GossamerVersionMismatch {
                                required: required.to_string(),
                                running: running.to_string(),
                            }
                        } else {
                            ManifestError::GossamerVersionTooNew {
                                required: required.version.to_string(),
                                running: running.to_string(),
                            }
                        });
                    }
                    Some(required)
                }
                None => None,
            };
        let authors =
            optional_toml_string_array(project, "authors", "project.authors")?.unwrap_or_default();
        let license = optional_toml_str(project, "license", "project.license")?.unwrap_or_default();
        let output = optional_toml_str(project, "output", "project.output")?;
        let entry = optional_toml_str(project, "entry", "project.entry")?;
        let enforce_format = project
            .get("enforce-format")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        let comptime_io = optional_toml_str(project, "comptime-io", "project.comptime-io")?;
        if let Some(level) = &comptime_io {
            if !matches!(level.as_str(), "none" | "confined" | "full") {
                return Err(ManifestError::Malformed {
                    line_no: 0,
                    line: format!(
                        "project.comptime-io must be one of `none`, `confined`, `full`; found `{level}`"
                    ),
                });
            }
        }
        let ffi = match project.get("ffi") {
            None => true,
            Some(toml::Value::Boolean(allowed)) => *allowed,
            Some(other) => {
                return Err(ManifestError::Malformed {
                    line_no: 0,
                    line: format!("project.ffi must be `true` or `false`; found `{other}`"),
                });
            }
        };
        let mut deps: BTreeMap<String, DependencySpec> = BTreeMap::new();
        let mut dep_modules: BTreeMap<String, String> = BTreeMap::new();
        let mut dep_features: BTreeMap<String, DependencyFeatures> = BTreeMap::new();
        if let Some(table) = optional_toml_table(root, "dependencies")? {
            for (key, value) in table {
                deps.insert(key.clone(), parse_dependency_toml(value, key)?);
                if let Some(module) = parse_dependency_module(value, key)? {
                    dep_modules.insert(key.clone(), module);
                }
                if let Some(chosen) = parse_dependency_features(value, key)? {
                    dep_features.insert(key.clone(), chosen);
                }
            }
        }
        let mut features: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if let Some(table) = optional_toml_table(root, "features")? {
            for key in table.keys() {
                let members = optional_toml_string_array(table, key, &format!("features.{key}"))?
                    .unwrap_or_default();
                features.insert(key.clone(), members);
            }
        }

        let mut registries: BTreeMap<String, String> = BTreeMap::new();
        if let Some(table) = optional_toml_table(root, "registries")? {
            for (key, value) in table {
                let url = toml_value_str(value, &format!("registries.{key}"))?.to_string();
                validate_http_url(&url, &format!("registries.{key}"))?;
                registries.insert(key.clone(), url);
            }
        }

        let mut trusted_publishers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(table) = optional_toml_table(root, "trusted-publishers")? {
            for (key, value) in table {
                ProjectId::parse(key)?;
                let public_key = toml_value_str(value, &format!("trusted-publishers.{key}"))?;
                if public_key.len() != 64
                    || !public_key.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(ManifestError::Malformed {
                        line_no: 0,
                        line: format!("trusted-publishers.{key} must be a 32-byte hex key"),
                    });
                }
                trusted_publishers.insert(key.clone(), public_key.to_ascii_lowercase());
            }
        }

        let mut rust_bindings: BTreeMap<String, RustBindingSpec> = BTreeMap::new();
        if let Some(table) = optional_toml_table(root, "rust-bindings")? {
            for (key, value) in table {
                if !is_valid_binding_name(key) {
                    return Err(ManifestError::BadBindingName(key.clone()));
                }
                rust_bindings.insert(key.clone(), parse_rust_binding_toml(value, key)?);
            }
        }

        let bins_parsed = parse_bins(root.get("bin"))?;
        let lib_parsed = optional_toml_table(root, "lib")?
            .map(|raw| {
                Ok::<_, ManifestError>(LibTarget {
                    name: optional_toml_str(raw, "name", "lib.name")?,
                    path: optional_toml_str(raw, "path", "lib.path")?,
                    kind: parse_lib_kinds(raw.get("kind"))?,
                })
            })
            .transpose()?;

        // Reject duplicate `[[bin]]` names - they would collide
        // at the artefact-filename level.
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for b in &bins_parsed {
            if !seen.insert(b.name.as_str()) {
                return Err(ManifestError::Malformed {
                    line_no: 0,
                    line: format!("duplicate [[bin]] name: {}", b.name),
                });
            }
        }

        Ok(Self {
            project: ProjectTable {
                id,
                version,
                gossamer_version,
                authors,
                license,
                output,
                entry,
                enforce_format,
                comptime_io,
                ffi,
            },
            dependencies: deps,
            dependency_modules: dep_modules,
            dependency_features: dep_features,
            features,
            registries,
            trusted_publishers,
            rust_bindings,
            bins: bins_parsed,
            lib: lib_parsed,
            native: parse_native(root)?,
        })
    }

    /// Returns `true` when the manifest declares any explicit
    /// `[[bin]]` or `[lib]` target. When `false`, the toolchain
    /// falls back to the legacy filesystem convention
    /// (`main.gos` / `lib.gos`) - and emits a deprecation
    /// warning.
    #[must_use]
    pub fn has_explicit_targets(&self) -> bool {
        !self.bins.is_empty() || self.lib.is_some()
    }

    /// SHA-256 of the canonicalised `[rust-bindings]` set, with
    /// path-deps resolved against `manifest_dir`. Used as the cache
    /// key for the per-project runner.
    #[must_use]
    pub fn rust_binding_fingerprint(&self, manifest_dir: &std::path::Path) -> [u8; 32] {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        for (name, spec) in &self.rust_bindings {
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
            for entry in canonical_binding_kv(spec, manifest_dir) {
                hasher.update(entry.as_bytes());
                hasher.update(b"\0");
            }
            hasher.update(b"\x1e");
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&hasher.finalize());
        out
    }

    /// Renders the manifest back to canonical TOML.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("[project]\n");
        out.push_str(&format!("id = {}\n", quoted(self.project.id.as_str())));
        out.push_str(&format!(
            "version = {}\n",
            quoted(&self.project.version.to_string())
        ));
        if let Some(requirement) = &self.project.gossamer_version {
            // Rendered with the `v` inside the requirement's spelling,
            // so `^0.55.0` round-trips as `^v0.55.0` rather than losing
            // its bound.
            let rendered = match requirement.bound {
                crate::version::VersionBound::Exact => format!("v{}", requirement.version),
                crate::version::VersionBound::AtLeast => format!("^v{}", requirement.version),
            };
            out.push_str(&format!("gossamer-version = {}\n", quoted(&rendered)));
        }
        if !self.project.authors.is_empty() {
            out.push_str(&format!(
                "authors = {}\n",
                quoted_list(&self.project.authors)
            ));
        }
        if !self.project.license.is_empty() {
            out.push_str(&format!("license = {}\n", quoted(&self.project.license)));
        }
        if let Some(output) = &self.project.output {
            out.push_str(&format!("output = {}\n", quoted(output)));
        }
        if let Some(entry) = &self.project.entry {
            out.push_str(&format!("entry = {}\n", quoted(entry)));
        }
        if self.project.enforce_format {
            out.push_str("enforce-format = true\n");
        }
        if let Some(level) = &self.project.comptime_io {
            out.push_str(&format!("comptime-io = {}\n", quoted(level)));
        }
        if !self.project.ffi {
            out.push_str("ffi = false\n");
        }
        self.render_source_tables(&mut out);
        self.render_targets(&mut out);
        self.render_build_tables(&mut out);
        out
    }

    /// `[dependencies]`, `[registries]`, `[trusted-publishers]`, and `[rust-bindings]`.
    fn render_source_tables(&self, out: &mut String) {
        if !self.dependencies.is_empty() {
            out.push_str("\n[dependencies]\n");
            for (id, spec) in &self.dependencies {
                let module = self.dependency_modules.get(id).map(String::as_str);
                let chosen = self.dependency_features.get(id);
                out.push_str(&format!(
                    "{} = {}\n",
                    render_table_key(id),
                    render_dependency(spec, module, chosen)
                ));
            }
        }
        if !self.registries.is_empty() {
            out.push_str("\n[registries]\n");
            for (prefix, url) in &self.registries {
                out.push_str(&format!("{} = {}\n", quoted(prefix), quoted(url)));
            }
        }
        if !self.trusted_publishers.is_empty() {
            out.push_str("\n[trusted-publishers]\n");
            for (id, public_key) in &self.trusted_publishers {
                out.push_str(&format!("{} = {}\n", quoted(id), quoted(public_key)));
            }
        }
        if !self.rust_bindings.is_empty() {
            out.push_str("\n[rust-bindings]\n");
            for (name, spec) in &self.rust_bindings {
                out.push_str(&format!("{name} = {}\n", render_rust_binding(spec)));
            }
        }
    }

    /// The `[[bin]]` and `[lib]` targets.
    fn render_targets(&self, out: &mut String) {
        for bin in &self.bins {
            out.push_str(&format!("\n[[bin]]\nname = {}\n", quoted(&bin.name)));
            if let Some(path) = &bin.path {
                out.push_str(&format!("path = {}\n", quoted(path)));
            }
        }
        if let Some(lib) = &self.lib {
            out.push_str("\n[lib]\n");
            if let Some(name) = &lib.name {
                out.push_str(&format!("name = {}\n", quoted(name)));
            }
            if let Some(path) = &lib.path {
                out.push_str(&format!("path = {}\n", quoted(path)));
            }
            if !lib.kind.is_empty() {
                let kinds: Vec<String> = lib.kind.iter().map(|k| quoted(k.as_str())).collect();
                out.push_str(&format!("kind = [{}]\n", kinds.join(", ")));
            }
        }
    }

    /// `[features]` and `[native]`.
    fn render_build_tables(&self, out: &mut String) {
        if !self.features.is_empty() {
            out.push_str("\n[features]\n");
            for (name, members) in &self.features {
                out.push_str(&format!(
                    "{} = {}\n",
                    render_table_key(name),
                    quoted_list(members)
                ));
            }
        }
        if let Some(native) = &self.native {
            out.push_str("\n[native]\n");
            out.push_str(&format!("sources = {}\n", quoted_list(&native.sources)));
            if !native.include.is_empty() {
                out.push_str(&format!("include = {}\n", quoted_list(&native.include)));
            }
            if !native.defines.is_empty() {
                let pairs: Vec<String> = native
                    .defines
                    .iter()
                    .map(|(name, value)| format!("{} = {}", quoted(name), quoted(value)))
                    .collect();
                out.push_str(&format!("defines = {{ {} }}\n", pairs.join(", ")));
            }
            if !native.flags.is_empty() {
                out.push_str(&format!("flags = {}\n", quoted_list(&native.flags)));
            }
            if let Some(feature) = &native.feature {
                out.push_str(&format!("feature = {}\n", quoted(feature)));
            }
        }
    }
}

/// `text` as a TOML basic string, with its quotes and backslashes escaped.
fn quoted(text: &str) -> String {
    toml::Value::String(text.to_string()).to_string()
}

/// `items` as a TOML array of strings.
fn quoted_list(items: &[String]) -> String {
    let listed: Vec<String> = items.iter().map(|item| quoted(item)).collect();
    format!("[{}]", listed.join(", "))
}

fn render_rust_binding(spec: &RustBindingSpec) -> String {
    let mut parts: Vec<String> = Vec::new();
    match spec {
        RustBindingSpec::Path {
            version,
            path,
            features,
            default_features,
        } => {
            if let Some(v) = version {
                parts.push(format!("version = {}", quoted(&v.to_string())));
            }
            parts.push(format!("path = {}", quoted(path)));
            push_features(&mut parts, features, *default_features);
        }
        RustBindingSpec::Git {
            version,
            url,
            reference,
            features,
            default_features,
        } => {
            if let Some(v) = version {
                parts.push(format!("version = {}", quoted(&v.to_string())));
            }
            parts.push(format!("git = {}", quoted(url)));
            if let Some(r) = reference {
                match r {
                    GitRef::Branch(b) => parts.push(format!("branch = {}", quoted(b))),
                    GitRef::Tag(t) => parts.push(format!("tag = {}", quoted(t))),
                    GitRef::Rev(r) => parts.push(format!("rev = {}", quoted(r))),
                }
            }
            push_features(&mut parts, features, *default_features);
        }
        RustBindingSpec::Crates {
            version,
            features,
            default_features,
        } => {
            parts.push(format!("version = {}", quoted(&version.to_string())));
            push_features(&mut parts, features, *default_features);
        }
        RustBindingSpec::Src { src, deps } => {
            parts.push(format!("src = {}", quoted(src)));
            if !deps.is_empty() {
                parts.push(format!("deps = {}", quoted(deps)));
            }
        }
    }
    format!("{{ {} }}", parts.join(", "))
}

fn push_features(parts: &mut Vec<String>, features: &[String], default_features: bool) {
    if !features.is_empty() {
        parts.push(format!("features = {}", quoted_list(features)));
    }
    if !default_features {
        parts.push("default-features = false".to_string());
    }
}

fn canonical_binding_kv(spec: &RustBindingSpec, manifest_dir: &std::path::Path) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    match spec {
        RustBindingSpec::Path {
            version,
            path,
            features,
            default_features,
        } => {
            entries.push("kind=path".to_string());
            if let Some(v) = version {
                entries.push(format!("version={v}"));
            }
            let resolved = resolve_path(manifest_dir, path);
            entries.push(format!("path={}", resolved.display()));
            push_canonical_features(&mut entries, features, *default_features);
        }
        RustBindingSpec::Git {
            version,
            url,
            reference,
            features,
            default_features,
        } => {
            entries.push("kind=git".to_string());
            if let Some(v) = version {
                entries.push(format!("version={v}"));
            }
            entries.push(format!("url={url}"));
            if let Some(r) = reference {
                match r {
                    GitRef::Branch(b) => entries.push(format!("branch={b}")),
                    GitRef::Tag(t) => entries.push(format!("tag={t}")),
                    GitRef::Rev(r) => entries.push(format!("rev={r}")),
                }
            }
            push_canonical_features(&mut entries, features, *default_features);
        }
        RustBindingSpec::Crates {
            version,
            features,
            default_features,
        } => {
            entries.push("kind=crates".to_string());
            entries.push(format!("version={version}"));
            push_canonical_features(&mut entries, features, *default_features);
        }
        RustBindingSpec::Src { src, deps } => {
            entries.push("kind=src".to_string());
            let resolved = resolve_path(manifest_dir, src);
            entries.push(format!("src={}", resolved.display()));
            entries.push(format!("deps={deps}"));
        }
    }
    entries.sort();
    entries
}

fn push_canonical_features(out: &mut Vec<String>, features: &[String], default_features: bool) {
    let mut sorted: Vec<String> = features.to_vec();
    sorted.sort();
    for f in sorted {
        out.push(format!("feature={f}"));
    }
    out.push(format!("default-features={default_features}"));
}

fn resolve_path(base: &std::path::Path, raw: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

fn required_toml_table<'a>(
    root: &'a toml::Table,
    key: &'static str,
) -> Result<&'a toml::Table, ManifestError> {
    root.get(key)
        .ok_or(ManifestError::MissingField(key))?
        .as_table()
        .ok_or_else(|| ManifestError::WrongType {
            field: key.to_string(),
            expected: "table",
        })
}

fn optional_toml_table<'a>(
    root: &'a toml::Table,
    key: &str,
) -> Result<Option<&'a toml::Table>, ManifestError> {
    root.get(key)
        .map(|value| {
            value.as_table().ok_or_else(|| ManifestError::WrongType {
                field: key.to_string(),
                expected: "table",
            })
        })
        .transpose()
}

fn toml_value_str<'a>(value: &'a toml::Value, field: &str) -> Result<&'a str, ManifestError> {
    value.as_str().ok_or_else(|| ManifestError::WrongType {
        field: field.to_string(),
        expected: "string",
    })
}

fn required_toml_str<'a>(
    table: &'a toml::Table,
    key: &'static str,
    missing_field: &'static str,
    field: &str,
) -> Result<&'a str, ManifestError> {
    let value = table
        .get(key)
        .ok_or(ManifestError::MissingField(missing_field))?;
    toml_value_str(value, field)
}

fn optional_toml_str(
    table: &toml::Table,
    key: &str,
    field: &str,
) -> Result<Option<String>, ManifestError> {
    table
        .get(key)
        .map(|value| toml_value_str(value, field).map(str::to_string))
        .transpose()
}

fn optional_toml_string_array(
    table: &toml::Table,
    key: &str,
    field: &str,
) -> Result<Option<Vec<String>>, ManifestError> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(ManifestError::WrongType {
            field: field.to_string(),
            expected: "array of strings",
        });
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| ManifestError::WrongType {
                    field: format!("{field}[{i}]"),
                    expected: "string",
                })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// The `[native]` table, when the manifest has one.
fn parse_native(root: &toml::Table) -> Result<Option<NativeSpec>, ManifestError> {
    let Some(table) = optional_toml_table(root, "native")? else {
        return Ok(None);
    };
    for key in table.keys() {
        if !matches!(
            key.as_str(),
            "sources" | "include" | "defines" | "flags" | "feature"
        ) {
            return Err(ManifestError::Malformed {
                line_no: 0,
                line: format!("unknown key `{key}` in [native]"),
            });
        }
    }
    let sources =
        optional_toml_string_array(table, "sources", "native.sources")?.unwrap_or_default();
    if sources.is_empty() {
        return Err(ManifestError::WrongType {
            field: "native.sources".to_string(),
            expected: "non-empty array of source paths",
        });
    }
    let mut defines = BTreeMap::new();
    if let Some(value) = table.get("defines") {
        let Some(entries) = value.as_table() else {
            return Err(ManifestError::WrongType {
                field: "native.defines".to_string(),
                expected: "table of strings",
            });
        };
        for (name, value) in entries {
            let value = value.as_str().ok_or_else(|| ManifestError::WrongType {
                field: format!("native.defines.{name}"),
                expected: "string",
            })?;
            defines.insert(name.clone(), value.to_string());
        }
    }
    Ok(Some(NativeSpec {
        sources,
        include: optional_toml_string_array(table, "include", "native.include")?
            .unwrap_or_default(),
        defines,
        flags: optional_toml_string_array(table, "flags", "native.flags")?.unwrap_or_default(),
        feature: optional_toml_str(table, "feature", "native.feature")?,
    }))
}

/// `[lib] kind`: one kind as a string or several as an array, each listed
/// once.
fn parse_lib_kinds(value: Option<&toml::Value>) -> Result<Vec<LibKind>, ManifestError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let names: Vec<&str> = match value {
        toml::Value::String(one) => vec![one.as_str()],
        toml::Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                item.as_str().ok_or_else(|| ManifestError::WrongType {
                    field: format!("lib.kind[{i}]"),
                    expected: "string",
                })
            })
            .collect::<Result<_, _>>()?,
        _ => {
            return Err(ManifestError::WrongType {
                field: "lib.kind".to_string(),
                expected: "string or array of strings",
            });
        }
    };
    let mut kinds = Vec::with_capacity(names.len());
    for name in names {
        let kind = match name {
            "staticlib" => LibKind::Staticlib,
            "cdylib" => LibKind::Cdylib,
            other => return Err(ManifestError::UnknownLibKind(other.to_string())),
        };
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    Ok(kinds)
}

fn parse_bins(value: Option<&toml::Value>) -> Result<Vec<BinTarget>, ManifestError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(ManifestError::WrongType {
            field: "bin".to_string(),
            expected: "array of tables",
        });
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let table = item.as_table().ok_or_else(|| ManifestError::WrongType {
                field: format!("bin[{i}]"),
                expected: "table",
            })?;
            let name = required_toml_str(table, "name", "bin.name", "bin.name")?.to_string();
            let path = optional_toml_str(table, "path", "bin.path")?;
            Ok(BinTarget { name, path })
        })
        .collect()
}

/// A TOML table key as written: bare when it spells an identifier, quoted
/// otherwise. A dependency keyed by its module name reads back the way the
/// source imports it.
fn render_table_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !key.chars().next().is_some_and(|c| c.is_ascii_digit());
    if bare { key.to_string() } else { quoted(key) }
}

fn parse_dependency_toml(value: &toml::Value, key: &str) -> Result<DependencySpec, ManifestError> {
    if let Some(literal) = value.as_str() {
        return Ok(DependencySpec::Registry(VersionReq::parse(literal)?));
    }
    let Some(table) = value.as_table() else {
        return Err(ManifestError::WrongType {
            field: key.to_string(),
            expected: "string version literal or inline-table dependency",
        });
    };
    let git_url = optional_toml_str(table, "git", &format!("{key}.git"))?;
    let path = optional_toml_str(table, "path", &format!("{key}.path"))?;
    let tarball = optional_toml_str(table, "tarball", &format!("{key}.tarball"))?;
    let active = [git_url.is_some(), path.is_some(), tarball.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if active != 1 {
        return Err(ManifestError::AmbiguousDependency(key.to_string()));
    }
    if let Some(url) = git_url {
        validate_url_with_scheme(&url, &format!("{key}.git"), &["https", "ssh"])?;
        // A git source is versioned by the reference it is checked out at,
        // so a caret range beside it has nothing to resolve against; leaving
        // it unread would pin whatever the default branch happens to hold.
        if optional_toml_str(table, "version", &format!("{key}.version"))?.is_some() {
            return Err(ManifestError::GitDependencyVersion(key.to_string()));
        }
        let git_ref = ["tag", "branch", "rev"]
            .iter()
            .find_map(|field| {
                optional_toml_str(table, field, &format!("{key}.{field}")).transpose()
            })
            .transpose()?
            .unwrap_or_else(|| "main".to_string());
        return Ok(DependencySpec::Inline(InlineDependency::Git {
            url,
            reference: git_ref,
        }));
    }
    if let Some(path) = path {
        return Ok(DependencySpec::Inline(InlineDependency::Path { path }));
    }
    let url = tarball.expect("active tarball case checked above");
    validate_http_url(&url, &format!("{key}.tarball"))?;
    let sha256 = optional_toml_str(table, "sha256", &format!("{key}.sha256"))?.ok_or(
        ManifestError::WrongType {
            field: format!("{key}.sha256"),
            expected: "string (mandatory for tarball)",
        },
    )?;
    Ok(DependencySpec::Inline(InlineDependency::Tarball {
        url,
        sha256,
    }))
}

/// Reads a dependency's `features`, `default-features`, and `optional`
/// keys, or `None` when it sets none of them.
fn parse_dependency_features(
    value: &toml::Value,
    key: &str,
) -> Result<Option<DependencyFeatures>, ManifestError> {
    let Some(table) = value.as_table() else {
        return Ok(None);
    };
    let flag = |name: &str, default: bool| -> Result<bool, ManifestError> {
        match table.get(name) {
            None => Ok(default),
            Some(value) => value.as_bool().ok_or_else(|| ManifestError::WrongType {
                field: format!("{key}.{name}"),
                expected: "boolean",
            }),
        }
    };
    let features = optional_toml_string_array(table, "features", &format!("{key}.features"))?;
    let default_features = flag("default-features", true)?;
    let optional = flag("optional", false)?;
    if features.is_none() && default_features && !optional {
        return Ok(None);
    }
    Ok(Some(DependencyFeatures {
        features: features.unwrap_or_default(),
        default_features,
        optional,
    }))
}

/// Reads a dependency's `module = "..."` override: the name its source is
/// reached under, in place of the one derived from its id. Two packages
/// whose ids share a final segment need one of these to coexist.
fn parse_dependency_module(
    value: &toml::Value,
    key: &str,
) -> Result<Option<String>, ManifestError> {
    let Some(table) = value.as_table() else {
        return Ok(None);
    };
    let Some(module) = optional_toml_str(table, "module", &format!("{key}.module"))? else {
        return Ok(None);
    };
    let valid = !module.is_empty()
        && module
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && module
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(ManifestError::WrongType {
            field: format!("{key}.module"),
            expected: "an identifier (letters, digits, and `_`, not starting with a digit)",
        });
    }
    Ok(Some(module))
}

fn parse_rust_binding_toml(
    value: &toml::Value,
    key: &str,
) -> Result<RustBindingSpec, ManifestError> {
    let Some(table) = value.as_table() else {
        return Err(ManifestError::WrongType {
            field: format!("rust-bindings.{key}"),
            expected: "inline table",
        });
    };
    let version = optional_toml_str(table, "version", &format!("rust-bindings.{key}.version"))?
        .map(|v| VersionReq::parse(&v))
        .transpose()?;
    let path = optional_toml_str(table, "path", &format!("rust-bindings.{key}.path"))?;
    let git = optional_toml_str(table, "git", &format!("rust-bindings.{key}.git"))?;
    let src = optional_toml_str(table, "src", &format!("rust-bindings.{key}.src"))?;
    if table.contains_key("prebuilt") {
        return Err(ManifestError::PrebuiltRustBinding(key.to_string()));
    }
    let active = [path.is_some(), git.is_some(), src.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if active > 1 {
        return Err(ManifestError::AmbiguousRustBinding(key.to_string()));
    }
    let features =
        optional_toml_string_array(table, "features", &format!("rust-bindings.{key}.features"))?
            .unwrap_or_default();
    let default_features = table
        .get("default-features")
        .or_else(|| table.get("default_features"))
        .map(|value| {
            value.as_bool().ok_or_else(|| ManifestError::WrongType {
                field: format!("rust-bindings.{key}.default-features"),
                expected: "boolean",
            })
        })
        .transpose()?
        .unwrap_or(true);
    let branch = optional_toml_str(table, "branch", &format!("rust-bindings.{key}.branch"))?;
    let tag = optional_toml_str(table, "tag", &format!("rust-bindings.{key}.tag"))?;
    let rev = optional_toml_str(table, "rev", &format!("rust-bindings.{key}.rev"))?;
    let git_ref_count = [branch.is_some(), tag.is_some(), rev.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if git_ref_count > 1 {
        return Err(ManifestError::AmbiguousGitRef(key.to_string()));
    }
    if let Some(src) = src {
        let deps = optional_toml_str(table, "deps", &format!("rust-bindings.{key}.deps"))?
            .unwrap_or_default();
        return Ok(RustBindingSpec::Src { src, deps });
    }
    if let Some(path) = path {
        return Ok(RustBindingSpec::Path {
            version,
            path,
            features,
            default_features,
        });
    }
    if let Some(url) = git {
        validate_url_with_scheme(&url, &format!("rust-bindings.{key}.git"), &["https", "ssh"])?;
        let reference = if let Some(b) = branch {
            Some(GitRef::Branch(b))
        } else if let Some(t) = tag {
            Some(GitRef::Tag(t))
        } else {
            rev.map(GitRef::Rev)
        };
        return Ok(RustBindingSpec::Git {
            version,
            url,
            reference,
            features,
            default_features,
        });
    }
    let version = version.ok_or_else(|| ManifestError::MissingBindingVersion(key.to_string()))?;
    Ok(RustBindingSpec::Crates {
        version,
        features,
        default_features,
    })
}

fn validate_http_url(url: &str, field: &str) -> Result<(), ManifestError> {
    validate_url_with_scheme(url, field, &["http", "https"])
}

fn validate_url_with_scheme(url: &str, field: &str, schemes: &[&str]) -> Result<(), ManifestError> {
    let parsed = url::Url::parse(url).map_err(|e| ManifestError::Malformed {
        line_no: 0,
        line: format!("{field} is not a valid URL: {e}"),
    })?;
    if !schemes.iter().any(|scheme| *scheme == parsed.scheme()) || parsed.host_str().is_none() {
        return Err(ManifestError::Malformed {
            line_no: 0,
            line: format!("{field} must be an absolute {} URL", schemes.join("/")),
        });
    }
    Ok(())
}

fn is_valid_binding_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn render_dependency(
    spec: &DependencySpec,
    module: Option<&str>,
    chosen: Option<&DependencyFeatures>,
) -> String {
    let mut parts = match spec {
        DependencySpec::Registry(requirement) => return quoted(&requirement.to_string()),
        DependencySpec::Inline(InlineDependency::Git { url, reference }) => {
            vec![
                format!("git = {}", quoted(url)),
                format!("tag = {}", quoted(reference)),
            ]
        }
        DependencySpec::Inline(InlineDependency::Path { path }) => {
            vec![format!("path = {}", quoted(path))]
        }
        DependencySpec::Inline(InlineDependency::Tarball { url, sha256 }) => {
            vec![
                format!("tarball = {}", quoted(url)),
                format!("sha256 = {}", quoted(sha256)),
            ]
        }
    };
    if let Some(module) = module {
        parts.push(format!("module = {}", quoted(module)));
    }
    if let Some(chosen) = chosen {
        if !chosen.features.is_empty() {
            parts.push(format!("features = {}", quoted_list(&chosen.features)));
        }
        if !chosen.default_features {
            parts.push("default-features = false".to_string());
        }
        if chosen.optional {
            parts.push("optional = true".to_string());
        }
    }
    format!("{{ {} }}", parts.join(", "))
}

#[cfg(test)]
mod entry_field_tests {
    use super::*;

    #[test]
    fn parses_optional_entry_field() {
        let src =
            "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\nentry = \"src/app.gos\"\n";
        let m = Manifest::parse(src).unwrap();
        assert_eq!(m.project.entry.as_deref(), Some("src/app.gos"));
    }

    #[test]
    fn entry_absent_is_none() {
        let src = "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n";
        let m = Manifest::parse(src).unwrap();
        assert_eq!(m.project.entry, None);
    }
}

#[cfg(test)]
mod gossamer_version_tests {
    use super::*;

    fn manifest_with(gossamer_version: &str) -> Result<Manifest, ManifestError> {
        Manifest::parse(&format!(
            "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n\
             gossamer-version = \"{gossamer_version}\"\n",
        ))
    }

    #[test]
    fn an_absent_gossamer_version_is_none_and_a_pin_round_trips() {
        let bare =
            Manifest::parse("[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n").unwrap();
        assert_eq!(bare.project.gossamer_version, None);

        let running = crate::toolchain_version();
        let stated = manifest_with(&format!("v{running}")).unwrap();
        assert_eq!(
            stated.project.gossamer_version,
            Some(crate::VersionReq::exact(running))
        );
        assert_eq!(Manifest::parse(&stated.render()).unwrap(), stated);
    }

    /// The `v` the release tag carries is optional in the manifest.
    #[test]
    fn a_bare_version_spelling_parses_to_the_same_value() {
        let running = crate::toolchain_version();
        let stated = manifest_with(&running.to_string()).unwrap();
        assert_eq!(
            stated.project.gossamer_version,
            Some(crate::VersionReq::exact(running))
        );
    }

    /// A pin names one toolchain. A project written against an older
    /// one is refused by name rather than compiled against a surface it
    /// was never checked on.
    #[test]
    fn a_pin_refuses_a_toolchain_that_is_not_the_one_it_names() {
        let running = crate::toolchain_version();
        let older = format!("v{}.{}.{}", running.major, running.minor, running.patch + 1);
        let error = manifest_with(&older).unwrap_err();
        assert!(
            matches!(error, ManifestError::GossamerVersionMismatch { .. }),
            "{error:?}"
        );

        let earlier = if running.patch > 0 {
            format!("v{}.{}.{}", running.major, running.minor, running.patch - 1)
        } else {
            return;
        };
        let error = manifest_with(&earlier).unwrap_err();
        assert!(
            matches!(error, ManifestError::GossamerVersionMismatch { .. }),
            "{error:?}"
        );
    }

    /// A `^` floor accepts this toolchain and every later one, which is
    /// what a project that wants to keep working across releases writes.
    #[test]
    fn a_caret_floor_accepts_this_toolchain_and_anything_later() {
        let running = crate::toolchain_version();
        let stated = manifest_with(&format!("^v{running}")).unwrap();
        assert_eq!(
            stated.project.gossamer_version,
            Some(crate::VersionReq::at_least(running.clone()))
        );
        assert_eq!(Manifest::parse(&stated.render()).unwrap(), stated);

        if running.patch > 0 {
            let earlier = format!(
                "^v{}.{}.{}",
                running.major,
                running.minor,
                running.patch - 1
            );
            assert!(manifest_with(&earlier).is_ok());
        }
    }

    #[test]
    fn an_edition_year_is_no_longer_a_gossamer_version() {
        let error = Manifest::parse(
            "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\ngossamer-version = \"2026\"\n",
        )
        .unwrap_err();
        assert!(
            matches!(error, ManifestError::UnsupportedGossamerVersion(value) if value == "2026")
        );
    }

    #[test]
    fn a_floor_above_this_toolchain_is_named() {
        let running = crate::toolchain_version();
        let error = manifest_with(&format!("^v{}.0.0", running.major + 1)).unwrap_err();
        assert!(
            matches!(error, ManifestError::GossamerVersionTooNew { .. }),
            "{error:?}"
        );
    }

    /// `edition` is Rust's spelling, so a manifest still carrying it is named
    /// rather than read as a project with no language version at all.
    #[test]
    fn the_rust_edition_key_is_rejected_by_name() {
        let error = Manifest::parse(
            "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\nedition = \"2027\"\n",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ManifestError::RenamedProjectKey {
                old: "edition",
                new: "gossamer-version"
            }
        ));
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    const FULL: &str = r#"[project]
id = "example.com/app"
version = "0.1.0"
gossamer-version = "^v0.55.0"
authors = ["Ada \"A\" Lovelace <ada@example.com>"]
license = "MIT"
output = "bin\\app"
entry = "src/app.gos"
enforce-format = true
comptime-io = "confined"
ffi = true

[dependencies]
"example.com/near" = { path = "..\\near", module = "near_lib" }
"example.org/far" = "^1.2.3"
"example.org/pinned" = { git = "https://git.example.org/pinned.git", tag = "v2.0.0" }

[registries]
"example.org" = "https://registry.example.org/v1"

[rust-bindings]
native = { path = "native", features = ["fast"], default-features = false }

[[bin]]
name = "app"
path = "src/app.gos"

[[bin]]
name = "helper"

[lib]
name = "applib"
path = "src/lib.gos"
"#;

    #[test]
    fn rendering_a_manifest_keeps_every_key_it_parsed() {
        let parsed = Manifest::parse(FULL).unwrap();
        assert_eq!(Manifest::parse(&parsed.render()).unwrap(), parsed);
    }

    #[test]
    fn ffi_reads_as_true_unless_stated() {
        let base = "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n";
        assert!(Manifest::parse(base).unwrap().project.ffi);
        let allowed = format!("{base}ffi = true\n");
        assert!(Manifest::parse(&allowed).unwrap().project.ffi);
        let denied = format!("{base}ffi = false\n");
        assert!(!Manifest::parse(&denied).unwrap().project.ffi);
    }

    #[test]
    fn ffi_must_be_a_boolean() {
        let text = "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n\
                    ffi = \"yes\"\n";
        assert!(Manifest::parse(text).is_err());
    }
}
