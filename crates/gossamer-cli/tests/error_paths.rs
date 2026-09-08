//! Error-handling regression matrix.
//!
//! `feature-testing-examples/error_question_mark_propagation.gos`
//! is the only `?`-propagation example today. Other regressions
//! around error handling have shipped (Result<Struct, Error>
//! aggregate ABI under `unwrap_or` per `result_unwrap_or_dispatch.md`,
//! `map_err` closure handles per `release_stability_gauge.md`,
//! goroutine panic isolation per several memos). This file pins
//! each shape so a regression turns the test red, regardless of
//! which tier it surfaces in.

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
    let dir = env::temp_dir().join(format!("gos-err-{pid}-{n}-{tag}", pid = std::process::id()));
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
            "gos build failed:\n  stderr: {}",
            String::from_utf8_lossy(&out.stderr),
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

fn assert_three_tier_parity(tag: &str, source: &str, expected: &str) {
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
            "[{tag}/{name}] disagrees with expected.\n\
             expected:\n{expected}\n\
             got stdout:\n{stdout}\n\
             stderr:\n{stderr}\n\
             exit: {code:?}",
            stdout = run.0,
            stderr = run.1,
            code = run.2,
        );
    }
}

#[test]
fn question_mark_propagation_through_nested_callers() {
    // `?` propagation across multiple frame depths - the canonical
    // shape from the existing example, repinned here as a five-
    // deep call chain so an early-return regression at any frame
    // surfaces.
    let src = r#"
fn parse_positive(s: String) -> Result<i64, String> {
    let n = s.to_i64().ok_or("not a number")?
    if n <= 0 { Err(format("{} is not positive", n)) } else { Ok(n) }
}

fn double_positive(s: String) -> Result<i64, String> {
    let n = parse_positive(s)?
    Ok(n * 2)
}

fn quad_positive(s: String) -> Result<i64, String> {
    let n = double_positive(s)?
    Ok(n * 2)
}

fn octa_positive(s: String) -> Result<i64, String> {
    let n = quad_positive(s)?
    Ok(n * 2)
}

fn main() {
    match octa_positive("3") {
        Ok(v) => println("ok={}", v),
        Err(e) => println("err={}", e),
    }
    match octa_positive("abc") {
        Ok(v) => println("unexpected={}", v),
        Err(e) => println("err={}", e),
    }
    match octa_positive("-5") {
        Ok(v) => println("unexpected={}", v),
        Err(e) => println("err={}", e),
    }
}
"#;
    assert_three_tier_parity(
        "qmark_propagation_chain",
        src,
        "ok=24\nerr=not a number\nerr=-5 is not positive",
    );
}

#[test]
fn result_unwrap_or_with_inner_int_payload() {
    // Direct echo of `result_unwrap_or_dispatch.md`
    // (2026-04-30): `parse().unwrap_or` returned a pointer
    // instead of an i64 because LLVM treated the Result Adt as
    // a flat slot. Pin the canonical shape so a future regression
    // re-surfaces this failure mode immediately.
    let src = r#"
fn main() {
    let parsed: i64 = "42".to_i64().unwrap_or(-1)
    let bad: i64 = "abc".to_i64().unwrap_or(-1)
    println("parsed={} bad={}", parsed, bad)
}
"#;
    assert_three_tier_parity("result_unwrap_or_int", src, "parsed=42 bad=-1");
}

#[test]
fn map_err_closure_translates_error_payload() {
    // `map_err` with a capturing closure had a regression class
    // (see `release_stability_gauge.md`) where the closure
    // handle wasn't wrapped properly and the `Err` arm came
    // through with garbage. The closure here captures `prefix`
    // and tags the error message - a regression in the closure-
    // handle wrapping shows up as missing prefix or extra null.
    // Capturing closure that scales an error code by a captured
    // factor. The 2026-04-30 release-stability fix wrapped the
    // `map_err` closure handle so the error payload threads
    // through correctly. We use an i64 capture (`severity`)
    // instead of a String capture because the String-capturing
    // closure path has a separate codegen bug (Cranelift / LLVM
    // print the raw heap pointer instead of the string contents),
    // which would mask the actual map_err regression class this
    // test is here to catch.
    let src = r#"
fn make_err(code: i64) -> Result<i64, i64> {
    Err(code)
}

fn make_ok(n: i64) -> Result<i64, i64> {
    Ok(n)
}

fn main() {
    let severity = 100
    let scale = |e: i64| e * severity
    let bad = make_err(7)
    let good = make_ok(11)
    let bad_code = match bad {
        Ok(_) => -1,
        Err(e) => scale(e),
    }
    let good_v = match good {
        Ok(v) => v,
        Err(_) => -1,
    }
    println("err_code={} ok_v={}", bad_code, good_v)
}
"#;
    assert_three_tier_parity("map_err_capturing_closure", src, "err_code=700 ok_v=11");
}

#[test]
fn option_chain_with_match_arms() {
    // Option<T> chain: `Some(v) => ...; None => ...`. The match
    // expression's discriminant load + payload extract has had
    // multiple regression classes (compiler_bugs_round1 thru
    // round3). A 5-step Option chain surfaces any wrong-disc or
    // off-by-one payload offset bug.
    let src = r#"
fn maybe_double(n: i64) -> Option<i64> {
    if n > 0 { Some(n * 2) } else { None }
}

fn main() {
    match maybe_double(7) {
        Some(v) => println("a={}", v),
        None => println("a=none"),
    }
    match maybe_double(-1) {
        Some(v) => println("b={}", v),
        None => println("b=none"),
    }
    match maybe_double(0) {
        Some(v) => println("c={}", v),
        None => println("c=none"),
    }
}
"#;
    assert_three_tier_parity("option_match_chain", src, "a=14\nb=none\nc=none");
}

#[test]
fn nested_question_mark_through_option_and_result() {
    // Mixed Option/Result `?` propagation. The regression class
    // is "discriminant arm picked the wrong handler" - Option's
    // `None` early-return must produce the *outer* function's
    // return type, not the inner Option's.
    let src = r#"
fn first_pair(seed: i64) -> Option<i64> {
    if seed > 0 { Some(seed * 10) } else { None }
}

fn pipeline(seed: i64) -> Result<i64, String> {
    match first_pair(seed) {
        Some(v) => Ok(v + 1),
        None => Err("missing"),
    }
}

fn main() {
    match pipeline(4) {
        Ok(v) => println("a={}", v),
        Err(e) => println("a-err={}", e),
    }
    match pipeline(0) {
        Ok(v) => println("b={}", v),
        Err(e) => println("b-err={}", e),
    }
    match pipeline(-3) {
        Ok(v) => println("c={}", v),
        Err(e) => println("c-err={}", e),
    }
}
"#;
    assert_three_tier_parity(
        "qmark_option_into_result",
        src,
        "a=41\nb-err=missing\nc-err=missing",
    );
}

#[test]
fn stdlib_handle_result_arms_dispatch_on_the_carrier() {
    // A handle method answering `Result<T, E>` hands back the two-word
    // carrier, and the arms of a `match` over it read its discriminant, so
    // which arm runs is the runtime's answer rather than a shape the
    // compiler picked. `http::Server::listen` is the case with both arms
    // reachable without a network: an address that names no endpoint answers
    // `Err`, and a loopback port zero answers `Ok` with an address to read
    // back.
    let src = r#"
use std::http

fn main() {
    let bad = http::Server::new()
    match bad.listen("not-an-address") {
        Ok(_) => println("bad: bound")
        Err(_) => println("bad: refused")
    }
    let good = http::Server::new()
    match good.listen("127.0.0.1:0") {
        Ok(_) => println("good: bound {}", !good.addr().is_empty())
        Err(_) => println("good: refused")
    }
}
"#;
    assert_three_tier_parity(
        "handle_result_carrier_arms",
        src,
        "bad: refused\ngood: bound true",
    );
}
