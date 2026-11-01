//! Process environment, command-line arguments, and well-known
//! directories. Mirrors Rust's `std::env` shape. The function
//! bodies delegate to the existing implementations in [`crate::os`]
//! during the transition; once the `os::*` filesystem surface is
//! gone the bodies can move here verbatim.

#![forbid(unsafe_code)]

use crate::io::IoError;

/// Returns the program's command-line arguments. The 0th element
/// is the executable path.
#[must_use]
pub fn args() -> Vec<String> {
    std::env::args().collect()
}

/// Returns the path used to invoke the program (argv\[0\]).
#[must_use]
pub fn program_name() -> String {
    std::env::args().next().unwrap_or_default()
}

/// Returns the value of the named environment variable, or `None`
/// if it is unset or contains invalid Unicode.
#[must_use]
pub fn var(name: &str) -> Option<String> {
    gossamer_runtime::safe_env::env_var(name)
}

/// Sets an environment variable. See [`crate::os::set_env`] for
/// the threading contract.
pub fn set_var(name: &str, value: &str) -> Result<(), IoError> {
    gossamer_runtime::safe_env::set_env(name, value);
    Ok(())
}

/// Removes an environment variable. Same threading contract as
/// [`set_var`].
pub fn unset_var(name: &str) {
    gossamer_runtime::safe_env::unset_env(name);
}

/// Returns the current working directory.
pub fn current_dir() -> Result<String, IoError> {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| IoError::from_std(e, ""))
}

/// The path of the running program's executable, with symbolic links resolved where the
/// platform reports them.
pub fn current_exe() -> Result<String, IoError> {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| IoError::from_std(e, ""))
}

/// Changes the current working directory.
pub fn set_current_dir(path: &str) -> Result<(), IoError> {
    std::env::set_current_dir(path).map_err(|e| IoError::from_std(e, path))
}

/// Returns the calling user's home directory if known.
#[must_use]
pub fn home_dir() -> Option<String> {
    #[allow(
        deprecated,
        reason = "std::env::home_dir is the only std path to the home directory and its Windows behaviour is what this wants"
    )]
    std::env::home_dir().map(|p| p.to_string_lossy().into_owned())
}

/// Returns the system's temporary-files directory.
#[must_use]
pub fn temp_dir() -> String {
    gossamer_runtime::platform::temp_dir()
        .to_string_lossy()
        .into_owned()
}

/// Every environment variable this process has, by name.
///
/// The pairs are a snapshot: a later `set_var` does not change a map
/// already handed out.
#[must_use]
pub fn vars() -> Vec<(String, String)> {
    gossamer_runtime::safe_env::env_vars()
}
