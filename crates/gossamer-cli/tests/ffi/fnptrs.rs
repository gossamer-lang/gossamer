//! Function pointers held in C data: `ffi::fn_addr` fills the fields of an
//! ops table and a registration array native code calls through.

use crate::support::{Project, fixture_program};

#[test]
fn an_ops_table_and_a_registration_array_call_back_into_the_program() {
    let project = Project::new(
        "fnptrs-tables",
        &fixture_program(
            r#"
#[repr(C)]
struct Ops {
    add: Ptr<ffi::c_void>
    mul: Ptr<ffi::c_void>
    name: Ptr<u8>
}

#[repr(C)]
struct Reg {
    name: Ptr<u8>
    f: Ptr<ffi::c_void>
}

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    fn ops_run(ops: &mut Ops, a: ffi::c_int, b: ffi::c_int) -> ffi::c_int
    fn reg_call(regs: [Reg], name: [u8], x: ffi::c_int) -> ffi::c_int
}

fn add(a: ffi::c_int, b: ffi::c_int) -> ffi::c_int {
    a + b
}

fn mul(a: ffi::c_int, b: ffi::c_int) -> ffi::c_int {
    a * b
}

fn square(x: ffi::c_int) -> ffi::c_int {
    x * x
}

fn negate(x: ffi::c_int) -> ffi::c_int {
    -x
}

fn main() {
    let mut ops = Ops { add: ffi::fn_addr(add), mul: ffi::fn_addr(mul), name: Ptr::null() }
    println(unsafe { ops_run(&mut ops, 3, 4) })
    let sq = unsafe { ffi::to_c_bytes(ffi::cstring("square").unwrap()) }
    let ng = unsafe { ffi::to_c_bytes(ffi::cstring("negate").unwrap()) }
    let regs = #[
        Reg { name: sq, f: ffi::fn_addr(square) },
        Reg { name: ng, f: ffi::fn_addr(negate) },
        Reg { name: Ptr::null(), f: Ptr::null() },
    ]
    println(unsafe { reg_call(regs, ffi::cstring("negate").unwrap(), 9) })
    println(unsafe { reg_call(regs, ffi::cstring("square").unwrap(), 9) })
    let again: Fn(ffi::c_int) -> ffi::c_int = unsafe { ffi::fn_from_ptr(ffi::fn_addr(square)) }
    println(again(12))
    println(f"{ops.name.is_null()} {ops.add.is_null()}")
    unsafe { ffi::free(sq) }
    unsafe { ffi::free(ng) }
}
"#,
        ),
    );
    project.expect_everywhere("712\n-9\n81\n144\ntrue false");
}
