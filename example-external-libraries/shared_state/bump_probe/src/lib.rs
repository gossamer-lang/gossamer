//! Writes the state `shared_counter` holds.

use gossamer_binding::register_module;

register_module!(
    name: bump_probe,
    doc: "Bumps the shared count.",

    fn bump() -> i64 {
        shared_counter::bump()
    }
);
