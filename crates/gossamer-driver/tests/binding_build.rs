//! Builds the `echo-binding` fixture with Cargo: the shared library `gos
//! run` loads, and the staticlib a native build links.

use std::fs;
use std::path::{Path, PathBuf};

use gossamer_driver::binding_build::{Profile, StaticBindingsLib};
use gossamer_driver::binding_library::ensure_bindings_library_in;
use gossamer_pkg::{Manifest, RustBindingSpec};

const HEADER: &str = "[project]\nid = \"example.com/binding-build-test\"\nversion = \"0.1.0\"\n";

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("echo-binding")
}

fn fresh_dir() -> PathBuf {
    // Test threads share this process, and two of them can read the clock in
    // the same tick, so the counter is what makes each path its own.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "gos-binding-build-test-{}-{now}-{seq}",
        std::process::id()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

/// A manifest naming the binding at `crate_dir`, with forward slashes so the
/// snippet reads alike on every platform; Cargo's strict TOML rejects an
/// unescaped `\`.
fn manifest_with_binding(dir: &Path, crate_dir: &Path) -> Manifest {
    let body = format!(
        "{HEADER}\n[rust-bindings]\necho-binding = {{ path = \"{}\" }}\n",
        crate_dir.display().to_string().replace('\\', "/")
    );
    fs::write(dir.join("project.toml"), &body).unwrap();
    Manifest::parse(&body).unwrap()
}

fn echo(spec: &RustBindingSpec) -> std::collections::BTreeMap<String, RustBindingSpec> {
    std::collections::BTreeMap::from([("echo-binding".to_string(), spec.clone())])
}

fn spec_of(manifest: &Manifest) -> RustBindingSpec {
    manifest.rust_bindings["echo-binding"].clone()
}

/// A copy of the fixture whose sources a test may edit.
fn fixture_copy() -> PathBuf {
    let dir = fresh_dir().join("echo-binding");
    fs::create_dir_all(dir.join("src")).unwrap();
    let cargo = fs::read_to_string(fixture_path().join("Cargo.toml"))
        .unwrap()
        .replace(
            "path = \"../../../../gossamer-binding\"",
            &format!(
                "path = \"{}\"",
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../gossamer-binding")
                    .display()
                    .to_string()
                    .replace('\\', "/")
            ),
        );
    fs::write(dir.join("Cargo.toml"), cargo).unwrap();
    fs::copy(fixture_path().join("src/lib.rs"), dir.join("src/lib.rs")).unwrap();
    dir
}

#[test]
fn a_library_builds_once_and_again_only_when_its_sources_change() {
    let cache = fresh_dir();
    let project = fresh_dir();
    let crate_dir = fixture_copy();
    let manifest = manifest_with_binding(&project, &crate_dir);
    let spec = spec_of(&manifest);

    let first = ensure_bindings_library_in(&echo(&spec), &project, &cache).expect("build");
    assert!(first.library.is_file(), "{}", first.library.display());
    assert!(first.library.starts_with(cache.join("bindings")));
    assert!(!first.has_signatures(), "the host records the signatures");
    let built_at = fs::metadata(&first.library).unwrap().modified().unwrap();

    let again = ensure_bindings_library_in(&echo(&spec), &project, &cache).unwrap();
    assert_eq!(again, first, "unchanged sources find the build they made");
    assert_eq!(
        fs::metadata(&again.library).unwrap().modified().unwrap(),
        built_at
    );

    let source = crate_dir.join("src/lib.rs");
    let edited = fs::read_to_string(&source)
        .unwrap()
        .replace("to_uppercase", "to_lowercase");
    fs::write(&source, edited).unwrap();
    let rebuilt = ensure_bindings_library_in(&echo(&spec), &project, &cache).unwrap();
    assert_ne!(
        rebuilt.library, first.library,
        "an edit lands under a new name"
    );
    assert!(rebuilt.library.is_file());
    assert!(!first.library.exists(), "the earlier build is pruned");
}

#[test]
fn two_projects_naming_one_binding_share_its_library() {
    let cache = fresh_dir();
    let crate_dir = fixture_copy();
    let one = fresh_dir();
    let two = fresh_dir();
    let a = ensure_bindings_library_in(
        &echo(&spec_of(&manifest_with_binding(&one, &crate_dir))),
        &one,
        &cache,
    )
    .unwrap();
    let b = ensure_bindings_library_in(
        &echo(&spec_of(&manifest_with_binding(&two, &crate_dir))),
        &two,
        &cache,
    )
    .unwrap();
    assert_eq!(a.library, b.library);
}

#[test]
fn staticlib_builds_and_exports_the_binding_init() {
    let cache = fresh_dir();
    let manifest_dir = fresh_dir();
    let manifest = manifest_with_binding(&manifest_dir, &fixture_path());
    let staticlib =
        StaticBindingsLib::from_manifest_in(&manifest, &manifest_dir, Profile::Debug, &cache)
            .unwrap()
            .expect("staticlib");

    let built = staticlib.ensure_built().expect("staticlib build succeeded");
    assert!(built.archive.is_file(), "archive must exist");
    let expected_ext = if cfg!(all(windows, target_env = "msvc")) {
        "lib"
    } else {
        "a"
    };
    assert_eq!(
        built.archive.extension().and_then(|s| s.to_str()),
        Some(expected_ext),
    );
    let bytes = fs::read(&built.archive).unwrap();
    for needle in [&b"gos_binding_init"[..], b"gos_binding_echo__shout"] {
        assert!(
            bytes.windows(needle.len()).any(|w| w == needle),
            "archive must export {}",
            String::from_utf8_lossy(needle)
        );
    }
    // Every Rust staticlib needs the platform C library std is built on.
    assert!(
        !built.native_libs.is_empty(),
        "native libs: {:?}",
        built.native_libs
    );

    let again = staticlib.ensure_built().unwrap();
    assert_eq!(again, built, "a second build short-circuits");
}
