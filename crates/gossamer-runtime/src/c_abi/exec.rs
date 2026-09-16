#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(unused_unsafe)]

use std::os::raw::c_char;
#[cfg(unix)]
use std::time::Duration;

use super::string::alloc_cstring;
use super::vec::{GosVec, gos_rt_result_new};

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
        let stages = match unsafe { gather_command_lines(commands) } {
            Ok(s) => s,
            Err(msg) => {
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                return unsafe { gos_rt_result_new(1, err as i64) };
            }
        };
        if stages.is_empty() {
            let err =
                crate::c_abi::errors::error_new_from_bytes(b"exec::pipeline_run: empty pipeline");
            return unsafe { gos_rt_result_new(1, err as i64) };
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
                    return unsafe { gos_rt_result_new(1, err as i64) };
                }
                unsafe { gos_rt_result_new(0, blob as i64) }
            }
            Ok(Err(msg)) | Err(msg) => {
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
        }
    })
}

unsafe fn gather_command_lines(commands: *mut GosVec) -> Result<Vec<Vec<String>>, String> {
    if commands.is_null() {
        return Err("exec::pipeline_run: commands vec is null".into());
    }
    let v = unsafe { &*commands };
    let elem_bytes = v.elem_bytes as usize;
    if elem_bytes == 0 || v.ptr.is_null() {
        return Ok(Vec::new());
    }
    let mut stages = Vec::with_capacity(v.len as usize);
    for i in 0..v.len {
        let slot = unsafe { v.ptr.add((i as usize) * elem_bytes) };
        let cstr_ptr = unsafe {
            crate::c_abi::vec::slot_read_word(slot)
                .cast_const()
                .cast::<c_char>()
        };
        if cstr_ptr.is_null() {
            continue;
        }
        let line = unsafe { crate::c_abi::gos_str_arg_string(cstr_ptr) };
        let parts = tokenize_shell(&line);
        if !parts.is_empty() {
            stages.push(parts);
        }
    }
    Ok(stages)
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

/// `exec::wait_timeout(pid: i64, ms: i64) -> i64`. Polls the pid via
/// `waitpid(WNOHANG)` on Unix until it exits or `ms` elapses.
/// Returns the exit code on success, `-1` if the pid is still
/// running after the timeout, `-2` on any other error (unknown pid,
/// permission denied). Windows falls back to a best-effort
/// `WaitForSingleObject` with the supplied timeout.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_exec_wait_timeout(pid: i64, ms: i64) -> i64 {
    ffi_entry!(-2, {
        if pid <= 0 {
            return -2;
        }
        #[cfg(unix)]
        {
            let deadline =
                crate::platform::Instant::now() + Duration::from_millis(ms.max(0) as u64);
            loop {
                let mut status: libc::c_int = 0;
                // SAFETY: waitpid with WNOHANG returns 0 if still
                // running, the child pid on reap, -1 on error.
                let status_ptr: *mut libc::c_int = &raw mut status;
                let rc = unsafe { libc::waitpid(pid as libc::pid_t, status_ptr, libc::WNOHANG) };
                if rc > 0 {
                    if libc::WIFEXITED(status) {
                        return i64::from(libc::WEXITSTATUS(status));
                    }
                    if libc::WIFSIGNALED(status) {
                        return i64::from(128 + libc::WTERMSIG(status));
                    }
                    return 0;
                }
                if rc < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::ECHILD) {
                        return -2;
                    }
                    return -2;
                }
                if crate::platform::Instant::now() >= deadline {
                    return -1;
                }
                crate::platform::sleep(Duration::from_millis(25));
            }
        }
        #[cfg(windows)]
        {
            wait_timeout_windows(pid as u32, ms.max(0) as u32)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (pid, ms);
            -2
        }
    })
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

#[cfg(windows)]
fn wait_timeout_windows(pid: u32, ms: u32) -> i64 {
    // SAFETY: OpenProcess + WaitForSingleObject + GetExitCodeProcess
    // + CloseHandle. Each call returns a documented sentinel on
    // failure; we never deref invalid handles.
    unsafe {
        unsafe extern "system" {
            fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
            fn WaitForSingleObject(handle: isize, ms: u32) -> u32;
            fn GetExitCodeProcess(handle: isize, exit_code: *mut u32) -> i32;
            fn CloseHandle(object: isize) -> i32;
        }
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const WAIT_OBJECT_0: u32 = 0;
        const WAIT_TIMEOUT: u32 = 0x0000_0102;
        let handle = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return -2;
        }
        let r = WaitForSingleObject(handle, ms);
        if r == WAIT_TIMEOUT {
            let _ = CloseHandle(handle);
            return -1;
        }
        if r != WAIT_OBJECT_0 {
            let _ = CloseHandle(handle);
            return -2;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &raw mut code);
        let _ = CloseHandle(handle);
        if ok == 0 {
            return -2;
        }
        code as i64
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

/// Reads the flat `Vec<String>` argv convention shared with
/// `gos_rt_exec_spawn`.
fn argv_strings(args: *mut GosVec) -> Vec<String> {
    let mut out = Vec::new();
    if args.is_null() {
        return out;
    }
    let v = unsafe { &*args };
    let elem_bytes = v.elem_bytes as usize;
    if elem_bytes == 0 || v.ptr.is_null() {
        return out;
    }
    for i in 0..v.len {
        let slot = unsafe { v.ptr.add((i as usize) * elem_bytes) };
        let cstr_ptr = unsafe {
            crate::c_abi::vec::slot_read_word(slot)
                .cast_const()
                .cast::<c_char>()
        };
        if cstr_ptr.is_null() {
            out.push(String::new());
        } else {
            out.push(unsafe { crate::c_abi::gos_str_arg_string(cstr_ptr) });
        }
    }
    out
}

fn err_result(msg: String) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    unsafe { gos_rt_result_new(1, err as i64) }
}

/// `process::spawn_piped(prog, args) -> Result<Child, errors::Error>`.
/// The Ok payload is the opaque child handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_exec_spawn_piped(prog: *const c_char, args: *mut GosVec) -> i128 {
    ffi_entry!(0i128, {
        if prog.is_null() {
            return err_result("process::spawn_piped: program is null".to_string());
        }
        let prog_str = unsafe { crate::c_abi::gos_str_arg_string(prog) };
        match piped_child_spawn(&prog_str, &argv_strings(args)) {
            Ok(handle) => unsafe { gos_rt_result_new(0, handle) },
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
            unsafe { crate::c_abi::gos_str_arg_bytes(s) }.to_vec()
        };
        i64::from(piped_child_write_stdin(handle, &bytes))
    })
}

/// `child.close_stdin()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_child_close_stdin(handle: i64) -> i64 {
    ffi_entry!(0, {
        piped_child_close_stdin(handle);
        0
    })
}

/// `child.read_line() -> Option<String>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_child_read_line(handle: i64) -> i128 {
    ffi_entry!(1i128, {
        match piped_child_read_line(handle) {
            Some(line) => {
                let ptr = alloc_cstring(line.as_bytes()) as i64;
                unsafe { gos_rt_result_new(0, ptr) }
            }
            None => 1i128,
        }
    })
}

/// `child.read_stdout() -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_child_read_stdout(handle: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let text = piped_child_read_stdout(handle).unwrap_or_default();
        alloc_cstring(text.as_bytes())
    })
}

/// `child.wait() -> Result<i64, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_child_wait(handle: i64) -> i128 {
    ffi_entry!(0i128, {
        match piped_child_wait(handle) {
            Ok(code) => unsafe { gos_rt_result_new(0, code) },
            Err(msg) => err_result(msg),
        }
    })
}

/// `child.kill() -> bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_child_kill(handle: i64) -> i64 {
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
