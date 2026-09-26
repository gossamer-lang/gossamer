//! End-to-end proof that ecosystem adapters use the public binding ABI.

use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_gos").expect("gos binary path"))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn assert_expected_output(stdout: &str) {
    for expected in ["rows=1", "attrs=2", "args=2", "token=access-token"] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout}"
        );
    }
}

#[test]
fn external_binding_supports_ecosystem_library_shapes_without_builtins() {
    let root = workspace_root();
    let binding_api = root.join("crates/gossamer-binding");
    let dir = std::env::temp_dir().join(format!(
        "gos-ecosystem-binding-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ecosystem-binding/src")).expect("create binding crate");
    std::fs::create_dir_all(dir.join("src")).expect("create source directory");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/ecosystem-binding\"\nversion = \"0.1.0\"\n\n\
         [rust-bindings]\necosystem-binding = { path = \"ecosystem-binding\" }\n",
    )
    .expect("write project manifest");
    std::fs::write(
        dir.join("ecosystem-binding/Cargo.toml"),
        format!(
            "[package]\nname = \"ecosystem-binding\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             publish = false\n\n[workspace]\n\n[lib]\ncrate-type = [\"rlib\"]\n\n\
             [dependencies]\ngossamer-binding = {{ path = {binding_api:?} }}\n"
        ),
    )
    .expect("write binding manifest");
    std::fs::write(
        dir.join("ecosystem-binding/src/lib.rs"),
        r#"
use std::collections::HashMap;
use gossamer_binding::{Bytes, register_module};

register_module!(
    name: ecosystem,
    doc: "Generic external-library capability fixture.",
    // SQL/Postgres-style row batches and fallible driver calls.
    fn database_query(sql: String) -> Result<Vec<String>, String> {
        if sql == "select id" { Ok(vec!["row:7".to_string()]) } else { Err("bad query".to_string()) }
    }
    // OpenTelemetry-style attribute maps.
    fn attribute_count(attrs: HashMap<String, String>) -> i64 {
        i64::try_from(attrs.len()).unwrap_or(i64::MAX)
    }
    // CLI parser output and diagnostics.
    fn parse_args(args: Vec<String>) -> Result<Vec<String>, String> {
        if args.is_empty() { Err("missing command".to_string()) } else { Ok(args) }
    }
    // Redis/RPC/protobuf/MessagePack/CBOR payloads share typed Bytes.
    fn binary_round_trip(payload: Bytes) -> Result<Bytes, String> { Ok(payload) }
    // OAuth/OIDC wrappers expose tokens through the ordinary Result ABI.
    fn exchange_code(code: String) -> Result<String, String> {
        if code == "good" { Ok("access-token".to_string()) } else { Err("invalid code".to_string()) }
    }
);

pub fn __bindings_force_link() { __gos_ecosystem::force_link(); }
"#,
    )
    .expect("write external binding source");
    std::fs::write(
        dir.join("src/main.gos"),
        r#"
use ecosystem::database_query
use ecosystem::attribute_count
use ecosystem::parse_args
use ecosystem::exchange_code
use std::collections::Map

fn main() {
    match database_query("select id") {
        Ok(rows) => { println("rows={}", rows.len()) },
        Err(err) => { panic(err) },
    }
    let mut attrs: Map<String, String> = Map::new()
    attrs.insert("service", "fixture")
    attrs.insert("environment", "test")
    println("attrs={}", attribute_count(attrs))
    match parse_args(["serve", "--dry-run"]) {
        Ok(args) => { println("args={}", args.len()) },
        Err(err) => { panic(err) },
    }
    match exchange_code("good") {
        Ok(token) => { println("token={}", token) },
        Err(err) => { panic(err) },
    }
}
"#,
    )
    .expect("write Gossamer source");

    let out = Command::new(gos_bin())
        .arg("run")
        .arg("src/main.gos")
        .current_dir(&dir)
        .env("GOSSAMER_ROOT", &root)
        .env("GOSSAMER_CACHE", dir.join("cache"))
        .output()
        .expect("run ecosystem binding fixture");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "ecosystem binding fixture failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_expected_output(&stdout);
}

const SCAFFOLD_ADDITIONS: &str = r#"
#[gos_module("extras")]
mod extras {
    pub fn touch() {}
}

pub struct Counter {
    value: i64,
}

#[gossamer_binding::gos_opaque]
impl Counter {
    pub fn new() -> Self {
        Self { value: 0 }
    }

    pub fn inc(&mut self) -> i64 {
        self.value += 1;
        self.value
    }

    pub fn get(&self) -> i64 {
        self.value
    }
}

gossamer_binding::register_module!(
    name: calls,
    doc: "Callbacks into Gossamer.",

    cb_fn apply(d, f: gossamer_binding::PersistentCallback, x: i64) -> i64 {
        match f.invoke(d, vec![gossamer_binding::Value::Int(x)]) {
            Ok(gossamer_binding::Value::Int(n)) => n,
            other => panic!("unexpected {other:?}"),
        }
    }

    cb_fn shout(d, f: gossamer_binding::BindingCallback, s: String) -> String {
        match f.invoke(d, vec![gossamer_binding::Value::String(s.as_str().into())]) {
            Ok(gossamer_binding::Value::String(t)) => t.to_string(),
            other => panic!("unexpected {other:?}"),
        }
    }
);

pub fn __bindings_force_link() {
    __gos_calls::force_link();
}
"#;

const SCAFFOLD_MAIN: &str = r#"use probe
use extras
use calls
use Counter

fn double(x: i64) -> i64 {
    x * 2
}

fn main() {
    println("{}", probe::greet("gossamer"))
    println("{:?}", probe::parse_int("12"))
    println("{:?}", extras::touch())
    let c = Counter::new()
    Counter::inc(c)
    Counter::inc(c)
    println("{}", Counter::get(c))
    let k = 3
    println("{}", calls::apply(|x: i64| x * k, 14))
    println("{}", calls::apply(|x: i64| x + 1, 14))
    println("{}", calls::apply(double, 5))
    println("{}", calls::shout(|s: String| s.to_uppercase() + "!", "hey"))
}
"#;

const SCAFFOLD_EXPECTED: &str = "hello, gossamer\nOk(12)\n()\n2\n42\n15\n10\nHEY!\n";

/// The crate `gos new --template binding` writes builds as written, and a
/// project calling it - a unit function, an opaque type sharing a standard
/// handle's name, and callbacks of every closure shape - prints the same
/// under `gos run` and from `gos build` binaries.
#[test]
fn scaffolded_binding_runs_and_builds() {
    let root = workspace_root();
    let dir = std::env::temp_dir().join(format!("gos-binding-scaffold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let gos = |args: &[&str], cwd: &std::path::Path| {
        Command::new(gos_bin())
            .args(args)
            .current_dir(cwd)
            .env("GOSSAMER_ROOT", &root)
            .env("GOSSAMER_CACHE", dir.join("cache"))
            .env("GOSSAMER_CACHE_DIR", dir.join("cache"))
            .output()
            .expect("spawn gos")
    };
    let app = dir.join("app");
    let out = gos(&["new", "example.com/app", "--path", "app"], &dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = gos(
        &[
            "new",
            "example.com/probe",
            "--template",
            "binding",
            "--path",
            "app/probe",
        ],
        &dir,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lib = app.join("probe/src/lib.rs");
    let mut source = std::fs::read_to_string(&lib).expect("read scaffolded lib.rs");
    source.push_str(SCAFFOLD_ADDITIONS);
    std::fs::write(&lib, source).expect("extend scaffolded lib.rs");
    let mut manifest =
        std::fs::read_to_string(app.join("project.toml")).expect("read project manifest");
    manifest.push_str("\n[rust-bindings]\nprobe-binding = { path = \"probe\" }\n");
    std::fs::write(app.join("project.toml"), manifest).expect("write project manifest");
    std::fs::write(app.join("src/main.gos"), SCAFFOLD_MAIN).expect("write main");

    let run = gos(&["run", "."], &app);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "gos run failed:\n{stderr}");
    assert_eq!(String::from_utf8_lossy(&run.stdout), SCAFFOLD_EXPECTED);

    for (profile, flags) in [("debug", &[][..]), ("release", &["--release"][..])] {
        let mut args = vec!["build"];
        args.extend_from_slice(flags);
        args.push(".");
        let build = gos(&args, &app);
        assert!(
            build.status.success(),
            "{profile} build failed:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let binary =
            app.join("target")
                .join(profile)
                .join(if cfg!(windows) { "app.exe" } else { "app" });
        let native = Command::new(&binary).output().expect("run the artifact");
        assert!(
            native.status.success(),
            "{profile} binary failed:\n{}",
            String::from_utf8_lossy(&native.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&native.stdout),
            SCAFFOLD_EXPECTED,
            "{profile} binary"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
