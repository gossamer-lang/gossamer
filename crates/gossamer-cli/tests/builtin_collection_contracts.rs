//! Cross-surface regressions for Rust-guided built-in collection contracts.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn run(source: &str) -> std::process::Output {
    // Test threads share this process, and two of them can read the clock in
    // the same tick, so the counter is what makes each fixture path its own.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let fixture = env::temp_dir().join(format!(
        "gossamer-builtin-collection-contracts-{}-{}-{}.gos",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&fixture, source).expect("write fixture");
    let output = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("run fixture");
    let _ = std::fs::remove_file(fixture);
    output
}

/// `insert` reports an out-of-range index through its `Result` and leaves
/// the receiver alone; `swap` is an indexed write, so it yields unit and
/// mutates in place. An out-of-range `swap` is a bounds panic, covered by
/// `feature-testing-examples/vec_method_swap_oob_panic.gos`.
#[test]
fn vec_insert_returns_result_without_replacing_the_receiver() {
    let output = run(
        "fn main() {\n    let mut values: Vec<i64> = Vec::from([1, 2, 3])\n    println(values.insert(1, 9))\n    println(values)\n    println(values.insert(99, 8).is_err())\n    println(values)\n    values.swap(0, 3)\n    println(values)\n}\n",
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Ok(())\n#[1, 9, 2, 3]\ntrue\n#[1, 9, 2, 3]\n#[3, 9, 2, 1]\n"
    );
}

#[test]
fn map_insert_and_collection_from_follow_rust_shaped_contracts() {
    let output = run(
        "use std::collections::{Map, Set, BTreeSet}\n\nfn main() {\n    let mut map: Map<String, i64> = Map::new()\n    println(map.len())\n    println(map.insert(\"a\", 1))\n    println(map.insert(\"a\", 2))\n    println(map.get(\"a\"))\n    println(map.remove(\"a\"))\n    println(map.remove(\"a\"))\n    let made = {\"x\": 3, \"y\": 4}\n    println(made.len())\n    let b = {}\n    println(b.len())\n    let also: Map<String, i64> = Map::from([(\"z\", 5)])\n    println(also.get(\"z\"))\n    let empty: Map<String, i64> = {}\n    println(empty.len())\n    let set: Set<i64> = #{1, 2, 2, 3}\n    println(set.len())\n    let ordered: BTreeSet<i64> = #{3, 1, 2, 1}\n    println(ordered.len())\n    println(ordered.to_vec())\n    let also_ordered: BTreeSet<i64> = BTreeSet::from([4, 4, 5])\n    println(also_ordered.to_vec())\n}\n",
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "0\nNone\nSome(1)\nSome(2)\nSome(2)\nNone\n2\n0\nSome(5)\n0\n3\n3\n#[1, 2, 3]\n#[4, 5]\n"
    );
}

#[test]
fn option_method_dispatch_matches_the_single_option_surface() {
    let output = run(
        "fn main() {\n    let present: Option<i64> = Some(12)\n    let absent: Option<i64> = None\n    println(present.and_then(|value| Some(value + 1)))\n    println(present.filter(|value| value > 10))\n    println(Some(present).flatten())\n    println(absent.is_none())\n    println(present.is_some())\n    println(present.iter())\n    println(absent.iter())\n    println(present.map(|value| value * 2))\n    println(absent.or(Some(4)))\n    println(absent.or_else(|| Some(5)))\n    println(absent.unwrap_or(6))\n    println(absent.unwrap_or_else(|| 7))\n    println(present.zip(Some(3)))\n}\n",
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Some(13)\nSome(12)\nSome(12)\ntrue\ntrue\n#[12]\n#[]\nSome(24)\nSome(4)\nSome(5)\n6\n7\nSome((12, 3))\n"
    );
}
