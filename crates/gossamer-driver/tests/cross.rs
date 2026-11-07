//! cross-compilation tests.

use gossamer_driver::{
    LinkerOptions, ObjectFormat, PrebuiltRuntime, all_targets, compile_source, lookup_target,
};

#[test]
fn registry_covers_every_primary_target() {
    let count = all_targets().count();
    assert!(count >= 7, "expected at least 7 registered targets");
    let names: Vec<_> = all_targets()
        .map(|t| t.triple.as_str().to_string())
        .collect();
    for expected in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "x86_64-unknown-freebsd",
        "wasm32-unknown-unknown",
    ] {
        assert!(names.iter().any(|n| n == expected), "missing {expected}");
    }
}

#[test]
fn lookup_target_returns_info_for_known_triple() {
    let info = lookup_target("x86_64-unknown-linux-gnu").expect("known");
    assert_eq!(info.os, "linux");
    assert_eq!(info.arch, "x86_64");
    assert_eq!(info.pointer_width, 8);
    assert!(info.multi_threaded);
    assert_eq!(info.object_format, ObjectFormat::Elf);
}

#[test]
fn wasm32_is_single_threaded_and_uses_wasm_format() {
    let info = lookup_target("wasm32-unknown-unknown").expect("wasm");
    assert_eq!(info.pointer_width, 4);
    assert!(!info.multi_threaded);
    assert_eq!(info.object_format, ObjectFormat::Wasm);
    assert_eq!(info.object_format.extension(), "wasm");
}

#[test]
fn lookup_target_rejects_unknown_triple() {
    assert!(lookup_target("not-a-triple").is_none());
}

#[test]
fn compile_for_target_uses_requested_triple() {
    let options = LinkerOptions::for_target("aarch64-apple-darwin").expect("registered target");
    let artifact = compile_source("fn main() -> i64 { 0i64 }\n", "cross", &options);
    assert_eq!(artifact.target.as_str(), "aarch64-apple-darwin");
}

#[test]
fn linker_options_embeds_runtime_digest() {
    // gnu is always in the registered-targets set; musl is gated
    // behind the `musl` Cargo feature because most dev machines
    // lack the sysroot.
    let triple = "aarch64-apple-darwin";
    let options = LinkerOptions::for_target(triple).expect("darwin");
    let artifact = compile_source("fn main() -> i64 { 0i64 }\n", "embed", &options);
    let needle = format!("stub-{triple}");
    assert!(
        artifact
            .bytes
            .windows(needle.len())
            .any(|w| w == needle.as_bytes()),
        "expected runtime digest embedded in artifact"
    );
}

#[test]
fn cross_targets_produce_different_artifacts() {
    let opts_linux = LinkerOptions::for_target("x86_64-unknown-linux-gnu").unwrap();
    let opts_mac = LinkerOptions::for_target("aarch64-apple-darwin").unwrap();
    let a = compile_source("fn main() -> i64 { 0i64 }\n", "same", &opts_linux);
    let b = compile_source("fn main() -> i64 { 0i64 }\n", "same", &opts_mac);
    assert_ne!(a.bytes, b.bytes);
}

#[test]
fn prebuilt_runtime_stub_is_deterministic() {
    let a = PrebuiltRuntime::stub(gossamer_driver::TargetTriple(
        "x86_64-unknown-linux-gnu".to_string(),
    ));
    let b = PrebuiltRuntime::stub(gossamer_driver::TargetTriple(
        "x86_64-unknown-linux-gnu".to_string(),
    ));
    assert_eq!(a, b);
}
