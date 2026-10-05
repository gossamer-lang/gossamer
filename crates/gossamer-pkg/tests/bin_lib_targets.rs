//! Tests for the explicit `[[bin]]` / `[lib]` schema added in
//! 0.4.0. See `~/dev/contexts/lang/manifest_lib_bin_agenda.md`
//! for the migration plan.

use gossamer_pkg::manifest::{BinTarget, LibTarget, Manifest, ManifestError};

#[test]
fn explicit_bin_array_parses() {
    let src = r#"
[project]
id = "example.com/widget"
version = "0.1.0"

[[bin]]
name = "widget"
path = "src/main.gos"

[[bin]]
name = "widget-admin"
path = "src/bin/admin.gos"
"#;
    let m = Manifest::parse(src).expect("parse");
    assert_eq!(m.bins.len(), 2);
    assert_eq!(
        m.bins[0],
        BinTarget {
            name: "widget".to_string(),
            path: Some("src/main.gos".to_string()),
        }
    );
    assert_eq!(
        m.bins[1],
        BinTarget {
            name: "widget-admin".to_string(),
            path: Some("src/bin/admin.gos".to_string()),
        }
    );
    assert!(m.lib.is_none());
    assert!(m.has_explicit_targets());
}

#[test]
fn explicit_lib_table_parses() {
    let src = r#"
[project]
id = "example.com/widget"
version = "0.1.0"

[lib]
name = "widget"
path = "src/lib.gos"
"#;
    let m = Manifest::parse(src).expect("parse");
    assert!(m.bins.is_empty());
    assert_eq!(
        m.lib,
        Some(LibTarget {
            name: Some("widget".to_string()),
            path: Some("src/lib.gos".to_string()),
            kind: Vec::new(),
        })
    );
    assert!(m.has_explicit_targets());
}

#[test]
fn both_bin_and_lib_can_coexist() {
    let src = r#"
[project]
id = "example.com/widget"
version = "0.1.0"

[[bin]]
name = "widget"

[lib]
path = "src/lib.gos"
"#;
    let m = Manifest::parse(src).expect("parse");
    assert_eq!(m.bins.len(), 1);
    assert_eq!(m.bins[0].name, "widget");
    assert!(m.bins[0].path.is_none());
    assert!(m.lib.is_some());
    assert_eq!(
        m.lib.as_ref().unwrap().path,
        Some("src/lib.gos".to_string())
    );
}

#[test]
fn bin_without_path_is_accepted() {
    // Path is optional; defaults to src/bin/<name>.gos at the
    // toolchain level when not specified.
    let src = r#"
[project]
id = "example.com/x"
version = "0.1.0"

[[bin]]
name = "tool"
"#;
    let m = Manifest::parse(src).expect("parse");
    assert_eq!(m.bins[0].name, "tool");
    assert!(m.bins[0].path.is_none());
}

#[test]
fn bin_missing_name_is_rejected() {
    let src = r#"
[project]
id = "example.com/x"
version = "0.1.0"

[[bin]]
path = "src/main.gos"
"#;
    let err = Manifest::parse(src).unwrap_err();
    matches!(err, ManifestError::MissingField("bin.name"));
}

#[test]
fn duplicate_bin_names_are_rejected() {
    let src = r#"
[project]
id = "example.com/x"
version = "0.1.0"

[[bin]]
name = "twice"

[[bin]]
name = "twice"
"#;
    let err = Manifest::parse(src).unwrap_err();
    matches!(err, ManifestError::Malformed { .. });
}

#[test]
fn no_explicit_targets_uses_implicit_convention() {
    let src = r#"
[project]
id = "example.com/x"
version = "0.1.0"
"#;
    let m = Manifest::parse(src).expect("parse");
    assert!(m.bins.is_empty());
    assert!(m.lib.is_none());
    assert!(!m.has_explicit_targets());
}

#[test]
fn bin_section_coexists_with_dependencies() {
    let src = r#"
[project]
id = "example.com/x"
version = "0.1.0"

[[bin]]
name = "service"
path = "src/main.gos"

[dependencies]
"example.org/lib" = "1.2.3"
"#;
    let m = Manifest::parse(src).expect("parse");
    assert_eq!(m.bins.len(), 1);
    assert_eq!(m.dependencies.len(), 1);
}
