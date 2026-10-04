//! `ffi::Handle`: Gossamer values native code holds as an integer.

use crate::support::Project;

#[test]
fn a_handle_holds_reads_and_replaces_its_value() {
    let project = Project::new(
        "handle-basics",
        r#"use std::ffi

struct State {
    name: String,
    hits: i64,
}

fn main() {
    let h = ffi::Handle::new(State { name: "first", hits: 0 })
    h.update(|s: &mut State| s.hits += 2)
    h.update(|s: &mut State| s.hits += 3)
    let now = h.get()
    println(f"{now.name} {now.hits}")
    h.set(State { name: "second", hits: 1 })
    let back = unsafe { ffi::Handle::<State>::from_ptr(h.as_ptr()) }
    let last = back.take()
    println(f"{last.name} {last.hits}")
}
"#,
    );
    project.expect_everywhere("first 5\nsecond 1");
}

#[test]
fn a_handle_used_after_release_is_gx0014() {
    let project = Project::new(
        "handle-released",
        r"use std::ffi

fn main() {
    let h = ffi::Handle::new(#[1, 2, 3])
    h.release()
    println(h.get())
}
",
    );
    project.expect_fault_everywhere(101, &["GX0014"]);
}

#[test]
fn a_handle_crosses_goroutines() {
    // A handle is an integer-sized value: it can be sent to another
    // goroutine, which reads and writes the value it holds. Handles do not
    // synchronize; shared state guards itself with `std::sync`.
    let project = Project::new(
        "handle-goroutines",
        r"use std::ffi
use std::sync::channel

fn main() {
    let h = ffi::Handle::new(#[1i64])
    let tx, rx = channel()
    let done_tx, done_rx = channel()
    spawn(|| {
        let received: ffi::Handle<Vec<i64>> = rx.recv().unwrap()
        received.update(|v: &mut Vec<i64>| v.push(2))
        done_tx.send(true)
    })
    tx.send(h)
    let _ = done_rx.recv()
    println(h.take())
}
",
    );
    project.expect_everywhere("#[1, 2]");
}
