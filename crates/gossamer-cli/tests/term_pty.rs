//! `std::term` against a real pseudo-terminal on every tier: the size the
//! terminal reports, raw mode (an interrupt byte arrives as input rather
//! than as a signal), input bytes, and the terminal's original modes
//! restored when the program ends, whether by returning or by panicking.

#![cfg(unix)]
#![allow(missing_docs, unsafe_code)]

use std::io::Write as _;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROGRAM: &str = r#"
use std::env
use std::term

fn main() {
    println(f"tty {term::is_terminal(term::STDIN)}")
    let cols, rows = term::size().unwrap()
    println(f"size {cols}x{rows}")
    let raw = term::enter_raw().unwrap()
    print("ready\r\n")
    let bytes = term::read_input(-1).unwrap()
    print(f"got {bytes}\r\n")
    print(f"idle {term::read_input(20).unwrap().len()}\r\n")
    if !env::args().is_empty() {
        let xs = #[1]
        println(xs[3])
    }
    raw.restore()
    println("done")
}
"#;

fn gos() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

/// A pseudo-terminal pair sized 101 columns by 33 rows.
fn open_pty() -> (OwnedFd, OwnedFd) {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: 33,
        ws_col: 101,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // macOS declares the termios and size arguments `*mut`, Linux `*const`;
    // a `*mut` argument fits both.
    // SAFETY: both out-pointers are valid; the name and termios are optional.
    let rc = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: `openpty` answered two open descriptors this test now owns.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

fn local_modes(fd: &OwnedFd) -> (libc::tcflag_t, libc::tcflag_t) {
    // SAFETY: `termios` is plain data that `tcgetattr` fills in.
    let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is an open terminal and `attrs` is writable.
    assert_eq!(
        unsafe { libc::tcgetattr(fd.as_raw_fd(), &raw mut attrs) },
        0
    );
    (attrs.c_lflag, attrs.c_iflag)
}

/// Reads whatever the master has buffered without blocking.
fn drain(master: &OwnedFd, out: &mut Vec<u8>) {
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: `buf` is writable for its length.
        let n = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        let Ok(n) = usize::try_from(n) else {
            return;
        };
        if n == 0 {
            return;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// Runs `command` on a fresh terminal, types `ab` and an interrupt byte once
/// it prints `ready`, and answers its output, exit code, and whether the
/// terminal's modes afterwards are the ones it started with.
fn drive(mut command: Command) -> (String, i32, bool) {
    let (master, slave) = open_pty();
    // SAFETY: the master is open; non-blocking reads let the loop below
    // wait with `poll` and drain without stalling.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let before = local_modes(&slave);
    let stdio = |fd: &OwnedFd| Stdio::from(fd.try_clone().expect("dup the terminal"));
    let mut child = command
        .stdin(stdio(&slave))
        .stdout(stdio(&slave))
        .stderr(stdio(&slave))
        .spawn()
        .expect("spawn the program");
    let mut out = Vec::new();
    let mut typed = false;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            break status;
        }
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd`; the wait is bounded so the child's exit
        // is noticed between reads.
        unsafe { libc::poll(&raw mut poll, 1, 100) };
        drain(&master, &mut out);
        if !typed && out.windows(5).any(|w| w == b"ready") {
            let mut writer = std::fs::File::from(master.try_clone().expect("dup the master"));
            writer.write_all(b"ab\x03").expect("type into the terminal");
            typed = true;
        }
    };
    drain(&master, &mut out);
    let after = local_modes(&slave);
    let text = String::from_utf8_lossy(&out).replace('\r', "");
    (text, status.code().unwrap_or(-1), before == after)
}

fn source(dir: &Path) -> PathBuf {
    let path = dir.join("term_pty.gos");
    std::fs::write(&path, PROGRAM).expect("write the program");
    path
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-term-pty-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn assert_session(tier: &str, (text, code, restored): (String, i32, bool), panics: bool) {
    assert!(text.contains("tty true"), "{tier}: {text}");
    assert!(text.contains("size 101x33"), "{tier}: {text}");
    assert!(text.contains("got #[97, 98, 3]"), "{tier}: {text}");
    assert!(text.contains("idle 0"), "{tier}: {text}");
    assert!(
        restored,
        "{tier}: the terminal modes were not restored\n{text}"
    );
    if panics {
        assert_eq!(code, 101, "{tier}: {text}");
        assert!(text.contains("index out of bounds"), "{tier}: {text}");
    } else {
        assert_eq!(code, 0, "{tier}: {text}");
        assert!(text.contains("done"), "{tier}: {text}");
    }
}

#[test]
fn std_term_drives_a_pseudo_terminal_on_the_vm() {
    let dir = scratch("vm");
    let src = source(&dir);
    for panics in [false, true] {
        let mut command = Command::new(gos());
        command.arg("run").arg(&src).env("GOS_JIT", "0");
        if panics {
            command.arg("panic");
        }
        assert_session("vm", drive(command), panics);
    }
}

#[test]
fn std_term_drives_a_pseudo_terminal_on_the_jit() {
    let dir = scratch("jit");
    let src = source(&dir);
    for panics in [false, true] {
        let mut command = Command::new(gos());
        command
            .arg("run")
            .arg(&src)
            .env("GOSSAMER_JIT_THRESHOLD", "1");
        if panics {
            command.arg("panic");
        }
        assert_session("jit", drive(command), panics);
    }
}

#[test]
fn std_term_drives_a_pseudo_terminal_as_a_native_binary() {
    let dir = scratch("native");
    let src = source(&dir);
    for release in [false, true] {
        let out = dir.join(if release { "release" } else { "debug" });
        let mut build = Command::new(gos());
        build.arg("build").arg("--out-dir").arg(&out).arg(&src);
        if release {
            build.arg("--release");
        }
        let outcome = build.output().expect("run gos build");
        assert!(
            outcome.status.success(),
            "{}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        let binary = out.join("term_pty");
        for panics in [false, true] {
            let mut command = Command::new(&binary);
            if panics {
                command.arg("panic");
            }
            assert_session("native", drive(command), panics);
        }
    }
}

/// A blocking terminal read and a signal wait, each inside a cohort whose
/// deadline passes while nothing arrives: both waits end, so the cohorts
/// join and the program finishes.
const CANCEL_PROGRAM: &str = r#"
use std::errors
use std::os::signal
use std::term

fn read_until_cancelled() -> Result<(), errors::Error> {
    cohort(timeout: 100) {
        spawn(|| println(f"input {term::read_input(-1).unwrap().len()}"))
    }
}

fn wait_until_cancelled() -> Result<(), errors::Error> {
    cohort(timeout: 100) {
        spawn(|| {
            let n = signal::on(signal::SIGWINCH)
            println(f"fired {n.wait()}")
        })
    }
}

fn main() {
    println(f"read {read_until_cancelled()}")
    println(f"signal {wait_until_cancelled()}")
}
"#;

fn assert_cancelled_session(tier: &str, (text, code, _restored): (String, i32, bool)) {
    assert_eq!(code, 0, "{tier}: {text}");
    for line in [
        "input 0",
        "read Err(cohort timed out)",
        "fired false",
        "signal Err(cohort timed out)",
    ] {
        assert!(text.contains(line), "{tier}: missing `{line}`\n{text}");
    }
}

fn cancel_source(dir: &Path) -> PathBuf {
    let path = dir.join("term_cancel.gos");
    std::fs::write(&path, CANCEL_PROGRAM).expect("write the program");
    path
}

#[test]
fn cohort_cancellation_ends_terminal_and_signal_waits_on_the_vm() {
    let src = cancel_source(&scratch("cancel-vm"));
    let mut command = Command::new(gos());
    command.arg("run").arg(&src).env("GOS_JIT", "0");
    assert_cancelled_session("vm", drive(command));
}

#[test]
fn cohort_cancellation_ends_terminal_and_signal_waits_on_the_jit() {
    let src = cancel_source(&scratch("cancel-jit"));
    let mut command = Command::new(gos());
    command
        .arg("run")
        .arg(&src)
        .env("GOSSAMER_JIT_THRESHOLD", "1");
    assert_cancelled_session("jit", drive(command));
}

#[test]
fn cohort_cancellation_ends_terminal_and_signal_waits_as_a_native_binary() {
    let dir = scratch("cancel-native");
    let src = cancel_source(&dir);
    for release in [false, true] {
        let out = dir.join(if release { "release" } else { "debug" });
        let mut build = Command::new(gos());
        build.arg("build").arg("--out-dir").arg(&out).arg(&src);
        if release {
            build.arg("--release");
        }
        let outcome = build.output().expect("run gos build");
        assert!(
            outcome.status.success(),
            "{}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        assert_cancelled_session("native", drive(Command::new(out.join("term_cancel"))));
    }
}

/// The terminal reached through a descriptor the program opened, with its
/// standard input redirected away from it: the way a terminal program
/// started as `producer | program` reads keys from `/dev/tty`.
const TTY_PROGRAM: &str = r#"
use std::env
use std::fs
use std::term

fn main() {
    let path = env::args()[0]
    let opts = fs::OpenOptions::new()
    let opts = opts.read(true)
    let opts = opts.write(true)
    let tty = opts.open(path).unwrap()
    let fd = tty.fd().unwrap()
    println(f"stdin tty {term::is_terminal(term::STDIN)}")
    println(f"opened tty {term::is_terminal(fd)}")
    let cols, rows = term::size(fd).unwrap()
    println(f"size {cols}x{rows}")
    let raw = term::enter_raw(fd).unwrap()
    print("ready\r\n")
    let bytes = term::read_input(-1, fd).unwrap()
    print(f"got {bytes}\r\n")
    raw.restore()
    println("done")
}
"#;

/// The path of the terminal device `fd` refers to.
///
/// `ttyname_r` writes into a buffer of the caller's own: the tests run on
/// parallel threads, and `ttyname` answers one buffer the whole process
/// shares, so one test could read another's terminal.
fn tty_path(fd: &OwnedFd) -> String {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: `fd` is an open terminal and `buf` is writable for its length.
    let rc = unsafe { libc::ttyname_r(fd.as_raw_fd(), buf.as_mut_ptr(), buf.len()) };
    assert_eq!(
        rc,
        0,
        "ttyname_r: {}",
        std::io::Error::from_raw_os_error(rc)
    );
    // SAFETY: on success `ttyname_r` leaves a NUL-terminated name in `buf`.
    unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// Runs `command` with its standard input on `/dev/null` and its output on
/// a fresh terminal whose path it receives as its last argument.
fn drive_opened_tty(mut command: Command) -> (String, i32, bool) {
    let (master, slave) = open_pty();
    // SAFETY: the master is open; non-blocking reads keep the loop moving.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let before = local_modes(&slave);
    let stdio = |fd: &OwnedFd| Stdio::from(fd.try_clone().expect("dup the terminal"));
    let mut child = command
        .arg(tty_path(&slave))
        .stdin(Stdio::null())
        .stdout(stdio(&slave))
        .stderr(stdio(&slave))
        .spawn()
        .expect("spawn the program");
    let mut out = Vec::new();
    let mut typed = false;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            break status;
        }
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd`; the wait is bounded so the child's exit
        // is noticed between reads.
        unsafe { libc::poll(&raw mut poll, 1, 100) };
        drain(&master, &mut out);
        if !typed && out.windows(5).any(|w| w == b"ready") {
            let mut writer = std::fs::File::from(master.try_clone().expect("dup the master"));
            writer.write_all(b"ab\x03").expect("type into the terminal");
            typed = true;
        }
    };
    drain(&master, &mut out);
    let after = local_modes(&slave);
    let text = String::from_utf8_lossy(&out).replace('\r', "");
    (text, status.code().unwrap_or(-1), before == after)
}

fn assert_opened_tty_session(tier: &str, (text, code, restored): (String, i32, bool)) {
    assert_eq!(code, 0, "{tier}: {text}");
    for line in [
        "stdin tty false",
        "opened tty true",
        "size 101x33",
        "got #[97, 98, 3]",
        "done",
    ] {
        assert!(text.contains(line), "{tier}: missing `{line}`\n{text}");
    }
    assert!(
        restored,
        "{tier}: the terminal modes were not restored\n{text}"
    );
}

fn tty_source(dir: &Path) -> PathBuf {
    let path = dir.join("term_tty.gos");
    std::fs::write(&path, TTY_PROGRAM).expect("write the program");
    path
}

#[test]
fn std_term_drives_an_opened_terminal_on_the_vm() {
    let src = tty_source(&scratch("tty-vm"));
    let mut command = Command::new(gos());
    command.arg("run").arg(&src).env("GOS_JIT", "0");
    assert_opened_tty_session("vm", drive_opened_tty(command));
}

#[test]
fn std_term_drives_an_opened_terminal_on_the_jit() {
    let src = tty_source(&scratch("tty-jit"));
    let mut command = Command::new(gos());
    command
        .arg("run")
        .arg(&src)
        .env("GOSSAMER_JIT_THRESHOLD", "1");
    assert_opened_tty_session("jit", drive_opened_tty(command));
}

#[test]
fn std_term_drives_an_opened_terminal_as_a_native_binary() {
    let dir = scratch("tty-native");
    let src = tty_source(&dir);
    for release in [false, true] {
        let out = dir.join(if release { "release" } else { "debug" });
        let mut build = Command::new(gos());
        build.arg("build").arg("--out-dir").arg(&out).arg(&src);
        if release {
            build.arg("--release");
        }
        let outcome = build.output().expect("run gos build");
        assert!(
            outcome.status.success(),
            "{}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        assert_opened_tty_session(
            "native",
            drive_opened_tty(Command::new(out.join("term_tty"))),
        );
    }
}
