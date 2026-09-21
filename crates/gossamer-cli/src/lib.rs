//! Library entry point for the `gos` toolchain.
//!
//! `main.rs` is a tiny shim that calls [`run_main`]. The library
//! form exists so the per-project Rust-binding runner generated
//! from `gossamer-runner-template` can pull `gossamer-cli` in as
//! a dependency, statically link every binding, and dispatch the
//! same subcommand surface as the on-PATH `gos` binary.

#![forbid(unsafe_code)]

/// `print!` for the toolchain's own output: a reader that has gone away ends
/// the process the way it ends a Unix tool, rather than panicking.
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::write_stdout(format_args!($($arg)*))
    };
}

/// `println!` for the toolchain's own output; see [`out!`].
macro_rules! outln {
    () => {
        $crate::write_stdout(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::write_stdout(format_args!("{}\n", format_args!($($arg)*)))
    };
}

pub mod binding_dispatch;
pub mod child_processes;
pub mod cli;
pub mod cmd;
pub mod comptime_fold;
pub mod doc;
pub mod loaders;
pub mod paths;
pub mod repl;
pub mod repl_handles;
pub mod repl_helper;
pub mod style;

pub use binding_dispatch::{DispatchOutcome, dispatch_runner_if_needed, needs_runner_dispatch};

/// Library entry point. Equivalent to running `gos` from the
/// command line, but invokable from a `main()` that wants to do
/// pre-work first (notably the binding-runner shim).
#[must_use]
pub fn run_main() -> std::process::ExitCode {
    cli::run()
}

/// Uses the allocation-light common `gos run` path when its argument grammar
/// is unambiguous, then falls back to the complete Clap command surface.
#[must_use]
pub fn run_main_with_args(args: &[std::ffi::OsString]) -> std::process::ExitCode {
    cli::try_fast_run(args).unwrap_or_else(cli::run)
}

/// Writes the toolchain's own output to stdout.
///
/// A reader that has gone away ends the process as it would a compiled
/// program; any other failure is the panic `print!` raises.
#[doc(hidden)]
pub fn write_stdout(args: std::fmt::Arguments<'_>) {
    use std::io::Write;

    if let Err(err) = std::io::stdout().lock().write_fmt(args) {
        gossamer_runtime::c_abi::print::end_on_closed_stdio(&err);
        panic!("failed printing to stdout: {err}");
    }
}
