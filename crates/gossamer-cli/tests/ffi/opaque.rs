//! Foreign types: handles a C library owns, never inspected or built from
//! Gossamer, nullable where the API allows NULL.

use crate::support::{Project, fixture_program};

#[test]
fn a_handle_is_created_used_and_freed() {
    let project = Project::new(
        "opaque-lifecycle",
        &fixture_program(
            r#"
fn main() {
    let c = unsafe { counter_new(5) }
    unsafe { counter_add(c, 3) }
    println(unsafe { counter_get(c) })
    unsafe { counter_reset(c) }
    unsafe { counter_reset(c) }
    println(f"{unsafe { counter_get(c) }} after {unsafe { counter_resets(c) }} resets")
    unsafe { counter_free(c) }
}
"#,
        ),
    );
    project.expect_everywhere("8\n0 after 2 resets");
}

#[test]
fn a_nullable_handle_is_an_option() {
    let project = Project::new(
        "opaque-nullable",
        &fixture_program(
            r#"
fn main() {
    match unsafe { counter_find(1) } {
        Some(c) => println(f"found {unsafe { counter_get(c) }}")
        None => println("missing")
    }
    let absent = unsafe { counter_find(2) }
    println(f"key 2 present: {absent.is_some()}")
}
"#,
        ),
    );
    project.expect_everywhere("found 42\nkey 2 present: false");
}

#[test]
fn a_handle_arrives_through_a_pointer_to_pointer() {
    let project = Project::new(
        "opaque-out",
        &fixture_program(
            r#"
fn main() {
    let mut opened: Option<Ptr<Counter>> = None
    let status = unsafe { counter_open(9, &mut opened) }
    let c = opened.unwrap()
    println(f"{status} {unsafe { counter_get(c) }}")
    unsafe { counter_free(c) }
}
"#,
        ),
    );
    project.expect_everywhere("0 9");
}

#[test]
fn a_handle_is_an_ordinary_value() {
    // Stored in a struct, kept in a collection, compared, and sent to a
    // goroutine over a channel: transport is safe, only access is unsafe.
    let project = Project::new(
        "opaque-value",
        &fixture_program(
            r#"
use std::sync::channel
use std::time

struct Database {
    counter: Ptr<Counter>,
    name: String,
}

fn total(db: Database) -> ffi::c_int {
    unsafe { counter_get(db.counter) }
}

fn main() {
    let db = Database { counter: unsafe { counter_new(2) }, name: "main" }
    let all = #[db.counter, db.counter]
    println(f"{db.name} {total(db)} same={all[0] == all[1]}")
    let tx, rx = channel()
    spawn(|| {
        let c = rx.recv().unwrap()
        unsafe { counter_add(c, 40) }
        tx.close()
    })
    let sent: Ptr<Counter> = db.counter
    tx.send(sent)
    time_wait()
    println(unsafe { counter_get(db.counter) })
    unsafe { counter_free(db.counter) }
}

fn time_wait() {
    time::sleep(time::Duration::from_millis(50))
}
"#,
        ),
    );
    project.expect_everywhere("main 2 same=true\n42");
}

#[test]
fn a_null_where_ptr_was_promised_is_gx0013() {
    let project = Project::new(
        "opaque-null",
        r#"use std::ffi
use std::ffi::Ptr

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    type Counter
    #[link_name = "counter_find"]
    fn counter_must_find(key: ffi::c_int) -> Ptr<Counter>
}

fn main() {
    let c = unsafe { counter_must_find(2) }
    println(c)
}
"#,
    );
    project.expect_fault_everywhere(101, &["GX0013", "counter_find"]);
}
