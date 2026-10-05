//! Resolving the features every package of a program builds with.
//!
//! A build asks for features of the program's own project (`--features`,
//! `--no-default-features`); each enabled feature turns on the features,
//! `dep:<id>` optional dependencies, and `<id>/<feature>` dependency
//! features its `[features]` entry lists, and each dependency in turn gets
//! its default features unless the dependent says `default-features =
//! false`, plus the ones the dependent names. A package reached along
//! several paths builds with the union of what each asks for.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use parking_lot::RwLock;
use thiserror::Error;

use crate::manifest::{DependencyFeatures, Manifest};

/// The features a build asks of the program's own project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeatureRequest {
    /// Features named on the command line.
    pub features: Vec<String>,
    /// Whether the project's `default` features stay off.
    pub no_default: bool,
}

/// The resolved features of every package, and which optional
/// dependencies are built.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedFeatures {
    /// The program's own project under `""`, each dependency under its id.
    pub per_package: BTreeMap<String, BTreeSet<String>>,
    /// The optional dependencies an enabled feature turns on.
    pub optional_enabled: BTreeSet<String>,
}

/// Why features cannot be resolved.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FeatureError {
    /// A feature named that its package does not declare.
    #[error("{package} has no feature `{feature}`")]
    Unknown {
        /// The package, `the project` for the program's own.
        package: String,
        /// The feature named.
        feature: String,
    },
    /// A `dep:` or `<id>/` member naming no dependency of its package.
    #[error("feature `{feature}` of {package} names `{dependency}`, which is not a dependency")]
    NotADependency {
        /// The package declaring the feature.
        package: String,
        /// The feature.
        feature: String,
        /// What it names.
        dependency: String,
    },
    /// A manifest that could not be read.
    #[error("{0}")]
    Manifest(String),
}

/// One package's manifest, as resolution reads it.
struct Package {
    label: String,
    manifest: Manifest,
}

/// Resolves `request` for the program rooted at `entry`.
///
/// # Errors
///
/// A feature or dependency named that does not exist, or a manifest that
/// does not parse.
pub fn resolve(entry: &Path, request: &FeatureRequest) -> Result<ResolvedFeatures, FeatureError> {
    let Some(root_manifest) = crate::manifest::find_manifest(entry) else {
        if let Some(feature) = request.features.first() {
            return Err(FeatureError::Unknown {
                package: "the program (it has no project.toml)".to_string(),
                feature: feature.clone(),
            });
        }
        return Ok(ResolvedFeatures::default());
    };
    let mut packages: BTreeMap<String, Package> = BTreeMap::new();
    packages.insert(
        String::new(),
        Package {
            label: "the project".to_string(),
            manifest: read_manifest(&root_manifest)?,
        },
    );
    for (id, root) in all_dependency_roots(entry) {
        let manifest = read_manifest(&root.join("project.toml"))?;
        packages.insert(
            id.clone(),
            Package {
                label: format!("`{id}`"),
                manifest,
            },
        );
    }
    let mut resolver = Resolver {
        packages,
        resolved: ResolvedFeatures::default(),
        work: Vec::new(),
        pending: vec![String::new()],
        activated: BTreeSet::new(),
    };
    if !request.no_default
        && resolver.packages[""]
            .manifest
            .features
            .contains_key("default")
    {
        resolver.work.push((String::new(), "default".to_string()));
    }
    for feature in &request.features {
        resolver.work.push((String::new(), feature.clone()));
    }
    resolver
        .resolved
        .per_package
        .entry(String::new())
        .or_default();
    resolver.run()?;
    Ok(resolver.resolved)
}

fn read_manifest(path: &Path) -> Result<Manifest, FeatureError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| FeatureError::Manifest(format!("reading {}: {e}", path.display())))?;
    Manifest::parse(&text)
        .map_err(|e| FeatureError::Manifest(format!("parsing {}: {e}", path.display())))
}

/// The state of one resolution: features still to enable, packages still to
/// activate, and what is settled so far.
struct Resolver {
    packages: BTreeMap<String, Package>,
    resolved: ResolvedFeatures,
    /// `(package, feature)` pairs to enable.
    work: Vec<(String, String)>,
    /// Packages whose required dependencies are still to be activated.
    pending: Vec<String>,
    activated: BTreeSet<String>,
}

impl Resolver {
    fn run(&mut self) -> Result<(), FeatureError> {
        loop {
            self.activate_pending();
            let Some((id, feature)) = self.work.pop() else {
                return Ok(());
            };
            self.enable(&id, feature)?;
        }
    }

    /// A package becomes active once: its required dependencies with the
    /// features its manifest asks of them.
    fn activate_pending(&mut self) {
        while let Some(id) = self.pending.pop() {
            if !self.activated.insert(id.clone()) {
                continue;
            }
            let Some(package) = self.packages.get(&id) else {
                continue;
            };
            let deps: Vec<(String, DependencyFeatures)> = package
                .manifest
                .dependencies
                .keys()
                .map(|dep| (dep.clone(), chosen_features(&package.manifest, dep)))
                .collect();
            for (dep, chosen) in deps {
                if chosen.optional && !self.resolved.optional_enabled.contains(&dep) {
                    continue;
                }
                self.activate_dependency(&dep, &chosen);
            }
        }
    }

    /// Turns on `feature` of the package `id` and everything it lists.
    fn enable(&mut self, id: &str, feature: String) -> Result<(), FeatureError> {
        let Some(package) = self.packages.get(id) else {
            return Ok(());
        };
        let Some(members) = package.manifest.features.get(&feature).cloned() else {
            return Err(FeatureError::Unknown {
                package: package.label.clone(),
                feature,
            });
        };
        if !self
            .resolved
            .per_package
            .entry(id.to_string())
            .or_default()
            .insert(feature.clone())
        {
            return Ok(());
        }
        for member in members {
            // A feature name holds no `/`, so the last one splits a
            // dependency from its feature.
            if let Some(dep) = member.strip_prefix("dep:") {
                let (dep, chosen) = self.dependency_of(id, &feature, dep)?;
                self.resolved.optional_enabled.insert(dep.clone());
                self.activate_dependency(&dep, &chosen);
            } else if let Some((dep, dep_feature)) = member.rsplit_once('/') {
                let (dep, chosen) = self.dependency_of(id, &feature, dep)?;
                if chosen.optional {
                    self.resolved.optional_enabled.insert(dep.clone());
                }
                self.activate_dependency(&dep, &chosen);
                self.work.push((dep, dep_feature.to_string()));
            } else {
                self.work.push((id.to_string(), member));
            }
        }
        Ok(())
    }

    /// The dependency of `id` that `name` refers to, by its id or by the
    /// module it is reached under, with the features `id` chooses for it.
    fn dependency_of(
        &self,
        id: &str,
        feature: &str,
        name: &str,
    ) -> Result<(String, DependencyFeatures), FeatureError> {
        let package = &self.packages[id];
        let Some(key) = dependency_key(&package.manifest, name) else {
            return Err(FeatureError::NotADependency {
                package: package.label.clone(),
                feature: feature.to_string(),
                dependency: name.to_string(),
            });
        };
        let chosen = chosen_features(&package.manifest, &key);
        Ok((key, chosen))
    }

    /// Queues `dep`'s features as `chosen` selects them and marks it active.
    fn activate_dependency(&mut self, dep: &str, chosen: &DependencyFeatures) {
        self.resolved
            .per_package
            .entry(dep.to_string())
            .or_default();
        let declares_default = self
            .packages
            .get(dep)
            .is_some_and(|package| package.manifest.features.contains_key("default"));
        if chosen.default_features && declares_default {
            self.work.push((dep.to_string(), "default".to_string()));
        }
        for feature in &chosen.features {
            self.work.push((dep.to_string(), feature.clone()));
        }
        self.pending.push(dep.to_string());
    }
}

/// The features `manifest` chooses for its dependency `dep`.
fn chosen_features(manifest: &Manifest, dep: &str) -> DependencyFeatures {
    manifest
        .dependency_features
        .get(dep)
        .cloned()
        .unwrap_or_default()
}

/// The `[dependencies]` key `name` refers to: the key itself, or the one
/// reached under the module `name`.
fn dependency_key(manifest: &Manifest, name: &str) -> Option<String> {
    if manifest.dependencies.contains_key(name) {
        return Some(name.to_string());
    }
    manifest
        .dependencies
        .keys()
        .find(|id| {
            let module = manifest
                .dependency_modules
                .get(*id)
                .cloned()
                .unwrap_or_else(|| gossamer_resolve::project_dep_module_name(id));
            module == name
        })
        .cloned()
}

/// Every dependency of `entry`'s program, optional ones included, with its
/// root directory.
fn all_dependency_roots(entry: &Path) -> Vec<(String, PathBuf)> {
    let previous = active_optional();
    set_active(Some(BTreeSet::new()), true);
    let roots = crate::bundle::path_dependency_roots(entry);
    set_active(previous, false);
    roots
}

/// Which optional dependencies the bundler includes: `None` before any
/// resolution, when every dependency is; `all` lifts the filter while
/// resolution reads every manifest.
static ACTIVE_OPTIONAL: RwLock<(Option<BTreeSet<String>>, bool)> = RwLock::new((None, false));

fn active_optional() -> Option<BTreeSet<String>> {
    ACTIVE_OPTIONAL.read().0.clone()
}

fn set_active(optional: Option<BTreeSet<String>>, all: bool) {
    *ACTIVE_OPTIONAL.write() = (optional, all);
}

/// Records which optional dependencies the program builds with, for the
/// bundler.
pub fn install(resolved: &ResolvedFeatures) {
    set_active(Some(resolved.optional_enabled.clone()), false);
}

/// Whether the bundler includes the dependency `id`, which the manifest
/// declaring it marks `optional`.
#[must_use]
pub fn optional_dependency_included(id: &str) -> bool {
    let slot = ACTIVE_OPTIONAL.read();
    slot.1 || slot.0.as_ref().is_none_or(|set| set.contains(id))
}
