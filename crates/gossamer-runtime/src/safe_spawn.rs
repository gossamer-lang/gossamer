//! Safe-Rust wrapper that starts a process holding only the handles its
//! `Command` gives it, so crates with `#![forbid(unsafe_code)]` can start a
//! process that outlives them without keeping their output open.

#![allow(unsafe_code)]

use std::io;
use std::process::{Child, Command};

/// Starts `command` holding none of this process's standard handles beyond
/// the ones `command` sets. Unix replaces descriptors 0-2 in the child and
/// closes every other one Rust opened, so `spawn` already does this.
#[cfg(not(windows))]
pub fn spawn_apart(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

/// Starts `command` holding none of this process's standard handles beyond
/// the ones `command` sets. A Windows child inherits every inheritable handle
/// of its parent, whatever its own standard handles are, so the standard
/// handles this process inherited stay out of the child for the spawn: a
/// pipe on this process's output then closes when this process ends, not
/// when the child does. A `Stdio::inherit` child is unaffected, as it is
/// given its own inheritable duplicate.
#[cfg(windows)]
pub fn spawn_apart(command: &mut Command) -> io::Result<Child> {
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    let mut cleared: Vec<HANDLE> = Vec::new();
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` only reads the process's standard handle
        // table and answers a handle this process does not own a closing of.
        let handle = unsafe { GetStdHandle(id) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE || cleared.contains(&handle) {
            continue;
        }
        let mut flags = 0u32;
        // SAFETY: `handle` is a live standard handle of this process and
        // `flags` outlives the call that writes it.
        let inheritable = unsafe { GetHandleInformation(handle, &raw mut flags) } != 0
            && flags & HANDLE_FLAG_INHERIT != 0;
        // SAFETY: as above; only the inherit flag of `handle` changes.
        if inheritable && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0 {
            cleared.push(handle);
        }
    }
    let child = command.spawn();
    for handle in cleared {
        // SAFETY: `handle` is the standard handle cleared above, still open,
        // and gets back the inherit flag it had.
        unsafe {
            SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
        }
    }
    child
}

#[cfg(all(test, windows))]
mod tests {
    use std::io::Read as _;
    use std::os::windows::io::AsRawHandle as _;
    use std::process::{Command, Stdio};

    const ROLE: &str = "GOS_SPAWN_APART_ROLE";
    const LOCK: &str = "GOS_SPAWN_APART_LOCK";

    /// Whether some process still holds a writer of the pipe `out` reads,
    /// once everything already written is read.
    fn writer_still_open(out: &mut std::process::ChildStdout) -> bool {
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;
        loop {
            let mut available = 0u32;
            // SAFETY: `out` is a live pipe handle and `available` outlives
            // the call that writes it; no buffer is passed.
            let ok = unsafe {
                PeekNamedPipe(
                    out.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &raw mut available,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return false;
            }
            if available == 0 {
                return true;
            }
            let mut chunk = vec![0u8; available as usize];
            out.read_exact(&mut chunk).unwrap();
        }
    }

    /// Runs the `middle` probe with its stdout on a pipe; the probe starts a
    /// grandchild that lives until this test lets go of the lock.
    fn pipe_outlives_the_middle_process(mode: &str) -> bool {
        let dir =
            std::env::temp_dir().join(format!("gos-spawn-apart-{mode}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("lock");
        let lock = std::fs::File::create(&lock_path).unwrap();
        lock.lock().unwrap();
        let mut middle = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "safe_spawn::tests::middle", "--nocapture"])
            .env(ROLE, mode)
            .env(LOCK, &lock_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut out = middle.stdout.take().unwrap();
        assert!(middle.wait().unwrap().success());
        let held = writer_still_open(&mut out);
        drop(lock);
        held
    }

    #[test]
    fn a_process_started_apart_leaves_its_parents_output_to_close() {
        assert!(
            pipe_outlives_the_middle_process("plain"),
            "a plain spawn holds the parent's output open"
        );
        assert!(
            !pipe_outlives_the_middle_process("apart"),
            "a process started apart holds the parent's output open"
        );
    }

    #[test]
    fn middle() {
        let Ok(mode) = std::env::var(ROLE) else {
            return;
        };
        let mut grandchild = Command::new(std::env::current_exe().unwrap());
        grandchild
            .args(["--exact", "safe_spawn::tests::grandchild"])
            .env(ROLE, "grandchild")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let started = if mode == "apart" {
            super::spawn_apart(&mut grandchild)
        } else {
            grandchild.spawn()
        };
        // The grandchild outlives this probe on purpose: the test asks
        // whether it holds the probe's output once the probe has ended.
        assert!(started.is_ok());
    }

    #[test]
    fn grandchild() {
        if std::env::var(ROLE).as_deref() != Ok("grandchild") {
            return;
        }
        let lock = std::fs::File::open(std::env::var_os(LOCK).unwrap()).unwrap();
        lock.lock().unwrap();
    }
}
