//! Regression matrix for what may cross a goroutine boundary.
//!
//! These are soundness tests, not diagnostic snapshots. A spawned closure
//! takes a snapshot of each capture, so an aggregate holding growable storage
//! crosses as a value of its own; what stays rejected is a write that would
//! reach the spawning goroutine's storage, and a `sync::Shared` payload the
//! guarded slot cannot carry.

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

/// The codes the goroutine-capture analysis gives `source`, which runs on the
/// checked program after type checking.
fn capture_codes(source: &str) -> Vec<String> {
    let mut map = SourceMap::new();
    let file = map.add_file("goroutine-sharing.gos".to_string(), source.to_string());
    let (mut parsed, _) = parse_source_file(source, file);
    let (mut resolutions, _) = resolve_source_file(&parsed);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut parsed, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&parsed, &resolutions, &mut tcx);
    gossamer_types::check_capture_writes(&parsed, &resolutions, &table, &tcx)
        .iter()
        .map(|d| d.to_diagnostic().code.0.to_string())
        .collect()
}

fn codes(source: &str) -> Vec<String> {
    diagnostics(source)
        .iter()
        .map(|d| d.error.code().to_string())
        .collect()
}

/// Lending a captured aggregate to a callee as `&mut` from goroutines spawned
/// in a loop writes each goroutine's snapshot, never the spawning binding.
#[test]
fn lending_a_captured_aggregate_mutably_is_rejected() {
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
    let codes = capture_codes(source);
    assert!(
        codes.iter().any(|c| c == "GT0114"),
        "expected GT0114, got {codes:?}"
    );
}

/// A goroutine reads its own snapshot of an aggregate holding growable
/// storage.
#[test]
fn reading_a_captured_aggregate_with_nested_storage_is_accepted() {
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
    let mut codes = codes(source);
    codes.extend(capture_codes(source));
    assert!(
        !codes.iter().any(|c| c.starts_with("GT")),
        "expected no type errors, got {codes:?}"
    );
}

/// A goroutine that builds its own value keeps working.
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
