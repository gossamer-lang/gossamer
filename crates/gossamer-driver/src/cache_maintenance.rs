//! Shared inspection, cleanup, and bounded-retention support for toolchain
//! caches. Cache contents are disposable, so failed accounting never makes a
//! build fail.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Independently manageable cache classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheClass {
    /// Parsed frontend source blobs.
    Frontend,
    /// LLVM incremental object files.
    Ir,
    /// Rust-binding runner and staticlib Cargo builds.
    Runners,
    /// Downloaded package source trees.
    Packages,
    /// Artifacts left behind by the retired build-graph cache.
    Build,
    /// Stamps recording the inputs a linked binary was produced from.
    LinkStamps,
}

impl CacheClass {
    /// Stable command-line name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Frontend => "frontend",
            Self::Ir => "ir",
            Self::Runners => "runners",
            Self::Packages => "packages",
            Self::Build => "build",
            Self::LinkStamps => "link-stamps",
        }
    }

    /// Every known cache class.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Frontend,
            Self::Ir,
            Self::Runners,
            Self::Packages,
            Self::Build,
            Self::LinkStamps,
        ]
    }

    /// Directory name this class occupies inside a project's `.gos-cache/`,
    /// or `None` for a class that lives only in a shared root.
    #[must_use]
    pub const fn project_dir_name(self) -> Option<&'static str> {
        match self {
            Self::Frontend => Some("frontend"),
            Self::Ir => Some("ir-cache"),
            Self::LinkStamps => Some("link-stamps"),
            Self::Runners | Self::Packages | Self::Build => None,
        }
    }

    /// Parses a command-line name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::all()
            .iter()
            .copied()
            .find(|class| class.name() == value)
    }
}

/// Which cache roots an operation reaches.
///
/// The project's own `.gos-cache/` is disposable and belongs to the checkout,
/// while the shared roots under the user's cache directory are what every
/// project on the machine reuses. Naming the two apart is what lets a command
/// throw away this project's cache without emptying the machine's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheScope {
    /// Cache roots inside the working directory (`.gos-cache/`).
    #[default]
    Local,
    /// Shared roots outside the working directory: the user cache directory,
    /// the package cache, and the retired build-graph root.
    Global,
    /// Every root, local and shared.
    All,
}

impl CacheScope {
    /// Stable command-line name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Global => "global",
            Self::All => "all",
        }
    }

    /// Parses a command-line name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "local" => Some(Self::Local),
            "global" => Some(Self::Global),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// True when `path` is a root this scope reaches.
    ///
    /// A root under the working directory is the project's own; anything else
    /// is shared. Classifying by location rather than by class keeps a
    /// `GOSSAMER_CACHE_DIR` override on whichever side the user pointed it at.
    #[must_use]
    pub fn covers(self, path: &Path, cwd: &Path) -> bool {
        match self {
            Self::All => true,
            Self::Local => path.starts_with(cwd),
            Self::Global => !path.starts_with(cwd),
        }
    }
}

/// One cache root with its current accounting.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// Owning cache class.
    pub class: CacheClass,
    /// Absolute or project-relative cache root.
    pub path: PathBuf,
    /// Aggregate regular-file bytes.
    pub bytes: u64,
    /// Aggregate regular-file count.
    pub files: u64,
}

/// A retention policy. Environment variables intentionally make it easy for
/// CI and constrained developer machines to tighten the defaults.
#[derive(Debug, Clone, Copy)]
pub struct CachePolicy {
    /// Aggregate cache capacity across all discovered roots.
    pub max_bytes: u64,
    /// Age after which an entry is eligible for deletion.
    pub max_age: Duration,
}

impl Default for CachePolicy {
    fn default() -> Self {
        let max_bytes = env_u64("GOS_CACHE_MAX_BYTES").unwrap_or(20 * 1024 * 1024 * 1024);
        let days = env_u64("GOS_CACHE_MAX_AGE_DAYS").unwrap_or(30);
        Self {
            max_bytes,
            max_age: Duration::from_secs(days.saturating_mul(86_400)),
        }
    }
}

impl CachePolicy {
    /// Per-class cap. An explicit `GOS_CACHE_<CLASS>_MAX_BYTES` value wins.
    #[must_use]
    pub fn class_max_bytes(self, class: CacheClass) -> u64 {
        let default = match class {
            CacheClass::Runners => 10 * 1024 * 1024 * 1024,
            CacheClass::Ir => 5 * 1024 * 1024 * 1024,
            CacheClass::Frontend => 1024 * 1024 * 1024,
            CacheClass::Packages | CacheClass::Build => 2 * 1024 * 1024 * 1024,
            CacheClass::LinkStamps => 64 * 1024 * 1024,
        };
        let name = format!(
            "GOS_CACHE_{}_MAX_BYTES",
            class.name().to_ascii_uppercase().replace('-', "_")
        );
        env_u64(&name).unwrap_or(default)
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

/// Resolves all user and project cache roots. It mirrors the existing cache
/// producers rather than inventing another root.
#[must_use]
pub fn paths(cwd: &Path) -> Vec<(CacheClass, PathBuf)> {
    let shared = crate::frontend_cache::user_cache_root();
    let binding_root = if let Some(root) = std::env::var_os("GOSSAMER_CACHE") {
        PathBuf::from(root).join("gossamer")
    } else {
        shared.clone()
    };
    let mut out: Vec<(CacheClass, PathBuf)> =
        vec![(CacheClass::Frontend, crate::frontend_cache::cache_dir())];
    // `GOSSAMER_CACHE_DIR` names the one directory the frontend cache uses, so
    // an override is reported alone. Without it the cache resolves to the
    // project when one is in scope, so both conventional locations are listed
    // too; duplicates collapse below.
    if std::env::var_os("GOSSAMER_CACHE_DIR").is_none() {
        out.push((CacheClass::Frontend, shared.join("frontend")));
    }
    out.extend([
        (CacheClass::Ir, shared.join("ir-cache")),
        (CacheClass::Runners, binding_root.join("runners")),
    ]);
    // Each project anchors its `.gos-cache/` at its own `project.toml`, so a
    // workspace, an examples directory, and an integration-test tree each
    // carry one below the directory the command runs in.
    for root in project_cache_roots(cwd) {
        for class in CacheClass::all() {
            if let Some(name) = class.project_dir_name() {
                out.push((*class, root.join(name)));
            }
        }
    }
    if let Some(root) = gossamer_pkg::default_cache_root() {
        out.push((CacheClass::Packages, root));
    }
    out.push((CacheClass::Build, legacy_build_cache_root()));
    let mut seen: Vec<PathBuf> = Vec::with_capacity(out.len());
    out.retain(|(_, path)| {
        if seen.contains(path) {
            return false;
        }
        seen.push(path.clone());
        true
    });
    out
}

/// The cache roots `scope` reaches, in the order [`paths`] reports them.
#[must_use]
pub fn paths_in_scope(cwd: &Path, scope: CacheScope) -> Vec<(CacheClass, PathBuf)> {
    paths(cwd)
        .into_iter()
        .filter(|(_, path)| scope.covers(path, cwd))
        .collect()
}

/// Every `.gos-cache/` at or below `cwd`, in path order.
///
/// The working directory's own root is always reported, present or not, so a
/// status listing names it with zeroes rather than omitting it. Directories
/// that hold a project's outputs or its dependencies' sources are skipped:
/// neither writes a toolchain cache, and both can be large.
#[must_use]
pub fn project_cache_roots(cwd: &Path) -> Vec<PathBuf> {
    const SKIPPED: &[&str] = &[".git", ".gos-bindings", "target", "vendor", "node_modules"];

    let mut out = vec![cwd.join(".gos-cache")];
    let mut pending = vec![cwd.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let cache = dir.join(".gos-cache");
        if cache.is_dir() && !out.contains(&cache) {
            out.push(cache);
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // A symlink is followed by `is_dir`, so testing the entry's own
            // type keeps the walk inside this tree.
            if !entry.file_type().is_ok_and(|ty| ty.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".gos-cache" || SKIPPED.contains(&name.as_ref()) {
                continue;
            }
            pending.push(entry.path());
        }
    }
    out.sort();
    out
}

/// Directory the retired build-graph cache wrote to. Still reported and
/// cleaned so an upgrade does not strand gigabytes in a user's home.
fn legacy_build_cache_root() -> PathBuf {
    // Windows names the home directory `USERPROFILE`; without the fallback the
    // root resolves to `.`, and the sweep would walk the current project.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".gossamer").join("build")
}

/// Reports the cache roots `scope` reaches. Missing roots are represented as
/// zeroes.
#[must_use]
pub fn status(cwd: &Path, scope: CacheScope) -> Vec<CacheEntry> {
    paths_in_scope(cwd, scope)
        .into_iter()
        .map(|(class, path)| {
            let (bytes, files) = dir_size(&path);
            CacheEntry {
                class,
                path,
                bytes,
                files,
            }
        })
        .collect()
}

/// Removes selected cache classes within `scope`, returning the paths actually
/// removed.
pub fn remove(
    cwd: &Path,
    classes: &[CacheClass],
    scope: CacheScope,
    dry_run: bool,
) -> std::io::Result<Vec<CacheEntry>> {
    let mut removed = Vec::new();
    for (class, path) in paths_in_scope(cwd, scope) {
        if !classes.contains(&class) || !path.is_dir() {
            continue;
        }
        let (bytes, files) = dir_size(&path);
        if !dry_run {
            // The caller asked for the directory to be gone; another process
            // removing it first satisfies that, so only a real I/O failure
            // propagates.
            match fs::remove_dir_all(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            // A `.gos-cache/` whose last class directory just went is itself
            // cache, so it leaves with them.
            cleanup_empty_cache_dirs(&path, &path);
        }
        removed.push(CacheEntry {
            class,
            path,
            bytes,
            files,
        });
    }
    Ok(removed)
}

/// Prunes expired units first, then oldest units until the aggregate budget is
/// met. A runner workdir is one unit: it is reclaimed whole or not at all, so a
/// surviving workdir always holds the complete artifact set its stamp records.
/// Workdirs with a build lock are skipped so an active build is never
/// disrupted. Returns reclaimed bytes and files.
pub fn prune(
    cwd: &Path,
    policy: CachePolicy,
    scope: CacheScope,
    dry_run: bool,
) -> std::io::Result<(u64, u64)> {
    let now = SystemTime::now();
    let mut units = Vec::new();
    let roots: Vec<(CacheClass, PathBuf)> = paths_in_scope(cwd, scope);
    for (index, (class, root)) in roots.iter().enumerate() {
        for unit in collect_units(*class, root) {
            units.push((*class, index, unit));
        }
    }
    units.sort_by_key(|(_, _, unit)| unit.modified);
    let mut total: u64 = units.iter().map(|(_, _, unit)| unit.bytes).sum();
    let mut class_totals: HashMap<CacheClass, u64> = HashMap::new();
    for (class, _, unit) in &units {
        *class_totals.entry(*class).or_default() += unit.bytes;
    }
    let mut reclaimed = 0;
    let mut count = 0;
    for (class, root_index, unit) in units {
        let expired = now
            .duration_since(unit.modified)
            .is_ok_and(|age| age > policy.max_age);
        let class_over =
            class_totals.get(&class).copied().unwrap_or_default() > policy.class_max_bytes(class);
        if !expired && total <= policy.max_bytes && !class_over {
            continue;
        }
        if runner_locked(&unit.path) {
            continue;
        }
        if !dry_run {
            let root = &roots[root_index].1;
            if class == CacheClass::Runners && unit.whole_dir {
                if !reclaim_runner_workdir(&unit.path, root) {
                    continue;
                }
            } else {
                remove_unit(&unit, root);
            }
        }
        total = total.saturating_sub(unit.bytes);
        let class_total = class_totals.entry(class).or_default();
        *class_total = class_total.saturating_sub(unit.bytes);
        reclaimed += unit.bytes;
        count += unit.files;
    }
    Ok((reclaimed, count))
}

/// Applies the runner-class age and byte limits directly to one resolved
/// runner root. Binding startup uses this once per process so the documented
/// 10 GiB class cap is enforced without requiring a manual cache command.
///
/// `in_use` names the workdir the calling process resolved. It is never
/// reclaimed: its artifacts are read for the rest of that process's run,
/// past the build lock that only spans their production.
pub fn prune_runner_root(
    root: &Path,
    policy: CachePolicy,
    dry_run: bool,
    in_use: Option<&Path>,
) -> std::io::Result<(u64, u64)> {
    let now = SystemTime::now();
    let mut units = collect_units(CacheClass::Runners, root);
    units.sort_by_key(|unit| unit.modified);
    let mut total: u64 = units.iter().map(|unit| unit.bytes).sum();
    let cap = policy.class_max_bytes(CacheClass::Runners);
    let mut reclaimed = 0u64;
    let mut count = 0u64;
    for unit in units {
        let expired = now
            .duration_since(unit.modified)
            .is_ok_and(|age| age > policy.max_age);
        if !expired && total <= cap {
            continue;
        }
        if in_use.is_some_and(|dir| dir.starts_with(&unit.path)) {
            continue;
        }
        if runner_locked(&unit.path) {
            continue;
        }
        if !dry_run && !reclaim_runner_workdir(&unit.path, root) {
            continue;
        }
        total = total.saturating_sub(unit.bytes);
        reclaimed = reclaimed.saturating_add(unit.bytes);
        count = count.saturating_add(unit.files);
    }
    Ok((reclaimed, count))
}

/// One reclaimable cache unit: a single file, or - for the runner class - a
/// whole workdir, whose artifacts and freshness stamp only mean anything
/// together.
#[derive(Debug)]
struct CacheUnit {
    path: PathBuf,
    bytes: u64,
    files: u64,
    modified: SystemTime,
    whole_dir: bool,
}

/// Enumerates what the class reclaims in one piece: a runner root hands back
/// one unit per workdir, every other root one unit per file.
fn collect_units(class: CacheClass, root: &Path) -> Vec<CacheUnit> {
    if class != CacheClass::Runners {
        let mut files = Vec::new();
        collect_files(root, &mut files);
        return files
            .into_iter()
            .map(|entry| CacheUnit {
                path: entry.path,
                bytes: entry.bytes,
                files: 1,
                modified: entry.modified,
                whole_dir: false,
            })
            .collect();
    }
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut units = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            let mut files = Vec::new();
            collect_files(&path, &mut files);
            let modified = files
                .iter()
                .map(|f| f.modified)
                .max()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            units.push(CacheUnit {
                path,
                bytes: files.iter().map(|f| f.bytes).sum(),
                files: files.len() as u64,
                modified,
                whole_dir: true,
            });
        } else if meta.is_file() {
            units.push(CacheUnit {
                path,
                bytes: meta.len(),
                files: 1,
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                whole_dir: false,
            });
        }
    }
    units
}

/// Reclaims one unit and the empty cache directories it leaves behind.
/// Removes one runner workdir while holding the build lock that guards it,
/// and answers whether the removal happened.
///
/// A build takes that lock before it creates anything, so holding it here is
/// what makes reclamation and a build mutually exclusive rather than merely
/// unlikely to overlap. The lock file itself outlives the tree around it and
/// goes last: a `gos` waiting on it starts writing only once the directory it
/// would have written into is gone.
fn reclaim_runner_workdir(path: &Path, root: &Path) -> bool {
    let Some(lock) = crate::binding_runner::AdvisoryLock::try_acquire(
        &crate::binding_runner::build_lock_path(path),
    ) else {
        return false;
    };
    // A process running the runner, or reading what it produced, holds a
    // lease; the workdir stays until the last one is given back.
    if crate::binding_runner::has_live_lease(path) {
        return false;
    }
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let child = entry.path();
            if child == lock.path() {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let _ = fs::remove_dir_all(&child);
            } else {
                let _ = fs::remove_file(&child);
            }
        }
    }
    drop(lock);
    let _ = fs::remove_dir(path);
    cleanup_empty_cache_dirs(path, root);
    true
}

fn remove_unit(unit: &CacheUnit, root: &Path) {
    if unit.whole_dir {
        let _ = fs::remove_dir_all(&unit.path);
    } else {
        let _ = fs::remove_file(&unit.path);
    }
    cleanup_empty_cache_dirs(&unit.path, root);
}

#[derive(Debug)]
struct FileEntry {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

fn collect_files(root: &Path, out: &mut Vec<FileEntry>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect_files(&path, out);
        } else if meta.is_file() {
            out.push(FileEntry {
                path,
                bytes: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }
}

fn dir_size(root: &Path) -> (u64, u64) {
    let mut entries = Vec::new();
    collect_files(root, &mut entries);
    (
        entries.iter().map(|entry| entry.bytes).sum(),
        entries.len() as u64,
    )
}

fn runner_locked(path: &Path) -> bool {
    path.ancestors()
        .any(|ancestor| ancestor.join(".gos-build.lock").is_file())
}

/// Removes the now-empty directories `path` leaves behind, from its parent up
/// to `root`, then the `.gos-cache/` that owns `root`. Nothing above a cache
/// root is cache, so the sweep stays inside what the caller asked to reclaim.
fn cleanup_empty_cache_dirs(path: &Path, root: &Path) {
    for parent in path.ancestors().skip(1) {
        if !parent.starts_with(root) {
            if parent.file_name().is_some_and(|name| name == ".gos-cache")
                && root.parent() == Some(parent)
            {
                let _ = fs::remove_dir(parent);
            }
            return;
        }
        if fs::remove_dir(parent).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "gossamer-cache-maintenance-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn walker_counts_regular_files_without_following_symlinks() {
        let root = scratch("size");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("a"), b"abc").unwrap();
        fs::write(root.join("nested").join("b"), b"12345").unwrap();
        assert_eq!(dir_size(&root), (8, 2));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runner_lock_marks_descendant_files_ineligible() {
        let root = scratch("lock");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("runner").join("target")).unwrap();
        fs::write(root.join("runner").join(".gos-build.lock"), b"lock").unwrap();
        let artifact = root.join("runner").join("target").join("artifact");
        fs::write(&artifact, b"x").unwrap();
        assert!(runner_locked(&artifact));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runner_fingerprint_lock_marks_sibling_artifacts_ineligible() {
        let root = scratch("fingerprint-lock");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("runner")).unwrap();
        fs::create_dir_all(root.join("sigs")).unwrap();
        fs::write(root.join(".gos-build.lock"), b"lock").unwrap();
        let sigs = root.join("sigs").join("signatures.json");
        let runner = root.join("runner").join("gos-runner");
        fs::write(&sigs, b"{}").unwrap();
        fs::write(&runner, b"x").unwrap();
        assert!(runner_locked(&sigs));
        assert!(runner_locked(&runner));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scope_splits_project_roots_from_shared_ones() {
        let cwd = scratch("scope-cwd");
        let local = cwd.join(".gos-cache").join("frontend");
        let shared = scratch("scope-shared").join("frontend");
        assert!(CacheScope::Local.covers(&local, &cwd));
        assert!(!CacheScope::Local.covers(&shared, &cwd));
        assert!(CacheScope::Global.covers(&shared, &cwd));
        assert!(!CacheScope::Global.covers(&local, &cwd));
        assert!(CacheScope::All.covers(&local, &cwd));
        assert!(CacheScope::All.covers(&shared, &cwd));
    }

    #[test]
    fn scope_names_round_trip_through_parse() {
        for scope in [CacheScope::Local, CacheScope::Global, CacheScope::All] {
            assert_eq!(CacheScope::parse(scope.name()), Some(scope));
        }
        assert_eq!(CacheScope::parse("machine"), None);
        assert_eq!(CacheScope::default(), CacheScope::Local);
    }

    #[test]
    fn local_scope_reports_only_roots_under_the_working_directory() {
        let cwd = scratch("scope-paths");
        let local = paths_in_scope(&cwd, CacheScope::Local);
        assert!(!local.is_empty(), "a project always has its own roots");
        assert!(
            local.iter().all(|(_, path)| path.starts_with(&cwd)),
            "local scope reached a shared root: {local:?}"
        );
        let global = paths_in_scope(&cwd, CacheScope::Global);
        assert!(
            global.iter().all(|(_, path)| !path.starts_with(&cwd)),
            "global scope reached a project root: {global:?}"
        );
        assert_eq!(
            local.len() + global.len(),
            paths_in_scope(&cwd, CacheScope::All).len(),
            "the two scopes partition every root"
        );
    }

    #[test]
    fn a_local_clear_leaves_the_shared_roots_alone() {
        let cwd = scratch("scope-clear");
        let _ = fs::remove_dir_all(&cwd);
        for (_, path) in paths_in_scope(&cwd, CacheScope::Local) {
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("blob"), b"xyz").unwrap();
        }
        let removed = remove(&cwd, CacheClass::all(), CacheScope::Local, false).unwrap();
        assert!(!removed.is_empty(), "nothing was removed");
        assert!(
            removed.iter().all(|entry| entry.path.starts_with(&cwd)),
            "a shared root was removed: {removed:?}"
        );
        for (_, path) in paths_in_scope(&cwd, CacheScope::Local) {
            assert!(!path.is_dir(), "{} survived the clear", path.display());
        }
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn default_policy_has_a_smaller_frontend_cap_than_runner_cap() {
        let policy = CachePolicy::default();
        assert!(
            policy.class_max_bytes(CacheClass::Frontend)
                < policy.class_max_bytes(CacheClass::Runners)
        );
    }

    #[test]
    fn runner_root_prunes_oldest_files_to_class_cap() {
        let root = scratch("runner-prune");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("old"), vec![0u8; 8]).unwrap();
        fs::write(root.join("new"), vec![0u8; 8]).unwrap();
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::MAX,
        };
        // The environment-independent class cap is large, so dry-run proves
        // traversal without deleting. Byte-limit behavior is covered by the
        // shared prune path; this regression protects the direct-root API.
        assert_eq!(
            prune_runner_root(&root, policy, true, None).unwrap(),
            (0, 0)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runner_workdir_is_reclaimed_whole_or_not_at_all() {
        let root = scratch("runner-workdir");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("deadbeef").join("runner").join("target");
        fs::create_dir_all(&workdir).unwrap();
        fs::write(workdir.join("libgossamer_runtime.a"), vec![0u8; 4096]).unwrap();
        fs::write(workdir.join("gos-runner"), vec![0u8; 4096]).unwrap();
        fs::write(root.join("deadbeef").join("stamp.json"), b"{}").unwrap();
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::ZERO,
        };
        let (bytes, files) = prune_runner_root(&root, policy, false, None).unwrap();
        assert_eq!(files, 3);
        assert_eq!(bytes, 8194);
        assert!(!root.join("deadbeef").exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// Reclamation and a build are mutually exclusive through the workdir's
    /// build lock, so the removal must leave the lock free for the next build
    /// rather than a file nothing owns.
    #[test]
    fn reclaiming_a_runner_workdir_leaves_no_lock_behind() {
        let root = scratch("runner-workdir-reclaim");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("cafed00d");
        fs::create_dir_all(workdir.join("runner").join("target")).unwrap();
        fs::write(workdir.join("runner").join("main.rs"), b"fn main() {}").unwrap();

        assert!(reclaim_runner_workdir(&workdir, &root));

        assert!(!workdir.exists(), "the workdir must be gone");
        assert!(!crate::binding_runner::build_lock_path(&workdir).exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// A lock whose owner died must not pin a workdir in the cache forever.
    #[test]
    fn a_stale_lock_does_not_pin_a_runner_workdir() {
        let root = scratch("runner-workdir-stale-lock");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("d15ea5e0");
        fs::create_dir_all(workdir.join("runner")).unwrap();
        fs::write(workdir.join("runner").join("main.rs"), b"fn main() {}").unwrap();
        // No process can hold this pid: it is past every platform's pid_max.
        fs::write(
            crate::binding_runner::build_lock_path(&workdir),
            format!("{}\n", u32::MAX),
        )
        .unwrap();

        assert!(reclaim_runner_workdir(&workdir, &root));

        assert!(!workdir.exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// The lock a live build holds is the whole guarantee: reclamation reports
    /// that it did nothing rather than removing files out from under it.
    #[test]
    fn reclaiming_a_workdir_a_build_holds_removes_nothing() {
        let root = scratch("runner-workdir-held");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("feedface");
        fs::create_dir_all(workdir.join("runner")).unwrap();
        let source = workdir.join("runner").join("main.rs");
        fs::write(&source, b"fn main() {}").unwrap();
        let held = crate::binding_runner::AdvisoryLock::try_acquire(
            &crate::binding_runner::build_lock_path(&workdir),
        )
        .expect("a free lock is takeable");

        assert!(!reclaim_runner_workdir(&workdir, &root));
        assert!(source.exists(), "a held workdir keeps its files");

        drop(held);
        assert!(reclaim_runner_workdir(&workdir, &root));
        let _ = fs::remove_dir_all(&root);
    }

    /// A process that re-executed into a runner links and runs out of its
    /// workdir after the build lock is released, so its lease alone keeps the
    /// workdir in place.
    #[test]
    fn a_leased_runner_workdir_is_not_reclaimed() {
        let root = scratch("runner-workdir-leased");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("0ddba11");
        fs::create_dir_all(workdir.join("runner")).unwrap();
        let archive = workdir.join("runner").join("libgossamer_runtime.a");
        fs::write(&archive, b"x").unwrap();
        fs::write(
            workdir.join(format!(".gos-lease-{}-0", std::process::id())),
            b"",
        )
        .unwrap();

        assert!(!reclaim_runner_workdir(&workdir, &root));
        assert!(archive.exists(), "a leased workdir keeps its files");

        fs::remove_file(workdir.join(format!(".gos-lease-{}-0", std::process::id()))).unwrap();
        assert!(reclaim_runner_workdir(&workdir, &root));
        let _ = fs::remove_dir_all(&root);
    }

    /// A lease whose process exited must not pin a workdir forever.
    #[test]
    fn a_stale_lease_does_not_pin_a_runner_workdir() {
        let root = scratch("runner-workdir-stale-lease");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("5ca1ab1e");
        fs::create_dir_all(workdir.join("runner")).unwrap();
        fs::write(workdir.join(format!(".gos-lease-{}-0", u32::MAX)), b"").unwrap();

        assert!(reclaim_runner_workdir(&workdir, &root));
        assert!(!workdir.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_locked_runner_workdir_survives_the_prune() {
        let root = scratch("runner-workdir-locked");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("cafebabe");
        fs::create_dir_all(workdir.join("runner")).unwrap();
        fs::write(workdir.join(".gos-build.lock"), b"1").unwrap();
        fs::write(workdir.join("runner").join("gos-runner"), vec![0u8; 4096]).unwrap();
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::ZERO,
        };
        assert_eq!(
            prune_runner_root(&root, policy, false, None).unwrap(),
            (0, 0)
        );
        assert!(workdir.join("runner").join("gos-runner").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
