//! Minimal binding the tier-parity walk runs on every execution tier.
//!
//! One scalar crossing and one `String` crossing, so a tier that marshals
//! either shape differently prints a different line.

use gossamer_binding::register_module;

register_module!(
    name: tier_probe,
    doc: "Tier-parity probe for the Rust binding path.",

    fn double(n: i64) -> i64 {
        n * 2
    }

    fn greet(name: String) -> String {
        format!("hello, {name}")
    }
);

/// Keeps the registration linked into the runner.
pub fn __bindings_force_link() {
    __gos_tier_probe::force_link();
}
