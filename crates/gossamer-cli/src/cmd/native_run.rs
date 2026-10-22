//! `gos run` runs a program as the native executable `gos build` makes of
//! it, as `go run` does: every run behaves as the shipped binary does. A
//! current release build starts at once; otherwise the program builds with
//! the debug profile, reusing every object an earlier build left, and starts
//! that, while the release build follows in the background for the next run.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::cmd::build::{BACKGROUND_BUILD_KEY, BACKGROUND_BUILD_LOCK, failed_build_record};

/// Names the engine a run uses: `vm` keeps every run on the bytecode VM and
/// the JIT; anything else runs the program natively.
pub(crate) const RUN_ENGINE: &str = "GOS_RUN_ENGINE";

/// Variables that ask the VM or the JIT to report on or change a run, which a
/// native executable cannot honour.
const VM_ONLY_VARS: &[&str] = &["GOS_DUMP_CLIF", "GOS_PROFILE_RSS", "GOS_VM_PROFILE"];

/// Prefixes of the VM's and the JIT's own families of variables.
const VM_ONLY_PREFIXES: &[&str] = &["GOS_JIT", "GOSSAMER_JIT", "GOSSAMER_VM_"];

/// Whether this run may be native: neither the command line nor the
/// environment asks for the VM.
pub(crate) fn allowed() -> bool {
    if std::env::var(RUN_ENGINE).is_ok_and(|engine| engine == "vm") {
        return false;
    }
    !std::env::vars_os().any(|(name, _)| {
        name.to_str().is_some_and(|name| {
            VM_ONLY_VARS.contains(&name)
                || VM_ONLY_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
        })
    })
}

/// Why a run did not start natively.
pub(crate) enum NotNative {
    /// The entry is a library, which has no `main` to start.
    Library,
    /// This host cannot build native code, for the reason given.
    Unavailable(String),
}

/// Where `gos run` keeps the builds of `entry`: the project's `.gos-cache`,
/// or for a file outside any project a directory of the user cache named
/// for the file.
fn build_dir(entry: &Path) -> PathBuf {
    if let Some(root) = crate::paths::project_root_for_entry(entry) {
        return root.join(".gos-cache").join("run");
    }
    let absolute = std::fs::canonicalize(entry).unwrap_or_else(|_| entry.to_path_buf());
    let name = gossamer_pkg::sha256::hex(absolute.to_string_lossy().as_bytes());
    gossamer_driver::user_cache_root()
        .join("run")
        .join(&name[..32])
}

/// Runs `entry`, bundled as `source`, natively with `forwarded` as its
/// arguments. Answers only when the run does not happen natively; a program
/// that ran ends this process with its exit code.
pub(crate) fn run(
    entry: &Path,
    source: &str,
    label: &str,
    forwarded: &[String],
) -> Result<NotNative> {
    let dir = build_dir(entry);
    let Some(release) = crate::cmd::build::run_artifact(entry, source, &dir, true) else {
        return Ok(NotNative::Library);
    };
    if release.current {
        return Ok(start(&release.path, label, forwarded));
    }
    let debug_dir = dir.join("debug");
    let Some(debug) = crate::cmd::build::run_artifact(entry, source, &debug_dir, false) else {
        return Ok(NotNative::Library);
    };
    if !debug.current {
        if let Some(problem) = toolchain_problem() {
            return Ok(NotNative::Unavailable(problem));
        }
        crate::cmd::build::build_for_run(entry, &debug_dir, false)?;
    }
    build_in_background(entry, &dir, &release.key);
    Ok(start(&debug.path, label, forwarded))
}

/// Why this host cannot link a native program, or `None` when it can.
fn toolchain_problem() -> Option<String> {
    if let Err(err) = crate::cmd::build::find_runtime_lib() {
        return Some(err.user_message());
    }
    gossamer_codegen_llvm::debug_toolchain_problem()
}

/// Starts `executable` as the program; answers only when it cannot be
/// started, which names the reason.
fn start(executable: &Path, label: &str, forwarded: &[String]) -> NotNative {
    let mut command = Command::new(executable);
    command.args(forwarded);
    NotNative::Unavailable(format!(
        "starting {}: {}",
        executable.display(),
        exec(command, label)
    ))
}

/// Replaces this process with the program, so it keeps the process's
/// identity, terminal, and signals; answers only when that fails.
#[cfg(unix)]
fn exec(mut command: Command, label: &str) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    // The program sees the name it was run as, as it does on the VM.
    command.arg0(label);
    command.exec()
}

/// Windows cannot replace a process, so the program runs as a child whose
/// exit code becomes this one's.
#[cfg(not(unix))]
fn exec(mut command: Command, _label: &str) -> std::io::Error {
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(err) => err,
    }
}

/// Starts `gos build --release` of `entry` into `dir`, detached, unless those
/// inputs already failed to build or a build already holds the lock; the
/// build lowers its own priority.
fn build_in_background(entry: &Path, dir: &Path, key: &str) {
    let lock_path = dir.join("build.lock");
    if std::fs::read_to_string(failed_build_record(&lock_path)).is_ok_and(|failed| failed == key) {
        return;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(lock) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
    else {
        return;
    };
    if lock.try_lock().is_err() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Ok(log) = std::fs::File::create(dir.join("build.log")) else {
        return;
    };
    let Ok(log_err) = log.try_clone() else {
        return;
    };
    let mut command = Command::new(exe);
    let request = crate::paths::feature_request();
    if !request.features.is_empty() {
        command.arg("--features").arg(request.features.join(","));
    }
    if request.no_default {
        command.arg("--no-default-features");
    }
    command
        .arg("build")
        .arg("--release")
        .arg("--out-dir")
        .arg(dir)
        .arg(entry)
        .env(BACKGROUND_BUILD_LOCK, &lock_path)
        .env(BACKGROUND_BUILD_KEY, key)
        .stdout(log)
        .stderr(log_err);
    hand_over_lock(&mut command, lock);
    detach(&mut command);
    let _ = command.spawn();
}

/// Gives the build the lock this process took, as its stdin: a lock belongs
/// to the open file, which the build then holds from the moment it starts
/// until it ends, so no second build starts in between and anyone waiting on
/// the lock waits for this build.
#[cfg(unix)]
fn hand_over_lock(command: &mut Command, lock: std::fs::File) {
    command
        .stdin(lock)
        .env(crate::cmd::build::BACKGROUND_BUILD_LOCK_HELD, "1");
}

/// Elsewhere a lock does not pass to a child, so the build takes it itself
/// once this process lets it go.
#[cfg(not(unix))]
fn hand_over_lock(command: &mut Command, lock: std::fs::File) {
    drop(lock);
    command.stdin(std::process::Stdio::null());
}

/// Puts the build in a process group of its own, so an interrupt sent to the
/// program leaves the build running.
#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// Starts the build with no console of its own, in a process group of its
/// own.
#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

/// Other targets start the build as it is.
#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {}
