//! Foreign memory: allocation, typed reads and writes by copy, byte buffers,
//! C strings, structs with pointer fields, and pointer conversions.

use crate::support::{Project, fixture_program};

#[test]
fn typed_reads_and_writes_copy_through_a_pointer() {
    let project = Project::new(
        "memory-typed",
        &fixture_program(
            r#"
fn main() {
    let p: Ptr<i64> = unsafe { ffi::alloc(4) }
    unsafe { ffi::write(p, 11i64) }
    unsafe { ffi::write_at(p, 3, -7i64) }
    let first: i64 = unsafe { ffi::read(p) }
    let last: i64 = unsafe { ffi::read_at(p, 3) }
    let middle: i64 = unsafe { ffi::read_at(p, 1) }
    println(f"{first} {middle} {last}")
    let narrow: Ptr<u8> = p.cast()
    println(f"low byte {unsafe { ffi::read_at::<u8>(narrow, 0) }}")
    println(f"sizes {ffi::size_of::<i64>()} {ffi::size_of::<u8>()} {ffi::size_of::<Slice>()}")
    unsafe { ffi::free(p) }
}
"#,
        ),
    );
    project.expect_everywhere("11 0 -7\nlow byte 11\nsizes 8 1 24");
}

#[test]
fn floats_and_bools_round_trip_through_memory() {
    let project = Project::new(
        "memory-floats",
        &fixture_program(
            r#"
fn main() {
    let d: Ptr<f64> = unsafe { ffi::alloc(1) }
    let f: Ptr<f32> = unsafe { ffi::alloc(1) }
    let b: Ptr<bool> = unsafe { ffi::alloc(2) }
    unsafe { ffi::write(d, 2.5) }
    unsafe { ffi::write(f, 0.25f32) }
    unsafe { ffi::write_at(b, 1, true) }
    let dv: f64 = unsafe { ffi::read(d) }
    let fv: f32 = unsafe { ffi::read(f) }
    let b0: bool = unsafe { ffi::read(b) }
    let b1: bool = unsafe { ffi::read_at(b, 1) }
    println(f"{dv} {fv} {b0} {b1}")
    unsafe { ffi::free(d) }
    unsafe { ffi::free(f) }
    unsafe { ffi::free(b) }
}
"#,
        ),
    );
    project.expect_everywhere("2.5 0.25 false true");
}

#[test]
fn byte_buffers_and_c_strings_copy_in_and_out() {
    let project = Project::new(
        "memory-bytes",
        &fixture_program(
            r#"
fn main() {
    let text = unsafe { ffi::to_c_bytes(ffi::cstring("gossamer").unwrap()) }
    println(unsafe { ffi::read_cstr(text) }.unwrap())
    unsafe { ffi::write_bytes(text, #[71u8, 79, 83]) }
    println(unsafe { ffi::read_cstr(text) }.unwrap())
    println(unsafe { ffi::read_bytes(text, 4) })
    unsafe { ffi::free(text) }
    println(unsafe { ffi::read_cstr(greeting()) }.unwrap())
}
"#,
        ),
    );
    project.expect_everywhere("gossamer\nGOSsamer\n#[71, 79, 83, 115]\nhello from C");
}

#[test]
fn a_struct_with_a_pointer_field_crosses_both_ways() {
    let project = Project::new(
        "memory-struct",
        &fixture_program(
            r#"
fn main() {
    let out: Ptr<Slice> = unsafe { ffi::alloc(1) }
    unsafe { slice_fill(out) }
    let filled: Slice = unsafe { ffi::read(out) }
    println(f"{filled.len} {filled.tag} {unsafe { ffi::read_bytes(filled.data, 4) }}")
    println(unsafe { slice_sum(filled) })
    let mine = unsafe { ffi::to_c_bytes(#[1u8, 2, 3]) }
    let own = Slice { data: mine, len: 3, tag: 100 }
    println(unsafe { slice_sum(own) })
    unsafe { ffi::write(out, own) }
    let back: Slice = unsafe { ffi::read(out) }
    println(f"{back.len} {back.tag} {back.data == mine}")
    unsafe { ffi::free(mine) }
    unsafe { ffi::free(out) }
}
"#,
        ),
    );
    project.expect_everywhere("4 7 #[10, 20, 30, 40]\n107\n106\n3 100 true");
}

#[test]
fn a_pointer_converts_and_prints() {
    let project = Project::new(
        "memory-convert",
        &fixture_program(
            r#"
fn main() {
    let p: Ptr<u8> = unsafe { ffi::alloc(1) }
    let address = p.address()
    let again: Option<Ptr<u8>> = unsafe { Ptr::from_address(address) }
    let none: Option<Ptr<u8>> = unsafe { Ptr::from_address(0) }
    println(f"{again.unwrap() == p} {none.is_none()} {f"{p}".starts_with("Ptr(0x")}")
    unsafe { ffi::free(p) }
}
"#,
        ),
    );
    project.expect_everywhere("true true true");
}
