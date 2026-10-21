#![allow(missing_docs)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("file has a parent")).unwrap();
    std::fs::write(path, text).unwrap();
}

// A library whose crate root re-exports an enum through an alias, with a
// struct deriving `Default` over that enum in one module and a test module of
// its own in another, and an application that depends on it by path.
fn packages(root: &Path) {
    write(
        &root.join("lib/project.toml"),
        "[project]\nid = \"example.com/scopelib\"\nversion = \"0.1.0\"\ngossamer-version = \"^v0.74.0\"\n\n[lib]\nname = \"scopelib\"\n",
    );
    write(
        &root.join("lib/src/state.gos"),
        "#[derive(Default, PartialEq)]\npub enum State {\n    #[default]\n    Off\n    On\n}\n",
    );
    write(
        &root.join("lib/src/lib.gos"),
        "pub type State = state::State\npub type Holder = widgets::Holder\n",
    );
    write(
        &root.join("lib/src/widgets/mod.gos"),
        "use crate::state::State\n\n#[derive(Default)]\npub struct Holder {\n    pub state: State\n}\n\n#[cfg(test)]\nmod tests {\n    use crate::state::State\n\n    #[derive(Default)]\n    struct Probe {\n        state: State\n    }\n\n    #[test]\n    fn probe_defaults_off() { assert(Probe::default().state == State::Off) }\n}\n",
    );
    write(
        &root.join("app/project.toml"),
        "[project]\nid = \"example.com/scopeapp\"\nversion = \"0.1.0\"\ngossamer-version = \"^v0.74.0\"\n\n[dependencies]\n\"example.com/scopelib\" = { path = \"../lib\" }\n",
    );
    write(
        &root.join("app/src/main.gos"),
        "use scopelib\n\nfn main() {\n    println(f\"{scopelib::Holder::default().state == scopelib::State::Off}\")\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn holder_defaults_off() { assert(scopelib::Holder::default().state == scopelib::State::Off) }\n}\n",
    );
}

#[test]
fn a_package_tests_without_its_dependencies_tests_and_aliases_keep_derives() {
    let root = std::env::temp_dir().join(format!("gos-dep-test-scope-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    packages(&root);
    let run = Command::new(gos_bin())
        .arg("run")
        .arg(".")
        .current_dir(root.join("app"))
        .env("GOS_JIT", "0")
        .output()
        .expect("run gos");
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "true",
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    for package in ["app", "lib"] {
        let test = Command::new(gos_bin())
            .arg("test")
            .current_dir(root.join(package))
            .output()
            .expect("test");
        let out = String::from_utf8_lossy(&test.stdout);
        assert!(
            test.status.success() && out.contains("1 passed, 0 failed"),
            "{package}: {out}{}",
            String::from_utf8_lossy(&test.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
