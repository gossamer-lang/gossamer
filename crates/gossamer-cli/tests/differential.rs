//! Differential VM-vs-LLVM-AOT execution harness
//!
//! Generates small Gossamer programs from a deterministic grammar
//! seeded by the test index, runs each through `gos` (VM) and
//! `gos build` (LLVM AOT), then byte-compares stdout. Divergence
//! is a tier-parity bug. The grammar is intentionally conservative
//! (no I/O, bounded loops, no goroutines) so the harness can run
//! in a few seconds during normal `cargo test`.
//!
//! Why a deterministic generator instead of cargo-fuzz: the
//! end-to-end build invocation is too slow for libFuzzer's
//! coverage-feedback loop. A bounded grammar with ~200 seeded
//! programs catches the common shapes without the build cost.
//!
//! Set `GOSSAMER_DIFFERENTIAL_SEEDS=N` to override the default
//! sample count (8). Run with `GOSSAMER_DIFFERENTIAL_SEEDS=200`
//! locally for a deeper sweep.

#![allow(missing_docs)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

/// Linear-congruential PRNG. Deterministic, single-state - keeps
/// the grammar walk reproducible from the seed alone.
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Self(
            seed.wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407),
        )
    }
    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 32) as u32
    }
    fn pick<T: Copy>(&mut self, choices: &[T]) -> T {
        let i = self.next_u32() as usize % choices.len();
        choices[i]
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        let span = (hi - lo) as u64;
        lo + (u64::from(self.next_u32()) % span) as i64
    }
}

/// Generates a small Gossamer program that prints a sequence of
/// arithmetic results. Limited to shapes both tiers should handle
/// identically.
fn generate_program(seed: u64) -> String {
    let mut rng = Lcg::new(seed);
    let mut src = String::from("fn main() {\n");
    let stmts = rng.range(2, 8) as usize;
    for _ in 0..stmts {
        match rng.pick(&[0, 1, 2, 3, 4]) {
            0 => {
                let a = rng.range(-1000, 1000);
                let b = rng.range(-1000, 1000);
                let op = rng.pick(&["+", "-", "*"]);
                src += &format!("    println(\"{{}}\", {a} {op} {b});\n");
            }
            1 => {
                let a = rng.range(-1000, 1000);
                let b = rng.range(1, 100);
                src += &format!("    println(\"{{}}\", {a} / {b});\n");
            }
            2 => {
                // Integer comparison.
                let a = rng.range(-100, 100);
                let b = rng.range(-100, 100);
                let op = rng.pick(&["==", "!=", "<", "<=", ">", ">="]);
                src += &format!("    println(\"{{}}\", {a} {op} {b});\n");
            }
            3 => {
                // Bounded loop sum.
                let n = rng.range(1, 20);
                src += &format!(
                    "    let mut sum = 0\n    let mut i = 0\n    loop {{ if i >= {n} {{ break }} ; sum += i; i += 1 }}\n    println(\"{{}}\", sum);\n"
                );
            }
            4 => {
                // String concatenation.
                let a = rng.range(0, 10);
                let b = rng.range(0, 10);
                src += &format!(
                    "    let s = \"a=\" + &{a}.to_string() + \", b=\" + &{b}.to_string()\n    println(\"{{}}\", s);\n"
                );
            }
            _ => unreachable!(),
        }
    }
    src += "}\n";
    src
}

struct Run {
    stdout: String,
    code: Option<i32>,
}

fn run_interp(source: &Path) -> Run {
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(source)
        .output()
        .expect("spawn gos");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        code: out.status.code(),
    }
}

fn run_native(source: &Path) -> Option<Run> {
    let build = Command::new(gos_bin())
        .arg("build")
        .arg(source)
        .output()
        .expect("spawn gos build");
    if !build.status.success() {
        return None;
    }
    let stem = source.file_stem().expect("source has stem");
    let out_path = source
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        .join("target")
        .join("debug")
        .join(stem);
    let run_out = Command::new(&out_path)
        .output()
        .expect("run native artifact");
    let _ = std::fs::remove_file(&out_path);
    Some(Run {
        stdout: String::from_utf8_lossy(&run_out.stdout).into_owned(),
        code: run_out.status.code(),
    })
}

#[test]
fn vm_and_llvm_aot_agree_on_grammar_generated_programs() {
    let n: u64 = std::env::var("GOSSAMER_DIFFERENTIAL_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let dir = std::env::temp_dir().join(format!("gos-differential-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mk tmp dir");
    let mut divergences: Vec<(u64, String, String, String)> = Vec::new();
    for seed in 0..n {
        let source = generate_program(seed);
        let path = dir.join(format!("p-{seed:04}.gos"));
        std::fs::write(&path, &source).expect("write source");
        let interp = run_interp(&path);
        let Some(native) = run_native(&path) else {
            // Build failures are tracked separately; M3 is about
            // *execution* divergence between tiers that both build.
            continue;
        };
        if interp.stdout != native.stdout {
            divergences.push((
                seed,
                source.clone(),
                interp.stdout.clone(),
                native.stdout.clone(),
            ));
        }
        if interp.code != native.code {
            // Use a distinct marker so a code-only divergence is
            // visible in the failure message even when stdout matches.
            divergences.push((
                seed,
                source,
                format!("<exit code {:?}>", interp.code),
                format!("<exit code {:?}>", native.code),
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    if !divergences.is_empty() {
        let mut msg = format!(
            "tier divergence on {} of {n} grammar-seeded programs:\n",
            divergences.len()
        );
        for (seed, src, vm, llvm) in divergences.iter().take(3) {
            msg += &format!(
                "\nseed={seed}\n--- source ---\n{src}\n--- interp stdout ---\n{vm}\n--- llvm stdout ---\n{llvm}\n"
            );
        }
        panic!("{msg}");
    }
}
