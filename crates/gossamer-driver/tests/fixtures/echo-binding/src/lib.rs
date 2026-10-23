//! Test fixture binding crate.

use gossamer_binding::register_module;

register_module!(
    binding,
    path: "echo",
    symbol_prefix: echo,
    doc: "Test binding for the runner end-to-end.",

    fn shout(s: String) -> String {
        s.to_uppercase()
    }

    fn sum(xs: Vec<i64>) -> i64 {
        xs.iter().sum()
    }
);

