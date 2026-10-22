//! ABI 0.4 native-export integration test.
//!
//! Exercises every new ABI type (`Bytes`, `Map<K, V>`, `Variant`,
//! `Callback`) by declaring a binding through the same
//! `register_module!` macro user code uses and calling the
//! generated `extern "C"` thunks directly.

#![allow(unsafe_code, clippy::missing_safety_doc)]

use std::collections::HashMap;

use gossamer_binding::native::{
    BindingAbi, BindingGosMap, GosBytes, GosDynVariant, NativeCallback,
};
use gossamer_binding::{Bytes, DynValue};

gossamer_binding::register_module! {
    abi04_bindings,
    path: "test::abi04",
    symbol_prefix: test__abi04,
    doc: "ABI 0.4 export integration binding.",

    fn upper(b: Bytes) -> Bytes {
        let mut out = b.into_inner();
        for byte in &mut out { byte.make_ascii_uppercase(); }
        Bytes::new(out)
    }

    fn double_bytes(b: Bytes) -> Bytes {
        let v = b.into_inner();
        let mut out = Vec::with_capacity(v.len() * 2);
        out.extend_from_slice(&v);
        out.extend_from_slice(&v);
        Bytes::new(out)
    }

    fn empty_bytes() -> Bytes {
        Bytes::default()
    }

    fn headers_count(headers: HashMap<String, String>) -> i64 {
        headers.len() as i64
    }

    fn build_headers() -> HashMap<String, String> {
        let mut m: HashMap<String, String> = HashMap::new();
        m.insert("content-type".to_string(), "text/plain".to_string());
        m.insert("x-request-id".to_string(), "abc123".to_string());
        m
    }

    fn build_int_map() -> HashMap<i64, i64> {
        let mut m: HashMap<i64, i64> = HashMap::new();
        m.insert(1, 10);
        m.insert(2, 20);
        m
    }

    fn make_resp_integer(n: i64) -> DynValue {
        DynValue::Tagged {
            name: "Integer".to_string(),
            payload: vec![DynValue::Int(n)],
        }
    }

    fn make_resp_array() -> DynValue {
        DynValue::Tagged {
            name: "Array".to_string(),
            payload: vec![
                DynValue::Tagged {
                    name: "Integer".to_string(),
                    payload: vec![DynValue::Int(7)],
                },
                DynValue::Tagged {
                    name: "BulkString".to_string(),
                    payload: vec![DynValue::Bytes(b"hello".to_vec())],
                },
            ],
        }
    }

    fn reflect_dyn(v: DynValue) -> DynValue {
        v
    }

    fn callback_pass_through(cb: NativeCallback) -> u64 {
        cb.handle
    }
}

unsafe extern "C" {
    fn gos_binding_test__abi04__upper(b: *const GosBytes) -> *mut GosBytes;
    fn gos_binding_test__abi04__double_bytes(b: *const GosBytes) -> *mut GosBytes;
    fn gos_binding_test__abi04__empty_bytes() -> *mut GosBytes;
    fn gos_binding_test__abi04__headers_count(headers: *const BindingGosMap) -> i64;
    fn gos_binding_test__abi04__build_headers() -> *mut BindingGosMap;
    fn gos_binding_test__abi04__build_int_map() -> *mut BindingGosMap;
    fn gos_binding_test__abi04__make_resp_integer(n: i64) -> *mut GosDynVariant;
    fn gos_binding_test__abi04__make_resp_array() -> *mut GosDynVariant;
    fn gos_binding_test__abi04__reflect_dyn(v: *const GosDynVariant) -> *mut GosDynVariant;
    fn gos_binding_test__abi04__callback_pass_through(handle: u64) -> u64;
}

#[test]
fn bytes_round_trip_through_extern_c() {
    gossamer_binding::testing::install_host();
    let input = Bytes::new(b"hello".to_vec());
    let in_ptr = input.to_output();
    let out_ptr = unsafe { gos_binding_test__abi04__upper(in_ptr) };
    let out = unsafe { <Bytes as BindingAbi>::from_input(out_ptr) };
    assert_eq!(out.as_slice(), b"HELLO");
}

#[test]
fn bytes_with_large_payload_through_extern_c() {
    gossamer_binding::testing::install_host();
    let payload: Vec<u8> = (0..32 * 1024).map(|i| (i & 0xff) as u8).collect();
    let in_ptr = Bytes::new(payload.clone()).to_output();
    let out_ptr = unsafe { gos_binding_test__abi04__double_bytes(in_ptr) };
    let out = unsafe { <Bytes as BindingAbi>::from_input(out_ptr) };
    assert_eq!(out.len(), payload.len() * 2);
    assert_eq!(&out.as_slice()[..payload.len()], payload.as_slice());
    assert_eq!(&out.as_slice()[payload.len()..], payload.as_slice());
}

#[test]
fn empty_bytes_through_extern_c() {
    gossamer_binding::testing::install_host();
    let out_ptr = unsafe { gos_binding_test__abi04__empty_bytes() };
    let out = unsafe { <Bytes as BindingAbi>::from_input(out_ptr) };
    assert!(out.is_empty());
}

#[test]
fn map_input_count_through_extern_c() {
    gossamer_binding::testing::install_host();
    let mut m: HashMap<String, String> = HashMap::new();
    m.insert("a".to_string(), "1".to_string());
    m.insert("b".to_string(), "2".to_string());
    m.insert("c".to_string(), "3".to_string());
    let in_ptr = m.to_output();
    let n = unsafe { gos_binding_test__abi04__headers_count(in_ptr) };
    assert_eq!(n, 3);
}

#[test]
fn map_output_string_string_through_extern_c() {
    gossamer_binding::testing::install_host();
    let out_ptr = unsafe { gos_binding_test__abi04__build_headers() };
    let out: HashMap<String, String> =
        unsafe { <HashMap<String, String> as BindingAbi>::from_input(out_ptr) };
    assert_eq!(
        out.get("content-type").map(String::as_str),
        Some("text/plain")
    );
    assert_eq!(out.get("x-request-id").map(String::as_str), Some("abc123"));
}

#[test]
fn map_output_i64_i64_through_extern_c() {
    gossamer_binding::testing::install_host();
    let out_ptr = unsafe { gos_binding_test__abi04__build_int_map() };
    let out: HashMap<i64, i64> = unsafe { <HashMap<i64, i64> as BindingAbi>::from_input(out_ptr) };
    assert_eq!(out.get(&1), Some(&10));
    assert_eq!(out.get(&2), Some(&20));
}

#[test]
fn dyn_value_integer_through_extern_c() {
    gossamer_binding::testing::install_host();
    let raw = unsafe { gos_binding_test__abi04__make_resp_integer(42) };
    let out = unsafe { <DynValue as BindingAbi>::from_input(raw) };
    let DynValue::Tagged { name, payload } = out else {
        panic!("expected Tagged");
    };
    assert_eq!(name, "Integer");
    assert_eq!(payload, vec![DynValue::Int(42)]);
}

#[test]
fn dyn_value_array_with_bytes_through_extern_c() {
    gossamer_binding::testing::install_host();
    let raw = unsafe { gos_binding_test__abi04__make_resp_array() };
    let out = unsafe { <DynValue as BindingAbi>::from_input(raw) };
    let DynValue::Tagged { name, payload } = out else {
        panic!("expected Tagged");
    };
    assert_eq!(name, "Array");
    assert_eq!(payload.len(), 2);
    // First element: Integer(7)
    let DynValue::Tagged {
        name: n0,
        payload: p0,
    } = &payload[0]
    else {
        panic!("expected Tagged Integer");
    };
    assert_eq!(n0, "Integer");
    assert_eq!(p0, &vec![DynValue::Int(7)]);
    // Second element: BulkString(Bytes("hello"))
    let DynValue::Tagged {
        name: n1,
        payload: p1,
    } = &payload[1]
    else {
        panic!("expected Tagged BulkString");
    };
    assert_eq!(n1, "BulkString");
    assert_eq!(p1.len(), 1);
    match &p1[0] {
        DynValue::Bytes(b) => assert_eq!(b, b"hello"),
        other => panic!("expected Bytes, got {other:?}"),
    }
}

#[test]
fn dyn_value_reflect_through_extern_c() {
    gossamer_binding::testing::install_host();
    let in_value = DynValue::Tagged {
        name: "TestArm".to_string(),
        payload: vec![DynValue::Int(99), DynValue::String("nine-nine".to_string())],
    };
    let in_ptr = in_value.clone().to_output();
    let out_ptr = unsafe { gos_binding_test__abi04__reflect_dyn(in_ptr) };
    let out = unsafe { <DynValue as BindingAbi>::from_input(out_ptr) };
    let DynValue::Tagged { name, payload } = out else {
        panic!("expected Tagged");
    };
    assert_eq!(name, "TestArm");
    assert_eq!(payload.len(), 2);
    assert_eq!(payload[0], DynValue::Int(99));
    assert_eq!(payload[1], DynValue::String("nine-nine".to_string()));
}

#[test]
fn callback_handle_through_extern_c() {
    gossamer_binding::testing::install_host();
    let cb = NativeCallback { handle: 99_999 };
    let out = unsafe { gos_binding_test__abi04__callback_pass_through(cb.handle) };
    assert_eq!(out, 99_999);
}

#[test]
fn signature_metadata_matches_abi04_types() {
    use gossamer_binding::Type;

    let m = gossamer_binding::module("test::abi04").expect("module registered");
    let upper = m
        .items
        .iter()
        .find(|i| i.name == "upper")
        .expect("upper item");
    assert!(matches!(upper.signature.params[0], Type::Bytes));
    assert!(matches!(upper.signature.ret, Type::Bytes));

    let count = m
        .items
        .iter()
        .find(|i| i.name == "headers_count")
        .expect("headers_count item");
    assert!(matches!(
        count.signature.params[0],
        Type::Map(&Type::String, &Type::String)
    ));

    let cb = m
        .items
        .iter()
        .find(|i| i.name == "callback_pass_through")
        .expect("callback_pass_through item");
    assert!(matches!(cb.signature.params[0], Type::Callback(_, _)));
}

#[test]
fn fuzz_bytes_round_trip_random_lengths() {
    gossamer_binding::testing::install_host();
    // Lightweight inline fuzzer: deterministic seeded LCG covers
    // a spread of payload sizes including zero, single byte,
    // word-aligned, page-sized, and odd sizes around alignment
    // boundaries.
    let sizes = [0_usize, 1, 7, 8, 9, 31, 32, 33, 4095, 4096, 4097, 16_384];
    let mut state: u64 = 0xC0FFEE_u64;
    for &size in &sizes {
        let mut payload = Vec::with_capacity(size);
        for _ in 0..size {
            // SplitMix64-style permutation.
            state = state.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^= z >> 31;
            payload.push((z & 0xff) as u8);
        }
        let in_ptr = Bytes::new(payload.clone()).to_output();
        let out_ptr = unsafe { gos_binding_test__abi04__double_bytes(in_ptr) };
        let out = unsafe { <Bytes as BindingAbi>::from_input(out_ptr) };
        assert_eq!(out.len(), payload.len() * 2);
        assert_eq!(&out.as_slice()[..payload.len()], payload.as_slice());
    }
}

#[test]
fn fuzz_dyn_value_deep_nesting() {
    gossamer_binding::testing::install_host();
    // Build a 32-level-nested tagged variant and round-trip it.
    let mut v = DynValue::Int(0);
    for i in 0..32 {
        v = DynValue::Tagged {
            name: format!("Level{i}"),
            payload: vec![v],
        };
    }
    let in_ptr = v.clone().to_output();
    let out_ptr = unsafe { gos_binding_test__abi04__reflect_dyn(in_ptr) };
    let out = unsafe { <DynValue as BindingAbi>::from_input(out_ptr) };

    // Walk both trees in parallel.
    let mut depth = 0;
    let mut cur_in = &v;
    let mut cur_out = &out;
    loop {
        match (cur_in, cur_out) {
            (
                DynValue::Tagged {
                    name: ni,
                    payload: pi,
                },
                DynValue::Tagged {
                    name: no,
                    payload: po,
                },
            ) => {
                assert_eq!(ni, no, "name mismatch at depth {depth}");
                assert_eq!(pi.len(), 1);
                assert_eq!(po.len(), 1);
                cur_in = &pi[0];
                cur_out = &po[0];
                depth += 1;
            }
            (DynValue::Int(a), DynValue::Int(b)) => {
                assert_eq!(a, b);
                break;
            }
            (a, b) => panic!("shape mismatch at depth {depth}: {a:?} vs {b:?}"),
        }
    }
    assert_eq!(depth, 32);
}
