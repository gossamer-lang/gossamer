//! Reads the state `shared_counter` holds.

use gossamer_binding::register_module;

register_module!(
    name: read_probe,
    doc: "Reads the shared count.",

    fn count() -> i64 {
        shared_counter::count()
    }
);
