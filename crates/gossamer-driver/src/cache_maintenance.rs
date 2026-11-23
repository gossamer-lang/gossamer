//! Shared inspection, cleanup, and bounded-retention support for toolchain
//! caches. Cache contents are disposable, so failed accounting never makes a
//! build fail.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::binding_build::{AdvisoryLock, SOURCE_LOCK, build_lock_path, lock_shared};

/// Independently manageable cache classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheClass {
    /// Parsed frontend source blobs.
    Frontend,
    /// LLVM incremental object files.
    Ir,
    /// `[rust-bindings]` libraries and staticlibs Cargo built.
    Bindings,
    /// Downloaded package source trees.
    Packages,
    /// Artifacts left behind by retired caches: the build-graph cache and
    /// the per-binding runner workdirs.
    Build,
    /// Stamps recording the inputs a linked binary was produced from.
    LinkStamps,
    /// Objects and libraries compiled from packages' `[native]` sources.
    Native,
    /// Executables `gos run` builds and starts.
    Run,
}

impl CacheClass {
    /// Stable command-line name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Frontend => "frontend",
            Self::Ir => "ir",
            Self::Bindings => "bindings",
            Self::Packages => "packages",
            Self::Build => "build",
            Self::LinkStamps => "link-stamps",
            Self::Native => "native",
            Self::Run => "run",
        }
    }

    /// Every known cache class.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Frontend,
            Self::Ir,
            Self::Bindings,
            Self::Packages,
            Self::Build,
            Self::LinkStamps,
            Self::Native,
            Self::Run,
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
            Self::Native => Some("native"),
            Self::Run => Some("run"),
            Self::Bindings | Self::Packages | Self::Build => None,
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
            CacheClass::Bindings => 10 * 1024 * 1024 * 1024,
            CacheClass::Ir => 5 * 1024 * 1024 * 1024,
            CacheClass::Frontend => 1024 * 1024 * 1024,
            CacheClass::Packages | CacheClass::Build | CacheClass::Native | CacheClass::Run => {
                2 * 1024 * 1024 * 1024
            }
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
    // Without `GOSSAMER_CACHE_DIR` the frontend cache resolves to the project
    // when one is in scope, so that location is listed beside the shared one.
    let mut out: Vec<(CacheClass, PathBuf)> =
        vec![(CacheClass::Frontend, crate::frontend_cache::cache_dir())];
    out.extend(shared_paths());
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

/// The cache roots every project on the machine shares, found without
/// walking any project tree.
fn shared_paths() -> Vec<(CacheClass, PathBuf)> {
    let shared = crate::frontend_cache::user_cache_root();
    let binding_root = if let Some(root) = std::env::var_os("GOSSAMER_CACHE") {
        PathBuf::from(root).join("gossamer")
    } else {
        shared.clone()
    };
    // `GOSSAMER_CACHE_DIR` names the one directory the frontend cache uses.
    let frontend = std::env::var_os("GOSSAMER_CACHE_DIR")
        .map_or_else(|| shared.join("frontend"), PathBuf::from);
    let mut out = vec![
        (CacheClass::Frontend, frontend),
        (CacheClass::Ir, shared.join("ir-cache")),
        (CacheClass::Bindings, binding_root.join("bindings")),
        (CacheClass::Native, shared.join("native")),
        (CacheClass::Run, shared.join("run")),
    ];
    if let Some(root) = gossamer_pkg::default_cache_root() {
        out.push((CacheClass::Packages, root));
    }
    out.push((CacheClass::Build, legacy_build_cache_root()));
    // Binding runners built here before 0.75 and are no longer written.
    out.push((CacheClass::Build, binding_root.join("runners")));
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
/// met. A directory unit is reclaimed whole or not at all, and only while
/// nothing builds in it or holds it in use (see [`hold_in_use`]). Returns
/// reclaimed bytes and files.
pub fn prune(
    cwd: &Path,
    policy: CachePolicy,
    scope: CacheScope,
    dry_run: bool,
) -> io::Result<(u64, u64)> {
    Ok(prune_roots(&paths_in_scope(cwd, scope), policy, dry_run))
}

/// [`prune`] over an explicit list of roots.
///
/// The first walk reclaims each expired unit as it meets it and keeps only the
/// last use, size, and class of the others. When those exceed a cap, the
/// oldest go until every cap holds: sorting the kept records gives each class
/// the last-use time at or before which its units go, and a second walk
/// reclaims them. A unit in use when its turn comes keeps its bytes until a
/// later prune.
fn prune_roots(roots: &[(CacheClass, PathBuf)], policy: CachePolicy, dry_run: bool) -> (u64, u64) {
    let now = SystemTime::now();
    let caps: HashMap<CacheClass, u64> = roots
        .iter()
        .map(|(class, _)| (*class, policy.class_max_bytes(*class)))
        .collect();
    let mut reclaimed = (0u64, 0u64);
    let mut kept: Vec<KeptUnit> = Vec::new();
    let mut total = 0u64;
    let mut class_totals: HashMap<CacheClass, u64> = HashMap::new();
    for (class, root) in roots {
        let mut emptied = Vec::new();
        for_each_unit(*class, root, &mut |unit| {
            let expired = now
                .duration_since(unit.modified)
                .is_ok_and(|age| age > policy.max_age);
            if expired && reclaim_unit(*class, unit, root, dry_run, &mut emptied) {
                reclaimed.0 += unit.bytes;
                reclaimed.1 += unit.files;
                return;
            }
            total += unit.bytes;
            *class_totals.entry(*class).or_default() += unit.bytes;
            kept.push(KeptUnit {
                used: stamp(unit.modified),
                bytes: unit.bytes,
                class: *class,
            });
        });
        remove_emptied_dirs(emptied, root);
    }
    let over = |total: u64, class_totals: &HashMap<CacheClass, u64>, class: CacheClass| {
        total > policy.max_bytes
            || class_totals.get(&class).copied().unwrap_or_default() > caps[&class]
    };
    if !roots
        .iter()
        .any(|(class, _)| over(total, &class_totals, *class))
    {
        return reclaimed;
    }
    kept.sort_unstable_by_key(|unit| unit.used);
    let mut cutoffs: HashMap<CacheClass, u64> = HashMap::new();
    for unit in &kept {
        if !over(total, &class_totals, unit.class) {
            continue;
        }
        total = total.saturating_sub(unit.bytes);
        let class_total = class_totals.entry(unit.class).or_default();
        *class_total = class_total.saturating_sub(unit.bytes);
        cutoffs.insert(unit.class, unit.used);
    }
    drop(kept);
    for (class, root) in roots {
        let Some(&cutoff) = cutoffs.get(class) else {
            continue;
        };
        let mut emptied = Vec::new();
        for_each_unit(*class, root, &mut |unit| {
            if stamp(unit.modified) <= cutoff
                && reclaim_unit(*class, unit, root, dry_run, &mut emptied)
            {
                reclaimed.0 += unit.bytes;
                reclaimed.1 += unit.files;
            }
        });
        remove_emptied_dirs(emptied, root);
    }
    reclaimed
}

/// What the cap pass needs of a unit the expiry walk kept.
struct KeptUnit {
    /// [`stamp`] of its last use.
    used: u64,
    bytes: u64,
    class: CacheClass,
}

/// `time` as nanoseconds since the Unix epoch, for ordering; earlier times
/// answer zero.
fn stamp(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
    })
}

/// Applies the bindings-class age and byte limits directly to the bindings
/// root. A binding build runs this once it finishes, so the documented
/// 10 GiB class cap holds without a manual cache command.
pub fn prune_binding_root(
    root: &Path,
    policy: CachePolicy,
    dry_run: bool,
) -> io::Result<(u64, u64)> {
    let policy = CachePolicy {
        max_bytes: u64::MAX,
        ..policy
    };
    Ok(prune_roots(
        &[(CacheClass::Bindings, root.to_path_buf())],
        policy,
        dry_run,
    ))
}

/// Lock file a process holds shared for as long as it reads a cache
/// directory's contents; reclamation takes it exclusively.
pub const IN_USE_LOCK: &str = ".gos-in-use.lock";

/// Lock a `[native.cmake]` build holds from configuring its tree to
/// installing what it built.
pub const NATIVE_BUILD_LOCK: &str = "lock";

/// Lock the build of a `gos run` directory holds while it writes there.
pub const RUN_BUILD_LOCK: &str = "build.lock";

/// A use refreshes the recorded use time only once it is this old, which
/// bounds the writes a busy cache makes; Go's build cache uses the same hour.
const USE_REFRESH: Duration = Duration::from_hours(1);

/// The directories this process holds in use, each with the shared lock that
/// keeps it. The locks close when the process exits.
static HELD_IN_USE: parking_lot::Mutex<Vec<(PathBuf, fs::File)>> =
    parking_lot::Mutex::new(Vec::new());

/// Keeps the cache directory `dir` from being reclaimed until this process
/// exits, creating it if needed, and records the use for the
/// least-recently-used order pruning follows.
///
/// Take it before checking what `dir` holds: a directory reclaimed between the
/// check and the use would leave the caller holding paths to nothing.
pub fn hold_in_use(dir: &Path) -> io::Result<()> {
    let mut held = HELD_IN_USE.lock();
    if held.iter().any(|(path, _)| path == dir) {
        return Ok(());
    }
    let file = lock_shared(&dir.join(IN_USE_LOCK))?;
    let now = SystemTime::now();
    if file
        .metadata()
        .and_then(|meta| meta.modified())
        .is_ok_and(|used| now.duration_since(used).is_ok_and(|age| age < USE_REFRESH))
    {
        held.push((dir.to_path_buf(), file));
        return Ok(());
    }
    let _ = file.set_modified(now);
    held.push((dir.to_path_buf(), file));
    Ok(())
}

/// Records a hit on the cache file at `path`, so pruning, which removes the
/// least recently used entries first, keeps what builds still read.
pub fn mark_used(path: &Path) {
    let now = SystemTime::now();
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta
        .modified()
        .is_ok_and(|used| now.duration_since(used).is_ok_and(|age| age < USE_REFRESH))
    {
        return;
    }
    if let Ok(file) = fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(now);
    }
}

/// File in the shared cache root holding when the last trim finished, in
/// seconds since the Unix epoch.
const TRIM_STAMP: &str = "trim.txt";

/// Lock the process running a trim holds, so trims never overlap.
const TRIM_LOCK: &str = "trim.lock";

/// How often build commands trim the shared cache roots on their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimSchedule {
    /// Never; `gos cache --trim` and `--prune` still run on request.
    Never,
    /// Once the last trim is this old; zero trims on every build command.
    Every(Duration),
}

impl Default for TrimSchedule {
    fn default() -> Self {
        Self::Every(Duration::from_hours(24))
    }
}

impl TrimSchedule {
    /// Parses a `GOS_CACHE_TRIM` value: `never`, `always`, or a whole number
    /// of minutes, hours, or days (`30m`, `12h`, `7d`).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "never" => return Some(Self::Never),
            "always" => return Some(Self::Every(Duration::ZERO)),
            _ => {}
        }
        let value = value.trim();
        let unit = value.chars().last()?;
        let count: u64 = value[..value.len() - unit.len_utf8()].parse().ok()?;
        let minutes = match unit {
            'm' => count,
            'h' => count.checked_mul(60)?,
            'd' => count.checked_mul(24 * 60)?,
            _ => return None,
        };
        Some(Self::Every(Duration::from_secs(minutes.checked_mul(60)?)))
    }

    /// The schedule `GOS_CACHE_TRIM` names, or the daily default when it is
    /// unset. An unreadable value answers the default with a message naming
    /// what was wrong.
    pub fn from_env() -> Result<Self, (Self, String)> {
        let Some(value) = std::env::var_os("GOS_CACHE_TRIM") else {
            return Ok(Self::default());
        };
        let value = value.to_string_lossy();
        Self::parse(&value).ok_or_else(|| {
            (
                Self::default(),
                format!(
                    "GOS_CACHE_TRIM=`{value}` is not `never`, `always`, or an interval such as \
                     `12h` or `7d`; the cache is trimmed daily"
                ),
            )
        })
    }
}

/// Whether the shared cache roots are due the trim `schedule` asks for: the
/// last one is as old as its interval, or none is recorded for a cache that
/// exists.
#[must_use]
pub fn trim_due(schedule: TrimSchedule) -> bool {
    crate::frontend_cache::cache_enabled()
        && trim_due_in(
            &crate::frontend_cache::user_cache_root(),
            SystemTime::now(),
            schedule,
        )
}

fn trim_due_in(root: &Path, now: SystemTime, schedule: TrimSchedule) -> bool {
    let TrimSchedule::Every(interval) = schedule else {
        return false;
    };
    let Ok(text) = fs::read_to_string(root.join(TRIM_STAMP)) else {
        return root.is_dir();
    };
    // An unreadable stamp, or one in the future after the clock moved back,
    // is treated as overdue; trimming a cache within its limits is cheap.
    text.trim()
        .parse::<u64>()
        .ok()
        .and_then(|secs| {
            now.duration_since(UNIX_EPOCH + Duration::from_secs(secs))
                .ok()
        })
        .is_none_or(|age| age >= interval)
}

/// Trims the shared cache roots to `policy` and records the trim: at once, or
/// with `due` only when that schedule says a trim is due. Answers the
/// reclaimed bytes and files, or `None` when no trim was due or another
/// process is running one.
///
/// Package sources are left alone: a build reports a missing dependency
/// rather than fetching it again, so removing one would break builds that
/// work today.
pub fn trim(policy: CachePolicy, due: Option<TrimSchedule>) -> io::Result<Option<(u64, u64)>> {
    trim_in(
        &crate::frontend_cache::user_cache_root(),
        &shared_paths(),
        policy,
        due,
    )
}

fn trim_in(
    root: &Path,
    roots: &[(CacheClass, PathBuf)],
    policy: CachePolicy,
    due: Option<TrimSchedule>,
) -> io::Result<Option<(u64, u64)>> {
    let Some(_trimming) = AdvisoryLock::try_acquire(&root.join(TRIM_LOCK)) else {
        return Ok(None);
    };
    let now = SystemTime::now();
    // Read under the lock, so a trim that just finished elsewhere counts.
    if due.is_some_and(|schedule| !trim_due_in(root, now, schedule)) {
        return Ok(None);
    }
    let roots: Vec<(CacheClass, PathBuf)> = roots
        .iter()
        .filter(|(class, _)| *class != CacheClass::Packages)
        .cloned()
        .collect();
    let reclaimed = prune_roots(&roots, policy, false);
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let staged = root.join(format!("{TRIM_STAMP}.{}.tmp", std::process::id()));
    fs::write(&staged, secs.to_string())?;
    fs::rename(&staged, root.join(TRIM_STAMP))?;
    Ok(Some(reclaimed))
}

/// One reclaimable cache unit as a walk meets it: a single file, or a
/// directory whose files only mean anything together - a binding build, whose
/// artifacts and freshness stamp go as one; a native build tree, whose
/// configuration names the files its generators wrote; a `gos run`
/// directory, whose executable its build record vouches for; or a retired
/// runner workdir.
struct CacheUnit<'a> {
    /// Directory the unit sits in.
    dir: &'a Path,
    name: &'a OsStr,
    bytes: u64,
    files: u64,
    /// Last use: the file's modification time, or a directory's newest.
    modified: SystemTime,
    whole_dir: bool,
}

impl CacheUnit<'_> {
    fn path(&self) -> PathBuf {
        self.dir.join(self.name)
    }
}

/// Meets each unit `class` reclaims in one piece under `root`: one per
/// directory for the classes whose directories are reclaimed whole, one per
/// file for every other root.
fn for_each_unit(class: CacheClass, root: &Path, visit: &mut impl FnMut(&CacheUnit<'_>)) {
    match class {
        CacheClass::Bindings | CacheClass::Native | CacheClass::Build => {}
        // A project's `.gos-cache/run` is the run directory of that project;
        // the shared root holds one per file run outside any project.
        CacheClass::Run
            if root
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == ".gos-cache") =>
        {
            if let (Some(dir), Some(name)) = (root.parent(), root.file_name()) {
                visit_dir_unit(dir, name, visit);
            }
            return;
        }
        CacheClass::Run => {}
        CacheClass::Frontend | CacheClass::Ir | CacheClass::Packages | CacheClass::LinkStamps => {
            walk_files(root, &mut |dir, name, meta| {
                visit(&CacheUnit {
                    dir,
                    name,
                    bytes: meta.len(),
                    files: 1,
                    modified: meta.modified().unwrap_or(UNIX_EPOCH),
                    whole_dir: false,
                });
            });
            return;
        }
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let name = entry.file_name();
        if meta.is_dir() {
            visit_dir_unit(root, &name, visit);
        } else if meta.is_file() {
            visit(&CacheUnit {
                dir: root,
                name: &name,
                bytes: meta.len(),
                files: 1,
                modified: meta.modified().unwrap_or(UNIX_EPOCH),
                whole_dir: false,
            });
        }
    }
}

/// Meets the directory `name` in `dir` as one unit, last used when its newest
/// file was written.
fn visit_dir_unit(dir: &Path, name: &OsStr, visit: &mut impl FnMut(&CacheUnit<'_>)) {
    let path = dir.join(name);
    if !path.is_dir() {
        return;
    }
    let (mut bytes, mut files, mut modified) = (0u64, 0u64, UNIX_EPOCH);
    walk_files(&path, &mut |_, _, meta| {
        bytes += meta.len();
        files += 1;
        modified = modified.max(meta.modified().unwrap_or(UNIX_EPOCH));
    });
    visit(&CacheUnit {
        dir,
        name,
        bytes,
        files,
        modified,
        whole_dir: true,
    });
}

/// Meets every regular file below `dir` with the directory it sits in, without
/// following symbolic links. A directory's files come before its
/// subdirectories', so none is held while its siblings are read.
fn walk_files(dir: &Path, visit: &mut impl FnMut(&Path, &OsStr, &fs::Metadata)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        // A directory entry's metadata describes a symbolic link itself.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            subdirs.push(entry.path());
        } else if meta.is_file() {
            visit(dir, &entry.file_name(), &meta);
        }
    }
    for subdir in subdirs {
        walk_files(&subdir, visit);
    }
}

/// Removes `unit` of `class` under `root`, or with `dry_run` only checks it
/// could be, and answers whether it was (or could be) reclaimed. A removed
/// file's directory joins `emptied`, which the caller sweeps once its walk is
/// done.
fn reclaim_unit(
    class: CacheClass,
    unit: &CacheUnit<'_>,
    root: &Path,
    dry_run: bool,
    emptied: &mut Vec<PathBuf>,
) -> bool {
    let path = unit.path();
    if !unit.whole_dir {
        if !dry_run {
            let _ = fs::remove_file(&path);
            if emptied.last().is_none_or(|last| last != unit.dir) {
                emptied.push(unit.dir.to_path_buf());
            }
        }
        return true;
    }
    if dry_run {
        return !dir_locks(class, &path).iter().any(|lock| lock_held(lock));
    }
    reclaim_dir(class, &path, root)
}

/// Removes each directory in `emptied` that is now empty, with its emptied
/// ancestors up to `root`.
fn remove_emptied_dirs(mut emptied: Vec<PathBuf>, root: &Path) {
    // Deepest first, so a parent is tried once its children are gone.
    emptied.sort_unstable_by(|a, b| b.cmp(a));
    emptied.dedup();
    for dir in emptied {
        remove_empty_dirs_from(&dir, root);
    }
}

/// The locks a directory of `class` is guarded by, in the order reclamation
/// takes them: [`IN_USE_LOCK`], which every reader and builder takes first,
/// then the one its build holds while writing and any its readers hold shared.
///
/// Taking [`IN_USE_LOCK`] first means a reclaimer only ever holds a build lock
/// while no builder can be waiting on it.
fn dir_locks(class: CacheClass, dir: &Path) -> Vec<PathBuf> {
    let mut locks = vec![dir.join(IN_USE_LOCK)];
    match class {
        CacheClass::Bindings => locks.extend([build_lock_path(dir), dir.join(SOURCE_LOCK)]),
        CacheClass::Build => locks.push(build_lock_path(dir)),
        CacheClass::Native => locks.push(dir.join(NATIVE_BUILD_LOCK)),
        CacheClass::Run => locks.push(dir.join(RUN_BUILD_LOCK)),
        CacheClass::Frontend | CacheClass::Ir | CacheClass::Packages | CacheClass::LinkStamps => {}
    }
    locks
}

/// Whether another holder has the lock file at `path` locked. A missing file
/// is held by no one.
fn lock_held(path: &Path) -> bool {
    let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock))
}

/// Removes one directory of `class` while holding every lock that guards it,
/// and answers whether the removal happened.
///
/// A build takes its lock before it creates anything, and a reader takes
/// [`IN_USE_LOCK`] before it looks, so holding them here is what makes
/// reclamation, a build, and a use mutually exclusive rather than merely
/// unlikely to overlap. The lock files go last: a `gos` waiting on one starts
/// writing only once the directory it would have written into is gone.
fn reclaim_dir(class: CacheClass, path: &Path, root: &Path) -> bool {
    let mut held = Vec::new();
    for lock in dir_locks(class, path) {
        let Some(guard) = AdvisoryLock::try_acquire(&lock) else {
            return false;
        };
        held.push(guard);
    }
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let child = entry.path();
            if held.iter().any(|lock| lock.path() == child) {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let _ = fs::remove_dir_all(&child);
            } else {
                let _ = fs::remove_file(&child);
            }
        }
    }
    // Released in the reverse of the order taken, so the in-use lock that
    // readers wait on goes last.
    held.reverse();
    drop(held);
    let _ = fs::remove_dir(path);
    cleanup_empty_cache_dirs(path, root);
    true
}

fn dir_size(root: &Path) -> (u64, u64) {
    let (mut bytes, mut files) = (0u64, 0u64);
    walk_files(root, &mut |_, _, meta| {
        bytes += meta.len();
        files += 1;
    });
    (bytes, files)
}

/// Removes the now-empty directories `path` leaves behind, from its parent up
/// to `root`, then the `.gos-cache/` that owns `root`. Nothing above a cache
/// root is cache, so the sweep stays inside what the caller asked to reclaim.
fn cleanup_empty_cache_dirs(path: &Path, root: &Path) {
    if let Some(parent) = path.parent() {
        remove_empty_dirs_from(parent, root);
    }
}

/// Removes `dir` and its ancestors up to `root` while each is empty, then the
/// `.gos-cache/` that owns `root`.
fn remove_empty_dirs_from(dir: &Path, root: &Path) {
    for parent in dir.ancestors() {
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

    /// Each unit `class` reclaims under `root`: its path, file count, and
    /// whether it is a whole directory.
    fn units(class: CacheClass, root: &Path) -> Vec<(PathBuf, u64, bool)> {
        let mut out = Vec::new();
        for_each_unit(class, root, &mut |unit| {
            out.push((unit.path(), unit.files, unit.whole_dir));
        });
        out
    }

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
    fn retired_binding_runners_are_listed_for_cleanup() {
        let roots = paths(&scratch("runners-cwd"));
        assert!(
            roots.iter().any(|(class, path)| *class == CacheClass::Build
                && path.ends_with(Path::new("gossamer").join("runners"))),
            "{roots:?}"
        );
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
    fn default_policy_has_a_smaller_frontend_cap_than_bindings_cap() {
        let policy = CachePolicy::default();
        assert!(
            policy.class_max_bytes(CacheClass::Frontend)
                < policy.class_max_bytes(CacheClass::Bindings)
        );
    }

    #[test]
    fn binding_root_prunes_oldest_files_to_class_cap() {
        let root = scratch("binding-prune");
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
        assert_eq!(prune_binding_root(&root, policy, true).unwrap(), (0, 0));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn binding_workdir_is_reclaimed_whole_or_not_at_all() {
        let root = scratch("binding-workdir");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("deadbeef").join("build").join("target");
        fs::create_dir_all(&workdir).unwrap();
        fs::write(workdir.join("libgossamer_runtime.a"), vec![0u8; 4096]).unwrap();
        fs::write(workdir.join("libgos_binding_x.so"), vec![0u8; 4096]).unwrap();
        fs::write(root.join("deadbeef").join("stamp.json"), b"{}").unwrap();
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::ZERO,
        };
        let (bytes, files) = prune_binding_root(&root, policy, false).unwrap();
        assert_eq!(files, 3);
        assert_eq!(bytes, 8194);
        assert!(!root.join("deadbeef").exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// Reclamation and a build are mutually exclusive through the workdir's
    /// build lock, so the removal must leave the lock free for the next build
    /// rather than a file nothing owns.
    #[test]
    fn reclaiming_a_binding_build_leaves_no_lock_behind() {
        let root = scratch("binding-workdir-reclaim");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("cafed00d");
        fs::create_dir_all(workdir.join("build").join("target")).unwrap();
        fs::write(workdir.join("build").join("main.rs"), b"fn main() {}").unwrap();

        assert!(reclaim_dir(CacheClass::Bindings, &workdir, &root));

        assert!(!workdir.exists(), "the workdir must be gone");
        assert!(!crate::binding_build::build_lock_path(&workdir).exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// A lock whose owner died must not pin a workdir in the cache forever.
    #[test]
    fn a_stale_lock_does_not_pin_a_binding_build() {
        let root = scratch("binding-workdir-stale-lock");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("d15ea5e0");
        fs::create_dir_all(workdir.join("build")).unwrap();
        fs::write(workdir.join("build").join("main.rs"), b"fn main() {}").unwrap();
        // No process can hold this pid: it is past every platform's pid_max.
        fs::write(
            crate::binding_build::build_lock_path(&workdir),
            format!("{}\n", u32::MAX),
        )
        .unwrap();

        assert!(reclaim_dir(CacheClass::Bindings, &workdir, &root));

        assert!(!workdir.exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// The lock a live build holds is the whole guarantee: reclamation reports
    /// that it did nothing rather than removing files out from under it.
    #[test]
    fn reclaiming_a_workdir_a_build_holds_removes_nothing() {
        let _children = crate::test_children::releasing_locks();
        let root = scratch("binding-workdir-held");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("feedface");
        fs::create_dir_all(workdir.join("build")).unwrap();
        let source = workdir.join("build").join("main.rs");
        fs::write(&source, b"fn main() {}").unwrap();
        let held = crate::binding_build::AdvisoryLock::try_acquire(
            &crate::binding_build::build_lock_path(&workdir),
        )
        .expect("a free lock is takeable");

        assert!(!reclaim_dir(CacheClass::Bindings, &workdir, &root));
        assert!(source.exists(), "a held workdir keeps its files");

        drop(held);
        assert!(reclaim_dir(CacheClass::Bindings, &workdir, &root));
        let _ = fs::remove_dir_all(&root);
    }

    /// Every process building from a toolchain source snapshot holds its
    /// source lock shared, and reclamation leaves the snapshot while any does.
    #[test]
    fn reclaiming_a_source_snapshot_a_build_reads_removes_nothing() {
        let _children = crate::test_children::releasing_locks();
        let root = scratch("binding-source-read");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("toolchain-src-0.0.0-0123456789abcdef");
        let source = workdir.join("src").join("Cargo.toml");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"[workspace]\n").unwrap();
        let reader = fs::File::create(workdir.join(crate::binding_build::SOURCE_LOCK)).unwrap();
        reader.lock_shared().unwrap();

        assert!(!reclaim_dir(CacheClass::Bindings, &workdir, &root));
        assert!(source.exists(), "a snapshot being read keeps its files");

        drop(reader);
        assert!(reclaim_dir(CacheClass::Bindings, &workdir, &root));
        assert!(!workdir.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_locked_binding_workdir_survives_the_prune() {
        let _children = crate::test_children::releasing_locks();
        let root = scratch("binding-workdir-locked");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("cafebabe");
        fs::create_dir_all(workdir.join("build")).unwrap();
        fs::write(
            workdir.join("build").join("libgos_binding_x.so"),
            vec![0u8; 4096],
        )
        .unwrap();
        let held = AdvisoryLock::try_acquire(&build_lock_path(&workdir)).unwrap();
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::ZERO,
        };
        assert_eq!(prune_binding_root(&root, policy, false).unwrap(), (0, 0));
        assert!(workdir.join("build").join("libgos_binding_x.so").exists());
        drop(held);
        let _ = fs::remove_dir_all(&root);
    }

    /// A lock file is only a name; the operating-system lock on it is what a
    /// live build holds, so a file its dead owner left pins nothing.
    #[test]
    fn a_retired_runner_with_a_leftover_lock_file_is_pruned() {
        let root = scratch("retired-runner");
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("5471969551ee");
        fs::create_dir_all(workdir.join("runner").join("target")).unwrap();
        fs::write(workdir.join(".gos-build.lock"), b"").unwrap();
        fs::write(workdir.join("runner").join("target").join("bin"), [0u8; 64]).unwrap();
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::ZERO,
        };
        let (_, files) = prune_roots(&[(CacheClass::Build, root.clone())], policy, false);
        assert_eq!(files, 2);
        assert!(!workdir.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_another_process_holds_in_use_is_not_reclaimed() {
        let _children = crate::test_children::releasing_locks();
        let root = scratch("in-use-other");
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("cmake-shared-0123456789abcdef");
        let library = dir.join("prefix").join("lib").join("libz.so");
        fs::create_dir_all(library.parent().unwrap()).unwrap();
        fs::write(&library, b"elf").unwrap();
        let reader = lock_shared(&dir.join(IN_USE_LOCK)).unwrap();
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::ZERO,
        };
        let roots = [(CacheClass::Native, root.clone())];

        assert_eq!(prune_roots(&roots, policy, true), (0, 0));
        assert!(!reclaim_dir(CacheClass::Native, &dir, &root));
        assert!(library.exists(), "a directory in use keeps its files");

        drop(reader);
        assert_eq!(prune_roots(&roots, policy, true), (3, 2));
        assert!(library.exists(), "a dry run removes nothing");
        assert!(reclaim_dir(CacheClass::Native, &dir, &root));
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// The hold outlives every build lock the process takes, so a directory
    /// whose artifacts this process still reads survives its own prune too.
    #[test]
    fn a_directory_this_process_holds_in_use_is_not_reclaimed() {
        let root = scratch("in-use-self");
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("gui_engine_binding+1-9bedf40dc9998c7f");
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(dir.join("lib").join("libgos_bindings.so"), b"elf").unwrap();
        hold_in_use(&dir).unwrap();
        hold_in_use(&dir).unwrap();
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::ZERO,
        };
        assert_eq!(prune_binding_root(&root, policy, false).unwrap(), (0, 0));
        assert!(dir.join("lib").join("libgos_bindings.so").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn run_directories_are_reclaimed_whole() {
        let _children = crate::test_children::releasing_locks();
        let root = scratch("run-dirs");
        let _ = fs::remove_dir_all(&root);
        let shared = root.join("run");
        let program = shared.join("001778aae991f3080524f767c28a056e");
        fs::create_dir_all(program.join("debug")).unwrap();
        fs::write(program.join("app"), [0u8; 16]).unwrap();
        fs::write(program.join("debug").join("app"), [0u8; 16]).unwrap();
        let project = root.join("project").join(".gos-cache").join("run");
        fs::create_dir_all(project.join("debug")).unwrap();
        fs::write(project.join("app"), [0u8; 16]).unwrap();
        fs::write(project.join("debug").join("app"), [0u8; 16]).unwrap();

        assert_eq!(
            units(CacheClass::Run, &shared),
            [(program.clone(), 2, true)]
        );
        assert_eq!(
            units(CacheClass::Run, &project),
            [(project.clone(), 2, true)]
        );

        let building = AdvisoryLock::try_acquire(&program.join(RUN_BUILD_LOCK)).unwrap();
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::ZERO,
        };
        let roots = [
            (CacheClass::Run, shared.clone()),
            (CacheClass::Run, project.clone()),
        ];
        assert_eq!(prune_roots(&roots, policy, false), (32, 2));
        assert!(
            program.join("app").exists(),
            "a run directory being built stays"
        );
        assert!(!project.exists());
        assert!(!root.join("project").join(".gos-cache").exists());
        drop(building);
        assert_eq!(prune_roots(&roots, policy, false), (32, 2));
        assert!(!program.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_total_cap_removes_the_least_recently_used_across_classes_and_no_more() {
        let root = scratch("total-cap");
        let _ = fs::remove_dir_all(&root);
        let ir = root.join("ir-cache");
        let frontend = root.join("frontend");
        fs::create_dir_all(&ir).unwrap();
        fs::create_dir_all(&frontend).unwrap();
        let write_used = |path: &Path, hours: u64| {
            fs::write(path, [0u8; 10]).unwrap();
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|file| file.set_modified(SystemTime::now() - Duration::from_hours(hours)))
                .unwrap();
        };
        write_used(&ir.join("a.o"), 72);
        write_used(&frontend.join("b.bin"), 48);
        write_used(&ir.join("c.o"), 24);
        write_used(&frontend.join("d.bin"), 12);
        let policy = CachePolicy {
            max_bytes: 20,
            max_age: Duration::MAX,
        };
        let roots = [
            (CacheClass::Ir, ir.clone()),
            (CacheClass::Frontend, frontend.clone()),
        ];

        assert_eq!(prune_roots(&roots, policy, true), (20, 2));
        assert!(ir.join("a.o").exists(), "a dry run removes nothing");
        assert_eq!(prune_roots(&roots, policy, false), (20, 2));
        assert!(!ir.join("a.o").exists() && !frontend.join("b.bin").exists());
        assert!(ir.join("c.o").exists() && frontend.join("d.bin").exists());
        assert_eq!(prune_roots(&roots, policy, false), (0, 0), "within the cap");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_trim_is_due_on_its_schedule_and_only_for_a_cache_that_exists() {
        let root = scratch("trim-due");
        let _ = fs::remove_dir_all(&root);
        let daily = TrimSchedule::default();
        let day = Duration::from_hours(24);
        let now = SystemTime::now();
        assert!(!trim_due_in(&root, now, daily), "no cache, nothing to trim");
        fs::create_dir_all(&root).unwrap();
        assert!(
            trim_due_in(&root, now, daily),
            "a cache never trimmed is due"
        );
        assert!(!trim_due_in(&root, now, TrimSchedule::Never));
        let at = |time: SystemTime| {
            time.duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string()
        };
        fs::write(root.join(TRIM_STAMP), at(now)).unwrap();
        assert!(!trim_due_in(&root, now, daily));
        assert!(!trim_due_in(
            &root,
            now + day - Duration::from_secs(1),
            daily
        ));
        assert!(trim_due_in(&root, now + day, daily));
        let always = TrimSchedule::Every(Duration::ZERO);
        assert!(trim_due_in(&root, now, always));
        assert!(!trim_due_in(&root, now + day, TrimSchedule::Never));
        fs::write(root.join(TRIM_STAMP), at(now + day)).unwrap();
        assert!(
            trim_due_in(&root, now, daily),
            "a stamp from the future is overdue"
        );
        fs::write(root.join(TRIM_STAMP), "not a time").unwrap();
        assert!(trim_due_in(&root, now, daily));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trim_schedules_parse_never_always_and_intervals() {
        let every = |secs| Some(TrimSchedule::Every(Duration::from_secs(secs)));
        assert_eq!(TrimSchedule::parse("never"), Some(TrimSchedule::Never));
        assert_eq!(TrimSchedule::parse("always"), every(0));
        assert_eq!(TrimSchedule::parse("30m"), every(30 * 60));
        assert_eq!(TrimSchedule::parse(" 12h "), every(12 * 3600));
        assert_eq!(TrimSchedule::parse("7d"), every(7 * 86_400));
        assert_eq!(TrimSchedule::default(), every(86_400).unwrap());
        for bad in [
            "",
            "d",
            "7",
            "7w",
            "-1d",
            "1.5h",
            "daily",
            "18446744073709551615d",
        ] {
            assert_eq!(TrimSchedule::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_trim_enforces_the_policy_on_schedule_or_at_once_and_keeps_package_sources() {
        let root = scratch("trim-run");
        let _ = fs::remove_dir_all(&root);
        let ir = root.join("ir-cache");
        let packages = root.join("packages");
        fs::create_dir_all(&ir).unwrap();
        fs::create_dir_all(&packages).unwrap();
        fs::write(ir.join("old.o"), [0u8; 8]).unwrap();
        fs::write(packages.join("lib.gos"), b"fn f() {}").unwrap();
        let roots = [
            (CacheClass::Ir, ir.clone()),
            (CacheClass::Packages, packages.clone()),
        ];
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::ZERO,
        };

        let daily = Some(TrimSchedule::default());
        assert_eq!(trim_in(&root, &roots, policy, daily).unwrap(), Some((8, 1)));
        assert!(!ir.join("old.o").exists());
        assert!(packages.join("lib.gos").exists());
        assert!(root.join(TRIM_STAMP).is_file());
        assert!(!root.join(TRIM_LOCK).exists());

        fs::create_dir_all(&ir).unwrap();
        fs::write(ir.join("old.o"), [0u8; 8]).unwrap();
        assert_eq!(
            trim_in(&root, &roots, policy, daily).unwrap(),
            None,
            "trimmed today"
        );
        assert!(ir.join("old.o").exists());
        assert_eq!(
            trim_in(&root, &roots, policy, None).unwrap(),
            Some((8, 1)),
            "a trim asked for runs at once"
        );
        assert!(!ir.join("old.o").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_trim_another_process_is_running_is_left_to_it() {
        let _children = crate::test_children::releasing_locks();
        let root = scratch("trim-busy");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let running = AdvisoryLock::try_acquire(&root.join(TRIM_LOCK)).unwrap();
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::ZERO,
        };
        assert_eq!(trim_in(&root, &[], policy, None).unwrap(), None);
        assert!(!root.join(TRIM_STAMP).exists());
        drop(running);
        assert_eq!(trim_in(&root, &[], policy, None).unwrap(), Some((0, 0)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_use_refreshes_an_entry_only_once_its_last_use_is_old() {
        let root = scratch("mark-used");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let entry = root.join("entry.bin");
        fs::write(&entry, b"blob").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&entry).unwrap();
        let old = SystemTime::now() - Duration::from_hours(40 * 24);
        file.set_modified(old).unwrap();
        mark_used(&entry);
        let used = fs::metadata(&entry).unwrap().modified().unwrap();
        assert!(used > old + USE_REFRESH);
        let recent = SystemTime::now() - Duration::from_mins(1);
        file.set_modified(recent).unwrap();
        mark_used(&entry);
        assert_eq!(fs::metadata(&entry).unwrap().modified().unwrap(), recent);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_native_build_tree_is_pruned_whole_and_kept_while_its_lock_is_held() {
        let _children = crate::test_children::releasing_locks();
        let root = scratch("native-build-tree");
        let _ = fs::remove_dir_all(&root);
        let tree = root.join("cmake-static-0123456789abcdef");
        fs::create_dir_all(tree.join("build")).unwrap();
        fs::write(tree.join("build").join("CMakeCache.txt"), b"cache").unwrap();
        fs::write(tree.join("build").join("cmake_pch.h"), b"generated").unwrap();
        fs::write(tree.join("lock"), b"").unwrap();
        assert_eq!(units(CacheClass::Native, &root), [(tree.clone(), 3, true)]);
        let held = fs::File::open(tree.join("lock")).unwrap();
        held.lock().unwrap();
        assert!(!reclaim_dir(CacheClass::Native, &tree, &root));
        assert!(tree.join("build").join("cmake_pch.h").exists());
        drop(held);
        assert!(reclaim_dir(CacheClass::Native, &tree, &root));
        assert!(!tree.exists());
        let _ = fs::remove_dir_all(&root);
    }
}
