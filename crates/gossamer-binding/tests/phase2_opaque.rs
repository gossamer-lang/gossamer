//! Phase-2 `#[gos_opaque]` smoke. Exposes a tiny `Counter` opaque
//! type with constructor + `&self` + `&mut self` methods; verifies
//! every method appears as a `Counter::method` binding item.

#![allow(
    missing_docs,
    dead_code,
    clippy::must_use_candidate,
    clippy::new_without_default
)]

use gossamer_binding::gos_opaque;

pub struct Counter {
    pub value: i64,
}

#[gos_opaque]
impl Counter {
    pub fn new() -> Self {
        Self { value: 0 }
    }

    pub fn get(&self) -> i64 {
        self.value
    }

    pub fn inc(&mut self) -> i64 {
        self.value += 1;
        self.value
    }
}

#[test]
fn opaque_methods_register_as_type_qualified_items() {
    let m = gossamer_binding::modules()
        .iter()
        .find(|m| m.path == "Counter")
        .copied()
        .expect("Counter module registered");
    // The item name is the method's own name under the type's module, so a
    // call is spelled `Counter::new(..)`; the mangled form is the exported
    // symbol, not the name Gossamer writes.
    let names: Vec<&str> = m.items.iter().map(|i| i.name).collect();
    assert!(names.contains(&"new"), "got {names:?}");
    assert!(names.contains(&"get"), "got {names:?}");
    assert!(names.contains(&"inc"), "got {names:?}");
}

#[test]
fn an_opaque_handle_carries_its_state_between_calls() {
    use gossamer_binding::Value;
    use gossamer_binding::testing::call;
    let handle = call("Counter::new", &[]).unwrap();
    assert!(matches!(handle, Value::Int(_)));
    assert_eq!(
        call("Counter::inc", std::slice::from_ref(&handle)).unwrap(),
        Value::Int(1)
    );
    assert_eq!(
        call("Counter::inc", std::slice::from_ref(&handle)).unwrap(),
        Value::Int(2)
    );
    assert_eq!(call("Counter::get", &[handle]).unwrap(), Value::Int(2));
}

#[test]
fn an_unknown_opaque_handle_panics_with_its_name() {
    use gossamer_binding::{RuntimeError, Value};
    let err = gossamer_binding::testing::call("Counter::get", &[Value::Int(999_999)]).unwrap_err();
    let RuntimeError::Panic(message) = err else {
        panic!("expected a panic, got {err:?}");
    };
    assert!(
        message.contains("get") && message.contains("999999"),
        "{message}"
    );
}
