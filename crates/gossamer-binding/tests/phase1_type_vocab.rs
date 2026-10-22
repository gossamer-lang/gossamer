//! Phase 1 regression: the expanded type vocabulary
//! (`Option<String>`, `Result<bool, String>`, tuples, additional
//! `HashMap` combinations) registers and marshals through the VM entry.

#![allow(
    unsafe_code,
    clippy::missing_safety_doc,
    clippy::explicit_auto_deref,
    clippy::items_after_statements
)]

use std::collections::HashMap;

use gossamer_binding::register_module;

register_module!(
    name: p1,
    doc: "Phase-1 vocab smoke.",

    /// Some/None over String.
    fn maybe_name(present: bool) -> Option<String> {
        if present { Some("ada".to_string()) } else { None }
    }

    /// Ok/Err over bool.
    fn maybe_flag(flip: bool) -> Result<bool, String> {
        if flip { Ok(true) } else { Err("no".to_string()) }
    }

    /// `Result<(), String>` for "did the thing, no payload".
    fn maybe_void(ok: bool) -> Result<(), String> {
        if ok { Ok(()) } else { Err("nope".to_string()) }
    }

    /// String/i64 tuple - common "name, count" return.
    fn pair() -> (String, i64) {
        ("kingdom".to_string(), 42)
    }

    /// Three-tuple to verify the (i64, String, bool) impl.
    fn triple() -> (i64, String, bool) {
        (1, "two".to_string(), true)
    }

    /// HashMap<String, Vec<i64>> for "name -> series".
    fn series() -> HashMap<String, Vec<i64>> {
        let mut m = HashMap::new();
        m.insert("a".to_string(), vec![1, 2, 3]);
        m.insert("b".to_string(), vec![10]);
        m
    }

    /// HashMap<i64, String> for "id -> label".
    fn labels() -> HashMap<i64, String> {
        let mut m = HashMap::new();
        m.insert(1, "one".to_string());
        m.insert(2, "two".to_string());
        m
    }
);

use gossamer_binding::value::Value;

fn dispatch_module() -> &'static gossamer_binding::Module {
    *gossamer_binding::modules()
        .iter()
        .find(|m| m.path == "p1")
        .expect("p1 module registered")
}

fn call(name: &str, args: Vec<Value>) -> Value {
    gossamer_binding::testing::call(&format!("p1::{name}"), &args).expect("call")
}

#[test]
fn maybe_name_some_round_trip() {
    let v = call("maybe_name", vec![Value::Bool(true)]);
    let Value::Variant(inner) = &v else {
        panic!("expected Variant, got {v:?}")
    };
    assert_eq!(inner.name, "Some");
    assert_eq!(inner.fields.len(), 1);
    let Value::String(s) = &inner.fields[0] else {
        panic!()
    };
    assert_eq!(s.as_str(), "ada");
}

#[test]
fn maybe_name_none_round_trip() {
    let v = call("maybe_name", vec![Value::Bool(false)]);
    let Value::Variant(inner) = &v else {
        panic!("expected Variant, got {v:?}")
    };
    assert_eq!(inner.name, "None");
}

#[test]
fn maybe_flag_ok_round_trip() {
    let v = call("maybe_flag", vec![Value::Bool(true)]);
    let Value::Variant(inner) = &v else { panic!() };
    assert_eq!(inner.name, "Ok");
    let Value::Bool(b) = &inner.fields[0] else {
        panic!()
    };
    assert!(*b);
}

#[test]
fn maybe_void_ok_round_trip() {
    let v = call("maybe_void", vec![Value::Bool(true)]);
    let Value::Variant(inner) = &v else { panic!() };
    assert_eq!(inner.name, "Ok");
    assert!(matches!(inner.fields[0], Value::Unit));
}

#[test]
fn pair_round_trip() {
    let v = call("pair", vec![]);
    let Value::Tuple(arc) = &v else {
        panic!("expected Tuple, got {v:?}")
    };
    assert_eq!(arc.len(), 2);
    let Value::String(name) = &arc[0] else {
        panic!()
    };
    assert_eq!(name.as_str(), "kingdom");
    let Value::Int(n) = &arc[1] else { panic!() };
    assert_eq!(*n, 42);
}

#[test]
fn triple_round_trip() {
    let v = call("triple", vec![]);
    let Value::Tuple(arc) = &v else { panic!() };
    assert_eq!(arc.len(), 3);
    let Value::Int(a) = &arc[0] else { panic!() };
    assert_eq!(*a, 1);
    let Value::String(s) = &arc[1] else { panic!() };
    assert_eq!(s.as_str(), "two");
    let Value::Bool(b) = &arc[2] else { panic!() };
    assert!(*b);
}

#[test]
fn series_hashmap_round_trip() {
    let v = call("series", vec![]);
    let Value::Map(m) = &v else {
        panic!("expected Map, got {v:?}")
    };
    assert_eq!(m.len(), 2);
}

#[test]
fn labels_hashmap_round_trip() {
    let v = call("labels", vec![]);
    let Value::Map(m) = &v else {
        panic!("expected Map, got {v:?}")
    };
    assert_eq!(m.len(), 2);
}

#[test]
fn signatures_advertise_new_shapes() {
    let m = dispatch_module();
    let pair = m.items.iter().find(|i| i.name == "pair").unwrap();
    use gossamer_binding::Type;
    assert!(matches!(pair.signature.ret, Type::Tuple(arms) if arms.len() == 2));
    let series = m.items.iter().find(|i| i.name == "series").unwrap();
    assert!(matches!(
        series.signature.ret,
        Type::Map(&Type::String, &Type::Vec(&Type::I64))
    ));
}
