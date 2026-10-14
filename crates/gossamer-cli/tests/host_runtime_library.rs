//! A `[lib] runtime = "host"` archive carries no runtime: its host links
//! the Gossamer runtime once, the library's calls resolve against it, and
//! the archive's only global definitions are its C entry points.

use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

const LIBRARY: &str = r#"use std::ffi
use std::ffi::Ptr

fn greeting(n: i64) -> String {
    let mut out = ""
    let joined = cohort {
        let worker = spawn(|| f"hello {n} from a goroutine")
        out = worker.join().unwrap_or("")
    }
    match joined {
        Ok(()) => out
        Err(e) => f"cohort failed: {e}"
    }
}

#[export]
fn hl_greet(n: i64) -> Ptr<u8> {
    let mut text = greeting(n).as_bytes()
    text.push(0)
    unsafe { ffi::to_c_bytes(text) }
}

#[export]
fn hl_sum(n: i64) -> i64 {
    let mut parts: Vec<String> = #[]
    for i in 0..n {
        parts.push(f"{i}")
    }
    parts.join(",").len()
}
"#;

const HOST: &str = r#"
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include "hl.h"

int64_t gos_lib_abi_fingerprint(void);

int main(void) {
    char *text = (char *)hl_greet(7);
    printf("%s\n", text);
    free(text);
    printf("%lld\n", (long long)hl_sum(12));
    printf("fingerprint %llu\n", (unsigned long long)gos_lib_abi_fingerprint());
    hl_shutdown();
    return 0;
}
"#;

fn runtime_archive() -> PathBuf {
    let out = Command::new(gos_bin())
        .arg("env")
        .output()
        .expect("gos env");
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix("runtime_lib"))
        .map(|rest| PathBuf::from(rest.trim()))
        .expect("gos env names the runtime archive")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-host-runtime-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("project dir");
    dir
}

fn build_library(dir: &Path) -> PathBuf {
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/hl\"\nversion = \"0.1.0\"\n\n\
         [lib]\nname = \"hl\"\nkind = [\"staticlib\"]\nruntime = \"host\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("src/lib.gos"), LIBRARY).expect("library");
    let out = Command::new(gos_bin())
        .current_dir(dir)
        .arg("build")
        .output()
        .expect("gos build");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir.join("target/debug/libhl.a")
}

#[cfg(target_os = "linux")]
#[test]
fn a_host_runtime_archive_defines_only_its_entry_points() {
    let dir = scratch("symbols");
    let archive = build_library(&dir);
    let out = Command::new("nm").arg(&archive).output().expect("nm");
    let symbols = String::from_utf8_lossy(&out.stdout);
    let globals: Vec<&str> = symbols
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace().rev();
            let name = parts.next()?;
            let kind = parts.next()?;
            // Weak definitions (`V`, `W`) such as the personality reference
            // stay global for the link to merge with the host's.
            matches!(kind, "T" | "D" | "B" | "R" | "C" | "G" | "S").then_some(name)
        })
        .collect();
    let mut sorted = globals.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        [
            "gos_lib_abi_fingerprint",
            "hl_greet",
            "hl_shutdown",
            "hl_sum"
        ],
        "{symbols}"
    );
    assert!(
        symbols
            .lines()
            .any(|l| l.trim_start().starts_with("U gos_rt_")),
        "the runtime is left to the host:\n{symbols}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
#[test]
fn a_host_runs_a_host_runtime_library_on_its_one_runtime() {
    let dir = scratch("host");
    let archive = build_library(&dir);
    std::fs::write(dir.join("host.c"), HOST).expect("host source");
    let exe = dir.join("host");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let out = Command::new(cc)
        .arg(dir.join("host.c"))
        .arg("-I")
        .arg(dir.join("target/debug/include"))
        .arg(&archive)
        .arg(runtime_archive())
        // As `gos build` links a program against the runtime archive.
        .args(["-lpthread", "-ldl", "-lm", "-Wl,--gc-sections", "-o"])
        .arg(&exe)
        .output()
        .expect("cc");
    assert!(
        out.status.success(),
        "linking the library and one runtime: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = Command::new(&exe)
        .env("GOS_LEAK_LEDGER", "1")
        .output()
        .expect("run the host");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stdout}{stderr}");
    let fingerprint = gossamer_abi::registry_fingerprint();
    assert_eq!(
        stdout,
        format!("hello 7 from a goroutine\n25\nfingerprint {fingerprint}\n"),
        "{stderr}"
    );
    let ledgers: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("LEAK LEDGER"))
        .collect();
    assert_eq!(ledgers.len(), 1, "one runtime reports once:\n{stderr}");
    assert_eq!(
        ledgers[0],
        baseline_ledger(),
        "the library's calls leave nothing live beyond what any program does"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The ledger line an empty compiled program reports, which counts what the
/// runtime itself keeps for the life of the process.
fn baseline_ledger() -> String {
    let dir = scratch("baseline");
    let source = dir.join("empty.gos");
    std::fs::write(&source, "fn main() {}\n").expect("empty program");
    let out_dir = dir.join("empty");
    let built = Command::new(gos_bin())
        .arg("build")
        .arg(&source)
        .arg("--out-dir")
        .arg(&out_dir)
        .output()
        .expect("gos build");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = Command::new(out_dir.join("empty"))
        .env("GOS_LEAK_LEDGER", "1")
        .output()
        .expect("run the empty program");
    let line = String::from_utf8_lossy(&run.stderr)
        .lines()
        .find(|l| l.starts_with("LEAK LEDGER"))
        .expect("the empty program reports its ledger")
        .to_string();
    let _ = std::fs::remove_dir_all(&dir);
    line
}

#[test]
fn a_host_runtime_library_is_a_static_archive_alone() {
    let dir = scratch("kinds");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/hl\"\nversion = \"0.1.0\"\n\n\
         [lib]\nname = \"hl\"\nkind = [\"staticlib\", \"cdylib\"]\nruntime = \"host\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("src/lib.gos"), LIBRARY).expect("library");
    let out = Command::new(gos_bin())
        .current_dir(&dir)
        .arg("build")
        .output()
        .expect("gos build");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is a static archive alone"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
