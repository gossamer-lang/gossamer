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

#[test]
fn an_offset_of_path_that_names_no_field_is_gt0109() {
    rejects(
        "cf-offset-field",
        r#"use std::ffi

#[repr(C)]
struct Header {
    len: u32
    flags: u16
}

fn main() {
    println(ffi::offset_of::<Header>("length"))
}
"#,
        "GT0109",
        &["has no field `length`"],
    );
    rejects(
        "cf-offset-literal",
        r#"use std::ffi

#[repr(C)]
struct Header {
    len: u32
}

fn main() {
    let name = "len"
    println(ffi::offset_of::<Header>(name))
}
"#,
        "GT0109",
        &["not a string literal"],
    );
}

#[test]
fn a_layout_query_over_a_type_without_c_layout_is_gt0105() {
    rejects(
        "cf-size-of-string",
        r"use std::ffi

fn main() {
    println(ffi::size_of::<String>())
}
",
        "GT0105",
        &["String"],
    );
}

#[test]
fn a_union_operation_on_a_type_it_does_not_list_is_gt0110() {
    rejects(
        "cf-union-member",
        r"use std::ffi

fn main() {
    let u: ffi::Union<(i32, f64)> = ffi::Union::new(1i32)
    let b: u8 = u.get()
    println(b)
}
",
        "GT0110",
        &["has no member of type `u8`"],
    );
    rejects(
        "cf-union-string",
        r"use std::ffi

fn main() {
    let u: ffi::Union<(i32, String)> = ffi::Union::new(1i32)
    println(u.get::<i32>())
}
",
        "GT0110",
        &["which has no C layout"],
    );
    rejects(
        "cf-union-twice",
        r"use std::ffi

fn main() {
    let u: ffi::Union<(i32, i32)> = ffi::Union::zeroed()
    println(u.get::<i32>())
}
",
        "GT0110",
        &["twice"],
    );
}

#[test]
fn a_foreign_static_named_outside_addr_of_is_gt0111() {
    rejects(
        "cf-static-value",
        r"use std::ffi

unsafe extern {
    static gosffi_counter: i32
}
",
        "GP0016",
        &["unsafe extern \"C\""],
    );
    rejects(
        "cf-static-use",
        r#"use std::ffi

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    static gosffi_counter: i32
}

fn main() {
    let n = gosffi_counter
    println(n)
}
"#,
        "GT0111",
        &["`gosffi_counter` is used as a value"],
    );
    rejects(
        "cf-addr-of-local",
        r"use std::ffi

fn main() {
    let n = 3
    let p = ffi::addr_of(n)
    println(p)
}
",
        "GT0111",
        &["takes the name of a foreign static"],
    );
}

const LEGACY_STRUCT_PARAM: &str = r#"use std::ffi

#[repr(C)]
struct Item {
    id: i32
    weight: f64
}

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    fn items_total(items: [Item], n: ffi::size_t) -> f64
    fn slice_len(item: Item) -> i64
}

fn main() {
    let mut item = Item { id: 1, weight: 2.0 }
    println(unsafe { slice_len(item) })
}
"#;

#[test]
fn a_struct_parameter_written_before_0_67_is_gt0112_with_a_fix() {
    let project = Project::with_manifest(
        "cf-legacy-by-value",
        LEGACY_STRUCT_PARAM,
        "gossamer-version = \"^0.66.0\"\n",
    );
    let out = project.gos(&["check", "src/main.gos"]);
    assert!(!out.ok, "accepted:\n{}", out.all());
    assert!(out.all().contains("GT0112"), "{}", out.all());
    assert!(out.all().contains("`item` of `slice_len`"), "{}", out.all());
    let fixed = project.gos(&["check", "--fix", "src/main.gos"]);
    let source = std::fs::read_to_string(project.dir.join("src/main.gos")).unwrap();
    assert!(
        source.contains("fn slice_len(item: &mut Item) -> i64")
            && source.contains("slice_len(&mut item)"),
        "--fix wrote:\n{source}\n{}",
        fixed.all()
    );
    let current = Project::with_manifest(
        "cf-current-by-value",
        LEGACY_STRUCT_PARAM,
        "gossamer-version = \"^0.67.0\"\n",
    );
    let out = current.gos(&["check", "src/main.gos"]);
    assert!(out.ok, "{}", out.all());
}

#[test]
fn an_export_c_cannot_call_is_gt0113() {
    rejects(
        "cf-export-string",
        "#[export]\nfn takes(s: String) -> i64 { s.len() }\nfn main() {}\n",
        "GT0113",
        &[
            "`takes` cannot be exported",
            "`String` has no C representation",
        ],
    );
    rejects(
        "cf-export-generic",
        "#[export]\nfn pick<T>(x: T) -> T { x }\nfn main() {}\n",
        "GT0113",
        &["a generic function has no single C entry"],
    );
    rejects(
        "cf-export-method",
        "struct A {}\nimpl A {\n    #[export]\n    fn get(&self) -> i32 { 1 }\n}\nfn main() {}\n",
        "GT0113",
        &["a method has no C symbol"],
    );
    rejects(
        "cf-export-twice",
        "#[export(\"one\")]\nfn a() -> i32 { 1 }\n#[export(\"one\")]\nfn b() -> i32 { 2 }\nfn main() {}\n",
        "GT0113",
        &["`a` already exports the symbol `one`"],
    );
    rejects(
        "cf-export-symbol",
        "#[export(\"not a name\")]\nfn a() -> i32 { 1 }\nfn main() {}\n",
        "GT0113",
        &["the symbol `not a name` is not a C identifier"],
    );
    rejects(
        "cf-export-main",
        "#[export(\"main\")]\nfn a() -> i32 { 1 }\nfn main() {}\n",
        "GT0113",
        &["the symbol `main` is the C program's entry"],
    );
}
