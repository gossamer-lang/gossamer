//! Regression matrix for what may cross a goroutine boundary.
//!
//! These are soundness tests, not diagnostic snapshots. A rejected program
//! is one that would otherwise reach the same nested growable storage from
//! two goroutines with nothing serialising the access - the shape that
//! compiled cleanly, ran on the bytecode VM, and hard-faulted in a native
//! build. The accepted cases pin what stays legal: a value built inside the
//! goroutine, and `sync::Shared`, which exists to be reached from several.

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, TypeDiagnostic, typecheck_source_file};

fn diagnostics(source: &str) -> Vec<TypeDiagnostic> {
    let mut map = SourceMap::new();
    let file = map.add_file("goroutine-sharing.gos".to_string(), source.to_string());
    let (mut parsed, parse_errors) = parse_source_file(source, file);
    assert!(
        parse_errors.is_empty(),
        "unexpected parse errors: {parse_errors:?}"
    );
    let (mut resolutions, _resolve_errors) = resolve_source_file(&parsed);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut parsed, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (_table, diagnostics) = typecheck_source_file(&parsed, &resolutions, &mut tcx);
    diagnostics
}

fn codes(source: &str) -> Vec<String> {
    diagnostics(source)
        .iter()
        .map(|d| d.error.code().to_string())
        .collect()
}

/// The shape that used to pass `gos check`, run on the VM, and fault in a
/// native build: a goroutine reading an outer binding whose type carries
/// nested growable storage.
#[test]
fn capturing_an_aggregate_with_nested_storage_is_rejected() {
    let source = r"
struct Pool {
    connections: Vec<i64>
}

fn use_pool(pool: &mut Pool, shard: i64) -> i64 {
    pool.connections.len() + shard
}

fn main() {
    let mut pool = Pool { connections: #[1, 2] }
    let _ = cohort {
        for shard in #[1, 2] {
            spawn(|| use_pool(&mut pool, shard))
        }
    }
}
";
    let codes = codes(source);
    assert!(
        codes.iter().any(|c| c == "GT0076"),
        "expected GT0076, got {codes:?}"
    );
}

/// The `go` spelling of the same hazard.
#[test]
fn a_detached_goroutine_capture_is_rejected_too() {
    let source = r#"
struct Registry {
    names: Vec<String>
}

fn count(registry: Registry) -> i64 {
    registry.names.len()
}

fn main() {
    let registry = Registry { names: #["a"] }
    spawn(|| { let _ = count(registry) })
}
"#;
    let codes = codes(source);
    assert!(
        codes.iter().any(|c| c == "GT0076"),
        "expected GT0076, got {codes:?}"
    );
}

/// A goroutine that builds its own value keeps working - the rejection is
/// about reaching the spawning goroutine's storage, not about aggregates.
#[test]
fn a_goroutine_that_builds_its_own_aggregate_is_accepted() {
    let source = r"
struct Pool {
    connections: Vec<i64>
}

fn work(shard: i64) -> i64 {
    let pool = Pool { connections: #[1, 2] }
    pool.connections.len() + shard
}

fn main() {
    let _ = cohort {
        for shard in #[1, 2] {
            spawn(|| work(shard))
        }
    }
}
";
    assert!(
        codes(source).is_empty(),
        "unexpected diagnostics: {:?}",
        codes(source)
    );
}

/// Scalars and the concurrency handles stay capturable; over-rejecting here
/// would break every goroutine that reads a counter or a channel end.
#[test]
fn scalars_and_channel_ends_stay_capturable() {
    let source = r"
use std::sync::channel

fn main() {
    let limit = 10
    let tx, rx = channel()
    let _ = cohort {
        spawn(|| {
            tx.send(limit)
            tx.close()
        })
        spawn(|| {
            while let Some(v) = rx.recv() { let _ = v }
        })
    }
}
";
    assert!(
        codes(source).is_empty(),
        "unexpected diagnostics: {:?}",
        codes(source)
    );
}

/// `sync::Shared` is the sanctioned way to reach one value from several
/// goroutines, so capturing one is not the hazard the check is about.
#[test]
fn a_shared_value_is_capturable() {
    let source = r"
use std::sync

fn bump(counter: sync::Shared) -> i64 {
    counter.update(|v| v + 1)
}

fn main() {
    let counter = sync::Shared::new(0)
    let _ = cohort {
        spawn(|| bump(counter))
        spawn(|| bump(counter))
    }
}
";
    assert!(
        codes(source).is_empty(),
        "unexpected diagnostics: {:?}",
        codes(source)
    );
}

/// The guarded slot is one word every tier reads back as an integer, so a
/// payload without that agreement is refused rather than compiling on one
/// tier and failing to lower on another.
#[test]
fn a_shared_payload_the_slot_cannot_carry_is_rejected() {
    for payload in ["#[1, 2]", "\"text\"", "true", "'c'", "1.5"] {
        let source = format!(
            r"
use std::sync

fn main() {{
    let guarded = sync::Shared::new({payload})
    let _ = guarded.get()
}}
"
        );
        let codes = codes(&source);
        assert!(
            codes.iter().any(|c| c == "GT0077"),
            "expected GT0077 for {payload}, got {codes:?}"
        );
    }
}

/// A `Shared<T>` annotation naming a type the slot cannot carry is refused
/// where it is written, and its uses report nothing further.
#[test]
fn a_shared_annotation_the_slot_cannot_carry_is_its_only_error() {
    let source = r#"
use std::sync::Shared

fn main() {
    let m: Shared<Map<i64, String>> = Shared::new(Map::new())
    let mut copy = m.get()
    copy[1] = "a"
    m.set(copy)
    println(f"{m.get().get(1)}")
}
"#;
    let codes = codes(source);
    assert!(!codes.is_empty(), "expected GT0077");
    assert!(
        codes.iter().all(|c| c == "GT0077"),
        "only GT0077, got {codes:?}"
    );
}

#[test]
fn an_integer_payload_is_accepted() {
    let source = r"
use std::sync

fn main() {
    let guarded = sync::Shared::new(0)
    guarded.set(7)
    let _ = guarded.update(|v| v + 1)
    let _ = guarded.with(|v| v * 2)
    let _ = guarded.get()
}
";
    assert!(
        codes(source).is_empty(),
        "unexpected diagnostics: {:?}",
        codes(source)
    );
}
