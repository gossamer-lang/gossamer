//! Capability policy for compile-time evaluation.
//!
//! A `comptime { ... }` region and every `comptime fn` call run on the
//! bytecode VM while the program is being compiled, so whatever they
//! reach they reach with the privileges of whoever typed `gos check`.
//! The policy decides which capabilities that evaluation has: no I/O at
//! all, reads confined to the source tree, or the unrestricted escape.
//!
//! The level is process-global and the confinement anchor is
//! thread-local, matching [`crate::comptime_paths`]: both are set only
//! while a fold is in progress, so a program's own run-time I/O is
//! unaffected.

use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

/// How much of the host a compile-time region may reach.
///
/// Ordered by increasing privilege, so the more restrictive of two
/// requested levels is the smaller one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum ComptimeIo {
    /// No I/O at all. A compile-time region is pure computation over
    /// its inputs.
    None,
    /// Reads under the confinement root; writes, process spawn,
    /// network, and environment mutation denied.
    #[default]
    Confined,
    /// Every capability the host grants the compiling user. Never a
    /// default; reaching it takes an affirmative act by whoever runs
    /// the command.
    Full,
}

impl ComptimeIo {
    /// The level named by `text`, or `None` when it names none.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "none" => Some(Self::None),
            "confined" => Some(Self::Confined),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// The spelling `--comptime-io` and `project.toml` accept.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Confined => "confined",
            Self::Full => "full",
        }
    }
}

/// Resolves the effective level from what the command line asked for
/// and what the manifest asked for.
///
/// The more restrictive of the two wins. The manifest is authored by
/// the party the policy defends against, so it may tighten the posture
/// freely and may never loosen it: an absent command-line flag still
/// resolves against the [`ComptimeIo::Confined`] default, so a
/// dependency asking for `full` loses.
#[must_use]
pub fn resolve(command_line: Option<ComptimeIo>, manifest: Option<ComptimeIo>) -> ComptimeIo {
    let base = command_line.unwrap_or_default();
    manifest.map_or(base, |from_manifest| base.min(from_manifest))
}

/// The capability class an operation needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Reading a path.
    Read,
    /// Creating, writing, removing, or renaming a path.
    Write,
    /// Starting a process.
    Exec,
    /// Reaching the network.
    Network,
    /// Mutating the process environment or working directory.
    Env,
    /// Calling a function declared in an `unsafe extern "C"` block, whose
    /// effects the compiler cannot see.
    Foreign,
}

impl Capability {
    /// Name used in a denial diagnostic.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "filesystem read",
            Self::Write => "filesystem write",
            Self::Exec => "process execution",
            Self::Network => "network access",
            Self::Env => "environment mutation",
            Self::Foreign => "foreign function call",
        }
    }
}

/// Process-wide level, encoded as a [`ComptimeIo`] discriminant.
static LEVEL: AtomicU8 = AtomicU8::new(ComptimeIo::Confined as u8);

thread_local! {
    /// Root that a `confined` read may not escape, set for the
    /// duration of a fold. `None` outside a fold, which is what makes
    /// the gate inert at run time.
    static CONFINEMENT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Sets the process-wide level. Called once by the toolchain entry
/// point after resolving the command line against the manifest.
pub fn set_level(level: ComptimeIo) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// The process-wide level.
#[must_use]
pub fn level() -> ComptimeIo {
    match LEVEL.load(Ordering::Relaxed) {
        0 => ComptimeIo::None,
        2 => ComptimeIo::Full,
        _ => ComptimeIo::Confined,
    }
}

/// Confines compile-time I/O to `root` until the guard is dropped.
///
/// Outside such a guard the gate is inert: a program's own run-time
/// I/O is never subject to the compile-time policy.
pub struct Confined(Option<PathBuf>);

impl Confined {
    /// Confines to the directory holding `source`, which may name a
    /// file or a directory. A source with no directory part confines
    /// to the process working directory, so a bare filename still has
    /// a root.
    #[must_use]
    pub fn at_source(source: &str) -> Self {
        let dir = Path::new(source)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(
                || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                Path::to_path_buf,
            );
        Self::at_root(canonical_or_lexical(&dir))
    }

    /// Confines to an already-resolved `root`. The scheduler uses this
    /// to carry a fold's confinement onto the worker thread a
    /// compile-time goroutine lands on, so leaving the folding thread
    /// is not a way out of the policy.
    #[must_use]
    pub fn at_root(root: PathBuf) -> Self {
        Self(CONFINEMENT.with(|cell| cell.replace(Some(root))))
    }

    /// The root in force on this thread, or `None` outside a fold.
    #[must_use]
    pub fn root() -> Option<PathBuf> {
        CONFINEMENT.with(|cell| cell.borrow().clone())
    }

    /// Whether a fold is in progress on this thread.
    #[must_use]
    pub fn active() -> bool {
        CONFINEMENT.with(|cell| cell.borrow().is_some())
    }
}

impl Drop for Confined {
    fn drop(&mut self) {
        let previous = self.0.take();
        CONFINEMENT.with(|cell| *cell.borrow_mut() = previous);
    }
}

/// A denial: the operation that was attempted, the capability it
/// needed, and the level that refused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    /// Gossamer spelling of the operation, e.g. `fs::write`.
    pub operation: String,
    /// Capability class the operation needed.
    pub capability: Capability,
    /// Level in force when the attempt was made.
    pub level: ComptimeIo,
    /// Path the attempt named, when the capability is path-shaped.
    pub path: Option<String>,
}

impl Denied {
    /// Whether the denial is a `confined` read that left the source
    /// tree rather than a capability the level withholds outright.
    fn is_out_of_tree_read(&self) -> bool {
        self.level == ComptimeIo::Confined && self.capability == Capability::Read
    }
}

impl std::fmt::Display for Denied {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_out_of_tree_read() {
            write!(
                out,
                "error[GX0010]: compile-time evaluation cannot call `{}`: \
                 --comptime-io=confined reads only under the source tree",
                self.operation
            )?;
            if let Some(path) = &self.path {
                write!(out, "\n  the path resolves to {path}")?;
            }
            return write!(
                out,
                "\n  re-run with --comptime-io=full to permit it, or embed a \
                 file from inside the project"
            );
        }
        write!(
            out,
            "error[GX0010]: compile-time evaluation cannot call `{}`: {} is denied at --comptime-io={}",
            self.operation,
            self.capability.as_str(),
            self.level.as_str()
        )?;
        if let Some(path) = &self.path {
            write!(out, " (path: {path})")?;
        }
        match self.level {
            ComptimeIo::None => write!(
                out,
                "\n  --comptime-io=none denies all compile-time I/O; \
                 --comptime-io=confined permits reads under the source tree, \
                 --comptime-io=full permits everything"
            ),
            _ => write!(
                out,
                "\n  re-run with --comptime-io=full to permit it, or move the \
                 work out of the comptime region"
            ),
        }
    }
}

/// Checks `capability` against the level in force, returning `Err`
/// when the compile-time region may not have it.
///
/// Outside a fold this is a thread-local read and an `Ok`, so run-time
/// I/O pays one branch and nothing else.
pub fn check(operation: &str, capability: Capability) -> Result<(), Denied> {
    if !Confined::active() {
        return Ok(());
    }
    let level = level();
    match (level, capability) {
        (ComptimeIo::Full, _) => Ok(()),
        (ComptimeIo::Confined, Capability::Read) => Ok(()),
        _ => Err(Denied {
            operation: operation.to_string(),
            capability,
            level,
            path: None,
        }),
    }
}

/// Checks a path-shaped `capability` against the level in force.
///
/// At `confined` a read must resolve under the confinement root; a
/// symlink pointing out of the tree is denied at its target, because
/// the comparison is made against the canonical path.
pub fn check_path(operation: &str, capability: Capability, path: &str) -> Result<(), Denied> {
    if !Confined::active() {
        return Ok(());
    }
    let level = level();
    let deny = |path: Option<String>| {
        Err(Denied {
            operation: operation.to_string(),
            capability,
            level,
            path,
        })
    };
    match (level, capability) {
        (ComptimeIo::Full, _) => Ok(()),
        (ComptimeIo::Confined, Capability::Read) => {
            let resolved = canonical_or_lexical(Path::new(path));
            let inside = CONFINEMENT.with(|cell| {
                cell.borrow()
                    .as_ref()
                    .is_some_and(|root| resolved.starts_with(root))
            });
            if inside {
                Ok(())
            } else {
                deny(Some(resolved.display().to_string()))
            }
        }
        _ => deny(Some(path.to_string())),
    }
}

/// The canonical form of `path`, or its lexically-normalized form when
/// it does not exist yet.
///
/// A read of a missing path still has to be compared against the root,
/// and canonicalization fails on a path with no target, so the
/// lexical form carries the comparison in that case.
fn canonical_or_lexical(path: &Path) -> PathBuf {
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    // Canonicalize the longest existing prefix so a symlinked parent
    // resolves even when the leaf does not exist, then re-attach the
    // remainder lexically.
    let mut prefix = absolute.as_path();
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    while let Some(parent) = prefix.parent() {
        if let Ok(resolved) = prefix.canonicalize() {
            let mut out = resolved;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return normalize(&out);
        }
        if let Some(name) = prefix.file_name() {
            tail.push(name);
        }
        prefix = parent;
    }
    normalize(&absolute)
}

/// Lexical normalization: drops `.` and resolves `..` against the
/// preceding component.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod comptime_policy_tests {
    use super::*;

    #[test]
    fn a_manifest_cannot_loosen_the_command_line_default() {
        assert_eq!(resolve(None, Some(ComptimeIo::Full)), ComptimeIo::Confined);
        assert_eq!(resolve(None, Some(ComptimeIo::None)), ComptimeIo::None);
        assert_eq!(
            resolve(Some(ComptimeIo::Full), Some(ComptimeIo::None)),
            ComptimeIo::None
        );
        assert_eq!(
            resolve(Some(ComptimeIo::Full), Some(ComptimeIo::Full)),
            ComptimeIo::Full
        );
        assert_eq!(resolve(None, None), ComptimeIo::Confined);
    }

    #[test]
    fn the_gate_is_inert_outside_a_fold() {
        set_level(ComptimeIo::None);
        assert!(check("fs::write", Capability::Write).is_ok());
        set_level(ComptimeIo::Confined);
    }

    #[test]
    fn levels_parse_and_render_round_trip() {
        for level in [ComptimeIo::None, ComptimeIo::Confined, ComptimeIo::Full] {
            assert_eq!(ComptimeIo::parse(level.as_str()), Some(level));
        }
        assert_eq!(ComptimeIo::parse("nope"), None);
    }

    #[test]
    fn normalization_resolves_parent_components() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }
}
