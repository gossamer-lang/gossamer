//! Runtime support for `std::os::exec`.
//!
//! Wraps `std::process::Command` in the surface Gossamer programs
//! see. The user-facing API is a `Command` builder that mirrors
//! Go's `os/exec` package:
//!
//! - `Command::new(prog)` constructs a builder with no args.
//! - `cmd.arg(s)` / `cmd.args(xs)` append positional arguments.
//! - `cmd.env(k, v)` overrides an environment variable; `cmd.envs`
//!   merges a map.
//! - `cmd.cwd(path)` sets the working directory.
//! - `cmd.stdin(stdin)` / `cmd.stdout(...)` / `cmd.stderr(...)` wire
//!   I/O streams. `Stdio::piped()` captures into a `Vec<u8>`;
//!   `Stdio::inherit()` is the default.
//! - `cmd.output()` runs the child to completion and returns a
//!   captured `Output { status, stdout, stderr }`.
//! - `cmd.status()` runs to completion and returns the `ExitStatus`.
//! - `cmd.spawn()` returns a `Child` handle that the caller can
//!   wait on later.
//! - `cmd.with_context(ctx)` ties the spawned child to a
//!   cancellation `Context` - SIGTERM on cancel, SIGKILL after the
//!   grace window (configurable via `cmd.cancel_grace(ms)`).
//! - `cmd.process_group(true)` puts the child in a fresh process
//!   group on Unix (and `CREATE_NEW_PROCESS_GROUP` on Windows) so
//!   `child.kill_group()` can take down forked descendants too.
//! - `cmd.signal(sig)` sends an arbitrary signal to the child.
//!
//! Pipelines are built via `Pipeline::of(vec![cmd1, cmd2, ...])`;
//! `Pipeline::output()` runs every stage with stdout→stdin pipes
//! and captures the tail's stdout/stderr.
//!
//! Goroutine semantics: `output()` and `status()` block the calling
//! goroutine on `wait`; the runtime scheduler releases the OS thread
//! while waiting. `spawn()` does not block. The streaming readers
//! returned by `Child::stdout_reader()` cooperatively yield to the
//! scheduler when no bytes are immediately available.

// Narrow `unsafe` carve-out for the POSIX `libc::kill` shim and the
// Windows `TerminateProcess` shim in `send_signal`. Every other site
// in this file is safe-Rust; the two FFI calls validate their inputs
// (positive pid, recognised signum) before crossing the FFI line.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{self, ChildStderr, ChildStdout};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gossamer_runtime::platform::Instant;
use parking_lot::Mutex;

use crate::context::Context;
use crate::io::IoError;

/// Spec for spawning a child process. Built up via the builder
/// methods, then executed via `output`, `status`, or `spawn`.
#[derive(Debug, Clone)]
pub struct Command {
    program: String,
    args: Vec<String>,
    envs: HashMap<String, String>,
    env_clear: bool,
    cwd: Option<PathBuf>,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
    process_group: bool,
    ctx: Option<Context>,
    cancel_grace: Duration,
}

/// What to wire a child's stdin/stdout/stderr to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdio {
    /// Inherit from the parent process. Default.
    Inherit,
    /// Capture into a buffer (only meaningful for stdout/stderr) or
    /// supply an empty buffer (stdin).
    Piped,
    /// Discard / source `/dev/null`-equivalent.
    Null,
}

/// Captured output from a finished child.
#[derive(Debug, Clone)]
pub struct Output {
    /// Exit status of the child.
    pub status: ExitStatus,
    /// Captured bytes from the child's stdout. Empty unless stdout
    /// was set to `Stdio::Piped`.
    pub stdout: Vec<u8>,
    /// Captured bytes from the child's stderr. Empty unless stderr
    /// was set to `Stdio::Piped`.
    pub stderr: Vec<u8>,
}

/// Exit status of a finished child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    code: Option<i32>,
}

impl ExitStatus {
    /// Numeric exit code, or `None` if the child was killed by a
    /// signal (Unix) or terminated abnormally (Windows).
    #[must_use]
    pub const fn code(self) -> Option<i32> {
        self.code
    }

    /// Returns whether the child exited with code 0.
    #[must_use]
    pub fn success(self) -> bool {
        self.code == Some(0)
    }
}

/// Portable signal selector. Crosses platforms via the
/// `signum()` mapping; Windows treats `Term`/`Kill`/`Int` as
/// `TerminateProcess` and rejects the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGTERM - polite termination.
    Term,
    /// SIGKILL - unconditional termination.
    Kill,
    /// SIGSTOP - pause the process.
    Stop,
    /// SIGCONT - resume a stopped process.
    Cont,
    /// SIGHUP - terminal hangup.
    Hup,
    /// SIGINT - interactive interrupt.
    Int,
    /// SIGUSR1 - user-defined signal 1.
    Usr1,
    /// SIGUSR2 - user-defined signal 2.
    Usr2,
    /// SIGPIPE - broken pipe.
    Pipe,
    /// SIGQUIT - quit + dump core.
    Quit,
}

impl Signal {
    /// POSIX signal number for this variant. On Unix the numbers come
    /// from `libc`, so the variants that differ across kernels carry
    /// the correct per-OS value (e.g. SIGUSR1 is 10 on Linux but 30 on
    /// macOS / BSD, and SIGSTOP is 19 on Linux but 17 on macOS). A
    /// single hardcoded Linux table silently mis-sent signals on macOS
    /// - `Stop` resolved to macOS's SIGCONT and resumed the target.
    #[cfg(unix)]
    #[must_use]
    pub const fn signum(self) -> i32 {
        match self {
            Self::Hup => libc::SIGHUP,
            Self::Int => libc::SIGINT,
            Self::Quit => libc::SIGQUIT,
            Self::Kill => libc::SIGKILL,
            Self::Usr1 => libc::SIGUSR1,
            Self::Usr2 => libc::SIGUSR2,
            Self::Pipe => libc::SIGPIPE,
            Self::Term => libc::SIGTERM,
            Self::Stop => libc::SIGSTOP,
            Self::Cont => libc::SIGCONT,
        }
    }

    /// Signals are a POSIX concept; on non-Unix targets the kill path
    /// uses `TerminateProcess` and never consults these numbers. The
    /// conventional Linux values keep the function total and callable.
    #[cfg(not(unix))]
    #[must_use]
    pub const fn signum(self) -> i32 {
        match self {
            Self::Hup => 1,
            Self::Int => 2,
            Self::Quit => 3,
            Self::Kill => 9,
            Self::Usr1 => 10,
            Self::Usr2 => 12,
            Self::Pipe => 13,
            Self::Term => 15,
            Self::Stop => 19,
            Self::Cont => 18,
        }
    }

    /// Short uppercase name (`"TERM"`, `"KILL"`, ...). Useful for
    /// shelling out to `kill -<name>` portability.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Term => "TERM",
            Self::Kill => "KILL",
            Self::Stop => "STOP",
            Self::Cont => "CONT",
            Self::Hup => "HUP",
            Self::Int => "INT",
            Self::Usr1 => "USR1",
            Self::Usr2 => "USR2",
            Self::Pipe => "PIPE",
            Self::Quit => "QUIT",
        }
    }
}

/// Handle to a running child process.
#[derive(Debug)]
pub struct Child {
    inner: Arc<Mutex<Option<process::Child>>>,
    pid: u32,
    process_group: bool,
    stdout_taken: bool,
    stderr_taken: bool,
    raw_stdout: Option<ChildStdout>,
    raw_stderr: Option<ChildStderr>,
    _cancel_thread: Option<thread::JoinHandle<()>>,
}

impl Child {
    /// Blocks until the child exits; returns its [`ExitStatus`].
    pub fn wait(self) -> Result<ExitStatus, IoError> {
        let mut guard = self.inner.lock();
        let Some(child) = guard.as_mut() else {
            return Err(IoError::Other("wait: child already reaped".into()));
        };
        let status = child.wait().map_err(|e| IoError::from_std(e, "wait"))?;
        *guard = None;
        unregister_child(self.pid);
        Ok(ExitStatus {
            code: status.code(),
        })
    }

    /// Waits up to `ms` milliseconds for the child to exit. Returns
    /// `Some(status)` if it finished in time, `None` on timeout
    /// (the child is left alive). Polls with a 25 ms backoff -
    /// short enough to feel responsive, long enough to avoid burning
    /// CPU on long waits.
    pub fn wait_with_timeout(&mut self, ms: u64) -> Result<Option<ExitStatus>, IoError> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            {
                let mut guard = self.inner.lock();
                let Some(child) = guard.as_mut() else {
                    return Ok(Some(ExitStatus { code: None }));
                };
                match child.try_wait() {
                    Ok(Some(status)) => {
                        *guard = None;
                        unregister_child(self.pid);
                        return Ok(Some(ExitStatus {
                            code: status.code(),
                        }));
                    }
                    Ok(None) => {}
                    Err(e) => return Err(IoError::from_std(e, "try_wait")),
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            gossamer_runtime::platform::sleep(Duration::from_millis(25));
        }
    }

    /// Sends SIGKILL (Unix) / `TerminateProcess` (Windows). The
    /// caller must still `wait` on the child afterwards to reap it.
    pub fn kill(&mut self) -> Result<(), IoError> {
        let mut guard = self.inner.lock();
        let Some(child) = guard.as_mut() else {
            return Ok(());
        };
        child.kill().map_err(|e| IoError::from_std(e, "kill"))
    }

    /// Sends an arbitrary signal. On Unix uses `libc::kill`; on
    /// Windows only `Term`/`Kill`/`Int` map to `TerminateProcess`
    /// and the rest return `IoError::Other`.
    pub fn signal(&mut self, sig: Signal) -> Result<(), IoError> {
        send_signal(self.pid as i32, sig, false)
    }

    /// Sends a signal to the whole process group. On Unix this targets the
    /// process group (`kill(-pid, …)`) when the child was started with
    /// `Command::process_group(true)`, otherwise it behaves like `signal()`.
    /// Windows has no process-group signalling here: the group flag is ignored
    /// and the lead process is terminated via `TerminateProcess`, so child
    /// subprocesses are not reached.
    pub fn kill_group(&mut self) -> Result<(), IoError> {
        send_signal(self.pid as i32, Signal::Term, self.process_group)
    }

    /// Returns the child's PID.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Returns a streaming line/chunk reader over the child's
    /// stdout. The child must have been spawned with
    /// `Stdio::Piped`; calling twice (or after `output()`) returns
    /// `None`.
    pub fn stdout_reader(&mut self) -> Option<ChildStdoutReader> {
        if self.stdout_taken {
            return None;
        }
        let raw = self.raw_stdout.take()?;
        self.stdout_taken = true;
        Some(ChildStdoutReader {
            inner: BufReader::new(raw),
        })
    }

    /// Like [`Self::stdout_reader`] but for stderr.
    pub fn stderr_reader(&mut self) -> Option<ChildStderrReader> {
        if self.stderr_taken {
            return None;
        }
        let raw = self.raw_stderr.take()?;
        self.stderr_taken = true;
        Some(ChildStderrReader {
            inner: BufReader::new(raw),
        })
    }
}

/// Streaming line/chunk reader over a child's stdout. Returned by
/// [`Child::stdout_reader`].
pub struct ChildStdoutReader {
    inner: BufReader<ChildStdout>,
}

impl ChildStdoutReader {
    /// Blocking read of the next newline-terminated line. The
    /// trailing newline is stripped. Returns `None` at EOF.
    pub fn read_line(&mut self) -> Option<String> {
        read_one_line(&mut self.inner)
    }

    /// Reads up to `n` bytes, yielding to the scheduler if no
    /// data is immediately available.
    pub fn read_chunk(&mut self, n: usize) -> Vec<u8> {
        read_chunk_yielding(&mut self.inner, n)
    }
}

/// Streaming line/chunk reader over a child's stderr.
pub struct ChildStderrReader {
    inner: BufReader<ChildStderr>,
}

impl ChildStderrReader {
    /// Blocking read of the next newline-terminated line.
    pub fn read_line(&mut self) -> Option<String> {
        read_one_line(&mut self.inner)
    }

    /// Reads up to `n` bytes, yielding to the scheduler if no
    /// data is immediately available.
    pub fn read_chunk(&mut self, n: usize) -> Vec<u8> {
        read_chunk_yielding(&mut self.inner, n)
    }
}

fn read_one_line<R: BufRead>(reader: &mut R) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => {
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            Some(line)
        }
        Err(_) => None,
    }
}

fn read_chunk_yielding<R: Read>(reader: &mut R, n: usize) -> Vec<u8> {
    if n == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; n];
    // Cooperative yield: hand off the OS thread before we block on
    // `read`. Safe whether the caller is a plain OS thread or a
    // goroutine-backed worker; the cost is one syscall on hot
    // streaming loops which is dominated by the I/O wait itself.
    thread::yield_now();
    match reader.read(&mut buf) {
        Ok(0) => Vec::new(),
        Ok(read) => {
            buf.truncate(read);
            buf
        }
        Err(_) => Vec::new(),
    }
}

impl Command {
    /// Constructs a builder for `program`. Args, env, and cwd default
    /// to inherited.
    #[must_use]
    pub fn new<S: Into<String>>(program: S) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: HashMap::new(),
            env_clear: false,
            cwd: None,
            stdin: Stdio::Inherit,
            stdout: Stdio::Inherit,
            stderr: Stdio::Inherit,
            process_group: false,
            ctx: None,
            cancel_grace: Duration::from_secs(5),
        }
    }

    /// Appends a positional argument.
    #[must_use]
    pub fn arg<S: Into<String>>(mut self, arg: S) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends every entry of `args` in order.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets an environment variable in the child. Stacks; the last
    /// `env` for a given key wins.
    #[must_use]
    pub fn env<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.envs.insert(key.into(), value.into());
        self
    }

    /// Wipes the parent's environment for the child; only entries
    /// supplied via [`Self::env`] are passed through.
    #[must_use]
    pub fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self
    }

    /// Sets the child's working directory.
    #[must_use]
    pub fn cwd<P: Into<PathBuf>>(mut self, dir: P) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Wires the child's stdin.
    #[must_use]
    pub const fn stdin(mut self, stdio: Stdio) -> Self {
        self.stdin = stdio;
        self
    }

    /// Wires the child's stdout.
    #[must_use]
    pub const fn stdout(mut self, stdio: Stdio) -> Self {
        self.stdout = stdio;
        self
    }

    /// Wires the child's stderr.
    #[must_use]
    pub const fn stderr(mut self, stdio: Stdio) -> Self {
        self.stderr = stdio;
        self
    }

    /// Spawns the child in its own process group. Unix calls
    /// `setpgid(0,0)` after fork via `process::Command::process_group`
    /// (Rust 1.64+). Windows passes `CREATE_NEW_PROCESS_GROUP`. The
    /// group flag enables [`Child::kill_group`].
    #[must_use]
    pub const fn process_group(mut self, enabled: bool) -> Self {
        self.process_group = enabled;
        self
    }

    /// Ties the spawned child to a [`Context`]. When the context
    /// cancels (or its deadline elapses), a watcher thread sends
    /// SIGTERM, waits the grace window, then sends SIGKILL.
    #[must_use]
    pub fn with_context(mut self, ctx: &Context) -> Self {
        self.ctx = Some(ctx.clone());
        self
    }

    /// Time between SIGTERM and SIGKILL when a context cancels
    /// the child. Defaults to 5000 ms.
    #[must_use]
    pub const fn cancel_grace(mut self, ms: u64) -> Self {
        self.cancel_grace = Duration::from_millis(ms);
        self
    }

    fn build(&self) -> process::Command {
        let mut cmd = process::Command::new(&self.program);
        cmd.args(&self.args);
        if self.env_clear {
            cmd.env_clear();
        }
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(map_stdio(self.stdin));
        cmd.stdout(map_stdio(self.stdout));
        cmd.stderr(map_stdio(self.stderr));
        apply_process_group(&mut cmd, self.process_group);
        cmd
    }

    /// Runs the child to completion and returns its captured output.
    pub fn output(&self) -> Result<Output, IoError> {
        let raw = self
            .build()
            .output()
            .map_err(|e| IoError::from_std(e, &self.program))?;
        Ok(Output {
            status: ExitStatus {
                code: raw.status.code(),
            },
            stdout: raw.stdout,
            stderr: raw.stderr,
        })
    }

    /// Runs the child to completion and returns just its exit status.
    /// Stdin/stdout/stderr are inherited unless overridden.
    pub fn status(&self) -> Result<ExitStatus, IoError> {
        let raw = self
            .build()
            .status()
            .map_err(|e| IoError::from_std(e, &self.program))?;
        Ok(ExitStatus { code: raw.code() })
    }

    /// Starts the child and returns a [`Child`] handle without
    /// waiting.
    pub fn spawn(&self) -> Result<Child, IoError> {
        let mut raw = self
            .build()
            .spawn()
            .map_err(|e| IoError::from_std(e, &self.program))?;
        let pid = raw.id();
        let raw_stdout = raw.stdout.take();
        let raw_stderr = raw.stderr.take();
        register_child(pid, self.process_group);
        let inner = Arc::new(Mutex::new(Some(raw)));
        let cancel_thread = self
            .ctx
            .as_ref()
            .map(|ctx| spawn_cancel_watcher(ctx, pid, self.cancel_grace, &inner));
        Ok(Child {
            inner,
            pid,
            process_group: self.process_group,
            stdout_taken: false,
            stderr_taken: false,
            raw_stdout,
            raw_stderr,
            _cancel_thread: cancel_thread,
        })
    }
}

fn spawn_cancel_watcher(
    ctx: &Context,
    pid: u32,
    grace: Duration,
    inner: &Arc<Mutex<Option<process::Child>>>,
) -> thread::JoinHandle<()> {
    let ctx = ctx.clone();
    let inner = Arc::clone(inner);
    thread::spawn(move || {
        // Poll-wait on cancellation so we don't depend on the
        // context's hook plumbing.
        loop {
            if ctx.is_cancelled() {
                break;
            }
            if has_exited(&inner) {
                return;
            }
            gossamer_runtime::platform::sleep(Duration::from_millis(50));
        }
        let _ = send_signal(pid as i32, Signal::Term, false);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if has_exited(&inner) {
                return;
            }
            gossamer_runtime::platform::sleep(Duration::from_millis(50));
        }
        let _ = send_signal(pid as i32, Signal::Kill, false);
    })
}

fn has_exited(inner: &Arc<Mutex<Option<process::Child>>>) -> bool {
    let mut guard = inner.lock();
    let Some(child) = guard.as_mut() else {
        return true;
    };
    matches!(child.try_wait(), Ok(Some(_)))
}

fn map_stdio(s: Stdio) -> process::Stdio {
    match s {
        Stdio::Inherit => process::Stdio::inherit(),
        Stdio::Piped => process::Stdio::piped(),
        Stdio::Null => process::Stdio::null(),
    }
}

#[cfg(unix)]
fn apply_process_group(cmd: &mut process::Command, enabled: bool) {
    if enabled {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
}

#[cfg(windows)]
fn apply_process_group(cmd: &mut process::Command, enabled: bool) {
    if enabled {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP = 0x0000_0200.
        cmd.creation_flags(0x0000_0200);
    }
}

#[cfg(not(any(unix, windows)))]
fn apply_process_group(_cmd: &mut process::Command, _enabled: bool) {}

/// Children this process started and has not reaped, with whether each leads
/// a process group of its own.
///
/// A host that ends while a child is still running - the REPL leaving its
/// loop, a supervisor interrupted - would otherwise orphan it, and an
/// orphan that is looping keeps consuming the machine with nobody left to
/// stop it. Registration is what lets the host end what it started.
static LIVE_CHILDREN: Mutex<Vec<(u32, bool)>> = Mutex::new(Vec::new());

fn register_child(pid: u32, group: bool) {
    LIVE_CHILDREN.lock().push((pid, group));
}

fn unregister_child(pid: u32) {
    LIVE_CHILDREN.lock().retain(|(live, _)| *live != pid);
}

/// Ends every child this process started and has not reaped.
///
/// A child leading its own process group is signalled through the group, so
/// what it started goes with it. Each is asked to stop and then killed, and
/// a child that has already exited is skipped.
pub fn terminate_live_children() {
    let live: Vec<(u32, bool)> = std::mem::take(&mut *LIVE_CHILDREN.lock());
    if live.is_empty() {
        return;
    }
    for (pid, group) in &live {
        let _ = send_signal(*pid as i32, Signal::Term, *group);
    }
    gossamer_runtime::platform::sleep(Duration::from_millis(100));
    for (pid, group) in &live {
        let _ = send_signal(*pid as i32, Signal::Kill, *group);
    }
}

/// Sends `sig` to the process group led by `pid`.
///
/// The caller started the leader in a group of its own, so one signal reaches
/// every process it went on to start. A group that has already exited answers
/// with an error rather than a fault.
pub fn signal_process_group(pid: u32, sig: Signal) -> Result<(), IoError> {
    send_signal(
        i32::try_from(pid).map_err(|_| IoError::Other(format!("invalid pid {pid}")))?,
        sig,
        true,
    )
}

#[cfg(unix)]
#[allow(
    unsafe_code,
    reason = "POSIX kill(2) shim; pid + signum validated before FFI call"
)]
fn send_signal(pid: i32, sig: Signal, group: bool) -> Result<(), IoError> {
    if pid <= 0 {
        return Err(IoError::Other(format!("invalid pid {pid}")));
    }
    let target: libc::pid_t = if group { -pid } else { pid };
    // SAFETY: libc::kill validates pid + sig and returns -1 on
    // failure; never crashes the caller.
    let rc = unsafe { libc::kill(target, sig.signum()) };
    if rc == 0 {
        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        Err(IoError::from_std(err, "kill"))
    }
}

#[cfg(windows)]
fn send_signal(pid: i32, sig: Signal, group: bool) -> Result<(), IoError> {
    if pid <= 0 {
        return Err(IoError::Other(format!("invalid pid {pid}")));
    }
    match sig {
        Signal::Term | Signal::Kill | Signal::Int => terminate_process(pid as u32, group),
        other => Err(IoError::Other(format!(
            "signal {} is not supported on Windows",
            other.name()
        ))),
    }
}

#[cfg(windows)]
fn terminate_process(pid: u32, _group: bool) -> Result<(), IoError> {
    // SAFETY: Win32 OpenProcess / TerminateProcess / CloseHandle.
    unsafe {
        unsafe extern "system" {
            fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
            fn TerminateProcess(process: isize, exit_code: u32) -> i32;
            fn CloseHandle(object: isize) -> i32;
        }
        const PROCESS_TERMINATE: u32 = 0x0001;
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle == 0 {
            return Err(IoError::Other(format!("OpenProcess({pid}) failed")));
        }
        let ok = TerminateProcess(handle, 1);
        let _ = CloseHandle(handle);
        if ok != 0 {
            Ok(())
        } else {
            Err(IoError::Other(format!("TerminateProcess({pid}) failed")))
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn send_signal(_pid: i32, _sig: Signal, _group: bool) -> Result<(), IoError> {
    Err(IoError::Other(
        "signals are not supported on this platform".into(),
    ))
}

/// Sends a raw signal number to a target pid. Wraps the unsafe
/// `libc::kill` on Unix and `TerminateProcess` on Windows.
/// Returns true on success. Used by the interp builtin so the
/// VM can call into the same audited unsafe surface the compiled
/// tiers use.
#[must_use]
pub fn send_raw_signal(pid: i64, signum: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: libc::kill returns -1 on failure rather than
        // crashing the caller; pid/signum are integer values.
        let rc = unsafe { libc::kill(pid as libc::pid_t, signum as libc::c_int) };
        rc == 0
    }
    #[cfg(windows)]
    {
        let _ = signum;
        terminate_pid(pid as u32)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, signum);
        false
    }
}

/// Sends SIGTERM to an entire process group on Unix; best-effort
/// `TerminateProcess` on Windows.
#[must_use]
pub fn send_group_term(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: libc::kill with a negative pid targets the group.
        let rc = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGTERM) };
        rc == 0
    }
    #[cfg(windows)]
    {
        // CREATE_NEW_PROCESS_GROUP does not make TerminateProcess recursive.
        // taskkill's tree mode terminates the worker and every descendant.
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// The exit code of `pid` once it ends within `ms` milliseconds, `-1` on
/// timeout, or `-2` on unknown-pid / permission-denied / OS error.
#[must_use]
pub fn wait_pid_timeout(pid: i64, ms: i64) -> i64 {
    gossamer_runtime::c_abi::exec::wait_timeout(pid, ms)
}

#[cfg(windows)]
fn terminate_pid(pid: u32) -> bool {
    // SAFETY: see `terminate_process` above.
    unsafe {
        unsafe extern "system" {
            fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
            fn TerminateProcess(process: isize, exit_code: u32) -> i32;
            fn CloseHandle(object: isize) -> i32;
        }
        const PROCESS_TERMINATE: u32 = 0x0001;
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle == 0 {
            return false;
        }
        let ok = TerminateProcess(handle, 1);
        let _ = CloseHandle(handle);
        ok != 0
    }
}

/// Subprocess pipeline. Each stage's stdout is wired to the next
/// stage's stdin; the final stage's stdout (and stderr) feed the
/// pipeline's captured `Output`.
#[derive(Debug, Clone)]
pub struct Pipeline {
    stages: Vec<Command>,
}

impl Pipeline {
    /// Builds a pipeline from `commands` (front-to-back data flow).
    /// At least one stage is required; an empty pipeline returns
    /// `IoError::Other` from `output()` / `status()`.
    #[must_use]
    pub const fn of(commands: Vec<Command>) -> Self {
        Self { stages: commands }
    }

    /// Number of stages in the pipeline.
    #[must_use]
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    /// Returns whether the pipeline has zero stages.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// Runs every stage, returning the final stage's captured
    /// output. Stages are spawned in order; failures to spawn
    /// produce `IoError::Other` with the offending stage index.
    pub fn output(&self) -> Result<Output, IoError> {
        let children = self.spawn_all()?;
        finalize_pipeline(children)
    }

    /// Runs every stage and returns each stage's exit status in
    /// order. Useful for diagnosing which stage of a long pipe
    /// failed.
    pub fn status(&self) -> Result<Vec<ExitStatus>, IoError> {
        let children = self.spawn_all()?;
        let mut statuses = Vec::with_capacity(children.len());
        for (i, mut child) in children.into_iter().enumerate() {
            let raw = child
                .wait()
                .map_err(|e| IoError::from_std(e, &format!("pipeline stage {i}")))?;
            statuses.push(ExitStatus { code: raw.code() });
        }
        Ok(statuses)
    }

    fn spawn_all(&self) -> Result<Vec<process::Child>, IoError> {
        if self.stages.is_empty() {
            return Err(IoError::Other("pipeline: empty".into()));
        }
        let last = self.stages.len() - 1;
        let mut children: Vec<process::Child> = Vec::with_capacity(self.stages.len());
        for (i, stage) in self.stages.iter().enumerate() {
            let mut cmd = stage.build();
            if i > 0 {
                let Some(prev_stdout) = children.last_mut().and_then(|c| c.stdout.take()) else {
                    return Err(IoError::Other(format!(
                        "pipeline stage {i}: predecessor has no piped stdout"
                    )));
                };
                cmd.stdin(prev_stdout);
            }
            if i < last {
                cmd.stdout(process::Stdio::piped());
            } else {
                cmd.stdout(process::Stdio::piped());
                cmd.stderr(process::Stdio::piped());
            }
            let child = cmd.spawn().map_err(|e| {
                IoError::from_std(e, &format!("pipeline stage {i}: {}", stage.program))
            })?;
            children.push(child);
        }
        Ok(children)
    }
}

fn finalize_pipeline(mut children: Vec<process::Child>) -> Result<Output, IoError> {
    let Some(mut tail) = children.pop() else {
        return Err(IoError::Other("pipeline: no stages".into()));
    };
    let mut stdout = Vec::new();
    if let Some(mut s) = tail.stdout.take() {
        let _ = s.read_to_end(&mut stdout);
    }
    let mut stderr = Vec::new();
    if let Some(mut e) = tail.stderr.take() {
        let _ = e.read_to_end(&mut stderr);
    }
    let tail_status = tail
        .wait()
        .map_err(|e| IoError::from_std(e, "pipeline tail wait"))?;
    for (i, mut c) in children.into_iter().enumerate() {
        let _ = c
            .wait()
            .map_err(|e| IoError::from_std(e, &format!("pipeline stage {i} wait")))?;
    }
    Ok(Output {
        status: ExitStatus {
            code: tail_status.code(),
        },
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_program() -> &'static str {
        if cfg!(target_os = "windows") {
            "cmd"
        } else {
            "sh"
        }
    }

    fn echo_args(text: &str) -> Vec<String> {
        if cfg!(target_os = "windows") {
            vec!["/C".to_string(), format!("echo {text}")]
        } else {
            vec!["-c".to_string(), format!("printf '%s' {text}")]
        }
    }

    #[test]
    fn output_captures_stdout() {
        let cmd = Command::new(echo_program())
            .args(echo_args("hello"))
            .stdout(Stdio::Piped)
            .stderr(Stdio::Piped);
        let out = cmd.output().expect("output");
        assert!(out.status.success(), "exit code: {:?}", out.status.code());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("hello"), "stdout was: {text:?}");
    }

    #[test]
    fn status_reports_zero_for_success() {
        let cmd = Command::new(echo_program())
            .args(echo_args(""))
            .stdout(Stdio::Null)
            .stderr(Stdio::Null);
        let status = cmd.status().expect("status");
        assert!(status.success());
        assert_eq!(status.code(), Some(0));
    }

    #[test]
    fn nonzero_exit_is_not_a_rust_error() {
        let cmd = Command::new(echo_program())
            .args({
                if cfg!(target_os = "windows") {
                    vec!["/C".to_string(), "exit 7".to_string()]
                } else {
                    vec!["-c".to_string(), "exit 7".to_string()]
                }
            })
            .stdout(Stdio::Null)
            .stderr(Stdio::Null);
        let status = cmd.status().expect("status");
        assert!(!status.success());
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn env_overrides_propagate_to_child() {
        if cfg!(target_os = "windows") {
            return;
        }
        let cmd = Command::new("sh")
            .args(["-c", "printf '%s' \"$GOSSAMER_TEST_VAR\""].map(String::from))
            .env("GOSSAMER_TEST_VAR", "value123")
            .stdout(Stdio::Piped)
            .stderr(Stdio::Piped);
        let out = cmd.output().expect("output");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "value123");
    }

    #[test]
    fn missing_program_returns_io_error_not_panic() {
        let cmd = Command::new("definitely-not-a-real-binary-xyzzy-zorch");
        match cmd.output() {
            Ok(_) => panic!("should have failed"),
            Err(IoError::NotFound(_) | IoError::Other(_)) => {}
            Err(e) => panic!("unexpected error kind: {e}"),
        }
    }

    #[test]
    fn spawn_kill_wait_round_trip() {
        if cfg!(target_os = "windows") {
            return;
        }
        let mut child = Command::new("sleep")
            .arg("60")
            .stdout(Stdio::Null)
            .spawn()
            .expect("spawn");
        let pid = child.pid();
        assert!(pid > 0);
        child.kill().expect("kill");
        let status = child.wait().expect("wait");
        // Killed by signal: exit code is None on Unix.
        assert!(!status.success());
    }

    #[test]
    fn pipeline_chains_echo_tr_to_uppercase() {
        if cfg!(target_os = "windows") {
            return;
        }
        let stage1 = Command::new("sh").args(["-c", "printf 'hello'"].map(String::from));
        let stage2 = Command::new("tr").args(["a-z", "A-Z"].map(String::from));
        let pipe = Pipeline::of(vec![stage1, stage2]);
        let out = pipe.output().expect("pipeline output");
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(text.trim(), "HELLO", "stdout was: {text:?}");
        assert!(out.status.success());
    }

    #[test]
    fn pipeline_status_reports_every_stage() {
        if cfg!(target_os = "windows") {
            return;
        }
        let stage1 = Command::new("sh").args(["-c", "printf 'abc'"].map(String::from));
        let stage2 = Command::new("cat");
        let pipe = Pipeline::of(vec![stage1, stage2]);
        let statuses = pipe.status().expect("status");
        assert_eq!(statuses.len(), 2);
        for (i, s) in statuses.iter().enumerate() {
            assert!(s.success(), "stage {i} did not succeed: {:?}", s.code());
        }
    }

    #[test]
    fn wait_with_timeout_returns_none_on_long_sleep() {
        if cfg!(target_os = "windows") {
            return;
        }
        let mut child = Command::new("sleep")
            .arg("5")
            .stdout(Stdio::Null)
            .spawn()
            .expect("spawn sleep");
        let r = child.wait_with_timeout(200).expect("timeout");
        assert!(r.is_none(), "expected timeout, got {r:?}");
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn wait_with_timeout_returns_status_when_child_finishes_fast() {
        if cfg!(target_os = "windows") {
            return;
        }
        let mut child = Command::new("sh")
            .args(["-c", "exit 4"].map(String::from))
            .spawn()
            .expect("spawn quick");
        let r = child.wait_with_timeout(5000).expect("ok");
        let Some(status) = r else {
            panic!("expected status, got None");
        };
        assert_eq!(status.code(), Some(4));
    }

    #[test]
    fn stdout_reader_yields_lines_in_order() {
        if cfg!(target_os = "windows") {
            return;
        }
        let mut child = Command::new("sh")
            .args(["-c", "for i in 1 2 3 4 5; do echo line$i; done"].map(String::from))
            .stdout(Stdio::Piped)
            .spawn()
            .expect("spawn");
        let mut reader = child.stdout_reader().expect("reader");
        let mut got = Vec::new();
        while let Some(line) = reader.read_line() {
            got.push(line);
        }
        child.wait().ok();
        assert_eq!(got, vec!["line1", "line2", "line3", "line4", "line5"]);
    }

    #[test]
    fn signal_term_kills_long_sleep() {
        if cfg!(target_os = "windows") {
            return;
        }
        let mut child = Command::new("sleep")
            .arg("60")
            .stdout(Stdio::Null)
            .spawn()
            .expect("spawn");
        child.signal(Signal::Term).expect("signal");
        let status = child.wait().expect("wait");
        assert!(!status.success());
    }

    #[test]
    fn context_cancel_kills_child_within_grace() {
        if cfg!(target_os = "windows") {
            return;
        }
        let (ctx, cancel) = crate::context::with_cancel(&Context::background());
        let mut child = Command::new("sleep")
            .arg("60")
            .stdout(Stdio::Null)
            .with_context(&ctx)
            .cancel_grace(500)
            .spawn()
            .expect("spawn");
        // Cancel after a short delay so the watcher thread observes
        // the live child first.
        let _ = std::thread::spawn(move || {
            gossamer_runtime::platform::sleep(Duration::from_millis(80));
            cancel.cancel();
        });
        let start = Instant::now();
        let r = child.wait_with_timeout(3000).expect("timeout");
        let elapsed = start.elapsed();
        assert!(r.is_some(), "child should have exited within 3 s");
        assert!(
            elapsed < Duration::from_secs(2),
            "child took {elapsed:?} to die after cancel"
        );
    }

    #[test]
    fn kill_group_signals_descendants() {
        if cfg!(target_os = "windows") {
            return;
        }
        // Parent forks two background sleeps; killing the group
        // should reap them all. We assert the parent exits (via
        // kill_group) - we can't trivially observe the grandchildren
        // without ps, so the lifecycle assertion is the proxy.
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & sleep 30 & wait"].map(String::from))
            .stdout(Stdio::Null)
            .process_group(true)
            .spawn()
            .expect("spawn group");
        gossamer_runtime::platform::sleep(Duration::from_millis(100));
        child.kill_group().expect("kill group");
        let status = child.wait().expect("wait");
        assert!(!status.success());
    }
}
