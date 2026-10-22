//! ABI 0.4 perf characterization.
//!
//! These tests measure round-trip cost across the C-ABI boundary
//! for each new shape.
//!
//! The pass criterion is "doesn't regress catastrophically". A
//! 64KiB Bytes round-trip should complete in < 1 ms on a modern
//! workstation; we assert < 50 ms as a generous regression
//! bound that survives even a heavily loaded CI runner. Same
//! shape for the other types.

#![allow(unsafe_code, clippy::missing_safety_doc)]

use std::collections::HashMap;
use std::time::Instant;

use gossamer_binding::native::{BindingAbi, BindingGosMap, GosBytes, GosDynVariant};
use gossamer_binding::{Bytes, DynValue};

gossamer_binding::register_module! {
    abi04_perf_bindings,
    path: "test::abi04_perf",
    symbol_prefix: test__abi04_perf,
    doc: "ABI 0.4 perf-bench binding.",

    fn echo_bytes(b: Bytes) -> Bytes { b }

    fn echo_map(m: HashMap<String, String>) -> HashMap<String, String> { m }

    fn echo_dyn(v: DynValue) -> DynValue { v }
}

unsafe extern "C" {
    fn gos_binding_test__abi04_perf__echo_bytes(b: *const GosBytes) -> *mut GosBytes;
    fn gos_binding_test__abi04_perf__echo_map(m: *const BindingGosMap) -> *mut BindingGosMap;
    fn gos_binding_test__abi04_perf__echo_dyn(v: *const GosDynVariant) -> *mut GosDynVariant;
}

const ITERS: usize = 1_000;

#[test]
fn bytes_64k_round_trip_throughput() {
    gossamer_binding::testing::install_host();
    let payload: Vec<u8> = (0..64 * 1024).map(|i| (i & 0xff) as u8).collect();
    let start = Instant::now();
    for _ in 0..ITERS {
        let raw = Bytes::new(payload.clone()).to_output();
        let out = unsafe { gos_binding_test__abi04_perf__echo_bytes(raw) };
        let back = unsafe { <Bytes as BindingAbi>::from_input(out) };
        std::hint::black_box(back);
    }
    let elapsed = start.elapsed();
    let per_op = elapsed / ITERS as u32;
    eprintln!("bytes_64k_round_trip: {ITERS} ops in {elapsed:?} ({per_op:?}/op)");
    assert!(
        per_op.as_millis() < 50,
        "per-op cost regression: {per_op:?}"
    );
}

#[test]
fn map_30_entries_round_trip_throughput() {
    gossamer_binding::testing::install_host();
    let mut m: HashMap<String, String> = HashMap::with_capacity(30);
    for i in 0..30 {
        m.insert(format!("header-{i}"), format!("value-{i}"));
    }
    let start = Instant::now();
    for _ in 0..ITERS {
        let raw = m.clone().to_output();
        let out = unsafe { gos_binding_test__abi04_perf__echo_map(raw) };
        let back = unsafe { <HashMap<String, String> as BindingAbi>::from_input(out) };
        std::hint::black_box(back);
    }
    let elapsed = start.elapsed();
    let per_op = elapsed / ITERS as u32;
    eprintln!("map_30_entries_round_trip: {ITERS} ops in {elapsed:?} ({per_op:?}/op)");
    assert!(
        per_op.as_millis() < 50,
        "per-op cost regression: {per_op:?}"
    );
}

#[test]
fn dyn_value_depth_8_round_trip_throughput() {
    gossamer_binding::testing::install_host();
    let mut v = DynValue::Int(42);
    for i in 0..8 {
        v = DynValue::Tagged {
            name: format!("Level{i}"),
            payload: vec![v],
        };
    }
    let start = Instant::now();
    for _ in 0..ITERS {
        let raw = v.clone().to_output();
        let out = unsafe { gos_binding_test__abi04_perf__echo_dyn(raw) };
        let back = unsafe { <DynValue as BindingAbi>::from_input(out) };
        std::hint::black_box(back);
    }
    let elapsed = start.elapsed();
    let per_op = elapsed / ITERS as u32;
    eprintln!("dyn_value_depth_8_round_trip: {ITERS} ops in {elapsed:?} ({per_op:?}/op)");
    assert!(
        per_op.as_millis() < 50,
        "per-op cost regression: {per_op:?}"
    );
}
