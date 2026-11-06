//! Shared macOS deployment-target policy for compiler subprocesses.

use std::process::Command;

/// The oldest supported macOS: release artifacts and the programs `gos`
/// builds run there and later, wherever they are built.
pub const DEFAULT_MACOSX_DEPLOYMENT_TARGET: &str = "12.0";

/// Environment variable understood by Cargo, rustc, clang, and Apple ld.
pub const MACOSX_DEPLOYMENT_TARGET_ENV: &str = "MACOSX_DEPLOYMENT_TARGET";

/// Returns the configured target, falling back to [`default_deployment_target`].
///
/// A non-empty environment value is intentionally preserved so users building
/// the complete toolchain from source can opt into an unsupported older macOS
/// target.
#[must_use]
pub fn effective_deployment_target() -> String {
    resolve_deployment_target(std::env::var(MACOSX_DEPLOYMENT_TARGET_ENV).ok().as_deref())
}

/// Resolves an optional environment value, falling back to [`default_deployment_target`].
#[must_use]
pub fn resolve_deployment_target(configured: Option<&str>) -> String {
    configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(default_deployment_target, str::to_string)
}

/// The supported default, lowered to the running macOS when that is older.
///
/// A native build on an older Mac then targets the system it runs on, which
/// works there although only the default is supported.
#[must_use]
pub fn default_deployment_target() -> String {
    default_for_host(host_macos_version().as_deref())
}

fn default_for_host(host_version: Option<&str>) -> String {
    match (
        host_version.and_then(major_minor),
        major_minor(DEFAULT_MACOSX_DEPLOYMENT_TARGET),
    ) {
        (Some(host), Some(default)) if host < default => format!("{}.{}", host.0, host.1),
        _ => DEFAULT_MACOSX_DEPLOYMENT_TARGET.to_string(),
    }
}

fn major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |part| part.parse().ok())?;
    Some((major, minor))
}

/// The running macOS's product version (`12.7.6`), or `None` off macOS.
fn host_macos_version() -> Option<String> {
    static VERSION: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    if !cfg!(target_os = "macos") {
        return None;
    }
    VERSION
        .get_or_init(|| {
            let output = Command::new("sw_vers")
                .arg("-productVersion")
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .clone()
}

/// Reports whether a Cargo target is macOS.
///
/// A missing triple means a native build, so callers pass whether their host
/// is macOS. An explicit triple always wins over the host.
#[must_use]
pub fn is_macos_target(target_triple: Option<&str>, host_is_macos: bool) -> bool {
    target_triple.map_or(host_is_macos, |target| target.ends_with("-apple-darwin"))
}

/// Applies a resolved deployment target to a child process.
pub fn set_command_deployment_target(command: &mut Command, deployment_target: &str) {
    command.env(MACOSX_DEPLOYMENT_TARGET_ENV, deployment_target);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_macos_12() {
        assert_eq!(default_for_host(None), "12.0");
        assert_eq!(default_for_host(Some("12.0")), "12.0");
        assert_eq!(default_for_host(Some("26.1")), "12.0");
    }

    #[test]
    fn an_older_host_lowers_the_default_to_itself() {
        assert_eq!(default_for_host(Some("11.7.10")), "11.7");
        assert_eq!(default_for_host(Some("12.7.6")), "12.0");
        assert_eq!(default_for_host(Some("not a version")), "12.0");
    }

    #[test]
    fn an_empty_override_falls_back_to_the_default() {
        assert_eq!(
            resolve_deployment_target(Some("")),
            default_deployment_target()
        );
    }

    #[test]
    fn source_build_override_is_preserved() {
        assert_eq!(resolve_deployment_target(Some(" 11.0 ")), "11.0");
    }

    #[test]
    fn explicit_cargo_target_wins_over_host() {
        assert!(is_macos_target(Some("aarch64-apple-darwin"), false));
        assert!(!is_macos_target(Some("aarch64-unknown-linux-gnu"), true));
        assert!(is_macos_target(None, true));
    }

    #[test]
    fn command_inherits_resolved_target() {
        let mut command = Command::new("cargo");
        set_command_deployment_target(&mut command, DEFAULT_MACOSX_DEPLOYMENT_TARGET);
        let configured = command
            .get_envs()
            .find(|(name, _)| *name == MACOSX_DEPLOYMENT_TARGET_ENV)
            .and_then(|(_, value)| value)
            .expect("deployment target environment");
        assert_eq!(configured, "12.0");
    }
}
