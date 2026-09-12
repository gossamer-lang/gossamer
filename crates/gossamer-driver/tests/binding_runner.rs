//! End-to-end test for [`gossamer_driver::BindingRunner`].
//!
//! Builds a real Cargo runner against the `echo-binding` fixture
//! and asserts that the runner exists, the sigs-dump produces
//! parseable JSON describing the fixture's items, and the
//! staticlib build also lands an archive with the expected
//! `gos_static_install_bindings` entry point.

use std::fs;
use std::path::{Path, PathBuf};

use gossamer_driver::binding_runner::{
    BindingRunner, Profile, StaticBindingsLib, parse_signature_dump,
};
use gossamer_pkg::Manifest;

const HEADER: &str = "[project]\nid = \"example.com/runner-test\"\nversion = \"0.1.0\"\n";

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<workspace>/crates/gossamer-driver`.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    PathBuf::from(manifest)
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap()
}

fn fixture_path() -> PathBuf {
    workspace_root()
        .join("crates")
        .join("gossamer-driver")
        .join("tests")
        .join("fixtures")
        .join("echo-binding")
}

fn fresh_cache() -> PathBuf {
    // Test threads share this process, and two of them can read the clock in
    // the same tick, so the counter is what makes each cache path its own.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "gos-runner-test-{}-{now}-{seq}",
        std::process::id()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

fn write_manifest_with_echo(dir: &Path, fixture: &Path) -> Manifest {
    // Use a TOML basic string with the path's forward-slash form so
    // the snippet is identical on every platform. Native Windows
    // separators (`D:\a\...`) parse fine through Manifest::parse
    // (it strips the outer `"` and keeps inner content verbatim),
    // but Cargo's strict TOML parser later rejects unescaped `\`,
    // so we render the same path style the production code does
    // - see `BindingRunner::cargo_dep_line` / `toml_path_kv`.
    let body = format!(
        "{HEADER}\n[rust-bindings]\necho-binding = {{ path = \"{}\" }}\n",
        fixture.display().to_string().replace('\\', "/")
    );
    fs::write(dir.join("project.toml"), &body).unwrap();
    Manifest::parse(&body).unwrap()
}

#[test]
fn runner_builds_and_produces_signatures() {
    let cache = fresh_cache();
    let manifest_dir = fresh_cache();
    let fixture = fixture_path();
    let manifest = write_manifest_with_echo(&manifest_dir, &fixture);

    let runner = BindingRunner::from_manifest_in(
        &manifest,
        &manifest_dir,
        &workspace_root(),
        Profile::Debug,
        &cache,
    )
    .unwrap()
    .expect("runner");

    let sigs_path = runner
        .ensure_signatures()
        .expect("sigs-dump build + run succeeded");
    assert!(sigs_path.is_file(), "signatures.json must exist");
    let json = fs::read_to_string(&sigs_path).unwrap();
    let dump = parse_signature_dump(&json).expect("valid sigs json");
    let echo = dump
        .modules
        .iter()
        .find(|m| m.path == "echo")
        .expect("`echo` module present");
    assert_eq!(echo.items.len(), 2);
    let names: Vec<&str> = echo.items.iter().map(|i| i.name.as_str()).collect();
    assert!(names.contains(&"shout"));
    assert!(names.contains(&"sum"));

    // Second run is a no-op (cache hit).
    let _again = runner.ensure_signatures().unwrap();
}

#[test]
fn staticlib_builds_and_archive_is_present() {
    let cache = fresh_cache();
    let manifest_dir = fresh_cache();
    let fixture = fixture_path();
    let manifest = write_manifest_with_echo(&manifest_dir, &fixture);

    let staticlib = StaticBindingsLib::from_manifest_in(
        &manifest,
        &manifest_dir,
        &workspace_root(),
        Profile::Debug,
        &cache,
    )
    .unwrap()
    .expect("staticlib");

    let archive = staticlib.ensure_built().expect("staticlib build succeeded");
    assert!(archive.is_file(), "archive must exist");
    let expected_ext = if cfg!(all(windows, target_env = "msvc")) {
        "lib"
    } else {
        "a"
    };
    assert_eq!(
        archive.extension().and_then(|s| s.to_str()),
        Some(expected_ext),
    );

    let bytes = fs::read(&archive).unwrap();
    let needle = b"gos_static_install_bindings";
    let mut found = false;
    for window in bytes.windows(needle.len()) {
        if window == needle {
            found = true;
            break;
        }
    }
    assert!(found, "archive must export gos_static_install_bindings");

    // Second build short-circuits.
    let again = staticlib.ensure_built().unwrap();
    assert_eq!(again, archive);
}
