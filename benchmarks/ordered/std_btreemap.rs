//! The same measurements as `btreemap.gos`, over Rust's
//! `std::collections::BTreeMap`, so the two can be read side by side. Build
//! with `rustc -O std_btreemap.rs`; `run.gos` does that and compares.

use std::collections::BTreeMap;
use std::time::Instant;

fn ns_per_op(start: Instant, ops: u64) -> u64 {
    let elapsed = start.elapsed().as_nanos() as u64;
    if ops == 0 { 0 } else { elapsed / ops }
}

fn report(keys: &str, op: &str, size: u64, ns: u64) {
    println!("std::BTreeMap {keys} {op} {size} {ns}");
}

/// The keys `0..size` in the same scrambled order the Gossamer benchmark
/// fills its tree in.
fn int_keys(size: i64) -> Vec<i64> {
    let mut out: Vec<i64> = (0..size).collect();
    let mut x: i64 = 12_345;
    for i in 0..size {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407)
            % 1_000_000_007;
        let j = x.abs() % size;
        out.swap(i as usize, j as usize);
    }
    out
}

fn measure_int(size: i64) {
    let keys = int_keys(size);
    let n = size as u64;
    let mut tree: BTreeMap<i64, i64> = BTreeMap::new();
    let start = Instant::now();
    for (i, k) in keys.iter().enumerate() {
        tree.insert(*k, i as i64);
    }
    report("i64", "insert", n, ns_per_op(start, n));

    let hit = Instant::now();
    let mut found = 0u64;
    for k in &keys {
        if tree.contains_key(k) {
            found += 1;
        }
    }
    report("i64", "get_hit", n, ns_per_op(hit, n));

    let miss = Instant::now();
    for k in &keys {
        if !tree.contains_key(&(-1 - *k)) {
            found += 1;
        }
    }
    report("i64", "get_miss", n, ns_per_op(miss, n));

    let walk = Instant::now();
    let mut sum: i64 = 0;
    for (k, v) in &tree {
        sum = sum.wrapping_add(*k).wrapping_add(*v);
    }
    report("i64", "traverse", n, ns_per_op(walk, tree.len() as u64));

    let lo = size / 2;
    let ranged = Instant::now();
    let mut seen = 0usize;
    for _ in 0..16 {
        let slice: Vec<(i64, i64)> = tree.range(lo..lo + 1000).map(|(k, v)| (*k, *v)).collect();
        seen += slice.len();
    }
    report("i64", "range", n, ns_per_op(ranged, 16));

    let drain = Instant::now();
    let mut drained = 0u64;
    while tree.pop_first().is_some() {
        drained += 1;
    }
    report("i64", "pop_first", n, ns_per_op(drain, drained));
    eprintln!("# checks {found} {seen} {}", sum != 0);
}

fn measure_text(size: i64) {
    let keys: Vec<String> = int_keys(size)
        .into_iter()
        .map(|k| format!("key-{k:010}"))
        .collect();
    let n = size as u64;
    let mut tree: BTreeMap<String, i64> = BTreeMap::new();
    let start = Instant::now();
    for (i, k) in keys.iter().enumerate() {
        tree.insert(k.clone(), i as i64);
    }
    report("String", "insert", n, ns_per_op(start, n));

    let hit = Instant::now();
    let mut found = 0u64;
    for k in &keys {
        if tree.contains_key(k) {
            found += 1;
        }
    }
    report("String", "get_hit", n, ns_per_op(hit, n));

    let miss = Instant::now();
    for k in &keys {
        let absent = format!("absent-{k}");
        if !tree.contains_key(&absent) {
            found += 1;
        }
    }
    report("String", "get_miss", n, ns_per_op(miss, n));

    let walk = Instant::now();
    let mut total: usize = 0;
    for (k, v) in &tree {
        total += k.len() + *v as usize;
    }
    report("String", "traverse", n, ns_per_op(walk, tree.len() as u64));

    let lo = format!("key-{:010}", size / 2);
    let hi = format!("key-{:010}", size / 2 + 1000);
    let ranged = Instant::now();
    let mut seen = 0usize;
    for _ in 0..16 {
        let slice: Vec<(String, i64)> = tree
            .range(lo.clone()..hi.clone())
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        seen += slice.len();
    }
    report("String", "range", n, ns_per_op(ranged, 16));

    let drain = Instant::now();
    let mut drained = 0u64;
    while tree.pop_first().is_some() {
        drained += 1;
    }
    report("String", "pop_first", n, ns_per_op(drain, drained));
    eprintln!("# checks {found} {seen} {}", total != 0);
}

fn main() {
    for size in [1_000i64, 100_000, 1_000_000] {
        measure_int(size);
        measure_text(size);
    }
}
