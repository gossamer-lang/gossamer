//! The calls the binding cost benchmark makes: a scalar, a string, an opaque
//! handle, and a 4 KB buffer.

use gossamer_binding::{Bytes, gos_opaque, register_module};

register_module!(
    name: bench_binding,
    doc: "Binding call cost probes.",

    fn add(a: i64, b: i64) -> i64 {
        a + b
    }

    fn echo(s: String) -> String {
        s
    }

    fn checksum(b: Bytes) -> i64 {
        b.iter().map(|byte| i64::from(*byte)).sum()
    }
);

pub struct Tally {
    total: i64,
}

#[gos_opaque]
impl Tally {
    pub fn new() -> Self {
        Self { total: 0 }
    }

    pub fn add(&mut self, n: i64) -> i64 {
        self.total += n;
        self.total
    }
}
