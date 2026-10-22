//! Minimal Rust binding: a single `add` function exposed to Gossamer.
//!
//! `register_module!` emits both the VM thunk `gos run` calls and the
//! C-ABI thunk (`gos_binding_addlib__add`) a native build links, so the same
//! crate is callable from `gos run`, `gos test`, and `gos build --release`.
//!
//! `name: addlib` doubles as the Gossamer-side spelling and the C-ABI symbol
//! prefix.

use gossamer_binding::register_module;

register_module!(
    name: addlib,
    doc: "Minimal demo binding: one Rust function callable from Gossamer.",

    fn add(a: i64, b: i64) -> i64 {
        a + b
    }
);

