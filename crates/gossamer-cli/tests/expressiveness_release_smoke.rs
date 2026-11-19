//! Field defaults, in-body methods, and grouped `use` through
//! `gos build --release`: each
//! fixture's native binary prints its pinned transcript, and the tier-parity
//! walk holds the other tiers to the same output.

#![allow(missing_docs)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn run_release(stem: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let src = root.join(format!("feature-testing-examples/{stem}.gos"));
    let dir = env::temp_dir().join(format!("gos-{stem}-release-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let built = Command::new(env!("CARGO_BIN_EXE_gos"))
        .args(["build", "--release", "--out-dir"])
        .arg(&dir)
        .arg(&src)
        .output()
        .expect("spawn gos build");
    assert!(
        built.status.success(),
        "gos build --release failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = Command::new(dir.join(stem))
        .output()
        .expect("run release binary");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        run.status.success(),
        "release binary exited {:?}: stderr={}",
        run.status.code(),
        String::from_utf8_lossy(&run.stderr)
    );
    String::from_utf8_lossy(&run.stdout).into_owned()
}

fn pinned(stem: &str) -> String {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{stem}.stdout")),
    )
    .expect("read the pinned transcript")
}

#[test]
fn field_defaults_print_their_pinned_transcript_in_a_release_build() {
    assert_eq!(
        run_release("struct_field_defaults"),
        pinned("struct_field_defaults")
    );
}

#[test]
fn packed_handle_structs_print_their_pinned_transcript_in_a_release_build() {
    assert_eq!(
        run_release("packed_handle_structs"),
        pinned("packed_handle_structs")
    );
}

#[test]
fn packed_enum_payloads_print_their_pinned_transcript_in_a_release_build() {
    assert_eq!(
        run_release("packed_enum_payload_children"),
        pinned("packed_enum_payload_children")
    );
}

#[test]
fn generic_types_render_and_order_in_a_release_build() {
    assert_eq!(
        run_release("generic_render_and_order"),
        pinned("generic_render_and_order")
    );
}

#[test]
fn grouped_uses_print_their_pinned_transcript_in_a_release_build() {
    assert_eq!(run_release("use_groups"), pinned("use_groups"));
}

#[test]
fn in_body_methods_print_their_pinned_transcript_in_a_release_build() {
    assert_eq!(
        run_release("type_body_methods"),
        pinned("type_body_methods")
    );
}
