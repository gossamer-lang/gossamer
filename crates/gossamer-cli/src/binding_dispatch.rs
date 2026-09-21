//! Pre-clap dispatch step that diverts `gos run`, `gos build`, and
//! `gos check` to a per-project Rust-binding runner when the
//! current project's `project.toml` declares `[rust-bindings]`.
//!
//! The runner is a Cargo binary statically linking every binding;
//! it's built on demand by [`gossamer_driver::BindingRunner`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use gossamer_driver::binding_runner::{
    BindingRunner, BindingRunnerError, DumpedType, Profile as RunnerProfile, parse_signature_dump,
};
use gossamer_resolve::{
    BindingType, ExternalItem, ExternalModule, all_external_modules, set_external_modules,
};

/// Outcome of [`dispatch_runner_if_needed`].
#[derive(Debug)]
pub enum DispatchOutcome {
    /// Runner not needed - fall through to the in-process CLI.
    InProcess,
    /// Runner was dispatched; this never returns on success
    /// because the runner replaces the current process. Returned
    /// only on failure.
    Failed(gossamer_driver::binding_runner::BindingRunnerError),
}

/// Subcommands that load user code and therefore want a runner.
const RUNNER_SUBCOMMANDS: &[&str] = &["build", "check", "doc", "repl", "run", "test"];

/// Returns whether the parsed argv warrants a runner dispatch.
///
/// Filters out re-entry (`GOSSAMER_IN_RUNNER=1`), commands that
/// don't load user code, and explicit overrides
/// (`GOSSAMER_NO_RUNNER=1`).
#[must_use]
pub fn needs_runner_dispatch(args: &[OsString]) -> bool {
    if std::env::var_os("GOSSAMER_IN_RUNNER").is_some() {
        return false;
    }
    if std::env::var_os("GOSSAMER_NO_RUNNER").is_some() {
        return false;
    }
    let Some(sub) = first_subcommand(args) else {
        return false;
    };
    RUNNER_SUBCOMMANDS.contains(&sub.as_str())
}

/// Walks `argv` past the binary name and global flags, returning
/// the first positional that names a subcommand.
fn first_subcommand(args: &[OsString]) -> Option<String> {
    for arg in args.iter().skip(1) {
        let s = arg.to_string_lossy();
        if s.starts_with('-') {
            continue;
        }
        return Some(s.into_owned());
    }
    None
}

/// The entry path a runner subcommand was pointed at, when it names one.
///
/// A file argument carries its own project: `gos run a/b/src/main.gos`
/// declares which manifest governs the program regardless of where it was
/// invoked from, and the bindings that manifest declares are the ones the
/// runner has to link. Without an argument the nearest project to the
/// working directory is the subject, as it is for a bare `gos run`.
fn runner_entry_arg(args: &[OsString]) -> Option<PathBuf> {
    let mut positionals = args.iter().skip(1).filter(|arg| {
        let s = arg.to_string_lossy();
        !s.starts_with('-')
    });
    let _subcommand = positionals.next()?;
    let candidate = PathBuf::from(positionals.next()?);
    candidate.exists().then_some(candidate)
}

/// Top-level pre-step: if the current project declares
/// `[rust-bindings]`, build the runner and `exec` into it. The
/// runner sets `GOSSAMER_IN_RUNNER=1` so the second pass through
/// this function is a no-op.
///
/// Returns:
/// - `DispatchOutcome::InProcess` - caller should continue with
///   the in-process CLI.
/// - `DispatchOutcome::Failed(err)` - runner build / spawn
///   failed. Caller should print the error and exit non-zero.
///
/// Successful dispatch never returns: [`BindingRunner::exec`]
/// calls `std::process::exit` after the child completes.
#[must_use]
pub fn dispatch_runner_if_needed(args: &[OsString]) -> DispatchOutcome {
    if !needs_runner_dispatch(args) {
        return DispatchOutcome::InProcess;
    }
    let project = match runner_entry_arg(args) {
        Some(entry) => crate::paths::project_context_for_entry(&entry),
        None => crate::paths::project_context(),
    };
    // A present-but-malformed manifest is a hard error, never a
    // silent "no bindings": a bare `id = "name"` used to skip the
    // binding runner here while check/test kept passing, leaving
    // every binding call unbound at runtime.
    let Some(manifest_result) = project.manifest_result() else {
        return DispatchOutcome::InProcess;
    };
    let manifest = match manifest_result {
        Ok(m) => m,
        Err(err) => {
            return DispatchOutcome::Failed(
                gossamer_driver::binding_runner::BindingRunnerError::Manifest(err.to_string()),
            );
        }
    };
    if manifest.rust_bindings.is_empty() {
        return DispatchOutcome::InProcess;
    }
    let manifest_dir = project.manifest_dir().unwrap_or_else(|| PathBuf::from("."));
    let Some(gossamer_root) = locate_gossamer_root() else {
        return DispatchOutcome::Failed(gossamer_driver::binding_runner::BindingRunnerError::Io(
            std::io::Error::other("cannot locate gossamer source root (set GOSSAMER_ROOT)"),
        ));
    };
    let profile = profile_for_args(args);
    let runner =
        match BindingRunner::from_manifest(manifest, &manifest_dir, &gossamer_root, profile) {
            Ok(Some(r)) => r,
            Ok(None) => return DispatchOutcome::InProcess,
            Err(err) => {
                return DispatchOutcome::Failed(
                    gossamer_driver::binding_runner::BindingRunnerError::Io(err),
                );
            }
        };
    if std::env::var_os("GOSSAMER_DISPATCH_TRACE").is_some() {
        eprintln!("dispatch: runner ({})", runner.fingerprint_hex);
    }
    // The lease keeps the runner's workdir, which it also links its runtime
    // from, in place for as long as the runner runs.
    let lease = match runner.ensure_built() {
        Ok(lease) => lease,
        Err(err) => return DispatchOutcome::Failed(err),
    };
    match BindingRunner::exec(lease.artifact(), args) {
        Ok(code) => {
            drop(lease);
            std::process::exit(code)
        }
        Err(err) => DispatchOutcome::Failed(err),
    }
}

/// Picks debug or release runner based on the parsed argv. `gos
/// build --release` selects [`RunnerProfile::Release`]; everything
/// else uses [`RunnerProfile::Debug`].
fn profile_for_args(args: &[OsString]) -> RunnerProfile {
    let mut iter = args.iter().skip(1);
    let mut subcommand: Option<String> = None;
    for arg in iter.by_ref() {
        let s = arg.to_string_lossy();
        if s.starts_with('-') {
            continue;
        }
        subcommand = Some(s.into_owned());
        break;
    }
    if subcommand.as_deref() != Some("build") {
        return RunnerProfile::Debug;
    }
    for arg in iter {
        if arg == "--release" {
            return RunnerProfile::Release;
        }
    }
    RunnerProfile::Debug
}

/// Locates the gossamer source-tree root.
///
/// Order:
/// 1. `GOSSAMER_ROOT` env var (caller override).
/// 2. The compile-time `CARGO_MANIFEST_DIR` of `gossamer-cli`'s
///    parent's parent, when the binary was built from this very
///    workspace (covers `cargo run -p gossamer-cli ...`).
/// 3. Walk up from the binary's own location looking for a
///    `Cargo.toml` whose `[workspace]` includes `gossamer-cli`.
pub fn locate_gossamer_root() -> Option<PathBuf> {
    if let Some(s) = std::env::var_os("GOSSAMER_ROOT") {
        let p = PathBuf::from(s);
        if p.is_dir() {
            return Some(p);
        }
    }
    // Compile-time: gossamer-cli is at <root>/crates/gossamer-cli.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let p = PathBuf::from(manifest_dir);
    let candidate = p.parent().and_then(Path::parent).map(Path::to_path_buf);
    if let Some(root) = candidate
        && root.join("Cargo.toml").is_file()
    {
        return Some(root);
    }
    // Walk up from the running binary.
    if let Ok(exe) = std::env::current_exe() {
        let mut cursor: Option<&Path> = exe.parent();
        while let Some(dir) = cursor {
            if dir.join("crates").join("gossamer-cli").is_dir() {
                return Some(dir.to_path_buf());
            }
            cursor = dir.parent();
        }
    }
    None
}

/// Populates the resolver's external-modules table from the
/// per-project signature dump, when the current project declares
/// `[rust-bindings]`. Idempotent and silently skips when:
///
/// - we're already inside a runner (the runner ran
///   `gossamer_binding::install_all` which populated the table),
/// - no `project.toml` is reachable,
/// - the manifest declares no `[rust-bindings]`,
/// - the table is already non-empty.
///
/// Returns the number of external modules now visible to the
/// resolver (zero is fine - caller treats it as "no bindings").
pub fn ensure_external_signatures() -> Result<usize, BindingRunnerError> {
    ensure_external_signatures_for_project(crate::paths::project_context())
}

/// Populates external binding signatures for the project containing `entry`.
///
/// This is used by explicit path builds/checks so the binding signature table
/// follows the source file rather than the process cwd.
pub fn ensure_external_signatures_for_entry(entry: &Path) -> Result<usize, BindingRunnerError> {
    ensure_external_signatures_for_project(crate::paths::project_context_for_entry(entry))
}

fn ensure_external_signatures_for_project(
    project: crate::paths::ProjectContext,
) -> Result<usize, BindingRunnerError> {
    if std::env::var_os("GOSSAMER_IN_RUNNER").is_some() {
        return Ok(all_external_modules().len());
    }
    if !all_external_modules().is_empty() {
        return Ok(all_external_modules().len());
    }
    // Mirror `dispatch_runner_if_needed`: a malformed manifest must
    // not silently degrade to "no bindings".
    let Some(manifest_result) = project.manifest_result() else {
        return Ok(0);
    };
    let manifest = match manifest_result {
        Ok(m) => m,
        Err(err) => return Err(BindingRunnerError::Manifest(err.to_string())),
    };
    if manifest.rust_bindings.is_empty() {
        return Ok(0);
    }
    let manifest_dir = project.manifest_dir().unwrap_or_else(|| PathBuf::from("."));
    let Some(gossamer_root) = locate_gossamer_root() else {
        return Ok(0);
    };
    let runner = match BindingRunner::from_manifest(
        manifest,
        &manifest_dir,
        &gossamer_root,
        RunnerProfile::Debug,
    ) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(0),
        Err(err) => return Err(BindingRunnerError::Io(err)),
    };
    let signatures = runner.ensure_signatures()?;
    let json = std::fs::read_to_string(signatures.artifact()).map_err(BindingRunnerError::Io)?;
    drop(signatures);
    let dump = parse_signature_dump(&json)?;
    let modules: Vec<ExternalModule> = dump
        .modules
        .into_iter()
        .map(|m| ExternalModule {
            path: m.path,
            doc: m.doc,
            items: m
                .items
                .into_iter()
                .map(|item| ExternalItem {
                    name: item.name,
                    doc: item.doc,
                    params: item.params.iter().map(dumped_to_binding).collect(),
                    ret: dumped_to_binding(&item.ret),
                })
                .collect(),
        })
        .collect();
    let count = modules.len();
    set_external_modules(modules);
    Ok(count)
}

fn dumped_to_binding(t: &DumpedType) -> BindingType {
    match t {
        DumpedType::Unit => BindingType::Unit,
        DumpedType::Bool => BindingType::Bool,
        DumpedType::I64 => BindingType::I64,
        DumpedType::F64 => BindingType::F64,
        DumpedType::Char => BindingType::Char,
        DumpedType::String => BindingType::String,
        DumpedType::Bytes => BindingType::Bytes,
        DumpedType::Tuple { items } => {
            BindingType::Tuple(items.iter().map(dumped_to_binding).collect())
        }
        DumpedType::Vec { of } => BindingType::Vec(Box::new(dumped_to_binding(of))),
        DumpedType::Option { of } => BindingType::Option(Box::new(dumped_to_binding(of))),
        DumpedType::Result { ok, err } => BindingType::Result(
            Box::new(dumped_to_binding(ok)),
            Box::new(dumped_to_binding(err)),
        ),
        DumpedType::Map { key, value } => BindingType::Map(
            Box::new(dumped_to_binding(key)),
            Box::new(dumped_to_binding(value)),
        ),
        DumpedType::Variant { arms } => BindingType::Variant(
            arms.iter()
                .map(|a| gossamer_resolve::BindingVariantArm {
                    name: a.name.clone(),
                    payload: a.payload.iter().map(dumped_to_binding).collect(),
                })
                .collect(),
        ),
        DumpedType::Callback { args, ret } => BindingType::Callback(
            args.iter().map(dumped_to_binding).collect(),
            Box::new(dumped_to_binding(ret)),
        ),
        DumpedType::Opaque { name } => BindingType::Opaque(name.clone()),
        DumpedType::Any => BindingType::Any,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(|s| OsString::from(*s)).collect()
    }

    /// `gos run path/to/src/main.gos` and `gos build path/to/src/main.gos`
    /// name the same program, so both take the manifest beside it.
    #[test]
    fn the_runner_takes_its_project_from_the_entry_argument() {
        let root = std::env::temp_dir().join(format!("gos-runner-entry-{}", std::process::id()));
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("scratch project");
        std::fs::write(src.join("main.gos"), "fn main() {}\n").expect("entry");
        let entry = src.join("main.gos");

        assert_eq!(
            runner_entry_arg(&argv(&["gos", "run", entry.to_str().expect("utf-8 path")]))
                .as_deref(),
            Some(entry.as_path()),
        );
        // A directory argument names its project just as a file does.
        assert_eq!(
            runner_entry_arg(&argv(&["gos", "build", root.to_str().expect("utf-8 path")]))
                .as_deref(),
            Some(root.as_path()),
        );
        // No positional: the working directory's project is the subject.
        assert_eq!(runner_entry_arg(&argv(&["gos", "run"])), None);
        // Flags never stand in for the entry.
        assert_eq!(
            runner_entry_arg(&argv(&["gos", "build", "--release"])),
            None
        );
        // A positional that names nothing on disk is a program argument.
        assert_eq!(
            runner_entry_arg(&argv(&["gos", "run", "no-such-file.gos"])),
            None
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn first_subcommand_skips_flags() {
        let a = argv(&["gos", "--quiet", "build", "x.gos"]);
        assert_eq!(first_subcommand(&a).as_deref(), Some("build"));
    }

    #[test]
    fn first_subcommand_returns_none_for_no_command() {
        let a = argv(&["gos"]);
        assert!(first_subcommand(&a).is_none());
    }

    #[test]
    fn profile_release_only_for_build_release() {
        assert!(matches!(
            profile_for_args(&argv(&["gos", "x.gos"])),
            RunnerProfile::Debug
        ));
        assert!(matches!(
            profile_for_args(&argv(&["gos", "build", "x.gos"])),
            RunnerProfile::Debug
        ));
        assert!(matches!(
            profile_for_args(&argv(&["gos", "build", "x.gos", "--release"])),
            RunnerProfile::Release
        ));
        assert!(matches!(
            profile_for_args(&argv(&["gos", "build", "--release", "x.gos"])),
            RunnerProfile::Release
        ));
    }
}
