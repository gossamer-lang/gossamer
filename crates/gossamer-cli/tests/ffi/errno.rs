//! Error codes across a foreign call: each call starts with `errno` (and on
//! Windows the thread's last error) cleared, so a call that succeeds without
//! touching them reads back zero, and `last_errno` and `last_os_error` report
//! the C and operating-system codes a failing call set.

use crate::support::Project;

const DECLS: &str = r#"use std::ffi

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    fn errno_on_entry() -> ffi::c_int
    fn fail_with_errno(code: ffi::c_int) -> ffi::c_int
    fn succeed_quietly() -> ffi::c_int
    #[cfg(windows)]
    fn last_error_on_entry() -> ffi::c_int
    #[cfg(windows)]
    fn fail_with_last_error(code: ffi::c_int) -> ffi::c_int
}
"#;

#[test]
fn errno_is_cleared_before_every_foreign_call() {
    let project = Project::new(
        "errno-cleared",
        &format!(
            "{DECLS}{}",
            r#"
fn main() {
    unsafe { fail_with_errno(34) }
    println(f"failed: {ffi::last_errno()}")
    unsafe { succeed_quietly() }
    println(f"after a success: {ffi::last_errno()}")
    unsafe { fail_with_errno(22) }
    let seen = unsafe { errno_on_entry() }
    println(f"on entry: {seen} then {ffi::last_errno()}")
}
"#
        ),
    );
    project.expect_everywhere("failed: 34\nafter a success: 0\non entry: 0 then 0");
}

#[cfg(unix)]
#[test]
fn the_os_error_is_errno_on_unix() {
    let project = Project::new(
        "errno-os-unix",
        &format!(
            "{DECLS}{}",
            r#"
fn main() {
    unsafe { fail_with_errno(13) }
    println(f"{ffi::last_errno()} {ffi::last_os_error()}")
}
"#
        ),
    );
    project.expect_everywhere("13 13");
}

#[cfg(windows)]
#[test]
fn windows_keeps_the_crt_errno_and_the_last_error_apart() {
    let project = Project::new(
        "errno-os-windows",
        &format!(
            "{DECLS}{}",
            r#"
fn main() {
    unsafe { fail_with_errno(13) }
    println(f"crt: {ffi::last_errno()} {ffi::last_os_error()}")
    unsafe { fail_with_last_error(5) }
    println(f"win32: {ffi::last_errno()} {ffi::last_os_error()}")
    unsafe { fail_with_last_error(6) }
    let seen = unsafe { last_error_on_entry() }
    println(f"on entry: {seen}")
}
"#
        ),
    );
    project.expect_everywhere("crt: 13 0\nwin32: 0 5\non entry: 0");
}
