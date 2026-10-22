//! A binding the host's tests load as a shared library.

use std::collections::HashMap;

use gossamer_binding::{BindingCallback, Bytes, Value, gos_opaque, register_module};

register_module!(
    name: host_probe,
    doc: "Host loader probe.",

    fn double(n: i64) -> i64 {
        n * 2
    }

    fn greet(name: String) -> String {
        format!("hello, {name}")
    }

    fn reverse(b: Bytes) -> Bytes {
        let mut bytes = b.into_inner();
        bytes.reverse();
        Bytes::new(bytes)
    }

    fn total(m: HashMap<String, i64>) -> i64 {
        m.values().sum()
    }

    fn pair(n: i64) -> (i64, String) {
        (n, n.to_string())
    }

    fn maybe(n: i64) -> Option<i64> {
        (n > 0).then_some(n)
    }

    fn explode(message: String) -> i64 {
        panic!("{message}")
    }

    cb_fn twice(d, f: BindingCallback, x: i64) -> i64 {
        let once = match f.invoke(d, vec![Value::Int(x)]) {
            Ok(Value::Int(n)) => n,
            other => panic!("unexpected {other:?}"),
        };
        match f.invoke(d, vec![Value::Int(once)]) {
            Ok(Value::Int(n)) => n,
            other => panic!("unexpected {other:?}"),
        }
    }
);

pub struct Counter {
    value: i64,
}

#[gos_opaque]
impl Counter {
    pub fn new() -> Self {
        Self { value: 0 }
    }

    pub fn inc(&mut self) -> i64 {
        self.value += 1;
        self.value
    }
}
