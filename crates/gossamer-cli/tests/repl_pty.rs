//! The REPL on a real pseudo-terminal: output a `print` leaves without a
//! newline is ended before the line editor draws its next prompt, which
//! starts by erasing the line it is on.

#![cfg(unix)]
#![allow(missing_docs, unsafe_code)]

use std::io::Write as _;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn gos() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

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

/// The line editor enables bracketed paste each time it starts reading a
/// line, so its count is the number of prompts drawn.
const PROMPT_START: &[u8] = b"\x1b[?2004h";

/// Runs the REPL on a fresh terminal, typing each of `lines` once the
/// prompt before it is up, and answers everything it wrote.
fn drive_repl(lines: &[&str]) -> String {
    let (master, slave) = open_pty();
    // SAFETY: the master is open; non-blocking reads let the loop below
    // wait with `poll` and drain without stalling.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let home = std::env::temp_dir().join(format!("gos-repl-pty-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("scratch home");
    let stdio = |fd: &OwnedFd| Stdio::from(fd.try_clone().expect("dup the terminal"));
    let mut child = Command::new(gos())
        .arg("repl")
        .env("HOME", &home)
        .env("TERM", "xterm")
        .stdin(stdio(&slave))
        .stdout(stdio(&slave))
        .stderr(stdio(&slave))
        .spawn()
        .expect("spawn the REPL");
    let mut out = Vec::new();
    let mut typed = 0;
    loop {
        if child.try_wait().expect("poll the child").is_some() {
            break;
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
        let prompts = out
            .windows(PROMPT_START.len())
            .filter(|w| *w == PROMPT_START)
            .count();
        if typed < lines.len() && prompts > typed {
            let mut writer = std::fs::File::from(master.try_clone().expect("dup the master"));
            writer
                .write_all(format!("{}\r", lines[typed]).as_bytes())
                .expect("type into the terminal");
            typed += 1;
        }
    }
    drain(&master, &mut out);
    let _ = std::fs::remove_dir_all(&home);
    String::from_utf8_lossy(&out).replace('\r', "")
}

#[test]
fn print_output_is_ended_before_the_next_prompt() {
    let text = drive_repl(&["let y = 29", "print(y)", "print(\"[{y}]\")", "%q"]);
    assert!(
        text.contains("\n29\n"),
        "`print(y)` kept on its own line:\n{text:?}"
    );
    assert!(
        text.contains("\n[29]\n"),
        "`print(\"[{{y}}]\")` kept on its own line:\n{text:?}"
    );
}
