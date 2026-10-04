#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]

use std::os::raw::c_char;
use std::time::Duration;

use super::result::gos_rt_result_new;
use super::string::alloc_cstring;
use super::vec::GosVec;

// ---------------------------------------------------------------
// Pipeline, streaming, signal, wait-with-timeout, kill-group.
//
// Every entry uses the flat Ptr/I64 ABI shape so cranelift's
// runtime-symbol table and LLVM's lazy-declare path can wire the
// dispatch without bespoke aggregate plumbing. The pipeline shape
// takes a flat `Vec<String>` where each entry is a single
// whitespace-tokenised command (`"echo hello"` -> `["echo",
// "hello"]`); richer Pipeline construction is available to Rust
// callers through `gossamer_std::exec::Pipeline`.
// ---------------------------------------------------------------

/// `exec::pipeline_run(commands: Vec<String>) -> Result<Output, errors::Error>`.
///
/// Each entry of `commands` is a whitespace-split shell command
/// (single-quote / double-quote groups are honoured; backslashes
/// pass through verbatim). Stages are spawned in order; stdout of
/// stage N feeds stdin of stage N+1. The Ok payload is the counted
/// `(stdout, stderr, code)` tuple `gos_rt_exec_run_raw` answers, which owns
/// both strings. Err payload is `*mut GosError`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_exec_pipeline_run_raw(commands: *mut GosVec) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `commands` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `gather_command_lines` accepts.
        let stages = match unsafe { gather_command_lines(commands) } {
            Ok(s) => s,
            Err(msg) => {
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                return gos_rt_result_new(1, err as i64);
            }
        };
        if stages.is_empty() {
            let err =
                crate::c_abi::errors::error_new_from_bytes(b"exec::pipeline_run: empty pipeline");
            return gos_rt_result_new(1, err as i64);
        }
        match crate::sched_global::run_blocking("exec-pipeline", move || run_pipeline(stages)) {
            Ok(Ok((stdout, stderr, code))) => {
                let stdout_cs = alloc_cstring(stdout.as_bytes()) as i64;
                let stderr_cs = alloc_cstring(stderr.as_bytes()) as i64;
                let blob = crate::c_abi::rc::counted_words(
                    &[stdout_cs, stderr_cs, code],
                    &crate::c_abi::args::OUTPUT_META,
                );
                if blob.is_null() {
                    let err = crate::c_abi::errors::error_new_from_bytes(
                        b"exec::pipeline_run: out of memory",
                    );
                    return gos_rt_result_new(1, err as i64);
                }
                gos_rt_result_new(0, blob as i64)
            }
            Ok(Err(msg)) | Err(msg) => {
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
        }
    })
}

/// # Safety
/// `commands` is null or a live `Vec<String>`.
unsafe fn gather_command_lines(commands: *mut GosVec) -> Result<Vec<Vec<String>>, String> {
    // SAFETY: this `unsafe fn`'s caller passes `commands` null or a live `Vec<String>`.
    let Some(lines) = (unsafe { crate::c_abi::vec::StrVecView::of(commands) }) else {
        return Err("exec::pipeline_run: commands vec is null".into());
    };
    Ok((0..lines.len())
        .filter(|&i| !lines.is_null(i))
        .map(|i| tokenize_shell(&lines.text(i)))
        .filter(|parts| !parts.is_empty())
        .collect())
}

fn tokenize_shell(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in line.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn run_pipeline(stages: Vec<Vec<String>>) -> Result<(String, String, i64), String> {
    use std::process::{Command, Stdio};
    let last = stages.len() - 1;
    let mut children: Vec<std::process::Child> = Vec::with_capacity(stages.len());
    for (i, parts) in stages.iter().enumerate() {
        let mut cmd = Command::new(&parts[0]);
        if parts.len() > 1 {
            cmd.args(&parts[1..]);
        }
        if i > 0 {
            let Some(prev_stdout) = children.last_mut().and_then(|c| c.stdout.take()) else {
                return Err(format!("pipeline stage {i}: predecessor stdout missing"));
            };
            cmd.stdin(prev_stdout);
        }
        cmd.stdout(Stdio::piped());
        if i == last {
            cmd.stderr(Stdio::piped());
        }
        match cmd.spawn() {
            Ok(child) => children.push(child),
            Err(e) => return Err(format!("pipeline stage {i} ({}): {e}", parts[0])),
        }
    }
    use std::io::Read;
    let mut tail = children.pop().expect("checked nonempty");
    let mut stdout_bytes = Vec::new();
    if let Some(mut s) = tail.stdout.take() {
        let _ = s.read_to_end(&mut stdout_bytes);
    }
    let mut stderr_bytes = Vec::new();
    if let Some(mut e) = tail.stderr.take() {
        let _ = e.read_to_end(&mut stderr_bytes);
    }
    let tail_status = tail.wait().map_err(|e| format!("tail wait: {e}"))?;
    for (i, mut c) in children.into_iter().enumerate() {
        let _ = c.wait().map_err(|e| format!("stage {i} wait: {e}"))?;
    }
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    let code = i64::from(tail_status.code().unwrap_or(-1));
    Ok((stdout, stderr, code))
}

/// `exec::signal(pid: i64, signum: i64) -> bool`. Returns true on
/// success. Sends the supplied signal number to the pid via
/// `libc::kill` on Unix; on Windows, recognises only 9 / 15 / 2
/// (KILL / TERM / INT) and routes them through `TerminateProcess`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_exec_signal(pid: i64, signum: i64) -> i64 {
    ffi_entry!(0, {
        if pid <= 0 {
            return 0;
        }
        #[cfg(unix)]
        {
            // SAFETY: libc::kill validates the pid/signum and returns
            // -1 on failure rather than crashing.
            let rc = unsafe { libc::kill(pid as libc::pid_t, signum as libc::c_int) };
            i64::from(rc == 0)
        }
        #[cfg(windows)]
        {
            let _ = signum;
            terminate_pid(pid as u32)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (pid, signum);
            0
        }
    })
}

/// `exec::kill_group(pid: i64) -> bool`. Unix: sends SIGTERM to the
/// process group whose leader is `pid` (equivalent to `kill -- -pid`).
/// Windows: best-effort `TerminateProcess` on the pid itself.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_exec_kill_group(pid: i64) -> i64 {
    ffi_entry!(0, {
        if pid <= 0 {
            return 0;
        }
        #[cfg(unix)]
        {
            // SAFETY: libc::kill with a negative pid targets the
            // entire group; returns -1 on failure.
            let rc = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGTERM) };
            i64::from(rc == 0)
        }
        #[cfg(windows)]
        {
            terminate_pid(pid as u32)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = pid;
            0
        }
    })
}

/// `exec::wait_timeout(pid: i64, ms: i64) -> i64`: the exit code of `pid`
/// once it ends within `ms` milliseconds, `-1` if it is still running at
/// the timeout, `-2` on any other error (unknown pid, permission denied).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_exec_wait_timeout(pid: i64, ms: i64) -> i64 {
    ffi_entry!(-2, { wait_timeout(pid, ms) })
}

/// [`gos_rt_exec_wait_timeout`] for the bytecode tier and the standard
/// library: the calling goroutine parks while the wait runs on the blocking
/// pool.
#[must_use]
pub fn wait_timeout(pid: i64, ms: i64) -> i64 {
    if pid <= 0 || ms < 0 {
        return -2;
    }
    let timeout = Duration::from_millis(ms.unsigned_abs());
    match crate::sched_global::run_blocking("process::wait_timeout", move || {
        wait_exit(pid, Some(timeout))
    }) {
        Ok(Ok(Some(code))) => code,
        Ok(Ok(None)) => -1,
        Ok(Err(_)) | Err(_) => -2,
    }
}

/// Waits for the child `pid` to end, for at most `timeout` (forever when
/// `None`): `Some(code)` once it ends, `None` at the timeout. A child ended
/// by a signal reports 128 plus the signal number. The wait sleeps in the
/// kernel - a process descriptor on Linux, `kqueue` on the BSDs and macOS, the
/// process handle on Windows - rather than polling.
///
/// # Errors
///
/// `pid` is not a child of this process, or the platform reports a failure.
pub fn wait_exit(pid: i64, timeout: Option<Duration>) -> Result<Option<i64>, String> {
    exit_wait::wait(pid, timeout)
}

/// Runs `program` with `args` on the program's own standard input, output,
/// and error, and answers its exit code once it ends; the calling goroutine
/// parks meanwhile. A child ended by a signal reports 128 plus the signal
/// number.
///
/// # Errors
///
/// The program cannot be started.
pub fn run_inherit(program: &str, args: Vec<String>) -> Result<i64, String> {
    // The child writes to the same stream, so what this program printed
    // reaches it first.
    super::gos_rt_flush_stdout();
    let program = program.to_string();
    crate::sched_global::run_blocking("process::run_inherit", move || {
        std::process::Command::new(&program)
            .args(&args)
            .status()
            .map(status_code)
            .map_err(|e| format!("process::run_inherit({program}): {e}"))
    })?
}

/// The exit code a finished child reports, 128 plus the signal number for
/// one a signal ended on Unix.
fn status_code(status: std::process::ExitStatus) -> i64 {
    if let Some(code) = status.code() {
        return i64::from(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return 128 + i64::from(signal);
        }
    }
    0
}

/// `process::run_inherit(program, args) -> Result<i64, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_exec_run_inherit(prog: *const c_char, args: *mut GosVec) -> i128 {
    ffi_entry!(0i128, {
        if prog.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes(
                b"process::run_inherit: program is null",
            );
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `prog` is a live String argument from compiled code.
        let program = unsafe { crate::c_abi::gos_str_arg_string(prog) };
        // SAFETY: `args` is null or a live `Vec<String>` for the call.
        let argv = unsafe { argv_strings(args) };
        match run_inherit(&program, argv) {
            Ok(code) => gos_rt_result_new(0, code),
            Err(message) => {
                let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
        }
    })
}

#[cfg(unix)]
mod exit_wait {
    use std::time::Duration;

    /// The exit code `status` from `waitpid` reports.
    fn decode(status: libc::c_int) -> i64 {
        if libc::WIFEXITED(status) {
            i64::from(libc::WEXITSTATUS(status))
        } else if libc::WIFSIGNALED(status) {
            128 + i64::from(libc::WTERMSIG(status))
        } else {
            0
        }
    }

    /// Reaps `pid` when it has ended (`Some`), or answers `None` while it
    /// runs; `block` waits for it to end.
    fn reap(pid: libc::pid_t, block: bool) -> Result<Option<i64>, String> {
        let flags = if block { 0 } else { libc::WNOHANG };
        loop {
            let mut status: libc::c_int = 0;
            // SAFETY: `status` is a live local for the call.
            let rc = unsafe { libc::waitpid(pid, &raw mut status, flags) };
            if rc > 0 {
                return Ok(Some(decode(status)));
            }
            if rc == 0 {
                return Ok(None);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(format!("process::wait({pid}): {error}"));
            }
        }
    }

    pub(super) fn wait(pid: i64, timeout: Option<Duration>) -> Result<Option<i64>, String> {
        let pid = libc::pid_t::try_from(pid).map_err(|_| format!("process::wait: {pid}"))?;
        let Some(timeout) = timeout else {
            return reap(pid, true);
        };
        if let Some(code) = reap(pid, false)? {
            return Ok(Some(code));
        }
        if exited_within(pid, timeout)? {
            reap(pid, true)
        } else {
            Ok(None)
        }
    }

    /// Whether `pid` ends within `timeout`, watched through a process
    /// descriptor.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn exited_within(pid: libc::pid_t, timeout: Duration) -> Result<bool, String> {
        // SAFETY: `pidfd_open` takes a pid and flags and answers a new
        // descriptor or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(format!(
                "process::wait({pid}): {}",
                std::io::Error::last_os_error()
            ));
        }
        let fd = libc::c_int::try_from(fd).map_err(|_| format!("process::wait({pid})"))?;
        let deadline = std::time::Instant::now() + timeout;
        let result = loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let ms = libc::c_int::try_from(left.as_millis()).unwrap_or(libc::c_int::MAX);
            let mut entry = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid `pollfd` for the call.
            let n = unsafe { libc::poll(&raw mut entry, 1, ms) };
            if n >= 0 {
                break Ok(n > 0);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                break Err(format!("process::wait({pid}): {error}"));
            }
        };
        // SAFETY: `fd` is the descriptor opened above, closed once.
        unsafe { libc::close(fd) };
        result
    }

    /// Whether `pid` ends within `timeout`, watched through `kqueue`.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    fn exited_within(pid: libc::pid_t, timeout: Duration) -> Result<bool, String> {
        // SAFETY: `kqueue` takes no arguments and answers a descriptor or -1.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(format!(
                "process::wait({pid}): {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: an all-zero `kevent` is a valid value of the C struct.
        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = pid as _;
        change.filter = libc::EVFILT_PROC;
        change.flags = libc::EV_ADD | libc::EV_ONESHOT;
        change.fflags = libc::NOTE_EXIT;
        // SAFETY: as above.
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        let ts = libc::timespec {
            tv_sec: timeout.as_secs().try_into().unwrap_or(libc::time_t::MAX),
            tv_nsec: timeout.subsec_nanos().into(),
        };
        // SAFETY: one change and room for one event, both live locals.
        let n = unsafe { libc::kevent(kq, &raw const change, 1, &raw mut event, 1, &raw const ts) };
        let error = std::io::Error::last_os_error();
        // SAFETY: `kq` is the queue opened above, closed once.
        unsafe { libc::close(kq) };
        if n >= 0 {
            return Ok(n > 0);
        }
        // A child that ended before the registration cannot be watched; it
        // is waiting to be reaped.
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(true);
        }
        Err(format!("process::wait({pid}): {error}"))
    }
}

#[cfg(windows)]
mod exit_wait {
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    pub(super) fn wait(pid: i64, timeout: Option<Duration>) -> Result<Option<i64>, String> {
        let pid = u32::try_from(pid).map_err(|_| format!("process::wait: {pid}"))?;
        let ms = timeout.map_or(INFINITE, |t| {
            u32::try_from(t.as_millis()).unwrap_or(INFINITE - 1)
        });
        // SAFETY: OpenProcess takes plain values and answers a handle or null.
        let handle = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return Err(format!(
                "process::wait({pid}): {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: `handle` is the process handle opened above.
        let status = unsafe { WaitForSingleObject(handle, ms) };
        let result = match status {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: `handle` is open and `code` is a live local.
                if unsafe { GetExitCodeProcess(handle, &raw mut code) } == 0 {
                    Err(format!(
                        "process::wait({pid}): {}",
                        std::io::Error::last_os_error()
                    ))
                } else {
                    Ok(Some(i64::from(code)))
                }
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(format!(
                "process::wait({pid}): {}",
                std::io::Error::last_os_error()
            )),
        };
        // SAFETY: closed once.
        unsafe { CloseHandle(handle) };
        result
    }
}

#[cfg(not(any(unix, windows)))]
mod exit_wait {
    pub(super) fn wait(
        _pid: i64,
        _timeout: Option<std::time::Duration>,
    ) -> Result<Option<i64>, String> {
        Err("process::wait: processes are not available on this target".to_string())
    }
}

#[cfg(windows)]
fn terminate_pid(pid: u32) -> i64 {
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
            return 0;
        }
        let ok = TerminateProcess(handle, 1);
        let _ = CloseHandle(handle);
        i64::from(ok != 0)
    }
}

// ---------------------------------------------------------------
// Interactive piped children (`process::spawn_piped`).
//
// A spawned child's stdin/stdout are held in a process-global
// registry keyed by an opaque i64 handle; the `Child` methods take
// the handle. Reads go through a BufReader so `read_line` is
// incremental. The registry entry is removed at `wait`.
// ---------------------------------------------------------------

/// One live piped child: the process plus its retained pipe ends.
struct PipedChild {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<std::io::BufReader<std::process::ChildStdout>>,
}

static PIPED_CHILDREN: parking_lot::Mutex<Option<std::collections::HashMap<i64, PipedChild>>> =
    parking_lot::Mutex::new(None);
static NEXT_CHILD_HANDLE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

fn with_piped_child<R>(handle: i64, f: impl FnOnce(&mut PipedChild) -> R) -> Option<R> {
    let mut table = PIPED_CHILDREN.lock();
    table
        .get_or_insert_with(Default::default)
        .get_mut(&handle)
        .map(f)
}

fn take_piped_child(handle: i64) -> Option<PipedChild> {
    PIPED_CHILDREN
        .lock()
        .get_or_insert_with(Default::default)
        .remove(&handle)
}

fn restore_piped_child(handle: i64, child: PipedChild) {
    PIPED_CHILDREN
        .lock()
        .get_or_insert_with(Default::default)
        .insert(handle, child);
}

/// Spawns `prog` with piped stdin/stdout (stderr nulled) and returns
/// the opaque registry handle. Shared by the C-ABI shims and the
/// interpreter builtins so mixed VM/JIT execution sees one registry.
pub fn piped_child_spawn(prog: &str, args: &[String]) -> Result<i64, String> {
    let prog = prog.to_owned();
    let args = args.to_vec();
    match crate::sched_global::run_blocking("child-spawn", move || {
        let mut command = std::process::Command::new(prog);
        command.args(args);
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::null());
        command.spawn()
    }) {
        Ok(Ok(mut child)) => {
            let stdin = child.stdin.take();
            let stdout = child.stdout.take().map(std::io::BufReader::new);
            let handle = NEXT_CHILD_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            PIPED_CHILDREN
                .lock()
                .get_or_insert_with(Default::default)
                .insert(
                    handle,
                    PipedChild {
                        child,
                        stdin,
                        stdout,
                    },
                );
            Ok(handle)
        }
        Ok(Err(e)) => Err(format!("process::spawn_piped: {e}")),
        Err(e) => Err(e),
    }
}

/// Writes `bytes` to the child's stdin; false after `close_stdin`,
/// on a reaped handle, or when the pipe is broken.
pub fn piped_child_write_stdin(handle: i64, bytes: &[u8]) -> bool {
    use std::io::Write;
    let Some(mut child) = take_piped_child(handle) else {
        return false;
    };
    let bytes = bytes.to_vec();
    let wrote = crate::sched_global::run_blocking("child-stdin-write", move || {
        let wrote = child
            .stdin
            .as_mut()
            .is_some_and(|w| w.write_all(&bytes).and_then(|()| w.flush()).is_ok());
        (child, wrote)
    });
    match wrote {
        Ok((child, wrote)) => {
            restore_piped_child(handle, child);
            wrote
        }
        // A worker creation failure or panic leaves the child unavailable. This
        // is preferable to retaining the registry lock across an unbounded
        // pipe write, and matches the existing false-on-I/O-error contract.
        Err(_) => false,
    }
}

/// Drops the child's stdin write end so it sees EOF.
pub fn piped_child_close_stdin(handle: i64) {
    with_piped_child(handle, |pc| {
        pc.stdin = None;
    });
}

/// Next stdout line without its trailing newline; `None` at EOF or
/// on a reaped handle.
pub fn piped_child_read_line(handle: i64) -> Option<String> {
    use std::io::BufRead;
    let mut child = take_piped_child(handle)?;
    let result = crate::sched_global::run_blocking("child-stdout-read-line", move || {
        let line = child.stdout.as_mut().and_then(|reader| {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => None,
                Ok(_) => {
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    Some(line)
                }
            }
        });
        (child, line)
    });
    match result {
        Ok((child, line)) => {
            restore_piped_child(handle, child);
            line
        }
        Err(_) => None,
    }
}

/// Drains the child's stdout to EOF.
pub fn piped_child_read_stdout(handle: i64) -> Option<String> {
    use std::io::Read;
    let mut child = take_piped_child(handle)?;
    let result = crate::sched_global::run_blocking("child-stdout-read-all", move || {
        let text = child.stdout.as_mut().and_then(|reader| {
            let mut buf = String::new();
            reader.read_to_string(&mut buf).ok().map(|_| buf)
        });
        (child, text)
    });
    match result {
        Ok((child, text)) => {
            restore_piped_child(handle, child);
            text
        }
        Err(_) => None,
    }
}

/// Closes stdin, reaps the child, and removes the registry entry.
pub fn piped_child_wait(handle: i64) -> Result<i64, String> {
    let entry = take_piped_child(handle);
    let Some(mut pc) = entry else {
        return Err("process::Child::wait: unknown or reaped handle".to_string());
    };
    pc.stdin = None;
    match crate::sched_global::run_blocking("child-wait", move || pc.child.wait()) {
        Ok(Ok(status)) => Ok(i64::from(status.code().unwrap_or(-1))),
        Ok(Err(e)) => Err(format!("process::Child::wait: {e}")),
        Err(e) => Err(e),
    }
}

/// Best-effort terminate; the handle stays until `wait` reaps it.
pub fn piped_child_kill(handle: i64) -> bool {
    with_piped_child(handle, |pc| pc.child.kill().is_ok()).unwrap_or(false)
}

/// Reads the flat `Vec<String>` argv convention every child-process entry
/// point takes.
///
/// # Safety
///
/// `args` is null or a live `Vec` of strings.
pub(crate) unsafe fn argv_strings(args: *mut GosVec) -> Vec<String> {
    // SAFETY: this `unsafe fn`'s caller passes `args` null or a live `Vec<String>`.
    unsafe { crate::c_abi::vec::StrVecView::of(args) }
        .map_or_else(Vec::new, |argv| argv.texts().collect())
}

fn err_result(msg: String) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    gos_rt_result_new(1, err as i64)
}

/// `process::spawn_piped(prog, args) -> Result<Child, errors::Error>`.
/// The Ok payload is the opaque child handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_exec_spawn_piped(prog: *const c_char, args: *mut GosVec) -> i128 {
    ffi_entry!(0i128, {
        if prog.is_null() {
            return err_result("process::spawn_piped: program is null".to_string());
        }
        // SAFETY: `prog` is a String argument from compiled code, null or a live string body for the whole call.
        let prog_str = unsafe { crate::c_abi::gos_str_arg_string(prog) };
        // SAFETY: `args` is this shim's argument, as `argv_strings` requires (C-ABI contract).
        match piped_child_spawn(&prog_str, &unsafe { argv_strings(args) }) {
            Ok(handle) => gos_rt_result_new(0, handle),
            Err(msg) => err_result(msg),
        }
    })
}

/// `child.write_stdin(s) -> bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_child_write_stdin(handle: i64, s: *const c_char) -> i64 {
    ffi_entry!(0, {
        let bytes = if s.is_null() {
            Vec::new()
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_bytes(s) }.to_vec()
        };
        i64::from(piped_child_write_stdin(handle, &bytes))
    })
}

/// `child.close_stdin()`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_child_close_stdin(handle: i64) -> i64 {
    ffi_entry!(0, {
        piped_child_close_stdin(handle);
        0
    })
}

/// `child.read_line() -> Option<String>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_child_read_line(handle: i64) -> i128 {
    ffi_entry!(1i128, {
        match piped_child_read_line(handle) {
            Some(line) => {
                let ptr = alloc_cstring(line.as_bytes()) as i64;
                gos_rt_result_new(0, ptr)
            }
            None => 1i128,
        }
    })
}

/// `child.read_stdout() -> String`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_child_read_stdout(handle: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let text = piped_child_read_stdout(handle).unwrap_or_default();
        alloc_cstring(text.as_bytes())
    })
}

/// `child.wait() -> Result<i64, errors::Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_child_wait(handle: i64) -> i128 {
    ffi_entry!(0i128, {
        match piped_child_wait(handle) {
            Ok(code) => gos_rt_result_new(0, code),
            Err(msg) => err_result(msg),
        }
    })
}

/// `child.kill() -> bool`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_child_kill(handle: i64) -> i64 {
    ffi_entry!(0, { i64::from(piped_child_kill(handle)) })
}

#[cfg(test)]
mod tests {
    use super::tokenize_shell;

    #[test]
    fn tokenize_splits_on_whitespace() {
        assert_eq!(
            tokenize_shell("echo hello world"),
            vec!["echo", "hello", "world"]
        );
    }

    #[test]
    fn tokenize_honours_single_quotes() {
        assert_eq!(
            tokenize_shell("echo 'a b c' done"),
            vec!["echo", "a b c", "done"]
        );
    }

    #[test]
    fn tokenize_honours_double_quotes() {
        assert_eq!(
            tokenize_shell("tr \"a-z\" \"A-Z\""),
            vec!["tr", "a-z", "A-Z"]
        );
    }

    #[test]
    fn tokenize_empty_returns_empty() {
        assert!(tokenize_shell("   ").is_empty());
    }
}
