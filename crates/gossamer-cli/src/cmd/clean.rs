//! `gos clean [--vendor] [--dry-run]` - drop build artifacts and caches:
//! the project `target/` directory this toolchain wrote, the per-project
//! `.gos-cache` incremental IR-object cache, the frontend cache, and
//! optionally the vendor tree.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Entry point for `gos clean`.
pub(crate) struct Options {
    pub(crate) vendor: bool,
    pub(crate) dry_run: bool,
    pub(crate) classes: Vec<gossamer_driver::cache_maintenance::CacheClass>,
}

pub(crate) fn run(options: Options) -> Result<()> {
    let Options {
        vendor,
        dry_run,
        mut classes,
    } = options;
    let mut removed_bytes: u64 = 0;
    let mut removed_files: u32 = 0;

    // Project-local build artifacts are disposable, but only the ones
    // this toolchain produced: `gos build` writes the binary under
    // `target/{debug,release}` and stamps that directory, and a
    // `project.toml` at the current directory anchors the same layout
    // for a project built by an earlier toolchain. `target/` is a
    // conventional name several build systems claim, so an unstamped one
    // in a directory that is not a Gossamer project belongs to whichever
    // of them made it and is left alone.
    let cwd = std::env::current_dir()?;
    let dir = cwd.join("target");
    if owns_build_dir(&cwd, &dir) {
        remove_dir(
            &dir,
            "build artifacts (target/)",
            dry_run,
            &mut removed_bytes,
            &mut removed_files,
        )?;
    } else if dir.is_dir() {
        outln!(
            "kept {} - no {} stamp and no project.toml here, so `gos build` \
             did not write it",
            dir.display(),
            crate::paths::BUILD_DIR_STAMP,
        );
    }
    if classes.is_empty() {
        classes.extend([
            gossamer_driver::cache_maintenance::CacheClass::Frontend,
            gossamer_driver::cache_maintenance::CacheClass::Ir,
        ]);
    }
    // `gos clean` keeps reaching every root it always has; `gos cache` is
    // where the local / global distinction is named.
    for entry in gossamer_driver::cache_maintenance::remove(
        &cwd,
        &classes,
        gossamer_driver::cache_maintenance::CacheScope::All,
        dry_run,
    )? {
        let verb = if dry_run { "would remove" } else { "removed" };
        outln!(
            "{verb} {} cache at {} ({} bytes)",
            entry.class.name(),
            entry.path.display(),
            entry.bytes
        );
        removed_bytes += entry.bytes;
        removed_files += 1;
    }

    if vendor {
        remove_dir(
            &cwd.join("vendor"),
            "vendor tree",
            dry_run,
            &mut removed_bytes,
            &mut removed_files,
        )?;
    }

    let verb = if dry_run { "would remove" } else { "removed" };
    outln!("clean: {verb} {removed_files} target(s), {removed_bytes} bytes total");
    Ok(())
}

/// Whether `gos clean` may remove `target_dir`: it carries this
/// toolchain's build stamp, or `cwd` is a Gossamer project root, whose
/// `target/` this toolchain owns by layout.
fn owns_build_dir(cwd: &Path, target_dir: &Path) -> bool {
    target_dir.join(crate::paths::BUILD_DIR_STAMP).exists() || cwd.join("project.toml").is_file()
}

/// Removes `dir` (recursively) if present, accumulating the byte/entry
/// tally. A `dry_run` only reports. Absent targets print a note and are
/// skipped - `gos clean` is idempotent.
fn remove_dir(
    dir: &Path,
    label: &str,
    dry_run: bool,
    removed_bytes: &mut u64,
    removed_files: &mut u32,
) -> Result<()> {
    if !dir.is_dir() {
        outln!("{label} absent at {}", dir.display());
        return Ok(());
    }
    let bytes = dir_size(dir);
    if dry_run {
        outln!("would remove {label} at {} ({bytes} bytes)", dir.display());
    } else {
        fs::remove_dir_all(dir).with_context(|| format!("remove {}", dir.display()))?;
        outln!("removed {label} at {} ({bytes} bytes)", dir.display());
    }
    *removed_bytes += bytes;
    *removed_files += 1;
    Ok(())
}

/// Sums every regular file's byte length under `root`. Broken
/// symlinks and per-entry I/O errors are treated as 0 bytes - the
/// tally is advisory, never required for correctness.
fn dir_size(root: &std::path::Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::owns_build_dir;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("gos-clean-{pid}-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn unstamped_target_outside_a_project_is_not_owned() {
        let cwd = scratch("foreign");
        let target = cwd.join("target");
        std::fs::create_dir_all(target.join("release")).unwrap();
        assert!(!owns_build_dir(&cwd, &target));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn stamped_target_is_owned() {
        let cwd = scratch("stamped");
        let target = cwd.join("target");
        std::fs::create_dir_all(&target).unwrap();
        crate::paths::stamp_build_dir(&target);
        assert!(owns_build_dir(&cwd, &target));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn project_root_target_is_owned_without_a_stamp() {
        let cwd = scratch("project");
        let target = cwd.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(cwd.join("project.toml"), b"[project]\nid = \"a.b/c\"\n").unwrap();
        assert!(owns_build_dir(&cwd, &target));
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
