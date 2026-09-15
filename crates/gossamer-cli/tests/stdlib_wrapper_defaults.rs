//! Pins the constants the autoderive stdlib wrappers spell out in Gossamer
//! source against the Rust values they mirror. A wrapper is source text, so
//! nothing in the compiler notices when the runtime's own default moves.

#![allow(missing_docs)]

#[test]
fn http2_config_defaults_match_the_runtime() {
    let config = gossamer_std::http_h2::Config::default();
    let expected = [
        (
            "max_concurrent_streams",
            i64::from(config.max_concurrent_streams),
        ),
        ("initial_window_size", i64::from(config.initial_window_size)),
        (
            "initial_connection_window_size",
            i64::from(config.initial_connection_window_size),
        ),
        ("max_frame_size", i64::from(config.max_frame_size)),
        (
            "max_header_list_size",
            i64::from(config.max_header_list_size),
        ),
    ];
    let source = gossamer_parse::autoderive::stdlib_wrapper_source("Http2Config");
    for (field, value) in expected {
        let needle = format!("{field}: {value}");
        assert!(
            source.contains(&needle),
            "the injected Http2Config wrapper does not spell `{needle}`; the \
             runtime default moved and the Gossamer source in \
             crates/gossamer-parse/src/autoderive/stdlib_wrappers.rs must \
             follow it.\n{source}"
        );
    }
}

/// An item import binds the bare name to the wrapper it reaches, so the
/// wrapper source is injected for that spelling too.
#[test]
fn item_imports_inject_the_wrappers_they_name() {
    let cases = [
        (
            "use std::fs::{read_dir, DirInfo}\nfn main() { let _ = read_dir(\".\") }\n",
            &["fn __gos_fs_read_dir", "struct __gos_fs_DirInfo"][..],
        ),
        (
            "use std::fs::metadata\nfn main() { let _ = metadata(\".\") }\n",
            &["fn __gos_fs_metadata"][..],
        ),
        (
            "use std::process::run\nfn main() { let _ = run(\"true\", #[]) }\n",
            &["fn __gos_process_run", "struct __gos_process_Output"][..],
        ),
    ];
    for (program, wanted) in cases {
        let source = gossamer_parse::autoderive::stdlib_wrapper_source(program);
        for needle in wanted {
            assert!(
                source.contains(needle),
                "the wrappers injected for\n{program}\ndo not declare `{needle}`:\n{source}"
            );
        }
    }
}

/// A name the file declares itself keeps its own meaning, so no wrapper is
/// injected for it.
#[test]
fn a_declared_name_pulls_in_no_wrapper() {
    let program = "use std::fs\nfn read_dir(path: String) -> i64 { path.len() }\n\
                   fn main() { let _ = read_dir(\".\") }\n";
    let source = gossamer_parse::autoderive::stdlib_wrapper_source(program);
    assert!(
        !source.contains("fn __gos_fs_read_dir"),
        "a program's own `read_dir` pulled in the stdlib wrapper:\n{source}"
    );
}

/// A stdlib module imported under an alias reaches the same wrappers as its
/// own name does.
#[test]
fn module_aliases_inject_the_wrappers_they_reach() {
    let cases = [
        (
            "use std::fs as f\nfn main() { let _ = f::read_dir(\".\") }\n",
            &["fn __gos_fs_read_dir", "struct __gos_fs_DirInfo"][..],
        ),
        (
            "use std::process as p\nfn main() { let _ = p::run(\"true\", #[]) }\n",
            &["fn __gos_process_run", "struct __gos_process_Output"][..],
        ),
    ];
    for (program, wanted) in cases {
        let source = gossamer_parse::autoderive::stdlib_wrapper_source(program);
        for needle in wanted {
            assert!(
                source.contains(needle),
                "the wrappers injected for\n{program}\ndo not declare `{needle}`:\n{source}"
            );
        }
    }
}

/// A stdlib path the program never imported injects nothing, so the report
/// names the path as written.
#[test]
fn an_unimported_stdlib_path_injects_no_wrapper() {
    let program = "fn main() { let _ = std::fs::read_dir(\".\") }\n";
    let source = gossamer_parse::autoderive::stdlib_wrapper_source(program);
    assert!(
        !source.contains("fn __gos_fs_read_dir"),
        "an unimported `std::fs::read_dir` pulled in the wrapper:\n{source}"
    );
}
