//! End-to-end smoke test for the `register_module!` macro.
//!
//! Declares a tiny binding, walks the registry, calls each item through its
//! VM entry, and asserts results.

use gossamer_binding::testing::call;
use gossamer_binding::{CallbackRef, NativeDispatch, REGISTRY, RuntimeError, RuntimeResult, Value};

gossamer_binding::register_module! {
    smoke_bindings,
    path: "test::smoke",
    doc: "Smoke-test binding.",

    fn add(a: i64, b: i64) -> i64 {
        a + b
    }

    fn greet(name: String) -> String {
        format!("hello, {name}")
    }

    fn divide(a: i64, b: i64) -> Result<i64, String> {
        if b == 0 {
            Err("divide by zero".to_string())
        } else {
            Ok(a / b)
        }
    }

    fn maybe(flag: bool) -> Option<i64> {
        if flag { Some(1) } else { None }
    }

    fn sum(items: Vec<i64>) -> i64 {
        items.into_iter().sum()
    }

    fn explode(message: String) -> i64 {
        panic!("{message}")
    }
}

#[test]
fn module_registers_via_linkme() {
    let module = REGISTRY
        .iter()
        .copied()
        .find(|m| m.path == "test::smoke")
        .expect("test::smoke registered");
    assert_eq!(module.items.len(), 6);
    assert_eq!(module.doc, "Smoke-test binding.");
}

#[test]
fn add_dispatches_through_thunk() {
    let out = call("test::smoke::add", &[Value::Int(3), Value::Int(4)]).unwrap();
    assert_eq!(out, Value::Int(7));
}

#[test]
fn greet_marshals_strings() {
    let out = call("test::smoke::greet", &[Value::String("jane".into())]).unwrap();
    assert_eq!(out, Value::String("hello, jane".into()));
}

#[test]
fn divide_returns_result_variant() {
    let ok = call("test::smoke::divide", &[Value::Int(10), Value::Int(2)]).unwrap();
    assert_eq!(ok, Value::variant("Ok", vec![Value::Int(5)]));
    let err = call("test::smoke::divide", &[Value::Int(10), Value::Int(0)]).unwrap();
    assert_eq!(
        err,
        Value::variant("Err", vec![Value::String("divide by zero".into())])
    );
}

#[test]
fn maybe_returns_option_variant() {
    let some = call("test::smoke::maybe", &[Value::Bool(true)]).unwrap();
    assert_eq!(some, Value::variant("Some", vec![Value::Int(1)]));
    let none = call("test::smoke::maybe", &[Value::Bool(false)]).unwrap();
    assert_eq!(none, Value::variant("None", Vec::new()));
}

#[test]
fn sum_marshals_vec() {
    let arg = Value::Array(std::sync::Arc::new(vec![
        Value::Int(1),
        Value::Int(2),
        Value::Int(3),
        Value::Int(4),
    ]));
    assert_eq!(call("test::smoke::sum", &[arg]).unwrap(), Value::Int(10));
}

#[test]
fn arity_mismatch_returns_typed_error() {
    let err = call("test::smoke::add", &[Value::Int(1)]).unwrap_err();
    assert_eq!(err.to_string(), "expected 2 arguments, found 1");
}

#[test]
fn a_mistyped_argument_names_both_types() {
    let err = call("test::smoke::add", &[Value::Int(1), Value::Bool(true)]).unwrap_err();
    assert_eq!(
        err,
        RuntimeError::Type("expected i64, found bool".to_string())
    );
}

#[test]
fn a_panic_answers_its_message() {
    let err = call("test::smoke::explode", &[Value::String("no window".into())]).unwrap_err();
    assert_eq!(err, RuntimeError::Panic("no window".to_string()));
}

#[test]
fn signatures_are_advertised() {
    use gossamer_binding::Type;
    let (_, item) = gossamer_binding::item("test::smoke::add").unwrap();
    assert_eq!(item.signature.params, &[Type::I64, Type::I64]);
    assert_eq!(item.signature.ret, Type::I64);
}

gossamer_binding::register_module! {
    cb_real_bindings,
    path: "test::cb_real",
    doc: "Real-callback dispatch test.",

    cb_fn run(d, f: gossamer_binding::BindingCallback) -> i64 {
        f.invoke(d, vec![Value::Int(1), Value::Int(2)])
            .map_or(-1, |v| if let Value::Int(n) = v { n } else { -1 })
    }
}

/// Answers how many arguments each callback call passed.
struct CountingDispatch;

impl NativeDispatch for CountingDispatch {
    fn invoke_callback(
        &mut self,
        _callback: CallbackRef,
        args: Vec<Value>,
    ) -> RuntimeResult<Value> {
        Ok(Value::Int(i64::try_from(args.len()).unwrap_or(i64::MAX)))
    }
}

#[test]
fn cb_fn_body_calls_back_through_its_dispatch() {
    let callback = gossamer_binding::BindingCallback::from_handle(7);
    assert_eq!(cb_real_bindings::run(&mut CountingDispatch, callback), 2);
}

#[test]
fn a_callback_with_no_vm_to_call_reports_it() {
    let callable = Value::Callback(CallbackRef {
        handle: 1,
        origin: gossamer_binding::CallbackOrigin::Vm,
    });
    assert_eq!(
        call("test::cb_real::run", &[callable]).unwrap(),
        Value::Int(-1)
    );
}
