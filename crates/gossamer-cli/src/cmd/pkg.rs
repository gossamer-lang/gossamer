//! Package-management subcommands: `add`, `remove`, `tidy`,
//! `fetch`, `update`, `vendor`, `publish`, `yank`, `login`, `logout`, `owner`.
//! Each operates on the nearest enclosing `project.toml` (or an
//! explicit `--manifest PATH`).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use gossamer_driver::binding_runner::toml_path_kv;

use crate::paths::friendly_io_error;

/// Selects the registry transport. Production builds use
/// `HttpsTransport::new_mozilla_roots`; test runs can flip to a
/// `StaticTransport` via `GOS_REGISTRY_TRANSPORT=static`. The static
/// mode means "no network": `Fetcher` will surface `Unsupported`
/// errors for every registry/git fetch, which is exactly what tests
/// want when verifying offline behaviour without dialling out.
fn registry_transport() -> Arc<dyn gossamer_pkg::Transport> {
    match std::env::var("GOS_REGISTRY_TRANSPORT").as_deref() {
        Ok("static") => Arc::new(gossamer_pkg::StaticTransport::new()),
        _ if insecure_registry_opt_in() => {
            Arc::new(gossamer_pkg::HttpsTransport::new_mozilla_roots_insecure())
        }
        _ => Arc::new(gossamer_pkg::HttpsTransport::new_mozilla_roots()),
    }
}

/// Whether the operator opted into plaintext registry traffic via
/// `GOS_ALLOW_INSECURE_REGISTRY=1`. Loopback hosts are allowed without
/// this; it only relaxes the https requirement for remote hosts.
fn insecure_registry_opt_in() -> bool {
    matches!(
        std::env::var("GOS_ALLOW_INSECURE_REGISTRY").as_deref(),
        Ok("1" | "true")
    )
}

/// Returns the registry URL the CLI should consult. Honours the
/// `GOS_REGISTRY_URL` env var first, falls back to the manifest's
/// `[registries]` table (when keyed under `default`), and finally
/// the public default.
fn registry_url(manifest: &gossamer_pkg::Manifest) -> String {
    if let Ok(env) = std::env::var("GOS_REGISTRY_URL") {
        return env;
    }
    if let Some(url) = manifest.registries.get("default") {
        return url.clone();
    }
    gossamer_pkg::DEFAULT_REGISTRY_URL.to_string()
}

/// Loads the optional bearer token for `registry_url` from the
/// credential store. Silent on missing-file / parse errors so the
/// CLI keeps working without a credential store.
fn credential_for(registry_url: &str) -> Option<String> {
    let store = gossamer_pkg::CredentialStore::load_default().ok()?;
    store.get(registry_url).map(|c| c.token.clone())
}

/// Builds a `Fetcher` populated with the chosen transport + auth
/// token, plus an in-memory catalogue hydrated against the registry
/// for every direct registry dep declared in `manifest`. Path / git /
/// tarball deps don't need an index walk, so they're skipped.
fn build_fetcher(
    manifest: &gossamer_pkg::Manifest,
    options: gossamer_pkg::FetchOptions,
) -> Result<gossamer_pkg::Fetcher> {
    let transport = registry_transport();
    let mut catalogue = gossamer_pkg::VersionCatalogue::new();
    for (raw_id, spec) in &manifest.dependencies {
        if matches!(spec, gossamer_pkg::DependencySpec::Registry(_)) {
            let id = gossamer_pkg::dependency_identity(raw_id, spec, None)
                .with_context(|| format!("invalid id `{raw_id}`"))?;
            if let Err(err) =
                catalogue.load_from_registry(transport.as_ref(), &options.registry_url, &id)
            {
                eprintln!(
                    "warning: registry index for {raw_id} unavailable: {err}; \
                     resolution will fail unless a cached / vendored copy exists"
                );
            }
        }
    }
    Ok(gossamer_pkg::Fetcher::with_transport(options, transport)
        .with_catalogue(catalogue)
        .with_trusted_publisher_keys(manifest.trusted_publishers.clone()))
}

/// When `locked` is set, looks up the nearest `project.toml`,
/// re-resolves direct deps, and refuses to continue if the lockfile
/// is missing or has drifted. Returns `Ok(())` when no lock check is
/// requested or the lock matches.
pub(crate) fn enforce_lockfile_if_requested(locked: bool) -> Result<()> {
    if !locked {
        return Ok(());
    }
    let cwd = std::env::current_dir().context("locating cwd for lockfile check")?;
    let Some(manifest_path) = gossamer_pkg::find_manifest(&cwd) else {
        // No manifest means no deps; --locked is vacuously satisfied.
        return Ok(());
    };
    let project_root = manifest_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let source =
        fs::read_to_string(&manifest_path).map_err(|e| friendly_io_error(e, &manifest_path))?;
    let manifest = gossamer_pkg::Manifest::parse(&source)?;
    let mut options = gossamer_pkg::FetchOptions {
        offline: true,
        registry_url: registry_url(&manifest),
        ..gossamer_pkg::FetchOptions::default()
    };
    options.auth_token = credential_for(&options.registry_url);
    let fetcher = build_fetcher(&manifest, options)?;
    let plan = gossamer_pkg::Resolver::new(fetcher.catalogue().clone())
        .with_root(&project_root)
        .resolve(&manifest)
        .map_err(|e| anyhow!("resolve: {e}"))?;
    let lock = gossamer_pkg::Lockfile::load_required(&project_root)
        .map_err(|e| anyhow!("lockfile: {e}"))?;
    lock.verify_against(&plan)
        .map_err(|e| anyhow!("lockfile drift: {e}"))?;
    Ok(())
}

/// Returns a `Cache` anchored on the per-user cache directory.
fn build_cache() -> gossamer_pkg::Cache {
    match gossamer_pkg::default_cache_root() {
        Some(root) => gossamer_pkg::Cache::with_disk_root(root),
        None => gossamer_pkg::Cache::new(),
    }
}

/// `gos add SPEC [--manifest PATH]` - declares a registry
/// dependency. `SPEC` is `<id>` or `<id>@<requirement>`, where the
/// requirement is `x.y.z` to pin that version, or `^x.y.z` to accept it
/// or anything later.
///
/// A spec with no requirement writes a floor rather than a pin: the
/// user named no version, so pinning one they never chose would be the
/// tool making the decision for them.
pub(crate) fn add(spec: &str, manifest: Option<PathBuf>) -> Result<()> {
    let path = manifest.unwrap_or_else(|| PathBuf::from("project.toml"));
    let (id_text, requirement_text) = match spec.split_once('@') {
        Some((id, requirement)) => (id, requirement.to_string()),
        None => (spec, "^0.1.0".to_string()),
    };
    let id = gossamer_pkg::ProjectId::parse(id_text)
        .with_context(|| format!("invalid id `{id_text}`"))?;
    let requirement = gossamer_pkg::VersionReq::parse(&requirement_text)
        .with_context(|| format!("invalid version requirement `{requirement_text}`"))?;
    let source = fs::read_to_string(&path).map_err(|e| friendly_io_error(e, &path))?;
    let mut m = gossamer_pkg::Manifest::parse(&source)?;
    let changed = gossamer_pkg::add_registry(&mut m, &id, requirement.clone());
    fs::write(&path, m.render()).with_context(|| format!("writing {}", path.display()))?;
    outln!(
        "add: {action} {id} ({requirement})",
        action = if changed { "added" } else { "kept" }
    );
    Ok(())
}

/// `gos add --rust-binding SPEC` - declares an entry in
/// `[rust-bindings]`. Three spec shapes are supported:
///
/// - `<crate>` - crates.io with version `0.0.1` placeholder
///   (user is expected to update it).
/// - `<crate>@<version>` - crates.io with explicit version.
/// - `path:<dir>` - local Cargo crate at `<dir>` (interpreted
///   relative to the manifest).
///
/// For crates that don't already depend on `gossamer-binding`,
/// scaffolds a wrapper crate under `.gos-bindings/<name>/` and
/// rewrites the manifest entry to point at the wrapper.
pub(crate) fn add_rust_binding(spec: &str, manifest: Option<PathBuf>) -> Result<()> {
    let path = manifest.unwrap_or_else(|| PathBuf::from("project.toml"));
    let parent = path
        .parent()
        .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf);
    let source = fs::read_to_string(&path).map_err(|e| friendly_io_error(e, &path))?;
    let mut m = gossamer_pkg::Manifest::parse(&source)?;

    let (name, binding) = parse_rust_binding_spec(spec, &parent)?;
    if !is_valid_cargo_name(&name) {
        return Err(anyhow!("invalid crate name `{name}`"));
    }

    let action = if m.rust_bindings.contains_key(&name) {
        "kept"
    } else {
        "added"
    };
    m.rust_bindings.insert(name.clone(), binding.clone());
    fs::write(&path, m.render()).with_context(|| format!("writing {}", path.display()))?;

    let scaffolded = scaffold_wrapper_if_needed(&name, &binding, &parent)?;

    outln!("add: {action} rust-binding `{name}`");
    if let Some(wrapper) = scaffolded {
        outln!(
            "scaffolded wrapper at {}",
            wrapper.strip_prefix(&parent).unwrap_or(&wrapper).display()
        );
    }
    Ok(())
}

fn parse_rust_binding_spec(
    spec: &str,
    manifest_dir: &std::path::Path,
) -> Result<(String, gossamer_pkg::RustBindingSpec)> {
    if let Some(rest) = spec.strip_prefix("path:") {
        let abs = if std::path::Path::new(rest).is_absolute() {
            PathBuf::from(rest)
        } else {
            manifest_dir.join(rest)
        };
        let crate_name = read_cargo_package_name(&abs)
            .with_context(|| format!("reading {}/Cargo.toml", abs.display()))?;
        let binding = gossamer_pkg::RustBindingSpec::Path {
            version: None,
            path: rest.to_string(),
            features: Vec::new(),
            default_features: true,
        };
        return Ok((crate_name, binding));
    }
    let (name, version) = match spec.split_once('@') {
        Some((n, v)) => (n.to_string(), Some(v.to_string())),
        None => (spec.to_string(), None),
    };
    let version_text = version.unwrap_or_else(|| "0.0.1".to_string());
    let normalized = normalize_version(&version_text);
    let range = gossamer_pkg::VersionReq::parse(&normalized)
        .with_context(|| format!("parsing version `{version_text}`"))?;
    let binding = gossamer_pkg::RustBindingSpec::Crates {
        version: range,
        features: Vec::new(),
        default_features: true,
    };
    Ok((name, binding))
}

/// Completes a partial version literal to a `MAJOR.MINOR.PATCH` triple,
/// keeping whatever bound it carries: stripping the `^` here would turn
/// a floor into a pin without the user asking.
fn normalize_version(input: &str) -> String {
    let trimmed = input.trim();
    let (bound, rest) = match trimmed.strip_prefix('^') {
        Some(rest) => ("^", rest),
        None => match trimmed.strip_prefix('=') {
            Some(rest) => ("=", rest),
            None => ("", trimmed),
        },
    };
    let parts: Vec<&str> = rest.split('.').collect();
    let completed = match parts.len() {
        1 => format!("{}.0.0", parts[0]),
        2 => format!("{}.{}.0", parts[0], parts[1]),
        _ => rest.to_string(),
    };
    format!("{bound}{completed}")
}

fn read_cargo_package_name(crate_root: &std::path::Path) -> Result<String> {
    let cargo_toml = crate_root.join("Cargo.toml");
    let text = fs::read_to_string(&cargo_toml)?;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name") {
            let after_eq = rest.trim().strip_prefix('=').map(str::trim);
            if let Some(value) = after_eq
                && let Some(stripped) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
            {
                return Ok(stripped.to_string());
            }
        }
    }
    Err(anyhow!(
        "Cargo.toml at {} is missing a `name = \"...\"` line",
        cargo_toml.display()
    ))
}

fn is_valid_cargo_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn scaffold_wrapper_if_needed(
    name: &str,
    binding: &gossamer_pkg::RustBindingSpec,
    manifest_dir: &std::path::Path,
) -> Result<Option<PathBuf>> {
    let crate_root = match binding {
        gossamer_pkg::RustBindingSpec::Path { path, .. } => {
            if std::path::Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                manifest_dir.join(path)
            }
        }
        _ => return Ok(None),
    };
    let cargo_toml = crate_root.join("Cargo.toml");
    if !cargo_toml.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&cargo_toml)?;
    if text.contains("gossamer-binding") {
        return Ok(None);
    }
    let wrapper_dir = manifest_dir
        .join(".gos-bindings")
        .join(format!("gos-{name}"));
    if wrapper_dir.exists() {
        return Ok(Some(wrapper_dir));
    }
    fs::create_dir_all(wrapper_dir.join("src"))?;
    let dep_abs = if crate_root.is_absolute() {
        crate_root.clone()
    } else {
        std::fs::canonicalize(&crate_root).unwrap_or_else(|_| crate_root.clone())
    };
    // `gossamer-binding` is stated by version, not by a path into a
    // toolchain checkout: the runner redirects it to the `gos` that
    // builds the project, so the wrapper resolves wherever it is written.
    let wrapper_cargo_toml = format!(
        "[package]\nname = \"gos-{name}\"\nversion = \"0.0.1\"\nedition = \"2024\"\npublish = false\n\n[workspace]\n\n[lib]\ncrate-type = [\"rlib\"]\n\n[dependencies]\n{name} = {{ {} }}\ngossamer-binding = \"{binding_version}\"\n",
        toml_path_kv("path", &dep_abs),
        binding_version = gossamer_pkg::toolchain_version(),
    );
    fs::write(wrapper_dir.join("Cargo.toml"), wrapper_cargo_toml)?;
    let module_name = name.replace('-', "_");
    let wrapper_lib = format!(
        "//! Wrapper crate exposing `{name}` to Gossamer code.\n//!\n//! Fill in the `#[gos_module]` block below with the API surface\n//! you want reachable from Gossamer. `///` doc comments flow\n//! through to the registered item.\n\nuse gossamer_binding::gos_module;\n\n#[gos_module(\"{module_name}\")]\nmod bindings {{\n    use super::*;\n\n    // Example:\n    // /// Version of the wrapped crate.\n    // pub fn version() -> String {{\n    //     env!(\"CARGO_PKG_VERSION\").to_string()\n    // }}\n}}\n",
    );
    fs::write(wrapper_dir.join("src").join("lib.rs"), wrapper_lib)?;
    Ok(Some(wrapper_dir))
}

/// `gos remove ID [--manifest PATH]` - drops the matching
/// dependency entry; errors when nothing matched.
pub(crate) fn remove(id_text: &str, manifest: Option<PathBuf>) -> Result<()> {
    let path = manifest.unwrap_or_else(|| PathBuf::from("project.toml"));
    let id = gossamer_pkg::ProjectId::parse(id_text)
        .with_context(|| format!("invalid id `{id_text}`"))?;
    let source = fs::read_to_string(&path).map_err(|e| friendly_io_error(e, &path))?;
    let mut m = gossamer_pkg::Manifest::parse(&source)?;
    let removed = gossamer_pkg::remove(&mut m, &id);
    if !removed {
        return Err(anyhow!("dependency {id} is not declared"));
    }
    fs::write(&path, m.render()).with_context(|| format!("writing {}", path.display()))?;
    outln!("remove: dropped {id}");
    Ok(())
}

/// `gos tidy [--manifest PATH]` - removes direct project dependencies
/// unused by any `.gos` source and renders canonical manifest ordering.
pub(crate) fn tidy(manifest: Option<PathBuf>) -> Result<()> {
    let path = manifest.unwrap_or_else(|| PathBuf::from("project.toml"));
    let project_root = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let source = fs::read_to_string(&path).map_err(|e| friendly_io_error(e, &path))?;
    let mut m = gossamer_pkg::Manifest::parse(&source)?;
    let mut source_files = Vec::new();
    collect_project_sources(&project_root, &mut source_files)?;
    source_files.sort();

    let used = project_imports(&source_files)?;
    let declared: Vec<String> = m.dependencies.keys().cloned().collect();
    if !source_files.is_empty() {
        // A dependency is reached by its key or by the module name its
        // package normalizes to, so either spelling keeps it.
        m.dependencies.retain(|id, _| {
            used.contains(id) || used.contains(&gossamer_resolve::project_dep_module_name(id))
        });
    }
    let removed: Vec<String> = declared
        .into_iter()
        .filter(|id| !m.dependencies.contains_key(id))
        .collect();
    fs::write(&path, m.render()).with_context(|| format!("writing {}", path.display()))?;
    outln!(
        "tidy: canonicalised {} ({} source file(s), {} unused dependency/dependencies removed)",
        path.display(),
        source_files.len(),
        removed.len(),
    );
    for id in removed {
        outln!("  removed {id}");
    }
    Ok(())
}

fn collect_project_sources(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    const SKIP_DIRS: &[&str] = &[".git", ".gos-bindings", ".gos-cache", "target", "vendor"];
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let name = entry.file_name();
            if SKIP_DIRS.iter().any(|skip| name == *skip) {
                continue;
            }
            collect_project_sources(&path, out)?;
        } else if file_type.is_file()
            && path.extension().and_then(|extension| extension.to_str()) == Some("gos")
        {
            out.push(path);
        }
    }
    Ok(())
}

fn project_imports(files: &[PathBuf]) -> Result<BTreeSet<String>> {
    let mut imports = BTreeSet::new();
    let mut sources = gossamer_lex::SourceMap::new();
    for path in files {
        let source = fs::read_to_string(path).map_err(|e| friendly_io_error(e, path))?;
        let file = sources.add_file(path.to_string_lossy().into_owned(), source.clone());
        let (parsed, diagnostics) = gossamer_parse::parse_source_file(&source, file);
        if !diagnostics.is_empty() {
            return Err(anyhow!(
                "tidy: refusing to edit the manifest because {} has {} parse error(s)",
                path.display(),
                diagnostics.len(),
            ));
        }
        for declaration in parsed.uses {
            match declaration.target {
                // `use "id"` names the package outright.
                gossamer_ast::UseTarget::Project { id, .. } => {
                    imports.insert(gossamer_resolve::project_dep_module_name(&id));
                    imports.insert(id);
                }
                // `use module`, `use module::item`, and `use module::{..}`
                // all name the dependency through the module its package
                // normalizes to.
                gossamer_ast::UseTarget::Module(path) => {
                    if let Some(head) = path.segments.first() {
                        imports.insert(head.name.clone());
                    }
                }
            }
        }
    }
    Ok(imports)
}

/// `gos fetch [--manifest PATH] [--offline] [--update]` -
/// populates the download cache for every transitive dependency and
/// writes / refreshes `project.lock`. The `--update` flag instructs
/// the resolver to re-walk the registry index even when a lockfile
/// already pins a satisfying version.
pub(crate) fn fetch(manifest: Option<PathBuf>, offline: bool, update: bool) -> Result<()> {
    let path = manifest.unwrap_or_else(|| PathBuf::from("project.toml"));
    let project_root = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let source = fs::read_to_string(&path).map_err(|e| friendly_io_error(e, &path))?;
    let m = gossamer_pkg::Manifest::parse(&source)?;
    let mut options = gossamer_pkg::FetchOptions {
        offline,
        registry_url: registry_url(&m),
        ..gossamer_pkg::FetchOptions::default()
    };
    options.auth_token = credential_for(&options.registry_url);
    let existing_lock = gossamer_pkg::Lockfile::load(&project_root)
        .map_err(|e| anyhow!("loading lockfile: {e}"))?;
    let pinned_keys = existing_lock
        .as_ref()
        .map(gossamer_pkg::Lockfile::pinned_keys)
        .unwrap_or_default();
    let fetcher = build_fetcher(&m, options.clone())?.with_pinned_keys(pinned_keys);
    let plan = gossamer_pkg::Resolver::new(fetcher.catalogue().clone())
        .with_root(&project_root)
        .resolve(&m)
        .map_err(|e| anyhow!("resolve failed: {e}"))?;
    if !update && let Some(existing) = &existing_lock {
        existing
            .verify_against(&plan)
            .map_err(|e| anyhow!("lockfile check: {e}"))?;
    }
    let mut cache = build_cache();
    let pkgs = fetcher
        .fetch_all(&plan, &mut cache)
        .map_err(|e| anyhow!("fetch failed: {e}"))?;
    let lock = gossamer_pkg::Lockfile::from_fetched(&pkgs);
    lock.write(&project_root)
        .with_context(|| format!("writing {}", project_root.join("project.lock").display()))?;
    outln!("fetch: {} project(s) cached", pkgs.len());
    for entry in &pkgs {
        outln!("  {} → {}", entry.resolved.id, entry.source.digest);
    }
    Ok(())
}

/// `gos vendor [--manifest PATH] [--out DIR]` - materialises every
/// transitive dependency into `<out>/` for an offline / reproducible
/// build.
pub(crate) fn vendor(manifest: Option<PathBuf>, out: Option<PathBuf>) -> Result<()> {
    let path = manifest.unwrap_or_else(|| PathBuf::from("project.toml"));
    let project_root = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let source = fs::read_to_string(&path).map_err(|e| friendly_io_error(e, &path))?;
    let m = gossamer_pkg::Manifest::parse(&source)?;
    let mut options = gossamer_pkg::FetchOptions {
        registry_url: registry_url(&m),
        ..gossamer_pkg::FetchOptions::default()
    };
    options.auth_token = credential_for(&options.registry_url);
    let pinned_keys = gossamer_pkg::Lockfile::load(&project_root)
        .map_err(|e| anyhow!("loading lockfile: {e}"))?
        .map(|l| l.pinned_keys())
        .unwrap_or_default();
    let fetcher = build_fetcher(&m, options)?.with_pinned_keys(pinned_keys);
    let plan = gossamer_pkg::Resolver::new(fetcher.catalogue().clone())
        .with_root(&project_root)
        .resolve(&m)
        .map_err(|e| anyhow!("resolve failed: {e}"))?;
    let mut cache = build_cache();
    let pkgs = fetcher
        .fetch_all(&plan, &mut cache)
        .map_err(|e| anyhow!("fetch failed: {e}"))?;
    let dest = out.unwrap_or_else(|| PathBuf::from("vendor"));
    let written = gossamer_pkg::vendor(&pkgs, &dest)
        .with_context(|| format!("writing vendor dir {}", dest.display()))?;
    let total: usize = written.values().map(Vec::len).sum();
    outln!(
        "vendor: wrote {total} file(s) for {} project(s) to {}",
        written.len(),
        dest.display()
    );
    Ok(())
}

/// `gos publish [--registry URL] [--dry-run]` - pack the current
/// project deterministically, sha256 it, optionally sign with
/// ed25519, and POST to `<registry>/v1/upload/<id>/<ver>`.
/// Reports any advisory this project can reach, without blocking.
fn warn_on_reachable_advisories(project_root: &Path) {
    let Ok(report) = std::process::Command::new(std::env::current_exe().unwrap_or_default())
        .arg("audit")
        .current_dir(project_root)
        .output()
    else {
        return;
    };
    if report.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&report.stdout);
    for line in text.lines().filter(|l| l.starts_with("advisory[")) {
        eprintln!("warning: {line}");
    }
    eprintln!("warning: publishing anyway; `gos audit` has the detail");
}

pub(crate) fn publish(
    manifest: Option<PathBuf>,
    registry: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let path = manifest.unwrap_or_else(|| PathBuf::from("project.toml"));
    // `Path::new("project.toml").parent()` is `Some("")`, not `None`, so
    // a bare manifest name resolves the project root to the empty path
    // and every later read fails with a bare "No such file or directory".
    let project_root = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let source = fs::read_to_string(&path).map_err(|e| friendly_io_error(e, &path))?;
    let m = gossamer_pkg::Manifest::parse(&source)?;
    // Warn-only: publishing a package whose own dependencies carry a
    // known advisory is worth saying out loud, but refusing the publish
    // would put the registry's advisory feed in the path of every
    // release, where an entry added in error becomes an outage.
    warn_on_reachable_advisories(&project_root);
    let registry_url = registry.unwrap_or_else(|| self::registry_url(&m));
    let artifact = gossamer_pkg::pack_crate_streaming(&project_root)
        .map_err(|e| anyhow!("pack failed: {e}"))?;
    outln!(
        "publish: packed {bytes} byte(s), sha256 {sha}",
        bytes = artifact.bytes,
        sha = artifact.sha256
    );
    let signature = match gossamer_pkg::signing::load_publish_key(m.project.id.as_str()) {
        Ok(key) => {
            // Publish protocol v2 signs the archive's immutable digest. The
            // archive itself stays in its private spool and is copied to the
            // registry directly by the reader-based transport.
            let sig = key.sign(artifact.sha256.as_bytes());
            let pk = key.verifying_key().to_bytes();
            outln!(
                "publish: signed with ed25519 pubkey {pk}",
                pk = key.verifying_key().to_hex()
            );
            Some((sig, pk))
        }
        Err(gossamer_pkg::signing::SigningError::Missing(_)) => {
            eprintln!("publish: no signing key configured; uploading unsigned");
            None
        }
        Err(e) => return Err(anyhow!("signing: {e}")),
    };
    if dry_run {
        outln!("publish: --dry-run set; skipping upload to {registry_url}");
        return Ok(());
    }
    let token = credential_for(&registry_url);
    let transport = registry_transport();
    let uploader = gossamer_pkg::publish::HttpUploader {
        transport: transport.as_ref(),
    };
    let request = gossamer_pkg::publish::StreamingPublishRequest {
        project_id: m.project.id.as_str(),
        version: &m.project.version.to_string(),
        artifact: &artifact,
        signature: signature.map(|(s, _)| s),
        public_key: signature.map(|(_, k)| k),
        auth_token: token.as_deref(),
    };
    gossamer_pkg::publish::upload_streaming_with(&uploader, &registry_url, &request)
        .map_err(|e| anyhow!("upload: {e}"))?;
    outln!("publish: uploaded to {registry_url}");
    Ok(())
}

/// `gos yank <id>@<ver> [--reason MSG]` - flag a previously published
/// version as yanked. New installs refuse to use it unless
/// `--allow-yanked` is set.
pub(crate) fn yank(spec: &str, reason: Option<String>) -> Result<()> {
    let (id_text, version_text) = spec
        .split_once('@')
        .ok_or_else(|| anyhow!("yank spec must be `<id>@<version>`"))?;
    let id = gossamer_pkg::ProjectId::parse(id_text)
        .with_context(|| format!("invalid id `{id_text}`"))?;
    let _ = gossamer_pkg::Version::parse(version_text)
        .with_context(|| format!("invalid version `{version_text}`"))?;
    let registry_url = std::env::var("GOS_REGISTRY_URL")
        .unwrap_or_else(|_| gossamer_pkg::DEFAULT_REGISTRY_URL.to_string());
    let token = credential_for(&registry_url);
    let transport = registry_transport();
    let uploader = gossamer_pkg::publish::HttpUploader {
        transport: transport.as_ref(),
    };
    gossamer_pkg::publish::yank_with(
        &uploader,
        &registry_url,
        id.as_str(),
        version_text,
        reason.as_deref(),
        token.as_deref(),
    )
    .map_err(|e| anyhow!("yank: {e}"))?;
    outln!("yank: marked {id}@{version_text} as yanked");
    Ok(())
}

/// `gos login --registry URL` - prompt for a bearer token (or read
/// from `$GOS_TOKEN`) and write it to the credential store.
pub(crate) fn login(registry: String) -> Result<()> {
    let token = if let Ok(token) = std::env::var("GOS_TOKEN") {
        token
    } else {
        prompt_token(&registry)?
    };
    let path = gossamer_pkg::CredentialStore::default_path()
        .map_err(|e| anyhow!("locating credentials: {e}"))?;
    let mut store =
        gossamer_pkg::CredentialStore::load(&path).map_err(|e| anyhow!("loading: {e}"))?;
    store.insert(registry.clone(), gossamer_pkg::Credential { token });
    store
        .save(&path)
        .map_err(|e| anyhow!("writing credentials: {e}"))?;
    outln!("login: token stored for {registry} at {}", path.display());
    Ok(())
}

/// `gos logout --registry URL` - drop the saved token.
pub(crate) fn logout(registry: String) -> Result<()> {
    let path = gossamer_pkg::CredentialStore::default_path()
        .map_err(|e| anyhow!("locating credentials: {e}"))?;
    let mut store =
        gossamer_pkg::CredentialStore::load(&path).map_err(|e| anyhow!("loading: {e}"))?;
    let removed = store.remove(&registry);
    if removed {
        store
            .save(&path)
            .map_err(|e| anyhow!("writing credentials: {e}"))?;
        outln!("logout: dropped credential for {registry}");
    } else {
        outln!("logout: no credential stored for {registry}");
    }
    Ok(())
}

/// `gos owner [add|remove|list] <id> [<user>]` - manage registry ACLs.
pub(crate) fn owner(op: &str, id_text: &str, user: Option<String>) -> Result<()> {
    let id = gossamer_pkg::ProjectId::parse(id_text)
        .with_context(|| format!("invalid id `{id_text}`"))?;
    let registry_url = std::env::var("GOS_REGISTRY_URL")
        .unwrap_or_else(|_| gossamer_pkg::DEFAULT_REGISTRY_URL.to_string());
    let token = credential_for(&registry_url);
    let transport = registry_transport();
    let uploader = gossamer_pkg::publish::HttpUploader {
        transport: transport.as_ref(),
    };
    gossamer_pkg::publish::owner_op_with(
        &uploader,
        &registry_url,
        id.as_str(),
        op,
        user.as_deref(),
        token.as_deref(),
    )
    .map_err(|e| anyhow!("owner: {e}"))?;
    outln!("owner: {op} applied to {id}");
    Ok(())
}

fn prompt_token(registry: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    write!(err, "token for {registry}: ").map_err(|e| anyhow!("prompt: {e}"))?;
    err.flush().map_err(|e| anyhow!("prompt flush: {e}"))?;
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .map_err(|e| anyhow!("reading token: {e}"))?;
    let token = line.trim().to_string();
    if token.is_empty() {
        return Err(anyhow!("empty token; aborting"));
    }
    Ok(token)
}
