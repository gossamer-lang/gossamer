//! `ffi::View` over foreign memory, read and written in place, and the
//! atomic operations on a foreign integer.

use crate::support::{Project, fixture_program};

const DECLS: &str = r#"
#[repr(C)]
struct Item {
    id: i32
    weight: f64
}

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    fn samples_buffer() -> Ptr<f32>
    fn samples_total() -> f32
    fn items_buffer() -> Ptr<Item>
    fn counter_address() -> Ptr<i64>
    fn counter_value() -> i64
}
"#;

#[test]
fn a_view_reads_and_writes_foreign_memory_in_place() {
    let project = Project::new(
        "views-in-place",
        &fixture_program(&format!(
            "{DECLS}{}",
            r#"
fn main() {
    let view = unsafe { ffi::View::new(samples_buffer(), 8) }
    println(f"{view.len()} {view.get(0)} {view[7]}")
    view.set(0, 10.0)
    println(unsafe { samples_total() })
    let tail = view.slice(6, 8)
    tail.fill(1.0)
    println(unsafe { samples_total() })
    println(view.to_vec())
    view.copy_from(#[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0])
    println(unsafe { samples_total() })
    let items = unsafe { ffi::View::new(items_buffer(), 3) }
    let second = items[1]
    println(f"{second.id} {second.weight}")
    items.set(2, Item { id: 30, weight: 9.5 })
    println(items.to_vec())
}
"#
        )),
    );
    project.expect_everywhere(
        "8 0.5 7.5\n41.5\n29.5\n#[10.0, 1.5, 2.5, 3.5, 4.5, 5.5, 1.0, 1.0]\n36\n2 1.5\n\
         #[Item { id: 1, weight: 0.5 }, Item { id: 2, weight: 1.5 }, Item { id: 30, weight: 9.5 }]",
    );
}

#[test]
fn an_access_outside_a_view_panics_the_same_on_every_tier() {
    let project = Project::new(
        "views-bounds",
        &fixture_program(&format!(
            "{DECLS}{}",
            r"
fn main() {
    let view = unsafe { ffi::View::new(samples_buffer(), 8) }
    println(view.get(8))
}
"
        )),
    );
    project.expect_fault_everywhere(
        101,
        &["ffi::View index out of bounds: the len is 8 but the index is 8"],
    );
}

#[test]
fn atomics_change_a_foreign_integer() {
    let project = Project::new(
        "views-atomics",
        &fixture_program(&format!(
            "{DECLS}{}",
            r"
fn main() {
    let p = unsafe { counter_address() }
    unsafe { ffi::atomic_store(p, 5) }
    println(unsafe { ffi::atomic_fetch_add(p, 3) })
    println(unsafe { counter_value() })
    println(unsafe { ffi::atomic_swap(p, 100) })
    println(unsafe { ffi::atomic_compare_exchange(p, 100, 7) })
    println(unsafe { ffi::atomic_compare_exchange(p, 100, 9) })
    println(unsafe { ffi::atomic_fetch_sub(p, 2) })
    println(unsafe { ffi::atomic_fetch_or(p, 8) })
    println(unsafe { ffi::atomic_load(p) })
    println(unsafe { ffi::read(p) })
    unsafe { ffi::write(p, 42) }
    println(unsafe { counter_value() })
}
"
        )),
    );
    project.expect_everywhere("5\n8\n8\nOk(100)\nErr(7)\n7\n5\n13\n13\n42");
}
