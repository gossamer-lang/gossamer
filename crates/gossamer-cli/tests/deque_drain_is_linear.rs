//! A queue drained front to back costs its own length, not its length squared.

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

fn gos_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

/// Draining a queue reclaims the dead prefix at most once per halving.
///
/// Moving the live range down on every pop makes a drain quadratic, which
/// four times the elements turns into sixteen times the work. The bound here
/// sits between the two: linear is four, and the margin covers the noise of a
/// run this short.
#[test]
fn draining_a_queue_costs_its_own_length() {
    let dir = std::env::temp_dir().join(format!("gos-deque-drain-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("drain.gos");
    std::fs::write(
        &source,
        "
use std::env

fn main() {
    let n = env::args().first().unwrap_or(\"1000000\").to_i64().unwrap_or(1000000)
    let mut q: Deque<i64> = Deque::new()
    for i in 0..n { q.push_back(i) }
    let mut total = 0
    while let Some(v) = q.pop_front() { total += v }
    println(\"{}\", total)
}
",
    )
    .unwrap();
    let build = Command::new(gos_bin())
        .args(["build", "--release", "--out-dir"])
        .arg(&dir)
        .arg(&source)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join("drain");
    let run = |n: &str| -> f64 {
        let start = Instant::now();
        let out = Command::new(&binary).arg(n).output().expect("run drain");
        assert!(out.status.success(), "drain run failed at {n}");
        start.elapsed().as_secs_f64()
    };
    // One warm run so the first measured one is not paying process start-up
    // and page faults the second does not.
    let _ = run("1000000");
    let small = run("1000000");
    let large = run("4000000");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        large < small * 8.0,
        "four times the elements took {large:.3}s against {small:.3}s for one \
         times: the drain is moving the live range on every pop"
    );
}
