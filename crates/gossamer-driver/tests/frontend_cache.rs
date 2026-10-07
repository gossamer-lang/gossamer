//! The frontend cache must round-trip a checked program exactly: a hit
//! replaces parse, resolve, and typecheck, so any lost detail becomes a
//! miscompile rather than a slow build.

use std::fs;
use std::path::PathBuf;

use gossamer_driver::{
    CachedFrontend, FrontendCacheKey, check_frontend, load_blob_in, store_frontend_in,
};
use gossamer_lex::SourceMap;

const PROGRAM: &str = r#"
struct Point { x: f64, y: f64 }

enum Shape { Circle(f64), Rect(f64, f64) }

fn area(s: Shape) -> f64 {
    match s {
        Shape::Circle(r) => 3.14159 * r * r,
        Shape::Rect(w, h) => w * h,
    }
}

fn shift(p: Point, dx: f64) -> Point {
    Point { x: p.x + dx, y: p.y }
}

fn total(xs: [i64]) -> i64 {
    let mut acc = 0
    for x in xs { acc += x }
    acc
}

struct Wrapper<T> { value: T }

fn unwrap_sum<const N: usize>(xs: [i64; N]) -> i64 {
    let mut acc = 0
    for x in xs { acc += x }
    acc
}

fn main() {
    let p = shift(Point { x: 1.0, y: 2.0 }, 0.5)
    println("{} {}", p.x, area(Shape::Rect(2.0, 3.0)))
    println("{}", total([1, 2, 3]))
    let w = Wrapper { value: 7 }
    println("{} {}", w.value, unwrap_sum([1, 2, 3, 4]))
}
"#;

#[test]
fn cached_frontend_round_trips_every_side_table() {
    let root = scratch("round-trip");
    let mut map = SourceMap::new();
    let file = map.add_file("round_trip.gos".to_string(), PROGRAM.to_string());
    let outcome = check_frontend(map.source(file), file);
    assert!(
        outcome.diagnostics.is_empty(),
        "fixture must type-check: {:?}",
        outcome.diagnostics
    );
    let checked = outcome.checked;

    let key = FrontendCacheKey::new(PROGRAM, "round-trip");
    assert!(load_blob_in::<CachedFrontend>(&root, &key).is_none());
    store_frontend_in(
        &root,
        &key,
        &checked.sf,
        &checked.resolutions,
        &checked.table,
        &checked.tcx,
        &[],
    );

    let restored: CachedFrontend = load_blob_in(&root, &key).expect("blob was published");
    assert_eq!(
        format!("{:?}", restored.sf),
        format!("{:?}", checked.sf),
        "restored AST differs"
    );
    assert_eq!(
        restored.resolutions.sorted_entries(),
        checked.resolutions.sorted_entries()
    );
    assert_eq!(
        restored.table.sorted_entries(),
        checked.table.sorted_entries()
    );
    assert_eq!(
        restored.tcx.stable_snapshot_key(),
        checked.tcx.stable_snapshot_key(),
        "restored type interner differs"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_edited_source_does_not_hit_the_previous_entry() {
    let root = scratch("edit");
    let mut map = SourceMap::new();
    let file = map.add_file("edit.gos".to_string(), PROGRAM.to_string());
    let checked = check_frontend(map.source(file), file).checked;

    let key = FrontendCacheKey::new(PROGRAM, "edit");
    store_frontend_in(
        &root,
        &key,
        &checked.sf,
        &checked.resolutions,
        &checked.table,
        &checked.tcx,
        &[],
    );

    let edited = PROGRAM.replace("3.14159", "3.14");
    let edited_key = FrontendCacheKey::new(&edited, "edit");
    assert_ne!(key, edited_key);
    assert!(load_blob_in::<CachedFrontend>(&root, &edited_key).is_none());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_truncated_blob_is_a_miss_rather_than_a_panic() {
    let root = scratch("truncated");
    let mut map = SourceMap::new();
    let file = map.add_file("truncated.gos".to_string(), PROGRAM.to_string());
    let checked = check_frontend(map.source(file), file).checked;

    let key = FrontendCacheKey::new(PROGRAM, "truncated");
    store_frontend_in(
        &root,
        &key,
        &checked.sf,
        &checked.resolutions,
        &checked.table,
        &checked.tcx,
        &[],
    );

    let path = root.join(format!("{}.bin", key.as_hex()));
    let full = fs::read(&path).expect("blob exists");
    fs::write(&path, &full[..full.len() / 2]).expect("truncate blob");
    assert!(load_blob_in::<CachedFrontend>(&root, &key).is_none());

    let _ = fs::remove_dir_all(&root);
}

fn scratch(name: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!(
        "gossamer-frontend-cache-{name}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create scratch dir");
    path
}

#[test]
fn a_restored_pass_reports_the_warnings_the_first_pass_did() {
    const WARNS: &str = "fn main() {\n    let x: u8 = 5\n    match x { 0..=255 => println(\"all\"), _ => println(\"never\") }\n}\n";
    let root = scratch("warnings");
    let mut map = SourceMap::new();
    let file = map.add_file("warnings.gos".to_string(), WARNS.to_string());
    let outcome = check_frontend(map.source(file), file);
    assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
    let codes: Vec<&str> = outcome.warnings.iter().map(|w| w.code.as_str()).collect();
    assert_eq!(codes, ["GM0002"]);

    let key = FrontendCacheKey::new(WARNS, "warnings");
    let checked = outcome.checked;
    store_frontend_in(
        &root,
        &key,
        &checked.sf,
        &checked.resolutions,
        &checked.table,
        &checked.tcx,
        &outcome.warnings,
    );
    let restored: CachedFrontend = load_blob_in(&root, &key).expect("blob was published");
    assert_eq!(
        format!("{:?}", restored.warnings),
        format!("{:?}", outcome.warnings),
        "restored warnings differ"
    );

    let _ = fs::remove_dir_all(&root);
}
