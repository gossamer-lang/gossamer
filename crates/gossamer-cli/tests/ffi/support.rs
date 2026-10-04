//! Building the C fixture library, laying out projects that bind it, and
//! running their programs on every tier.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

pub(crate) fn gos_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

/// What a run printed, and how it ended.
pub(crate) struct Outcome {
    pub(crate) ok: bool,
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Outcome {
    fn from(out: &Output) -> Self {
        Self {
            ok: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Everything the run printed, for assertion messages.
    pub(crate) fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// The tiers a program runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// `gos run` with the JIT off.
    Bytecode,
    /// `gos run` with the JIT compiling at a function's first call.
    Jit,
    /// `gos build --release`, then the binary.
    Native,
}

pub(crate) const TIERS: [Tier; 3] = [Tier::Bytecode, Tier::Jit, Tier::Native];

/// The file names the fixture library is built as on this platform.
fn fixture_files() -> &'static [&'static str] {
    if cfg!(windows) {
        &["gosffi.dll", "gosffi.lib"]
    } else if cfg!(target_os = "macos") {
        &["libgosffi.dylib"]
    } else {
        &["libgosffi.so"]
    }
}

/// The directory the fixture library is built into, once per test process.
fn fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("gos-ffi-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ffi/fixture/gosffi.c");
        build_shared_library(&source, &dir);
        dir
    })
}

#[cfg(not(windows))]
fn build_shared_library(source: &Path, dir: &Path) {
    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let output = dir.join(fixture_files()[0]);
    let mut cmd = Command::new(compiler);
    cmd.arg("-shared")
        .arg("-fPIC")
        .arg("-O1")
        .arg(source)
        .arg("-o")
        .arg(&output);
    if cfg!(target_os = "linux") {
        cmd.arg("-lpthread");
    }
    if cfg!(target_os = "macos") {
        cmd.arg("-install_name").arg("@rpath/libgosffi.dylib");
    }
    let out = cmd.output().expect("run the C compiler");
    assert!(
        out.status.success(),
        "building the fixture library failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(windows)]
fn build_shared_library(source: &Path, dir: &Path) {
    let target = if cfg!(target_arch = "aarch64") {
        "aarch64-pc-windows-msvc"
    } else {
        "x86_64-pc-windows-msvc"
    };
    let tool = cc::Build::new()
        .target(target)
        .host(target)
        .opt_level(1)
        .cargo_metadata(false)
        .get_compiler();
    let mut cmd = tool.to_command();
    cmd.current_dir(dir)
        .arg("/nologo")
        .arg("/LD")
        .arg(source)
        .arg(format!("/Fe:{}", dir.join("gosffi.dll").display()));
    let out = cmd.output().expect("run the C compiler");
    assert!(
        out.status.success(),
        "building the fixture library failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The environment variable a platform's loader searches for shared
/// libraries.
fn loader_path_var() -> &'static str {
    if cfg!(windows) {
        "PATH"
    } else if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

/// A project on disk: `project.toml`, `src/main.gos`, and the fixture
/// library vendored under `native/`.
pub(crate) struct Project {
    pub(crate) dir: PathBuf,
    tag: String,
}

impl Project {
    /// A fresh project named `tag` whose manifest leaves `ffi` at its
    /// default, with `source` as its entry and the fixture library under
    /// `native/`.
    pub(crate) fn new(tag: &str, source: &str) -> Self {
        Self::with_manifest(tag, source, "")
    }

    /// As [`Project::new`] with `extra` in the `[project]` table.
    pub(crate) fn with_manifest(tag: &str, source: &str, extra: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("gos-ffi-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).expect("create the project");
        std::fs::create_dir_all(dir.join("native")).expect("create the native directory");
        std::fs::write(
            dir.join("project.toml"),
            format!("[project]\nid = \"example.com/{tag}\"\nversion = \"0.1.0\"\n{extra}"),
        )
        .expect("write the manifest");
        std::fs::write(dir.join("src/main.gos"), source).expect("write the entry");
        for file in fixture_files() {
            std::fs::copy(fixture_dir().join(file), dir.join("native").join(file))
                .expect("vendor the fixture library");
        }
        Self {
            dir,
            tag: tag.to_string(),
        }
    }

    fn loader_path(&self) -> std::ffi::OsString {
        let native = self.dir.join("native");
        let mut paths = vec![native];
        if let Some(existing) = std::env::var_os(loader_path_var()) {
            paths.extend(std::env::split_paths(&existing));
        }
        std::env::join_paths(paths).expect("join the loader path")
    }

    /// Runs `gos <args>` in the project.
    pub(crate) fn gos(&self, args: &[&str]) -> Outcome {
        let out = Command::new(gos_binary())
            .current_dir(&self.dir)
            .args(args)
            .env(loader_path_var(), self.loader_path())
            .output()
            .expect("run gos");
        Outcome::from(&out)
    }

    /// Runs the entry on `tier`.
    pub(crate) fn run_on(&self, tier: Tier) -> Outcome {
        match tier {
            Tier::Bytecode | Tier::Jit => {
                let mut cmd = Command::new(gos_binary());
                cmd.current_dir(&self.dir)
                    .arg("run")
                    .arg("src/main.gos")
                    .env(loader_path_var(), self.loader_path());
                if tier == Tier::Bytecode {
                    cmd.env("GOS_JIT", "0");
                } else {
                    cmd.env_remove("GOS_JIT").env("GOSSAMER_JIT_THRESHOLD", "1");
                }
                Outcome::from(&cmd.output().expect("run gos"))
            }
            Tier::Native => {
                let out_dir = self.dir.join("out");
                let built = Command::new(gos_binary())
                    .current_dir(&self.dir)
                    .arg("build")
                    .arg("--release")
                    .arg("--out-dir")
                    .arg(&out_dir)
                    .arg("src/main.gos")
                    .output()
                    .expect("run gos build");
                if !built.status.success() {
                    return Outcome::from(&built);
                }
                let name = if cfg!(windows) {
                    format!("{}.exe", self.tag)
                } else {
                    self.tag.clone()
                };
                let exe = out_dir.join(name);
                let out = Command::new(&exe)
                    .current_dir(&self.dir)
                    .env(loader_path_var(), self.loader_path())
                    .output()
                    .unwrap_or_else(|e| panic!("run {}: {e}", exe.display()));
                Outcome::from(&out)
            }
        }
    }

    /// Runs the entry on every tier, asserting each succeeds and prints
    /// `expected`.
    pub(crate) fn expect_everywhere(&self, expected: &str) {
        for tier in TIERS {
            let out = self.run_on(tier);
            assert!(out.ok, "{tier:?} failed:\n{}", out.all());
            assert_eq!(
                out.stdout.trim_end(),
                expected.trim_end(),
                "{tier:?} printed something else; stderr:\n{}",
                out.stderr
            );
        }
    }

    /// Runs the entry on every tier, asserting each ends with `code` and a
    /// report holding every one of `needles`.
    pub(crate) fn expect_fault_everywhere(&self, code: i32, needles: &[&str]) {
        for tier in TIERS {
            let out = self.run_on(tier);
            assert_eq!(
                out.code,
                Some(code),
                "{tier:?} ended otherwise:\n{}",
                out.all()
            );
            for needle in needles {
                assert!(
                    out.all().contains(needle),
                    "{tier:?} report lacks `{needle}`:\n{}",
                    out.all()
                );
            }
        }
    }
}

/// The extern block every fixture program starts with.
pub(crate) const FIXTURE_DECLS: &str = r#"use std::ffi
use std::ffi::Ptr

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    type Counter

    fn counter_new(start: ffi::c_int) -> Ptr<Counter>
    fn counter_add(c: Ptr<Counter>, amount: ffi::c_int)
    fn counter_get(c: Ptr<Counter>) -> ffi::c_int
    fn counter_reset(c: Ptr<Counter>)
    fn counter_resets(c: Ptr<Counter>) -> ffi::c_int
    fn counter_free(c: Ptr<Counter>)
    fn counter_find(key: ffi::c_int) -> Option<Ptr<Counter>>
    fn counter_open(start: ffi::c_int, out: &mut Option<Ptr<Counter>>) -> ffi::c_int
    fn split_pair(value: i64, high: &mut i32, low: &mut i32) -> ffi::c_int
    fn scale(factor: f64, value: &mut f64)
    fn counter_bytes(c: Ptr<Counter>, data: &mut Option<Ptr<u8>>, len: &mut ffi::size_t)
    fn buffer_make(len: ffi::size_t, seed: u8) -> Ptr<u8>
    fn buffer_free(data: Ptr<u8>)
    fn buffer_take_sum(data: Ptr<u8>, len: ffi::size_t) -> ffi::c_int
    fn buffer_fill(data: Ptr<u8>, len: ffi::size_t, seed: u8)
    fn greeting() -> Ptr<u8>
    fn slice_sum(s: Slice) -> ffi::c_int
    fn slice_fill(out: Ptr<Slice>)
    fn apply(f: Fn(ffi::c_int, Ptr<ffi::c_void>) -> ffi::c_int, x: ffi::c_int, ctx: Ptr<ffi::c_void>) -> ffi::c_int
    fn each(xs: [i32], n: ffi::size_t, f: Fn(i32, Ptr<ffi::c_void>), ctx: Ptr<ffi::c_void>)
    fn map_double(f: Fn(f64) -> f64, x: f64) -> f64
    fn map_float(f: Fn(f32) -> f32, x: f32) -> f32
    fn map_wide(f: Fn(i8, u16, i64, u8) -> i64, x: i64) -> i64
    fn map_pointer(f: Fn(Ptr<ffi::c_void>) -> Option<Ptr<ffi::c_void>>, p: Ptr<ffi::c_void>) -> Option<Ptr<ffi::c_void>>
    fn map_flag(f: Fn(bool) -> ffi::c_int, flag: ffi::c_int) -> ffi::c_int
    fn set_handler(f: Fn(ffi::c_int, Ptr<ffi::c_void>), ctx: Ptr<ffi::c_void>)
    fn fire(value: ffi::c_int)
    fn fire_on_thread(value: ffi::c_int)
    fn get_doubler() -> Ptr<ffi::c_void>
    fn get_halver() -> Ptr<ffi::c_void>
}

#[repr(C)]
struct Slice {
    data: Ptr<u8>,
    len: u64,
    tag: i32,
}
"#;

/// A fixture program: `body`'s imports, [`FIXTURE_DECLS`], then the rest
/// of `body`.
pub(crate) fn fixture_program(body: &str) -> String {
    let (imports, rest): (Vec<&str>, Vec<&str>) =
        body.lines().partition(|line| line.starts_with("use "));
    format!(
        "{}\n{FIXTURE_DECLS}\n{}",
        imports.join("\n"),
        rest.join("\n")
    )
}
