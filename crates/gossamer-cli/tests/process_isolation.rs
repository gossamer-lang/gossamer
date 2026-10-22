//! Process isolation: a panic in a spawned goroutine terminates only that
//! goroutine - the process keeps running and exits cleanly - while a panic on
//! the main goroutine stays fatal (isolation is goroutine-scoped, not
//! panic-swallowing). Verified on BOTH the bytecode VM (`gos`) and the
//! native binary (`gos build`), since isolation is a runtime/scheduler
//! property that must hold identically on every tier.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        // On Windows the built binary is `<stem>.exe`; the `.gos` source in
        // the same dir is excluded by the extension.
        p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    }
}

// A goroutine that panics immediately; main waits long enough for the
// goroutine to run and crash, then prints a sentinel. If the goroutine panic
// were fatal, the process would die before the sentinel and exit non-zero.
const PANICKING_GOROUTINE: &str = r#"
use std::time

fn boom() {
    panic("intentional goroutine crash")
}

fn main() {
    spawn(|| boom())
    time::sleep(200)
    println("MAIN_SURVIVED")
}
"#;

// A panic on the main goroutine must stay fatal, just like Rust's `fn main`.
const PANICKING_MAIN: &str = r#"
fn main() {
    println("BEFORE_PANIC")
    panic("fatal main crash")
}
"#;

fn write_src(tag: &str, src: &str) -> (PathBuf, PathBuf) {
    let dir = env::temp_dir().join(format!("gos-iso-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join(format!("{tag}.gos"));
    std::fs::write(&source, src).unwrap();
    (dir, source)
}

fn run_vm(source: &Path) -> Output {
    Command::new(gos_bin())
        .arg("run")
        .arg(source)
        .output()
        .expect("spawn gos")
}

fn build_and_run(dir: &Path, source: &Path) -> Output {
    let build = Command::new(gos_bin())
        .arg("build")
        .arg("--out-dir")
        .arg(dir)
        .arg(source)
        .output()
        .expect("spawn gos build");
    assert!(
        build.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bin = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| is_executable(p))
        .expect("no built binary");
    Command::new(&bin).output().expect("run native")
}

fn assert_goroutine_panic_is_isolated(out: &Output, tier: &str) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The goroutine actually panicked (so isolation was genuinely exercised,
    // not skipped because the goroutine never ran).
    assert!(
        stderr.contains("intentional goroutine crash"),
        "[{tier}] expected the goroutine panic on stderr (isolation not exercised); stderr: {stderr:?}"
    );
    // ...yet the process survived and ran to completion.
    assert!(
        out.status.success(),
        "[{tier}] process exited non-zero after an ISOLATED goroutine panic; stderr: {stderr:?}"
    );
    assert!(
        stdout.contains("MAIN_SURVIVED"),
        "[{tier}] main did not continue past the goroutine panic; stdout: {stdout:?}"
    );
}

#[test]
fn goroutine_panic_is_isolated_on_vm() {
    let (dir, source) = write_src("goro-vm", PANICKING_GOROUTINE);
    let out = run_vm(&source);
    let _ = std::fs::remove_dir_all(&dir);
    assert_goroutine_panic_is_isolated(&out, "vm");
}

#[test]
fn goroutine_panic_is_isolated_native() {
    let (dir, source) = write_src("goro-native", PANICKING_GOROUTINE);
    let out = build_and_run(&dir, &source);
    let _ = std::fs::remove_dir_all(&dir);
    assert_goroutine_panic_is_isolated(&out, "native");
}

#[test]
fn main_goroutine_panic_stays_fatal_on_vm() {
    let (dir, source) = write_src("main-vm", PANICKING_MAIN);
    let out = run_vm(&source);
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("BEFORE_PANIC"),
        "main should run up to the panic; stdout: {stdout:?}"
    );
    assert!(
        !out.status.success(),
        "a panic on the MAIN goroutine must stay fatal (isolation is goroutine-scoped, not panic-swallowing)"
    );
    assert_eq!(
        out.status.code(),
        Some(101),
        "main-goroutine panic exit code is pinned to 101 (Rust parity; scripts depend on it)"
    );
}

#[test]
fn main_goroutine_panic_stays_fatal_native() {
    let (dir, source) = write_src("main-native", PANICKING_MAIN);
    let out = build_and_run(&dir, &source);
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("BEFORE_PANIC"),
        "main should run up to the panic; stdout: {stdout:?}"
    );
    assert!(
        !out.status.success(),
        "a panic on the MAIN goroutine must stay fatal in the native binary"
    );
    assert_eq!(
        out.status.code(),
        Some(101),
        "main-goroutine panic exit code is pinned to 101 (Rust parity; scripts depend on it)"
    );
}

/// `panic = "abort"` in any workspace profile silently breaks goroutine
/// isolation and `join()` Err delivery (unwinding is load-bearing). The
/// setting sat in the workspace manifest from 0.0.0 until 0.11.1 and
/// broke both in release builds; this guard prevents reintroduction.
#[test]
fn no_panic_abort_in_any_workspace_profile() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let text = std::fs::read_to_string(&root).expect("read workspace Cargo.toml");
    let mut in_profile = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_profile = trimmed.starts_with("[profile");
            continue;
        }
        if in_profile {
            let no_spaces: String = trimmed.split_whitespace().collect();
            assert!(
                !no_spaces.starts_with("panic=\"abort\""),
                "panic = \"abort\" found in a workspace profile: unwinding is \
                 load-bearing for goroutine isolation and join() Err delivery"
            );
        }
    }
}
