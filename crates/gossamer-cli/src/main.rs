//! `gos` - command-line entry point for the Gossamer toolchain.
//!
//! The actual subcommand surface lives in [`gossamer_cli::cli`], its clap
//! derive and dispatch table. This shim installs the project's
//! `[rust-bindings]` first, for the subcommands that compile or run it, so
//! the resolver, the VM, and the JIT see their items.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    gossamer_cli::prepare_main_thread();
    // Every subcommand runs on one growable stack, so the compiler and the
    // program it runs share the pages either touches.
    gossamer_coro::on_growable_stack(gossamer_coro::stack_limit_bytes(), || {
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        if let Err(err) = gossamer_cli::install_bindings_for_args(&args) {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
        gossamer_cli::run_main_with_args(&args)
    })
}
