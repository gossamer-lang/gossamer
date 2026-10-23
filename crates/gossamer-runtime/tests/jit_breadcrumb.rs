//! The fault handler reports on a hard fault (a non-stack-overflow
//! SIGSEGV / Windows access violation): it names the JIT-compiled body
//! that was running, or states the fault was outside any JIT body. Each
//! case runs a child that arms the guard and dereferences an unmapped
//! address; the parent asserts the expected line reached the child's
//! stderr. The "outside" case proves an empty stderr means the handler
//! never fired, not merely that no body was active.

use std::process::Command;

const MODE_ENV: &str = "GOS_BREADCRUMB_CRASH_MODE";
const BODY_NAME: &str = "deliberately_faulting_body";

#[test]
fn fault_handler_names_the_running_jit_body() {
    let stderr = run_crashing_child("in_body");
    let expected = format!("fault inside JIT-compiled body '{BODY_NAME}'");
    assert!(
        stderr.contains(&expected),
        "breadcrumb missing from child stderr.\nexpected substring: {expected}\nstderr was:\n{stderr}"
    );
}

#[test]
fn fault_handler_reports_fault_outside_any_jit_body() {
    let stderr = run_crashing_child("no_body");
    assert!(
        stderr.contains("outside any JIT-compiled body"),
        "outside-body note missing from child stderr.\nstderr was:\n{stderr}"
    );
}

/// Spawns this test binary as a child in the given crash `mode`, returns
/// its captured stderr, and asserts it crashed rather than exiting clean.
fn run_crashing_child(mode: &str) -> String {
    if let Ok(active) = std::env::var(MODE_ENV) {
        run_as_crashing_child(&active);
    }
    let exe = std::env::current_exe().expect("current_exe");
    let output = Command::new(exe)
        .args(["--exact", test_name_for(mode), "--nocapture"])
        .env(MODE_ENV, mode)
        .output()
        .expect("spawn crashing child");
    assert!(
        !output.status.success(),
        "child was expected to crash, but exited cleanly"
    );
    // The reported fault still ends the child by its own signal, without
    // the core dump the report makes redundant.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGSEGV),
            "{:?}",
            output.status
        );
        assert!(!output.status.core_dumped(), "the child dumped core");
    }
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn test_name_for(mode: &str) -> &'static str {
    match mode {
        "in_body" => "fault_handler_names_the_running_jit_body",
        "no_body" => "fault_handler_reports_fault_outside_any_jit_body",
        other => panic!("unknown crash mode {other}"),
    }
}

fn run_as_crashing_child(mode: &str) -> ! {
    gossamer_runtime::stack_guard::install_stack_guard();
    if mode == "in_body" {
        gossamer_runtime::stack_guard::set_jit_breadcrumb(BODY_NAME);
    }
    // An unmapped, non-null, non-stack address: faults as a plain access
    // violation (not stack overflow), the path that prints the note.
    let bad = 0xdead_0000_usize as *const u8;
    // SAFETY: intentional fault to exercise the handler; this read never
    // returns - the process is killed by the re-raised signal.
    let _ = unsafe { std::ptr::read_volatile(bad) };
    std::process::exit(0);
}
