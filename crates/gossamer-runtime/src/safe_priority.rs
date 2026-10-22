//! Safe-Rust wrapper that lowers the calling process's scheduling priority,
//! so crates with `#![forbid(unsafe_code)]` can run work in the background
//! without competing with the foreground. Processes it starts afterwards
//! inherit the lowered priority.

#![allow(unsafe_code)]

/// Lowers this process below normal scheduling priority: niceness 10 on
/// Unix, the below-normal priority class on Windows. Best effort; a system
/// that refuses leaves the priority as it was.
#[cfg(unix)]
pub fn lower_own_priority() {
    // SAFETY: `setpriority` only reads its integer arguments; `who == 0`
    // names the calling process.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS as _, 0, 10);
    }
}

/// Lowers this process below normal scheduling priority: niceness 10 on
/// Unix, the below-normal priority class on Windows. Best effort; a system
/// that refuses leaves the priority as it was.
#[cfg(windows)]
pub fn lower_own_priority() {
    use windows_sys::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
    };
    // SAFETY: `GetCurrentProcess` answers a pseudo-handle that needs no
    // closing, and `SetPriorityClass` only reads its arguments.
    unsafe {
        SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
}

/// Other targets have no priority to lower.
#[cfg(not(any(unix, windows)))]
pub fn lower_own_priority() {}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    #[cfg_attr(miri, ignore = "spawns a child process, which Miri does not support")]
    fn a_lowered_process_runs_at_a_higher_niceness() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("safe_priority::tests::probe")
            .env("GOS_PRIORITY_PROBE", "1")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
    }

    #[test]
    fn probe() {
        if std::env::var_os("GOS_PRIORITY_PROBE").is_none() {
            return;
        }
        super::lower_own_priority();
        // A process already nicer than 10 keeps its niceness, which an
        // unprivileged process cannot lower.
        // SAFETY: `getpriority` only reads its integer arguments.
        let after = unsafe { libc::getpriority(libc::PRIO_PROCESS as _, 0) };
        assert!(after >= 10, "niceness is {after}");
    }
}
