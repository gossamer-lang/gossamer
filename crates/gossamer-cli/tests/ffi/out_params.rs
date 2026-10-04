//! Out-parameters and buffer ownership: `int *`, `double *`, borrowed
//! callee buffers, caller-owned buffers, and transferred ownership.

use crate::support::{Project, fixture_program};

#[test]
fn scalar_out_parameters_are_written_back() {
    let project = Project::new(
        "out-scalars",
        &fixture_program(
            r#"
fn main() {
    let mut high: i32 = 0
    let mut low: i32 = 0
    let written = unsafe { split_pair(0x0000000500000007, &mut high, &mut low) }
    println(f"{written} {high} {low}")
    let mut value = 1.5
    unsafe { scale(4.0, &mut value) }
    println(value)
}
"#,
        ),
    );
    project.expect_everywhere("2 5 7\n6");
}

#[test]
fn a_borrowed_buffer_is_copied_before_the_next_call() {
    let project = Project::new(
        "out-borrowed",
        &fixture_program(
            r"
fn main() {
    let c = unsafe { counter_new(0x04030201) }
    let mut data: Option<Ptr<u8>> = None
    let mut len: ffi::size_t = 0
    unsafe { counter_bytes(c, &mut data, &mut len) }
    let bytes = unsafe { ffi::read_bytes(data.unwrap(), len as i64) }
    unsafe { counter_add(c, 1) }
    println(bytes)
    unsafe { counter_free(c) }
}
",
        ),
    );
    project.expect_everywhere("#[1, 2, 3, 4]");
}

#[test]
fn buffer_ownership_follows_the_api() {
    let project = Project::new(
        "out-ownership",
        &fixture_program(
            r"
fn main() {
    // The library allocates; the program frees with the library's free.
    let made = unsafe { buffer_make(4, 10) }
    println(unsafe { ffi::read_bytes(made, 4) })
    unsafe { buffer_free(made) }
    // The program allocates; the library takes it over and frees it.
    let given = unsafe { ffi::to_c_bytes(#[1u8, 2, 3, 4]) }
    println(unsafe { buffer_take_sum(given, 4) })
    // The program allocates and frees; the library only fills it.
    let room: Ptr<u8> = unsafe { ffi::alloc(3) }
    unsafe { buffer_fill(room, 3, 5) }
    println(unsafe { ffi::read_bytes(room, 3) })
    unsafe { ffi::free(room) }
}
",
        ),
    );
    project.expect_everywhere("#[10, 11, 12, 13]\n10\n#[5, 10, 15]");
}

#[test]
fn a_void_function_runs_on_every_tier() {
    // A C function answering `void` is called for its effect alone.
    let project = Project::new(
        "out-void",
        &fixture_program(
            r"
fn main() {
    let c = unsafe { counter_new(1) }
    unsafe { counter_reset(c) }
    unsafe { counter_add(c, 6) }
    println(unsafe { counter_get(c) })
    unsafe { counter_free(c) }
}
",
        ),
    );
    project.expect_everywhere("6");
}
