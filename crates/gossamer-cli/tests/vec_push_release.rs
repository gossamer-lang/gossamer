//! `Vec::push` + indexing parity across the three tiers.
//!
//! Regression coverage for the 2026-05-07 daemon-launch +
//! tool-calling report: the LLVM tier emitted
//! `gos_rt_vec_push(vec, i64_value)` for `v.push(x)` calls,
//! passing the i64 value where the helper expected a `*const u8`
//! pointer. The helper then `memcpy`'d from the address-shaped-
//! like-the-value (`SEGV_MAPERR` at the value bits), so any
//! release-tier program that pushed onto a Vec at runtime
//! crashed. The Cranelift backend already had a stack-slot dance
//! for the same symbol; this test gates parity.

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
    let dir = env::temp_dir().join(format!("gos-vp-{pid}-{n}-{tag}", pid = std::process::id()));
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
            "gos build {flag} failed:\n  stderr: {}",
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

fn assert_three_tier_stdout(tag: &str, source: &str, expected: &str) {
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
fn empty_vec_push_then_index_works_in_all_tiers() {
    // The smallest repro - `let mut v: Vec<i64> = Vec::from([]);
    // v.push(42); v[0]`. The empty-vec path used to allocate a
    // zero-cap `GosVec` whose backing buffer was null; the
    // first push grew it but the LLVM tier passed the i64
    // value where the helper expected a pointer, so the
    // memcpy crashed at `si_addr=42`.
    let src = r#"
fn main() {
    let mut v: Vec<i64> = Vec::from([])
    v.push(42)
    println("v[0]={}", v[0])
}
"#;
    assert_three_tier_stdout("empty_vec_push_index", src, "v[0]=42");
}

#[test]
fn core_method_contract_mutators_work_in_all_tiers() {
    let src = r#"
fn main() {
    let mut xs: Vec<i64> = [1, 2].to_vec()
    xs.extend([3, 4])
    println("len1={}", xs.len())
    xs.truncate(3)
    println("len2={}", xs.len())
    xs.clear()
    println("len3={}", xs.len())

    let mut ys: Vec<i64> = [5].to_vec()
    ys.extend_from_slice([6, 7])
    println("y2={}", ys[2])

    let mut words: Vec<String> = ["a"].to_vec()
    let more: Vec<String> = ["b"].to_vec()
    words.extend(more)
    println("word={}", words[1])

    let mut s = String::from("hello")
    s.truncate(3)
    println("s={}", s)
    s.clear()
    println("slen={}", s.len())

    let ok = String::from_utf8([104, 105])
    println("utf8={}", ok.unwrap())
    let bad = String::from_utf8([255])
    println("bad={}", bad.is_err())
}
"#;
    assert_three_tier_stdout(
        "core_method_contract_mutators",
        src,
        "len1=4\nlen2=3\nlen3=0\ny2=7\nword=b\ns=hel\nslen=0\nutf8=hi\nbad=true",
    );
}

#[test]
fn vec_from_preserves_nested_fixed_array_elements_in_all_tiers() {
    let src = r#"
fn main() {
    let mut values = Vec::from([[0, 0]])
    values.pop()
    let mut i = 0
    while i < 30 {
        values.push([i, i + 1])
        i += 1
    }
    let mut sum = 0
    for value in values {
        sum += value[0] + value[1]
    }
    println("{}", sum)
}
"#;
    assert_three_tier_stdout("vec_from_nested_fixed_arrays", src, "900");
}

#[test]
fn vec_push_string_then_index_works_in_all_tiers() {
    // String elements take the same dispatch - the runtime's
    // `gos_rt_vec_push` writes the i64-shaped pointer through
    // a stack slot. Catches a regression where the slot's
    // i64 cast loses the pointer's bytes.
    let src = r#"
fn main() {
    let mut v: Vec<String> = Vec::from([])
    v.push("hello")
    v.push("world")
    println("{},{}", v[0], v[1])
}
"#;
    assert_three_tier_stdout("vec_push_string_index", src, "hello,world");
}

#[test]
fn vec_push_in_loop_then_render_via_ref_param_works_in_all_tiers() {
    // Mirrors the SSE-streaming + tool-call accumulator shape:
    // build a Vec<String> via `push`, then pass it as `&[String]`
    // to a renderer that indexes each element.
    let src = r#"
fn render(ids: [String]) -> String {
    let mut out = ""
    let mut i: i64 = 0
    let n = ids.len() as i64
    while i < n {
        out = format("{}{},", out, ids[i])
        i += 1
    }
    out
}

fn main() {
    let mut v: Vec<String> = Vec::from([])
    let mut i: i64 = 0
    while i < 5 {
        v.push(format("item_{}", i))
        i += 1
    }
    let rendered = render(v)
    println("{}", rendered)
}
"#;
    assert_three_tier_stdout(
        "vec_push_loop_render",
        src,
        "item_0,item_1,item_2,item_3,item_4,",
    );
}
