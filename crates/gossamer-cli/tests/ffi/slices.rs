//! Slices of `#[repr(C)]` structs: a pointer to the first of their C-layout
//! elements, read by the callee or written back through `&mut`.

use crate::support::{Project, fixture_program};

const ITEMS: &str = r#"
#[repr(C)]
struct Item {
    id: i32
    weight: f64
}

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    fn items_total(items: [Item], n: ffi::size_t) -> f64
    fn items_scale(items: &mut [Item], n: ffi::size_t, by: f64)
}
"#;

#[test]
fn a_slice_of_structs_reaches_c_as_an_array() {
    let project = Project::new(
        "slices-read",
        &fixture_program(&format!(
            "{ITEMS}{}",
            r"
fn main() {
    let items = #[Item { id: 2, weight: 1.5 }, Item { id: 3, weight: 2.0 }]
    println(unsafe { items_total(items, items.len() as ffi::size_t) })
    let fixed = [Item { id: 1, weight: 4.0 }]
    println(unsafe { items_total(fixed, 1) })
    let none: Vec<Item> = #[]
    println(unsafe { items_total(none, 0) })
}
"
        )),
    );
    project.expect_everywhere("9\n4\n0");
}

#[test]
fn writes_to_a_mut_slice_of_structs_come_back() {
    let project = Project::new(
        "slices-write",
        &fixture_program(&format!(
            "{ITEMS}{}",
            r"
fn main() {
    let mut items = #[Item { id: 1, weight: 1.0 }, Item { id: 2, weight: 3.0 }, Item { id: 3, weight: 5.0 }]
    unsafe { items_scale(&mut items, 3, 2.0) }
    println(items)
    unsafe { items_scale(&mut items[1..3], 2, 0.5) }
    println(items)
}
"
        )),
    );
    project.expect_everywhere(
        "#[Item { id: 101, weight: 2.0 }, Item { id: 102, weight: 6.0 }, Item { id: 103, weight: 10.0 }]\n\
         #[Item { id: 101, weight: 2.0 }, Item { id: 202, weight: 3.0 }, Item { id: 203, weight: 5.0 }]",
    );
}

#[cfg(unix)]
#[test]
fn poll_reads_and_writes_an_array_of_pollfd() {
    let project = Project::new(
        "slices-poll",
        r#"use std::ffi

#[repr(C)]
struct PollFd {
    fd: i32
    events: i16
    revents: i16
}

unsafe extern "C" {
    fn pipe(fds: &mut [i32]) -> i32
    fn write(fd: i32, buf: [u8], n: usize) -> isize
    fn poll(fds: &mut [PollFd], n: u64, timeout: i32) -> i32
}

fn main() {
    let mut fds = #[0i32, 0]
    unsafe { pipe(&mut fds) }
    unsafe { write(fds[1], #[7u8], 1) }
    let mut watched = #[PollFd { fd: fds[0], events: 1, revents: 0 }, PollFd { fd: fds[1], events: 4, revents: 0 }]
    let ready = unsafe { poll(&mut watched, 2, 0) }
    println(f"{ready} {watched[0].revents & 1} {watched[1].revents & 4}")
}
"#,
    );
    project.expect_everywhere("2 1 4");
}
