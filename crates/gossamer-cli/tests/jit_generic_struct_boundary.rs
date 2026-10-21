#![allow(missing_docs)]

use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

#[test]
fn bodies_taking_generic_struct_instantiations_run_as_native_entries() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/jit_generic_struct_shapes.gos");
    let output = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .env("GOS_JIT", "1")
        .env("GOSSAMER_JIT_THRESHOLD", "1")
        .env("GOS_JIT_TRACE", "1")
        .output()
        .expect("run gos");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout={stdout} stderr={stderr}");
    assert_eq!(stdout.trim(), "3001 600 a2a2:299 900 7 1 true");
    for body in ["bump", "label", "shift"] {
        assert!(
            stderr.contains(&format!("jit: native hit {body}")),
            "{body} was not entered as compiled code: {stderr}"
        );
    }
}

/// The leak-ledger line a run prints at exit.
fn ledger(stderr: &str) -> String {
    stderr
        .lines()
        .find(|line| line.starts_with("LEAK LEDGER"))
        .unwrap_or_default()
        .to_string()
}

/// Runs `fixture` on the VM and with every body below `main` compiled, and
/// checks the two agree on output and on what is left live at exit, with each
/// of `bodies` entered as compiled code.
fn assert_boundary_parity(fixture: &str, bodies: &[&str]) {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples")
        .join(fixture);
    let run = |jit: bool| {
        let mut command = Command::new(gos_bin());
        command
            .arg("run")
            .arg(&fixture)
            .env("GOS_LEAK_LEDGER", "1")
            .env("GOS_JIT", if jit { "1" } else { "0" });
        if jit {
            // Keeping `main` interpreted makes every call below it cross the
            // boundary with its enum arguments and answers.
            command
                .env("GOS_JIT_SKIP", "main")
                .env("GOSSAMER_JIT_THRESHOLD", "1")
                .env("GOS_JIT_TRACE", "1");
        }
        command.output().expect("run gos")
    };
    let vm = run(false);
    let jit = run(true);
    let vm_err = String::from_utf8_lossy(&vm.stderr);
    let jit_err = String::from_utf8_lossy(&jit.stderr);
    assert!(
        vm.status.success() && jit.status.success(),
        "{vm_err}\n{jit_err}"
    );
    assert_eq!(
        String::from_utf8_lossy(&jit.stdout),
        String::from_utf8_lossy(&vm.stdout)
    );
    for body in bodies {
        assert!(
            jit_err.contains(&format!("jit: native hit {body}")),
            "{body} was not entered as compiled code: {jit_err}"
        );
    }
    assert_eq!(ledger(&jit_err), ledger(&vm_err), "{jit_err}");
}

#[test]
fn enum_payloads_of_structs_cross_the_jit_boundary_both_ways() {
    assert_boundary_parity(
        "jit_enum_struct_payloads.gos",
        &["render", "ramp", "grid", "many", "maybe"],
    );
}

#[test]
fn struct_vectors_wide_tuples_and_optional_structs_cross_the_jit_boundary() {
    assert_boundary_parity(
        "jit_struct_boundary_shapes.gos",
        &["color_at", "stats", "first_invertible", "scale_or", "blur"],
    );
}
