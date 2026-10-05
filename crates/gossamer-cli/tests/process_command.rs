#![allow(missing_docs)]

//! `process::Command` behaviour beyond what the tier-parity fixture covers
//! portably: the POSIX session and process-group facilities, a cleared
//! environment, a working directory, a file as a child's stream, and two
//! goroutines draining stdout and stderr at once. Each program runs on the
//! bytecode VM, the JIT, and a native build, and all three print the same.

mod common;

use common::{TIERS, gos_run_on, stderr, stdout};

fn everywhere(src: &str, expected: &str) {
    for tier in TIERS {
        let out = gos_run_on(tier, src, None, &[]);
        assert!(out.status.success(), "{tier:?} failed:\n{}", stderr(&out));
        assert_eq!(stdout(&out), expected, "{tier:?} printed otherwise");
    }
}

#[cfg(unix)]
#[test]
fn a_cleared_environment_and_a_directory_reach_the_child() {
    everywhere(
        r#"use std::process::Command

fn main() {
    let out = Command::new("/bin/sh")
        .args(#["-c", "echo ${HOME:-unset} $ONLY; pwd"])
        .env_clear()
        .env("ONLY", "this")
        .dir("/")
        .output()
        .unwrap()
    print(out.stdout)
}
"#,
        "unset this\n/\n",
    );
}

#[cfg(unix)]
#[test]
fn stdout_and_stderr_drain_from_their_own_goroutines() {
    everywhere(
        r#"use std::errors
use std::process::{Command, Stdio}

fn count(lines: String) -> i64 {
    lines.split("\n").filter(|line| line.len() > 0).count()
}

fn drain() -> Result<(), errors::Error> {
    let script = "i=0; while [ $i -lt 20000 ]; do echo out $i; echo err $i >&2; i=$((i+1)); done"
    let child = Command::new("sh").args(#["-c", script]).stdout(Stdio::Piped).stderr(Stdio::Piped).spawn()?
    cohort {
        let out = spawn(|| count(child.read_stdout()))
        let err = spawn(|| count(child.read_stderr()))
        println(f"out {out.join()?} err {err.join()?}")
    }?
    println(f"{child.wait()?}")
    Ok(())
}

fn main() {
    drain().unwrap()
}
"#,
        "out 20000 err 20000\n0\n",
    );
}

#[cfg(unix)]
#[test]
fn a_file_is_a_childs_stream() {
    everywhere(
        r#"use std::fs
use std::process::{Command, Stdio}

fn main() {
    let file, path = fs::temp_file("gos-command").unwrap()
    let code = Command::new("sh").args(#["-c", "echo into the file"]).stdout(Stdio::File(file)).status().unwrap()
    println(f"{code} {fs::read_to_string(path).unwrap().trim()}")
    fs::remove_file(path).unwrap()
}
"#,
        "0 into the file\n",
    );
}

#[cfg(unix)]
#[test]
fn a_process_group_is_signalled_whole() {
    everywhere(
        r#"use std::process::Command

fn main() {
    let leader = Command::new("sh").args(#["-c", "sleep 30 & sleep 30; wait"]).new_process_group().spawn().unwrap()
    println(f"running: {leader.wait_timeout(100).unwrap()}")
    leader.kill_group(9).unwrap()
    println(f"ended: {leader.wait().unwrap()}")
}
"#,
        "running: None\nended: 137\n",
    );
}

#[cfg(unix)]
#[test]
fn a_new_session_leads_its_own_group() {
    everywhere(
        r#"use std::process::{Command, Stdio}

fn main() {
    // `setsid` makes the child the leader of a new process group too; `pgid`
    // reads the same on Linux and macOS, where `sid` does not.
    let child = Command::new("sh").args(#["-c", "ps -o pgid= -p $$; echo $$"]).new_session().stdout(Stdio::Piped).spawn().unwrap()
    let text = child.read_stdout()
    let lines = text.split("\n").map(|line| line.trim()).filter(|line| line.len() > 0)
    println(f"session leader: {lines[0] == lines[1]} {child.wait().unwrap()}")
}
"#,
        "session leader: true 0\n",
    );
}

#[cfg(unix)]
#[test]
fn a_controlling_terminal_needs_a_new_session() {
    everywhere(
        r#"use std::process::Command

fn main() {
    match Command::new("true").controlling_terminal(0).spawn() {
        Ok(_) => println("started"),
        Err(e) => println(f"{e}"),
    }
}
"#,
        "process::Command: a controlling terminal belongs to a new session\n",
    );
}

#[test]
fn the_piped_child_of_spawn_piped_is_a_command_child() {
    everywhere(
        r#"use std::process

fn main() {
    let child = process::spawn_piped("sort", #[]).unwrap()
    // Whole words: Windows `sort` takes a few bytes of text for UTF-16.
    child.write_stdin("pear\napple\nmango\n")
    child.close_stdin()
    println(f"{child.read_line()} {child.read_line()} {child.read_line()} {child.read_line()} {child.id() > 0}")
    println(f"{child.wait().unwrap()}")
}
"#,
        "Some(\"apple\") Some(\"mango\") Some(\"pear\") None true\n0\n",
    );
}
