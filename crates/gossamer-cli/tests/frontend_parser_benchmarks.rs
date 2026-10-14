//! The two Gossamer ports of the parser under `benchmarks/frontend_parser`
//! still type-check, so a language change that breaks them shows up here
//! rather than when someone next measures them.

#![allow(missing_docs)]

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonicalize workspace root")
}

#[test]
fn both_parser_benchmarks_type_check() {
    for variant in ["idiomatic", "flat"] {
        let dir = workspace_root()
            .join("benchmarks/frontend_parser")
            .join(variant);
        let out = Command::new(env!("CARGO_BIN_EXE_gos"))
            .arg("check")
            .current_dir(&dir)
            .env("GOS_NO_CACHE", "1")
            .output()
            .expect("run gos check");
        assert!(
            out.status.success(),
            "{variant} parser benchmark does not check:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
