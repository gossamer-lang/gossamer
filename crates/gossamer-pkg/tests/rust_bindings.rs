//! Tests for the `[rust-bindings]` manifest section.

use std::path::PathBuf;

use gossamer_pkg::{GitRef, Manifest, ManifestError, RustBindingSpec};

const HEADER: &str = "[project]\nid = \"example.com/proj\"\nversion = \"0.1.0\"\n";

#[test]
fn parses_path_form() {
    let src = format!("{HEADER}\n[rust-bindings]\necho-binding = {{ path = \"./echo\" }}\n");
    let m = Manifest::parse(&src).unwrap();
    assert_eq!(m.rust_bindings.len(), 1);
    let spec = &m.rust_bindings["echo-binding"];
    match spec {
        RustBindingSpec::Path {
            path,
            version,
            default_features,
            features,
            ..
        } => {
            assert_eq!(path, "./echo");
            assert!(version.is_none());
            assert!(*default_features);
            assert!(features.is_empty());
        }
        other => panic!("expected Path, got {other:?}"),
    }
}

#[test]
fn parses_src_single_file_form() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\necho = {{ src = \"bindings/echo.rs\", deps = \"unic-segment = \\\"0.9\\\"\" }}\n"
    );
    let m = Manifest::parse(&src).unwrap();
    let spec = &m.rust_bindings["echo"];
    match spec {
        RustBindingSpec::Src { src, deps } => {
            assert_eq!(src, "bindings/echo.rs");
            assert!(deps.contains("unic-segment"));
        }
        other => panic!("expected Src, got {other:?}"),
    }
}

#[test]
fn rejects_a_prebuilt_archive() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\necho = {{ prebuilt = \"lib/libecho.a\", abi = \"1.0\" }}\n"
    );
    let err = Manifest::parse(&src).unwrap_err();
    assert!(
        matches!(&err, ManifestError::PrebuiltRustBinding(k) if k == "echo"),
        "got {err:?}"
    );
}

#[test]
fn rejects_mixed_path_and_src() {
    let src =
        format!("{HEADER}\n[rust-bindings]\nfoo = {{ path = \"./foo\", src = \"foo.rs\" }}\n");
    let err = Manifest::parse(&src).unwrap_err();
    assert!(
        matches!(err, ManifestError::AmbiguousRustBinding(k) if k == "foo"),
        "expected AmbiguousRustBinding"
    );
}

#[test]
fn parses_git_form_with_branch() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\nfoo = {{ git = \"https://example.com/foo.git\", branch = \"main\" }}\n"
    );
    let m = Manifest::parse(&src).unwrap();
    let spec = &m.rust_bindings["foo"];
    match spec {
        RustBindingSpec::Git { url, reference, .. } => {
            assert_eq!(url, "https://example.com/foo.git");
            assert!(matches!(reference, Some(GitRef::Branch(b)) if b == "main"));
        }
        other => panic!("expected Git, got {other:?}"),
    }
}

#[test]
fn parses_git_form_with_tag() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\nfoo = {{ git = \"https://example.com/foo.git\", tag = \"v1.2.3\" }}\n"
    );
    let m = Manifest::parse(&src).unwrap();
    let spec = &m.rust_bindings["foo"];
    assert!(
        matches!(spec, RustBindingSpec::Git { reference: Some(GitRef::Tag(t)), .. } if t == "v1.2.3")
    );
}

#[test]
fn parses_git_form_with_rev() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\nfoo = {{ git = \"https://example.com/foo.git\", rev = \"abc1234\" }}\n"
    );
    let m = Manifest::parse(&src).unwrap();
    let spec = &m.rust_bindings["foo"];
    assert!(
        matches!(spec, RustBindingSpec::Git { reference: Some(GitRef::Rev(r)), .. } if r == "abc1234")
    );
}

#[test]
fn parses_crates_form() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\nratatui = {{ version = \"0.26.0\", features = [\"crossterm\"] }}\n"
    );
    let m = Manifest::parse(&src).unwrap();
    let spec = &m.rust_bindings["ratatui"];
    match spec {
        RustBindingSpec::Crates {
            version,
            features,
            default_features,
        } => {
            assert_eq!(version.version.major, 0);
            assert_eq!(version.version.minor, 26);
            assert_eq!(features, &vec!["crossterm".to_string()]);
            assert!(*default_features);
        }
        other => panic!("expected Crates, got {other:?}"),
    }
}

#[test]
fn parses_default_features_false() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\nratatui = {{ version = \"0.26.0\", default-features = false }}\n"
    );
    let m = Manifest::parse(&src).unwrap();
    let spec = &m.rust_bindings["ratatui"];
    match spec {
        RustBindingSpec::Crates {
            default_features, ..
        } => {
            assert!(!*default_features);
        }
        other => panic!("expected Crates, got {other:?}"),
    }
}

#[test]
fn coexists_with_dependencies_of_same_name() {
    let src = format!(
        "{HEADER}\n[dependencies]\necho = \"1.0.0\"\n\n[rust-bindings]\necho = {{ path = \"./echo-rs\" }}\n"
    );
    let m = Manifest::parse(&src).unwrap();
    assert!(m.dependencies.contains_key("echo"));
    assert!(m.rust_bindings.contains_key("echo"));
}

#[test]
fn rejects_invalid_binding_name() {
    let src = format!("{HEADER}\n[rust-bindings]\n9bad = {{ path = \"./x\" }}\n");
    let err = Manifest::parse(&src).unwrap_err();
    assert!(matches!(err, ManifestError::BadBindingName(n) if n == "9bad"));
}

#[test]
fn rejects_conflicting_path_and_git() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\nfoo = {{ path = \"./a\", git = \"https://x/y.git\" }}\n"
    );
    let err = Manifest::parse(&src).unwrap_err();
    assert!(matches!(err, ManifestError::AmbiguousRustBinding(n) if n == "foo"));
}

#[test]
fn rejects_conflicting_branch_and_tag() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\nfoo = {{ git = \"https://x/y.git\", branch = \"main\", tag = \"v1\" }}\n"
    );
    let err = Manifest::parse(&src).unwrap_err();
    assert!(matches!(err, ManifestError::AmbiguousGitRef(n) if n == "foo"));
}

#[test]
fn rejects_crates_without_version() {
    let src = format!("{HEADER}\n[rust-bindings]\nfoo = {{ features = [\"x\"] }}\n");
    let err = Manifest::parse(&src).unwrap_err();
    assert!(matches!(err, ManifestError::MissingBindingVersion(n) if n == "foo"));
}

#[test]
fn empty_section_yields_empty_map() {
    let src = format!("{HEADER}\n[rust-bindings]\n");
    let m = Manifest::parse(&src).unwrap();
    assert!(m.rust_bindings.is_empty());
}

#[test]
fn missing_section_yields_empty_map() {
    let m = Manifest::parse(HEADER).unwrap();
    assert!(m.rust_bindings.is_empty());
}

#[test]
fn fingerprint_is_deterministic_for_same_inputs() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\nb = {{ path = \"/abs/b\" }}\na = {{ path = \"/abs/a\" }}\n"
    );
    let m = Manifest::parse(&src).unwrap();
    let fp1 = m.rust_binding_fingerprint(&PathBuf::from("/dummy"));
    let fp2 = m.rust_binding_fingerprint(&PathBuf::from("/dummy"));
    assert_eq!(fp1, fp2);
}

#[test]
fn fingerprint_changes_when_path_changes() {
    let src1 = format!("{HEADER}\n[rust-bindings]\nfoo = {{ path = \"./a\" }}\n");
    let src2 = format!("{HEADER}\n[rust-bindings]\nfoo = {{ path = \"./b\" }}\n");
    let m1 = Manifest::parse(&src1).unwrap();
    let m2 = Manifest::parse(&src2).unwrap();
    let dir = PathBuf::from("/manifest");
    assert_ne!(
        m1.rust_binding_fingerprint(&dir),
        m2.rust_binding_fingerprint(&dir)
    );
}

#[test]
fn fingerprint_resolves_relative_paths_against_manifest_dir() {
    let src = format!("{HEADER}\n[rust-bindings]\nfoo = {{ path = \"./a\" }}\n");
    let m = Manifest::parse(&src).unwrap();
    let fp_under_root = m.rust_binding_fingerprint(&PathBuf::from("/root"));
    let fp_under_other = m.rust_binding_fingerprint(&PathBuf::from("/other"));
    assert_ne!(fp_under_root, fp_under_other);
}

#[test]
fn render_round_trips_path_form() {
    let src = format!("{HEADER}\n[rust-bindings]\necho = {{ path = \"./echo\" }}\n");
    let m = Manifest::parse(&src).unwrap();
    let rendered = m.render();
    let m2 = Manifest::parse(&rendered).unwrap();
    assert_eq!(m, m2);
}

#[test]
fn render_round_trips_git_form() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\nfoo = {{ version = \"1.2.0\", git = \"https://x/y.git\", tag = \"v1.2.3\" }}\n"
    );
    let m = Manifest::parse(&src).unwrap();
    let rendered = m.render();
    let m2 = Manifest::parse(&rendered).unwrap();
    assert_eq!(m, m2);
}

#[test]
fn render_round_trips_crates_form_with_features() {
    let src = format!(
        "{HEADER}\n[rust-bindings]\nratatui = {{ version = \"0.26.0\", features = [\"crossterm\", \"macros\"], default-features = false }}\n"
    );
    let m = Manifest::parse(&src).unwrap();
    let rendered = m.render();
    let m2 = Manifest::parse(&rendered).unwrap();
    assert_eq!(m, m2);
}
