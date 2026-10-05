//! `process::Command`: the child processes the `std::process` wrapper source
//! starts, reached through foreign declarations of these entries.
//!
//! A child lives in a registry under an opaque handle. Its standard streams
//! are locked one by one, so a goroutine draining stdout and another
//! draining stderr never wait on each other; every blocking read, write,
//! and wait runs on the blocking pool while the calling goroutine parks.
//! Data of a length the caller cannot know in advance is produced into the
//! stream's pending buffer by one call and copied out by
//! [`gos_rt_command_take`].
//!
//! A child is reaped only under its own lock, by `try_wait`, so a signal
//! sent under that lock never reaches a process id the system has reused.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

/// The stream numbers the entries take.
const STDIN: i64 = 0;
const STDOUT: i64 = 1;
const STDERR: i64 = 2;

/// `stdio` modes, per stream.
const INHERIT: i64 = 0;
const NULL: i64 = 1;
const PIPED: i64 = 2;
const FD: i64 = 3;

/// `flags` bits, which only a platform with processes reads.
#[cfg(any(unix, windows))]
const NEW_PROCESS_GROUP: i64 = 1;
#[cfg(any(unix, windows))]
const NEW_SESSION: i64 = 2;

/// One readable stream: the pipe, and what a read produced for the caller
/// to take.
struct Reader {
    pipe: Option<Box<dyn BufRead + Send>>,
    pending: Vec<u8>,
}

impl Reader {
    fn new(pipe: Option<Box<dyn BufRead + Send>>) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            pipe,
            pending: Vec::new(),
        }))
    }
}

struct CommandChild {
    pid: i64,
    /// The process, reaped only through `try_wait` under this lock.
    process: Mutex<std::process::Child>,
    exit: Mutex<Option<i64>>,
    stdin: Arc<Mutex<Option<std::process::ChildStdin>>>,
    stdout: Arc<Mutex<Reader>>,
    stderr: Arc<Mutex<Reader>>,
}

static CHILDREN: Mutex<Option<HashMap<i64, Arc<CommandChild>>>> = Mutex::new(None);
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

/// Messages of failed calls, by the id the call answered.
static ERRORS: Mutex<Option<HashMap<i64, String>>> = Mutex::new(None);
static NEXT_ERROR: AtomicI64 = AtomicI64::new(2);

fn child(handle: i64) -> Option<Arc<CommandChild>> {
    CHILDREN
        .lock()
        .get_or_insert_with(HashMap::new)
        .get(&handle)
        .cloned()
}

/// Records `message` and answers the negative code a call returns for it.
fn fail(message: String) -> i64 {
    let id = NEXT_ERROR.fetch_add(1, Ordering::Relaxed);
    ERRORS
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(id, message);
    -id
}

fn unknown(handle: i64) -> i64 {
    fail(format!("process::Child: unknown handle {handle}"))
}

/// The program, arguments, environment, and directory a spec carries:
/// records of a tag byte, a four-byte little-endian length, and that many
/// bytes. `P` names the program, `A` an argument, `E` a `key=value` to set,
/// `R` a key to remove, `C` clears the environment, `D` the directory.
fn decode(spec: &[u8]) -> Result<std::process::Command, String> {
    let mut command: Option<std::process::Command> = None;
    let mut rest = spec;
    let text = |bytes: &[u8]| -> Result<String, String> {
        String::from_utf8(bytes.to_vec()).map_err(|e| format!("process::Command: {e}"))
    };
    while let [tag, a, b, c, d, tail @ ..] = rest {
        let len = u32::from_le_bytes([*a, *b, *c, *d]) as usize;
        if tail.len() < len {
            return Err("process::Command: truncated specification".to_string());
        }
        let (value, after) = tail.split_at(len);
        rest = after;
        let value = text(value)?;
        if *tag == b'P' {
            command = Some(std::process::Command::new(value));
            continue;
        }
        let Some(command) = command.as_mut() else {
            return Err("process::Command: no program".to_string());
        };
        match tag {
            b'A' => {
                command.arg(value);
            }
            b'E' => {
                let (key, val) = value.split_once('=').unwrap_or((value.as_str(), ""));
                command.env(key, val);
            }
            b'R' => {
                command.env_remove(value);
            }
            b'C' => {
                command.env_clear();
            }
            b'D' => {
                if !value.is_empty() {
                    command.current_dir(value);
                }
            }
            other => {
                return Err(format!("process::Command: unknown record {other}"));
            }
        }
    }
    command.ok_or_else(|| "process::Command: no program".to_string())
}

/// The `Stdio` for one stream's `mode`, duplicating `fd` for `FD` so the
/// caller's file stays its own.
fn stdio(mode: i64, fd: i64, stream: &str) -> Result<std::process::Stdio, String> {
    Ok(match mode {
        INHERIT => std::process::Stdio::inherit(),
        NULL => std::process::Stdio::null(),
        PIPED => std::process::Stdio::piped(),
        FD => duplicate(fd).map_err(|e| format!("process::Command: {stream}: {e}"))?,
        other => return Err(format!("process::Command: {stream}: unknown mode {other}")),
    })
}

#[cfg(unix)]
fn duplicate(fd: i64) -> std::io::Result<std::process::Stdio> {
    use std::os::fd::BorrowedFd;
    let fd = i32::try_from(fd).map_err(|_| std::io::Error::from_raw_os_error(libc::EBADF))?;
    // SAFETY: the descriptor belongs to a file the caller keeps open for the
    // spawn; it is only duplicated here.
    let owned = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
    Ok(std::process::Stdio::from(owned))
}

#[cfg(windows)]
fn duplicate(handle: i64) -> std::io::Result<std::process::Stdio> {
    use std::os::windows::io::BorrowedHandle;
    // SAFETY: the handle belongs to a file the caller keeps open for the
    // spawn; it is only duplicated here.
    let owned = unsafe { BorrowedHandle::borrow_raw(handle as isize as _) }.try_clone_to_owned()?;
    Ok(std::process::Stdio::from(owned))
}

#[cfg(not(any(unix, windows)))]
fn duplicate(_fd: i64) -> std::io::Result<std::process::Stdio> {
    Err(std::io::Error::other(
        "descriptors are not available on this target",
    ))
}

/// Applies the process-group, session, and controlling-terminal requests.
#[cfg(unix)]
fn place(command: &mut std::process::Command, flags: i64, ctty: i64) -> Result<(), String> {
    use std::os::unix::process::CommandExt as _;
    if ctty >= 0 && flags & NEW_SESSION == 0 {
        return Err(
            "process::Command: a controlling terminal belongs to a new session".to_string(),
        );
    }
    if flags & NEW_SESSION != 0 {
        let ctty = i32::try_from(ctty).unwrap_or(-1);
        // SAFETY: the closure runs in the child between fork and exec and
        // calls only `setsid` and `ioctl`, which are async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if ctty >= 0 && libc::ioctl(ctty, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    } else if flags & NEW_PROCESS_GROUP != 0 {
        command.process_group(0);
    }
    Ok(())
}

#[cfg(windows)]
fn place(command: &mut std::process::Command, flags: i64, ctty: i64) -> Result<(), String> {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    if flags & NEW_SESSION != 0 || ctty >= 0 {
        return Err(
            "process::Command: sessions and controlling terminals are POSIX facilities".to_string(),
        );
    }
    if flags & NEW_PROCESS_GROUP != 0 {
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn place(_command: &mut std::process::Command, _flags: i64, _ctty: i64) -> Result<(), String> {
    Err("process::Command: processes are not available on this target".to_string())
}

fn spawn(spec: &[u8], modes: [i64; 6], flags: i64, ctty: i64) -> Result<i64, String> {
    let mut command = decode(spec)?;
    command.stdin(stdio(modes[0], modes[3], "stdin")?);
    command.stdout(stdio(modes[1], modes[4], "stdout")?);
    command.stderr(stdio(modes[2], modes[5], "stderr")?);
    place(&mut command, flags, ctty)?;
    let program = command.get_program().to_string_lossy().into_owned();
    // A child writing to an inherited stream follows what this program
    // already printed there.
    if modes[1] == INHERIT || modes[2] == INHERIT {
        super::gos_rt_flush_stdout();
    }
    let mut process =
        crate::sched_global::run_blocking("process::Command::spawn", move || command.spawn())?
            .map_err(|e| format!("process::Command::spawn({program}): {e}"))?;
    let stdin = process.stdin.take();
    let stdout = process
        .stdout
        .take()
        .map(|pipe| Box::new(BufReader::new(pipe)) as Box<dyn BufRead + Send>);
    let stderr = process
        .stderr
        .take()
        .map(|pipe| Box::new(BufReader::new(pipe)) as Box<dyn BufRead + Send>);
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let entry = CommandChild {
        pid: i64::from(process.id()),
        process: Mutex::new(process),
        exit: Mutex::new(None),
        stdin: Arc::new(Mutex::new(stdin)),
        stdout: Reader::new(stdout),
        stderr: Reader::new(stderr),
    };
    CHILDREN
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(handle, Arc::new(entry));
    Ok(handle)
}

/// Starts the child a spec describes: `spec` holds `len` bytes of records
/// (see the module source), `stdio` six words - the stdin, stdout, and
/// stderr modes (inherit, null, piped, descriptor) then the descriptor each
/// `FD` mode duplicates - `flags` the process-group and session bits, and
/// `ctty` the terminal to make the new session's controlling terminal, or
/// -1. Answers the child's handle, or a negative error code.
///
/// # Safety
///
/// `spec` addresses `len` bytes and `stdio` six words.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_command_spawn(
    spec: *const u8,
    len: u64,
    stdio: *const i64,
    flags: i64,
    ctty: i64,
) -> i64 {
    let len = usize::try_from(len).unwrap_or(0);
    let spec = if len == 0 {
        &[][..]
    } else {
        // SAFETY: `spec` addresses `len` bytes (contract).
        unsafe { std::slice::from_raw_parts(spec, len) }
    };
    let mut modes = [0i64; 6];
    // SAFETY: `stdio` addresses six words (contract).
    unsafe { std::ptr::copy_nonoverlapping(stdio, modes.as_mut_ptr(), 6) };
    spawn(spec, modes, flags, ctty).unwrap_or_else(fail)
}

/// The message of the error code `code` answered, copied to `dst` when
/// `cap` holds all of it, which forgets it. Answers the message's length.
///
/// # Safety
///
/// `dst` addresses `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_command_error(code: i64, dst: *mut u8, cap: u64) -> u64 {
    let id = code.saturating_neg();
    let mut errors = ERRORS.lock();
    let table = errors.get_or_insert_with(HashMap::new);
    let Some(message) = table.get(&id) else {
        return 0;
    };
    let len = message.len();
    if usize::try_from(cap).is_ok_and(|cap| cap >= len) {
        // SAFETY: `dst` holds at least `len` bytes (checked above).
        unsafe { std::ptr::copy_nonoverlapping(message.as_ptr(), dst, len) };
        table.remove(&id);
    }
    len as u64
}

/// The child's process id, or a negative error code.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_command_pid(handle: i64) -> i64 {
    child(handle).map_or_else(|| unknown(handle), |entry| entry.pid)
}

/// Writes `len` bytes from `src` to the child's stdin: 0, or a negative
/// error code.
///
/// # Safety
///
/// `src` addresses `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_command_write(handle: i64, src: *const u8, len: u64) -> i64 {
    let Some(entry) = child(handle) else {
        return unknown(handle);
    };
    let len = usize::try_from(len).unwrap_or(0);
    let bytes = if len == 0 {
        Vec::new()
    } else {
        // SAFETY: `src` addresses `len` bytes (contract).
        unsafe { std::slice::from_raw_parts(src, len) }.to_vec()
    };
    let stdin = Arc::clone(&entry.stdin);
    let written = crate::sched_global::run_blocking("process::Child::write_stdin", move || {
        let mut stdin = stdin.lock();
        match stdin.as_mut() {
            Some(pipe) => pipe
                .write_all(&bytes)
                .and_then(|()| pipe.flush())
                .map_err(|e| e.to_string()),
            None => Err("stdin is closed or not piped".to_string()),
        }
    });
    match written {
        Ok(Ok(())) => 0,
        Ok(Err(message)) | Err(message) => fail(format!("process::Child::write_stdin: {message}")),
    }
}

/// Closes the child's end of `stream`: stdin's close is the end of input
/// the child reads; a closed stdout or stderr reads as ended.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_command_close(handle: i64, stream: i64) {
    let Some(entry) = child(handle) else {
        return;
    };
    match stream {
        STDIN => *entry.stdin.lock() = None,
        STDOUT => entry.stdout.lock().pipe = None,
        STDERR => entry.stderr.lock().pipe = None,
        _ => {}
    }
    forget_if_done(handle, &entry);
}

/// Reads from the child's `stream` (1 stdout, 2 stderr) into its pending
/// buffer: up to `max` bytes for `mode` 0, a line without its ending for
/// `mode` 1, everything to the end for `mode` 2. Answers the length read,
/// -1 at the end of the stream, or a negative error code below -1.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_command_read(handle: i64, stream: i64, mode: i64, max: i64) -> i64 {
    let Some(entry) = child(handle) else {
        return unknown(handle);
    };
    let reader = match stream {
        STDOUT => Arc::clone(&entry.stdout),
        STDERR => Arc::clone(&entry.stderr),
        other => return fail(format!("process::Child: stream {other} is not readable")),
    };
    let max = usize::try_from(max).unwrap_or(0).max(1);
    let read = crate::sched_global::run_blocking(
        "process::Child::read",
        move || -> std::io::Result<Option<usize>> {
            let mut reader = reader.lock();
            let n = {
                let Reader { pipe, pending } = &mut *reader;
                pending.clear();
                let Some(pipe) = pipe.as_mut() else {
                    return Ok(None);
                };
                match mode {
                    0 => {
                        pending.resize(max, 0);
                        let n = pipe.read(pending)?;
                        pending.truncate(n);
                        n
                    }
                    1 => {
                        let n = pipe.read_until(b'\n', pending)?;
                        if pending.last() == Some(&b'\n') {
                            pending.pop();
                            if pending.last() == Some(&b'\r') {
                                pending.pop();
                            }
                        }
                        n
                    }
                    _ => pipe.read_to_end(pending)?,
                }
            };
            if n == 0 {
                reader.pipe = None;
                return Ok(None);
            }
            Ok(Some(reader.pending.len()))
        },
    );
    let answer = match read {
        Ok(Ok(Some(len))) => i64::try_from(len).unwrap_or(i64::MAX),
        Ok(Ok(None)) => -1,
        Ok(Err(error)) => fail(format!("process::Child::read: {error}")),
        Err(message) => fail(format!("process::Child::read: {message}")),
    };
    if answer == -1 {
        forget_if_done(handle, &entry);
    }
    answer
}

/// How many bytes are pending on `stream` (1 stdout, 2 stderr).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_command_pending(handle: i64, stream: i64) -> u64 {
    let Some(entry) = child(handle) else {
        return 0;
    };
    let len = match stream {
        STDOUT => entry.stdout.lock().pending.len(),
        STDERR => entry.stderr.lock().pending.len(),
        _ => 0,
    };
    len as u64
}

/// Copies the pending bytes of `stream` to `dst`, which holds `cap` bytes,
/// and clears them. Answers how many it copied.
///
/// # Safety
///
/// `dst` addresses `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_command_take(
    handle: i64,
    stream: i64,
    dst: *mut u8,
    cap: u64,
) -> u64 {
    let Some(entry) = child(handle) else {
        return 0;
    };
    let reader = match stream {
        STDOUT => &entry.stdout,
        STDERR => &entry.stderr,
        _ => return 0,
    };
    let len = {
        let mut reader = reader.lock();
        let len = reader.pending.len().min(usize::try_from(cap).unwrap_or(0));
        // SAFETY: `dst` holds `cap >= len` bytes (contract).
        unsafe { std::ptr::copy_nonoverlapping(reader.pending.as_ptr(), dst, len) };
        reader.pending.clear();
        len
    };
    forget_if_done(handle, &entry);
    len as u64
}

/// Waits for the child to end, for at most `timeout_ms` milliseconds (with
/// no limit when negative). Answers 1 with the exit code - 128 plus the
/// signal number for one a signal ended - stored to `code`, 0 at the
/// timeout, or a negative error code.
///
/// # Safety
///
/// `code` addresses a word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_command_wait(handle: i64, timeout_ms: i64, code: *mut i64) -> i64 {
    let Some(entry) = child(handle) else {
        return unknown(handle);
    };
    if let Some(exit) = *entry.exit.lock() {
        // SAFETY: `code` addresses a word (contract).
        unsafe { code.write(exit) };
        return 1;
    }
    let timeout = u64::try_from(timeout_ms).ok().map(Duration::from_millis);
    let waiting = Arc::clone(&entry);
    let waited = crate::sched_global::run_blocking("process::Child::wait", move || {
        if !super::exec::wait_until_exited(waiting.pid, timeout)? {
            return Ok(None);
        }
        let mut process = waiting.process.lock();
        match process.try_wait() {
            Ok(Some(status)) => Ok(Some(super::exec::status_code(status))),
            Ok(None) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    });
    match waited {
        Ok(Ok(Some(exit))) => {
            *entry.exit.lock() = Some(exit);
            // SAFETY: `code` addresses a word (contract).
            unsafe { code.write(exit) };
            forget_if_done(handle, &entry);
            1
        }
        Ok(Ok(None)) => 0,
        Ok(Err(message)) | Err(message) => fail(format!("process::Child::wait: {message}")),
    }
}

/// Sends `signal` to the child, or to its process group when `group` is
/// nonzero; `signal` 9 on a single child is the platform's forced end
/// (`TerminateProcess` on Windows). Answers 0, or a negative error code.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_command_kill(handle: i64, signal: i64, group: i64) -> i64 {
    let Some(entry) = child(handle) else {
        return unknown(handle);
    };
    let mut process = entry.process.lock();
    // A reaped child's id may name another process now.
    match process.try_wait() {
        Ok(Some(status)) => {
            *entry.exit.lock() = Some(super::exec::status_code(status));
            return 0;
        }
        Ok(None) => {}
        Err(error) => return fail(format!("process::Child::kill: {error}")),
    }
    match send(&mut process, entry.pid, signal, group != 0) {
        Ok(()) => 0,
        Err(message) => fail(format!("process::Child::kill: {message}")),
    }
}

#[cfg(unix)]
fn send(
    _process: &mut std::process::Child,
    pid: i64,
    signal: i64,
    group: bool,
) -> Result<(), String> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| format!("pid {pid}"))?;
    let target = if group { -pid } else { pid };
    let signal = libc::c_int::try_from(signal).map_err(|_| format!("signal {signal}"))?;
    // SAFETY: `kill` takes plain values; the child is unreaped, so `pid`
    // still names it.
    if unsafe { libc::kill(target, signal) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(not(unix))]
fn send(
    process: &mut std::process::Child,
    _pid: i64,
    signal: i64,
    group: bool,
) -> Result<(), String> {
    if group || signal != 9 {
        return Err("signals and process groups are POSIX facilities; use kill()".to_string());
    }
    process.kill().map_err(|e| e.to_string())
}

/// Drops the registry entry once the child has ended and nothing is left
/// to read, take, or write.
fn forget_if_done(handle: i64, entry: &CommandChild) {
    let drained = |reader: &Mutex<Reader>| {
        let reader = reader.lock();
        reader.pipe.is_none() && reader.pending.is_empty()
    };
    let done = entry.exit.lock().is_some()
        && entry.stdin.lock().is_none()
        && drained(&entry.stdout)
        && drained(&entry.stderr);
    if done {
        CHILDREN
            .lock()
            .get_or_insert_with(HashMap::new)
            .remove(&handle);
    }
}

/// Reads stdout and stderr to their ends at once - each on its own thread,
/// so a child that fills one pipe while this side reads the other cannot
/// stall - and waits for the child. The output is left pending on each
/// stream; the exit code is stored to `code`. Answers 0, or a negative error
/// code.
///
/// # Safety
///
/// `code` addresses a word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_command_collect(handle: i64, code: *mut i64) -> i64 {
    let Some(entry) = child(handle) else {
        return unknown(handle);
    };
    *entry.stdin.lock() = None;
    let stdout = Arc::clone(&entry.stdout);
    let stderr = Arc::clone(&entry.stderr);
    let drained = crate::sched_global::run_blocking("process::Command::output", move || {
        let drain = |reader: &Mutex<Reader>| -> std::io::Result<()> {
            let mut reader = reader.lock();
            let Reader { pipe, pending } = &mut *reader;
            pending.clear();
            if let Some(mut taken) = pipe.take() {
                taken.read_to_end(pending)?;
            }
            Ok(())
        };
        std::thread::scope(|scope| {
            let err = scope.spawn(|| drain(&stderr));
            let out = drain(&stdout);
            let err = err
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("reader panicked")));
            out.and(err)
        })
    });
    match drained {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return fail(format!("process::Command::output: {error}")),
        Err(message) => return fail(format!("process::Command::output: {message}")),
    }
    // SAFETY: forwarded under the same contract.
    let waited = unsafe { gos_rt_command_wait(handle, -1, code) };
    if waited == 1 { 0 } else { waited }
}
