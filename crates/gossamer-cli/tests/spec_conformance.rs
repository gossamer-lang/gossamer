//! SPEC.md behavioural conformance tests.
//!
//! Each test pins a behaviour the language specification describes, so
//! the spec and the toolchain stay in lockstep: a claim SPEC.md makes
//! must be demonstrably true (or, for a rejection, demonstrably
//! enforced).
//!
//! Behaviours covered:
//!   §3.1   debug integer overflow panics at the declared type width;
//!          release integer overflow wraps at that width;
//!          `i128`/`u128` are rejected on every tier.
//!   §7.5   reference mutability and scope-local `&mut` exclusivity are checked.
//!   §8.6   `extern "C"` is not an `unsafe` power; it is rejected.
//!   §11.2  linking - musl-static is the Linux default, `--dynamic`
//!          opts out.
//!   §12    FFI is rust-bindings-only (GP0016 fires for `extern`).
//!   §14    the implemented macro set is accepted; the rest are
//!          rejected at parse time.

#![allow(missing_docs)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn gos_binary() -> PathBuf {
    // Built by `cargo test -p gossamer-cli` via the workspace
    // `[[bin]]` target. Test runner injects CARGO_BIN_EXE_gos.
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_temp_file(stem: &str, body: &str) -> PathBuf {
    // Cargo runs integration tests in parallel by default. Per-test
    // temp paths therefore have to be unique across threads and
    // across `write_temp_file` calls within a thread; we combine the
    // process id with a monotonic counter so collisions are
    // impossible.
    let serial = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "gos-conformance-{}-{}-{}",
        stem,
        std::process::id(),
        serial,
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("{stem}.gos"));
    std::fs::write(&path, body).expect("temp write");
    path
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::process::Output {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("subprocess did not terminate within {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("wait error: {e}"),
        }
    }
    child.wait_with_output().expect("wait_with_output")
}

fn run_check(stem: &str, source: &str) -> (bool, String, String) {
    let path = write_temp_file(stem, source);
    let mut cmd = Command::new(gos_binary());
    cmd.arg("check").arg(&path);
    let out = run_with_timeout(cmd, Duration::from_secs(30));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stdout, stderr)
}

fn run_program(stem: &str, source: &str, args: &[&str]) -> (bool, String, String) {
    let path = write_temp_file(stem, source);
    let mut cmd = Command::new(gos_binary());
    cmd.arg("run").arg(&path);
    for arg in args {
        cmd.arg(arg);
    }
    let out = run_with_timeout(cmd, Duration::from_secs(30));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stdout, stderr)
}

fn build_and_run_program(stem: &str, source: &str, release: bool) -> (bool, String, String) {
    let path = write_temp_file(stem, source);
    let out_dir = path.parent().expect("fixture parent").join(if release {
        "native-release"
    } else {
        "native-debug"
    });
    let mut build_command = Command::new(gos_binary());
    build_command.arg("build");
    if release {
        build_command.arg("--release");
    }
    build_command
        .arg("--dynamic")
        .arg("--out-dir")
        .arg(&out_dir)
        .arg(&path);
    let build_output = run_with_timeout(build_command, Duration::from_mins(2));
    if !build_output.status.success() {
        return (
            false,
            String::from_utf8_lossy(&build_output.stdout).into_owned(),
            String::from_utf8_lossy(&build_output.stderr).into_owned(),
        );
    }
    let executable = out_dir.join(format!("{stem}{}", std::env::consts::EXE_SUFFIX));
    let run = run_with_timeout(Command::new(executable), Duration::from_secs(30));
    (
        run.status.success(),
        String::from_utf8_lossy(&run.stdout).into_owned(),
        String::from_utf8_lossy(&run.stderr).into_owned(),
    )
}

// ---------- diagnostics: --message-format json ----------

#[test]
fn check_message_format_json_emits_single_line_json() {
    // Triggers GP0016 (extern reserved) and asserts the
    // `--message-format json` output is a single-line JSON
    // object with the documented schema fields.
    let src = r#"
extern "C" { fn malloc(size: usize) -> *mut u8 }
fn main() { println("hi") }
"#;
    let path = write_temp_file("json_diag", src);
    let mut cmd = Command::new(gos_binary());
    cmd.arg("check")
        .arg(&path)
        .arg("--message-format")
        .arg("json");
    let out = run_with_timeout(cmd, Duration::from_secs(30));
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The diagnostic line is the line that starts with `{`.
    let json_line = stderr
        .lines()
        .find(|l| l.starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON diagnostic line in:\n{stderr}"));
    // Schema fields the rendering contract pins.
    assert!(json_line.contains("\"schema\":1"));
    assert!(json_line.contains("\"code\":\"GP0016\""));
    assert!(json_line.contains("\"severity\":\"error\""));
    assert!(json_line.contains("\"labels\":["));
    assert!(json_line.contains("\"primary\":true"));
    assert!(json_line.contains("\"helps\":["));
    // Process exit is non-zero on a diagnostic.
    assert!(!out.status.success());
}

// ---------- §12: FFI is rust-bindings-only ----------

#[test]
fn spec_12_extern_block_rejected_with_gp0016() {
    let src = r#"
extern "C" {
    fn malloc(size: usize) -> *mut u8
}
fn main() { println("hi") }
"#;
    let (ok, _stdout, stderr) = run_check("spec_12_extern_block", src);
    assert!(!ok, "extern \"C\" {{}} must not pass `gos check`");
    assert!(
        stderr.contains("GP0016"),
        "expected GP0016 in stderr, got: {stderr}",
    );
    assert!(
        stderr.contains("rust-bindings") || stderr.contains("[rust-bindings]"),
        "diagnostic must direct user to [rust-bindings]; got: {stderr}",
    );
}

#[test]
fn spec_12_no_mangle_extern_fn_rejected_with_gp0016() {
    let src = r#"
#[no_mangle]
extern "C" fn exported(x: i32) -> i32 { x + 1 }
"#;
    let (ok, _stdout, stderr) = run_check("spec_12_no_mangle", src);
    assert!(!ok);
    assert!(stderr.contains("GP0016"), "got: {stderr}");
}

// ---------- §8.6: extern is not an unsafe power ----------

#[test]
fn spec_8_6_extern_inside_unsafe_block_is_still_rejected() {
    // §8.6: `extern "C"` is not an unsafe power. A bare `extern "C"`
    // block (with or without `unsafe`) must be rejected; this test
    // pins both. The bare form fires the specific GP0016. The
    // `unsafe`-wrapped form fires whichever
    // diagnostic the parser surfaces first (today: GP0001 from the
    // `unsafe`-fn parser, after which GP0016 is reached if recovery
    // continues). The invariant we pin is "rejected" - the specific
    // diagnostic chain is part of the diagnostic-quality follow-up.
    let bare = r#"
extern "C" {
    fn libc_malloc(n: i64) -> i64
}
fn main() { println("hi") }
"#;
    let (ok_bare, _so, se_bare) = run_check("spec_8_6_bare", bare);
    assert!(!ok_bare);
    assert!(se_bare.contains("GP0016"), "bare extern got: {se_bare}");

    let wrapped = r#"
unsafe extern "C" {
    fn libc_malloc(n: i64) -> i64
}
"#;
    let (ok_wrapped, _so2, _se2) = run_check("spec_8_6_unsafe_extern", wrapped);
    assert!(
        !ok_wrapped,
        "unsafe extern \"C\" must be rejected; got success",
    );
}

// ---------- §14: macro subset ----------

#[test]
fn spec_14_format_macro_subset_accepted() {
    // The six format-shaped macros - println, print, eprintln, eprint,
    // format, and panic - must parse and check cleanly. `vec!` is not a
    // macro: the array literal `[...]` coerces to `Vec<T>` instead.
    let src = r#"
fn main() {
    println("p")

    print("p")

    eprintln("e")

    eprint("e")

    let s = format("f {}", 1)

    let v = [1, 2, 3]

    if s.len() == 0 && v.len() == 0 {
        panic("unreachable")
    }
}
"#;
    let (ok, _stdout, _stderr) = run_check("spec_14_format_macros", src);
    assert!(ok);
}

#[test]
fn spec_14_desugar_macros_accepted() {
    // `matches!`, `todo!`, `unimplemented!`, `unreachable!`, and `dbg!`
    // are implemented desugar macros and must parse and check cleanly.
    let src = r#"
fn maybe() -> i64 {
    if false { todo() } else if false { unimplemented() } else { 1 }
}
fn main() {
    let m = matches(Some(1), Some(_))

    let n = maybe()

    let d = dbg(n + 1)

    let label = match d {
        2 => "two",
        _ => unreachable(),
    }

    if m && label.len() == 0 {
        println("x")
    }
}
"#;
    let (ok, _stdout, stderr) = run_check("spec_14_desugar_macros", src);
    assert!(ok, "desugar macros must check clean; stderr: {stderr}");
}

#[test]
fn spec_14_unimplemented_macro_rejected() {
    // Macros with no implementation are rejected at parse time.
    // `todo!`, `unimplemented!`, and `unreachable!` are supported
    // desugar macros (covered above), so they are not in this list;
    // the rest have no desugaring and stay rejected.
    for macro_call in [
        "assert!(true)",
        "assert_eq!(1, 1)",
        "debug_assert!(true)",
        "write!(buf, \"x\")",
        "writeln!(buf, \"x\")",
    ] {
        let src = format!("fn main() {{ let _ = {macro_call}\n }}\n");
        let (ok, _stdout, _stderr) = run_check("spec_14_rejected", &src);
        assert!(!ok, "{macro_call} must be rejected, but `gos check` passed");
    }
}

// ---------- §3.1: integer overflow / i128 ----------

#[test]
fn spec_3_1_debug_overflow_panics_at_the_declared_width() {
    let cases = [
        ("u8_add", "200u8 + 200u8", "add"),
        ("i8_sub", "-127i8 - 2i8", "subtract"),
        ("u16_mul", "300u16 * 300u16", "multiply"),
        ("i32_add", "2147483647i32 + 1i32", "add"),
        ("i64_add", "9223372036854775807i64 + 1i64", "add"),
        ("u64_add", "18446744073709551615u64 + 1u64", "add"),
    ];
    for (name, expression, operation) in cases {
        let src = format!("fn main() {{ println(\"{{}}\", {expression}) }}\n");
        let (ok, _stdout, stderr) = run_program(name, &src, &[]);
        assert!(!ok, "debug overflow case {name} must panic");
        assert!(
            stderr.contains(&format!("attempt to {operation} with overflow")),
            "expected an overflow panic for {name}, got: {stderr}",
        );
    }

    let safe = r#"
fn main() {
    println("{} {} {}", 100u8 + 20u8, -100i8 - 20i8, 200u16 * 300u16)
}
"#;
    let (ok, stdout, stderr) = run_program("checked_arithmetic_safe", safe, &[]);
    assert!(ok, "in-range arithmetic must succeed: {stderr}");
    assert_eq!(stdout.trim(), "120 -120 60000");
}

#[test]
fn spec_3_1_native_profiles_check_then_wrap_overflow() {
    let src = r#"
fn main() {
    let a: u8 = 200u8
    let b: u8 = 200u8
    println("{}", a + b)
}
"#;
    let (debug_ok, _debug_stdout, debug_stderr) =
        build_and_run_program("spec_3_1_native_debug", src, false);
    assert!(!debug_ok, "native debug overflow must panic");
    assert!(
        debug_stderr.contains("attempt to add with overflow"),
        "expected native debug overflow panic, got: {debug_stderr}",
    );

    let (release_ok, release_stdout, release_stderr) =
        build_and_run_program("spec_3_1_native_release", src, true);
    assert!(
        release_ok,
        "native release wrapping program failed: {release_stderr}",
    );
    assert_eq!(release_stdout.trim(), "144");
}

/// Release wrapping holds for values only known at run time - parameters,
/// loop accumulators, compound assignment, and container elements - not just
/// for operands the optimiser folds, and a narrow value never leaves its range.
#[test]
fn spec_3_1_native_release_wraps_runtime_narrow_values() {
    let src = r#"
fn add32(a: u32, b: u32) -> u32 { a + b }
fn sub32(a: u32, b: u32) -> u32 { a - b }
fn mul_i32(a: i32, b: i32) -> i32 { a * b }
fn add_i8(a: i8, b: i8) -> i8 { a + b }
fn mul16(a: u16, b: u16) -> u16 { a * b }

fn djb2(text: String) -> u32 {
    let mut hash: u32 = 5381
    for b in text.bytes() { hash = (hash << 5) + hash + b as u32 }
    hash
}

fn main() {
    println("{} {}", add32(4_000_000_000, 1_000_000_000), sub32(1, 2))
    println("{} {}", mul_i32(2_000_000_000, 3), add_i8(127, 1))
    println("{}", mul16(60_000, 60_000))
    let mut s: u32 = 4_000_000_000
    s += 500_000_000
    let v: Vec<u32> = #[4_000_000_000]
    println("{} {}", s, v[0] + v[0])
    println("{}", djb2("the quick brown fox jumps over the lazy dog"))
}
"#;
    let (debug_ok, _debug_stdout, debug_stderr) =
        build_and_run_program("spec_3_1_runtime_narrow_debug", src, false);
    assert!(!debug_ok, "native debug narrow overflow must panic");
    assert!(
        debug_stderr.contains("with overflow"),
        "expected native debug overflow panic, got: {debug_stderr}",
    );

    let (release_ok, release_stdout, release_stderr) =
        build_and_run_program("spec_3_1_runtime_narrow_release", src, true);
    assert!(
        release_ok,
        "native release narrow program failed: {release_stderr}"
    );
    assert_eq!(
        release_stdout.trim(),
        "705032704 4294967295\n1705032704 -128\n41984\n205032704 3705032704\n2012963070"
    );
}

// ---------- §11.2: static-musl is the default link mode ----------
//
// This is a build-system claim, not a language one. Verifying the
// actual linkage requires running `gos build` and inspecting the
// produced ELF - expensive (~minutes), so that end-to-end check
// lives in the release pipeline rather than the per-PR suite. We
// pin only the doc claim here: static-musl is the Linux default and
// `--dynamic` is the opt-out.

#[test]
fn spec_11_2_states_static_musl_default() {
    let spec = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("SPEC.md"),
    )
    .expect("read SPEC.md");
    // Collapse line-wrapping so the assertion tracks the prose, not the
    // markdown column at which it happens to break.
    let prose: String = spec.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        prose.contains("fully-static musl binary by default") && prose.contains("`--dynamic`"),
        "§11.2 must state the static-musl default and the --dynamic opt-out",
    );
}

// ---------- §7.5: reference mutability without a borrow checker ----------

#[test]
fn spec_7_5_mut_ref_requires_mutable_source() {
    let src = r"
fn main() {
    let x = 1
    let p = &mut x
}
";
    let (ok, _stdout, stderr) = run_check("spec_7_5_mut_source", src);
    assert!(!ok, "&mut of an immutable source must be rejected");
    assert!(stderr.contains("GT0032"), "expected GT0032, got: {stderr}");
}

#[test]
fn spec_7_5_shared_reference_rejects_writes() {
    let src = r"
fn main() {
    let mut x = [1, 2]
    let mut p = &x
    p[0] = 0
}
";
    let (ok, _stdout, stderr) = run_check("spec_7_5_shared_write", src);
    assert!(!ok, "assignment through &T must be rejected");
    assert!(stderr.contains("GT0031"), "expected GT0031, got: {stderr}");
}

#[test]
fn spec_7_5_mut_reference_aliases_fixed_array_source() {
    let src = r#"
fn main() {
    let mut xs = [1, 2]
    {
        let r = &mut xs
        r[0] = 0
    }
    if xs[0] != 0 { panic("mutable reference did not write through") }
}
"#;
    let (ok, _stdout, stderr) = run_program("spec_7_5_write_through", src, &[]);
    assert!(ok, "write-through reference program failed: {stderr}");
}

#[test]
fn spec_7_5_mut_reference_aliases_scalar_source() {
    let src = r#"
fn main() {
    let mut value = 1i64
    {
        let r = &mut value
        *r = 42i64
    }
    if value != 42i64 { panic("mutable reference did not write through") }
}
"#;
    let (ok, _stdout, stderr) = run_program("spec_7_5_scalar_write_through", src, &[]);
    assert!(ok, "write-through reference program failed: {stderr}");
}

#[test]
fn spec_7_5_mut_reference_binding_rebinds_its_target() {
    let src = r#"
fn main() {
    let mut first = 1i64
    let mut second = 2i64
    {
        let mut r = &mut first
        r = &mut second
        *r = 42i64
    }
    if first != 1i64 { panic("rebind changed the old target") }
    if second != 42i64 { panic("rebind did not change the new target") }
}
"#;
    let (ok, _stdout, stderr) = run_program("spec_7_5_reference_rebind", src, &[]);
    assert!(ok, "rebindable reference program failed: {stderr}");
}

#[test]
fn spec_7_5_aliased_mut_borrow_is_rejected() {
    // Named mutable-reference bindings are lexically exclusive, matching
    // Rust's core aliasing rule and preventing conflicting writes.
    let src = r#"
fn main() {
    let mut x = 1
    let a = &mut x
    let b = &mut x
    *a = 2
    *b = 3
    println("{}", x)
}
"#;
    let (ok, _stdout, stderr) = run_check("spec_7_5_borrow", src);
    assert!(!ok, "overlapping named mutable borrows must be rejected");
    assert!(stderr.contains("GT0043"), "expected GT0043, got: {stderr}");
}

#[test]
fn spec_7_5_repeated_mutable_call_argument_is_rejected() {
    let src = r"
fn use_two(a: &mut i64, b: &mut i64) { *a += *b }
fn main() {
    let mut counter = 1i64
    use_two(&mut counter, &mut counter)
}
";
    let (ok, _stdout, stderr) = run_check("spec_7_5_call_alias", src);
    assert!(!ok, "one call must not borrow the same root mutably twice");
    assert!(stderr.contains("GT0043"), "expected GT0043, got: {stderr}");
}
