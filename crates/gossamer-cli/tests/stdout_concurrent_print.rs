//! Catches multi-thread tearing on `GOS_RT_STDOUT_*` (C3 in
//! `~/dev/contexts/lang/adversarial_analysis.md`).

use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

#[test]
fn concurrent_println_lines_do_not_tear() {
    let dir = env::temp_dir().join(format!("gos-stdout-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("concurrent.gos");
    std::fs::write(
        &source,
        "
use std::sync::WaitGroup

fn worker(id: i64, wg: WaitGroup) {
    let mut i = 0
    while i < 256 {
        println(\"G{}:{}\", id, i)
        i = i + 1
    }
    wg.done()
}

fn main() {
    let wg = WaitGroup::new()
    let mut k: i64 = 0
    while k < 16 {
        wg.add(1)
        spawn(|| worker(k, wg))
        k = k + 1
    }
    wg.wait()
}
",
    )
    .unwrap();

    // The bytecode VM and the JIT reach the same writer, so the property
    // has to hold on all four configurations, not only the compiled two.
    for jit in [false, true] {
        let mut cmd = Command::new(gos_bin());
        cmd.arg("run").arg(&source);
        if !jit {
            cmd.env("GOS_JIT", "0");
        }
        let out = cmd.output().expect("spawn gos run");
        assert!(
            out.status.success(),
            "gos run failed (jit={jit}): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).expect("non-utf8 stdout");
        assert_lines_intact(&stdout, &format!("jit={jit}"));
    }

    for release in [false, true] {
        let mut cmd = Command::new(gos_bin());
        cmd.arg("build");
        if release {
            cmd.arg("--release");
        }
        cmd.arg(&source);
        let build = cmd.output().expect("spawn gos build");
        assert!(
            build.status.success(),
            "build failed (release={release}): {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let profile = if release { "release" } else { "debug" };
        let bin = dir
            .join("target")
            .join(profile)
            .join(format!("concurrent{}", std::env::consts::EXE_SUFFIX));
        assert!(bin.exists(), "missing {}", bin.display());

        let out = Command::new(&bin).output().expect("run concurrent");
        assert!(
            out.status.success(),
            "binary exited non-zero (release={release}): stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).expect("non-utf8 stdout");
        assert_lines_intact(&stdout, &format!("release={release}"));
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every line the workers wrote arrived whole, and all of them arrived.
/// A spliced pair shows up as a line that does not parse.
fn assert_lines_intact(stdout: &str, label: &str) {
    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    let mut bad: Vec<&str> = Vec::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix('G') else {
            bad.push(line);
            continue;
        };
        let Some((id_text, count_text)) = rest.split_once(':') else {
            bad.push(line);
            continue;
        };
        match (id_text.parse::<i64>(), count_text.parse::<i64>()) {
            (Ok(id), Ok(i)) => {
                seen.insert((id, i));
            }
            _ => bad.push(line),
        }
    }
    assert!(
        bad.is_empty(),
        "found {} torn line(s) ({label}) - first 5: {:?}",
        bad.len(),
        &bad.iter().take(5).collect::<Vec<_>>()
    );
    assert_eq!(
        seen.len(),
        16 * 256,
        "unique line count mismatch ({label}): saw {} lines",
        seen.len()
    );
}

/// `println` / `print` write to stdout and `eprintln` / `eprint` to
/// stderr, on every configuration, and a `print` with no terminator
/// leaves the stream without one.
#[test]
fn each_formatting_call_writes_to_its_own_sink() {
    let dir = env::temp_dir().join(format!("gos-sinks-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("sinks.gos");
    std::fs::write(
        &source,
        r#"fn main() {
    println("out-line")
    print("out-part")
    eprintln("err-line")
    eprint("err-part")
}
"#,
    )
    .unwrap();

    let check = |stdout: &str, stderr: &str, label: &str| {
        assert_eq!(stdout, "out-line\nout-part", "stdout ({label})");
        assert_eq!(stderr, "err-line\nerr-part", "stderr ({label})");
    };

    for jit in [false, true] {
        let mut cmd = Command::new(gos_bin());
        cmd.arg("run").arg(&source);
        if !jit {
            cmd.env("GOS_JIT", "0");
        }
        let out = cmd.output().expect("spawn gos run");
        assert!(out.status.success(), "gos run failed (jit={jit})");
        check(
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
            &format!("jit={jit}"),
        );
    }

    for release in [false, true] {
        let mut cmd = Command::new(gos_bin());
        cmd.arg("build");
        if release {
            cmd.arg("--release");
        }
        cmd.arg(&source);
        let build = cmd.output().expect("spawn gos build");
        assert!(
            build.status.success(),
            "build failed (release={release}): {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let profile = if release { "release" } else { "debug" };
        let bin = dir
            .join("target")
            .join(profile)
            .join(format!("sinks{}", std::env::consts::EXE_SUFFIX));
        let out = Command::new(&bin).output().expect("run sinks");
        assert!(out.status.success(), "binary exited non-zero");
        check(
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
            &format!("release={release}"),
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
