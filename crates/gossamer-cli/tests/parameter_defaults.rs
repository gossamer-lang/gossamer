//! Parameter defaults that hold data: variants, `None`, constructions,
//! collection literals, functions, and consts of any type. Each is spliced
//! into the calls that omit it, resolved where the function is declared, and
//! must print the same on the bytecode VM, the JIT, and a release binary.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("gos-defaults-{}-{n}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn stdout_of(what: &str, out: &Output) -> String {
    assert!(
        out.status.success(),
        "{what} failed ({:?}):\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

fn run(src: &Path, jit: bool) -> Output {
    let mut cmd = Command::new(gos_bin());
    cmd.arg("run").arg(src).stdin(Stdio::null());
    if jit {
        cmd.env("GOSSAMER_JIT_THRESHOLD", "1");
    }
    cmd.output().expect("spawn gos run")
}

fn release_binary(src: &Path, dir: &Path) -> PathBuf {
    let out_dir = dir.join("bin");
    let out = Command::new(gos_bin())
        .arg("build")
        .arg("--release")
        .arg("--out-dir")
        .arg(&out_dir)
        .arg(src)
        .stdin(Stdio::null())
        .output()
        .expect("spawn gos build");
    stdout_of("gos build --release", &out);
    let stem = src.file_stem().expect("source stem");
    out_dir
        .join(stem)
        .with_extension(env::consts::EXE_EXTENSION)
}

/// Runs `source` on the VM, the JIT, and as a release binary, and checks
/// each prints `expected`.
fn expect_on_every_tier(tag: &str, source: &str, expected: &str) {
    let dir = scratch(tag);
    let src = dir.join(format!("{tag}.gos"));
    fs::write(&src, source).expect("write source");
    assert_eq!(stdout_of("gos run", &run(&src, false)), expected, "VM");
    assert_eq!(
        stdout_of("gos run (JIT)", &run(&src, true)),
        expected,
        "JIT"
    );
    let bin = release_binary(&src, &dir);
    let native = Command::new(&bin)
        .stdin(Stdio::null())
        .output()
        .expect("spawn release binary");
    assert_eq!(stdout_of("release binary", &native), expected, "release");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn data_defaults_mean_what_they_mean_where_the_function_is_declared() {
    // The root declares its own `Profile`, `Size`, and `ORIGIN`; every call
    // still receives the module's.
    let source = r#"use gfx::open

mod gfx {
    pub enum Profile { Core, Compat, Es }

    pub struct Size { pub w: i64, pub h: i64 }

    pub const ORIGIN: Size = Size { w: 0, h: 0 }

    fn twice(x: i64) -> i64 { x * 2 }

    pub fn open(
        title: String,
        profile: Profile = Profile::Core,
        size: Size = Size { w: 800, h: 600 },
        at: Size = ORIGIN,
        parent: Option<String> = None,
        samples: Option<i64> = Some(4),
        tags: Vec<String> = #[],
        pair: (i64, bool) = (1, true),
        f: Fn(i64) -> i64 = twice,
        limit: i64 = i64::MAX,
    ) -> String {
        let p = match profile {
            Profile::Core => "core",
            Profile::Compat => "compat",
            Profile::Es => "es",
        }
        f"{title} {p} {size.w}x{size.h} at {at.w},{at.h} {parent} {samples} {tags.len()} {pair} {f(21)} {limit}"
    }
}

enum Profile { Other }
struct Size { w: i64 }
const ORIGIN: i64 = 5

fn main() {
    println(open("a"))
    println(open("b", profile: gfx::Profile::Es, samples: None))
    println(open("c", tags: #["x"], parent: Some("root")))
}
"#;
    expect_on_every_tier(
        "declared_scope",
        source,
        "a core 800x600 at 0,0 None Some(4) 0 (1, true) 42 9223372036854775807\n\
         b es 800x600 at 0,0 None None 0 (1, true) 42 9223372036854775807\n\
         c core 800x600 at 0,0 Some(\"root\") Some(4) 1 (1, true) 42 9223372036854775807\n",
    );
}

#[test]
fn methods_traits_aliases_and_generics_take_data_defaults() {
    // `A::go` and `B::go` default to different variants, so each call takes
    // the default of its receiver's type.
    let source = r#"use shapes::Kind as K

mod shapes {
    pub enum Kind { Round, Square }

    pub struct V { pub x: i64, pub y: i64 }

    impl V {
        pub const ZERO: V = V { x: 0, y: 0 }
    }

    pub fn describe(kind: Kind = Round, at: V = V::ZERO) -> String {
        let k = match kind {
            Kind::Round => "round",
            Kind::Square => "square",
        }
        f"{k} {at.x},{at.y}"
    }
}

enum Mode { Fast, Slow }
enum Level { Low, High }

struct A {}
struct B {}

impl A {
    fn go(&self, mode: Mode = Mode::Fast) -> String {
        match mode {
            Mode::Fast => "A fast",
            Mode::Slow => "A slow",
        }
    }
}

impl B {
    fn go(&self, mode: Mode = Mode::Slow) -> String {
        match mode {
            Mode::Fast => "B fast",
            Mode::Slow => "B slow",
        }
    }
}

trait Speaker {
    fn speak(&self, level: Level = Level::High) -> String
}

impl Speaker for A {
    fn speak(&self, level: Level = Level::High) -> String {
        match level {
            Level::Low => "quiet",
            Level::High => "LOUD",
        }
    }
}

fn first<T>(xs: Vec<T>, fallback: Option<T> = None) -> Option<T> {
    if xs.len() > 0 { Some(xs[0]) } else { fallback }
}

fn pick(kind: K = K::Square) -> String {
    shapes::describe(kind: kind)
}

fn table(m: Map<String, i64> = {"a": 1}, s: Set<i64> = #{7}, grid: [i64; 3] = [0; 3]) -> String {
    f"{m["a"]} {s.len()} {grid.len()}"
}

fn main() {
    println(shapes::describe())
    println(shapes::describe(at: shapes::V { x: 1, y: 2 }))
    println(pick())
    let a = A {}
    let b = B {}
    println(a.go())
    println(b.go())
    println(a.go(mode: Mode::Slow))
    println(a.speak())
    println(first(#[3, 4]))
    let empty: Vec<String> = #[]
    println(first(empty))
    println(first(#["x"], fallback: Some("y")))
    println(table())
}
"#;
    expect_on_every_tier(
        "methods_and_aliases",
        source,
        "round 0,0\nround 1,2\nsquare 0,0\nA fast\nB slow\nA slow\nLOUD\nSome(3)\nNone\n\
         Some(\"x\")\n1 1 3\n",
    );
}

#[test]
fn associated_consts_in_modules_read_their_own_module() {
    // Two modules each declare `V` with a `K`; every read reaches the one its
    // path, its module, or its import names.
    let source = r"use b::V

mod a {
    pub struct V { pub x: i64 }
    impl V { pub const K: V = V { x: 1 } }
    pub fn own() -> i64 { V::K.x }
    pub mod inner {
        pub fn up() -> i64 { super::V::K.x }
    }
}

mod b {
    pub struct V { pub x: i64 }
    impl V {
        pub const K: V = V { x: 2 }
        pub fn me() -> i64 { Self::K.x }
    }
    pub fn own() -> i64 { V::K.x }
}

fn main() {
    println(a::V::K.x)
    println(b::V::K.x)
    println(crate::a::V::K.x)
    println(V::K.x)
    println(a::own())
    println(b::own())
    println(a::inner::up())
    println(b::V::me())
}
";
    expect_on_every_tier(
        "assoc_consts_in_modules",
        source,
        "1\n2\n1\n2\n1\n2\n1\n2\n",
    );
}

#[test]
fn defaults_computed_at_run_time_are_rejected() {
    let source = r"struct P { x: i64 }
impl P {
    fn new(x: i64) -> P { P { x: x } }
}
fn make() -> i64 { 3 }
static mut COUNT: i64 = 0

fn a(x: i64, y: i64 = x + 1) -> i64 { y }
fn b(p: P = P::new(1)) -> i64 { p.x }
fn c(n: i64 = make()) -> i64 { n }
fn d(p: P = P { x: make() }) -> i64 { p.x }
fn e(n: i64 = COUNT) -> i64 { n }
fn f(g: Fn(i64) -> i64 = |v| v + 1) -> i64 { g(1) }
fn h(xs: Vec<i64> = #[make()]) -> i64 { xs.len() }

fn main() {
    println(a(1))
}
";
    let dir = scratch("rejected");
    let src = dir.join("rejected.gos");
    fs::write(&src, source).expect("write source");
    let out = Command::new(gos_bin())
        .arg("check")
        .arg(&src)
        .stdin(Stdio::null())
        .output()
        .expect("spawn gos check");
    assert!(!out.status.success(), "a run-time default was accepted");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<usize> = stderr
        .lines()
        .zip(stderr.lines().skip(1))
        .filter(|(head, _)| head.starts_with("error[GR0014]"))
        .filter_map(|(_, at)| at.rsplit(':').nth(1)?.parse().ok())
        .collect();
    assert_eq!(lines, [8, 9, 10, 11, 12, 13, 14], "{stderr}");
    assert!(
        !stderr.contains("GR0001"),
        "a default naming a parameter is not unresolved: {stderr}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_method_two_user_types_declare_differently_still_resolves_on_other_receivers() {
    // `AtomicI64::load` is no user type's, and `Catalog::load` takes no
    // receiver, so neither makes `COUNT.load()` ambiguous; `b.load(uri: ..)`
    // still fills its defaults from `Book`'s declaration.
    expect_on_every_tier(
        "method_name_shared_by_types",
        r#"use std::sync::AtomicI64

static COUNT: AtomicI64 = AtomicI64::new(0)

pub struct Catalog { pub name: String }

impl Catalog {
    pub fn load(file: String) -> Catalog { Catalog { name: file } }
}

pub struct Page { pub id: i64 }

impl Page {
    pub fn load(&self, uri: String) { println(f"{self.id} {uri}") }
}

pub struct Book { pub pages: i64 }

impl Book {
    pub fn load(&self, uri: String, depth: i64 = 1) { println(f"book {uri} {depth}") }
}

fn main() {
    COUNT.store(3)
    println(f"{COUNT.load()} {Catalog::load("c").name}")
    Page { id: 1 }.load("x")
    let b = Book { pages: 2 }
    b.load(uri: "y")
}
"#,
        "3 c\n1 x\nbook y 1\n",
    );
}
