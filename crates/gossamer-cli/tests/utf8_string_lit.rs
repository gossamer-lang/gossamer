//! UTF-8 fidelity through string literals + `format!()` templates.
//!
//! Regression coverage for the 2026-05-07 report: a `format!()`
//! template containing a 3-byte UTF-8 code point printed as `â`
//! followed by two control codes because the parse-time
//! `parse_format_template` walk pushed every UTF-8 byte as a
//! separate `char`, double-encoding multi-byte sequences. The bug
//! first surfaced with an em dash; any 3-byte code point exercises
//! the same path, so these tests use the euro sign (`€`, U+20AC,
//! 3 UTF-8 bytes). Each test asserts that the same source produces
//! byte-identical stdout across `gos` (VM), `gos build`
//! (Cranelift), and `gos build --release` (LLVM) and matches the
//! expected UTF-8 output.

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
        "gos-utf8-{pid}-{n}-{tag}",
        pid = std::process::id()
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
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
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

fn build_native(src: &Path, release: bool, scratch: &Path) -> Result<PathBuf, String> {
    let mut cmd = Command::new(gos_bin());
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--out-dir").arg(scratch).arg(src);
    let out = cmd.output().expect("spawn gos build");
    if !out.status.success() {
        return Err(format!(
            "gos build {flag} failed:\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            flag = if release { "--release" } else { "" },
        ));
    }
    let mut binaries = Vec::new();
    for entry in fs::read_dir(scratch)
        .map_err(|e| format!("read_dir: {e}"))?
        .flatten()
    {
        let p = entry.path();
        if p.is_file() && is_executable(&p) {
            binaries.push(p);
        }
    }
    binaries
        .into_iter()
        .next()
        .ok_or_else(|| format!("no binary in {}", scratch.display()))
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

fn assert_three_tier_utf8(tag: &str, source: &str, expected: &str) {
    let dir = fresh_dir(tag);
    let src = dir.join(format!("{tag}.gos"));
    let mut f = fs::File::create(&src).expect("write src");
    f.write_all(source.as_bytes()).unwrap();
    drop(f);

    let vm = run_vm(&src);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let cl_bin = build_native(&src, false, &cl_dir).expect("cranelift build");
    let cl = run_native(&cl_bin);
    let ll_dir = dir.join("ll");
    fs::create_dir_all(&ll_dir).unwrap();
    let ll_bin = build_native(&src, true, &ll_dir).expect("llvm build");
    let ll = run_native(&ll_bin);

    let _ = fs::remove_dir_all(&dir);

    for (name, run) in [("vm", &vm), ("cranelift", &cl), ("llvm", &ll)] {
        assert_eq!(
            run.0.trim_end(),
            expected.trim_end(),
            "[{tag}/{name}] stdout disagrees with expected.\n\
             expected (bytes):\n{exp_b:?}\n\
             expected (text):\n{expected}\n\
             got (bytes):\n{got_b:?}\n\
             got (text):\n{stdout}\n\
             stderr:\n{stderr}\n\
             exit: {code:?}",
            exp_b = expected.as_bytes(),
            got_b = run.0.as_bytes(),
            stdout = run.0,
            stderr = run.1,
            code = run.2,
        );
    }
}

#[test]
fn multibyte_in_format_template_round_trips_uncorrupted() {
    // The 2026-05-07 report: a format!() template with a 3-byte
    // UTF-8 code point printed as `â\u{80}\u{94}` because the
    // parse-time template walk pushed each UTF-8 byte as its own
    // char, double-encoding 3-byte code points into 6-byte garbage.
    // The euro sign is 3 UTF-8 bytes, exercising the same path.
    let src = r#"
fn main() {
    let balance = 5
    let msg = format("balance is €{} after fees", balance)
    println("{}", msg)
}
"#;
    let expected = "balance is €5 after fees";
    assert_three_tier_utf8("multibyte_format", src, expected);
}

#[test]
fn bare_string_and_open_start_range_iteration_have_three_tier_parity() {
    let src = r#"
fn main() {
    for c in "ąčęėšž" { println(c) }
    for i in ..3 { println(i) }
}
"#;
    let expected = "ą\nč\nę\nė\nš\nž\n0\n1\n2";
    assert_three_tier_utf8("direct_iteration", src, expected);
}

#[test]
fn range_binding_iteration_has_three_tier_parity() {
    let src = r"
fn main() {
    let a = 0..3
    for i in a { println(i) }
}
";
    assert_three_tier_utf8("bound_range_iteration", src, "0\n1\n2");
}

#[test]
fn ascii_only_format_template_round_trips_uncorrupted() {
    // Sanity check that the same dispatch path doesn't perturb
    // ASCII-only templates - the fix must not regress the
    // common case.
    let src = r#"
fn main() {
    let n = 42
    let msg = format("answer is {}", n)
    println("{}", msg)
}
"#;
    assert_three_tier_utf8("ascii_format", src, "answer is 42");
}

#[test]
fn multibyte_glyphs_in_println_round_trip() {
    // String literal with assorted multi-byte UTF-8 chars
    // (euro sign, trademark, copyright, ellipsis, accented latin).
    // Each 3-byte and 2-byte char path must survive parse-time +
    // lower-time + runtime emission untouched.
    let src = r#"
fn main() {
    println("€ ™ © … é á")
}
"#;
    assert_three_tier_utf8("multibyte_glyphs", src, "€ ™ © … é á");
}

#[test]
fn unicode_in_format_with_named_arg_round_trips() {
    let src = r#"
fn main() {
    let n = 7
    println("{n} € total")
}
"#;
    assert_three_tier_utf8("named_arg_unicode", src, "7 € total");
}
