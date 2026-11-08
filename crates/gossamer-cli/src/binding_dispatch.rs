//! A project's `[rust-bindings]`, built as shared libraries and installed
//! into this process before a command that compiles or runs the project.
//!
//! The bindings build as one library into the user cache, shared by every
//! project naming the same set (`gossamer_driver::binding_library`). Installing
//! one reads the signatures cached beside it, so the resolver and the type
//! checker see its items, and registers its items with the VM and the JIT;
//! the library itself loads on its first call.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use gossamer_binding_host::Signatures;

/// The subcommands that compile or run the project, and so need its
/// bindings.
const BINDING_SUBCOMMANDS: &[&str] = &["build", "check", "doc", "repl", "run", "test"];

fn first_subcommand(args: &[OsString]) -> Option<String> {
    positionals(args).into_iter().next()
}

/// The arguments that are neither options nor the value `--features` takes.
fn positionals(args: &[OsString]) -> Vec<String> {
    let mut out = Vec::new();
    let mut words = args.iter().skip(1).map(|arg| arg.to_string_lossy());
    while let Some(word) = words.next() {
        if word == "--features" {
            words.next();
        } else if !word.starts_with('-') {
            out.push(word.into_owned());
        }
    }
    out
}

/// The features the command line asks for, read as the parser will read them:
/// the bindings build with the Cargo features they turn on, before the
/// command itself runs.
fn feature_request(args: &[OsString]) -> gossamer_pkg::features::FeatureRequest {
    let mut request = gossamer_pkg::features::FeatureRequest::default();
    let mut words = args.iter().skip(1).map(|arg| arg.to_string_lossy());
    while let Some(word) = words.next() {
        let listed = if word == "--features" {
            words.next().map(std::borrow::Cow::into_owned)
        } else {
            word.strip_prefix("--features=").map(str::to_string)
        };
        if let Some(listed) = listed {
            request.features.extend(
                listed
                    .split(',')
                    .filter(|feature| !feature.is_empty())
                    .map(str::to_string),
            );
        } else if word == "--no-default-features" {
            request.no_default = true;
        }
    }
    request
}

/// The path the subcommand names, when its first positional names something
/// on disk; otherwise the subcommand works on the working directory.
fn entry_arg(args: &[OsString]) -> Option<PathBuf> {
    let candidate = PathBuf::from(positionals(args).into_iter().nth(1)?);
    candidate.exists().then_some(candidate)
}

/// Installs the bindings of the project `args` names, when its subcommand
/// compiles or runs it. Answers the number of binding modules installed.
pub fn install_for_args(args: &[OsString]) -> Result<usize> {
    let Some(subcommand) = first_subcommand(args) else {
        return Ok(0);
    };
    if !BINDING_SUBCOMMANDS.contains(&subcommand.as_str()) {
        return Ok(0);
    }
    crate::paths::set_feature_request(feature_request(args));
    let project = match entry_arg(args) {
        Some(entry) => crate::paths::project_context_for_entry(&entry),
        None => crate::paths::project_context(),
    };
    install_for_project(&project)
}

/// Installs the bindings of the project containing `entry`.
pub fn install_for_entry(entry: &Path) -> Result<usize> {
    install_for_project(&crate::paths::project_context_for_entry(entry))
}

fn install_for_project(project: &crate::paths::ProjectContext) -> Result<usize> {
    if !gossamer_resolve::all_external_modules().is_empty() {
        return Ok(gossamer_resolve::all_external_modules().len());
    }
    // The bindings build with the Cargo features the package features turn
    // on. Resolution starts from a file of the project, as a build's entry is.
    if let Some(dir) = project.manifest_dir() {
        crate::paths::install_features(&std::path::absolute(dir.join("project.toml"))?)?;
    }
    // A malformed manifest is an error, never a silent "no bindings". The
    // binding manifest carries the bindings the project's dependencies
    // declare as well as its own.
    let Some(manifest) = project.binding_manifest() else {
        return Ok(0);
    };
    let manifest = manifest.map_err(|err| anyhow!("{err}"))?;
    if manifest.rust_bindings.is_empty() {
        return Ok(0);
    }
    // A binding library is native code like an `extern` block, and the root
    // project's `ffi = false` refuses it before anything builds or loads.
    if !manifest.project.ffi {
        let names: Vec<&str> = manifest.rust_bindings.keys().map(String::as_str).collect();
        return Err(anyhow!(
            "`ffi = false` in project.toml refuses native code, and the build names \
             Rust bindings: {}",
            names.join(", ")
        ));
    }
    let manifest_dir = project.manifest_dir().unwrap_or_else(|| PathBuf::from("."));
    // One library for the whole set, so crates two bindings share are one
    // copy in the process, as in a native build's link.
    let built = gossamer_driver::binding_library::ensure_bindings_library(
        &manifest.rust_bindings,
        &manifest_dir,
    )
    .with_context(|| {
        let names: Vec<&str> = manifest.rust_bindings.keys().map(String::as_str).collect();
        format!("building the Rust bindings {}", names.join(", "))
    })?;
    let signatures = if built.has_signatures() {
        Signatures::read(&built.signatures)?
    } else {
        let signatures = gossamer_binding_host::read_library_signatures(&built.library)?;
        signatures.write(&built.signatures)?;
        signatures
    };
    let libraries = [(built.library, signatures)];
    Ok(gossamer_binding_host::install(&libraries))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(|s| OsString::from(*s)).collect()
    }

    /// `gos run path/to/src/main.gos` and `gos build path/to/src/main.gos`
    /// name the same program, so both take the manifest beside it.
    #[test]
    fn the_entry_argument_names_the_project() {
        let root = std::env::temp_dir().join(format!("gos-binding-entry-{}", std::process::id()));
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("scratch project");
        std::fs::write(src.join("main.gos"), "fn main() {}\n").expect("entry");
        let entry = src.join("main.gos");
        assert_eq!(
            entry_arg(&argv(&["gos", "run", entry.to_str().expect("utf-8 path")])).as_deref(),
            Some(entry.as_path()),
        );
        assert_eq!(
            entry_arg(&argv(&["gos", "build", root.to_str().expect("utf-8 path")])).as_deref(),
            Some(root.as_path()),
        );
        assert_eq!(entry_arg(&argv(&["gos", "run"])), None);
        assert_eq!(entry_arg(&argv(&["gos", "build", "--release"])), None);
        assert_eq!(entry_arg(&argv(&["gos", "run", "no-such-file.gos"])), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn first_subcommand_skips_flags() {
        assert_eq!(
            first_subcommand(&argv(&["gos", "--quiet", "build", "x.gos"])).as_deref(),
            Some("build")
        );
        assert!(first_subcommand(&argv(&["gos"])).is_none());
    }

    #[test]
    fn a_command_that_compiles_nothing_installs_nothing() {
        assert_eq!(
            install_for_args(&argv(&["gos", "fmt", "x.gos"])).unwrap(),
            0
        );
        assert_eq!(install_for_args(&argv(&["gos", "--version"])).unwrap(), 0);
    }
}
