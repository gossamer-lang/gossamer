//! The JIT lowers the lane loop of a `Simd<f64, _>` or `Simd<i64, _>` operation
//! to two-lane vector instructions, and the answer is the one the bytecode VM
//! computes lane by lane.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::process::Command;

const KERNEL: &str = r#"use std::env

fn affine(a: Simd<f64, 4>, b: Simd<f64, 4>, c: Simd<f64, 4>) -> Simd<f64, 4> {
    a * b + c - a / b
}

fn mix(a: Simd<i64, 8>, b: Simd<i64, 8>) -> Simd<i64, 8> {
    (a * b + a - b) ^ (a & b | b)
}

fn main() {
    let rounds = env::args().len() as i64 + 40
    let mut acc: Simd<f64, 4> = Simd::splat(0.5)
    let b = Simd::from_array([1.5, -2.0, 0.25, 3.0])
    let c = Simd::from_array([-0.0, 1.0e300, -1.0e-300, 7.0])
    let mut ints: Simd<i64, 8> = Simd::from_array([1, -2, 3, -4, 5, -6, 7, 9223372036854775807])
    let seven: Simd<i64, 8> = Simd::splat(7)
    for _ in 0..rounds {
        acc = affine(acc, b, c)
        ints = mix(ints, seven)
    }
    println("{:?}", acc)
    println("{:?}", ints)
}
"#;

fn run(dir: &std::path::Path, source: &std::path::Path, jit: bool) -> (String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gos"));
    command.current_dir(dir).arg("run").arg(source);
    if jit {
        command.env("GOS_DUMP_CLIF", "1");
    } else {
        command.env("GOS_JIT", "0");
    }
    let output = command.output().expect("run gos");
    assert!(
        output.status.success(),
        "gos run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn jit_simd_lane_loops_use_vector_instructions_and_match_the_vm() {
    let dir = env::temp_dir().join(format!("gos-simd-jit-vectors-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create fixture directory");
    let source = dir.join("simd_kernel.gos");
    fs::write(&source, KERNEL).expect("write fixture");

    let (jit_out, clif) = run(&dir, &source, true);
    let (vm_out, _) = run(&dir, &source, false);
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(jit_out, vm_out, "the JIT's lanes must match the VM's");
    for vector in ["f64x2", "i64x2"] {
        assert!(
            clif.contains(vector),
            "the lane loops should lower to `{vector}` instructions:\n{clif}"
        );
    }
}
