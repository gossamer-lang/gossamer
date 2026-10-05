//! `#[export]` functions: ordinary functions on every tier, and the C entry
//! points of a `[lib]` built as a static archive and a shared library, called
//! from a C program on its main thread and on four threads of its own.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::support::{Project, gos_binary};

const EXPORTS: &str = r#"use std::ffi
use std::ffi::Ptr
use std::runtime

#[repr(C)]
struct V2 {
    x: f64,
    y: f64,
}

static mut NAMES: Vec<String> = #["seed"]

#[export("gx_add")]
fn add(a: i64, b: i32) -> i64 {
    a + b as i64
}

#[export]
fn gx_norm(v: V2) -> f64 {
    v.x * v.x + v.y * v.y
}

#[export]
fn gx_scale(v: V2, k: f64) -> V2 {
    V2 { x: v.x * k, y: v.y * k }
}

#[export]
fn gx_sum(data: Ptr<i32>, len: ffi::size_t) -> i64 {
    let view = unsafe { ffi::View::new(data, len as i64) }
    let mut total = 0
    for i in 0..view.len() {
        total += view.get(i) as i64
    }
    total
}

#[export]
fn gx_remember(n: i32) -> i64 {
    unsafe { NAMES.push(f"n{n}") }
    unsafe { NAMES.len() as i64 }
}

#[export]
fn gx_on_exit() {
    runtime::at_exit(|| println("exit hook"))
}

#[export]
fn gx_fail(n: i32) -> i32 {
    if n > 2 {
        panic(f"too many: {n}")
    }
    n
}
"#;

#[test]
fn exported_functions_are_ordinary_functions_on_every_tier() {
    let project = Project::new(
        "export-tiers",
        &format!(
            "{EXPORTS}{}",
            r#"
fn main() {
    let xs = #[1, 2, 3, 4]
    let v = gx_scale(V2 { x: 3.0, y: 4.0 }, 2.0)
    println(f"{add(2, 3)} {gx_norm(V2 { x: 3.0, y: 4.0 })} {v.x} {v.y}")
    println(f"{gx_remember(1)} {gx_remember(2)} {gx_fail(2)} {xs.len()}")
    gx_on_exit()
}
"#
        ),
    );
    project.expect_everywhere("5 25 6 8\n2 3 2 4\nexit hook");
}

/// A `[lib]` project whose root is `source`, built as both artifacts.
fn library(tag: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-export-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("create the project");
    std::fs::write(
        dir.join("project.toml"),
        format!(
            "[project]\nid = \"example.com/{tag}\"\nversion = \"0.1.0\"\n\n\
             [lib]\nname = \"gx\"\nkind = [\"staticlib\", \"cdylib\"]\n"
        ),
    )
    .expect("write the manifest");
    std::fs::write(dir.join("src/lib.gos"), source).expect("write the library");
    dir
}

fn gos_build(dir: &Path) -> std::process::Output {
    Command::new(gos_binary())
        .current_dir(dir)
        .args(["build", "--release"])
        .output()
        .expect("run gos build")
}

const DRIVER: &str = r#"
#include <stdio.h>
#include "gx.h"

#ifdef _WIN32
#include <windows.h>
static DWORD WINAPI work(LPVOID arg) {
    long long id = (long long)(size_t)arg, total = 0;
    for (int i = 0; i < 1000; i++) total += gx_add(i, (int32_t)id);
    return (DWORD)(total % 1000003);
}
static long long run_threads(void) {
    HANDLE threads[4];
    long long total = 0;
    for (size_t i = 0; i < 4; i++) threads[i] = CreateThread(NULL, 0, work, (LPVOID)i, 0, NULL);
    for (int i = 0; i < 4; i++) {
        DWORD code;
        WaitForSingleObject(threads[i], INFINITE);
        GetExitCodeThread(threads[i], &code);
        total += code;
        CloseHandle(threads[i]);
    }
    return total;
}
#else
#include <pthread.h>
static void *work(void *arg) {
    long long id = (long long)(size_t)arg, total = 0;
    for (int i = 0; i < 1000; i++) total += gx_add(i, (int32_t)id);
    return (void *)(size_t)(total % 1000003);
}
static long long run_threads(void) {
    pthread_t threads[4];
    long long total = 0;
    for (size_t i = 0; i < 4; i++) pthread_create(&threads[i], NULL, work, (void *)i);
    for (int i = 0; i < 4; i++) {
        void *code;
        pthread_join(threads[i], &code);
        total += (long long)(size_t)code;
    }
    return total;
}
#endif

int main(int argc, char **argv) {
    V2 v = {3.0, 4.0};
    V2 s = gx_scale(v, 2.0);
    int32_t xs[4] = {1, 2, 3, 4};
    printf("%lld %g %g %g %lld\n", (long long)gx_add(2, 3), gx_norm(v), s.x, s.y,
           (long long)gx_sum(xs, 4));
    long long first = gx_remember(1);
    long long second = gx_remember(2);
    printf("%lld %lld\n", first, second);
    printf("threads %lld\n", run_threads());
    gx_on_exit();
    fflush(stdout);
    gx_shutdown();
    if (argc > 1) {
        fflush(stdout);
        printf("%d\n", gx_fail(atoi(argv[1])));
    }
    return 0;
}
"#;

/// The four threads' sums of `gx_add(i, id)` over `i < 1000`, each reduced
/// as the driver reduces it.
fn thread_total() -> i64 {
    (0..4i64)
        .map(|id| (0..1000i64).map(|i| i + id).sum::<i64>() % 1_000_003)
        .sum()
}

/// The library's artifacts, built under `dir/target/release`.
struct Artifacts {
    out: PathBuf,
}

impl Artifacts {
    fn header(&self) -> PathBuf {
        self.out.join("include").join("gx.h")
    }

    fn shared(&self) -> PathBuf {
        self.out.join(if cfg!(windows) {
            "gx.dll"
        } else if cfg!(target_os = "macos") {
            "libgx.dylib"
        } else {
            "libgx.so"
        })
    }

    fn archive(&self) -> PathBuf {
        self.out.join(if cfg!(all(windows, target_env = "msvc")) {
            "gx.lib"
        } else {
            "libgx.a"
        })
    }
}

fn build_library(tag: &str) -> (PathBuf, Artifacts) {
    let dir = library(tag, EXPORTS);
    let built = gos_build(&dir);
    assert!(
        built.status.success(),
        "gos build failed:\n{}{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    let artifacts = Artifacts {
        out: dir.join("target").join("release"),
    };
    (dir, artifacts)
}

/// Compiles the C driver against the shared library (`shared`) or the
/// static archive, answering the program's path.
fn compile_driver(dir: &Path, artifacts: &Artifacts, shared: bool) -> PathBuf {
    let source = dir.join("driver.c");
    std::fs::write(&source, format!("#include <stdlib.h>\n{DRIVER}")).expect("write the driver");
    let name = if shared {
        "driver_shared"
    } else {
        "driver_static"
    };
    let exe = dir.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    });
    let include = artifacts.out.join("include");
    let mut cmd = c_compiler();
    if cfg!(all(windows, target_env = "msvc")) {
        cmd.arg("/nologo").arg("/MD").arg(&source);
        cmd.arg(format!("/I{}", include.display()));
        cmd.arg(format!("/Fe:{}", exe.display()));
        cmd.arg(format!("/Fo:{}\\", dir.display()));
        if shared {
            cmd.arg(artifacts.out.join("gx.dll.lib"));
        } else {
            cmd.arg(artifacts.archive());
            for lib in [
                "advapi32.lib",
                "bcrypt.lib",
                "kernel32.lib",
                "ntdll.lib",
                "userenv.lib",
                "ws2_32.lib",
                "synchronization.lib",
                "dbghelp.lib",
            ] {
                cmd.arg(lib);
            }
        }
    } else {
        cmd.arg(&source).arg("-I").arg(&include).arg("-o").arg(&exe);
        if shared {
            cmd.arg(format!("-L{}", artifacts.out.display()))
                .arg("-lgx");
        } else {
            cmd.arg(artifacts.archive());
        }
        if cfg!(windows) {
            for lib in ["ws2_32", "bcrypt", "advapi32", "userenv", "ntdll"] {
                cmd.arg(format!("-l{lib}"));
            }
        } else {
            cmd.arg("-lpthread").arg("-lm");
            if cfg!(target_os = "linux") {
                cmd.arg("-ldl");
            }
        }
    }
    let out = cmd.output().expect("run the C compiler");
    assert!(
        out.status.success(),
        "compiling the driver failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    exe
}

#[cfg(windows)]
fn c_compiler() -> Command {
    let target = if cfg!(target_arch = "aarch64") {
        "aarch64-pc-windows-msvc"
    } else {
        "x86_64-pc-windows-msvc"
    };
    cc::Build::new()
        .target(target)
        .host(target)
        .opt_level(1)
        .cargo_metadata(false)
        .get_compiler()
        .to_command()
}

#[cfg(not(windows))]
fn c_compiler() -> Command {
    Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".to_string()))
}

fn run_driver(exe: &Path, artifacts: &Artifacts, args: &[&str]) -> std::process::Output {
    let var = if cfg!(windows) {
        "PATH"
    } else if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    let mut paths = vec![artifacts.out.clone()];
    if let Some(existing) = std::env::var_os(var) {
        paths.extend(std::env::split_paths(&existing));
    }
    Command::new(exe)
        .args(args)
        .env(
            var,
            std::env::join_paths(paths).expect("join the loader path"),
        )
        .output()
        .unwrap_or_else(|e| panic!("run {}: {e}", exe.display()))
}

fn expected_driver_output() -> String {
    format!("5 25 6 8 10\n2 3\nthreads {}\nexit hook\n", thread_total())
}

#[test]
fn a_library_writes_a_header_declaring_its_exports_and_structs() {
    let (_dir, artifacts) = build_library("header");
    let header = std::fs::read_to_string(artifacts.header()).expect("read the header");
    for line in [
        "typedef struct V2 {",
        "    double x;",
        "int64_t gx_add(int64_t a, int32_t b);",
        "double gx_norm(V2 v);",
        "V2 gx_scale(V2 v, double k);",
        "int64_t gx_sum(int32_t * data, size_t len);",
        "int32_t gx_fail(int32_t n);",
        "void gx_shutdown(void);",
    ] {
        assert!(
            header.contains(line),
            "the header lacks `{line}`:\n{header}"
        );
    }
}

#[test]
fn a_shared_library_serves_a_c_program_on_its_own_threads() {
    let (dir, artifacts) = build_library("shared");
    assert!(
        artifacts.shared().is_file(),
        "no shared library was written"
    );
    let exe = compile_driver(&dir, &artifacts, true);
    let out = run_driver(&exe, &artifacts, &[]);
    assert!(
        out.status.success(),
        "the driver failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
        expected_driver_output()
    );
}

#[test]
fn a_static_archive_links_into_a_c_program() {
    let (dir, artifacts) = build_library("static");
    assert!(artifacts.archive().is_file(), "no archive was written");
    let exe = compile_driver(&dir, &artifacts, false);
    let out = run_driver(&exe, &artifacts, &[]);
    assert!(
        out.status.success(),
        "the driver failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
        expected_driver_output()
    );
}

#[test]
fn a_panic_in_an_exported_function_ends_the_host_with_the_report() {
    let (dir, artifacts) = build_library("panic");
    let exe = compile_driver(&dir, &artifacts, true);
    let out = run_driver(&exe, &artifacts, &["3"]);
    assert_eq!(out.status.code(), Some(101), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("too many: 3"), "{stderr}");
}

#[cfg(not(windows))]
#[test]
fn the_shared_library_exports_only_the_exported_symbols() {
    let (_dir, artifacts) = build_library("symbols");
    let mut cmd = Command::new("nm");
    if cfg!(target_os = "macos") {
        cmd.arg("-gU");
    } else {
        cmd.args(["-D", "--defined-only"]);
    }
    let out = cmd.arg(artifacts.shared()).output().expect("run nm");
    let listing = String::from_utf8_lossy(&out.stdout);
    let mut names: Vec<&str> = listing
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .map(|name| name.trim_start_matches('_'))
        .filter(|name| !name.is_empty())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "gx_add",
            "gx_fail",
            "gx_norm",
            "gx_on_exit",
            "gx_remember",
            "gx_scale",
            "gx_shutdown",
            "gx_sum"
        ],
        "{listing}"
    );
}

#[test]
fn a_library_root_with_main_is_refused() {
    let dir = library(
        "main",
        "#[export]\nfn gx_one() -> i32 { 1 }\nfn main() {}\n",
    );
    let out = gos_build(&dir);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("declares `fn main`"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_library_without_exports_is_refused() {
    let dir = library("bare", "pub fn one() -> i32 { 1 }\n");
    let out = gos_build(&dir);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("exports nothing"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
