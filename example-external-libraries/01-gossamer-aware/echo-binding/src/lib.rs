//! Tiny binding crate exposing string helpers to Gossamer code.
//!
//! The `register_module!` macro emits both a VM thunk (the bytecode VM
//! calls it in the library `gos run` loads) and a C-ABI thunk
//! (`gos_binding_echo__shout`, etc.) a native build links.

use gossamer_binding::register_module;

register_module!(
    binding,
    path: "echo",
    symbol_prefix: echo,
    doc: "String helpers exposed by the example echo-binding crate.",

    fn shout(s: String) -> String {
        s.to_uppercase()
    }

    fn sum(xs: Vec<i64>) -> i64 {
        xs.iter().sum()
    }

    fn count(xs: Vec<i64>) -> i64 {
        i64::try_from(xs.len()).unwrap_or(i64::MAX)
    }
);

