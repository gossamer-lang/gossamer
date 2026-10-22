//! `gos run --native` runs a program as the native executable `gos build`
//! makes of it, as `go run` does: the run behaves as the shipped binary does.
//! A current release build starts at once; otherwise the program builds with
//! the debug profile, reusing every object an earlier build left, and starts
//! that, while the release build follows in the background for the next run.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::cmd::build::{BACKGROUND_BUILD_KEY, BACKGROUND_BUILD_LOCK, failed_build_record};

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
    // A dump or trace the environment asks for comes from a compile, which
    // a current build skips.
    let reports = crate::cmd::build::compile_reports_requested();
    if release.current && !reports {
        return Ok(start(&release.path, label, forwarded));
    }
    let debug_dir = dir.join("debug");
    let Some(debug) = crate::cmd::build::run_artifact(entry, source, &debug_dir, false) else {
        return Ok(NotNative::Library);
    };
    if !debug.current || reports {
        if let Some(problem) = toolchain_problem() {
            return Ok(NotNative::Unavailable(problem));
        }
        build_before_run(entry, &debug_dir)?;
    }
    build_in_background(entry, &dir, &release.key);
    Ok(start(&debug.path, label, forwarded))
}

/// Builds the debug executable of `entry` into `dir` with a `gos build` of
/// its own, whose output is the run's only when the build fails or the
/// environment asks the compile to report: the program owns the run's stdout
/// and stderr, and what a successful build has to say
/// about the toolchain (an LLVM version note, a linker warning) goes to
/// `dir/build.log` instead. A program that does not build ends the run with
/// the build's exit code after its diagnostics.
fn build_before_run(entry: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut command = build_command(entry, dir, false)?;
    command.stdin(std::process::Stdio::null());
    if crate::paths::stderr_supports_colour() {
        command.env("CLICOLOR_FORCE", "1");
    }
    // A dump or trace the environment asked for is for its reader, so the
    // build's stderr is the run's.
    if crate::cmd::build::compile_reports_requested() {
        command.stderr(std::process::Stdio::inherit());
    }
    let output = command.output()?;
    let mut log = output.stdout;
    log.extend_from_slice(&output.stderr);
    let _ = std::fs::write(dir.join("build.log"), &log);
    if output.status.success() {
        return Ok(());
    }
    {
        use std::io::Write as _;
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(&log);
        let _ = stderr.flush();
    }
    std::process::exit(output.status.code().unwrap_or(1));
}

/// A `gos build` of `entry` into `dir`, with the features and the
/// compile-time capabilities this command was given, building for a run.
fn build_command(entry: &Path, dir: &Path, release: bool) -> Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    let request = crate::paths::feature_request();
    if !request.features.is_empty() {
        command.arg("--features").arg(request.features.join(","));
    }
    if request.no_default {
        command.arg("--no-default-features");
    }
    if let Some(level) = crate::comptime_fold::command_line_flag() {
        command.arg("--comptime-io").arg(level);
    }
    command.arg("build");
    if release {
        command.arg("--release");
    }
    command
        .arg("--out-dir")
        .arg(dir)
        .arg(entry)
        .env(crate::cmd::build::RUN_BUILD, "1");
    Ok(command)
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
    let Ok(mut command) = build_command(entry, dir, true) else {
        return;
    };
    let Ok(log) = std::fs::File::create(dir.join("build.log")) else {
        return;
    };
    let Ok(log_err) = log.try_clone() else {
        return;
    };
    command
        .env(BACKGROUND_BUILD_LOCK, &lock_path)
        .env(BACKGROUND_BUILD_KEY, key)
        .stdout(log)
        .stderr(log_err);
    hand_over_lock(&mut command, lock);
    detach(&mut command);
    let _ = gossamer_runtime::safe_spawn::spawn_apart(&mut command);
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
