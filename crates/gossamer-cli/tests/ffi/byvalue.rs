//! `#[repr(C)]` structs passed and returned by value, across a matrix of
//! shapes each calling convention classifies differently: float pairs and
//! triples, four doubles, mixed integer and float words, large structs,
//! one- and three-byte structs, nested structs, and arguments that run a
//! register class out before a struct.

use crate::support::{Project, fixture_program};

const DECLS: &str = r#"
#[repr(C)]
struct V2 {
    x: f32
    y: f32
}

#[repr(C)]
struct V3 {
    x: f32
    y: f32
    z: f32
}

#[repr(C)]
struct D4 {
    a: f64
    b: f64
    c: f64
    d: f64
}

#[repr(C)]
struct IF {
    i: i32
    f: f32
}

#[repr(C)]
struct ID {
    i: i32
    d: f64
}

#[repr(C)]
struct BigI {
    a: i64
    b: i64
    c: i64
}

#[repr(C)]
struct Small {
    a: u8
}

#[repr(C)]
struct Arr3 {
    b: [u8; 3]
}

#[repr(C)]
struct Nested {
    v: V2
    n: i32
}

#[repr(C)]
struct PairI {
    a: i64
    b: i64
}

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    fn v2_scale(v: V2, k: f32) -> V2
    fn v2_dot(a: V2, b: V2) -> f32
    fn v3_cross(a: V3, b: V3) -> V3
    fn d4_add(a: D4, b: D4) -> D4
    fn id_sum(x: ID) -> f64
    fn if_make(i: i32, f: f32) -> IF
    fn big_rot(x: BigI) -> BigI
    fn small_arr(s: Small, a: Arr3) -> i32
    fn nested_bump(n: Nested) -> Nested
    fn many_then_v2(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64, g: f64, v: V2) -> f64
    fn many8_then_v2(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64, g: f64, h: f64, v: V2, last: f64) -> f64
    fn ints_then_pair(a: i64, b: i64, c: i64, d: i64, e: i64, p: PairI, g: i64) -> i64
}
"#;

#[test]
fn structs_cross_by_value_in_every_shape() {
    let project = Project::new(
        "byvalue-shapes",
        &fixture_program(&format!(
            "{DECLS}{}",
            r#"
fn main() {
    let v = unsafe { v2_scale(V2 { x: 1.5, y: -2.0 }, 2.0) }
    println(f"{v.x} {v.y}")
    println(unsafe { v2_dot(V2 { x: 1.0, y: 2.0 }, V2 { x: 3.0, y: 4.0 }) })
    let c = unsafe { v3_cross(V3 { x: 1.0, y: 0.0, z: 0.0 }, V3 { x: 0.0, y: 1.0, z: 0.0 }) }
    println(f"{c.x} {c.y} {c.z}")
    let d = unsafe { d4_add(D4 { a: 1.0, b: 2.0, c: 3.0, d: 4.0 }, D4 { a: 10.0, b: 20.0, c: 30.0, d: 40.0 }) }
    println(f"{d.a} {d.b} {d.c} {d.d}")
    println(unsafe { id_sum(ID { i: 7, d: 0.25 }) })
    let m = unsafe { if_make(21, 5.0) }
    println(f"{m.i} {m.f}")
    let r = unsafe { big_rot(BigI { a: 1, b: 2, c: 3 }) }
    println(f"{r.a} {r.b} {r.c}")
    println(unsafe { small_arr(Small { a: 4 }, Arr3 { b: [5, 6, 7] }) })
    let n = unsafe { nested_bump(Nested { v: V2 { x: 1.0, y: 1.0 }, n: 10 }) }
    println(f"{n.v.x} {n.v.y} {n.n}")
}
"#
        )),
    );
    project.expect_everywhere("3 -4\n11\n0 0 1\n11 22 33 44\n7.25\n42 2.5\n2 3 1\n4567\n2 3 13");
}

#[test]
fn a_struct_after_the_registers_run_out_reaches_the_stack() {
    let project = Project::new(
        "byvalue-exhaustion",
        &fixture_program(&format!(
            "{DECLS}{}",
            r"
fn main() {
    let v = V2 { x: 1.0, y: 2.0 }
    println(unsafe { many_then_v2(1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, v) })
    println(unsafe { many8_then_v2(1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, v, 3.0) })
    println(unsafe { ints_then_pair(1, 1, 1, 1, 1, PairI { a: 2, b: 3 }, 4) })
}
"
        )),
    );
    project.expect_everywhere("217\n3218\n43205");
}

#[test]
fn callbacks_take_and_answer_structs_by_value() {
    let project = Project::new(
        "byvalue-callbacks",
        &fixture_program(&format!(
            "{DECLS}{}",
            r#"
#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    fn apply_v2(f: Fn(V2, f32) -> V2, v: V2, k: f32) -> V2
    fn apply_d4(f: Fn(D4) -> f64, d: D4) -> f64
    fn apply_big(f: Fn(BigI) -> BigI) -> BigI
    fn get_v2_scale() -> Ptr<ffi::c_void>
}

fn shift(v: V2, k: f32) -> V2 {
    V2 { x: v.x + k, y: v.y - k }
}

fn spread(d: D4) -> f64 {
    d.a + d.b * 10.0 + d.c * 100.0 + d.d * 1000.0
}

fn reverse(b: BigI) -> BigI {
    BigI { a: b.c, b: b.b, c: b.a }
}

fn main() {
    let v = unsafe { apply_v2(shift, V2 { x: 1.0, y: 1.0 }, 0.5) }
    println(f"{v.x} {v.y}")
    println(unsafe { apply_d4(spread, D4 { a: 1.0, b: 2.0, c: 3.0, d: 4.0 }) })
    let r = unsafe { apply_big(reverse) }
    println(f"{r.a} {r.b} {r.c}")
    let scale: Fn(V2, f32) -> V2 = unsafe { ffi::fn_from_ptr(get_v2_scale()) }
    let s = scale(V2 { x: 2.0, y: 3.0 }, 2.0)
    println(f"{s.x} {s.y}")
}
"#
        )),
    );
    project.expect_everywhere("1.5 0.5\n4321\n3 2 1\n4 6");
}
