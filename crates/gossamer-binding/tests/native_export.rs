//! Compiled-mode export-ABI integration test.
//!
//! Declares a binding with `symbol_prefix:`, calls each exported
//! `extern "C"` thunk directly through the symbol the codegen
//! would emit, and asserts the marshalling layer round-trips
//! correctly across every `BindingAbi`-impl'd type.

#![allow(unsafe_code, clippy::missing_safety_doc)]

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use gossamer_binding::native::{BindingAbi, GosVec};

gossamer_binding::register_module! {
    nativeexport_bindings,
    path: "test::nativeexport",
    symbol_prefix: test__nativeexport,
    doc: "Native-export integration binding.",

    fn add(a: i64, b: i64) -> i64 { a + b }

    fn shout(text: String) -> String { text.to_uppercase() }

    fn double_each(items: Vec<i64>) -> Vec<i64> {
        items.into_iter().map(|n| n * 2).collect()
    }

    fn nest(items: Vec<i64>) -> Vec<Vec<i64>> {
        vec![items.clone(), items]
    }

    fn flag_to_int(flag: bool) -> i64 {
        i64::from(flag)
    }

    fn maybe(flag: bool) -> Option<i64> {
        if flag { Some(7) } else { None }
    }

    fn divide(a: i64, b: i64) -> Result<i64, String> {
        if b == 0 {
            Err("divide by zero".to_string())
        } else {
            Ok(a / b)
        }
    }
}

unsafe extern "C" {
    fn gos_binding_test__nativeexport__add(a: i64, b: i64) -> i64;
    fn gos_binding_test__nativeexport__shout(text: *const c_char) -> *mut c_char;
    fn gos_binding_test__nativeexport__double_each(items: *const GosVec) -> *mut GosVec;
    fn gos_binding_test__nativeexport__nest(items: *const GosVec) -> *mut std::os::raw::c_char;
    fn gos_binding_test__nativeexport__flag_to_int(flag: bool) -> i64;
    fn gos_binding_test__nativeexport__maybe(
        flag: bool,
    ) -> *mut gossamer_binding::native::GosVariant;
    fn gos_binding_test__nativeexport__divide(
        a: i64,
        b: i64,
    ) -> *mut gossamer_binding::native::GosVariant;
}

#[test]
fn primitive_export_round_trip() {
    gossamer_binding::testing::install_host();
    let result = unsafe { gos_binding_test__nativeexport__add(2, 3) };
    assert_eq!(result, 5);
    let bool_result = unsafe { gos_binding_test__nativeexport__flag_to_int(true) };
    assert_eq!(bool_result, 1);
}

#[test]
fn string_export_round_trip() {
    gossamer_binding::testing::install_host();
    let input = CString::new("hello").unwrap();
    let raw = unsafe { gos_binding_test__nativeexport__shout(input.as_ptr()) };
    let out = unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(out, "HELLO");
    // The string lives in the runtime arena; reclamation
    // happens at `gos_rt_gc_reset` boundaries owned by the
    // host. Tests exit before the next reset, so no explicit
    // free is needed here.
}

#[test]
fn vec_i64_export_round_trip() {
    gossamer_binding::testing::install_host();
    let v: Vec<i64> = vec![1, 2, 3, 4];
    let in_ptr = v.to_output();
    let out_ptr = unsafe { gos_binding_test__nativeexport__double_each(in_ptr) };
    let out: Vec<i64> = unsafe { <Vec<i64> as BindingAbi>::from_input(out_ptr) };
    assert_eq!(out, vec![2, 4, 6, 8]);
}

#[test]
fn a_nested_vector_crosses_as_its_text() {
    gossamer_binding::testing::install_host();
    const { assert!(<Vec<Vec<i64>> as BindingAbi>::JSON) };
    let v: Vec<i64> = vec![10, 20];
    let in_ptr = v.to_output();
    let out_ptr = unsafe { gos_binding_test__nativeexport__nest(in_ptr) };
    let text = unsafe { <String as BindingAbi>::from_input(out_ptr) };
    assert_eq!(text, "[[10,20],[10,20]]");
}

#[test]
fn option_export_round_trip() {
    gossamer_binding::testing::install_host();
    let some_raw = unsafe { gos_binding_test__nativeexport__maybe(true) };
    let some = unsafe { <Option<i64> as BindingAbi>::from_input(some_raw) };
    assert_eq!(some, Some(7));

    let none_raw = unsafe { gos_binding_test__nativeexport__maybe(false) };
    let none = unsafe { <Option<i64> as BindingAbi>::from_input(none_raw) };
    assert_eq!(none, None);
}

#[test]
fn result_export_round_trip() {
    gossamer_binding::testing::install_host();
    let ok_raw = unsafe { gos_binding_test__nativeexport__divide(10, 2) };
    let ok = unsafe { <Result<i64, String> as BindingAbi>::from_input(ok_raw) };
    assert_eq!(ok, Ok(5));

    let err_raw = unsafe { gos_binding_test__nativeexport__divide(10, 0) };
    let err = unsafe { <Result<i64, String> as BindingAbi>::from_input(err_raw) };
    assert_eq!(err, Err("divide by zero".to_string()));
}

#[test]
fn mangle_helper_matches_macro_emitted_symbol() {
    gossamer_binding::testing::install_host();
    let mangled = gossamer_binding::mangle_binding_symbol("test::nativeexport", "add");
    assert_eq!(mangled, "gos_binding_test__nativeexport__add");
}
