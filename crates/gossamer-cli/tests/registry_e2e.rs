//! A package goes through a registry end to end: `gos registry serve` runs
//! one, `gos keygen` makes the publisher key, `gos publish` uploads a
//! library, and a second project fetches it, runs it, and builds it.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

/// A registry server that is stopped when the test ends, however it ends.
struct Registry {
    child: Child,
    url: String,
}

impl Drop for Registry {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One test's directory tree, with `HOME` and the caches pointed inside it
/// so keys, credentials, and fetched packages never touch the real ones.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("gos-registry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).expect("create sandbox");
        Self { root }
    }

    fn gos(&self, dir: &Path) -> Command {
        let mut command = Command::new(gos_bin());
        command
            .current_dir(dir)
            .env("HOME", self.root.join("home"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env_remove("GOS_CACHE_DIR")
            .env_remove("GOS_REGISTRY_URL")
            .env_remove("GOS_PUBLISH_KEY");
        command
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.gos(dir).args(args).output().expect("spawn gos")
    }

    fn serve(&self) -> Registry {
        let mut child = self
            .gos(&self.root)
            .args(["registry", "serve", "--root"])
            .arg(self.root.join("registry"))
            .args(["--addr", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn gos registry serve");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("read the serving line");
        let url = line
            .split_whitespace()
            .last()
            .filter(|word| word.starts_with("http://"))
            .unwrap_or_else(|| panic!("no URL in {line:?}"))
            .to_string();
        Registry { child, url }
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dirs");
        std::fs::write(&path, text).expect("write file");
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn assert_ok(output: &Output, what: &str) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{what} failed\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// Publishes `example.com/greet` 0.1.0 to `registry` and answers the
/// publisher's public key.
fn publish_greet(sandbox: &Sandbox, registry: &Registry) -> String {
    let keygen = assert_ok(
        &sandbox.run(&sandbox.root, &["keygen", "example.com/greet"]),
        "gos keygen",
    );
    let public_key = keygen
        .lines()
        .find_map(|line| line.strip_prefix("\"example.com/greet\" = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("no public key in {keygen}"))
        .to_string();
    sandbox.write(
        "greet/project.toml",
        "[project]\nid = \"example.com/greet\"\nversion = \"0.1.0\"\n",
    );
    sandbox.write(
        "greet/src/lib.gos",
        "pub fn hello(name: String) -> String {\n    format(\"hello, {name}, from greet\")\n}\n",
    );
    let greet = sandbox.root.join("greet");
    let publish = sandbox
        .gos(&greet)
        .arg("publish")
        .env("GOS_REGISTRY_URL", &registry.url)
        .output()
        .expect("spawn gos publish");
    assert_ok(&publish, "gos publish");
    public_key
}

fn write_app(sandbox: &Sandbox, dir: &str, registry: &Registry, trusted: Option<&str>) -> PathBuf {
    let trust = trusted.map_or_else(String::new, |key| {
        format!("\n[trusted-publishers]\n\"example.com/greet\" = \"{key}\"\n")
    });
    sandbox.write(
        &format!("{dir}/project.toml"),
        &format!(
            "[project]\nid = \"example.com/{dir}\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\n\"example.com/greet\" = \"0.1.0\"\n\n\
             [registries]\ndefault = \"{}\"\n{trust}",
            registry.url
        ),
    );
    sandbox.write(
        &format!("{dir}/src/main.gos"),
        "use greet\n\nfn main() {\n    println(greet::hello(\"registry\"))\n}\n",
    );
    sandbox.root.join(dir)
}

#[test]
fn a_published_package_is_fetched_run_and_built_by_a_consumer() {
    let sandbox = Sandbox::new("roundtrip");
    let registry = sandbox.serve();
    let public_key = publish_greet(&sandbox, &registry);
    let app = write_app(&sandbox, "app", &registry, Some(&public_key));

    assert_ok(&sandbox.run(&app, &["fetch"]), "gos fetch");
    let lock = std::fs::read_to_string(app.join("project.lock")).expect("project.lock written");
    assert!(lock.contains("example.com/greet"), "{lock}");

    let run = assert_ok(&sandbox.run(&app, &["run", "."]), "gos run");
    assert_eq!(run.trim(), "hello, registry, from greet");

    assert_ok(
        &sandbox.run(&app, &["build", "--out-dir", "bin"]),
        "gos build",
    );
    let binary = std::fs::read_dir(app.join("bin"))
        .expect("build output")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_file())
        .expect("a built binary");
    let native = Command::new(&binary)
        .output()
        .expect("run the built binary");
    assert_eq!(
        assert_ok(&native, "the built binary").trim(),
        "hello, registry, from greet"
    );
}

#[test]
fn a_registry_refuses_a_second_upload_of_one_version() {
    let sandbox = Sandbox::new("immutable");
    let registry = sandbox.serve();
    publish_greet(&sandbox, &registry);
    let again = sandbox
        .gos(&sandbox.root.join("greet"))
        .arg("publish")
        .env("GOS_REGISTRY_URL", &registry.url)
        .output()
        .expect("spawn gos publish");
    assert!(!again.status.success(), "a republish must fail");
}

#[test]
fn a_package_from_an_unpinned_publisher_is_not_fetched() {
    let sandbox = Sandbox::new("untrusted");
    let registry = sandbox.serve();
    publish_greet(&sandbox, &registry);
    let app = write_app(&sandbox, "stranger", &registry, None);
    let fetch = sandbox.run(&app, &["fetch"]);
    assert!(
        !fetch.status.success(),
        "an untrusted publisher must be refused"
    );
    let stderr = String::from_utf8_lossy(&fetch.stderr);
    assert!(stderr.contains("not trusted"), "{stderr}");
}

#[test]
fn a_registry_dependency_with_no_registry_says_so() {
    let sandbox = Sandbox::new("unconfigured");
    sandbox.write(
        "lonely/project.toml",
        "[project]\nid = \"example.com/lonely\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\n\"example.com/greet\" = \"0.1.0\"\n",
    );
    sandbox.write("lonely/src/main.gos", "fn main() {}\n");
    let fetch = sandbox.run(&sandbox.root.join("lonely"), &["fetch"]);
    assert!(!fetch.status.success());
    let stderr = String::from_utf8_lossy(&fetch.stderr);
    assert!(
        stderr.contains("no package registry is configured"),
        "{stderr}"
    );
}
