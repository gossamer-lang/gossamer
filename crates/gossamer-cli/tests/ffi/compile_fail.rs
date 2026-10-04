//! The diagnostics the C boundary's rules raise, each from `gos check`.

use crate::support::Project;

/// `source` checked in a project that allows foreign functions must fail
/// with `code`, its report holding every one of `needles`.
fn rejects(tag: &str, source: &str, code: &str, needles: &[&str]) {
    let project = Project::new(tag, source);
    let out = project.gos(&["check", "src/main.gos"]);
    assert!(!out.ok, "`{tag}` was accepted:\n{}", out.all());
    assert!(
        out.all().contains(code),
        "`{tag}` lacks {code}:\n{}",
        out.all()
    );
    for needle in needles {
        assert!(
            out.all().contains(needle),
            "`{tag}` lacks `{needle}`:\n{}",
            out.all()
        );
    }
}

#[test]
fn a_memory_operation_outside_unsafe_is_gt0103() {
    rejects(
        "cf-unsafe-read",
        r"use std::ffi
use std::ffi::Ptr

fn main() {
    let p: Ptr<i64> = unsafe { ffi::alloc(1) }
    let v: i64 = ffi::read(p)
    println(v)
}
",
        "GT0103",
        &["ffi::read"],
    );
    rejects(
        "cf-unsafe-forge",
        r"use std::ffi
use std::ffi::Ptr

fn main() {
    let p: Option<Ptr<u8>> = Ptr::from_address(4096)
    println(p.is_some())
}
",
        "GT0103",
        &["from_address"],
    );
    rejects(
        "cf-unsafe-handle",
        r"use std::ffi
use std::ffi::Ptr

fn main() {
    let h = ffi::Handle::new(1)
    let back: ffi::Handle<i64> = ffi::Handle::from_ptr(h.as_ptr())
    println(back.get())
}
",
        "GT0103",
        &["from_ptr"],
    );
}

#[test]
fn a_foreign_type_by_value_is_gt0104() {
    rejects(
        "cf-opaque-param",
        r#"unsafe extern "C" {
    type Window
    fn show(w: Window)
}

fn main() {}
"#,
        "GT0104",
        &["Window"],
    );
    rejects(
        "cf-opaque-build",
        r#"unsafe extern "C" {
    type Window
}

fn main() {
    let w = Window
}
"#,
        "GT0104",
        &["cannot be constructed"],
    );
    rejects(
        "cf-opaque-read",
        r#"use std::ffi
use std::ffi::Ptr

unsafe extern "C" {
    type Window
    fn top() -> Ptr<Window>
}

fn main() {
    let w: Window = unsafe { ffi::read(top()) }
}
"#,
        "GT0104",
        &["Window"],
    );
}

#[test]
fn a_pointee_without_c_layout_is_gt0105() {
    rejects(
        "cf-pointee-param",
        r#"use std::ffi::Ptr

unsafe extern "C" {
    fn takes(p: Ptr<String>)
}

fn main() {}
"#,
        "GT0105",
        &["String"],
    );
    rejects(
        "cf-pointee-alloc",
        r"use std::ffi
use std::ffi::Ptr

fn main() {
    let p: Ptr<Vec<u8>> = unsafe { ffi::alloc(1) }
}
",
        "GT0105",
        &["Vec<u8>"],
    );
}

#[test]
fn a_native_address_leaving_the_process_is_gt0106() {
    rejects(
        "cf-comptime",
        r"use std::ffi
use std::ffi::Ptr

fn main() {
    let p = comptime { unsafe { Ptr::<u8>::from_address(8) } }
}
",
        "GT0106",
        &["after compilation"],
    );
    rejects(
        "cf-json",
        r"use std::encoding::json
use std::ffi
use std::ffi::Ptr

struct Holder {
    p: Ptr<u8>,
}

fn main() {
    let p: Ptr<u8> = unsafe { ffi::alloc(1) }
    println(json::encode(Holder { p: p }))
}
",
        "GT0106",
        &["outside this process"],
    );
}

#[test]
fn a_callback_type_without_c_form_is_gt0107() {
    rejects(
        "cf-callback-type",
        r#"unsafe extern "C" {
    fn register(f: Fn(String) -> i32)
}

fn main() {}
"#,
        "GT0107",
        &["Fn(String) -> i32"],
    );
}

#[test]
fn a_closure_or_mismatched_function_as_callback_is_gt0108() {
    rejects(
        "cf-callback-closure",
        r#"unsafe extern "C" {
    fn register(f: Fn(i32) -> i32)
}

fn main() {
    let offset = 3
    unsafe { register(|x: i32| x + offset) }
}
"#,
        "GT0108",
        &["a closure"],
    );
    rejects(
        "cf-callback-mismatch",
        r#"unsafe extern "C" {
    fn register(f: Fn(i32) -> i32)
}

fn wide(x: i64) -> i64 {
    x
}

fn main() {
    unsafe { register(wide) }
}
"#,
        "GT0108",
        &["Fn(i64) -> i64"],
    );
}

#[test]
fn an_invalid_pointer_type_in_a_signature_is_gt0098() {
    rejects(
        "cf-signature",
        r#"unsafe extern "C" {
    fn takes(values: Vec<String>)
}

fn main() {}
"#,
        "GT0098",
        &["Vec<String>"],
    );
}

#[test]
fn a_foreign_declaration_under_ffi_false_is_gt0102() {
    let project = Project::with_manifest(
        "cf-ffi-false",
        r#"use std::ffi::Ptr

unsafe extern "C" {
    type Window
    fn top() -> Option<Ptr<Window>>
}

fn main() {}
"#,
        "ffi = false\n",
    );
    let out = project.gos(&["check", "src/main.gos"]);
    assert!(!out.ok, "{}", out.all());
    assert!(out.all().contains("GT0102"), "{}", out.all());
}
