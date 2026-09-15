//! Path-resolution + filesystem helpers shared by every subcommand.
//!
//! Centralising these here keeps `main.rs` minimal and gives every
//! subcommand the same project-root / source-discovery behaviour.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use gossamer_pkg::{Manifest, find_manifest};

/// `true` when ANSI colour escapes should be written to stderr.
/// Honours `NO_COLOR` and `CLICOLOR=0`; otherwise tests for a TTY.
pub(crate) fn stderr_supports_colour() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if matches!(std::env::var("CLICOLOR").as_deref(), Ok("0")) {
        return false;
    }
    std::io::stderr().is_terminal()
}

/// Reads `file` (after `.gos`-resolution) into a `String`. Wraps
/// the OS error in [`friendly_io_error`] so the diagnostic stream
/// stays free of libc artefacts.
pub(crate) fn read_source(file: &Path) -> Result<String> {
    let resolved = resolve_gos_source(file);
    fs::read_to_string(&resolved).map_err(|err| friendly_io_error(err, &resolved))
}

/// Reads `file` and auto-bundles every sibling `*.gos` in the same
/// directory by wrapping each in `mod NAME { ... }` and appending
/// it to the entry source. Used by entry-point commands
/// (`gos`, `gos build`) so cross-module calls
/// (`other::greet()` in `main.gos` referencing
/// `src/other.gos::greet`) resolve at runtime. See
/// [`gossamer_pkg::bundle`] for the bundling contract.
pub(crate) fn read_entry_source(file: &Path) -> Result<String> {
    Ok(read_entry_unit(file)?.source)
}

/// An entry's assembled compilation unit, plus the provenance needed to
/// report a diagnostic against the file its bytes were written in rather
/// than the assembled unit they were checked in.
pub(crate) struct EntryUnit {
    pub(crate) source: String,
    /// The entry path the unit was assembled from, matching the
    /// `origin` of every span that came from the entry itself.
    pub(crate) entry: PathBuf,
    pub(crate) origins: Vec<gossamer_pkg::bundle::BundledSpan>,
}

/// As [`read_entry_source`], keeping the assembled unit's provenance.
pub(crate) fn read_entry_unit(file: &Path) -> Result<EntryUnit> {
    // A bare relative entry (`gos run main.gos`) has an empty
    // `parent()`; the module scan must read the entry's real
    // directory, so anchor the path to the cwd first.
    let resolved =
        std::path::absolute(resolve_gos_source(file)).unwrap_or_else(|_| resolve_gos_source(file));
    let entry = fs::read_to_string(&resolved).map_err(|err| friendly_io_error(err, &resolved))?;
    let (source, origins) = gossamer_pkg::bundle::bundle_entry_source_traced(&resolved, entry);
    Ok(EntryUnit {
        source,
        entry: resolved,
        origins,
    })
}

/// Registers every file `unit` was assembled from and records which of
/// its regions came from which, so a diagnostic raised against the unit
/// resolves to the file the user wrote. Regions from the entry itself
/// already carry the right name and stay pointed at `unit`.
pub(crate) fn register_unit_origins(
    map: &mut gossamer_lex::SourceMap,
    unit: gossamer_lex::FileId,
    entry: &Path,
    spans: &[gossamer_pkg::bundle::BundledSpan],
) {
    if spans.iter().all(|span| span.origin == entry) {
        return;
    }
    let mut ids: Vec<(PathBuf, gossamer_lex::FileId)> = vec![(entry.to_path_buf(), unit)];
    let mut origins = Vec::with_capacity(spans.len());
    for span in spans {
        let known = ids
            .iter()
            .find(|(path, _)| *path == span.origin)
            .map(|(_, id)| *id);
        let id = if let Some(id) = known {
            id
        } else {
            let Ok(text) = fs::read_to_string(&span.origin) else {
                continue;
            };
            let id = map.add_file(span.origin.to_string_lossy().into_owned(), text);
            ids.push((span.origin.clone(), id));
            id
        };
        origins.push(gossamer_lex::OriginSpan {
            start: span.start,
            end: span.end,
            origin: id,
            origin_start: span.origin_start,
        });
    }
    map.set_origins(unit, origins);
}

/// Name the source map gives the code autoderive synthesizes for a unit.
const GENERATED_FILE: &str = "<generated>";

/// Records the trailing `generated_len` bytes of `unit` - the code autoderive
/// appends after the program - as a file of their own.
///
/// A position in that code then resolves to a line within it, so a generated
/// method keeps its position, and a debug build its cached object, whatever
/// edits change the length of the files assembled before it.
pub(crate) fn register_generated_tail(
    map: &mut gossamer_lex::SourceMap,
    unit: gossamer_lex::FileId,
    generated_len: usize,
) {
    let source = map.source(unit);
    let Some(start) = source.len().checked_sub(generated_len) else {
        return;
    };
    if generated_len == 0 || !source.is_char_boundary(start) {
        return;
    }
    let (Ok(start), Ok(end)) = (u32::try_from(start), u32::try_from(source.len())) else {
        return;
    };
    let tail = source[start as usize..].to_string();
    let generated = map.add_file(GENERATED_FILE, tail);
    map.add_origin(
        unit,
        gossamer_lex::OriginSpan {
            start,
            end,
            origin: generated,
            origin_start: 0,
        },
    );
}

/// Renders a `std::io::Error` as a clean diagnostic free of
/// libc artefacts (`(os error N)` tails, `stat`/`reading`
/// syscall prefixes). Path-aware where a path is available.
pub(crate) fn friendly_io_error(err: std::io::Error, path: &Path) -> anyhow::Error {
    use std::io::ErrorKind;
    let display = path.display();
    let msg = match err.kind() {
        ErrorKind::NotFound => format!("file not found: {display}"),
        ErrorKind::PermissionDenied => format!("permission denied: {display}"),
        ErrorKind::IsADirectory => format!("expected a file, found a directory: {display}"),
        ErrorKind::NotADirectory => format!("expected a directory, found a file: {display}"),
        ErrorKind::AlreadyExists => format!("already exists: {display}"),
        ErrorKind::InvalidData => format!("invalid file contents: {display}"),
        ErrorKind::TimedOut => format!("timed out reading {display}"),
        ErrorKind::WriteZero => format!("could not write to {display}"),
        // Other (genuinely surprising) errors keep the kind name
        // so the user has something to grep for, but still drop
        // the libc `(os error N)` tail that std prepends.
        kind => format!("{display}: {kind:?}"),
    };
    anyhow!(msg)
}

/// Resolves a source argument while preserving explicit user intent.
/// Existing files are read exactly as named, regardless of extension.
/// A missing extensionless path may still resolve to `path.gos` as a
/// convenience for `gos run foo`.
pub(crate) fn resolve_gos_source(path: &Path) -> PathBuf {
    if path.exists() {
        return path.to_path_buf();
    }
    if path.extension().is_none() {
        let with_ext = path.with_extension("gos");
        if with_ext.exists() {
            return with_ext;
        }
    }
    path.to_path_buf()
}

/// Current-project filesystem context discovered from the process cwd.
///
/// Commands often need the nearest `project.toml` more than once during a
/// single invocation (runner dispatch, source entry resolution, static binding
/// setup). This memoizes only the manifest lookup and parse; source files and
/// dependency bundles still read fresh from disk at their existing call sites.
#[derive(Debug, Clone)]
pub(crate) struct ProjectContext {
    pub(crate) cwd: PathBuf,
    pub(crate) manifest_path: Option<PathBuf>,
    manifest: Option<std::result::Result<Manifest, String>>,
}

impl ProjectContext {
    pub(crate) fn manifest_dir(&self) -> Option<PathBuf> {
        self.manifest_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    }

    pub(crate) fn manifest_result(&self) -> Option<std::result::Result<&Manifest, &str>> {
        self.manifest
            .as_ref()
            .map(|result| result.as_ref().map_err(String::as_str))
    }
}

fn load_project_context(start: &Path) -> ProjectContext {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let anchor = if start.is_absolute() {
        start.to_path_buf()
    } else {
        cwd.join(start)
    };
    let manifest_path = find_manifest(&anchor);
    let manifest = manifest_path.as_ref().and_then(|path| {
        let text = fs::read_to_string(path).ok()?;
        Some(Manifest::parse(&text).map_err(|err| format!("{}: {err}", path.display())))
    });
    ProjectContext {
        cwd,
        manifest_path,
        manifest,
    }
}

/// Returns the memoized current-project context, recomputing when cwd changes.
pub(crate) fn project_context() -> ProjectContext {
    static CACHE: Mutex<Option<ProjectContext>> = Mutex::new(None);

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut guard = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(ctx) = guard.as_ref()
        && ctx.cwd == cwd
    {
        return ctx.clone();
    }

    let ctx = load_project_context(&cwd);
    *guard = Some(ctx.clone());
    ctx
}

/// Returns the project context containing `entry`, independent of the process
/// cwd. Explicit path builds/checks use this so project-local bindings follow
/// the source file the user named.
pub(crate) fn project_context_for_entry(entry: &Path) -> ProjectContext {
    load_project_context(entry)
}

/// Whether the project asks `gos test` to fail on non-canonical
/// formatting (`project.enforce-format`). False outside a project.
#[must_use]
pub(crate) fn project_enforces_format() -> bool {
    project_context()
        .manifest_result()
        .and_then(Result::ok)
        .is_some_and(|manifest| manifest.project.enforce_format)
}

/// Walks up from the cwd looking for the nearest `project.toml`.
/// Returns the directory that contains it (the project root).
pub(crate) fn find_project_root() -> Option<PathBuf> {
    project_context().manifest_dir()
}

/// Default source root for whole-project commands (`check`, `lint`,
/// `test`): the project root's `src/` if present, otherwise the
/// project root itself, otherwise the current directory.
pub(crate) fn default_test_root() -> Result<PathBuf> {
    if let Some(root) = find_project_root() {
        let src = root.join("src");
        if src.is_dir() {
            return Ok(src);
        }
        return Ok(root);
    }
    std::env::current_dir().context("read current directory")
}

/// Resolves an entry-point command argument to a concrete `.gos` file.
/// `None` uses the nearest project's conventional entry; a directory
/// argument resolves that directory's project entry (so `gos
/// my_project` works); a file argument is used as given.
pub(crate) fn resolve_entry_arg(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        None => default_main_entry(),
        Some(p) if p.is_dir() => resolve_project_entry(&p),
        Some(p) => Ok(p),
    }
}

/// Returns the manifest root containing `entry`, when it belongs to a project.
/// Used by the development supervisor to watch the complete local source tree.
pub(crate) fn project_root_for_entry(entry: &Path) -> Option<PathBuf> {
    entry.parent().and_then(|dir| {
        dir.ancestors()
            .find(|candidate| candidate.join("project.toml").is_file())
            .map(Path::to_path_buf)
    })
}

/// Discovers every transitive local `path` dependency of `entry`'s project.
/// This shares the traversal used by [`read_entry_source`] so the development
/// watcher observes exactly the source trees included in a bundled revision.
pub(crate) fn local_path_dependency_roots(entry: &Path) -> Vec<PathBuf> {
    let mut visited = Vec::new();
    let mut worklist = Vec::new();
    gossamer_pkg::bundle::collect_path_deps(entry, &mut visited, &mut worklist);
    while let Some((_, dep_entry)) = worklist.pop() {
        gossamer_pkg::bundle::collect_path_deps(&dep_entry, &mut visited, &mut worklist);
    }
    visited
}

/// Default entry point for whole-project run/build commands.
/// Resolves via [`resolve_project_entry`] from the nearest project
/// root; returns `Err` with a useful diagnostic otherwise.
pub(crate) fn default_main_entry() -> Result<PathBuf> {
    let root = find_project_root().ok_or_else(|| {
        anyhow!(
            "no project.toml found above the current directory; pass a path or run from inside a project"
        )
    })?;
    resolve_project_entry(&root)
}

/// Entry-point resolution for a project root, as
/// [`gossamer_pkg::resolve_project_entry`] defines it.
///
/// # Errors
///
/// Propagates the package layer's diagnostic when the root declares a
/// missing entry, holds several candidates, or holds none.
pub(crate) fn resolve_project_entry(root: &Path) -> Result<PathBuf> {
    gossamer_pkg::resolve_project_entry(root).map_err(|err| anyhow!("{err}"))
}

/// Collapses every target that belongs to a project down to that project's
/// entry, keeping loose sources as themselves. The result is deduplicated
/// and keeps the sweep's order, so each project is checked once, as the unit
/// `gos run` and `gos build` compile it.
pub(crate) fn group_targets_by_project(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(files.len());
    for file in files {
        let target = enclosing_project_entry(file).unwrap_or_else(|| file.clone());
        if !out.contains(&target) {
            out.push(target);
        }
    }
    out
}

/// The entry of the nearest project above `file`, when it has one.
pub(crate) fn enclosing_project_entry(file: &Path) -> Option<PathBuf> {
    gossamer_pkg::enclosing_project_entry(file)
}

/// Recursively gathers every `.gos` file under `root`. If `root`
/// names a single file, returns it as a one-element list.
pub(crate) fn collect_lint_targets(root: &PathBuf) -> Result<Vec<PathBuf>> {
    let meta = fs::metadata(root).map_err(|e| friendly_io_error(e, root))?;
    if meta.is_file() {
        return Ok(vec![root.clone()]);
    }
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("read_dir {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("gos") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Picks the default binary name for a build, matching Rust's
/// `cargo build` rule: derive from the package name, not the
/// source filename. When the source sits inside a project (a
/// `project.toml` is found by walking parents), the binary takes
/// the last `/`-separated segment of `[project] id`. Loose-file
/// builds with no manifest fall back to the source stem.
pub(crate) fn default_unit_name(file: &Path) -> String {
    if let Some(manifest_path) = gossamer_pkg::find_manifest(file)
        && let Ok(text) = fs::read_to_string(&manifest_path)
        && let Ok(manifest) = gossamer_pkg::Manifest::parse(&text)
    {
        return manifest.project.id.tail().to_string();
    }
    file.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("main")
        .to_string()
}

/// Resolves the build output path.
///
/// Resolution order (first hit wins):
/// 1. `project.output` in the nearest enclosing `project.toml`
///    (relative paths resolve against the manifest's directory).
/// 2. `<project-root>/target/{debug,release}/<unit>`.
/// 3. `<source-dir>/target/{debug,release}/<unit>` for loose-file
///    builds with no manifest.
///
/// `target_is_windows` names the *produced binary's* OS, which is the
/// host's only for a native build - a `--target` cross build is always
/// Linux (the only OS `gos build` can cross-produce), regardless of which
/// OS the compiler itself runs on. Using `cfg!(windows)` (the host) here
/// unconditionally would misname a Linux binary cross-built from a
/// Windows host with a trailing `.exe`.
pub(crate) fn resolve_output_path(
    file: &Path,
    unit_name: &str,
    release: bool,
    target_is_windows: bool,
) -> Result<PathBuf> {
    if let Some(manifest_path) = gossamer_pkg::find_manifest(file) {
        let manifest_text =
            fs::read_to_string(&manifest_path).map_err(|e| friendly_io_error(e, &manifest_path))?;
        let manifest = gossamer_pkg::Manifest::parse(&manifest_text)
            .with_context(|| format!("parsing {}", manifest_path.display()))?;
        if let Some(output) = manifest.project.output {
            let mut raw = PathBuf::from(&output);
            // A manifest `output` with no extension still needs the
            // platform executable suffix on Windows, or the linker
            // writes a non-runnable extensionless file. An explicit
            // extension (`tool.exe`, `tool.bin`) is left untouched.
            if target_is_windows && raw.extension().is_none() {
                raw.set_extension("exe");
            }
            let resolved = if raw.is_absolute() {
                raw
            } else {
                manifest_path
                    .parent()
                    .map_or_else(|| raw.clone(), |dir| dir.join(&raw))
            };
            return Ok(resolved);
        }
        if let Some(root) = manifest_path.parent() {
            let profile = if release { "release" } else { "debug" };
            let target_dir = root.join("target").join(profile);
            fs::create_dir_all(&target_dir)
                .map_err(|e| anyhow!("creating {}: {e}", target_dir.display()))?;
            stamp_build_dir(&root.join("target"));
            return Ok(target_dir.join(platform_exe_name(unit_name, target_is_windows)));
        }
    }
    let parent = file.parent().filter(|p| !p.as_os_str().is_empty());
    let base = parent.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let profile = if release { "release" } else { "debug" };
    let target_dir = base.join("target").join(profile);
    fs::create_dir_all(&target_dir)
        .map_err(|e| anyhow!("creating {}: {e}", target_dir.display()))?;
    stamp_build_dir(&base.join("target"));
    Ok(target_dir.join(platform_exe_name(unit_name, target_is_windows)))
}

/// Name of the stamp `gos build` leaves at the root of a `target/`
/// directory it writes into.
pub(crate) const BUILD_DIR_STAMP: &str = ".gos-build";

/// Marks `dir` as a `target/` directory this toolchain writes binaries
/// into, so `gos clean` can tell one it owns from one another build
/// system created under the same conventional name. A stamp that cannot
/// be written is not an error: the only consequence is that `gos clean`
/// leaves the directory alone.
pub(crate) fn stamp_build_dir(dir: &Path) {
    let stamp = dir.join(BUILD_DIR_STAMP);
    if stamp.exists() {
        return;
    }
    let _ = fs::write(
        &stamp,
        b"This directory holds `gos build` output and is removed by `gos clean`.\n",
    );
}

/// Binary name with the correct platform extension: `stem.exe` when the
/// *produced binary's* target OS is Windows, bare `stem` otherwise. See
/// [`resolve_output_path`] for why this is not simply `cfg!(windows)`.
pub(crate) fn platform_exe_name(stem: &str, target_is_windows: bool) -> String {
    if target_is_windows {
        format!("{stem}.exe")
    } else {
        stem.to_owned()
    }
}

/// Returns the path the REPL uses to persist line-edit history
/// across sessions. Prefers `$GOSSAMER_HISTORY` → `$XDG_STATE_HOME/
/// gossamer/history` → `$HOME/.gossamer_history`. `None` is returned
/// only when no reasonable home directory can be discovered, in
/// which case history is kept in-memory for the current session.
pub(crate) fn repl_history_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("GOSSAMER_HISTORY") {
        return Some(PathBuf::from(explicit));
    }
    if let Ok(state) = std::env::var("XDG_STATE_HOME") {
        let mut path = PathBuf::from(state);
        path.push("gossamer");
        let _ = fs::create_dir_all(&path);
        path.push("history");
        return Some(path);
    }
    if let Ok(home) = std::env::var("HOME") {
        let mut path = PathBuf::from(home);
        path.push(".gossamer_history");
        return Some(path);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_project(tag: &str, manifest: &str, files: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("gos-entry-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("project.toml"), manifest).unwrap();
        for file in files {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "fn main() { }\n").unwrap();
        }
        root
    }

    const MANIFEST: &str = "[project]\nid = \"example.com/widget\"\nversion = \"0.1.0\"\n";

    #[test]
    fn entry_prefers_src_main() {
        let root = scratch_project("srcmain", MANIFEST, &["src/main.gos", "widget.gos"]);
        assert_eq!(
            resolve_project_entry(&root).unwrap(),
            root.join("src").join("main.gos")
        );
    }

    #[test]
    fn entry_falls_back_to_manifest_id_name() {
        let root = scratch_project("idname", MANIFEST, &["widget.gos", "helper.gos"]);
        assert_eq!(
            resolve_project_entry(&root).unwrap(),
            root.join("widget.gos")
        );
    }

    #[test]
    fn entry_uses_sole_candidate() {
        let root = scratch_project(
            "sole",
            MANIFEST,
            &["tool.gos", "_scratch.gos", "x_test.gos"],
        );
        assert_eq!(resolve_project_entry(&root).unwrap(), root.join("tool.gos"));
    }

    #[test]
    fn entry_project_context_comes_from_entry_not_cwd() {
        let root = scratch_project(
            "entryctx",
            &format!(
                "[project]\nid = \"example.com/entryctx\"\nversion = \"0.1.0\"\n\
                 gossamer-version = \"v{}\"\n[rust-bindings]\naddlib = {{ path = \"bindings/addlib\" }}\n",
                env!("CARGO_PKG_VERSION")
            ),
            &["src/main.gos"],
        );
        let ctx = project_context_for_entry(&root.join("src").join("main.gos"));
        assert_eq!(ctx.manifest_dir().as_deref(), Some(root.as_path()));
        assert!(
            ctx.manifest_result()
                .and_then(std::result::Result::ok)
                .is_some_and(|manifest| manifest.rust_bindings.contains_key("addlib"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn entry_ambiguity_lists_candidates() {
        let root = scratch_project("ambig", MANIFEST, &["alpha.gos", "beta.gos"]);
        let err = resolve_project_entry(&root).unwrap_err().to_string();
        assert!(err.contains("alpha.gos"), "missing candidate in: {err}");
        assert!(err.contains("beta.gos"), "missing candidate in: {err}");
    }

    #[test]
    fn entry_errors_when_no_sources() {
        let root = scratch_project("empty", MANIFEST, &[]);
        let err = resolve_project_entry(&root).unwrap_err().to_string();
        assert!(err.contains("no .gos source"), "unexpected: {err}");
    }

    #[test]
    fn source_resolution_preserves_existing_explicit_paths() {
        let dir = std::env::temp_dir().join(format!("gos-source-path-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let extensionless = dir.join("tool");
        let explicit = dir.join("tool.py");
        let inferred = dir.join("tool.gos");
        for path in [&extensionless, &explicit, &inferred] {
            fs::write(path, "fn main() {}\n").unwrap();
        }

        assert_eq!(resolve_gos_source(&extensionless), extensionless);
        assert_eq!(resolve_gos_source(&explicit), explicit);
        let missing_extensionless = dir.join("missing");
        let missing_gos = dir.join("missing.gos");
        fs::write(&missing_gos, "fn main() {}\n").unwrap();
        assert_eq!(resolve_gos_source(&missing_extensionless), missing_gos);

        let _ = fs::remove_dir_all(&dir);
    }
}
