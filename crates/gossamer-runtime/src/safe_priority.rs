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

/// Moves this process to background priority for housekeeping no one waits
/// on: the lowest CPU priority, and disk access that yields to every other
/// process. Best effort; a system that refuses leaves the priority as it was.
#[cfg(unix)]
pub fn enter_background_priority() {
    // SAFETY: `setpriority` only reads its integer arguments; `who == 0`
    // names the calling process.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS as _, 0, 19);
    }
    background_disk_priority();
}

// Linux schedules disk requests by I/O class; the idle class is served only
// when no other process waits on the disk.
#[cfg(target_os = "linux")]
fn background_disk_priority() {
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    const IOPRIO_CLASS_IDLE: libc::c_long = 3;
    const IOPRIO_CLASS_SHIFT: libc::c_long = 13;
    // SAFETY: `ioprio_set` only reads its integer arguments; `who == 0`
    // names the calling process.
    unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0 as libc::c_long,
            IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT,
        );
    }
}

// macOS throttles the disk access of a process whose I/O policy says so,
// whenever another process needs the disk.
#[cfg(target_vendor = "apple")]
fn background_disk_priority() {
    const IOPOL_TYPE_DISK: libc::c_int = 0;
    const IOPOL_SCOPE_PROCESS: libc::c_int = 0;
    const IOPOL_THROTTLE: libc::c_int = 3;
    unsafe extern "C" {
        fn setiopolicy_np(
            iotype: libc::c_int,
            scope: libc::c_int,
            policy: libc::c_int,
        ) -> libc::c_int;
    }
    // SAFETY: `setiopolicy_np` only reads its integer arguments.
    unsafe {
        setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_PROCESS, IOPOL_THROTTLE);
    }
}

// Other Unix systems schedule disk access by CPU priority alone.
#[cfg(all(unix, not(target_os = "linux"), not(target_vendor = "apple")))]
fn background_disk_priority() {}

/// Moves this process to background priority for housekeeping no one waits
/// on: Windows' background mode lowers its CPU, disk, and memory priority
/// together. Best effort; a system that refuses leaves the priority as it
/// was.
#[cfg(windows)]
pub fn enter_background_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_MODE_BACKGROUND_BEGIN, SetPriorityClass,
    };
    // SAFETY: `GetCurrentProcess` answers a pseudo-handle that needs no
    // closing, and `SetPriorityClass` only reads its arguments.
    unsafe {
        SetPriorityClass(GetCurrentProcess(), PROCESS_MODE_BACKGROUND_BEGIN);
    }
}

/// Other targets have no priority to lower.
#[cfg(not(any(unix, windows)))]
pub fn enter_background_priority() {}

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
    #[cfg_attr(miri, ignore = "spawns a child process, which Miri does not support")]
    fn a_background_process_runs_at_the_lowest_cpu_and_disk_priority() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("safe_priority::tests::background_probe")
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
    fn background_probe() {
        if std::env::var_os("GOS_PRIORITY_PROBE").is_none() {
            return;
        }
        super::enter_background_priority();
        // SAFETY: `getpriority` only reads its integer arguments.
        let after = unsafe { libc::getpriority(libc::PRIO_PROCESS as _, 0) };
        assert_eq!(after, 19, "niceness is {after}");
        #[cfg(target_os = "linux")]
        {
            // SAFETY: `ioprio_get` only reads its integer arguments.
            let ioprio = unsafe {
                libc::syscall(libc::SYS_ioprio_get, 1 as libc::c_long, 0 as libc::c_long)
            };
            assert_eq!(ioprio >> 13, 3, "I/O priority is {ioprio:#x}");
        }
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
