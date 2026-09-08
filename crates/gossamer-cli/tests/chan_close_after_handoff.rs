//! A send whose value has been received is complete, so closing the channel
//! after the last receive raises nothing in the senders still winding down.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

const SOURCE: &str = "
const WORKERS: i64 = 24

fn main() {
    let tx, rx = channel()
    let _ = cohort {
        let mut w = 0
        while w < WORKERS {
            let id = w
            let sender = tx
            spawn(|| sender.send(id))
            w += 1
        }
        let mut seen = 0
        while seen < WORKERS {
            let _ = rx.recv()
            seen += 1
        }
        tx.close()
    }
    println(\"received {}\", WORKERS)
}
";

/// The race is a timing one, so the run is repeated: each iteration is a
/// fresh chance for a close to land between a handoff and the sender's next
/// look at the channel.
const RUNS: usize = 20;

#[test]
fn close_after_the_last_receive_leaves_senders_alone() {
    let dir = env::temp_dir().join(format!("gos-chan-close-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("close_after_handoff.gos");
    std::fs::write(&source, SOURCE).unwrap();

    let build = Command::new(gos_bin())
        .arg("build")
        .arg(&source)
        .output()
        .expect("spawn gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bin = dir
        .join("target")
        .join("debug")
        .join(format!("close_after_handoff{}", env::consts::EXE_SUFFIX));
    assert!(bin.exists(), "missing {}", bin.display());

    for run in 0..RUNS {
        let out = Command::new(&bin)
            .output()
            .expect("run close_after_handoff");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("send on closed channel"),
            "run {run}: a completed send was reported as a send into a closed \
             channel:\n{stderr}"
        );
        assert!(
            out.status.success(),
            "run {run}: exit {:?}\nstderr: {stderr}",
            out.status.code()
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim_end(),
            "received 24",
            "run {run}: stdout"
        );
    }

    // The bytecode VM answers the same, which is what made the compiled
    // report a divergence rather than a program error.
    for run in 0..RUNS {
        let out = Command::new(gos_bin())
            .arg("run")
            .arg(&source)
            .output()
            .expect("spawn gos run");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("send on closed channel"),
            "vm run {run}: {stderr}"
        );
        assert!(out.status.success(), "vm run {run}: {stderr}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
