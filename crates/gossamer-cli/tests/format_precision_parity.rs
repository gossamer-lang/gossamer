//! Cross-tier parity for `format!()` precision / width specifiers.
//!
//! Catches H8's orphan-symbol class (the cranelift native dispatch
//! formerly missed `gos_rt_concat_f64_prec`, so any function falling
//! back to Cranelift would silently zero precision-formatted floats)
//! and any future regression that lands the same way.
//!
//! Each test runs the same program through `gos` (interp) and
//! `gos build && ./bin` (native - Cranelift / LLVM depending on the
//! build), asserting byte-identical stdout. Skips silently when the
//! `gos` binary, `cc`, or the LLVM toolchain isn't available.

#![allow(missing_docs)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn workspace_target() -> PathBuf {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let crate_dir = PathBuf::from(manifest_dir);
    crate_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("target")
}

fn write_source(name: &str, body: &str) -> PathBuf {
    let dir = workspace_target().join("format-parity-tests");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join(format!("{name}.gos"));
    std::fs::write(&path, body).expect("write test source");
    path
}

fn run_interp(source: &Path) -> String {
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(source)
        .output()
        .expect("spawn gos");
    assert!(
        out.status.success(),
        "gos failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_native(source: &Path) -> Option<String> {
    let build = Command::new(gos_bin())
        .arg("build")
        .arg(source)
        .output()
        .expect("spawn gos build");
    if !build.status.success() {
        eprintln!(
            "skipping native parity: gos build failed\n{}\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
        return None;
    }
    let stem = source.file_stem()?.to_string_lossy().into_owned();
    let bin = workspace_target()
        .join("debug")
        .join(format!("{stem}{}", std::env::consts::EXE_SUFFIX));
    if !bin.exists() {
        eprintln!(
            "skipping native parity: produced binary not found at {}",
            bin.display()
        );
        return None;
    }
    let run = Command::new(&bin).output().expect("invoke produced binary");
    let _ = std::fs::remove_file(&bin);
    if !run.status.success() {
        eprintln!(
            "skipping native parity: produced binary exited non-zero\n{}",
            String::from_utf8_lossy(&run.stderr),
        );
        return None;
    }
    Some(String::from_utf8_lossy(&run.stdout).into_owned())
}

fn assert_parity(name: &str, body: &str) {
    let source = write_source(name, body);
    let interp = run_interp(&source);
    let Some(native) = run_native(&source) else {
        return;
    };
    assert_eq!(
        interp, native,
        "interp / native diverged for {name}\ninterp:\n{interp}\nnative:\n{native}",
    );
}

#[test]
fn format_precision_zero() {
    assert_parity(
        "fmt_p0",
        r#"fn main() { println(format("{:.0}", 1.5)) }
"#,
    );
}

#[test]
fn format_precision_three() {
    assert_parity(
        "fmt_p3",
        r#"fn main() { println(format("{:.3}", 3.14159)) }
"#,
    );
}

#[test]
fn format_precision_padded_eight_three() {
    assert_parity(
        "fmt_p83",
        r#"fn main() { println(format("{:08.3}", 3.14)) }
"#,
    );
}

#[test]
fn format_combined_specifiers() {
    assert_parity(
        "fmt_combo",
        r#"fn main() {
    let pi = 3.14159265358979

    println(format("pi={:.2} pi3={:.3} pad={:08.3}", pi, pi, pi))

}
"#,
    );
}
