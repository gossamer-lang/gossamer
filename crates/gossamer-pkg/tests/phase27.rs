//!: manifest, MVS resolver, lockfile, scaffolders.

use gossamer_pkg::{
    DependencySpec, InlineDependency, LockedEntry, Lockfile, Manifest, ManifestError, ProjectId,
    ProjectIdError, Resolved, ResolvedSource, Resolver, Version, VersionCatalogue, VersionReq,
    add_registry, pin_to_resolved, remove, render_initial_manifest, render_main_source, tidy,
};

#[test]
fn project_id_round_trips_canonical_examples() {
    let id = ProjectId::parse("example.com/math").unwrap();
    assert_eq!(id.as_str(), "example.com/math");
    assert_eq!(id.domain(), "example.com");
    assert_eq!(id.path(), "math");
    assert_eq!(id.tail(), "math");
}

#[test]
fn project_id_rejects_short_or_malformed_inputs() {
    assert!(matches!(
        ProjectId::parse("math").unwrap_err(),
        ProjectIdError::InvalidDomain(_)
    ));
    assert!(matches!(
        ProjectId::parse("Foo.com/bar").unwrap_err(),
        ProjectIdError::InvalidDomain(_)
    ));
    assert!(matches!(
        ProjectId::parse("example.com/Bad").unwrap_err(),
        ProjectIdError::InvalidSegment(_)
    ));
    assert!(matches!(
        ProjectId::parse("").unwrap_err(),
        ProjectIdError::Empty
    ));
}

#[test]
fn version_parses_and_orders_lexicographically() {
    let a = Version::parse("0.1.0").unwrap();
    let b = Version::parse("0.2.0").unwrap();
    let c = Version::parse("1.0.0").unwrap();
    assert!(a < b);
    assert!(b < c);
}

/// A `^` requirement has no ceiling: it accepts the version it names
/// and every later one, across minor and major alike. A bare literal
/// accepts that version and nothing else.
#[test]
fn a_caret_is_a_floor_and_a_bare_literal_is_a_pin() {
    let floor = VersionReq::parse("^0.1.5").unwrap();
    assert!(floor.matches(Version::new(0, 1, 5)));
    assert!(floor.matches(Version::new(0, 1, 9)));
    assert!(floor.matches(Version::new(0, 2, 0)));
    assert!(floor.matches(Version::new(1, 0, 0)));
    assert!(!floor.matches(Version::new(0, 1, 4)));

    let pinned = VersionReq::parse("1.2.3").unwrap();
    assert!(pinned.matches(Version::new(1, 2, 3)));
    assert!(!pinned.matches(Version::new(1, 2, 4)));
    assert!(!pinned.matches(Version::new(1, 9, 0)));
    assert!(!pinned.matches(Version::new(2, 0, 0)));
}

#[test]
fn manifest_round_trips_through_render() {
    let source = r#"[project]
id = "example.com/math"
version = "0.3.1"
authors = ["Jane Doe <jane@example.com>"]
license = "Apache-2.0"

[dependencies]
"example.org/linalg" = "1.2.0"
"example.com/logging" = { git = "https://git.example.com/logging.git", tag = "v0.8.0" }
"example.net/internal" = { path = "../internal" }

[registries]
"example.org" = "https://registry.example.org/v1"

[trusted-publishers]
"example.org/linalg" = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#;
    let manifest = Manifest::parse(source).unwrap();
    assert_eq!(manifest.project.id.as_str(), "example.com/math");
    assert_eq!(manifest.project.version, Version::new(0, 3, 1));
    assert_eq!(
        manifest.project.authors,
        vec!["Jane Doe <jane@example.com>"]
    );
    assert_eq!(manifest.project.license, "Apache-2.0");
    assert_eq!(manifest.dependencies.len(), 3);
    assert!(manifest.registries.contains_key("example.org"));
    assert_eq!(
        manifest
            .trusted_publishers
            .get("example.org/linalg")
            .map(String::as_str),
        Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
    );

    let rendered = manifest.render();
    let reparsed = Manifest::parse(&rendered).unwrap();
    assert_eq!(reparsed, manifest);
}

#[test]
fn manifest_rejects_missing_id_or_version() {
    let missing_id = "[project]\nversion = \"0.1.0\"\n";
    assert!(matches!(
        Manifest::parse(missing_id).unwrap_err(),
        ManifestError::MissingField("project.id")
    ));
    let missing_version = "[project]\nid = \"example.com/foo\"\n";
    assert!(matches!(
        Manifest::parse(missing_version).unwrap_err(),
        ManifestError::MissingField("project.version")
    ));
}

fn catalogue_with(versions: &[(u32, u32, u32)]) -> (VersionCatalogue, ProjectId) {
    let mut catalogue = VersionCatalogue::new();
    let lib = ProjectId::parse("example.org/lib").unwrap();
    for (major, minor, patch) in versions {
        catalogue.add(&lib, Version::new(*major, *minor, *patch));
    }
    (catalogue, lib)
}

fn app_depending_on(requirement: &str) -> Manifest {
    Manifest::parse(&format!(
        "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\n\"example.org/lib\" = \"{requirement}\"\n",
    ))
    .unwrap()
}

/// A `^` requirement is a floor with no ceiling, so the resolver takes
/// the highest thing the registry has - including across a major
/// boundary, which is what distinguishes it from a caret range.
#[test]
fn a_floor_resolves_to_the_highest_version_available() {
    let (catalogue, _) = catalogue_with(&[(1, 2, 3), (1, 2, 5), (1, 4, 0), (2, 0, 0)]);
    let resolved = Resolver::new(catalogue)
        .resolve(&app_depending_on("^1.2.0"))
        .unwrap();
    assert_eq!(resolved.len(), 1);
    match &resolved[0].pin {
        ResolvedSource::Registry(v) => assert_eq!(*v, Version::new(2, 0, 0)),
        other => panic!("unexpected: {other:?}"),
    }
}

/// A bare literal pins, so the resolver takes that version even when
/// newer ones are available - and fails when it is not published.
#[test]
fn a_pin_resolves_to_the_version_it_names_and_no_other() {
    let (catalogue, _) = catalogue_with(&[(1, 2, 3), (1, 2, 5), (1, 4, 0), (2, 0, 0)]);
    let resolved = Resolver::new(catalogue)
        .resolve(&app_depending_on("1.2.3"))
        .unwrap();
    match &resolved[0].pin {
        ResolvedSource::Registry(v) => assert_eq!(*v, Version::new(1, 2, 3)),
        other => panic!("unexpected: {other:?}"),
    }

    let (catalogue, _) = catalogue_with(&[(1, 2, 3), (1, 4, 0)]);
    let error = Resolver::new(catalogue)
        .resolve(&app_depending_on("1.3.0"))
        .unwrap_err();
    assert!(
        matches!(
            error,
            gossamer_pkg::ResolveError::IncompatibleVersions { .. }
        ),
        "{error:?}"
    );
}

#[test]
fn resolver_passes_inline_dependencies_through() {
    let manifest = Manifest::parse(
        r#"[project]
id = "example.com/app"
version = "0.1.0"

[dependencies]
"example.com/local" = { path = "../local" }
"example.com/repo" = { git = "https://git/foo.git", tag = "v1" }
"#,
    )
    .unwrap();
    let resolved = Resolver::new(VersionCatalogue::new())
        .resolve(&manifest)
        .unwrap();
    assert_eq!(resolved.len(), 2);
    let by_id: std::collections::BTreeMap<_, _> = resolved
        .into_iter()
        .map(|r| (r.id.as_str().to_string(), r.pin))
        .collect();
    assert!(matches!(
        by_id["example.com/local"],
        ResolvedSource::Path(_)
    ));
    assert!(matches!(
        by_id["example.com/repo"],
        ResolvedSource::Git { .. }
    ));
}

#[test]
fn resolver_reports_unsatisfiable_ranges() {
    let manifest = Manifest::parse(
        r#"[project]
id = "example.com/app"
version = "0.1.0"

[dependencies]
"example.org/lib" = "2.0.0"
"#,
    )
    .unwrap();
    let mut catalogue = VersionCatalogue::new();
    let lib = ProjectId::parse("example.org/lib").unwrap();
    catalogue.add(&lib, Version::new(1, 9, 0));
    let err = Resolver::new(catalogue).resolve(&manifest).unwrap_err();
    // 0.8.0: when candidates exist but none satisfy the consumer
    // ranges, the resolver returns `IncompatibleVersions`. When the
    // catalogue is empty, it still returns `Unsatisfiable`.
    let text = format!("{err:?}");
    assert!(
        text.contains("Unsatisfiable") || text.contains("IncompatibleVersions"),
        "unexpected: {text}"
    );
}

#[test]
fn lockfile_round_trips_through_render() {
    let entries = vec![
        Resolved {
            id: ProjectId::parse("example.com/a").unwrap(),
            pin: ResolvedSource::Registry(Version::new(1, 2, 3)),
        },
        Resolved {
            id: ProjectId::parse("example.com/b").unwrap(),
            pin: ResolvedSource::Git {
                url: "https://git/b.git".to_string(),
                reference: "v0.4.0".to_string(),
            },
        },
    ];
    let lock = Lockfile::from_resolved(&entries);
    let rendered = lock.render();
    let reparsed = Lockfile::parse(&rendered).unwrap();
    let expected: Vec<LockedEntry> = entries
        .into_iter()
        .map(|resolved| LockedEntry {
            resolved,
            sha256: None,
            owner_pubkey: None,
        })
        .collect();
    assert_eq!(reparsed.entries, expected);
}

#[test]
fn add_registry_inserts_or_updates_entry() {
    let mut manifest = Manifest::parse("[project]\nid = \"a.b/c\"\nversion = \"0.1.0\"\n").unwrap();
    let id = ProjectId::parse("example.org/lib").unwrap();
    let pin = |major, minor| VersionReq::exact(Version::new(major, minor, 0));
    let changed = add_registry(&mut manifest, &id, pin(1, 0));
    assert!(changed);
    assert!(manifest.dependencies.contains_key(id.as_str()));
    let unchanged = add_registry(&mut manifest, &id, pin(1, 0));
    assert!(!unchanged);
    let updated = add_registry(&mut manifest, &id, pin(1, 1));
    assert!(updated);
    if let Some(DependencySpec::Registry(requirement)) = manifest.dependencies.get(id.as_str()) {
        assert_eq!(requirement, &pin(1, 1));
    } else {
        panic!("expected registry entry");
    }

    // A floor is a different requirement from the pin on the same
    // version, so replacing one with the other is a change.
    let floor = add_registry(
        &mut manifest,
        &id,
        VersionReq::at_least(Version::new(1, 1, 0)),
    );
    assert!(floor);
}

#[test]
fn remove_drops_entry_when_present() {
    let mut manifest = Manifest::parse(
        "[project]\nid = \"a.b/c\"\nversion = \"0.1.0\"\n\n[dependencies]\n\"example.org/lib\" = \"1.0.0\"\n",
    )
    .unwrap();
    let id = ProjectId::parse("example.org/lib").unwrap();
    assert!(remove(&mut manifest, &id));
    assert!(manifest.dependencies.is_empty());
    assert!(!remove(&mut manifest, &id));
}

#[test]
fn tidy_keeps_only_resolved_entries() {
    let mut manifest = Manifest::parse(
        "[project]\nid = \"a.b/c\"\nversion = \"0.1.0\"\n\n[dependencies]\n\"example.org/keep\" = \"1.0.0\"\n\"example.org/drop\" = \"2.0.0\"\n",
    )
    .unwrap();
    let kept = vec![Resolved {
        id: ProjectId::parse("example.org/keep").unwrap(),
        pin: ResolvedSource::Registry(Version::new(1, 0, 0)),
    }];
    tidy(&mut manifest, &kept);
    assert!(manifest.dependencies.contains_key("example.org/keep"));
    assert!(!manifest.dependencies.contains_key("example.org/drop"));
}

#[test]
fn pin_to_resolved_writes_an_exact_pin() {
    let mut manifest = Manifest::parse(
        "[project]\nid = \"a.b/c\"\nversion = \"0.1.0\"\n\n[dependencies]\n\"example.org/lib\" = \"1.0.0\"\n",
    )
    .unwrap();
    let resolved = Resolved {
        id: ProjectId::parse("example.org/lib").unwrap(),
        pin: ResolvedSource::Registry(Version::new(1, 4, 2)),
    };
    pin_to_resolved(&mut manifest, &resolved);
    if let Some(DependencySpec::Registry(requirement)) =
        manifest.dependencies.get("example.org/lib")
    {
        assert_eq!(requirement, &VersionReq::exact(Version::new(1, 4, 2)));
        assert!(requirement.is_exact());
    } else {
        panic!("expected registry entry");
    }
}

#[test]
fn scaffold_renders_initial_manifest_and_main_source() {
    let id = ProjectId::parse("example.com/widget").unwrap();
    let manifest = render_initial_manifest(&id, Version::new(0, 1, 0));
    assert!(manifest.contains("id = \"example.com/widget\""));
    assert!(manifest.contains("version = \"0.1.0\""));
    // A scaffolded project states a floor, not a pin: it must keep
    // building on the next toolchain without an edit.
    assert!(manifest.contains("gossamer-version = \"^v"), "{manifest}");
    let main = render_main_source(&id);
    assert!(main.contains("hello from widget"));
    // The scaffolded manifest should round-trip through the parser.
    let parsed = Manifest::parse(&manifest).unwrap();
    assert_eq!(parsed.project.id.as_str(), "example.com/widget");
    assert!(
        parsed
            .project
            .gossamer_version
            .is_some_and(|requirement| !requirement.is_exact())
    );
}

#[test]
fn ambiguous_inline_dependency_is_rejected() {
    let source = "[project]\nid = \"a.b/c\"\nversion = \"0.1.0\"\n\n[dependencies]\n\"x.y/z\" = { path = \"../z\", git = \"https://git\" }\n";
    let err = Manifest::parse(source).unwrap_err();
    assert!(matches!(err, ManifestError::AmbiguousDependency(ref id) if id == "x.y/z"));
    let _: InlineDependency; // ensure type re-export compiles
}

#[test]
fn native_sources_and_library_kinds_round_trip_through_render() {
    let source = r#"[project]
id = "example.com/sqlite"
version = "0.1.0"

[lib]
kind = ["staticlib", "cdylib"]

[native]
sources = ["csrc/sqlite3.c", "csrc/shim.c"]
include = ["csrc"]
defines = { SQLITE_THREADSAFE = "1", SQLITE_OMIT_LOAD_EXTENSION = "" }
flags = ["-O2"]
"#;
    let manifest = Manifest::parse(source).unwrap();
    let native = manifest.native.as_ref().expect("the [native] table");
    assert_eq!(native.sources, vec!["csrc/sqlite3.c", "csrc/shim.c"]);
    assert_eq!(native.include, vec!["csrc"]);
    assert_eq!(
        native.defines.get("SQLITE_THREADSAFE").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        native
            .defines
            .get("SQLITE_OMIT_LOAD_EXTENSION")
            .map(String::as_str),
        Some("")
    );
    assert_eq!(native.flags, vec!["-O2"]);
    let lib = manifest.lib.as_ref().expect("the [lib] table");
    assert_eq!(
        lib.kind,
        vec![
            gossamer_pkg::LibKind::Staticlib,
            gossamer_pkg::LibKind::Cdylib
        ]
    );
    let reparsed = Manifest::parse(&manifest.render()).unwrap();
    assert_eq!(reparsed, manifest);
}

#[test]
fn a_native_table_without_sources_or_with_an_unknown_key_is_refused() {
    let base = "[project]\nid = \"example.com/x\"\nversion = \"0.1.0\"\n\n[native]\n";
    assert!(Manifest::parse(&format!("{base}include = [\"c\"]\n")).is_err());
    assert!(Manifest::parse(&format!("{base}sources = [\"a.c\"]\nlibs = [\"z\"]\n")).is_err());
    assert!(
        Manifest::parse(
            "[project]\nid = \"example.com/x\"\nversion = \"0.1.0\"\n\n[lib]\nkind = \"dll\"\n"
        )
        .is_err()
    );
}
