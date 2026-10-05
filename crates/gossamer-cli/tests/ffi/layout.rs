//! C layout from the program's side: unions, `size_of`, `align_of`,
//! `offset_of`, and C globals reached through `ffi::addr_of`, checked against
//! what the C compiler laid out.

use crate::support::{Project, fixture_program};

const TAGGED: &str = r#"
#[repr(C)]
struct Pair {
    x: f32
    y: f32
}

#[repr(C)]
struct Tagged {
    tag: u8
    value: ffi::Union<(i32, f64, Pair)>
    after: i16
}

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    fn tagged_size() -> ffi::size_t
    fn tagged_value_offset() -> ffi::size_t
    fn tagged_after_offset() -> ffi::size_t
    fn tagged_read(t: &mut Tagged) -> f64
    fn tagged_store_double(t: &mut Tagged, d: f64)
    static mut gosffi_counter: i32
    fn gosffi_bump() -> i32
}
"#;

#[test]
fn a_union_and_the_fields_after_it_sit_where_c_puts_them() {
    let project = Project::new(
        "layout-union",
        &fixture_program(&format!(
            "{TAGGED}{}",
            r#"
fn main() {
    let c_size = unsafe { tagged_size() }
    let c_value = unsafe { tagged_value_offset() }
    let c_after = unsafe { tagged_after_offset() }
    println(f"size {ffi::size_of::<Tagged>() == c_size as i64} {ffi::size_of::<Tagged>()}")
    println(f"value {ffi::offset_of::<Tagged>("value") == c_value as i64}")
    println(f"after {ffi::offset_of::<Tagged>("after") == c_after as i64}")
    println(f"align {ffi::align_of::<Tagged>()} {ffi::align_of::<Pair>()}")
    println(f"nested {ffi::offset_of::<Tagged>("value")}")
}
"#
        )),
    );
    project.expect_everywhere("size true 24\nvalue true\nafter true\nalign 8 4\nnested 8");
}

#[test]
fn union_members_cross_in_both_directions() {
    let project = Project::new(
        "layout-union-values",
        &fixture_program(&format!(
            "{TAGGED}{}",
            r#"
fn main() {
    let mut t = Tagged { tag: 0, value: ffi::Union::new(41i32), after: 0 }
    println(unsafe { tagged_read(&mut t) })
    t.tag = 2
    t.value.set(Pair { x: 1.5, y: 2.0 })
    println(unsafe { tagged_read(&mut t) })
    unsafe { tagged_store_double(&mut t, 6.25) }
    let d: f64 = t.value.get()
    println(f"{t.tag} {d} {t.after}")
    let z: ffi::Union<(i32, f64, Pair)> = ffi::Union::zeroed()
    println(z.get::<i32>())
}
"#
        )),
    );
    project.expect_everywhere("41\n3.5\n1 6.25 -7\n0");
}

#[test]
fn a_c_global_is_read_and_written_through_its_address() {
    let project = Project::new(
        "layout-static",
        &fixture_program(&format!(
            "{TAGGED}{}",
            r"
fn main() {
    let counter = ffi::addr_of(gosffi_counter)
    println(unsafe { ffi::read(counter) })
    unsafe { gosffi_bump() }
    println(unsafe { ffi::read(counter) })
    unsafe { ffi::write(counter, 100) }
    println(unsafe { gosffi_bump() })
}
"
        )),
    );
    project.expect_everywhere("41\n42\n101");
}
