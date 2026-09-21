//! A program whose stdout reader goes away ends the way a Unix tool piped into
//! `head` does - by `SIGPIPE`, with nothing on stderr - on the bytecode VM, a
//! compiled binary, and the toolchain's own output alike.

#![cfg(unix)]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROGRAM: &str =
    "fn main() {\n    for i in 0..1000000 {\n        println(\"line {i}\")\n    }\n}\n";

fn gos() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-closed-stdout-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Reads one line of `command`'s stdout, closes the pipe, and asserts the
/// process then ends by `SIGPIPE` without writing a panic to stderr.
fn assert_ends_quietly(mut command: Command, what: &str) {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut first = String::new();
    stdout.read_line(&mut first).expect("read first line");
    assert!(!first.is_empty(), "{what}: printed nothing");
    drop(stdout);
    let output = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "{what}: stderr: {stderr}");
    assert_eq!(
        output.status.signal(),
        Some(libc_sigpipe()),
        "{what}: status {:?}, stderr: {stderr}",
        output.status
    );
}

const fn libc_sigpipe() -> i32 {
    13
}

fn write_program(dir: &Path) -> PathBuf {
    let src = dir.join("lines.gos");
    std::fs::write(&src, PROGRAM).expect("write program");
    src
}

#[test]
fn bytecode_vm_ends_when_stdout_closes() {
    let dir = scratch("vm");
    let src = write_program(&dir);
    let mut command = Command::new(gos());
    command.env("GOS_JIT", "0").arg("run").arg(&src);
    assert_ends_quietly(command, "gos run");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compiled_binary_ends_when_stdout_closes() {
    let dir = scratch("aot");
    let src = write_program(&dir);
    let built = Command::new(gos())
        .arg("build")
        .arg("--out-dir")
        .arg(&dir)
        .arg(&src)
        .output()
        .expect("gos build");
    assert!(
        built.status.success(),
        "build: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert_ends_quietly(Command::new(dir.join("lines")), "compiled binary");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn toolchain_output_ends_when_stdout_closes() {
    let dir = scratch("parse");
    // An AST dump well past any pipe's buffer, so the toolchain is still
    // writing when the reader goes away.
    let mut body = String::new();
    for i in 0..4000 {
        let _ = writeln!(body, "    println(\"{i}\")");
    }
    let src = dir.join("wide.gos");
    std::fs::write(&src, format!("fn main() {{\n{body}}}\n")).expect("write program");
    let mut command = Command::new(gos());
    command.arg("parse").arg(&src);
    assert_ends_quietly(command, "gos parse");
    let _ = std::fs::remove_dir_all(&dir);
}
