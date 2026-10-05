#![allow(missing_docs)]

//! Programs whose answer once differed between the bytecode VM, the JIT,
//! and a native build, or that one tier refused: each prints the same on
//! all three.

mod common;

use common::{TIERS, gos_run_on, stderr, stdout};

fn everywhere(src: &str, expected: &str) {
    for tier in TIERS {
        let out = gos_run_on(tier, src, None, &[]);
        assert!(out.status.success(), "{tier:?} failed:\n{}", stderr(&out));
        assert_eq!(stdout(&out), expected, "{tier:?} printed otherwise");
    }
}

#[test]
fn an_if_whose_first_branch_returns_has_the_other_branches_type() {
    everywhere(
        r#"fn width(lead: i64) -> i64 {
    let extra = if lead < 0xC2 {
        return -1
    } else if lead < 0xE0 {
        1
    } else {
        2
    }
    extra + 1
}

fn main() {
    println(f"{width(0)} {width(0xC5)} {width(0xE5)}")
}
"#,
        "-1 2 3\n",
    );
}

#[test]
fn byte_vectors_compare_by_their_bytes_inside_an_option() {
    everywhere(
        r#"use std::encoding::base64

fn main() {
    for n in #[1, 7, 8, 9, 42, 43, 48, 64, 100] {
        let decoded = base64::decode(base64::encode(#[65u8; n])).ok()
        let mut built: Vec<u8> = #[]
        for _ in 0..n {
            built.push(65)
        }
        let mut other = built.clone()
        other.push(1)
        println(f"{n} {decoded == Some(built)} {Some(other) == decoded}")
    }
    let low: Vec<u8> = #[1, 200]
    let high: Vec<u8> = #[1, 3]
    println(f"{Some(low) > Some(high)}")
}
"#,
        "1 true false\n7 true false\n8 true false\n9 true false\n42 true false\n\
         43 true false\n48 true false\n64 true false\n100 true false\ntrue\n",
    );
}

#[test]
fn bitwise_operators_combine_booleans() {
    everywhere(
        r#"fn main() {
    let a = true
    let b = false
    let mut seen = 0
    let c = { seen += 1; a } & { seen += 1; b }
    println(f"{a & b} {a | b} {a ^ b} {b ^ b} {c} {seen}")
}
"#,
        "false true true false false 2\n",
    );
}

#[test]
fn a_register_reused_after_a_block_forgets_its_array_kind() {
    // The byte vector `abs_diff` builds inside the interpolation and the
    // mask `lanes_gt` builds next share one register.
    everywhere(
        r#"fn main() {
    let a: Simd<i8, 16> = Simd::from_array([100, -100, 5, -5, 127, -128, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9])
    let b: Simd<i8, 16> = Simd::splat(50)
    println(f"diff {a.abs_diff(b)}")
    let u: Simd<u8, 16> = Simd::from_array([0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 250, 255])
    let t: Simd<u8, 16> = Simd::splat(20)
    println(f"{u.lanes_gt(t).to_bitmask()}")
}
"#,
        "diff Simd([50, 150, 45, 55, 77, 178, 50, 49, 48, 47, 46, 45, 44, 43, 42, 41])\n65528\n",
    );
}

#[test]
fn integer_methods_named_only_inside_an_interpolation_are_available() {
    everywhere(
        r#"fn main() {
    let a: i64 = 7
    let u: u8 = 10
    println(f"{a.saturating_sub(10)} {a.checked_add(1)} {u.saturating_sub(20)} {u.abs_diff(30)}")
}
"#,
        "-3 Some(8) 0 20\n",
    );
}

#[test]
fn a_fused_multiply_add_rounds_once() {
    everywhere(
        r#"fn main() {
    let x: f64 = 0.1
    let y: f32 = 16777216.0
    println(f"{x.mul_add(10.0, -1.0)} {f64::mul_add(2.0, 3.0, 1.0)} {y.mul_add(1.0, 1.0)}")
    println(f"{(1.0f32 / 3.0).mul_add(3.0, -1.0)}")
}
"#,
        "0.00000000000000005551115123125783 7 16777216\n0.000000029802322\n",
    );
}

#[test]
fn f32_and_u64_arrays_render_alike_inside_an_interpolation() {
    everywhere(
        r#"fn main() {
    let xs: [f32; 3] = [0.1, 1.5, -2.25]
    let big: [u64; 2] = [18446744073709551615, 9223372036854775808]
    let lanes: Simd<f32, 4> = Simd::from_array([0.1, 0.2, 0.3, 0.4])
    let wide: Simd<u64, 2> = Simd::from_array([18446744073709551615, 1])
    println(f"f32 {xs} u64 {big}")
    println(f"lanes {lanes} wide {wide}")
    // A loop admits `main` to the JIT, which renders the arrays itself.
    let mut n = 0
    for i in 0..3 {
        n += i
    }
    println(n)
}
"#,
        "f32 [0.1, 1.5, -2.25] u64 [18446744073709551615, 9223372036854775808]\n\
         lanes Simd([0.1, 0.2, 0.3, 0.4]) wide Simd([18446744073709551615, 1])\n3\n",
    );
}
