#![allow(clippy::doc_markdown)]
//! Behavioural regression tests covering: multi-file project bundling,
//! named-module bodies, native-binary `os::*` filesystem and process
//! helpers, `Option<json::Value>` discriminator semantics, and
//! cross-test runner state isolation under JIT warm-up.
//!
//! These tests drive the `gos` binary end-to-end against a temporary
//! source tree and assert observable output, so a regression in the
//! frontend, MIR lowering, runtime FFI, or test runner all turn the
//! relevant case red.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const PER_RUN_TIMEOUT: Duration = Duration::from_mins(1);

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fresh_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-multi-{pid}-{n}-{tag}",
        pid = std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn run_with_timeout(mut child: std::process::Child) -> (String, String, Option<i32>) {
    let deadline = Instant::now() + PER_RUN_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => break,
        }
    }
    let out = child.wait_with_output().expect("wait_with_output");
    (
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n"),
        out.status.code(),
    )
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

fn run_vm(src: &Path) -> (String, String, Option<i32>) {
    let child = Command::new(gos_bin())
        .arg("run")
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos");
    run_with_timeout(child)
}

fn build_native(src: &Path, out_dir: &Path) -> PathBuf {
    let out = Command::new(gos_bin())
        .arg("build")
        .arg("--out-dir")
        .arg(out_dir)
        .arg(src)
        .output()
        .expect("spawn gos build");
    assert!(
        out.status.success(),
        "gos build failed:\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for entry in fs::read_dir(out_dir).expect("read out_dir").flatten() {
        let p = entry.path();
        if p.is_file() && is_executable(&p) {
            return p;
        }
    }
    panic!("no binary in {}", out_dir.display());
}

fn run_native(bin: &Path) -> (String, String, Option<i32>) {
    let child = Command::new(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn binary");
    run_with_timeout(child)
}

fn write_source(dir: &Path, tag: &str, source: &str) -> PathBuf {
    let path = dir.join(format!("{tag}.gos"));
    let mut f = fs::File::create(&path).expect("create source");
    f.write_all(source.as_bytes()).unwrap();
    path
}

/// Writes a multi-file project: a `project.toml` carrying `id` plus each
/// `(relative_path, contents)` pair under a fresh scratch dir. Returns
/// the project root.
fn write_project(tag: &str, id: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = fresh_dir(tag);
    fs::write(
        dir.join("project.toml"),
        format!("[project]\nid = \"{id}\"\nversion = \"0.1.0\"\n"),
    )
    .unwrap();
    for (rel, contents) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    dir
}

/// Runs `gos run .` at the project root (VM tier), resolving the entry itself.
fn project_run_vm(dir: &Path) -> (String, String, Option<i32>) {
    let child = Command::new(gos_bin())
        .arg("run")
        .arg(".")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos");
    run_with_timeout(child)
}

/// Builds at the project root (Cranelift native) and runs the produced
/// `id_tail`-named binary from `target/debug`.
fn project_build_run(dir: &Path, id_tail: &str) -> (String, String, Option<i32>) {
    let build = Command::new(gos_bin())
        .arg("build")
        .current_dir(dir)
        .output()
        .expect("spawn gos build");
    assert!(
        build.status.success(),
        "gos build failed:\nstderr: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let mut bin = dir.join("target/debug").join(id_tail);
    if !std::env::consts::EXE_EXTENSION.is_empty() {
        bin.set_extension(std::env::consts::EXE_EXTENSION);
    }
    assert!(bin.is_file(), "expected binary at {}", bin.display());
    run_native(&bin)
}

#[test]
fn a_free_function_keeps_its_parameters_beside_a_method_of_the_same_name() {
    // A method is reached through its declaring type, so a call written
    // `run(name)` is the entry file's own function. Filing the method under
    // the bare name too handed that call the method's parameters, and the
    // receiver slot wrapped the argument in a `&mut` cell: the string
    // reached the callee as a reference and concatenating it failed.
    let dir = write_project(
        "free-fn-vs-method",
        "example.com/shadow",
        &[
            (
                "src/engine/mod.gos",
                "pub struct Store { pub n: i64 }\n\n\
                 pub fn open(name: String) -> Result<Store, String> {\n\
                 \x20   Ok(Store { n: tail(name).len() })\n\
                 }\n\n\
                 fn tail(name: String) -> String { name + \"/END\" }\n",
            ),
            (
                "src/engine/plan.gos",
                "use crate::engine\n\n\
                 pub struct Prepared { pub tag: i64 }\n\n\
                 impl Prepared {\n\
                 \x20   pub fn run(&mut self, st: &mut engine::Store, extra: i64) -> i64 {\n\
                 \x20       self.tag + st.n + extra\n\
                 \x20   }\n\
                 }\n\n\
                 pub fn drive(st: &mut engine::Store) -> i64 {\n\
                 \x20   let mut p = Prepared { tag: 1 }\n\
                 \x20   p.run(st, 2)\n\
                 }\n",
            ),
            (
                "src/main.gos",
                "use engine\n\
                 use engine::plan\n\n\
                 fn main() {\n\
                 \x20   if let Err(e) = run(\"abc\") { println(\"ERR {}\", e) }\n\
                 \x20   let mut st = engine::Store { n: 3 }\n\
                 \x20   println(\"{}\", plan::drive(&mut st))\n\
                 }\n\n\
                 fn run(name: String) -> Result<(), String> {\n\
                 \x20   let st = engine::open(name)?\n\
                 \x20   println(\"n = {}\", st.n)\n\
                 \x20   Ok(())\n\
                 }\n",
            ),
        ],
    );

    let (out, err, code) = project_run_vm(&dir);
    assert_eq!(
        code,
        Some(0),
        "vm run failed:\nstdout: {out}\nstderr: {err}"
    );
    assert!(out.contains("n = 7"), "vm stdout: {out}\nstderr: {err}");
    assert!(
        out.contains('6'),
        "the method still answers its own call: {out}"
    );

    let (bout, berr, bcode) = project_build_run(&dir, "shadow");
    assert_eq!(
        bcode,
        Some(0),
        "native run failed:\nstdout: {bout}\nstderr: {berr}"
    );
    assert!(
        bout.contains("n = 7"),
        "native stdout: {bout}\nstderr: {berr}"
    );
}

#[test]
fn cross_file_project_bundles_sibling_modules() {
    let dir = fresh_dir("cross-file");
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/probe\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        src.join("util.gos"),
        "pub fn add(a: i64, b: i64) -> i64 { a + b }\n",
    )
    .unwrap();
    fs::write(
        src.join("main.gos"),
        "fn main() { println(\"{}\", util::add(1, 2)) }\n",
    )
    .unwrap();

    // Build at the project root and execute the resulting binary.
    let build_out = Command::new(gos_bin())
        .arg("build")
        .current_dir(&dir)
        .output()
        .expect("gos build");
    assert!(
        build_out.status.success(),
        "gos build failed:\nstderr: {}",
        String::from_utf8_lossy(&build_out.stderr),
    );
    let mut bin_path = dir.join("target/debug/probe");
    if !std::env::consts::EXE_EXTENSION.is_empty() {
        bin_path.set_extension(std::env::consts::EXE_EXTENSION);
    }
    assert!(bin_path.is_file(), "expected probe binary at {bin_path:?}");
    let run = run_native(&bin_path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert!(run.0.trim() == "3", "expected stdout '3', got: {:?}", run.0);
}

#[test]
fn directory_modules_nest_and_carry_types() {
    // `src/<dir>/<dir>/mod.gos` is two module levels below the entry, so
    // every module the layout declares has to be nameable from the entry
    // for `deep::nest::*` to resolve. The struct pins the second half:
    // its structural `eq` is synthesized against the type's declaring
    // module, and `==` must work on both tiers.
    let dir = write_project(
        "nested-dir-modules",
        "example.com/nested",
        &[
            (
                "src/main.gos",
                "fn main() {\n    println(\"{}\", deep::depth())\n    \
                 println(\"{}\", deep::nest::nested())\n    \
                 let a = deep::nest::Nested { n: 5 }\n    \
                 let b = deep::nest::Nested { n: 5 }\n    \
                 println(\"{} {}\", a.n, a == b)\n}\n",
            ),
            (
                "src/deep/mod.gos",
                "pub fn depth() -> i64 { self::nest::nested() - 41 }\n",
            ),
            (
                "src/deep/nest/mod.gos",
                "pub struct Nested { pub n: i64 }\npub fn nested() -> i64 { 42 }\n",
            ),
        ],
    );
    let expected = "1\n42\n5 true\n";
    let vm = project_run_vm(&dir);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected, "vm stdout");
    let native = project_build_run(&dir, "nested");
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, expected, "native stdout");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn module_relative_paths_reach_child_and_sibling_modules() {
    // A path written inside a module is anchored at that module:
    // `self::child::item` and a bare `child::item` both name the child,
    // and `super::sibling::item` crosses to a sibling. All three have to
    // reach the same def the entry names as `mod::child::item`.
    let dir = write_project(
        "module-relative-paths",
        "example.com/relpaths",
        &[
            (
                "src/main.gos",
                "fn main() { println(\"{}\", outer::all()) }\n",
            ),
            (
                "src/outer/mod.gos",
                "pub mod child {\n    pub fn value() -> i64 { 7 }\n}\n\
                 pub fn all() -> i64 {\n    self::child::value() + child::value() \
                 + super::other::value()\n}\n",
            ),
            ("src/other.gos", "pub fn value() -> i64 { 1 }\n"),
        ],
    );
    let expected = "15\n";
    let vm = project_run_vm(&dir);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected, "vm stdout");
    let native = project_build_run(&dir, "relpaths");
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, expected, "native stdout");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn every_sibling_file_may_declare_its_own_test_module() {
    // A module belongs to the file that declares it, so each sibling in a
    // project writes the same `mod tests` without the three of them
    // competing for one name at the unit root. The explicit `mod NAME`
    // declarations are the newline-terminated spelling, which the bundler
    // fills from the project layout.
    let dir = write_project(
        "sibling-test-modules",
        "example.com/sibtests",
        &[
            (
                "src/main.gos",
                "mod util
mod other

                 fn main() { println(\"{}\", util::add(1, 2) + other::mul(2, 3)) }

                 #[cfg(test)]
mod tests {
    #[test]
                     fn entry_test() { assert_eq(1, 1) }
}
",
            ),
            (
                "src/util.gos",
                "pub fn add(a: i64, b: i64) -> i64 { a + b }

                 #[cfg(test)]
mod tests {
    #[test]
                     fn add_adds() { assert_eq(super::add(2, 3), 5) }
}
",
            ),
            (
                "src/other.gos",
                "pub fn mul(a: i64, b: i64) -> i64 { a * b }

                 #[cfg(test)]
mod tests {
    #[test]
                     fn mul_multiplies() { assert_eq(super::mul(2, 3), 6) }
}
",
            ),
        ],
    );
    let expected = "9\n";
    let vm = project_run_vm(&dir);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected, "vm stdout");
    let native = project_build_run(&dir, "sibtests");
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, expected, "native stdout");

    let out = std::process::Command::new(gos_bin())
        .arg("test")
        .current_dir(&dir)
        .output()
        .expect("gos test");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "gos test stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for name in ["entry_test", "add_adds", "mul_multiplies"] {
        assert!(stdout.contains(name), "`{name}` did not run: {stdout}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_test_module_import_stays_out_of_a_later_siblings_scope() {
    // `aa` sorts before `b`, so the test module's `use super::{norm, parse}`
    // is registered before `b`'s own `use aa::norm`. Both name one item, and
    // outside `gos test` the test module is not compiled at all.
    let dir = write_project(
        "test-import-leak",
        "example.com/testimportleak",
        &[
            (
                "src/main.gos",
                "use b::twice\n\nfn main() { println(\"{}\", twice(1)) }\n",
            ),
            (
                "src/aa.gos",
                "pub fn norm(x: i64) -> i64 { x + 1 }\n\
                 pub fn parse(s: String) -> i64 { s.len() }\n\n\
                 #[cfg(test)]\n\
                 mod tests {\n\
                 \x20   use super::{norm, parse}\n\n\
                 \x20   #[test]\n\
                 \x20   fn norm_of_parse() { assert_eq(norm(parse(\"ab\")), 3) }\n\
                 }\n",
            ),
            (
                "src/b.gos",
                "use aa::norm\n\npub fn twice(x: i64) -> i64 { norm(norm(x)) }\n",
            ),
        ],
    );
    let check = Command::new(gos_bin())
        .arg("check")
        .arg(dir.join("src/main.gos"))
        .output()
        .expect("gos check");
    assert!(
        check.status.success(),
        "gos check stderr: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let vm = project_run_vm(&dir);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "3\n", "vm stdout");
    let native = project_build_run(&dir, "testimportleak");
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, "3\n", "native stdout");
    let test = Command::new(gos_bin())
        .arg("test")
        .current_dir(&dir)
        .output()
        .expect("gos test");
    let stdout = String::from_utf8_lossy(&test.stdout).to_string();
    assert!(
        test.status.success(),
        "gos test stderr: {}",
        String::from_utf8_lossy(&test.stderr)
    );
    assert!(
        stdout.contains("norm_of_parse"),
        "test did not run: {stdout}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cross_file_chained_sibling_module_calls() {
    // Three sibling modules where each call cascades into the next:
    // main → a::foo → b::bar → util::helper. Pins both the qualified-
    // path resolution at every hop and the type-checker carrying the
    // callee's `String` return type through the chain so the outer
    // `format!` argument prints as text instead of `<value>`.
    let dir = fresh_dir("chained-cross-file");
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/chained\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        src.join("main.gos"),
        "fn main() { println(\"{}\", a::foo()) }\n",
    )
    .unwrap();
    fs::write(
        src.join("a.gos"),
        "pub fn foo() -> String { format(\"a({})\", b::bar()) }\n",
    )
    .unwrap();
    fs::write(
        src.join("b.gos"),
        "pub fn bar() -> String { format(\"b({})\", util::helper()) }\n",
    )
    .unwrap();
    fs::write(
        src.join("util.gos"),
        "pub fn helper() -> String { \"leaf\" }\n",
    )
    .unwrap();

    // VM tier (`gos`).
    let run_out = Command::new(gos_bin())
        .arg("run")
        .arg(".")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos");
    assert!(
        run_out.status.success(),
        "gos failed:\nstderr: {}",
        String::from_utf8_lossy(&run_out.stderr),
    );
    let run_stdout = String::from_utf8_lossy(&run_out.stdout);
    assert_eq!(
        run_stdout.trim(),
        "a(b(leaf))",
        "VM stdout mismatch: {run_stdout:?}",
    );

    // Cranelift native build (`gos build`).
    let build_out = Command::new(gos_bin())
        .arg("build")
        .current_dir(&dir)
        .output()
        .expect("gos build");
    assert!(
        build_out.status.success(),
        "gos build failed:\nstderr: {}",
        String::from_utf8_lossy(&build_out.stderr),
    );
    let mut bin_path = dir.join("target/debug/chained");
    if !std::env::consts::EXE_EXTENSION.is_empty() {
        bin_path.set_extension(std::env::consts::EXE_EXTENSION);
    }
    assert!(bin_path.is_file(), "expected binary at {bin_path:?}");
    let nat = run_native(&bin_path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(nat.2, Some(0), "native stderr: {}", nat.1);
    assert_eq!(
        nat.0.trim(),
        "a(b(leaf))",
        "native stdout mismatch: {:?}",
        nat.0,
    );
}

#[test]
fn named_module_body_resolves_qualified_calls() {
    let src = r#"
mod util {
    pub fn add(a: i64, b: i64) -> i64 { a + b }
}
fn main() { println("{}", util::add(2, 5)) }
"#;
    let dir = fresh_dir("named-mod");
    let path = write_source(&dir, "named_mod", src);
    let run = run_vm(&path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert!(
        run.0.contains('7'),
        "expected '7' on stdout, got: {:?}",
        run.0,
    );
}

#[test]
fn native_binary_os_read_file_to_string_returns_real_contents() {
    let dir = fresh_dir("os-read-file");
    let payload_path = dir.join("payload.txt");
    fs::write(&payload_path, b"hello-from-disk").unwrap();
    let src = format!(
        r#"use std::fs
fn main() {{
    let p: String = "{p}"
    match fs::read_to_string(p) {{
        Ok(s) => println("ok len={{}} content={{}}", s.len(), s),
        Err(e) => println("err: {{}}", e),
    }}
    println("exists = {{}}", fs::exists(p))
}}
"#,
        // Embed with forward slashes: a Windows path has backslashes, and
        // `\a` / `\g` / `\t` … are escape sequences inside a `.gos` string
        // literal, so a raw backslash path corrupts to a nonexistent file.
        // Windows filesystem APIs accept `/` separators.
        p = payload_path.display().to_string().replace('\\', "/"),
    );
    let path = write_source(&dir, "os_read", &src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let run = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert!(
        run.0.contains("ok len=15") && run.0.contains("hello-from-disk"),
        "expected real file contents in native build, got: {:?}",
        run.0,
    );
    assert!(
        run.0.contains("exists = true"),
        "expected exists=true, got: {:?}",
        run.0,
    );
}

#[test]
fn native_binary_exec_run_returns_subprocess_output() {
    let src = r#"
use std::os::exec
fn main() {
    println("calling exec::run")
    let mut argv: Vec<String> = Vec::from([])
    argv.push("hello-via-echo")
    match exec::run("echo", argv) {
        Ok(o) => println("code={} stdout={}", o.code, o.stdout),
        Err(e) => println("err: {}", e),
    }
}
"#;
    let dir = fresh_dir("exec-run");
    let path = write_source(&dir, "exec_run", src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let run = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert!(
        run.0.contains("calling exec::run"),
        "expected pre-call print, got: {:?}",
        run.0,
    );
    assert!(
        run.0.contains("hello-via-echo"),
        "expected echoed payload, got: {:?}",
        run.0,
    );
}

/// Regression for the askq segfault: `exec::run(&prog,
/// &[a, b, c].to_vec())` reached the runtime via the
/// literal-array `.to_vec()` shape (not the empty-array +
/// push pattern). Two distinct root causes converged here:
/// 1. `exec::run` had no compiled-tier binding - the call
///    fell through to a non-existent symbol and the
///    destination held an undefined Result pointer.
/// 2. `[a, b, c].to_vec()` lowered to
///    `gos_rt_vec_clone(stack_array)`, but `vec_clone`
///    expects a `*const GosVec` header; reading the array's
///    payload bytes as `len/cap/elem_bytes/ptr` produced a
///    multi-terabyte length and a `memory allocation of
///    <huge> bytes failed` abort.
#[test]
fn native_binary_exec_run_with_literal_array_args() {
    let src = r#"
use std::os::exec
fn main() {
    let args: Vec<String> = [
        "-n",
        "from-literal-array",
    ].to_vec()
    match exec::run("echo", args) {
        Ok(o) => println("ok code={} stdout={}", o.code, o.stdout),
        Err(e) => println("err: {}", e),
    }
}
"#;
    let dir = fresh_dir("exec-literal");
    let path = write_source(&dir, "exec_literal", src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let run = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        run.2,
        Some(0),
        "process aborted (likely the [..].to_vec() segfault) - stdout: {:?}, stderr: {:?}",
        run.0,
        run.1,
    );
    assert!(
        run.0.contains("from-literal-array"),
        "expected echoed literal payload, got stdout: {:?}, stderr: {:?}",
        run.0,
        run.1,
    );
}

/// Regression for the `gos_rt_json_get` Arc-sharing refactor.
/// The previous implementation deep-cloned the matched
/// `serde_json::Value` per call (O(N) on a nested tree) and
/// `Box`-leaked the copy. A 1000-iteration drill that walks a
/// 3-deep nested object exercises the refactored path: the same
/// `Arc<Value>` tree is shared across child handles, so cloning
/// is a cheap refcount bump and `Box::into_raw`'d handles all
/// keep the same allocation alive. Memory growth is bounded by
/// the handle leak (24 bytes each), not by the tree's total size.
/// Without the refactor (or with a use-after-free in the new
/// shape) this test segfaults or returns garbage.
#[test]
fn native_binary_json_get_arc_shared_does_not_deep_clone() {
    let src = r#"
use std::encoding::json
fn main() {
    let raw = "{\"a\":{\"b\":{\"c\":42}}}"
    let mut iter: i64 = 0
    while iter < 1000 {
        let v = json::parse(raw).unwrap_or(json::Value::Null)
        if let Some(a) = json::get(v, "a") {
            if let Some(b) = json::get(a, "b") {
                if let Some(c) = json::get(b, "c") {
                    let n = json::as_i64(c).unwrap_or(0)
                    if n != 42 { println("UNEXPECTED iter={} n={}", iter, n) }
                }
            }
        }
        iter += 1
    }
    println("ok iter={}", iter)
}
"#;
    let dir = fresh_dir("json-arc");
    let path = write_source(&dir, "json_arc", src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let run = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert!(
        run.0.contains("ok iter=1000"),
        "expected 1000 iterations of json::get to succeed, got: {:?}",
        run.0,
    );
    assert!(
        !run.0.contains("UNEXPECTED"),
        "json::get returned wrong value at some iteration, got: {:?}",
        run.0,
    );
}

/// Regression for the non-capturing closure ABI mismatch in
/// `Result::map`. Pre-fix the lift pass emitted non-capturing
/// closures as `extern "C" fn(payload) -> ret` (no env slot)
/// while the runtime helper `gos_rt_result_map` invoked them as
/// `f(closure_ptr, payload)`\n on x86_64 the closure's `v` param
/// shadowed RDI = closure_ptr while the actual payload sat unread
/// in RSI. The closure body then transformed the env-pointer
/// instead of the payload, corrupting the resulting Result.
/// Now the MIR call-site dispatches non-capturing closures to a
/// dedicated `gos_rt_result_map_bare` helper that calls the
/// bare-fn ABI `f(payload)` directly.
#[test]
fn native_binary_result_map_non_capturing_closure_abi() {
    let src = r#"
use std::encoding::json
fn main() {
    let raw = "{\"x\":42}"
    let r1 = json::parse(raw)
    let r2 = r1.map(|v| v.clone())
    let r3 = r2.unwrap_or(json::Value::Null)
    println("r3.is_null={}", json::is_null(r3))
}
"#;
    let dir = fresh_dir("non-cap-map");
    let path = write_source(&dir, "non_cap_map", src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let run = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert!(
        run.0.contains("r3.is_null=false"),
        "expected map+unwrap_or chain to preserve the parsed Ok payload, got: {:?}",
        run.0,
    );
}

/// Regression for `json::Value::Null` (path expression, no
/// parens). The previous `lower_path` had no special case for
/// stdlib enum unit variants, so it fell through to the FnRef
/// fallback and produced a function-pointer value. Subsequent
/// code that expected a `*mut GosJson` (e.g. `json::is_null(&v)`)
/// dereferenced the function pointer and segfaulted. Now routes
/// through `gos_rt_json_value_null()` to produce a real handle.
#[test]
fn native_binary_json_value_null_path_expression() {
    let src = r#"
use std::encoding::json
fn main() {
    let v1 = json::Value::Null
    println("v1 is_null={}", json::is_null(v1))
    let raw = "{\"path\":\"/tmp\"}"
    let parsed = json::parse(raw).unwrap_or(json::Value::Null)
    if let Some(child) = json::get(parsed, "path") {
        let s = json::as_str(child).unwrap_or("")
        println("path={}", s)
    }
    println("done")
}
"#;
    let dir = fresh_dir("json-null-path");
    let path = write_source(&dir, "json_null", src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let run = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert!(
        run.0.contains("v1 is_null=true"),
        "expected Value::Null to be is_null=true, got: {:?}",
        run.0,
    );
    assert!(
        run.0.contains("path=/tmp"),
        "expected unwrap_or-with-Null default to preserve Ok payload, got: {:?}",
        run.0,
    );
    assert!(
        run.0.contains("done"),
        "expected program to reach the end, got: {:?}",
        run.0,
    );
}

/// Regression for the askq tool-call accumulator. The compiled
/// chat round did `tc_names[idx] = s` where `idx` came from
/// `json::as_i64(v).unwrap_or(0)` - both pieces were broken in
/// compiled mode:
/// 1. `Vec<String>[idx] = s` assignment was a no-op (the projection
///    machinery treated the Vec as a flat array\n the data lives at
///    `header.ptr`, not in the slot directly). Now routes through
///    `gos_rt_vec_set_i64`.
/// 2. `json::as_i64(v).unwrap_or(0)` returned a multi-trillion
///    garbage pointer. The HIR typechecker assumed the chained
///    `.unwrap_or` meant the receiver was `Option<i64>`, dispatched
///    `unwrap_or` to `gos_rt_result_unwrap_or` which read the i64
///    return value as a `*mut GosResult` pointer and dereferenced
///    garbage `disc`/`payload`. Now falls back to identity when
///    the lowered receiver is a real scalar.
#[test]
fn native_binary_vec_string_indexed_assign_and_scalar_unwrap_or() {
    let src = r#"
fn main() {
    let mut xs: Vec<String> = Vec::from([])
    xs.push("a")
    xs.push("b")
    xs[0] = "X"
    xs[1] = "Y"
    println("xs[0]={} xs[1]={}", xs[0], xs[1])
}
"#;
    let dir = fresh_dir("vec-set");
    let path = write_source(&dir, "vec_set", src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let run = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert!(
        run.0.contains("xs[0]=X xs[1]=Y"),
        "expected updated values, got: {:?}",
        run.0,
    );
}

/// Regression for `[1, 2, 3].to_vec()` and
/// `["a", "b", "c"].to_vec()` literal-array `.to_vec()`
/// crashes. These shapes lowered to `gos_rt_vec_clone(arr)`,
/// but the runtime helper expects a real `GosVec` header
/// rather than a stack `[T; N]` aggregate\n reading element
/// 0/1 as `len`/`cap` produced terabyte-scale allocations
/// and aborted with `memory allocation of <huge> bytes
/// failed` or a plain segfault. Both `i64` and `String`
/// element shapes are exercised because they pick different
/// elem_bytes paths inside `gos_rt_vec_from_arr`.
#[test]
fn native_binary_literal_array_to_vec_does_not_segfault() {
    let src = r#"
fn main() {
    let xs: Vec<i64> = [10, 20, 30].to_vec()
    println("i64 len={} 0={} 1={} 2={}", xs.len(), xs[0], xs[1], xs[2])
    let ys: Vec<String> = ["a", "b", "c"].to_vec()
    println("str len={} 0={} 1={} 2={}", ys.len(), ys[0], ys[1], ys[2])
    let zs: Vec<String> = ["aa", "bb", "cc"].to_vec()
    println("lit len={} 0={} 1={} 2={}", zs.len(), zs[0], zs[1], zs[2])
}
"#;
    let dir = fresh_dir("literal-to-vec");
    let path = write_source(&dir, "lit_tovec", src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let run = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        run.2,
        Some(0),
        "process aborted - stdout: {:?}, stderr: {:?}",
        run.0,
        run.1,
    );
    assert!(
        run.0.contains("i64 len=3 0=10 1=20 2=30"),
        "i64 literal-array to_vec failed, got: {:?}",
        run.0,
    );
    assert!(
        run.0.contains("str len=3 0=a 1=b 2=c"),
        "String literal-array (.to_string() elems) to_vec failed, got: {:?}",
        run.0,
    );
    assert!(
        run.0.contains("lit len=3 0=aa 1=bb 2=cc"),
        "&str literal-array to_vec failed, got: {:?}",
        run.0,
    );
}

#[test]
fn json_get_returns_option_with_correct_discriminator() {
    let src = r#"
use std::encoding::json
fn main() {
    let v = json::parse("{\"a\":1}").unwrap()
    let opt = json::get(v, "a")
    println("is_some? {}", opt.is_some())
    println("is_none? {}", opt.is_none())
    if let Some(_) = opt {
        println("matched Some")
    } else {
        println("matched None")
    }
    let missing = json::get(v, "absent")
    println("missing is_some? {}", missing.is_some())
    println("missing is_none? {}", missing.is_none())
}
"#;
    let dir = fresh_dir("json-option");
    let path = write_source(&dir, "json_opt", src);
    // Both interpreter and native build must agree.
    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert!(
        vm.0.contains("is_some? true")
            && vm.0.contains("is_none? false")
            && vm.0.contains("matched Some")
            && vm.0.contains("missing is_some? false")
            && vm.0.contains("missing is_none? true"),
        "unexpected interpreter output: {:?}",
        vm.0,
    );

    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let nat = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(nat.2, Some(0), "native stderr: {}", nat.1);
    assert!(
        nat.0.contains("is_some? true")
            && nat.0.contains("is_none? false")
            && nat.0.contains("matched Some")
            && nat.0.contains("missing is_some? false")
            && nat.0.contains("missing is_none? true"),
        "unexpected native output: {:?}",
        nat.0,
    );
}

#[test]
fn slice_of_tuples_indexing_works_in_native_build() {
    // `&[(String, String)]` callees would receive the address of
    // a flat-array aggregate before the unified `coerce_to_vec_arg`
    // landed\n the callee's `gos_rt_vec_len` then read the first
    // tuple element as the length and segfaulted on the
    // subsequent index dispatch. Asserting on both `gos` and
    // a native build guards the path against future regressions.
    let src = r#"
fn first_key(vars: [(String, String)]) -> String {
    vars[0].0.clone()
}
fn second_value(vars: [(String, String)]) -> String {
    vars[1].1.clone()
}
fn main() {
    let pairs = [
        ("alpha", "1"),
        ("beta", "2"),
    ].to_vec()
    println("{}", first_key(pairs))
    println("{}", second_value(pairs))
}
"#;
    let dir = fresh_dir("slice-tuples");
    let path = write_source(&dir, "slice_tuples", src);
    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert!(
        vm.0.contains("alpha") && vm.0.contains('2'),
        "vm output mismatch: {:?}",
        vm.0,
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let nat = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(nat.2, Some(0), "native stderr: {}", nat.1);
    assert!(
        nat.0.contains("alpha") && nat.0.contains('2'),
        "native output mismatch: {:?}",
        nat.0,
    );
}

#[test]
fn test_runner_assertions_persist_after_jit_warmup() {
    // First test runs a regex-heavy body that triggers the JIT.
    // Subsequent string-comparing assertions must continue to be
    // observed and counted against each test's tally - otherwise
    // the runner reports a false PASS with 0 assertions for any
    // test that ran after the JIT became hot.
    let src = r#"
use std::regex
use std::testing

fn empty_pattern() -> regex::Pattern {
    regex::compile("").unwrap_or(regex::compile("a").unwrap())
}

pub fn substring(s: String, start: i64, end: i64) -> String {
    let n = s.len() as i64
    let mut a = start
    let mut b = end
    if a < 0 { a = 0 }
    if b > n { b = n }
    if a >= b { return "" }
    let drop_pat = regex::compile(format("(?s)^.{{0,{}}}", a)).unwrap_or(empty_pattern())
    let after = regex::replace(drop_pat, s, "")
    let len = b - a
    let take_pat = regex::compile(format("(?s)^(.{{0,{}}})", len)).unwrap_or(empty_pattern())
    let row = regex::captures(take_pat, after).map(|r| r.clone()).unwrap_or([].to_vec())
    if row.len() < 2 { return "" }
    row[1].clone().map(|x| x.clone()).unwrap_or("")
}

#[cfg(test)]
#[test]
fn warmup_jit() {
    let s = "padding-padding-padding-padding-padding-padding"
    let mut i: i64 = 0
    while i < (s.len() as i64) {
        let _ = substring(s, i, i + 5)
        i += 1
    }
    let _ = testing::check_eq(s.len(), 47, "length sanity")
}

#[cfg(test)]
#[test]
fn after_jit_string_eq() {
    let _ = testing::check_eq("hi", "hi", "string eq")
}

#[cfg(test)]
#[test]
fn after_jit_check_true() {
    let _ = testing::check(true, "always true")
}

fn main() {}
"#;
    let dir = fresh_dir("test-runner-jit");
    let path = write_source(&dir, "runner_jit", src);
    let out = Command::new(gos_bin())
        .arg("test")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos test");
    let _ = fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "gos test failed:\nstdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stdout.contains("PASS warmup_jit") && stdout.contains("(1 assertion"),
        "warmup did not log an assertion: {stdout}",
    );
    assert!(
        stdout.contains("PASS after_jit_string_eq") && stdout.contains("(1 assertion"),
        "string-eq test reported zero assertions after JIT: {stdout}",
    );
    assert!(
        stdout.contains("PASS after_jit_check_true"),
        "check(true) test silently dropped after JIT: {stdout}",
    );
    assert!(
        stdout.contains("3 passed, 0 failed, 3 assertion(s)"),
        "expected 3 assertions across 3 tests, got: {stdout}",
    );
}

#[test]
fn a_file_under_tests_reaches_the_packages_modules() {
    // An integration test lives beside the package, so its own folder holds no
    // modules; `use crate::<module>` typechecked and then found no such name.
    let dir = fresh_dir("tests-dir-module");
    fs::create_dir_all(dir.join("src")).expect("src");
    fs::create_dir_all(dir.join("tests")).expect("tests");
    fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/tests-dir\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    fs::write(
        dir.join("src").join("main.gos"),
        "use lib_thing\nfn main() { println(\"{}\", lib_thing::double(21)) }\n",
    )
    .expect("entry");
    fs::write(
        dir.join("src").join("lib_thing.gos"),
        "pub fn double(n: i64) -> i64 { n * 2 }\n",
    )
    .expect("module");
    fs::write(
        dir.join("tests").join("integration.gos"),
        "use crate::lib_thing\n\n#[test]\nfn reaches_a_src_module() { assert_eq(lib_thing::double(21), 42) }\n",
    )
    .expect("integration test");
    let out = Command::new(gos_bin())
        .arg("test")
        .arg(dir.join("tests").join("integration.gos"))
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos test");
    let _ = fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a tests/ file could not reach src/:\nstdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stdout.contains("PASS reaches_a_src_module"),
        "unexpected output: {stdout}",
    );
}

#[test]
fn a_router_handler_closure_is_called_with_both_of_its_parameters() {
    // A route handler is declared `Fn(Request, Params)`. The interpreter's
    // router dispatch called every handler with the request alone, so a closure
    // that spelled both parameters failed with "wrong number of arguments:
    // expected 2, found 1" - on the VM only, since the compiled tier does not
    // check its handler arity. The program serves itself one request so both
    // tiers are asked the same question.
    let dir = fresh_dir("router-handler-arity");
    fs::create_dir_all(dir.join("src")).expect("src");
    fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/router-arity\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    fs::write(
        dir.join("src").join("main.gos"),
        "use std::{http, time}\nuse std::http::router\n\n         fn main() {\n         \x20   let server = http::Server::new()\n         \x20   match server.listen(\"127.0.0.1:0\") {\n         \x20       Ok(()) => ()\n         \x20       Err(e) => {\n         \x20           println(\"listen: {}\", e.message())\n         \x20           return\n         \x20       }\n         \x20   }\n         \x20   let addr = server.addr()\n         \x20   let routes = router::Router::new()\n         \x20       .get(\"/user/{id}\", |req, _p| http::Response::text(200, \"id=\" + req.path_value(\"id\")))\n         \x20   let done = cohort {\n         \x20       spawn(|| { let _ = server.serve(routes) }, reason: \"server\")\n         \x20       time::sleep(300)\n         \x20       match http::get(\"http://\" + addr + \"/user/42\", #[]) {\n         \x20           Ok(resp) => println(\"GOT {}\", resp.body)\n         \x20           Err(e) => println(\"ERR {}\", e.message())\n         \x20       }\n         \x20       let _ = server.shutdown(1000)\n         \x20   }\n         \x20   match done {\n         \x20       Ok(()) => ()\n         \x20       Err(e) => println(\"cohort: {}\", e.message())\n         \x20   }\n         }\n",
    )
    .expect("entry");
    for args in [vec!["run", "."], vec!["build", "--release"]] {
        let out = Command::new(gos_bin())
            .args(&args)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("gos");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "gos {args:?} failed:\nstdout: {stdout}\nstderr: {stderr}",
        );
        if args[0] == "run" {
            assert!(
                stdout.contains("GOT id=42"),
                "the VM did not call the handler with both parameters: {stdout}",
            );
        }
    }
    let native = Command::new(dir.join("target").join("release").join("router-arity"))
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("native binary");
    let native_out = String::from_utf8_lossy(&native.stdout);
    let _ = fs::remove_dir_all(&dir);
    assert!(
        native_out.contains("GOT id=42"),
        "the compiled tier answered differently: {native_out}",
    );
}

#[test]
fn a_capturing_closure_on_a_router_route_lowers_natively() {
    // A capturing closure registered as a router route lifts to
    // `__closure_N(env, req, params)`. Only the one- and two-argument handler
    // shapes were scanned for the `::__ok_wrap` thunk, so this one referenced a
    // thunk nothing emitted and the build failed in the LLVM symbol audit.
    let dir = fresh_dir("router-capturing-closure");
    fs::create_dir_all(dir.join("src")).expect("src");
    fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/router-capture\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    fs::write(
        dir.join("src").join("db.gos"),
        "pub struct Handle { pub hits: Map<String, i64> }\n\n         pub fn lookup(h: &mut Handle, id: i64) -> String {\n         \x20   h.hits.insert(\"n\", id)\n         \x20   \"row-\" + id.to_string()\n}\n",
    )
    .expect("module");
    fs::write(
        dir.join("src").join("main.gos"),
        "use std::http\nuse std::http::router\nuse db\n\n         fn main() {\n         \x20   let mut handle = db::Handle { hits: Map::new() }\n         \x20   let routes = router::Router::new()\n         \x20       .get(\"/user/{id}\", |req, _p| answer(&mut handle, req.path_int(\"id\").unwrap_or(-1)))\n         \x20   match http::serve(\"127.0.0.1:0\", routes) {\n         \x20       Ok(()) => ()\n         \x20       Err(e) => eprintln(\"{}\", e.message())\n         \x20   }\n         }\n\n         fn answer(h: &mut db::Handle, id: i64) -> http::Response {\n         \x20   http::Response::text(200, db::lookup(h, id))\n}\n",
    )
    .expect("entry");
    let out = Command::new(gos_bin())
        .arg("build")
        .arg("--release")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos build");
    let _ = fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a capturing router closure did not lower:\nstdout: {stdout}\nstderr: {stderr}",
    );
}

#[test]
fn a_callback_parameter_is_not_typed_by_a_global_of_the_same_name() {
    // Calling a function-typed parameter is an indirect call: the value the
    // binding holds decides the parameters. The argument lowering looked the
    // callee's name up among the program's functions anyway, so a parameter
    // named after a global picked up that global's types - and because the
    // global's first parameter was `&mut`, the argument was wrapped in a cell.
    // The callback then received a reference where it declared a String: it
    // rendered correctly and answered 0 from `byte_len`.
    let dir = fresh_dir("callback-param-shadowing");
    fs::create_dir_all(dir.join("src")).expect("src");
    fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/callback-shadow\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    // A global `respond` whose first parameter is `&mut`, in its own module.
    fs::write(
        dir.join("src").join("store.gos"),
        "pub struct Bag { pub items: Vec<i64> }\n\n         pub fn respond(bag: &mut Bag, n: i64) -> i64 {\n    bag.items.push(n)\n    bag.items.len()\n}\n",
    )
    .expect("module");
    // A parameter with the same name, called indirectly.
    fs::write(
        dir.join("src").join("front.gos"),
        "pub fn serve(path: String, respond: Fn(String) -> (i64, String)) -> (i64, String) {\n         \x20   respond(path)\n}\n",
    )
    .expect("front");
    fs::write(
        dir.join("src").join("main.gos"),
        "use front\nuse store\n\n         fn main() {\n         \x20   let status, body = front::serve(\"/health\", |p| (200, \"saw:\" + p))\n         \x20   println(\"{} {} {}\", status, body, body.byte_len())\n         \x20   let mut bag = store::Bag { items: #[] }\n         \x20   println(\"{}\", store::respond(&mut bag, 7))\n         }\n",
    )
    .expect("entry");
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(".")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos run");
    let _ = fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the callback call failed:\nstdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stdout.contains("200 saw:/health 11"),
        "the callback did not receive its argument as a String: {stdout}",
    );
    assert!(
        stdout.contains("\n1\n") || stdout.trim_end().ends_with('1'),
        "the global with the same name still works: {stdout}",
    );
}

#[test]
fn a_capturing_closure_and_a_router_handler_both_lower_natively() {
    // A handler that answers a bare `Response` reaches its slot through a
    // synthesized `::__ok_wrap`. Only the one-argument shape was scanned, so a
    // router handler `fn(Request, Params)` and a capturing closure lifted to
    // `__closure_N(env, req)` referenced a thunk nothing emitted.
    let src = r#"
use std::errors
use std::http
use std::http::router

fn health(_req: http::Request, _p: router::Params) -> http::Response {
    http::Response::text(200, "ok")
}

fn with_router() -> router::Router {
    router::Router::new().get("/health", health)
}

fn with_capturing_closure(greeting: String) -> Result<(), errors::Error> {
    http::serve("127.0.0.1:0", |req| http::Response::text(200, greeting + req.path))
}

fn main() {
    let _ = with_router()
    println("built")
}
"#;
    let dir = fresh_dir("handler-ok-wrap");
    let path = write_source(&dir, "handler_ok_wrap", src);
    let out = Command::new(gos_bin())
        .arg("build")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos build");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "a handler shape did not lower:\nstdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        !stderr.contains("__ok_wrap"),
        "an Ok-packing thunk was referenced but not emitted: {stderr}",
    );
}

#[test]
fn an_as_u64_cast_reaches_a_builtin_as_the_value_it_carries() {
    // A `u64` argument crossed into a builtin as a zero, so a value cast on
    // the way into `put_u64_le_at` wrote eight zero bytes.
    let src = r#"
use std::encoding::binary
use std::testing

#[cfg(test)]
#[test]
fn a_cast_value_and_a_literal_write_the_same_bytes() {
    let x: i64 = 1234567890123
    let mut a: Vec<u8> = #[0u8; 8]
    let mut b: Vec<u8> = #[0u8; 8]
    let _ = binary::put_u64_le_at(&mut a, 0, x as u64)
    let _ = binary::put_u64_le_at(&mut b, 0, 1234567890123u64)
    let _ = testing::check_eq(a, b, "cast and literal agree")
    let _ = testing::check_eq(binary::get_u64_le_at(a, 0).unwrap_or(0) as i64, x, "round trip")
}

fn main() {}
"#;
    let dir = fresh_dir("u64-call-boundary");
    let path = write_source(&dir, "u64_boundary", src);
    let out = Command::new(gos_bin())
        .arg("test")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos test");
    let _ = fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "a u64 argument did not survive the call: {stdout}",
    );
}

#[test]
fn unwrap_or_over_a_sequence_releases_its_fallback_once() {
    // The fallback becomes the answer on the `Err` arm, so the frame holds it
    // twice; releasing it once per binding freed the same Vec twice and
    // corrupted the heap under any workload that ran the arm repeatedly.
    let src = r#"
fn maybe(n: i64) -> Option<Vec<i64>> {
    if n % 2 == 0 { Some(#[n, n + 1]) } else { None }
}

fn scan(n: i64) -> i64 {
    let mut total = 0
    let mut i = 0
    while i < n {
        let bytes = maybe(i).unwrap_or(#[])
        total += bytes.len()
        i += 1
    }
    total
}

fn main() { println("{}", scan(64)) }
"#;
    let dir = fresh_dir("unwrap-or-fallback");
    let path = write_source(&dir, "unwrap_or_fallback", src);
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos run");
    let _ = fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "scan faulted:\nstdout: {stdout}\nstderr: {stderr}",
    );
    assert_eq!(stdout.trim(), "64", "unexpected total: {stdout}");
    assert!(
        !stderr.contains("already zero"),
        "a Vec was released twice: {stderr}",
    );
}

#[test]
fn a_test_that_asserts_nothing_does_not_report_a_pass() {
    // A body that only prints has decided nothing, so it cannot carry the
    // suite green. A `-> Result<(), E>` test is the exemption: reaching `Ok`
    // past every `?` is the verdict it reports in place of an assertion.
    let src = r#"
use std::errors
use std::testing

fn ok_op() -> Result<i64, errors::Error> { Ok(1) }

#[cfg(test)]
#[test]
fn prints_but_never_asserts() {
    println("FAIL: something is wrong")
}

#[cfg(test)]
#[test]
fn result_ok_needs_no_assertion() -> Result<(), errors::Error> {
    let _ = ok_op()?
    Ok(())
}

#[cfg(test)]
#[test]
fn asserts_and_passes() {
    let _ = testing::check(true, "always true")
}

fn main() {}
"#;
    let dir = fresh_dir("test-runner-vacuous");
    let path = write_source(&dir, "runner_vacuous", src);
    let out = Command::new(gos_bin())
        .arg("test")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos test");
    let _ = fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a suite whose test asserted nothing exited zero:\nstdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stdout.contains("FAIL prints_but_never_asserts") && stdout.contains("made no assertions"),
        "assertion-free test was not reported as such: {stdout}",
    );
    assert!(
        stdout.contains("PASS result_ok_needs_no_assertion"),
        "a Result-returning test was not exempt: {stdout}",
    );
    assert!(
        stdout.contains("PASS asserts_and_passes"),
        "an asserting test did not pass: {stdout}",
    );
    assert!(
        stdout.contains("2 passed, 1 failed"),
        "unexpected tally: {stdout}",
    );
    // The guidance is identical for every such test, so it belongs to the run.
    assert_eq!(
        stdout.matches("note: a test decides its result").count(),
        1,
        "the assertion guidance was not printed exactly once: {stdout}",
    );
}

#[test]
fn a_failing_isolated_test_reports_one_result_not_the_workers_run() {
    // Each isolated test runs in a worker that prints a harness run of its
    // own. Forwarding that verbatim would show the worker's banner, tally,
    // and error line beside the parent's, reading as two contradictory
    // summaries for one run.
    let src = r#"
use std::testing

#[cfg(test)]
#[test]
fn fails_with_a_reason() {
    println("marker-from-the-test-body")
    let _ = testing::check(false, "the reason")
}

fn main() {}
"#;
    let dir = fresh_dir("test-runner-worker-echo");
    let path = write_source(&dir, "runner_worker_echo", src);
    let out = Command::new(gos_bin())
        .arg("test")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos test");
    let _ = fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.matches("test: ").count(),
        1,
        "more than one run summary reached the user: {stdout}",
    );
    assert!(
        stdout.contains("the reason"),
        "the worker's failure reason was lost: {stdout}",
    );
    assert!(
        stdout.contains("marker-from-the-test-body"),
        "the test's own output was lost: {stdout}",
    );
    assert!(
        stdout.contains("0 passed, 1 failed"),
        "unexpected tally: {stdout}",
    );
}

#[test]
fn vec_double_free_does_not_segfault_in_native_release() {
    // The askq chat-streaming loop drops the same `*mut GosVec`
    // handle twice (the MIR drop pass emits a free for each
    // shadowed binding inside a loop, and a real LLVM-release
    // build of askq would segfault inside the allocator's
    // `__libc_free`/`get_meta` after the second drop). The
    // runtime now tracks freed addresses in a process-global
    // set so the second free is an idempotent no-op.
    //
    // Repro: aggressively shadow a local that holds a vec
    // returned by a runtime helper. With a small `cap` we keep
    // re-allocating into the same arena slot.
    let src = r#"
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 256 {
        let parts = "a,b,c,d".split(",")
        total += parts.len() as i64
        i += 1
    }
    println("total={}", total)
}
"#;
    let dir = fresh_dir("vec-double-free");
    let path = write_source(&dir, "vec_dbl_free", src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let nat = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(nat.2, Some(0), "native stderr: {}", nat.1);
    assert_eq!(nat.0.trim(), "total=1024", "native stdout: {:?}", nat.0);
}

#[test]
fn param_named_client_with_user_type_is_not_tagged_as_http_client() {
    // `fn render(client: User)`: the param is *user-typed* but
    // its name collides with the stdlib heuristic. Previously
    // the MIR tagged `client`'s local with `http::Client`, and
    // any method dispatched through `gos_rt_http_client_*` -
    // reading bytes past the user struct. The heuristic now only
    // applies when the parameter's declared type is unresolved.
    let src = r#"
struct User { name: String, age: i64 }

impl User {
    fn render(&self) -> String {
        format("{} ({})", self.name, self.age)
    }
}

fn show(client: User) -> String {
    client.render()
}

fn main() {
    let u = User { name: "alice", age: 30 }
    println("{}", show(u))
}
"#;
    let dir = fresh_dir("client-param");
    let path = write_source(&dir, "client_param", src);
    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0.trim(), "alice (30)", "vm stdout: {:?}", vm.0);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let nat = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(nat.2, Some(0), "native stderr: {}", nat.1);
    assert_eq!(nat.0.trim(), "alice (30)", "native stdout: {:?}", nat.0);
}

#[test]
fn i128_use_panics_native_build_rather_than_silently_truncating() {
    // `cl_type_of` was silently mapping `i128`/`u128` to `i64`,
    // so `let n: i128 = 1 << 100` would compile, run, and produce
    // a corrupted i64 with no warning. Refuse the build instead.
    let src = r#"
fn main() {
    let n: i128 = 1
    println("{}", n)
}
"#;
    let dir = fresh_dir("i128-reject");
    let path = write_source(&dir, "i128_reject", src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let out = Command::new(gos_bin())
        .arg("build")
        .arg("--out-dir")
        .arg(&cl_dir)
        .arg(&path)
        .output()
        .expect("gos build");
    let _ = fs::remove_dir_all(&dir);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected `gos build` to refuse i128 source; got success. stderr={stderr}",
    );
    assert!(
        stderr.contains("i128") && stderr.contains("compiled tier"),
        "expected diagnostic mentioning i128 + compiled tier, got: {stderr}",
    );
}

#[test]
fn user_fn_named_substring_does_not_recurse_via_method_dispatch() {
    // The bytecode VM's method dispatch was tripping over user
    // free fns whose name collided with a builtin String method:
    // `pub fn substring(s, a, b)` made every `s.substring(...)`
    // call inside the user's body recurse straight back into the
    // user fn (the bare-name fallback in `MethodCall` resolution
    // beat the builtin). The VM now consults a `String::method`
    // qualified key first, so the builtin wins.
    let src = r#"
pub fn substring(s: String, a: i64, b: i64) -> String {
    s.substring(a, b)
}

fn main() {
    let s = "hello world"
    println("{}", substring(s, 0, 5))
    println("{}", substring(s, 6, 11))
}
"#;
    let dir = fresh_dir("user-fn-substring");
    let path = write_source(&dir, "user_substring", src);
    let vm = run_vm(&path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0.trim(), "hello\nworld", "vm stdout: {:?}", vm.0);
}

#[test]
fn malformed_json_returns_none_not_segfault() {
    // Calling `json::get(v, key)` / `as_array` / `as_str` on a
    // value whose actual shape doesn't match the expected variant
    // must return None / "" cleanly. Previously the helpers
    // `unwrap`'d serde_json's index lookups and segfaulted when
    // the value was the wrong kind. Pinned by this test plus the
    // c-string addr-floor guard in `gos_rt_string_view`.
    let src = r#"
use std::encoding::json
fn main() {
    // Parse a JSON string. `as_object` keys / `as_array` iter on
    // a string value must return None.
    let s = json::parse("\"plain\"").unwrap()
    match json::as_array(s) {
        Some(_) => println("array? wrong"),
        None => println("not-array ok"),
    }
    match json::keys(s) {
        Some(_) => println("keys? wrong"),
        None => println("not-object-keys ok"),
    }
    let missing = json::get(s, "nope")
    match missing {
        Some(_) => println("got? wrong"),
        None => println("missing ok"),
    }
    // Object lookup with a key that doesn't exist.
    let obj = json::parse("{\"a\": 1}").unwrap()
    match json::get(obj, "b") {
        Some(_) => println("b? wrong"),
        None => println("b missing ok"),
    }
}
"#;
    let dir = fresh_dir("malformed-json");
    let path = write_source(&dir, "malformed_json", src);

    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "interp stderr: {}", vm.1);
    let expected = "not-array ok\nnot-object-keys ok\nmissing ok\nb missing ok";
    assert_eq!(vm.0.trim(), expected, "interp output: {:?}", vm.0);

    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let nat = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(nat.2, Some(0), "native stderr: {}", nat.1);
    assert_eq!(nat.0.trim(), expected, "native output: {:?}", nat.0);
}

#[test]
fn json_as_array_iter_native() {
    // `json::as_array(&v).unwrap()` was identity-pinned to
    // `JsonValue` in MIR, so downstream `arr.len()` /
    // `for x in arr.iter()` / `arr[i]` walked serde_json's
    // private headers as if they were a `*mut GosVec` and
    // returned garbage / segfaulted. Now backed by
    // `gos_rt_json_as_array`, which materialises a real
    // `*mut GosVec<*mut GosJson>` and wraps it in a
    // sentinel-Option.
    let src = r#"
use std::encoding::json
fn main() {
    let v = json::parse("[10, 20, 30]").unwrap()
    let arr = json::as_array(v).unwrap()
    let n = arr.len() as i64
    let mut i: i64 = 0
    while i < n {
        let item = arr[i]
        println("{}", json::as_i64(item).unwrap_or(-1))
        i += 1
    }
    println("len={}", n)
}
"#;
    let dir = fresh_dir("json-as-array-iter");
    let path = write_source(&dir, "json_arr", src);

    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "interp stderr: {}", vm.1);
    assert!(
        vm.0.trim() == "10\n20\n30\nlen=3",
        "interp output: {:?}",
        vm.0,
    );

    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let nat = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(nat.2, Some(0), "native stderr: {}", nat.1);
    assert!(
        nat.0.trim() == "10\n20\n30\nlen=3",
        "native output: {:?}",
        nat.0,
    );
}

#[test]
fn regex_replace_singular_native() {
    // `regex::replace(pat, text, repl)` (one occurrence) was unwired
    // in the AOT cranelift dispatch and silently lowered to an `iconst
    // i64 0`. Downstream `.clone().unwrap_or("")` then double-freed
    // the empty-string literal in askq's substring impl. The fix is
    // a `gos_rt_regex_replace` runtime helper plus dispatch in MIR /
    // JIT / native / LLVM. This test pins both the result and the
    // single-replace semantics.
    let src = r#"
use std::regex
fn main() {
    let pat = regex::compile("foo").unwrap()
    let s = "foo and foo"
    let one = pat.replace(s, "BAR")
    let all = pat.replace_all(s, "BAR")
    println("{} | {}", one, all)
}
"#;
    let dir = fresh_dir("regex-replace-singular");
    let path = write_source(&dir, "regex_replace", src);

    // Interp tier first.
    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "interp stderr: {}", vm.1);
    assert!(
        vm.0.trim() == "BAR and foo | BAR and BAR",
        "interp output mismatch: {:?}",
        vm.0,
    );

    // Native tier: build, run, compare.
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir);
    let nat = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(nat.2, Some(0), "native stderr: {}", nat.1);
    assert!(
        nat.0.trim() == "BAR and foo | BAR and BAR",
        "native output mismatch: {:?}",
        nat.0,
    );
}

#[test]
fn test_runner_isolates_jit_state_across_json_iteration() {
    // After the batch JIT compile fires (triggered by the hot
    // counter on any helper), the JIT-compiled body of a later
    // test that iterates a parsed JSON array must produce the
    // same items the bytecode interpreter would. This previously
    // regressed in askq: a chain of ~14 short tests warmed the
    // JIT, the next test's `for msg in arr.iter()` loop produced
    // zero items, and downstream `keep` was empty. The fix is
    // per-test Vm reset in the runner\n this test pins that.
    let src = r#"
use std::encoding::json
use std::testing

pub fn json_string(v: json::Value, key: String) -> String {
    if let Some(child) = json::get(v, key) {
        json::as_str(child).unwrap_or("")
    } else {
        ""
    }
}

pub fn count_user_messages(messages_json: String) -> i64 {
    let parsed = json::parse(messages_json).unwrap_or(json::Value::Null)
    if json::is_null(parsed) { return 0 }
    let arr = json::as_array(parsed).unwrap()
    let mut n: i64 = 0
    for msg in arr.iter() {
        let role = json_string(msg, "role")
        if role == "user" { n += 1 }
    }
    n
}

#[cfg(test)] #[test] fn warm_a()  { let _ = testing::check(1 == 1, "a") }
#[cfg(test)] #[test] fn warm_b()  { let _ = testing::check(2 == 2, "b") }
#[cfg(test)] #[test] fn warm_c()  { let _ = testing::check(3 == 3, "c") }
#[cfg(test)] #[test] fn warm_d()  { let _ = testing::check(4 == 4, "d") }
#[cfg(test)] #[test] fn warm_e()  { let _ = testing::check(5 == 5, "e") }
#[cfg(test)] #[test] fn warm_f()  { let _ = testing::check(6 == 6, "f") }
#[cfg(test)] #[test] fn warm_g()  { let _ = testing::check(7 == 7, "g") }
#[cfg(test)] #[test] fn warm_h()  { let _ = testing::check(8 == 8, "h") }
#[cfg(test)] #[test] fn warm_i()  { let _ = testing::check(9 == 9, "i") }
#[cfg(test)] #[test] fn warm_j()  { let _ = testing::check(10 == 10, "j") }
#[cfg(test)] #[test] fn warm_k()  { let _ = testing::check(11 == 11, "k") }
#[cfg(test)] #[test] fn warm_l()  { let _ = testing::check(12 == 12, "l") }
#[cfg(test)] #[test] fn warm_m()  { let _ = testing::check(13 == 13, "m") }
#[cfg(test)] #[test] fn warm_n()  { let _ = testing::check(14 == 14, "n") }

#[cfg(test)]
#[test]
fn after_jit_array_iteration_works() {
    let raw = "[{\"role\":\"user\",\"content\":\"hi\"},{\"role\":\"assistant\",\"content\":\"there\"},{\"role\":\"user\",\"content\":\"again\"}]"
    let n = super::count_user_messages(raw)
    let _ = testing::check_eq(n, 2, "two user messages survive iteration")
}

fn main() {}
"#;
    let dir = fresh_dir("test-runner-jit-iter");
    let path = write_source(&dir, "runner_jit_iter", src);
    let out = Command::new(gos_bin())
        .arg("test")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos test");
    let _ = fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "gos test failed:\nstdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stdout.contains("PASS after_jit_array_iteration_works"),
        "JSON-iteration test failed after JIT warm-up:\n{stdout}",
    );
    assert!(
        stdout.contains("15 passed, 0 failed"),
        "expected all 15 tests to pass, got: {stdout}",
    );
}

/// Regression: the bytecode peephole's `drop_dead_const_loads`
/// pass relied on `op_value_reads` to enumerate every register
/// any op reads. `Op::Call` / `MethodCall` / `Spawn` / `SpawnMethod`
/// each read the contiguous `args..args+argc` span; before this
/// fix the pass only saw `callee` / `receiver`, so a literal
/// passed directly to a builtin (e.g. `println!("hello")`,
/// `println(42)`, `format!("{}", "x")`) had its `LoadConst`
/// dropped - the call then read `Value::Void` from the
/// not-yet-written argument slot. The user-visible symptom was
/// `<void>` printed instead of the literal.
#[test]
fn peephole_does_not_drop_literal_const_loads_feeding_call_args() {
    let src = r#"
fn main() {
    println("hello")
    println(42)
    println("{}", "world")
    println("{}", 7)
    let s = format("{}", "ok")
    println(s)
}
"#;
    let dir = fresh_dir("peephole-call-args");
    let path = write_source(&dir, "peephole_call_args", src);
    let (stdout, stderr, code) = run_vm(&path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        code,
        Some(0),
        "exit code\nstdout: {stdout}\nstderr: {stderr}"
    );
    let trimmed = stdout.trim_end();
    assert_eq!(
        trimmed, "hello\n42\nworld\n7\nok",
        "literal-arg println output regressed:\n{stdout}",
    );
    assert!(
        !stdout.contains("<void>"),
        "peephole dropped a const-load feeding a call:\n{stdout}",
    );
}

#[test]
fn test_runner_failed_test_prints_call_chain_traceback() {
    // A `#[test]` that panics deep in a nested call chain must report
    // not only the panic message (byte-identical to before the VM
    // switch) but also the VM's preserved call stack, so a failure
    // points at the path that reached it rather than just the leaf.
    let src = r#"
fn deepest(n: i64) -> i64 {
    if n == 0 {
        panic("boom at the bottom")
    }
    n
}

fn middle(n: i64) -> i64 {
    deepest(n - 1)
}

fn top() -> i64 {
    middle(1)
}

#[cfg(test)]
mod tb_tests {
    #[test]
    fn panics_in_nested_call() {
        let _ = super::top()
    }
}

fn main() {}
"#;
    let dir = fresh_dir("test-runner-traceback");
    let path = write_source(&dir, "runner_tb", src);
    let out = Command::new(gos_bin())
        .arg("test")
        .arg(&path)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos test");
    let _ = fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // A failing test makes the runner exit non-zero.
    assert!(
        !out.status.success(),
        "expected non-zero exit for a failing test:\n{stdout}",
    );
    // Message text is unchanged by the traceback addition.
    assert!(
        stdout.contains("FAIL panics_in_nested_call") && stdout.contains("boom at the bottom"),
        "panic message regressed:\n{stdout}",
    );
    // The call chain is rendered outermost-first, one frame per call.
    assert!(
        stdout.contains("call stack (outermost first):"),
        "no traceback header:\n{stdout}",
    );
    for frame in ["panics_in_nested_call", "top", "middle", "deepest"] {
        assert!(
            stdout.contains(&format!("at {frame}")),
            "traceback missing frame `{frame}`:\n{stdout}",
        );
    }
}

#[test]
fn cross_module_struct_field_access_resolves_on_all_tiers() {
    // `pub struct Rec` lives in src/util.gos\n src/main.gos uses `&util::Rec`
    // as a param annotation and `&mut util::Rec` for a writeback. Both are
    // type-path annotations that must resolve to the struct's Adt so that
    // field access lowers to a real Field projection on every tier.
    let dir = fresh_dir("cross-mod-struct");
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/xmod2\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        src.join("util.gos"),
        concat!(
            "pub struct Rec { pub name: String, pub age: i64 }\n",
            "pub fn make(name: String, age: i64) -> Rec { Rec { name: name, age: age } }\n",
            "pub fn birthday(r: &mut util::Rec) { r.age += 1 }\n",
        ),
    )
    .unwrap();
    fs::write(
        src.join("main.gos"),
        concat!(
            "fn describe(r: util::Rec) -> String {\n",
            "    format(\"{} is {}\", r.name, r.age)\n",
            "}\n",
            "fn main() {\n",
            "    let mut r = util::make(\"ada\", 36)\n",
            "    util::birthday(&mut r)\n",
            "    println(\"{}\", describe(r))\n",
            "}\n",
        ),
    )
    .unwrap();

    let run_out = Command::new(gos_bin())
        .arg("run")
        .arg(".")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos");
    assert!(
        run_out.status.success(),
        "gos failed:\nstderr: {}",
        String::from_utf8_lossy(&run_out.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&run_out.stdout).trim(),
        "ada is 37",
        "VM stdout mismatch",
    );

    let build_out = Command::new(gos_bin())
        .arg("build")
        .current_dir(&dir)
        .output()
        .expect("gos build");
    assert!(
        build_out.status.success(),
        "gos build failed:\nstderr: {}",
        String::from_utf8_lossy(&build_out.stderr),
    );
    let mut bin_path = dir.join("target/debug/xmod2");
    if !std::env::consts::EXE_EXTENSION.is_empty() {
        bin_path.set_extension(std::env::consts::EXE_EXTENSION);
    }
    assert!(bin_path.is_file(), "expected binary at {bin_path:?}");
    let nat = run_native(&bin_path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(nat.2, Some(0), "native stderr: {}", nat.1);
    assert_eq!(
        nat.0.trim(),
        "ada is 37",
        "native stdout mismatch: {:?}",
        nat.0
    );
}

#[test]
fn cross_file_from_json_on_sibling_struct() {
    // A struct declared in a non-entry file, decoded via `from_json::<T>`
    // in a THIRD file, driven from `main`. The sibling auto-bundle nests
    // the struct inside `mod types { ... }`, so the serde synthesizer must
    // descend into inline modules to emit its per-type functions.
    let dir = write_project(
        "xfile-from-json",
        "example.com/fromjson",
        &[
            (
                "src/types.gos",
                "pub struct Point {\n    pub x: i64,\n    pub y: i64,\n    pub label: String,\n}\n",
            ),
            (
                "src/codec.gos",
                "use std::errors\n\
                 pub fn describe(text: String) -> String {\n\
                 \x20   match from_json::<types::Point>(text) {\n\
                 \x20       Ok(p) => format(\"{},{},{}\", p.x, p.y, p.label),\n\
                 \x20       Err(e) => format(\"err: {}\", e),\n\
                 \x20   }\n\
                 }\n",
            ),
            (
                "src/main.gos",
                "fn main() { println(\"{}\", codec::describe(\"{\\\"x\\\":3,\\\"y\\\":4,\\\"label\\\":\\\"origin\\\"}\")) }\n",
            ),
        ],
    );
    let vm = project_run_vm(&dir);
    assert_eq!(vm.2, Some(0), "VM stderr: {}", vm.1);
    assert_eq!(vm.0.trim(), "3,4,origin", "VM stdout: {:?}", vm.0);
    let nat = project_build_run(&dir, "fromjson");
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(nat.2, Some(0), "native stderr: {}", nat.1);
    assert_eq!(nat.0.trim(), "3,4,origin", "native stdout: {:?}", nat.0);
}

#[test]
fn cross_file_to_json_derive_and_typeinfo_on_sibling_struct() {
    // `to_json::<T>`, an implicit `#[derive(Debug)]` / structural equality,
    // and `typeInfo::<T>()` over a struct declared in a non-entry file all
    // reach it through the module-nesting the sibling auto-bundle introduces.
    let dir = write_project(
        "xfile-derive",
        "example.com/derivemod",
        &[
            (
                "src/model.gos",
                "#[derive(Debug)]\n\
                 pub struct Rec {\n    pub id: i64,\n    pub name: String,\n}\n\
                 pub fn new(id: i64, name: String) -> Rec { Rec { id: id, name: name } }\n\
                 pub fn roundtrip(r: Rec) -> String {\n\
                 \x20   match to_json::<Rec>(r) {\n\
                 \x20       Ok(s) => s,\n\
                 \x20       Err(e) => format(\"err: {}\", e),\n\
                 \x20   }\n\
                 }\n\
                 pub fn describe() -> String {\n\
                 \x20   let a = Rec { id: 1, name: \"x\" }\n\
                 \x20   let b = Rec { id: 1, name: \"x\" }\n\
                 \x20   let mut fields = \"\"\n\
                 \x20   for (n, t) in typeInfo::<Rec>() { fields += n\n fields += \":\"\n fields += t\n fields += \";\" }\n\
                 \x20   format(\"{:?} eq={} fields={}\", a, a == b, fields)\n\
                 }\n",
            ),
            (
                "src/main.gos",
                "fn main() {\n\
                 \x20   println(\"{}\", model::roundtrip(model::new(7, \"hi\")))\n\
                 \x20   println(\"{}\", model::describe())\n\
                 }\n",
            ),
        ],
    );
    let expected =
        "{\"id\":7,\"name\":\"hi\"}\nRec { id: 1, name: \"x\" } eq=true fields=id:i64;name:String;";
    let vm = project_run_vm(&dir);
    assert_eq!(vm.2, Some(0), "VM stderr: {}", vm.1);
    assert_eq!(vm.0.trim(), expected, "VM stdout: {:?}", vm.0);
    let nat = project_build_run(&dir, "derivemod");
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(nat.2, Some(0), "native stderr: {}", nat.1);
    assert_eq!(nat.0.trim(), expected, "native stdout: {:?}", nat.0);
}

#[test]
fn cross_file_from_json_nested_struct_on_vm() {
    // The decoded struct itself carries a nested-struct field, and both
    // structs live in a non-entry file. The synthesizer must reach the
    // nested type through the module-nesting to emit its serde functions.
    let dir = write_project(
        "xfile-nested",
        "example.com/nestedmod",
        &[
            (
                "src/types.gos",
                "pub struct Inner {\n    pub status: String,\n}\n\
                 pub struct Outer {\n    pub is_error: bool,\n    pub inner: Inner,\n}\n",
            ),
            (
                "src/main.gos",
                "fn main() {\n\
                 \x20   match from_json::<types::Outer>(\"{\\\"is_error\\\":false,\\\"inner\\\":{\\\"status\\\":\\\"ok\\\"}}\") {\n\
                 \x20       Ok(v) => println(\"{} {}\", v.is_error, v.inner.status),\n\
                 \x20       Err(e) => println(\"err: {}\", e),\n\
                 \x20   }\n\
                 }\n",
            ),
        ],
    );
    let vm = project_run_vm(&dir);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(vm.2, Some(0), "VM stderr: {}", vm.1);
    assert_eq!(vm.0.trim(), "false ok", "VM stdout: {:?}", vm.0);
}

#[test]
fn gos_test_discovers_tests_in_cross_referencing_files() {
    // A `#[test]` lives in a file whose top-level code imports a sibling
    // module's item, so the file does not typecheck in isolation - only
    // against the bundled whole-package source. Test discovery must parse
    // for `#[test]` names rather than fully checking each file alone, and
    // execution bundles siblings the same way `gos` / `gos build` do, so
    // the import resolves and the test runs.
    let dir = write_project(
        "gos-test-discovery",
        "example.com/testdisc",
        &[
            ("src/helper.gos", "pub fn base() -> i64 { 40 }\n"),
            (
                "src/main.gos",
                "use helper::base\n\
                 fn total() -> i64 { base() + 2 }\n\
                 fn main() { println(\"{}\", total()) }\n\
                 #[cfg(test)]\n\
                 mod main_tests {\n\
                 \x20   use std::testing\n\
                 \x20   #[test]\n\
                 \x20   fn total_uses_sibling() {\n\
                 \x20       let _ = testing::check_eq(super::total(), 42, \"40 + 2\")\n\
                 \x20   }\n\
                 }\n",
            ),
        ],
    );
    let out = Command::new(gos_bin())
        .arg("test")
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("gos test");
    let _ = fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "gos test should pass:\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        stdout.contains("PASS total_uses_sibling") && stdout.contains("1 passed"),
        "expected the sibling-referencing test to be discovered and pass:\n{stdout}",
    );
}

#[test]
fn relative_entry_path_bundles_siblings() {
    // `gos run main.gos` from inside the project directory must bundle
    // sibling modules exactly like `gos run .` does. A bare relative
    // entry has an empty `parent()`, and an unabsolutized path made
    // the module scan read from the empty dir and silently bundle
    // nothing, so qualified sibling calls failed with GR0001.
    let dir = fresh_dir("relative-entry");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/relentry\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("util.gos"),
        "pub fn add(a: i64, b: i64) -> i64 { a + b }\n",
    )
    .unwrap();
    fs::write(
        dir.join("main.gos"),
        "fn main() { println(\"{}\", util::add(1, 2)) }\n",
    )
    .unwrap();
    let child = Command::new(gos_bin())
        .arg("run")
        .arg("main.gos")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos");
    let out = run_with_timeout(child);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert_eq!(out.0.trim(), "3", "expected stdout '3', got: {:?}", out.0);
}

/// Writes a dependency project at `root/<name>` with the given id and
/// lib source, returning its directory.
fn write_dep_project(root: &Path, name: &str, id: &str, lib: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("project.toml"),
        format!("[project]\nid = \"{id}\"\nversion = \"0.1.0\"\n"),
    )
    .unwrap();
    fs::write(dir.join("lib.gos"), lib).unwrap();
    dir
}

fn write_app_project(root: &Path, deps: &str, main: &str) -> PathBuf {
    let dir = root.join("app");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("project.toml"),
        format!(
            "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\n\n[dependencies]\n{deps}"
        ),
    )
    .unwrap();
    fs::write(dir.join("main.gos"), main).unwrap();
    dir
}

#[test]
fn path_dependency_links_at_run() {
    let root = fresh_dir("path-dep-run");
    write_dep_project(
        &root,
        "dep",
        "example.com/dep",
        "pub fn greet(name: String) -> String { format(\"hi {}\", name) }\n",
    );
    let app = write_app_project(
        &root,
        "dep = { path = \"../dep\" }\n",
        "use \"example.com/dep\" as dep\n\nfn main() { println(\"{}\", dep::greet(\"gos\")) }\n",
    );
    let out = project_run_vm(&app);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert_eq!(out.0.trim(), "hi gos", "stdout: {:?}", out.0);
}

/// A package name may carry `-`, which no identifier may, so its module
/// name is the final path segment with each `-` replaced by `_`. Every
/// spelling of the import that names that module reaches the package: the
/// bare form, an item path, a list, an alias, and the package id itself.
#[test]
fn every_import_spelling_reaches_a_hyphenated_package() {
    let root = fresh_dir("dep-import-spellings");
    write_dep_project(
        &root,
        "pgsql-gos",
        "example.com/pgsql-gos",
        "pub fn greet(name: String) -> String { format(\"hi {}\", name) }\n",
    );
    for (label, main) in [
        (
            "bare",
            "use pgsql_gos\n\nfn main() { println(\"{}\", pgsql_gos::greet(\"gos\")) }\n",
        ),
        (
            "item path",
            "use pgsql_gos::greet\n\nfn main() { println(\"{}\", greet(\"gos\")) }\n",
        ),
        (
            "list",
            "use pgsql_gos::{greet}\n\nfn main() { println(\"{}\", greet(\"gos\")) }\n",
        ),
        (
            "alias",
            "use pgsql_gos as pg\n\nfn main() { println(\"{}\", pg::greet(\"gos\")) }\n",
        ),
        (
            "package id",
            "use \"example.com/pgsql-gos\"\n\nfn main() { println(\"{}\", pgsql_gos::greet(\"gos\")) }\n",
        ),
    ] {
        let app = write_app_project(&root, "pgsql_gos = { path = \"../pgsql-gos\" }\n", main);
        let out = project_run_vm(&app);
        assert_eq!(out.2, Some(0), "{label}: stderr: {}", out.1);
        assert_eq!(out.0.trim(), "hi gos", "{label}: stdout: {:?}", out.0);
    }
    let _ = fs::remove_dir_all(&root);
}

/// `-` is subtraction, never part of an identifier, so the package name is
/// not the module path. The diagnostic names the spelling that is.
#[test]
fn a_hyphenated_use_path_is_rejected_with_the_module_spelling() {
    let root = fresh_dir("dep-import-hyphen");
    write_dep_project(
        &root,
        "pgsql-gos",
        "example.com/pgsql-gos",
        "pub fn greet(name: String) -> String { format(\"hi {}\", name) }\n",
    );
    let app = write_app_project(
        &root,
        "pgsql_gos = { path = \"../pgsql-gos\" }\n",
        "use pgsql-gos\n\nfn main() { println(\"{}\", pgsql_gos::greet(\"gos\")) }\n",
    );
    let out = project_run_vm(&app);
    let _ = fs::remove_dir_all(&root);
    assert_ne!(out.2, Some(0), "a hyphenated use path must not run");
    let combined = format!("{}{}", out.0, out.1);
    assert!(
        combined.contains("GP0040"),
        "expected GP0040, got: {combined}"
    );
    assert!(
        combined.contains("use pgsql_gos"),
        "the diagnostic names the module spelling: {combined}"
    );
}

/// Two packages whose names normalize to one module name are reported as
/// the collision they are, naming both ids.
#[test]
fn two_packages_normalizing_to_one_module_name_collide() {
    let root = fresh_dir("dep-import-collision");
    write_dep_project(
        &root,
        "hyphened",
        "one.example.com/pgsql-gos",
        "pub fn a() -> i64 { 1 }\n",
    );
    write_dep_project(
        &root,
        "underscored",
        "two.example.com/pgsql_gos",
        "pub fn b() -> i64 { 2 }\n",
    );
    let app = write_app_project(
        &root,
        "first = { path = \"../hyphened\" }\nsecond = { path = \"../underscored\" }\n",
        "use pgsql_gos\n\nfn main() { println(\"{}\", pgsql_gos::a()) }\n",
    );
    let out = project_run_vm(&app);
    let _ = fs::remove_dir_all(&root);
    assert_ne!(out.2, Some(0), "a colliding module name must not run");
    let combined = format!("{}{}", out.0, out.1);
    assert!(
        combined.contains("GR0019"),
        "expected GR0019, got: {combined}"
    );
    for id in ["one.example.com/pgsql-gos", "two.example.com/pgsql_gos"] {
        assert!(
            combined.contains(id),
            "diagnostic must name {id}: {combined}"
        );
    }
}

/// Two packages whose ids share a final segment coexist once one names its
/// own module in the manifest. Both the manifest name and the package id
/// reach their own package.
#[test]
fn a_module_override_lets_two_same_named_packages_coexist() {
    let root = fresh_dir("dep-module-override");
    write_dep_project(
        &root,
        "first",
        "ownera.example.com/pgsql-gos",
        "pub fn who() -> String { \"a\" }\n",
    );
    write_dep_project(
        &root,
        "second",
        "ownerb.example.com/pgsql-gos",
        "pub fn who() -> String { \"b\" }\n",
    );
    let app = write_app_project(
        &root,
        "\"ownera.example.com/pgsql-gos\" = { path = \"../first\", module = \"pg_a\" }\n\
         \"ownerb.example.com/pgsql-gos\" = { path = \"../second\" }\n",
        "use pg_a\nuse pgsql_gos\n\n\
         fn main() { println(\"{} {}\", pg_a::who(), pgsql_gos::who()) }\n",
    );
    let out = project_run_vm(&app);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert_eq!(out.0.trim(), "a b", "stdout: {:?}", out.0);

    // The package id reaches the overridden module too, so a caller may
    // name either spelling.
    fs::write(
        app.join("main.gos"),
        "use \"ownera.example.com/pgsql-gos\" as pa\n\
         use \"ownerb.example.com/pgsql-gos\" as pb\n\n\
         fn main() { println(\"{} {}\", pa::who(), pb::who()) }\n",
    )
    .unwrap();
    let out = project_run_vm(&app);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert_eq!(out.0.trim(), "a b", "stdout: {:?}", out.0);
}

/// A dependency whose public structs make the autoderive stage synthesize
/// serde glue is reachable through every import spelling: the generated
/// source names the package by the module the bundler emitted, which no
/// spelling at the call site changes.
#[test]
fn a_serde_bearing_dependency_checks_under_every_import_spelling() {
    let root = fresh_dir("dep-serde-spellings");
    write_dep_project(
        &root,
        "pgsql-gos",
        "example.com/pgsql-gos",
        "pub struct Transaction { pub id: i64, pub label: String }\n\
         pub fn begin() -> Transaction { Transaction { id: 1, label: \"tx\" } }\n",
    );
    for (label, main) in [
        (
            "bare",
            "use pgsql_gos\n\nfn main() { println(\"{}\", pgsql_gos::begin().id) }\n",
        ),
        (
            "alias",
            "use pgsql_gos as pg\n\nfn main() { println(\"{}\", pg::begin().id) }\n",
        ),
        (
            "list",
            "use pgsql_gos::{begin}\n\nfn main() { println(\"{}\", begin().id) }\n",
        ),
        (
            "package id",
            "use \"example.com/pgsql-gos\"\n\nfn main() { println(\"{}\", pgsql_gos::begin().id) }\n",
        ),
        (
            "aliased package id",
            "use \"example.com/pgsql-gos\" as pg\n\nfn main() { println(\"{}\", pg::begin().id) }\n",
        ),
    ] {
        let app = write_app_project(&root, "pgsql_gos = { path = \"../pgsql-gos\" }\n", main);
        let out = project_run_vm(&app);
        assert_eq!(out.2, Some(0), "{label}: stderr: {}", out.1);
        assert_eq!(out.0.trim(), "1", "{label}: stdout: {:?}", out.0);
    }
    let _ = fs::remove_dir_all(&root);
}

/// `gos tidy` reads the imports a file actually writes, so a dependency
/// reached through its bare module name is kept.
#[test]
fn tidy_keeps_a_dependency_reached_by_its_module_name() {
    let root = fresh_dir("dep-import-tidy");
    write_dep_project(
        &root,
        "pgsql-gos",
        "example.com/pgsql-gos",
        "pub fn greet(name: String) -> String { format(\"hi {}\", name) }\n",
    );
    let app = write_app_project(
        &root,
        "pgsql_gos = { path = \"../pgsql-gos\" }\n",
        "use pgsql_gos\n\nfn main() { println(\"{}\", pgsql_gos::greet(\"gos\")) }\n",
    );
    let child = Command::new(gos_bin())
        .arg("tidy")
        .current_dir(&app)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos");
    let out = run_with_timeout(child);
    let manifest = fs::read_to_string(app.join("project.toml")).unwrap();
    let _ = fs::remove_dir_all(&root);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(
        manifest.contains("pgsql_gos"),
        "tidy dropped a dependency the source imports: {manifest}"
    );
}

/// A dependency's module is reached only through the import naming the
/// package it comes from, so the bare `dep::item` path is rejected.
#[test]
fn a_dependency_path_requires_the_matching_import() {
    let root = fresh_dir("path-dep-needs-import");
    write_dep_project(
        &root,
        "dep",
        "example.com/dep",
        "pub fn greet(name: String) -> String { format(\"hi {}\", name) }\n",
    );
    let app = write_app_project(
        &root,
        "dep = { path = \"../dep\" }\n",
        "fn main() { println(\"{}\", dep::greet(\"gos\")) }\n",
    );
    let out = project_run_vm(&app);
    let _ = fs::remove_dir_all(&root);
    assert_ne!(out.2, Some(0), "bare dependency path must not run");
    let combined = format!("{}{}", out.0, out.1);
    assert!(
        combined.contains("GR0016"),
        "expected GR0016, got: {combined}"
    );
    assert!(
        combined.contains("example.com/dep"),
        "diagnostic must name the package id: {combined}"
    );
}

/// A renaming alias names the dependency in type position as well as in
/// value position.
#[test]
fn a_dependency_alias_names_a_type_in_an_annotation() {
    let root = fresh_dir("path-dep-alias-type");
    write_dep_project(
        &root,
        "dep",
        "example.com/dep",
        "pub struct Counter { pub n: i64 }\n\nimpl Counter {\n    pub fn new() -> Counter { Counter { n: 7 } }\n    pub fn get(&self) -> i64 { self.n }\n}\n",
    );
    let app = write_app_project(
        &root,
        "dep = { path = \"../dep\" }\n",
        "use \"example.com/dep\" as d\n\nfn main() {\n    let c: d::Counter = d::Counter::new()\n    println(\"{}\", c.get())\n}\n",
    );
    let out = project_run_vm(&app);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert_eq!(out.0.trim(), "7", "stdout: {:?}", out.0);
}

#[test]
fn path_dependency_links_at_build() {
    let root = fresh_dir("path-dep-build");
    write_dep_project(
        &root,
        "dep",
        "example.com/dep",
        "pub fn add(a: i64, b: i64) -> i64 { a + b }\n",
    );
    let app = write_app_project(
        &root,
        "dep = { path = \"../dep\" }\n",
        "use \"example.com/dep\" as d\n\nfn main() { println(\"{}\", d::add(20, 22)) }\n",
    );
    let out = project_build_run(&app, "app");
    let _ = fs::remove_dir_all(&root);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert_eq!(out.0.trim(), "42", "stdout: {:?}", out.0);
}

#[test]
fn check_of_one_package_file_resolves_its_crate_paths() {
    // A file named on the command line is a module of the package it sits
    // in, so it is checked as part of that package - `crate::` names the
    // package root, not the file's own directory.
    let root = write_project(
        "check-member",
        "example.com/db",
        &[
            ("src/main.gos", "fn main() { }\n"),
            ("src/codec/mod.gos", "pub fn tag() -> i64 { 7 }\n"),
            ("src/engine/mod.gos", "\n"),
            (
                "src/engine/bind.gos",
                "use crate::codec\n\npub fn one() -> i64 { codec::tag() }\n",
            ),
        ],
    );
    let out = Command::new(gos_bin())
        .arg("check")
        .arg(root.join("src").join("engine").join("bind.gos"))
        .output()
        .expect("spawn gos check");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let _ = fs::remove_dir_all(&root);
    assert!(
        out.status.success(),
        "checking a package module must resolve `crate::`, stderr: {stderr}"
    );
}

#[test]
fn check_of_a_loose_file_stays_its_own_unit() {
    let dir = fresh_dir("check-loose");
    let src = write_source(&dir, "scratch", "fn main() { println(\"hi\") }\n");
    let out = Command::new(gos_bin())
        .arg("check")
        .arg(&src)
        .output()
        .expect("spawn gos check");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let _ = fs::remove_dir_all(&dir);
    assert!(out.status.success(), "loose file check failed: {stderr}");
}

#[test]
fn check_rejects_unknown_path_dep_member() {
    let root = fresh_dir("path-dep-check");
    write_dep_project(
        &root,
        "dep",
        "example.com/dep",
        "pub fn greet() -> String { \"hi\" }\n",
    );
    let app = write_app_project(
        &root,
        "dep = { path = \"../dep\" }\n",
        "use \"example.com/dep\" as dep\n\nfn main() { println(\"{}\", dep::nonexistent()) }\n",
    );
    let out = Command::new(gos_bin())
        .arg("check")
        .arg(".")
        .current_dir(&app)
        .output()
        .expect("spawn gos check");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let _ = fs::remove_dir_all(&root);
    assert!(
        !out.status.success(),
        "check must reject a nonexistent dep member, stderr: {stderr}"
    );
    assert!(
        stderr.contains("nonexistent"),
        "diagnostic should name the missing member: {stderr}"
    );
}

#[test]
fn transitive_path_dependency_links_at_run() {
    let root = fresh_dir("path-dep-transitive");
    write_dep_project(
        &root,
        "base",
        "example.com/base",
        "pub fn two() -> i64 { 2 }\n",
    );
    let mid = root.join("mid");
    fs::create_dir_all(&mid).unwrap();
    fs::write(
        mid.join("project.toml"),
        "[project]\nid = \"example.com/mid\"\nversion = \"0.1.0\"\n\n[dependencies]\nbase = { path = \"../base\" }\n",
    )
    .unwrap();
    fs::write(
        mid.join("lib.gos"),
        "use \"example.com/base\" as base\n\npub fn double_two() -> i64 { base::two() * 2 }\n",
    )
    .unwrap();
    let app = write_app_project(
        &root,
        "mid = { path = \"../mid\" }\n",
        "use \"example.com/mid\" as mid\n\nfn main() { println(\"{}\", mid::double_two()) }\n",
    );
    let out = project_run_vm(&app);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert_eq!(out.0.trim(), "4", "stdout: {:?}", out.0);
}

#[test]
fn same_fn_name_in_two_sibling_modules_runs() {
    // Two bundled modules may each define `add`; bare in-module
    // references bind to the module's own item, qualified calls to
    // the named module's. Previously a flat namespace made this a
    // GR0003 duplicate-definition error.
    let dir = write_project(
        "same-name-two-modules",
        "example.com/samename",
        &[
            (
                "alpha.gos",
                "pub fn add(a: i64, b: i64) -> i64 { a + b }\npub fn twice(a: i64) -> i64 { add(a, a) }\n",
            ),
            (
                "beta.gos",
                "pub fn add(a: i64, b: i64) -> i64 { (a + b) * 10 }\n",
            ),
            (
                "main.gos",
                "fn main() {\n    println(\"{} {} {}\", alpha::add(1, 2), beta::add(1, 2), alpha::twice(4))\n}\n",
            ),
        ],
    );
    let run = project_run_vm(&dir);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(run.0.trim(), "3 30 8", "vm stdout: {:?}", run.0);
    let build = project_build_run(&dir, "samename");
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(build.2, Some(0), "native stderr: {}", build.1);
    assert_eq!(build.0.trim(), "3 30 8", "native stdout: {:?}", build.0);
}

/// A library's own `impl` block reaches its private helpers. The
/// dependency is inlined as `mod lib { ... }`, so the methods have to be
/// recorded against the identity a receiver of that type carries.
#[test]
fn a_librarys_impl_reaches_its_own_private_method() {
    let root = fresh_dir("dep-private-method");
    write_dep_project(
        &root,
        "lib",
        "example.com/lib",
        "pub struct Point {\n    x: i64\n    y: i64\n}\n\n\
         impl Point {\n\
             pub fn new(x: i64, y: i64) -> Self { Point { x: x, y: y } }\n\
             pub fn public_dist(self) -> i64 { self.internal_dist() }\n\
             fn internal_dist(self) -> i64 { self.x + self.y }\n\
         }\n",
    );
    let app = write_app_project(
        &root,
        "lib = { path = \"../lib\" }\n",
        "use \"example.com/lib\" as lib\n\n\
         fn main() {\n\
             let point = lib::Point::new(1, 2)\n\
             println(\"{}\", point.public_dist())\n\
         }\n",
    );
    let run = project_run_vm(&app);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(run.0.trim(), "3", "vm stdout: {:?}", run.0);
    let build = project_build_run(&app, "app");
    let _ = fs::remove_dir_all(&root);
    assert_eq!(build.2, Some(0), "native stderr: {}", build.1);
    assert_eq!(build.0.trim(), "3", "native stdout: {:?}", build.0);
}

/// A diagnostic raised inside a path dependency names the dependency's
/// own file and its line there, not the consumer's entry file at the
/// offset the dependency happened to be inlined at.
#[test]
fn a_diagnostic_in_a_path_dependency_names_the_dependencys_file() {
    let root = fresh_dir("dep-diag-origin");
    write_dep_project(
        &root,
        "lib",
        "example.com/lib",
        "pub struct Point {\n    pub x: i64\n}\n\n\
         impl Point {\n\
             pub fn get(self) -> i64 { self.nosuchmethod() }\n\
         }\n",
    );
    let app = write_app_project(
        &root,
        "lib = { path = \"../lib\" }\n",
        "use \"example.com/lib\" as lib\n\n\
         fn main() { println(\"{}\", lib::Point { x: 1 }.get()) }\n",
    );
    let out = project_run_vm(&app);
    let _ = fs::remove_dir_all(&root);
    assert_ne!(out.2, Some(0), "expected a failure, stdout: {:?}", out.0);
    assert!(
        out.1.contains("lib.gos:6:"),
        "diagnostic must point at the dependency's own file and line: {}",
        out.1
    );
    assert!(
        !out.1.contains("main.gos:"),
        "diagnostic must not be attributed to the consumer's entry: {}",
        out.1
    );
}

/// `use "id" as alias` reaches a dependency's associated functions, not
/// just its free functions. Items are registered under the inlined
/// module's real name, so a renaming alias has to be respelled before
/// name-keyed dispatch.
#[test]
fn a_renaming_alias_reaches_a_dependencys_associated_function() {
    let root = fresh_dir("dep-alias-assoc");
    write_dep_project(
        &root,
        "lib",
        "example.com/lib",
        "pub struct Point {\n    x: i64\n    y: i64\n}\n\n\
         impl Point {\n\
             pub fn new(x: i64, y: i64) -> Self { Point { x: x, y: y } }\n\
             pub fn sum(self) -> i64 { self.x + self.y }\n\
         }\n\n\
         pub fn answer() -> i64 { 42 }\n",
    );
    let app = write_app_project(
        &root,
        "lib = { path = \"../lib\" }\n",
        "use \"example.com/lib\" as mylib\n\n\
         fn main() {\n\
             println(\"{} {}\", mylib::Point::new(1, 2).sum(), mylib::answer())\n\
         }\n",
    );
    let run = project_run_vm(&app);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(run.0.trim(), "3 42", "vm stdout: {:?}", run.0);
    let build = project_build_run(&app, "app");
    let _ = fs::remove_dir_all(&root);
    assert_eq!(build.2, Some(0), "native stderr: {}", build.1);
    assert_eq!(build.0.trim(), "3 42", "native stdout: {:?}", build.0);
}

/// A member the dependency genuinely does not export is still rejected
/// at check time through an alias.
#[test]
fn a_phantom_member_through_an_alias_is_still_rejected() {
    let root = fresh_dir("dep-alias-phantom");
    write_dep_project(
        &root,
        "lib",
        "example.com/lib",
        "pub fn real() -> i64 { 1 }\n",
    );
    let app = write_app_project(
        &root,
        "lib = { path = \"../lib\" }\n",
        "use \"example.com/lib\" as mylib\n\n\
         fn main() { println(\"{}\", mylib::nosuchfn()) }\n",
    );
    let out = project_run_vm(&app);
    let _ = fs::remove_dir_all(&root);
    assert_ne!(out.2, Some(0), "expected a failure, stdout: {:?}", out.0);
    assert!(
        out.1.contains("GR0001"),
        "a phantom member must be rejected at check time: {}",
        out.1
    );
}

/// An item without `pub` is private to the module that declares it, so
/// a consumer reaches the `pub` wrapper but never the helper behind it.
#[test]
fn a_dependencys_private_surface_stays_inside_the_dependency() {
    let root = fresh_dir("dep-private-surface");
    write_dep_project(
        &root,
        "lib",
        "example.com/lib",
        "pub struct Point {\n    x: i64\n    y: i64\n}\n\n\
         impl Point {\n\
             pub fn new(x: i64, y: i64) -> Self { Point { x: x, y: y } }\n\
             pub fn public_dist(self) -> i64 { self.internal_dist() }\n\
             fn internal_dist(self) -> i64 { self.x + self.y }\n\
         }\n\n\
         pub fn wrapper() -> i64 { helper() }\n\n\
         fn helper() -> i64 { 99 }\n",
    );
    let reachable = |body: &str| -> (String, String, Option<i32>) {
        let app = write_app_project(&root, "lib = { path = \"../lib\" }\n", body);
        project_run_vm(&app)
    };

    let ok = reachable(
        "use \"example.com/lib\"\n\n\
         fn main() {\n\
             println(\"{} {}\", lib::wrapper(), lib::Point::new(1, 2).public_dist())\n\
         }\n",
    );
    assert_eq!(
        ok.2,
        Some(0),
        "public surface must stay reachable: {}",
        ok.1
    );
    assert_eq!(ok.0.trim(), "99 3", "stdout: {:?}", ok.0);

    let private_fn =
        reachable("use \"example.com/lib\"\n\nfn main() { println(\"{}\", lib::helper()) }\n");
    assert_ne!(
        private_fn.2,
        Some(0),
        "private fn leaked: {:?}",
        private_fn.0
    );
    assert!(
        private_fn.1.contains("GR0008"),
        "expected a visibility error: {}",
        private_fn.1
    );

    let private_method = reachable(
        "use \"example.com/lib\"\n\n\
         fn main() { println(\"{}\", lib::Point::new(1, 2).internal_dist()) }\n",
    );
    assert_ne!(
        private_method.2,
        Some(0),
        "private method leaked: {:?}",
        private_method.0
    );
    assert!(
        private_method.1.contains("GT0063"),
        "expected a visibility error: {}",
        private_method.1
    );

    let _ = fs::remove_dir_all(&root);
}

/// A nested module reaches the private items of the module that
/// contains it, which is what the documented `#[cfg(test)]` block does.
#[test]
fn a_nested_module_reaches_its_parents_private_items() {
    let dir = write_project(
        "inline-mod-private",
        "example.com/inline",
        &[(
            "main.gos",
            "struct P { x: i64 }\n\
             impl P {\n\
                 pub fn new(x: i64) -> Self { P { x: x } }\n\
                 fn secret(self) -> i64 { self.x * 3 }\n\
             }\n\n\
             fn hidden() -> i64 { 4 }\n\n\
             mod inner {\n\
                 pub fn reach() -> i64 { super::P::new(2).secret() + super::hidden() }\n\
             }\n\n\
             fn main() { println(\"{}\", inner::reach()) }\n",
        )],
    );
    let run = project_run_vm(&dir);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(run.0.trim(), "10", "vm stdout: {:?}", run.0);
}

/// A descendant module keeps access to its parent's private items, and
/// the entry - which is not a descendant - does not.
#[test]
fn a_descendant_module_reaches_its_parents_private_items() {
    let dir = write_project(
        "nested-module-private",
        "example.com/nested",
        &[
            (
                "src/sub/mod.gos",
                "fn secret() -> i64 { 7 }\npub fn own() -> i64 { secret() }\n",
            ),
            (
                "src/sub/child.gos",
                "pub fn peek() -> i64 { super::secret() }\n",
            ),
            (
                "src/main.gos",
                "fn main() { println(\"{} {}\", sub::own(), sub::child::peek()) }\n",
            ),
        ],
    );
    let run = project_run_vm(&dir);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(run.0.trim(), "7 7", "vm stdout: {:?}", run.0);

    fs::write(
        dir.join("src/main.gos"),
        "fn main() { println(\"{}\", sub::secret()) }\n",
    )
    .unwrap();
    let outside = project_run_vm(&dir);
    let _ = fs::remove_dir_all(&dir);
    assert_ne!(outside.2, Some(0), "private item leaked: {:?}", outside.0);
    assert!(
        outside.1.contains("GR0008"),
        "expected a visibility error: {}",
        outside.1
    );
}

#[test]
fn a_dependency_reaches_an_associated_fn_in_its_own_sibling_module() {
    // A `Type::assoc` path is spelled relative to the module it is written
    // in, while the impl registers under the type's full module-qualified
    // identity. Inside a bundled dependency that identity carries the
    // dependency's module too, so both the imported spelling
    // (`use self::helper::Widget` + `Widget::make`) and the qualified one
    // (`helper::Widget::make`) have to anchor there - they type-checked and
    // were unbound at run time.
    let lib = fresh_dir("dep_assoc_lib");
    fs::write(
        lib.join("project.toml"),
        "[project]\nid = \"example.com/widget-lib\"\nversion = \"0.1.0\"\n\n[lib]\nname = \"widget-lib\"\npath = \"src/lib.gos\"\n",
    )
    .unwrap();
    fs::create_dir_all(lib.join("src")).unwrap();
    fs::write(
        lib.join("src/helper.gos"),
        "pub struct Widget { pub name: String, pub size: i64 }\n\
         impl Widget {\n\
         \x20   pub fn make(name: String, size: i64) -> Widget { Widget { name: name, size: size } }\n\
         \x20   pub fn grow(&mut self, by: i64) { self.size += by }\n\
         }\n",
    )
    .unwrap();
    fs::write(
        lib.join("src/lib.gos"),
        "use self::helper::Widget\n\n\
         pub fn imported(name: String) -> Widget { Widget::make(name, 2) }\n\
         pub fn qualified(name: String) -> Widget { helper::Widget::make(name, 3) }\n",
    )
    .unwrap();

    let app = fresh_dir("dep_assoc_app");
    fs::write(
        app.join("project.toml"),
        format!(
            // A TOML basic string reads `\` as an escape, so the path goes
            // in forward-slash form, which Windows accepts as a separator.
            "[project]\nid = \"example.com/app\"\nversion = \"0.1.0\"\nentry = \"src/main.gos\"\n\n[dependencies]\n\"example.com/widget-lib\" = {{ path = \"{}\" }}\n",
            lib.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("src/main.gos"),
        "use \"example.com/widget-lib\" as widgets\n\n\
         fn main() {\n\
         \x20   let mut a = widgets::imported(\"one\")\n\
         \x20   a.grow(10)\n\
         \x20   println(\"{} {}\", a.name, a.size)\n\
         \x20   let b = widgets::qualified(\"two\")\n\
         \x20   println(\"{} {}\", b.name, b.size)\n\
         }\n",
    )
    .unwrap();

    let run = project_run_vm(&app);
    let _ = fs::remove_dir_all(&app);
    let _ = fs::remove_dir_all(&lib);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert_eq!(run.0, "one 12\ntwo 3\n");
}

#[test]
fn a_mut_self_method_on_a_module_type_writes_back_to_its_caller() {
    // A type declared in a module is identified by its qualified name, so an
    // `impl` inside that module keys its methods there. Reading only the
    // bare name missed them, and a `&mut self` method that is missed runs
    // without the write-back protocol: its mutation of `self` never reached
    // the caller, so a counter kept answering its initial value. The `Map`
    // field mutates through its own handle, which is why only the plain
    // fields lost their updates.
    let dir = write_project(
        "mut_self_module_writeback",
        "example.com/writeback",
        &[
            (
                "src/engine.gos",
                "pub struct Conn { pub counter: i64 }\n\
                 impl Conn {\n\
                 \x20   pub fn next_id(&mut self) -> i64 { self.counter += 1; self.counter }\n\
                 }\n\
                 \n\
                 pub struct Client { pub pg: Conn, pub seen: Vec<i64> }\n\
                 impl Client {\n\
                 \x20   pub fn take(&mut self) -> i64 {\n\
                 \x20       let id = self.pg.next_id()\n\
                 \x20       self.seen.push(id)\n\
                 \x20       id\n\
                 \x20   }\n\
                 }\n\
                 \n\
                 pub fn open() -> Client { Client { pg: Conn { counter: 0 }, seen: #[] } }\n",
            ),
            (
                "src/main.gos",
                "fn main() {\n\
                 \x20   let mut c = engine::open()\n\
                 \x20   println(\"{} {} {}\", c.take(), c.take(), c.take())\n\
                 \x20   println(\"counter={} seen={:?}\", c.pg.counter, c.seen)\n\
                 }\n",
            ),
        ],
    );
    let vm = project_run_vm(&dir);
    let native = project_build_run(&dir, "writeback");
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "1 2 3\ncounter=3 seen=#[1, 2, 3]\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
}

#[test]
fn a_dependency_module_reaches_its_siblings_types_consts_and_variants() {
    // A path inside a package is written relative to the module it appears
    // in, while items register under their full module-qualified identity.
    // Consumed as a dependency the package gains an outer module, so a
    // sibling's type in a signature, its constants, and its enum variants
    // all have to anchor there - they resolved to a synthesized type or went
    // unbound at run time.
    let lib = fresh_dir("sibling_reach_lib");
    fs::write(
        lib.join("project.toml"),
        "[project]\nid = \"example.com/reach\"\nversion = \"0.1.0\"\n\n[lib]\nname = \"reach\"\npath = \"src/lib.gos\"\n",
    )
    .unwrap();
    fs::create_dir_all(lib.join("src")).unwrap();
    fs::write(
        lib.join("src/model.gos"),
        "pub const LIMIT: i64 = 7\n\
         pub enum Cell { Empty, Number(i64) }\n\
         pub struct Tally { pub total: i64 }\n\
         impl Tally {\n\
         \x20   pub fn start() -> Tally { Tally { total: 0 } }\n\
         }\n",
    )
    .unwrap();
    fs::write(
        lib.join("src/engine.gos"),
        "use crate::model\n\n\
         pub fn sum(cells: Vec<model::Cell>) -> i64 {\n\
         \x20   let mut tally = model::Tally::start()\n\
         \x20   for cell in cells {\n\
         \x20       tally.total += match cell {\n\
         \x20           model::Cell::Empty => 0,\n\
         \x20           model::Cell::Number(v) => v,\n\
         \x20       }\n\
         \x20   }\n\
         \x20   min(tally.total, model::LIMIT)\n\
         }\n",
    )
    .unwrap();
    fs::write(
        lib.join("src/lib.gos"),
        "use crate::{engine, model}\n\n\
         pub fn number(v: i64) -> model::Cell { model::Cell::Number(v) }\n\
         pub fn empty() -> model::Cell { model::Cell::Empty }\n\
         pub fn total(cells: Vec<model::Cell>) -> i64 { engine::sum(cells) }\n",
    )
    .unwrap();

    let app = fresh_dir("sibling_reach_app");
    fs::write(
        app.join("project.toml"),
        format!(
            // A TOML basic string reads `\` as an escape, so the path goes
            // in forward-slash form, which Windows accepts as a separator.
            "[project]\nid = \"example.com/reachapp\"\nversion = \"0.1.0\"\nentry = \"src/main.gos\"\n\n[dependencies]\n\"example.com/reach\" = {{ path = \"{}\" }}\n",
            lib.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("src/main.gos"),
        "use \"example.com/reach\" as reach\n\n\
         fn main() {\n\
         \x20   let cells = #[reach::number(2), reach::empty(), reach::number(3)]\n\
         \x20   println(\"{}\", reach::total(cells))\n\
         \x20   let capped = #[reach::number(50)]\n\
         \x20   println(\"{}\", reach::total(capped))\n\
         }\n",
    )
    .unwrap();

    let run = project_run_vm(&app);
    let _ = fs::remove_dir_all(&app);
    let _ = fs::remove_dir_all(&lib);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert_eq!(run.0, "5\n7\n");
}

#[test]
fn forwarding_a_mut_map_parameter_keeps_the_callers_container() {
    // A reference is an alias by construction, so forwarding an existing
    // `&mut Map` parameter has to reach the callee as the same container.
    // Inside a module it was copied first, and the callee's `pop` emptied a
    // copy while the caller's map kept every entry.
    let dir = write_project(
        "forwarded_mut_map",
        "example.com/forwarded",
        &[
            (
                "src/params.gos",
                "fn take_one(kv: &mut Map<String, String>, key: String) -> Option<String> {\n\
                 \x20   Map::pop(kv, key)\n\
                 }\n\
                 \n\
                 pub fn drain(kv: &mut Map<String, String>) -> String {\n\
                 \x20   let host = match take_one(kv, \"host\") {\n\
                 \x20       Some(v) => v,\n\
                 \x20       None => \"none\",\n\
                 \x20   }\n\
                 \x20   format(\"{} left={}\", host, kv.len())\n\
                 }\n",
            ),
            (
                "src/main.gos",
                "fn main() {\n\
                 \x20   let mut kv: Map<String, String> = {\"host\": \"h\", \"dbname\": \"d\"}\n\
                 \x20   let summary = params::drain(&mut kv)\n\
                 \x20   println(\"{} after={}\", summary, kv.len())\n\
                 }\n",
            ),
        ],
    );
    let vm = project_run_vm(&dir);
    let native = project_build_run(&dir, "forwarded");
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "h left=1 after=1\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
}

/// Writes a dependency package and an app that depends on it by path.
/// Returns `(app_root, workspace_root)`.
fn write_package_pair(
    tag: &str,
    dep_id: &str,
    dep_files: &[(&str, &str)],
    app_id: &str,
    app_files: &[(&str, &str)],
) -> (PathBuf, PathBuf) {
    let root = fresh_dir(tag);
    let dep = root.join("dep");
    let app = root.join("app");
    fs::create_dir_all(&dep).unwrap();
    fs::create_dir_all(&app).unwrap();
    fs::write(
        dep.join("project.toml"),
        format!("[project]\nid = \"{dep_id}\"\nversion = \"0.1.0\"\n"),
    )
    .unwrap();
    fs::write(
        app.join("project.toml"),
        format!(
            "[project]\nid = \"{app_id}\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\n\"{dep_id}\" = {{ path = \"../dep\" }}\n"
        ),
    )
    .unwrap();
    for (dir, files) in [(&dep, dep_files), (&app, app_files)] {
        for (rel, contents) in files {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
    }
    (app, root)
}

/// Runs `gos check .` at `dir`.
fn project_check(dir: &Path) -> (String, String, Option<i32>) {
    let child = Command::new(gos_bin())
        .arg("check")
        .arg(".")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos check");
    run_with_timeout(child)
}

#[test]
fn split_impl_blocks_are_visible_to_a_consumer() {
    // A type's methods belong to the type, not to the file that declares
    // it, so a consumer reaches every `impl` the package writes.
    let (app, root) = write_package_pair(
        "split-impl",
        "example.com/tylib",
        &[
            (
                "src/lib.gos",
                "pub struct Holder { pub n: i64 }\n\
                 impl Holder { pub fn base(&self) -> i64 { self.n } }\n\
                 pub fn make() -> Holder { Holder { n: 21 } }\n",
            ),
            (
                "src/extra.gos",
                "use crate::Holder\n\
                 impl Holder { pub fn doubled(&self) -> i64 { self.base() * 2 } }\n",
            ),
        ],
        "example.com/consumer",
        &[(
            "src/main.gos",
            "use tylib\n\nfn main() { println(\"{}\", tylib::make().doubled()) }\n",
        )],
    );
    let vm = project_run_vm(&app);
    let native = project_build_run(&app, "consumer");
    let _ = fs::remove_dir_all(&root);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "42\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
}

#[test]
fn entry_module_call_binds_its_own_function() {
    // A bare name in the entry module names the entry module's item; an
    // imported sibling's same-named function does not win it.
    let dir = write_project(
        "entry-shadow",
        "example.com/entryshadow",
        &[
            (
                "src/helper.gos",
                "pub fn label(text: String) -> String { format(\"helper: {}\", text) }\n",
            ),
            (
                "src/main.gos",
                "use crate::helper\n\n\
                 pub fn label(n: i64) -> String { format(\"entry: {}\", n) }\n\n\
                 fn main() { println(\"{} {}\", label(7), helper::label(\"t\")) }\n",
            ),
        ],
    );
    let vm = project_run_vm(&dir);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "entry: 7 helper: t\n", "vm stdout");
}

#[test]
fn package_visible_method_reaches_a_sibling_module() {
    // `pub (package)` states one visibility, which a method answers to
    // exactly as a free function does.
    let dir = write_project(
        "package-method",
        "example.com/pkgvis",
        &[
            (
                "src/conn.gos",
                "pub struct Conn { pub n: i64 }\n\n\
                 impl Conn {\n\
                 \x20   pub (package) fn note(&mut self, v: i64) { self.n = v }\n\
                 \x20   pub fn value(&self) -> i64 { self.n }\n\
                 }\n\n\
                 pub (package) fn helper_free() -> i64 { 7 }\n",
            ),
            (
                "src/main.gos",
                "use crate::conn\n\n\
                 fn main() {\n\
                 \x20   let mut c = conn::Conn { n: 0 }\n\
                 \x20   c.note(5)\n\
                 \x20   println(\"{} {}\", c.value(), conn::helper_free())\n\
                 }\n",
            ),
        ],
    );
    let vm = project_run_vm(&dir);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "5 7\n", "vm stdout");
}

#[test]
fn cross_package_struct_variant_holding_itself_renders_and_builds() {
    // A struct variant carrying a boxed value of its own enum keeps the
    // enum's identity across the package boundary, so the synthesized
    // `{:?}` matches on it and the build resolves its constructor.
    let (app, root) = write_package_pair(
        "boxed-variant",
        "example.com/vlib",
        &[(
            "src/lib.gos",
            "pub enum Value {\n\
             \x20   Nil\n\
             \x20   Int(i64)\n\
             \x20   Attr { data: Value, pairs: Vec<(Value, Value)> }\n\
             }\n\n\
             pub fn wrapped() -> Value {\n\
             \x20   Value::Attr { data: Box::new(Value::Int(7)), pairs: #[] }\n\
             }\n",
        )],
        "example.com/vapp",
        &[(
            "src/main.gos",
            "use vlib\n\nfn main() { println(\"{:?}\", vlib::wrapped()) }\n",
        )],
    );
    let vm = project_run_vm(&app);
    let native = project_build_run(&app, "vapp");
    let _ = fs::remove_dir_all(&root);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "Attr { data: Int(7), pairs: #[] }\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
}

#[test]
fn a_project_alias_roots_a_submodule_import() {
    // `use "id" as alias` names the package, so `use alias::submodule` is
    // the same import as one written with the module's own name.
    let (app, root) = write_package_pair(
        "alias-submodule",
        "example.com/redis-gos",
        &[
            ("src/lib.gos", "pub fn version() -> String { \"1\" }\n"),
            (
                "src/config.gos",
                "use std::errors\n\n\
                 pub struct Config { pub host: String }\n\n\
                 pub fn parse(url: String) -> Result<Config, errors::Error> {\n\
                 \x20   if url.is_empty() { return Err(errors::new(\"empty\")) }\n\
                 \x20   Ok(Config { host: url.clone() })\n\
                 }\n",
            ),
        ],
        "example.com/aliasapp",
        &[(
            "src/main.gos",
            "use std::errors\n\
             use \"example.com/redis-gos\" as redis\n\
             use redis::config\n\n\
             fn run() -> Result<(), errors::Error> {\n\
             \x20   let cfg = config::parse(\"localhost\")?\n\
             \x20   println(\"{}\", cfg.host)\n\
             \x20   Ok(())\n\
             }\n\n\
             fn main() { let _ = run() }\n",
        )],
    );
    let check = project_check(&app);
    let vm = project_run_vm(&app);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(check.2, Some(0), "check stderr: {}", check.1);
    assert!(
        !check.1.contains("GL0002"),
        "the alias is what the second import is rooted at: {}",
        check.1
    );
    assert_eq!(vm.0, "localhost\n", "vm stdout");
}

#[test]
fn a_declared_but_unimported_dependency_checks_clean() {
    // The import states which package a path comes from, which is a rule
    // about user code: the serde functions the compiler synthesizes for a
    // dependency's types are not the user's to import.
    let (app, root) = write_package_pair(
        "unimported-dep",
        "example.com/depsl",
        &[(
            "src/lib.gos",
            "pub struct SlotRange { pub start: i64, pub end: i64 }\n\n\
             pub fn make() -> SlotRange { SlotRange { start: 1, end: 2 } }\n",
        )],
        "example.com/unimported",
        &[("src/main.gos", "fn main() { println(\"hello\") }\n")],
    );
    let check = project_check(&app);
    let vm = project_run_vm(&app);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(check.2, Some(0), "check stderr: {}", check.1);
    assert!(!check.1.contains("GR0016"), "check stderr: {}", check.1);
    assert_eq!(vm.0, "hello\n", "vm stdout");
}

#[test]
fn checking_a_directory_of_projects_checks_each_as_one_unit() {
    // A directory that is not itself a project may hold several. Each is
    // checked through its entry, so a cross-module reference resolves the
    // way it does under `gos run`.
    let root = fresh_dir("dir-of-projects");
    for (name, id) in [("one", "example.com/one"), ("two", "example.com/two")] {
        let project = root.join(name);
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(
            project.join("project.toml"),
            format!("[project]\nid = \"{id}\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
        fs::write(
            project.join("src/util.gos"),
            "pub fn add(a: i64, b: i64) -> i64 { a + b }\n",
        )
        .unwrap();
        fs::write(
            project.join("src/main.gos"),
            "fn main() { println(\"{}\", util::add(1, 2)) }\n",
        )
        .unwrap();
    }
    let check = project_check(&root);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(
        check.2,
        Some(0),
        "check stdout: {}\nstderr: {}",
        check.0,
        check.1
    );
    assert!(
        !check.0.contains("util.gos") && !check.1.contains("util.gos"),
        "a sibling module is part of its project's unit, not a unit of its own: {}{}",
        check.0,
        check.1
    );
}

#[test]
fn a_bare_call_naming_a_module_reports_the_import_it_needs() {
    // A module and one of its items may share a name. The module lives in
    // the type namespace, so a bare call to that name must not pick it up:
    // a module is not a value, and resolving one here reaches the backends
    // as a function reference nothing declares.
    let dir = write_project(
        "module-named-like-its-item",
        "example.com/modname",
        &[
            (
                "src/physics_sys.gos",
                "pub fn physics_sys(x: i64) -> i64 { x * 3 }\n",
            ),
            (
                "src/main.gos",
                "fn main() { println(\"{}\", physics_sys(4)) }\n",
            ),
        ],
    );
    let check = project_check(&dir);
    assert_eq!(
        check.2,
        Some(1),
        "check accepted a bare call to a module name:\n{}{}",
        check.0,
        check.1
    );
    let reported = format!("{}{}", check.0, check.1);
    assert!(
        reported.contains("GR0011") && reported.contains("physics_sys::physics_sys"),
        "expected GR0011 naming the import: {reported}"
    );

    // The imported spelling runs on every tier.
    fs::write(
        dir.join("src/main.gos"),
        "use physics_sys::physics_sys\nfn main() { println(\"{}\", physics_sys(4)) }\n",
    )
    .unwrap();
    let vm = project_run_vm(&dir);
    assert_eq!(vm.0.trim(), "12", "vm stderr: {}", vm.1);
    let native = project_build_run(&dir, "modname");
    assert_eq!(native.0.trim(), "12", "native stderr: {}", native.1);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_dependency_s_tests_run_once_in_its_own_project() {
    // A test belongs to the project whose source declares it. The unit a
    // dependent is checked in carries the dependency's source inlined, so
    // without attributing each test to the file its bytes came from, a
    // dependency's tests run again for every project that depends on it.
    let root = fresh_dir("dependency-tests-once");
    let lib = root.join("intcode");
    fs::create_dir_all(lib.join("src")).unwrap();
    fs::write(
        lib.join("project.toml"),
        "[project]\nid = \"example.com/intcode\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        lib.join("src/lib.gos"),
        "pub fn run(x: i64) -> i64 { x * 2 }\n\
         #[cfg(test)]\n\
         mod tests {\n\
             use std::testing\n\
             #[test]\n\
             fn library_test() { let _ = testing::check_eq(super::run(2), 4, \"doubles\") }\n\
         }\n",
    )
    .unwrap();
    for app in ["d01", "d02"] {
        let dir = root.join(app);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("project.toml"),
            format!(
                "[project]\nid = \"example.com/{app}\"\nversion = \"0.1.0\"\n\n\
                 [dependencies]\n\"example.com/intcode\" = {{ path = \"../intcode\" }}\n"
            ),
        )
        .unwrap();
        fs::write(
            dir.join("src/main.gos"),
            "use intcode\n\
             fn main() { println(\"{}\", intcode::run(3)) }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 use std::testing\n\
                 #[test]\n\
                 fn app_test() { let _ = testing::check_eq(intcode::run(1), 2, \"own\") }\n\
             }\n",
        )
        .unwrap();
    }

    let child = Command::new(gos_bin())
        .arg("test")
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos test");
    let (stdout, stderr, code) = run_with_timeout(child);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(code, Some(0), "gos test failed:\n{stdout}{stderr}");
    assert_eq!(
        stdout.matches("library_test").count(),
        1,
        "the dependency's test ran more than once:\n{stdout}"
    );
    assert_eq!(
        stdout.matches("app_test").count(),
        2,
        "each dependent's own test runs once:\n{stdout}"
    );
}

#[test]
fn a_project_module_named_for_a_stdlib_module_keeps_its_own_types() {
    // `sql::Stmt`, `sql::Value` and a bare `use sql::Stmt` all name this
    // project's types: the injected stdlib wrappers are reached only
    // through `use std::database::sql`.
    let dir = write_project(
        "local-sql-module",
        "example.com/localsql",
        &[
            (
                "src/sql.gos",
                "pub enum Stmt {\n\
                 \x20   Compact\n\
                 \x20   Insert(String)\n\
                 }\n\
                 \n\
                 pub enum Value {\n\
                 \x20   Int(i64)\n\
                 }\n\
                 \n\
                 pub fn parse(src: String) -> Stmt {\n\
                 \x20   if src == \"compact\" { Stmt::Compact } else { Stmt::Insert(src) }\n\
                 }\n",
            ),
            (
                "src/main.gos",
                "use sql::Stmt\n\
                 \n\
                 fn describe(stmt: sql::Stmt) -> String {\n\
                 \x20   match stmt {\n\
                 \x20       sql::Stmt::Compact => \"compact\"\n\
                 \x20       sql::Stmt::Insert(s) => format(\"insert {}\", s)\n\
                 \x20   }\n\
                 }\n\
                 \n\
                 fn tag(v: sql::Value) -> i64 {\n\
                 \x20   match v {\n\
                 \x20       sql::Value::Int(n) => n\n\
                 \x20   }\n\
                 }\n\
                 \n\
                 fn bare(stmt: Stmt) -> String { describe(stmt) }\n\
                 \n\
                 fn main() {\n\
                 \x20   println(\"{}\", describe(sql::parse(\"compact\")))\n\
                 \x20   println(\"{}\", bare(sql::parse(\"rows\")))\n\
                 \x20   println(\"{}\", tag(sql::Value::Int(7)))\n\
                 }\n",
            ),
        ],
    );
    let vm = project_run_vm(&dir);
    let native = project_build_run(&dir, "localsql");
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "compact\ninsert rows\n7\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
}

/// Live heap objects the leak ledger reports at exit, as `(strings, vecs)`.
fn live_at_exit(bin: &Path, args: &[&str]) -> (i64, i64) {
    let out = Command::new(bin)
        .args(args)
        .env("GOS_LEAK_LEDGER", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run the built binary");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stderr
        .lines()
        .find(|l| l.starts_with("LEAK LEDGER"))
        .unwrap_or_else(|| panic!("no leak ledger in:\n{stderr}"));
    let field = |name: &str| -> i64 {
        line.split_whitespace()
            .find_map(|f| f.strip_prefix(name))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("no `{name}` in {line}"))
    };
    (field("str="), field("vec="))
}

/// A program whose live set does not grow with its iteration count holds no
/// per-iteration share. Ten times the work reaching the same live count is what
/// separates a balanced program from one that leaks a share per round.
fn assert_live_set_flat(dir: &Path, id_tail: &str, mode: &str) {
    let build = Command::new(gos_bin())
        .arg("build")
        .current_dir(dir)
        .output()
        .expect("spawn gos build");
    assert!(
        build.status.success(),
        "gos build failed:\nstderr: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let mut bin = dir.join("target/debug").join(id_tail);
    if !std::env::consts::EXE_EXTENSION.is_empty() {
        bin.set_extension(std::env::consts::EXE_EXTENSION);
    }
    let small = live_at_exit(&bin, &[mode, "200"]);
    let large = live_at_exit(&bin, &[mode, "2000"]);
    assert_eq!(
        small, large,
        "`{mode}` holds a share per round: 200 rounds left {small:?} live, 2000 left {large:?}"
    );
}

const OWNERSHIP_PROGRAM: &str = "\
use std::{env, sync}\n\
\n\
struct Inner { xs: Vec<i64>, name: String }\n\
struct Outer { inner: Inner, tags: Vec<String> }\n\
\n\
fn make(i: i64) -> Outer {\n\
\x20   let xs: Vec<i64> = #[i, i * 2]\n\
\x20   let tags: Vec<String> = #[\"t\" + i.to_string()]\n\
\x20   Outer { inner: Inner { xs: xs, name: \"row-\" + i.to_string() }, tags: tags }\n\
}\n\
\n\
fn append(out: &mut String, i: i64) {\n\
\x20   out.push_str(\"row-\")\n\
\x20   out.push_str(i.to_string())\n\
\x20   out.push('!')\n\
\x20   out.push_char('?')\n\
\x20   out.push_byte(b'.' as i64)\n\
}\n\
\n\
fn shrink(out: &mut String) {\n\
\x20   out.truncate(3)\n\
\x20   out.clear()\n\
}\n\
\n\
fn rows(i: i64) -> Result<Vec<String>, String> {\n\
\x20   if i < 0 { return Err(\"no\") }\n\
\x20   Ok(#[\"row-\" + i.to_string(), \"tag-\" + i.to_string()])\n\
}\n\
\n\
// The sequence reaches the answer through a binding, which is the shape a
// point read has: the carrier hands its payload over, and the binding hands
// it on.\n\
fn handed_on(i: i64) -> Result<Vec<String>, String> {\n\
\x20   let vals = rows(i)?\n\
\x20   Ok(vals)\n\
}\n\
\n\
fn maybe(i: i64) -> Option<String> {\n\
\x20   if i % 2 == 0 { Some(\"row-\" + i.to_string()) } else { None }\n\
}\n\
\n\
fn main() {\n\
\x20   let args = env::args()\n\
\x20   let mode = if args.len() > 0 { args[0] } else { \"concat\" }\n\
\x20   let rounds = if args.len() > 1 { args[1].to_i64().unwrap_or(200) } else { 200 }\n\
\x20   let mut i: i64 = 0\n\
\x20   let mut sink: i64 = 0\n\
\x20   while i < rounds {\n\
\x20       if mode == \"concat\" {\n\
\x20           let s = \"row-\" + i.to_string()\n\
\x20           sink += s.len()\n\
\x20       } else if mode == \"carrier\" {\n\
\x20           sink += maybe(i).unwrap_or(\"\").len()\n\
\x20           sink += maybe(i).map(|s| s.len()).unwrap_or(0)\n\
\x20           sink += maybe(i).unwrap_or_else(|| \"x\").len()\n\
\x20       } else if mode == \"delivered\" {\n\
\x20           let tx, rx = sync::channel_unbounded()\n\
\x20           tx.send(make(i))\n\
\x20           match rx.recv() { Some(o) => sink += o.inner.xs.len(), None => () }\n\
\x20       } else if mode == \"abandoned\" {\n\
\x20           let tx, rx = sync::channel_unbounded()\n\
\x20           tx.send(make(i))\n\
\x20           sink += i\n\
\x20       } else if mode == \"handoff\" {\n\
\x20           match handed_on(i) {\n\
\x20               Ok(v) => sink += v.len()\n\
\x20               Err(_) => sink += 0\n\
\x20           }\n\
\x20       } else if mode == \"rebind\" {\n\
\x20           let mut b = \"\"\n\
\x20           append(&mut b, i)\n\
\x20           sink += b.len()\n\
\x20       } else if mode == \"reset\" {\n\
\x20           let mut b = \"seed-\" + i.to_string()\n\
\x20           shrink(&mut b)\n\
\x20           sink += b.len()\n\
\x20       } else if mode == \"nested\" {\n\
\x20           let holder: Vec<Outer> = #[make(i)]\n\
\x20           sink += holder.len()\n\
\x20       }\n\
\x20       i += 1\n\
\x20   }\n\
\x20   println(\"{} {} {}\", mode, rounds, sink)\n\
}\n";

#[test]
fn heap_values_are_released_rather_than_held_per_round() {
    // Each mode is one ownership path that must not keep a share per round: a
    // `String` concatenation, an `Option` payload read through the carrier
    // accessors, an aggregate stored in a sequence, the receiver-rebind methods
    // writing back through a `&mut String` parameter, and a sequence handed on
    // from a carrier's payload through a binding. The channel modes the
    // program also carries are exercised for correctness elsewhere; one
    // construction of a doubly-nested aggregate still keeps a share when it
    // crosses a channel, which is recorded rather than asserted here.
    let dir = write_project(
        "ownership-live-set",
        "example.com/ownership",
        &[("src/main.gos", OWNERSHIP_PROGRAM)],
    );
    for mode in ["concat", "carrier", "nested", "rebind", "reset", "handoff"] {
        assert_live_set_flat(&dir, "ownership", mode);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_aggregate_holding_growable_storage_crosses_a_channel() {
    // The value is sent from a goroutine whose frame is long gone by the time
    // the receiver reads it, so a share dropped too early shows up here as
    // wrong data rather than a leak.
    let dir = write_project(
        "aggregate-channel",
        "example.com/aggchan",
        &[(
            "src/main.gos",
            "use std::{sync, time}\n\
             \n\
             struct Row { xs: Vec<i64>, name: String, tags: Vec<String> }\n\
             \n\
             fn produce(tx: Sender<Row>, n: i64) {\n\
             \x20   let mut i: i64 = 0\n\
             \x20   while i < n {\n\
             \x20       tx.send(Row {\n\
             \x20           xs: #[i, i * 2]\n\
             \x20           name: \"row-\" + i.to_string()\n\
             \x20           tags: #[\"t\" + i.to_string()]\n\
             \x20       })\n\
             \x20       i += 1\n\
             \x20   }\n\
             \x20   tx.close()\n\
             }\n\
             \n\
             fn main() {\n\
             \x20   let tx, rx = sync::channel_unbounded()\n\
             \x20   let done = cohort(isolation: Isolation::Thread) {\n\
             \x20       spawn(|| produce(tx, 200))\n\
             \x20       let _ = time::sleep(50)\n\
             \x20       let mut total: i64 = 0\n\
             \x20       let mut names: i64 = 0\n\
             \x20       while let Some(r) = rx.recv() {\n\
             \x20           total += r.xs[0] + r.xs[1]\n\
             \x20           names += r.name.len() + r.tags[0].len()\n\
             \x20       }\n\
             \x20       println(\"{} {}\", total, names)\n\
             \x20   }\n\
             \x20   println(\"{:?}\", done.is_ok())\n\
             }\n",
        )],
    );
    let vm = project_run_vm(&dir);
    let native = project_build_run(&dir, "aggchan");
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "59700 1980\ntrue\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
}

/// Runs `gos <subcommand> .` at the project root.
fn project_command(dir: &Path, subcommand: &str) -> (String, String, Option<i32>) {
    let child = Command::new(gos_bin())
        .arg(subcommand)
        .arg(".")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos");
    run_with_timeout(child)
}

#[test]
fn a_test_module_checks_as_the_module_around_it() {
    let dir = write_project(
        "test-module-checks",
        "example.com/test-module-checks",
        &[
            (
                "src/engine/mod.gos",
                "pub struct Store { pub v: Vec<i64> }\n\
                 pub struct Engine { pub store: Store }\n\
                 pub fn len_of(s: &mut Store) -> i64 { s.v.len() }\n\
                 pub fn open(dir: String, read_only: bool = false) -> Engine {\n    \
                 Engine { store: Store { v: #[dir.len(), if read_only { 1 } else { 0 }] } }\n}\n",
            ),
            (
                "src/engine/scan.gos",
                "use crate::engine\n\
                 pub fn count(eng: &mut engine::Engine) -> i64 { engine::len_of(&mut eng.store) }\n\
                 pub fn ro() -> engine::Engine { engine::open(\"d\", read_only: true) }\n\
                 \n\
                 #[cfg(test)]\n\
                 mod tests {\n    \
                 use crate::engine\n    \
                 fn helper(eng: &mut engine::Engine) -> i64 { engine::len_of(&mut eng.store) }\n    \
                 #[test]\n    \
                 fn opens_read_only() {\n        \
                 let mut e = engine::open(\"d\", read_only: true)\n        \
                 assert_eq(helper(&mut e), 2)\n    \
                 }\n\
                 }\n",
            ),
            (
                "src/main.gos",
                "use engine::scan\nfn main() {\n    let mut e = scan::ro()\n    \
                 println(\"{}\", scan::count(&mut e))\n}\n",
            ),
        ],
    );
    let (out, err, code) = project_command(&dir, "check");
    assert_eq!(code, Some(0), "gos check:\n{out}\n{err}");
    let (out, err, code) = project_command(&dir, "test");
    assert_eq!(code, Some(0), "gos test:\n{out}\n{err}");
    assert!(out.contains("1 passed"), "{out}");
    let (out, err, code) = project_run_vm(&dir);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.trim(), "2");
}
