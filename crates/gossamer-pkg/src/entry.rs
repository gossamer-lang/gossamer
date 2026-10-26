//! Which file is a project's compilation root.
//!
//! A package is one compilation unit assembled from its entry file (see
//! [`crate::bundle`]), so every front end that reads a `.gos` file has to
//! agree on which file that is. The command line resolves it from a
//! directory target and the language server from whichever file the editor
//! opened; both land here.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Manifest;

/// Why a project root has no entry file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryError {
    /// `[project] entry` names a file that is not there.
    DeclaredMissing {
        /// The path as the manifest spelled it.
        declared: String,
        /// Where it resolved to.
        resolved: PathBuf,
    },
    /// The root's source directory holds several equally plausible entries.
    Ambiguous {
        /// The project root.
        root: PathBuf,
        /// The directory that was searched.
        dir: PathBuf,
        /// File names of the candidates, sorted.
        candidates: Vec<String>,
    },
    /// The root holds no source that could be an entry.
    None {
        /// The project root.
        root: PathBuf,
    },
}

impl std::fmt::Display for EntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeclaredMissing { declared, resolved } => write!(
                f,
                "project.toml sets [project] entry = {declared:?} but {} does not exist",
                resolved.display()
            ),
            Self::Ambiguous {
                root,
                dir,
                candidates,
            } => write!(
                f,
                "project root {} has no src/main.gos (or main.gos), and {} holds several candidates ({}); pass a path explicitly",
                root.display(),
                dir.display(),
                candidates.join(", ")
            ),
            Self::None { root } => write!(
                f,
                "project root {} has no src/main.gos (or main.gos) and no .gos source to run; pass a path explicitly",
                root.display()
            ),
        }
    }
}

impl std::error::Error for EntryError {}

/// Entry-point resolution for a project root. An explicit `[project] entry`
/// in the manifest wins; otherwise the convention order applies:
/// `src/main.gos`, `main.gos`, the `[lib] path` or `src/lib.gos` library
/// root, the manifest-id-named source (`src/<id-tail>.gos`, then
/// `<id-tail>.gos`), and finally a sole `.gos` candidate under `src/` or the
/// root. A directory with several nameless candidates is an error that lists
/// them.
///
/// # Errors
///
/// Returns [`EntryError`] when the root declares an entry that is missing,
/// holds several equally plausible candidates, or holds none at all.
pub fn resolve_project_entry(root: &Path) -> Result<PathBuf, EntryError> {
    if let Some(entry) = manifest_entry(root) {
        let path = root.join(&entry);
        if path.is_file() {
            return Ok(path);
        }
        return Err(EntryError::DeclaredMissing {
            declared: entry,
            resolved: path,
        });
    }
    let canonical = root.join("src").join("main.gos");
    if canonical.is_file() {
        return Ok(canonical);
    }
    let bare = root.join("main.gos");
    if bare.is_file() {
        return Ok(bare);
    }
    // A library package has no `main`; its root is the `[lib] path`, or
    // `src/lib.gos` / `lib.gos` by convention. Resolved before the
    // sole-candidate fallback so a library with several sibling modules
    // roots at its own entry rather than reporting them as ambiguous.
    if let Some(path) = manifest_lib_path(root) {
        let path = root.join(path);
        if path.is_file() {
            return Ok(path);
        }
    }
    for candidate in [root.join("src").join("lib.gos"), root.join("lib.gos")] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(tail) = manifest_id_tail(root) {
        let named = root.join("src").join(format!("{tail}.gos"));
        if named.is_file() {
            return Ok(named);
        }
        let named = root.join(format!("{tail}.gos"));
        if named.is_file() {
            return Ok(named);
        }
    }
    for dir in [root.join("src"), root.to_path_buf()] {
        match entry_candidates(&dir).as_slice() {
            [] => {}
            [sole] => return Ok(sole.clone()),
            many => {
                return Err(EntryError::Ambiguous {
                    root: root.to_path_buf(),
                    dir: dir.clone(),
                    candidates: many
                        .iter()
                        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                        .collect(),
                });
            }
        }
    }
    Err(EntryError::None {
        root: root.to_path_buf(),
    })
}

/// The compilation root `file` belongs to: the entry of the nearest project
/// above it, when `file` is one of that project's own modules.
///
/// A file the package's layout does not reach - a script under `tools/`, a
/// `_`-prefixed scratch file, a `*_test.gos`, a directory with no `mod.gos` -
/// is its own root, as are an integration test under `tests/` and a file
/// under no project at all.
#[must_use]
pub fn enclosing_project_entry(file: &Path) -> Option<PathBuf> {
    let entry = nearest_project_entry(file)?;
    if crate::bundle::same_file(&entry, file) {
        return Some(entry);
    }
    let source = fs::read_to_string(&entry).ok()?;
    let (_, origins) = crate::bundle::bundle_sibling_modules_traced(&entry, source);
    origins
        .iter()
        .any(|span| crate::bundle::same_file(&span.origin, file))
        .then_some(entry)
}

/// The entry of the nearest project above `file`, whether or not the
/// package's layout reaches `file`. An integration test under `tests/` and a
/// file under no project have none.
pub(crate) fn nearest_project_entry(file: &Path) -> Option<PathBuf> {
    let mut dir = file.parent()?;
    loop {
        if dir.join("project.toml").is_file() {
            if file.starts_with(dir.join("tests")) {
                return None;
            }
            return resolve_project_entry(dir).ok();
        }
        dir = dir.parent()?;
    }
}

/// Last segment of the manifest's `[project] id`, when the root's
/// `project.toml` parses.
fn manifest_id_tail(root: &Path) -> Option<String> {
    Some(read_manifest(root)?.project.id.tail().to_string())
}

/// `[lib] path` from the root's manifest, when it declares a library.
fn manifest_lib_path(root: &Path) -> Option<String> {
    read_manifest(root)?.lib.and_then(|lib| lib.path)
}

/// `[project] entry` from the root's manifest, when present and the
/// `project.toml` parses.
fn manifest_entry(root: &Path) -> Option<String> {
    read_manifest(root)?.project.entry
}

fn read_manifest(root: &Path) -> Option<Manifest> {
    let text = fs::read_to_string(root.join("project.toml")).ok()?;
    Manifest::parse(&text).ok()
}

/// `.gos` files in `dir` that qualify as an entry point, sorted by
/// name. Skips `_`-prefixed scratch files and `*_test.gos` (the
/// same exclusions the sibling auto-bundler applies).
fn entry_candidates(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for dirent in read.flatten() {
        let path = dirent.path();
        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("gos") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.starts_with('_') || stem.ends_with("_test") {
            continue;
        }
        out.push(path);
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_files_the_package_reaches_belong_to_its_entry() {
        let root = std::env::temp_dir().join(format!("gos-entry-members-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["src/net", "tools", "tests"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(
            root.join("project.toml"),
            "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        for (file, text) in [
            ("src/main.gos", "fn main() { }\n"),
            ("src/util.gos", "pub fn one() -> i64 { 1 }\n"),
            ("src/net/mod.gos", "pub fn two() -> i64 { 2 }\n"),
            ("src/_scratch.gos", "fn main() { }\n"),
            ("tools/gen.gos", "fn main() { }\n"),
            ("tests/flow.gos", "fn main() { }\n"),
        ] {
            fs::write(root.join(file), text).unwrap();
        }
        let entry = root.join("src/main.gos");
        for member in ["src/main.gos", "src/util.gos", "src/net/mod.gos"] {
            assert_eq!(
                enclosing_project_entry(&root.join(member)),
                Some(entry.clone()),
                "{member}"
            );
        }
        for own_root in ["src/_scratch.gos", "tools/gen.gos", "tests/flow.gos"] {
            assert_eq!(
                enclosing_project_entry(&root.join(own_root)),
                None,
                "{own_root}"
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
