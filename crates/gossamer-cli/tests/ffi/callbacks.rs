//! C callbacks: named functions passed as C function pointers, context
//! through `ffi::Handle`, callbacks run during the call and from a later
//! call, faults raised inside one, a callback from a thread the library
//! starts, and calls through native function pointers.

use crate::support::{Project, fixture_program};

#[test]
fn a_callback_runs_during_the_call_with_its_context() {
    let project = Project::new(
        "callback-context",
        &fixture_program(
            r"
fn scaled(x: ffi::c_int, ctx: Ptr<ffi::c_void>) -> ffi::c_int {
    let factor = unsafe { ffi::Handle::<i32>::from_ptr(ctx) }.get()
    x * factor
}

fn collect(x: i32, ctx: Ptr<ffi::c_void>) {
    let seen = unsafe { ffi::Handle::<Vec<i32>>::from_ptr(ctx) }
    seen.update(|v: &mut Vec<i32>| v.push(x * x))
}

fn main() {
    let factor = ffi::Handle::new(10i32)
    println(unsafe { apply(scaled, 2, factor.as_ptr()) })
    factor.release()
    let seen = ffi::Handle::new(Vec::<i32>::new())
    let xs = #[1i32, 2, 3]
    unsafe { each(xs, 3, collect, seen.as_ptr()) }
    println(seen.take())
}
",
        ),
    );
    project.expect_everywhere("50\n#[1, 4, 9]");
}

#[test]
fn every_scalar_class_crosses_a_callback() {
    let project = Project::new(
        "callback-classes",
        &fixture_program(
            r#"
fn half(x: f64) -> f64 {
    x / 2.0
}

fn quarter(x: f32) -> f32 {
    x / 4.0
}

fn widen(a: i8, b: u16, c: i64, d: u8) -> i64 {
    a as i64 + b as i64 + c + d as i64
}

fn identity(p: Ptr<ffi::c_void>) -> Option<Ptr<ffi::c_void>> {
    Some(p)
}

fn nothing(p: Ptr<ffi::c_void>) -> Option<Ptr<ffi::c_void>> {
    None
}

fn flagged(flag: bool) -> ffi::c_int {
    if flag { 7 } else { 3 }
}

fn main() {
    println(unsafe { map_double(half, 9.0) })
    println(unsafe { map_float(quarter, 3.0f32) })
    println(unsafe { map_wide(widen, 1000000000000) })
    let p: Ptr<u8> = unsafe { ffi::alloc(1) }
    let same = unsafe { map_pointer(identity, p.cast()) }
    let gone = unsafe { map_pointer(nothing, p.cast()) }
    println(f"{same.unwrap().address() == p.address()} {gone.is_none()}")
    unsafe { ffi::free(p) }
    println(f"{unsafe { map_flag(flagged, 1) }} {unsafe { map_flag(flagged, 0) }}")
}
"#,
        ),
    );
    project.expect_everywhere("9\n1.25\n1000000064998\ntrue true\n7 3");
}

#[test]
fn a_stored_callback_runs_from_a_later_call() {
    // Registered by one call, run by another on the same thread: the shape
    // of event loops such as `glfwPollEvents` or `gtk_main`. The callback
    // makes a foreign call of its own.
    let project = Project::new(
        "callback-stored",
        &fixture_program(
            r"
fn on_event(value: ffi::c_int, ctx: Ptr<ffi::c_void>) {
    let counter: Ptr<Counter> = ctx.cast()
    unsafe { counter_add(counter, value) }
}

fn main() {
    let c = unsafe { counter_new(0) }
    unsafe { set_handler(on_event, c.cast()) }
    unsafe { fire(3) }
    unsafe { fire(4) }
    println(unsafe { counter_get(c) })
    unsafe { counter_free(c) }
}
",
        ),
    );
    project.expect_everywhere("7");
}

#[test]
fn a_fault_in_a_callback_is_raised_after_the_call_returns() {
    let project = Project::new(
        "callback-panic",
        &fixture_program(
            r#"
fn refuse(x: ffi::c_int, ctx: Ptr<ffi::c_void>) -> ffi::c_int {
    if x > 1 {
        panic("refused {x}")
    }
    x
}

fn main() {
    println("before")
    let total = unsafe { apply(refuse, 1, ffi::Handle::new(0).as_ptr()) }
    println(f"not reached {total}")
}
"#,
        ),
    );
    project.expect_fault_everywhere(101, &["refused 2"]);
    for tier in crate::support::TIERS {
        let out = project.run_on(tier);
        assert!(out.stdout.contains("before"), "{tier:?}:\n{}", out.all());
        assert!(
            !out.stdout.contains("not reached"),
            "{tier:?}:\n{}",
            out.all()
        );
    }
}

#[test]
fn a_callback_reading_a_released_handle_is_gx0014() {
    let project = Project::new(
        "callback-released",
        &fixture_program(
            r"
fn scaled(x: ffi::c_int, ctx: Ptr<ffi::c_void>) -> ffi::c_int {
    x * unsafe { ffi::Handle::<i32>::from_ptr(ctx) }.get()
}

fn main() {
    let factor = ffi::Handle::new(3i32)
    let context = factor.as_ptr()
    factor.release()
    println(unsafe { apply(scaled, 2, context) })
}
",
        ),
    );
    project.expect_fault_everywhere(101, &["GX0014"]);
}

#[test]
fn a_callback_on_a_thread_the_library_started_is_gx0015() {
    let project = Project::new(
        "callback-thread",
        &fixture_program(
            r#"
fn on_event(value: ffi::c_int, ctx: Ptr<ffi::c_void>) {
    println(f"ran {value}")
}

fn main() {
    let c = unsafe { counter_new(0) }
    unsafe { set_handler(on_event, c.cast()) }
    unsafe { fire_on_thread(5) }
    println("not reached")
}
"#,
        ),
    );
    project.expect_fault_everywhere(101, &["GX0015", "on_event"]);
    for tier in crate::support::TIERS {
        let out = project.run_on(tier);
        assert!(!out.stdout.contains("ran 5"), "{tier:?}:\n{}", out.all());
    }
}

#[test]
fn a_native_function_pointer_is_called_through() {
    let project = Project::new(
        "callback-fnptr",
        &fixture_program(
            r#"
fn main() {
    let doubler = unsafe { ffi::fn_from_ptr::<Fn(ffi::c_int) -> ffi::c_int>(get_doubler()) }
    let halver = unsafe { ffi::fn_from_ptr::<Fn(f64) -> f64>(get_halver()) }
    println(f"{doubler(21)} {halver(5.0)}")
}
"#,
        ),
    );
    project.expect_everywhere("42 2.5");
}

#[test]
fn process_exit_inside_a_callback_ends_the_program() {
    let project = Project::new(
        "callback-exit",
        &fixture_program(
            r#"
use std::process
use std::runtime

fn leave(x: ffi::c_int, ctx: Ptr<ffi::c_void>) -> ffi::c_int {
    process::exit(3)
    x
}

fn main() {
    runtime::at_exit(|| println("hook ran"))
    println(unsafe { apply(leave, 1, ffi::Handle::new(0).as_ptr()) })
}
"#,
        ),
    );
    for tier in crate::support::TIERS {
        let out = project.run_on(tier);
        assert_eq!(out.code, Some(3), "{tier:?}:\n{}", out.all());
        assert!(out.stdout.contains("hook ran"), "{tier:?}:\n{}", out.all());
    }
}

#[test]
fn a_callback_passed_from_a_compiled_loop_runs_on_every_tier() {
    // `sum_halves` is hot enough for the JIT, so the callback's C entry is
    // generated by the JIT's own backend there.
    let project = Project::new(
        "callback-hot",
        r#"use std::ffi

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    fn map_double(f: Fn(f64) -> f64, x: f64) -> f64
}

fn half(x: f64) -> f64 {
    x / 2.0
}

fn sum_halves(n: i64) -> f64 {
    let mut total = 0.0
    for i in 0..n {
        total += unsafe { map_double(half, i as f64) }
    }
    total
}

fn main() {
    println(sum_halves(10))
}
"#,
    );
    project.expect_everywhere("45");
}
