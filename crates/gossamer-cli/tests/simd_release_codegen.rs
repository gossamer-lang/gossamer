//! Machine code a release build emits for a `Simd<f64, 4>` kernel: packed vector
//! instructions, and no fused multiply-add, which would round differently from
//! the separate multiply and add every other tier performs.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
#![allow(missing_docs)]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const KERNEL: &str = r#"use std::env

fn simd_affine_kernel(a: Simd<f64, 4>, b: Simd<f64, 4>, c: Simd<f64, 4>) -> Simd<f64, 4> {
    a * b + c
}

fn main() {
    let seed = env::args().len() as f64
    let a = Simd::from_array([seed, 2.0, 3.0, 4.0])
    let b: Simd<f64, 4> = Simd::splat(0.1)
    let c: Simd<f64, 4> = Simd::splat(seed * 0.3)
    println("{:?}", simd_affine_kernel(a, b, c))
}
"#;

fn write_fixture() -> (PathBuf, PathBuf) {
    let dir = env::temp_dir().join(format!("gos-simd-release-codegen-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create fixture directory");
    let source = dir.join("simd_affine.gos");
    fs::write(&source, KERNEL).expect("write fixture");
    (dir, source)
}

fn build_release(dir: &PathBuf, source: &PathBuf) -> PathBuf {
    let output = Command::new(env!("CARGO_BIN_EXE_gos"))
        .args(["build", "--release", "--out-dir"])
        .arg(dir)
        .arg(source)
        .output()
        .expect("run gos build --release");
    assert!(
        output.status.success(),
        "release build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    dir.join("simd_affine")
}

/// The disassembled instructions of `symbol`, one per line.
fn disassembly_of(binary: &PathBuf, symbol: &str) -> Vec<String> {
    let output = Command::new("objdump")
        .args(["-d", "--no-show-raw-insn"])
        .arg(binary)
        .output()
        .expect("run objdump");
    assert!(output.status.success(), "objdump failed");
    let text = String::from_utf8_lossy(&output.stdout);
    let header = format!("<{symbol}>:");
    text.lines()
        .skip_while(|line| !line.ends_with(&header))
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn simd_kernel_compiles_to_vector_instructions_without_fused_multiply_add() {
    let (dir, source) = write_fixture();
    let binary = build_release(&dir, &source);
    let native = Command::new(&binary).output().expect("run release fixture");
    assert!(native.status.success(), "fixture failed");
    let bytecode = Command::new(env!("CARGO_BIN_EXE_gos"))
        .arg("run")
        .arg(&source)
        .env("GOS_JIT", "0")
        .output()
        .expect("run fixture on the bytecode VM");
    assert!(bytecode.status.success(), "bytecode run failed");
    assert_eq!(
        String::from_utf8_lossy(&native.stdout),
        String::from_utf8_lossy(&bytecode.stdout),
        "release and bytecode lanes differ"
    );

    let body = disassembly_of(&binary, "simd_affine_kernel");
    assert!(
        !body.is_empty(),
        "no `simd_affine_kernel` in the release binary"
    );
    let fused: Vec<&String> = body
        .iter()
        .filter(|line| {
            ["vfmadd", "vfmsub", "vfnmadd", "vfnmsub"]
                .iter()
                .any(|op| line.contains(op))
        })
        .collect();
    assert!(
        fused.is_empty(),
        "fused multiply-add in the kernel: {fused:?}"
    );
    assert!(
        body.iter().any(|line| line.contains("mulpd")),
        "no packed multiply in the kernel:\n{}",
        body.join("\n")
    );
}
