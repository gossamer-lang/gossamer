//! Assembles a project's compilation unit from its on-disk layout.
//!
//! An entry file's siblings and subdirectory `mod.gos` packages become
//! inline `mod NAME { ... }` items appended to the entry source, and
//! every dependency is inlined the same way - a `path = "..."` one from
//! where it points, a git, registry, or tarball one from the source tree
//! `gos fetch` or `gos vendor` prepared for it (SPEC 6.7: the compiler
//! reads a prepared tree and never fetches code itself). Every
//! front end that type-checks project code - the CLI and the language
//! server alike - assembles the same unit, so a cross-module reference
//! resolves identically in an editor and on the command line.
//!
//! Appending (never prefixing) keeps every byte offset in the entry
//! source unchanged, so diagnostics map back to the file the user is
//! editing.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::cache::{cached_source_dir, default_cache_root};
use crate::lockfile::Lockfile;
use crate::resolver::dependency_identity;
use crate::{DependencySpec, InlineDependency, Manifest};

/// A byte range of an assembled unit and the file its bytes were read
/// from. Bodies are inlined verbatim - `neutralize_external_mod_decls`
/// blanks a declaration in place rather than resizing it - so a position
/// `p` inside `start..end` sits at `origin_start + (p - start)` in
/// `origin`, which is what maps a diagnostic back to the file the user
/// wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledSpan {
    /// First byte of the region in the bundled text.
    pub start: u32,
    /// One past the last byte of the region in the bundled text.
    pub end: u32,
    /// File the region's bytes were read from.
    pub origin: PathBuf,
    /// Byte offset of `start` within `origin`.
    pub origin_start: u32,
}

/// Returns `spans` with every range moved `by` bytes further into the
/// text they are being embedded in.
fn shift_spans(spans: Vec<BundledSpan>, by: usize) -> Vec<BundledSpan> {
    let by = u32::try_from(by).unwrap_or(u32::MAX);
    spans
        .into_iter()
        .map(|span| BundledSpan {
            start: span.start.saturating_add(by),
            end: span.end.saturating_add(by),
            origin: span.origin,
            origin_start: span.origin_start,
        })
        .collect()
}

/// The whole of `text` attributed to `origin`.
fn whole_file_span(origin: &Path, text: &str) -> BundledSpan {
    BundledSpan {
        start: 0,
        end: u32::try_from(text.len()).unwrap_or(u32::MAX),
        origin: origin.to_path_buf(),
        origin_start: 0,
    }
}

/// Inlines every `path = "..."` dependency of the entry's project as
/// a top-level `mod <dep-module-name> { <dep source> }`. Transitive
/// path dependencies hoist to top-level modules too (deduplicated by
/// canonical root), so a dependency's own `use "id" as alias` binds
/// against a sibling module exactly as the consumer's does. The
/// module name derives from the dependency's project id via
/// `gossamer_resolve::project_dep_module_name`, which is also what
/// the resolver binds `use "id" as alias` against. Non-path
/// dependencies are untouched.
#[must_use]
pub fn bundle_path_dependencies(
    entry: &Path,
    source: String,
    visited: &mut Vec<PathBuf>,
) -> String {
    bundle_path_dependencies_traced(entry, source, visited).0
}

/// As [`bundle_path_dependencies`], also reporting which file each region
/// of the result came from.
#[must_use]
pub fn bundle_path_dependencies_traced(
    entry: &Path,
    source: String,
    visited: &mut Vec<PathBuf>,
) -> (String, Vec<BundledSpan>) {
    let mut out = source;
    let mut spans = Vec::new();
    let mut worklist: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut dep_modules: BTreeMap<String, String> = BTreeMap::new();
    collect_path_deps_with_modules(entry, visited, &mut worklist, &mut dep_modules);
    let mut i = 0;
    while i < worklist.len() {
        let (dep_root, dep_entry) = worklist[i].clone();
        i += 1;
        let Some((dep_id, _)) = path_dep_entry(&dep_root) else {
            continue;
        };
        let Ok(dep_source) = fs::read_to_string(&dep_entry) else {
            continue;
        };
        let (dep_bundled, dep_spans) = bundle_sibling_modules_traced(&dep_entry, dep_source);
        collect_path_deps_with_modules(&dep_entry, visited, &mut worklist, &mut dep_modules);
        // A `module = "..."` override in the manifest that declares the
        // dependency names the module its source is reached under; without
        // one the final segment of its id does.
        let mod_name = dep_modules
            .get(&dep_id)
            .cloned()
            .unwrap_or_else(|| gossamer_resolve::project_dep_module_name(&dep_id));
        // The attribute is what tells the resolver this module came from
        // another package, so a reference to it needs the matching import.
        let header = format!(
            "\n// auto-bundled dependency: {} ({})\n#[dependency(\"{}\")]\nmod {} {{\n",
            dep_id,
            dep_root.display(),
            dep_id,
            mod_name
        );
        out.push_str(&header);
        spans.extend(shift_spans(dep_spans, out.len()));
        out.push_str(&dep_bundled);
        out.push_str("\n}\n");
    }
    (out, spans)
}

/// Appends the (root, entry) of each not-yet-visited path dependency
/// of `entry`'s project to `worklist`.
///
/// `visited` holds canonical roots, so two spellings of one directory
/// are the same dependency. The root handed to `worklist` keeps the
/// spelling the manifest used, which is the form that reaches the user
/// in diagnostics and bundle comments.
pub fn collect_path_deps(
    entry: &Path,
    visited: &mut Vec<PathBuf>,
    worklist: &mut Vec<(PathBuf, PathBuf)>,
) {
    collect_path_deps_with_modules(entry, visited, worklist, &mut BTreeMap::new());
}

/// As [`collect_path_deps`], also recording each dependency's
/// `module = "..."` override from the manifest that declares it.
pub fn collect_path_deps_with_modules(
    entry: &Path,
    visited: &mut Vec<PathBuf>,
    worklist: &mut Vec<(PathBuf, PathBuf)>,
    modules: &mut BTreeMap<String, String>,
) {
    let Some(dir) = entry.parent() else {
        return;
    };
    let manifest_path = [dir.join("project.toml")]
        .into_iter()
        .chain(dir.parent().map(|p| p.join("project.toml")))
        .find(|p| p.is_file());
    let Some(manifest_path) = manifest_path else {
        return;
    };
    let Ok(manifest_text) = fs::read_to_string(&manifest_path) else {
        return;
    };
    let Ok(manifest) = Manifest::parse(&manifest_text) else {
        return;
    };
    let manifest_dir = manifest_path.parent().unwrap_or(dir);
    for (id, module) in &manifest.dependency_modules {
        modules.insert(id.clone(), module.clone());
    }
    for (key, spec) in &manifest.dependencies {
        let Some(spelled) = dependency_path(spec).map_or_else(
            || prepared_dependency_root(manifest_dir, key, spec),
            |rel| Some(manifest_dir.join(rel)),
        ) else {
            continue;
        };
        let Ok(identity) = spelled.canonicalize() else {
            continue;
        };
        if visited.contains(&identity) {
            continue;
        }
        visited.push(identity);
        let dep_root = lexically_normalized(&spelled);
        if let Some((_, dep_entry)) = path_dep_entry(&dep_root) {
            worklist.push((dep_root, dep_entry));
        }
    }
}

/// Resolves `.` and `..` in `path` without consulting the filesystem,
/// so a dependency's files are reported under the path its manifest
/// spelled rather than the platform's canonical alias for it.
fn lexically_normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    out
}

/// The directory holding a fetched dependency's source, for the sources
/// that are not read live from a local path. `gos vendor` writes one under
/// the project's own `vendor/`, and `gos fetch` writes one into the shared
/// package cache, keyed by the digest the lockfile pins. A dependency with
/// neither prepared yet has no source to compile against.
fn prepared_dependency_root(
    manifest_dir: &Path,
    key: &str,
    spec: &DependencySpec,
) -> Option<PathBuf> {
    let id = dependency_identity(key, spec, Some(manifest_dir)).ok()?;
    let vendored = manifest_dir
        .join("vendor")
        .join(id.as_str().replace('/', "__"));
    if vendored.join("project.toml").is_file() {
        return Some(vendored);
    }
    let lock = Lockfile::load(manifest_dir).ok().flatten()?;
    let digest = lock
        .entries
        .iter()
        .find(|entry| entry.resolved.id == id)?
        .sha256
        .as_deref()?;
    let cached = cached_source_dir(&default_cache_root()?, digest)?;
    cached.join("project.toml").is_file().then_some(cached)
}

/// The `path` field of a dependency spec, when it is a local-path
/// dependency.
fn dependency_path(spec: &DependencySpec) -> Option<&str> {
    match spec {
        DependencySpec::Inline(InlineDependency::Path { path }) => Some(path),
        _ => None,
    }
}

/// Resolves a path dependency's project id and entry source file:
/// `lib.gos` (flat or under `src/`) is the library entry; `main.gos`
/// is accepted as a fallback for binary-shaped projects.
fn path_dep_entry(dep_root: &Path) -> Option<(String, PathBuf)> {
    let manifest_text = fs::read_to_string(dep_root.join("project.toml")).ok()?;
    let manifest = Manifest::parse(&manifest_text).ok()?;
    let entry = manifest
        .project
        .entry
        .as_deref()
        .map(|rel| dep_root.join(rel))
        .into_iter()
        .chain([
            dep_root.join("src/lib.gos"),
            dep_root.join("lib.gos"),
            dep_root.join("src/main.gos"),
            dep_root.join("main.gos"),
        ])
        .find(|p| p.is_file())?;
    Some((manifest.project.id.as_str().to_string(), entry))
}

/// One auto-bundled module: its `name`, its already-assembled `body`,
/// and the source path it came from (for the bundle comment).
struct BundledModule {
    name: String,
    body: String,
    origin: PathBuf,
    /// Provenance of `body`, relative to its own first byte.
    spans: Vec<BundledSpan>,
}

/// The editor's unsaved text for one file of the unit, substituted for
/// that file's on-disk contents wherever the bundler would read it.
///
/// A language server holds the buffer the user is typing into, which the
/// filesystem has not seen yet; every other file of the unit still reads
/// from disk.
#[derive(Debug, Clone, Copy, Default)]
pub struct Overlay<'a> {
    entry: Option<(&'a Path, &'a str)>,
}

impl<'a> Overlay<'a> {
    /// An overlay substituting `text` for the contents of `path`.
    #[must_use]
    pub fn new(path: &'a Path, text: &'a str) -> Self {
        Self {
            entry: Some((path, text)),
        }
    }

    /// The contents of `path`: the overlaid text when it names the
    /// overlaid file, and the file's own bytes otherwise.
    fn read(self, path: &Path) -> Option<String> {
        match self.entry {
            Some((overlaid, text)) if same_file(overlaid, path) => Some(text.to_string()),
            _ => fs::read_to_string(path).ok(),
        }
    }
}

/// Whether two paths name one file. The bundler walks a project from its
/// entry with `Path::join`, so the paths it builds share the entry's
/// spelling; an editor's URI may spell the same file with a different
/// prefix, which `canonicalize` reconciles.
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Auto-bundles a multi-file package into the entry source so the
/// resolver sees one inline module tree. Every sibling `*.gos` file in
/// the entry's directory becomes `pub mod <stem> { ... }`, and every
/// subdirectory holding a `mod.gos` becomes `pub mod <dir> { ... }` whose
/// body is that `mod.gos` plus its own files and subdirectories,
/// recursively. The entry file itself, `_`-prefixed scratch files, and
/// `*_test.gos` files are skipped. A `mod NAME;` declaration for a
/// bundled module is rewritten to a comment so the synthetic inline
/// body is the sole definition.
///
/// This is the "sibling auto-bundle" contract: items inside
/// `src/<name>.gos` are reachable from `src/main.gos` as `name::item`,
/// items inside `src/<dir>/mod.gos` as `dir::item`, and a module
/// reaches a sibling module via `super::sibling::item`.
#[must_use]
pub fn bundle_sibling_modules(entry: &Path, source: String) -> String {
    bundle_sibling_modules_traced(entry, source).0
}

/// As [`bundle_sibling_modules`], reading `overlay`'s file from the
/// editor's buffer rather than from disk.
#[must_use]
pub fn bundle_sibling_modules_overlaid(
    entry: &Path,
    source: String,
    overlay: Overlay<'_>,
) -> (String, Vec<BundledSpan>) {
    bundle_sibling_modules_inner(entry, source, overlay)
}

/// As [`bundle_sibling_modules`], also reporting which file each region
/// of the result came from. The entry's own region is reported too, so a
/// nested unit (a path dependency's own package) stays attributable once
/// it is embedded in a larger one.
#[must_use]
pub fn bundle_sibling_modules_traced(entry: &Path, source: String) -> (String, Vec<BundledSpan>) {
    bundle_sibling_modules_inner(entry, source, Overlay::default())
}

fn bundle_sibling_modules_inner(
    entry: &Path,
    source: String,
    overlay: Overlay<'_>,
) -> (String, Vec<BundledSpan>) {
    let Some(dir) = entry.parent() else {
        let span = whole_file_span(entry, &source);
        return (source, vec![span]);
    };
    // Auto-bundle only fires inside an actual project - i.e. when a
    // `project.toml` lives next to the entry's directory or one
    // level up (`<root>/src/main.gos` is the canonical case). Loose
    // single-file `gos run /tmp/foo.gos` invocations must NOT pick
    // up unrelated `.gos` files sitting in the same directory.
    if !is_inside_project(dir) {
        let span = whole_file_span(entry, &source);
        return (source, vec![span]);
    }
    let entry_stem = entry.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let mut modules = collect_package_modules(dir, Some(entry_stem), overlay);
    // An integration test lives beside the package rather than inside it, so
    // the crate's own modules are not its siblings. Bundle them too, which is
    // what `use crate::<module>` in a `tests/` file names.
    if let Some(src) = crate_src_dir_for_tests(dir) {
        let existing: Vec<String> = modules.iter().map(|m| m.name.clone()).collect();
        // The package entry is the program's own root, not a module an
        // integration test reaches through `crate::`; inlining it would bring
        // its imports into a scope that already has them.
        for module in collect_package_modules(&src, Some("main"), overlay) {
            if !existing.contains(&module.name) {
                modules.push(module);
            }
        }
        modules.sort_by(|a, b| a.name.cmp(&b.name));
    }
    if modules.is_empty() {
        let span = whole_file_span(entry, &source);
        return (source, vec![span]);
    }
    let names: Vec<&str> = modules.iter().map(|m| m.name.as_str()).collect();
    let mut bundled = neutralize_external_mod_decls(&source, &names);
    let mut spans = vec![whole_file_span(entry, &bundled)];
    for module in &modules {
        spans.extend(append_inline_module(&mut bundled, module));
    }
    (bundled, spans)
}

/// Collects the modules declared by the contents of `dir`: each
/// sibling `*.gos` file (a leaf module) and each subdirectory holding a
/// `mod.gos` (a nested module, assembled recursively). `skip_stem`
/// excludes the entry file at the top level; `mod.gos` is always
/// excluded here because it is the body of its own directory module,
/// not a sibling. Results are sorted by name for deterministic output.
fn collect_package_modules(
    dir: &Path,
    skip_stem: Option<&str>,
    overlay: Overlay<'_>,
) -> Vec<BundledModule> {
    let mut modules: Vec<BundledModule> = Vec::new();
    let Ok(read) = fs::read_dir(dir) else {
        return modules;
    };
    for dirent in read.flatten() {
        let path = dirent.path();
        if path.is_file() {
            if path.extension().and_then(|s| s.to_str()) != Some("gos") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem == "mod" || Some(stem) == skip_stem {
                continue;
            }
            if stem.starts_with('_') || stem.ends_with("_test") {
                continue;
            }
            if !is_valid_module_ident(stem) {
                continue;
            }
            let Some(body) = overlay.read(&path) else {
                continue;
            };
            let spans = vec![whole_file_span(&path, &body)];
            modules.push(BundledModule {
                name: stem.to_string(),
                body,
                origin: path,
                spans,
            });
        } else if path.is_dir() {
            // A subdirectory is a module only when it carries a
            // `mod.gos` root - the documented `src/<dir>/mod.gos`
            // convention. Directories without one (e.g. `target`)
            // are ignored.
            let mod_gos = path.join("mod.gos");
            if !mod_gos.is_file() {
                continue;
            }
            let Some(dir_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if dir_name.starts_with('_') || !is_valid_module_ident(dir_name) {
                continue;
            }
            let (body, spans) = assemble_dir_module(&path, overlay);
            modules.push(BundledModule {
                name: dir_name.to_string(),
                body,
                origin: mod_gos,
                spans,
            });
        }
    }
    modules.sort_by(|a, b| a.name.cmp(&b.name));
    modules
}

/// Builds the body of a directory module: its `mod.gos` contents with
/// any `mod NAME;` for a child neutralized, followed by each child file
/// and subdirectory inlined as a nested module.
fn assemble_dir_module(dir: &Path, overlay: Overlay<'_>) -> (String, Vec<BundledSpan>) {
    let mod_gos = dir.join("mod.gos");
    let root = overlay.read(&mod_gos).unwrap_or_default();
    let children = collect_package_modules(dir, None, overlay);
    let names: Vec<&str> = children.iter().map(|m| m.name.as_str()).collect();
    let mut body = neutralize_external_mod_decls(&root, &names);
    let mut spans = vec![whole_file_span(&mod_gos, &body)];
    for child in &children {
        spans.extend(append_inline_module(&mut body, child));
    }
    (body, spans)
}

/// Appends `pub mod <name> { <body> }` (with a provenance comment) to
/// `out`. The on-disk layout is the module's only declaration site, so
/// there is nowhere for a package author to write `pub`; emitting the
/// declaration as public keeps every module in the tree nameable from
/// the entry at any nesting depth, which is the layout's contract.
/// Item visibility is untouched - a module's own items still need `pub`
/// to be reachable from outside it.
/// Returns the module's provenance, rebased onto `out`.
fn append_inline_module(out: &mut String, module: &BundledModule) -> Vec<BundledSpan> {
    out.push('\n');
    out.push_str("// auto-bundled module: ");
    out.push_str(module.origin.to_string_lossy().as_ref());
    out.push('\n');
    out.push_str("pub mod ");
    out.push_str(&module.name);
    out.push_str(" {\n");
    let spans = shift_spans(module.spans.clone(), out.len());
    out.push_str(&module.body);
    out.push_str("\n}\n");
    spans
}

/// Comments out any line that exactly matches `mod NAME;` for one
/// of the supplied sibling stems. The regex shape is intentionally
/// narrow: only an unindented `mod NAME;` (with optional whitespace)
/// is rewritten, so a real inline `mod NAME { ... }` declaration
/// inside the entry source survives untouched.
fn neutralize_external_mod_decls(source: &str, sibling_stems: &[&str]) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        // A declaration is newline-terminated; the legacy `;` form still
        // parses (reported as GP0043), so both spellings are neutralized.
        let decl = trimmed
            .strip_prefix("pub mod ")
            .or_else(|| trimmed.strip_prefix("mod "));
        if let Some(rest) = decl {
            let name = rest.strip_suffix(';').unwrap_or(rest).trim();
            if sibling_stems.contains(&name) {
                // Blank the declaration in place rather than prefixing a
                // comment: an editor maps positions against this text, so
                // the rewrite must not move any byte that follows it on
                // the line.
                for ch in line.chars() {
                    out.push(if ch == '\n' || ch == '\r' { ch } else { ' ' });
                }
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

/// `true` when `dir` is the source root of a Gossamer project: a
/// `project.toml` lives in `dir` itself or in `dir`'s immediate
/// parent. Used by [`bundle_sibling_modules`] to refuse bundling in
/// loose-file invocations like `gos run /tmp/foo.gos`.
/// The package's `src` directory when `dir` is a project's `tests` directory.
/// A file there is compiled against the package, not against its own folder.
fn crate_src_dir_for_tests(dir: &Path) -> Option<PathBuf> {
    if dir.file_name().and_then(|n| n.to_str()) != Some("tests") {
        return None;
    }
    let root = dir.parent()?;
    if !root.join("project.toml").is_file() {
        return None;
    }
    let src = root.join("src");
    src.is_dir().then_some(src)
}

fn is_inside_project(dir: &Path) -> bool {
    if dir.join("project.toml").is_file() {
        return true;
    }
    if let Some(parent) = dir.parent() {
        if parent.join("project.toml").is_file() {
            return true;
        }
    }
    false
}

fn is_valid_module_ident(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    true
}

/// Assembles the compilation unit for `entry` from `source`: its
/// sibling / subdirectory modules first, then every `path = "..."`
/// dependency. `source` is the entry's text, which lets an editor pass
/// its unsaved buffer while the rest of the unit is read from disk.
#[must_use]
pub fn bundle_entry_source(entry: &Path, source: String) -> String {
    bundle_entry_source_traced(entry, source).0
}

/// As [`bundle_entry_source`], also reporting which file each region of
/// the assembled unit came from, so a diagnostic raised against the unit
/// can be reported against the file the user actually wrote.
#[must_use]
pub fn bundle_entry_source_traced(entry: &Path, source: String) -> (String, Vec<BundledSpan>) {
    let (bundled, mut spans) = bundle_sibling_modules_traced(entry, source);
    let mut visited = Vec::new();
    let (bundled, dep_spans) = bundle_path_dependencies_traced(entry, bundled, &mut visited);
    spans.extend(dep_spans);
    (bundled, spans)
}

/// A file's compilation unit: the assembled source, where its own text
/// sits inside it, and which file each region came from.
#[derive(Debug, Clone)]
pub struct DocumentUnit {
    /// The assembled compilation unit.
    pub source: String,
    /// The entry file `source` was assembled from.
    pub entry: PathBuf,
    /// Provenance of every region of `source`.
    pub origins: Vec<BundledSpan>,
    /// First byte of the document's own text within `source`.
    pub window_start: u32,
    /// Byte length of the document's own text.
    pub window_len: u32,
}

/// Assembles the compilation unit a file is compiled as part of, using
/// `text` for the file itself and disk for everything else.
///
/// A package is one unit rooted at its entry, so a module of a project is
/// checked inside that project - `crate::`, `super::`, and a bare sibling
/// module name all name what they name when the whole package compiles.
/// A file under no project, an integration test under `tests/`, and a file
/// its project's layout does not reach are each their own root.
#[must_use]
pub fn bundle_document_unit(path: &Path, text: &str) -> DocumentUnit {
    let overlay = Overlay::new(path, text);
    if let Some(entry) = crate::entry::enclosing_project_entry(path)
        && !same_file(&entry, path)
        && let Ok(entry_source) = fs::read_to_string(&entry)
    {
        let (source, mut origins) = bundle_sibling_modules_inner(&entry, entry_source, overlay);
        let (source, dep_origins) =
            bundle_path_dependencies_traced(&entry, source, &mut Vec::new());
        origins.extend(dep_origins);
        // A file the layout does not reach - a `_`-prefixed scratch file, a
        // `*_test.gos`, a directory with no `mod.gos` - is not part of the
        // package's unit, so it is compiled as its own root instead.
        if let Some(window) = document_window(&origins, path, text.len()) {
            return DocumentUnit {
                source,
                entry,
                origins,
                window_start: window.0,
                window_len: window.1,
            };
        }
    }
    let (source, origins) = bundle_entry_source_traced(path, text.to_string());
    DocumentUnit {
        source,
        entry: path.to_path_buf(),
        origins,
        window_start: 0,
        window_len: u32::try_from(text.len()).unwrap_or(u32::MAX),
    }
}

/// Where `path`'s own `len` bytes sit inside an assembled unit.
///
/// Module bodies are inlined verbatim, so the region attributed to `path`
/// is exactly the text that was handed in; a region of a different length
/// describes a different read and is declined.
fn document_window(origins: &[BundledSpan], path: &Path, len: usize) -> Option<(u32, u32)> {
    let len = u32::try_from(len).ok()?;
    origins
        .iter()
        .find(|span| {
            span.origin_start == 0 && span.end - span.start == len && same_file(&span.origin, path)
        })
        .map(|span| (span.start, len))
}

#[cfg(test)]
mod bundle_tests {
    use std::fs;

    use super::*;

    const MANIFEST: &str = "[project]\nid = \"example.com/widget\"\nversion = \"0.1.0\"\n";

    /// Writes a consumer project whose only dependency is `spec`, plus the
    /// dependency's own package under `dep_root`, and answers the entry file.
    fn project_with_dependency(name: &str, spec: &str, dep_root: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("gos-bundle-dep-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("project.toml"),
            format!("{MANIFEST}\n[dependencies]\n{spec}\n"),
        )
        .unwrap();
        let entry = root.join("src").join("main.gos");
        fs::write(&entry, "fn main() { }\n").unwrap();

        let dep = root.join(dep_root);
        fs::create_dir_all(dep.join("src")).unwrap();
        fs::write(
            dep.join("project.toml"),
            "[project]\nid = \"github.com/gossamer-lang/pgsql-gos\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(dep.join("src").join("lib.gos"), "pub fn connect() { }\n").unwrap();
        entry
    }

    /// Writes a package whose entry reaches a `codec` directory module and
    /// an `engine` directory module holding `bind.gos`, and answers its root.
    fn nested_module_project(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("gos-doc-unit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src").join("codec")).unwrap();
        fs::create_dir_all(root.join("src").join("engine")).unwrap();
        fs::write(root.join("project.toml"), MANIFEST).unwrap();
        fs::write(root.join("src").join("main.gos"), "fn main() { }\n").unwrap();
        fs::write(
            root.join("src").join("codec").join("mod.gos"),
            "pub fn tag() -> i64 { 7 }\n",
        )
        .unwrap();
        fs::write(root.join("src").join("engine").join("mod.gos"), "\n").unwrap();
        fs::write(
            root.join("src").join("engine").join("bind.gos"),
            "use crate::codec\n\npub fn one() -> i64 { codec::tag() }\n",
        )
        .unwrap();
        root
    }

    #[test]
    fn a_nested_module_is_bundled_as_part_of_its_package() {
        let root = nested_module_project("nested");
        let bind = root.join("src").join("engine").join("bind.gos");
        let text = fs::read_to_string(&bind).unwrap();
        let unit = bundle_document_unit(&bind, &text);
        assert_eq!(unit.entry, root.join("src").join("main.gos"));
        assert!(
            unit.source.contains("pub mod codec {"),
            "the package's own modules are missing from the unit:\n{}",
            unit.source
        );
        let start = unit.window_start as usize;
        assert_eq!(
            &unit.source[start..start + unit.window_len as usize],
            text,
            "the window does not name the document's own text"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_editors_buffer_replaces_the_files_bytes_in_the_unit() {
        let root = nested_module_project("overlay");
        let bind = root.join("src").join("engine").join("bind.gos");
        let buffer = "use crate::codec\n\npub fn two() -> i64 { codec::tag() + 1 }\n";
        let unit = bundle_document_unit(&bind, buffer);
        let start = unit.window_start as usize;
        assert_eq!(
            &unit.source[start..start + unit.window_len as usize],
            buffer
        );
        assert!(
            !unit.source.contains("pub fn one()"),
            "the on-disk text was bundled beside the buffer:\n{}",
            unit.source
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_package_entry_stays_the_root_of_its_own_unit() {
        let root = nested_module_project("entry");
        let main = root.join("src").join("main.gos");
        let text = fs::read_to_string(&main).unwrap();
        let unit = bundle_document_unit(&main, &text);
        assert_eq!(unit.entry, main);
        assert_eq!(unit.window_start, 0);
        assert!(unit.source.starts_with(&text));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_under_no_project_is_its_own_unit() {
        let dir = std::env::temp_dir().join(format!("gos-doc-unit-loose-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let loose = dir.join("scratch.gos");
        let text = "fn main() { }\n";
        fs::write(&loose, text).unwrap();
        let unit = bundle_document_unit(&loose, text);
        assert_eq!(unit.entry, loose);
        assert_eq!(unit.window_start, 0);
        assert_eq!(unit.source, text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_git_dependency_compiles_against_the_tree_vendoring_prepared() {
        let entry = project_with_dependency(
            "vendored",
            "pgsql_gos = { git = \"https://github.com/gossamer-lang/pgsql-gos\", rev = \"cf4da891f2e1a37eade4637ad6455a8d65d4a0b4\" }",
            "vendor/github.com__gossamer-lang__pgsql-gos",
        );
        let bundled =
            bundle_path_dependencies(&entry, fs::read_to_string(&entry).unwrap(), &mut Vec::new());
        assert!(
            bundled.contains("#[dependency(\"github.com/gossamer-lang/pgsql-gos\")]"),
            "vendored dependency not bundled:\n{bundled}"
        );
        assert!(
            bundled.contains("mod pgsql_gos {") && bundled.contains("pub fn connect"),
            "vendored dependency body missing:\n{bundled}"
        );
        let _ = fs::remove_dir_all(entry.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_dependency_with_no_prepared_tree_is_left_to_the_fetch_step() {
        let entry = project_with_dependency(
            "unfetched",
            "pgsql_gos = { git = \"https://github.com/gossamer-lang/pgsql-gos\", rev = \"cf4da891f2e1a37eade4637ad6455a8d65d4a0b4\" }",
            "elsewhere",
        );
        let bundled =
            bundle_path_dependencies(&entry, fs::read_to_string(&entry).unwrap(), &mut Vec::new());
        assert!(
            !bundled.contains("mod pgsql_gos {"),
            "an unfetched dependency has no source to bundle:\n{bundled}"
        );
        let _ = fs::remove_dir_all(entry.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn bundle_includes_siblings_and_subdirectory_modules() {
        let root = std::env::temp_dir().join(format!("gos-bundle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src").join("sub").join("deep")).unwrap();
        fs::write(root.join("project.toml"), MANIFEST).unwrap();
        let entry = root.join("src").join("main.gos");
        fs::write(&entry, "mod helper;\nmod sub;\nfn main() { }\n").unwrap();
        fs::write(root.join("src").join("helper.gos"), "pub fn h() { }\n").unwrap();
        fs::write(
            root.join("src").join("sub").join("mod.gos"),
            "pub fn ping() { }\n",
        )
        .unwrap();
        fs::write(
            root.join("src").join("sub").join("deep").join("mod.gos"),
            "pub fn depth() { }\n",
        )
        .unwrap();

        let bundled = bundle_sibling_modules(&entry, fs::read_to_string(&entry).unwrap());
        // Flat sibling -> top-level module.
        assert!(
            bundled.contains("pub mod helper {"),
            "no helper module:\n{bundled}"
        );
        assert!(bundled.contains("pub fn h"), "no helper body:\n{bundled}");
        // Subdirectory with mod.gos -> module, recursively including its
        // own subdirectory module.
        assert!(
            bundled.contains("pub mod sub {"),
            "no sub module:\n{bundled}"
        );
        assert!(bundled.contains("pub fn ping"), "no sub body:\n{bundled}");
        // A module nested two levels deep must be public too, or the
        // entry cannot name `sub::deep::depth`.
        assert!(
            bundled.contains("pub mod deep {"),
            "no nested deep module:\n{bundled}"
        );
        assert!(bundled.contains("pub fn depth"), "no deep body:\n{bundled}");
        // The entry's `mod NAME;` declarations are neutralized so the
        // synthetic inline bodies are the sole definitions.
        // The entry's `mod NAME;` declarations are blanked in place, so
        // the inline bodies are the sole definitions and every byte that
        // follows keeps its offset.
        assert!(
            !bundled.starts_with("mod helper;"),
            "mod decls not neutralized:\n{bundled}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn loose_file_outside_project_is_not_bundled() {
        let dir = std::env::temp_dir().join(format!("gos-loose-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let entry = dir.join("a.gos");
        fs::write(&entry, "fn main() { }\n").unwrap();
        fs::write(dir.join("b.gos"), "pub fn other() { }\n").unwrap();
        // No project.toml -> a loose-file invocation must not pull in
        // unrelated siblings.
        let bundled = bundle_sibling_modules(&entry, "fn main() { }\n".to_string());
        assert!(
            !bundled.contains("mod b"),
            "loose file wrongly bundled:\n{bundled}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Every byte of an assembled unit must be attributable to the file
    /// it was read from, so a diagnostic raised anywhere in the unit is
    /// reported against a file the user can open.
    #[test]
    fn traced_spans_map_dependency_and_sibling_bytes_back_to_their_files() {
        let root = std::env::temp_dir().join(format!("gos-trace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("app").join("src")).unwrap();
        fs::create_dir_all(root.join("lib").join("src")).unwrap();
        fs::write(
            root.join("app").join("project.toml"),
            "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\n\"example.com/lib\" = { path = \"../lib\" }\n",
        )
        .unwrap();
        fs::write(
            root.join("lib").join("project.toml"),
            "[project]\nid = \"example.com/lib\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let entry = root.join("app").join("src").join("main.gos");
        let entry_source = "fn main() { }\n".to_string();
        fs::write(&entry, &entry_source).unwrap();
        let helper = root.join("app").join("src").join("helper.gos");
        fs::write(&helper, "pub fn h() { }\n").unwrap();
        let lib_entry = root.join("lib").join("src").join("lib.gos");
        fs::write(&lib_entry, "pub fn marker_fn() { }\n").unwrap();

        let (bundled, spans) = bundle_entry_source_traced(&entry, entry_source);

        // A position inside the dependency's body resolves to the
        // dependency's own file, at the same offset it sits there.
        let needle = "marker_fn";
        let at = u32::try_from(bundled.find(needle).expect("dependency body inlined")).unwrap();
        let span = spans
            .iter()
            .rev()
            .find(|s| at >= s.start && at < s.end)
            .expect("dependency bytes are attributed");
        assert_eq!(span.origin, lib_entry, "wrong origin file for {needle}");
        let local = (span.origin_start + (at - span.start)) as usize;
        let origin_text = fs::read_to_string(&span.origin).unwrap();
        assert!(
            origin_text[local..].starts_with(needle),
            "offset {local} in {} is not `{needle}`",
            span.origin.display()
        );

        // The same holds for an auto-bundled sibling module.
        let at = u32::try_from(bundled.find("pub fn h").expect("sibling body inlined")).unwrap();
        let span = spans
            .iter()
            .rev()
            .find(|s| at >= s.start && at < s.end)
            .expect("sibling bytes are attributed");
        assert_eq!(span.origin, helper, "wrong origin file for the sibling");

        // The entry's own bytes stay pointed at the entry.
        let at = u32::try_from(bundled.find("fn main").expect("entry retained")).unwrap();
        let span = spans
            .iter()
            .rev()
            .find(|s| at >= s.start && at < s.end)
            .expect("entry bytes are attributed");
        assert_eq!(span.origin, entry, "entry bytes must stay on the entry");

        let _ = fs::remove_dir_all(&root);
    }

    /// A path the user can reach only through a symlink is the path a
    /// diagnostic must name, so the origin of a dependency's bytes keeps
    /// the spelling the manifest used rather than the resolved target.
    #[cfg(unix)]
    #[test]
    fn dependency_origins_keep_the_spelling_the_manifest_used() {
        let base = std::env::temp_dir().join(format!("gos-symlink-{}", std::process::id()));
        let real = base.join("real");
        let linked = base.join("linked");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(real.join("app").join("src")).unwrap();
        fs::create_dir_all(real.join("lib").join("src")).unwrap();
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        fs::write(
            real.join("app").join("project.toml"),
            "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\n\"example.com/lib\" = { path = \"../lib\" }\n",
        )
        .unwrap();
        fs::write(
            real.join("lib").join("project.toml"),
            "[project]\nid = \"example.com/lib\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            real.join("lib").join("src").join("lib.gos"),
            "pub fn marker_fn() { }\n",
        )
        .unwrap();
        let entry = linked.join("app").join("src").join("main.gos");
        let entry_source = "fn main() { }\n".to_string();
        fs::write(&entry, &entry_source).unwrap();

        let (bundled, spans) =
            bundle_path_dependencies_traced(&entry, entry_source, &mut Vec::new());
        let at =
            u32::try_from(bundled.find("marker_fn").expect("dependency body inlined")).unwrap();
        let span = spans
            .iter()
            .rev()
            .find(|s| at >= s.start && at < s.end)
            .expect("dependency bytes are attributed");
        assert_eq!(
            span.origin,
            linked.join("lib").join("src").join("lib.gos"),
            "dependency origin must stay on the path the manifest reaches it by"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn neutralized_mod_decl_preserves_every_following_offset() {
        let source = "mod helper;\nfn main() { }\n";
        let blanked = neutralize_external_mod_decls(source, &["helper"]);
        assert_eq!(
            blanked.len(),
            source.len(),
            "neutralization must not move any byte: {blanked:?}"
        );
        assert_eq!(
            source.find("fn main"),
            blanked.find("fn main"),
            "code after a neutralized decl must keep its offset"
        );
    }

    #[test]
    fn every_module_declaration_spelling_is_neutralized() {
        for source in [
            "mod helper\nfn main() { }\n",
            "mod helper;\nfn main() { }\n",
            "pub mod helper\nfn main() { }\n",
            "pub mod helper;\nfn main() { }\n",
        ] {
            let blanked = neutralize_external_mod_decls(source, &["helper"]);
            assert!(
                !blanked.contains("mod helper"),
                "declaration left in place: {blanked:?}"
            );
            assert_eq!(blanked.len(), source.len(), "offsets moved: {blanked:?}");
        }
    }

    #[test]
    fn a_module_with_no_sibling_file_keeps_its_declaration() {
        let source = "mod absent\nfn main() { }\n";
        assert_eq!(
            neutralize_external_mod_decls(source, &["helper"]),
            source,
            "only a declaration the bundler fills is neutralized"
        );
    }
}
