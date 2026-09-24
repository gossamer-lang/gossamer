//! LLVM AOT coverage regression gates.
//!
//! Each test below writes a tiny Gossamer program, builds it with
//! `gos build --release` (the LLVM-AOT pipeline), runs the produced
//! binary, and asserts the exact stdout. A failure means LLVM AOT
//! has diverged from language semantics for that feature.
//!
//! These cover the broken features found in the 0.9.0 LLVM-tier
//! audit. Each test uses an API that has been verified to work in
//! the bytecode VM (`gos`), so a red light here means the
//! LLVM-tier wiring has fallen behind.

#![allow(missing_docs)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::needless_raw_string_hashes)]

use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fresh_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-llvm-aot-{pid}-{n}-{name}",
        pid = std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

struct Program {
    dir: PathBuf,
    bin: PathBuf,
}

impl Drop for Program {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn build_release(name: &str, body: &str) -> Program {
    let dir = fresh_dir(name);
    let source = dir.join(format!("{name}.gos"));
    std::fs::write(&source, body).expect("write source");
    let out = Command::new(gos_bin())
        .arg("build")
        .arg("--release")
        .arg(&source)
        .output()
        .expect("spawn gos build --release");
    assert!(
        out.status.success(),
        "gos build --release {name} failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let bin = dir
        .join("target")
        .join("release")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(bin.exists(), "release binary missing at {}", bin.display());
    Program { dir, bin }
}

fn run(prog: &Program) -> (i32, String, String) {
    let out = Command::new(&prog.bin).output().expect("run binary");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn assert_release_stdout_eq(name: &str, body: &str, expected: &str) {
    let prog = build_release(name, body);
    let (code, stdout, stderr) = run(&prog);
    assert_eq!(
        code, 0,
        "{name}: exit={code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert_eq!(
        stdout, expected,
        "{name}: stdout drift\n--- expected ---\n{expected}\n--- actual ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}

// ===============================================================
// strconv free-fn dispatch - `parse_*` return Result<T,Error>,
// `format_*` return String directly.
// ===============================================================

#[test]
fn aot_strconv_parse_i64() {
    assert_release_stdout_eq(
        "strconv_parse_i64",
        r#"
use std::strconv
use std::errors
fn parse(s: String) -> Result<i64, errors::Error> {
    let n = strconv::parse_i64(s)?
    Ok(n)
}
fn main() {
    if let Ok(n) = parse("42") { println("n={}", n) }
}
"#,
        "n=42\n",
    );
}

#[test]
fn aot_strconv_parse_f64() {
    assert_release_stdout_eq(
        "strconv_parse_f64",
        r#"
use std::strconv
use std::errors
fn parse(s: String) -> Result<f64, errors::Error> {
    let n = strconv::parse_f64(s)?
    Ok(n)
}
fn main() {
    if let Ok(n) = parse("3.5") { println("n={}", n) }
}
"#,
        "n=3.5\n",
    );
}

#[test]
fn aot_strconv_parse_bool() {
    assert_release_stdout_eq(
        "strconv_parse_bool",
        r#"
use std::strconv
use std::errors
fn parse(s: String) -> Result<bool, errors::Error> {
    let b = strconv::parse_bool(s)?
    Ok(b)
}
fn main() {
    if let Ok(b) = parse("true") { println("b={}", b) }
}
"#,
        "b=true\n",
    );
}

#[test]
fn aot_strconv_format_i64() {
    assert_release_stdout_eq(
        "strconv_format_i64",
        r#"
use std::strconv
fn main() {
    println("s={}", strconv::format_i64(123))
}
"#,
        "s=123\n",
    );
}

#[test]
fn aot_strconv_format_f64() {
    assert_release_stdout_eq(
        "strconv_format_f64",
        r#"
use std::strconv
fn main() {
    println("s={}", strconv::format_f64(2.5))
}
"#,
        "s=2.5\n",
    );
}

#[test]
fn aot_integer_abs_uses_integer_lowering() {
    assert_release_stdout_eq(
        "integer_abs",
        r#"
fn main() {
    let x: i64 = -42
    println("{}", x.abs())
}
"#,
        "42\n",
    );
}

// ===============================================================
// strings free-fn dispatch - every entry has a `gos_rt_str_*`
// runtime shim, the MIR free-fn table just doesn't route to it.
// ===============================================================

#[test]
fn aot_strings_trim() {
    assert_release_stdout_eq(
        "strings_trim",
        r#"
use std::strings
fn main() {
    println("[{}]", strings::trim("  hi  "))
}
"#,
        "[hi]\n",
    );
}

#[test]
fn aot_strings_split() {
    assert_release_stdout_eq(
        "strings_split",
        r#"
use std::strings
fn main() {
    let xs = strings::split("a,b,c", ",")
    println("n={}", xs.len())
}
"#,
        "n=3\n",
    );
}

#[test]
fn aot_strings_to_upper() {
    assert_release_stdout_eq(
        "strings_to_upper",
        r#"
use std::strings
fn main() {
    println("{}", strings::to_uppercase("hi"))
}
"#,
        "HI\n",
    );
}

#[test]
fn aot_strings_to_lower() {
    assert_release_stdout_eq(
        "strings_to_lower",
        r#"
use std::strings
fn main() {
    println("{}", strings::to_lowercase("HI"))
}
"#,
        "hi\n",
    );
}

#[test]
fn aot_strings_contains() {
    assert_release_stdout_eq(
        "strings_contains",
        r#"
use std::strings
fn main() {
    println("b={}", strings::contains("hello", "ell"))
}
"#,
        "b=true\n",
    );
}

#[test]
fn aot_strings_replace() {
    assert_release_stdout_eq(
        "strings_replace",
        r#"
use std::strings
fn main() {
    println("{}", strings::replace("aaa", "a", "b"))
}
"#,
        "bbb\n",
    );
}

#[test]
fn aot_strings_starts_with() {
    assert_release_stdout_eq(
        "strings_starts_with",
        r#"
use std::strings
fn main() {
    println("b={}", strings::starts_with("hello", "he"))
}
"#,
        "b=true\n",
    );
}

#[test]
fn aot_strings_ends_with() {
    assert_release_stdout_eq(
        "strings_ends_with",
        r#"
use std::strings
fn main() {
    println("b={}", strings::ends_with("hello", "lo"))
}
"#,
        "b=true\n",
    );
}

#[test]
fn aot_strings_lines() {
    assert_release_stdout_eq(
        "strings_lines",
        r#"
use std::strings
fn main() {
    let xs = strings::lines("a\nb\nc")
    println("n={}", xs.len())
}
"#,
        "n=3\n",
    );
}

#[test]
fn aot_strings_find() {
    assert_release_stdout_eq(
        "strings_find",
        r#"
use std::strings
fn main() {
    if let Some(i) = strings::find("hello", "ll") {
        println("i={}", i)
    }
}
"#,
        "i=2\n",
    );
}

#[test]
fn aot_strings_repeat() {
    assert_release_stdout_eq(
        "strings_repeat",
        r#"
use std::strings
fn main() {
    println("{}", strings::repeat("ab", 3))
}
"#,
        "ababab\n",
    );
}

#[test]
fn aot_strings_trim_start() {
    assert_release_stdout_eq(
        "strings_trim_start",
        r#"
use std::strings
fn main() {
    println("[{}]", strings::trim_start("  hi"))
}
"#,
        "[hi]\n",
    );
}

#[test]
fn aot_strings_trim_end() {
    assert_release_stdout_eq(
        "strings_trim_end",
        r#"
use std::strings
fn main() {
    println("[{}]", strings::trim_end("hi  "))
}
"#,
        "[hi]\n",
    );
}

// ===============================================================
// math free-fn dispatch - extended trig / log / round entries.
// ===============================================================

#[test]
fn aot_math_atan2() {
    assert_release_stdout_eq(
        "math_atan2",
        r#"
use std::math
fn main() {
    println("{}", math::atan2(0.0, 1.0))
}
"#,
        "0\n",
    );
}

#[test]
fn aot_math_log10() {
    assert_release_stdout_eq(
        "math_log10",
        r#"
use std::math
fn main() {
    println("{}", math::log10(1000.0))
}
"#,
        "3\n",
    );
}

#[test]
fn aot_math_tan() {
    assert_release_stdout_eq(
        "math_tan",
        r#"
use std::math
fn main() {
    println("{}", math::tan(0.0))
}
"#,
        "0\n",
    );
}

#[test]
fn aot_math_round() {
    assert_release_stdout_eq(
        "math_round",
        r#"
use std::math
fn main() {
    println("{}", math::round(2.7))
}
"#,
        "3\n",
    );
}

// ===============================================================
// path/env/fs/crypto free-fn dispatch.
// ===============================================================

#[test]
fn aot_path_parent() {
    assert_release_stdout_eq(
        "path_parent",
        r#"
use std::path
fn main() {
    if let Some(p) = path::parent("/a/b/c") {
        println("{}", p)
    }
}
"#,
        "/a/b\n",
    );
}

#[test]
fn aot_path_stem() {
    assert_release_stdout_eq(
        "path_stem",
        r#"
use std::path
fn main() {
    if let Some(s) = path::file_stem("/a/file.txt") {
        println("{}", s)
    }
}
"#,
        "file\n",
    );
}

#[test]
fn aot_path_file_name() {
    assert_release_stdout_eq(
        "path_file_name",
        r#"
use std::path
fn main() {
    if let Some(s) = path::file_name("/a/file.txt") {
        println("{}", s)
    }
}
"#,
        "file.txt\n",
    );
}

#[test]
fn aot_env_set_var() {
    assert_release_stdout_eq(
        "env_set_var",
        r#"
use std::env
fn main() {
    env::set_var("AOT_TEST_KEY", "hi")
    if let Some(v) = env::var("AOT_TEST_KEY") {
        println("v={}", v)
    }
}
"#,
        "v=hi\n",
    );
}

#[test]
fn aot_env_program_name() {
    // `env::program_name()` returns the executable's basename.
    // The Program is built into `<dir>/target/release/env_program_name`,
    // so the value should end with "env_program_name".
    let prog = build_release(
        "env_program_name",
        r#"
use std::env
fn main() {
    let n = env::program_name()
    println("{}", n)
}
"#,
    );
    let (code, stdout, stderr) = run(&prog);
    assert_eq!(code, 0, "exit={code} stderr={stderr}");
    // The binary is `env_program_name` on unix and `env_program_name.exe`
    // on Windows; `program_name()` reports the real argv[0], so accept both.
    let name = stdout.trim_end();
    let name = name.strip_suffix(".exe").unwrap_or(name);
    assert!(
        name.ends_with("env_program_name"),
        "program_name stdout={stdout:?}"
    );
}

#[test]
fn aot_fs_metadata() {
    assert_release_stdout_eq(
        "fs_metadata",
        r#"
use std::fs
fn main() {
    if let Ok(_m) = fs::metadata(".") {
        println("ok")
    }
}
"#,
        "ok\n",
    );
}

#[test]
fn aot_crypto_rand_bytes() {
    // 16 bytes random - content non-deterministic; assert len only.
    assert_release_stdout_eq(
        "crypto_rand_bytes",
        r#"
use std::crypto
use std::errors
fn main() -> Result<(), errors::Error> {
    let b = crypto::rand::bytes(16)?
    println("n={}", b.len())
    Ok(())
}
"#,
        "n=16\n",
    );
}

#[test]
fn aot_crypto_rand_bytes_rejects_negative_count() {
    assert_release_stdout_eq(
        "crypto_rand_bytes_negative",
        r#"
use std::crypto
fn main() {
    match crypto::rand::bytes(-1) {
        Ok(_) => println("unexpected"),
        Err(_) => println("err"),
    }
}
"#,
        "err\n",
    );
}

// ===============================================================
// iter / option / result data-last combinators threading through
// the forward-pipe.
// ===============================================================

#[test]
fn aot_iter_sum_by() {
    assert_release_stdout_eq(
        "iter_sum_by",
        r#"
use std::iter
fn main() {
    let xs = [1, 2, 3]
    let total = xs |> |v| iter::sum_by(v, |n| n*2)
    println("total={}", total)
}
"#,
        "total=12\n",
    );
}

#[test]
fn aot_option_map() {
    assert_release_stdout_eq(
        "option_map",
        r#"
use std::option
fn main() {
    let o = Some(2)
    let m = o |> |v| option::map(v, |n| n + 1)
    if let Some(v) = m { println("v={}", v) }
}
"#,
        "v=3\n",
    );
}

#[test]
fn aot_result_map() {
    assert_release_stdout_eq(
        "result_map",
        r#"
use std::result
use std::errors
fn main() {
    let r: Result<i64, errors::Error> = Ok(2)
    let m = r |> |v| result::map(v, |n| n + 1)
    if let Ok(v) = m { println("v={}", v) }
}
"#,
        "v=3\n",
    );
}

// ===============================================================
// Method dispatch fallthroughs.
// ===============================================================

#[test]
fn aot_hashmap_contains() {
    assert_release_stdout_eq(
        "hashmap_contains",
        r#"
use std::collections::Map
fn main() {
    let mut m: Map<String, i64> = Map::new()
    m.insert("a", 1)
    println("b={}", m.contains("a"))
}
"#,
        "b=true\n",
    );
}

#[test]
fn aot_btreemap_get() {
    assert_release_stdout_eq(
        "btreemap_get",
        r#"
use std::collections::BTreeMap
fn main() {
    let mut m: BTreeMap<String, i64> = BTreeMap::new()
    m.insert("a", 5)
    if let Some(v) = m.get("a") { println("v={}", v) }
}
"#,
        "v=5\n",
    );
}

#[test]
fn aot_btreemap_contains() {
    assert_release_stdout_eq(
        "btreemap_contains",
        r#"
use std::collections::BTreeMap
fn main() {
    let mut m: BTreeMap<String, i64> = BTreeMap::new()
    m.insert("a", 5)
    println("b={}", m.contains("a"))
}
"#,
        "b=true\n",
    );
}

// ===============================================================
// Sync types - atomic widths beyond i64.
// ===============================================================

#[test]
fn aot_atomic_bool() {
    // `load()` answers the `bool` an `AtomicBool` holds, which renders as
    // one on every tier.
    assert_release_stdout_eq(
        "atomic_bool",
        r#"
use std::sync
fn main() {
    let a = sync::AtomicBool::new(false)
    a.store(true)
    let v = a.load()
    if v { println("v=true {}", v) } else { println("v=false") }
}
"#,
        "v=true true\n",
    );
}

#[test]
fn aot_atomic_u64() {
    assert_release_stdout_eq(
        "atomic_u64",
        r#"
use std::sync
fn main() {
    let a = sync::AtomicU64::new(0)
    a.fetch_add(5)
    println("v={}", a.load())
}
"#,
        "v=5\n",
    );
}

// ===============================================================
// time::Duration static methods (`as_millis`).
// ===============================================================

#[test]
fn aot_duration_as_millis() {
    assert_release_stdout_eq(
        "duration_as_millis",
        r#"
use std::time
fn main() {
    let d = time::Duration::from_secs(2)
    println("ms={}", time::Duration::as_millis(d))
}
"#,
        "ms=2000\n",
    );
}

// ===============================================================
// LLVM SIGSEGV / silent-miscompile gates - these tests catch the
// worst category: clean build, runtime crash.
// ===============================================================

#[test]
fn aot_vec_push_runtime() {
    assert_release_stdout_eq(
        "vec_push_runtime",
        r#"
fn main() {
    let mut xs = Vec::from([1, 2, 3])
    xs.push(4)
    println("len={} last={}", xs.len(), xs[3])
}
"#,
        "len=4 last=4\n",
    );
}

#[test]
fn aot_vec_sort_runtime() {
    assert_release_stdout_eq(
        "vec_sort_runtime",
        r#"
fn main() {
    let mut xs = [3, 1, 2]
    xs.sort()
    println("{} {} {}", xs[0], xs[1], xs[2])
}
"#,
        "1 2 3\n",
    );
}

#[test]
fn aot_iter_for_each_runtime() {
    assert_release_stdout_eq(
        "iter_for_each_runtime",
        r#"
use std::iter
fn main() {
    let xs = [10, 20, 30]
    xs |> |v| iter::for_each(v, |n| println("n={}", n))
}
"#,
        "n=10\nn=20\nn=30\n",
    );
}
