//! Dependency resolver.
//!
//! For each `(id, range)` consumer requirement, the resolver picks
//! the *highest* registry version that satisfies every range the
//! graph imposes. Two consumers requiring incompatible ranges (no
//! version satisfies their intersection) raise
//! [`ResolveError::IncompatibleVersions`]. Inline (git / path /
//! tarball) pins pass through unchanged; two inline pins for the
//! same id that disagree raise [`ResolveError::ConflictingPins`].
//!
//! The resolver is transitive - after picking a concrete version for
//! a direct dep, the dep's own `project.toml` (read out of the
//! cached source tree) is parsed and its dependencies are pushed
//! onto the work queue. A visited set keyed on `(id, source-pin)`
//! breaks cycles.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value as JsonValue;
use thiserror::Error;

use crate::cache::{Cache, CachedSource};
use crate::id::ProjectId;
use crate::manifest::{DependencySpec, InlineDependency, Manifest};
use crate::transport::{Transport, TransportError};
use crate::version::{Version, VersionReq};

const MAX_REGISTRY_INDEX_BYTES: usize = 1024 * 1024;
const MAX_REGISTRY_VERSIONS: usize = 16_384;

/// One published version of a project as advertised by the registry
/// index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogueEntry {
    /// Concrete version.
    pub version: Version,
    /// Whether this version is yanked.
    pub yanked: bool,
    /// Optional download URL - required for tarball fetches.
    pub download_url: Option<String>,
    /// SHA-256 of the on-the-wire tarball, if known.
    pub tarball_sha256: Option<String>,
    /// Optional yank reason for surfacing to users.
    pub yank_reason: Option<String>,
    /// Hex-encoded ed25519 signature over the tarball's lowercase SHA-256
    /// hex digest, as advertised by the registry index. Required for a registry fetch
    /// to be admitted.
    pub signature: Option<String>,
    /// Hex-encoded ed25519 public key of the publisher.
    pub public_key: Option<String>,
}

/// Catalogue of every version known for a project. Tests inject a
/// catalogue directly; the production resolver populates it from
/// [`VersionCatalogue::from_registry`].
#[derive(Debug, Clone, Default)]
pub struct VersionCatalogue {
    entries: BTreeMap<String, Vec<CatalogueEntry>>,
}

impl VersionCatalogue {
    /// Returns an empty catalogue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `id` is available at `version`. Convenience for
    /// tests; production fetches go through [`Self::add_entry`].
    pub fn add(&mut self, id: &ProjectId, version: Version) {
        self.add_entry(
            id,
            CatalogueEntry {
                version,
                yanked: false,
                download_url: None,
                tarball_sha256: None,
                yank_reason: None,
                signature: None,
                public_key: None,
            },
        );
    }

    /// Records a full `CatalogueEntry`.
    pub fn add_entry(&mut self, id: &ProjectId, entry: CatalogueEntry) {
        let bucket = self.entries.entry(id.as_str().to_string()).or_default();
        if !bucket.iter().any(|e| e.version == entry.version) {
            bucket.push(entry);
            bucket.sort_by_key(|e| e.version.clone());
        }
    }

    /// Returns every recorded version for `id`.
    #[must_use]
    pub fn versions(&self, id: &ProjectId) -> Vec<Version> {
        self.entries
            .get(id.as_str())
            .map(|v| v.iter().map(|e| e.version.clone()).collect())
            .unwrap_or_default()
    }

    /// Returns every recorded entry for `id`.
    #[must_use]
    pub fn entries(&self, id: &ProjectId) -> &[CatalogueEntry] {
        self.entries
            .get(id.as_str())
            .map_or(&[] as &[CatalogueEntry], Vec::as_slice)
    }

    /// Returns the catalogue entry for `id @ version`, if known.
    #[must_use]
    pub fn entry(&self, id: &ProjectId, version: &Version) -> Option<&CatalogueEntry> {
        self.entries(id).iter().find(|e| &e.version == version)
    }

    /// Fetches the registry index for `id` from `registry_url` via
    /// `transport` and folds every advertised version into the
    /// catalogue. The index document lives at
    /// `<registry_url>/v1/index/<id>.json`. Returns `Ok(false)` when
    /// no entries were added and `Ok(true)` otherwise.
    pub fn load_from_registry(
        &mut self,
        transport: &dyn Transport,
        registry_url: &str,
        id: &ProjectId,
    ) -> Result<bool, TransportError> {
        let url = format!(
            "{base}/v1/index/{id}.json",
            base = registry_url.trim_end_matches('/'),
            id = id.as_str(),
        );
        let body = transport.get(&url)?;
        let added = parse_index_json(&body, id, self)
            .map_err(|e| TransportError::Io(format!("index parse: {e}")))?;
        Ok(added)
    }

    /// Constructs a catalogue pre-populated by walking every dep in
    /// `ids` via `transport` against `registry_url`.
    pub fn from_registry(
        transport: &dyn Transport,
        registry_url: &str,
        ids: &[ProjectId],
    ) -> Result<Self, TransportError> {
        let mut out = Self::new();
        for id in ids {
            let _ = out.load_from_registry(transport, registry_url, id)?;
        }
        Ok(out)
    }
}

fn parse_index_json(
    bytes: &[u8],
    id: &ProjectId,
    catalogue: &mut VersionCatalogue,
) -> Result<bool, String> {
    if bytes.len() > MAX_REGISTRY_INDEX_BYTES {
        return Err(format!(
            "index exceeds {MAX_REGISTRY_INDEX_BYTES}-byte limit"
        ));
    }
    let document: JsonValue =
        serde_json::from_slice(bytes).map_err(|e| format!("malformed JSON: {e}"))?;
    let versions_array = document
        .as_object()
        .and_then(|object| object.get("versions"))
        .and_then(JsonValue::as_array)
        .ok_or("missing `versions` array")?;
    if versions_array.len() > MAX_REGISTRY_VERSIONS {
        return Err(format!(
            "index has more than {MAX_REGISTRY_VERSIONS} versions"
        ));
    }
    let mut added = false;
    for (index, entry) in versions_array.iter().enumerate() {
        let object = entry
            .as_object()
            .ok_or_else(|| format!("versions[{index}] is not an object"))?;
        let version_text = json_required_string(object, "version", index)?;
        let version = Version::parse(version_text).map_err(|e| format!("version: {e}"))?;
        let yanked = match object.get("yanked") {
            Some(value) => value
                .as_bool()
                .ok_or_else(|| format!("versions[{index}].yanked is not a boolean"))?,
            None => false,
        };
        let download_url = json_optional_string(object, "url", index)?;
        let tarball_sha256 = json_optional_string(object, "sha256", index)?;
        let yank_reason = json_optional_string(object, "yank_reason", index)?;
        let signature = json_optional_string(object, "signature", index)?;
        let public_key = json_optional_string(object, "public_key", index)?;
        catalogue.add_entry(
            id,
            CatalogueEntry {
                version,
                yanked,
                download_url,
                tarball_sha256,
                yank_reason,
                signature,
                public_key,
            },
        );
        added = true;
    }
    Ok(added)
}

fn json_required_string<'a>(
    object: &'a serde_json::Map<String, JsonValue>,
    field: &str,
    index: usize,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| format!("versions[{index}].{field} is missing or not a string"))
}

fn json_optional_string(
    object: &serde_json::Map<String, JsonValue>,
    field: &str,
    index: usize,
) -> Result<Option<String>, String> {
    match object.get(field) {
        Some(value) => value
            .as_str()
            .map(str::to_string)
            .map(Some)
            .ok_or_else(|| format!("versions[{index}].{field} is not a string")),
        None => Ok(None),
    }
}

#[cfg(test)]
mod index_tests {
    use super::*;

    #[test]
    fn registry_index_uses_json_decoding_and_populates_catalogue() {
        let id = ProjectId::parse("example.com/widget").unwrap();
        let mut catalogue = VersionCatalogue::new();
        let index = br#"{
            "versions": [{
                "version": "1.2.3",
                "url": "https://registry.example/packages/widget%2D1.2.3.tar",
                "sha256": "abc",
                "yanked": false,
                "yank_reason": "no \"longer\" supported"
            }]
        }"#;

        assert!(parse_index_json(index, &id, &mut catalogue).unwrap());
        let entry = catalogue
            .entry(&id, &Version::parse("1.2.3").unwrap())
            .unwrap();
        assert_eq!(
            entry.yank_reason.as_deref(),
            Some("no \"longer\" supported")
        );
        assert_eq!(
            entry.download_url.as_deref(),
            Some("https://registry.example/packages/widget%2D1.2.3.tar")
        );
    }

    #[test]
    fn registry_index_rejects_wrong_field_types_and_oversized_documents() {
        let id = ProjectId::parse("example.com/widget").unwrap();
        let mut catalogue = VersionCatalogue::new();
        assert!(
            parse_index_json(
                br#"{"versions":[{"version":"1.2.3","yanked":"false"}]}"#,
                &id,
                &mut catalogue
            )
            .is_err()
        );

        let oversized = vec![b' '; MAX_REGISTRY_INDEX_BYTES + 1];
        assert!(parse_index_json(&oversized, &id, &mut catalogue).is_err());
    }
}

/// Per-dependency declaration the resolver receives from a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    /// Project being depended on.
    pub id: ProjectId,
    /// Source kind. Inline (git/path/tarball) declarations are
    /// surfaced unchanged.
    pub spec: RequirementSpec,
}

/// Distilled form of [`DependencySpec`] for the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementSpec {
    /// Versioned registry dependency.
    Range(VersionReq),
    /// Pinned non-registry source.
    Inline(InlineDependency),
}

impl Requirement {
    /// Builds a requirement from a [`DependencySpec`].
    #[must_use]
    pub fn from_spec(id: ProjectId, spec: &DependencySpec) -> Self {
        let spec = match spec {
            DependencySpec::Registry(range) => RequirementSpec::Range(range.clone()),
            DependencySpec::Inline(inline) => RequirementSpec::Inline(inline.clone()),
        };
        Self { id, spec }
    }
}

/// One row in the resolved dependency graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Project being resolved.
    pub id: ProjectId,
    /// Concrete pin.
    pub pin: ResolvedSource,
}

/// Concrete source pin produced by the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedSource {
    /// Registry version pin.
    Registry(Version),
    /// Git checkout pin.
    Git {
        /// Repository URL.
        url: String,
        /// Reference (tag/branch/rev).
        reference: String,
    },
    /// Local path pin.
    Path(String),
    /// Tarball pin.
    Tarball {
        /// Archive URL.
        url: String,
        /// sha256 of the archive.
        sha256: String,
    },
}

/// Resolution failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResolveError {
    /// No version in the catalogue satisfies the union of requested
    /// ranges.
    #[error("no version of {id} satisfies the requested ranges")]
    Unsatisfiable {
        /// Project being resolved.
        id: String,
    },
    /// Two non-registry pins for the same project disagree.
    #[error("conflicting non-registry pins for {id}")]
    ConflictingPins {
        /// Project being resolved.
        id: String,
    },
    /// Two consumers asked for ranges that have no version in common.
    #[error("incompatible versions for {id}: {detail}")]
    IncompatibleVersions {
        /// Project being resolved.
        id: String,
        /// Human-readable summary of the conflict.
        detail: String,
    },
}

/// Resolver entry point.
#[derive(Debug, Default)]
pub struct Resolver {
    catalogue: VersionCatalogue,
    root: Option<PathBuf>,
}

impl Resolver {
    /// Returns a resolver backed by `catalogue`.
    #[must_use]
    pub fn new(catalogue: VersionCatalogue) -> Self {
        Self {
            catalogue,
            root: None,
        }
    }

    /// Anchors relative `path` dependencies on the directory holding the
    /// manifest being resolved, which is what lets one state its identity
    /// in its own `project.toml` rather than in the consumer's key.
    #[must_use]
    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Resolves the direct dependencies listed in `manifest`. Picks
    /// the *highest* version satisfying every consumer's range.
    pub fn resolve(&self, manifest: &Manifest) -> Result<Vec<Resolved>, ResolveError> {
        let mut requirements: BTreeMap<String, (ProjectId, Vec<RequirementSpec>)> = BTreeMap::new();
        for (raw_id, spec) in &manifest.dependencies {
            let id = dependency_identity(raw_id, spec, self.root.as_deref())?;
            let req = Requirement::from_spec(id.clone(), spec);
            let entry = requirements
                .entry(raw_id.clone())
                .or_insert_with(|| (id.clone(), Vec::new()));
            entry.1.push(req.spec);
        }
        let mut resolved = Vec::with_capacity(requirements.len());
        for (raw_id, (id, specs)) in requirements {
            resolved.push(self.resolve_one(&raw_id, &id, &specs)?);
        }
        resolved.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(resolved)
    }

    fn resolve_one(
        &self,
        raw_id: &str,
        id: &ProjectId,
        specs: &[RequirementSpec],
    ) -> Result<Resolved, ResolveError> {
        let inline_pins: Vec<&InlineDependency> = specs
            .iter()
            .filter_map(|s| match s {
                RequirementSpec::Inline(inline) => Some(inline),
                RequirementSpec::Range(_) => None,
            })
            .collect();
        if !inline_pins.is_empty() {
            if inline_pins.iter().any(|p| !inline_eq(p, inline_pins[0])) {
                return Err(ResolveError::ConflictingPins {
                    id: raw_id.to_string(),
                });
            }
            let pin = inline_pin_to_resolved(inline_pins[0]);
            return Ok(Resolved {
                id: id.clone(),
                pin,
            });
        }
        let ranges: Vec<&VersionReq> = specs
            .iter()
            .filter_map(|s| match s {
                RequirementSpec::Range(r) => Some(r),
                RequirementSpec::Inline(_) => None,
            })
            .collect();
        let candidates = self.catalogue.versions(id);
        // Highest matching wins. Iterate in reverse to pick the
        // highest version that satisfies every consumer's range.
        for version in candidates.iter().rev() {
            if ranges.iter().all(|r| r.matches(version)) {
                if let Some(entry) = self.catalogue.entry(id, version)
                    && entry.yanked
                {
                    continue;
                }
                return Ok(Resolved {
                    id: id.clone(),
                    pin: ResolvedSource::Registry(version.clone()),
                });
            }
        }
        if candidates.is_empty() {
            return Err(ResolveError::Unsatisfiable {
                id: raw_id.to_string(),
            });
        }
        let detail = format!(
            "tried versions [{}], requirements [{}]",
            candidates
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            ranges
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        );
        Err(ResolveError::IncompatibleVersions {
            id: raw_id.to_string(),
            detail,
        })
    }
}

/// The project identity of one `[dependencies]` entry, with relative
/// `path` sources anchored on `base` - the directory holding the manifest
/// the entry was written in.
///
/// The key is the identity when it spells one, which is how a registry
/// dependency is written. A git dependency carries its identity in the URL
/// instead, and a path dependency in the `project.toml` it points at, so
/// either one's key is free to be the module name source reaches it by -
/// `pgsql_gos = { git = "https://github.com/gossamer-lang/pgsql-gos" }` and
/// `pgsql_gos = { path = "../.." }` are both the same package as
/// `"github.com/gossamer-lang/pgsql-gos"`.
///
/// # Errors
///
/// Returns [`ResolveError::Unsatisfiable`] when neither the key nor the
/// source names a project identity.
pub fn dependency_identity(
    key: &str,
    spec: &DependencySpec,
    base: Option<&Path>,
) -> Result<ProjectId, ResolveError> {
    if let Ok(id) = ProjectId::parse(key) {
        return Ok(id);
    }
    match spec {
        DependencySpec::Inline(InlineDependency::Git { url, .. }) => {
            if let Some(id) = git_url_identity(url) {
                return Ok(id);
            }
        }
        DependencySpec::Inline(InlineDependency::Path { path }) => {
            if let Some(base) = base
                && let Some(id) = path_dependency_identity(&base.join(path))
            {
                return Ok(id);
            }
        }
        _ => {}
    }
    Err(ResolveError::Unsatisfiable {
        id: key.to_string(),
    })
}

/// The identity a path dependency declares in its own manifest.
fn path_dependency_identity(root: &Path) -> Option<ProjectId> {
    let text = std::fs::read_to_string(root.join("project.toml")).ok()?;
    Manifest::parse(&text).ok().map(|m| m.project.id)
}

/// The project identity a git URL names: its host and repository path, with
/// the scheme, any userinfo, and a trailing `.git` removed.
fn git_url_identity(url: &str) -> Option<ProjectId> {
    let rest = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .trim_end_matches('/');
    let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    ProjectId::parse(rest).ok()
}

/// Tooling to resolve a dependency graph transitively.
pub trait TransitiveLoader {
    /// Returns the manifest of the dependency rooted at `resolved`,
    /// or `Ok(None)` when no manifest is reachable.
    fn load(&self, resolved: &Resolved) -> Result<Option<Manifest>, ResolveError>;
}

/// Walks the dependency graph rooted at `root`, returning every
/// `(id, pin)` reachable from the root. Cycles terminate via a
/// visited set keyed on `(id, pin)`. `base` is the directory holding
/// `root`'s manifest, which anchors the relative `path` dependencies
/// written in it.
pub fn resolve_transitive(
    root: &Manifest,
    base: Option<&Path>,
    catalogue: &VersionCatalogue,
    loader: &dyn TransitiveLoader,
) -> Result<Vec<Resolved>, ResolveError> {
    let mut ranges: BTreeMap<String, Vec<VersionReq>> = BTreeMap::new();
    let mut inlines: BTreeMap<String, Vec<InlineDependency>> = BTreeMap::new();
    let mut id_index: BTreeMap<String, ProjectId> = BTreeMap::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut work: Vec<Manifest> = vec![root.clone()];
    while let Some(m) = work.pop() {
        for (raw_id, spec) in &m.dependencies {
            let id = dependency_identity(raw_id, spec, base)?;
            id_index.entry(raw_id.clone()).or_insert(id.clone());
            match spec {
                DependencySpec::Registry(range) => {
                    ranges
                        .entry(raw_id.clone())
                        .or_default()
                        .push(range.clone());
                }
                DependencySpec::Inline(inline) => {
                    inlines
                        .entry(raw_id.clone())
                        .or_default()
                        .push(inline.clone());
                }
            }
        }
        for (raw_id, spec) in &m.dependencies {
            let id = id_index
                .get(raw_id)
                .cloned()
                .ok_or_else(|| ResolveError::Unsatisfiable { id: raw_id.clone() })?;
            let pin = if let DependencySpec::Inline(inline) = spec {
                inline_pin_to_resolved(inline)
            } else {
                let range_set = ranges.get(raw_id).cloned().unwrap_or_default();
                pick_highest(&id, &range_set, catalogue)?
            };
            let key = format!("{raw_id}|{}", debug_pin(&pin));
            if visited.insert(key) {
                let resolved = Resolved {
                    id: id.clone(),
                    pin: pin.clone(),
                };
                if let Some(child) = loader.load(&resolved)? {
                    work.push(child);
                }
            }
        }
    }
    let mut out: Vec<Resolved> = Vec::with_capacity(id_index.len());
    for (raw_id, id) in &id_index {
        if let Some(pin_set) = inlines.get(raw_id) {
            if pin_set.iter().any(|p| !inline_eq(p, &pin_set[0])) {
                return Err(ResolveError::ConflictingPins { id: raw_id.clone() });
            }
            out.push(Resolved {
                id: id.clone(),
                pin: inline_pin_to_resolved(&pin_set[0]),
            });
            continue;
        }
        let range_set = ranges.get(raw_id).cloned().unwrap_or_default();
        let pin = pick_highest(id, &range_set, catalogue)?;
        out.push(Resolved {
            id: id.clone(),
            pin,
        });
    }
    out.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    Ok(out)
}

fn pick_highest(
    id: &ProjectId,
    ranges: &[VersionReq],
    catalogue: &VersionCatalogue,
) -> Result<ResolvedSource, ResolveError> {
    let candidates = catalogue.versions(id);
    if candidates.is_empty() {
        return Err(ResolveError::Unsatisfiable {
            id: id.as_str().to_string(),
        });
    }
    for v in candidates.iter().rev() {
        if ranges.iter().all(|r| r.matches(v)) {
            if let Some(entry) = catalogue.entry(id, v)
                && entry.yanked
            {
                continue;
            }
            return Ok(ResolvedSource::Registry(v.clone()));
        }
    }
    let detail = format!(
        "tried versions [{}], requirements [{}]",
        candidates
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        ranges
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    );
    Err(ResolveError::IncompatibleVersions {
        id: id.as_str().to_string(),
        detail,
    })
}

fn debug_pin(pin: &ResolvedSource) -> String {
    match pin {
        ResolvedSource::Registry(v) => format!("registry/{v}"),
        ResolvedSource::Git { url, reference } => format!("git/{url}@{reference}"),
        ResolvedSource::Path(p) => format!("path/{p}"),
        ResolvedSource::Tarball { url, sha256 } => format!("tarball/{url}#{sha256}"),
    }
}

fn inline_pin_to_resolved(pin: &InlineDependency) -> ResolvedSource {
    match pin {
        InlineDependency::Git { url, reference } => ResolvedSource::Git {
            url: url.clone(),
            reference: reference.clone(),
        },
        InlineDependency::Path { path } => ResolvedSource::Path(path.clone()),
        InlineDependency::Tarball { url, sha256 } => ResolvedSource::Tarball {
            url: url.clone(),
            sha256: sha256.clone(),
        },
    }
}

fn inline_eq(a: &InlineDependency, b: &InlineDependency) -> bool {
    a == b
}

/// Loader that reads `project.toml` out of a [`Cache`] hit. Returns
/// `Ok(None)` when the digest is not in the cache or the cached tree
/// contains no `project.toml`.
pub struct CacheBackedLoader<'a> {
    /// Cache to consult.
    pub cache: &'a Cache,
    /// Map of `(id, pin)` → cached digest so the loader can find
    /// the right tree for a given dep. The caller (the fetch driver)
    /// populates this as each dep is fetched.
    pub digests: BTreeMap<String, String>,
}

impl CacheBackedLoader<'_> {
    /// Returns the key used in `digests` for the given dep.
    #[must_use]
    pub fn key(resolved: &Resolved) -> String {
        format!("{}|{}", resolved.id.as_str(), debug_pin(&resolved.pin))
    }
}

impl TransitiveLoader for CacheBackedLoader<'_> {
    fn load(&self, resolved: &Resolved) -> Result<Option<Manifest>, ResolveError> {
        let key = Self::key(resolved);
        let Some(digest) = self.digests.get(&key) else {
            return Ok(None);
        };
        let Some(src) = lookup_cache(self.cache, digest) else {
            return Ok(None);
        };
        load_manifest_from_source(src)
    }
}

fn lookup_cache<'a>(cache: &'a Cache, digest: &str) -> Option<&'a CachedSource> {
    cache
        .iter()
        .find_map(|(d, src)| (d == digest).then_some(src))
}

fn load_manifest_from_source(src: &CachedSource) -> Result<Option<Manifest>, ResolveError> {
    let Some(toml) = src.files.get("project.toml") else {
        return Ok(None);
    };
    let text = std::str::from_utf8(toml).map_err(|e| ResolveError::Unsatisfiable {
        id: format!("{} (utf-8 in project.toml: {e})", src.id),
    })?;
    let manifest = Manifest::parse(text).map_err(|e| ResolveError::Unsatisfiable {
        id: format!("{} (project.toml parse: {e})", src.id),
    })?;
    Ok(Some(manifest))
}

/// Convenience adaptor - wraps a closure as a [`TransitiveLoader`].
pub struct FnLoader<F>(pub F);

impl<F> TransitiveLoader for FnLoader<F>
where
    F: Fn(&Resolved) -> Result<Option<Manifest>, ResolveError>,
{
    fn load(&self, resolved: &Resolved) -> Result<Option<Manifest>, ResolveError> {
        (self.0)(resolved)
    }
}

/// Empty loader - every dep reports "no manifest". Useful for tests
/// and for the direct-deps-only fast path.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopLoader;

impl TransitiveLoader for NoopLoader {
    fn load(&self, _: &Resolved) -> Result<Option<Manifest>, ResolveError> {
        Ok(None)
    }
}

/// Shared loader handle.
pub type SharedLoader = Arc<dyn TransitiveLoader>;

#[cfg(test)]
mod identity_tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SCRATCH_ID: AtomicU64 = AtomicU64::new(0);

    /// Writes a package rooted at a fresh temp directory, answering both the
    /// directory holding it and the directory the package itself lives in.
    fn package_at(id: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "gossamer-identity-{}-{}",
            std::process::id(),
            SCRATCH_ID.fetch_add(1, Ordering::Relaxed),
        ));
        let root = base.join("driver");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("project.toml"),
            format!("[project]\nid = \"{id}\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
        (base, root)
    }

    fn path_spec(path: &str) -> DependencySpec {
        DependencySpec::Inline(InlineDependency::Path {
            path: path.to_string(),
        })
    }

    #[test]
    fn a_path_dependency_takes_its_identity_from_the_manifest_it_points_at() {
        let (base, _) = package_at("github.com/gossamer-lang/pgsql-gos");
        let id = dependency_identity("pgsql_gos", &path_spec("driver"), Some(&base)).unwrap();
        assert_eq!(id.as_str(), "github.com/gossamer-lang/pgsql-gos");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_key_that_spells_an_identity_outranks_the_manifest_it_points_at() {
        let (base, _) = package_at("github.com/gossamer-lang/pgsql-gos");
        let id =
            dependency_identity("example.com/alias", &path_spec("driver"), Some(&base)).unwrap();
        assert_eq!(id.as_str(), "example.com/alias");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_path_dependency_with_no_base_to_anchor_on_is_unsatisfiable() {
        let err = dependency_identity("pgsql_gos", &path_spec("driver"), None).unwrap_err();
        assert!(matches!(err, ResolveError::Unsatisfiable { id } if id == "pgsql_gos"));
    }

    #[test]
    fn a_path_dependency_pointing_at_no_manifest_is_unsatisfiable() {
        let (base, root) = package_at("example.com/driver");
        std::fs::remove_file(root.join("project.toml")).unwrap();
        let err = dependency_identity("pgsql_gos", &path_spec("driver"), Some(&base)).unwrap_err();
        assert!(matches!(err, ResolveError::Unsatisfiable { id } if id == "pgsql_gos"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_git_dependency_still_takes_its_identity_from_the_url() {
        let spec = DependencySpec::Inline(InlineDependency::Git {
            url: "https://github.com/gossamer-lang/pgsql-gos".to_string(),
            reference: "main".to_string(),
        });
        let id = dependency_identity("pgsql_gos", &spec, None).unwrap();
        assert_eq!(id.as_str(), "github.com/gossamer-lang/pgsql-gos");
    }
}
